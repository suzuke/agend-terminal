//! #2044: inject-delivery verification — a safety net against an actionable
//! dispatch wake being SWALLOWED by an operator-driven interactive dialog
//! (the incident: a `/model` picker was open in the agent's pane, the injected
//! dispatch's keystrokes went to the picker, the prompt never submitted, and
//! the dispatch was lost — discovered only because the operator noticed the
//! agent never reacted).
//!
//! Signal: a landed actionable inject submits a prompt → the backend fires a
//! `UserPromptSubmit` hook. A dialog-swallowed inject submits NOTHING → no
//! such hook. So: when an actionable wake is injected, record the time; if no
//! `UserPromptSubmit` is observed within [`VERIFY_WINDOW`], re-deliver ONCE and
//! WARN, then give up (latched — never a retry storm; noise discipline #2008).
//!
//! Per-backend honesty: this can only verify backends that emit hooks. The arm
//! is gated on the agent already having a hook-shadow entry (empirical proof
//! hooks flow for it — claude today). A non-hook backend never arms, so it can
//! never be falsely re-injected. In-memory state: a daemon restart simply
//! forgets in-flight verifications (the durable re-nudge for dispatches is the
//! #1888 ci-handoff track on a longer horizon — this is the fast 30s
//! delivery-physical-landing net, complementary).

use std::collections::HashMap;
use std::path::Path;

use parking_lot::Mutex;

/// No `UserPromptSubmit` within this wall-clock window after an actionable
/// inject ⇒ treat as not-delivered. 30s comfortably outlasts a normal
/// submit→hook round-trip while still reacting fast to a swallowed dispatch.
const VERIFY_WINDOW_MS: u64 = 30_000;

#[derive(Debug, Clone)]
struct Pending {
    /// When the (most recent) actionable wake was injected (epoch ms).
    injected_at_ms: u64,
    /// The wake text, re-injected verbatim on the one re-delivery attempt.
    text: String,
    /// True once the single re-delivery has fired (the latch).
    redelivered: bool,
}

fn store() -> &'static Mutex<HashMap<String, Pending>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, Pending>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    crate::daemon::heartbeat_pair::now_ms()
}

/// Arm delivery-verification for an actionable wake just injected to `agent`.
/// No-op unless the agent has a hook-shadow entry (only hook-emitting backends
/// can be verified — never falsely re-inject a non-hook backend). A second arm
/// for the same agent replaces the first (we verify the latest wake; a newer
/// dispatch landing implies the pane is responsive anyway).
pub(crate) fn arm(agent: &str, text: &str) {
    if crate::daemon::hook_shadow::snapshot_for(agent).is_none() {
        return;
    }
    store().lock().insert(
        agent.to_string(),
        Pending {
            injected_at_ms: now_ms(),
            text: text.to_string(),
            redelivered: false,
        },
    );
}

/// Per-tick verification pass. For each armed agent:
/// - a `UserPromptSubmit` recorded AFTER the inject ⇒ delivered, clear silently.
/// - else past [`VERIFY_WINDOW_MS`] and not yet re-delivered ⇒ re-inject once,
///   WARN, latch (reset the clock so the re-delivery gets its own window).
/// - else past the window AND already re-delivered ⇒ final WARN, give up.
pub(crate) fn verify_pass(home: &Path) {
    let now = now_ms();
    // Decide under the lock, act (re-inject) after dropping it — the inject is a
    // self-IPC vector (#1492) and must not run while holding our mutex.
    let mut to_redeliver: Vec<(String, String)> = Vec::new();
    let mut gave_up: Vec<String> = Vec::new();
    {
        let mut guard = store().lock();
        guard.retain(|agent, p| {
            let ups = crate::daemon::hook_shadow::last_user_prompt_submit_for(agent);
            if ups.is_some_and(|t| t > p.injected_at_ms) {
                return false; // delivered — drop silently
            }
            if now.saturating_sub(p.injected_at_ms) < VERIFY_WINDOW_MS {
                return true; // still inside the window — keep waiting
            }
            if !p.redelivered {
                to_redeliver.push((agent.clone(), p.text.clone()));
                p.redelivered = true;
                p.injected_at_ms = now; // fresh window for the re-delivery
                true
            } else {
                gave_up.push(agent.clone());
                false // give up — no storm
            }
        });
    }
    for (agent, text) in to_redeliver {
        tracing::warn!(
            agent = %agent,
            tag = "#2044-inject-redeliver",
            "actionable inject unconfirmed after {}s (no UserPromptSubmit) — re-delivering once \
             (likely swallowed by an open interactive dialog)",
            VERIFY_WINDOW_MS / 1000
        );
        crate::event_log::log(
            home,
            "inject_redelivered",
            &agent,
            "actionable inject unconfirmed (no UserPromptSubmit) — re-delivered once",
        );
        // Re-inject via the plain submit path — NOT compose_aware_inject — so the
        // re-delivery does not re-arm verification (the latch lives in `Pending`).
        let _ = crate::inbox::notify::inject_notification_with_submit(home, &agent, &text);
    }
    for agent in gave_up {
        tracing::warn!(
            agent = %agent,
            tag = "#2044-inject-undelivered",
            "re-delivered inject STILL unconfirmed after {}s — giving up (operator dialog may \
             still be open; check the pane)",
            VERIFY_WINDOW_MS / 1000
        );
        crate::event_log::log(
            home,
            "inject_undelivered",
            &agent,
            "re-delivered inject still unconfirmed — gave up (no retry storm)",
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #2044 test isolation: these tests share the process-global `store()`
    /// AND drive `verify_pass`, which is a PRODUCTION whole-store pass
    /// (`retain` over every agent). Under plain `cargo test` (in-process
    /// parallel — the Coverage job's mode, run 27396184642), two tests'
    /// `verify_pass` calls interleave on the shared map and mutate each
    /// other's entries → the flaky `left:None right:Some(true)`. A unique
    /// agent name per test is NOT enough (verify_pass touches all agents), so
    /// serialize the whole group; nextest is unaffected (per-test process).
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
        G.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remove ONLY this test's own agent (never a global wipe that would nuke
    /// a sibling's in-flight pending).
    fn forget(agent: &str) {
        store().lock().remove(agent);
    }

    /// Test seam: arm with an EXPLICIT inject time so the verify window + the
    /// UserPromptSubmit ordering are deterministic (no clock-collision races).
    /// Bypasses the hook-history gate — the gate is covered separately.
    fn arm_at(agent: &str, text: &str, injected_at_ms: u64) {
        store().lock().insert(
            agent.to_string(),
            Pending {
                injected_at_ms,
                text: text.to_string(),
                redelivered: false,
            },
        );
    }

    fn pending_redelivered(agent: &str) -> Option<bool> {
        store().lock().get(agent).map(|p| p.redelivered)
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agend-2044-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).ok();
        d
    }

    /// Arm requires a hook-shadow entry — a non-hook backend (no entry) is
    /// never tracked, so it can never be falsely re-injected.
    #[test]
    fn arm_noop_without_hook_history() {
        let _g = test_guard();
        let agent = "no-hooks-2044";
        forget(agent);
        super::arm(agent, "wake");
        assert!(
            store().lock().get(agent).is_none(),
            "no hook history → not armed"
        );
        forget(agent);
    }

    /// A UserPromptSubmit recorded AFTER the inject clears the pending silently
    /// — even when the window has elapsed (delivery beats the timeout).
    #[test]
    fn delivered_clears_without_redelivery() {
        let _g = test_guard();
        let home = tmp_home("delivered");
        let agent = "deliv-2044";
        forget(agent);
        let now = now_ms();
        let injected = now - VERIFY_WINDOW_MS - 1_000; // window already elapsed
        arm_at(agent, "wake", injected);
        // Agent submitted the prompt AFTER the inject.
        crate::daemon::hook_shadow::set_user_prompt_submit_for_test(agent, injected + 500);
        super::verify_pass(&home);
        assert!(
            store().lock().get(agent).is_none(),
            "UserPromptSubmit after inject ⇒ delivered, cleared (no re-delivery)"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// No UserPromptSubmit within the window ⇒ exactly one re-delivery, then
    /// (still unconfirmed) give up — never a storm.
    #[test]
    fn unconfirmed_redelivers_once_then_gives_up() {
        let _g = test_guard();
        let home = tmp_home("unconfirmed");
        let agent = "unconf-2044";
        forget(agent);
        let now = now_ms();
        // Fresh inject (inside the window) → no action yet.
        arm_at(agent, "wake", now);
        super::verify_pass(&home);
        assert_eq!(pending_redelivered(agent), Some(false), "still waiting");
        // Window elapsed, no UserPromptSubmit → re-deliver once (latch set).
        arm_at_elapsed(agent, "wake");
        super::verify_pass(&home);
        assert_eq!(
            pending_redelivered(agent),
            Some(true),
            "one re-delivery fired, latched"
        );
        // Window elapsed again, still no UserPromptSubmit → give up (cleared).
        store().lock().get_mut(agent).unwrap().injected_at_ms = now_ms() - VERIFY_WINDOW_MS - 1;
        super::verify_pass(&home);
        assert!(
            store().lock().get(agent).is_none(),
            "gave up after the single re-delivery — no storm"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// A UserPromptSubmit that PRE-dates the inject does NOT count as delivery
    /// (a stale earlier submit must not mask a swallowed new inject).
    #[test]
    fn stale_prior_user_prompt_submit_does_not_confirm() {
        let _g = test_guard();
        let home = tmp_home("stale-ups");
        let agent = "stale-2044";
        forget(agent);
        let now = now_ms();
        let injected = now - VERIFY_WINDOW_MS - 1_000; // window elapsed
                                                       // UserPromptSubmit BEFORE the inject (stale).
        crate::daemon::hook_shadow::set_user_prompt_submit_for_test(agent, injected - 5_000);
        arm_at(agent, "wake", injected);
        super::verify_pass(&home);
        assert_eq!(
            pending_redelivered(agent),
            Some(true),
            "the pre-inject UserPromptSubmit must not confirm the new inject"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Helper: re-stamp an armed inject so the verify window has elapsed.
    fn arm_at_elapsed(agent: &str, _text: &str) {
        if let Some(p) = store().lock().get_mut(agent) {
            p.injected_at_ms = now_ms() - VERIFY_WINDOW_MS - 1;
        }
    }
}
