//! Generate MCP server configuration for each backend.

use anyhow::Result;
use serde_json::json;
use std::path::Path;
use tracing::{info, warn};

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

/// Upsert mcpServers.agend-terminal in a JSON file.
fn upsert_mcp_servers(path: &Path) -> Result<()> {
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if let Some(servers) = config.get("mcpServers") {
        if servers.get("agend-terminal").is_some() {
            return Ok(());
        }
    }

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = mcp_server_entry();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&config)?)?;
    info!("Configured MCP: {}", path.display());
    Ok(())
}

/// Claude Code: .claude/settings.json
fn configure_claude(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".claude").join("settings.json");
    upsert_mcp_servers(&path)
}

/// Kiro: .kiro/settings/mcp.json — uses { "mcpServers": { ... } } format
fn configure_kiro(working_dir: &Path) -> Result<()> {
    let path = working_dir
        .join(".kiro")
        .join("settings")
        .join("mcp.json");

    // Clean up old format: remove top-level "agend-terminal" key (pre-mcpServers era)
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&content) {
                if config.get("agend-terminal").is_some() && config.get("mcpServers").is_none() {
                    // Old format — remove top-level key, will be re-created under mcpServers
                    if let Some(obj) = config.as_object_mut() {
                        obj.remove("agend-terminal");
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
        warn!("Failed to configure MCP: {e:#}");
    }
}
