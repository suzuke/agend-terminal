//! Generate MCP server configuration for each backend.
//!
//! Reference: https://github.com/suzuke/AgEnD (TypeScript version)
//!
//! **Scope rule:** every write here must land inside `$AGEND_HOME` or inside
//! the agent's `working_directory`. User-global tool configs (`~/.codex/`,
//! `~/.claude/`, etc.) are off-limits — mutating them risks corrupting the
//! user's personal CLI setup and can't be cleanly undone. If a backend seems
//! to need global state (codex trust prompt was the reason for the old
//! `codex_trust_directory` write), reach for a CLI flag or `dismiss_patterns`
//! in `src/backend.rs` instead.

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Get the agend-terminal binary path for MCP server config.
fn binary_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string())
}

/// Get the agend-mcp-bridge binary path. Lives alongside the main binary.
/// Falls back to the main binary with `mcp` arg if bridge not found.
fn bridge_binary_path() -> (String, Vec<&'static str>) {
    if let Ok(exe) = std::env::current_exe() {
        let bridge = exe.with_file_name("agend-mcp-bridge");
        if bridge.exists() {
            return (bridge.display().to_string(), vec![]);
        }
    }
    // Fallback: use main binary with --mcp flag (pre-Option-F behaviour)
    (binary_path(), vec!["mcp"])
}

/// Get the AGEND_HOME value.
fn home_path() -> String {
    crate::home_dir().display().to_string()
}

/// Standard MCP server entry with env vars.
fn mcp_server_entry(instance_name: Option<&str>) -> serde_json::Value {
    let mut env = json!({
        "AGEND_HOME": home_path()
    });
    if let Some(name) = instance_name {
        env["AGEND_INSTANCE_NAME"] = json!(name);
    }
    let (cmd, args) = bridge_binary_path();
    json!({
        "command": cmd,
        "args": args,
        "env": env
    })
}

/// Per-config flock path for a given config file. Two concurrent `configure`
/// calls targeting the same working_directory would otherwise interleave
/// their read→mutate→write cycles (one reads stale content, applies its
/// edit, overwrites the other's edit). We use a sibling `.lock` file so
/// the lock is local to the project dir and auto-released on drop.
fn config_lock_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    parent.join(format!(".{name}.lock"))
}

/// Upsert mcpServers.agend-terminal in a JSON file (Claude, Gemini, Kiro format).
///
/// Flock-serialised + atomic write. Prior implementation `fs::write`'d
/// directly with no lock, so two concurrent `create_instance` calls
/// targeting the same working_directory could drop one of their edits.
fn upsert_mcp_servers(path: &Path, instance_name: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = mcp_server_entry(instance_name);

    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// Claude Code: .claude/settings.local.json + mcp-config.json
fn configure_claude(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    // Ensure working dir is a git repo (Claude Code needs git root to find .claude/)
    let git_dir = working_dir.join(".git");
    if !git_dir.exists() {
        match std::process::Command::new("git")
            .args(["init"])
            .current_dir(working_dir)
            .output()
        {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    dir = %working_dir.display(),
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "git init failed"
                );
            }
            Err(e) => {
                tracing::warn!(dir = %working_dir.display(), error = %e, "git init failed");
            }
            _ => {}
        }
    }

    // Write project-local MCP config
    let path = working_dir.join(".claude").join("settings.local.json");
    upsert_mcp_servers(&path, instance_name)?;

    // Write standalone mcp-config.json for --mcp-config flag
    let standalone = working_dir.join("mcp-config.json");
    upsert_mcp_servers(&standalone, instance_name)?;

    Ok(())
}

/// Kiro: .kiro/settings/mcp.json — uses wrapper script because Kiro ignores env block.
///
/// All edits run under a per-path flock + atomic write so two concurrent
/// `create_instance` calls sharing a working_directory can't interleave
/// their read→mutate→write cycles into a corrupt mcp.json.
fn configure_kiro(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join(".kiro").join("settings").join("mcp.json");

    // Generate wrapper script (Kiro ignores "env" in mcp.json). Atomic so
    // a racing read never sees a partially-written script.
    let wrapper_dir = working_dir.join(".kiro").join("settings");
    std::fs::create_dir_all(&wrapper_dir)?;
    let wrapper_ext = if cfg!(windows) { "cmd" } else { "sh" };
    let wrapper_path = wrapper_dir.join(format!("agend-mcp-wrapper.{wrapper_ext}"));
    let instance_env_win = instance_name
        .map(|n| format!("set \"AGEND_INSTANCE_NAME={n}\"\r\n"))
        .unwrap_or_default();
    let instance_env_unix = instance_name
        .map(|n| format!("export AGEND_INSTANCE_NAME={}\n", shell_escape(n)))
        .unwrap_or_default();
    let (bridge_cmd, bridge_args) = bridge_binary_path();
    let bridge_args_str = bridge_args.join(" ");
    let wrapper = if cfg!(windows) {
        format!(
            "@echo off\r\nset \"AGEND_HOME={home}\"\r\n{instance_env_win}\"{bin}\" {args}\r\n",
            home = home_path(),
            bin = bridge_cmd,
            args = bridge_args_str,
        )
    } else {
        format!(
            "#!/bin/bash\nexport AGEND_HOME={home}\n{instance_env_unix}exec {bin} {args}\n",
            home = shell_escape(&home_path()),
            bin = shell_escape(&bridge_cmd),
            args = bridge_args_str,
        )
    };
    crate::store::atomic_write(&wrapper_path, wrapper.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Hold the mcp.json flock across the cleanup+rewrite so legacy-key
    // removal and mcpServers upsert can't race a sibling caller.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Clean up old format: remove top-level "agend-terminal" key (pre-dates
    // the mcpServers schema). Done under the same lock as the upsert below.
    if let Some(obj) = config.as_object_mut() {
        obj.remove("agend-terminal");
    }

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = json!({
        "command": wrapper_path.display().to_string(),
        "args": [],
        // Auto-approve every tool from this server. `--trust-all-tools` only
        // covers Kiro's built-in tools; per-MCP-server trust is set here.
        "autoApprove": ["*"]
    });
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");

    Ok(())
}

/// Gemini: .gemini/settings.json — uses { "mcpServers": { ... } } format.
///
/// Adds `"trust": true` to the agend-terminal entry. `--yolo` / `--approval-mode
/// yolo` only auto-approve built-in tools and shell; MCP tool calls still
/// prompt unless the server is marked trusted in settings.json.
fn configure_gemini(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join(".gemini").join("settings.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Flock + atomic write so concurrent spawn calls can't lose each
    // other's edits to the shared settings.json.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    let mut env = json!({ "AGEND_HOME": home_path() });
    if let Some(name) = instance_name {
        env["AGEND_INSTANCE_NAME"] = json!(name);
    }
    config["mcpServers"]["agend-terminal"] = json!({
        "command": binary_path(),
        "args": ["mcp"],
        "env": env,
        "trust": true
    });
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// OpenCode: opencode.json — uses { "mcp": { ... } } with command as array.
///
/// Also sets the `permission` block to "allow" for the actions an autonomous
/// agent will hit (edit / bash / webfetch / external_directory). Each instance
/// has its own working_dir/opencode.json so this does not bleed into the
/// user's manual opencode usage.
fn configure_opencode(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join("opencode.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Flock + atomic write so concurrent spawns can't interleave their
    // load-modify-save cycles on opencode.json.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Remove old wrong format if present
    if let Some(obj) = config.as_object_mut() {
        obj.remove("mcpServers");
    }

    if config.get("mcp").is_none() {
        config["mcp"] = json!({});
    }
    let mut oc_env = json!({
        "AGEND_HOME": home_path()
    });
    if let Some(name) = instance_name {
        oc_env["AGEND_INSTANCE_NAME"] = json!(name);
    }
    config["mcp"]["agend-terminal"] = json!({
        "type": "local",
        "command": [binary_path(), "mcp"],
        "enabled": true,
        "environment": oc_env
    });

    // Force `permission` to an object so we can insert keys; replaces any
    // pre-existing scalar form (e.g. "ask") since autonomous agents must
    // not block on prompts.
    if !config
        .get("permission")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        config["permission"] = json!({});
    }
    let perm = config["permission"]
        .as_object_mut()
        .expect("permission set to object above");
    for key in ["edit", "bash", "webfetch", "external_directory"] {
        perm.insert(key.to_string(), json!("allow"));
    }

    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// Codex: write .codex/config.toml per-project + trust in ~/.codex/config.toml.
///
/// `codex mcp add` only writes to global config and doesn't support per-project.
/// But Codex loads .codex/config.toml from the project root (trusted projects only).
/// This gives us per-instance AGEND_INSTANCE_NAME via project-level config.
/// Section headers owned by this writer — the two tables we strip+rewrite.
const CODEX_MCP_HEADER: &str = "[mcp_servers.agend-terminal]";
const CODEX_MCP_ENV_HEADER: &str = "[mcp_servers.agend-terminal.env]";

fn configure_codex(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    configure_codex_with_home(working_dir, &home_path(), instance_name)
}

/// Split out so tests can drive a scratch `home` without mutating
/// process-wide `HOME` / `USERPROFILE`. `cargo test` runs tests in parallel
/// inside one process, and `user_home_dir()` is read by many backends — env
/// mutation here races with other tests.
fn configure_codex_with_home(
    working_dir: &Path,
    home: &str,
    instance_name: Option<&str>,
) -> Result<()> {
    let bin = binary_path();

    let codex_dir = working_dir.join(".codex");
    std::fs::create_dir_all(&codex_dir)?;
    let config_path = codex_dir.join("config.toml");

    // Flock + atomic write so two concurrent spawns can't interleave their
    // strip→append cycles on the same config.toml.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&config_path))?;

    // Re-read under the lock. Any existing agend-terminal block is stripped
    // before we write a fresh one — otherwise a stale binary path from a
    // prior build (e.g. a worktree that has since been removed) would
    // silently persist and fail at codex MCP startup with ENOENT.
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut stripped = strip_agend_mcp_sections(&existing);
    // Normalize to exactly one trailing newline on non-empty content so the
    // next `\n` we emit produces a single blank-line separator.
    while stripped.ends_with("\n\n") {
        stripped.pop();
    }
    if !stripped.is_empty() && !stripped.ends_with('\n') {
        stripped.push('\n');
    }
    let separator = if stripped.is_empty() { "" } else { "\n" };

    // Single-quoted TOML literal strings preserve backslashes verbatim;
    // a double-quoted basic string interprets `\U` / `\n` / `\t` as escapes
    // and codex rejects its own config.toml on Windows when the binary path
    // happens to contain any of them. See `toml_string_value` for the
    // apostrophe fallback.
    let bin_lit = toml_string_value(&bin);
    let home_lit = toml_string_value(home);
    let instance_line = instance_name
        .map(|n| format!("AGEND_INSTANCE_NAME = {}\n", toml_string_value(n)))
        .unwrap_or_default();
    let body = format!(
        r#"{stripped}{separator}{CODEX_MCP_HEADER}
command = {bin_lit}
args = ["mcp"]

{CODEX_MCP_ENV_HEADER}
AGEND_HOME = {home_lit}
{instance_line}"#
    );

    // Skip the atomic_write (temp file + fsync + rename) when the file is
    // already up to date. configure_codex runs on every codex pane spawn, so
    // the steady-state call is the no-op case.
    if existing != body {
        crate::store::atomic_write(&config_path, body.as_bytes())?;
    }
    tracing::debug!(path = %config_path.display(), "configured MCP");

    // NOTE: intentionally no `codex_trust_directory` write to
    // `~/.codex/config.toml`. That file is the user's personal codex config
    // and must stay untouched. The trust prompt is handled by
    // `--dangerously-bypass-approvals-and-sandbox` on the codex command line
    // (see `src/backend.rs`) plus the "Do you trust" dismiss_pattern as a
    // fallback. Writing here would pollute user state and has caused multiple
    // production bugs (see removed `codex_trust_directory` in git history).

    Ok(())
}

/// Render a string value as a TOML string, picking whichever quoting style
/// survives on the target. Windows paths routinely contain `\U` / `\d` / …
/// which a double-quoted basic string interprets as escapes and then fails
/// to parse. Single-quoted literal strings don't interpret anything, so they
/// round-trip any path. Fall back to an escaped basic string only if the
/// value contains a `'`, which a single-line literal can't represent.
fn toml_string_value(s: &str) -> String {
    if s.contains('\'') {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        format!("'{s}'")
    }
}

/// Remove any `[mcp_servers.agend-terminal]` / `[mcp_servers.agend-terminal.env]`
/// sections from a TOML string, preserving every other section and comment.
/// A section runs from its `[header]` line through the line before the next
/// `[header]` at the start of a line, or end-of-file.
fn strip_agend_mcp_sections(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_target = false;
    for raw_line in content.split_inclusive('\n') {
        let trimmed = raw_line.trim();
        // A section header is a line whose trimmed form is `[...]` — matches
        // both tables (`[foo]`) and array-of-tables (`[[foo]]`), and excludes
        // value lines that start with `[` inside a string.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_target = trimmed == CODEX_MCP_HEADER || trimmed == CODEX_MCP_ENV_HEADER;
            if in_target {
                continue;
            }
        }
        if !in_target {
            out.push_str(raw_line);
        }
    }
    out
}

/// Detect backend from command name and configure MCP.
pub fn configure(working_dir: &Path, command: &str, instance_name: Option<&str>) {
    let backend = crate::backend::Backend::from_command(command);
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => configure_claude(working_dir, instance_name),
        Some(crate::backend::Backend::KiroCli) => configure_kiro(working_dir, instance_name),
        Some(crate::backend::Backend::Gemini) => configure_gemini(working_dir, instance_name),
        Some(crate::backend::Backend::OpenCode) => configure_opencode(working_dir, instance_name),
        Some(crate::backend::Backend::Codex) => configure_codex(working_dir, instance_name),
        // Non-preset backends (Shell, Raw) have no MCP wiring.
        Some(crate::backend::Backend::Shell) | Some(crate::backend::Backend::Raw(_)) | None => {
            return
        }
    };

    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to configure MCP");
    }
}

/// Escape a string for use in a bash script.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-mcp-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("/usr/bin/foo"), "'/usr/bin/foo'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(
            shell_escape("/path with spaces/bin"),
            "'/path with spaces/bin'"
        );
    }

    // --- OpenCode: must use "mcp" key, not "mcpServers" ---

    #[test]
    fn opencode_uses_mcp_key() {
        let dir = tmp_dir("oc_key");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(config.get("mcp").is_some(), "must have 'mcp' key");
        assert!(
            config.get("mcpServers").is_none(),
            "must NOT have 'mcpServers'"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_command_is_array() {
        let dir = tmp_dir("oc_cmd");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let cmd = &config["mcp"]["agend-terminal"]["command"];
        assert!(cmd.is_array(), "command must be array, got: {cmd}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_has_type_local() {
        let dir = tmp_dir("oc_type");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(config["mcp"]["agend-terminal"]["type"], "local");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_sets_permission_allow_for_autonomous_actions() {
        let dir = tmp_dir("oc_perm");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let perm = &config["permission"];
        for key in ["edit", "bash", "webfetch", "external_directory"] {
            assert_eq!(
                perm[key], "allow",
                "permission.{key} must be \"allow\" so autonomous agents don't block"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_permission_replaces_scalar_form() {
        let dir = tmp_dir("oc_perm_scalar");
        // Pre-existing scalar form must be coerced to object — otherwise our
        // insert would silently fail and external_directory keeps prompting.
        let pre = json!({"permission": "ask"});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&pre).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            config["permission"].is_object(),
            "scalar permission must be replaced with object"
        );
        assert_eq!(config["permission"]["external_directory"], "allow");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_permission_preserves_unrelated_keys() {
        let dir = tmp_dir("oc_perm_preserve");
        let pre = json!({"permission": {"read": "deny", "edit": "deny"}});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&pre).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        // Our managed keys overwrite (autonomous context demands "allow").
        assert_eq!(config["permission"]["edit"], "allow");
        // Keys we don't manage stay untouched.
        assert_eq!(config["permission"]["read"], "deny");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_mcp_server_has_autoapprove_wildcard() {
        let dir = tmp_dir("kiro_autoapprove");
        configure_kiro(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let entry = &config["mcpServers"]["agend-terminal"];
        let auto = entry["autoApprove"]
            .as_array()
            .expect("autoApprove must be array");
        assert!(
            auto.iter().any(|v| v == "*"),
            "autoApprove must contain \"*\" wildcard so MCP tool calls don't prompt"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_autoapprove_idempotent_across_runs() {
        let dir = tmp_dir("kiro_autoapprove_idem");
        configure_kiro(&dir, None).expect("first configure");
        configure_kiro(&dir, None).expect("second configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let auto = config["mcpServers"]["agend-terminal"]["autoApprove"]
            .as_array()
            .expect("autoApprove must be array");
        assert_eq!(auto.len(), 1, "autoApprove must not duplicate on re-run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_uses_environment_not_env() {
        let dir = tmp_dir("oc_env");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let entry = &config["mcp"]["agend-terminal"];
        assert!(
            entry.get("environment").is_some(),
            "must have 'environment'"
        );
        assert!(entry.get("env").is_none(), "must NOT have 'env'");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_removes_old_mcpservers() {
        let dir = tmp_dir("oc_migrate");
        // Write old wrong format
        let old = json!({"mcpServers": {"agend-terminal": {"command": "old"}}});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&old).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            config.get("mcpServers").is_none(),
            "old mcpServers must be removed"
        );
        assert!(config.get("mcp").is_some(), "new mcp key must exist");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Kiro: must use wrapper script ---

    #[test]
    fn kiro_creates_wrapper_script() {
        let dir = tmp_dir("kiro_wrapper");
        configure_kiro(&dir, None).expect("configure");
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        let wrapper = dir.join(format!(".kiro/settings/agend-mcp-wrapper.{ext}"));
        assert!(
            wrapper.exists(),
            "wrapper script must exist at {}",
            wrapper.display()
        );
        let content = std::fs::read_to_string(&wrapper).expect("read");
        assert!(content.contains("AGEND_HOME"));
        if cfg!(windows) {
            assert!(content.starts_with("@echo off"));
        } else {
            assert!(content.starts_with("#!/bin/bash"));
            assert!(content.contains("exec"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_mcp_json_points_to_wrapper() {
        let dir = tmp_dir("kiro_json");
        configure_kiro(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let cmd = config["mcpServers"]["agend-terminal"]["command"]
            .as_str()
            .expect("command str");
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        let needle = format!("agend-mcp-wrapper.{ext}");
        assert!(cmd.contains(&needle), "expected {needle} in {cmd}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_no_env_in_mcp_json() {
        let dir = tmp_dir("kiro_noenv");
        configure_kiro(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            config["mcpServers"]["agend-terminal"].get("env").is_none(),
            "kiro mcp.json should NOT have env (use wrapper instead)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Gemini: mcpServers format ---

    #[test]
    fn gemini_uses_mcpservers() {
        let dir = tmp_dir("gemini");
        configure_gemini(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".gemini/settings.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(config.get("mcpServers").is_some());
        assert!(config["mcpServers"]["agend-terminal"]["command"].is_string());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_mcp_server_marked_trusted() {
        let dir = tmp_dir("gemini_trust");
        configure_gemini(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".gemini/settings.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(
            config["mcpServers"]["agend-terminal"]["trust"], true,
            "Gemini --yolo doesn't cover MCP — settings.json must mark server trusted"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Claude: mcp-config.json + .claude/settings.local.json ---

    #[test]
    fn claude_creates_mcp_config() {
        let dir = tmp_dir("claude");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .ok();
        configure_claude(&dir, None).expect("configure");
        assert!(dir.join("mcp-config.json").exists());
        assert!(dir.join(".claude/settings.local.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- configure() dispatches correctly ---

    #[test]
    fn configure_dispatches_opencode() {
        let dir = tmp_dir("dispatch_oc");
        configure(&dir, "opencode", None);
        assert!(dir.join("opencode.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configure_dispatches_gemini() {
        let dir = tmp_dir("dispatch_gem");
        configure(&dir, "gemini", None);
        assert!(dir.join(".gemini/settings.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configure_unknown_backend_no_crash() {
        let dir = tmp_dir("dispatch_unknown");
        configure(&dir, "unknown-tool", None);
        // Should not create any config files
        assert!(!dir.join("opencode.json").exists());
        assert!(!dir.join(".gemini").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- toml_string_value helper ---

    #[test]
    fn toml_string_value_uses_literal_for_paths_with_backslashes() {
        // Windows paths contain `\U` / `\d` / … which a TOML basic string
        // interprets as escape triggers. Literal (single-quoted) form is the
        // safe choice — it's what configure_codex emits for command/AGEND_HOME.
        assert_eq!(
            toml_string_value(r"C:\Users\alice\agend"),
            "'C:\\Users\\alice\\agend'"
        );
        assert_eq!(toml_string_value("/home/alice"), "'/home/alice'");
    }

    #[test]
    fn toml_string_value_escapes_basic_string_when_apostrophe_present() {
        // Single-line literal can't contain a `'` — fall back to basic string
        // with `\` and `"` escaped.
        assert_eq!(toml_string_value("it's mine"), "\"it's mine\"");
        assert_eq!(
            toml_string_value(r"C:\Program' Files\x"),
            r#""C:\\Program' Files\\x""#
        );
    }

    #[test]
    fn gemini_concurrent_configure_keeps_json_valid_and_trusted() {
        // Stage 2 extended the per-path flock to Gemini/Kiro/Claude/OpenCode.
        // Race configure_gemini from 8 threads on the same working_dir and
        // assert the settings.json is still parseable and retains the trust flag.
        let dir = tmp_dir("gemini_concurrent");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    configure_gemini(&dir, None).expect("configure_gemini");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }

        let content = std::fs::read_to_string(dir.join(".gemini/settings.json")).expect("read");
        let config: serde_json::Value =
            serde_json::from_str(&content).expect("concurrent writes must leave valid JSON");
        assert_eq!(
            config["mcpServers"]["agend-terminal"]["trust"], true,
            "trust must still be true after concurrent configures"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_concurrent_configure_keeps_json_valid() {
        // Same race test against configure_opencode — opencode.json is
        // read→mutate→atomic_write under a flock.
        let dir = tmp_dir("opencode_concurrent");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    configure_opencode(&dir, None).expect("configure_opencode");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }

        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value =
            serde_json::from_str(&content).expect("concurrent writes must leave valid JSON");
        // The agend-terminal entry must still be present and well-formed.
        assert!(config["mcp"]["agend-terminal"]["command"].is_array());
        assert_eq!(config["mcp"]["agend-terminal"]["type"], "local");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- configure_codex: MCP block must refresh, not skip-if-exists ---

    /// Regression for the `~/.codex/config.toml` write that was removed in
    /// this refactor. configure_codex used to trail with `codex_trust_directory`
    /// which mutated the user's global config — shipped two escape bugs into
    /// production, raced with the codex CLI's own writer, and left entries
    /// behind on uninstall. `agend must never touch user-global tool config`
    /// is now the rule (see the module doc comment).
    ///
    /// The test drives `configure_codex_with_home` directly with a scratch
    /// home path rather than mutating `$HOME` / `$USERPROFILE`. Env mutation
    /// would race with parallel tests that read `user_home_dir()`.
    #[test]
    fn configure_codex_writes_nothing_under_home() {
        let scratch = tmp_dir("no_home_write");
        let fake_home = scratch.join("agend_home");
        std::fs::create_dir_all(&fake_home).expect("mkdir fake_home");
        let work_dir = scratch.join("project");
        std::fs::create_dir_all(&work_dir).expect("mkdir project");

        configure_codex_with_home(
            &work_dir,
            &fake_home.display().to_string(),
            Some("test-instance"),
        )
        .expect("configure_codex");

        // Sanity: per-project config must exist.
        assert!(
            work_dir.join(".codex/config.toml").exists(),
            "per-project .codex/config.toml missing under working_dir"
        );
        // Guard: nothing may be written under the passed-in home. A regression
        // that reintroduces `codex_trust_directory` (writing to `home/.codex/`)
        // would land files here and fail the check.
        let entries: Vec<_> = std::fs::read_dir(&fake_home)
            .expect("read_dir fake_home")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert!(
            entries.is_empty(),
            "configure_codex must not write under its home arg, found: {entries:?}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn codex_config_refreshes_stale_binary_path() {
        // Regression guard: pre-fix code used an append-only write gated by
        // `!existing.contains("[mcp_servers.agend-terminal]")`, so a stale
        // binary path (e.g. from a removed worktree build) silently persisted
        // and codex MCP startup failed with ENOENT. The rewrite must replace
        // the `command` field with the current binary path.
        let dir = tmp_dir("codex_cfg_refresh");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[mcp_servers.agend-terminal]\n\
             command = \"/nonexistent/stale/binary\"\n\
             args = [\"mcp\"]\n\
             \n\
             [mcp_servers.agend-terminal.env]\n\
             AGEND_HOME = \"/old/home\"\n",
        )
        .expect("seed stale");

        configure_codex(&dir, None).expect("configure");

        let content = std::fs::read_to_string(&config_path).expect("read");
        assert!(
            !content.contains("/nonexistent/stale/binary"),
            "stale command must be overwritten:\n{content}"
        );
        assert!(
            !content.contains("/old/home"),
            "stale AGEND_HOME must be overwritten:\n{content}"
        );
        let parsed: toml::Value = toml::from_str(&content).expect("valid TOML after rewrite");
        let cmd = parsed["mcp_servers"]["agend-terminal"]["command"]
            .as_str()
            .expect("command string");
        assert_ne!(cmd, "/nonexistent/stale/binary");
        // Exactly one of each header — the stripper must not leave orphans.
        assert_eq!(content.matches(CODEX_MCP_HEADER).count(), 1);
        assert_eq!(content.matches(CODEX_MCP_ENV_HEADER).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_preserves_unrelated_sections() {
        // Other TOML sections (user settings, profiles) must survive the
        // strip+rewrite cycle. The stripper only targets the two agend-terminal
        // headers by exact match.
        let dir = tmp_dir("codex_cfg_preserve");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "model = \"gpt-5\"\n\
             \n\
             [mcp_servers.agend-terminal]\n\
             command = \"/old\"\n\
             args = [\"mcp\"]\n\
             \n\
             [mcp_servers.agend-terminal.env]\n\
             AGEND_HOME = \"/old\"\n\
             \n\
             [profile.custom]\n\
             model = \"other\"\n",
        )
        .expect("seed");

        configure_codex(&dir, None).expect("configure");

        let content = std::fs::read_to_string(&config_path).expect("read");
        let parsed: toml::Value = toml::from_str(&content).expect("valid TOML");
        assert_eq!(
            parsed["model"].as_str(),
            Some("gpt-5"),
            "top-level key dropped:\n{content}"
        );
        assert_eq!(
            parsed["profile"]["custom"]["model"].as_str(),
            Some("other"),
            "unrelated section dropped:\n{content}"
        );
        assert!(
            !content.contains("\"/old\""),
            "stale value leaked:\n{content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_idempotent_across_reruns() {
        // Re-running configure twice must leave the file byte-identical —
        // no duplicated headers, no drifting whitespace.
        //
        // Use `_with_home(scratch)` rather than `configure_codex` so the
        // emitted `AGEND_HOME = ...` value is fixed for both runs.
        // `configure_codex` reads `home_path()` (global `AGEND_HOME` env
        // var), which races with any other test in the process that
        // mutates that env — e.g. the `mcp::handlers::tests` Fleet
        // emission tests — and caused this test to drift across reruns.
        let dir = tmp_dir("codex_cfg_idem");
        std::fs::create_dir_all(dir.join(".codex")).expect("create .codex");
        let scratch_home = "/tmp/agend-test-home-codex-idem";

        configure_codex_with_home(&dir, scratch_home, Some("test-instance")).expect("first");
        let after_first =
            std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read first");
        configure_codex_with_home(&dir, scratch_home, Some("test-instance")).expect("second");
        let after_second =
            std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read second");

        assert_eq!(
            after_first, after_second,
            "second run drifted file:\nfirst:\n{after_first}\nsecond:\n{after_second}"
        );
        assert_eq!(after_second.matches(CODEX_MCP_HEADER).count(), 1);
        assert_eq!(after_second.matches(CODEX_MCP_ENV_HEADER).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_strips_only_agend_headers() {
        // Unit test for the stripper: only the two exact header names match.
        // A sibling `[mcp_servers.other]` must survive, and a comment that
        // mentions the header text but doesn't open a section must not
        // trigger the stripper.
        let input = "# [mcp_servers.agend-terminal] mentioned in top-level comment\n\
                     [other]\n\
                     value = 1\n\
                     \n\
                     [mcp_servers.agend-terminal]\n\
                     command = \"x\"\n\
                     \n\
                     [mcp_servers.other]\n\
                     command = \"y\"\n\
                     \n\
                     [mcp_servers.agend-terminal.env]\n\
                     AGEND_HOME = \"h\"\n";
        let out = strip_agend_mcp_sections(input);
        assert!(
            !out.contains("command = \"x\""),
            "target body leaked: {out}"
        );
        assert!(
            !out.contains("AGEND_HOME = \"h\""),
            "target env body leaked: {out}"
        );
        assert!(out.contains("[other]"), "unrelated section dropped: {out}");
        assert!(
            out.contains("[mcp_servers.other]"),
            "sibling mcp_servers dropped: {out}"
        );
        assert!(
            out.contains("# [mcp_servers.agend-terminal] mentioned in top-level comment"),
            "top-level comment dropped — stripper matched comment as header: {out}"
        );
    }

    // -----------------------------------------------------------------
    // AGEND_INSTANCE_NAME injection pins (fix/mcp-instance-name-env)
    // -----------------------------------------------------------------

    #[test]
    fn codex_config_includes_instance_name() {
        let scratch = tmp_dir("codex_inst_name");
        let dir = scratch.join("project");
        std::fs::create_dir_all(&dir).ok();
        configure_codex_with_home(&dir, "/fake/home", Some("my-agent")).expect("configure_codex");
        let content = std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read toml");
        assert!(
            content.contains("AGEND_INSTANCE_NAME"),
            "TOML env must contain AGEND_INSTANCE_NAME: {content}"
        );
        assert!(
            content.contains("my-agent"),
            "TOML env must contain the instance name value: {content}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn codex_idempotent_preserves_instance_name() {
        let scratch = tmp_dir("codex_idempotent");
        let dir = scratch.join("project");
        std::fs::create_dir_all(&dir).ok();
        configure_codex_with_home(&dir, "/fake/home", Some("agent-1")).expect("first");
        configure_codex_with_home(&dir, "/fake/home", Some("agent-1")).expect("second");
        let content = std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read toml");
        // Must appear exactly once (strip+rewrite cycle doesn't duplicate)
        assert_eq!(
            content.matches("AGEND_INSTANCE_NAME").count(),
            1,
            "AGEND_INSTANCE_NAME must appear exactly once after idempotent rewrite: {content}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn json_backends_include_instance_name() {
        let entry = mcp_server_entry(Some("dev-2"));
        assert_eq!(
            entry["env"]["AGEND_INSTANCE_NAME"], "dev-2",
            "mcp_server_entry must include AGEND_INSTANCE_NAME in env"
        );
        // None case: no AGEND_INSTANCE_NAME key
        let entry_none = mcp_server_entry(None);
        assert!(
            entry_none["env"].get("AGEND_INSTANCE_NAME").is_none(),
            "mcp_server_entry(None) must not include AGEND_INSTANCE_NAME"
        );
    }
}
