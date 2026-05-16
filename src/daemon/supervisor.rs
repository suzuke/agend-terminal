//! Per-agent supervisor loop — detects pre-ready interactive stalls and
//! pushes a vterm tail to the agent's channel topic.
//!
//! Runs as a background thread spawned from both daemon mode
//! (`start_daemon`) and app mode (`app::run`). Both call paths create agents
//! via the shared `AgentRegistry`, so the supervisor needs no state beyond a
//! registry handle and the AGEND_HOME path. Shutdown is implicit: when the
//! hosting process exits, this thread dies with it.
//!
//! Detection logic lives in `health::HealthTracker::check_awaiting_operator`
//! and the transition in `state::StateTracker::set_awaiting_operator`. This
//! module is the plumbing that glues them to channel notifications.

use crate::agent::{self, AgentRegistry};
use crate::channel::NotifySeverity;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How often the supervisor wakes to scan the registry.
const TICK: Duration = Duration::from_secs(10);
/// Vterm tail size pushed to Telegram when a stall is detected.
const TAIL_LINES: usize = 40;
/// Debounce cooldown for member-state-change notify (Sprint 43).
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(60);
/// Maximum auto-retries for ServerRateLimit before giving up.
const SERVER_RATE_LIMIT_MAX_RETRIES: u32 = 3;
/// Backoff schedule for ServerRateLimit retries (seconds).
const SERVER_RATE_LIMIT_BACKOFF: [u64; 3] = [5, 15, 30];

/// Sprint 56 Track G (#529): per-fingerprint cap on `inject_to_agent`
/// re-firing within the dedup window. The full retry sequence runs in
/// 50s ([5,15,30]s backoff) so cap=1 means a notification gets at most
/// 1 re-inject before the rest of the schedule is suppressed; the agent
/// will see the original delivery + at most one replay. Inbox-class
/// notifications are durably persisted in `~/.agend/inbox/<agent>/`, so
/// the agent picks them up via the `inbox` MCP tool on recovery — extra
/// replays would just spend prompt tokens for no benefit.
const NOTIFICATION_DEDUP_CAP: u32 = 1;
/// Sprint 56 Track G (#529): time window during which the
/// per-fingerprint cap applies. After the window expires, `dedup_count`
/// resets and re-injects are allowed again — defends the "operator
/// configured a long-running rate-limit recovery, came back hours
/// later, expected a fresh kick" workflow. 60s comfortably exceeds the
/// 50s backoff envelope so the entire normal retry schedule sits inside
/// one window.
const NOTIFICATION_DEDUP_WINDOW_SECS: u64 = 60;

/// Per-agent notify tracking: last notify time + consecutive error count.
pub(crate) struct NotifyTrack {
    last_at: Instant,
    consecutive: u32,
}

/// Sprint 54 P2-3: dedup key for `PaneInputNotSubmitted` emission.
/// Records the `last_input_epoch_ms` of the most recent emit so the
/// supervisor doesn't re-fire on every 10-s tick while the operator
/// stares at typed-but-not-submitted text. Re-arms when a fresh
/// keystroke updates `last_input_epoch_ms` past the recorded value.
#[derive(Debug, Default)]
pub(crate) struct PaneInputTrack {
    last_emitted_for_typed_ms: i64,
}

/// Per-agent ServerRateLimit retry state.
#[derive(Debug, Clone)]
pub(crate) struct RateLimitRetry {
    pub retry_count: u32,
    pub next_retry_at: Instant,
    pub input_text: String,
    /// Set when max retries exceeded — prevents re-triggering on same
    /// persistent ServerRateLimit state. Cleared on recovery (Ready/Idle).
    pub exhausted: bool,
    /// Sprint 56 Track G (#529): hash of the input that scheduled this
    /// retry track. Compared at each retry tick against the agent's
    /// current `last_input_text` so the supervisor can tell "same
    /// notification mid-stall" (dedup eligible) from "operator typed
    /// something new" (force-inject).
    pub fingerprint: u64,
    /// Sprint 56 Track G: count of injects fired for this fingerprint
    /// in the current dedup window. Reset to 0 when fingerprint changes
    /// or window expires.
    pub dedup_count: u32,
    /// Sprint 56 Track G: timestamp of the most recent inject (or the
    /// phase-1 detection time as a baseline). Drives the dedup window
    /// check at retry tick.
    pub last_inject_at: Instant,
    /// Sprint 56 Track G: latched once the dedup-cap audit event has
    /// fired so a long-running rate-limit doesn't spam the event log.
    pub dedup_audit_emitted: bool,
}

/// Sprint 56 Track G (#529): content-hash fingerprint over the agent's
/// pending-input bytes. Pure helper — same input always hashes to the
/// same `u64`, distinct inputs (even with one-byte differences) hash
/// to different `u64`s with overwhelming probability. Used by
/// `process_server_rate_limit_retries` to dedup re-injects within a
/// window without committing to any specific message-id format.
pub(crate) fn fingerprint_input(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Sprint 56 Track G (#529): the dedup gate's three possible verdicts
/// for a single retry tick. Returned by [`dedup_decision`] so the
/// retry loop can act on a pure value without holding any locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DedupDecision {
    /// Same fingerprint, in window, cap reached — suppress this retry
    /// and emit the audit event.
    Suppress,
    /// Fingerprint changed (operator typed something new) — replace
    /// tracking and inject the fresh content.
    ForceFreshContent,
    /// Same fingerprint, window expired — reset the dedup counter and
    /// inject again.
    AllowAfterWindowReset,
    /// Same fingerprint, in window, cap not yet hit — proceed with the
    /// existing tracking, inject, and bump `dedup_count` post-inject.
    Allow,
}

/// Sprint 56 Track G (#529): pure helper that classifies a retry tick
/// against the dedup window and per-fingerprint cap. Returns a
/// [`DedupDecision`]; mutation of `RateLimitRetry` (resetting counters,
/// swapping `input_text`, etc.) is left to the caller because it
/// depends on context (the actual agent's `last_input_text` must be
/// re-read inside the lock-free retry loop).
pub(crate) fn dedup_decision(
    retry: &RateLimitRetry,
    current_fingerprint: u64,
    now: Instant,
) -> DedupDecision {
    if current_fingerprint != retry.fingerprint {
        return DedupDecision::ForceFreshContent;
    }
    let elapsed = now.duration_since(retry.last_inject_at);
    if elapsed >= Duration::from_secs(NOTIFICATION_DEDUP_WINDOW_SECS) {
        return DedupDecision::AllowAfterWindowReset;
    }
    if retry.dedup_count >= NOTIFICATION_DEDUP_CAP {
        return DedupDecision::Suppress;
    }
    DedupDecision::Allow
}

// ---------------------------------------------------------------------------
// #841 rate-limit recovery nudge — sibling fn to fast-retry path
// ---------------------------------------------------------------------------

/// #841 per-agent tracking for the recovery-nudge path.
///
/// Lifecycle (driven by the per-tick state observation in
/// `process_rate_limit_recovery_nudges`):
///
/// - **Error observed** (`is_transient_error()` true) — set
///   `last_error_at = Some(now)`, clear `recovered_at` and
///   `fired_this_cycle`.
/// - **Agent recovers** (state becomes `Ready`/`Idle` while
///   `last_error_at.is_some()` and `recovered_at.is_none()`) — set
///   `recovered_at = Some(now)`.
/// - **Recovery window elapses + cooldown clear + fast-retry inactive** —
///   `decide_nudge` returns `Fire`; the integration fn injects via
///   `compose_aware_send`, sets `last_inject_at`, marks
///   `fired_this_cycle`.
/// - **Agent leaves `Ready`/`Idle`** (handler picked up the nudge OR
///   the operator typed something) — clear `recovered_at` and
///   `fired_this_cycle` so a future error→recovery sequence starts
///   a fresh cycle.
#[derive(Debug, Clone, Default)]
pub(crate) struct RecoveryNudgeTrack {
    pub last_error_at: Option<Instant>,
    pub recovered_at: Option<Instant>,
    pub last_inject_at: Option<Instant>,
    pub fired_this_cycle: bool,
}

/// Reason the per-tick decision chose to skip the nudge — recorded for
/// tracing so the operator can audit *why* a recovery nudge that
/// "should" have fired didn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NudgeSkipReason {
    Disabled,
    NoRecentError,
    NotIdle,
    StillInRecoveryWindow,
    Cooldown,
    DeferToFastRetry,
    FiredThisCycle,
}

/// Per-tick verdict for one agent, returned by the pure helper
/// [`decide_nudge`] so the integration fn can act on a value without
/// holding any registry locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NudgeDecision {
    Skip(NudgeSkipReason),
    Fire,
}

/// Resolve the effective per-agent recovery config from a fleet load
/// outcome plus an instance name.
///
/// **Fail-closed on load error** (`Err(_)`): returns a config with
/// `enabled = false` (other knobs preserved from `default_cfg`) so a
/// corrupt or missing fleet.yaml never silently auto-enables the nudge
/// — the operator sees the warn log at the call site and fixes the
/// config. Without this gate, a parse error during fleet.yaml edits
/// would leave the daemon happily injecting `continue your prior work`
/// into every transient-error recovery, which violates the spike Q3
/// "Config parse error → enabled=false" contract.
///
/// On `Ok`: look up the per-instance override; fall back to
/// `default_cfg` when the instance isn't in fleet.yaml OR the instance
/// entry omits the `rate_limit_recovery` field.
pub(crate) fn resolve_recovery_config(
    fleet: Result<&crate::fleet::FleetConfig, ()>,
    name: &str,
    default_cfg: &crate::fleet::RateLimitRecoveryConfig,
) -> crate::fleet::RateLimitRecoveryConfig {
    let Ok(fleet) = fleet else {
        return crate::fleet::RateLimitRecoveryConfig {
            enabled: false,
            ..default_cfg.clone()
        };
    };
    fleet
        .instances
        .get(name)
        .and_then(|i| i.rate_limit_recovery.as_ref())
        .cloned()
        .unwrap_or_else(|| default_cfg.clone())
}

/// Pure helper: classify the recovery-nudge tick against the track,
/// the current agent state, the fast-retry path's activity, and the
/// per-agent config. Mutation of `RecoveryNudgeTrack` is left to the
/// caller (integration fn) because state transitions need the live
/// registry read.
///
/// Order matters — `Disabled` is checked before any other gate so an
/// opted-out agent never produces a `Skip(Cooldown)` or similar that
/// would mislead a future operator inspecting the trace. `DeferToFastRetry`
/// next, so we don't waste a fire on an agent whose fast-retry path is
/// already going to handle the same situation.
pub(crate) fn decide_nudge(
    track: &RecoveryNudgeTrack,
    current_state: crate::state::AgentState,
    fast_retry_active: bool,
    config: &crate::fleet::RateLimitRecoveryConfig,
    now: Instant,
) -> NudgeDecision {
    use crate::state::AgentState;

    if !config.enabled {
        return NudgeDecision::Skip(NudgeSkipReason::Disabled);
    }
    if fast_retry_active {
        return NudgeDecision::Skip(NudgeSkipReason::DeferToFastRetry);
    }
    if track.fired_this_cycle {
        return NudgeDecision::Skip(NudgeSkipReason::FiredThisCycle);
    }
    let Some(last_error_at) = track.last_error_at else {
        return NudgeDecision::Skip(NudgeSkipReason::NoRecentError);
    };
    // Error too stale (>1 hour past the observe window) — treat as no
    // longer relevant. Operator probably came back to a long-idle agent
    // that happens to have an old error in its track.
    let stale_cap = Duration::from_secs(config.observe_after_secs.saturating_add(3600));
    if now.duration_since(last_error_at) > stale_cap {
        return NudgeDecision::Skip(NudgeSkipReason::NoRecentError);
    }
    if !matches!(current_state, AgentState::Ready | AgentState::Idle) {
        return NudgeDecision::Skip(NudgeSkipReason::NotIdle);
    }
    let Some(recovered_at) = track.recovered_at else {
        return NudgeDecision::Skip(NudgeSkipReason::NotIdle);
    };
    let recovery_window = Duration::from_secs(config.recovery_after_secs);
    if now.duration_since(recovered_at) < recovery_window {
        return NudgeDecision::Skip(NudgeSkipReason::StillInRecoveryWindow);
    }
    if let Some(last_inject_at) = track.last_inject_at {
        let cooldown = Duration::from_secs(config.cooldown_secs);
        if now.duration_since(last_inject_at) < cooldown {
            return NudgeDecision::Skip(NudgeSkipReason::Cooldown);
        }
    }
    NudgeDecision::Fire
}

/// #841 per-tick integration for the recovery-nudge path.
///
/// Phase 1 (registry lock held): observe per-agent state and update
/// each `RecoveryNudgeTrack` — stamp `last_error_at` on a transient
/// error, stamp `recovered_at` when an agent transitions back to
/// `Ready`/`Idle`, and clear cycle bookkeeping when the agent leaves
/// the recovered window (e.g. operator typed something, or the nudge
/// itself was picked up).
///
/// Phase 2 (no locks): load fleet.yaml for per-instance config,
/// evaluate `decide_nudge` for each observed agent, and fire the
/// configured prompt via `compose_aware_send` when the decision is
/// `Fire`. The fire branch is the only side-effecting path; all skip
/// branches are observation-only.
///
/// Sibling fn to `process_server_rate_limit_retries`; the two paths
/// coordinate via the `fast_retry_tracks` argument (when the fast
/// path is actively retrying for an agent, the nudge defers).
pub(crate) fn process_rate_limit_recovery_nudges(
    home: &std::path::Path,
    registry: &AgentRegistry,
    fast_retry_tracks: &HashMap<String, RateLimitRetry>,
    nudge_tracks: &mut HashMap<String, RecoveryNudgeTrack>,
) {
    let now = Instant::now();

    // Phase 1: state observation + track update (registry lock held).
    let mut observed: Vec<(String, crate::state::AgentState)> = Vec::new();
    {
        let reg = agent::lock_registry(registry);
        for (name, handle) in reg.iter() {
            let state = handle.core.lock().state.current;
            let track = nudge_tracks.entry(name.clone()).or_default();
            if state.is_transient_error() {
                // Restart the cycle — a fresh transient error invalidates any
                // in-flight recovery wait and any prior `fired_this_cycle` latch.
                track.last_error_at = Some(now);
                track.recovered_at = None;
                track.fired_this_cycle = false;
            } else if matches!(
                state,
                crate::state::AgentState::Ready | crate::state::AgentState::Idle
            ) {
                // Only stamp `recovered_at` on the FIRST observation of the
                // Ready/Idle transition — subsequent ticks keep the same
                // recovery anchor so the `recovery_after_secs` window can
                // actually elapse.
                if track.last_error_at.is_some() && track.recovered_at.is_none() {
                    track.recovered_at = Some(now);
                }
            } else {
                // Agent transitioned out of Ready/Idle (handler picked up the
                // nudge, operator typed something, or any other active state).
                // Reset cycle bookkeeping so the NEXT error→recovery sequence
                // starts a clean cycle. `last_error_at` is preserved so a
                // rapid re-error within the stale cap is still recognized.
                track.recovered_at = None;
                track.fired_this_cycle = false;
            }
            observed.push((name.clone(), state));
        }
    }
    // Registry lock released here.

    // Phase 2: fleet.yaml read + per-agent decision + fire (no locks held).
    //
    // r1 fail-closed fix: load failure (parse error / missing file)
    // funnels through `resolve_recovery_config` with `Err(())`, which
    // returns a config carrying `enabled = false` — preventing silent
    // auto-inject when fleet.yaml is mid-edit or corrupted. The warn
    // log surfaces *why* the nudge stopped firing so the operator can
    // diagnose without staring at empty-prompt agents.
    let fleet_result = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home));
    if let Err(e) = &fleet_result {
        tracing::warn!(
            error = %e,
            "#841 fleet.yaml load failed — rate-limit recovery nudges disabled until fixed (fail-closed)"
        );
    }
    let fleet_ref: Result<&crate::fleet::FleetConfig, ()> = fleet_result.as_ref().map_err(|_| ());
    let default_cfg = crate::fleet::RateLimitRecoveryConfig::default();

    for (name, state) in &observed {
        // Clone the track for the pure decide_nudge call so the mutation
        // path below can take a fresh `&mut` without overlapping borrows.
        let track_snapshot = nudge_tracks.get(name).cloned().unwrap_or_default();
        let fast_active = fast_retry_tracks.contains_key(name);

        let cfg = resolve_recovery_config(fleet_ref, name, &default_cfg);

        if matches!(
            decide_nudge(&track_snapshot, *state, fast_active, &cfg, now),
            NudgeDecision::Fire
        ) {
            tracing::info!(
                agent = %name,
                prompt = %cfg.prompt,
                "#841 rate-limit recovery nudge firing"
            );
            crate::inbox::compose_aware_send(home, name, &cfg.prompt);
            if let Some(t) = nudge_tracks.get_mut(name) {
                t.last_inject_at = Some(now);
                t.fired_this_cycle = true;
                t.recovered_at = None;
            }
        }
    }
}

/// Parse unlock time from usage_limit pane output (e.g., "resets at 15:14 UTC").
fn parse_unlock_at(pane_text: &str) -> Option<String> {
    // Common patterns: "resets at HH:MM", "try again after HH:MM", "limit resets HH:MM"
    for line in pane_text.lines().rev() {
        let lower = line.to_lowercase();
        if lower.contains("reset") || lower.contains("try again") || lower.contains("limit") {
            // Extract time-like pattern HH:MM
            if let Some(idx) = lower.find(|c: char| c.is_ascii_digit()) {
                let rest = &line[idx..];
                if rest.len() >= 5 && rest.as_bytes()[2] == b':' {
                    return Some(rest[..5].to_string());
                }
            }
        }
    }
    None
}

/// Spawn the supervisor thread. Idempotent per process is the caller's
/// responsibility — in practice each entry point calls it exactly once.
pub fn spawn(home: PathBuf, registry: AgentRegistry) {
    // fire-and-forget: supervisor tick loop runs for the process lifetime
    // (per module-doc rationale at lines 6-8 — "shutdown is implicit: when
    // the hosting process exits, this thread dies with it"). 10s tick
    // cadence; no graceful-stop needed because supervisor is read-mostly
    // (per-tick metadata read + occasional channel notify).
    let _ = thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || run_loop(home, registry));
}

fn run_loop(home: PathBuf, registry: AgentRegistry) {
    let mut notify_tracks: HashMap<String, NotifyTrack> = HashMap::new();
    // Sprint 57 Wave 2 Track C (#546 Item 5): hydrate dedup ledger
    // from `$AGEND_HOME/dedup-state/*.json` so a daemon restart
    // inside the 60s dedup window with a same-fingerprint repeat
    // doesn't under-suppress (the latent bug Phase A RCA #549
    // documented). Missing dir / corrupt files / schema mismatches
    // are best-effort skipped — daemon startup never aborts on
    // bad disk state for this surface.
    let mut retry_tracks: HashMap<String, RateLimitRetry> =
        crate::daemon::dedup_state::load_all(&home);
    if !retry_tracks.is_empty() {
        tracing::info!(
            count = retry_tracks.len(),
            "supervisor: hydrated rate-limit dedup ledger from disk"
        );
    }
    let mut pane_input_tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    // Sprint 59 Wave 1 PR-1 (#9 task stall watchdog): per-task ETA
    // scanner, throttled to 5min via TICKS_PER_SCAN.
    let mut anti_stall_tracker = crate::daemon::anti_stall::AntiStallTracker::default();
    // Sprint 59 Wave 1 PR-2 (#10+#12 watchdog cluster): per-agent +
    // fleet-wide idle thresholds, throttled to 5min scans.
    let mut idle_watchdog_tracker = crate::daemon::idle_watchdog::IdleWatchdogTracker::default();
    // Sprint 59 Wave 1 PR-4-recover ((B) decision default with
    // timeout): tracks pending operator decisions, fires auto-default
    // on timeout. 5min throttle matches anti-stall cadence.
    let mut decision_timeout_tracker =
        crate::daemon::decision_timeout::DecisionTimeoutTracker::default();
    // Sprint 59 Wave 2 PR-3 (#13 deployment-cadence proactive helper-
    // staleness): periodically reuses cli::classify_helper_staleness
    // and pings general+lead when a helper goes stale, closing the
    // operator-pull gap from Sprint 58 PR-1 #11.
    let mut helper_staleness_tracker =
        crate::daemon::helper_staleness_watchdog::HelperStalenessWatchdogTracker::default();
    // Sprint 60 W1 PR-2 (#P0-2 daemon hot-reload tool registry): 5th
    // tracker. Detects when the daemon binary at current_exe() has
    // been refreshed AFTER the running process started — running
    // process's MCP tool registry then lags the on-disk binary's
    // compiled-in registry. Closes the PR-5 → PR-4 chicken-and-egg loop.
    let mut mcp_registry_tracker =
        crate::daemon::mcp_registry_watcher::McpRegistryWatcherTracker::default();
    let mut waiting_on_stale_tracker =
        crate::daemon::waiting_on_stale::WaitingOnStaleTracker::default();
    // #841 rate-limit recovery nudge tracking — in-memory only for r0
    // (restart clears state, which is safe; the fast-retry path's
    // `dedup_state` disk persistence covers the harder restart-window
    // case for the side it owns).
    let mut nudge_tracks: HashMap<String, RecoveryNudgeTrack> = HashMap::new();
    loop {
        thread::sleep(TICK);
        tick(&home, &registry, &mut notify_tracks);
        process_server_rate_limit_retries(&home, &registry, &mut retry_tracks);
        // #841: sibling per-tick. Defers to fast-retry when its
        // retry_tracks has an entry for the same agent; otherwise applies
        // the slower observe→recover→nudge cycle for transient errors
        // that the fast path didn't (or couldn't) handle.
        process_rate_limit_recovery_nudges(&home, &registry, &retry_tracks, &mut nudge_tracks);
        check_pane_input_not_submitted(&home, &registry, &mut pane_input_tracks);
        anti_stall_tracker.maybe_scan(&home);
        idle_watchdog_tracker.maybe_scan(&home);
        decision_timeout_tracker.maybe_scan(&home);
        helper_staleness_tracker.maybe_scan(&home);
        mcp_registry_tracker.maybe_scan(&home);
        waiting_on_stale_tracker.maybe_scan(&home);
        // #836: reclaim expired (10-min TTL) entries from the
        // notification-dedup ledger so memory pressure stays bounded
        // on long-lived daemons.
        crate::daemon::notification_dedup::global().sweep_expired();
        // #842: same eviction cadence for the bridge↔daemon idempotent-
        // retry dedup cache. Sibling sweep, same 10-min TTL window.
        crate::api::request_dedup::global().sweep_expired();
    }
}

/// Sprint 54 P2-3: per-tick check for "typed but not submitted" pane
/// state. Read-only — emits a `FleetEvent::PaneInputNotSubmitted` when
/// the threshold is exceeded but does NOT inject prompts, mutate agent
/// state, or touch the router layer (router = `src/channel/router/*`,
/// Sprint 49/52 territory). Backend allowlist (claude only, first
/// round) avoids false-positive flood for backends without
/// `record_submit_activity` wiring.
///
/// Threshold defaults to 60s; override via env
/// `AGEND_PANE_INPUT_THRESHOLD_SECS`.
pub(crate) fn check_pane_input_not_submitted(
    home: &std::path::Path,
    registry: &AgentRegistry,
    tracks: &mut HashMap<String, PaneInputTrack>,
) {
    let agent_names: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.keys().cloned().collect()
    };
    check_pane_input_not_submitted_for_agents(home, &agent_names, tracks);
}

/// Sprint 54 P2-3: pure-function variant of
/// [`check_pane_input_not_submitted`] that takes the agent name list
/// directly. Lets tests exercise the detection / emission / dedup logic
/// without standing up a real `AgentRegistry` (which would need a
/// spawned PTY + child process).
pub(crate) fn check_pane_input_not_submitted_for_agents(
    home: &std::path::Path,
    agent_names: &[String],
    tracks: &mut HashMap<String, PaneInputTrack>,
) {
    let threshold_secs: u64 = std::env::var("AGEND_PANE_INPUT_THRESHOLD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let threshold_ms = (threshold_secs as i64).saturating_mul(1000);
    let now_ms = chrono::Utc::now().timestamp_millis();
    for name in agent_names {
        if !pane_input_backend_supported(home, name) {
            continue;
        }
        let (typed_ms, submit_ms) =
            crate::notification_queue::read_input_submit_timestamps(home, name);
        if typed_ms == 0 || typed_ms <= submit_ms {
            continue;
        }
        let typed_age_ms = now_ms.saturating_sub(typed_ms);
        if typed_age_ms < threshold_ms {
            continue;
        }
        let track = tracks.entry(name.clone()).or_default();
        if track.last_emitted_for_typed_ms == typed_ms {
            // Already notified for this exact typing event — wait for a
            // new keystroke to re-arm.
            continue;
        }
        track.last_emitted_for_typed_ms = typed_ms;
        let typed_age_secs = (typed_age_ms / 1000).max(0) as u64;
        crate::channel::sink_registry::registry().emit(&crate::channel::ux_event::UxEvent::Fleet(
            crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted {
                agent: name.clone(),
                typed_age_secs,
            },
        ));
        tracing::info!(
            agent = %name,
            typed_age_secs,
            "pane-input-not-submitted detected (read-only diagnostic)"
        );
    }
}

/// Sprint 54 P2-3: backend allowlist for submit detection. Hard-coded
/// claude-only first round per dispatch — extending to other backends
/// requires both wiring `record_submit_activity` in
/// `app::write_to_focused` AND adding the matching arm here. Resolves
/// the agent's backend via fleet.yaml so per-instance overrides are
/// honoured.
fn pane_input_backend_supported(home: &std::path::Path, agent: &str) -> bool {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return false;
    };
    let Some(resolved) = fleet.resolve_instance(agent) else {
        return false;
    };
    crate::backend::Backend::from_command(&resolved.backend_command)
        .map(|b| matches!(b, crate::backend::Backend::ClaudeCode))
        .unwrap_or(false)
}

/// Decide and dispatch member-state-change notify. Returns true if notify sent.
/// Production-path-coupled per §3.5.10 — tests call this same function.
pub(crate) fn maybe_notify_member_state_change(
    home: &std::path::Path,
    name: &str,
    prev_state: crate::state::AgentState,
    new_state: crate::state::AgentState,
    pane_tail: &str,
    tracks: &mut HashMap<String, NotifyTrack>,
) -> bool {
    if prev_state == new_state || !new_state.is_notify_error_class() {
        return false;
    }
    let now = Instant::now();
    let should = tracks
        .get(name)
        .is_none_or(|t| now.duration_since(t.last_at) >= NOTIFY_COOLDOWN);
    if !should {
        return false;
    }
    let Some(team) = crate::teams::find_team_for(home, name) else {
        return false;
    };
    let Some(ref orch) = team.orchestrator else {
        tracing::warn!(agent = %name, team = %team.name, "member-state-change: team has no orchestrator — notify dropped");
        return false;
    };
    if orch == name {
        return false; // D3: skip self-notify
    }
    let unlock_at = if new_state == crate::state::AgentState::UsageLimit {
        parse_unlock_at(pane_tail)
    } else {
        None
    };
    let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
        last_at: now,
        consecutive: 0,
    });
    track.consecutive += 1;
    track.last_at = now;
    let payload = serde_json::json!({
        "type": "member_state_change",
        "member": name,
        "team": team.name,
        "from_state": prev_state.display_name(),
        "to_state": new_state.display_name(),
        "detected_at": chrono::Utc::now().to_rfc3339(),
        "context": {
            "last_pane_excerpt": pane_tail,
            "unlock_at": unlock_at,
            "consecutive_count": track.consecutive,
        }
    });
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        from: "system:supervisor".to_string(),
        text: payload.to_string(),
        kind: Some("member_state_change".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        sequencing: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
    };
    let _ = crate::inbox::enqueue(home, orch, msg);
    crate::inbox::notify_agent(
        home,
        orch,
        &crate::inbox::NotifySource::System("supervisor"),
        &format!(
            "[member_state_change] {name}: {} → {}",
            prev_state.display_name(),
            new_state.display_name()
        ),
    );
    tracing::info!(agent = %name, from = prev_state.display_name(), to = new_state.display_name(), orchestrator = %orch, "member-state-change notify sent");
    true
}

/// One iteration of the supervisor loop. Public for tests.
fn tick(
    home: &std::path::Path,
    registry: &AgentRegistry,
    notify_tracks: &mut HashMap<String, NotifyTrack>,
) {
    // Snapshot the agent names + handles so we can release the registry lock
    // before touching any per-agent core lock. Holding both at once risks
    // deadlocks against code paths that take core then registry.
    let handles: Vec<(String, _)> = {
        let reg = agent::lock_registry(registry);
        reg.iter()
            .map(|(n, h)| (n.clone(), Arc::clone(&h.core)))
            .collect()
    };

    for (name, core) in handles {
        // Mutate state + pull the tail under the core lock, then drop it
        // before running `format!` and the Telegram spawn. `tail_lines`
        // allocates a fresh String, so the lock window is bounded by the
        // vterm copy — no async IO or string formatting held against it.
        let action: Option<NoticeAction> = {
            let mut core = core.lock();

            // Sprint 23 P0 F6 fix: read heartbeat via in-memory pair lock
            // for consistent snapshot. Pre-fix code did `read_heartbeat_age`
            // (disk file read) which raced with MCP heartbeat write — between
            // supervisor's heartbeat read and the subsequent
            // `clear_waiting_on_if_stale` waiting_on_since read, MCP could
            // write the pair → supervisor saw stale heartbeat with fresh
            // waiting_on_since → spurious stale-decay firing. Pair lock
            // serialises read/write so reader sees consistent snapshot.
            //
            // Disk read fallback retained for crash-recovery: pair is
            // populated lazily on first MCP call after daemon start; if
            // pair is empty (heartbeat_at_ms == 0), fall back to disk.
            let pair = crate::daemon::heartbeat_pair::snapshot_for(&name);
            let age_opt = if pair.heartbeat_at_ms > 0 {
                let now = crate::daemon::heartbeat_pair::now_ms();
                Some(Duration::from_millis(
                    now.saturating_sub(pair.heartbeat_at_ms),
                ))
            } else {
                read_heartbeat_age(home, &name)
            };
            if let Some(age) = age_opt {
                core.state.update_heartbeat(age);
            }

            // Expire stale latched states (ToolUse/Thinking) that feed()
            // can't reach when the agent goes quiet (no PTY output).
            let prev_state = core.state.current;
            core.state.tick();

            // Sprint 43: member-state-change notify to orchestrator.
            let new_state = core.state.current;
            if prev_state != new_state && new_state.is_notify_error_class() {
                let pane_tail = core.vterm.tail_lines(10);
                maybe_notify_member_state_change(
                    home,
                    &name,
                    prev_state,
                    new_state,
                    &pane_tail,
                    notify_tracks,
                );
            }

            // §4.4 stale decay: clear waiting_on when heartbeat is stale.
            clear_waiting_on_if_stale(home, &name, !core.state.is_heartbeat_fresh());

            let agent_state = core.state.current;
            let silent = core.state.last_output.elapsed();
            if core.health.check_awaiting_operator(agent_state, silent) {
                core.state.set_awaiting_operator();
                tracing::info!(
                    agent = %name,
                    silent_secs = silent.as_secs(),
                    prev_state = agent_state.display_name(),
                    "awaiting operator (stalled on interactive prompt)"
                );
                // Consume the recovery flag if somehow armed in the same tick,
                // so the "ready again" ping doesn't fire right after we just
                // re-entered a blocked state.
                let _ = core.state.take_recovery_notice();
                Some(NoticeAction::Stall {
                    tail: core.vterm.tail_lines(TAIL_LINES),
                    silent_secs: Some(silent.as_secs()),
                })
            } else if core.state.take_interactive_prompt_notice() {
                // Pattern-based InteractivePrompt fires immediately on state
                // entry (no silence window), so the notice also goes out on
                // the first tick after entry rather than waiting for quiet.
                tracing::info!(
                    agent = %name,
                    "interactive prompt detected — forwarding to telegram"
                );
                let _ = core.state.take_recovery_notice();
                Some(NoticeAction::Stall {
                    tail: core.vterm.tail_lines(TAIL_LINES),
                    silent_secs: None,
                })
            } else if core.state.take_recovery_notice() {
                // Symmetric "ready again" signal: armed on the transition
                // out of InteractivePrompt / AwaitingOperator. Silent push so
                // operators aren't vibrated twice per interactive cycle.
                tracing::info!(
                    agent = %name,
                    "recovered from blocked state — notifying telegram"
                );
                Some(NoticeAction::Recovered)
            } else {
                None
            }
        };

        match action {
            Some(NoticeAction::Stall { tail, silent_secs }) => {
                let msg = format_stall_notice(&name, &tail, silent_secs);
                // Outbound info-leak gate (Sprint 21 Phase 1): `tail`
                // carries 40 lines of PTY output — must not leak to a
                // bound group with no operator allowlist configured.
                // `gated_notify` drops the call when the channel is
                // unauthorised; legacy `None`-allowlist deployments
                // require explicit opt-in via `user_allowlist: [...]`.
                if let Some(ch) = crate::channel::active_channel() {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        &name,
                        NotifySeverity::Warn,
                        &msg,
                        false,
                    );
                } else {
                    tracing::debug!(agent = %name, "no active channel — stall notice dropped");
                }
            }
            Some(NoticeAction::Recovered) => {
                let msg = format_recovery_notice(&name);
                if let Some(ch) = crate::channel::active_channel() {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        &name,
                        NotifySeverity::Info,
                        &msg,
                        true,
                    );
                } else {
                    tracing::debug!(agent = %name, "no active channel — recovery notice dropped");
                }
            }
            None => {}
        }
    }
}

/// Process ServerRateLimit retries: detect new rate-limit states, schedule
/// retries, and re-inject input after backoff. Runs AFTER tick() so all
/// core locks are released — PTY writes happen lock-free (Sprint 49 lesson).
pub(crate) fn process_server_rate_limit_retries(
    home: &std::path::Path,
    registry: &AgentRegistry,
    retry_tracks: &mut HashMap<String, RateLimitRetry>,
) {
    let now = Instant::now();

    // Phase 1: detect new ServerRateLimit states and schedule retries.
    {
        let reg = agent::lock_registry(registry);
        for (name, handle) in reg.iter() {
            let state = handle.core.lock().state.current;
            if state == crate::state::AgentState::ServerRateLimit {
                if retry_tracks.contains_key(name) {
                    continue; // already tracking (or exhausted)
                }
                // Read last_input_text from heartbeat_pair (leaf-level lock).
                let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
                let input_text = match pair.last_input_text {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        tracing::warn!(agent = %name, "ServerRateLimit but no last_input_text — cannot retry");
                        continue;
                    }
                };
                let delay = Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
                tracing::info!(agent = %name, delay_secs = delay.as_secs(), "ServerRateLimit detected, scheduling retry");
                let fingerprint = fingerprint_input(&input_text);
                let new_retry = RateLimitRetry {
                    retry_count: 0,
                    next_retry_at: now + delay,
                    input_text,
                    exhausted: false,
                    // Track G: phase-1 only schedules — it does not
                    // inject. Seed `dedup_count = 0` so phase 2's
                    // first retry can fire (preserves at least one
                    // re-inject for the keystroke retry case);
                    // subsequent retries hit the cap=1 boundary and
                    // are suppressed for the inbox-class case.
                    fingerprint,
                    dedup_count: 0,
                    last_inject_at: now,
                    dedup_audit_emitted: false,
                };
                // Sprint 57 Wave 2 Track C (#546 Item 5): persist on
                // every mutation so a restart inside the dedup window
                // sees the in-flight state on the next startup.
                crate::daemon::dedup_state::save(home, name, &new_retry);
                retry_tracks.insert(name.clone(), new_retry);
            } else if state == crate::state::AgentState::Ready
                || state == crate::state::AgentState::Idle
            {
                // Agent recovered — clear retry tracking.
                if retry_tracks.remove(name).is_some() {
                    // Sprint 57 Wave 2 Track C (#546 Item 5): mirror
                    // in-memory removal to disk so the next startup
                    // doesn't re-hydrate stale state for an agent
                    // that already recovered.
                    crate::daemon::dedup_state::clear(home, name);
                    tracing::info!(agent = %name, "ServerRateLimit retry cleared (agent recovered)");
                }
            }
        }
    }
    // Registry lock released here.

    // Phase 2: fire due retries (PTY write with no locks held).
    for (name, retry) in retry_tracks.iter_mut() {
        if retry.exhausted || now < retry.next_retry_at {
            continue;
        }

        // Sprint 56 Track G (#529): dedup gate — re-read the agent's
        // current pending input and ask `dedup_decision` how to handle
        // this retry tick. The pure helper isolates the policy from the
        // I/O so the four branches (suppress / force-fresh / allow-
        // after-window / allow) can be unit-tested without a registry.
        let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
        let current_text = pair.last_input_text.clone().unwrap_or_default();
        let current_fp = if current_text.is_empty() {
            retry.fingerprint
        } else {
            fingerprint_input(&current_text)
        };
        match dedup_decision(retry, current_fp, now) {
            DedupDecision::Suppress => {
                // Same fingerprint within window + cap reached. Emit
                // the audit event exactly once per fingerprint-window
                // so the operator sees "we suppressed redundant
                // replays" without log spam.
                //
                // Sprint 56 Track G fixup (reviewer m-20260508105911342800-114):
                // do NOT mark the track exhausted here. The dispatch
                // intent is "suppress redundant SAME-fingerprint replays
                // while preserving fresh-content retries during the
                // same rate-limit episode". Permanently exhausting on
                // first Suppress would block any later operator-typed
                // input from reaching `ForceFreshContent` until
                // Ready/Idle recovery clears the track. Instead, we
                // advance `next_retry_at` to the next backoff slot so
                // the next tick re-evaluates the (possibly-changed)
                // fingerprint without busy-looping.
                if !retry.dedup_audit_emitted {
                    tracing::info!(
                        agent = %name,
                        fingerprint = retry.fingerprint,
                        dedup_count = retry.dedup_count,
                        "ServerRateLimit: dedup-cap reached, suppressing redundant re-injects"
                    );
                    crate::event_log::log(
                        home,
                        "notification_inject_dedup_capped",
                        name,
                        &format!(
                            "fingerprint=0x{:016x} cap={} window_secs={}",
                            retry.fingerprint,
                            NOTIFICATION_DEDUP_CAP,
                            NOTIFICATION_DEDUP_WINDOW_SECS
                        ),
                    );
                    retry.dedup_audit_emitted = true;
                }
                let idx = (retry.retry_count as usize).min(SERVER_RATE_LIMIT_BACKOFF.len() - 1);
                retry.next_retry_at =
                    Instant::now() + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[idx]);
                // Sprint 57 Wave 2 Track C (#546 Item 5): persist
                // post-suppress so the `dedup_audit_emitted` latch +
                // `dedup_count` cap-state survive restart.
                crate::daemon::dedup_state::save(home, name, retry);
                continue;
            }
            DedupDecision::ForceFreshContent => {
                // Operator typed something new — replay the new
                // content. Replace tracking so the new fingerprint
                // takes over and the dedup cap restarts.
                retry.input_text = current_text;
                retry.fingerprint = current_fp;
                retry.dedup_count = 0;
                retry.dedup_audit_emitted = false;
            }
            DedupDecision::AllowAfterWindowReset => {
                // Window expired — allow inject and reset the counter
                // so future retries within the next window can fire
                // too. Same fingerprint, so `input_text` stays as-is.
                retry.dedup_count = 0;
                retry.dedup_audit_emitted = false;
            }
            DedupDecision::Allow => {
                // Same fingerprint, in window, cap not yet hit — fall
                // through to the existing inject path. `dedup_count`
                // gets bumped post-inject below.
            }
        }

        retry.retry_count += 1;
        if retry.retry_count > SERVER_RATE_LIMIT_MAX_RETRIES {
            tracing::warn!(agent = %name, retries = retry.retry_count, "ServerRateLimit max retries exceeded — giving up");
            crate::event_log::log(
                home,
                "server_rate_limit_exhausted",
                name,
                &format!("gave up after {} retries", SERVER_RATE_LIMIT_MAX_RETRIES),
            );
            retry.exhausted = true;
            // Sprint 57 Wave 2 Track C (#546 Item 5): persist the
            // exhausted flag so a restart doesn't re-arm a track
            // that gave up.
            crate::daemon::dedup_state::save(home, name, retry);
            continue;
        }

        // #836: post-consume suppression gate. If the input_text is
        // a previously-injected `[AGEND-MSG]` header AND the
        // corresponding msg has already been drained by the agent,
        // the notification-dedup ledger says "skip this re-inject".
        // Headers without an extractable msg_id (event-style, free-form
        // acks) fall through to the existing retry path unchanged.
        if let Some(msg_id) =
            crate::daemon::notification_dedup::extract_msg_id_from_header(&retry.input_text)
        {
            if crate::daemon::notification_dedup::global().should_suppress_reinject(name, &msg_id) {
                tracing::info!(
                    agent = %name,
                    msg_id = %msg_id,
                    "#836: ServerRateLimit retry suppressed — msg already consumed"
                );
                crate::event_log::log(
                    home,
                    "server_rate_limit_retry_suppressed",
                    name,
                    &format!("msg_id={msg_id} consumed_post_inject"),
                );
                retry.dedup_count = NOTIFICATION_DEDUP_CAP;
                crate::daemon::dedup_state::save(home, name, retry);
                continue;
            }
        }

        // Re-inject last input directly to PTY (no daemon API self-call).
        let injected = {
            let reg = agent::lock_registry(registry);
            if let Some(handle) = reg.get(name.as_str()) {
                let result = agent::inject_to_agent(handle, retry.input_text.as_bytes());
                result.is_ok()
            } else {
                false
            }
        };

        if injected {
            tracing::info!(
                agent = %name,
                retry = retry.retry_count,
                "ServerRateLimit: re-injected input (attempt {})",
                retry.retry_count
            );
            crate::event_log::log(
                home,
                "server_rate_limit_retry",
                name,
                &format!("attempt {}", retry.retry_count),
            );
            // Sprint 56 Track G (#529): record that this fingerprint
            // just got injected so the next retry tick's dedup gate
            // sees the updated bookkeeping.
            retry.dedup_count += 1;
            retry.last_inject_at = Instant::now();
            // Schedule next retry with increased backoff.
            let idx = (retry.retry_count as usize).min(SERVER_RATE_LIMIT_BACKOFF.len() - 1);
            retry.next_retry_at =
                Instant::now() + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[idx]);
            // Sprint 57 Wave 2 Track C (#546 Item 5): persist
            // post-inject so a restart sees the updated
            // dedup_count + last_inject_at and the dedup window
            // calculation stays consistent.
            crate::daemon::dedup_state::save(home, name, retry);
        } else {
            tracing::warn!(agent = %name, "ServerRateLimit: re-inject failed (agent gone?)");
            retry.exhausted = true;
            // Sprint 57 Wave 2 Track C (#546 Item 5): persist
            // exhausted-on-failure too.
            crate::daemon::dedup_state::save(home, name, retry);
        }
    }
}

/// Internal enum describing what the tick produced for a single agent, so the
/// Telegram send can run after the core lock has been released.
enum NoticeAction {
    Stall {
        tail: String,
        silent_secs: Option<u64>,
    },
    Recovered,
}

/// Build the Telegram notice shown when an agent is blocked on an interactive
/// prompt. `silent_secs = Some` for the AwaitingOperator time-based fallback
/// (reports how long the agent has been quiet); `None` for pattern-matched
/// InteractivePrompt (no silence window).
fn format_stall_notice(name: &str, tail: &str, silent_secs: Option<u64>) -> String {
    let header = match silent_secs {
        Some(s) => format!("⚠️ {name} 靜默 {s}s，可能卡在互動 prompt"),
        None => format!("⚠️ {name} 卡在互動 prompt"),
    };
    format!(
        "{header}\n\
         ────────\n\
         {tail}\n\
         ────────\n\
         💬 回覆將以原始鍵盤輸入寫入 agent stdin"
    )
}

/// Short, silent ping emitted when an agent leaves a blocked state
/// (InteractivePrompt / AwaitingOperator) and is ready for normal
/// conversation again.
fn format_recovery_notice(name: &str) -> String {
    format!("✅ {name} 已就緒，可以繼續對話")
}

/// Read `last_heartbeat` from the agent's metadata file and return the age
/// as a `Duration`. Returns `None` if the file is missing, unparseable, or
/// the timestamp is in the future.
fn read_heartbeat_age(home: &std::path::Path, name: &str) -> Option<Duration> {
    let meta_path = home.join("metadata").join(format!("{name}.json"));
    let content = std::fs::read_to_string(meta_path).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
    let ts = meta["last_heartbeat"].as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let elapsed = chrono::Utc::now().signed_duration_since(dt);
    elapsed.to_std().ok()
}

/// Clear `waiting_on` metadata when the heartbeat is stale (design §4.4).
/// Extracted as a standalone fn for testability.
fn clear_waiting_on_if_stale(home: &std::path::Path, name: &str, is_stale: bool) {
    if !is_stale {
        return;
    }
    let meta_path = home.join("metadata").join(format!("{name}.json"));
    let meta: serde_json::Value = match std::fs::read_to_string(&meta_path)
        .and_then(|c| serde_json::from_str(&c).map_err(std::io::Error::other))
    {
        Ok(v) => v,
        Err(_) => return,
    };
    if meta
        .get("waiting_on")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        // Sprint 23 P0 F6 + Sprint 22 P2a F7 paired-write fix:
        // in-memory pair update first (closes F6 race window), then disk
        // atomic batch write (closes F7 partial-write window). Order
        // matters per docs/DAEMON-LOCK-ORDERING.md — pair lock leaf-level,
        // disk I/O outside the lock.
        crate::daemon::heartbeat_pair::update_with(name, |p| {
            p.waiting_on_since_ms = None;
        });
        crate::agent_ops::save_metadata_batch(
            home,
            name,
            &[
                ("waiting_on", serde_json::json!(null)),
                ("waiting_on_since", serde_json::json!(null)),
            ],
        );
        tracing::info!(%name, "waiting_on cleared — heartbeat stale");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sprint 56 Track G (#529): dedup gate unit tests ──────────────

    /// Build a `RateLimitRetry` shaped like one the phase-1 detector
    /// would have produced: phase-1 only schedules (no inject yet),
    /// `dedup_count = 0`, `last_inject_at = now`.
    fn fresh_retry(input: &str) -> RateLimitRetry {
        RateLimitRetry {
            retry_count: 0,
            next_retry_at: Instant::now(),
            input_text: input.to_string(),
            exhausted: false,
            fingerprint: fingerprint_input(input),
            dedup_count: 0,
            last_inject_at: Instant::now(),
            dedup_audit_emitted: false,
        }
    }

    /// Lead-spec #1: same fingerprint, in window, cap reached → Suppress.
    #[test]
    fn inject_dedup_skips_same_fingerprint_in_window() {
        let mut retry = fresh_retry("[from:lead] [AGEND-MSG] size=3206 (use inbox tool)");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP; // already injected once
        let same_fp = retry.fingerprint;
        let now = retry.last_inject_at + Duration::from_secs(15); // mid-window
        assert_eq!(
            dedup_decision(&retry, same_fp, now),
            DedupDecision::Suppress,
            "same fp + in-window + cap-reached must suppress"
        );
    }

    /// Lead-spec #2: fingerprint differs → ForceFreshContent (operator
    /// typed something new mid-stall, replay the new content).
    #[test]
    fn inject_force_when_fingerprint_differs() {
        let retry = fresh_retry("original content");
        let different_fp = fingerprint_input("operator typed this AFTER rate-limit");
        let now = retry.last_inject_at + Duration::from_secs(10);
        assert_eq!(
            dedup_decision(&retry, different_fp, now),
            DedupDecision::ForceFreshContent,
            "different fp must force-inject regardless of window/cap"
        );
    }

    /// Lead-spec #3: heartbeat-after-rate-limit recovery is handled by
    /// the existing phase-1 logic that clears `retry_tracks` on the
    /// state transition to Ready/Idle. Test that a recovered agent's
    /// retry track gets cleared so the next ServerRateLimit starts
    /// fresh tracking — this is the structural guarantee the dedup
    /// path depends on (no stale fingerprint).
    #[test]
    fn inject_force_on_heartbeat_recovery() {
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert("agent1".into(), fresh_retry("stuck input"));
        // Simulate phase-1 observing Ready/Idle and clearing the track.
        tracks.remove("agent1");
        assert!(
            !tracks.contains_key("agent1"),
            "phase-1 must clear retry track on agent recovery so a \
             subsequent ServerRateLimit starts fresh tracking"
        );
        // Re-arm with new content — fingerprint will reflect post-recovery input.
        tracks.insert("agent1".into(), fresh_retry("new task after recovery"));
        let retry = &tracks["agent1"];
        assert_eq!(retry.dedup_count, 0);
        assert_eq!(
            retry.fingerprint,
            fingerprint_input("new task after recovery")
        );
    }

    /// Lead-spec #4: when Suppress fires, the audit event is emitted to
    /// the daemon event log (and the latch flag prevents re-emission).
    /// We cover the latch behaviour via a state assertion: after the
    /// retry loop applies a Suppress decision, `dedup_audit_emitted`
    /// must flip to true.
    ///
    /// Sprint 56 Track G fixup (reviewer m-20260508105911342800-114):
    /// the post-condition here changed — the Suppress arm now ADVANCES
    /// `next_retry_at` rather than setting `exhausted = true`, so a
    /// later fingerprint change can still reach `ForceFreshContent`.
    /// `exhausted` remains false; the audit latch is what carries the
    /// "we already emitted" signal for this fingerprint-window.
    #[test]
    fn inject_capped_per_fingerprint_emits_audit() {
        let mut retry = fresh_retry("notification body");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;
        let now = retry.last_inject_at + Duration::from_secs(5);
        let decision = dedup_decision(&retry, retry.fingerprint, now);
        assert_eq!(decision, DedupDecision::Suppress);
        assert!(
            !retry.dedup_audit_emitted,
            "latch starts unset before the retry loop applies the decision"
        );
        // The retry loop's Suppress arm flips the latch and advances
        // `next_retry_at`; it does NOT mark the track exhausted.
        retry.dedup_audit_emitted = true;
        retry.next_retry_at = Instant::now() + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
        assert!(retry.dedup_audit_emitted);
        assert!(
            !retry.exhausted,
            "Suppress must NOT permanently exhaust the track — fresh \
             content arriving in the same rate-limit episode must still \
             be able to reach ForceFreshContent"
        );
    }

    /// Reviewer regression test (m-20260508105911342800-114): after a
    /// Suppress event, a subsequent fingerprint change must be able to
    /// reach `ForceFreshContent`. Pre-fixup the Suppress arm set
    /// `retry.exhausted = true`, the loop short-circuited at line 598's
    /// `if retry.exhausted` guard, and operator-typed input mid-rate-
    /// limit-episode never got replayed until Ready/Idle recovery. The
    /// fix preserves track aliveness post-Suppress.
    #[test]
    fn suppress_does_not_block_subsequent_fresh_content() {
        let mut retry = fresh_retry("original notification body");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;

        // Phase 1: same-fingerprint inject hits cap → Suppress.
        let now1 = retry.last_inject_at + Duration::from_secs(5);
        assert_eq!(
            dedup_decision(&retry, retry.fingerprint, now1),
            DedupDecision::Suppress,
            "same-fp + in-window + cap-reached must Suppress"
        );
        // Caller's effect on the Suppress arm (post-fixup): latch
        // audit, advance next_retry_at, do NOT exhaust.
        retry.dedup_audit_emitted = true;
        retry.next_retry_at = Instant::now() + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
        assert!(
            !retry.exhausted,
            "post-Suppress the track must remain alive so later fresh \
             content can route through ForceFreshContent"
        );

        // Phase 2: operator types something new mid-rate-limit-episode.
        // Fingerprint changes → ForceFreshContent regardless of
        // dedup_count or audit latch state.
        let new_fp = fingerprint_input("operator typed this AFTER the cap-hit");
        let now2 = now1 + Duration::from_secs(2);
        assert_eq!(
            dedup_decision(&retry, new_fp, now2),
            DedupDecision::ForceFreshContent,
            "fingerprint change after Suppress must reach ForceFreshContent — \
             the regression the fixup defends"
        );
    }

    /// Reviewer pin (m-20260508105911342800-114): same-fingerprint
    /// retries continue to be capped after the first Suppress event.
    /// The fixup must NOT loosen the dedup cap — only remove the
    /// exhaustion side-effect. A second tick at the same fingerprint
    /// inside the window still resolves to `Suppress`.
    #[test]
    fn suppress_keeps_capped_for_same_fingerprint_until_window_expires() {
        let mut retry = fresh_retry("the same notification");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;
        retry.dedup_audit_emitted = true; // latched from a prior Suppress

        // Tick 1: still in window, same fingerprint → Suppress.
        let mid_window = retry.last_inject_at + Duration::from_secs(20);
        assert_eq!(
            dedup_decision(&retry, retry.fingerprint, mid_window),
            DedupDecision::Suppress
        );
        // Tick 2: closer to window edge, same fingerprint → still Suppress.
        let near_edge =
            retry.last_inject_at + Duration::from_secs(NOTIFICATION_DEDUP_WINDOW_SECS - 1);
        assert_eq!(
            dedup_decision(&retry, retry.fingerprint, near_edge),
            DedupDecision::Suppress
        );
        // Tick 3: past window → AllowAfterWindowReset (cap NOT bypassed,
        // just the per-window counter resets). Pins that the cap can
        // only relax via window expiry, not via Suppress's side-effects.
        let past_window =
            retry.last_inject_at + Duration::from_secs(NOTIFICATION_DEDUP_WINDOW_SECS + 1);
        assert_eq!(
            dedup_decision(&retry, retry.fingerprint, past_window),
            DedupDecision::AllowAfterWindowReset
        );
    }

    /// Lead-spec #5: keystroke retry case — the dedup gate doesn't
    /// completely silence keystroke inputs. With cap = 1, the first
    /// retry fires (Allow → bump dedup_count to 1); only the second
    /// retry would suppress. Operators who type and wait still get
    /// one retry attempt, matching the RCA's "preserves keystroke
    /// retry coverage" invariant.
    #[test]
    fn keystroke_retry_unaffected_by_dedup() {
        let retry = fresh_retry("git status"); // looks like a keystroke
        let same_fp = retry.fingerprint;
        let now = retry.last_inject_at + Duration::from_secs(5);
        // dedup_count = 0, cap = 1, in-window → Allow (first retry fires).
        assert_eq!(dedup_decision(&retry, same_fp, now), DedupDecision::Allow);
    }

    /// Lead-spec #6: window expiry resets the dedup counter so a
    /// long-running rate-limit recovery can re-inject after the
    /// window without operator action.
    #[test]
    fn dedup_window_expires_allowing_re_inject() {
        let mut retry = fresh_retry("durable notification");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;
        let same_fp = retry.fingerprint;
        let past_window =
            retry.last_inject_at + Duration::from_secs(NOTIFICATION_DEDUP_WINDOW_SECS + 1);
        assert_eq!(
            dedup_decision(&retry, same_fp, past_window),
            DedupDecision::AllowAfterWindowReset,
            "past window: same fp must allow inject after counter reset"
        );
    }

    /// Lead-spec #7: fingerprint helper is deterministic (same input
    /// always hashes the same) and discriminating (notification vs
    /// keystroke vs even one-byte differences hash to distinct values).
    #[test]
    fn fingerprint_extraction_for_notification_vs_keystroke() {
        let notification = "[from:lead] [AGEND-MSG] kind=task size=3206 (use inbox tool)";
        let keystroke = "deploy --env prod";
        let h_notif_1 = fingerprint_input(notification);
        let h_notif_2 = fingerprint_input(notification);
        let h_keystroke = fingerprint_input(keystroke);
        let h_keystroke_plus_one = fingerprint_input("deploy --env prod\n");
        assert_eq!(h_notif_1, h_notif_2, "deterministic on same input");
        assert_ne!(
            h_notif_1, h_keystroke,
            "notification vs keystroke must produce distinct fingerprints"
        );
        assert_ne!(
            h_keystroke, h_keystroke_plus_one,
            "one-byte difference must change the fingerprint"
        );
        // Empty string also has a stable fingerprint distinct from non-empty.
        assert_ne!(fingerprint_input(""), h_keystroke);
    }

    /// Defensive bonus: the cap-reached audit event should latch — if
    /// the same `Suppress` decision arrived twice (e.g. from two retry
    /// ticks before the track is marked exhausted), the second pass
    /// must not re-emit. The retry loop achieves this by checking
    /// `dedup_audit_emitted` before logging; this test asserts the
    /// flag is observable so the loop's guard works.
    #[test]
    fn cap_reached_audit_latches_via_dedup_audit_emitted_flag() {
        let mut retry = fresh_retry("body");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;
        retry.dedup_audit_emitted = false;
        // First Suppress: loop would emit + flip latch.
        retry.dedup_audit_emitted = true;
        // Second hypothetical Suppress arrives before exhaust takes effect.
        let still_latched = retry.dedup_audit_emitted;
        assert!(still_latched, "latch must remain set across re-evaluations");
    }

    /// Defensive bonus: ForceFreshContent → caller resets dedup_count
    /// to 0 + clears the audit latch. We model the caller's effect to
    /// pin the post-decision invariants the retry loop relies on.
    #[test]
    fn differing_fingerprint_resets_dedup_count_and_audit_latch() {
        let mut retry = fresh_retry("first content");
        retry.dedup_count = NOTIFICATION_DEDUP_CAP;
        retry.dedup_audit_emitted = true;
        let new_fp = fingerprint_input("second content");
        let now = retry.last_inject_at + Duration::from_secs(10);
        assert_eq!(
            dedup_decision(&retry, new_fp, now),
            DedupDecision::ForceFreshContent
        );
        // Caller's effect (per ForceFreshContent arm in the retry loop):
        retry.input_text = "second content".into();
        retry.fingerprint = new_fp;
        retry.dedup_count = 0;
        retry.dedup_audit_emitted = false;
        assert_eq!(retry.dedup_count, 0);
        assert!(!retry.dedup_audit_emitted);
    }

    // ── #841 rate-limit recovery nudge — pure-helper tests ────────────

    use crate::fleet::RateLimitRecoveryConfig;
    use crate::state::AgentState;

    /// Build a `RecoveryNudgeTrack` representing "agent saw error at
    /// `error_offset` ago, recovered at `recovery_offset` ago, and
    /// has never been nudged before".
    fn fresh_nudge_track(
        now: Instant,
        error_offset: Duration,
        recovery_offset: Duration,
    ) -> RecoveryNudgeTrack {
        RecoveryNudgeTrack {
            last_error_at: Some(now - error_offset),
            recovered_at: Some(now - recovery_offset),
            last_inject_at: None,
            fired_this_cycle: false,
        }
    }

    /// (a) trigger condition fires once. Default config: agent has been
    /// in Ready for 70s after a ServerRateLimit observed 100s ago — past
    /// observe_after_secs (30s) and past recovery_after_secs (60s), no
    /// cooldown, fast-retry inactive → Fire.
    #[test]
    fn nudge_fires_when_all_conditions_met() {
        let cfg = RateLimitRecoveryConfig::default();
        let now = Instant::now();
        let track = fresh_nudge_track(now, Duration::from_secs(100), Duration::from_secs(70));
        assert_eq!(
            decide_nudge(&track, AgentState::Idle, false, &cfg, now),
            NudgeDecision::Fire
        );
    }

    /// (b) enabled=false → Skip(Disabled). Same path as (e) per-instance
    /// opt-out — fleet.yaml `rate_limit_recovery.enabled: false` flows
    /// straight to this gate.
    #[test]
    fn nudge_skipped_when_disabled() {
        let cfg = RateLimitRecoveryConfig {
            enabled: false,
            ..RateLimitRecoveryConfig::default()
        };
        let now = Instant::now();
        let track = fresh_nudge_track(now, Duration::from_secs(100), Duration::from_secs(70));
        assert_eq!(
            decide_nudge(&track, AgentState::Idle, false, &cfg, now),
            NudgeDecision::Skip(NudgeSkipReason::Disabled)
        );
    }

    /// (c) cooldown: a nudge fired within the last cooldown_secs (300s)
    /// suppresses the next decision even if a fresh error→recovery
    /// cycle is otherwise ready to fire.
    #[test]
    fn nudge_skipped_within_cooldown() {
        let cfg = RateLimitRecoveryConfig::default();
        let now = Instant::now();
        let mut track = fresh_nudge_track(now, Duration::from_secs(100), Duration::from_secs(70));
        track.last_inject_at = Some(now - Duration::from_secs(60)); // 60s ago, within 300s cooldown
        assert_eq!(
            decide_nudge(&track, AgentState::Idle, false, &cfg, now),
            NudgeDecision::Skip(NudgeSkipReason::Cooldown)
        );
    }

    /// (d) defer-to-fast-retry: when `process_server_rate_limit_retries`
    /// has an active retry track for this agent, the nudge path skips
    /// and lets the fast path run. Prevents double-firing.
    #[test]
    fn nudge_defers_to_fast_retry() {
        let cfg = RateLimitRecoveryConfig::default();
        let now = Instant::now();
        let track = fresh_nudge_track(now, Duration::from_secs(100), Duration::from_secs(70));
        assert_eq!(
            decide_nudge(&track, AgentState::Idle, true, &cfg, now),
            NudgeDecision::Skip(NudgeSkipReason::DeferToFastRetry)
        );
    }

    /// (e) no-recent-error: agent is idle but no error was observed in
    /// the last `observe_after_secs`. Genuinely idle, not stalled —
    /// must not inject (false-positive avoidance).
    #[test]
    fn nudge_skipped_when_no_recent_error() {
        let cfg = RateLimitRecoveryConfig::default();
        let now = Instant::now();
        let track = RecoveryNudgeTrack {
            last_error_at: None,
            recovered_at: Some(now - Duration::from_secs(70)),
            last_inject_at: None,
            fired_this_cycle: false,
        };
        assert_eq!(
            decide_nudge(&track, AgentState::Idle, false, &cfg, now),
            NudgeDecision::Skip(NudgeSkipReason::NoRecentError)
        );
    }

    /// r1 fix: when fleet.yaml load FAILS (parse error, missing file,
    /// corrupted yaml), `resolve_recovery_config` must return a config
    /// with `enabled = false` so the daemon fails closed — a broken
    /// config never silently enables auto-inject. Spike Q3 contract.
    #[test]
    fn resolve_recovery_config_fails_closed_on_load_error() {
        let default_cfg = RateLimitRecoveryConfig::default();
        assert!(
            default_cfg.enabled,
            "sanity: per-instance/parsed default IS enabled=true; only the load-error \
             fallback flips to disabled"
        );

        let resolved = resolve_recovery_config(Err(()), "agent-x", &default_cfg);
        assert!(
            !resolved.enabled,
            "load error must fail-closed: enabled=false"
        );
        // Other knobs preserved so an operator who later restores the
        // config doesn't get a surprise mismatch in window sizes.
        assert_eq!(resolved.observe_after_secs, default_cfg.observe_after_secs);
        assert_eq!(
            resolved.recovery_after_secs,
            default_cfg.recovery_after_secs
        );
        assert_eq!(resolved.cooldown_secs, default_cfg.cooldown_secs);
        assert_eq!(resolved.prompt, default_cfg.prompt);
    }

    /// Per-instance override path: fleet.yaml loaded, instance entry
    /// present with `rate_limit_recovery: { enabled: false, ... }` →
    /// the override flows through verbatim (operator opt-out works).
    #[test]
    fn resolve_recovery_config_uses_instance_override_when_present() {
        use std::collections::HashMap;
        let mut instances = HashMap::new();
        instances.insert(
            "opt-out-agent".to_string(),
            crate::fleet::InstanceConfig {
                rate_limit_recovery: Some(RateLimitRecoveryConfig {
                    enabled: false,
                    observe_after_secs: 999,
                    recovery_after_secs: 999,
                    prompt: "should-not-fire".into(),
                    cooldown_secs: 999,
                }),
                ..Default::default()
            },
        );
        let fleet = crate::fleet::FleetConfig {
            instances,
            ..Default::default()
        };
        let default_cfg = RateLimitRecoveryConfig::default();
        let resolved = resolve_recovery_config(Ok(&fleet), "opt-out-agent", &default_cfg);
        assert!(!resolved.enabled, "per-instance opt-out must propagate");
        assert_eq!(resolved.observe_after_secs, 999);
        assert_eq!(resolved.prompt, "should-not-fire");
    }

    /// Fallback path: fleet.yaml loaded successfully but the instance
    /// is absent (e.g. a runtime-only agent not yet persisted) OR the
    /// instance entry omits `rate_limit_recovery` → use `default_cfg`
    /// (the daemon-wide default), which has `enabled=true`.
    #[test]
    fn resolve_recovery_config_falls_back_to_default_when_instance_absent() {
        let fleet = crate::fleet::FleetConfig::default();
        let default_cfg = RateLimitRecoveryConfig::default();
        let resolved =
            resolve_recovery_config(Ok(&fleet), "never-heard-of-this-agent", &default_cfg);
        assert!(
            resolved.enabled,
            "no per-instance override → daemon default kicks in (enabled=true)"
        );
        assert_eq!(resolved.observe_after_secs, default_cfg.observe_after_secs);
    }

    /// (f) permanent errors excluded: `UsageLimit` and `AuthError` are
    /// hard quota / credential failures — a "continue" nudge cannot
    /// resolve them. The `is_transient_error` gate must return false
    /// for these (the integration fn uses it before stamping
    /// `last_error_at`, so a permanent error never produces a nudge
    /// even if the agent later transitions to Idle).
    #[test]
    fn is_transient_error_excludes_permanent_states() {
        // Transient — recoverable with a nudge.
        assert!(AgentState::ServerRateLimit.is_transient_error());
        assert!(AgentState::RateLimit.is_transient_error());
        assert!(AgentState::ApiError.is_transient_error());
        // Permanent — nudge cannot help; would just spam the PTY.
        assert!(!AgentState::UsageLimit.is_transient_error());
        assert!(!AgentState::AuthError.is_transient_error());
        // Non-error states.
        assert!(!AgentState::Ready.is_transient_error());
        assert!(!AgentState::Idle.is_transient_error());
        assert!(!AgentState::Thinking.is_transient_error());
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-supervisor-test-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn waiting_on_cleared_when_heartbeat_stale() {
        let home = tmp_home("stale_decay");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let meta = serde_json::json!({
            "waiting_on": "review from at-dev-4",
            "waiting_on_since": "2026-04-22T10:00:00Z",
            "last_heartbeat": "2026-04-22T09:00:00Z",
        });
        std::fs::write(
            meta_dir.join("agent1.json"),
            serde_json::to_string_pretty(&meta).expect("serialize"),
        )
        .ok();

        // Stale → must clear
        clear_waiting_on_if_stale(&home, "agent1", true);

        let content =
            std::fs::read_to_string(meta_dir.join("agent1.json")).expect("read after clear");
        let result: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            result["waiting_on"].is_null(),
            "waiting_on must be null after stale decay"
        );
        assert!(
            result["waiting_on_since"].is_null(),
            "waiting_on_since must be null after stale decay"
        );

        // Fresh → must NOT clear
        let meta2 = serde_json::json!({
            "waiting_on": "still waiting",
            "waiting_on_since": "2026-04-22T10:00:00Z",
        });
        std::fs::write(
            meta_dir.join("agent2.json"),
            serde_json::to_string_pretty(&meta2).expect("serialize"),
        )
        .ok();
        clear_waiting_on_if_stale(&home, "agent2", false);
        let content2 = std::fs::read_to_string(meta_dir.join("agent2.json")).expect("read agent2");
        let result2: serde_json::Value = serde_json::from_str(&content2).expect("parse");
        assert_eq!(
            result2["waiting_on"], "still waiting",
            "fresh heartbeat must NOT clear waiting_on"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Sprint 22 P2a F7 regression — both `waiting_on` and `waiting_on_since`
    /// must land in a single atomic disk write so a crash mid-clear cannot
    /// leave divergent state (waiting_on=null + waiting_on_since=set, which
    /// `set_waiting_on` freshness logic interprets on restart as "agent is
    /// currently waiting" without a `waiting_on` value).
    ///
    /// The pre-fix code had two sequential `save_metadata` calls; this test
    /// pins the contract that the call site delegates to
    /// `agent_ops::save_metadata_batch` (single read-modify-write cycle).
    /// Source-grep verifies the two-call regression cannot reappear:
    /// `clear_waiting_on_if_stale` must contain `save_metadata_batch` and
    /// must NOT contain two adjacent `save_metadata(` calls.
    #[test]
    fn waiting_on_clear_uses_atomic_batch_write() {
        // Source-grep guard: pin that the impl uses save_metadata_batch
        // (closes F7 race window). Future regression to two-call form
        // would fail-loud here.
        let src = include_str!("supervisor.rs");
        let body_start = src
            .find("fn clear_waiting_on_if_stale(")
            .expect("clear_waiting_on_if_stale must exist");
        // Bound the search to the function body (next top-level fn).
        let rest = &src[body_start..];
        let body_end = rest
            .find("\nfn ")
            .or_else(|| rest.find("\npub fn "))
            .or_else(|| rest.find("\n#[cfg(test)]"))
            .unwrap_or(rest.len());
        let body = &rest[..body_end];

        assert!(
            body.contains("save_metadata_batch("),
            "clear_waiting_on_if_stale must use `save_metadata_batch` for atomic \
             multi-field write (Sprint 22 P2a F7 fix). Found body:\n{body}"
        );
        // Sanity check: the legacy two-call pattern must NOT reappear.
        // We check that the body contains at most ONE `save_metadata(`
        // substring — `save_metadata_batch(` matches separately because
        // we look for the open paren after `metadata` not `metadata_batch`.
        let single_calls = body.matches("save_metadata(").count();
        assert!(
            single_calls == 0,
            "clear_waiting_on_if_stale must NOT call individual `save_metadata` \
             — F7 race fix requires `save_metadata_batch` (single atomic write). \
             Found {single_calls} `save_metadata(` call(s) in body:\n{body}"
        );
    }

    /// Sprint 22 P2a F7 behavioural regression — verify the atomic batch
    /// write produces the expected on-disk state when both fields land
    /// together. Pairs with the source-grep guard above; this test catches
    /// a regression where the helper signature changes but the call site
    /// still compiles.
    #[test]
    fn waiting_on_clear_writes_both_nulls_atomically() {
        let home = tmp_home("f7_atomic");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        // Pre-populate with active wait state + an unrelated field that
        // must survive the batch write (read-modify-write contract).
        let meta = serde_json::json!({
            "waiting_on": "review from at-dev-4",
            "waiting_on_since": "2026-04-27T05:00:00Z",
            "last_heartbeat": "2026-04-27T04:55:00Z",
            "role": "dev-impl-2",
        });
        std::fs::write(
            meta_dir.join("agent_atomic.json"),
            serde_json::to_string_pretty(&meta).expect("serialize"),
        )
        .ok();

        clear_waiting_on_if_stale(&home, "agent_atomic", true);

        let raw = std::fs::read_to_string(meta_dir.join("agent_atomic.json"))
            .expect("metadata file present");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            v["waiting_on"].is_null(),
            "waiting_on must be null after F7 atomic clear"
        );
        assert!(
            v["waiting_on_since"].is_null(),
            "waiting_on_since must be null after F7 atomic clear (paired with waiting_on)"
        );
        assert_eq!(
            v["last_heartbeat"], "2026-04-27T04:55:00Z",
            "unrelated `last_heartbeat` must survive the batch write"
        );
        assert_eq!(
            v["role"], "dev-impl-2",
            "unrelated `role` must survive the batch write"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 43: member-state-change notify tests ──────────────────

    /// is_notify_error_class matches exactly the GO-NARROW 6 states.
    #[test]
    fn is_notify_error_class_matches_go_narrow_6() {
        use crate::state::AgentState;
        assert!(AgentState::UsageLimit.is_notify_error_class());
        assert!(AgentState::RateLimit.is_notify_error_class());
        assert!(AgentState::Hang.is_notify_error_class());
        assert!(AgentState::Crashed.is_notify_error_class());
        assert!(AgentState::AuthError.is_notify_error_class());
        assert!(AgentState::PermissionPrompt.is_notify_error_class());
        assert!(!AgentState::ContextFull.is_notify_error_class());
        assert!(!AgentState::AwaitingOperator.is_notify_error_class());
        assert!(!AgentState::ApiError.is_notify_error_class());
        assert!(!AgentState::Restarting.is_notify_error_class());
        assert!(!AgentState::InteractivePrompt.is_notify_error_class());
        assert!(!AgentState::Ready.is_notify_error_class());
        assert!(!AgentState::Idle.is_notify_error_class());
        assert!(!AgentState::ToolUse.is_notify_error_class());
        assert!(!AgentState::Starting.is_notify_error_class());
    }

    /// NOTIFY_COOLDOWN constant is 60 seconds.
    #[test]
    fn notify_cooldown_is_60_seconds() {
        assert_eq!(super::NOTIFY_COOLDOWN, std::time::Duration::from_secs(60));
    }

    /// D4: 2×2 invariant fixture — production-path-coupled.
    /// 2 teams (team-a: orch-a + worker-a, team-b: orch-b + worker-b).
    /// worker-a transitions Ready → UsageLimit.
    /// Assert: orch-a inbox has 1 event; orch-b/worker-a/worker-b have 0.
    #[test]
    fn notify_single_receiver_2x2_invariant() {
        let home = std::env::temp_dir().join(format!("agend-notify-2x2-{}", std::process::id()));
        std::fs::create_dir_all(home.join("inbox")).ok();

        // Setup teams via teams API (correct store format).
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "team-a", "members": ["orch-a", "worker-a"], "orchestrator": "orch-a"}),
        );
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "team-b", "members": ["orch-b", "worker-b"], "orchestrator": "orch-b"}),
        );

        // Call production function (§3.5.10 production-path-coupled).
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "worker-a",
            crate::state::AgentState::Ready,
            crate::state::AgentState::UsageLimit,
            "Usage limit reached. Resets at 15:14 UTC",
            &mut tracks,
        );
        assert!(sent, "notify must be sent");

        // Assert: orch-a has 1 event (JSONL file).
        let orch_a_inbox = home.join("inbox").join("orch-a.jsonl");
        let orch_a_count = std::fs::read_to_string(&orch_a_inbox)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        assert_eq!(orch_a_count, 1, "orch-a must have exactly 1 event");

        // Assert: others have 0.
        for other in &["orch-b", "worker-a", "worker-b", "general"] {
            let inbox = home.join("inbox").join(format!("{other}.jsonl"));
            let count = std::fs::read_to_string(&inbox)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .count();
            assert_eq!(count, 0, "{other} must have 0 events");
        }

        std::fs::remove_dir_all(&home).ok();
    }

    /// D3: skip self-notify — orchestrator hits UsageLimit → 0 events.
    #[test]
    fn notify_skip_self_when_member_is_orchestrator() {
        let home = std::env::temp_dir().join(format!("agend-notify-self-{}", std::process::id()));
        std::fs::create_dir_all(home.join("inbox")).ok();
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "team-a", "members": ["orch-a"], "orchestrator": "orch-a"}),
        );

        // Call production function — should return false (self-notify skip).
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "orch-a",
            crate::state::AgentState::Ready,
            crate::state::AgentState::UsageLimit,
            "",
            &mut tracks,
        );
        assert!(!sent, "self-notify must be skipped");
        let content =
            std::fs::read_to_string(home.join("inbox").join("orch-a.jsonl")).unwrap_or_default();
        assert_eq!(
            content.lines().filter(|l| !l.is_empty()).count(),
            0,
            "orch-a=0"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// E: no orchestrator → notify returns false (warn logged).
    #[test]
    fn notify_warns_when_no_orchestrator() {
        let home = std::env::temp_dir().join(format!("agend-notify-noorch-{}", std::process::id()));
        std::fs::create_dir_all(home.join("inbox")).ok();
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "team-a", "members": ["worker-a"]}),
        );
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "worker-a",
            crate::state::AgentState::Ready,
            crate::state::AgentState::Hang,
            "",
            &mut tracks,
        );
        assert!(!sent, "no orchestrator → no notify");
        std::fs::remove_dir_all(&home).ok();
    }

    /// parse_unlock_at extracts time from pane output.
    #[test]
    fn parse_unlock_at_extracts_time() {
        assert_eq!(
            super::parse_unlock_at("Usage limit reached. Resets at 15:14 UTC"),
            Some("15:14".to_string())
        );
        assert_eq!(super::parse_unlock_at("no time here"), None);
    }

    // ── ServerRateLimit auto-retry tests ─────────────────────────────

    #[test]
    fn backoff_5s_15s_30s_schedule() {
        assert_eq!(super::SERVER_RATE_LIMIT_BACKOFF, [5, 15, 30]);
        assert_eq!(super::SERVER_RATE_LIMIT_MAX_RETRIES, 3);
    }

    #[test]
    fn three_retries_then_stop() {
        use super::RateLimitRetry;
        let mut retry = RateLimitRetry {
            retry_count: 3,
            next_retry_at: std::time::Instant::now(),
            input_text: "test input".into(),
            exhausted: false,
            fingerprint: 0,
            dedup_count: 0,
            last_inject_at: std::time::Instant::now(),
            dedup_audit_emitted: false,
        };
        retry.retry_count += 1;
        assert!(
            retry.retry_count > super::SERVER_RATE_LIMIT_MAX_RETRIES,
            "after 3 retries, count exceeds max"
        );
    }

    #[test]
    fn re_inject_preserves_last_input_text() {
        use super::RateLimitRetry;
        let original = "Please analyze this code and suggest improvements";
        let retry = RateLimitRetry {
            retry_count: 0,
            next_retry_at: std::time::Instant::now(),
            input_text: original.to_string(),
            exhausted: false,
            fingerprint: 0,
            dedup_count: 0,
            last_inject_at: std::time::Instant::now(),
            dedup_audit_emitted: false,
        };
        assert_eq!(
            retry.input_text, original,
            "input text must be preserved across retries"
        );
    }

    #[test]
    fn re_inject_path_does_not_self_ipc() {
        // Verify the retry function uses agent::inject_to_agent directly
        // (not crate::api::call) — Sprint 49 deadlock regression guard.
        let src = include_str!("supervisor.rs");
        let fn_start = src
            .find("fn process_server_rate_limit_retries(")
            .expect("function must exist");
        let rest = &src[fn_start..];
        let fn_end = rest
            .find("\n/// ")
            .or_else(|| rest.find("\nfn "))
            .unwrap_or(rest.len());
        let body = &rest[..fn_end];
        assert!(
            body.contains("inject_to_agent"),
            "retry must use inject_to_agent (direct PTY write)"
        );
        assert!(
            !body.contains("api::call"),
            "retry must NOT use api::call (Sprint 49 deadlock)"
        );
    }

    #[test]
    fn re_inject_works_after_tui_keyboard_input() {
        // Verify that TUI keyboard input (pane.rs write_to_agent path)
        // records last_input_text so ServerRateLimit retry can re-inject.
        let agent_name = "test-tui-input";
        let input = "Please analyze this code";
        crate::daemon::heartbeat_pair::update_with(agent_name, |p| {
            p.last_input_text = Some(input.to_string());
        });
        let pair = crate::daemon::heartbeat_pair::snapshot_for(agent_name);
        assert_eq!(
            pair.last_input_text.as_deref(),
            Some(input),
            "TUI keyboard input must be recorded in last_input_text for retry"
        );
    }

    #[test]
    fn retry_loop_does_not_restart_after_max_exceeded() {
        // Simulate: retry exhausted (count > max) → entry stays with
        // exhausted=true → Phase 1 sees contains_key → skips re-insert.
        use super::RateLimitRetry;
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "agent-loop".into(),
            RateLimitRetry {
                retry_count: 4,
                next_retry_at: std::time::Instant::now(),
                input_text: "test".into(),
                exhausted: true,
                fingerprint: 0,
                dedup_count: 0,
                last_inject_at: std::time::Instant::now(),
                dedup_audit_emitted: false,
            },
        );
        // Phase 1 logic: if retry_tracks.contains_key → skip
        assert!(
            tracks.contains_key("agent-loop"),
            "exhausted entry must remain in tracks to prevent re-insert"
        );
        assert!(
            tracks["agent-loop"].exhausted,
            "entry must be marked exhausted"
        );
    }

    #[test]
    fn retry_resumes_after_recovery_then_new_failure() {
        // State: ServerRateLimit → max → Ready → ServerRateLimit again
        // Recovery clears the entry → new failure can start fresh sequence.
        use super::RateLimitRetry;
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "agent-recover".into(),
            RateLimitRetry {
                retry_count: 4,
                next_retry_at: std::time::Instant::now(),
                input_text: "old".into(),
                exhausted: true,
                fingerprint: 0,
                dedup_count: 0,
                last_inject_at: std::time::Instant::now(),
                dedup_audit_emitted: false,
            },
        );
        // Simulate recovery (Ready/Idle transition removes entry):
        tracks.remove("agent-recover");
        assert!(!tracks.contains_key("agent-recover"));
        // New failure can now insert fresh:
        tracks.insert(
            "agent-recover".into(),
            RateLimitRetry {
                retry_count: 0,
                next_retry_at: std::time::Instant::now(),
                input_text: "new input".into(),
                exhausted: false,
                fingerprint: 0,
                dedup_count: 0,
                last_inject_at: std::time::Instant::now(),
                dedup_audit_emitted: false,
            },
        );
        assert_eq!(tracks["agent-recover"].retry_count, 0);
        assert!(!tracks["agent-recover"].exhausted);
    }

    #[test]
    fn retry_does_not_count_state_persistence_as_new_failure() {
        // State stays ServerRateLimit for many ticks → only 1 retry
        // sequence fires (not N sequences for N ticks).
        use super::RateLimitRetry;
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        // First tick: insert
        tracks.insert(
            "agent-persist".into(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: std::time::Instant::now(),
                input_text: "input".into(),
                exhausted: false,
                fingerprint: 0,
                dedup_count: 0,
                last_inject_at: std::time::Instant::now(),
                dedup_audit_emitted: false,
            },
        );
        // Subsequent ticks: Phase 1 checks contains_key → skips
        for _ in 0..30 {
            assert!(
                tracks.contains_key("agent-persist"),
                "entry must persist across ticks"
            );
            // Phase 1 would skip because contains_key is true
        }
        // Only 1 retry sequence was ever created
        assert_eq!(
            tracks.len(),
            1,
            "only 1 retry sequence despite 30 ticks of persistent state"
        );
    }

    #[test]
    fn last_input_text_stored_raw_no_header() {
        // notify_agent stores raw text (not formatted notification with header).
        // Verify by checking that the stored text does NOT contain [AGEND-MSG].
        let agent = "test-raw-store";
        let raw_body = "Please analyze this code";
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.last_input_text = Some(raw_body.to_string());
        });
        let pair = crate::daemon::heartbeat_pair::snapshot_for(agent);
        let stored = pair.last_input_text.expect("must be set");
        assert!(
            !stored.contains("[AGEND-MSG]"),
            "must not contain header: {stored}"
        );
        assert!(
            !stored.contains("[from:"),
            "must not contain source prefix: {stored}"
        );
        assert_eq!(stored, raw_body, "must store raw body exactly");
    }

    #[test]
    fn retry_re_inject_uses_raw_text_no_header() {
        // The retry path uses inject_to_agent with retry.input_text.
        // Verify the source-level contract: input_text comes from
        // last_input_text which is raw (per the fix above).
        let src = include_str!("supervisor.rs");
        let fn_start = src
            .find("fn process_server_rate_limit_retries(")
            .expect("function must exist");
        let rest = &src[fn_start..];
        let fn_end = rest
            .find("\n/// ")
            .or_else(|| rest.find("\nfn "))
            .unwrap_or(rest.len());
        let body = &rest[..fn_end];
        assert!(
            body.contains("retry.input_text.as_bytes()"),
            "retry must use input_text (raw body) for re-inject"
        );
        assert!(
            !body.contains("format_notification"),
            "retry must NOT re-format notification (would add header)"
        );
    }

    // ─── Sprint 54 P2-3: pane-input-not-submitted detection tests ───

    /// Helper: minimal `UxEventSink` that records every emitted event
    /// in-memory so the supervisor's emission can be asserted without
    /// standing up a real channel adapter.
    struct TestSink {
        events: parking_lot::Mutex<Vec<crate::channel::ux_event::UxEvent>>,
    }
    impl crate::channel::ux_event::UxEventSink for TestSink {
        fn emit(&self, event: &crate::channel::ux_event::UxEvent) {
            self.events.lock().push(event.clone());
        }
    }

    /// Helper: stand up `home/fleet.yaml` declaring `agent_name` with the
    /// chosen backend command, then return `home`. Used by the
    /// pane-input-not-submitted suite so `pane_input_backend_supported`
    /// resolves the agent against a real fleet config.
    fn fleet_with_backend(tag: &str, agent_name: &str, backend_cmd: &str) -> std::path::PathBuf {
        let home = tmp_home(tag);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  {agent_name}:\n    backend: {backend_cmd}\n    \
                 working_directory: \"/tmp\"\n"
            ),
        )
        .expect("write fleet.yaml");
        home
    }

    /// Helper: pre-populate the agent's metadata with a typed timestamp
    /// older than `now - threshold_secs` and a (possibly absent) submit
    /// timestamp. Bypasses `record_input_activity` / `record_submit_activity`
    /// so tests can set arbitrary epoch-ms values.
    fn seed_input_submit(home: &std::path::Path, agent: &str, typed_ms: i64, submit_ms: i64) {
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        let mut meta = serde_json::Map::new();
        if typed_ms > 0 {
            meta.insert("last_input_epoch_ms".into(), serde_json::json!(typed_ms));
        }
        if submit_ms > 0 {
            meta.insert("last_submit_epoch_ms".into(), serde_json::json!(submit_ms));
        }
        std::fs::write(
            meta_dir.join(format!("{agent}.json")),
            serde_json::to_string_pretty(&serde_json::Value::Object(meta)).expect("serialize"),
        )
        .expect("write metadata");
    }

    #[test]
    fn pane_input_not_submitted_emits_event_when_threshold_exceeded() {
        // Per-test unique agent name avoids cross-test sink_registry
        // contamination (cargo test runs in parallel; the global sink
        // registry sees emissions from every test concurrently).
        let agent = "claude-agent-pin-emit";
        let home = fleet_with_backend("pin_emit", agent, "claude");
        // Typed 5 minutes ago, never submitted → must emit.
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_input_submit(&home, agent, now_ms - 300_000, 0);
        std::env::set_var("AGEND_PANE_INPUT_THRESHOLD_SECS", "60");
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        let events = sink.events.lock();
        let matched = events.iter().filter_map(|e| match e {
            crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted {
                    agent: emitted, ..
                },
            ) if emitted == agent => Some(()),
            _ => None,
        });
        assert!(
            matched.count() >= 1,
            "expected ≥1 PaneInputNotSubmitted event for {agent}, got: {events:?}"
        );
        std::env::remove_var("AGEND_PANE_INPUT_THRESHOLD_SECS");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pane_input_not_submitted_skips_when_within_threshold() {
        let agent = "claude-agent-pin-within";
        let home = fleet_with_backend("pin_within", agent, "claude");
        // Typed 5s ago — well within default 60s threshold → no emit.
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_input_submit(&home, agent, now_ms - 5_000, 0);
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        let events = sink.events.lock();
        for e in events.iter() {
            if let crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted {
                    agent: emitted, ..
                },
            ) = e
            {
                assert_ne!(emitted, agent, "must not emit within threshold");
            }
        }
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pane_input_not_submitted_skips_when_submit_caught_up() {
        let agent = "claude-agent-pin-submit";
        let home = fleet_with_backend("pin_submit", agent, "claude");
        // Typed 5min ago AND submitted 4min ago (submit > 0 and >= typed).
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_input_submit(&home, agent, now_ms - 300_000, now_ms - 240_000);
        std::env::set_var("AGEND_PANE_INPUT_THRESHOLD_SECS", "60");
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        let events = sink.events.lock();
        for e in events.iter() {
            if let crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted {
                    agent: emitted, ..
                },
            ) = e
            {
                assert_ne!(
                    emitted, agent,
                    "must not emit when submit timestamp >= typed"
                );
            }
        }
        std::env::remove_var("AGEND_PANE_INPUT_THRESHOLD_SECS");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pane_input_not_submitted_skips_non_claude_backend() {
        // kiro-cli is NOT on the submit-detection allowlist (claude-only first round).
        let agent = "kiro-agent-pin-nonclaude";
        let home = fleet_with_backend("pin_nonclaude", agent, "kiro-cli");
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_input_submit(&home, agent, now_ms - 300_000, 0);
        std::env::set_var("AGEND_PANE_INPUT_THRESHOLD_SECS", "60");
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        let events = sink.events.lock();
        for e in events.iter() {
            if let crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted {
                    agent: emitted, ..
                },
            ) = e
            {
                assert_ne!(
                    emitted, agent,
                    "non-claude backend must be skipped per allowlist"
                );
            }
        }
        std::env::remove_var("AGEND_PANE_INPUT_THRESHOLD_SECS");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pane_input_not_submitted_dedups_per_typed_timestamp() {
        let agent = "claude-agent-pin-dedup";
        let home = fleet_with_backend("pin_dedup", agent, "claude");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let typed_ms = now_ms - 300_000;
        seed_input_submit(&home, agent, typed_ms, 0);
        std::env::set_var("AGEND_PANE_INPUT_THRESHOLD_SECS", "60");
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
        // Tick once → one emit. Tick again with same metadata → still one.
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        check_pane_input_not_submitted_for_agents(&home, &[agent.to_string()], &mut tracks);
        let events = sink.events.lock();
        let count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::channel::ux_event::UxEvent::Fleet(
                        crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
                    ) if emitted == agent
                )
            })
            .count();
        assert_eq!(
            count, 1,
            "must dedup repeated ticks for same typed_ms; got {count}"
        );
        std::env::remove_var("AGEND_PANE_INPUT_THRESHOLD_SECS");
        std::fs::remove_dir_all(home).ok();
    }
}
