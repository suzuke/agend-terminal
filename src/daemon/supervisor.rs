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

/// #1552: minimum continuous time in a runtime prompt state before escalating
/// to AwaitingOperator (stability gate — a transient streaming flicker of the
/// permission chrome never holds this long).
const AWAITING_STABILITY: Duration = Duration::from_secs(8);
/// #1552: how many live bottom rows the permission chrome must render in for
/// the escalation position gate. A scrollback footer (the meta-FP: a working
/// agent whose pane merely echoes the chrome) scrolls above this and fails.
const AWAITING_TAIL_LINES: usize = 12;
/// #1552: suppress escalation when the operator typed into the pane within this
/// window — they're actively dealing with the prompt, no buzz needed.
const AWAITING_ENGAGEMENT_WINDOW_MS: i64 = 15_000;
/// #1523: minimum continuous time in `AuthError` before the operator re-auth
/// alert fires. The `AuthError` pattern is content-FP-prone (transient PTY
/// content can flip it cosmetically — an instance was observed self-healing back
/// to `Thinking` in ~31s, producing a false "check credentials" buzz). This
/// stability gate defers the NOTIFICATION (only) until the state has been held
/// well past that observed self-heal window: a real auth failure persists
/// indefinitely and still notifies; a transient blip clears before the window
/// and never does. 90s = ~3× the observed 31s self-heal, with margin. Sibling to
/// `AWAITING_STABILITY` (#1552); classification/retry/timers are untouched.
const AUTH_ERROR_NOTIFY_STABILITY: Duration = Duration::from_secs(90);
/// #1696: tiered retry budget. Phase A burst (5/15/30s) handles instant jitter;
/// Phase B backoff (1m/2m/5m) handles minute-scale proxy faults; Phase C
/// sustained (10m × 6 = 1hr) keeps a "pilot light" through a long outage. The
/// 2026-06-02 incident gave up at ~80s (Phase A only) against a multi-minute
/// proxy fault. Total budget ~75min over 12 retries.
const SERVER_RATE_LIMIT_MAX_RETRIES: u32 = 12;
/// #1742-F4: max ApiError quick-nudges per flicker-window before the nudge stops.
/// Mirrors the ServerRateLimit 12-cap. A content false-positive `ApiError↔Thinking`
/// flicker re-arms the per-episode dedup every cycle, so without a total cap the
/// quick-nudge would inject indefinitely (bounded only by `CONTINUE_INJECT_MIN_INTERVAL`).
/// The count resets on genuine recovery (`Idle`), so a real single ApiError still
/// nudges and only a pathological flicker is capped.
const APIERROR_NUDGE_MAX: u32 = 12;
/// Backoff schedule for ServerRateLimit retries (seconds). Phase A | B | C.
const SERVER_RATE_LIMIT_BACKOFF: [u64; 12] =
    [5, 15, 30, 60, 120, 300, 600, 600, 600, 600, 600, 600];
/// #1696: backoff index where Phase B (minute-scale) begins (for the escalation
/// INFO log).
const RETRY_PHASE_B_START: u32 = 3;
/// #1696: backoff index where Phase C (sustained 10-min) begins.
const RETRY_PHASE_C_START: u32 = 6;
/// #1696/#1697: minimum spacing between two continue-injects to the SAME agent
/// across the retry and ApiError-nudge paths (guards a ServerRateLimit↔ApiError
/// state flicker from double-injecting).
const CONTINUE_INJECT_MIN_INTERVAL: Duration = Duration::from_secs(5);
/// #1742: consecutive ServerRateLimit `continue`-inject FAILURES (agent still
/// present, but the PTY write erred) tolerated before giving up. A single failure
/// is treated as a transient PTY blip that self-heals, so the track is KEPT and
/// re-attempted next tick — only after this many back-to-back failures is the
/// track exhausted (and only THEN via the full notification path). Before #1742 a
/// single failed inject silently set `exhausted` with no operator/orchestrator
/// notice, permanently disabling auto-recovery for a still-throttled agent.
const MAX_INJECT_FAILURES: u32 = 3;
/// #1742: short re-attempt delay after a transient inject failure (well below the
/// Phase-A first backoff) so a recoverable PTY blip is retried promptly rather
/// than waiting out the tiered backoff.
const RETRY_AFTER_INJECT_FAIL: Duration = Duration::from_secs(5);
/// Fixed payload injected on ServerRateLimit recovery retry.
/// "continue\n" is a universal resume signal: all supported backends
/// (ClaudeCode, KiroCli, Codex, OpenCode, Gemini, Agy) accept it as
/// free-form user input. `inject_to_agent` appends the backend's
/// `submit_key` ("\r" for all current backends) after this payload.
///
/// #1316: replaces the old `last_input_text` replay that caused
/// infinite modal-keystroke loops.
const CONTINUE_RETRY_PAYLOAD: &[u8] = b"continue\n";

/// Per-agent notify tracking: last notify time + consecutive error count.
pub(crate) struct NotifyTrack {
    last_at: Instant,
    consecutive: u32,
}

/// #1523: a deferred `AuthError` member-notify awaiting stability confirmation.
/// Recorded when the agent transitions INTO `AuthError`; the actual operator
/// notify is held until the state has been continuously held
/// `AUTH_ERROR_NOTIFY_STABILITY`. Carries the originating `from` state + pane
/// tail captured at the edge so the eventual notify renders the real transition.
struct PendingAuthError {
    from: crate::state::AgentState,
    pane_tail: String,
}

/// #1523: outcome of the `AuthError` content-FP stability gate, evaluated each
/// tick against the agent's current held-duration in `AuthError`.
#[derive(Debug, PartialEq, Eq)]
enum AuthErrorGate {
    /// Still in `AuthError` but not held long enough — keep deferring.
    Wait,
    /// Held ≥ `AUTH_ERROR_NOTIFY_STABILITY` — emit the deferred notify (once).
    Fire,
    /// No longer in `AuthError` — the blip self-healed; drop any pending notify.
    Cancel,
}

/// #1523: decide the stability-gate action for a (possibly) pending `AuthError`.
///
/// `auth_error_held` is `Some(elapsed)` iff the agent's CURRENT state is
/// `AuthError` (the held-duration comes straight from `StateTracker::since`, the
/// authoritative state-age — no separate clock to drift), else `None`.
///
/// Pure + testable: `None` → `Cancel`; `Some(held < N)` → `Wait`;
/// `Some(held ≥ N)` → `Fire`. This is the whole FP fix — a transient `AuthError`
/// (state leaves before N → `None` on a later tick → `Cancel`) never reaches
/// `Fire`, while a sustained one does.
fn auth_error_gate(auth_error_held: Option<Duration>) -> AuthErrorGate {
    match auth_error_held {
        Some(held) if held >= AUTH_ERROR_NOTIFY_STABILITY => AuthErrorGate::Fire,
        Some(_) => AuthErrorGate::Wait,
        None => AuthErrorGate::Cancel,
    }
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
    /// Set when max retries exceeded — prevents re-triggering on same
    /// persistent ServerRateLimit state. Cleared on recovery (Idle).
    pub exhausted: bool,
    /// #1742: consecutive `continue`-inject failures while the agent is STILL
    /// present (PTY write erred). Reset to 0 on a successful inject. Only when it
    /// reaches `MAX_INJECT_FAILURES` is the track exhausted — and then via the
    /// full notification path, never silently.
    pub inject_failures: u32,
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
///
/// `daemon_binary_stale` is the shared TUI status-bar flag the
/// mcp_registry_watcher tracker flips when a post-startup binary
/// refresh is detected (#1027). Callers without a TUI (headless daemon
/// mode) pass a throwaway `Arc<AtomicBool>` — the flag still gets set
/// but nothing is wired to surface it.
pub fn spawn(
    home: PathBuf,
    registry: AgentRegistry,
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) {
    // fire-and-forget: supervisor tick loop runs for the process lifetime
    // (per module-doc rationale at lines 6-8 — "shutdown is implicit: when
    // the hosting process exits, this thread dies with it"). 10s tick
    // cadence; no graceful-stop needed because supervisor is read-mostly
    // (per-tick metadata read + occasional channel notify).
    let _ = thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || run_loop(home, registry, daemon_binary_stale));
}

fn run_loop(
    home: PathBuf,
    registry: AgentRegistry,
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) {
    let mut notify_tracks: HashMap<String, NotifyTrack> = HashMap::new();
    // #1523: deferred AuthError member-notifies awaiting stability confirmation.
    let mut pending_auth: HashMap<String, PendingAuthError> = HashMap::new();
    let mut retry_tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    // #1697: agents currently in a nudged ApiError episode (re-armed on leaving
    // the ApiError state). #1696/#1697: last continue-inject time per agent,
    // shared by the retry + ApiError-nudge paths for the anti-thrash min-interval
    // and the #1586 clear cooldown.
    let mut apierror_episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    // #1742-F4: per-agent total ApiError-nudge count for the current flicker
    // window. Distinct lifecycle from `apierror_episodes` (which re-arms every
    // episode): this only resets on genuine recovery (Idle), so it caps a
    // pathological ApiError↔Thinking flicker at APIERROR_NUDGE_MAX nudges.
    let mut apierror_nudge_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut last_continue_inject: HashMap<String, Instant> = HashMap::new();
    let mut pane_input_tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    // Sprint 59 Wave 1 PR-1 (#9 task stall watchdog): per-task ETA
    // scanner, throttled to 5min via TICKS_PER_SCAN.
    let mut anti_stall_tracker = crate::daemon::anti_stall::AntiStallTracker::default();
    // Sprint 59 Wave 1 PR-2 (#10+#12 watchdog cluster): per-agent +
    // fleet-wide idle thresholds, throttled to 5min scans.
    let mut idle_watchdog_tracker = crate::daemon::idle_watchdog::IdleWatchdogTracker::default();
    // #1022: purge activity sidecars for instances not in fleet.yaml
    // so ghost agents from prior runs don't pollute the tracking list.
    crate::daemon::idle_watchdog::gc_stale_activity_sidecars(&home);
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
    // Phase A Piece-1+2: per-tick git conflict observation + 30min
    // escalation. Sibling to waiting_on_stale (same TICKS_PER_SCAN
    // cadence, same REALERT_INTERVAL_SECS dedup window). No new
    // spawn site — supervisor's per-tick loop hosts the scan.
    let mut conflict_notify_tracker =
        crate::daemon::conflict_notify::ConflictNotifyTracker::default();
    // #852 residual PR-B: per-tick canonical-drift scan. Sibling to
    // waiting_on_stale + conflict_notify (same TICKS_PER_SCAN cadence,
    // same supervisor-hosted no-new-spawn-site pattern). Catches
    // detached-HEAD residue accrued AFTER daemon boot for long-lived
    // daemons; reuses the boot-time canonical_hygiene helper.
    let mut canonical_drift_tracker =
        crate::daemon::canonical_drift::CanonicalDriftTracker::default();
    // #870: per-tick auto-release scan. Sibling pattern; faster
    // cadence (TICKS_PER_SCAN=3 ≈ 30s) than the 30-tick siblings
    // because release latency directly gates the next-cycle lease-
    // conflict surface this module exists to eliminate. No new spawn
    // site — supervisor's per-tick loop hosts the scan.
    let mut auto_release_tracker = crate::daemon::auto_release::AutoReleaseTracker::default();
    // PR1 watchdog L1: cross-team-safe dispatch-idle scan. Sibling
    // pattern; TICKS_PER_SCAN=6 (~60s) because the threshold this
    // gates (single-digit-minute orchestrator dispatches) demands
    // sub-minute fire-time accuracy. No new spawn site — supervisor's
    // per-tick loop hosts the scan.
    let mut dispatch_idle_tracker = crate::daemon::dispatch_idle::DispatchIdleTracker::default();
    // PR1 watchdog L2: fixup-team-specific auto-nudge on exceeded
    // dispatches. Same cadence as L1; hard-coded for fixup until a
    // second team requests its own policy (L2.1 schema bump).
    let mut dispatch_idle_fixup_nudge_tracker =
        crate::daemon::dispatch_idle::fixup_nudge::DispatchIdleFixupNudgeTracker::default();
    let mut retention_supervisor = crate::daemon::retention::RetentionSupervisor::default();
    // #1741 boot-grace anchor: this Instant ≈ supervisor/daemon boot (set once,
    // never reset). It gates the every-tick pane-input diagnostic so a restart's
    // freshly-empty `pane_input_tracks` dedup map can't re-emit for inputs typed
    // BEFORE the restart (typically a pre-existing operator draft). Mirrors the
    // #1736 `NOTIFICATION_BOOT_GRACE` suppression already used by the notification
    // watchdogs; the slow (5-min-scan) sibling watchdogs are out of scope here
    // because their first scan lands after the 180s grace window.
    let loop_started_at = Instant::now();
    loop {
        thread::sleep(TICK);
        // #1125 M1: wrap the entire tick body in catch_unwind so a panic
        // in any tracker's maybe_scan() doesn't kill the supervisor thread.
        // Mirrors the run_handlers_with_panic_guard pattern from per_tick.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tick(&home, &registry, &mut notify_tracks, &mut pending_auth);
            process_error_recovery(
                &home,
                &registry,
                &mut retry_tracks,
                &mut apierror_episodes,
                &mut apierror_nudge_counts,
                &mut last_continue_inject,
            );
            check_pane_input_not_submitted(
                &home,
                &registry,
                &mut pane_input_tracks,
                loop_started_at,
            );
            anti_stall_tracker.maybe_scan(&home);
            idle_watchdog_tracker.maybe_scan(&home);
            decision_timeout_tracker.maybe_scan(&home);
            helper_staleness_tracker.maybe_scan(&home);
            mcp_registry_tracker.maybe_scan(&daemon_binary_stale);
            waiting_on_stale_tracker.maybe_scan(&home);
            conflict_notify_tracker.maybe_scan(&home, &registry);
            canonical_drift_tracker.maybe_scan(&home);
            auto_release_tracker.maybe_scan(&home);
            dispatch_idle_tracker.maybe_scan(&home);
            dispatch_idle_fixup_nudge_tracker.maybe_scan(&home);
            retention_supervisor.maybe_sweep(&home);
            // #1002 Phase 2: pr_state per-tick scan must run here so APP
            // mode (`agend-terminal app`) drives the #972 aggregator + #986
            // gh-poll integration the same way as daemon mode. Before this
            // line, `PrStateScanHandler` was wired ONLY into `run_core`'s
            // `PerTickHandler` vec (dual-entry-point divergence); APP-mode
            // operators (the agent fleet) saw `last_gh_poll_at: null`
            // indefinitely + no `[pr-ready-for-merge]` events.
            // Source-pin: `pr_state_scan_wired_into_supervisor_loop`.
            crate::daemon::pr_state::scan_and_emit(&home, &registry);
            // #836: reclaim expired (10-min TTL) entries from the
            // notification-dedup ledger so memory pressure stays bounded
            // on long-lived daemons.
            crate::daemon::notification_dedup::global().sweep_expired();
            // #842: same eviction cadence for the bridge↔daemon idempotent-
            // retry dedup cache. Sibling sweep, same 10-min TTL window.
            crate::api::request_dedup::global().sweep_expired();
        }));
        if let Err(payload) = outcome {
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else {
                "<non-string panic payload>".to_string()
            };
            tracing::error!(
                payload = %msg,
                "#1125 supervisor tick panicked — next tick will re-run all scans"
            );
        }
    }
}

/// Sprint 54 P2-3: per-tick check for "typed but not submitted" pane
/// state. Read-only — emits a `FleetEvent::PaneInputNotSubmitted` when
/// the threshold is exceeded but does NOT inject prompts, mutate agent
/// state, or touch the router layer (router = `src/channel/router/*`,
/// Sprint 49/52 territory). Backend support is now all backends that
/// declare a submit key (via `preset().submit_key`) — #1457 widened it
/// from the original claude-only first round once `record_submit_activity`
/// was wired for every backend.
///
/// Threshold defaults to 60s; override via env
/// `AGEND_PANE_INPUT_THRESHOLD_SECS`.
pub(crate) fn check_pane_input_not_submitted(
    home: &std::path::Path,
    registry: &AgentRegistry,
    tracks: &mut HashMap<String, PaneInputTrack>,
    loop_started_at: Instant,
) {
    let agent_names: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|h| h.name.to_string()).collect()
    };
    check_pane_input_not_submitted_for_agents(home, &agent_names, tracks, loop_started_at);
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
    loop_started_at: Instant,
) {
    // #1741 boot-grace: `tracks` (the per-agent last-emitted dedup) is in-memory
    // and zeroed on every daemon restart. Within the first ticks after a restart
    // the diagnostic would therefore re-fire for any input typed BEFORE the
    // restart — which the pane-input detector cannot distinguish from a genuine
    // fresh strand (the timestamps it reads are operator-keystroke-only). Suppress
    // the whole scan for `NOTIFICATION_BOOT_GRACE` after boot; a still-stranded
    // input re-emits exactly once after the grace ends (its typed_ms is well past
    // the 60s threshold by then). Reuses the #1736 watchdog boot-grace helper.
    if crate::daemon::per_tick::in_boot_grace(loop_started_at) {
        return;
    }
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

/// Backend support for submit detection. #1457 widened this from the
/// Sprint 54 P2-3 claude-only first round to ALL backends that declare a
/// submit key — paired with `app::pane_input_contains_submit` (which now
/// records the submit timestamp for every backend). Resolves the agent's
/// backend via fleet.yaml so per-instance overrides are honoured. A backend
/// with no submit key (Shell/Raw) is unsupported (can't detect submission).
fn pane_input_backend_supported(home: &std::path::Path, agent: &str) -> bool {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return false;
    };
    let Some(resolved) = fleet.resolve_instance(agent) else {
        return false;
    };
    crate::backend::Backend::from_command(&resolved.backend_command)
        .map(|b| !b.preset().submit_key.is_empty())
        .unwrap_or(false)
}

/// #1563: resolve an agent's idle policy from fleet.yaml (cached load).
/// Unknown agent / unreadable config → `Active` (default; preserves pre-#1563
/// behavior so a misconfiguration never silently suppresses a real stall).
fn idle_expectation_for(home: &std::path::Path, agent: &str) -> crate::fleet::IdleExpectation {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|cfg| cfg.instances.get(agent).map(|ic| ic.idle_expectation))
        .unwrap_or_default()
}

/// #1595 Step 2 (pure): which states warrant escalating a self-orchestrator
/// (the orchestrator IS the affected agent → no peer can relay) straight to
/// operator Telegram? Only `AuthError` — terminal with operator-only resolution
/// (re-auth). Transient states (RateLimit/UsageLimit) recover and the agent
/// relays then; Crashed/Hang are not live AgentStates via this hook (#1701).
fn self_orchestrator_escalates(new_state: crate::state::AgentState) -> bool {
    new_state == crate::state::AgentState::AuthError
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
        // #1595 Step 2: the orchestrator IS the affected agent — no peer can relay
        // its inbox P0. For a state only the operator can resolve (AuthError: only
        // the operator can re-authenticate), escalate straight to operator Telegram
        // via gated_notify(Error) — the same Sleep-penetrating path #1594 allows
        // through. Cooldown-stamped so a persistent AuthError escalates at most
        // once per NOTIFY_COOLDOWN, not every tick. Other states keep the D3
        // self-notify skip (transient / the agent reads its own inbox).
        // NOTE: Crashed/Hang are NOT live AgentStates via this hook (never assigned
        // to `state.current`); real crash/hang self-orchestrator escalation is a
        // follow-up (#1701) using the process-exit / HealthState::Hung paths (the
        // latter strong-gated for the known 348-FP).
        if self_orchestrator_escalates(new_state) {
            let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
                last_at: now,
                consecutive: 0,
            });
            track.consecutive += 1;
            track.last_at = now;
            if let Some(ch) = crate::channel::active_channel() {
                let msg = format!(
                    "🔑 {name} (team orchestrator) hit AuthError — only the operator can re-authenticate, and no peer can relay this. Check credentials / re-auth the agent."
                );
                let _ = crate::channel::gated_notify(
                    ch.as_ref(),
                    name,
                    NotifySeverity::Error,
                    &msg,
                    false,
                );
            }
            tracing::info!(agent = %name, "#1595: self-orchestrator AuthError escalated to operator Telegram (no peer to relay)");
        }
        return false; // D3: still skip the inbox self-notify (no peer reads it)
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
    // #event-bus pattern #9, Step 2 (legacy-zero): freeze the only now()-derived
    // value (detected_at) here so the subscriber renders the inbox payload
    // byte-identically, then emit MemberStateChanged (the subscriber delivers via
    // `deliver_member_state_change`). The bus is the sole delivery path.
    let detected_at = chrono::Utc::now().to_rfc3339();
    let from_display = prev_state.display_name();
    let to_display = new_state.display_name();
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::MemberStateChanged {
            agent: name.to_string(),
            team: team.name.clone(),
            from_state: from_display.to_string(),
            to_state: to_display.to_string(),
            orch: orch.clone(),
            new_state,
            pane_tail: pane_tail.to_string(),
            unlock_at: unlock_at.clone(),
            consecutive_count: track.consecutive,
            detected_at,
        },
    );
    true
}

/// Shared deliver for the member-state-change notify: (A) enqueue the structured
/// JSON event to the orchestrator's inbox, (B) PTY-notify the orchestrator with
/// the human-readable line + action hint. Called by BOTH the legacy direct path
/// AND the event-bus subscriber, so A and B are byte-identical by construction
/// (the gate only chooses which path invokes this fn — the fn itself is fixed).
/// The notify_agent half (B) is a PTY-inject, not an inbox enqueue, so it is not
/// drain-assertable in tests; it is covered by this shared-deliver-fn invariant
/// (parity tests assert the inbox half A). All now()-derived input (`detected_at`)
/// is passed in frozen so the bus path reproduces A byte-for-byte.
#[allow(clippy::too_many_arguments)]
fn deliver_member_state_change(
    home: &std::path::Path,
    orch: &str,
    name: &str,
    team_name: &str,
    from_display: &str,
    to_display: &str,
    new_state: crate::state::AgentState,
    pane_tail: &str,
    unlock_at: Option<&str>,
    consecutive: u32,
    detected_at: &str,
) {
    // (A) structured JSON inbox enqueue.
    let payload = serde_json::json!({
        "type": "member_state_change",
        "member": name,
        "team": team_name,
        "from_state": from_display,
        "to_state": to_display,
        "detected_at": detected_at,
        "context": {
            "last_pane_excerpt": pane_tail,
            "unlock_at": unlock_at,
            "consecutive_count": consecutive,
        }
    });
    let msg = crate::inbox::InboxMessage::new_system(
        "system:supervisor",
        "member_state_change",
        payload.to_string(),
    );
    persist_or_log!(
        crate::inbox::enqueue(home, orch, msg),
        "member_state_change",
        orch
    );
    // (B) human-readable PTY notify with action hint.
    let action_hint = match new_state {
        crate::state::AgentState::Hang => {
            "\nAction: check agent pane snapshot, consider restart if no progress >5min"
        }
        crate::state::AgentState::UsageLimit => {
            "\nAction: wait for limit reset or switch backend. Do NOT retry."
        }
        crate::state::AgentState::Crashed => {
            "\nAction: check logs, restart agent, reassign task if needed"
        }
        crate::state::AgentState::PermissionPrompt => {
            "\nAction: approve or deny the pending permission prompt"
        }
        crate::state::AgentState::RateLimit => {
            "\nAction: wait for rate limit cooldown, auto-retry expected"
        }
        crate::state::AgentState::AuthError => {
            "\nAction: check credentials, may need operator re-auth"
        }
        _ => "",
    };
    crate::inbox::notify_agent(
        home,
        orch,
        &crate::inbox::NotifySource::System("supervisor"),
        &format!("[member_state_change] {name}: {from_display} → {to_display}{action_hint}"),
    );
    tracing::info!(agent = %name, from = %from_display, to = %to_display, orchestrator = %orch, "member-state-change notify sent");
}

/// #event-bus pattern #9 subscriber: re-deliver a `MemberStateChanged` event via
/// the shared `deliver_member_state_change`.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::MemberStateChanged {
        agent,
        team,
        from_state,
        to_state,
        orch,
        new_state,
        pane_tail,
        unlock_at,
        consecutive_count,
        detected_at,
    } = &event.kind
    {
        deliver_member_state_change(
            &event.home,
            orch,
            agent,
            team,
            from_state,
            to_state,
            *new_state,
            pane_tail,
            unlock_at.as_deref(),
            *consecutive_count,
            detected_at,
        );
        true
    } else {
        false
    }
}

/// Register the member-state-change subscriber once at daemon startup (`run_core`).
/// Home-agnostic — the home travels on each event.
pub(crate) fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

/// #1530: a reaction-worthy net state change for one agent in one tick.
/// `to` is guaranteed reaction-worthy (`is_notify_error_class`, which
/// includes `UsageLimit`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReactionDecision {
    from: crate::state::AgentState,
    to: crate::state::AgentState,
}

/// #1530: enriched [`ReactionDecision`] carrying the data captured under the
/// core lock so the actual reaction emit can run lock-free after `drop(core)`.
struct ReactionIntent {
    from: crate::state::AgentState,
    to: crate::state::AgentState,
    backend: Option<crate::backend::Backend>,
    /// 3-line PTY tail for the operator UsageLimit notice.
    snippet: String,
    /// 10-line PTY tail for the member-state-change notice.
    pane_tail: String,
}

/// #1530: which reactions a net `to` state drives. Pure + testable — proves the
/// emit routing (esp. that a `UsageLimit` final state ALSO produces a
/// `MemberNotify`, which the pre-#1530 `propagate ... continue` silently ate).
#[derive(Debug, PartialEq, Eq)]
enum ReactionKind {
    NotifyOperator,
    Propagate,
    MemberNotify,
}

/// #1530: derive the NET state change across the drained transition list and
/// return a reaction decision iff the net `to` differs from the net `from` AND
/// is reaction-worthy.
///
/// Net-state (not per-transition) semantics: an intra-tick flap that enters
/// then leaves an error state (e.g. `Idle→UsageLimit→Idle`) has no net change
/// → no reaction, so transient blips don't spam the operator/orchestrator.
/// Transition LOGGING records every transition separately (#1527); only the
/// reaction converges to the final state.
///
/// This replaces the pre-#1530 `if prev_state != new_state` gate, which was
/// blind to feed-driven transitions (they complete async in the read-loop
/// thread, so `prev == new` by the next supervisor tick) — see #1530.
fn reactions_from_transitions(
    transitions: &[crate::state::TransitionRecord],
) -> Vec<ReactionDecision> {
    let (Some(first), Some(last)) = (transitions.first(), transitions.last()) else {
        return Vec::new();
    };
    let (from, to) = (first.from, last.to);
    if from == to || !to.is_notify_error_class() {
        return Vec::new();
    }
    vec![ReactionDecision { from, to }]
}

/// #1530: pure emit-routing. `UsageLimit` → operator notice (+ propagate when
/// enabled) AND member-notify (UsageLimit ∈ `is_notify_error_class`); any other
/// error-class state → member-notify only. Keeping this separate from the emit
/// lets a unit test assert no reaction is dropped (the regression the removed
/// `continue` caused).
fn reaction_kinds(to: crate::state::AgentState, propagation_enabled: bool) -> Vec<ReactionKind> {
    let mut kinds = Vec::new();
    if to == crate::state::AgentState::UsageLimit {
        kinds.push(ReactionKind::NotifyOperator);
        if propagation_enabled {
            kinds.push(ReactionKind::Propagate);
        }
    }
    if to.is_notify_error_class() {
        kinds.push(ReactionKind::MemberNotify);
    }
    kinds
}

/// #1552: escalation FP-gate for runtime AwaitingOperator (dev-2 design,
/// decision d-20260531141354559067-0). `check_awaiting_operator` is the
/// *necessary* silence+state condition; this adds the *sufficient* guards so a
/// permission-chrome false positive — a working agent whose pane merely echoes
/// the footer chrome (e.g. in scrollback: state-detection is full-screen, so it
/// fires `PermissionPrompt` on healthy agents working on detection code) — can't
/// escalate into a false operator buzz + a sticky false blocked-state.
///
/// `Starting` keeps its original ungated path (a startup stall has no chrome to
/// position-gate). The runtime prompt states require all three gates; the real
/// dialog satisfies all three, the meta-FP fails ≥1. Pure (file/now values are
/// passed in) so it is unit-testable without a daemon.
fn awaiting_escalation_allowed(
    state: crate::state::AgentState,
    state_held: Duration,
    backend: Option<crate::backend::Backend>,
    live_tail: &str,
    operator_typed_ms: i64,
    now_ms: i64,
    idle_expectation: crate::fleet::IdleExpectation,
) -> bool {
    use crate::state::AgentState;
    match state {
        // Original startup-stall fallback — no chrome, no position gate.
        // #1563: an `OnDemand` coordinator (e.g. `general`) is permanently stuck
        // in `Starting` (never matches an Idle banner), so this ungated path
        // would forward its normal pane to the operator forever. Gate the
        // startup-stall fallback by role. (Part-B below role-gates
        // `InteractivePrompt` too — same prose-FP root — while `PermissionPrompt`
        // stays role-blind so a real permission dialog still escalates, #1552.)
        AgentState::Starting => idle_expectation == crate::fleet::IdleExpectation::Active,
        AgentState::PermissionPrompt | AgentState::InteractivePrompt => {
            // #1563 part-B: split the role policy across the two prompt states.
            // `PermissionPrompt` is chrome-anchored (#1546, near-zero FP), so a
            // REAL permission dialog must escalate for ANY role (#1552/#1564) —
            // role-blind. `InteractivePrompt`'s ONLY source is the weak,
            // prose-FP-prone `is_generic_startup_prompt` (`Starting`-only), so an
            // `OnDemand` coordinator (e.g. `general`) permanently stuck in
            // `Starting` forwards its PR-review prose as a fake "interactive
            // prompt". Gate `InteractivePrompt` by role, mirroring the `Starting`
            // arm above; the #1564 gates (position/stability/engagement) below
            // still apply to BOTH.
            let role_ok = state != AgentState::InteractivePrompt
                || idle_expectation == crate::fleet::IdleExpectation::Active;
            role_ok
                // (b) stability: the prompt state held continuously long enough.
                && state_held >= AWAITING_STABILITY
                // (a) position (mandatory): the prompt chrome must re-detect in
                // the LIVE bottom rows. A scrollback echo fails this — it's the
                // only gate that catches a finished agent sitting on a footer.
                && backend.is_some_and(|b| {
                    matches!(
                        crate::state::StatePatterns::for_backend(&b).detect(live_tail),
                        Some(AgentState::PermissionPrompt | AgentState::InteractivePrompt)
                    )
                })
                // (c) engagement: the operator isn't actively typing into it.
                && !(operator_typed_ms > 0
                    && now_ms.saturating_sub(operator_typed_ms) < AWAITING_ENGAGEMENT_WINDOW_MS)
        }
        _ => false,
    }
}

/// One iteration of the supervisor loop. Public for tests.
fn tick(
    home: &std::path::Path,
    registry: &AgentRegistry,
    notify_tracks: &mut HashMap<String, NotifyTrack>,
    pending_auth: &mut HashMap<String, PendingAuthError>,
) {
    // Snapshot the agent names + handles so we can release the registry lock
    // before touching any per-agent core lock. Holding both at once risks
    // deadlocks against code paths that take core then registry.
    // #1441: registry is UUID-keyed; carry the id for the re-lock lookup and
    // the display name for the many name-keyed side channels in this loop.
    // #1530/F2: also capture each agent's backend_command here (under the
    // registry lock, registry→core order) so the per-agent loop never needs to
    // RE-acquire the registry while holding that agent's core — the core→registry
    // inversion that risked an AB-BA deadlock with the registry→core render/
    // monitor loops. `Backend::from_command` on the captured string is lock-free.
    let handles: Vec<(String, String, _)> = {
        let reg = agent::lock_registry(registry);
        reg.values()
            .map(|h| {
                (
                    h.name.to_string(),
                    h.backend_command.clone(),
                    Arc::clone(&h.core),
                )
            })
            .collect()
    };

    for (name, backend_command, core) in handles {
        // #1665 reply-ledger: TTL/settled fallback for a user-message turn that
        // never hit a clear site (no reply, no mirror, no takeover). Lock-free
        // snapshot read; warns only past the grace window AND when the agent has
        // settled. Infallible — never blocks the supervisor loop.
        crate::reply_ledger::sweep(home, &name);
        // Mutate state + pull the tail under the core lock, then drop it
        // before running `format!` and the Telegram spawn. `tail_lines`
        // allocates a fresh String, so the lock window is bounded by the
        // vterm copy — no async IO or string formatting held against it.
        //
        // #1530: reaction intents collected under the core lock below, emitted
        // lock-free after the lock drops (the member-notify path self-IPCs).
        let mut reaction_intents: Vec<ReactionIntent> = Vec::new();
        // #1523: held-duration in AuthError captured under the lock (Some iff the
        // current state IS AuthError), consumed by the stability gate after the
        // lock drops. Sourced from `StateTracker::since` — the authoritative
        // state-age — so no separate clock can drift. Assigned exactly once
        // inside the (unconditional) lock block below.
        let auth_error_held: Option<Duration>;
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
            core.state.tick();

            // #1527: log EVERY transition recorded at its source (read-loop
            // `feed` AND `tick`), by draining the per-tracker buffer — replaces
            // the old prev/new-at-tick comparison, which silently missed any
            // transition that completed async between two supervisor ticks
            // (prev==new), i.e. nearly all feed-driven ones including the error
            // states. `log_state_transition_at` is a file append (no self-IPC,
            // no new lock) so logging under the core lock is #1492-safe.
            let snippet = core.vterm.tail_lines(3);
            let (transitions, dropped) = core.state.drain_pending_transitions();
            if dropped > 0 {
                tracing::warn!(
                    agent = %name,
                    dropped,
                    "#1527: transition-log buffer overflowed (drainer fell behind) — oldest dropped"
                );
            }
            for t in &transitions {
                crate::daemon::usage_limit::log_state_transition_at(
                    home, &name, t.from, t.to, &t.ts, &snippet,
                );
            }

            // #1530: de-gate the UsageLimit + member-state reactions off the
            // (feed-blind) `prev != new` tick comparison. React on the NET state
            // change derived from the #1527 drained transition list, which is
            // authoritative and includes feed-driven transitions (the ones the
            // old gate missed — they complete async, so prev==new at tick).
            //
            // Collect intents UNDER the core lock here (capturing backend + PTY
            // tails), then emit them lock-free AFTER `drop(core)` below. The
            // member-notify path self-IPCs (`api::call(INJECT)` → orchestrator)
            // and must not run under the core lock — that would risk the #1492
            // lock-across-self-IPC deadlock. This boundary is now enforced two
            // ways (the comment-only era is over): (1) RUNTIME — `core` is a
            // `CoreMutex`, so `core.lock()` bumps `CORE_LOCK_DEPTH`, and the
            // self-IPC vectors call `assert_no_registry_lock_for_self_ipc`, which
            // since #1535 checks the core tier too — an emit moved under the lock
            // returns a fail-fast `Err` (daemon stays live, no freeze) instead of
            // deadlocking; (2) CI — `tick_emitters_run_after_core_lock_drops`
            // (#1644) source-grep-pins that the self-IPC emitters live after the
            // `let action = { … }` lock block, catching a regression before it
            // ships. The big `supervise_one()->TickOutcome` extraction that would
            // make it compile-impossible is deferred (#1644) — revisit when a new
            // reaction is added to this loop; both guards above make that safe.
            for decision in reactions_from_transitions(&transitions) {
                // #1530/F2: backend resolved from the pre-captured command
                // (no registry re-acquire while holding core).
                let backend = crate::backend::Backend::from_command(&backend_command);
                reaction_intents.push(ReactionIntent {
                    from: decision.from,
                    to: decision.to,
                    backend,
                    snippet: snippet.clone(),
                    pane_tail: core.vterm.tail_lines(10),
                });
            }

            // §4.4 stale decay: clear waiting_on when heartbeat is stale.
            clear_waiting_on_if_stale(home, &name, !core.state.is_heartbeat_fresh());

            let agent_state = core.state.current;
            // #1523: capture how long AuthError has been continuously held (state
            // age) for the post-lock stability gate. `Some` iff currently in
            // AuthError; the gate uses it to confirm/cancel a deferred notify.
            auth_error_held = (agent_state == crate::state::AgentState::AuthError)
                .then(|| core.state.since.elapsed());
            let silent = core.state.last_output.elapsed();
            // #1563: role policy gates the two `Starting`-context stall-forward
            // paths (branch-1 startup-stall, branch-2 startup-prose prompt) for
            // an `OnDemand` coordinator; the runtime permission/interactive
            // escalation stays role-blind (handled inside the fn).
            let idle_expectation = idle_expectation_for(home, &name);
            if core.health.check_awaiting_operator(agent_state, silent) && {
                // #1552 escalation FP-gates (only reached when silent>30s +
                // a prompt state). #1530/F2: backend resolved from the
                // pre-captured command — NO registry re-acquire while holding
                // core (removes the core→registry inversion).
                let backend = crate::backend::Backend::from_command(&backend_command);
                let (typed_ms, _submit_ms) =
                    crate::notification_queue::read_input_submit_timestamps(home, &name);
                awaiting_escalation_allowed(
                    agent_state,
                    core.state.since.elapsed(),
                    backend,
                    &core.vterm.tail_lines(AWAITING_TAIL_LINES),
                    typed_ms,
                    crate::daemon::heartbeat_pair::now_ms() as i64,
                    idle_expectation,
                )
            } {
                // #1552 Half-1 #3: set the HEALTH reason — `check_hang` exempts
                // on `current_reason`, not on the state, so setting the state
                // alone would NOT stop the Hung→Stage-1-ESC path (which would
                // dismiss the real prompt). The reason also doubles as the
                // once-per-episode buzz dedup: if it's already AwaitingOperator
                // we re-affirm state+reason (stay exempt) but don't re-notify,
                // so the chrome (prio 8 > AwaitingOperator prio 2) flipping the
                // state back each tick can't buzz the operator repeatedly.
                let already_escalated = matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::AwaitingOperator)
                );
                core.state.set_awaiting_operator();
                core.health
                    .set_blocked_reason(crate::health::BlockedReason::AwaitingOperator);
                // Consume the recovery flag if somehow armed in the same tick,
                // so the "ready again" ping doesn't fire right after we just
                // re-entered a blocked state.
                let _ = core.state.take_recovery_notice();
                if already_escalated {
                    None
                } else {
                    tracing::info!(
                        agent = %name,
                        silent_secs = silent.as_secs(),
                        prev_state = agent_state.display_name(),
                        "awaiting operator (stalled on prompt) — escalating"
                    );
                    Some(NoticeAction::Stall {
                        tail: core.vterm.tail_lines(TAIL_LINES),
                        silent_secs: Some(silent.as_secs()),
                    })
                }
            } else if core.state.take_interactive_prompt_notice()
                && idle_expectation == crate::fleet::IdleExpectation::Active
            {
                // #1563: `take_…` runs first (always consumes the one-shot flag,
                // so an `OnDemand` agent's notice doesn't accumulate) — then the
                // role gate suppresses the forward. This is the startup-prose
                // co-FP path (`is_generic_startup_prompt` is `Starting`-gated, so
                // a stuck-`Starting` coordinator reading `(y/n)` PR prose would
                // otherwise forward it). The real pattern-based InteractivePrompt
                // escalation for `Active` workers is unchanged.
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
                // #1552: clear the AwaitingOperator health reason on recovery so
                // `check_hang` is no longer exempt and a future stall can
                // re-notify (the once-per-episode dedup re-arms). #1638: the
                // operator-resolution clear-policy is now on the type, so this
                // never clears a different blocked reason (RateLimit / etc.).
                if core.health.current_reason.as_ref().is_some_and(|r| {
                    r.auto_clears_on(crate::health::RecoverySignal::OperatorResolved)
                }) {
                    core.health.clear_blocked_reason();
                }
                tracing::info!(
                    agent = %name,
                    "recovered from blocked state — notifying telegram"
                );
                Some(NoticeAction::Recovered)
            } else {
                None
            }
        };

        // #1530: emit the collected reactions now that the core lock is dropped
        // (#1492-safe — the member-notify self-IPC no longer runs under the
        // lock; propagate already required the drop pre-#1530). Routed through
        // `reaction_kinds` so a UsageLimit final state fires BOTH the operator/
        // propagate path AND member-notify — the latter was silently eaten by
        // the old propagate `continue`.
        if !reaction_intents.is_empty() {
            let config = crate::runtime_config::get();
            for intent in &reaction_intents {
                for kind in reaction_kinds(intent.to, config.usage_limit_propagation_enabled) {
                    match kind {
                        ReactionKind::NotifyOperator => {
                            if let Some(ref b) = intent.backend {
                                crate::daemon::usage_limit::notify_operator_usage_limit(
                                    home,
                                    &name,
                                    b,
                                    &intent.snippet,
                                    &[],
                                );
                            }
                        }
                        ReactionKind::Propagate => {
                            if let Some(ref b) = intent.backend {
                                let affected = crate::daemon::usage_limit::propagate_usage_limit(
                                    home, &name, b, registry,
                                );
                                tracing::warn!(
                                    agent = %name,
                                    affected = ?affected,
                                    "UsageLimit propagated to same-backend agents"
                                );
                            }
                        }
                        ReactionKind::MemberNotify => {
                            // #1523: defer the AuthError member-notify — record it
                            // pending; the stability gate below fires it only once
                            // AuthError has been held past the FP self-heal window.
                            // All other notify-error states keep firing on the edge.
                            if intent.to == crate::state::AgentState::AuthError {
                                pending_auth
                                    .entry(name.clone())
                                    .or_insert(PendingAuthError {
                                        from: intent.from,
                                        pane_tail: intent.pane_tail.clone(),
                                    });
                            } else {
                                maybe_notify_member_state_change(
                                    home,
                                    &name,
                                    intent.from,
                                    intent.to,
                                    &intent.pane_tail,
                                    notify_tracks,
                                );
                            }
                        }
                    }
                }
            }
        }

        // #1523: AuthError content-FP stability gate. AuthError flips cosmetically
        // on transient PTY content and self-heals within ~31s (observed); an
        // edge-triggered operator re-auth alert therefore false-fires. The edge
        // above only RECORDS the pending notify; here it is confirmed (held past
        // AUTH_ERROR_NOTIFY_STABILITY → fire once) or cancelled (state left
        // AuthError → drop pending). Classification, retry, and timers are
        // untouched — only the operator NOTIFICATION is gated.
        match auth_error_gate(auth_error_held) {
            AuthErrorGate::Fire => {
                if let Some(p) = pending_auth.remove(&name) {
                    maybe_notify_member_state_change(
                        home,
                        &name,
                        p.from,
                        crate::state::AgentState::AuthError,
                        &p.pane_tail,
                        notify_tracks,
                    );
                }
            }
            AuthErrorGate::Cancel => {
                pending_auth.remove(&name);
            }
            AuthErrorGate::Wait => {}
        }

        match action {
            Some(NoticeAction::Stall { tail, silent_secs }) => {
                let msg = format_stall_notice(&name, &tail, silent_secs);
                // Outbound info-leak gate (Sprint 21 Phase 1): `tail`
                // carries 40 lines of PTY output — must not leak to a
                // bound group with no operator allowlist configured.
                // `gated_notify` drops the call when the channel is
                // unauthorised; legacy `None`-allowlist deployments
                // require explicit opt-in via `user_allowlist: [...]`.
                //
                // #1339 PR-2 (M3): `Error`, not `Warn`. Both producers of this
                // `Stall` notice are "agent blocked, awaiting the operator" —
                // the #1552 AwaitingOperator escalation and the #1563
                // InteractivePrompt forward. That is exactly the P0 that must
                // break through `Sleep`/`Away`: at `Warn` the gate would
                // silently drop it while the operator is asleep, leaving the
                // agent stuck on a prompt forever. (Severity is routing-only —
                // the Telegram adapter ignores it for rendering — so this does
                // not change the Active-mode message.)
                if let Some(ch) = crate::channel::active_channel() {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        &name,
                        NotifySeverity::Error,
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

/// States that CLEAR a pending ServerRateLimit retry track (cross-episode reset).
///
/// #1713 root-fix: the `continue` inject is now gated on a FRESH `ServerRateLimit`
/// observation at the decision point (Phase 1, under the lock), so a track can no
/// longer blind-fire into a non-error state. That makes this clear-set purely a
/// HYGIENE / cross-episode reset: drop the track once the agent has genuinely
/// recovered, so a later ServerRateLimit episode starts fresh at Phase A (rather
/// than inheriting a stale `retry_count`/`exhausted`).
///
/// Reverted to {Idle} (genuine terminal recovery). #1586 had broadened it
/// to also include Thinking / ToolUse purely to kill the blind-fire retry STORM
/// before the backoff fired — but the #1713 state-gate makes that storm
/// structurally impossible (no inject unless freshly ServerRateLimit), so the
/// Thinking/ToolUse broadening (and the `suppress_thinking_clear` band-aid it
/// required) are no longer needed. A working agent reaches Idle between
/// turns and clears then; mid-work Thinking/ToolUse no longer clears (and never
/// injects either, so no storm).
fn clears_server_rate_limit_retry(state: crate::state::AgentState) -> bool {
    use crate::state::AgentState;
    matches!(state, AgentState::Idle)
}

/// #1470 (re-scoped slice, credit @cheerc): notify an agent's team
/// orchestrator, via its INBOX, that the transient-error auto-retry has been
/// exhausted. This is the agent-inbox path (not operator Telegram), so it does
/// NOT go through `gated_notify` — the operator Telegram alert is a separate
/// call at the exhaustion site. Same team/orchestrator guards as
/// `maybe_notify_member_state_change` (skip when no team, no orchestrator, or
/// the agent is its own orchestrator).
fn notify_orchestrator_retry_exhausted(home: &std::path::Path, name: &str, retries: u32) {
    let Some(team) = crate::teams::find_team_for(home, name) else {
        return;
    };
    let Some(ref orch) = team.orchestrator else {
        return;
    };
    if orch == name {
        return; // skip self-notify
    }
    // Persist to the orchestrator's inbox (durable — read on its next inbox
    // drain, not dependent on it being live at a prompt). Mirrors
    // `maybe_notify_member_state_change`'s `enqueue` path. NOT `notify_agent`,
    // which injects into a live PTY and would be lost if the orchestrator is
    // idle/away.
    let payload = serde_json::json!({
        "type": "member_retry_exhausted",
        "member": name,
        "team": team.name,
        "retries": retries,
        "detected_at": chrono::Utc::now().to_rfc3339(),
        "action": "Manual intervention required — check the agent pane, restart, or reassign the task.",
    });
    let msg = crate::inbox::InboxMessage::new_system(
        "system:supervisor",
        "member_retry_exhausted",
        payload.to_string(),
    );
    persist_or_log!(
        crate::inbox::enqueue(home, orch, msg),
        "retry_exhausted_notify",
        orch
    );
    tracing::info!(
        agent = %name,
        orchestrator = %orch,
        retries,
        "retry-exhaustion orchestrator-inbox notify sent"
    );
}

/// #1742: outcome of a `continue`-inject attempt. Distinguishes the agent having
/// VANISHED (snap=None — resolve_uuid / registry miss → nobody to inject into,
/// reaped next tick, harmless) from a TRANSIENT inject failure (agent present but
/// the PTY write erred → worth retrying). Before #1742 both collapsed to `false`
/// and silently exhausted the retry track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectOutcome {
    Injected,
    AgentGone,
    InjectFailed,
}

/// #1696/#1697: snapshot the inject target under the registry lock (released
/// BEFORE the blocking PTY write — #1530/F1), then inject the fixed `continue`
/// payload **gated by the operator-draft check** (#1680: `force=false` →
/// `should_defer_direct_inject` defers while the operator is typing instead of
/// clobbering their half-typed draft; a deferral is enqueued and counts as
/// handled — so a deferral reports `Injected`). Shared by the ServerRateLimit
/// retry path and the ApiError quick-nudge. #1742: returns a 3-state
/// `InjectOutcome` (was `bool`) so the caller can keep retrying a transient PTY
/// failure instead of silently giving up.
fn inject_continue_gated(
    home: &std::path::Path,
    registry: &AgentRegistry,
    name: &str,
) -> InjectOutcome {
    let snap = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, name)
            .and_then(|id| reg.get(&id))
            .map(agent::InjectTarget::from_handle)
    };
    match snap {
        Some(tgt) => {
            if agent::inject_with_target_gated(&tgt, name, CONTINUE_RETRY_PAYLOAD, false).is_ok() {
                InjectOutcome::Injected
            } else {
                InjectOutcome::InjectFailed
            }
        }
        None => InjectOutcome::AgentGone,
    }
}

/// #1742 (pure, unit-tested): given the running count of consecutive inject
/// failures (after incrementing for the current failure), decide whether to give
/// up (`Exhaust`) or keep the track and re-attempt next tick (`RetrySoon`). A
/// transient PTY blip (fewer than `MAX_INJECT_FAILURES`) self-heals, so we
/// retry; only a sustained run of failures exhausts — and the caller routes that
/// through the FULL notification path (never silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectFailAction {
    Exhaust,
    RetrySoon,
}

fn classify_inject_failure(consecutive_failures: u32) -> InjectFailAction {
    if consecutive_failures >= MAX_INJECT_FAILURES {
        InjectFailAction::Exhaust
    } else {
        InjectFailAction::RetrySoon
    }
}

pub(crate) fn process_error_recovery(
    home: &std::path::Path,
    registry: &AgentRegistry,
    retry_tracks: &mut HashMap<String, RateLimitRetry>,
    apierror_episodes: &mut std::collections::HashSet<String>,
    apierror_nudge_counts: &mut HashMap<String, u32>,
    last_continue_inject: &mut HashMap<String, Instant>,
) {
    use crate::state::AgentState;
    let now = Instant::now();

    // Phase 1: classify states under the registry lock (NO PTY writes here).
    let mut active_names = std::collections::HashSet::new();
    let mut apierror_to_nudge: Vec<String> = Vec::new();
    // #1713 root-fix: names DECIDED (with fresh state, under the lock) to receive a
    // ServerRateLimit `continue` inject this tick. Phase 2 only executes these.
    let mut srl_to_inject: Vec<String> = Vec::new();
    {
        let reg = agent::lock_registry(registry);
        // #1441: registry is UUID-keyed; the tracking maps stay name-keyed, so
        // index them by the handle's display name.
        for handle in reg.values() {
            let name = handle.name.as_str();
            active_names.insert(name.to_string());
            let state = handle.core.lock().state.current;

            // ── #1713 root-fix: ServerRateLimit retry — DECIDE with fresh state ──
            // The "should we inject this tick" decision lives HERE, under the lock,
            // gated on the agent being FRESHLY observed in ServerRateLimit — not on a
            // stale persisted timer. The track still persists across ticks to carry
            // the tiered backoff (retry_count / next_retry_at / exhausted); Phase 2
            // only EXECUTES the lock-free PTY inject for the names decided here. So a
            // track can never blind-fire `continue` into a non-error state (e.g. a
            // PermissionPrompt the agent reached after the throttle cleared).
            if state == AgentState::ServerRateLimit {
                let track = retry_tracks.entry(name.to_string()).or_insert_with(|| {
                    let delay = Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
                    tracing::info!(agent = %name, delay_secs = delay.as_secs(), "ServerRateLimit detected, scheduling retry (Phase A)");
                    RateLimitRetry {
                        retry_count: 0,
                        next_retry_at: now + delay,
                        exhausted: false,
                        inject_failures: 0,
                    }
                });
                // Due + not exhausted + outside the anti-thrash MIN_INTERVAL (guards a
                // ServerRateLimit↔ApiError flicker). A skip here does NOT consume a
                // retry — next tick re-decides once the agent is still ServerRateLimit.
                let min_interval_ok = last_continue_inject
                    .get(name)
                    .is_none_or(|t| now.duration_since(*t) >= CONTINUE_INJECT_MIN_INTERVAL);
                if !track.exhausted && now >= track.next_retry_at && min_interval_ok {
                    srl_to_inject.push(name.to_string());
                }
            } else if clears_server_rate_limit_retry(state) {
                // #1713: clear on genuine recovery (Idle) — cross-episode reset
                // so a later ServerRateLimit episode starts fresh at Phase A. No
                // suppress window needed: the inject is state-gated above, so a
                // mid-work Thinking/ToolUse transient neither clears here nor injects.
                if retry_tracks.remove(name).is_some() {
                    tracing::info!(
                        agent = %name,
                        ?state,
                        "ServerRateLimit retry cleared — agent recovered (Idle)"
                    );
                }
            }

            // ── #1697: ApiError-at-prompt quick-nudge (per-episode anti-thrash) ──
            if state == AgentState::ApiError {
                // #1742-F4: cap the total nudges per flicker-window. A content-FP
                // `ApiError↔Thinking` flicker re-arms `apierror_episodes` every
                // cycle, so the per-episode dedup alone lets it nudge indefinitely
                // (only MIN_INTERVAL-rate-limited). Stop once the window count hits
                // APIERROR_NUDGE_MAX; the count resets on genuine recovery (below).
                let capped =
                    apierror_nudge_counts.get(name).copied().unwrap_or(0) >= APIERROR_NUDGE_MAX;
                if !apierror_episodes.contains(name) && !capped {
                    apierror_to_nudge.push(name.to_string());
                }
            } else {
                // Left the ApiError state → re-arm so the NEXT episode nudges again.
                apierror_episodes.remove(name);
                // #1742-F4: reset the flicker cap ONLY on genuine recovery (Idle) —
                // NOT on a mid-flicker Thinking/ToolUse, else the cap could never
                // accumulate across the ApiError↔Thinking oscillation it bounds.
                if clears_server_rate_limit_retry(state) {
                    apierror_nudge_counts.remove(name);
                }
            }
        }
    }

    // #1470: drop tracking state for agents no longer in the registry (killed /
    // restarted / deleted) so the maps don't grow unbounded across agent churn.
    retry_tracks.retain(|name, _| active_names.contains(name));
    apierror_episodes.retain(|name| active_names.contains(name));
    apierror_nudge_counts.retain(|name, _| active_names.contains(name));
    last_continue_inject.retain(|name, _| active_names.contains(name));

    // Phase 2: EXECUTE the ServerRateLimit injects decided in Phase 1 with fresh
    // state — inject "continue\n" lock-free, advance the tiered backoff, escalate on
    // exhaustion. Only names confirmed in ServerRateLimit this tick reach here, so
    // `continue` is never injected into a non-error (e.g. waiting-prompt) state.
    for name in &srl_to_inject {
        let Some(retry) = retry_tracks.get_mut(name) else {
            continue;
        };
        retry.retry_count += 1;
        if retry.retry_count > SERVER_RATE_LIMIT_MAX_RETRIES {
            tracing::warn!(agent = %name, retries = retry.retry_count, "ServerRateLimit max retries exceeded — giving up");
            retry.exhausted = true;
            // #1470: tell the team orchestrator (via its INBOX) that auto-retry
            // gave up, so a stuck member is reassigned/intervened. Inbox path only;
            // the operator Telegram alert below is the separate `gated_notify`.
            notify_orchestrator_retry_exhausted(home, name, retry.retry_count);
            if let Some(ch) = crate::channel::active_channel() {
                let msg = format!(
                    "⚠️ {name} transient upstream error auto-retry exhausted ({} retries). Manual intervention required.",
                    SERVER_RATE_LIMIT_MAX_RETRIES
                );
                // #1595 Step 1: `Error`, not `Warn`. Exhaustion of the #1696 tiered
                // retry (the full ~75min budget burned) means the agent is stuck
                // and auto-recovery has given up — a genuine P0 that MUST break
                // through `Sleep`/`Away` to wake the operator (the #1594 gate
                // suppresses `Warn` in Sleep, so at `Warn` this alert was silently
                // dropped exactly when the operator most needs it). Severity is
                // routing-only (the Telegram adapter ignores it for rendering).
                let _ = crate::channel::gated_notify(
                    ch.as_ref(),
                    name,
                    NotifySeverity::Error,
                    &msg,
                    false,
                );
            }
            continue;
        }
        // #1696: escalation-phase observability (so a long outage is visible in the log).
        if retry.retry_count == RETRY_PHASE_B_START {
            tracing::info!(agent = %name, "ServerRateLimit: entering retry Phase B (minute-scale backoff)");
        } else if retry.retry_count == RETRY_PHASE_C_START {
            tracing::info!(agent = %name, "ServerRateLimit: entering retry Phase C (sustained 10-min retry)");
        }

        match inject_continue_gated(home, registry, name) {
            InjectOutcome::Injected => {
                retry.inject_failures = 0; // #1742: a success clears the failure streak
                last_continue_inject.insert(name.clone(), Instant::now());
                tracing::info!(
                    agent = %name,
                    retry = retry.retry_count,
                    "ServerRateLimit: injected \"continue\" (attempt {})",
                    retry.retry_count
                );
                let idx = (retry.retry_count as usize).min(SERVER_RATE_LIMIT_BACKOFF.len() - 1);
                retry.next_retry_at =
                    Instant::now() + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[idx]);
            }
            InjectOutcome::AgentGone => {
                // #1742: the agent vanished between the Phase-1 decision and here —
                // nobody to inject into. The `retain` at the top of the NEXT tick
                // reaps the track (agent no longer in the registry), so do NOT
                // exhaust or notify (harmless). Roll back the attempt the top of
                // the loop pre-counted so `retry_count` stays "successful injects".
                retry.retry_count -= 1;
                tracing::debug!(agent = %name, "ServerRateLimit: agent gone before inject — track reaped next tick");
            }
            InjectOutcome::InjectFailed => {
                // #1742: the agent is STILL present but the PTY write erred. Do NOT
                // silently exhaust (the bug) — roll back the pre-counted attempt so a
                // failure never burns the tiered budget, bump the consecutive-failure
                // counter, and retry next tick after a short delay. Only after
                // MAX_INJECT_FAILURES back-to-back failures do we give up — and then
                // via the FULL notification path (orchestrator inbox + operator
                // Telegram), never silently.
                retry.retry_count -= 1;
                retry.inject_failures += 1;
                tracing::warn!(
                    agent = %name,
                    failures = retry.inject_failures,
                    "ServerRateLimit: inject failed (agent present, transient PTY error?) — will retry"
                );
                match classify_inject_failure(retry.inject_failures) {
                    InjectFailAction::RetrySoon => {
                        retry.next_retry_at = now + RETRY_AFTER_INJECT_FAIL;
                    }
                    InjectFailAction::Exhaust => {
                        retry.exhausted = true;
                        notify_orchestrator_retry_exhausted(home, name, retry.retry_count);
                        if let Some(ch) = crate::channel::active_channel() {
                            let msg = format!(
                                "⚠️ {name} ServerRateLimit auto-retry inject 連續失敗 {} 次、已放棄 — agent 可能 unreachable,需人工介入(檢查 pane / 重啟 / 重新指派)。",
                                retry.inject_failures
                            );
                            // #1742: same Error severity + same Sleep-penetrating gate as
                            // the budget-exhausted alert (#1595 Step 1) — a stuck agent
                            // whose auto-recovery cannot even deliver `continue` is the
                            // same P0 that must wake a sleeping operator.
                            let _ = crate::channel::gated_notify(
                                ch.as_ref(),
                                name,
                                NotifySeverity::Error,
                                &msg,
                                false,
                            );
                        }
                    }
                }
            }
        }
    }

    // Phase 2b: #1697 ApiError quick-nudge — inject "continue\n" once per episode,
    // immediately (no 300s-silence wait), respecting the shared MIN_INTERVAL.
    for name in apierror_to_nudge {
        if last_continue_inject
            .get(&name)
            .is_some_and(|t| now.duration_since(*t) < CONTINUE_INJECT_MIN_INTERVAL)
        {
            continue;
        }
        // #1742: the ApiError nudge is per-episode best-effort (not budgeted), so only
        // a genuine `Injected` marks the episode + stamps the interval; AgentGone /
        // InjectFailed leave it un-nudged to re-attempt next tick.
        if inject_continue_gated(home, registry, &name) == InjectOutcome::Injected {
            apierror_episodes.insert(name.clone());
            // #1742-F4: count this nudge toward the per-flicker-window cap.
            *apierror_nudge_counts.entry(name.clone()).or_insert(0) += 1;
            last_continue_inject.insert(name.clone(), Instant::now());
            tracing::info!(agent = %name, "#1697: ApiError-at-prompt quick-nudge — injected \"continue\"");
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
    let meta_path = crate::agent_ops::metadata_path_resolved(home, name);
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
    let meta_path = crate::agent_ops::metadata_path_resolved(home, name);
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

    fn fresh_retry() -> RateLimitRetry {
        RateLimitRetry {
            retry_count: 0,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
        }
    }

    /// Phase-1 clears retry track on agent recovery so the next
    /// ServerRateLimit starts fresh.
    #[test]
    fn recovery_clears_retry_track() {
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert("agent1".into(), fresh_retry());
        tracks.remove("agent1");
        assert!(!tracks.contains_key("agent1"));
        tracks.insert("agent1".into(), fresh_retry());
        assert_eq!(tracks["agent1"].retry_count, 0);
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
        assert!(!AgentState::Idle.is_notify_error_class());
        assert!(!AgentState::Idle.is_notify_error_class());
        assert!(!AgentState::ToolUse.is_notify_error_class());
        assert!(!AgentState::Starting.is_notify_error_class());
    }

    // ── #1530: feed-driven UsageLimit / member-state reaction de-gate ──

    fn tr(
        from: crate::state::AgentState,
        to: crate::state::AgentState,
    ) -> crate::state::TransitionRecord {
        crate::state::TransitionRecord {
            from,
            to,
            ts: "2026-05-31T00:00:00+00:00".to_string(),
        }
    }

    /// RED ①: a feed-driven `Idle → UsageLimit` (the read-loop records it, so
    /// the drain carries it even though `prev == new` at the supervisor tick)
    /// MUST still produce a reaction decision. Pre-#1530 the `prev != new` gate
    /// skipped it → the UsageLimit reaction was dead since #1176.
    #[test]
    fn reactions_from_transitions_fires_on_feed_driven_usagelimit() {
        use crate::state::AgentState;
        let decisions = reactions_from_transitions(&[tr(AgentState::Idle, AgentState::UsageLimit)]);
        assert_eq!(
            decisions,
            vec![ReactionDecision {
                from: AgentState::Idle,
                to: AgentState::UsageLimit
            }],
            "feed-driven →UsageLimit must yield a reaction decision (de-gated off prev!=new)"
        );
    }

    /// RED ②: an intra-tick flap (`Idle → UsageLimit → Idle`) has no NET state
    /// change → no reaction. Avoids double/noise notifications. (Logging still
    /// records every transition via #1527 — that path is independent.)
    #[test]
    fn reactions_from_transitions_converges_on_net_state_no_flap_double_fire() {
        use crate::state::AgentState;
        let decisions = reactions_from_transitions(&[
            tr(AgentState::Idle, AgentState::UsageLimit),
            tr(AgentState::UsageLimit, AgentState::Idle),
        ]);
        assert!(
            decisions.is_empty(),
            "flap in-and-out (net Idle→Idle) must not fire a reaction, got {decisions:?}"
        );
    }

    /// Net change to a non-error state, and the empty drain, both yield nothing.
    #[test]
    fn reactions_from_transitions_ignores_non_error_and_empty() {
        use crate::state::AgentState;
        assert!(
            reactions_from_transitions(&[]).is_empty(),
            "empty drain → no reaction"
        );
        assert!(
            reactions_from_transitions(&[tr(AgentState::Idle, AgentState::ToolUse)]).is_empty(),
            "net change to a non-error state → no reaction"
        );
    }

    /// Net change THROUGH a flap into a different error state reacts on the
    /// final state: `UsageLimit → Idle → Hang` ⇒ react on Hang, not UsageLimit.
    #[test]
    fn reactions_from_transitions_reacts_on_final_error_state() {
        use crate::state::AgentState;
        let decisions = reactions_from_transitions(&[
            tr(AgentState::UsageLimit, AgentState::Idle),
            tr(AgentState::Idle, AgentState::Hang),
        ]);
        assert_eq!(
            decisions,
            vec![ReactionDecision {
                from: AgentState::UsageLimit,
                to: AgentState::Hang
            }],
            "net from = first.from, net to = last.to (final state)"
        );
    }

    /// RED ③: a UsageLimit final state drives BOTH the operator/propagate path
    /// AND member-notify — the latter was silently eaten by the pre-#1530
    /// propagate `continue`. A non-UsageLimit error state drives member-notify
    /// only; a non-error state drives nothing.
    #[test]
    fn reaction_kinds_usagelimit_does_not_drop_member_notify() {
        use crate::state::AgentState;
        assert_eq!(
            reaction_kinds(AgentState::UsageLimit, true),
            vec![
                ReactionKind::NotifyOperator,
                ReactionKind::Propagate,
                ReactionKind::MemberNotify
            ],
            "UsageLimit + propagation: all three reactions fire (member-notify NOT eaten)"
        );
        assert_eq!(
            reaction_kinds(AgentState::UsageLimit, false),
            vec![ReactionKind::NotifyOperator, ReactionKind::MemberNotify],
            "UsageLimit without propagation: operator notice + member-notify"
        );
        assert_eq!(
            reaction_kinds(AgentState::Hang, true),
            vec![ReactionKind::MemberNotify],
            "non-UsageLimit error state: member-notify only"
        );
        assert!(
            reaction_kinds(AgentState::Idle, true).is_empty(),
            "non-error state: no reaction"
        );
    }

    // ── #1552: runtime AwaitingOperator escalation FP-gates ──

    /// ClaudeCode permission chrome footer — the self-identifying anchor #1546
    /// installed; `StatePatterns` detects it as `PermissionPrompt`.
    const PERM_CHROME: &str = "Do you want to proceed?\nEsc to cancel · Tab to amend";

    #[test]
    fn awaiting_gate_starting_is_ungated() {
        // Legacy startup-stall path: fires regardless of chrome/position for an
        // `Active` worker (the default).
        assert!(awaiting_escalation_allowed(
            crate::state::AgentState::Starting,
            Duration::from_secs(0),
            None,
            "no chrome here",
            0,
            0,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_starting_ondemand_suppressed() {
        // #1563: a stuck-`Starting` `OnDemand` coordinator (e.g. `general`) must
        // NOT forward its startup-stall pane to the operator.
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::Starting,
            Duration::from_secs(0),
            None,
            "no chrome here",
            0,
            0,
            crate::fleet::IdleExpectation::OnDemand,
        ));
    }

    #[test]
    fn awaiting_gate_runtime_permission_all_gates_pass() {
        assert!(awaiting_escalation_allowed(
            crate::state::AgentState::PermissionPrompt,
            AWAITING_STABILITY, // held long enough
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME, // chrome IS in the live tail
            0,           // operator never typed
            10_000,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_ondemand_real_permission_still_escalates() {
        // #1563 preserves #1552: the role gate covers ONLY the `Starting`
        // startup-stall arm. A genuine runtime permission prompt that satisfies
        // all three FP-gates STILL escalates for an `OnDemand` agent — otherwise
        // a coordinator stuck on a real permission dialog would never be surfaced.
        assert!(awaiting_escalation_allowed(
            crate::state::AgentState::PermissionPrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME,
            0,
            10_000,
            crate::fleet::IdleExpectation::OnDemand,
        ));
    }

    // ── #1563 part-B: InteractivePrompt role gate ──
    // NB: `StatePatterns::detect` has NO `InteractivePrompt` regex (that state
    // only comes from the weak `is_generic_startup_prompt` at the StateTracker
    // level), so the position gate (a) for an `InteractivePrompt`-STATE agent is
    // satisfiable only by a tail that detects as `PermissionPrompt`. `PERM_CHROME`
    // models exactly the real FP combo: an agent latched to `InteractivePrompt`
    // whose live tail also shows prompt chrome.

    #[test]
    fn awaiting_gate_ondemand_interactive_prompt_suppressed() {
        // #1563 part-B: `general` (OnDemand) latched to `InteractivePrompt` by a
        // `(y/n)` in its PR-review prose must NOT escalate, even with all three
        // #1564 gates satisfied — the InteractivePrompt source is prose-FP-prone.
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::InteractivePrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME,
            0,
            10_000,
            crate::fleet::IdleExpectation::OnDemand,
        ));
    }

    #[test]
    fn awaiting_gate_active_interactive_prompt_still_escalates() {
        // An `Active` worker's InteractivePrompt still escalates (gates pass) —
        // the role gate only suppresses OnDemand.
        assert!(awaiting_escalation_allowed(
            crate::state::AgentState::InteractivePrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME,
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_interactive_prompt_1564_gates_still_apply_when_active() {
        // The new role gate is ADDITIVE: an `Active` InteractivePrompt with the
        // chrome NOT in the live tail still fails the position gate (a).
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::InteractivePrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            "no chrome in the live tail",
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn idle_expectation_for_resolves_role_and_defaults() {
        let home = tmp_home("idle_exp_resolve");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            r#"
defaults:
  backend: claude
instances:
  worker:
    role: worker
  general:
    role: General assistant
    idle_expectation: on-demand
"#,
        )
        .expect("write fleet.yaml");
        // The shared resolver both branch-1 (startup-stall) and branch-2
        // (startup-prose forward) gate on. `on-demand` → OnDemand suppresses
        // BOTH forwards; omitted → Active leaves the worker unchanged; an
        // unknown agent fails open to Active (never silently suppress).
        assert_eq!(
            idle_expectation_for(&home, "general"),
            crate::fleet::IdleExpectation::OnDemand
        );
        assert_eq!(
            idle_expectation_for(&home, "worker"),
            crate::fleet::IdleExpectation::Active
        );
        assert_eq!(
            idle_expectation_for(&home, "nonexistent"),
            crate::fleet::IdleExpectation::Active
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn awaiting_gate_blocks_scrollback_footer_fp() {
        // The meta-FP: state is PermissionPrompt (full-screen detection saw the
        // chrome), held + no typing — but the chrome is NOT in the live bottom
        // tail (it scrolled up / is a working agent's echo). Position gate (a)
        // must block escalation. This is the dev-2 live case.
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::PermissionPrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            "just normal working output, no dialog chrome at the bottom",
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_blocks_when_not_stable() {
        // (b) stability: prompt state held < AWAITING_STABILITY → no escalate.
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::PermissionPrompt,
            Duration::from_secs(1),
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME,
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_blocks_when_operator_typing() {
        // (c) engagement: operator typed 2s ago (< 15s window) → suppress.
        let now = 100_000i64;
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::PermissionPrompt,
            AWAITING_STABILITY,
            Some(crate::backend::Backend::ClaudeCode),
            PERM_CHROME,
            now - 2_000,
            now,
            crate::fleet::IdleExpectation::Active,
        ));
    }

    #[test]
    fn awaiting_gate_non_prompt_state_never_escalates() {
        for s in [
            crate::state::AgentState::Idle,
            crate::state::AgentState::Thinking,
            crate::state::AgentState::ToolUse,
        ] {
            assert!(
                !awaiting_escalation_allowed(
                    s,
                    AWAITING_STABILITY,
                    Some(crate::backend::Backend::ClaudeCode),
                    PERM_CHROME,
                    0,
                    10_000,
                    crate::fleet::IdleExpectation::Active,
                ),
                "{s:?} must never escalate via this path"
            );
        }
    }

    /// NOTIFY_COOLDOWN constant is 60 seconds.
    #[test]
    fn notify_cooldown_is_60_seconds() {
        assert_eq!(super::NOTIFY_COOLDOWN, std::time::Duration::from_secs(60));
    }

    /// #1530/F2 (lockaudit): the per-agent tick must NOT re-acquire the registry
    /// while holding an agent core (the core→registry inversion that risked an
    /// AB-BA deadlock with the registry→core render/monitor loops). The backend
    /// is pre-captured in the handles snapshot (registry→core order) and resolved
    /// lock-free; the old nested per-agent registry lookups under the core lock
    /// are gone. Source-grep pin (mirrors #1146); scoped to the `tick` fn body so
    /// it never matches its own assertion text.
    #[test]
    fn tick_does_not_reacquire_registry_under_core_f2() {
        let src = include_str!("supervisor.rs");
        let start = src
            .find("\nfn tick(")
            .expect("supervisor tick fn must exist");
        // The per-agent loop lives well within the first 18 KB of the fn; the
        // test module is far past that, so this window excludes this test.
        let body = &src[start..(start + 18_000).min(src.len())];
        // The removed nested lookup keyed the registry by the per-agent id.
        let needle = ["reg.get(&", "instance_id)"].concat();
        assert!(
            !body.contains(&needle),
            "#1530/F2: the tick per-agent loop must not re-look-up the registry by \
             agent id while holding the core — the backend is pre-captured in the \
             handles snapshot (registry→core)"
        );
        assert!(
            body.contains("backend_command"),
            "#1530/F2: tick must pre-capture each agent's backend_command in the \
             handles snapshot and resolve Backend lock-free"
        );
    }

    /// #1644: CI-time pin of the collect→drop→emit boundary in `tick`. The
    /// self-IPC / blocking emitters (member-notify `api::call(INJECT)`, the
    /// usage-limit propagate, the Telegram `gated_notify`) must run AFTER the
    /// per-agent `let action = { … core.lock() … }` block drops the core lock —
    /// never inside it (a core-held self-IPC is the #1492/#1535 deadlock class).
    /// The runtime guard (`CORE_LOCK_DEPTH` + `assert_no_registry_lock_for_self_ipc`,
    /// #1535) already fail-fasts a violation; this source-grep catches it earlier,
    /// at CI. It is the cheap structural slice of the deferred
    /// `supervise_one()->TickOutcome` extraction (#1644). Brace-matches the
    /// lock block and scopes to the `tick` fn body so it never matches itself.
    #[test]
    fn tick_emitters_run_after_core_lock_drops_1644() {
        let src = include_str!("supervisor.rs");
        let tick_start = src.find("\nfn tick(").expect("tick fn must exist");
        let after = &src[tick_start..];
        // End the slice at the next top-level `fn ` so the test module (and its
        // needle literals) are excluded.
        let tick_end = after[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .unwrap_or(after.len());
        let tick = &after[..tick_end];

        // Brace-match the per-agent core-lock block `let action … = { … };`.
        let anchor = ["let action", ": Option<NoticeAction> = {"].concat();
        let astart = tick.find(&anchor).expect("tick core-lock block present");
        let open = astart + tick[astart..].find('{').expect("block opens");
        let mut depth = 0usize;
        let mut close = open;
        for (i, c) in tick[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(close > open, "core-lock block must close");
        let in_block = &tick[open..=close];
        let after_block = &tick[close..];

        for emitter in [
            ["maybe_notify", "_member_state_change("].concat(),
            ["gated", "_notify("].concat(),
            ["propagate", "_usage_limit("].concat(),
        ] {
            assert!(
                !in_block.contains(&emitter),
                "#1644: `{emitter}` is a self-IPC/blocking emitter and must NOT run inside the \
                 core-lock block (collect→drop→emit; #1492/#1535 deadlock class)"
            );
            assert!(
                after_block.contains(&emitter),
                "#1644: `{emitter}` must run AFTER the core lock drops"
            );
        }
    }

    // ── #1523: AuthError content-FP stability gate ──────────────────────

    /// The stability window must exceed the observed self-heal time (~31s) by a
    /// safe margin so a transient AuthError can never reach the alert.
    #[test]
    fn auth_error_notify_stability_exceeds_observed_self_heal() {
        assert!(
            super::AUTH_ERROR_NOTIFY_STABILITY >= std::time::Duration::from_secs(60),
            "stability window must be well above the observed 31s self-heal"
        );
    }

    /// Transient (self-healed): on a later tick the state is no longer AuthError
    /// → `None` → Cancel → NO alert. This is the FP that #1523 fixes.
    #[test]
    fn auth_error_gate_cancels_when_state_left() {
        assert_eq!(super::auth_error_gate(None), super::AuthErrorGate::Cancel);
    }

    /// Still in AuthError but inside the window (e.g. the 31s blip before it
    /// heals) → Wait → no alert yet.
    #[test]
    fn auth_error_gate_waits_within_window() {
        let held = super::AUTH_ERROR_NOTIFY_STABILITY - std::time::Duration::from_secs(1);
        assert_eq!(
            super::auth_error_gate(Some(held)),
            super::AuthErrorGate::Wait
        );
        // The observed self-heal point (31s) is firmly in the Wait band.
        assert_eq!(
            super::auth_error_gate(Some(std::time::Duration::from_secs(31))),
            super::AuthErrorGate::Wait
        );
    }

    /// Sustained (real auth failure): held ≥ window → Fire → alert sent.
    #[test]
    fn auth_error_gate_fires_when_held_past_window() {
        assert_eq!(
            super::auth_error_gate(Some(super::AUTH_ERROR_NOTIFY_STABILITY)),
            super::AuthErrorGate::Fire
        );
        let well_past = super::AUTH_ERROR_NOTIFY_STABILITY + std::time::Duration::from_secs(120);
        assert_eq!(
            super::auth_error_gate(Some(well_past)),
            super::AuthErrorGate::Fire
        );
    }

    /// D4: 2×2 invariant fixture — production-path-coupled.
    /// 2 teams (team-a: orch-a + worker-a, team-b: orch-b + worker-b).
    /// worker-a transitions Idle → UsageLimit.
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
            crate::state::AgentState::Idle,
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
            crate::state::AgentState::Idle,
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

    /// #1595 Step 2 (pure): only AuthError escalates a self-orchestrator.
    #[test]
    fn self_orchestrator_escalates_only_on_autherror_1595() {
        use crate::state::AgentState;
        assert!(super::self_orchestrator_escalates(AgentState::AuthError));
        for s in [
            AgentState::UsageLimit,
            AgentState::RateLimit,
            AgentState::Hang,
            AgentState::Crashed,
            AgentState::PermissionPrompt,
            AgentState::Idle,
            AgentState::Idle,
        ] {
            assert!(
                !super::self_orchestrator_escalates(s),
                "{s:?} must NOT escalate (only AuthError is terminal + operator-only)"
            );
        }
    }

    /// #1595 Step 2: a self-orchestrator (orch==name) hitting AuthError escalates
    /// (Telegram path + cooldown-track stamp) but still skips the inbox self-notify;
    /// a non-terminal state stays a plain drop (no stamp). Telegram is a no-op here
    /// (no active channel in tests) — the cooldown-track stamp is the observable
    /// signal that the escalation branch ran. Cooldown prevents re-escalation.
    #[test]
    fn self_orchestrator_autherror_escalates_others_drop_1595() {
        let home = std::env::temp_dir().join(format!("agend-1595-selforch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("inbox")).ok();
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "t", "members": ["solo"], "orchestrator": "solo"}),
        );

        // Non-terminal (RateLimit) self-orchestrator → plain drop, no escalation.
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "solo",
            crate::state::AgentState::Idle,
            crate::state::AgentState::RateLimit,
            "",
            &mut tracks,
        );
        assert!(!sent, "self-notify skipped");
        assert!(
            !tracks.contains_key("solo"),
            "#1595: a non-AuthError self-orchestrator must NOT escalate (no track stamp)"
        );

        // AuthError self-orchestrator → escalation branch runs (stamps cooldown
        // track), still returns false (the escalation is Telegram, not inbox).
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "solo",
            crate::state::AgentState::Idle,
            crate::state::AgentState::AuthError,
            "",
            &mut tracks,
        );
        assert!(!sent, "inbox self-notify still skipped");
        let t = tracks
            .get("solo")
            .expect("#1595: AuthError self-orchestrator must escalate → cooldown track stamped");
        assert_eq!(t.consecutive, 1, "escalation counted once");
        let inbox =
            std::fs::read_to_string(home.join("inbox").join("solo.jsonl")).unwrap_or_default();
        assert_eq!(
            inbox.lines().filter(|l| !l.is_empty()).count(),
            0,
            "escalation is Telegram, not an inbox self-notify"
        );

        // Cooldown: a second immediate AuthError must NOT re-escalate.
        let sent2 = super::maybe_notify_member_state_change(
            &home,
            "solo",
            crate::state::AgentState::Idle,
            crate::state::AgentState::AuthError,
            "",
            &mut tracks,
        );
        assert!(!sent2);
        assert_eq!(
            tracks["solo"].consecutive, 1,
            "#1595: NOTIFY_COOLDOWN must prevent re-escalation within the window"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #event-bus pattern #9: gate-ON emit→subscriber re-delivers the inbox half
    /// (A) BYTE-IDENTICALLY to the legacy `deliver_member_state_change`. The
    /// frozen `detected_at` is passed identically to both paths, so the structured
    /// payloads match exactly. The notify_agent half (B) is a PTY-inject covered by
    /// the shared-deliver-fn invariant (same fn invoked by both paths), so it is
    /// not separately drain-asserted (PTY-readback would be platform-gated + fragile).
    #[test]
    fn member_state_change_gate_on_emit_subscriber_matches_legacy() {
        let detected_at = "2026-06-03T09:00:00+00:00";
        let mk = |tag: &str| {
            let h =
                std::env::temp_dir().join(format!("agend-msc-parity-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&h);
            std::fs::create_dir_all(h.join("inbox")).ok();
            h
        };
        let payloads =
            |home: &std::path::Path| -> Vec<(String, Option<String>, String, Option<String>)> {
                crate::inbox::drain(home, "orch-a")
                    .into_iter()
                    .map(|m| (m.from, m.kind, m.text, m.correlation_id))
                    .collect()
            };

        let home_legacy = mk("legacy");
        super::deliver_member_state_change(
            &home_legacy,
            "orch-a",
            "worker-a",
            "team-a",
            crate::state::AgentState::Idle.display_name(),
            crate::state::AgentState::UsageLimit.display_name(),
            crate::state::AgentState::UsageLimit,
            "Usage limit reached. Resets at 15:14 UTC",
            Some("15:14"),
            1,
            detected_at,
        );

        let home_bus = mk("bus");
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(super::handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::MemberStateChanged {
                agent: "worker-a".into(),
                team: "team-a".into(),
                from_state: crate::state::AgentState::Idle.display_name().to_string(),
                to_state: crate::state::AgentState::UsageLimit
                    .display_name()
                    .to_string(),
                orch: "orch-a".into(),
                new_state: crate::state::AgentState::UsageLimit,
                pane_tail: "Usage limit reached. Resets at 15:14 UTC".into(),
                unlock_at: Some("15:14".into()),
                consecutive_count: 1,
                detected_at: detected_at.into(),
            },
        );

        let legacy = payloads(&home_legacy);
        let via_bus = payloads(&home_bus);
        assert!(!legacy.is_empty(), "legacy enqueue must land");
        assert_eq!(
            legacy, via_bus,
            "bus inbox-half (A) must match legacy byte-for-byte"
        );

        std::fs::remove_dir_all(&home_legacy).ok();
        std::fs::remove_dir_all(&home_bus).ok();
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
            crate::state::AgentState::Idle,
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

    /// #1696: tiered schedule — Phase A burst (5/15/30s), Phase B backoff
    /// (1m/2m/5m), Phase C sustained (10m × 6). 12 retries, ~75min budget.
    #[test]
    fn backoff_tiered_phase_a_b_c_schedule_1696() {
        assert_eq!(
            super::SERVER_RATE_LIMIT_BACKOFF,
            [5, 15, 30, 60, 120, 300, 600, 600, 600, 600, 600, 600]
        );
        assert_eq!(super::SERVER_RATE_LIMIT_MAX_RETRIES, 12);
        // Phase boundaries (for the escalation INFO logs) must index into the array.
        assert_eq!(
            super::SERVER_RATE_LIMIT_BACKOFF[super::RETRY_PHASE_B_START as usize],
            60
        );
        assert_eq!(
            super::SERVER_RATE_LIMIT_BACKOFF[super::RETRY_PHASE_C_START as usize],
            600
        );
    }

    #[test]
    fn retries_stop_at_tiered_max_1696() {
        // #1696: the budget is now MAX_RETRIES (12, tiered A/B/C), not 3.
        let mut retry = RateLimitRetry {
            retry_count: super::SERVER_RATE_LIMIT_MAX_RETRIES,
            next_retry_at: std::time::Instant::now(),
            exhausted: false,
            inject_failures: 0,
        };
        retry.retry_count += 1;
        assert!(
            retry.retry_count > super::SERVER_RATE_LIMIT_MAX_RETRIES,
            "the (count+1 > max) guard exhausts only after the full tiered budget"
        );
    }

    /// #1325: validate the retry payload constant value and that it ends
    /// with a newline (required for CLI agent prompt submission).
    #[test]
    fn continue_retry_payload_is_valid() {
        assert_eq!(
            super::CONTINUE_RETRY_PAYLOAD,
            b"continue\n",
            "payload must be the fixed resume signal"
        );
        assert!(
            super::CONTINUE_RETRY_PAYLOAD.ends_with(b"\n"),
            "payload must end with newline for prompt submission"
        );
    }

    /// #1325: validate "continue" works as input for all backends that can
    /// enter ServerRateLimit (backends with API-backed models). Shell/Raw
    /// backends never enter ServerRateLimit so they're excluded.
    #[test]
    fn continue_payload_compatible_with_all_api_backends() {
        use crate::backend::Backend;
        let api_backends = [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
            Backend::Agy,
        ];
        for backend in &api_backends {
            let preset = backend.preset();
            assert_eq!(
                preset.submit_key, "\r",
                "{:?} must use \\r submit_key for continue inject to work",
                backend
            );
        }
    }

    /// Helper: create a minimal AgentHandle with a real PTY for behavioral
    /// tests. Spawns a stdin-echoing process (Unix: `cat`, Windows: `findstr .*`).
    fn mock_agent_handle(
        name: &str,
        state: crate::state::AgentState,
    ) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: 10,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        #[cfg(not(target_os = "windows"))]
        let mut cmd = portable_pty::CommandBuilder::new("cat");
        #[cfg(target_os = "windows")]
        let mut cmd = {
            let mut c = portable_pty::CommandBuilder::new("cmd");
            c.args(["/c", "findstr", ".*"]);
            c
        };
        cmd.cwd(std::env::temp_dir());
        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("spawn stdin-echo process");
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().expect("clone reader");
        let writer = pair.master.take_writer().expect("take writer");
        let pty_writer: crate::agent::PtyWriter = Arc::new(parking_lot::Mutex::new(writer));
        let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
            vterm: crate::vterm::VTerm::with_pty_writer(80, 10, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: crate::state::StateTracker::new(None),
            health: crate::health::HealthTracker::new(),
        }));
        core.lock().state.current = state;
        let handle = crate::agent::AgentHandle {
            id: crate::types::InstanceId::default(),
            name: name.to_string().into(),
            backend_command: "claude".to_string(),
            pty_writer,
            pty_master: Arc::new(parking_lot::Mutex::new(pair.master)),
            core,
            child: Arc::new(parking_lot::Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (handle, reader)
    }

    /// #1325: phase 1 — ServerRateLimit detection populates retry_tracks.
    #[test]
    fn phase1_detects_rate_limit_and_schedules_retry() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("phase1-detect");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        // #1441: registry is UUID-keyed — insert under the handle's own id.
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        assert!(
            tracks.contains_key("test-agent"),
            "phase 1 must detect ServerRateLimit and insert retry track"
        );
        assert_eq!(tracks["test-agent"].retry_count, 0);
        assert!(!tracks["test-agent"].exhausted);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1325: phase 1 — recovery (Idle) clears retry track.
    #[test]
    fn phase1_recovery_clears_retry_track() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("phase1-recovery");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
            },
        );

        let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
        // #1441: registry is UUID-keyed — insert under the handle's own id.
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        assert!(
            !tracks.contains_key("test-agent"),
            "phase 1 must clear retry track on Idle recovery"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1713: the recovery-clear predicate is now {Idle} (genuine terminal
    /// recovery → cross-episode reset). #1586's Thinking/ToolUse broadening is
    /// removed: with the #1713 state-gate at the decision point the blind-fire
    /// storm is structurally impossible, so the broadening that compensated for it
    /// is unnecessary. Thinking/ToolUse no longer clear (a working agent reaches
    /// Ready/Idle between turns and clears then; it never injects mid-work anyway).
    #[test]
    fn clears_server_rate_limit_retry_covers_only_terminal_recovery_1713() {
        use crate::state::AgentState::*;
        // Genuine terminal recovery → clear (cross-episode reset). (`Idle` is the
        // sole terminal-recovery state since the Ready/Idle merge.)
        assert!(
            super::clears_server_rate_limit_retry(Idle),
            "#1713: terminal-recovery state Idle must clear the retry track"
        );
        // Everything else — incl mid-work Thinking/ToolUse and every waiting/error
        // state — must NOT clear.
        for s in [
            Thinking,
            ToolUse,
            ServerRateLimit,
            RateLimit,
            ApiError,
            AuthError,
            UsageLimit,
            ContextFull,
            Hang,
            PermissionPrompt,
            Starting,
        ] {
            assert!(
                !super::clears_server_rate_limit_retry(s),
                "#1713: state {s:?} must NOT clear the retry track"
            );
        }
    }

    /// #1713 reachability regression (credit @cheerc + angle-A trace): a track
    /// scheduled while the agent was ServerRateLimit must NOT inject `continue`
    /// once the agent has moved into a legit WAITING state (here PermissionPrompt
    /// — e.g. the resumed agent ran a tool needing approval). PermissionPrompt is
    /// not a clearing state, so the track PERSISTS (a genuine throttle could still
    /// be present and resume), but the #1713 decision-point gate only fires an
    /// inject on a FRESH ServerRateLimit observation — so no `continue` is injected
    /// into the prompt. Pre-#1713 the state-blind Phase-2 loop injected every backoff.
    #[test]
    fn permission_prompt_keeps_track_but_does_not_inject_1713() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("1713-permission-no-inject");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        // A live, DUE track (as if scheduled while the agent was ServerRateLimit).
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: Instant::now(), // due now
                exhausted: false,
                inject_failures: 0,
            },
        );
        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::PermissionPrompt);
        registry.lock().insert(handle.id, handle);

        let mut last_inject: HashMap<String, Instant> = HashMap::new();
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
        );

        // Track persists (PermissionPrompt is not a clearing state)…
        assert!(
            tracks.contains_key("test-agent"),
            "#1713: PermissionPrompt must NOT clear the track (a real throttle could resume)"
        );
        // …but NO inject fired and the retry budget was NOT consumed (the bug was
        // injecting `continue` into the waiting prompt every backoff).
        assert!(
            last_inject.is_empty(),
            "#1713: no `continue` inject into a non-ServerRateLimit (waiting) state"
        );
        assert_eq!(
            tracks["test-agent"].retry_count, 1,
            "#1713: a non-ServerRateLimit tick must not advance the retry count"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1586 FP-direction (the OTHER half): a genuine throttle leaves the agent
    /// STUCK in ServerRateLimit — the retry track must PERSIST (so the
    /// `continue` nudge still fires for real throttles).
    #[test]
    fn phase1_stuck_throttle_keeps_retry_track_1586() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("phase1-real-stuck");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        // Far-future retry so phase 2 doesn't fire / inject during this test.
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: Instant::now() + Duration::from_secs(3600),
                exhausted: false,
                inject_failures: 0,
            },
        );
        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        assert!(
            tracks.contains_key("test-agent"),
            "#1586: a still-throttled (stuck) agent must KEEP its retry track"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1325: phase 2 — due retry injects "continue\n" to PTY. Captures
    /// actual PTY output via the reader end to verify the injected payload.
    /// Windows PTY injects ANSI escapes (`\x1b[6n`) that contaminate the
    /// read — skip on Windows where `findstr` cannot echo stdin faithfully.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn phase2_injects_continue_to_pty() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("phase2-inject");

        let (handle, mut reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        // #1441: phase 2 inject resolves the name-keyed track via fleet.yaml;
        // seed the entry with the handle's own id so resolution hits this
        // registry entry (registry key == handle.id == resolve_uuid(name)).
        let agent_id = handle.id;
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  test-agent:\n    id: {}\n", agent_id.full()),
        )
        .ok();
        registry.lock().insert(agent_id, handle);

        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 0,
                next_retry_at: Instant::now() - Duration::from_secs(1),
                exhausted: false,
                inject_failures: 0,
            },
        );

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        assert_eq!(
            tracks["test-agent"].retry_count, 1,
            "retry_count must increment after inject"
        );

        let mut buf = vec![0u8; 256];
        use std::io::Read;
        let n = reader.read(&mut buf).expect("read from PTY");
        let captured = String::from_utf8_lossy(&buf[..n]);
        assert!(
            captured.contains("continue"),
            "PTY must receive \"continue\" payload, got: {:?}",
            captured.trim_end_matches('\0')
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn retry_loop_does_not_restart_after_max_exceeded() {
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "agent-loop".into(),
            RateLimitRetry {
                retry_count: 4,
                next_retry_at: std::time::Instant::now(),
                exhausted: true,
                inject_failures: 0,
            },
        );
        assert!(tracks.contains_key("agent-loop"));
        assert!(tracks["agent-loop"].exhausted);
    }

    /// #1470 (slice): a retry track for an agent no longer in the registry
    /// (killed / restarted / deleted) is dropped — the map can't grow unbounded
    /// across agent churn.
    #[test]
    fn retry_track_cleared_when_agent_removed_from_registry() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("slice-clear-removed-agent");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "ghost-agent".to_string(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: Instant::now() + Duration::from_secs(60),
                exhausted: false,
                inject_failures: 0,
            },
        );

        // Empty registry → the agent is gone → its track must be reaped.
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        assert!(
            !tracks.contains_key("ghost-agent"),
            "retry track must be cleared when the agent is no longer in the registry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1470 (slice): when auto-retry is exhausted, the agent's team
    /// orchestrator is notified via its INBOX (not operator Telegram).
    #[test]
    fn retry_exhaustion_notifies_orchestrator_inbox() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("slice-exhaustion-notify");
        std::fs::create_dir_all(home.join("inbox")).ok();

        // Agent stays in ServerRateLimit so Phase 1 keeps its seeded track
        // (a productive state would clear it via clears_server_rate_limit_retry).
        let (handle, _reader) =
            mock_agent_handle("worker-x", crate::state::AgentState::ServerRateLimit);
        let agent_id = handle.id;
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "teams:\n  team-x:\n    members: [orch-x, worker-x]\n    orchestrator: orch-x\n\
                 instances:\n  worker-x:\n    id: {}\n",
                agent_id.full()
            ),
        )
        .ok();
        registry.lock().insert(agent_id, handle);

        // Seed at MAX so the next increment exceeds it → exhaustion branch.
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "worker-x".to_string(),
            RateLimitRetry {
                retry_count: super::SERVER_RATE_LIMIT_MAX_RETRIES,
                next_retry_at: Instant::now() - Duration::from_secs(1),
                exhausted: false,
                inject_failures: 0,
            },
        );

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );

        assert!(
            tracks["worker-x"].exhausted,
            "track must be marked exhausted after exceeding max retries"
        );
        let orch_inbox = home.join("inbox").join("orch-x.jsonl");
        let content = std::fs::read_to_string(&orch_inbox).unwrap_or_default();
        assert!(
            content.contains("member_retry_exhausted") && content.contains("worker-x"),
            "orchestrator inbox must carry the retry-exhaustion notice, got: {content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1742: ServerRateLimit inject-failure handling (no silent exhaust) ──

    /// #1742 (pure): a transient inject failure self-heals, so fewer than
    /// `MAX_INJECT_FAILURES` consecutive failures keep retrying; only the Nth
    /// back-to-back failure exhausts (and the caller routes THAT through the full
    /// notification path). This is the unit gate for the InjectFailed branch,
    /// which is otherwise hard to drive (a PTY write failing while the agent is
    /// still present can't be cheaply mocked) — see PR notes.
    #[test]
    fn classify_inject_failure_exhausts_only_after_max_1742() {
        use super::{classify_inject_failure, InjectFailAction, MAX_INJECT_FAILURES};
        assert_eq!(
            MAX_INJECT_FAILURES, 3,
            "design-pinned: give up after 3 fails"
        );
        for n in 0..MAX_INJECT_FAILURES {
            assert_eq!(
                classify_inject_failure(n),
                InjectFailAction::RetrySoon,
                "#1742: {n} consecutive failures (< {MAX_INJECT_FAILURES}) must keep retrying, not exhaust"
            );
        }
        for n in MAX_INJECT_FAILURES..(MAX_INJECT_FAILURES + 3) {
            assert_eq!(
                classify_inject_failure(n),
                InjectFailAction::Exhaust,
                "#1742: {n} consecutive failures (>= {MAX_INJECT_FAILURES}) must exhaust"
            );
        }
    }

    /// #1742 regression (the silent-drop bug): a due ServerRateLimit track whose
    /// inject hits `AgentGone` (the agent vanished between the Phase-1 decision and
    /// the Phase-2 PTY write — here modelled by an unresolvable name: in the
    /// registry but absent from fleet.yaml, so `resolve_uuid` returns None) must
    /// NOT be marked exhausted and must NOT consume a retry. The track is left for
    /// the next-tick `retain` to reap. Pre-#1742 this set `exhausted=true` with a
    /// bare warn — permanently disabling auto-recovery with no notification.
    #[test]
    fn srl_inject_agent_gone_does_not_exhaust_1742() {
        let home = tmp_home("1742-agent-gone");
        // Registry has the agent in ServerRateLimit, but NO fleet.yaml mapping →
        // Phase 1 schedules it (it iterates the registry directly), Phase 2's
        // resolve_uuid misses → InjectOutcome::AgentGone.
        let (handle, _reader) =
            mock_agent_handle("gone-x", crate::state::AgentState::ServerRateLimit);
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        registry.lock().insert(handle.id, handle);

        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "gone-x".to_string(),
            RateLimitRetry {
                retry_count: 2,
                next_retry_at: Instant::now() - Duration::from_secs(1), // due
                exhausted: false,
                inject_failures: 0,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
        );

        let t = tracks
            .get("gone-x")
            .expect("#1742: track must survive an AgentGone tick (reaped only once the agent leaves the registry)");
        assert!(
            !t.exhausted,
            "#1742: AgentGone must NOT silently exhaust the retry track"
        );
        assert_eq!(
            t.retry_count, 2,
            "#1742: a no-op AgentGone tick must roll back the pre-counted attempt (retry_count == successful injects)"
        );
        assert!(
            t.inject_failures == 0,
            "#1742: AgentGone is not a present-agent inject failure → no failure-streak bump"
        );
        assert!(
            last_inject.is_empty(),
            "#1742: nothing was injected (no PTY write happened)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1742: a SUCCESSFUL inject clears any accumulated failure streak and
    /// advances the tiered budget — so a recovered PTY blip doesn't leave the
    /// track one failure away from giving up. Unix-only (mirrors the other PTY
    /// inject tests: the Windows mock PTY doesn't accept the write the same way).
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn srl_successful_inject_resets_failure_streak_1742() {
        let (home, registry, _reader) = one_agent_registry(
            "ok-x",
            crate::state::AgentState::ServerRateLimit,
            "1742-reset-streak",
        );
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "ok-x".to_string(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: Instant::now() - Duration::from_secs(1), // due
                exhausted: false,
                inject_failures: 2, // one short of MAX_INJECT_FAILURES
            },
        );
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
        );
        let t = tracks
            .get("ok-x")
            .expect("track persists after a successful inject");
        assert!(!t.exhausted, "#1742: a successful inject must not exhaust");
        assert_eq!(
            t.inject_failures, 0,
            "#1742: a successful inject must reset the consecutive-failure streak"
        );
        assert_eq!(
            t.retry_count, 2,
            "#1742: a real inject advances the tiered budget (1 → 2)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn retry_resumes_after_recovery_then_new_failure() {
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "agent-recover".into(),
            RateLimitRetry {
                retry_count: 4,
                next_retry_at: std::time::Instant::now(),
                exhausted: true,
                inject_failures: 0,
            },
        );
        tracks.remove("agent-recover");
        assert!(!tracks.contains_key("agent-recover"));
        tracks.insert(
            "agent-recover".into(),
            RateLimitRetry {
                retry_count: 0,
                next_retry_at: std::time::Instant::now(),
                exhausted: false,
                inject_failures: 0,
            },
        );
        assert_eq!(tracks["agent-recover"].retry_count, 0);
        assert!(!tracks["agent-recover"].exhausted);
    }

    #[test]
    fn retry_does_not_count_state_persistence_as_new_failure() {
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "agent-persist".into(),
            RateLimitRetry {
                retry_count: 1,
                next_retry_at: std::time::Instant::now(),
                exhausted: false,
                inject_failures: 0,
            },
        );
        for _ in 0..30 {
            assert!(tracks.contains_key("agent-persist"));
        }
        assert_eq!(tracks.len(), 1);
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

    /// A `loop_started_at` far enough in the past that `in_boot_grace` is
    /// false, so the #1741 boot-grace gate added to
    /// `check_pane_input_not_submitted_for_agents` lets the detection run.
    /// Mirrors the `past` helper in per_tick/{poll_reminder,inbox_stuck,
    /// handoff_timeout}.rs boot-grace tests.
    fn past_boot_grace() -> Instant {
        Instant::now() - crate::daemon::per_tick::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)
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
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
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
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
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
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
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
    fn pane_input_not_submitted_now_fires_for_non_claude_backend() {
        // #1457: submit detection widened from claude-only to ALL backends with
        // a submit key. kiro-cli (submit_key=`\r`) is now supported, so a
        // typed-but-not-submitted kiro pane MUST emit the diagnostic.
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
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
        let events = sink.events.lock();
        let fired = events.iter().any(|e| {
            matches!(
                e,
                crate::channel::ux_event::UxEvent::Fleet(
                    crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
                ) if emitted == agent
            )
        });
        assert!(
            fired,
            "non-claude backend with a submit key must now emit PaneInputNotSubmitted (#1457)"
        );
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
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
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

    #[test]
    fn pane_input_not_submitted_suppressed_during_boot_grace() {
        // #1741: a daemon restart zeroes `pane_input_tracks`, so without the
        // boot-grace gate the diagnostic re-fires on the first ticks for an
        // input typed BEFORE the restart (a pre-existing operator draft the
        // detector cannot tell apart from a fresh strand). A `loop_started_at`
        // still within NOTIFICATION_BOOT_GRACE must suppress the emit AND leave
        // the dedup map untouched; once the grace elapses the same
        // still-stranded input emits exactly once.
        let agent = "claude-agent-pin-bootgrace";
        let home = fleet_with_backend("pin_bootgrace", agent, "claude");
        let now_ms = chrono::Utc::now().timestamp_millis();
        seed_input_submit(&home, agent, now_ms - 300_000, 0);
        std::env::set_var("AGEND_PANE_INPUT_THRESHOLD_SECS", "60");
        let sink = std::sync::Arc::new(TestSink {
            events: parking_lot::Mutex::new(Vec::new()),
        });
        crate::channel::sink_registry::registry().register(sink.clone());
        let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();

        // Within boot-grace (loop just started) → suppressed, dedup untouched.
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            Instant::now(),
        );
        let fired_in_grace = sink.events.lock().iter().any(|e| {
            matches!(
                e,
                crate::channel::ux_event::UxEvent::Fleet(
                    crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
                ) if emitted == agent
            )
        });
        assert!(!fired_in_grace, "must NOT emit during boot-grace");
        assert!(
            !tracks.contains_key(agent),
            "boot-grace must skip the scan entirely (no dedup-map mutation)"
        );

        // After boot-grace elapsed → the still-stranded input emits once.
        check_pane_input_not_submitted_for_agents(
            &home,
            &[agent.to_string()],
            &mut tracks,
            past_boot_grace(),
        );
        let count_after = sink
            .events
            .lock()
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
        assert_eq!(count_after, 1, "must emit exactly once after grace ends");
        std::env::remove_var("AGEND_PANE_INPUT_THRESHOLD_SECS");
        std::fs::remove_dir_all(home).ok();
    }

    /// #1125 M1 source-pin: supervisor's per-tick loop body MUST be
    /// wrapped in `catch_unwind` so a panic in any tracker doesn't kill
    /// the supervisor thread (silent loss of all health monitoring).
    #[test]
    fn supervisor_tick_loop_has_catch_unwind() {
        let src = include_str!("supervisor.rs");
        let loop_start = src.find("fn run_loop(").expect("run_loop must exist");
        let rest = &src[loop_start..];
        assert!(
            rest.contains("catch_unwind"),
            "supervisor run_loop must wrap tick body in catch_unwind (#1125 M1)"
        );
    }

    /// #1002 Phase 2 source-pin: supervisor's per-tick loop MUST call
    /// `crate::daemon::pr_state::scan_and_emit`. The #972 / #986
    /// aggregator + gh-poll integration was previously wired only via
    /// `run_core`'s `PerTickHandler` vec (daemon-only entry). In APP
    /// mode (`agend-terminal app`), `run_core` is never called — the
    /// supervisor loop is the canonical per-tick driver instead.
    /// Without this wiring, `last_gh_poll_at: null` persists
    /// indefinitely and `[pr-ready-for-merge]` events never emit.
    ///
    /// File-level positive pin (cross-platform-safe; same pattern as
    /// `app::tests::flush_idle_notifications_wired_to_submit_aware_inject`
    /// from #982 RC2). If a future refactor moves the call out of
    /// the loop, update this assertion alongside.
    #[test]
    fn pr_state_scan_wired_into_supervisor_loop() {
        let source = std::fs::read_to_string("src/daemon/supervisor.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/supervisor.rs"))
            .expect("source file must be readable from test cwd");
        assert!(
            source.contains("pr_state::scan_and_emit"),
            "supervisor per-tick loop must invoke pr_state::scan_and_emit \
             (#1002 Phase 2 dual-entry-point fix). Without this, APP-mode \
             daemons silently skip the #972 aggregator + #986 gh-poll path."
        );
    }

    // ── #1696 / #1697: tiered retry + ApiError quick-nudge ──

    /// Build a registry with one agent at `state`, fleet.yaml seeded so the
    /// name-keyed tracking resolves to the handle. Returns (home, registry, reader).
    fn one_agent_registry(
        name: &str,
        state: crate::state::AgentState,
        tag: &str,
    ) -> (
        std::path::PathBuf,
        AgentRegistry,
        Box<dyn std::io::Read + Send>,
    ) {
        let home = tmp_home(tag);
        let (handle, reader) = mock_agent_handle(name, state);
        let id = handle.id;
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  {name}:\n    id: {}\n", id.full()),
        )
        .ok();
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        registry.lock().insert(id, handle);
        (home, registry, reader)
    }

    /// #1713 (replaces the #1696 `suppress_thinking_clear` band-aid): a
    /// continue-inject's transient Thinking must NOT clear the retry track (else
    /// tiered Phase B/C would restart at Phase A) AND must not itself inject
    /// (Thinking != ServerRateLimit). With #1713 this holds STRUCTURALLY — Thinking
    /// is neither a clearing state ({Ready,Idle}) nor the decision state
    /// (ServerRateLimit) — so no inject-cooldown suppression window is needed.
    /// Tiered progress (retry_count) is preserved; the next ServerRateLimit
    /// observation continues it.
    #[test]
    fn thinking_transient_keeps_track_and_progress_1713() {
        let (home, registry, _r) = one_agent_registry(
            "ag",
            crate::state::AgentState::Thinking,
            "1713-thinking-keep",
        );
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        // next_retry_at = now (DUE) — proves a due track still does NOT inject when
        // the fresh state is Thinking (the gate is state, not merely the timer).
        tracks.insert(
            "ag".into(),
            RateLimitRetry {
                retry_count: 4,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
        );
        assert!(
            tracks.contains_key("ag"),
            "#1713: a Thinking transient must NOT clear the retry track"
        );
        assert_eq!(
            tracks["ag"].retry_count, 4,
            "#1713: tiered retry progress preserved (no Phase-A restart)"
        );
        assert!(
            last_inject.is_empty(),
            "#1713: Thinking is not ServerRateLimit → no `continue` inject even when due"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1697: an ApiError-at-prompt agent gets an immediate `continue` nudge, ONCE
    /// per episode (no re-nudge while still in the same ApiError episode).
    // Reads the injected payload back off the PTY — Windows' mock PTY (`cmd
    // findstr`) doesn't echo like unix `cat`, so this is unix-only, mirroring the
    // existing `phase2_injects_continue_to_pty` gate.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn apierror_at_prompt_quick_nudge_once_per_episode_1697() {
        let (home, registry, mut reader) =
            one_agent_registry("ag", crate::state::AgentState::ApiError, "apierror-nudge");
        let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut episodes,
            &mut Default::default(),
            &mut last_inject,
        );
        assert!(
            episodes.contains("ag"),
            "#1697: ApiError episode must be marked nudged"
        );
        let mut buf = vec![0u8; 256];
        use std::io::Read;
        let n = reader.read(&mut buf).expect("read from PTY");
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("continue"),
            "#1697: ApiError nudge must inject \"continue\""
        );

        // Second tick, STILL ApiError + in episode → no re-nudge.
        let before = last_inject.get("ag").copied();
        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut episodes,
            &mut Default::default(),
            &mut last_inject,
        );
        assert_eq!(
            last_inject.get("ag").copied(),
            before,
            "#1697: must not re-nudge within the same ApiError episode"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn apierror_nudge_caps_per_flicker_window_1742_f4() {
        // #1742-F4: a content-FP `ApiError↔Thinking` flicker re-arms the
        // per-episode dedup every cycle, so without a total cap the quick-nudge
        // injects indefinitely (bounded only by MIN_INTERVAL). Simulate the
        // flicker by clearing `episodes` (re-armed) + `last_inject` (>MIN_INTERVAL
        // elapsed) each cycle while the agent stays ApiError, and assert the nudge
        // count caps at APIERROR_NUDGE_MAX instead of growing unbounded.
        let (home, registry, _reader) =
            one_agent_registry("ag", crate::state::AgentState::ApiError, "apierror-cap");
        let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut counts: HashMap<String, u32> = HashMap::new();
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        for _ in 0..(super::APIERROR_NUDGE_MAX + 3) {
            episodes.clear(); // flicker → next ApiError counts as a "new episode"
            last_inject.clear(); // >CONTINUE_INJECT_MIN_INTERVAL elapsed
            super::process_error_recovery(
                &home,
                &registry,
                &mut Default::default(),
                &mut episodes,
                &mut counts,
                &mut last_inject,
            );
        }

        // The first APIERROR_NUDGE_MAX cycles nudge (proving a single ApiError
        // still nudges); the rest are capped — so the count is exactly the cap,
        // not APIERROR_NUDGE_MAX + 3. (Negative-probe: drop the `!capped` gate and
        // this reaches APIERROR_NUDGE_MAX + 3.)
        assert_eq!(
            counts.get("ag").copied(),
            Some(super::APIERROR_NUDGE_MAX),
            "#1742-F4: ApiError nudge count must cap at APIERROR_NUDGE_MAX despite \
             continued flicker"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1680 regression (source guard): the shared continue-inject MUST pass
    /// `force=false` so it routes through `should_defer_direct_inject` and defers
    /// while the operator is typing — never clobbering a half-typed draft. Pins the
    /// fix of the pre-existing force-true retry inject.
    #[test]
    fn continue_inject_is_draft_gated_force_false_1680() {
        let src = include_str!("supervisor.rs");
        assert!(
            src.contains("inject_with_target_gated(&tgt, name, CONTINUE_RETRY_PAYLOAD, false)"),
            "#1680: the continue-inject must pass force=false (draft-gated)"
        );
        // Split needle so this assertion's own text can't false-match the source.
        let force_true = format!("CONTINUE_RETRY_PAYLOAD,{}true)", " ");
        assert!(
            !src.contains(&force_true),
            "#1680: no force=true continue-inject may remain"
        );
    }

    /// #1595 Step 1 (source guard): the ServerRateLimit retry-exhausted Telegram
    /// notify MUST be `Error` (not Warn) so it breaks through the #1594 Sleep-mode
    /// gate. The gate's Error-passes-Sleep / Warn-suppressed policy is pinned by
    /// `channel::tests::should_notify_in_mode_policy_grid`; this pins the producer
    /// side — that exhaustion (the full #1696 tiered budget burned) escalates to a
    /// P0 that wakes a sleeping operator instead of being silently dropped.
    #[test]
    fn server_rate_limit_exhausted_notify_is_error_severity_1595() {
        let src = include_str!("supervisor.rs");
        // Window the exhaust branch (production), well away from this test body.
        let idx = src
            .find("auto-retry exhausted")
            .expect("exhaust notice present in source");
        let window = &src[idx..(idx + 1200).min(src.len())];
        assert!(
            window.contains("NotifySeverity::Error"),
            "#1595: the ServerRateLimit-exhausted gated_notify must use Error severity"
        );
        assert!(
            !window.contains("NotifySeverity::Warn"),
            "#1595: the exhaust notify must not remain Warn (suppressed in Sleep)"
        );
    }
}
