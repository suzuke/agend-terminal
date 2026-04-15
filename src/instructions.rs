use anyhow::Result;
use std::path::Path;

/// Claude Code: statusline for session ID capture
fn generate_claude(working_dir: &Path) -> Result<()> {
    let statusline_path = working_dir.join("statusline.json");
    let script_path = working_dir.join("statusline.sh");
    if !script_path.exists() {
        let escaped = statusline_path.display().to_string().replace('\'', "'\\''");
        let script = format!("#!/bin/bash\ncat > '{}'\necho ok\n", escaped);
        std::fs::write(&script_path, &script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    let settings_path = working_dir.join("claude-settings.json");
    if !settings_path.exists() {
        let settings = serde_json::json!({
            "statusLine": {
                "type": "command",
                "command": script_path.display().to_string()
            }
        });
        std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    }
    Ok(())
}

/// Codex: auto-trust working directory
fn codex_trust_directory(dir: &Path) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let config_path = std::path::PathBuf::from(home)
        .join(".codex")
        .join("config.toml");
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let dir_str = dir.display().to_string();
    let toml_key = format!("[projects.\"{dir_str}\"]");
    if content.contains(&toml_key) {
        return;
    }
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
    }
}

/// Context for generating agent instructions.
pub struct AgentContext<'a> {
    pub name: &'a str,
    pub role: Option<&'a str>,
    pub fleet_peers: &'a [(String, Option<String>)], // (name, role)
}

/// Write agent instructions file to the backend-specific path.
fn generate_agent_instructions(working_dir: &Path, command: &str, ctx: Option<&AgentContext>) {
    let backend = match crate::backend::Backend::from_command(command) {
        Some(b) => b,
        None => return,
    };
    let preset = backend.preset();
    let instr_path = working_dir.join(preset.instructions_path);

    if let Some(parent) = instr_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut content = String::new();
    content.push_str("# AgEnD — Multi-Agent Coordination\n\n");
    content.push_str("You are managed by AgEnD (Agent Environment Daemon).\n");
    content.push_str("You have MCP tools for communicating with other agents.\n\n");

    if let Some(ctx) = ctx {
        content.push_str(&format!("## Identity\n\n- **Name**: `{}`\n", ctx.name));
        if let Some(role) = ctx.role {
            content.push_str(&format!("- **Role**: {role}\n"));
        }
        content.push('\n');

        if !ctx.fleet_peers.is_empty() {
            content.push_str("## Fleet Peers\n\n");
            for (name, role) in ctx.fleet_peers {
                if *name != ctx.name {
                    let role_str = role.as_deref().unwrap_or("(no role)");
                    content.push_str(&format!("- `{name}` — {role_str}\n"));
                }
            }
            content.push('\n');
        }
    }

    content.push_str("## Communication (v3-mcp)\n\n");
    content.push_str("Use these MCP tools to collaborate:\n\n");
    content.push_str("- `send_to_instance` — send a message to a specific agent\n");
    content.push_str("- `broadcast` — send to multiple agents\n");
    content.push_str("- `inbox` — check your inbox for unread messages\n");
    content.push_str("- `delegate_task` — assign work to another agent\n");
    content.push_str("- `report_result` — reply with task results\n");
    content.push_str("- `request_information` — ask another agent a question\n");
    content.push_str("- `list_instances` — see all running agents\n\n");
    content.push_str("Always reply to messages using `send_to_instance`, NOT direct text output.\n");
    content.push_str("Check your `inbox` periodically for pending messages.\n");

    let _ = std::fs::write(&instr_path, &content);
}

/// Generate MCP config + backend-specific files for the working directory.
/// Generate MCP config + backend-specific files + agent instructions.
pub fn generate(working_dir: &Path, command: &str) {
    generate_with_context(working_dir, command, None);
}

/// Generate with fleet context (name, role, peers).
pub fn generate_with_context(working_dir: &Path, command: &str, ctx: Option<&AgentContext>) {
    let backend = crate::backend::Backend::from_command(command);

    // Backend-specific setup (non-MCP)
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => generate_claude(working_dir),
        Some(crate::backend::Backend::Codex) => {
            codex_trust_directory(working_dir);
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to generate backend config");
    }

    // MCP config for all backends
    crate::mcp_config::configure(working_dir, command);

    // Agent instructions (identity, role, communication guide)
    generate_agent_instructions(working_dir, command, ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-instr-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn generate_claude_creates_statusline() {
        let dir = tmp_dir("gen_claude");
        generate(&dir, "claude");
        assert!(dir.join("statusline.sh").exists(), "missing statusline.sh");
        assert!(
            dir.join("claude-settings.json").exists(),
            "missing claude-settings.json"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_codex_trusts_directory() {
        let dir = tmp_dir("gen_codex");
        generate(&dir, "codex");
        let home = std::env::var("HOME").unwrap_or_default();
        let codex_config = std::path::PathBuf::from(&home).join(".codex/config.toml");
        if codex_config.exists() {
            let toml = std::fs::read_to_string(&codex_config).unwrap();
            assert!(
                toml.contains(&dir.display().to_string()),
                "codex trust missing for {}",
                dir.display()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_unknown_backend_no_crash() {
        let dir = tmp_dir("gen_unknown");
        generate(&dir, "unknown-tool");
        assert!(std::fs::read_dir(&dir).unwrap().count() == 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_claude_instructions_with_context() {
        let dir = tmp_dir("gen_claude_ctx");
        let peers = vec![
            ("dev".to_string(), Some("developer".to_string())),
            ("reviewer".to_string(), Some("code reviewer".to_string())),
        ];
        let ctx = AgentContext {
            name: "dev",
            role: Some("developer"),
            fleet_peers: &peers,
        };
        generate_with_context(&dir, "claude", Some(&ctx));
        let path = dir.join(".claude").join("rules").join("agend.md");
        assert!(path.exists(), "missing agend.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("v3-mcp"), "missing v3-mcp");
        assert!(content.contains("reply"), "missing reply reference");
        assert!(content.contains("send_to_instance"), "missing send_to_instance");
        assert!(content.contains("inbox"), "missing inbox");
        assert!(content.contains("dev"), "missing agent name");
        assert!(content.contains("developer"), "missing role");
        assert!(content.contains("reviewer"), "missing peer");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_kiro_instructions_basic() {
        let dir = tmp_dir("gen_kiro_instr");
        generate(&dir, "kiro-cli");
        let path = dir.join(".kiro").join("steering").join("agend.md");
        assert!(path.exists(), "missing kiro agend.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("send_to_instance"), "missing communication guide");
        std::fs::remove_dir_all(&dir).ok();
    }
}
