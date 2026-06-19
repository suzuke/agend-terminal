//! [perf-audit R3 — t-…84833-14] Equivalence proof for the `UnreadProbe`
//! unread-count refactor.
//!
//! The hot-path counter (`enqueue_returning_unread_count`) and `unread_count`
//! now count unread rows by deserializing each JSONL line into the minimal
//! `UnreadProbe` (3 filter fields + timestamp) instead of the full
//! `InboxMessage`, to skip the large `text`/`from`/… String allocations. Because
//! `unread_count` gates the inbox-stuck watchdog's re-page decision (#2299
//! excluded `delivering` rows from actionable-unread precisely to not re-page a
//! healthy agent), the new count MUST be byte-identical to the old full-struct
//! count. This module proves it:
//!
//!   1. `probe_count_equals_full_struct_count` — proptest over randomized
//!      `InboxMessage`s (every read_at/delivering_at/superseded_by combo, forward
//!      schema_version, adversarial text incl. JSON-special chars), each
//!      serialized via the REAL producer (`serde_json::to_string`, #1493). The
//!      probe count must equal both the prior full-struct count AND the
//!      hand-computed filter.
//!   2. `state_coverage_equiv_serde_fixture` — one row of EVERY state, asserts
//!      all three counters (`count_unread_in_content`, the full-struct reference,
//!      `unread_count`, `enqueue_returning_unread_count`) agree.
//!   3. `cross_mutator_consistency` — drive the inbox through the REAL mutators
//!      (enqueue → drain → ack → mark_ci_watch_superseded) and after each step
//!      assert the two production counters agree with a hand-tracked expected.
//!   4. edge cases — empty lines, torn/invalid-JSON line, forward-schema row.

use super::{
    count_unread_in_content, drain, enqueue, enqueue_returning_unread_count, inbox_path,
    mark_ci_watch_superseded, unread_count,
};
use crate::inbox::InboxMessage;
use proptest::prelude::*;
use std::fs;
use std::path::PathBuf;

/// Unique temp HOME per test (mirrors `inbox/tests.rs::tmp_home`). The `tag`
/// keeps concurrent tests in the same process from colliding on one path.
fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-r3-equiv-{}-{}", tag, std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(dir.join("inbox")).ok();
    dir
}

/// EXACT replica of the prior full-`InboxMessage` count loop (the behavior we
/// must preserve). `enqueue_returning_unread_count` used the `trim().is_empty()`
/// guard; `unread_count` let empty lines fail `from_str` — both net to the same
/// count, so this single reference is valid for both.
fn count_full_struct_reference(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            serde_json::from_str::<InboxMessage>(l)
                .map(|m| {
                    m.read_at.is_none() && m.delivering_at.is_none() && m.superseded_by.is_none()
                })
                .unwrap_or(false)
        })
        .count()
}

/// Build a message in a chosen state, then serialize it via the real producer.
fn row(read: bool, delivering: bool, superseded: bool, forward_schema: bool, text: &str) -> String {
    let mut m = InboxMessage {
        schema_version: if forward_schema {
            InboxMessage::CURRENT_VERSION + 5
        } else {
            InboxMessage::CURRENT_VERSION
        },
        from: "from:peer".to_string(),
        text: text.to_string(),
        kind: Some("report".to_string()),
        timestamp: "2026-06-19T00:00:00Z".to_string(),
        ..Default::default()
    };
    if read {
        m.read_at = Some("2026-06-19T01:00:00Z".to_string());
    }
    if delivering {
        m.delivering_at = Some("2026-06-19T00:30:00Z".to_string());
    }
    if superseded {
        m.superseded_by = Some("m-newer".to_string());
    }
    serde_json::to_string(&m).expect("serialize row")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// The load-bearing equivalence: for any set of producer-emitted rows, the
    /// cheap probe count == the prior full-struct count == the explicit filter.
    #[test]
    fn probe_count_equals_full_struct_count(
        specs in prop::collection::vec(
            // (read, delivering, superseded, forward_schema, text-with-JSON-special-chars)
            (
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
                any::<bool>(),
                r#"[a-zA-Z0-9 "\\:{}\[\]]{0,48}"#,
            ),
            0..40usize,
        )
    ) {
        let mut lines = Vec::with_capacity(specs.len());
        let mut expected = 0usize;
        for (read, delivering, superseded, fwd, text) in &specs {
            if !read && !delivering && !superseded {
                expected += 1;
            }
            lines.push(row(*read, *delivering, *superseded, *fwd, text));
        }
        let content = lines.join("\n") + "\n";

        let probe = count_unread_in_content(&content);
        let full = count_full_struct_reference(&content);
        prop_assert_eq!(probe, full, "probe count diverged from full-struct count");
        prop_assert_eq!(probe, expected, "probe count diverged from explicit filter");
    }
}

#[test]
fn state_coverage_equiv_serde_fixture() {
    let home = tmp_home("state-coverage");
    let name = "agent";

    // One row of EVERY state, all via the real serializer (#1493):
    let lines = [
        row(false, false, false, false, "plain unread"), // UNREAD            → counts
        row(true, false, false, false, "processed"),     // read/processed    → excluded
        row(false, true, false, false, "in-flight"),     // delivering        → excluded
        row(false, false, true, false, "old ci-watch"),  // superseded+unread → excluded
        row(true, false, true, false, "read+superseded"), // read+superseded  → excluded
        row(false, false, false, true, "forward schema unread"), // fwd-schema UNREAD → counts
    ];
    fs::write(inbox_path(&home, name), lines.join("\n") + "\n").expect("seed");

    // Two genuine unread rows: the plain one + the forward-schema one.
    let expected = 2usize;

    let content = fs::read_to_string(inbox_path(&home, name)).unwrap();
    assert_eq!(count_unread_in_content(&content), expected, "probe count");
    assert_eq!(
        count_full_struct_reference(&content),
        expected,
        "full-struct reference count"
    );

    let (authoritative, oldest) = unread_count(&home, name);
    assert_eq!(authoritative, expected, "unread_count must match");
    assert!(
        oldest.is_some(),
        "oldest must be derived for the unread rows"
    );

    // enqueue_returning_unread_count appends one more unread row → expected + 1.
    let reported = enqueue_returning_unread_count(
        &home,
        name,
        InboxMessage::new_system("system:test", "report", "freshly appended"),
    )
    .expect("enqueue_returning_unread_count");
    assert_eq!(
        reported,
        expected + 1,
        "reported must include the appended row"
    );
    assert_eq!(
        unread_count(&home, name).0,
        reported,
        "post-append authoritative must equal reported"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn cross_mutator_consistency() {
    let home = tmp_home("cross-mutator");
    let name = "agent";

    let mk = |id: &str, text: &str| {
        let mut m = InboxMessage::new_system("from:peer", "report", text);
        m.id = Some(id.to_string());
        m
    };

    // 1. enqueue A,B,C → 3 unread.
    for id in ["A", "B", "C"] {
        enqueue(&home, name, mk(id, "body")).expect("enqueue");
    }
    assert_eq!(unread_count(&home, name).0, 3, "3 unread after enqueue");

    // 2. drain → A,B,C become `delivering` (excluded from unread).
    let drained = drain(&home, name);
    assert_eq!(drained.len(), 3, "all three drained");
    assert_eq!(
        unread_count(&home, name).0,
        0,
        "delivering rows are NOT actionable-unread"
    );

    // 3. enqueue D → 1 unread (D); A,B,C still delivering.
    enqueue(&home, name, mk("D", "body")).expect("enqueue D");
    assert_eq!(unread_count(&home, name).0, 1, "only D is unread");

    // 4. ack all delivering → A,B,C processed; D still unread.
    let acked = super::ack(&home, name, None);
    assert_eq!(acked, 3, "three delivering rows acked");
    assert_eq!(unread_count(&home, name).0, 1, "D still unread after ack");

    // 5. enqueue a ci-watch E, then supersede it → E unread+superseded (excluded).
    let mut e = InboxMessage::new_system("system:ci", "ci-watch", "ci-watch owner/repo@main built");
    e.id = Some("E".to_string());
    enqueue(&home, name, e).expect("enqueue E");
    assert_eq!(unread_count(&home, name).0, 2, "D + E unread");
    mark_ci_watch_superseded(&home, name, "owner/repo@main", "m-newer");
    assert_eq!(
        unread_count(&home, name).0,
        1,
        "E superseded → only D unread"
    );

    // The two production counters must agree at the end: appending F reports the
    // existing unread (D) + the appended F = 2.
    let reported = enqueue_returning_unread_count(&home, name, mk("F", "body")).expect("F");
    assert_eq!(reported, 2, "reported = D + F");
    assert_eq!(
        unread_count(&home, name).0,
        reported,
        "authoritative agrees with reported"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn edge_cases_empty_torn_forward_schema() {
    // Empty + whitespace lines: skipped by both (probe and full).
    let content = "\n   \n";
    assert_eq!(count_unread_in_content(content), 0);
    assert_eq!(count_full_struct_reference(content), 0);

    // Torn / invalid-JSON trailing line: invalid JSON → Err → skipped by both.
    let unread = row(false, false, false, false, "real unread");
    let torn = format!("{unread}\n{{\"schema_version\":1,\"from\":\"x\"");
    assert_eq!(
        count_unread_in_content(&torn),
        1,
        "the one valid unread row counts; torn line skipped"
    );
    assert_eq!(count_full_struct_reference(&torn), 1);

    // Forward-schema unread row: full valid JSON with extra fields the struct
    // ignores → counted by both (the count loops do not gate on schema_version).
    let fwd = row(false, false, false, true, "future unread");
    assert_eq!(count_unread_in_content(&fwd), 1);
    assert_eq!(count_full_struct_reference(&fwd), 1);
}
