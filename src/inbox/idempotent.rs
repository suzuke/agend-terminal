//! PR #3495: exactly-once inbox enqueue keyed by a producer-supplied
//! `idempotency_key`.
//!
//! A durable-outbox producer (the Claude self-kick watchdog is the first) must
//! be able to REPLAY an enqueue: it persists the intent to notify BEFORE the
//! notice exists in the recipient's inbox, so a crash or a failed append leaves
//! the intent pending and the next pass runs the enqueue again. That replay is
//! only safe if a second append of the same logical notice is a no-op.
//!
//! The check and the append run inside ONE acquisition of the target inbox's
//! per-agent flock ([`super::lock::with_inbox_lock`], the same lock
//! `enqueue` / `drain` / `sweep_expired` take), so two concurrent producers
//! holding the same key cannot both observe "absent" and both append —
//! cross-process, not just cross-thread.
//!
//! Scope of the scan: the target's inbox JSONL. `drain`/`ack` do NOT move rows
//! to another file — they rewrite the same file with `read_at`/`delivering_at`
//! stamped — so a delivered-and-processed notice is still visible to the scan
//! and still suppresses a duplicate. `clear_compact` is the one operation that
//! removes rows; a key whose row has been compacted away can be re-inserted.
//! That is acceptable for this producer: the pending intent it replays lives
//! for at most the gap between two per-tick passes, orders of magnitude shorter
//! than a compaction cycle.

use super::lock::with_inbox_lock;
use super::message::InboxMessage;
use super::storage;
use std::path::Path;

/// Append `msg` to `name`'s inbox unless a row carrying `idempotency_key` is
/// already there.
///
/// Returns `Some(unread_count_after_append)` when the row was inserted and
/// `None` when an existing row already carries the key (the caller must then
/// treat the notice as already delivered — NOT as a failure).
pub(crate) fn enqueue_once_returning_unread_count(
    home: &Path,
    name: &str,
    mut msg: InboxMessage,
    idempotency_key: &str,
) -> anyhow::Result<Option<usize>> {
    if super::disk::is_readonly() {
        anyhow::bail!("inbox readonly: disk space critically low");
    }
    msg.schema_version = InboxMessage::CURRENT_VERSION;
    msg.idempotency_key = Some(idempotency_key.to_string());
    storage::ensure_msg_id(&mut msg);
    let line = format!("{}\n", serde_json::to_string(&msg)?);
    let key = idempotency_key.to_string();

    with_inbox_lock(home, name, move |path| {
        use std::io::Write;
        // PR #3495 r3: FAIL CLOSED. `unwrap_or_default()` turned a permission
        // or IO error — and invalid UTF-8 (`InvalidData`) — into "the inbox is
        // empty", and the append below then wrote a SECOND row for a key that
        // was already there. Only a genuinely absent file is empty; every
        // other error returns BEFORE any append, and the caller (the per-tick
        // self-kick announce, `daemon::per_tick::claude_self_kick::announce`)
        // treats an `Err` as "retry next pass": it leaves the notice intent
        // pending, warns once, and the next pass replays the enqueue.
        let existing = match std::fs::read_to_string(path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "idempotent inbox enqueue: cannot read inbox {} — refusing to append, a duplicate would be indistinguishable from a first delivery",
                    path.display()
                )))
            }
        };
        if content_has_key(&existing, &key) {
            return Ok(None);
        }
        let count = storage::count_unread_in_content(&existing);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(Some(count + 1))
    })?
}

/// Cheap `idempotency_key`-only probe over an inbox JSONL body. Deliberately
/// does NOT deserialize the full [`InboxMessage`]: a forward-schema row a newer
/// daemon wrote must still suppress a duplicate, and such a row can fail the
/// full-struct parse.
fn content_has_key(content: &str, key: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct KeyOnly {
        #[serde(default)]
        idempotency_key: Option<String>,
    }
    content.lines().any(|line| {
        serde_json::from_str::<KeyOnly>(line)
            .ok()
            .and_then(|row| row.idempotency_key)
            .is_some_and(|found| found == key)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Own temp-dir family (#3245 prefix ratchet): no other fixture helper in
    /// this crate builds a path from `agend-selfkick-outbox-`.
    fn tmp_home_outbox(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-selfkick-outbox-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn notice(text: &str) -> InboxMessage {
        InboxMessage::new_system("system:test", "claude_self_kick_ack_late", text)
    }

    /// (f) The primitive's whole contract: one row per key, `Some` then
    /// `None`, and a different key is a different row.
    #[test]
    fn idempotent_enqueue_inserts_once_per_key() {
        let home = tmp_home_outbox("once-per-key");
        let key = "self-kick:d-1:late_ack";

        let first = enqueue_once_returning_unread_count(&home, "lead", notice("first"), key)
            .expect("first enqueue");
        assert!(first.is_some(), "the first insert must report an append");
        let second = enqueue_once_returning_unread_count(&home, "lead", notice("second"), key)
            .expect("second enqueue");
        assert_eq!(
            second, None,
            "a second enqueue under the same key must not append"
        );

        let rows = crate::inbox::drain(&home, "lead");
        assert_eq!(rows.len(), 1, "exactly one durable row: {rows:?}");
        assert_eq!(rows[0].text, "first", "the FIRST body is the durable one");
        assert_eq!(rows[0].idempotency_key.as_deref(), Some(key));

        let other = enqueue_once_returning_unread_count(
            &home,
            "lead",
            notice("other"),
            "self-kick:d-1:ack_overdue",
        )
        .expect("different key");
        assert!(other.is_some(), "a different key is a different notice");
        let rows = crate::inbox::drain(&home, "lead");
        assert_eq!(rows.len(), 1, "and it appended exactly one more row");
        assert_eq!(rows[0].text, "other");

        std::fs::remove_dir_all(&home).ok();
    }

    /// A row already DELIVERED (drained → `read_at`/`delivering_at` stamped,
    /// same file) must still suppress the replay. This is the case the
    /// crash-gap retry actually hits in production.
    #[test]
    fn idempotent_enqueue_suppresses_after_delivery() {
        let home = tmp_home_outbox("after-delivery");
        let key = "self-kick:d-2:ack_overdue";
        assert!(
            enqueue_once_returning_unread_count(&home, "lead", notice("only"), key)
                .expect("enqueue")
                .is_some()
        );
        assert_eq!(crate::inbox::drain(&home, "lead").len(), 1);
        assert_eq!(
            enqueue_once_returning_unread_count(&home, "lead", notice("replay"), key)
                .expect("replay"),
            None,
            "a delivered notice must not be re-appended"
        );
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "nothing new was enqueued"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// PR #3495 r3 (P1): an inbox the scan CANNOT read must fail CLOSED.
    /// `read_to_string(..).unwrap_or_default()` treated a permission/IO error
    /// and invalid UTF-8 as "no rows", so the append below could add a SECOND
    /// row for a key that is already there — the duplicate this module exists
    /// to prevent. NotFound (a genuinely absent inbox) is still empty.
    #[test]
    fn idempotent_enqueue_fails_closed_on_unreadable_inbox() {
        let home = tmp_home_outbox("unreadable");
        let key = "self-kick:d-3:ack_overdue";
        // NotFound stays "empty": the very first enqueue creates the file.
        assert!(
            enqueue_once_returning_unread_count(&home, "lead", notice("first"), key)
                .expect("absent inbox is an empty inbox")
                .is_some()
        );
        let path = with_inbox_lock(&home, "lead", |path| path.to_path_buf()).expect("path");
        let before = std::fs::read(&path).expect("bytes");
        // The row is intact, but the file no longer decodes as UTF-8.
        let mut corrupted = before.clone();
        corrupted.extend_from_slice(b"\xff\xfe not utf-8\n");
        std::fs::write(&path, &corrupted).expect("corrupt");

        let error = enqueue_once_returning_unread_count(&home, "lead", notice("dup"), key)
            .expect_err("an unreadable inbox must not be treated as empty");
        assert!(
            format!("{error:#}").contains("inbox"),
            "the error must name the inbox it refused to append to: {error:#}"
        );
        assert_eq!(
            std::fs::read(&path).expect("bytes after"),
            corrupted,
            "and nothing may be appended before the read succeeds"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
