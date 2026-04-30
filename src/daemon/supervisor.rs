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

/// Per-agent notify tracking: last notify time + consecutive error count.
pub(crate) struct NotifyTrack {
    last_at: Instant,
    consecutive: u32,
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
    loop {
        thread::sleep(TICK);
        tick(&home, &registry, &mut notify_tracks);
    }
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
}
