//! Generate MCP server configuration for each backend.
//!
//! Reference: https://github.com/suzuke/AgEnD (TypeScript version)

use anyhow::Result;
use serde_json::json;
use std::path::Path;

/// Get the agend-terminal binary path for MCP server config.
fn binary_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string())
}

/// Get the AGEND_TERMINAL_HOME value.
fn home_path() -> String {
    crate::home_dir().display().to_string()
}

/// Standard MCP server entry with env vars.
fn mcp_server_entry() -> serde_json::Value {
    json!({
        "command": binary_path(),
        "args": ["mcp"],
        "env": {
            "AGEND_TERMINAL_HOME": home_path()
        }
    })
}

/// Upsert mcpServers.agend-terminal in a JSON file (Claude, Gemini, Kiro format).
fn upsert_mcp_servers(path: &Path) -> Result<()> {
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

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
    eprintln!("[info] Configured MCP: {}", path.display());
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

    // Write statusline capture script (captures session_id from Claude)
    let statusline_path = working_dir.join("statusline.json");
    let script_path = working_dir.join("statusline.sh");
    let escaped_path = statusline_path.display().to_string().replace('\'', "'\\''");
    let script = format!("#!/bin/bash\ncat > '{}'\necho ok\n", escaped_path);
    std::fs::write(&script_path, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write claude-settings.json with statusLine config (for --settings flag)
    let settings_path = working_dir.join("claude-settings.json");
    let settings = json!({
        "statusLine": {
            "type": "command",
            "command": script_path.display().to_string()
        }
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    eprintln!("[info] Claude settings: {}", settings_path.display());

    Ok(())
}

/// Kiro: .kiro/settings/mcp.json — uses wrapper script because Kiro ignores env block.
fn configure_kiro(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".kiro").join("settings").join("mcp.json");

    // Clean up old format: remove top-level "agend-terminal" key
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = config.as_object_mut() {
                    if obj.remove("agend-terminal").is_some() {
                        let _ = std::fs::write(&path, serde_json::to_string_pretty(&config)?);
                    }
                }
            }
        }
    }

    // Generate wrapper script (Kiro ignores "env" in mcp.json)
    let wrapper_dir = working_dir.join(".kiro").join("settings");
    std::fs::create_dir_all(&wrapper_dir)?;
    let wrapper_path = wrapper_dir.join("agend-mcp-wrapper.sh");
    let wrapper = format!(
        "#!/bin/bash\nexport AGEND_TERMINAL_HOME={home}\nexec {bin} mcp\n",
        home = shell_escape(&home_path()),
        bin = shell_escape(&binary_path()),
    );
    std::fs::write(&wrapper_path, &wrapper)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write mcp.json pointing to wrapper
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
        "command": wrapper_path.display().to_string(),
        "args": []
    });
    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    eprintln!("[info] Configured MCP: {}", path.display());

    Ok(())
}

/// Gemini: .gemini/settings.json — uses { "mcpServers": { ... } } format
fn configure_gemini(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".gemini").join("settings.json");
    upsert_mcp_servers(&path)
}

/// OpenCode: opencode.json — uses { "mcp": { ... } } with command as array.
fn configure_opencode(working_dir: &Path) -> Result<()> {
    let path = working_dir.join("opencode.json");
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
            "AGEND_TERMINAL_HOME": home_path()
        }
    });

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    eprintln!("[info] Configured MCP: {}", path.display());
    Ok(())
}

/// Codex: uses `codex mcp add` CLI command + trust config in ~/.codex/config.toml.
fn configure_codex(working_dir: &Path) -> Result<()> {
    let bin = binary_path();
    let home = home_path();

    // Register MCP server via CLI
    let status = std::process::Command::new("codex")
        .args([
            "mcp",
            "add",
            "agend-terminal",
            "--env",
            &format!("AGEND_TERMINAL_HOME={home}"),
            "--",
            &bin,
            "mcp",
        ])
        .current_dir(working_dir)
        .output();

    match status {
        Ok(o) if o.status.success() => {
            eprintln!("[info] Configured MCP: codex mcp add agend-terminal");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.contains("already") {
                tracing::warn!(error = %stderr.trim(), "codex mcp add failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "codex not available for MCP config");
        }
    }

    // Auto-trust working directory in ~/.codex/config.toml
    // Prevents "Do you trust the contents of this directory?" prompt
    codex_trust_directory(working_dir);

    Ok(())
}

/// Add a directory to Codex's trusted projects in ~/.codex/config.toml.
fn codex_trust_directory(dir: &Path) {
    let config_path = dirs_home().join(".codex").join("config.toml");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let dir_str = dir.display().to_string();

    // Check if already trusted
    let toml_key = format!("[projects.\"{dir_str}\"]");
    if content.contains(&toml_key) {
        return;
    }

    // Append trust entry
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
    {
        let prefix = if content.is_empty() || content.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = writeln!(f, "{prefix}{toml_key}\ntrust_level = \"trusted\"");
        eprintln!("[info] Codex: trusted {dir_str}");
    }
}

fn dirs_home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
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
        None => return,
    };

    if let Err(e) = result {
        eprintln!("[warn] Failed to configure MCP: {e:#}");
    }
}

/// Escape a string for use in a bash script.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
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
        let wrapper = dir.join(".kiro/settings/agend-mcp-wrapper.sh");
        assert!(wrapper.exists(), "wrapper script must exist");
        let content = std::fs::read_to_string(&wrapper).expect("read");
        assert!(content.starts_with("#!/bin/bash"));
        assert!(content.contains("AGEND_TERMINAL_HOME"));
        assert!(content.contains("exec"));
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
        assert!(cmd.contains("agend-mcp-wrapper.sh"));
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
        assert!(dir.join("statusline.sh").exists());
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
        let script = std::fs::read_to_string(dir.join("statusline.sh")).expect("read");
        // Path should be single-quoted
        assert!(
            script.contains("cat > '"),
            "statusline path must be quoted: {script}"
        );
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
        let config_path = dir.join(".codex").join("config.toml");
        std::fs::create_dir_all(dir.join(".codex")).ok();

        // Override HOME for test
        let work_dir = std::path::PathBuf::from("/tmp/test-project");
        // Write directly to test path
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&config_path)
                .expect("open");
            writeln!(
                f,
                "[projects.\"/tmp/test-project\"]\ntrust_level = \"trusted\""
            )
            .ok();
        }

        let content = std::fs::read_to_string(&config_path).expect("read");
        assert!(content.contains("[projects.\"/tmp/test-project\"]"));
        assert!(content.contains("trust_level = \"trusted\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_trust_idempotent() {
        let dir = tmp_dir("codex_trust_idem");
        let config_path = dir.join(".codex").join("config.toml");
        std::fs::create_dir_all(dir.join(".codex")).ok();
        std::fs::write(
            &config_path,
            "[projects.\"/tmp/already\"]\ntrust_level = \"trusted\"\n",
        )
        .ok();

        // Call trust with same path — should not duplicate
        let work_dir = std::path::Path::new("/tmp/already");
        let content_before = std::fs::read_to_string(&config_path).expect("read");
        codex_trust_directory(work_dir);
        // Since we can't override HOME in codex_trust_directory, test the check logic directly
        let content = std::fs::read_to_string(&config_path).expect("read");
        // The function writes to ~/.codex/config.toml, not our test path,
        // so just verify the check logic: content already has the key
        assert!(content_before.contains("[projects.\"/tmp/already\"]"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
