//! Poll-reminder: nudge idle agents that have unread inbox messages.

use crate::agent::{self, AgentRegistry};
use crate::state::AgentState;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;

/// Per-agent de-dup state: last notified unread count.
static LAST_NOTIFIED: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Atomic check-and-record: returns true if count changed (should notify),
/// and records the new count in the same lock scope.
fn should_notify_and_record(name: &str, count: usize) -> bool {
    let mut guard = LAST_NOTIFIED.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    let prev = map.get(name).copied().unwrap_or(0);
    if prev == count {
        return false;
    }
    map.insert(name.to_string(), count);
    true
}

/// H3: Remove agent from dedup state when deleted (prevents unbounded growth).
pub fn remove_agent(name: &str) {
    let mut guard = LAST_NOTIFIED.lock();
    if let Some(map) = guard.as_mut() {
        map.remove(name);
    }
}

/// Pure collector: returns (agent_name, reminder_string) for each agent
/// that should be nudged. No side effects — does not inject into PTY.
pub fn collect_poll_reminders(home: &Path, registry: &AgentRegistry) -> Vec<(String, String)> {
    // #1617-class (mirror conflict_notify's phase-1-collect / phase-2-IO): snapshot
    // the idle-agent names UNDER the registry lock, then DROP the guard before any
    // inbox file IO. `inbox::unread_count` does `fs::read_to_string` + full-file
    // parse; holding the GLOBAL registry lock across that (per agent, in a loop)
    // stalls every other registry user when the inbox is large or the FS is slow.
    let idle_names: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values()
            // #2465: operated (hook-primary) state — a screen that mis-scrapes Idle while
            // a fresh hook proves the agent is mid-turn no longer gets a poll nudge it
            // shouldn't. A correction never flips TO idle, so a genuinely idle agent is
            // unaffected; raw==operated ⇒ byte-identical to pre-#2465.
            .filter(|handle| {
                let c = handle.core.lock();
                crate::daemon::shadow::operated_state(c.state.current, c.observed_status.as_ref())
                    == AgentState::Idle
            })
            .map(|handle| handle.name.to_string())
            .collect()
    };

    // Phase 2: the inbox reads + dedup + formatting run lock-free.
    let mut result = Vec::new();
    for name in &idle_names {
        let (count, oldest) = crate::inbox::unread_count(home, name);
        if count == 0 {
            continue;
        }
        if !should_notify_and_record(name, count) {
            continue;
        }
        let age_str = match oldest {
            Some(ts) => {
                let mins = chrono::Utc::now()
                    .signed_duration_since(ts)
                    .num_minutes()
                    .max(0);
                format!("{mins}m")
            }
            None => "?".to_string(),
        };
        let count_str = count.to_string();
        let reminder = crate::inbox::format_event_header(
            "poll-reminder",
            &[("unread", &count_str), ("oldest", &age_str)],
        );
        result.push((name.to_string(), reminder));
    }
    result
}

/// Run one poll-reminder pass. Called from daemon tick every N ticks.
/// Collects reminders via [`collect_poll_reminders`] then delivers each.
///
/// #event-bus pattern #8, Step 2 (legacy-zero): emit a `PollReminder` per nudge;
/// the subscriber delivers via [`deliver_poll_reminder`]. The bus is the sole
/// delivery path.
pub fn poll_reminder_pass(home: &Path, registry: &AgentRegistry) {
    for (name, reminder) in collect_poll_reminders(home, registry) {
        crate::daemon::event_bus::global().emit(
            home,
            crate::daemon::event_bus::EventKind::PollReminder {
                agent: name,
                reminder,
            },
        );
    }
}

/// #event-bus pattern #8: the single delivery primitive, shared by the legacy
/// pass and the bus subscriber. `reminder` is the already-formatted string from
/// [`collect_poll_reminders`] (frozen at collect time — see [`EventKind::PollReminder`]
/// for why the age is NOT recomputed here).
fn deliver_poll_reminder(home: &Path, agent: &str, reminder: &str) {
    crate::inbox::compose_aware_inject(home, agent, reminder);
}

/// #event-bus pattern #8: subscriber — re-deliver the frozen reminder text.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::PollReminder { agent, reminder } = &event.kind {
        deliver_poll_reminder(&event.home, agent, reminder);
        true
    } else {
        false
    }
}

/// #event-bus pattern #8: register the delivery subscriber at daemon startup.
/// Home-agnostic — the home travels on each event. Wired beside the other
/// patterns in `daemon::mod`.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentCore, AgentHandle};
    use crate::state::StateTracker;
    use parking_lot::Mutex;
    use portable_pty::native_pty_system;
    use std::sync::Arc;

    /// #1617-class invariant: `collect_poll_reminders` must NEVER hold the
    /// global registry lock across the blocking `inbox::unread_count` file read.
    /// Holding the registry across per-agent inbox reads stalls every other
    /// registry user (same class #1593/#1617 closed elsewhere; conflict_notify
    /// already does the phase-1-collect / phase-2-IO split this mirrors).
    ///
    /// Structural source-scan (mirrors #1593 F2): brace-match the idle-name
    /// snapshot block and assert (a) `unread_count` is NOT inside it (not under
    /// the lock) and (b) `unread_count` IS called after the block closes (the
    /// IO runs lock-free). Needles are `concat`-built and the scan is sliced to
    /// the production region so this test can't self-satisfy.
    #[test]
    fn poll_reminder_unread_read_not_held_across_registry_lock() {
        let src = include_str!("poll_reminder.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        // Fix marker: idle agent names are snapshotted into a Vec under the lock.
        let bind_needle = ["let idle_names: Vec<String>", " = {"].concat();
        let bstart = prod
            .find(&bind_needle)
            .expect("idle-name snapshot binding present (fix marker)");

        let open_rel = prod[bstart..].find('{').expect("binding block opens");
        let block_start = bstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "binding block must close");

        // #t-…61487: match the CALL form (`unread_count(home`), not a bare
        // `unread_count` token — a doc/comment mention must NOT satisfy the guard
        // (that is exactly how v1 left this guard "blind": the obligation-count
        // refactor moved the real call away but a comment kept the loose needle green).
        let io_needle = ["unread_count", "(home"].concat();
        let locked_region = &prod[block_start..=block_end];
        assert!(
            !locked_region.contains(&io_needle),
            "collect_poll_reminders must NOT call inbox::unread_count under the registry lock (#1617 class)"
        );
        assert!(
            prod[block_end..].contains(&io_needle),
            "inbox::unread_count must run AFTER the registry lock is dropped"
        );
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-poll-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn seed_unread(home: &Path, agent: &str, count: usize) {
        for i in 0..count {
            let _ = crate::inbox::enqueue(
                home,
                agent,
                crate::inbox::InboxMessage {
                    schema_version: 1,
                    id: Some(format!("m-{agent}-{i}")),
                    from: "test".into(),
                    text: format!("msg {i}"),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    ..Default::default()
                },
            );
        }
    }

    /// Seed `count` STALE `delivering` rows (read_at None, delivering_at older
    /// than `RECLAIM_TTL_SECS`) directly into the agent's name-based inbox file,
    /// so `reclaim_stale_delivering` reverts them back to unread. Distinct from
    /// `seed_unread` (which seeds plain unread rows reclaim would not touch).
    ///
    /// Seeds `kind=query` (an OBLIGATION) on purpose: post-#t-…61487 reclaim only
    /// re-arms the poll-reminder for re-nudge-worthy rows (obligation / unknown
    /// kind). The `kind` parameter lets a caller seed an OBLIGATION (`query` →
    /// reclaim re-arms, the #2299 promise) or a NON-obligation (`report` → reclaim
    /// must NOT re-arm, the #t-…61487 noise fix). Name-based inbox (`{agent}.jsonl`),
    /// so `reclaim`'s file-stem agent name == the poll-reminder ledger key and the
    /// re-arm (or its absence) is observable.
    fn seed_stale_delivering(home: &Path, agent: &str, count: usize, kind: &str) {
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).expect("mkdir inbox");
        let stale = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let mut content = String::new();
        for i in 0..count {
            let msg = crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some(format!("m-{agent}-{i}")),
                from: "test".into(),
                text: format!("msg {i}"),
                kind: Some(kind.to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                delivering_at: Some(stale.clone()), // delivered, unconfirmed, stale
                ..Default::default()                // read_at = None
            };
            content.push_str(&serde_json::to_string(&msg).expect("serialize"));
            content.push('\n');
        }
        std::fs::write(inbox_dir.join(format!("{agent}.jsonl")), content).expect("write inbox");
    }

    fn mock_registry(name: &str, state: AgentState) -> AgentRegistry {
        use portable_pty::{CommandBuilder, PtySize};
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("true"))
            .expect("spawn");
        let writer = pair.master.take_writer().expect("writer");
        let mut st = StateTracker::new(None);
        st.current = state;
        let core = AgentCore {
            vterm: crate::vterm::VTerm::new(24, 80),
            subscribers: Vec::new(),
            state: st,
            health: crate::health::HealthTracker::new(),
            api_activity: crate::agent::ApiActivity::default(),
            observed_status: None,
        };
        let core = Arc::new(crate::sync_audit::CoreMutex::new(core));
        let published_state = core.lock().state.published_handle();
        let published_observed = core.lock().state.published_observed_handle();
        // `st.current` was set directly (bypasses record_set), so sync the
        // lock-free mirror to match.
        published_state.store(state as u8, std::sync::atomic::Ordering::Relaxed);
        let handle = AgentHandle {
            id: crate::types::InstanceId::default(),
            name: name.to_string().into(),
            backend_command: "test".to_string(),
            pty_writer: Arc::new(Mutex::new(writer)),
            pty_master: Arc::new(Mutex::new(pair.master)),
            core,
            published_state,
            published_observed,
            child: Arc::new(Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let reg: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // #1441: registry is UUID-keyed — insert under the handle's own id.
        reg.lock().insert(handle.id, handle);
        reg
    }

    /// Reset dedup state for a specific agent to allow fresh test runs.
    fn reset_dedup(name: &str) {
        let mut guard = LAST_NOTIFIED.lock();
        if let Some(map) = guard.as_mut() {
            map.remove(name);
        }
    }

    #[test]
    fn test_collect_poll_reminders_returns_reminder_when_idle_with_unread() {
        let home = tmp_home("collect-idle");
        let agent = "collect-idle-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v = collect_poll_reminders(&home, &registry);
        assert_eq!(v.len(), 1, "should produce 1 reminder");
        assert_eq!(v[0].0, agent);
        assert!(v[0].1.contains("[AGEND-MSG]"), "must contain header prefix");
        assert!(v[0].1.contains("kind=poll-reminder"), "must contain kind");
        assert!(v[0].1.contains("unread=3"), "must contain unread count");

        std::fs::remove_dir_all(&home).ok();
    }

    /// Restore an env var to its pre-test value on drop. The shadow-observer tests mutate
    /// `AGEND_SHADOW_OBSERVER` / `AGEND_OBSERVED_DISPATCH` process-globally, so they run
    /// `#[serial_test::serial(shadow_observer)]` (same group as the snapshot operated-state
    /// test) to avoid racing other readers of those vars.
    struct EnvGuard(&'static str, Option<String>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
    fn guard_shadow_env() -> (EnvGuard, EnvGuard) {
        (
            EnvGuard(
                "AGEND_SHADOW_OBSERVER",
                std::env::var("AGEND_SHADOW_OBSERVER").ok(),
            ),
            EnvGuard(
                "AGEND_OBSERVED_DISPATCH",
                std::env::var("AGEND_OBSERVED_DISPATCH").ok(),
            ),
        )
    }

    /// #2465 regression (REAL path): an agent whose raw screen scrapes Idle but a FRESH
    /// high-confidence Hook proves it is Active (the mid-API false-idle shape) must NOT be
    /// poll-nudged — `collect_poll_reminders` now reads the operated (hook-primary) state via
    /// `operated_state`, so a genuinely-busy agent mis-scraped idle is no longer pestered.
    /// The `AGEND_OBSERVED_DISPATCH=0` kill-switch restores the raw-Idle reminder byte-for-byte.
    ///
    /// SCOPE (#2465): proves false-idle→consumer routing only. It deliberately does NOT model
    /// the dev-3 ApiError-as-ratelimit stuck incident — claude hooks never emit RateLimited, so
    /// that case has no observed signal to route and is a separate (SRL-arm) follow-up fix.
    #[test]
    #[serial_test::serial(shadow_observer)]
    fn false_idle_corrected_by_hook_suppresses_poll_reminder() {
        use crate::daemon::shadow::evidence::{Authority, Confidence};
        use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};

        let _env = guard_shadow_env();
        let home = tmp_home("false-idle-suppress");
        let agent = "false-idle-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        // Attach a fresh high-confidence Hook=Active correction; raw screen stays Idle.
        for handle in registry.lock().values() {
            handle.core.lock().observed_status = Some(ObservedStatus {
                state: ObservedState::Active,
                authority: Authority::Hook,
                confidence: Confidence::Strong,
                evidence: vec![],
                since_ms: 0,
            });
        }

        // operated-dispatch ON (default): operated = Active ⇒ not idle ⇒ no reminder.
        std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
        std::env::remove_var("AGEND_OBSERVED_DISPATCH");
        reset_dedup(agent);
        let v = collect_poll_reminders(&home, &registry);
        assert!(
            v.is_empty(),
            "false-idle agent (hook=Active) must NOT be poll-nudged, got: {v:?}"
        );

        // Kill-switch: raw Idle wins ⇒ reminder returns (byte-identical to pre-#2465).
        std::env::set_var("AGEND_OBSERVED_DISPATCH", "0");
        reset_dedup(agent);
        let v = collect_poll_reminders(&home, &registry);
        assert_eq!(
            v.len(),
            1,
            "AGEND_OBSERVED_DISPATCH=0 restores raw-Idle reminder"
        );
        assert_eq!(v[0].0, agent);

        std::fs::remove_dir_all(&home).ok();
    }

    /// #2465 gate firewall: a SCREEN-authority observed correction — the only plane a
    /// hook/stream-less backend (e.g. agy) ever has — can NEVER satisfy the high-confidence
    /// override gate, so the raw Idle stands and the agent is still nudged. Zero regression
    /// for screen-only backends, by construction.
    #[test]
    #[serial_test::serial(shadow_observer)]
    fn screen_authority_observed_does_not_suppress_poll_reminder() {
        use crate::daemon::shadow::evidence::{Authority, Confidence};
        use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};

        let _env = guard_shadow_env();
        std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
        std::env::remove_var("AGEND_OBSERVED_DISPATCH");

        let home = tmp_home("screen-no-suppress");
        let agent = "screen-obs-agent";
        seed_unread(&home, agent, 2);
        let registry = mock_registry(agent, AgentState::Idle);
        for handle in registry.lock().values() {
            handle.core.lock().observed_status = Some(ObservedStatus {
                state: ObservedState::Active,
                authority: Authority::Screen, // weak/screen-only plane — the agy shape
                confidence: Confidence::Strong,
                evidence: vec![],
                since_ms: 0,
            });
        }
        reset_dedup(agent);
        let v = collect_poll_reminders(&home, &registry);
        assert_eq!(
            v.len(),
            1,
            "screen-authority observed must NOT override raw Idle (screen-only backends unaffected)"
        );
        assert_eq!(v[0].0, agent);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_dedupes_same_count() {
        let home = tmp_home("collect-dedup");
        let agent = "collect-dedup-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v1 = collect_poll_reminders(&home, &registry);
        assert_eq!(v1.len(), 1);

        let v2 = collect_poll_reminders(&home, &registry);
        assert!(
            v2.is_empty(),
            "second call with same count must be suppressed by dedup"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_re_notifies_on_count_change() {
        let home = tmp_home("collect-renotify");
        let agent = "collect-renotify-agent";
        seed_unread(&home, agent, 3);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        let v1 = collect_poll_reminders(&home, &registry);
        assert_eq!(v1.len(), 1);
        assert!(v1[0].1.contains("unread=3"));

        // Add 2 more unread → count changes to 5
        seed_unread(&home, agent, 2);
        let v2 = collect_poll_reminders(&home, &registry);
        assert_eq!(v2.len(), 1, "count changed → should re-notify");
        assert!(
            v2[0].1.contains("unread=5"),
            "must reflect new count, got: {}",
            v2[0].1
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-98760-9 (#2299 regression, production-path): a dead turn's stale
    /// `delivering` rows reclaimed back to unread must RE-PAGE the idle agent —
    /// even when the restored unread count equals the count the poll-reminder last
    /// notified at. Bug: draining to `delivering` left `LAST_NOTIFIED` at N (the
    /// `count==0` path never records 0), so reverting back to N read as "no change"
    /// and the nudge was withheld until the 10-min reclaim TTL. NOTE: no poll pass
    /// observes the `count==0` window here (drain→reclaim between passes — the real
    /// scenario), so this is fixed only by `reclaim_stale_delivering` actively
    /// clearing the agent's poll-reminder dedup, NOT by recording 0 on a 0-count pass.
    ///
    /// #t-…61487: reclaim now re-arms CONDITIONALLY — only for re-nudge-worthy rows
    /// (obligation / unknown kind). This test seeds `kind=query` (obligations), so the
    /// re-arm still fires. The non-obligation case (a reclaimed `report` must NOT
    /// re-arm) is `reclaim_does_not_rearm_for_non_obligation_report` below.
    #[test]
    fn reclaim_rearms_poll_reminder_even_when_restored_count_equals_last_notified() {
        let home = tmp_home("reclaim-rearm");
        let agent = "reclaim-rearm-agent";
        let registry = mock_registry(agent, AgentState::Idle);

        // Prior state: the poll-reminder last notified this agent at count 3.
        reset_dedup(agent);
        assert!(
            should_notify_and_record(agent, 3),
            "seed: record last-notified count = 3"
        );

        // A dead turn's 3 OBLIGATION (query) messages are now stale `delivering`
        // (drained, never acked). Unread count is 0; no poll pass runs in this window,
        // so the ledger stays at 3 — the production bug scenario.
        seed_stale_delivering(&home, agent, 3, "query");

        // Reclaim reverts the stale delivering rows back to unread (count → 3).
        crate::inbox::storage::reclaim_stale_delivering(&home);

        // Restored count (3) EQUALS the last-notified count (3). Pre-fix this was
        // deduped to nothing; the fix re-arms the reminder on reclaim → re-page.
        let v = collect_poll_reminders(&home, &registry);
        assert_eq!(
            v.len(),
            1,
            "reclaim must re-arm the poll-reminder even when restored count == last notified"
        );
        assert_eq!(v[0].0, agent);
        assert!(
            v[0].1.contains("unread=3"),
            "reminder reflects the restored count, got: {}",
            v[0].1
        );

        reset_dedup(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-…61487 THE NOISE FIX (complement of `reclaim_rearms_*`): a stale-`delivering`
    /// REPORT (a non-obligation — the fire-and-forget kind that re-paged every ~2h)
    /// reclaimed back to unread must NOT re-arm the poll-reminder, so the idle agent is
    /// not re-paged. Identical name-based fixture + restored-count-equals-last-notified
    /// setup as `reclaim_rearms_*`, but the conditional re-arm gate SUPPRESSES the
    /// re-arm for a report. Non-vacuous: reverting the gate to the unconditional
    /// `poll_reminder::remove_agent` (the pre-#t-…61487 code) makes this fail (the
    /// dedup clears → the restored count re-pages).
    #[test]
    fn reclaim_does_not_rearm_for_non_obligation_report() {
        let home = tmp_home("reclaim-report-norearm");
        let agent = "reclaim-report-agent";
        let registry = mock_registry(agent, AgentState::Idle);

        // Prior state: the poll-reminder last notified this agent at count 1.
        reset_dedup(agent);
        assert!(
            should_notify_and_record(agent, 1),
            "seed: record last-notified count = 1"
        );

        // A drained-then-stale `report` (non-obligation), reverted by reclaim.
        seed_stale_delivering(&home, agent, 1, "report");
        crate::inbox::storage::reclaim_stale_delivering(&home);

        // Restored count (1, kind-agnostic `unread_count`) EQUALS the last-notified
        // count (1). The gate did NOT re-arm (report is a recognised non-obligation),
        // so the dedup still holds 1 → no re-page. (Pre-fix: reclaim's unconditional
        // re-arm cleared the dedup → re-page = the ~2h noise.)
        let v = collect_poll_reminders(&home, &registry);
        assert!(
            v.is_empty(),
            "a reclaimed report (non-obligation) must NOT re-arm the poll-reminder: {v:?}"
        );

        reset_dedup(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_collect_poll_reminders_skips_when_busy() {
        let home = tmp_home("collect-busy");
        let agent = "collect-busy-agent";
        seed_unread(&home, agent, 5);
        let registry = mock_registry(agent, AgentState::Active);
        reset_dedup(agent);

        let v = collect_poll_reminders(&home, &registry);
        assert!(v.is_empty(), "busy agent must not get reminder");

        std::fs::remove_dir_all(&home).ok();
    }

    // ── #event-bus pattern #8: PTY-inject migration parity ──────────────
    //
    // PTY-INJECT TEST TEMPLATE (first of the PTY-inject patterns; reuse for the
    // rest). `compose_aware_inject` is a complex SHARED delivery fn — the
    // migration only changes the dispatch ROUTE (legacy pass vs bus subscriber),
    // never the deliver behavior, so parity = "the subscriber feeds
    // compose_aware_inject byte-identical (agent, text)". PTY bytes are not a
    // drainable sink and PTY-readback is windows-flaky (#1699), so we observe via
    // `notification_queue` — a drainable file sink that `compose_aware_inject`
    // writes to on its DEFER path. Staging a `thinking` SNAPSHOT forces that
    // path deterministically + cross-platform (the snapshot agent_state, read by
    // `should_defer_inject`, is independent of the registry-handle state that the
    // idle-filter in `collect` reads).

    /// Stage a snapshot with `agent` mid-generation so `compose_aware_inject`
    /// takes the defer path and enqueues to the drainable `notification_queue`.
    fn stage_thinking_snapshot(home: &Path, agent: &str) {
        crate::snapshot::save(
            home,
            &[crate::snapshot::AgentSnapshot {
                name: agent.to_string(),
                backend_command: "test".to_string(),
                args: vec![],
                working_dir: None,
                submit_key: "\r".to_string(),
                health_state: "Healthy".to_string(),
                agent_state: "active".to_string(),
                silent_secs: 0,
                output_silent_secs: 0,
            }],
        );
    }

    /// PARITY (gate-ON): the bus `emit`→subscriber path delivers the SAME frozen
    /// reminder text to `compose_aware_inject` as the legacy direct path — proven
    /// by byte-comparing the drained `notification_queue` payloads. Separate
    /// homes isolate the queues; poll-reminder headers carry no `msg_id`, so the
    /// #911 dedup gate never fires across them. No `env_lock`: the recipient is a
    /// registry agent name, not env-derived.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_inject() {
        let agent = "poll-parity-agent";
        // ONE frozen reminder string fed to BOTH paths (mirrors collect output —
        // the age is frozen here, never recomputed by the subscriber).
        let reminder = crate::inbox::format_event_header(
            "poll-reminder",
            &[("unread", "3"), ("oldest", "5m")],
        );

        // Legacy direct deliver (gate-OFF path).
        let home_legacy = tmp_home("parity-legacy");
        stage_thinking_snapshot(&home_legacy, agent);
        deliver_poll_reminder(&home_legacy, agent, &reminder);

        // Bus emit→subscriber (gate-ON path) via a local enabled test bus.
        let home_bus = tmp_home("parity-bus");
        stage_thinking_snapshot(&home_bus, agent);
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::PollReminder {
                agent: agent.to_string(),
                reminder: reminder.clone(),
            },
        );

        let legacy: Vec<String> = crate::notification_queue::drain(&home_legacy, agent)
            .into_iter()
            .map(|q| q.text)
            .collect();
        let viabus: Vec<String> = crate::notification_queue::drain(&home_bus, agent)
            .into_iter()
            .map(|q| q.text)
            .collect();
        assert_eq!(
            legacy, viabus,
            "emit→subscriber inject text must be byte-identical to legacy"
        );
        assert!(!legacy.is_empty(), "legacy path must have delivered");

        std::fs::remove_dir_all(&home_legacy).ok();
        std::fs::remove_dir_all(&home_bus).ok();
    }

    /// #event-bus Step 2 (legacy-zero): `poll_reminder_pass` emits to the global
    /// bus; the registered subscriber delivers via `deliver_poll_reminder`. Registry
    /// handle is Idle (so `collect` picks the agent up); the snapshot is `thinking`
    /// (so the deliver defers into the drainable queue).
    #[test]
    fn pass_delivers_via_bus() {
        let home = tmp_home("via-bus");
        let agent = "poll-gateoff-agent";
        seed_unread(&home, agent, 2);
        stage_thinking_snapshot(&home, agent);
        let registry = mock_registry(agent, AgentState::Idle);
        reset_dedup(agent);

        poll_reminder_pass(&home, &registry);

        assert!(
            crate::notification_queue::pending_count(&home, agent) > 0,
            "#event-bus Option A: gate-off must deliver via legacy (no regression)"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
