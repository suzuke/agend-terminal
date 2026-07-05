//! #t-777-3: respawn-stuck watchdog — auto-recover an agent whose `Resume`
//! spawn hung and never came up.
//!
//! ## The incident (RCA `workspace/fixup-dev-2/RESPAWN-WATCHDOG-RCA-777-3.md`)
//! A broad `pkill` killed codex agents mid-session-write; the daemon then
//! re-spawned them via the `Resume` path (`daemon/mod.rs:1850`
//! `SpawnMode::Resume.downgraded_for` / app session-restore `app/session.rs:238`).
//! `resume --last` on a half-written (corrupt) session **hung**, and the agent
//! sat in `AgentState::Restarting` ~45 min with no watchdog covering it — the
//! Hung recovery ladder keys on `HealthState::Hung`, never on a respawn that is
//! itself stuck. Manual `restart_instance(fresh)` was the only fix.
//!
//! ## What this handler does
//! Each tick it flags any agent that (a) was spawned via `Resume`, (b) is still
//! in a not-yet-ready state (`Starting`/`Restarting`), (c) has been there past a
//! timeout, AND (d) has emitted no output for that timeout — then auto-recovers
//! via a **Fresh** restart through the PROVEN API path
//! (`restart_instance_autonomic` → direct `DELETE`+`SPAWN` →
//! `ApiEvent::InstanceCreated` → a fresh pane). That path works in BOTH the
//! `run_core` daemon and the **live app-mode daemon** (`run_app`), where the
//! `crash_tx`→respawn machinery is inert (agents spawn with `crash_tx: None`,
//! `pane_factory.rs`) — which is exactly why this watchdog drives recovery via
//! `api::call` instead of emitting an `AgentExitEvent`.
//!
//! ## Why `Resume`-only (the load-bearing false-kill guard)
//! Firing ONLY on `SpawnMode::Resume` is what lets the watchdog ship
//! ON-by-default safely: a slow-but-healthy *Fresh* boot is NEVER force-killed
//! (its `spawn_mode != Resume`). The Fresh respawn this watchdog itself triggers
//! is therefore never re-detected → the Resume-gate is the structural
//! loop-breaker; the K-retry cap is the explicit belt for a recurring
//! stuck-Resume pathology (e.g. a daemon that keeps restarting + re-resuming a
//! corrupt session). After K failed auto-Fresh attempts within the stability
//! window it stops retrying and escalates a P0 to the operator + pauses (the
//! terminal "auto when possible, page when auto fails" escalation).
//!
//! ## Disjoint from the Hung ladder (no double-fire)
//! `hang_detection` → `recovery_dispatcher` key on `HealthState::Hung`. This
//! watchdog keys on `AgentState::{Starting,Restarting}` + `SpawnMode::Resume`
//! and skips any agent already `HealthState::Paused` — a disjoint state class,
//! so the two never act on the same agent in the same tick.
//!
//! ## Gate-exempt by construction (operator authority)
//! The recovery goes through `restart_instance_autonomic`, whose inner
//! `DELETE`/`SPAWN` are DIRECT api methods (operator-transport — `operator_gate`
//! returns `Ok` before `classify`). It is reached ONLY from this internal
//! hang-detection tick (never agent-invocable), so it is gate-exempt by the same
//! daemon-autonomic-self-heal rationale as crash-respawn / hang-recovery: a
//! stuck resume must self-heal even while the operator is away/asleep.

use super::{PerTickHandler, TickContext};
use crate::agent;
use crate::backend::SpawnMode;
use crate::health::HealthState;
use crate::state::AgentState;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// `tracing` target for the watchdog's telemetry, so dashboards can aggregate
/// its decisions alongside the `recovery_shadow` recovery-ladder surface.
const TARGET: &str = "respawn_watchdog";

/// Time an agent must sit in a not-yet-ready state — with NO output — after a
/// `Resume` spawn before the watchdog judges the resume stuck. Generous vs a
/// normal resume (which completes in seconds) so a slow-but-legit resume is
/// never force-killed, while a hung one self-heals within ~1 min instead of the
/// 45-min incident. Lead-set (#t-777-3 msg m-…100): ~60s.
const RESPAWN_STUCK_TIMEOUT: Duration = Duration::from_secs(60);

/// Anti-thrash: once the watchdog fires an auto-Fresh for an agent it will not
/// re-fire for this long. Covers the async window between firing and the
/// `DELETE`+`SPAWN` actually replacing the (still-`Resume`) handle, so a slow
/// restart never queues a second fire. > `RESPAWN_STUCK_TIMEOUT` by design.
const RESPAWN_RETRY_COOLDOWN: Duration = Duration::from_secs(90);

/// Max auto-Fresh attempts per agent within the stability window before the
/// watchdog gives up and escalates (terminal). Mirrors `STAGE2_MAX_RESTARTS`.
const RESPAWN_MAX_RETRIES: u32 = 3;

/// A retry record for an agent that is no longer stuck is forgiven (cleared)
/// once this much stability elapses — mirrors `HealthTracker`'s
/// `STABILITY_WINDOW` decay discipline so a long-recovered agent does not carry
/// retry attribution forever.
const RESPAWN_STABILITY_WINDOW: Duration = Duration::from_secs(1800);

/// Kill-switch env var. The watchdog is ON by default (it is the missing safety
/// floor); set `AGEND_RESPAWN_WATCHDOG=0` to disable without a daemon restart
/// (read each tick, like the recovery-ladder gates).
const DISABLE_ENV_VAR: &str = "AGEND_RESPAWN_WATCHDOG";

/// Per-agent retry bookkeeping. Lives on the HANDLER (daemon-lifetime), NOT the
/// agent handle, so it SURVIVES the `DELETE`+`SPAWN` that the auto-Fresh
/// performs — a handle-local counter would reset to 0 on every fresh handle,
/// making the K-cap unreachable. This is what lets the cap bound repeated
/// stuck-Resume episodes across respawns.
struct RetryRecord {
    /// Auto-Fresh attempts fired for this agent in the current stability window.
    count: u32,
    /// When the last auto-Fresh fired (`None` = never) — drives both the
    /// anti-thrash cooldown and the stability-window forgiveness.
    last_retry_at: Option<Instant>,
    /// Fire-once latch for the terminal P0 escalation, so a persistently-stuck
    /// agent pages the operator at most once per cap cycle.
    escalated: bool,
}

impl RetryRecord {
    fn new() -> Self {
        Self {
            count: 0,
            last_retry_at: None,
            escalated: false,
        }
    }
}

/// What `decide` says to do with one stuck-Resume detection this tick.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// In cooldown, or already-escalated — do nothing this tick.
    None,
    /// Fire an auto-Fresh restart; carries the (1-based) attempt number.
    Fire(u32),
    /// Cap reached — pause + page the operator (terminal).
    Escalate,
}

/// Pure decision + record mutation for one stuck detection, extracted so the
/// bounded-retry / cooldown / fire-once-escalate state machine is unit-testable
/// without a registry or the api round-trip. Takes `&mut RetryRecord` so a test
/// can assert both the returned `Action` AND the resulting record state.
fn decide(rec: &mut RetryRecord, now: Instant, cooldown: Duration, max: u32) -> Action {
    if let Some(t) = rec.last_retry_at {
        if now.saturating_duration_since(t) < cooldown {
            return Action::None;
        }
    }
    if rec.count >= max {
        if rec.escalated {
            return Action::None;
        }
        rec.escalated = true;
        return Action::Escalate;
    }
    rec.count += 1;
    rec.last_retry_at = Some(now);
    Action::Fire(rec.count)
}

/// The stuck-Resume predicate, pure for unit tests. `true` ⟺ this spawn was a
/// `Resume`, the agent is still in a not-yet-ready state, it has been there past
/// `timeout`, AND it has produced no output for `timeout` (the conservative
/// no-productive-output gate that spares a slow-but-emitting resume).
fn is_stuck_resume(
    spawn_mode: SpawnMode,
    state: AgentState,
    since_elapsed: Duration,
    silent: Duration,
    timeout: Duration,
) -> bool {
    spawn_mode == SpawnMode::Resume
        && matches!(state, AgentState::Starting | AgentState::Restarting)
        && since_elapsed > timeout
        && silent > timeout
}

/// What the watchdog should do with one agent this tick — computed BEFORE any
/// auto-Fresh decision so an auth-expired agent is routed away from the
/// (useless-for-auth) restart ladder.
#[derive(Debug, PartialEq, Eq)]
enum Situation {
    /// Nothing actionable this tick.
    Ignore,
    /// Claude authorization expired: exclude from auto-Fresh and page the
    /// operator (fire-once). A respawn cannot fix auth — a fresh process is just
    /// as unauthenticated — only the operator can re-authenticate.
    AuthExpired,
    /// A stuck `Resume`: eligible for the bounded auto-Fresh ladder.
    StuckResume,
}

/// Pure classifier for one agent, so the auth-vs-stuck precedence is
/// unit-testable without a registry. AUTH TAKES PRECEDENCE: it is recognised via
/// the EXISTING detection (`backend_profile.rs` regex → `AgentState::AuthError`).
/// t-...30532-0: that regex is red-anchor-guarded but the STATE it drives is
/// still content-FP-prone at the instant level (a transient blip flips it, ~31s
/// self-heal observed), so AuthExpired is additionally gated on the supervisor's
/// `AUTH_ERROR_NOTIFY_STABILITY` continuous-held window — the same FP defence the
/// #1523 re-auth alert uses — rather than trusting the bare instant signal. It
/// is gated to `Resume` to stay inside this watchdog's domain (the module force-
/// restarts only `Resume` spawns) and disjoint from the supervisor's general
/// AuthError flow. `AuthError` and `Starting/Restarting` are different
/// `AgentState`s, so this never reclassifies a genuine stuck Resume.
fn classify(
    spawn_mode: SpawnMode,
    state: AgentState,
    since_elapsed: Duration,
    silent: Duration,
    timeout: Duration,
) -> Situation {
    if spawn_mode == SpawnMode::Resume
        && state == AgentState::AuthError
        && since_elapsed >= crate::daemon::supervisor::AUTH_ERROR_NOTIFY_STABILITY
    {
        // t-...30532-0 (reviewer5 REJECTED): the `AuthError` STATE is content-FP-
        // prone at the instant level — transient PTY content flips it cosmetically
        // (an instance self-healed back to Thinking in ~31s), so firing on the
        // bare signal blip-pages the operator AND blindly skips the auto-Fresh
        // that would have recovered it. Gate on the SAME stability window the
        // supervisor's #1523 re-auth alert uses: only page once AuthError has been
        // held CONTINUOUSLY past it. `since_elapsed` is `state.since.elapsed()`,
        // reset on every transition (state/mod.rs `record_set`), so it is exactly
        // the continuous-`AuthError` held-duration — a flicker out-and-back never
        // accumulates. UNDER the window: fall through — `is_stuck_resume` needs
        // `Starting`/`Restarting` (never `AuthError`), so this lands on `Ignore`
        // and the blip is left to self-heal, no page, no auto-Fresh interference.
        return Situation::AuthExpired;
    }
    if is_stuck_resume(spawn_mode, state, since_elapsed, silent, timeout) {
        return Situation::StuckResume;
    }
    Situation::Ignore
}

/// Fire-once-until-recovered latch resolver, pure for unit tests. Given the set
/// of agents currently auth-expired this tick and the latch of already-paged
/// names (mutated in place): clears the latch for any agent that recovered (no
/// longer auth-expired) or left, then returns the newly-auth-expired agents to
/// page now (and latches them). Net effect: each auth-expiry episode pages the
/// operator exactly once, but a *later* re-expiry after recovery pages again.
fn auth_notify_targets(auth_now: &HashSet<String>, latch: &mut HashSet<String>) -> Vec<String> {
    // Forgive recovered/absent agents so a future re-expiry can page again.
    latch.retain(|n| auth_now.contains(n));
    // Page each newly-detected auth-expired agent once.
    let mut to_notify = Vec::new();
    for n in auth_now {
        if latch.insert(n.clone()) {
            to_notify.push(n.clone());
        }
    }
    to_notify
}

pub(crate) struct RespawnWatchdogHandler {
    retries: Mutex<HashMap<String, RetryRecord>>,
    /// Fire-once-until-recovered latch for the auth-expiry operator page. Holds
    /// the names already paged this auth-expiry episode so a still-AuthError
    /// agent (re-detected every tick) is not re-paged. Lives on the HANDLER
    /// (daemon-lifetime); `auth_notify_targets` clears an entry when its agent
    /// recovers (leaves AuthError) or leaves the registry, so a *later*
    /// re-expiry pages again. Separate from `retries` because an auth-expired
    /// agent is deliberately NOT on the auto-Fresh ladder.
    auth_notified: Mutex<HashSet<String>>,
}

impl RespawnWatchdogHandler {
    pub(crate) fn new() -> Self {
        Self {
            retries: Mutex::new(HashMap::new()),
            auth_notified: Mutex::new(HashSet::new()),
        }
    }
}

impl PerTickHandler for RespawnWatchdogHandler {
    fn name(&self) -> &'static str {
        "respawn_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // ON by default; operator kill-switch read each tick.
        if std::env::var(DISABLE_ENV_VAR)
            .map(|v| v == "0")
            .unwrap_or(false)
        {
            return;
        }

        // Phase 1 (under the registry lock): collect the stuck-Resume names and
        // the full live name-set. No I/O / api::call under the lock — the
        // recovery work runs in phase 2 after the guard drops (lock-ordering /
        // #1593 deadlock-class discipline, mirroring recovery_dispatcher).
        let (stuck, auth, live): (Vec<String>, HashSet<String>, HashSet<String>) = {
            let reg = agent::lock_registry_tracked(ctx.registry, "respawn_watchdog");
            let mut stuck = Vec::new();
            let mut auth = HashSet::new();
            let mut live = HashSet::new();
            for handle in reg.values() {
                let name = handle.name.to_string();
                live.insert(name.clone());
                // Skip an instance mid-teardown (deleted flag) — don't fight a
                // delete with a respawn.
                if handle.deleted.load(Ordering::Acquire) {
                    continue;
                }
                let spawn_mode = handle.spawn_mode;
                let core = handle.core.lock();
                // Already operator/Stage3-paused → terminal, leave it alone
                // (keeps this disjoint from the Hung ladder's Paused class).
                if core.health.state == HealthState::Paused {
                    continue;
                }
                // KEEP-RAW (#2465): respawn-stuck detection is a recovery safety net — feeding it the
                // promoted/observed state could let a stale/false 'Active' hook MASK a genuinely stuck
                // (never-resumed) agent and suppress its respawn. Do NOT migrate to operated_state.
                let state = core.state.current;
                let since_elapsed = core.state.since.elapsed();
                let silent = core.state.last_output.elapsed();
                drop(core);
                // Consult auth-expiry BEFORE the stuck/auto-Fresh decision: an
                // AuthError agent must never be auto-Fresh'd (respawn can't
                // re-authenticate) — route it to an actionable operator page
                // instead, mirroring the `:214` Paused exclusion.
                match classify(
                    spawn_mode,
                    state,
                    since_elapsed,
                    silent,
                    RESPAWN_STUCK_TIMEOUT,
                ) {
                    Situation::AuthExpired => {
                        auth.insert(name);
                    }
                    Situation::StuckResume => {
                        stuck.push(name);
                    }
                    Situation::Ignore => {}
                }
            }
            (stuck, auth, live)
        };

        // Forgive/evict stale retry records (agent recovered or left).
        self.gc_records(&stuck, &live);

        // Auth-expiry pass (lock dropped): page the operator once per episode,
        // never auto-Fresh. The latch is on the handler so a still-AuthError
        // agent is not re-paged each tick.
        let auth_targets = {
            let mut latch = self.auth_notified.lock();
            auth_notify_targets(&auth, &mut latch)
        };
        for name in &auth_targets {
            crate::event_log::log(
                ctx.home,
                "respawn_watchdog_auth_expiry",
                name,
                "Claude auth expired on a resume — paged operator, skipped auto-Fresh",
            );
            notify_auth_expiry(name);
        }

        // Phase 2 (lock dropped): decide + act per stuck agent.
        for name in &stuck {
            self.handle_stuck(ctx, name);
        }
    }
}

impl RespawnWatchdogHandler {
    /// Drop retry records for agents that left the registry, and forgive records
    /// for agents that are no longer stuck once they've been stable for the
    /// window (so a transient stuck-Resume that auto-recovered doesn't keep
    /// counting toward the cap forever).
    fn gc_records(&self, stuck: &[String], live: &HashSet<String>) {
        let stuck_set: HashSet<&str> = stuck.iter().map(String::as_str).collect();
        let now = Instant::now();
        let mut retries = self.retries.lock();
        retries.retain(|name, rec| {
            if !live.contains(name) {
                return false; // agent gone
            }
            if stuck_set.contains(name.as_str()) {
                return true; // still stuck → keep counting
            }
            // Not stuck now: keep the record (so a quick re-stuck still counts)
            // until it has been stable for the window, then forgive.
            match rec.last_retry_at {
                Some(t) => now.saturating_duration_since(t) < RESPAWN_STABILITY_WINDOW,
                None => false,
            }
        });
    }

    /// Decide + act on one stuck-Resume agent. One lock acquisition mutates the
    /// retry record and yields an `Action`; the I/O (thread spawn / notify /
    /// enter_paused) runs AFTER the lock drops.
    fn handle_stuck(&self, ctx: &TickContext<'_>, name: &str) {
        let now = Instant::now();
        let action = {
            let mut retries = self.retries.lock();
            let rec = retries
                .entry(name.to_string())
                .or_insert_with(RetryRecord::new);
            decide(rec, now, RESPAWN_RETRY_COOLDOWN, RESPAWN_MAX_RETRIES)
        };
        match action {
            Action::None => {}
            Action::Fire(attempt) => self.fire_auto_fresh(ctx, name, attempt),
            Action::Escalate => self.escalate_terminal(ctx, name),
        }
    }

    /// (B) Trigger an auto-Fresh restart via the proven API path, off the tick.
    fn fire_auto_fresh(&self, ctx: &TickContext<'_>, name: &str, attempt: u32) {
        tracing::warn!(
            target: TARGET,
            agent = %name,
            attempt,
            max = RESPAWN_MAX_RETRIES,
            "respawn-stuck watchdog: stuck Resume detected — triggering auto-Fresh restart"
        );
        crate::event_log::log(
            ctx.home,
            "respawn_watchdog_fresh",
            name,
            &format!("auto-Fresh restart attempt {attempt}/{RESPAWN_MAX_RETRIES}"),
        );
        let home = ctx.home.to_path_buf();
        let name_owned = name.to_string();
        // fire-and-forget: the restart round-trips DELETE+SPAWN over the api
        // socket (~100ms+) and must not block the supervisor tick. No JoinHandle
        // is kept — the restart is self-contained, its outcome is reported via
        // tracing + the operator notify on failure, and progress is re-observed
        // by next-tick re-detection (the cap handles a persistent failure).
        std::thread::spawn(move || {
            let reason =
                format!("respawn-stuck watchdog auto-Fresh (#t-777-3, attempt {attempt}/{RESPAWN_MAX_RETRIES})");
            let spawned = crate::mcp::handlers::instance_state::restart_instance_autonomic(
                &home,
                &name_owned,
                &reason,
            );
            if spawned {
                tracing::info!(
                    target: TARGET,
                    agent = %name_owned,
                    attempt,
                    "respawn-stuck watchdog: auto-Fresh restart issued"
                );
            } else {
                tracing::error!(
                    target: TARGET,
                    agent = %name_owned,
                    attempt,
                    "respawn-stuck watchdog: auto-Fresh restart FAILED to spawn"
                );
                notify_restart_failed(&name_owned);
            }
        });
    }

    /// (A) Terminal escalation after the retry cap: pause the agent
    /// (`enter_paused`) and page the operator P0 (fire-once via the `escalated`
    /// latch set in `decide`).
    fn escalate_terminal(&self, ctx: &TickContext<'_>, name: &str) {
        let now = Instant::now();
        {
            let reg = agent::lock_registry(ctx.registry);
            if let Some(h) = reg.values().find(|h| h.name.as_str() == name) {
                h.core.lock().health.enter_paused(now);
            }
        }
        crate::event_log::log(
            ctx.home,
            "respawn_watchdog_escalate",
            name,
            &format!("auto-Fresh exhausted after {RESPAWN_MAX_RETRIES} attempts — paused + P0"),
        );
        notify_escalation(name, RESPAWN_MAX_RETRIES);
    }
}

/// Page the operator P0 that auto-recovery is exhausted. Mirrors
/// `hang_detection::notify_self_orch_hung` — same `notify_all_escalation_channels`
/// Error-severity, Sleep-penetrating path.
fn notify_escalation(name: &str, attempts: u32) {
    tracing::error!(
        target: TARGET,
        agent = %name,
        attempts,
        "respawn-stuck watchdog: auto-Fresh retries exhausted — escalating P0 + pausing"
    );
    let msg = format!(
        "🛑 {name}: stuck on a `resume` that never came up. Auto-Fresh restart was attempted \
         {attempts}× and it kept coming up stuck — giving up auto-recovery and PAUSING it. Manual \
         intervention required: investigate the (likely corrupt) session, then `restart_instance` \
         fresh / unpause."
    );
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &msg,
        false,
    );
}

/// Page the operator that an agent's Claude authorization has expired (detected
/// via the existing `AgentState::AuthError`). Unlike a stuck Resume this is NOT
/// auto-recoverable — a fresh respawn would be just as unauthenticated — so the
/// watchdog deliberately SKIPS auto-Fresh and asks the operator to re-login.
/// Mirrors `notify_escalation`'s Error-severity, Sleep-penetrating
/// `notify_all_escalation_channels` path; fire-once via the `auth_notified`
/// latch in `run`. Message names the only effective action (re-authenticate).
fn notify_auth_expiry(name: &str) {
    tracing::error!(
        target: TARGET,
        agent = %name,
        "respawn-stuck watchdog: agent in AuthError on a Resume — Claude auth expired, paging operator (auto-Fresh skipped)"
    );
    let msg = format!(
        "🔑 {name}: Claude 授權過期（偵測到 AuthError）。自動重啟無法修復授權，已略過 auto-Fresh —— \
         只有操作者能重新登入。請在該 agent 的 pane 重新授權（執行 `claude` / `/login` 完成登入），\
         授權後再 `restart_instance` resume 即可。"
    );
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &msg,
        false,
    );
}

/// Page the operator that a single auto-Fresh restart's SPAWN failed (agent
/// likely gone), so a failed recovery is surfaced, not silently dropped.
fn notify_restart_failed(name: &str) {
    let msg = format!(
        "⚠️ {name}: respawn-stuck watchdog attempted an auto-Fresh restart but the SPAWN failed — \
         the agent may be gone. Manual operator check needed."
    );
    crate::channel::notify_all_escalation_channels(
        name,
        crate::channel::NotifySeverity::Error,
        &msg,
        false,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    // ── detection predicate ──────────────────────────────────────────────

    #[test]
    fn stuck_resume_fires_on_resume_starting_past_both_timeouts() {
        assert!(is_stuck_resume(
            SpawnMode::Resume,
            AgentState::Starting,
            Duration::from_secs(61),
            Duration::from_secs(61),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    #[test]
    fn stuck_resume_fires_on_restarting_too() {
        assert!(is_stuck_resume(
            SpawnMode::Resume,
            AgentState::Restarting,
            Duration::from_secs(120),
            Duration::from_secs(120),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    #[test]
    fn fresh_spawn_never_fires_even_when_stuck() {
        // The load-bearing false-kill guard: a slow Fresh boot is NEVER
        // force-restarted, no matter how long it sits in Starting.
        assert!(!is_stuck_resume(
            SpawnMode::Fresh,
            AgentState::Starting,
            Duration::from_secs(600),
            Duration::from_secs(600),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    #[test]
    fn ready_state_never_fires() {
        // An agent that reached a ready state (Idle) is not stuck, even on a
        // Resume spawn — `since`/`silence` are irrelevant.
        assert!(!is_stuck_resume(
            SpawnMode::Resume,
            AgentState::Idle,
            Duration::from_secs(600),
            Duration::from_secs(600),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    #[test]
    fn slow_but_emitting_resume_is_spared() {
        // In Starting past the time-in-state threshold, BUT still emitting output
        // (silence below threshold) → a slow-but-alive resume, NOT stuck. The
        // dual-elapsed gate is exactly the "no productive output" requirement.
        assert!(!is_stuck_resume(
            SpawnMode::Resume,
            AgentState::Starting,
            Duration::from_secs(120),
            Duration::from_secs(5),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    #[test]
    fn within_timeout_never_fires() {
        assert!(!is_stuck_resume(
            SpawnMode::Resume,
            AgentState::Starting,
            Duration::from_secs(30),
            Duration::from_secs(30),
            RESPAWN_STUCK_TIMEOUT,
        ));
    }

    // ── auth-expiry classification (route away from auto-Fresh) ─────────

    #[test]
    fn classify_auth_error_on_resume_held_past_window_is_auth_expired() {
        // An AuthError agent spawned via Resume, held CONTINUOUSLY past the
        // stability window, is a real auth expiry → operator page, NOT the
        // auto-Fresh ladder (respawn can't re-authenticate).
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::AuthError,
                Duration::from_secs(120), // > 90s window
                Duration::from_secs(120),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::AuthExpired
        );
    }

    /// t-...30532-0 (reviewer5 REJECTED, flipped): a SHORT-lived AuthError blip
    /// (under the stability window) must NOT immediately page the operator or
    /// skip auto-Fresh — `AuthError` is content-FP-prone (~31s self-heal seen), so
    /// it must fall through and be left to self-heal. Pre-fix this returned
    /// `AuthExpired` off a 1s signal (the merged defect). `AuthError` isn't
    /// `Starting`/`Restarting`, so `is_stuck_resume` can't catch it either → `Ignore`.
    #[test]
    fn classify_auth_error_under_stability_window_is_ignored_not_paged() {
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::AuthError,
                Duration::from_secs(1), // << 90s window — a transient blip
                Duration::from_secs(1),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::Ignore,
            "a sub-window AuthError blip must self-heal, not page + skip auto-Fresh"
        );
    }

    /// Boundary pin on the reused supervisor window (single source of truth): at
    /// exactly `AUTH_ERROR_NOTIFY_STABILITY` it pages; one tick under, it doesn't.
    #[test]
    fn classify_auth_error_window_boundary() {
        let window = crate::daemon::supervisor::AUTH_ERROR_NOTIFY_STABILITY;
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::AuthError,
                window,
                window,
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::AuthExpired,
            "held == window → page"
        );
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::AuthError,
                window - Duration::from_millis(1),
                window - Duration::from_millis(1),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::Ignore,
            "held one tick under window → do not page yet"
        );
    }

    #[test]
    fn classify_auth_error_non_resume_is_ignored_by_watchdog() {
        // Gated to the watchdog's Resume domain (the supervisor's general
        // AuthError flow owns non-Resume auth pages); a Fresh AuthError agent is
        // not this handler's concern.
        assert_eq!(
            classify(
                SpawnMode::Fresh,
                AgentState::AuthError,
                Duration::from_secs(120),
                Duration::from_secs(120),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::Ignore
        );
    }

    #[test]
    fn classify_stuck_resume_still_fires_no_regression() {
        // The existing stuck-Resume behaviour is preserved: a non-auth stuck
        // Resume still routes to the auto-Fresh ladder.
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::Starting,
                Duration::from_secs(61),
                Duration::from_secs(61),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::StuckResume
        );
    }

    #[test]
    fn classify_healthy_and_slow_fresh_are_ignored() {
        // An idle (healthy) agent and a slow-but-healthy Fresh boot are both
        // left alone — no false auth page, no false auto-Fresh.
        assert_eq!(
            classify(
                SpawnMode::Resume,
                AgentState::Idle,
                Duration::from_secs(600),
                Duration::from_secs(600),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::Ignore
        );
        assert_eq!(
            classify(
                SpawnMode::Fresh,
                AgentState::Starting,
                Duration::from_secs(600),
                Duration::from_secs(600),
                RESPAWN_STUCK_TIMEOUT,
            ),
            Situation::Ignore
        );
    }

    // ── auth-expiry fire-once-until-recovered latch ─────────────────────

    fn names(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn auth_latch_pages_each_agent_once_then_stays_silent() {
        let mut latch = HashSet::new();
        let auth = names(&["a", "b"]);
        // First detection pages both (order-independent).
        let mut first = auth_notify_targets(&auth, &mut latch);
        first.sort();
        assert_eq!(first, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(latch, names(&["a", "b"]));
        // Still auth-expired next tick → no re-page.
        let second = auth_notify_targets(&auth, &mut latch);
        assert!(
            second.is_empty(),
            "fire-once: no re-page while still expired"
        );
        assert_eq!(latch, names(&["a", "b"]));
    }

    #[test]
    fn auth_latch_clears_on_recovery_then_repages_on_reexpiry() {
        let mut latch = names(&["a"]);
        // "a" recovered (no longer auth-expired) → latch cleared, nothing paged.
        let recovered = auth_notify_targets(&HashSet::new(), &mut latch);
        assert!(recovered.is_empty());
        assert!(latch.is_empty(), "recovered agent forgiven");
        // Later re-expiry pages again (new episode).
        let reexpiry = auth_notify_targets(&names(&["a"]), &mut latch);
        assert_eq!(reexpiry, vec!["a".to_string()]);
    }

    #[test]
    fn auth_latch_only_pages_newly_expired_agents() {
        let mut latch = names(&["a"]); // "a" already paged
        let now = names(&["a", "b"]); // "b" newly expired
        let targets = auth_notify_targets(&now, &mut latch);
        assert_eq!(targets, vec!["b".to_string()], "only the new one pages");
        assert_eq!(latch, names(&["a", "b"]));
    }

    // ── bounded-retry / cooldown / fire-once-escalate state machine ──────

    #[test]
    fn decide_first_detection_fires() {
        let mut rec = RetryRecord::new();
        let now = Instant::now();
        let action = decide(&mut rec, now, RESPAWN_RETRY_COOLDOWN, RESPAWN_MAX_RETRIES);
        assert_eq!(action, Action::Fire(1));
        assert_eq!(rec.count, 1);
        assert_eq!(rec.last_retry_at, Some(now));
    }

    #[test]
    fn decide_skips_within_cooldown() {
        let base = Instant::now();
        let mut rec = RetryRecord {
            count: 1,
            last_retry_at: Some(base),
            escalated: false,
        };
        // 30s after a fire, cooldown (90s) still active → no re-fire, no mutation.
        let action = decide(
            &mut rec,
            base + Duration::from_secs(30),
            RESPAWN_RETRY_COOLDOWN,
            RESPAWN_MAX_RETRIES,
        );
        assert_eq!(action, Action::None);
        assert_eq!(rec.count, 1);
    }

    #[test]
    fn decide_fires_again_past_cooldown_under_cap() {
        let base = Instant::now();
        let mut rec = RetryRecord {
            count: 1,
            last_retry_at: Some(base),
            escalated: false,
        };
        let later = base + RESPAWN_RETRY_COOLDOWN + Duration::from_secs(1);
        let action = decide(&mut rec, later, RESPAWN_RETRY_COOLDOWN, RESPAWN_MAX_RETRIES);
        assert_eq!(action, Action::Fire(2));
        assert_eq!(rec.count, 2);
        assert_eq!(rec.last_retry_at, Some(later));
    }

    #[test]
    fn decide_escalates_once_at_cap_then_latches() {
        let base = Instant::now();
        let mut rec = RetryRecord {
            count: RESPAWN_MAX_RETRIES,
            last_retry_at: Some(base),
            escalated: false,
        };
        let later = base + RESPAWN_RETRY_COOLDOWN + Duration::from_secs(1);
        // Cap reached + past cooldown → Escalate, and latch.
        assert_eq!(
            decide(&mut rec, later, RESPAWN_RETRY_COOLDOWN, RESPAWN_MAX_RETRIES),
            Action::Escalate
        );
        assert!(rec.escalated);
        // Subsequent detections are silent (fire-once page).
        let later2 = later + RESPAWN_RETRY_COOLDOWN + Duration::from_secs(1);
        assert_eq!(
            decide(
                &mut rec,
                later2,
                RESPAWN_RETRY_COOLDOWN,
                RESPAWN_MAX_RETRIES
            ),
            Action::None
        );
    }

    // ── handler plumbing ────────────────────────────────────────────────

    #[test]
    fn name_matches_module() {
        assert_eq!(RespawnWatchdogHandler::new().name(), "respawn_watchdog");
    }

    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = std::env::temp_dir().join(format!(
            "agend-respawn-wd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        RespawnWatchdogHandler::new().run(&ctx);
        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_drops_records_for_absent_agents() {
        let wd = RespawnWatchdogHandler::new();
        {
            let mut r = wd.retries.lock();
            r.insert("gone".to_string(), RetryRecord::new());
            r.insert("still-stuck".to_string(), RetryRecord::new());
        }
        let stuck = vec!["still-stuck".to_string()];
        let mut live = HashSet::new();
        live.insert("still-stuck".to_string()); // "gone" is no longer live
        wd.gc_records(&stuck, &live);
        let r = wd.retries.lock();
        assert!(!r.contains_key("gone"), "absent agent's record evicted");
        assert!(r.contains_key("still-stuck"), "stuck agent's record kept");
    }
}
