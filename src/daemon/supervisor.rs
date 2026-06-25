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
/// #1894: null-unlock_at usage_limit re-notify window. When the pane had no
/// parseable "try again at <time>" at detection (e.g. it was showing
/// poll-reminders), `parse_unlock_at` records `unlock_at: null` and the dedup
/// falls back to a TIMESTAMP cooldown. #1861/#1864 used the 60s `NOTIFY_COOLDOWN`
/// here, which restarts hours apart trivially exceed → the operator was re-paged
/// on every boot for the SAME ongoing limit. A real usage-limit episode lasts
/// hours-to-days, so suppress for a long window: a multi-day limit re-notifies at
/// most ~once/day instead of once/restart. Parseable-unlock_at suppression
/// (#1864) is unchanged, and a missing/corrupt record still FAILS OPEN (notify).
const NULL_UNLOCK_NOTIFY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

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
/// #2232: ServerRateLimit retry payload — `continue` plus a one-line instruction
/// telling a now-awake agent to self-clear its rate-limit block (MCP
/// `clear_blocked_reason` reason=rate_limit) so the daemon stops auto-retrying.
/// That agent-initiated clear is the ground-truth recovery signal the supervisor
/// latches on; LLM compliance isn't guaranteed, so a missed call gracefully
/// degrades to the `recovered_within` / state-exit / 12-cap heuristics. ASCII,
/// SINGLE line + one trailing "\n" (no embedded newline that would submit early).
/// Kept SEPARATE from the shared `CONTINUE_RETRY_PAYLOAD` so the apierror-nudge
/// wording is unchanged and the #1680 source-guard literal stays intact — same
/// split rationale as `inject_channel_reply_missing_gated`.
const RATELIMIT_RETRY_PAYLOAD: &[u8] = b"continue (if you can act on this you have recovered from a rate limit -- call the agend-terminal health MCP action clear_blocked_reason with reason=rate_limit to stop these auto-retries)\n";

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

/// #1861: persisted usage_limit notify dedup record (one per member). A daemon
/// restart wipes the in-mem `NotifyTrack` cooldown, so without persistence the
/// operator is re-notified of the SAME usage limit on every restart (the backend
/// boots `Starting` → re-detects UsageLimit).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct UsageNotifyRecord {
    /// Parsed "HH:MM" unlock string at notify time (None if the pane had no
    /// parseable reset time).
    unlock_at: Option<String>,
    /// When we notified (rfc3339 UTC) — anchors the unlock deadline + the
    /// null-unlock fallback cooldown.
    notified_at: String,
}

fn usage_limit_notify_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("usage_limit_notify.json")
}

/// #1906: drop one agent's usage-limit notify-dedup entry on delete, so a
/// same-name redeploy does NOT inherit stale suppression and silently eat its
/// first real usage_limit notify (until the #1894/#1895 stale-unlock window).
/// Mirrors `escalation_persist::remove` (#1680 stale-state-on-redeploy class).
/// Locked RMW via `with_json_state`; no-op when the store is absent.
pub(crate) fn remove_usage_limit_notify(home: &std::path::Path, name: &str) {
    let path = usage_limit_notify_path(home);
    if !path.exists() {
        return;
    }
    let _ = crate::store::with_json_state::<
        std::collections::HashMap<String, UsageNotifyRecord>,
        _,
        _,
    >(&path, |map| {
        map.remove(name);
    });
}

/// #1906: does the usage-limit notify-dedup store still hold `name`? For the
/// `full_delete_instance` residual audit (this store was a teardown blind spot).
pub(crate) fn usage_limit_notify_has(home: &std::path::Path, name: &str) -> bool {
    std::fs::read_to_string(usage_limit_notify_path(home))
        .ok()
        .and_then(|s| {
            serde_json::from_str::<std::collections::HashMap<String, UsageNotifyRecord>>(&s).ok()
        })
        .is_some_and(|m| m.contains_key(name))
}

/// The UTC instant an `HH:MM` unlock window elapses, anchored to `notified_at`
/// (the next occurrence of HH:MM at-or-after the notify, treated as UTC since the
/// pane renders e.g. "Resets at 15:14 UTC"). `None` if unparseable.
fn unlock_deadline(
    hhmm: &str,
    notified_at: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let (h, m) = hhmm.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    let naive = notified_at.date_naive().and_hms_opt(h, m, 0)?;
    let candidate = chrono::Utc.from_utc_datetime(&naive);
    Some(if candidate >= notified_at {
        candidate
    } else {
        candidate + chrono::Duration::days(1)
    })
}

/// #2127 Phase 1: time until `name`'s usage-limit window unlocks, per the
/// persisted notify record. `None` when there is no record, no parseable
/// `unlock_at`, or an unparseable `notified_at` — the reclaim caller then falls
/// back to a conservative long default (a missing reset time is treated as a long
/// block). A past deadline clamps to `Duration::ZERO`. Lock-free read; reuses the
/// same record + `unlock_deadline` math as the notify-suppression path so the two
/// agree on what "this window" means.
pub(crate) fn usage_limit_remaining(
    home: &std::path::Path,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let map: std::collections::HashMap<String, UsageNotifyRecord> =
        std::fs::read_to_string(usage_limit_notify_path(home))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())?;
    let rec = map.get(name)?;
    let unlock_at = rec.unlock_at.as_deref()?;
    let notified_at = chrono::DateTime::parse_from_rfc3339(&rec.notified_at)
        .ok()?
        .with_timezone(&chrono::Utc);
    let deadline = unlock_deadline(unlock_at, notified_at)?;
    Some(
        (deadline - now)
            .to_std()
            .unwrap_or(std::time::Duration::ZERO),
    )
}

/// True ⇒ suppress this usage_limit notify (already notified for the same still-
/// open window). Lock-free FAIL-OPEN read: a missing/corrupt record ⇒ NOT
/// suppressed (notify), so a real new limit is never silently swallowed.
fn usage_limit_notify_suppressed(
    home: &std::path::Path,
    name: &str,
    unlock_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let map: std::collections::HashMap<String, UsageNotifyRecord> =
        std::fs::read_to_string(usage_limit_notify_path(home))
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default();
    let Some(rec) = map.get(name) else {
        return false;
    };
    let Ok(notified_at) = chrono::DateTime::parse_from_rfc3339(&rec.notified_at) else {
        return false;
    };
    let notified_at = notified_at.with_timezone(&chrono::Utc);
    match unlock_at {
        // Different unlock window string ⇒ a NEW limit ⇒ notify.
        Some(u) if rec.unlock_at.as_deref() != Some(u) => false,
        // Same window: suppress until its deadline passes (then the limit reset →
        // notify again). Unparseable deadline ⇒ conservatively suppress (an
        // identical string is a strong same-window signal).
        Some(u) => unlock_deadline(u, notified_at).is_none_or(|deadline| now < deadline),
        // No parseable reset time ⇒ persisted-timestamp window (NOT the in-session
        // Instant cooldown a restart wipes). #1894: use the long
        // `NULL_UNLOCK_NOTIFY_WINDOW` (24h) instead of the 60s `NOTIFY_COOLDOWN`,
        // so restarts WITHIN the same ongoing usage-limit episode (which the
        // operator hit repeatedly) stay silent. A genuinely-new episode > the
        // window later re-notifies.
        None => {
            now.signed_duration_since(notified_at)
                < chrono::Duration::from_std(NULL_UNLOCK_NOTIFY_WINDOW)
                    .unwrap_or_else(|_| chrono::Duration::hours(24))
        }
    }
}

/// Persist that we notified `name` for `unlock_at` at `now` (locked RMW).
fn record_usage_limit_notified(
    home: &std::path::Path,
    name: &str,
    unlock_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) {
    let record = UsageNotifyRecord {
        unlock_at: unlock_at.map(String::from),
        notified_at: now.to_rfc3339(),
    };
    let _ = crate::store::with_json_state_or_create(
        &usage_limit_notify_path(home),
        std::collections::HashMap::<String, UsageNotifyRecord>::new,
        |map| {
            map.insert(name.to_string(), record);
        },
    );
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
    // #1744-M7: distinguish "teams config unreadable" (Err → can't determine the
    // orchestrator) from "loaded, no team for this member" (None). For the no-peer
    // AuthError P0 the unreadable case fails CLOSED — escalate to the operator
    // rather than silently dropping (we can't relay to an orchestrator we can't
    // identify, and AuthError is operator-only). Non-escalation states stay
    // dropped (we genuinely can't route them).
    let fleet = match crate::teams::try_load_fleet(home) {
        Ok(f) => f,
        Err(_) => {
            if self_orchestrator_escalates(new_state) {
                escalate_self_orch_autherror(name, now, tracks);
            }
            return false;
        }
    };
    let Some(team) = crate::teams::find_team_for_in(&fleet, name) else {
        return false;
    };
    let Some(ref orch) = team.orchestrator else {
        tracing::warn!(agent = %name, team = %team.name, "member-state-change: team has no orchestrator — notify dropped");
        return false;
    };
    if orch == name {
        // #1595 Step 2: the orchestrator IS the affected agent — no peer can relay
        // its inbox P0. For a state only the operator can resolve (AuthError: only
        // the operator can re-authenticate), escalate straight to the operator
        // via gated_notify(Error) — the same Sleep-penetrating path #1594 allows
        // through. Cooldown-stamped so a persistent AuthError escalates at most
        // once per NOTIFY_COOLDOWN, not every tick. Other states keep the D3
        // self-notify skip (transient / the agent reads its own inbox).
        // NOTE: Crashed/Hang are NOT live AgentStates via this hook (never assigned
        // to `state.current`); real crash/hang self-orchestrator escalation is a
        // follow-up (#1701) using the process-exit / HealthState::Hung paths (the
        // latter strong-gated for the known 348-FP).
        if self_orchestrator_escalates(new_state) {
            escalate_self_orch_autherror(name, now, tracks);
        }
        return false; // D3: still skip the inbox self-notify (no peer reads it)
    }
    let unlock_at = if new_state == crate::state::AgentState::UsageLimit {
        parse_unlock_at(pane_tail)
    } else {
        None
    };
    // #1861: usage_limit notify re-fired on EVERY daemon restart — the in-mem
    // `tracks` cooldown is Instant-based and wiped on restart, and the backend
    // boots `Starting` → re-detects UsageLimit → re-transitions. Persist the
    // "already notified" decision keyed (member, unlock_at) so a restart with the
    // SAME unlock window stays silent; re-notify only when unlock_at ADVANCES (new
    // limit) or has PASSED. Scoped to UsageLimit ONLY — other error-class notifies
    // keep the in-session cooldown unchanged.
    if new_state == crate::state::AgentState::UsageLimit
        && usage_limit_notify_suppressed(home, name, unlock_at.as_deref(), chrono::Utc::now())
    {
        // Stamp the in-mem track so same-session ticks short-circuit at the
        // cooldown gate above without re-reading the persisted record each tick.
        let track = tracks.entry(name.to_string()).or_insert(NotifyTrack {
            last_at: now,
            consecutive: 0,
        });
        track.last_at = now;
        return false;
    }
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
    // #1861: record the notify so a daemon restart with the SAME unlock window
    // stays silent (the in-mem track above is wiped on restart).
    if new_state == crate::state::AgentState::UsageLimit {
        record_usage_limit_notified(home, name, unlock_at.as_deref(), chrono::Utc::now());
    }
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
/// injects [`RATELIMIT_RETRY_PAYLOAD`] (`continue` + a self-clear instruction)
/// instead of the bare shared `CONTINUE_RETRY_PAYLOAD`. Kept separate so the
/// apierror-nudge keeps the plain payload and the #1680 source-guard literal on
/// the shared one stays intact (same split rationale as
/// [`inject_channel_reply_missing_gated`]). Same draft-gating (`force=false`) +
/// `[AGEND-AUTO kind=...]` tagging; returns the 3-state [`InjectOutcome`].
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
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        sequencing: None,
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
            let (state, recovered, self_cleared, has_throttle_hint, productive_silence) = {
                let mut core = handle.core.lock();
                // KEEP-RAW (#2465): the SRL retry arm reads raw core.state.current. claude hooks
                // never emit RateLimited (a StopFailure → ApiError, the API plane owns rate-limit),
                // so operated_state would be inert here; the true ApiError-as-rate-limit SRL fix is
                // tracked in #2466, out of this PR's scope.
                let state = core.state.current;
                let recovered = core.state.recovered_within(RECOVERY_SILENCE);
                // #2232: ground-truth recovery latch the agent set by self-clearing
                // its rate-limit block via the MCP `clear_blocked_reason` action.
                let self_cleared = core.health.rate_limit_self_cleared;
                let has_hint =
                    crate::state::screen_has_throttle_hint(&core.vterm.tail_lines(TAIL_LINES));
                let productive_silence = core.state.productive_silence();
                if state == AgentState::ServerRateLimit {
                    // #2232 D1(b): we are about to track/inject a rate-limit retry,
                    // so we ALREADY KNOW the agent is rate-limited — mark it blocked
                    // (only when not already RateLimit-latched, to avoid clobbering a
                    // watchdog-set `retry_after_secs`). This guarantees the agent's
                    // later self-clear (`clear_blocked_reason reason=rate_limit`)
                    // reliably matches and latches, making it a dependable
                    // ground-truth recovery signal rather than a tick-window
                    // best-effort. Skip once self-cleared so we never re-block an
                    // agent that already proved it recovered.
                    if !recovered
                        && !self_cleared
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
                } else {
                    // #2232: a genuine ServerRateLimit EXIT resets the latch so a
                    // FUTURE rate-limit episode re-arms the retry path
                    // (cross-episode), mirroring `clears_server_rate_limit_retry`.
                    core.health.rate_limit_self_cleared = false;
                }
                (state, recovered, self_cleared, has_hint, productive_silence)
            };

            // ── #t-26795 SRL hook-override (operator-reported sticky-screen flap) ──
            // A sticky screen-scraped ServerRateLimit while a FRESH claude hook proves
            // the agent is mid-tool-call = the screen text is stale. Seed a per-episode
            // FLOOR with the agent's latest hook seq at onset (or_insert never
            // overwrites → survives the detect→clear→re-detect flap; removed only on a
            // genuine screen exit); a fresh ACTIVE hook whose seq is STRICTLY newer
            // than the floor is a third recovery signal. ADD-ONLY — composes with
            // recovered/self_cleared; claude-only; a non-claude / missing / stale /
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

            // ── #1713 root-fix: ServerRateLimit retry — DECIDE with fresh state ──
            // The "should we inject this tick" decision lives HERE, under the lock,
            // gated on the agent being FRESHLY observed in ServerRateLimit — not on a
            // stale persisted timer. The track still persists across ticks to carry
            // the tiered backoff (retry_count / next_retry_at / exhausted); Phase 2
            // only EXECUTES the lock-free PTY inject for the names decided here. So a
            // track can never blind-fire `continue` into a non-error state (e.g. a
            // PermissionPrompt the agent reached after the throttle cleared).
            if state == AgentState::ServerRateLimit && (recovered || self_cleared || hook_recovered)
            {
                // #ratelimit-recovery: still latched ServerRateLimit (the stale
                // "Server is temporarily limiting" line re-matches in the tail and
                // working_state_below can't see a marker BELOW the most-recent error
                // line — #1769's positional defeat), BUT the agent recovered. Two
                // signals, EITHER suffices:
                //   • `recovered` — productive output within RECOVERY_SILENCE
                //     (heuristic; `last_productive_output` is position-independent,
                //     breaking the Thinking↔ServerRateLimit flicker). MISSES a pure
                //     fast TEXT reply that never stamped a behaviour marker (#2232).
                //   • `self_cleared` (#2232) — the agent itself called
                //     `clear_blocked_reason` on its rate-limit block: ground-truth
                //     proof it is awake and read the inject, backend-agnostic, zero
                //     false-positive for liveness. Closes the over-inject gap the
                //     heuristic alone left. The latch stays set (so no re-arm) until
                //     a genuine ServerRateLimit exit resets it (capture block above).
                // Either way: clear the track and do NOT inject. A genuinely-stuck
                // agent produces nothing AND can't call clear → the inject fires.
                if retry_tracks.remove(name).is_some() {
                    tracing::info!(
                        agent = %name,
                        productive_silent_secs = productive_silence.as_secs(),
                        recovered_via = if self_cleared {
                            "agent_self_clear"
                        } else if recovered {
                            "productive_output"
                        } else {
                            "hook_active" // #t-26795: fresh post-onset claude hook
                        },
                        "ServerRateLimit retry cleared — agent recovered"
                    );
                }
            } else if state == AgentState::ServerRateLimit {
                let track = retry_tracks.entry(name.to_string()).or_insert_with(|| {
                    let delay = Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]);
                    tracing::info!(agent = %name, delay_secs = delay.as_secs(), "ServerRateLimit detected, scheduling retry (Phase A)");
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
                // #t-81376 Phase-0 shadow: this Idle is the "gap arm" — a fast
                // 529→Idle the daemon treats as recovery and clears / never builds
                // a retry track. Record the failed-turn discriminator components +
                // hook-vs-raw layers so FP/FN can be measured. No-op unless
                // AGEND_RECOVERY_SHADOW=1; takes NO action on the clear below.
                // instrument-only: zero-behaviour shadow emit (D3 #2324) — no ?/return/exit/break/continue.
                {
                    let (had_retry_track, rc) = retry_tracks
                        .get(name)
                        .map(|t| (true, t.retry_count))
                        .unwrap_or((false, 0));
                    crate::daemon::recovery_shadow::record_recovery_shadow(
                        home,
                        &crate::daemon::recovery_shadow::GapObservation {
                            agent: name,
                            backend: handle.backend_command.as_str(),
                            recovered,
                            self_cleared,
                            has_throttle_hint,
                            had_retry_track,
                            retry_count: rc,
                            agent_state: state.display_name(),
                            productive_silent_secs: productive_silence.as_secs(),
                        },
                    );
                }
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
    // #t-81376 Phase-0 shadow: prune expectation/latch maps for churned agents
    // (no-op unless AGEND_RECOVERY_SHADOW=1). `()` → control-flow-inert.
    crate::daemon::recovery_shadow::retain_live(&|n| active_names.contains(n));

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
mod tests {
    use super::*;

    /// #2033: the recovery-notice gate — actionable iff the operator was told
    /// about the block AND it lasted past the threshold (actionable-or-silent).
    #[test]
    fn recovery_notice_gate_actionable_or_silent_2033() {
        use crate::state::RecoveryEpisode;
        let ep = |secs, notice_sent| RecoveryEpisode {
            block_duration: Duration::from_secs(secs),
            notice_sent,
        };
        // notified + long enough → fire
        assert!(recovery_notice_is_actionable(ep(60, true)));
        // notified but self-resolved fast → silent (the InteractivePrompt noise)
        assert!(!recovery_notice_is_actionable(ep(5, true)));
        // long but NEVER notified → silent (the #2020 false-AwaitingOperator class)
        assert!(!recovery_notice_is_actionable(ep(300, false)));
        // neither → silent
        assert!(!recovery_notice_is_actionable(ep(2, false)));
        // boundary: exactly the threshold is actionable (>=)
        assert!(recovery_notice_is_actionable(RecoveryEpisode {
            block_duration: RECOVERY_NOTICE_MIN_BLOCK,
            notice_sent: true,
        }));
    }

    // NOTE: `recovery_clears_retry_track` (+ its `fresh_retry` helper) was removed
    // here — it only asserted `HashMap` insert/remove semantics on a local map and
    // never exercised the production recovery path. The REAL recovery gate that
    // clears the retry track, `clears_server_rate_limit_retry`, is already covered
    // with real inputs by `clears_server_rate_limit_retry_covers_only_terminal_
    // recovery_1713` (Idle clears; every other state does not). The other clear
    // path (`ServerRateLimit && recovered` via productive-output) has no pure seam
    // without restructuring the registry-locked `process_error_recovery` hot loop,
    // which would not be a behavior-preserving extraction.

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
        assert!(!AgentState::Active.is_notify_error_class());
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
            reactions_from_transitions(&[tr(AgentState::Idle, AgentState::Active)]).is_empty(),
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        ));
    }

    /// #2020 live shape 2 (fixup-lead, 2026-06-11 20:09): a respawned agent
    /// that was injected work immediately never renders the clean
    /// ready-prompt — heuristic stays `Starting` — but it HAS rendered
    /// productive markers. The startup-stall arm must veto: demonstrably
    /// working ≠ stalled at a login prompt.
    #[test]
    fn starting_stall_vetoed_by_productive_output_2020() {
        assert!(!awaiting_escalation_allowed(
            crate::state::AgentState::Starting,
            Duration::from_secs(120),
            Some(crate::backend::Backend::ClaudeCode),
            "tail irrelevant for the Starting arm",
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
            true, // productive markers seen since this spawn
        ));
    }

    /// #2020 guard on the guard: with NO productive output since spawn the
    /// startup-stall fallback must still fire — a real login-prompt stall
    /// (the fallback's actual job) renders no tool chrome, and echoed
    /// injected text doesn't count (markers, not raw output).
    #[test]
    fn starting_stall_still_fires_without_productive_output_2020() {
        assert!(awaiting_escalation_allowed(
            crate::state::AgentState::Starting,
            Duration::from_secs(120),
            Some(crate::backend::Backend::ClaudeCode),
            "Please log in to continue",
            0,
            10_000,
            crate::fleet::IdleExpectation::Active,
            false,
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
            false,
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
            false,
        ));
    }

    #[test]
    fn awaiting_gate_non_prompt_state_never_escalates() {
        for s in [
            crate::state::AgentState::Idle,
            crate::state::AgentState::Active,
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
                    false,
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

    /// #1744-M7: when the teams config is UNREADABLE (exists but corrupt → the
    /// orchestrator can't be identified), a self-orch AuthError must STILL escalate
    /// to the operator (fail-closed) — we can't relay to a peer we can't find and
    /// AuthError is operator-only. A non-escalation state under the same unreadable
    /// config stays dropped (we genuinely can't route it).
    #[test]
    fn self_orch_autherror_fail_closed_on_unreadable_teams_1744_m7() {
        let home = std::env::temp_dir().join(format!("agend-1744m7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("inbox")).ok();
        // Corrupt (existing-but-invalid) fleet.yaml → try_load_fleet Err → Unknown.
        let _ = std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams: : : not valid [[[\n",
        );

        // AuthError → fail-closed escalation runs (stamps the cooldown track).
        let mut tracks = std::collections::HashMap::new();
        let sent = super::maybe_notify_member_state_change(
            &home,
            "solo",
            crate::state::AgentState::Idle,
            crate::state::AgentState::AuthError,
            "",
            &mut tracks,
        );
        assert!(!sent, "still not an inbox self-notify");
        assert_eq!(
            tracks.get("solo").map(|t| t.consecutive),
            Some(1),
            "#1744-M7: AuthError must escalate even when teams config is unreadable (fail-closed)"
        );

        // A non-escalation state under the same unreadable config → no escalation.
        let mut tracks2 = std::collections::HashMap::new();
        let sent2 = super::maybe_notify_member_state_change(
            &home,
            "solo",
            crate::state::AgentState::Idle,
            crate::state::AgentState::RateLimit,
            "",
            &mut tracks2,
        );
        assert!(!sent2);
        assert!(
            !tracks2.contains_key("solo"),
            "#1744-M7: a non-AuthError state under an unreadable config must NOT escalate"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #1861 §3.9: a usage_limit notify must NOT re-fire on daemon restart (fresh
    /// in-mem tracks) while the SAME unlock window is still open; a NEW unlock
    /// window (different reset time) must re-notify. Drives the real production
    /// entry `maybe_notify_member_state_change`.
    #[test]
    fn usage_limit_notify_not_refired_across_restart_1861() {
        let home = tmp_home("1861-restart");
        std::fs::create_dir_all(home.join("inbox")).ok();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [dev, lead]\n    orchestrator: lead\n",
        )
        .expect("seed fleet");
        // A parseable reset time well in the future → deadline-not-passed is
        // wall-clock-robust (avoids a flake if "now" happens to be past a fixed HH:MM).
        let future = (chrono::Utc::now() + chrono::Duration::hours(3))
            .format("%H:%M")
            .to_string();
        let pane = format!("Usage limit reached. Resets at {future} UTC");

        // First detection → notifies + persists the (member, unlock_at) record.
        let mut tracks = std::collections::HashMap::new();
        let sent1 = super::maybe_notify_member_state_change(
            &home,
            "dev",
            crate::state::AgentState::Idle,
            crate::state::AgentState::UsageLimit,
            &pane,
            &mut tracks,
        );
        assert!(
            sent1,
            "first usage_limit detection notifies the orchestrator"
        );

        // Simulate daemon RESTART: fresh in-mem tracks; the persisted record stays.
        let mut tracks_after_restart = std::collections::HashMap::new();
        let sent2 = super::maybe_notify_member_state_change(
            &home,
            "dev",
            crate::state::AgentState::Idle,
            crate::state::AgentState::UsageLimit,
            &pane,
            &mut tracks_after_restart,
        );
        assert!(
            !sent2,
            "#1861: the same unlock window after a restart must NOT re-notify"
        );

        // A NEW limit (different reset time) DOES notify, even after a restart.
        let later = (chrono::Utc::now() + chrono::Duration::hours(5))
            .format("%H:%M")
            .to_string();
        let pane2 = format!("Usage limit reached. Resets at {later} UTC");
        let mut tracks3 = std::collections::HashMap::new();
        let sent3 = super::maybe_notify_member_state_change(
            &home,
            "dev",
            crate::state::AgentState::Idle,
            crate::state::AgentState::UsageLimit,
            &pane2,
            &mut tracks3,
        );
        assert!(
            sent3,
            "#1861: a new unlock window (different reset time) must re-notify"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1861 §3.9 (helper, deterministic `now`): same unlock_at before its
    /// deadline → suppress; after the deadline (limit reset) → re-notify;
    /// different unlock_at (new limit) → re-notify; no record → re-notify.
    #[test]
    fn usage_limit_notify_suppressed_logic_1861() {
        let home = tmp_home("1861-helper");
        std::fs::create_dir_all(&home).ok();
        std::fs::write(
            super::usage_limit_notify_path(&home),
            r#"{"dev":{"unlock_at":"15:14","notified_at":"2026-06-09T14:00:00+00:00"}}"#,
        )
        .expect("seed record");
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .expect("valid rfc3339")
                .with_timezone(&chrono::Utc)
        };
        assert!(
            super::usage_limit_notify_suppressed(
                &home,
                "dev",
                Some("15:14"),
                at("2026-06-09T14:30:00+00:00")
            ),
            "same unlock_at, before the 15:14 deadline → suppress"
        );
        assert!(
            !super::usage_limit_notify_suppressed(
                &home,
                "dev",
                Some("15:14"),
                at("2026-06-09T16:00:00+00:00")
            ),
            "same unlock_at, past the deadline (limit reset) → re-notify"
        );
        assert!(
            !super::usage_limit_notify_suppressed(
                &home,
                "dev",
                Some("18:00"),
                at("2026-06-09T14:30:00+00:00")
            ),
            "different unlock_at (new limit) → re-notify"
        );
        assert!(
            !super::usage_limit_notify_suppressed(
                &home,
                "other",
                Some("15:14"),
                at("2026-06-09T14:30:00+00:00")
            ),
            "no record for this member → re-notify"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1894 §3.9 (helper): an UNPARSEABLE unlock time falls back to the long
    /// `NULL_UNLOCK_NOTIFY_WINDOW` (24h), NOT the 60s cooldown — so restarts hours
    /// apart WITHIN the same ongoing usage-limit episode stay silent (the operator
    /// pain). A genuinely-new episode past the window re-notifies. Regression-
    /// proof: revert to `NOTIFY_COOLDOWN` and the 5h-restart assertion flips to
    /// re-notify (the #1861/#1864 residual).
    #[test]
    fn usage_limit_null_unlock_long_window_1894() {
        let home = tmp_home("1894-null");
        std::fs::create_dir_all(&home).ok();
        std::fs::write(
            super::usage_limit_notify_path(&home),
            r#"{"dev":{"unlock_at":null,"notified_at":"2026-06-09T14:00:00+00:00"}}"#,
        )
        .expect("seed record");
        let at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .expect("valid rfc3339")
                .with_timezone(&chrono::Utc)
        };
        assert!(
            super::usage_limit_notify_suppressed(
                &home,
                "dev",
                None,
                at("2026-06-09T14:00:30+00:00")
            ),
            "null unlock_at, +30s → suppress"
        );
        // The fix: a restart HOURS later (same ongoing limit) is still suppressed.
        assert!(
            super::usage_limit_notify_suppressed(&home, "dev", None, at("2026-06-09T19:00:00+00:00")),
            "#1894: null unlock_at, +5h restart (same episode) → still suppress (was re-notify at 60s)"
        );
        // Past the 24h window (a genuinely-new episode) → re-notify.
        assert!(
            !super::usage_limit_notify_suppressed(
                &home,
                "dev",
                None,
                at("2026-06-10T15:00:00+00:00")
            ),
            "#1894: null unlock_at, +25h (past window) → re-notify"
        );
        // Missing record still FAILS OPEN (notify) — never silently swallowed.
        assert!(
            !super::usage_limit_notify_suppressed(
                &home,
                "ghost",
                None,
                at("2026-06-09T14:00:30+00:00")
            ),
            "no record → FAIL-OPEN re-notify (#1864 contract preserved)"
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
            abort_pending: false,
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
        mock_agent_handle_with_size(name, state, 10, 80)
    }

    fn mock_agent_handle_with_size(
        name: &str,
        state: crate::state::AgentState,
        rows: u16,
        cols: u16,
    ) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows,
                cols,
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
            vterm: crate::vterm::VTerm::with_pty_writer(cols, rows, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: crate::state::StateTracker::new(None),
            health: crate::health::HealthTracker::new(),
            api_activity: crate::agent::ApiActivity::default(),
            observed_status: None,
        }));
        core.lock().state.current = state;
        // Direct `.current` write bypasses record_set, so sync the lock-free mirror.
        let published_state = core.lock().state.published_handle();
        let published_observed = core.lock().state.published_observed_handle();
        published_state.store(state as u8, std::sync::atomic::Ordering::Relaxed);
        let handle = crate::agent::AgentHandle {
            id: crate::types::InstanceId::default(),
            name: name.to_string().into(),
            backend_command: "claude".to_string(),
            pty_writer,
            pty_master: Arc::new(parking_lot::Mutex::new(pair.master)),
            core,
            published_state,
            published_observed,
            child: Arc::new(parking_lot::Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            spawn_mode: crate::backend::SpawnMode::Fresh,
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
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            tracks.contains_key("test-agent"),
            "phase 1 must detect ServerRateLimit and insert retry track"
        );
        assert_eq!(tracks["test-agent"].retry_count, 0);
        assert!(!tracks["test-agent"].exhausted);
        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────────── #t-26795 SRL hook-override ───────────────────

    /// PURE truth-table + FORWARD-PROGRESS (test ②) for the hook→recovery decision:
    /// a hook seq STRICTLY greater than the floor recovers; a seq equal to (consumed)
    /// or below (pre-onset) the floor does not — so once the floor advances onto a
    /// hook, that SAME hook no longer overrides, only a NEWER one does.
    #[test]
    fn hook_recovered_for_srl_truth_table() {
        let floor = 1000u64;
        assert!(
            super::hook_recovered_for_srl(true, Some(1500), Some(floor)),
            "claude + a fresh active hook NEWER than the floor → recovered (forward progress)"
        );
        assert!(
            !super::hook_recovered_for_srl(true, Some(500), Some(floor)),
            "a hook seq BELOW the floor (pre-onset / prior turn) → genuine new SRL not masked"
        );
        assert!(
            !super::hook_recovered_for_srl(true, Some(1000), Some(floor)),
            "a hook seq EQUAL to the floor (already consumed — no newer hook) → not recovered → re-arms"
        );
        assert!(
            !super::hook_recovered_for_srl(true, None, Some(floor)),
            "no fresh ACTIVE hook (idle/stale/absent) → not recovered"
        );
        assert!(
            !super::hook_recovered_for_srl(true, Some(1500), None),
            "no floor (agent not in SRL) → not recovered"
        );
        assert!(
            !super::hook_recovered_for_srl(false, Some(1500), Some(floor)),
            "non-claude backend → never (unaffected)"
        );
    }

    /// FLAP REGRESSION (operator's exact symptom, #t-26795). An agent latched on a
    /// STICKY screen `ServerRateLimit` with `recovered`=false (no productive output
    /// this instant) but ALIVE — firing a NEW tool-call hook every tick. Each fresh
    /// hook seq exceeds the floor (forward progress) → the retry track stays CLEARED
    /// across re-detect ticks (no re-arm = the `continue`-spam flap killed) and the
    /// floor advances as each hook is consumed. NEUTER: drop `|| hook_recovered` from
    /// the clear gate → a recovered=false tick re-arms → this RED.
    #[test]
    #[serial_test::serial]
    fn srl_hook_override_kills_flap() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-hook-flap");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
        let name = "srl-flap-agent";
        let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
        let id = handle.id;
        registry.lock().insert(handle.id, handle);
        // Onset baseline: a pre-SRL hook pins the floor BELOW the recovery hooks.
        crate::daemon::hook_shadow::record_event(name, "Stop", None); // idle baseline
        let floor = crate::daemon::hook_shadow::latest_hook_seq(name);
        srl_floor.insert(id, floor);
        // The agent is actually ALIVE (false sticky SRL): it fires a NEW tool-call hook
        // each tick → each seq > floor → forward progress → the retry stays cleared.
        for _ in 0..3 {
            crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
            super::process_error_recovery(
                &home,
                &registry,
                &mut tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                &mut srl_floor,
                past_boot_grace(),
            );
            assert!(
                !tracks.contains_key(name),
                "a fresh post-floor claude hook each tick keeps the SRL retry cleared — the continue-spam flap is killed"
            );
        }
        assert!(
            srl_floor
                .get(&id)
                .copied()
                .expect("floor present after override")
                > floor,
            "the floor ADVANCES to the latest consumed hook seq (forward progress)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// FORWARD-PROGRESS (test ①, #t-26795 r6 finding-1): the multi-episode case the
    /// stable first-onset design missed, driven end-to-end through the code. The
    /// screen STAYS sticky-SRL the whole time (never emits a non-SRL tick → the floor
    /// is never reset). (a) onset: an idle baseline seeds the floor, no active hook →
    /// the retry ARMS. (b) episode A: a fresh tool-call hook (seq > floor) overrides
    /// AND ADVANCES the floor onto it → the retry clears. (c) episode B: the agent is
    /// now genuinely stuck — NO newer hook — so that SAME hook's seq == the advanced
    /// floor → no override → the retry must RE-ARM. NEUTER: drop the floor-advance
    /// (revert to the stable first-onset design) → the still-fresh episode-A hook
    /// stays seq > the un-advanced floor → it permanently re-masks B → no re-arm → RED.
    #[test]
    #[serial_test::serial]
    fn srl_forward_progress_rearms_genuine_episode_b() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-fwd-progress");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
        let name = "srl-fwd-agent";
        let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
        registry.lock().insert(handle.id, handle);
        let pe = |tracks: &mut HashMap<String, RateLimitRetry>,
                  srl_floor: &mut HashMap<crate::types::InstanceId, u64>| {
            super::process_error_recovery(
                &home,
                &registry,
                tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                srl_floor,
                past_boot_grace(),
            );
        };
        // (a) onset: an idle baseline seeds the floor; no active hook → the retry arms.
        crate::daemon::hook_shadow::record_event(name, "Stop", None);
        pe(&mut tracks, &mut srl_floor);
        assert!(
            tracks.contains_key(name),
            "onset with no active hook arms the retry"
        );
        // (b) episode A: a fresh tool-call hook NEWER than the floor overrides AND the
        // production code advances the floor onto it → the retry clears.
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        pe(&mut tracks, &mut srl_floor);
        assert!(
            !tracks.contains_key(name),
            "episode A: a fresh post-floor hook overrides — the retry clears"
        );
        // (c) episode B: agent genuinely stuck — NO newer hook. The same episode-A hook
        // is still Fresh(ToolUse) but its seq == the advanced floor → no override.
        pe(&mut tracks, &mut srl_floor);
        assert!(
            tracks.contains_key(name),
            "a genuine episode B (no hook newer than the CONSUMED floor) must re-arm the retry — forward progress, not permanent mask"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// CHURN PRUNE (#t-26795 r6 finding-2): an instance's SRL floor is dropped once it
    /// leaves the registry, so the UUID-keyed map stays bounded across agent churn.
    /// NEUTER: drop the `srl_floor.retain(...)` churn-prune → RED.
    #[test]
    #[serial_test::serial]
    fn srl_floor_pruned_on_agent_churn() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-floor-churn");
        let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
        // A prior instance left a floor seq behind; its uuid is NO LONGER in the
        // registry (deleted / restarted).
        let stale_id = crate::types::InstanceId::new();
        srl_floor.insert(stale_id, 1);
        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut srl_floor,
            past_boot_grace(),
        );
        assert!(
            !srl_floor.contains_key(&stale_id),
            "a churned-out instance's SRL floor must be pruned to bound the map across churn"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// UUID-KEY (test ③, #t-26795 r6 finding-2): the floor must key on the STABLE
    /// `InstanceId`, NOT the agent name — else a same-name handle SWAPPED between two
    /// consecutive ticks (delete/recreate/restart, with NO intermediate absent-name
    /// pass so the name-keyed retain never fires) lets the new instance INHERIT the old
    /// one's advanced floor and its genuine first SRL is wrongly overridden. Drives the
    /// old instance to advance its floor, swaps in a new same-name handle (new uuid)
    /// that emits a pre-onset hook (global seq > the old floor), and asserts the new
    /// instance's genuine SRL RE-ARMS. NEUTER: key the floor by name → the new instance
    /// inherits the old floor → its hook seq > inherited floor → override → no arm → RED.
    #[test]
    #[serial_test::serial]
    fn srl_floor_keyed_by_instance_id_survives_same_name_swap() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-floor-swap");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
        let name = "swapped-agent";
        let pe = |tracks: &mut HashMap<String, RateLimitRetry>,
                  srl_floor: &mut HashMap<crate::types::InstanceId, u64>| {
            super::process_error_recovery(
                &home,
                &registry,
                tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                srl_floor,
                past_boot_grace(),
            );
        };
        // OLD instance: onset baseline → then a fresh hook overrides + advances its
        // floor (so the old floor is LOW relative to later global seqs).
        let (old, _r1) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
        let old_id = old.id;
        registry.lock().insert(old_id, old);
        crate::daemon::hook_shadow::record_event(name, "Stop", None);
        pe(&mut tracks, &mut srl_floor);
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        pe(&mut tracks, &mut srl_floor);
        assert!(!tracks.contains_key(name), "old instance recovered");
        // SWAP: same NAME, NEW uuid — delete old (forget its hooks) + insert new, with
        // NO intermediate process_error_recovery call where the name is absent.
        registry.lock().clear();
        crate::daemon::hook_shadow::forget(name);
        let (new, _r2) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
        let new_id = new.id;
        registry.lock().insert(new_id, new);
        // The new instance emits a pre-onset hook (global seq > the OLD floor) BEFORE
        // its first genuine SRL.
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        pe(&mut tracks, &mut srl_floor);
        assert!(
            tracks.contains_key(name),
            "a same-name replacement's genuine first SRL must NOT be masked by the prior instance's inherited floor (UUID-keyed)"
        );
        assert!(
            !srl_floor.contains_key(&old_id),
            "the prior instance's floor is pruned (its uuid left the registry)"
        );
        assert!(
            srl_floor.contains_key(&new_id),
            "the new instance seeded its OWN floor under its own uuid"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// EDGE (#t-26795): a genuine NEW SRL must NOT be masked. The agent's latest hook
    /// PRESENT at onset seeds the floor to its OWN seq, so with NO newer hook the
    /// active seq EQUALS the floor → not strictly greater → no override → the retry
    /// arms. (A hook present at onset is "pre-onset" w.r.t. the floor it seeds.) This
    /// exercises the `or_insert_with(latest_hook_seq)` onset-init path — r4's blessed
    /// edge-a, preserved under the seq model. NEUTER: relax `h > f` to `h >= f` → the
    /// onset hook wrongly overrides → no arm → RED.
    #[test]
    #[serial_test::serial]
    fn srl_genuine_not_masked_by_pre_onset_hook() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-genuine");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
        let name = "srl-genuine-agent";
        let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
        registry.lock().insert(handle.id, handle);
        // A hook present at onset: the floor `or_insert`s to its seq → active seq ==
        // floor → no override. No newer hook arrives = a genuine SRL.
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut srl_floor,
            past_boot_grace(),
        );
        assert!(
            tracks.contains_key(name),
            "a hook no newer than the onset floor must NOT mask a genuine SRL — the retry must arm"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #ratelimit-recovery (the live storm that wedged fixup-lead): an agent still
    /// LATCHED ServerRateLimit (the stale "Server is temporarily limiting" line
    /// re-matches in the tail, and `working_state_below` can't see a marker BELOW
    /// the most-recent error line) but that has produced PRODUCTIVE output within
    /// RECOVERY_SILENCE has recovered — its retry track must be CLEARED and NO
    /// `continue` injected. `last_productive_output` is position-independent, so it
    /// breaks the Thinking↔ServerRateLimit flicker the Idle-only #1713 clear missed.
    #[test]
    fn server_rate_limit_recent_productive_output_clears_and_skips_inject() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-recovered");
        // Pre-arm an in-flight retry track (a ServerRateLimit episode already running).
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 2,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: false,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        // Recovered: produced productive output just now (< RECOVERY_SILENCE),
        // overriding the `None` (never-produced) default.
        handle.core.lock().state.last_productive_output = Some(Instant::now());
        registry.lock().insert(handle.id, handle);

        // Several ticks (the live flicker) — must never re-arm + inject.
        for _ in 0..3 {
            super::process_error_recovery(
                &home,
                &registry,
                &mut tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut last_inject,
                &mut Default::default(),
                past_boot_grace(),
            );
        }

        assert!(
            !tracks.contains_key("test-agent"),
            "#ratelimit-recovery: a recently-productive ServerRateLimit agent's retry \
             track must be cleared (recovered), not maintained/re-armed"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "#ratelimit-recovery: no `continue` may be injected into a recovered \
             (recently-productive) agent — that was the live storm"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2232 (a)+(c): an agent self-clearing its rate-limit block (MCP
    /// `clear_blocked_reason`, here the `rate_limit_self_cleared` latch) is
    /// ground-truth recovery even WITHOUT recent productive output (the pure-text
    /// fast-reply gap where `recovered_within` misses). Its retry track must clear
    /// and NO `continue` may be injected across repeated ticks (no over-inject).
    #[test]
    fn server_rate_limit_self_clear_clears_track_and_skips_inject_2232() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-self-clear");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 2,
                next_retry_at: Instant::now(), // DUE — would inject absent recovery
                exhausted: false,
                inject_failures: 0,
                abort_pending: false,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        // NOT recovered (last_productive_output stays None — the fast-text gap), but
        // the agent self-cleared its rate-limit block: the #2232 ground-truth signal.
        handle.core.lock().health.rate_limit_self_cleared = true;
        registry.lock().insert(handle.id, handle);

        for _ in 0..3 {
            super::process_error_recovery(
                &home,
                &registry,
                &mut tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut last_inject,
                &mut Default::default(),
                past_boot_grace(),
            );
        }

        assert!(
            !tracks.contains_key("test-agent"),
            "#2232: an agent that self-cleared its rate-limit block must have its \
             retry track dropped even with no recent productive output"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "#2232: no `continue` may be re-injected into a self-cleared agent \
             (the over-inject the issue reports)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2232 D1(b): when the supervisor tracks/injects a rate-limit retry it
    /// ALREADY knows the agent is rate-limited, so it marks the agent
    /// `RateLimit`-blocked — guaranteeing the agent's later filtered
    /// `clear_blocked_reason(reason=rate_limit)` matches and latches (reliable
    /// ground-truth, not tick-window best-effort).
    #[test]
    fn server_rate_limit_inject_schedule_marks_ratelimit_block_2232() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-mark-block");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        // No prior blocked_reason, not recovered, not self-cleared.
        assert!(handle.core.lock().health.current_reason.is_none());
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );

        let reg = registry.lock();
        let h = reg.values().next().expect("agent present");
        assert!(
            matches!(
                h.core.lock().health.current_reason,
                Some(crate::health::BlockedReason::RateLimit { .. })
            ),
            "#2232 D1(b): inject-schedule must mark the agent RateLimit-blocked so a \
             later filtered self-clear reliably matches"
        );
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2232 cross-episode: the self-clear latch resets on a genuine ServerRateLimit
    /// EXIT, so a FUTURE rate-limit episode re-arms the retry path (no stale latch
    /// permanently disabling auto-recovery).
    #[test]
    fn server_rate_limit_self_clear_latch_resets_on_exit_then_rearms_2232() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-rearm");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        handle.core.lock().health.rate_limit_self_cleared = true;
        registry.lock().insert(handle.id, handle);
        let tick = |tracks: &mut HashMap<String, RateLimitRetry>| {
            super::process_error_recovery(
                &home,
                &registry,
                tracks,
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                &mut Default::default(),
                past_boot_grace(),
            );
        };

        // Self-cleared SRL → no track armed.
        tick(&mut tracks);
        assert!(
            !tracks.contains_key("test-agent"),
            "self-cleared SRL must not re-arm"
        );

        // Agent genuinely leaves ServerRateLimit → latch resets.
        {
            let reg = registry.lock();
            reg.values()
                .next()
                .expect("agent present")
                .core
                .lock()
                .state
                .current = crate::state::AgentState::Idle;
        }
        tick(&mut tracks);
        assert!(
            !registry
                .lock()
                .values()
                .next()
                .expect("agent present")
                .core
                .lock()
                .health
                .rate_limit_self_cleared,
            "#2232: a genuine ServerRateLimit exit resets the self-clear latch"
        );

        // A NEW rate-limit episode (no productive output, latch now reset) re-arms.
        {
            let reg = registry.lock();
            reg.values()
                .next()
                .expect("agent present")
                .core
                .lock()
                .state
                .current = crate::state::AgentState::ServerRateLimit;
        }
        tick(&mut tracks);
        assert!(
            tracks.contains_key("test-agent"),
            "#2232: a future rate-limit episode must re-arm after the latch reset"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2232 (d): the self-clear signal is an idempotent no-op when there is no
    /// active retry track — the supervisor consumes it safely (no panic, no
    /// inject, and a self-cleared agent is not re-armed).
    #[test]
    fn server_rate_limit_self_clear_no_track_is_noop_2232() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("srl-noop");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new(); // empty
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        let (handle, _reader) =
            mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
        handle.core.lock().health.rate_limit_self_cleared = true;
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );

        assert!(
            !tracks.contains_key("test-agent"),
            "#2232: a self-cleared agent is not re-armed even from an empty track set"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "#2232: no inject for a self-cleared agent"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1325/#1946: phase 1 — GENUINE recovery (Idle + recent productive output)
    /// clears the retry track. (#1946 narrowed the clear: an Idle WITHOUT recent
    /// productive output and retries in flight is the abort shape and retains —
    /// see the 1946 tests below — so this genuine-recovery contract now requires
    /// the productive-output signal it always meant.)
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
                abort_pending: false,
            },
        );

        let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
        // Genuine recovery: the agent produced real output before idling.
        handle.core.lock().state.last_productive_output = Some(Instant::now());
        // #1441: registry is UUID-keyed — insert under the handle's own id.
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            !tracks.contains_key("test-agent"),
            "phase 1 must clear retry track on genuine Idle recovery"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1946 (closes #1808 Flaw 1, production-evidenced 2026-06-10 08:59 probe
    /// fire + 08:55-08:59 dev-2 freeze): an abort-to-Idle with an in-flight
    /// ServerRateLimit retry and NO recent productive output must RETAIN the
    /// track (ownership of recovery stays with the supervisor) and schedule a
    /// delayed after-abort retry on the SAME tiered backoff — not clear it
    /// (the pre-#1946 behavior, which froze the agent until manual rescue
    /// because post-#1936 detection never re-creates a track either).
    #[test]
    fn abort_to_idle_retains_track_and_resumes_retry_1946() {
        // one_agent_registry writes fleet.yaml so the Phase-2 inject can
        // resolve the agent (a bare registry insert reads as AgentGone).
        // The agent sits at an Idle prompt with NO recent productive output
        // (`last_productive_output` defaults to None) — the freeze shape.
        let (home, registry, _reader) = one_agent_registry(
            "test-agent",
            crate::state::AgentState::Idle,
            "abort-retain-1946",
        );
        {
            let reg = registry.lock();
            let handle = reg.values().next().expect("agent handle exists");
            handle
                .core
                .lock()
                .vterm
                .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
        }
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 2,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: false,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        // Tick 1: the abort is detected → track retained, abort_pending set,
        // next retry scheduled on the tiered backoff (BACKOFF[2] = 30s out) —
        // NOT due yet, so no inject this tick.
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
        {
            let track = tracks
                .get("test-agent")
                .expect("#1946: abort-to-Idle must RETAIN the in-flight track, not clear it");
            assert!(track.abort_pending, "#1946: abort_pending marked");
            assert!(
                track.next_retry_at > Instant::now() + Duration::from_secs(20),
                "#1946: delayed retry continues the tiered schedule (BACKOFF[2]=30s), not immediate"
            );
            assert!(
                !last_inject.contains_key("test-agent"),
                "#1946: no inject before the delayed retry is due"
            );
        }

        // Make the delayed retry due → tick 2 must inject the after-abort
        // `continue` (the ONLY Idle-state inject, gated on abort_pending +
        // !recovered) and keep the track.
        tracks
            .get_mut("test-agent")
            .expect("track retained")
            .next_retry_at = Instant::now();
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            last_inject.contains_key("test-agent"),
            "#1946: due after-abort retry must inject `continue` into the Idle agent"
        );
        let track = tracks.get("test-agent").expect("track survives the inject");
        assert_eq!(
            track.retry_count, 3,
            "#1946: after-abort attempts consume the SAME 12-retry budget"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1946: genuine recovery AFTER an abort (productive output appears — e.g.
    /// the operator dispatched work, or the after-abort `continue` revived the
    /// agent) clears the retained track; no further inject.
    #[test]
    fn abort_pending_recovered_clears_track_1946() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-recovered-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 3,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
        // Productive output landed after the abort — genuine recovery.
        handle.core.lock().state.last_productive_output = Some(Instant::now());
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            !tracks.contains_key("test-agent"),
            "#1946: genuine recovery after an abort clears the retained track"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "#1946: no `continue` into a recovered agent"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1946 / #1808: when the rate limit error has scrolled off the screen
    /// (the vterm no longer contains the throttle hint), the abort-pending retry track must be cleared
    /// even if the agent hasn't produced new output within the silence window.
    #[test]
    fn abort_pending_scrolled_off_clears_track_1946() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-scrolled-off-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 3,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
            },
        );

        let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
        // core.vterm is empty by default, so screen_has_throttle_hint returns false (scrolled off).
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );

        assert!(
            !tracks.contains_key("test-agent"),
            "scrolled off error must clear the abort-pending retry track"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1946: a fresh ServerRateLimit observation while abort-recovery is
    /// pending hands ownership back to the normal fresh-SRL retry path (same
    /// track, same budget — structurally a single owner, no double-continue).
    #[test]
    fn abort_pending_stands_down_on_srl_relatch_1946() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-relatch-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 3,
                // Not due — proves the stand-down happens on observation, not inject.
                next_retry_at: Instant::now() + Duration::from_secs(600),
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
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
            &mut Default::default(),
            past_boot_grace(),
        );
        let track = tracks.get("test-agent").expect("track persists");
        assert!(
            !track.abort_pending,
            "#1946: SRL re-latch resumes normal retry ownership (abort-recovery stands down)"
        );
        assert_eq!(track.retry_count, 3, "budget carries over, no reset");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1946: the after-abort path consumes the SAME tiered budget — at the
    /// 12-retry cap the existing exhaustion path (orchestrator inbox notify +
    /// Error-severity channel alert) finally becomes REACHABLE in a sustained
    /// outage (pre-#1946 the track died at the first abort, so exhaustion—and
    /// its escalation—never fired).
    #[test]
    fn abort_pending_budget_exhaustion_reachable_1946() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-exhaust-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: SERVER_RATE_LIMIT_MAX_RETRIES, // budget already burned
                next_retry_at: Instant::now(),              // due
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
            },
        );
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
        handle
            .core
            .lock()
            .vterm
            .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
        let track = tracks
            .get("test-agent")
            .expect("exhausted track retained this tick");
        assert!(
            track.exhausted,
            "#1946: after-abort attempts walk into the existing exhaustion path"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "no inject past the budget cap"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Verify that if the throttle error is sitting in rows 16–40 (e.g. at row 20 on a 50-row screen),
    /// the track is retained (proving the TAIL_LINES window correctly scans up to 40 rows).
    #[test]
    fn abort_pending_retains_track_when_error_in_rows_16_to_40() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-rows-16-40-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 3,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
            },
        );

        let (handle, _reader) =
            mock_agent_handle_with_size("test-agent", crate::state::AgentState::Idle, 50, 80);

        // Write the error message, then write 20 empty lines so the error is pushed to row 20 from bottom.
        {
            let mut core_lock = handle.core.lock();
            core_lock
                .vterm
                .process(b"API Error: Server is temporarily limiting requests\r\n");
            for _ in 0..20 {
                core_lock.vterm.process(b"\r\n");
            }
        }

        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );

        assert!(
            tracks.contains_key("test-agent"),
            "error at row 20 (within 40-row TAIL_LINES window) must retain the abort-pending retry track"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1985 / Item 4: Document the soft-wrap split edge case. If the throttle hint
    /// is split across a soft-wrap boundary, it won't match, and the track is cleared.
    #[test]
    fn abort_pending_split_wrap_clears_track_1946() {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let home = tmp_home("abort-split-wrap-1946");
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        tracks.insert(
            "test-agent".to_string(),
            RateLimitRetry {
                retry_count: 3,
                next_retry_at: Instant::now(),
                exhausted: false,
                inject_failures: 0,
                abort_pending: true,
            },
        );

        let (handle, _reader) =
            mock_agent_handle_with_size("test-agent", crate::state::AgentState::Idle, 10, 38);

        // Write the error message. Because cols is 38, "limiting" is soft-wrapped across lines.
        {
            let mut core_lock = handle.core.lock();
            core_lock
                .vterm
                .process(b"API Error: Server is temporarily limiting requests\r\n");
        }

        registry.lock().insert(handle.id, handle);

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );

        assert!(
            !tracks.contains_key("test-agent"),
            "soft-wrapped split error token does not match in tail_lines, so track is cleared"
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
        // Everything else — incl mid-work Active and every waiting/error
        // state — must NOT clear.
        for s in [
            Active,
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
                abort_pending: false,
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
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
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
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
            },
        );

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
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
        // #1769: the ServerRateLimit auto-retry inject is tagged so an
        // orchestrator can tell it apart from a real operator "continue".
        assert!(
            captured.contains("[AGEND-AUTO kind=ratelimit-retry]"),
            "#1769: daemon auto-inject must carry the [AGEND-AUTO kind=...] marker, got: {:?}",
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
                abort_pending: false,
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
                abort_pending: false,
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
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
            },
        );

        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
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
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
            },
        );
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
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
                abort_pending: false,
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
                abort_pending: false,
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
                abort_pending: false,
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
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pane_input_not_submitted_dedups_per_typed_timestamp() {
        let agent = "claude-agent-pin-dedup";
        let home = fleet_with_backend("pin_dedup", agent, "claude");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let typed_ms = now_ms - 300_000;
        seed_input_submit(&home, agent, typed_ms, 0);
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

    /// #986 source-pin (INVERTED from #1002 Phase 2): the supervisor's per-tick
    /// loop must NOT scan pr_state. The `PrStateScanHandler` per-tick handler is the
    /// SINGLE scanner+worker in EVERY mode — it runs in `run_core`'s handler vec
    /// (daemon) AND in `app::app_tick_handlers` (app standalone, attached AND owned,
    /// since `pr_state_scan` is not in `APP_TICK_ALLOWLIST`). The #1002-era direct
    /// supervisor scan was a vestigial belt from when the handler was run_core-only;
    /// with the handler now live in every mode it was a redundant second scanner +
    /// (post-#986) a second gh-poll worker. This pin guards against re-adding it.
    #[test]
    fn pr_state_scan_wired_into_supervisor_loop() {
        let source = std::fs::read_to_string("src/daemon/supervisor.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/supervisor.rs"))
            .expect("source file must be readable from test cwd");
        // #986: the supervisor loop must NOT scan pr_state. `PrStateScanHandler`
        // is the SINGLE scanner+worker in ALL modes (run_core handler vec + app
        // `app_tick_handlers`, both attached and owned). A supervisor scan would be
        // a redundant SECOND scanner + a SECOND gh-poll worker. Guard against
        // re-adding it. The needle is assembled from fragments so this assertion's
        // own source does not match (the file never contains the verbatim call).
        let needle = format!("{}{}", "scan_and", "_emit");
        assert!(
            !source.contains(&needle),
            "supervisor loop must NOT invoke a pr_state scan (#986: the handler is \
             the sole scanner+worker in every mode; a supervisor scan would double \
             both the scanner and the gh-poll worker)."
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
        let (home, registry, _r) =
            one_agent_registry("ag", crate::state::AgentState::Active, "1713-thinking-keep");
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
                abort_pending: false,
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
            &mut Default::default(),
            past_boot_grace(),
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
            &mut Default::default(),
            past_boot_grace(),
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
            &mut Default::default(),
            past_boot_grace(),
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
                &mut Default::default(),
                past_boot_grace(),
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

    #[test]
    fn resolve_pending_auth_holds_fire_during_boot_grace_1741() {
        use super::{resolve_pending_auth, AuthErrorGate, PendingAuthError};
        let entry = || PendingAuthError {
            from: crate::state::AgentState::Idle,
            pane_tail: String::new(),
        };

        // Fire WITHIN boot-grace → held: pending KEPT, nothing fired (the
        // confirm-window is preserved; it fires once the grace ends).
        let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
        pending.insert("ag".into(), entry());
        assert!(
            resolve_pending_auth(AuthErrorGate::Fire, true, "ag", &mut pending).is_none(),
            "#1741: Fire during boot-grace must NOT fire"
        );
        assert!(
            pending.contains_key("ag"),
            "#1741: Fire during boot-grace must KEEP pending (no lost notify)"
        );

        // Fire AFTER boot-grace → fires: entry returned + removed from pending.
        assert!(
            resolve_pending_auth(AuthErrorGate::Fire, false, "ag", &mut pending).is_some(),
            "#1741: Fire after grace must fire"
        );
        assert!(
            !pending.contains_key("ag"),
            "#1741: Fire after grace must remove pending"
        );

        // Cancel → drop pending, never fires (boot-grace irrelevant).
        let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
        pending.insert("ag".into(), entry());
        assert!(resolve_pending_auth(AuthErrorGate::Cancel, true, "ag", &mut pending).is_none());
        assert!(
            !pending.contains_key("ag"),
            "Cancel must drop the self-healed pending entry"
        );

        // Wait → keep pending, never fires.
        let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
        pending.insert("ag".into(), entry());
        assert!(resolve_pending_auth(AuthErrorGate::Wait, false, "ag", &mut pending).is_none());
        assert!(pending.contains_key("ag"), "Wait must keep pending");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    // Mirrors #1697's gate: the one_agent_registry PTY/inject path (the post-grace
    // `reader.read(...).contains("continue")` assertion) doesn't work under
    // Windows conpty. The boot-grace logic itself is platform-agnostic and the
    // pure `resolve_pending_auth` test covers the confirm-window path on all OSes.
    fn apierror_nudge_suppressed_during_boot_grace_1741() {
        let (home, registry, mut reader) = one_agent_registry(
            "ag",
            crate::state::AgentState::ApiError,
            "apierror-bootgrace-1741",
        );
        let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut last_inject: HashMap<String, Instant> = HashMap::new();

        // WITHIN boot-grace (loop just started) → no nudge queued, episode UNMARKED
        // (so a still-ApiError agent gets a fresh nudge after grace, not a phantom
        // "already nudged" mark).
        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut episodes,
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            Instant::now(),
        );
        assert!(
            !episodes.contains("ag"),
            "#1741: boot-grace must NOT mark the ApiError episode"
        );
        assert!(
            !last_inject.contains_key("ag"),
            "#1741: boot-grace must suppress the ApiError nudge"
        );

        // AFTER boot-grace → still ApiError → fresh nudge fires + episode marked.
        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut episodes,
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            episodes.contains("ag"),
            "#1741: after grace, a still-ApiError agent must be nudged fresh"
        );
        let mut buf = vec![0u8; 256];
        use std::io::Read;
        let n = reader.read(&mut buf).expect("read from PTY");
        assert!(
            String::from_utf8_lossy(&buf[..n]).contains("continue"),
            "#1741: post-grace ApiError nudge must inject \"continue\""
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
        // #1769: the call is now multi-line (gained the `auto_kind` arg), so
        // normalize whitespace before substring-matching the arg order.
        let norm = src.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            norm.contains("CONTINUE_RETRY_PAYLOAD, false, Some(auto_kind),"),
            "#1680: the continue-inject must pass force=false (draft-gated); \
             #1769: and the daemon-auto marker (auto_kind)"
        );
        // Split needle so this assertion's own text can't false-match the source.
        let force_true = format!("CONTINUE_RETRY_PAYLOAD,{}true", " ");
        assert!(
            !norm.contains(&force_true),
            "#1680: no force=true continue-inject may remain"
        );
        // #2232: the ratelimit-retry self-clear-guidance inject (sibling payload)
        // must ALSO be draft-gated (force=false) — same #1680 safety on the new path.
        assert!(
            norm.contains("RATELIMIT_RETRY_PAYLOAD, false, Some(auto_kind),"),
            "#2232: the ratelimit-retry inject must pass force=false (draft-gated)"
        );
        let rl_force_true = format!("RATELIMIT_RETRY_PAYLOAD,{}true", " ");
        assert!(
            !norm.contains(&rl_force_true),
            "#2232: no force=true ratelimit-retry inject may remain"
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

#[cfg(test)]
mod review_repro_daemon_supervisor;
