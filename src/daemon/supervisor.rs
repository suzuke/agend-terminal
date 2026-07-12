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
mod usage_limit;
pub(crate) use usage_limit::*;
mod reactions;
mod usage_limit_control;
pub(crate) use reactions::*;

/// Vterm tail size pushed to Telegram when a stall is detected.
const TAIL_LINES: usize = 40;
/// Debounce cooldown for member-state-change notify (Sprint 43).
const NOTIFY_COOLDOWN: Duration = Duration::from_secs(60);

/// #1552: minimum continuous time in a runtime prompt state before escalating
/// to AwaitingOperator (stability gate — a transient streaming flicker of the
/// permission chrome never holds this long).
const AWAITING_STABILITY: Duration = Duration::from_secs(8);
/// #2033: minimum blocked-episode duration for the "recovered from blocked state"
/// Telegram notice to be actionable. Set to the AwaitingOperator silence floor
/// (30s) — an episode that reached genuine operator-attention territory. Below it
/// is an InteractivePrompt that self-resolved before the operator could plausibly
/// engage, so the recovery notice would be non-actionable noise. The common
/// AwaitingOperator path always clears this bar: the ≥30s silence accrues while
/// already in (raw) InteractivePrompt, which the episode clock counts.
const RECOVERY_NOTICE_MIN_BLOCK: Duration = Duration::from_secs(30);
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
///
/// t-...30532-0: `pub(crate)` so `per_tick::respawn_watchdog`'s auth-expiry
/// classifier reuses this SAME window (single source of truth) rather than
/// copying the 90s — both gates defend the identical content-FP boundary on the
/// identical `StateTracker::since` continuous-`AuthError` signal.
pub(crate) const AUTH_ERROR_NOTIFY_STABILITY: Duration = Duration::from_secs(90);
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
/// ServerRateLimit recovery window — single source of truth in `state` (the
/// foundational layer). This retry-inject gate and the detection-side badge
/// re-latch gate share the SAME window, so they're one constant. See
/// `crate::state::SERVER_RATE_LIMIT_RECOVERY_SILENCE` for the full rationale.
use crate::state::SERVER_RATE_LIMIT_RECOVERY_SILENCE as RECOVERY_SILENCE;
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
/// #2232 / #26795-3: ServerRateLimit retry payload. It originally carried
/// `continue` PLUS a one-line instruction telling a now-awake agent to self-clear
/// its rate-limit block (MCP `clear_blocked_reason` reason=rate_limit). The
/// #26795-3 SRL spike found that instruction is INEFFECTIVE noise: agents
/// essentially never call it (recovery_shadow: 2746/2746 `self_cleared=false`),
/// and the recovery it was meant to signal is now covered by the hook-authoritative
/// path (#t-26795 `hook_recovered`, claude) + the `recovered_within` heuristic. So
/// the payload is now a PLAIN `continue` nudge — the `[AGEND-AUTO
/// kind=ratelimit-retry]` marker (driven by `auto_kind`, NOT this body) still tags
/// it, matching the protocol doc's `[AGEND-AUTO kind=ratelimit-retry] continue`
/// example. ASCII, SINGLE line + one trailing "\n". Kept as a SEPARATE const from
/// the shared `CONTINUE_RETRY_PAYLOAD` so the #1680 source-guard's distinct
/// `RATELIMIT_RETRY_PAYLOAD, false, Some(auto_kind)` scan stays intact. (Removing
/// the now-dead `self_cleared` recovery SIGNAL is the separate #26795-3 ① PR.)
const RATELIMIT_RETRY_PAYLOAD: &[u8] = b"continue\n";

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

/// #1741 boot-grace (B): apply the [`auth_error_gate`] verdict to the pending
/// map, honoring boot-grace. Returns `Some(entry)` — removed from `pending_auth`
/// — when the deferred operator notify should FIRE this tick.
///
/// During boot-grace a `Fire` is HELD: the entry stays pending and `None` is
/// returned, so a post-restart re-page (the in-mem confirm-window restarts on
/// restart → a pre-existing AuthError re-confirms and would re-fire ~90s in,
/// still within the 180s grace) is suppressed WITHOUT losing the notify — it
/// fires on a later tick once the grace ends. `Cancel` drops the pending entry
/// (self-healed blip); `Wait` leaves it untouched. Pure + testable; the
/// confirm-window classification (`auth_error_gate`) is never gated.
fn resolve_pending_auth(
    gate: AuthErrorGate,
    in_boot_grace: bool,
    name: &str,
    pending_auth: &mut HashMap<String, PendingAuthError>,
) -> Option<PendingAuthError> {
    match gate {
        AuthErrorGate::Fire if in_boot_grace => None,
        AuthErrorGate::Fire => pending_auth.remove(name),
        AuthErrorGate::Cancel => {
            pending_auth.remove(name);
            None
        }
        AuthErrorGate::Wait => None,
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
    /// #1946: set when an abort-to-Idle was detected with this retry in flight
    /// (retry_count > 0, no recent productive output). The track is RETAINED —
    /// ownership of recovery stays here — and the Idle-state after-abort inject
    /// continues the SAME tiered backoff budget. Cleared when a fresh
    /// ServerRateLimit observation resumes the normal retry path.
    pub abort_pending: bool,
}

/// Parse unlock time from usage_limit pane output (e.g., "resets at 15:14 UTC").
fn parse_unlock_at(pane_text: &str) -> Option<String> {
    // Common patterns: "resets at HH:MM", "try again after HH:MM", "limit resets HH:MM"
    for line in pane_text.lines().rev() {
        let lower = line.to_lowercase();
        if lower.contains("reset") || lower.contains("try again") || lower.contains("limit") {
            // Extract time-like pattern HH:MM. `idx` and the slice must come
            // from the SAME string: `to_lowercase()` is not byte-length-
            // preserving (U+212A Kelvin 'K' is 3 bytes, lowercases to ASCII 'k'),
            // so an offset found in `lower` can land mid-multibyte-char in `line`
            // and panic. Slice `lower`, and match the HH:MM shape char-by-char
            // (\d{2}:\d{2}) instead of fixed byte indexing, since `pane_text` is
            // content-controlled PTY output that may contain multibyte chars.
            if let Some(idx) = lower.find(|c: char| c.is_ascii_digit()) {
                let mut chars = lower[idx..].chars();
                let hhmm: Option<Vec<char>> = (0..5).map(|_| chars.next()).collect();
                if let Some(hhmm) = hhmm {
                    if hhmm[0].is_ascii_digit()
                        && hhmm[1].is_ascii_digit()
                        && hhmm[2] == ':'
                        && hhmm[3].is_ascii_digit()
                        && hhmm[4].is_ascii_digit()
                    {
                        return Some(hhmm.into_iter().collect());
                    }
                }
            }
        }
    }
    None
}

/// Spawn the supervisor thread. Idempotent per process is the caller's
/// responsibility — in practice each entry point calls it exactly once.
///
/// W1.1 (#2050): the 12 periodic trackers that used to run inline in this
/// loop (anti_stall … retention) moved to `PerTickHandler`s in
/// `build_default_handlers`. The `mcp_registry` tracker owned the only
/// external dependency this thread needed (the `DaemonBinaryStale` TUI flag),
/// so with it gone the supervisor no longer takes that argument — the handler
/// now holds the flag.
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
    // #t-26795: per-INSTANCE SRL forward-progress hook-seq floor, keyed by stable
    // InstanceId (NOT name — a same-name replacement must not inherit the prior
    // instance's floor; r6 finding-2). See `process_error_recovery`.
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    let mut pane_input_tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    // W1.1 (#2050): the 12 periodic trackers (anti_stall, idle_watchdog,
    // decision_timeout, helper_staleness, mcp_registry, waiting_on_stale,
    // conflict_notify, canonical_drift, auto_release, dispatch_idle,
    // dispatch_idle_nudge, retention) that used to be declared here and scanned
    // inline below moved to `PerTickHandler`s in `build_default_handlers`. This
    // one-shot boot purge is NOT a per-tick scan — it ran exactly once at
    // supervisor start, before the loop — so it stays here, unchanged (#1022:
    // drop ghost activity sidecars for instances no longer in fleet.yaml).
    crate::daemon::idle_watchdog::gc_stale_activity_sidecars(&home);
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
            tick(
                &home,
                &registry,
                &mut notify_tracks,
                &mut pending_auth,
                loop_started_at,
            );
            process_error_recovery(
                &home,
                &registry,
                &mut retry_tracks,
                &mut apierror_episodes,
                &mut apierror_nudge_counts,
                &mut last_continue_inject,
                &mut srl_floor,
                loop_started_at,
            );
            check_pane_input_not_submitted(
                &home,
                &registry,
                &mut pane_input_tracks,
                loop_started_at,
            );
            // W1.1 (#2050): the 12 inline tracker scans that ran here
            // (anti_stall … retention) moved to `PerTickHandler`s in
            // `build_default_handlers`, which the main loop runs at the same
            // 10s cadence. The trackers keep their internal TICKS_PER_SCAN
            // throttle, so the effective scan cadence is unchanged.
            // #986: the supervisor loop NO LONGER scans pr_state directly. The
            // `PrStateScanHandler` per-tick handler is the SINGLE scanner in ALL
            // modes — it runs in run_core's handler vec (daemon) AND in
            // `app::app_tick_handlers` (app standalone, both attached and owned,
            // since `pr_state_scan` is not in `APP_TICK_ALLOWLIST`). The old direct
            // scan here was a vestigial app-mode belt from when the handler was
            // run_core-only; with the handler now live in every mode it was a
            // redundant SECOND scanner + (post-#986) a SECOND gh-poll worker. The
            // handler owns the single snapshot cache + worker.
            // Source-pin: `pr_state_scan_wired_into_supervisor_loop` asserts the
            // supervisor does NOT scan (guards against re-adding it here).
            // #836: reclaim expired (10-min TTL) entries from the
            // notification-dedup ledger so memory pressure stays bounded
            // on long-lived daemons.
            crate::daemon::notification_dedup::global().sweep_expired();
            // #842: same eviction cadence for the bridge↔daemon idempotent-
            // retry dedup cache. Sibling sweep, same 10-min TTL window.
            crate::api::request_dedup::global().sweep_expired();
            // Reply-to correlation: prune the sent-message ledger (14-day TTL +
            // per-agent FIFO cap). Self-throttled to an hourly rewrite, so this
            // 10s-cadence call is cheap on the off-ticks.
            crate::sent_ledger::global(&home).maybe_gc(&home);

            // #1923 G4/G5: prune per-agent in-memory tracker state for agents no
            // longer in the registry (deleted / redeployed), mirroring the #1470
            // retry_tracks sweep in `process_error_recovery`. `notify_tracks`
            // lives in `run_loop` (not reachable from the cross-thread
            // `full_delete_instance` delete path), so the per-tick `.retain` IS
            // its cleanup-on-delete: a deleted agent leaves the registry → drops
            // out of `live_agents` → its entries are pruned next tick. Without
            // it a same-name redeploy inherits the old instance's state — a
            // false-SUPPRESSED crash notify (notify_tracks dedup).
            // W1.1 (#2050): the sibling `conflict_notify_tracker.retain_active`
            // prune moved into `ConflictNotifyHandler::run` alongside the
            // tracker it cleans (its state now lives in that handler).
            let live_agents = agent::live_agent_names(&registry);
            notify_tracks.retain(|name, _| live_agents.contains(name));
            // CR-2026-06-14 (resource-leak): `pending_auth` has NO
            // cleanup-on-delete path of its own — entries are only removed inside
            // `resolve_pending_auth` for agents present in THIS tick's `handles`
            // (live agents). An agent deleted / redeployed while a `Wait`
            // AuthError notify is still pending is never revisited, so its entry
            // leaks forever AND a same-name redeploy inherits the stale `from` /
            // `pane_tail` (the insert uses `.entry(name).or_insert(...)`, which
            // will NOT overwrite). Sweep it against the live set, mirroring the
            // `notify_tracks` prune directly above.
            pending_auth.retain(|name, _| live_agents.contains(name));
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
/// Threshold is a fixed 60s const (#env-cleanup: the
/// `AGEND_PANE_INPUT_THRESHOLD_SECS` override was demoted).
pub(crate) fn check_pane_input_not_submitted(
    home: &std::path::Path,
    registry: &AgentRegistry,
    tracks: &mut HashMap<String, PaneInputTrack>,
    loop_started_at: Instant,
) {
    let agent_names = agent::live_agent_names_vec(registry);
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
    // Fixed const 60s (#env-cleanup: was env-overridable via
    // `AGEND_PANE_INPUT_THRESHOLD_SECS`; demoted to YAGNI for single-user deploys).
    const PANE_INPUT_THRESHOLD_SECS: u64 = 60;
    let threshold_ms = (PANE_INPUT_THRESHOLD_SECS as i64).saturating_mul(1000);
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
    // #perf-R4: per-tick hot path → load_arc (Arc refcount bump, not deep clone).
    let Ok(fleet) = crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(home))
    else {
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
    // #perf-R4: per-tick hot path → load_arc (Arc refcount bump, not deep clone).
    crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(home))
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

/// #1595/#1744: escalate a self-orchestrator `AuthError` straight to the operator
/// — stamp the per-agent notify cooldown (so a persistent AuthError pages at most
/// once per `NOTIFY_COOLDOWN`) and dispatch the P0 to EVERY registered channel
/// (#1744-M6: multi-channel-safe; `active_channel()` would silently drop it).
/// Shared by the resolved-self-orch path AND the #1744-M7 fail-closed path
/// (teams config unreadable → can't identify a peer to relay to, and AuthError is
/// operator-only, so page the operator rather than drop).
fn escalate_self_orch_autherror(
    name: &str,
    now: Instant,
    tracks: &mut HashMap<String, NotifyTrack>,
) {
    let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
        last_at: now,
        consecutive: 0,
    });
    track.consecutive += 1;
    track.last_at = now;
    let msg = format!(
        "🔑 {name} (team orchestrator) hit AuthError — only the operator can re-authenticate, and no peer can relay this. Check credentials / re-auth the agent."
    );
    crate::channel::notify_all_escalation_channels(name, NotifySeverity::Error, &msg, false);
    tracing::info!(agent = %name, "#1595/#1744: self-orchestrator AuthError escalated to operator (all channels)");
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
#[allow(clippy::too_many_arguments)] // pure decision fn — every gate input passed explicitly for testability
fn awaiting_escalation_allowed(
    state: crate::state::AgentState,
    state_held: Duration,
    backend: Option<crate::backend::Backend>,
    live_tail: &str,
    operator_typed_ms: i64,
    now_ms: i64,
    idle_expectation: crate::fleet::IdleExpectation,
    produced_productive_output: bool,
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
        // #2020 veto: an agent that has rendered backend PRODUCTIVE MARKERS
        // since this spawn is demonstrably working, not stalled at a login
        // prompt — a busy respawned agent (injected work immediately, never
        // renders the clean ready-prompt) hit a >30s silence window in
        // `Starting` and was forced to a false AwaitingOperator (live:
        // fixup-lead, 2026-06-11 20:09). Markers (tool-use chrome) are
        // chosen over "any output after an inject" deliberately: a REAL
        // login prompt can echo injected text (output!) but never renders
        // tool chrome — so the veto can't blind the fallback's actual job.
        // In-memory per spawn (StateTracker resets on respawn), so evidence
        // from a previous life can't leak in.
        AgentState::Starting => {
            idle_expectation == crate::fleet::IdleExpectation::Active && !produced_productive_output
        }
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
    loop_started_at: Instant,
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
            // #1915 TIER-B1: skip a handle being deleted (deleted flag set in
            // delete_transaction Step1, handle removed Step4) — otherwise the
            // transition-reaction loop below fires spurious orchestrator
            // notifications (NotifySeverity::Error) about an instance that is
            // mid-teardown. Separate concern from the spawn chokepoint.
            .filter(|h| !h.deleted.load(std::sync::atomic::Ordering::Acquire))
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
        // #1665/#2042 reply-ledger: TTL/settled fallback for a user-message
        // turn that never hit a clear site (no reply, no mirror, no takeover).
        // Lock-free snapshot read; acts only past the grace window AND when the
        // agent has settled. Infallible — never blocks the supervisor loop.
        // The #2042 ladder routes the obligation to the actionable party, each
        // stage at most once per obligation (escalate-don't-repeat): nudge the
        // owing agent (with the message id + reply-tool instruction) → its lead
        // on the second miss → the operator only as last resort, phrased for
        // humans. The audit WARN stays in the logs (emitted inside `sweep`).
        match crate::reply_ledger::sweep(home, &name, &|n| {
            crate::teams::find_team_for(home, n).and_then(|t| t.orchestrator)
        }) {
            crate::reply_ledger::SweepAction::None => {}
            crate::reply_ledger::SweepAction::NudgeAgent {
                channel,
                msg_id,
                gap_d,
            } => {
                inject_channel_reply_missing_gated(
                    home,
                    registry,
                    &name,
                    channel,
                    msg_id.as_deref(),
                    gap_d,
                );
            }
            crate::reply_ledger::SweepAction::EscalateLead {
                lead,
                channel,
                msg_id,
                armed_at_ms,
            } => {
                enqueue_reply_ledger_lead_escalation(
                    home,
                    &name,
                    &lead,
                    channel,
                    msg_id.as_deref(),
                    armed_at_ms,
                );
            }
            crate::reply_ledger::SweepAction::NotifyOperator {
                channel,
                msg_id: _,
                armed_at_ms,
            } => {
                crate::reply_ledger::notify_operator_last_resort(home, &name, channel, armed_at_ms);
            }
        }
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
        // CR-2026-06-14 (concurrency): captured under the core lock, the actual
        // (disk-writing) `clear_waiting_on_if_stale` runs lock-free after the
        // lock drops — keeps blocking file IO out of the per-agent core-mutex
        // hold that the PTY read-loop `feed` contends on.
        let waiting_on_heartbeat_stale: bool;
        // UsageLimit Slice 1 consumes the same authoritative raw state for
        // detection and recovery. Capture it with the pane tail under the core
        // lock, then perform all disk/registry work after the guard drops.
        let usage_limit_raw_state: crate::state::AgentState;
        let usage_limit_pane_tail: String;
        // CR-2026-06-14 (concurrency): hoist the two per-agent disk reads that
        // feed the awaiting-operator gate OUT of the core lock. Both depend only
        // on (home, name), not on core state, so reading them lock-free here
        // removes blocking file IO from the lock window. `read_input_submit_
        // timestamps` was previously short-circuited behind `check_awaiting_
        // operator`; computing it unconditionally is a cheap notification-queue
        // read and the value is still only USED inside that gate, so behavior is
        // unchanged.
        let idle_expectation = idle_expectation_for(home, &name);
        let (typed_ms, _submit_ms) =
            crate::notification_queue::read_input_submit_timestamps(home, &name);
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

            // §4.4 stale decay: capture the staleness bool under the lock; the
            // disk-writing `clear_waiting_on_if_stale` runs lock-free after the
            // lock drops (CR-2026-06-14 concurrency).
            waiting_on_heartbeat_stale = !core.state.is_heartbeat_fresh();

            // KEEP-RAW (#2465): the AuthError stability / escalation gate reads raw
            // core.state.current. operated_state is inert here anyway (AuthError maps to the
            // gate's non-decisive 'Other' screen, which is never overridden), and this is a
            // recovery/escalation decider that must see raw. See `operated_state` docstring.
            let agent_state = core.state.current;
            usage_limit_raw_state = agent_state;
            usage_limit_pane_tail = core.vterm.tail_lines(10);
            // #1523: capture how long AuthError has been continuously held (state
            // age) for the post-lock stability gate. `Some` iff currently in
            // AuthError; the gate uses it to confirm/cancel a deferred notify.
            auth_error_held = (agent_state == crate::state::AgentState::AuthError)
                .then(|| core.state.since.elapsed());
            let silent = core.state.last_output.elapsed();
            // #1563: role policy gates the two `Starting`-context stall-forward
            // paths (branch-1 startup-stall, branch-2 startup-prose prompt) for
            // an `OnDemand` coordinator; the runtime permission/interactive
            // escalation stays role-blind (handled inside the fn). `idle_
            // expectation` is captured lock-free above (CR-2026-06-14).
            if core.health.check_awaiting_operator(agent_state, silent) && {
                // #1552 escalation FP-gates (only reached when silent>30s +
                // a prompt state). #1530/F2: backend resolved from the
                // pre-captured command — NO registry re-acquire while holding
                // core (removes the core→registry inversion).
                let backend = crate::backend::Backend::from_command(&backend_command);
                // `typed_ms` captured lock-free above (CR-2026-06-14).
                awaiting_escalation_allowed(
                    agent_state,
                    core.state.since.elapsed(),
                    backend,
                    &core.vterm.tail_lines(AWAITING_TAIL_LINES),
                    typed_ms,
                    crate::daemon::heartbeat_pair::now_ms() as i64,
                    idle_expectation,
                    core.state.last_productive_output.is_some(),
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
                    // #2033: pair this blocked notice with the recovery notice —
                    // record that the operator was actually told about the block.
                    core.state.mark_blocked_notice_sent();
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
                // #2033: pair this blocked notice with the recovery notice.
                core.state.mark_blocked_notice_sent();
                Some(NoticeAction::Stall {
                    tail: core.vterm.tail_lines(TAIL_LINES),
                    silent_secs: None,
                })
            } else if let Some(episode) = core.state.take_recovery_notice() {
                // Symmetric "ready again" signal: armed on the transition
                // out of InteractivePrompt / AwaitingOperator.
                // #1552: clear the AwaitingOperator health reason on recovery so
                // `check_hang` is no longer exempt and a future stall can
                // re-notify (the once-per-episode dedup re-arms). #1638: the
                // operator-resolution clear-policy is now on the type, so this
                // never clears a different blocked reason (RateLimit / etc.).
                // This health cleanup runs REGARDLESS of whether we forward the
                // telegram notice below — it is internal state, not operator-facing.
                if core.health.current_reason.as_ref().is_some_and(|r| {
                    r.auto_clears_on(crate::health::RecoverySignal::OperatorResolved)
                }) {
                    core.health.clear_blocked_reason();
                }
                // #2033: actionable-or-silent (#2008). A "recovered" notice is only
                // useful when the operator was actually told about the block AND it
                // lasted long enough that they might be reacting. A self-resolving /
                // never-notified block (the operator-flagged noise, e.g. the #2020
                // false AwaitingOperator) is log-only.
                if recovery_notice_is_actionable(episode) {
                    tracing::info!(
                        agent = %name,
                        block_secs = episode.block_duration.as_secs(),
                        "recovered from blocked state — notifying telegram"
                    );
                    Some(NoticeAction::Recovered)
                } else {
                    tracing::debug!(
                        agent = %name,
                        block_secs = episode.block_duration.as_secs(),
                        notice_sent = episode.notice_sent,
                        "recovered from blocked state — log-only (#2033: block not \
                         notified or under threshold, recovery notice non-actionable)"
                    );
                    None
                }
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

        // CR-2026-06-14 (concurrency): §4.4 stale decay runs here, lock-free —
        // it is a disk read + batch write (`clear_waiting_on_if_stale`) that
        // was contending with the PTY read-loop `feed` under the core lock. The
        // staleness bool was captured under the lock above.
        clear_waiting_on_if_stale(home, &name, waiting_on_heartbeat_stale);

        if let Err(error) = usage_limit_control::observe_supervisor_tick(
            home,
            registry,
            &name,
            usage_limit_raw_state,
            &backend_command,
            &usage_limit_pane_tail,
        ) {
            tracing::warn!(agent = %name, %error, "UsageLimit control-plane tick failed closed");
        }

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
                            } else if !crate::daemon::per_tick::in_boot_grace(loop_started_at) {
                                // #1741 boot-grace: a daemon restart resets
                                // notify_tracks (the 60s cooldown) AND re-derives
                                // agent states, so a pre-existing error state's
                                // re-detected edge would re-notify the orchestrator.
                                // Suppress for the post-boot window; a genuine NEW
                                // edge after grace still notifies.
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
        // #1741 boot-grace (B): `resolve_pending_auth` HOLDS a `Fire` during the
        // post-boot window (keeps the entry pending, returns None) so a
        // post-restart re-page is suppressed WITHOUT losing a genuine held-AuthError
        // notify — it fires once the grace ends. The confirm-window classification
        // (`auth_error_gate`) itself is never gated, and `Cancel`/`Wait` are
        // unaffected.
        let in_boot_grace = crate::daemon::per_tick::in_boot_grace(loop_started_at);
        if let Some(p) = resolve_pending_auth(
            auth_error_gate(auth_error_held),
            in_boot_grace,
            &name,
            pending_auth,
        ) {
            maybe_notify_member_state_change(
                home,
                &name,
                p.from,
                crate::state::AgentState::AuthError,
                &p.pane_tail,
                notify_tracks,
            );
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
                // #1744-M6: stall is an Error-severity operator P0 — deliver to
                // every registered channel (multi-channel-safe).
                crate::channel::notify_all_escalation_channels(
                    &name,
                    NotifySeverity::Error,
                    &msg,
                    false,
                );
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

/// #2466: the WORK-TURN 529 arm predicate — the looser signal the dev-3 incident proved
/// fired. A claude work-turn that hits an ApiError/529 latches `blocked_reason==RateLimit`
/// (via the watchdog's gate-free `classify_pty_output`) and leaves a `throttle_hint` token on
/// screen, yet the StateTracker `AgentState` stays Idle (the #1769 positional defeat) so the
/// strict `state==ServerRateLimit` arm never fires. This predicate lets the retry arm latch
/// that case, GUARDED against false-positives:
/// - `blocked_rl` (primary, lead-vetted) — classify matched a real RATE-LIMIT error pattern,
///   not a generic ApiError; `QuotaExceeded` is a distinct variant, so user quota is excluded.
/// - `has_throttle_hint` — corroborating throttle token still on screen.
/// - `!recovered` — the agent is not awake/progressing. `recovered` =
///   `recovered_within(RECOVERY_SILENCE)`, so `!recovered` already encodes the productive-silence
///   requirement AND preserves the never-produced fresh-agent edge (a just-spawned agent that
///   immediately 529s still arms — an explicit silence threshold would wrongly block it).
///
/// SINGLE-OWNERSHIP (decision d-20260625052109609105-0): there is NO live production recovery-arm
/// today — the Phase-0 `recovery_shadow` measure-only shadow this reasoning was originally scoped
/// against was itself removed in #2547 (zero real consumers of its verdict) — so there is no
/// episode for this work-turn arm to double-own, by any mechanism, shadow or otherwise. When
/// dev-2 promotes 529-recovery to active it will carve the boundary with a PRODUCTION signal.
///
/// StopFailure-hook corroboration is deliberately NOT required (hooks are best-effort/droppable,
/// and would re-introduce a false-negative); it only adds confidence when present (claude).
fn work_turn_throttle_arm(blocked_rl: bool, has_throttle_hint: bool, recovered: bool) -> bool {
    blocked_rl && has_throttle_hint && !recovered
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
    auto_kind: &str,
) -> InjectOutcome {
    let snap = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, name)
            .and_then(|id| reg.get(&id))
            .map(agent::InjectTarget::from_handle)
    };
    match snap {
        Some(tgt) => {
            // #1769: a daemon self-originated auto-nudge — tag it with
            // `[AGEND-AUTO kind=...]` so an orchestrator agent doesn't mistake the
            // injected "continue" for an operator command (the worker still sees
            // and acts on the inner "continue").
            if agent::inject_with_target_gated(
                &tgt,
                name,
                CONTINUE_RETRY_PAYLOAD,
                false,
                Some(auto_kind),
            )
            .is_ok()
            {
                InjectOutcome::Injected
            } else {
                InjectOutcome::InjectFailed
            }
        }
        None => InjectOutcome::AgentGone,
    }
}

/// #2232 sibling of [`inject_continue_gated`] for the ServerRateLimit retry path:
/// injects [`RATELIMIT_RETRY_PAYLOAD`] (a plain `continue` since #26795-3 dropped
/// the ineffective self-clear instruction) via its own const instead of the shared
/// `CONTINUE_RETRY_PAYLOAD`, so the #1680 source-guard's distinct
/// `RATELIMIT_RETRY_PAYLOAD, false, Some(auto_kind)` scan stays intact (same split
/// rationale as [`inject_channel_reply_missing_gated`]). Same draft-gating
/// (`force=false`) + `[AGEND-AUTO kind=...]` tagging; returns the 3-state
/// [`InjectOutcome`].
fn inject_ratelimit_retry_gated(
    home: &std::path::Path,
    registry: &AgentRegistry,
    name: &str,
    auto_kind: &str,
) -> InjectOutcome {
    let snap = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, name)
            .and_then(|id| reg.get(&id))
            .map(agent::InjectTarget::from_handle)
    };
    match snap {
        Some(tgt) => {
            if agent::inject_with_target_gated(
                &tgt,
                name,
                RATELIMIT_RETRY_PAYLOAD,
                false,
                Some(auto_kind),
            )
            .is_ok()
            {
                InjectOutcome::Injected
            } else {
                InjectOutcome::InjectFailed
            }
        }
        None => InjectOutcome::AgentGone,
    }
}

/// #1813: inject the one-shot channel-reply-missing nudge. Sibling of
/// [`inject_continue_gated`] (kept separate so the #1680 source-guard on the
/// continue-inject's literal `CONTINUE_RETRY_PAYLOAD, false, Some(auto_kind)`
/// stays intact). Same draft-gating (`force=false`) and `[AGEND-AUTO kind=...]`
/// tagging — only the payload + kind differ. Best-effort: a missing agent /
/// failed inject is dropped (the #1665 warn already recorded the miss).
fn inject_channel_reply_missing_gated(
    home: &std::path::Path,
    registry: &AgentRegistry,
    name: &str,
    channel: &str,
    msg_id: Option<&str>,
    gap_d: bool,
) {
    let snap = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, name)
            .and_then(|id| reg.get(&id))
            .map(agent::InjectTarget::from_handle)
    };
    if let Some(tgt) = snap {
        // #2042: the payload names the owed message id and the reply tool;
        // Gap D (reply send FAILED) gets retry wording instead of a false
        // "you didn't reply".
        let payload = crate::reply_ledger::nudge_text(channel, msg_id, gap_d);
        let _ = agent::inject_with_target_gated(
            &tgt,
            name,
            payload.as_bytes(),
            false,
            Some("channel-reply-missing"),
        );
    }
}

/// #2042 ladder stage 2: notify the owing agent's lead via its inbox (kind
/// `update` — informational, the lead acts at its discretion; no reply loop).
/// Best-effort: an enqueue failure logs and never blocks the supervisor.
fn enqueue_reply_ledger_lead_escalation(
    home: &std::path::Path,
    agent_name: &str,
    lead: &str,
    channel: &str,
    msg_id: Option<&str>,
    armed_at_ms: i64,
) {
    let text = crate::reply_ledger::lead_text(agent_name, channel, msg_id, armed_at_ms);
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        delivering_at: None,
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        from: "system:reply-ledger".to_string(),
        text,
        kind: Some("update".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        reply_target: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
        pr_number: None,
        terminal: None,
    };
    if let Err(e) = crate::inbox::enqueue(home, lead, msg) {
        tracing::warn!(
            agent = %agent_name,
            lead = %lead,
            error = %e,
            "reply-ledger lead escalation enqueue failed"
        );
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

/// #t-26795 (PURE, unit-tested): does a fresh claude hook prove the agent recovered
/// from a STICKY screen-scraped `ServerRateLimit`? True iff the backend is claude AND
/// a fresh ACTIVE hook's monotonic seq is STRICTLY GREATER than the per-episode floor
/// — i.e. a NEW tool-call/thinking hook arrived since the floor was last consumed
/// (forward progress → the agent is executing → the screen text is stale). The floor
/// starts at the agent's latest hook seq at onset, so a pre-onset hook (seq ≤ floor)
/// is rejected (the load-bearing edge against masking a genuine new SRL); once a
/// recovery hook is consumed the floor ADVANCES to it, so a later genuine episode with
/// NO newer hook (seq == floor) re-arms instead of being permanently masked.
/// non-claude / no-hook / stale-hook → false → the screen-driven path is unchanged.
fn hook_recovered_for_srl(
    is_claude: bool,
    hook_active_seq: Option<u64>,
    floor: Option<u64>,
) -> bool {
    is_claude && matches!((hook_active_seq, floor), (Some(h), Some(f)) if h > f)
}

// Threads the run_loop's long-lived per-agent error/retry state maps (retry_tracks,
// apierror_episodes, apierror_nudge_counts, last_continue_inject, srl_floor); bundling
// them into a struct would not improve clarity here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_error_recovery(
    home: &std::path::Path,
    registry: &AgentRegistry,
    retry_tracks: &mut HashMap<String, RateLimitRetry>,
    apierror_episodes: &mut std::collections::HashSet<String>,
    apierror_nudge_counts: &mut HashMap<String, u32>,
    last_continue_inject: &mut HashMap<String, Instant>,
    // #t-26795: per-INSTANCE forward-progress FLOOR = the monotonic hook seq last
    // CONSUMED to clear this ServerRateLimit episode. Keyed by stable `InstanceId`, NOT
    // name (r6 finding-2): a same-name handle swapped between two ticks (delete/recreate
    // /restart) gets a NEW uuid → a FRESH floor, so it can't inherit the prior
    // instance's and mask its own genuine first SRL. Seeded at onset with the agent's
    // latest hook seq (or_insert never overwrites → stable across the detect→clear→
    // re-detect flap; pruned when the instance leaves the registry), then ADVANCED on
    // every override. The hook-override fires only for a fresh active hook STRICTLY
    // newer than this — so a stale recovery hook can't permanently mask a later genuine
    // episode (finding-1), and a pre-onset hook (seq ≤ floor) can't mask edge-a.
    srl_floor: &mut HashMap<crate::types::InstanceId, u64>,
    loop_started_at: Instant,
) {
    use crate::state::AgentState;
    let now = Instant::now();

    // Phase 1: classify states under the registry lock (NO PTY writes here).
    let mut active_names = std::collections::HashSet::new();
    // #t-26795 (r6 finding-2): live InstanceIds for the UUID-keyed srl_floor prune.
    let mut active_ids: std::collections::HashSet<crate::types::InstanceId> =
        std::collections::HashSet::new();
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
            active_ids.insert(handle.id);
            // Capture current state + recovery signal under one lock. `recovered`
            // is the #ratelimit-recovery gate below: a recovered-but-still-latched
            // ServerRateLimit agent that produced output within RECOVERY_SILENCE.
            // `recovered_within` is None-safe — a just-spawned agent that has NEVER
            // produced (`last_productive_output == None`) is NOT recovery, so it
            // latches + injects normally (the fresh-agent edge the creation stamp
            // used to mis-suppress). `productive_silence` is for the log only.
            let (state, recovered, has_throttle_hint, blocked_rl, productive_silence) = {
                let mut core = handle.core.lock();
                // KEEP-RAW (#2465): the SRL retry arm reads raw core.state.current. claude hooks
                // never emit RateLimited (a StopFailure → ApiError, the API plane owns rate-limit),
                // so operated_state would be inert here; the true ApiError-as-rate-limit SRL fix is
                // #2466 (the work-turn looser-signal arm below).
                let state = core.state.current;
                // #2466: the watchdog's LOOSER rate-limit latch — `classify_pty_output` matched a
                // throttle banner (it runs `detect_with_match` WITHOUT the StateTracker's #919
                // red-anchor / #1769 positional gates), so this can be `RateLimit` even when the raw
                // `state` reads Idle (the dev-3 work-turn 529 shape). This is the signal that DID
                // fire in the incident; the loose arm below consults it. Read BEFORE the
                // `ServerRateLimit` set below so it reflects the watchdog latch, not our own write
                // (which only happens on the strict `state==ServerRateLimit` branch anyway).
                // Excludes `QuotaExceeded` (user quota) by construction — that is a distinct variant.
                let blocked_rl = matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::RateLimit { .. })
                );
                let recovered = core.state.recovered_within(RECOVERY_SILENCE);
                let has_hint =
                    crate::state::screen_has_throttle_hint(&core.vterm.tail_lines(TAIL_LINES));
                let productive_silence = core.state.productive_silence();
                if state == AgentState::ServerRateLimit {
                    // We are about to track/inject a rate-limit retry, so we ALREADY
                    // KNOW the agent is rate-limited — mark it blocked (only when not
                    // already RateLimit-latched, to avoid clobbering a watchdog-set
                    // `retry_after_secs`). Feeds the loose-arm `blocked_rl` read on
                    // the next tick and the operator-visible health status.
                    if !recovered
                        && !matches!(
                            core.health.current_reason,
                            Some(crate::health::BlockedReason::RateLimit { .. })
                        )
                    {
                        core.health
                            .set_blocked_reason(crate::health::BlockedReason::RateLimit {
                                retry_after_secs: None,
                            });
                    }
                }
                (state, recovered, has_hint, blocked_rl, productive_silence)
            };

            // ── #t-26795 SRL hook-override (operator-reported sticky-screen flap) ──
            // A sticky screen-scraped ServerRateLimit while a FRESH claude hook proves
            // the agent is mid-tool-call = the screen text is stale. Seed a per-episode
            // FLOOR with the agent's latest hook seq at onset (or_insert never
            // overwrites → survives the detect→clear→re-detect flap; removed only on a
            // genuine screen exit); a fresh ACTIVE hook whose seq is STRICTLY newer
            // than the floor is a third recovery signal. ADD-ONLY — composes with
            // recovered; claude-only; a non-claude / missing / stale /
            // pre-onset hook falls through to the unchanged screen-driven path.
            // r6 finding-2: key the floor by the STABLE InstanceId, not name, so a
            // same-name replacement gets a fresh floor (the hook STORE is still
            // name-keyed, so `latest_hook_seq(name)` correctly seeds from this
            // instance's own hooks).
            if state == AgentState::ServerRateLimit {
                srl_floor
                    .entry(handle.id)
                    .or_insert_with(|| crate::daemon::hook_shadow::latest_hook_seq(name));
            } else {
                srl_floor.remove(&handle.id);
            }
            let is_claude = crate::backend::Backend::parse_str(handle.backend_command.as_str())
                .has_state_hooks();
            let hook_active_seq = if is_claude {
                crate::daemon::hook_shadow::fresh_active_hook_seq(name)
            } else {
                None
            };
            let hook_recovered = hook_recovered_for_srl(
                is_claude,
                hook_active_seq,
                srl_floor.get(&handle.id).copied(),
            );
            if hook_recovered {
                // FORWARD-PROGRESS (r6 finding-1): advance the floor to the consumed
                // hook so a LATER genuine episode — screen still sticky-SRL but the
                // agent now truly stuck (no NEWER hook) — re-arms the retry instead of
                // being permanently masked by this episode's stale recovery hook.
                if let Some(seq) = hook_active_seq {
                    srl_floor.insert(handle.id, seq);
                }
            }

            // #2466: the WORK-TURN 529 looser-signal arm (see `work_turn_throttle_arm`). When the
            // strict `state==ServerRateLimit` did NOT latch (the #1769 positional defeat → screen
            // reads Idle) but the watchdog's gate-free classify still flagged a throttle
            // (`blocked_rl`) corroborated by a screen throttle token, latch the SAME retry track so
            // a work-turn 529 auto-retries instead of hanging. Threaded as an `|| loose_arm` into
            // the arm branch below so the Idle-clear branch (`clears_server_rate_limit_retry`) only
            // fires when this is FALSE — arm and clear stay synchronized on one signal (no same-tick
            // fight). Single-ownership vs the future 529-recovery is left to a PRODUCTION signal
            // (the #2547-removed measure-only recovery_shadow shadow never counted); see
            // `work_turn_throttle_arm`.
            let loose_arm = work_turn_throttle_arm(blocked_rl, has_throttle_hint, recovered);

            // ── #1713 root-fix: ServerRateLimit retry — DECIDE with fresh state ──
            // The "should we inject this tick" decision lives HERE, under the lock,
            // gated on the agent being FRESHLY observed in ServerRateLimit — not on a
            // stale persisted timer. The track still persists across ticks to carry
            // the tiered backoff (retry_count / next_retry_at / exhausted); Phase 2
            // only EXECUTES the lock-free PTY inject for the names decided here. So a
            // track can never blind-fire `continue` into a non-error state (e.g. a
            // PermissionPrompt the agent reached after the throttle cleared).
            if state == AgentState::ServerRateLimit && (recovered || hook_recovered) {
                // #ratelimit-recovery: still latched ServerRateLimit (the stale
                // "Server is temporarily limiting" line re-matches in the tail and
                // working_state_below can't see a marker BELOW the most-recent error
                // line — #1769's positional defeat), BUT the agent recovered. Two
                // signals, EITHER suffices:
                //   • `recovered` — productive output within RECOVERY_SILENCE
                //     (heuristic; `last_productive_output` is position-independent,
                //     breaking the Thinking↔ServerRateLimit flicker). MISSES a pure
                //     fast TEXT reply that never stamped a behaviour marker — that
                //     gap is now covered by `hook_recovered` below (#26795-3
                //     removed the earlier `self_cleared` agent-driven signal, which
                //     2746/2746 telemetry proved agents never set).
                //   • `hook_recovered` (#t-26795) — a fresh post-onset claude hook
                //     proves the agent is mid-tool-call (see the SRL hook-override
                //     block above): ground-truth liveness that closes the pure-text
                //     gap the heuristic alone left. claude-only, ADD-ONLY.
                // Either way: clear the track and do NOT inject. A genuinely-stuck
                // agent produces nothing AND fires no fresh hook → the inject fires.
                if retry_tracks.remove(name).is_some() {
                    tracing::info!(
                        agent = %name,
                        productive_silent_secs = productive_silence.as_secs(),
                        recovered_via = if recovered {
                            "productive_output"
                        } else {
                            "hook_active" // #t-26795: fresh post-onset claude hook
                        },
                        "ServerRateLimit retry cleared — agent recovered"
                    );
                }
            } else if state == AgentState::ServerRateLimit || loose_arm {
                let track = retry_tracks.entry(name.to_string()).or_insert_with(|| {
                    let delay = Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
                    tracing::info!(
                        agent = %name,
                        delay_secs = delay.as_secs(),
                        // #2466: distinguish the strict screen-classified SRL from the work-turn
                        // 529 caught by the looser blocked_reason+throttle_hint signal.
                        trigger = if state == AgentState::ServerRateLimit {
                            "server_rate_limit"
                        } else {
                            "work_turn_throttle"
                        },
                        "rate-limit retry scheduled (Phase A)"
                    );
                    RateLimitRetry {
                        retry_count: 0,
                        next_retry_at: now + delay,
                        exhausted: false,
                        inject_failures: 0,
                        abort_pending: false,
                    }
                });
                // #1946: a fresh ServerRateLimit observation while an abort-recovery
                // was pending = the throttle re-latched on the detection side. The
                // normal fresh-SRL retry path resumes ownership; the after-abort
                // Idle-inject stands down (same track, same budget — single owner).
                if track.abort_pending {
                    track.abort_pending = false;
                    tracing::info!(
                        agent = %name,
                        retry_count = track.retry_count,
                        "#1946: ServerRateLimit re-latched — abort-recovery stands down, normal retry ownership resumes"
                    );
                }
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
                // #1713: Idle = genuine terminal recovery → cross-episode reset…
                // EXCEPT the #1946 abort shape below.
                match retry_tracks.get_mut(name) {
                    // ── #1946 (closes #1808 Flaw 1, production-evidenced by the
                    // probe fire 2026-06-10 08:59) ──
                    // An abort-to-Idle with an in-flight retry and NO recent
                    // productive output is NOT recovery: the backend aborted the
                    // retry attempt back to its prompt. Post-#1936 the stale-sig
                    // re-latch is suppressed too, so detection will NOT re-create
                    // a track — dropping it here left NOBODY owning recovery (the
                    // agent froze until manual rescue). RETAIN ownership: mark
                    // abort-pending and keep walking the SAME tiered backoff
                    // budget via the Idle-state after-abort inject. `recovered`
                    // cleanly separates this from genuine recovery — failed-retry
                    // spinners do not count as productive output (probe evidence:
                    // productive_silent_secs=600 across 4 retries).
                    Some(track)
                        if track.retry_count > 0
                            && !track.exhausted
                            && !recovered
                            && has_throttle_hint =>
                    {
                        if !track.abort_pending {
                            track.abort_pending = true;
                            let idx = (track.retry_count as usize)
                                .min(SERVER_RATE_LIMIT_BACKOFF.len() - 1);
                            track.next_retry_at =
                                now + Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[idx]);
                            tracing::warn!(
                                agent = %name,
                                tag = "#1946-abort-retain",
                                retry_count = track.retry_count,
                                next_retry_secs = SERVER_RATE_LIMIT_BACKOFF[idx],
                                productive_silent_secs = productive_silence.as_secs(),
                                "abort-to-Idle with in-flight ServerRateLimit retry — retaining ownership, delayed retry scheduled (was: track cleared → freeze)"
                            );
                        } else {
                            // After-abort continuation: the ONLY Idle-state inject,
                            // and deliberately narrow — abort_pending is set solely
                            // in the probe-confirmed shape above, `!recovered`
                            // (guard on this arm) keeps a genuinely-working agent
                            // out, and the tiered schedule + MIN_INTERVAL + the
                            // shared 12-retry budget all still apply (#1713's
                            // anti-blind-fire intent holds: never into
                            // PermissionPrompt/working states).
                            let min_interval_ok = last_continue_inject.get(name).is_none_or(|t| {
                                now.duration_since(*t) >= CONTINUE_INJECT_MIN_INTERVAL
                            });
                            if now >= track.next_retry_at && min_interval_ok {
                                srl_to_inject.push(name.to_string());
                            }
                        }
                    }
                    Some(_) => {
                        // Genuine recovery (recent productive output), a pre-retry
                        // track (retry_count == 0), or an exhausted track reaching
                        // Idle — cross-episode reset so a later episode starts
                        // fresh at Phase A.
                        if let Some(cleared) = retry_tracks.remove(name) {
                            if cleared.retry_count > 0 && !recovered {
                                tracing::warn!(
                                    agent = %name,
                                    tag = "#1946-abort-clear-no-evidence",
                                    retry_count = cleared.retry_count,
                                    next_retry_at = ?cleared.next_retry_at,
                                    secs_until_retry = cleared.next_retry_at.saturating_duration_since(now).as_secs(),
                                    productive_silent_secs = productive_silence.as_secs(),
                                    "abort-to-Idle cleared — no throttle error visible on screen (likely scrolled off or recovered)"
                                );
                            } else {
                                tracing::info!(
                                    agent = %name,
                                    ?state,
                                    retry_count = cleared.retry_count,
                                    after_abort = cleared.abort_pending,
                                    recovered,
                                    productive_silent_secs = productive_silence.as_secs(),
                                    "ServerRateLimit retry cleared — agent recovered (Idle)"
                                );
                            }
                        }
                    }
                    None => {}
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
                // #1741 boot-grace: ALSO skip QUEUING the nudge during the post-boot
                // window. Because the `apierror_episodes` insert only happens in
                // Phase 2b when a queued nudge actually fires, NOT queuing here
                // leaves the episode unmarked → a still-ApiError agent gets a fresh
                // nudge once the grace ends. Detection (`state == ApiError`), the
                // #1742-F4 cap, and the re-arm below are all unaffected; SRL retry
                // above is untouched.
                if !crate::daemon::per_tick::in_boot_grace(loop_started_at)
                    && !apierror_episodes.contains(name)
                    && !capped
                {
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
    // #t-26795 (r6 finding-2): prune the UUID-keyed floor for instances no longer in
    // the registry. Keying by InstanceId (not name) is what actually closes the
    // same-name-replacement inherit; this retain just bounds the map across churn.
    srl_floor.retain(|id, _| active_ids.contains(id));

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
            // #1744-M6: deliver to every registered channel (multi-channel-safe).
            crate::channel::notify_all_escalation_channels(
                name,
                NotifySeverity::Error,
                &msg,
                false,
            );
            continue;
        }
        // #1696: escalation-phase observability (so a long outage is visible in the log).
        if retry.retry_count == RETRY_PHASE_B_START {
            tracing::info!(agent = %name, "ServerRateLimit: entering retry Phase B (minute-scale backoff)");
        } else if retry.retry_count == RETRY_PHASE_C_START {
            tracing::info!(agent = %name, "ServerRateLimit: entering retry Phase C (sustained 10-min retry)");
        }

        // #1946: tag the after-abort continuation distinctly so a pane/log reader
        // can tell which mechanism injected (and the episode leaves a trace).
        let auto_kind = if retry.abort_pending {
            "ratelimit-retry-after-abort"
        } else {
            "ratelimit-retry"
        };
        match inject_ratelimit_retry_gated(home, registry, name, auto_kind) {
            InjectOutcome::Injected => {
                retry.inject_failures = 0; // #1742: a success clears the failure streak
                last_continue_inject.insert(name.clone(), Instant::now());
                tracing::info!(
                    agent = %name,
                    retry = retry.retry_count,
                    after_abort = retry.abort_pending,
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
                        let msg = format!(
                            "⚠️ {name} ServerRateLimit auto-retry inject 連續失敗 {} 次、已放棄 — agent 可能 unreachable,需人工介入(檢查 pane / 重啟 / 重新指派)。",
                            retry.inject_failures
                        );
                        // #1742: same Error severity + same Sleep-penetrating gate as
                        // the budget-exhausted alert (#1595 Step 1) — a stuck agent
                        // whose auto-recovery cannot even deliver `continue` is the
                        // same P0 that must wake a sleeping operator.
                        // #1744-M6: every registered channel (multi-channel-safe).
                        crate::channel::notify_all_escalation_channels(
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
        if inject_continue_gated(home, registry, &name, "apierror-nudge") == InjectOutcome::Injected
        {
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

/// #2033: gate the "recovered from blocked state" Telegram notice on the
/// actionable-or-silent principle (#2008). It is operator-useful ONLY when both:
/// (a) the operator was actually told about the block (a Stall notice went out
/// this episode), and (b) the block lasted past [`RECOVERY_NOTICE_MIN_BLOCK`], so
/// they might be mid-reaction. A never-notified or self-resolving block (e.g. the
/// #2020 false AwaitingOperator the operator flagged) fails this → log-only.
fn recovery_notice_is_actionable(episode: crate::state::RecoveryEpisode) -> bool {
    episode.notice_sent && episode.block_duration >= RECOVERY_NOTICE_MIN_BLOCK
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
mod review_repro_daemon_supervisor;
#[cfg(test)]
mod tests;
