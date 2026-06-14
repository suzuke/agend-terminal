//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #10.
//!
//! `resolve_task_project` falls back to a full-board scan and, on a hit, calls
//! `record_task_project` to "repair" the index UNCONDITIONALLY. `task_index.jsonl`
//! is append-only and `lookup_task_project` takes the FIRST match, so if the
//! index is ever lost/truncated while boards still hold the tasks, every
//! subsequent resolve for those tasks re-appends a fresh entry — the file only
//! grows and is never de-duplicated. Hot read paths (done/update/claim/activity)
//! all hit `resolve_task_project`.
//!
//! The fix (per the finding's suggestion) is architectural: guard the repair
//! against re-appending an already-present entry, dedupe on read, and/or compact
//! the index. That guard does not yet exist, so this is an interim static guard
//! (see redesign_note in the manifest): the repair `record_task_project` call in
//! `resolve_task_project` is currently issued with no existence/dedup guard.
//! RED now (unconditional repair); GREEN once the repair is guarded/deduped.

use std::path::PathBuf;

fn read_board_router() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/board_router.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/board_router.rs")
}

/// Isolate the `resolve_task_project` body (the repair site).
fn resolve_task_project_body(text: &str) -> String {
    let start = text
        .find("fn resolve_task_project(")
        .expect("resolve_task_project exists");
    let after = &text[start..];
    let end = after[1..]
        .find("\nfn ")
        .map(|e| start + 1 + e)
        .or_else(|| after[1..].find("\npub(super) fn ").map(|e| start + 1 + e))
        .or_else(|| after[1..].find("\npub fn ").map(|e| start + 1 + e))
        .unwrap_or(text.len());
    text[start..end].to_string()
}

#[test]
#[ignore = "tasks-index-repair-unbounded-growth: red until fix; remove #[ignore] after fix to confirm"]
fn index_repair_is_guarded_against_duplicate_reappend_tasks() {
    let text = read_board_router();
    let body = resolve_task_project_body(&text);

    // The repair must be guarded: a dedup/existence check, or a compaction
    // marker, must accompany the `record_task_project` call so repeated index
    // loss can't accumulate duplicate entries unboundedly.
    //
    // NOTE: the unguarded current body already contains `contains_key(&tid)`
    // (the board membership probe) — so the dedup-guard needles below are
    // deliberately narrow and must NOT match that pre-existing probe.
    let calls_repair = body.contains("record_task_project(");
    let has_dedup_guard = body.contains("dedup")
        || body.contains("compact")
        || body.contains("already_indexed")
        || body.contains("lookup_task_project(home, task_id).is_none()")
        || body.contains("index_contains")
        || body.contains("HashSet")
        || body.contains("BTreeSet");

    assert!(
        !calls_repair || has_dedup_guard,
        "FINDING #10: resolve_task_project repairs the index by calling \
         record_task_project unconditionally on a scan hit, with no dedup/existence \
         guard. If the index is repeatedly lost/truncated, every resolve re-appends a \
         fresh entry and task_index.jsonl grows unboundedly. Guard the repair (dedupe \
         on read / compact / skip when already present)."
    );
}
