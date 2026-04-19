//! Generate MCP server configuration for each backend.
//!
//! Reference: https://github.com/suzuke/AgEnD (TypeScript version)

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    /// Test-only override for `~/.codex`. When set, [`codex_home`] returns this
    /// path instead of the real user home. Prevents test runs from polluting
    /// the developer's real `~/.codex/config.toml`.
    static CODEX_HOME_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Resolve the effective `~/.codex` directory. In tests, a thread-local
/// override takes precedence so integration-style tests can redirect writes
/// to a scratch directory.
fn codex_home() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = CODEX_HOME_OVERRIDE.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    dirs_home().join(".codex")
}

/// Run `f` with the codex-home path overridden to `path` on the current thread.
/// Test-only helper so tests that invoke `generate`/`codex_trust_directory`
/// don't write into the real `$HOME/.codex`.
#[cfg(test)]
pub(crate) fn with_codex_home_override<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    let prev = CODEX_HOME_OVERRIDE.with(|c| c.replace(Some(path.to_path_buf())));
    let result = f();
    CODEX_HOME_OVERRIDE.with(|c| *c.borrow_mut() = prev);
    result
}

/// Get the agend-terminal binary path for MCP server config.
fn binary_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string())
}

/// Get the AGEND_HOME value.
fn home_path() -> String {
    crate::home_dir().display().to_string()
}

/// Standard MCP server entry with env vars.
fn mcp_server_entry() -> serde_json::Value {
    json!({
        "command": binary_path(),
        "args": ["mcp"],
        "env": {
            "AGEND_HOME": home_path()
        }
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
fn upsert_mcp_servers(path: &Path) -> Result<()> {
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
    config["mcpServers"]["agend-terminal"] = mcp_server_entry();

    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// Claude Code: .claude/settings.local.json + mcp-config.json + claude-settings.json
fn configure_claude(working_dir: &Path) -> Result<()> {
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
    upsert_mcp_servers(&path)?;

    // Write standalone mcp-config.json for --mcp-config flag
    let standalone = working_dir.join("mcp-config.json");
    upsert_mcp_servers(&standalone)?;

    // Write statusline capture script (captures session_id from Claude).
    // Atomic write so a racing spawn never observes a half-written script
    // that fails to execute or appears executable with bad contents.
    let statusline_path = working_dir.join("statusline.json");
    let script_ext = if cfg!(windows) { "cmd" } else { "sh" };
    let script_path = working_dir.join(format!("statusline.{script_ext}"));
    let script = if cfg!(windows) {
        let escaped = statusline_path.display().to_string().replace('"', "\"\"");
        format!("@echo off\r\nfindstr \"^\" > \"{escaped}\"\r\necho ok\r\n")
    } else {
        let escaped = statusline_path.display().to_string().replace('\'', "'\\''");
        format!("#!/bin/bash\ncat > '{escaped}'\necho ok\n")
    };
    crate::store::atomic_write(&script_path, script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write claude-settings.json with statusLine config (for --settings flag).
    let settings_path = working_dir.join("claude-settings.json");
    let _settings_lock = crate::store::acquire_file_lock(&config_lock_path(&settings_path))?;
    let settings = json!({
        "statusLine": {
            "type": "command",
            "command": script_path.display().to_string()
        }
    });
    let settings_body = serde_json::to_string_pretty(&settings)?;
    crate::store::atomic_write(&settings_path, settings_body.as_bytes())?;
    tracing::debug!(path = %settings_path.display(), "Claude settings written");

    Ok(())
}

/// Kiro: .kiro/settings/mcp.json — uses wrapper script because Kiro ignores env block.
///
/// All edits run under a per-path flock + atomic write so two concurrent
/// `create_instance` calls sharing a working_directory can't interleave
/// their read→mutate→write cycles into a corrupt mcp.json.
fn configure_kiro(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".kiro").join("settings").join("mcp.json");

    // Generate wrapper script (Kiro ignores "env" in mcp.json). Atomic so
    // a racing read never sees a partially-written script.
    let wrapper_dir = working_dir.join(".kiro").join("settings");
    std::fs::create_dir_all(&wrapper_dir)?;
    let wrapper_ext = if cfg!(windows) { "cmd" } else { "sh" };
    let wrapper_path = wrapper_dir.join(format!("agend-mcp-wrapper.{wrapper_ext}"));
    let wrapper = if cfg!(windows) {
        format!(
            "@echo off\r\nset \"AGEND_HOME={home}\"\r\n\"{bin}\" mcp\r\n",
            home = home_path(),
            bin = binary_path(),
        )
    } else {
        format!(
            "#!/bin/bash\nexport AGEND_HOME={home}\nexec {bin} mcp\n",
            home = shell_escape(&home_path()),
            bin = shell_escape(&binary_path()),
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
fn configure_gemini(working_dir: &Path) -> Result<()> {
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
    config["mcpServers"]["agend-terminal"] = json!({
        "command": binary_path(),
        "args": ["mcp"],
        "env": { "AGEND_HOME": home_path() },
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
fn configure_opencode(working_dir: &Path) -> Result<()> {
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
    config["mcp"]["agend-terminal"] = json!({
        "type": "local",
        "command": [binary_path(), "mcp"],
        "enabled": true,
        "environment": {
            "AGEND_HOME": home_path()
        }
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
fn configure_codex(working_dir: &Path) -> Result<()> {
    let bin = binary_path();
    let home = home_path();

    // Write per-project .codex/config.toml with MCP server config
    let codex_dir = working_dir.join(".codex");
    std::fs::create_dir_all(&codex_dir)?;
    let config_path = codex_dir.join("config.toml");

    // Read existing config to preserve other settings
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Only write MCP section if not already configured
    if !existing.contains("[mcp_servers.agend-terminal]") {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_path)?;
        let prefix = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        writeln!(
            f,
            r#"{prefix}[mcp_servers.agend-terminal]
command = "{bin}"
args = ["mcp"]

[mcp_servers.agend-terminal.env]
AGEND_HOME = "{home}""#
        )?;
    }
    tracing::debug!(path = %config_path.display(), "configured MCP");

    // Auto-trust working directory in ~/.codex/config.toml
    codex_trust_directory(working_dir);

    Ok(())
}

/// Add a directory to Codex's trusted projects in ~/.codex/config.toml.
///
/// Serialized across concurrent spawns via a sibling `.lock` file. Without the
/// flock, parallel `create_instance` calls race: two writers interleave their
/// `writeln!` syscalls and produce `[projects."a"][projects."b"]` on one line,
/// which breaks `codex` config parsing. The lock scope also re-reads the file
/// *after* acquisition, so a racing writer's entry is visible and we don't
/// append a duplicate.
fn codex_trust_directory(dir: &Path) {
    let codex_dir = codex_home();
    // If ~/.codex doesn't exist, codex isn't installed — silently skip,
    // matching pre-lock behavior where OpenOptions::create would fail.
    if !codex_dir.exists() {
        return;
    }
    let config_path = codex_dir.join("config.toml");
    let lock_path = codex_dir.join(".config.toml.lock");
    let dir_str = dir.display().to_string();
    let toml_key = format!("[projects.\"{dir_str}\"]");

    // Shared helper — deliberately no truncate(true) (flock is on the
    // inode, file contents don't matter). Held for the rest of this
    // function and auto-released on drop.
    let _lock_file = match crate::store::acquire_file_lock(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "codex trust: lock acquisition failed");
            return;
        }
    };

    // Re-read under the lock so a racing writer's entry is visible.
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    if content.contains(&toml_key) {
        return;
    }

    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
    else {
        return;
    };
    let prefix = if content.is_empty() || content.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    // Single write_all of a pre-formatted buffer so a would-be interleave
    // across multiple write() syscalls is impossible.
    let entry = format!("{prefix}{toml_key}\ntrust_level = \"trusted\"\n");
    if let Err(e) = f.write_all(entry.as_bytes()) {
        tracing::warn!(error = %e, "codex trust: write failed");
        return;
    }
    tracing::debug!(dir = %dir_str, "Codex directory trusted");
}

fn dirs_home() -> std::path::PathBuf {
    crate::user_home_dir()
}

/// Detect backend from command name and configure MCP.
pub fn configure(working_dir: &Path, command: &str) {
    let backend = crate::backend::Backend::from_command(command);
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => configure_claude(working_dir),
        Some(crate::backend::Backend::KiroCli) => configure_kiro(working_dir),
        Some(crate::backend::Backend::Gemini) => configure_gemini(working_dir),
        Some(crate::backend::Backend::OpenCode) => configure_opencode(working_dir),
        Some(crate::backend::Backend::Codex) => configure_codex(working_dir),
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
        configure_opencode(&dir).expect("configure");
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
        configure_opencode(&dir).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let cmd = &config["mcp"]["agend-terminal"]["command"];
        assert!(cmd.is_array(), "command must be array, got: {cmd}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_has_type_local() {
        let dir = tmp_dir("oc_type");
        configure_opencode(&dir).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(config["mcp"]["agend-terminal"]["type"], "local");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_sets_permission_allow_for_autonomous_actions() {
        let dir = tmp_dir("oc_perm");
        configure_opencode(&dir).expect("configure");
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
        configure_opencode(&dir).expect("configure");
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
        configure_opencode(&dir).expect("configure");
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
        configure_kiro(&dir).expect("configure");
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
        configure_kiro(&dir).expect("first configure");
        configure_kiro(&dir).expect("second configure");
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
        configure_opencode(&dir).expect("configure");
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
        configure_opencode(&dir).expect("configure");
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
        configure_kiro(&dir).expect("configure");
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
        configure_kiro(&dir).expect("configure");
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
        configure_kiro(&dir).expect("configure");
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
        configure_gemini(&dir).expect("configure");
        let content = std::fs::read_to_string(dir.join(".gemini/settings.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(config.get("mcpServers").is_some());
        assert!(config["mcpServers"]["agend-terminal"]["command"].is_string());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_mcp_server_marked_trusted() {
        let dir = tmp_dir("gemini_trust");
        configure_gemini(&dir).expect("configure");
        let content = std::fs::read_to_string(dir.join(".gemini/settings.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(
            config["mcpServers"]["agend-terminal"]["trust"], true,
            "Gemini --yolo doesn't cover MCP — settings.json must mark server trusted"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Claude: mcp-config.json + claude-settings.json ---

    #[test]
    fn claude_creates_mcp_config_and_settings() {
        let dir = tmp_dir("claude");
        // git init so configure_claude doesn't fail
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .ok();
        configure_claude(&dir).expect("configure");
        assert!(dir.join("mcp-config.json").exists());
        assert!(dir.join(".claude/settings.local.json").exists());
        assert!(dir.join("claude-settings.json").exists());
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(dir.join(format!("statusline.{ext}")).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claude_statusline_script_has_quoted_path() {
        let dir = tmp_dir("claude_quote");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .ok();
        configure_claude(&dir).expect("configure");
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        let script = std::fs::read_to_string(dir.join(format!("statusline.{ext}"))).expect("read");
        // Path should be quoted — single quotes on Unix (bash), double quotes
        // on Windows (cmd.exe doesn't do single-quote quoting).
        if cfg!(windows) {
            assert!(
                script.contains("findstr \"^\" > \""),
                "statusline path must be double-quoted: {script}"
            );
        } else {
            assert!(
                script.contains("cat > '"),
                "statusline path must be single-quoted: {script}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- configure() dispatches correctly ---

    #[test]
    fn configure_dispatches_opencode() {
        let dir = tmp_dir("dispatch_oc");
        configure(&dir, "opencode");
        assert!(dir.join("opencode.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configure_dispatches_gemini() {
        let dir = tmp_dir("dispatch_gem");
        configure(&dir, "gemini");
        assert!(dir.join(".gemini/settings.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configure_unknown_backend_no_crash() {
        let dir = tmp_dir("dispatch_unknown");
        configure(&dir, "unknown-tool");
        // Should not create any config files
        assert!(!dir.join("opencode.json").exists());
        assert!(!dir.join(".gemini").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- codex_trust_directory ---

    #[test]
    fn codex_trust_writes_toml() {
        let dir = tmp_dir("codex_trust");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");

        let work_dir = dir.join("project");
        std::fs::create_dir_all(&work_dir).expect("create project");

        with_codex_home_override(&codex_dir, || {
            codex_trust_directory(&work_dir);
        });

        let content = std::fs::read_to_string(&config_path).expect("read");
        let key = format!("[projects.\"{}\"]", work_dir.display());
        assert!(content.contains(&key), "missing {key} in {content}");
        assert!(content.contains("trust_level = \"trusted\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_trust_idempotent() {
        let dir = tmp_dir("codex_trust_idem");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");

        let work_dir = dir.join("project");
        std::fs::create_dir_all(&work_dir).expect("create project");

        with_codex_home_override(&codex_dir, || {
            codex_trust_directory(&work_dir);
            codex_trust_directory(&work_dir);
        });

        let content = std::fs::read_to_string(&config_path).expect("read");
        let key = format!("[projects.\"{}\"]", work_dir.display());
        assert_eq!(
            content.matches(&key).count(),
            1,
            "entry must not duplicate on second call"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_trust_concurrent_writes_stay_valid() {
        // P0-5 regression guard: codex was the only backend with a flock in the
        // original code; Stage 2 extended the pattern to the others. This test
        // exercises codex_trust_directory from 8 threads racing on the same
        // config.toml and asserts:
        //   1. The file is syntactically valid TOML at the end.
        //   2. The trusted-project entry appears exactly once (idempotent under race).
        let dir = tmp_dir("codex_trust_concurrent");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");
        let work_dir = dir.join("project");
        std::fs::create_dir_all(&work_dir).expect("create project");

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let codex_dir = codex_dir.clone();
                let work_dir = work_dir.clone();
                std::thread::spawn(move || {
                    with_codex_home_override(&codex_dir, || {
                        codex_trust_directory(&work_dir);
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }

        let content = std::fs::read_to_string(&config_path).expect("read");
        let key = format!("[projects.\"{}\"]", work_dir.display());
        assert_eq!(
            content.matches(&key).count(),
            1,
            "8 concurrent calls must still produce exactly one entry, got:\n{content}"
        );
        assert!(
            content.contains("trust_level = \"trusted\""),
            "trust_level must be present after concurrent writes:\n{content}"
        );
        // Sanity: every non-empty line is either a section header or key=val.
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            assert!(
                trimmed.starts_with('[') || trimmed.contains('='),
                "malformed toml line detected (likely interleaved writes): {line:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
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
                    configure_gemini(&dir).expect("configure_gemini");
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
                    configure_opencode(&dir).expect("configure_opencode");
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

    #[test]
    fn codex_trust_skips_when_codex_dir_missing() {
        let dir = tmp_dir("codex_trust_absent");
        // Point override at a non-existent codex home — function should no-op.
        let fake_codex = dir.join("no-such-codex");
        let work_dir = dir.join("project");
        std::fs::create_dir_all(&work_dir).ok();

        with_codex_home_override(&fake_codex, || {
            codex_trust_directory(&work_dir);
        });

        assert!(
            !fake_codex.exists(),
            "function must not create the codex dir when it's absent"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
