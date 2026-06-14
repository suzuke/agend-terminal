//! Static-invariant repro (panic_io_extra scope) for: topic-registry persist
//! dropped -> duplicate Telegram forum topic on restart.
//!
//! In `resolve_fleet_binding` (src/channel/telegram/bootstrap.rs) the slow path
//! creates the forum topic, inserts the new `topic_id -> sentinel` mapping, and
//! persists with `let _ = save_topic_registry(home, reg)` — DROPPING the result.
//! If the write fails, memory has the topic but disk does not; on restart the
//! slow path runs again and `create_forum_topic` creates a DUPLICATE named
//! topic. The sibling call site (line ~157) correctly checks the result.
//!
//! The create goes through a live teloxide `bot.create_forum_topic` network
//! call, so the failure cannot be driven without a Telegram seam. This is a
//! source-scanning guard (mirrors tests/core_mutex_invariant.rs): it asserts the
//! dropped-result pattern is GONE. RED now (the `let _ = save_topic_registry(`
//! is present), GREEN after the fix checks/propagates the result.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

/// The dropped-result pattern in the slow path (line ~217). The only other
/// call site (line ~157) uses `if let Err(e) = save_topic_registry(...)`, so a
/// `let _ = save_topic_registry(` needle is unique to the buggy site.
const NEEDLE: &str = "let _ = save_topic_registry(";

#[test]
#[ignore = "telegram-topic-registry-dropped: red until fix; remove #[ignore] after fix to confirm"]
fn fleet_binding_topic_persist_result_not_dropped_panic_io_extra() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/channel/telegram/bootstrap.rs");
    let text = std::fs::read_to_string(&file).expect("read src/channel/telegram/bootstrap.rs");

    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue; // skip comment/doc lines
        }
        if line.contains(NEEDLE) {
            violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "resolve_fleet_binding drops the topic-registry persist result with \
         `let _ = save_topic_registry(...)` after creating a forum topic. If the write fails, \
         disk lacks the mapping and a restart creates a DUPLICATE named topic. Check/propagate \
         the result (mirror the `if let Err(e) = save_topic_registry(...)` call site):\n{}",
        violations.join("\n")
    );
}
