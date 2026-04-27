//! Sprint 24 P0 PR1 — task event log anti-bypass invariant.
//!
//! Mirrors `legacy_outbound_path_audit.rs` (Sprint 22 P0) and
//! `spawn_rationale_audit.rs` (Sprint 21 Phase 5) — same anti-growth
//! contract: only `src/task_events.rs` may reference `task_events.jsonl`
//! or the `"task_events"` log-name string. Every other production caller
//! MUST go through `task_events::append` / `task_events::append_batch`.
//!
//! Direct file access defeats:
//! - The monotonic per-instance seq guarantee (sister appender computes
//!   seq under the same lock as the write; bypass races on seq).
//! - Replay determinism (sister appender emits canonical
//!   [`task_events::TaskEventEnvelope`] shape; ad-hoc writers may emit
//!   schema-version-less or unknown-field payloads that fail-closed
//!   subsequent replays).
//! - Forensic completeness (sister appender's snapshot embedding;
//!   bypass writers omit provenance).
//!
//! `EXEMPTED_CALLERS` is empty by intent. Adding entries requires
//! explicit dispatch scope per Sprint 21 Phase 5 anti-growth pattern.
//!
//! **Note for PR2 reviewers**: this test ships in PR1 ahead of the
//! `src/tasks.rs` migration in PR2. The migration in PR2 routes the
//! existing MCP `task` tool through `task_events::append`, which will
//! introduce file-name references in `src/tasks.rs`. PR2 must keep
//! those references constrained to constants imported from
//! `task_events` (e.g. via a public re-export) — NOT add to
//! `EXEMPTED_CALLERS`.

use std::path::{Path, PathBuf};

/// Sites permitted to reference the task_events log directly. Empty by
/// intent. Adding here requires explicit dispatch scope.
const EXEMPTED_CALLERS: &[&str] = &[];

fn rust_files_in_src() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&src, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn rel(path: &Path, root: &Path) -> String {
    // Sprint 23 P1 r2 — normalize Windows backslash to forward-slash for
    // cross-platform EXEMPTED-list / inline `ends_with` suffix-match.
    // See PR #240 r2.
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn task_events_jsonl_only_referenced_by_task_events_module() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files_in_src() {
        let rel_path = rel(&path, &src_root);
        // task_events.rs is the canonical producer.
        if rel_path.ends_with("task_events.rs") {
            continue;
        }
        if EXEMPTED_CALLERS.iter().any(|s| rel_path.ends_with(s)) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut at first `#[cfg(test)]` so test fixtures aren't checked
        // (they may emit hand-crafted envelopes for fail-closed asserts).
        let cutoff = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod = &content[..cutoff];
        for (i, line) in prod.lines().enumerate() {
            let trim = line.trim_start();
            if trim.starts_with("//") || trim.starts_with("///") || trim.starts_with("//!") {
                continue;
            }
            if line.contains("task_events.jsonl") || line.contains("\"task_events\"") {
                violations.push(format!(
                    "  {}:{}: direct reference to task_events log\n      offending line: {}",
                    rel_path,
                    i + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 24 P0 PR1 anti-bypass invariant — {} site(s) reference `task_events.jsonl` or the `\"task_events\"` log-name constant outside `src/task_events.rs`.\n\nFix: route all task-event mutations through `task_events::append` or `task_events::append_batch`. Direct file access defeats the seq-monotonicity guarantee + replay determinism contract.\n\nDo NOT add to `EXEMPTED_CALLERS` without explicit dispatch scope (the list is meant to shrink, not grow).\n\nViolations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}
