//! Generate MCP server configuration for each backend.

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

/// MCP server entry that proxies through the daemon API socket.
///
/// The MCP process still runs (stdio is required by Claude Code), but tool calls
/// are forwarded to the daemon via `mcp_tool` API — the heavy work (Telegram,
/// tokio runtime, registry access) happens in the shared daemon process.
/// Falls back to local handling when no daemon is running.
#[allow(dead_code)]
pub fn mcp_server_entry_pooled() -> serde_json::Value {
    // The entry is identical to the non-pooled version because pooling is
    // transparent: the `agend-terminal mcp` process auto-detects a running
    // daemon and proxies tool calls to it. No config change needed.
    mcp_server_entry()
}

/// Upsert mcpServers.agend-terminal in a JSON file.
fn upsert_mcp_servers(path: &Path) -> Result<()> {
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Always update — ensures env vars and binary path are current
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

/// Claude Code: mcp-config.json + claude-settings.json (statusline for session ID capture)
fn configure_claude(working_dir: &Path) -> Result<()> {
    // Ensure working dir is a git repo (Claude Code needs git root to find .claude/)
    let git_dir = working_dir.join(".git");
    if !git_dir.exists() {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(working_dir)
            .output()
            .ok();
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
    let script = format!(
        "#!/bin/bash\ncat > {}\necho ok\n",
        statusline_path.display()
    );
    std::fs::write(&script_path, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write claude-settings.json with statusLine config (for --settings flag)
    let settings_path = working_dir.join("claude-settings.json");
    let settings = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": script_path.display().to_string()
        }
    });
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    eprintln!("[info] Claude settings: {}", settings_path.display());

    Ok(())
}

/// Kiro: .kiro/settings/mcp.json — uses { "mcpServers": { ... } } format
fn configure_kiro(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".kiro").join("settings").join("mcp.json");

    // Clean up old format: always remove top-level "agend-terminal" key
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

    upsert_mcp_servers(&path)
}

/// Gemini: .gemini/settings.json
fn configure_gemini(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".gemini").join("settings.json");
    upsert_mcp_servers(&path)
}

/// OpenCode: opencode.json — uses { "mcpServers": { ... } } format
fn configure_opencode(working_dir: &Path) -> Result<()> {
    let path = working_dir.join("opencode.json");
    upsert_mcp_servers(&path)
}

/// Detect backend from command name and configure MCP.
pub fn configure(working_dir: &Path, command: &str) {
    let cmd = command.to_lowercase();
    let result = if cmd.contains("claude") {
        configure_claude(working_dir)
    } else if cmd.contains("kiro") {
        configure_kiro(working_dir)
    } else if cmd.contains("gemini") {
        configure_gemini(working_dir)
    } else if cmd.contains("opencode") {
        configure_opencode(working_dir)
    } else if cmd.contains("codex") {
        // Codex uses MCP via CLI: `codex mcp add` — can't auto-configure from file
        return;
    } else {
        return;
    };

    if let Err(e) = result {
        eprintln!("[warn] Failed to configure MCP: {e:#}");
    }
}
