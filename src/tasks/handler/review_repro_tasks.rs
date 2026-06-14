//! Review-repro tests (SCOPEKEY: tasks) attached to `src/tasks/handler.rs`.
//!
//! RED against the current (buggy) code; GREEN once the cited finding is fixed.
//! `#[ignore]`d so CI stays green until the fix lands.

#![allow(clippy::expect_used)]

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

fn repro_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-tasks-repro-handler-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).expect("create temp home");
    dir
}

/// FINDING #4 (medium/security): `handle_done` deserializes the Done event's
/// `DoneSource` directly from caller-supplied `args[\"done_source\"]`. A caller
/// can forge `DoneSource::PrMerged { snapshot, .. }` — a forensic-provenance
/// variant that the design says only the daemon (which actually observed the
/// GitHub merge) may construct. The audit trail then records an
/// operator-manual close as if a real PR merge closed it.
///
/// CORRECT behavior (after fix — restrict caller-provided done_source to
/// operator-attestable variants): a caller forging PrMerged through the MCP
/// surface must NOT result in a persisted `DoneSource::PrMerged` event.
#[test]
#[ignore = "tasks-done-source-forgery: red until fix; remove #[ignore] after fix to confirm"]
fn caller_cannot_forge_pr_merge_provenance_on_done_tasks() {
    let home = repro_home("done-source-forgery");

    // Create an UNOWNED task so the ACL gate passes for any caller.
    let created = handle(
        &home,
        "operator",
        &serde_json::json!({ "action": "create", "title": "forge me" }),
    );
    let id = created["id"]
        .as_str()
        .expect("create returns task id")
        .to_string();

    // A caller forges full PR-merge provenance (including a fabricated
    // PrSnapshot) on the Done event via the raw MCP `done_source` arg.
    let forged = serde_json::json!({
        "action": "done",
        "id": id,
        "done_source": {
            "via": "PrMerged",
            "pr_id": 99999,
            "merge_sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "merged_at": "2026-06-14T00:00:00Z",
            "snapshot": {
                "pr_state": "merged",
                "merge_sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "api_response_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                "captured_at": "2026-06-14T00:00:00Z"
            }
        }
    });
    let res = handle(&home, "operator", &forged);
    assert!(
        res.get("error").is_none(),
        "the done call itself should succeed (the forgery is in the provenance), got {res}"
    );

    // Inspect the PERSISTED Done event's provenance.
    let envelopes = crate::task_events::stream_envelopes(&home).expect("stream envelopes");
    let done_source_is_pr_merged = envelopes.iter().any(|e| {
        matches!(
            &e.event,
            crate::task_events::TaskEvent::Done {
                source: crate::task_events::DoneSource::PrMerged { .. },
                ..
            }
        )
    });

    assert!(
        !done_source_is_pr_merged,
        "FINDING #4: a caller forged DoneSource::PrMerged through the MCP `done` \
         surface and it was persisted verbatim — forensic PR-merge provenance can \
         be fabricated by any agent. Forensic variants must only be constructed by \
         daemon paths that observed the GitHub state."
    );

    std::fs::remove_dir_all(&home).ok();
}

/// FINDING #8 (low/correctness): task ids are `t-<microsecond-ts>-<ID_SEQ>`
/// where ID_SEQ is a PROCESS-LOCAL AtomicU64. `tasks::handle` runs in every MCP
/// server process AND the daemon, so two processes creating a task in the same
/// microsecond both mint `t-<ts>-0` — identical ids. `apply_created` uses
/// `entry().or_insert_with()`, silently dropping the second Created at replay.
///
/// A true cross-process same-microsecond race can't be forced in-process, so
/// this STATIC-INVARIANT guard pins the root cause: the minted id must carry a
/// process-unique component (pid / uuid / random). RED now (none present);
/// GREEN once the id format is made globally unique across processes.
#[test]
#[ignore = "tasks-id-cross-process-collision: red until fix; remove #[ignore] after fix to confirm"]
fn task_id_has_process_unique_component_tasks() {
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/handler.rs");
    let text = std::fs::read_to_string(&src).expect("read handler.rs");

    // Isolate the `handle_create` body (where the id is minted).
    let body = {
        let start = text
            .find("fn handle_create(")
            .expect("handle_create exists");
        let after = &text[start..];
        let end = after.find("\nfn ").map(|e| start + e).unwrap_or(text.len());
        text[start..end].to_string()
    };

    // The id-minting line must include a process-unique disambiguator so two
    // processes minting in the same microsecond cannot collide.
    let has_process_unique = body.contains("std::process::id()")
        || body.contains("process::id()")
        || body.to_lowercase().contains("uuid")
        || body.contains("getrandom")
        || body.contains("rand::")
        || body.contains("/dev/urandom");

    assert!(
        has_process_unique,
        "FINDING #8: handle_create mints task ids as t-<ts>-<process-local ID_SEQ> \
         with NO process-unique component, so two processes in the same microsecond \
         mint identical ids and one Created is silently dropped at replay. Add a pid/\
         uuid/random component to the id."
    );
}
