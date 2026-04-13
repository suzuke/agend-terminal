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

const AGEND_MARKER_START: &str = "<!-- agend-terminal instructions";
const AGEND_MARKER_END: &str = "<!-- /agend-terminal -->";

/// Check if file has current version of instructions.
fn is_current(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    std::fs::read_to_string(path)
        .map(|c| c.contains(INSTRUCTIONS_VERSION))
        .unwrap_or(false)
}

/// Write instructions to a dedicated agend file (create dirs, overwrite if outdated).
/// Used for files that agend-terminal owns entirely (.claude/rules/agend.md, .kiro/steering/agend.md).
fn write_file(path: &Path, content: &str) -> Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Wrap in markers so is_current() works
    let wrapped = format!("{content}\n{AGEND_MARKER_END}\n");
    std::fs::write(path, wrapped)?;
    eprintln!("[info] Generated instructions: {}", path.display());
    Ok(())
}

/// Insert/replace agend instructions block in a shared file (AGENTS.md, GEMINI.md).
/// Uses start + end markers to only replace the agend section.
/// User content before and after the block is preserved.
fn write_with_marker(path: &Path, content: &str) -> Result<()> {
    if is_current(path) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let new_content = if let Some(start) = existing.find(AGEND_MARKER_START) {
        // Find end marker after start
        let end = existing[start..]
            .find(AGEND_MARKER_END)
            .map(|e| start + e + AGEND_MARKER_END.len())
            .unwrap_or(existing.len()); // no end marker = old format, replace to EOF
                                        // Preserve content before and after the block
        let before = existing[..start].trim_end();
        let after = existing[end..].trim_start();
        let mut result = String::new();
        if !before.is_empty() {
            result.push_str(before);
            result.push_str("\n\n");
        }
        result.push_str(content);
        result.push('\n');
        result.push_str(AGEND_MARKER_END);
        if !after.is_empty() {
            result.push_str("\n\n");
            result.push_str(after);
        }
        result.push('\n');
        result
    } else {
        // No existing block — append
        let mut result = existing;
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(content);
        result.push('\n');
        result.push_str(AGEND_MARKER_END);
        result.push('\n');
        result
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

const OPENCODE_ONESHOT: &str = r#"

## Example Conversation

User message arrives:
```
[user:alice via telegram] What's the weather like?
```

Your response — you MUST run this shell command with your FULL answer as the text:
```bash
agend-terminal agent reply "I don't have access to weather data, but you can check weather.com for the latest forecast!"
```

Another agent messages you:
```
[from:dev] Can you review my PR?
```

Your response:
```bash
agend-terminal agent send dev "Sure, send me the PR link and I'll take a look."
```

IMPORTANT: Always put your COMPLETE response inside the agend-terminal command. Do NOT reply as plain text.
"#;

/// OpenCode: AGENTS.md (auto-discovered by opencode)
fn generate_opencode(working_dir: &Path) -> Result<()> {
    let content = format!("{AGEND_RULES}{OPENCODE_ONESHOT}");
    write_with_marker(&working_dir.join("AGENTS.md"), &content)
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
    fn write_with_marker_creates_new_file() {
        let dir = tmp_dir("new_file");
        let path = dir.join("AGENTS.md");
        write_with_marker(&path, "# Test\n<!-- agend-terminal instructions v4-cli -->")
            .expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("# Test"));
        assert!(content.contains(AGEND_MARKER_END));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_with_marker_preserves_user_content_before() {
        let dir = tmp_dir("before");
        let path = dir.join("AGENTS.md");
        std::fs::write(&path, "# My Custom Rules\n\nDo not delete files.\n").ok();
        write_with_marker(&path, "# Test\n<!-- agend-terminal instructions v4-cli -->")
            .expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("# My Custom Rules"));
        assert!(content.contains("Do not delete files."));
        assert!(content.contains("v4-cli"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_with_marker_preserves_user_content_after() {
        let dir = tmp_dir("after");
        let path = dir.join("AGENTS.md");
        // Simulate: user content, then agend block, then more user content
        let initial = format!(
            "# Preamble\n\n# AgEnD\n<!-- agend-terminal instructions v3-mcp -->\nold stuff\n{}\n\n# My Notes\nKeep this.\n",
            AGEND_MARKER_END
        );
        std::fs::write(&path, &initial).ok();
        write_with_marker(&path, "# New\n<!-- agend-terminal instructions v4-cli -->")
            .expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("# Preamble"));
        assert!(content.contains("v4-cli"));
        assert!(!content.contains("v3-mcp"));
        assert!(!content.contains("old stuff"));
        assert!(content.contains("# My Notes"));
        assert!(content.contains("Keep this."));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_with_marker_replaces_old_block() {
        let dir = tmp_dir("replace");
        let path = dir.join("AGENTS.md");
        write_with_marker(&path, "# Old\n<!-- agend-terminal instructions v3-mcp -->")
            .expect("first write");
        // Force re-write by bumping version
        write_with_marker(&path, "# New\n<!-- agend-terminal instructions v4-cli -->")
            .expect("second write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("v4-cli"));
        assert!(!content.contains("v3-mcp"));
        // Should have exactly one end marker
        assert_eq!(
            content.matches(AGEND_MARKER_END).count(),
            1,
            "should have exactly one end marker"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_with_marker_idempotent() {
        let dir = tmp_dir("idempotent");
        let path = dir.join("AGENTS.md");
        let content = format!("# Test\n<!-- agend-terminal instructions v4-cli -->\nstuff\n");
        write_with_marker(&path, &content).expect("first");
        let first = std::fs::read_to_string(&path).expect("read");
        write_with_marker(&path, &content).expect("second");
        let second = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            first, second,
            "idempotent: second write should not change file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_with_marker_handles_old_format_no_end_marker() {
        let dir = tmp_dir("old_fmt");
        let path = dir.join("AGENTS.md");
        // Old format: start marker but no end marker (pre-v4)
        std::fs::write(
            &path,
            "# User stuff\n\n<!-- agend-terminal instructions v3-mcp -->\nold agend content\n",
        )
        .ok();
        write_with_marker(&path, "# New\n<!-- agend-terminal instructions v4-cli -->")
            .expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("# User stuff"));
        assert!(content.contains("v4-cli"));
        assert!(!content.contains("old agend content"));
        assert!(content.contains(AGEND_MARKER_END));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_file_creates_with_end_marker() {
        let dir = tmp_dir("wf");
        let path = dir.join("test.md");
        write_file(
            &path,
            "# Rules\n<!-- agend-terminal instructions v4-cli -->",
        )
        .expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains(AGEND_MARKER_END));
        std::fs::remove_dir_all(&dir).ok();
    }
}
