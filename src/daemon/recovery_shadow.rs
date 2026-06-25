//! #t-81376 Phase-0: 529-recovery failed-turn SHADOW telemetry. **Zero behaviour
//! change — measure-first.** When `AGEND_RECOVERY_SHADOW=1`, this side-logs (to
//! `recovery_shadow.jsonl`) the failed-turn discriminator's components at the
//! supervisor "gap arm" — the point where a fast `ServerRateLimit → Idle` is
//! treated as genuine recovery and the retry track is cleared / never built
//! (`clears_server_rate_limit_retry(Idle)`; a `Some(_)` clear with
//! `retry_count == 0` or no track at all). The daemon takes NO action on the
//! signal in Phase 0 — it only ever ADDS measurement.
//!
//! Soak goal — resolve three things from one log:
//!   1. the discriminator `(expectation) ∧ !recovered ∧ has_throttle_hint ∧
//!      ¬self_cleared` — its real FP/FN, recorded as components + `would_fire`;
//!   2. **does a 529 turn fire a Stop/StopFailure/ApiError HOOK?** — the
//!      dev-2 (lifecycle-only, raw-capture only) vs dev-3 (hook) conflict, by
//!      recording the agent's hook-shadow alongside the raw-capture marker;
//!   3. whether the existing SRL retry path already OWNS the episode
//!      (`had_retry_track` / `retry_count`).
//!
//! Invariants (shadow-family instrument):
//! - **D3 #2324 instrument-never-block**: [`arm_expectation`] and
//!   [`record_recovery_shadow`] return `()` (compiler-guaranteed un-`?`-able) and
//!   contain no `return`/`process::exit`; both are in the test's
//!   `INSTRUMENT_EMIT_FNS` allow-list. Off by default → early return → the real
//!   control flow is byte-identical.
//! - **D2 #2323 atomic write**: `recovery_shadow.jsonl` is an APPEND-only log, so
//!   it is written via the shadow-family `state::append_jsonl` (per-record
//!   `O_APPEND` atomicity) — the correct idiom for a log. D2 EXEMPTS
//!   `*.jsonl` appends (a whole-file `atomic_write` would be the wrong primitive).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// The dev-2 failed-turn discriminator (PURE — no env / IO / clock, so it is
/// deterministically testable): a recovery turn was EXPECTED, the agent produced
/// no productive output (`!recovered`), a transient-throttle marker is still in
/// the raw capture (`has_throttle_hint`), and the agent did not self-clear its
/// block. Phase 0 only RECORDS this verdict alongside its components — it NEVER
/// acts on it. Matches the active-phase fix dev-2 designed
/// (`d-20260617...` 529-recovery dialectic).
pub(crate) fn would_fire_discriminator(
    expectation: bool,
    recovered: bool,
    has_throttle_hint: bool,
    self_cleared: bool,
) -> bool {
    expectation && !recovered && has_throttle_hint && !self_cleared
}

/// Env-gate (the shadow-family env-flag idiom, e.g. `AGEND_PRODUCTIVE_GATE`).
/// Absent / not "1" ⇒ every entry point below early-returns ⇒ byte-identical behaviour.
pub(crate) fn enabled() -> bool {
    std::env::var("AGEND_RECOVERY_SHADOW")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Per-agent recovery-turn expectation: epoch-ms a daemon recovery turn
/// (`[AGEND-RESUME]` self-kick / recovery inject) was last armed. Phase-0
/// in-memory only (a daemon restart forgets in-flight expectations — acceptable
/// for shadow; a durable sidecar is the active-phase concern).
fn expectations() -> &'static Mutex<HashMap<String, u64>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-agent fire-once latch (last recorded signature) so a STATIC Idle frame
/// does not re-append the same observation every supervisor tick.
fn last_sig() -> &'static Mutex<HashMap<String, u64>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Shadow-arm a recovery-turn expectation for `agent` — called at the daemon's
/// `[AGEND-RESUME]` / recovery inject site. No-op unless the shadow is enabled.
/// `()` → control-flow-inert (D3).
pub(crate) fn arm_expectation(agent: &str) {
    if !enabled() {
        return;
    }
    expectations()
        .lock()
        .insert(agent.to_string(), crate::daemon::heartbeat_pair::now_ms());
}

/// Drop per-agent shadow state for agents no longer live (mirror the supervisor's
/// `retry_tracks.retain`). `()` → control-flow-inert.
pub(crate) fn retain_live(is_live: &dyn Fn(&str) -> bool) {
    if !enabled() {
        return;
    }
    expectations().lock().retain(|n, _| is_live(n));
    last_sig().lock().retain(|n, _| is_live(n));
}

/// The signals the gap arm already has in scope — passed in so the emit does NO
/// new hot-path computation beyond the hook-shadow read.
pub(crate) struct GapObservation<'a> {
    pub agent: &'a str,
    pub backend: &'a str,
    /// `recovered_within(RECOVERY_SILENCE)` — productive output since the turn.
    pub recovered: bool,
    /// `#2232` agent self-called `clear_blocked_reason` (awake ground-truth).
    pub self_cleared: bool,
    /// Raw-capture layer: a throttle/SRL marker is still visible in the tail.
    pub has_throttle_hint: bool,
    /// Existing-path ownership: was an SRL retry track present at this Idle?
    pub had_retry_track: bool,
    pub retry_count: u32,
    pub agent_state: &'a str,
    pub productive_silent_secs: u64,
}

/// Record ONE gap-arm shadow observation (the emit). `()` → control-flow-inert
/// (D3); all failures swallowed. Fires only on the "potentially-failed-turn"
/// Idles — an in-flight recovery expectation OR a visible throttle marker — so a
/// plain user-idle agent is silent.
pub(crate) fn record_recovery_shadow(home: &Path, obs: &GapObservation) {
    if !enabled() {
        return;
    }
    let expectation = expectations().lock().get(obs.agent).copied();
    if expectation.is_none() && !obs.has_throttle_hint {
        return; // boring Idle — nothing to measure
    }
    // dev-2 failed-turn discriminator (Phase 0 takes NO action on this — it is
    // recorded so FP/FN is computable offline).
    let would_fire = would_fire_discriminator(
        expectation.is_some(),
        obs.recovered,
        obs.has_throttle_hint,
        obs.self_cleared,
    );

    // HOOK LAYER (the Phase-0 raison d'être: does a 529 turn fire a Stop/
    // StopFailure/ApiError HOOK?). Record the ACTUAL HookShadow snapshot fields,
    // NOT just a 600s-fresh resolution (r6 #2332): `last_event` + `at_ms` are
    // what let analysis decide whether the hook belongs to THIS recovery turn
    // (`hook_age_vs_expectation_ms > 0` ⇒ hook fired after the turn was injected)
    // rather than a prior fresh hook. `resolved_state_for`'s freshness verdict is
    // kept as a secondary signal only.
    let hook_snap = crate::daemon::hook_shadow::snapshot_for(obs.agent);
    let hook_resolution = crate::daemon::hook_shadow::resolved_state_for(obs.agent);
    let hook_age_vs_expectation_ms = match (hook_snap.as_ref(), expectation) {
        (Some(s), Some(exp)) => Some(s.at_ms as i64 - exp as i64),
        _ => None,
    };

    let now = crate::daemon::heartbeat_pair::now_ms();
    let record = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "agent": obs.agent,
        "backend": obs.backend,
        // ── discriminator components ──
        "expectation_armed": expectation.is_some(),
        "expectation_age_ms": expectation.map(|t| now.saturating_sub(t)),
        "recovered": obs.recovered,
        "self_cleared": obs.self_cleared,
        "has_throttle_hint": obs.has_throttle_hint,
        "would_fire": would_fire,
        // ── hook layer (529-fires-a-hook question) — real snapshot fields ──
        "hook_backend": hook_snap.is_some(),
        "hook_last_event": hook_snap.as_ref().map(|s| s.last_event.clone()),
        "hook_derived_state": hook_snap
            .as_ref()
            .map(|s| s.derived_state.map(|st| format!("{st:?}"))),
        "hook_at_ms": hook_snap.as_ref().map(|s| s.at_ms),
        "hook_age_vs_expectation_ms": hook_age_vs_expectation_ms,
        "last_user_prompt_submit_ms": hook_snap
            .as_ref()
            .and_then(|s| s.last_user_prompt_submit_ms),
        "hook_resolution": format!("{hook_resolution:?}"),
        // ── existing-path ownership ──
        "had_retry_track": obs.had_retry_track,
        "retry_count": obs.retry_count,
        // ── state / raw-capture ──
        "agent_state": obs.agent_state,
        "productive_silent_secs": obs.productive_silent_secs,
    });

    // Fire-once latch keyed on the components that define this observation, so a
    // static throttle-hint Idle that churns the screen hash logs once — BUT the
    // hook snapshot (`last_event`, `at_ms`) IS part of the key (r6 #2332 re-review):
    // a gap-arm Idle observed BEFORE the recovery turn's hook arrives latches with
    // no/stale hook; when the hook lands a later tick must re-fire (new `at_ms` →
    // new sig) so the hook-bearing record — the soak's whole point — is captured.
    // The latch still dedups a genuinely-static frame (unchanged hook → same sig),
    // so this re-fires once per NEW hook event, not per tick.
    let sig = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        obs.agent.hash(&mut h);
        would_fire.hash(&mut h);
        obs.has_throttle_hint.hash(&mut h);
        obs.retry_count.hash(&mut h);
        expectation.hash(&mut h);
        hook_snap
            .as_ref()
            .map(|s| (s.last_event.as_str(), s.at_ms))
            .hash(&mut h);
        h.finish()
    };
    {
        let mut latch = last_sig().lock();
        if latch.get(obs.agent) == Some(&sig) {
            return;
        }
        latch.insert(obs.agent.to_string(), sig);
    }

    // Best-effort O_APPEND (the shadow-family helper — per-record atomic, the
    // correct idiom for an append-only log; D2 #2323 EXEMPTS *.jsonl appends).
    // Any failure is swallowed: the diagnostic must never affect control flow (D3).
    let path = home.join("recovery_shadow.jsonl");
    if let Err(e) = crate::state::append_jsonl(&path, &record) {
        tracing::debug!(
            target: "recovery_shadow",
            error = %e,
            "Phase-0 shadow: append failed (swallowed)"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique(tag: &str) -> String {
        static C: AtomicU32 = AtomicU32::new(0);
        format!(
            "rs-{tag}-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(unique(tag));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn obs<'a>(
        agent: &'a str,
        recovered: bool,
        hint: bool,
        self_cleared: bool,
    ) -> GapObservation<'a> {
        GapObservation {
            agent,
            backend: "claude",
            recovered,
            self_cleared,
            has_throttle_hint: hint,
            had_retry_track: false,
            retry_count: 0,
            agent_state: "Idle",
            productive_silent_secs: 42,
        }
    }

    fn records(home: &std::path::Path) -> Vec<serde_json::Value> {
        let p = home.join("recovery_shadow.jsonl");
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Pure discriminator truth table — deterministic, no env / IO.
    #[test]
    fn discriminator_fires_only_on_expectation_no_recovery_hint_no_selfclear() {
        // The one TRUE row: expectation ∧ !recovered ∧ has_hint ∧ !self_cleared.
        assert!(would_fire_discriminator(true, false, true, false));
        // Each component flipped suppresses it.
        assert!(!would_fire_discriminator(false, false, true, false)); // no expectation
        assert!(!would_fire_discriminator(true, true, true, false)); // recovered (normal completion)
        assert!(!would_fire_discriminator(true, false, false, false)); // no throttle marker
        assert!(!would_fire_discriminator(true, false, true, true)); // self-cleared (awake)
    }

    #[test]
    #[serial]
    fn disabled_is_a_noop() {
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        let home = tmp_home("disabled");
        let a = unique("agent");
        arm_expectation(&a); // no-op (gate off)
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        assert!(
            records(&home).is_empty(),
            "no file/records when the gate is off"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial]
    fn would_fire_record_carries_all_layers() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("fire");
        let a = unique("agent");
        arm_expectation(&a);
        // expectation armed + !recovered + throttle hint + !self_cleared → fires.
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        let recs = records(&home);
        assert_eq!(recs.len(), 1, "one shadow record");
        let r = &recs[0];
        assert_eq!(r["would_fire"], true);
        assert_eq!(r["expectation_armed"], true);
        assert_eq!(r["recovered"], false);
        assert_eq!(r["has_throttle_hint"], true); // raw-capture layer
        assert_eq!(r["self_cleared"], false);
        // hook layer — the real snapshot field KEYS must be present (null here:
        // this agent has no hook entry) so analysis can always read them.
        for k in [
            "hook_backend",
            "hook_last_event",
            "hook_derived_state",
            "hook_at_ms",
            "hook_age_vs_expectation_ms",
            "last_user_prompt_submit_ms",
            "hook_resolution",
        ] {
            assert!(r.get(k).is_some(), "hook field `{k}` must be in the record");
        }
        assert_eq!(r["had_retry_track"], false); // existing-path ownership
        assert_eq!(r["agent_state"], "Idle");
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }

    /// r6 #2332: the ACTUAL HookShadow snapshot fields must be recorded (not just
    /// a 600s-fresh resolution), so the soak can tell whether a Stop/StopFailure/
    /// ApiError hook fired for THIS recovery turn. Record a StopFailure hook
    /// after arming the expectation, then assert the snapshot fields land.
    #[test]
    #[serial]
    fn hook_snapshot_fields_are_captured_not_discarded() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("hooksnap");
        let a = unique("agent");
        arm_expectation(&a);
        // A real hook event for this agent (StopFailure → ApiError, fired after
        // the expectation was armed).
        crate::daemon::hook_shadow::record_event(&a, "StopFailure", None);
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        let recs = records(&home);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r["hook_backend"], true);
        assert_eq!(
            r["hook_last_event"], "StopFailure",
            "the raw hook event name must be recorded verbatim"
        );
        assert!(
            r["hook_at_ms"].is_u64(),
            "the hook receipt time must be recorded"
        );
        // hook fired AFTER the expectation → non-negative age → belongs to this turn.
        assert!(
            r["hook_age_vs_expectation_ms"].as_i64().unwrap() >= 0,
            "age vs expectation distinguishes this-turn hooks from prior ones"
        );
        crate::daemon::hook_shadow::forget(&a);
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial]
    fn recovered_idle_records_but_not_fires() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("recovered");
        let a = unique("agent");
        arm_expectation(&a);
        // recovered=true (normal completion) but a stale throttle hint is visible:
        // still recorded (so FP is measurable) but would_fire=false.
        record_recovery_shadow(&home, &obs(&a, true, true, false));
        let recs = records(&home);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["would_fire"], false);
        assert_eq!(recs[0]["recovered"], true);
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial]
    fn boring_idle_no_expectation_no_hint_is_skipped() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("boring");
        let a = unique("agent");
        // never armed + no throttle hint → not an interesting Idle → no record.
        record_recovery_shadow(&home, &obs(&a, false, false, false));
        assert!(records(&home).is_empty());
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial]
    fn fire_once_latch_dedups_identical_observation() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("latch");
        let a = unique("agent");
        arm_expectation(&a);
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        record_recovery_shadow(&home, &obs(&a, false, true, false)); // identical → deduped
        assert_eq!(
            records(&home).len(),
            1,
            "fire-once latch collapses the repeat"
        );
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }

    /// r6 #2332 re-review: a hook that ARRIVES after the first gap-arm Idle was
    /// latched must NOT be deduped away — the latch key includes the hook
    /// snapshot, so a new hook event re-fires and the hook-bearing record (the
    /// soak's whole point) is captured.
    #[test]
    #[serial]
    fn hook_arrival_after_latch_re_fires() {
        std::env::set_var("AGEND_RECOVERY_SHADOW", "1");
        let home = tmp_home("hooklatch");
        let a = unique("agent");
        arm_expectation(&a);
        // First Idle observation — no hook yet → latches.
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        assert_eq!(records(&home).len(), 1);
        // The recovery turn's hook now arrives.
        crate::daemon::hook_shadow::record_event(&a, "StopFailure", None);
        // Same discriminator inputs, but the hook changed → must re-fire.
        record_recovery_shadow(&home, &obs(&a, false, true, false));
        let recs = records(&home);
        assert_eq!(
            recs.len(),
            2,
            "a newly-arrived hook must NOT be deduped by the latch"
        );
        assert_eq!(
            recs[1]["hook_last_event"], "StopFailure",
            "the second record carries the arrived hook"
        );
        // hook fired AFTER the expectation → attributed to THIS recovery turn.
        assert!(
            recs[1]["hook_age_vs_expectation_ms"].as_i64().unwrap() >= 0,
            "the re-fired record attributes the hook to this turn (age >= 0)"
        );
        crate::daemon::hook_shadow::forget(&a);
        std::env::remove_var("AGEND_RECOVERY_SHADOW");
        std::fs::remove_dir_all(&home).ok();
    }
}
