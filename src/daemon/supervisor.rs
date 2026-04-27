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
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How often the supervisor wakes to scan the registry.
const TICK: Duration = Duration::from_secs(10);
/// Vterm tail size pushed to Telegram when a stall is detected.
const TAIL_LINES: usize = 40;

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
    loop {
        thread::sleep(TICK);
        tick(&home, &registry);
    }
}

/// One iteration of the supervisor loop. Public for tests.
fn tick(home: &std::path::Path, registry: &AgentRegistry) {
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
            let mut core = match core.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };

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
}
