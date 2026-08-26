//! Sprint 24 P0 PR1 — task event log anti-bypass invariant.
//!
//! Mirrors `legacy_outbound_path_audit.rs` (Sprint 22 P0) and
//! `spawn_rationale_audit.rs` (Sprint 21 Phase 5) — same anti-growth
//! contract: only the `src/task_events` module may reference `task_events.jsonl`
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

fn is_test_only_file(path: &Path) -> bool {
    // Conventional `foo.rs` + `foo/tests.rs` split: a file named `tests.rs` is a
    // test submodule (declared `#[cfg(test)] mod tests;` in the owning module
    // file, which lives one directory up — not a sibling, so the `#[path]`
    // heuristic below can't see it). By Rust convention `tests.rs` is never a
    // production file.
    if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
        return true;
    }
    let parent = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let path_needle = format!("\"{filename}\"");
    let Ok(siblings) = std::fs::read_dir(parent) else {
        return false;
    };
    for sibling in siblings.flatten() {
        let sp = sibling.path();
        if sp == *path || sp.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&sp) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if !(trimmed.starts_with("#[path") && trimmed.contains(&path_needle)) {
                continue;
            }
            let mut cursor = idx;
            for _ in 0..5 {
                let Some(prev) = cursor.checked_sub(1).and_then(|i| lines.get(i)) else {
                    break;
                };
                let ptrim = prev.trim();
                if ptrim.is_empty() || ptrim.starts_with("//") {
                    cursor -= 1;
                    continue;
                }
                if ptrim.starts_with("#[") && ptrim.contains("cfg") && ptrim.contains("test") {
                    return true;
                }
                if !ptrim.starts_with("#[") {
                    break;
                }
                cursor -= 1;
            }
        }
    }
    false
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
        // task_events.rs and its child modules are the canonical producer.
        if rel_path == "task_events.rs" || rel_path.starts_with("task_events/") {
            continue;
        }
        if EXEMPTED_CALLERS.iter().any(|s| rel_path.ends_with(s)) {
            continue;
        }
        if is_test_only_file(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut at the file's test module so earlier test-only items do not hide
        // production code that follows them.
        let cutoff = content.rfind("\nmod tests {").unwrap_or(content.len());
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
        "Sprint 24 P0 PR1 anti-bypass invariant — {} site(s) reference `task_events.jsonl` or the `\"task_events\"` log-name constant outside the `src/task_events` module.\n\nFix: route all task-event mutations through `task_events::append` or `task_events::append_batch`. Direct file access defeats the seq-monotonicity guarantee + replay determinism contract.\n\nDo NOT add to `EXEMPTED_CALLERS` without explicit dispatch scope (the list is meant to shrink, not grow).\n\nViolations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn task_event_appends_have_one_catalog_commit_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let facade =
        std::fs::read_to_string(root.join("src/task_events.rs")).expect("read task-events facade");
    let catalog = std::fs::read_to_string(root.join("src/task_events/catalog.rs"))
        .expect("read task catalog");
    let needle = "crate::event_log::append_lines_under_lock";

    assert_eq!(
        catalog.matches("pub(crate) fn commit_at").count(),
        1,
        "catalog::commit_at must remain the single task-event commit primitive"
    );
    assert_eq!(
        facade.matches(needle).count(),
        0,
        "task_events.rs must not write the task-event log directly"
    );
    assert_eq!(
        catalog.matches(needle).count(),
        1,
        "catalog may lock the task-event log only for its compaction rewrite"
    );
}

#[test]
fn task_authority_has_no_legacy_replay_or_index_path() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        "task_events::replay(",
        "task_events::replay_at(",
        "task_events::replay_strict_at(",
        "task_events::replay_uncached(",
        "task_events::replay_strict_scan(",
        "task_index.jsonl",
        "REPLAY_GENERATION",
    ];
    let mut violations = Vec::new();

    for path in rust_files_in_src() {
        if is_test_only_file(&path) {
            continue;
        }
        let rel_path = rel(&path, &src_root);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let cutoff = content.rfind("\nmod tests {").unwrap_or(content.len());
        for (line_no, line) in content[..cutoff].lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for needle in forbidden {
                if line.contains(needle) {
                    violations.push(format!("{rel_path}:{} contains {needle}", line_no + 1));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "task catalog cutover regressed:\n{}",
        violations.join("\n")
    );

    let task_events =
        std::fs::read_to_string(src_root.join("task_events.rs")).expect("read task-events facade");
    let tasks = std::fs::read_to_string(src_root.join("tasks/mod.rs")).expect("read task facade");
    let router = std::fs::read_to_string(src_root.join("tasks/board_router.rs"))
        .expect("read task board router");
    assert!(
        task_events.contains("catalog::for_home(&home)"),
        "compatibility-shaped task state must resolve through the catalog"
    );
    assert!(
        tasks.contains("crate::task_events::projected_state_at(board)"),
        "task list facade must resolve through the catalog view"
    );
    assert!(
        router.contains("crate::task_events::catalog::for_home(home)"),
        "task routing and strict listing must resolve through the catalog"
    );
}
