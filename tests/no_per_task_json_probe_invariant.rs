//! #1608b / #1614 regression guard: NO code may probe a per-task
//! `home/tasks/<id>.json` file for task state.
//!
//! The AgEnD task board is **event-sourced** — state lives in
//! `<home>/task_events.jsonl` and is read only via `task_events::replay`
//! (`tasks::load_by_id`). No production code ever WRITES a per-task
//! `tasks/<id>.json` file, so any code that builds + reads that path is reading
//! a file that never exists — an always-fail probe. This was the root cause of
//! #1600/#1608/#1614 (`until_success` schedules self-disabling, the dispatch-idle
//! live-check dying, and two fiction tests that seeded the phantom file). Once
//! fixed, the whole class must stay closed: this test fails if the
//! `("tasks").join(format!(...))` probe shape reappears anywhere under `src/`.
//!
//! If a legitimate need ever arises (it shouldn't — the board is event-sourced),
//! add `// allow-tasks-json-probe: <reason>` on the same line to opt out.

use std::path::{Path, PathBuf};

const SRC_DIR: &str = "src";
/// The forbidden path-build shape: `…join("tasks").join(format!(…))`.
const FORBIDDEN: &str = "\"tasks\").join(format!";
const ALLOW_MARKER: &str = "allow-tasks-json-probe";

fn rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    walk(root, &mut out);
    out
}

#[test]
fn no_per_task_json_filesystem_probe() {
    let mut offenders: Vec<String> = Vec::new();
    for file in rs_files(Path::new(SRC_DIR)) {
        // Skip this invariant file itself (it names the forbidden literal).
        if file.file_name().and_then(|n| n.to_str()) == Some("no_per_task_json_probe_invariant.rs")
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if line.contains(FORBIDDEN) && !line.contains(ALLOW_MARKER) {
                offenders.push(format!("{}:{}  {}", file.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "#1608b/#1614: the task board is event-sourced — read task state via \
         `tasks::load_by_id` / `task_events::replay`, NEVER a per-task \
         `home/tasks/<id>.json` file (it is never written, so the probe always \
         fails). Offending sites:\n{}",
        offenders.join("\n")
    );
}
