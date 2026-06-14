//! Repro test (panic_io_extra scope) for: decision_timeout persist failure
//! swallowed -> duplicate timeout notification every tick.
//!
//! In `scan_and_emit`, after flipping `current.status` to "timeout", the code
//! does `let _ = write_decision(home, &current); Some(current)` — discarding the
//! persist result but still emitting the timeout event. If the persist fails the
//! on-disk status stays "pending", so the NEXT scan re-times-out and re-emits ->
//! unbounded duplicate notifications. The sibling path (`mark_resolved_for_sender`)
//! correctly gates on `if write_decision(...)`.
//!
//! We force the persist to fail deterministically by making the
//! `pending-decisions` directory read-only AFTER pre-creating the decision's
//! `.lock` file: `acquire_file_lock` can still open the existing lock and
//! `read_to_string` still works, but `write_decision` -> `atomic_write` cannot
//! create its temp file in a read-only dir, so the persist fails. The CORRECT
//! behavior is to NOT emit when the persist failed (the timeout was never
//! durably recorded), so the inbox must contain zero `decision_timeout` events.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// unix-only: forces the persist to fail via a read-only dir (PermissionsExt::from_mode).
// The bug is platform-independent; verified on the unix CI runners (macOS + ubuntu).
#![cfg(unix)]

use super::{next_decision_id, pending_dir, pending_path, PendingDecision, SCHEMA_VERSION};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-decision-timeout-pio-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).expect("create tmp home");
    dir
}

fn set_dir_perms(dir: &Path, mode: u32) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
        .expect("set dir permissions");
}

/// RED now: with the persist forced to fail, the buggy code still emits, so the
/// 'general' inbox receives a `decision_timeout` event -> the `== 0` assert
/// fails. GREEN after fix: gating the emit on a successful persist means a
/// failed persist emits nothing -> zero events.
#[test]
#[serial]
#[ignore = "decision_timeout-persist-swallowed: red until fix; remove #[ignore] after fix to confirm"]
fn timeout_emit_gated_on_successful_persist_panic_io_extra() {
    // Default recipient resolution -> "general" when neither fleet config nor the
    // env override is set. Clear the env override for determinism.
    std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");

    let home = tmp_home("persist-fail");
    let dir = pending_dir(&home);
    std::fs::create_dir_all(&dir).expect("create pending dir");

    // Back-dated pending decision: issued 2000s ago, timeout 1800s -> timed out.
    let id = next_decision_id();
    let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
    let payload = PendingDecision {
        schema_version: SCHEMA_VERSION,
        decision_id: id.clone(),
        sender: "general".to_string(),
        default_action: "proceed".to_string(),
        timeout_secs: 1800,
        issued_at: issued.to_rfc3339(),
        status: "pending".to_string(),
    };
    let body = serde_json::to_string_pretty(&payload).expect("serialize pending");
    std::fs::write(pending_path(&home, &id), body).expect("write pending json");

    // Pre-create the lock file so `acquire_file_lock` can OPEN it for write even
    // after the directory is made read-only (opening an existing file for write
    // does not require write access to the directory).
    let lock_path = dir.join(format!("{id}.lock"));
    std::fs::write(&lock_path, b"").expect("pre-create lock file");

    // Make the pending-decisions directory read-only: reads + lock-open still
    // work, but `atomic_write`'s temp-file creation (a NEW file) fails -> the
    // status flip cannot be persisted.
    set_dir_perms(&dir, 0o555);

    // Drive the scan. With the persist failing, correct behavior emits nothing.
    super::scan_and_emit(&home);

    // Restore permissions before draining / cleanup.
    set_dir_perms(&dir, 0o755);

    let inbox = crate::inbox::drain(&home, "general");
    let timeout_events = inbox
        .iter()
        .filter(|m| {
            m.kind.as_deref() == Some("decision_timeout")
                && m.correlation_id.as_deref() == Some(id.as_str())
        })
        .count();

    assert_eq!(
        timeout_events, 0,
        "timeout event must NOT be emitted when the status flip could not be persisted \
         (persist failed because pending-decisions was read-only); emitting anyway leaves \
         disk 'pending' and re-fires every tick. inbox: {inbox:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}
