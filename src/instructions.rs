use anyhow::Result;
use std::path::Path;
use tracing::{info, warn};

const AGEND_RULES: &str = r#"# AgEnD Terminal Communication

## How to respond to messages

When you see messages like `[user:NAME via telegram]` or `[from:INSTANCE]`, respond using Bash:

```bash
# Reply to the user who messaged you
agend-terminal reply "Your response here"

# Send a message to another agent instance
agend-terminal send TARGET "Message text"

# Check for pending messages
agend-terminal inbox
```

## Rules

- **Always use `agend-terminal reply`** to respond to `[user:... via telegram]` messages. Do NOT use the `reply` MCP tool.
- **Always use `agend-terminal send`** to communicate with other instances. Do NOT use `send_to_instance` MCP tool.
- Messages appear in your terminal as `[user:NAME via telegram] text` or `[from:INSTANCE] text`.
- For long messages, run `agend-terminal inbox` to see the full content.
- Keep replies concise and direct.
- To create a new agent instance dynamically:
  ```bash
  agend-terminal create-instance --name NAME --command CMD --working-directory /path [--topic-name "Topic"]
  ```
"#;

const AGEND_MARKER: &str = "<!-- agend-terminal instructions -->";

/// Check if file already contains agend instructions.
fn has_instructions(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    std::fs::read_to_string(path)
        .map(|c| c.contains("agend-terminal reply") || c.contains(AGEND_MARKER))
        .unwrap_or(false)
}

/// Write instructions to a new file (create dirs as needed).
fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    info!("Generated instructions: {}", path.display());
    Ok(())
}

/// Append instructions to an existing file with a marker.
fn append_with_marker(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let new_content = format!("{existing}\n{AGEND_MARKER}\n{content}");
    std::fs::write(path, new_content)?;
    info!("Appended instructions: {}", path.display());
    Ok(())
}

/// Claude Code: .claude/rules/agend.md
fn generate_claude(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".claude").join("rules").join("agend.md");
    if has_instructions(&path) {
        return Ok(());
    }
    write_file(&path, AGEND_RULES)
}

/// Kiro: .kiro/steering/agend.md
fn generate_kiro(working_dir: &Path) -> Result<()> {
    let path = working_dir.join(".kiro").join("steering").join("agend.md");
    if has_instructions(&path) {
        return Ok(());
    }
    write_file(&path, AGEND_RULES)
}

/// Codex: AGENTS.md (append with marker)
fn generate_codex(working_dir: &Path) -> Result<()> {
    let path = working_dir.join("AGENTS.md");
    if has_instructions(&path) {
        return Ok(());
    }
    append_with_marker(&path, AGEND_RULES)
}

/// Gemini: GEMINI.md (append with marker)
fn generate_gemini(working_dir: &Path) -> Result<()> {
    let path = working_dir.join("GEMINI.md");
    if has_instructions(&path) {
        return Ok(());
    }
    append_with_marker(&path, AGEND_RULES)
}

/// OpenCode: opencode.json instructions array + instructions/agend.md
fn generate_opencode(working_dir: &Path) -> Result<()> {
    let instructions_path = working_dir.join("instructions").join("agend.md");
    if has_instructions(&instructions_path) {
        return Ok(());
    }
    write_file(&instructions_path, AGEND_RULES)?;

    // Add to opencode.json instructions array if it exists
    let json_path = working_dir.join("opencode.json");
    if json_path.exists() {
        let content = std::fs::read_to_string(&json_path)?;
        if !content.contains("instructions/agend.md") {
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(arr) = val.get_mut("instructions").and_then(|v| v.as_array_mut()) {
                    arr.push(serde_json::Value::String("instructions/agend.md".to_string()));
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
        return; // Unknown backend, skip
    };

    if let Err(e) = result {
        warn!("Failed to generate instructions: {e:#}");
    }
}
