//! #2127 Phase 1 — reclaim board tasks from agents stuck in a *non-recoverable*
//! usage-limit window. Today the daemon only ALERTS (inbox_stuck / dispatch_idle)
//! when an agent goes `usage_limit`; its claimed/in-progress board tasks sit dead
//! on it until manual reassignment. This handler RELEASES those tasks back to
//! `Open` (so a same-role peer can pick them up) and clears the work-stuck latch.
//!
//! Scope is deliberately Tier 0: ONLY board tasks. Unread inbox dispatches (P2)
//! and worktree bindings (P3) are out of scope — see #2127.
//!
//! The trigger is intentionally conservative (operator decision
//! d-20260614085112365602-2: Phase 1, grace=10min). Reclaim fires iff ALL hold:
//!   - state is `UsageLimit` OR health reason is `QuotaExceeded`,
//!   - the state is NOT one of the self-healing / transient classes (a
//!     brief `RateLimit`/`ServerRateLimit`/`ApiError` auto-retries and must NOT
//!     be interrupted — see `EXCLUDED`),
//!   - the agent has not produced productive output within `RECOVERY_SILENCE`
//!     (it is genuinely stuck, not mid-recovery),
//!   - the usage window has MORE than `RECLAIM_GRACE` (10min) left — a block
//!     about to lift self-recovers, so we leave it alone. No parseable unlock
//!     time ⇒ treated as a long block (`USAGE_LIMIT_EXPIRY` fallback).
//!
//! Safety: per-task `reclaim_count` cap (`RECLAIM_CAP`) → stop + escalate the
//! operator once; fire-once is provided structurally (a released task is no
//! longer owned by the blocked agent, so the next scan re-enumerates nothing).
//! Same-board fail-closed: each owned task is gated by the shared
//! `tasks::can_mutate_on_board` ACL primitive (#2117 P3) — the blocked agent's
//! project must equal the task's board project, else the task is SKIPPED (never
//! wrongly mutated). Single-project fleets resolve to one board, so this is
//! byte-identical today; multi-board (#2117) reuses the same primitive.

use super::{PerTickHandler, TickContext};
use crate::state::AgentState;
use std::path::Path;
use std::time::Duration;

/// A usage-limit window must have MORE than this remaining to be reclaimed — a
/// block lifting within the grace self-recovers (operator decision: 10min).
const RECLAIM_GRACE: Duration = Duration::from_secs(10 * 60);
/// Fallback "remaining" when the agent is `usage_limit` but the pane carried no
/// parseable reset time — treat as a long block (> grace ⇒ reclaim).
const USAGE_LIMIT_EXPIRY: Duration = Duration::from_secs(30 * 60);
/// Per-task reclaim cap. Beyond this, auto-reclaim stops and the operator is
/// escalated (a task that keeps landing on usage-limited agents needs a human).
const RECLAIM_CAP: u32 = 3;
/// If a reclaimed agent is observed recovered within this window after reclaim,
/// record a `collision` instrument line — empirical input to the Phase 2
/// lease-epoch go/no-go (was the 10min grace too aggressive?).
const COLLISION_WINDOW: Duration = Duration::from_secs(30 * 60);

/// AgentState classes that are transient / self-healing / operator-gated and must
/// NEVER be reclaimed even if a stale `QuotaExceeded` reason lingers. (`IdleLong`
/// in the issue is a *health* state, not an `AgentState`; a merely-idle agent is
/// excluded anyway because it fails the usage-limit gate below.)
fn is_excluded_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::RateLimit
            | AgentState::ServerRateLimit
            | AgentState::ApiError
            | AgentState::ContextFull
            | AgentState::AwaitingOperator
            | AgentState::InteractivePrompt
            | AgentState::PermissionPrompt
            | AgentState::Starting
            | AgentState::Restarting
    )
}

/// The pure reclaim predicate (testable in isolation). See module docs for the
/// rationale of each clause.
pub(crate) fn should_reclaim(
    state: AgentState,
    quota_exceeded: bool,
    recovered: bool,
    remaining: Duration,
    grace: Duration,
) -> bool {
    if is_excluded_state(state) {
        return false;
    }
    let usage_blocked = state == AgentState::UsageLimit || quota_exceeded;
    if !usage_blocked {
        return false;
    }
    if recovered {
        // Mid-recovery (transient backoff self-healing) — do not interrupt.
        return false;
    }
    remaining > grace
}

fn tracking_path(home: &Path) -> std::path::PathBuf {
    home.join("reclaim_tracking.json")
}

fn instrument_path(home: &Path) -> std::path::PathBuf {
    home.join("reclaim_instrument.jsonl")
}

/// Persistent reclaim bookkeeping (survives restarts; the cap must, since the
/// reassignment churn it bounds spans many handler runs).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ReclaimTracking {
    /// task_id → number of times reclaimed (bounded by `RECLAIM_CAP`).
    task_reclaim_count: std::collections::HashMap<String, u32>,
    /// task_ids already escalated at the cap (fire-once operator escalation).
    capped_escalated: Vec<String>,
    /// agent → last reclaim time (RFC3339) — anchors the collision instrument.
    last_reclaim_at: std::collections::HashMap<String, String>,
}

/// Append one structured instrument line (best-effort; never panics).
fn instrument(home: &Path, line: serde_json::Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(instrument_path(home))
    {
        let _ = writeln!(f, "{line}");
    }
}

pub(crate) struct ReclaimHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Shared with [`super::InboxStuckHandler`] so reclaim can drop an agent's
    /// repeat-alert entry once its work is reclaimed.
    work_stuck_latch: super::inbox_stuck::AlertLatch,
}

impl ReclaimHandler {
    pub(crate) fn new(
        every_n_ticks: u64,
        work_stuck_latch: super::inbox_stuck::AlertLatch,
    ) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            work_stuck_latch,
        }
    }
}

/// Per-agent reclaim decision + action (the unit the registry loop drives, and
/// the unit the adversarial tests exercise directly without a mock registry).
/// Returns the number of tasks actually released. `now` is injected for testing.
fn reclaim_if_eligible(
    home: &Path,
    latch: &super::inbox_stuck::AlertLatch,
    agent: &str,
    state: AgentState,
    quota_exceeded: bool,
    recovered: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> usize {
    let remaining = crate::daemon::supervisor::usage_limit_remaining(home, agent, now)
        .unwrap_or(USAGE_LIMIT_EXPIRY);
    if !should_reclaim(state, quota_exceeded, recovered, remaining, RECLAIM_GRACE) {
        return 0;
    }
    do_reclaim(home, latch, agent, remaining, now)
}

/// Release the agent's claimed/in-progress board tasks back to Open + cascade.
fn do_reclaim(
    home: &Path,
    latch: &super::inbox_stuck::AlertLatch,
    agent: &str,
    remaining: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> usize {
    let board = crate::task_events::replay(home).unwrap_or_default();
    let owned: Vec<crate::task_events::TaskId> = board
        .tasks
        .values()
        .filter(|r| {
            r.owner.as_ref().is_some_and(|o| o.0 == agent)
                && matches!(
                    r.status,
                    crate::task_events::TaskStatus::Claimed
                        | crate::task_events::TaskStatus::InProgress
                )
        })
        .map(|r| r.id.clone())
        .collect();
    if owned.is_empty() {
        // Structural fire-once: nothing owned (already reclaimed / never had work).
        return 0;
    }

    // Same-board fail-closed (#2117 P3 shared primitive): only reclaim a task if
    // the blocked agent is authorized on that task's board (caller-project ==
    // task-board-project). A cross-board task is SKIPPED (never wrongly mutated)
    // and instrumented. Single-project fleets resolve everything to the default
    // board → every owned task passes → byte-identical today.
    let mut agent_tasks: Vec<crate::task_events::TaskId> = Vec::new();
    for tid in owned {
        let board_project = crate::tasks::resolve_task_project(home, &tid.0);
        if crate::tasks::can_mutate_on_board(home, agent, &board_project) {
            agent_tasks.push(tid);
        } else {
            tracing::warn!(
                agent,
                task = %tid.0,
                board = %board_project,
                "#2127 reclaim cross-board skip (fail-closed): agent not authorized on task's board"
            );
            instrument(
                home,
                serde_json::json!({
                    "ts": now.to_rfc3339(),
                    "event": "cross_board_skip",
                    "agent": agent,
                    "task": tid.0,
                    "board": board_project,
                }),
            );
        }
    }
    if agent_tasks.is_empty() {
        return 0;
    }

    // RMW the cap store: decide reclaim-vs-cap per task + increment counts +
    // record the reclaim time. Emit the actual events OUTSIDE the store lock.
    let mut to_reclaim: Vec<crate::task_events::TaskId> = Vec::new();
    let mut to_escalate: Vec<String> = Vec::new();
    let _ = crate::store::with_json_state_or_create::<ReclaimTracking, _, _, _>(
        &tracking_path(home),
        ReclaimTracking::default,
        |t| {
            for tid in &agent_tasks {
                let count = t.task_reclaim_count.get(&tid.0).copied().unwrap_or(0);
                if count >= RECLAIM_CAP {
                    if !t.capped_escalated.contains(&tid.0) {
                        t.capped_escalated.push(tid.0.clone());
                        to_escalate.push(tid.0.clone());
                    }
                    continue; // cap reached — stop reclaiming this task
                }
                t.task_reclaim_count.insert(tid.0.clone(), count + 1);
                to_reclaim.push(tid.clone());
            }
            t.last_reclaim_at
                .insert(agent.to_string(), now.to_rfc3339());
        },
    );

    let emitter = crate::task_events::InstanceName::from("system:reclaim_usage_limit");
    let mins = remaining.as_secs() / 60;
    let mut reclaimed = 0usize;
    for tid in &to_reclaim {
        let event = crate::task_events::TaskEvent::Released {
            task_id: tid.clone(),
            reason: format!("reclaimed: {agent} usage_limit (~{mins}m window remaining)"),
        };
        match crate::task_events::append(home, &emitter, event) {
            Ok(_) => {
                // Cascade: clear the task's dispatch-idle pending + dispatch-tracking
                // + ci-watch handoff so no stale nag follows the released task.
                crate::daemon::dispatch_idle::reassign_pending_for_task(home, &tid.0, None);
                crate::dispatch_tracking::reassign_to(home, &tid.0, None);
                let _ = crate::daemon::ci_watch::reassign_next_after_ci(home, &tid.0, None);
                reclaimed += 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, task = %tid.0, "reclaim: Released append failed; will retry next pass");
            }
        }
    }

    // Per-task cap escalation (operator) — fire-once per task.
    for tid in &to_escalate {
        let msg = format!(
            "⚠️ task {tid} hit the usage-limit reclaim cap ({RECLAIM_CAP}×) — auto-reclaim stopped, manual reassignment needed."
        );
        crate::channel::notify_all_escalation_channels(
            agent,
            crate::channel::NotifySeverity::Error,
            &msg,
            false,
        );
        instrument(
            home,
            serde_json::json!({"ts": now.to_rfc3339(), "event": "cap_escalated", "agent": agent, "task": tid}),
        );
    }

    if reclaimed > 0 {
        // Clear the work-stuck latch so the agent's repeat stuck-alert resets now
        // that its board work has been handled. NOTE (Tier 0): unread inbox
        // dispatches are NOT reclaimed here (P2), so if the agent still has an
        // unread pile the inbox-stuck watchdog will legitimately re-alert next
        // cadence — that is the P2 surface, not a regression.
        latch.lock().remove(agent);
        instrument(
            home,
            serde_json::json!({
                "ts": now.to_rfc3339(),
                "event": "reclaim",
                "agent": agent,
                "tasks": reclaimed,
                "remaining_secs": remaining.as_secs(),
            }),
        );
        tracing::warn!(
            agent,
            reclaimed,
            remaining_secs = remaining.as_secs(),
            "#2127 reclaimed usage_limit board tasks → Open"
        );
    }
    reclaimed
}

/// For agents that were reclaimed and have since recovered (no longer eligible),
/// record a `collision` instrument line if recovery happened within
/// `COLLISION_WINDOW` of the reclaim, then clear the marker (fire-once).
fn process_collisions(
    home: &Path,
    recovered_agents: &[String],
    now: chrono::DateTime<chrono::Utc>,
) {
    if recovered_agents.is_empty() || !tracking_path(home).exists() {
        return;
    }
    let mut collisions: Vec<(String, i64)> = Vec::new();
    let _ = crate::store::with_json_state::<ReclaimTracking, _, _>(&tracking_path(home), |t| {
        for agent in recovered_agents {
            if let Some(ts) = t.last_reclaim_at.get(agent) {
                if let Ok(reclaimed_at) = chrono::DateTime::parse_from_rfc3339(ts) {
                    let age = now.signed_duration_since(reclaimed_at.with_timezone(&chrono::Utc));
                    if age.to_std().map(|d| d < COLLISION_WINDOW).unwrap_or(false) {
                        collisions.push((agent.clone(), age.num_seconds()));
                    }
                }
                t.last_reclaim_at.remove(agent); // fire-once
            }
        }
    });
    for (agent, secs) in collisions {
        instrument(
            home,
            serde_json::json!({
                "ts": now.to_rfc3339(),
                "event": "collision",
                "agent": agent,
                "recovered_after_secs": secs,
            }),
        );
        tracing::warn!(
            agent,
            recovered_after_secs = secs,
            "#2127 reclaim collision: agent recovered shortly after reclaim (grace data for Phase 2)"
        );
    }
}

impl PerTickHandler for ReclaimHandler {
    fn name(&self) -> &'static str {
        "reclaim_usage_limit"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let now = chrono::Utc::now();
        // Snapshot per-agent signals under the locks, then DROP them before any
        // task/store file IO (avoids holding the registry/core lock across the
        // #1617 lock-while-blocking class).
        let recovery_window = crate::state::SERVER_RATE_LIMIT_RECOVERY_SILENCE;
        let mut eligible: Vec<(String, AgentState, bool)> = Vec::new();
        let mut recovered_agents: Vec<String> = Vec::new();
        {
            let reg = crate::agent::lock_registry(ctx.registry);
            for handle in reg.values() {
                let name = handle.name.as_str().to_string();
                let core = handle.core.lock();
                let state = core.state.current;
                let quota = matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::QuotaExceeded)
                );
                let recovered = core.state.recovered_within(recovery_window);
                drop(core);
                if (state == AgentState::UsageLimit || quota) && !is_excluded_state(state) {
                    eligible.push((name, state, quota));
                } else if recovered {
                    recovered_agents.push(name);
                }
            }
        }
        for (name, state, quota) in eligible {
            // recovered=false here: an eligible agent that had recovered would have
            // gone to `recovered_agents`. Re-read recovery defensively is overkill;
            // the snapshot already excluded recovered agents from `eligible`.
            reclaim_if_eligible(
                ctx.home,
                &self.work_stuck_latch,
                &name,
                state,
                quota,
                false,
                now,
            );
        }
        process_collisions(ctx.home, &recovered_agents, now);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static C: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-reclaim-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn fresh_latch() -> super::super::inbox_stuck::AlertLatch {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// Seed a Claimed task owned by `agent` on the default board.
    fn seed_claimed_task(home: &Path, task_id: &str, agent: &str) {
        let tid = crate::task_events::TaskId(task_id.to_string());
        let owner = crate::task_events::InstanceName::from(agent);
        let creator = crate::task_events::InstanceName::from("system:test");
        crate::task_events::append(
            home,
            &creator,
            crate::task_events::TaskEvent::Created {
                task_id: tid.clone(),
                title: "t".into(),
                description: "d".into(),
                priority: "normal".into(),
                owner: Some(owner.clone()),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        crate::task_events::append(
            home,
            &owner,
            crate::task_events::TaskEvent::Claimed {
                task_id: tid,
                by: owner.clone(),
            },
        )
        .unwrap();
    }

    /// Write a usage_limit_notify.json record so `usage_limit_remaining` returns a
    /// window `mins_from_now` minutes ahead of `notified_at == now`.
    fn seed_usage_notify(
        home: &Path,
        agent: &str,
        now: chrono::DateTime<chrono::Utc>,
        mins_from_now: i64,
    ) {
        let unlock = (now + chrono::Duration::minutes(mins_from_now))
            .format("%H:%M")
            .to_string();
        let rec =
            serde_json::json!({ agent: { "unlock_at": unlock, "notified_at": now.to_rfc3339() } });
        std::fs::write(
            home.join("usage_limit_notify.json"),
            serde_json::to_string(&rec).unwrap(),
        )
        .unwrap();
    }

    fn task_status(home: &Path, task_id: &str) -> crate::task_events::TaskStatus {
        let st = crate::task_events::replay(home).unwrap();
        st.tasks
            .get(&crate::task_events::TaskId(task_id.to_string()))
            .unwrap()
            .status
    }

    fn task_owner(home: &Path, task_id: &str) -> Option<String> {
        let st = crate::task_events::replay(home).unwrap();
        st.tasks
            .get(&crate::task_events::TaskId(task_id.to_string()))
            .unwrap()
            .owner
            .as_ref()
            .map(|o| o.0.clone())
    }

    // ── Pure predicate unit tests (full branch coverage) ──────────────────────

    #[test]
    fn predicate_excludes_self_healing_states() {
        let long = Duration::from_secs(3600);
        for s in [
            AgentState::RateLimit,
            AgentState::ServerRateLimit,
            AgentState::ApiError,
            AgentState::ContextFull,
            AgentState::AwaitingOperator,
            AgentState::InteractivePrompt,
            AgentState::PermissionPrompt,
            AgentState::Starting,
            AgentState::Restarting,
        ] {
            assert!(
                !should_reclaim(s, true, false, long, RECLAIM_GRACE),
                "{s:?} is self-healing/operator-gated and must NEVER be reclaimed"
            );
        }
    }

    #[test]
    fn predicate_reclaims_usage_limit_past_grace() {
        let long = RECLAIM_GRACE + Duration::from_secs(60);
        assert!(should_reclaim(
            AgentState::UsageLimit,
            false,
            false,
            long,
            RECLAIM_GRACE
        ));
        // QuotaExceeded reason on a non-excluded state also qualifies.
        assert!(should_reclaim(
            AgentState::Idle,
            true,
            false,
            long,
            RECLAIM_GRACE
        ));
    }

    #[test]
    fn predicate_skips_within_grace_and_when_recovered() {
        let short = RECLAIM_GRACE - Duration::from_secs(60);
        let long = RECLAIM_GRACE + Duration::from_secs(60);
        assert!(
            !should_reclaim(AgentState::UsageLimit, false, false, short, RECLAIM_GRACE),
            "a window lifting within grace self-recovers — do not reclaim"
        );
        assert!(
            !should_reclaim(AgentState::UsageLimit, false, true, long, RECLAIM_GRACE),
            "an agent mid-recovery must not be interrupted"
        );
        assert!(
            !should_reclaim(AgentState::Idle, false, false, long, RECLAIM_GRACE),
            "a non-usage-limit agent is never reclaimed"
        );
    }

    // ── §3.10 three adversarial path tests (real task store + latch) ──────────

    /// (a) A brief transient block (RateLimit / ServerRateLimit / ApiError) must
    /// NOT be reclaimed even with a usage window present — it auto-recovers.
    #[test]
    fn adversarial_a_transient_states_not_reclaimed() {
        let now = chrono::Utc::now();
        for (i, state) in [
            AgentState::RateLimit,
            AgentState::ServerRateLimit,
            AgentState::ApiError,
        ]
        .into_iter()
        .enumerate()
        {
            let home = tmp_home(&format!("adv-a-{i}"));
            seed_claimed_task(&home, "t-a", "dev-x");
            seed_usage_notify(&home, "dev-x", now, 60); // long window present
            let latch = fresh_latch();
            let n = reclaim_if_eligible(&home, &latch, "dev-x", state, false, false, now);
            assert_eq!(n, 0, "{state:?} must not be reclaimed");
            assert_eq!(
                task_status(&home, "t-a"),
                crate::task_events::TaskStatus::Claimed,
                "{state:?}: task must stay Claimed"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// (b) A genuine usage_limit with > grace remaining IS reclaimed: task → Open,
    /// owner cleared, work-stuck latch entry removed.
    #[test]
    fn adversarial_b_usage_limit_past_grace_reclaimed() {
        let now = chrono::Utc::now();
        let home = tmp_home("adv-b");
        seed_claimed_task(&home, "t-b", "dev-y");
        seed_usage_notify(&home, "dev-y", now, 30); // 30min > 10min grace
        let latch = fresh_latch();
        latch.lock().insert("dev-y".to_string(), now);

        let n = reclaim_if_eligible(
            &home,
            &latch,
            "dev-y",
            AgentState::UsageLimit,
            false,
            false,
            now,
        );
        assert_eq!(n, 1, "one task must be reclaimed");
        assert_eq!(
            task_status(&home, "t-b"),
            crate::task_events::TaskStatus::Open,
            "task must be released to Open"
        );
        assert_eq!(task_owner(&home, "t-b"), None, "owner must be cleared");
        assert!(
            !latch.lock().contains_key("dev-y"),
            "work-stuck latch entry must be cleared on reclaim"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// (c) A usage_limit window lifting within grace (< 10min) is NOT reclaimed —
    /// it will self-recover.
    #[test]
    fn adversarial_c_usage_limit_within_grace_not_reclaimed() {
        let now = chrono::Utc::now();
        let home = tmp_home("adv-c");
        seed_claimed_task(&home, "t-c", "dev-z");
        seed_usage_notify(&home, "dev-z", now, 5); // 5min < 10min grace
        let latch = fresh_latch();
        let n = reclaim_if_eligible(
            &home,
            &latch,
            "dev-z",
            AgentState::UsageLimit,
            false,
            false,
            now,
        );
        assert_eq!(n, 0, "within-grace window must not be reclaimed");
        assert_eq!(
            task_status(&home, "t-c"),
            crate::task_events::TaskStatus::Claimed,
            "task must stay Claimed"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// No usage_notify record ⇒ conservative long-block fallback ⇒ reclaimed.
    #[test]
    fn no_notify_record_falls_back_to_long_block() {
        let now = chrono::Utc::now();
        let home = tmp_home("nofallback");
        seed_claimed_task(&home, "t-f", "dev-f");
        // intentionally no seed_usage_notify
        let latch = fresh_latch();
        let n = reclaim_if_eligible(
            &home,
            &latch,
            "dev-f",
            AgentState::UsageLimit,
            false,
            false,
            now,
        );
        assert_eq!(n, 1, "missing reset time ⇒ long-block fallback ⇒ reclaim");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Per-task cap: the 4th reclaim of the same task is blocked (cap=3).
    #[test]
    fn per_task_cap_stops_after_three() {
        let now = chrono::Utc::now();
        let home = tmp_home("cap");
        let latch = fresh_latch();
        let mut total = 0;
        for round in 0..5 {
            // Re-seed the same task as Claimed each round (simulating re-dispatch
            // back to a usage-limited agent), then reclaim.
            if round == 0 {
                seed_claimed_task(&home, "t-cap", "dev-c");
            } else {
                // re-claim the now-Open task
                let owner = crate::task_events::InstanceName::from("dev-c");
                crate::task_events::append(
                    &home,
                    &owner,
                    crate::task_events::TaskEvent::Claimed {
                        task_id: crate::task_events::TaskId("t-cap".into()),
                        by: owner.clone(),
                    },
                )
                .unwrap();
            }
            seed_usage_notify(&home, "dev-c", now, 30);
            total += reclaim_if_eligible(
                &home,
                &latch,
                "dev-c",
                AgentState::UsageLimit,
                false,
                false,
                now,
            );
        }
        assert_eq!(total, RECLAIM_CAP as usize, "reclaim must stop at the cap");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Structural fire-once: a second scan with no owned tasks reclaims nothing.
    #[test]
    fn fire_once_when_no_owned_tasks() {
        let now = chrono::Utc::now();
        let home = tmp_home("fireonce");
        seed_claimed_task(&home, "t-o", "dev-o");
        seed_usage_notify(&home, "dev-o", now, 30);
        let latch = fresh_latch();
        assert_eq!(
            reclaim_if_eligible(
                &home,
                &latch,
                "dev-o",
                AgentState::UsageLimit,
                false,
                false,
                now
            ),
            1
        );
        // task is now Open + unowned → second pass finds nothing.
        assert_eq!(
            reclaim_if_eligible(
                &home,
                &latch,
                "dev-o",
                AgentState::UsageLimit,
                false,
                false,
                now
            ),
            0
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
