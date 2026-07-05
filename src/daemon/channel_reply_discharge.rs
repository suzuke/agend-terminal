//! #2622 residual-axis PR-1: channel-reply discharge ledger — a disk-durable
//! record of channel-reply obligations that have been explicitly discharged
//! ("no longer owed a reply"), so a future consumer (PR-2) can suppress
//! re-arming an obligation someone already closed.
//!
//! ## Why durable (the #2622 root cause this closes)
//!
//! `reply_ledger`'s only "this obligation is settled" memory today is
//! `HeartbeatPair.settled_reply_groups` — **in-memory + a 600 s TTL**
//! (`SETTLED_GROUP_TTL_MS`). A channel message that keeps redelivering past
//! that window (the live `m-…-125`: an operator message now 13 days stale)
//! **re-arms a fresh obligation on every drain**, resetting the nudge ladder
//! to stage 0 forever. A disk-durable, long-lived discharge record — consulted
//! by `reply_ledger::arm` before it opens an obligation (PR-2) — is the
//! structural exit that in-memory + short-TTL state cannot provide. This
//! mirrors #2537's `discharge_ledger` premise (a memory-only cache replays
//! every triaged obligation on restart), made empirical by
//! [`tests::daemon_refresh_survival`] rather than asserted in prose.
//!
//! ## Key: recipient `agent` + `group_key`, with a `message_id` fallback
//!
//! `reply_ledger::arm` already keys its in-memory settled-suppression by the
//! obligation's `group_key` (`sender|content-hash`, `reply_ledger::group_key`).
//! Keying THIS durable record the same way makes the PR-2 arm guard the exact
//! durable parallel of that existing check — the "seamless" integration the
//! design vet required (a discharged group never re-arms, incl. after
//! restart, AND an operator's resend of the same text — same `group_key`,
//! new `message_id` — stays discharged). When the inbound had no usable text
//! (`group_key` is `None`), the record is keyed by `message_id` instead. Both
//! PR-2 consumers (`arm`, the reply-by-id settle) hold the inbox row and so
//! compute the same key, keeping lookups O(1). The named `message_id`s are
//! stored inside the record for audit + the text-less path.
//!
//! **`agent` dimension (#2622 reviewer4 r0 fix, post-PR-2 first review):** the
//! key is scoped by the recipient agent whose obligation is being discharged.
//! Without it, one agent's self-discharge of a message from a given sender
//! could suppress a DIFFERENT agent's independent channel-reply obligation
//! with the same `sender`+`text` (same `group_key`, different `message_id`)
//! — a cross-agent silent-loss hole the original global key missed.
//!
//! ## PR-1 scope (zero behavior change, per the dispatching task)
//!
//! This module is write/read only. Nothing on the obligation-decision path
//! (`reply_ledger::arm`, `handle_reply`) calls [`is_discharged`] yet — that
//! consumer wiring, plus the discharge MCP primitive and the
//! `channel-reply-discharged` operator-notification kind, are PR-2's job. GC
//! for stale records is likewise deferred (align to the inbox retention
//! window when built) — a natural PR-3 follow-up, not built now, mirroring
//! #2537 PR-1's GC deferral.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Canonical directory — sibling of `discharge-ledger` (#2537), same on-disk
/// convention (one JSON file per discharge key).
pub(crate) fn channel_reply_discharge_dir(home: &Path) -> PathBuf {
    home.join("channel-reply-discharge")
}

/// The durable discharge key: the recipient `agent`, plus the obligation's
/// `group_key` when the inbound carried usable text, else its `message_id`
/// (#2622 reviewer4: the `agent` dimension prevents one agent's discharge
/// from suppressing a different agent's same-sender/same-text obligation).
/// All inputs are untrusted / arbitrary strings, so the on-disk filename is
/// `sha256(key)` — path-traversal-safe, mirroring
/// `discharge_ledger::ledger_path` (#2537) and
/// `ci_watch::registry::watch_filename` (#943).
fn discharge_key(agent: &str, group_key: Option<&str>, message_id: &str) -> String {
    format!(
        "{agent}\u{1}{}",
        group_key.filter(|s| !s.is_empty()).unwrap_or(message_id)
    )
}

fn ledger_path(home: &Path, key: &str) -> PathBuf {
    channel_reply_discharge_dir(home).join(format!(
        "{}.json",
        crate::daemon::utils::sha256_hex(key.as_bytes())
    ))
}

/// One discharged channel-reply obligation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DischargeRecord {
    /// The agent name that self-discharged, or `"operator"` for an
    /// operator-authorized discharge (PR-2 sets this via `operator_gate`).
    pub discharged_by: String,
    /// RFC3339 discharge time.
    pub discharged_at: String,
    /// Mandatory for agent self-discharge (PR-2 enforces); optional for an
    /// operator discharge. Empty normalizes to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The recipient agent this discharge applies to — part of the durable
    /// key (#2622 reviewer4 fix). Stored for audit / introspection.
    #[serde(default)]
    pub agent: String,
    /// The `group_key` this record is keyed by, when the inbound had usable
    /// text (else `None`, keyed by `message_id`). Stored for audit /
    /// introspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    /// Every message id known to belong to this discharged obligation (the
    /// primary the discharge named, plus any that PR-2 records as group
    /// members). Audit + the text-less fallback path.
    #[serde(default)]
    pub message_ids: Vec<String>,
}

/// Record that the channel-reply obligation identified by
/// `(group_key, message_id)` was discharged by `discharged_by`. flock +
/// atomic RMW, mirroring `discharge_ledger::record_discharge` (#2537). The
/// record is keyed by [`discharge_key`]; a re-discharge of the same key
/// merges the `message_id` (latest `discharged_by`/`reason`/`at` win — the
/// obligation is closed either way).
pub(crate) fn record_discharge(
    home: &Path,
    agent: &str,
    group_key: Option<&str>,
    message_id: &str,
    discharged_by: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let key = discharge_key(agent, group_key, message_id);
    let path = ledger_path(home, &key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;
    let mut record: DischargeRecord = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| DischargeRecord {
            discharged_by: String::new(),
            discharged_at: String::new(),
            reason: None,
            agent: agent.to_string(),
            group_key: group_key.filter(|s| !s.is_empty()).map(String::from),
            message_ids: Vec::new(),
        });
    record.discharged_by = discharged_by.to_string();
    record.discharged_at = chrono::Utc::now().to_rfc3339();
    record.reason = reason.filter(|s| !s.is_empty()).map(String::from);
    record.agent = agent.to_string();
    record.group_key = group_key.filter(|s| !s.is_empty()).map(String::from);
    if !record.message_ids.iter().any(|m| m == message_id) {
        record.message_ids.push(message_id.to_string());
    }
    crate::store::save_atomic(&path, &record)
}

/// Look up whether the obligation identified by `(agent, group_key,
/// message_id)` has been discharged. `None` = not discharged (no record for
/// this key — the common case; the obligation is live). Consumed by
/// `reply_ledger::arm` (PR-2) before it opens an obligation.
pub(crate) fn is_discharged(
    home: &Path,
    agent: &str,
    group_key: Option<&str>,
    message_id: &str,
) -> Option<DischargeRecord> {
    let key = discharge_key(agent, group_key, message_id);
    let path = ledger_path(home, &key);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(label: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-channel-reply-discharge-{}-{label}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn write_then_read_round_trip_by_group_key() {
        let home = tmp_home("roundtrip-gk");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|deadbeef"),
            "m-1",
            "operator",
            Some("stale, no longer needed"),
        )
        .unwrap();

        let rec = is_discharged(&home, "agent-1", Some("user:op|deadbeef"), "m-1")
            .expect("a discharged group must read back");
        assert_eq!(rec.discharged_by, "operator");
        assert_eq!(rec.reason.as_deref(), Some("stale, no longer needed"));
        assert_eq!(rec.group_key.as_deref(), Some("user:op|deadbeef"));
        assert!(rec.message_ids.contains(&"m-1".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn undischarged_obligation_reads_none() {
        let home = tmp_home("undischarged");
        assert!(
            is_discharged(&home, "agent-1", Some("user:op|deadbeef"), "m-1").is_none(),
            "a live (never-discharged) obligation must read back None"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn operator_resend_same_text_new_id_stays_discharged() {
        // The seamless-integration guarantee the design vet required: an
        // operator resending the SAME text lands a new message_id but the
        // SAME group_key, so a group discharged under m-1 stays discharged
        // when m-2 (same content) arrives.
        let home = tmp_home("resend-same-group");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|cafe"),
            "m-1",
            "operator",
            None,
        )
        .unwrap();

        let rec = is_discharged(&home, "agent-1", Some("user:op|cafe"), "m-2")
            .expect("same group_key, different message_id must still read discharged");
        assert_eq!(rec.group_key.as_deref(), Some("user:op|cafe"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn text_less_obligation_keyed_by_message_id() {
        // No usable text → group_key is None → the record keys by message_id.
        let home = tmp_home("textless");
        record_discharge(
            &home,
            "agent-1",
            None,
            "m-textless",
            "general",
            Some("acted, no reply owed"),
        )
        .unwrap();

        assert!(
            is_discharged(&home, "agent-1", None, "m-textless").is_some(),
            "a text-less obligation must be reachable by its message_id"
        );
        assert!(
            is_discharged(&home, "agent-1", None, "m-other").is_none(),
            "a different message_id must not collide with it"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn distinct_groups_do_not_collide() {
        let home = tmp_home("distinct");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|aaaa"),
            "m-1",
            "operator",
            None,
        )
        .unwrap();

        assert!(
            is_discharged(&home, "agent-1", Some("user:op|bbbb"), "m-2").is_none(),
            "an unrelated group must read back None, not the sibling group's record"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2622 reviewer4 r0 fix: a discharge scoped to one agent must not
    /// suppress a DIFFERENT agent's independent obligation that happens to
    /// share the same `group_key` (same sender + same text, different
    /// message_id). This is the cross-agent collision the global key missed.
    #[test]
    fn cross_agent_same_group_key_does_not_collide() {
        let home = tmp_home("cross-agent");
        record_discharge(
            &home,
            "agent-a",
            Some("user:op|shared"),
            "m-1",
            "agent-a",
            None,
        )
        .unwrap();

        assert!(
            is_discharged(&home, "agent-b", Some("user:op|shared"), "m-2").is_none(),
            "agent-a's discharge must not suppress agent-b's obligation with the same group_key"
        );
        assert!(
            is_discharged(&home, "agent-a", Some("user:op|shared"), "m-1").is_some(),
            "agent-a's own discharge must still read back"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The empirical proof behind the "must persist to disk, not memory"
    /// premise: write via one call, read via a completely independent call
    /// chain sharing only `home` (i.e. the disk) — this module holds no
    /// in-memory state. A memory-only cache would fail this (the whole reason
    /// #2622's in-memory `settled_reply_groups` cannot fix the loop).
    #[test]
    fn daemon_refresh_survival() {
        let home = tmp_home("refresh-survival");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|beef"),
            "m-1",
            "operator",
            None,
        )
        .unwrap();

        let reopened_home = PathBuf::from(home.to_string_lossy().to_string());
        assert!(
            is_discharged(&reopened_home, "agent-1", Some("user:op|beef"), "m-1").is_some(),
            "the discharge must be readable from disk alone, independent of the writer's in-memory state"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn re_discharge_same_group_merges_ids_latest_wins() {
        let home = tmp_home("merge");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|dd"),
            "m-1",
            "operator",
            Some("first"),
        )
        .unwrap();
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|dd"),
            "m-2",
            "general",
            Some("second"),
        )
        .unwrap();

        let rec = is_discharged(&home, "agent-1", Some("user:op|dd"), "m-1").unwrap();
        assert_eq!(rec.discharged_by, "general", "latest discharge wins");
        assert_eq!(rec.reason.as_deref(), Some("second"));
        assert!(
            rec.message_ids.contains(&"m-1".to_string())
                && rec.message_ids.contains(&"m-2".to_string()),
            "both message_ids belonging to the group are retained for audit: {:?}",
            rec.message_ids
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_reason_normalizes_to_none() {
        let home = tmp_home("empty-reason");
        record_discharge(
            &home,
            "agent-1",
            Some("user:op|ee"),
            "m-1",
            "operator",
            Some(""),
        )
        .unwrap();

        let rec = is_discharged(&home, "agent-1", Some("user:op|ee"), "m-1").unwrap();
        assert_eq!(rec.reason, None, "empty-string reason normalizes to None");
        std::fs::remove_dir_all(&home).ok();
    }
}
