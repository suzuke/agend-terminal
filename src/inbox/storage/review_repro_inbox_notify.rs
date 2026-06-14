//! Verification/reproduction tests for the `inbox-notify` review batch.
//!
//! These tests encode the CORRECT expected behavior and FAIL (red) against the
//! current buggy code, so they prove the bug is caught; they PASS (green) once
//! the fix lands. Each is `#[ignore]`d so CI stays green until then — remove the
//! `#[ignore]` after the corresponding fix to confirm.
//!
//! Attached to `src/inbox/storage.rs`; private/`pub(super)`/`pub(crate)` items
//! are reached through `super::`.

use super::{
    drain, enqueue, enqueue_returning_unread_count, get_thread, inbox_path, inbox_path_resolved,
    msg_already_drained_in_jsonl, unread_count,
};
use crate::inbox::InboxMessage;
use std::fs;
use std::path::PathBuf;

/// Unique temp HOME per test (mirrors `inbox/tests.rs::tmp_home`).
fn tmp_home(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-inbox-notify-{}-{}",
        suffix,
        std::process::id()
    ));
    fs::create_dir_all(&dir).ok();
    dir
}

fn make_msg(from: &str, text: &str) -> InboxMessage {
    InboxMessage {
        schema_version: 1,
        from: from.to_string(),
        text: text.to_string(),
        timestamp: "2025-01-01T00:00:00Z".to_string(),
        ..Default::default()
    }
}

/// Write a minimal fleet.yaml so `fleet::resolve_uuid(home, name)` returns the
/// given UUID — making `name` an "id-native" instance whose inbox lives at the
/// UUID path (`inbox_path_resolved`), not the legacy `<name>.jsonl` path.
fn write_fleet_with_id(home: &std::path::Path, name: &str, uuid: &str) {
    let yaml = format!("instances:\n  {name}:\n    id: {uuid}\n");
    fs::write(crate::fleet::fleet_yaml_path(home), yaml).expect("write fleet.yaml");
}

// ───────────────────────────────────────────────────────────────────────────
// Finding #1 (HIGH): #911 JSONL dedup fallback is a permanent no-op for
// id-native instances — `msg_already_drained_in_jsonl` reads `inbox_path`
// (raw NAME path) while `drain` writes `read_at` via `inbox_path_resolved`
// (the UUID path). For an id-native instance there is no `<name>.jsonl`, so the
// fallback reads a nonexistent file and returns `false` unconditionally.
// ───────────────────────────────────────────────────────────────────────────
#[test]
fn msg_already_drained_reads_resolved_uuid_path_inbox_notify() {
    let home = tmp_home("f1-msg-drained-resolved");
    let name = "idnative";
    let uuid = "11111111-2222-4333-8444-555555555555";
    write_fleet_with_id(&home, name, uuid);

    // id-native instance: enqueue routes to the UUID inbox (inbox_path_resolved),
    // never creating `<name>.jsonl`.
    let mut msg = make_msg("system:911", "hello");
    msg.id = Some("m-already-drained-1".to_string());
    enqueue(&home, name, msg).expect("enqueue id-native");

    // drain sets `read_at` in the UUID file.
    let drained = drain(&home, name);
    assert_eq!(drained.len(), 1, "the message must drain once");

    // Sanity: the resolved (UUID) path is where the read row lives; the legacy
    // name path does NOT exist for an id-native instance.
    let resolved = inbox_path_resolved(&home, name);
    let name_path = inbox_path(&home, name);
    assert!(resolved.exists(), "resolved UUID inbox must exist");
    assert!(
        !name_path.exists(),
        "id-native instance must have no legacy <name>.jsonl: {}",
        name_path.display()
    );

    // The source-of-truth read-state fallback MUST see the drained row. It reads
    // the wrong (name) path today, so it returns false — the #911 dedup signal
    // is dead after a daemon restart (in-memory ledger gone), re-injecting an
    // already-delivered message.
    let drained_seen = msg_already_drained_in_jsonl(&home, name, "m-already-drained-1");
    assert!(
        drained_seen,
        "msg_already_drained_in_jsonl must read the same (resolved/UUID) file \
         that drain wrote read_at to; it returned false because it read the \
         nonexistent <name>.jsonl path"
    );

    fs::remove_dir_all(&home).ok();
}

// ───────────────────────────────────────────────────────────────────────────
// Finding #3 (MED): a migrated inbox is scanned twice via its symlink —
// `inbox_path_resolved` creates `<uuid>.jsonl -> <name>.jsonl` and never removes
// the name file, so `get_thread(.., None)` (and find_message/sweep) match both
// directory entries with no symlink filter / canonical dedup → every thread
// message is returned TWICE.
// ───────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "migrated-symlink-double-scan: red until fix; remove #[ignore] after fix to confirm"]
fn migrated_inbox_thread_not_double_counted_via_symlink_inbox_notify() {
    let home = tmp_home("f3-symlink-double");
    let name = "legacyagent";
    let uuid = "22222222-3333-4444-8555-666666666666";

    // Seed a LEGACY name-based inbox with a single thread message, BEFORE the
    // instance gains an id (so no UUID file exists yet).
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).expect("mk inbox dir");
    let thread_line = r#"{"schema_version":1,"id":"m-thread-1","from":"from:lead","text":"thread msg","kind":null,"timestamp":"2025-01-01T00:00:00Z","thread_id":"t-migrate"}"#;
    fs::write(inbox_path(&home, name), format!("{thread_line}\n")).expect("seed legacy name inbox");

    // Now the instance becomes id-native: trigger the migration, which creates
    // the `<uuid>.jsonl -> <name>.jsonl` symlink and LEAVES the name file.
    write_fleet_with_id(&home, name, uuid);
    let resolved = inbox_path_resolved(&home, name);
    assert!(
        resolved.exists(),
        "resolved (symlink) path must exist after migration"
    );
    assert!(
        inbox_path(&home, name).exists(),
        "legacy name file must still exist (migration does not remove it)"
    );

    // Cross-inbox scan (instance=None) walks the directory. Both `<name>.jsonl`
    // (regular file) and `<uuid>.jsonl` (symlink to it) point at the same
    // content; without a symlink filter / canonical dedup the message is counted
    // twice.
    let thread = get_thread(&home, "t-migrate", None);
    assert_eq!(
        thread.len(),
        1,
        "the single thread message must be returned ONCE; the migration symlink \
         + name file made the directory scan double-count it (got {})",
        thread.len()
    );

    fs::remove_dir_all(&home).ok();
}

// ───────────────────────────────────────────────────────────────────────────
// Finding #4 (MED): `enqueue_returning_unread_count` counts superseded+unread
// rows, inflating the pending-hint count. `unread_count` (MED-3) was fixed to
// EXCLUDE `superseded_by.is_some()` rows because `drain` silently consumes them;
// the sibling did not get the fix.
// ───────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "unread-count-superseded-drift: red until fix; remove #[ignore] after fix to confirm"]
fn enqueue_returning_unread_count_excludes_superseded_rows_inbox_notify() {
    let home = tmp_home("f4-superseded-count");
    let name = "countagent";

    // Pre-seed the (name-based; no fleet.yaml → resolved == name path) inbox with
    // a superseded-but-undrained obligation: read_at == null, superseded_by set.
    // `drain` will silently consume it and never surface it, so it is NOT
    // actionable unread.
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).expect("mk inbox dir");
    let superseded_line = r#"{"schema_version":1,"id":"m-superseded","from":"system:ci","text":"old ci-watch","kind":"ci-watch","timestamp":"2025-01-01T00:00:00Z","superseded_by":"m-newer"}"#;
    fs::write(inbox_path(&home, name), format!("{superseded_line}\n"))
        .expect("seed superseded row");

    // Append a genuinely-new unread message and read back the count it reports.
    let mut msg = make_msg("system:ci", "new ci-watch");
    msg.id = Some("m-newer".to_string());
    let reported =
        enqueue_returning_unread_count(&home, name, msg).expect("enqueue_returning_unread_count");

    // The authoritative actionable-unread count (fixed in MED-3) excludes the
    // superseded row → 1 (only the just-appended message).
    let (authoritative, _) = unread_count(&home, name);
    assert_eq!(
        authoritative, 1,
        "sanity: unread_count must exclude the superseded row"
    );

    assert_eq!(
        reported, 1,
        "enqueue_returning_unread_count must match unread_count's actionable \
         definition (exclude superseded rows); it counted the superseded row and \
         returned {reported}"
    );
    assert_eq!(
        reported, authoritative,
        "the pending-hint count must equal the authoritative unread_count"
    );

    fs::remove_dir_all(&home).ok();
}

// ───────────────────────────────────────────────────────────────────────────
// Finding #6 (MED): `drain` (and clear/sweep) silently DELETE
// forward-schema-version rows on rewrite. A `schema_version > CURRENT_VERSION`
// row is `continue`d out of `all_messages`, then the file is rewritten from
// `all_messages` via tmp+rename — permanently destroying a message an older
// daemon merely cannot understand (downgrade data loss). The existing
// `test_reject_future_schema_version` only checks it is not RETURNED, not that
// it survives on disk.
// ───────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "forward-schema-downgrade-loss: red until fix; remove #[ignore] after fix to confirm"]
fn drain_preserves_forward_schema_version_row_on_disk_inbox_notify() {
    let home = tmp_home("f6-forward-schema");
    let name = "futureagent";

    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).expect("mk inbox dir");
    let future_line = r#"{"schema_version":999,"id":"m-future","from":"future","text":"from a newer daemon","kind":null,"timestamp":"2099-01-01T00:00:00Z"}"#;
    let current_line = r#"{"schema_version":1,"id":"m-current","from":"ok","text":"current","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
    let path = inbox_path(&home, name);
    fs::write(&path, format!("{future_line}\n{current_line}\n")).expect("seed inbox");

    // Drain marks the current row read (changed=true → file rewrite) and skips
    // the future row for delivery (correct), but the rewrite drops it from disk.
    let drained = drain(&home, name);
    assert_eq!(
        drained.len(),
        1,
        "only the current-version message is delivered"
    );
    assert_eq!(drained[0].from, "ok");

    // The forward-version row must STILL be on disk — an older daemon must never
    // destroy a message it cannot understand (store.rs refuse-and-preserve).
    let after = fs::read_to_string(&path).expect("read inbox after drain");
    assert!(
        after.contains("\"schema_version\":999"),
        "forward-version row was DELETED on drain rewrite (downgrade data loss); \
         file after drain:\n{after}"
    );

    fs::remove_dir_all(&home).ok();
}
