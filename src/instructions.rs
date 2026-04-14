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

/// Generate MCP config + backend-specific files for the working directory.
pub fn generate(working_dir: &Path, command: &str) {
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
        eprintln!("[warn] Failed to generate backend config: {e:#}");
    }

    // MCP config for all backends
    crate::mcp_config::configure(working_dir, command);
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
}
