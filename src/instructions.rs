use anyhow::Result;
use std::path::Path;

const INSTRUCTIONS_VERSION: &str = "v4-cli";

const AGEND_RULES: &str = r#"# AgEnD Terminal Communication
<!-- agend-terminal instructions v4-cli -->

You are an agent managed by AgEnD Terminal. Use `agend-terminal agent` commands in bash to communicate.

## Message Types You Receive

1. **`[user:NAME via telegram] text`** — A human sent you a message.
   Reply: `agend-terminal agent reply "your response"`

2. **`[from:INSTANCE-NAME] text`** — Another agent sent you a message.
   Reply: `agend-terminal agent send INSTANCE-NAME "your response"`

3. **`[delegate_task] ...`** — You've been assigned a task.
   When done: `agend-terminal agent report REQUESTER "summary of results"`

## Quick Reference

```bash
# Communication
agend-terminal agent reply "text"                    # Reply to Telegram user
agend-terminal agent send TARGET "message"           # Message another agent
agend-terminal agent delegate TARGET "task"          # Assign work
agend-terminal agent report TARGET "summary"         # Report results
agend-terminal agent ask TARGET "question"           # Request info
agend-terminal agent broadcast "message"             # Message all agents
agend-terminal agent inbox                           # Check pending messages

# Instance Management
agend-terminal agent list                            # List running agents
agend-terminal agent spawn NAME --backend claude     # Create agent
agend-terminal agent delete NAME                     # Remove agent
agend-terminal agent describe NAME                   # Get agent details

# Task Board
agend-terminal agent task create "title"             # Create task
agend-terminal agent task list                       # List tasks
agend-terminal agent task claim ID                   # Claim task
agend-terminal agent task done ID --result "done"    # Complete task

# Teams
agend-terminal agent team create NAME m1 m2          # Create team
agend-terminal agent team list                       # List teams
```

## Rules

- All commands output JSON — parse the result for structured data
- `[user:... via telegram]` → use `agent reply` (NOT `agent send`)
- `[from:INSTANCE]` → use `agent send` (NOT `agent reply`)
- Check inbox regularly with `agent inbox`
- Run `agend-terminal agent --help` for full command list
"#;

const AGEND_MARKER: &str = "<!-- agend-terminal instructions";

/// Check if file has current version of instructions.
fn is_current(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    std::fs::read_to_string(path)
        .map(|c| c.contains(INSTRUCTIONS_VERSION))
        .unwrap_or(false)
}

/// Write instructions to a file (create dirs, overwrite if outdated).
fn write_file(path: &Path, content: &str) -> Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    eprintln!("[info] Generated instructions: {}", path.display());
    Ok(())
}

/// Append/replace instructions in an existing file with a marker.
fn write_with_marker(path: &Path, content: &str) -> Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        // Remove old agend instructions block if present
        if let Some(start) = text.find(AGEND_MARKER) {
            text[..start].trim_end().to_string()
        } else {
            text
        }
    } else {
        String::new()
    };
    let new_content = if existing.is_empty() {
        content.to_string()
    } else {
        format!("{existing}\n\n{content}")
    };
    std::fs::write(path, new_content)?;
    eprintln!("[info] Updated instructions: {}", path.display());
    Ok(())
}

/// Claude Code: .claude/rules/agend.md
fn generate_claude(working_dir: &Path) -> Result<()> {
    write_file(
        &working_dir.join(".claude").join("rules").join("agend.md"),
        AGEND_RULES,
    )
}

/// Kiro: .kiro/steering/agend.md
fn generate_kiro(working_dir: &Path) -> Result<()> {
    write_file(
        &working_dir.join(".kiro").join("steering").join("agend.md"),
        AGEND_RULES,
    )
}

/// Codex: AGENTS.md (marker append/replace)
fn generate_codex(working_dir: &Path) -> Result<()> {
    write_with_marker(&working_dir.join("AGENTS.md"), AGEND_RULES)
}

/// Gemini: GEMINI.md (marker append/replace)
fn generate_gemini(working_dir: &Path) -> Result<()> {
    write_with_marker(&working_dir.join("GEMINI.md"), AGEND_RULES)
}

/// OpenCode: instructions/agend.md
fn generate_opencode(working_dir: &Path) -> Result<()> {
    let instructions_path = working_dir.join("instructions").join("agend.md");
    write_file(&instructions_path, AGEND_RULES)?;

    // Add to opencode.json instructions array if it exists
    let json_path = working_dir.join("opencode.json");
    if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        if !content.contains("instructions/agend.md") {
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(arr) = val.get_mut("instructions").and_then(|v| v.as_array_mut()) {
                    arr.push(serde_json::Value::String(
                        "instructions/agend.md".to_string(),
                    ));
                    std::fs::write(&json_path, serde_json::to_string_pretty(&val)?)?;
                    eprintln!("[info] Updated opencode.json instructions array");
                }
            }
        }
    }
    Ok(())
}

/// Detect backend from command name and generate appropriate instructions.
pub fn generate(working_dir: &Path, command: &str) {
    let backend = crate::backend::Backend::from_command(command);
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => generate_claude(working_dir),
        Some(crate::backend::Backend::KiroCli) => generate_kiro(working_dir),
        Some(crate::backend::Backend::Codex) => generate_codex(working_dir),
        Some(crate::backend::Backend::Gemini) => generate_gemini(working_dir),
        Some(crate::backend::Backend::OpenCode) => generate_opencode(working_dir),
        None => return,
    };

    if let Err(e) = result {
        eprintln!("[warn] Failed to generate instructions: {e:#}");
    }
}

/// Remove MCP config files left by previous versions.
/// MCP has been replaced by CLI — these files cause backend warnings.
pub fn cleanup_mcp(working_dir: &Path) {
    let mcp_files = [
        // Claude
        ".claude/settings.local.json",
        "mcp-config.json",
        "claude-settings.json",
        "statusline.sh",
        // Gemini
        ".gemini/settings.json",
        // OpenCode
        "opencode.json",
        // Kiro
        ".kiro/settings/mcp.json",
        ".kiro/settings/agend-mcp-wrapper.sh",
    ];
    for file in &mcp_files {
        let path = working_dir.join(file);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Only remove if it's an agend-terminal MCP config
                if content.contains("agend-terminal") || content.contains("AGEND_HOME") {
                    let _ = std::fs::remove_file(&path);
                    eprintln!("[cleanup] removed old MCP config: {}", path.display());
                }
            }
        }
    }
    // Codex: remove from .codex/config.toml in working dir
    let codex_config = working_dir.join(".codex").join("config.toml");
    if codex_config.exists() {
        if let Ok(content) = std::fs::read_to_string(&codex_config) {
            if content.contains("[mcp_servers.agend-terminal]") {
                // Remove the MCP server section
                let cleaned: String = content
                    .lines()
                    .filter(|l| {
                        !l.contains("mcp_servers.agend-terminal")
                            && !l.starts_with("command = ")
                            && !l.starts_with("args = ")
                            && !l.starts_with("AGEND_HOME")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = std::fs::write(&codex_config, cleaned.trim_end().to_string() + "\n");
                eprintln!("[cleanup] removed MCP from {}", codex_config.display());
            }
        }
    }
}

/// Remove agend-terminal MCP server from global ~/.codex/config.toml.
pub fn cleanup_global_mcp() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let config_path = std::path::PathBuf::from(home)
        .join(".codex")
        .join("config.toml");
    if !config_path.exists() {
        return;
    }
    if let Ok(content) = std::fs::read_to_string(&config_path) {
        if !content.contains("[mcp_servers.agend-terminal]") {
            return;
        }
        // Remove the [mcp_servers.agend-terminal] section and its sub-keys
        let mut lines: Vec<&str> = Vec::new();
        let mut in_mcp_section = false;
        for line in content.lines() {
            if line.starts_with("[mcp_servers.agend-terminal") {
                in_mcp_section = true;
                continue;
            }
            if in_mcp_section {
                // End of section: next [section] header or blank line after content
                if line.starts_with('[') {
                    in_mcp_section = false;
                    lines.push(line);
                }
                // Skip lines in the MCP section
                continue;
            }
            lines.push(line);
        }
        let cleaned = lines.join("\n");
        let _ = std::fs::write(&config_path, cleaned.trim_end().to_string() + "\n");
        eprintln!("[cleanup] removed agend-terminal MCP from global codex config");
    }
}
