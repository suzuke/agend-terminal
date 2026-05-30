//! #836 AGEND-MSG notification dedup ledger.
//!
//! Closes the consume → retry race: after `inbox::drain` marks a
//! message `read_at = Some(now)`, the existing rate-limit retry
//! mechanism at `src/daemon/supervisor.rs::process_server_rate_limit_retries`
//! doesn't know the msg was consumed and can still re-inject the
//! `[AGEND-MSG]` header within the 60s `NOTIFICATION_DEDUP_CAP=1`
//! window. Recipient sees the header, calls `inbox`, gets empty —
//! the ghost-notification operator-confusion symptom.
//!
//! This module tracks a `(agent, msg_id)` ledger:
//! - `record_inject` at notify-agent time → entry created (not consumed)
//! - `mark_consumed` inside `inbox::drain` post-read_at-set → flag flipped
//! - `should_suppress_reinject` at retry tick → true iff entry exists AND
//!   consumed
//!
//! Entries expire after [`IDEMPOTENCY_WINDOW_SECS`] (10 min); a periodic
//! sweep called from the supervisor tick reclaims old state. The ledger
//! is in-memory only for v1 — disk persistence deferred to v1.5 per
//! spike design call 3; the existing `NOTIFICATION_DEDUP_CAP=1` provides
//! the safety floor for cross-restart edge cases.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Entry-staleness TTL. Notifications retried beyond this window get
/// allowed through (assumed to be a genuinely separate retry attempt
/// or a daemon restart cleared in-memory state). Wider than the
/// supervisor's `NOTIFICATION_DEDUP_WINDOW_SECS = 60` because the
/// observed re-emission pattern (Telegram inbound replays, channel
/// router retries) clusters within ~5 min; 10 min carries a 2× safety
/// margin without retaining state forever.
pub const IDEMPOTENCY_WINDOW_SECS: u64 = 600;

#[derive(Debug, Clone, Copy)]
struct Entry {
    injected_at: Instant,
    consumed: bool,
}

/// Per-(agent, msg_id) delivery state. Lock-grained at the whole
/// HashMap for simplicity — entries are short-lived (TTL=10min) and
/// sweep is cheap (O(N)).
#[derive(Default)]
pub struct Ledger {
    state: Mutex<HashMap<(String, String), Entry>>,
}

impl Ledger {
    /// Record that an `[AGEND-MSG]` header for `(agent, msg_id)` was
    /// just injected into the agent's PTY. Idempotent — re-recording
    /// the same key refreshes the timestamp without flipping
    /// `consumed` (so a retry that legitimately re-injects WITHIN the
    /// window before consume still keeps the entry as not-consumed).
    pub fn record_inject(&self, agent: &str, msg_id: &str) {
        self.record_inject_at(agent, msg_id, Instant::now());
    }

    /// Test-time variant that pins the timestamp. Production callers
    /// use [`record_inject`].
    pub fn record_inject_at(&self, agent: &str, msg_id: &str, at: Instant) {
        if let Ok(mut s) = self.state.lock() {
            s.entry((agent.to_string(), msg_id.to_string()))
                .and_modify(|e| e.injected_at = at)
                .or_insert(Entry {
                    injected_at: at,
                    consumed: false,
                });
        }
    }

    /// Flag the `(agent, msg_id)` entry as consumed. Called from
    /// `inbox::drain` post-`read_at`-set per spike design call 5.
    /// No-op when the entry doesn't exist (drain runs for every
    /// agent that calls `inbox`, but only some msgs have a
    /// corresponding recorded inject).
    pub fn mark_consumed(&self, agent: &str, msg_id: &str) {
        if let Ok(mut s) = self.state.lock() {
            if let Some(e) = s.get_mut(&(agent.to_string(), msg_id.to_string())) {
                e.consumed = true;
            }
        }
    }

    /// Suppression check at retry-tick time. Returns true iff the
    /// ledger has a non-expired entry for `(agent, msg_id)` AND that
    /// entry is flagged `consumed`. Other cases (entry absent,
    /// entry present but not consumed, entry expired) return false
    /// so the retry can proceed.
    pub fn should_suppress_reinject(&self, agent: &str, msg_id: &str) -> bool {
        self.should_suppress_reinject_at(agent, msg_id, Instant::now())
    }

    /// Test-time variant that pins the clock.
    pub fn should_suppress_reinject_at(&self, agent: &str, msg_id: &str, now: Instant) -> bool {
        let Ok(s) = self.state.lock() else {
            return false;
        };
        let Some(entry) = s.get(&(agent.to_string(), msg_id.to_string())) else {
            return false;
        };
        if !entry.consumed {
            return false;
        }
        now.duration_since(entry.injected_at) < Duration::from_secs(IDEMPOTENCY_WINDOW_SECS)
    }

    /// Drop expired entries. Called periodically from the supervisor
    /// tick (10s cadence) so memory pressure stays bounded.
    pub fn sweep_expired(&self) {
        self.sweep_expired_at(Instant::now());
    }

    /// Test-time variant. Returns the number of dropped entries for
    /// caller observability.
    pub fn sweep_expired_at(&self, now: Instant) -> usize {
        let Ok(mut s) = self.state.lock() else {
            return 0;
        };
        let before = s.len();
        let ttl = Duration::from_secs(IDEMPOTENCY_WINDOW_SECS);
        s.retain(|_, entry| now.duration_since(entry.injected_at) < ttl);
        before.saturating_sub(s.len())
    }

    /// Test-only: count entries (for assertions).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.state.lock().map(|s| s.len()).unwrap_or(0)
    }
}

/// Process-wide singleton. Production callers (notify_agent post-
/// inject, `inbox::drain` post-consume, supervisor retry-tick
/// suppression check) use this; tests construct local `Ledger::default()`
/// instances for isolation.
pub fn global() -> &'static Ledger {
    static LEDGER: OnceLock<Ledger> = OnceLock::new();
    LEDGER.get_or_init(Ledger::default)
}

/// Parse `id=<msg_id>` field from a single-line `[AGEND-MSG]` header.
/// Returns `None` for headers that don't include an id field (e.g.,
/// free-form notifications, event-style headers from
/// `inbox::format_event_header`) so the caller can default to "no
/// dedup, allow the retry through" per spike risk R3.
///
/// The header format is fixed by `inbox::format_header` —
/// space-separated `key=value` fields, `id=<value>` when set. The
/// parser splits on whitespace and looks for the first `id=` token;
/// stops at the next space or end-of-string. Robust to additional
/// fields and reordering as long as keys are space-delimited.
pub fn extract_msg_id_from_header(text: &str) -> Option<String> {
    text.split_ascii_whitespace()
        .find_map(|tok| tok.strip_prefix("id="))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #836 unit: insert + lookup contract. Fresh entry is NOT
    /// flagged consumed; should_suppress_reinject returns false
    /// until mark_consumed flips the flag.
    #[test]
    fn dedup_set_inserts_on_inject() {
        let l = Ledger::default();
        l.record_inject("alice836", "m-1");
        assert_eq!(l.len(), 1);
        assert!(
            !l.should_suppress_reinject("alice836", "m-1"),
            "fresh entry must not suppress — only consumed entries suppress"
        );
    }

    /// #836 unit: mark_consumed + should_suppress contract. Locks
    /// the load-bearing suppression case — record then consume then
    /// retry MUST be suppressed.
    #[test]
    fn dedup_set_suppresses_already_delivered() {
        let l = Ledger::default();
        l.record_inject("alice836", "m-1");
        l.mark_consumed("alice836", "m-1");
        assert!(
            l.should_suppress_reinject("alice836", "m-1"),
            "consumed entry MUST suppress re-inject (the load-bearing #836 case)"
        );
    }

    /// #836 unit: TTL expiry contract. An entry consumed long ago
    /// (past the 10-min window) is no longer suppressed — the retry
    /// gets to fire (assumed to be a genuinely-fresh re-attempt).
    #[test]
    fn dedup_set_expires_after_window() {
        let l = Ledger::default();
        let t0 = Instant::now();
        l.record_inject_at("alice836", "m-1", t0);
        l.mark_consumed("alice836", "m-1");
        let inside_window = t0 + Duration::from_secs(IDEMPOTENCY_WINDOW_SECS - 1);
        assert!(
            l.should_suppress_reinject_at("alice836", "m-1", inside_window),
            "consumed entry inside window must still suppress"
        );
        let outside_window = t0 + Duration::from_secs(IDEMPOTENCY_WINDOW_SECS + 1);
        assert!(
            !l.should_suppress_reinject_at("alice836", "m-1", outside_window),
            "consumed entry past 10-min window must NOT suppress"
        );
    }

    /// #836 unit: multi-msg drain — agent consumes msg-1 but not
    /// msg-2. Subsequent retry must suppress for msg-1 (consumed)
    /// but NOT msg-2 (still pending). Locks the per-msg-id precision
    /// that Option B was chosen for over the simpler unread-count
    /// approach in spike Q1.
    #[test]
    fn dedup_set_handles_multi_msg_drain() {
        let l = Ledger::default();
        l.record_inject("alice836", "m-1");
        l.record_inject("alice836", "m-2");
        l.mark_consumed("alice836", "m-1");
        // Only m-1 is consumed.
        assert!(l.should_suppress_reinject("alice836", "m-1"));
        assert!(
            !l.should_suppress_reinject("alice836", "m-2"),
            "partial-consume must NOT suppress the still-pending msg"
        );
    }

    /// #836 unit: header parser extracts the `id=` field. Locks the
    /// contract that production's retry-suppression path relies on
    /// (parse the AGEND-MSG header text, lookup by msg_id).
    #[test]
    fn extract_msg_id_finds_id_field_in_canonical_header() {
        let header = "\u{1b}[2m[AGEND-MSG]\u{1b}[0m from=fixup-lead \
                      id=m-20260515184625304633-133 kind=task size=1545";
        let id = extract_msg_id_from_header(header);
        assert_eq!(id, Some("m-20260515184625304633-133".to_string()));
    }

    /// #1493 producer→consumer contract: feed the REAL `format_header` output
    /// (not a hand-crafted string) through `extract_msg_id_from_header` and
    /// confirm the id round-trips. The crafted-header tests above are fine for
    /// exercising parser *edge cases* (missing/empty `id=`), but the happy-path
    /// contract must be pinned against the actual producer — otherwise a change
    /// to `format_header`'s `id=` rendering (e.g. the #1487 `now=` addition)
    /// could silently break suppression while the crafted tests stay green.
    #[test]
    fn extract_msg_id_round_trips_real_format_header() {
        let mut msg = crate::inbox::InboxMessage {
            from: "from:fixup-lead".to_string(),
            text: "do the thing".to_string(),
            ..Default::default()
        };
        msg.id = Some("m-20260530120000000000-42".to_string());
        msg.kind = Some("task".to_string());
        let header = crate::inbox::format_header(&msg);
        assert_eq!(
            extract_msg_id_from_header(&header),
            Some("m-20260530120000000000-42".to_string()),
            "id must round-trip through the real producer header: {header}"
        );
    }

    /// #836 unit: header without id= returns None — caller defaults
    /// to "allow through" per spike risk R3. Locks the conservative
    /// fallback.
    #[test]
    fn extract_msg_id_returns_none_when_field_absent() {
        let header = "[AGEND-MSG] from=fixup-lead kind=task size=200";
        assert_eq!(extract_msg_id_from_header(header), None);
        // Empty value also returns None (defensive against `id=` with
        // no value).
        let header_empty = "[AGEND-MSG] from=fixup-lead id= kind=task";
        assert_eq!(extract_msg_id_from_header(header_empty), None);
    }

    /// #836 integration race scenario: simulate the consume → retry
    /// flow. This is the load-bearing test that motivated the whole
    /// design — locking it GREEN proves the dedup set actually
    /// suppresses the redundant re-inject.
    ///
    /// Steps:
    /// 1. Sender delivers msg with id="m-race" → record_inject
    ///    (simulates the post-`notify_agent` hook in C2)
    /// 2. Agent calls `inbox` → drain marks read_at + invokes
    ///    `mark_consumed` (simulates the post-drain hook in C2)
    /// 3. Supervisor's retry-tick fires → `should_suppress_reinject`
    ///    must return true (the C2 supervisor wires this into its
    ///    retry phase)
    #[test]
    fn e2e_consume_then_retry_suppresses_reinject() {
        let l = Ledger::default();
        // Step 1: sender delivers + notify_agent hook records inject.
        l.record_inject("agent-race", "m-race");
        // Step 2: drain marks consumed.
        l.mark_consumed("agent-race", "m-race");
        // Step 3: retry-tick suppression check.
        assert!(
            l.should_suppress_reinject("agent-race", "m-race"),
            "post-consume retry MUST be suppressed (the #836 ghost-notification fix)"
        );
    }
}
