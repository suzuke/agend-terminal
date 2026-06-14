//! Repro for: "Hook-state-poc upsert silently discards a corrupt settings file
//! with no backup" (src/mcp_config.rs `upsert_state_hooks`).
//!
//! `upsert_state_hooks` reads + parses the SAME shared file as
//! `upsert_mcp_servers` (`.claude/settings.local.json`) but on parse failure
//! uses `unwrap_or(json!({}))` — silently discarding the operator's entire
//! settings file and then `atomic_write`ing a fresh document over it, with NO
//! backup at all (worse than the upsert_mcp_servers path which at least attempts
//! a copy). If the file is unreadable/corrupt, ALL the operator's other settings
//! and permissions in that shared file are lost without a trace.
//!
//! METHOD: behavioral_fs. Seed a CORRUPT settings.local.json, call
//! `upsert_state_hooks` directly (it's a private sibling, reachable from this
//! in-module submodule via `super::`), then assert the original corrupt content
//! was PRESERVED to a sibling backup file before the fresh document replaced it.
//!
//! RED now: no backup is created (the corrupt content is silently discarded), so
//! the "a backup of the original exists" assertion fails. GREEN after fix: the
//! corrupt original is backed up (rename/copy as in store.rs) before falling back
//! to an empty object, OR the call returns Err and leaves the file untouched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agend-hookpoc-corrupt-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).expect("create tmp dir");
    p
}

#[test]
#[ignore = "hookpoc-corrupt-silent-discard: red until fix; remove #[ignore] after fix to confirm"]
fn upsert_state_hooks_backs_up_corrupt_settings_before_discard_bootstrap_config_cli() {
    let dir = tmp_dir("discard");
    let claude = dir.join(".claude");
    std::fs::create_dir_all(&claude).expect("mkdir .claude");
    let path = claude.join("settings.local.json");

    // Operator's shared settings file — but corrupt/unparseable. The content is
    // load-bearing user data (permissions + other keys) that must survive a
    // failed parse, exactly like store.rs's corrupt-store contract.
    let original = "{ \"permissions\": { \"allow\": [\"Bash(ls *)\"] }  THIS IS NOT VALID JSON";
    std::fs::write(&path, original).expect("seed corrupt settings");

    // Drive the production entry point directly.
    let result = super::upsert_state_hooks(&path, "agent-x");

    // Two acceptable fixed behaviours:
    //  (a) return Err and leave the original file byte-for-byte untouched, OR
    //  (b) back up the original to a sibling before writing the fresh document.
    // Either preserves the operator's data. The current code does NEITHER:
    // it silently overwrites with a fresh document and keeps no backup.
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let file_untouched = result.is_err() && current == original;

    // A backup sibling preserving the corrupt original (e.g. `*.corrupt.*`,
    // mirroring store.rs / the upsert_mcp_servers `.with_extension("corrupt.…")`
    // convention). We accept ANY sibling whose bytes equal the original content.
    let mut backup_found = false;
    if let Ok(entries) = std::fs::read_dir(&claude) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p == path {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&p) {
                if body == original {
                    backup_found = true;
                    break;
                }
            }
        }
    }

    assert!(
        file_untouched || backup_found,
        "upsert_state_hooks silently discarded the operator's corrupt settings file \
         with no backup and no error: the original content survives neither on the \
         live path (result.is_err()={}) nor in any sibling backup. Back up the \
         original (rename/copy as in store.rs) before falling back to an empty \
         object, or return Err and leave the file untouched.",
        result.is_err()
    );

    std::fs::remove_dir_all(&dir).ok();
}
