//! #1682: metadata-path resolver invariant (best-effort lint).
//!
//! Per-instance metadata file paths must be constructed ONLY inside
//! `agent_ops` (via `metadata_path_resolved` / `save_metadata` /
//! `save_metadata_batch` / `metadata_path_for_id`). A hand-coded
//! `home.join("metadata").join(<file>)` writes/reads the legacy NAME file while
//! the resolver routes everyone else to the id file `<uuid>.json` — the two
//! split, which is the #1680/#1682 stale-metadata bug (operator keystrokes
//! frozen for days; the draft gate force-submitting drafts; delete audits going
//! blind to `<uuid>.json` residuals).
//!
//! ⚠ This is a BEST-EFFORT lint, NOT an AST-level guarantee. It flags the
//! two-join file construction `…join("metadata").join(…)` in any single- OR
//! multi-line form, with the filename built by `format!`, a literal, or an
//! intermediate variable. It CANNOT catch every bypass — e.g. an intermediate
//! *directory* variable (`let d = home.join("metadata"); d.join(format!(…))`) or
//! a fully dynamic `PathBuf` assembled elsewhere. We deliberately do NOT build an
//! AST checker for this (over-engineering); the lint catches the realistic
//! hand-coded shapes (including the ones that caused #1682) and the resolver
//! helpers are the documented path. New bypass shapes seen in review should be
//! added here.

use std::path::PathBuf;

/// Only `agent_ops` owns metadata-path construction (it IS the resolver).
const ALLOWED: &[&str] = &["src/agent_ops.rs"];

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
                // Inline `#[cfg(test)] mod tests` is conventionally at the file
                // end and fabricates fixtures directly; scan only production code.
                let prod = match content.find("#[cfg(test)]") {
                    Some(i) => &content[..i],
                    None => &content,
                };
                // Whitespace-strip so the needle matches across line breaks and
                // regardless of indentation (catches the multi-line chain form).
                let squashed: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
                if squashed.contains(r#".join("metadata").join("#) {
                    // Best-effort line hint: where the metadata join sits.
                    let hint = prod
                        .lines()
                        .enumerate()
                        .find(|(_, l)| l.contains(r#".join("metadata")"#))
                        .map(|(n, l)| format!("{}:{}: {}", rel, n + 1, l.trim()))
                        .unwrap_or_else(|| format!("{rel}: (cross-line metadata path join)"));
                    violations.push(hint);
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1682: hand-coded metadata file paths must go through the agent_ops \
         resolver (metadata_path_resolved / save_metadata / metadata_path_for_id), \
         else the write/read/audit splits from the id-resolved file:\n{}",
        violations.join("\n")
    );
}
