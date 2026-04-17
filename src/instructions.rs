use anyhow::Result;
use std::path::Path;

/// Migrate any Claude instructions file left by older agend versions at the
/// former path `.claude/rules/agend.md`. We now pass instructions explicitly
/// via `--append-system-prompt-file .claude/agend.md`, so keeping the old file
/// around would cause Claude to auto-load stale content as a rule on top of
/// the flag-provided version.
fn migrate_claude_old_rules_file(working_dir: &Path) {
    let old = working_dir.join(".claude").join("rules").join("agend.md");
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
}

/// Claude Code: statusline for session ID capture
fn generate_claude(working_dir: &Path) -> Result<()> {
    let statusline_path = working_dir.join("statusline.json");
    let script_ext = if cfg!(windows) { "cmd" } else { "sh" };
    let script_path = working_dir.join(format!("statusline.{script_ext}"));
    if !script_path.exists() {
        let script = if cfg!(windows) {
            let escaped = statusline_path.display().to_string().replace('"', "\"\"");
            format!("@echo off\r\nfindstr \"^\" > \"{escaped}\"\r\necho ok\r\n")
        } else {
            let escaped = statusline_path.display().to_string().replace('\'', "'\\''");
            format!("#!/bin/bash\ncat > '{escaped}'\necho ok\n")
        };
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
    let home = crate::user_home_dir();
    let config_path = home.join(".codex").join("config.toml");
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

/// Minimal .gitignore written on fresh git init: lists agend runtime artifacts
/// that are per-session state rather than source-controlled content.
const AGEND_GITIGNORE: &str = "\
# agend-managed runtime artifacts
statusline.json
statusline.sh
statusline.cmd
mcp-config.json
claude-settings.json
.claude/settings.local.json
";

/// Ensure `dir` is a git repo so Gemini/Codex scope their project-root search
/// here instead of walking up to `$HOME`. No-op if `dir` already lives inside
/// a git work tree (we never create nested repos). On a fresh init, also drops
/// a minimal `.gitignore` for agend runtime artifacts.
pub(crate) fn ensure_project_root(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let inside = std::process::Command::new("git")
        .args([
            "-C",
            &dir.display().to_string(),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if inside {
        return;
    }

    let init_ok = std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "init", "--quiet"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if init_ok {
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            let _ = std::fs::write(&ignore_path, AGEND_GITIGNORE);
        }
    }
}

/// Markers for agend-owned block inside user-shared instructions files
/// (e.g. AGENTS.md, GEMINI.md). Content between the markers is rewritten on
/// each spawn; anything outside is preserved.
const AGEND_BLOCK_START: &str = "<!-- agend:start -->";
const AGEND_BLOCK_END: &str = "<!-- agend:end -->";

/// Build the markdown content that describes the agent's identity and fleet.
pub(crate) fn build_instructions_body(ctx: Option<&AgentContext>) -> String {
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
    content
        .push_str("Always reply to messages using `send_to_instance`, NOT direct text output.\n");
    content.push_str("Check your `inbox` periodically for pending messages.\n");
    content
}

/// Merge an agend-owned block into a user-shared file, preserving all user
/// content outside the `<!-- agend:start --> ... <!-- agend:end -->` markers.
/// Creates the file if missing; replaces the existing block in place if present;
/// otherwise appends the block at the end.
pub(crate) fn merge_agend_block(existing: &str, body: &str) -> String {
    let block = format!("{AGEND_BLOCK_START}\n{body}\n{AGEND_BLOCK_END}\n");

    if let (Some(start), Some(end)) = (
        existing.find(AGEND_BLOCK_START),
        existing.find(AGEND_BLOCK_END),
    ) {
        if end > start {
            let tail = end + AGEND_BLOCK_END.len();
            // Swallow a single trailing newline so repeated merges don't accumulate blanks.
            let tail = tail + usize::from(existing.as_bytes().get(tail) == Some(&b'\n'));
            return format!("{}{block}{}", &existing[..start], &existing[tail..]);
        }
    }

    if existing.is_empty() {
        return block;
    }
    let sep = if existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{existing}{sep}{block}")
}

/// Write agent instructions file to the backend-specific path.
/// Shared files (AGENTS.md, GEMINI.md) use marker-merge; agend-owned files
/// (.claude/agend.md, .kiro/steering/agend.md) are rewritten in full.
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

    let body = build_instructions_body(ctx);

    let final_content = if preset.instructions_shared {
        let existing = std::fs::read_to_string(&instr_path).unwrap_or_default();
        merge_agend_block(&existing, &body)
    } else {
        body
    };

    let _ = std::fs::write(&instr_path, &final_content);
}

/// Generate MCP config + backend-specific files for the working directory.
/// Generate MCP config + backend-specific files + agent instructions.
pub fn generate(working_dir: &Path, command: &str) {
    generate_with_context(working_dir, command, None);
}

/// Generate with fleet context (name, role, peers).
pub fn generate_with_context(working_dir: &Path, command: &str, ctx: Option<&AgentContext>) {
    let backend = crate::backend::Backend::from_command(command);

    // Scope Gemini/Codex project-root discovery to this dir so the hierarchical
    // GEMINI.md / AGENTS.md search doesn't walk up into the user's $HOME.
    if backend.is_some() {
        ensure_project_root(working_dir);
    }

    // Backend-specific setup (non-MCP)
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => {
            migrate_claude_old_rules_file(working_dir);
            generate_claude(working_dir)
        }
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
#[allow(clippy::unwrap_used)]
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
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        let script = dir.join(format!("statusline.{ext}"));
        assert!(script.exists(), "missing {}", script.display());
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
        let path = dir.join(".claude").join("agend.md");
        assert!(path.exists(), "missing agend.md at {}", path.display());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("v3-mcp"), "missing v3-mcp");
        assert!(content.contains("reply"), "missing reply reference");
        assert!(
            content.contains("send_to_instance"),
            "missing send_to_instance"
        );
        assert!(content.contains("inbox"), "missing inbox");
        assert!(content.contains("dev"), "missing agent name");
        assert!(content.contains("developer"), "missing role");
        assert!(content.contains("reviewer"), "missing peer");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claude_migration_removes_stale_rules_file() {
        let dir = tmp_dir("claude_migrate_stale");
        let stale = dir.join(".claude").join("rules").join("agend.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "# old content from pre-migration agend").unwrap();
        generate(&dir, "claude");
        assert!(
            !stale.exists(),
            "stale .claude/rules/agend.md was not removed"
        );
        assert!(
            dir.join(".claude").join("agend.md").exists(),
            "new .claude/agend.md was not written"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claude_migration_preserves_user_rules_dir_contents() {
        let dir = tmp_dir("claude_migrate_other_rules");
        let user_rule = dir.join(".claude").join("rules").join("my-rule.md");
        std::fs::create_dir_all(user_rule.parent().unwrap()).unwrap();
        std::fs::write(&user_rule, "user-owned rule").unwrap();
        generate(&dir, "claude");
        assert!(
            user_rule.exists(),
            "migration must not touch user's other .claude/rules/*.md files"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_into_empty_file_produces_just_the_block() {
        let merged = merge_agend_block("", "hello");
        assert!(merged.starts_with(AGEND_BLOCK_START));
        assert!(merged.trim_end().ends_with(AGEND_BLOCK_END));
        assert!(merged.contains("hello"));
    }

    #[test]
    fn merge_preserves_user_content_outside_markers() {
        let user = "# My project\n\nSome user notes.\n";
        let merged = merge_agend_block(user, "agend body");
        assert!(merged.starts_with("# My project"));
        assert!(merged.contains("Some user notes."));
        assert!(merged.contains("agend body"));
        assert!(merged.contains(AGEND_BLOCK_START));
    }

    #[test]
    fn merge_replaces_existing_block_in_place() {
        let first = merge_agend_block("# keep me\n", "v1 body");
        let second = merge_agend_block(&first, "v2 body");
        assert!(second.contains("# keep me"));
        assert!(second.contains("v2 body"));
        assert!(!second.contains("v1 body"));
        // Exactly one block remains
        assert_eq!(second.matches(AGEND_BLOCK_START).count(), 1);
        assert_eq!(second.matches(AGEND_BLOCK_END).count(), 1);
    }

    #[test]
    fn merge_is_idempotent_for_same_body() {
        let once = merge_agend_block("# head\n", "same body");
        let twice = merge_agend_block(&once, "same body");
        assert_eq!(once, twice);
    }

    #[test]
    fn generate_codex_does_not_clobber_user_agents_md() {
        let dir = tmp_dir("gen_codex_preserve");
        let user_content = "# Existing project AGENTS\n\nImportant user rules.\n";
        std::fs::write(dir.join("AGENTS.md"), user_content).unwrap();
        generate(&dir, "codex");
        let after = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(
            after.contains("Important user rules."),
            "user content lost: {after}"
        );
        assert!(after.contains(AGEND_BLOCK_START));
        assert!(after.contains("send_to_instance"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_gemini_does_not_clobber_user_gemini_md() {
        let dir = tmp_dir("gen_gemini_preserve");
        let user_content = "# My Gemini rules\n\nKeep me.\n";
        std::fs::write(dir.join("GEMINI.md"), user_content).unwrap();
        generate(&dir, "gemini");
        let after = std::fs::read_to_string(dir.join("GEMINI.md")).unwrap();
        assert!(after.contains("Keep me."), "user content lost: {after}");
        assert!(after.contains("send_to_instance"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_shared_file_is_idempotent_across_spawns() {
        let dir = tmp_dir("gen_shared_idempotent");
        std::fs::write(dir.join("AGENTS.md"), "# user head\n").unwrap();
        generate(&dir, "codex");
        let once = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        generate(&dir, "codex");
        let twice = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert_eq!(once, twice, "shared-file merge drifted between spawns");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_project_root_inits_fresh_dir_with_gitignore() {
        let dir = tmp_dir("ensure_root_fresh");
        ensure_project_root(&dir);
        assert!(dir.join(".git").exists(), "missing .git after init");
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(ignore.contains("statusline.json"));
        assert!(ignore.contains("mcp-config.json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_project_root_noop_when_already_inside_git() {
        let outer = tmp_dir("ensure_root_nested_outer");
        // Make outer a git repo.
        let _ = std::process::Command::new("git")
            .args(["-C", &outer.display().to_string(), "init", "--quiet"])
            .status();
        let inner = outer.join("subdir");
        std::fs::create_dir_all(&inner).unwrap();
        ensure_project_root(&inner);
        assert!(
            !inner.join(".git").exists(),
            "should not create nested .git inside an existing repo"
        );
        assert!(
            !inner.join(".gitignore").exists(),
            "should not drop .gitignore in an existing repo subdir"
        );
        std::fs::remove_dir_all(&outer).ok();
    }

    #[test]
    fn ensure_project_root_preserves_user_gitignore() {
        let dir = tmp_dir("ensure_root_user_ignore");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "my-custom-rule\n").unwrap();
        ensure_project_root(&dir);
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(
            ignore, "my-custom-rule\n",
            "pre-existing .gitignore must not be overwritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_agent_init_repo_so_gemini_stops_here() {
        let dir = tmp_dir("gen_gemini_stops_here");
        generate(&dir, "gemini");
        assert!(
            dir.join(".git").exists(),
            "working_dir should be a git repo after generate() for gemini"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_kiro_instructions_basic() {
        let dir = tmp_dir("gen_kiro_instr");
        generate(&dir, "kiro-cli");
        let path = dir.join(".kiro").join("steering").join("agend.md");
        assert!(path.exists(), "missing kiro agend.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("send_to_instance"),
            "missing communication guide"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
