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

/// Codex: uses `codex mcp add` CLI command — no static config file for MCP.
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
            // Already registered is OK
            if !stderr.contains("already") {
                tracing::warn!(error = %stderr.trim(), "codex mcp add failed");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "codex not available for MCP config");
        }
    }

    Ok(())
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
