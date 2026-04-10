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

/// Claude Code: write MCP server config to .claude/settings.json
fn configure_claude(working_dir: &Path) -> Result<()> {
    let settings_dir = working_dir.join(".claude");
    std::fs::create_dir_all(&settings_dir)?;
    let settings_path = settings_dir.join("settings.json");

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Check if agend-terminal MCP server already configured
    if let Some(servers) = settings.get("mcpServers") {
        if servers.get("agend-terminal").is_some() {
            return Ok(());
        }
    }

    let bin = binary_path();
    let mcp_config = json!({
        "command": bin,
        "args": ["mcp"]
    });

    if settings.get("mcpServers").is_none() {
        settings["mcpServers"] = json!({});
    }
    settings["mcpServers"]["agend-terminal"] = mcp_config;

    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    info!("Configured MCP for Claude: {}", settings_path.display());
    Ok(())
}

/// Kiro: write .kiro/settings/mcp.json
fn configure_kiro(working_dir: &Path) -> Result<()> {
    let settings_dir = working_dir.join(".kiro").join("settings");
    std::fs::create_dir_all(&settings_dir)?;
    let mcp_path = settings_dir.join("mcp.json");

    let mut mcp_config: serde_json::Value = if mcp_path.exists() {
        let content = std::fs::read_to_string(&mcp_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if mcp_config.get("agend-terminal").is_some() {
        return Ok(());
    }

    let bin = binary_path();
    mcp_config["agend-terminal"] = json!({
        "command": bin,
        "args": ["mcp"]
    });

    std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp_config)?)?;
    info!("Configured MCP for Kiro: {}", mcp_path.display());
    Ok(())
}

/// Gemini: write .gemini/settings.json
fn configure_gemini(working_dir: &Path) -> Result<()> {
    let settings_dir = working_dir.join(".gemini");
    std::fs::create_dir_all(&settings_dir)?;
    let settings_path = settings_dir.join("settings.json");

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if let Some(servers) = settings.get("mcpServers") {
        if servers.get("agend-terminal").is_some() {
            return Ok(());
        }
    }

    let bin = binary_path();
    if settings.get("mcpServers").is_none() {
        settings["mcpServers"] = json!({});
    }
    settings["mcpServers"]["agend-terminal"] = json!({
        "command": bin,
        "args": ["mcp"]
    });

    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    info!("Configured MCP for Gemini: {}", settings_path.display());
    Ok(())
}

/// OpenCode: add to opencode.json mcpServers section
fn configure_opencode(working_dir: &Path) -> Result<()> {
    let json_path = working_dir.join("opencode.json");

    let mut config: serde_json::Value = if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if let Some(servers) = config.get("mcpServers") {
        if servers.get("agend-terminal").is_some() {
            return Ok(());
        }
    }

    let bin = binary_path();
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = json!({
        "command": bin,
        "args": ["mcp"]
    });

    std::fs::write(&json_path, serde_json::to_string_pretty(&config)?)?;
    info!("Configured MCP for OpenCode: {}", json_path.display());
    Ok(())
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
        // Fall back to instructions-based approach
        return;
    } else {
        return;
    };

    if let Err(e) = result {
        warn!("Failed to configure MCP: {e:#}");
    }
}
