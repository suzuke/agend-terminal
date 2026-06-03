//! #1682: metadata-path resolver invariant.
//!
//! Per-instance metadata file paths must be constructed ONLY inside
//! `agent_ops` (via `metadata_path_resolved` / `save_metadata` /
//! `save_metadata_batch` / `metadata_path_for_id`). A hand-coded
//! `home.join("metadata").join(format!("{name}.json"))` writes/reads the legacy
//! NAME file, while the resolver routes everyone else to the id file
//! `<uuid>.json` — the two split, which is the #1680/#1682 stale-metadata bug
//! (operator keystrokes frozen for days; the draft gate force-submitting
//! drafts). Centralizing construction in the resolver keeps write and read on
//! the same file.
//!
//! The needle targets the DYNAMIC form (`...join(format!(`). Dedicated `tests.rs`
//! files are test-only (they fabricate metadata fixtures directly) and are
//! skipped, mirroring `file_size_invariant`'s `tests.rs` skip.

use std::path::PathBuf;

/// Only `agent_ops` owns metadata-path construction (it IS the resolver).
const ALLOWED: &[&str] = &["src/agent_ops.rs"];

/// The banned hand-coded construction. Anchored from `.join("metadata")` so it
/// matches regardless of the receiver (`home.join` / `h.join` / `ctx.home.join`).
const NEEDLE: &str = r#".join("metadata").join(format!("#;

#[test]
fn metadata_paths_go_through_agent_ops_resolver_1682() {
    let mut violations = Vec::new();
    let mut stack = vec![PathBuf::from("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().map(|e| e == "rs").unwrap_or(false) {
                let rel = path.to_string_lossy().replace('\\', "/");
                if ALLOWED.iter().any(|a| rel == *a) {
                    continue;
                }
                // Dedicated test files build metadata fixtures directly.
                if rel.ends_with("/tests.rs") {
                    continue;
                }
                let content = std::fs::read_to_string(&path).expect("read file");
                for (i, line) in content.lines().enumerate() {
                    if line.contains(NEEDLE) {
                        violations.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1682: hand-coded metadata file paths must go through the agent_ops \
         resolver (metadata_path_resolved / save_metadata / metadata_path_for_id), \
         else the write splits from the resolver-based read:\n{}",
        violations.join("\n")
    );
}
