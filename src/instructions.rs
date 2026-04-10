use anyhow::Result;
use std::path::Path;
use tracing::{info, warn};

const INSTRUCTIONS_VERSION: &str = "v3-mcp";

const AGEND_RULES: &str = r#"# AgEnD Terminal Communication
<!-- agend-terminal instructions v3-mcp -->

## Message Types

You will receive two types of messages:

1. **`[user:NAME via telegram] text`** — A human user sent you a message via Telegram.
   → Respond using the **`reply`** MCP tool.

2. **`[from:INSTANCE-NAME] text`** — Another agent instance sent you a message.
   → Respond using the **`send`** MCP tool with `target` set to the instance name.

## MCP Tools

| Tool | When to use |
|------|-------------|
| **reply** | Reply to `[user:... via telegram]` messages. Sends your response back to Telegram. |
| **send** | Reply to `[from:INSTANCE]` messages, or proactively message another instance. Set `target` to the instance name. |
| **inbox** | Retrieve full message content when notification says "Run: agend-terminal inbox". |
| **list_instances** | See all active agent instances. |
| **create_instance** | Spawn a new agent instance dynamically. |
| **delete_instance** | Stop and remove an agent instance. |

## Rules

- `[user:... via telegram]` → use **reply** (NOT send)
- `[from:INSTANCE]` → use **send** with target=INSTANCE (NOT reply)
- For long messages, use **inbox** to see the full content
- Keep replies concise and direct
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
    info!("Generated instructions: {}", path.display());
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
    info!("Updated instructions: {}", path.display());
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
                    info!("Updated opencode.json instructions array");
                }
            }
        }
    }
    Ok(())
}

/// Detect backend from command name and generate appropriate instructions.
pub fn generate(working_dir: &Path, command: &str) {
    let cmd = command.to_lowercase();
    let result = if cmd.contains("claude") {
        generate_claude(working_dir)
    } else if cmd.contains("kiro") {
        generate_kiro(working_dir)
    } else if cmd.contains("codex") {
        generate_codex(working_dir)
    } else if cmd.contains("gemini") {
        generate_gemini(working_dir)
    } else if cmd.contains("opencode") {
        generate_opencode(working_dir)
    } else {
        return;
    };

    if let Err(e) = result {
        warn!("Failed to generate instructions: {e:#}");
    }
}
