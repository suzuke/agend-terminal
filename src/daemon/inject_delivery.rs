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
    /// Transport generation that admitted the original actionable wake.
    /// Re-delivery must be admitted against this exact generation so a
    /// delete/redeploy cannot route the stale wake to a successor.
    transport_epoch: u64,
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
pub(crate) fn arm_with_transport_epoch(agent: &str, text: &str, transport_epoch: u64) {
    if crate::daemon::hook_shadow::snapshot_for(agent).is_none() {
        return;
    }
    store().lock().insert(
        agent.to_string(),
        Pending {
            injected_at_ms: now_ms(),
            text: text.to_string(),
            redelivered: false,
            transport_epoch,
        },
    );
    #[cfg(test)]
    test_support::run_arm_hook(agent);
}

#[cfg(test)]
pub(crate) fn arm(agent: &str, text: &str) {
    arm_with_transport_epoch(agent, text, 0);
}

/// Forget any pending verification for a deleted agent so a same-name
/// redeploy cannot inherit a stale actionable wake.
pub(crate) fn forget(agent: &str) {
    store().lock().remove(agent);
}

#[cfg(test)]
pub(crate) fn is_armed_for_test(agent: &str) -> bool {
    store().lock().contains_key(agent)
}

#[cfg(test)]
pub(crate) fn clear_for_test(agent: &str) {
    forget(agent);
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::sync::OnceLock;

    pub(crate) type ArmHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
    pub(crate) type VerifyBeforeRedeliveryHook =
        std::sync::Arc<dyn Fn(&Path, &str, u64) + Send + Sync>;

    static ARM_HOOK: OnceLock<parking_lot::Mutex<Option<ArmHook>>> = OnceLock::new();
    static VERIFY_BEFORE_REDELIVERY_HOOK: OnceLock<
        parking_lot::Mutex<Option<VerifyBeforeRedeliveryHook>>,
    > = OnceLock::new();
    static ARM_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static VERIFY_BEFORE_REDELIVERY_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    pub(crate) struct ArmHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn arm_hook_guard() -> ArmHookGuard {
        let lock = ARM_HOOK_LOCK.lock();
        set_arm_hook(None);
        ArmHookGuard { _lock: lock }
    }

    pub(crate) fn set_arm_hook(hook: Option<ArmHook>) {
        *ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for ArmHookGuard {
        fn drop(&mut self) {
            set_arm_hook(None);
        }
    }

    pub(crate) struct VerifyBeforeRedeliveryHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn verify_before_redelivery_hook_guard() -> VerifyBeforeRedeliveryHookGuard {
        let lock = VERIFY_BEFORE_REDELIVERY_HOOK_LOCK.lock();
        set_verify_before_redelivery_hook(None);
        VerifyBeforeRedeliveryHookGuard { _lock: lock }
    }

    pub(crate) fn set_verify_before_redelivery_hook(hook: Option<VerifyBeforeRedeliveryHook>) {
        *VERIFY_BEFORE_REDELIVERY_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for VerifyBeforeRedeliveryHookGuard {
        fn drop(&mut self) {
            set_verify_before_redelivery_hook(None);
        }
    }

    pub(super) fn run_arm_hook(agent: &str) {
        let hook = ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(agent);
        }
    }

    pub(super) fn run_verify_before_redelivery_hook(home: &Path, agent: &str, epoch: u64) {
        let hook = VERIFY_BEFORE_REDELIVERY_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }
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
    let mut to_redeliver: Vec<(String, String, u64)> = Vec::new();
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
                to_redeliver.push((agent.clone(), p.text.clone(), p.transport_epoch));
                p.redelivered = true;
                p.injected_at_ms = now; // fresh window for the re-delivery
                true
            } else {
                gave_up.push(agent.clone());
                false // give up — no storm
            }
        });
    }
    for (agent, text, transport_epoch) in to_redeliver {
        // Re-inject via the plain submit path — NOT compose_aware_inject — so the
        // re-delivery does not re-arm verification (the latch lives in `Pending`).
        #[cfg(test)]
        test_support::run_verify_before_redelivery_hook(home, &agent, transport_epoch);
        let result = crate::inbox::notify::inject_notification_with_submit_at_epoch(
            home,
            &agent,
            &text,
            transport_epoch,
        );
        match result {
            Ok(()) => {
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
            }
            Err(error) => {
                let fenced = error.to_string().contains("fenced");
                let (tag, kind, detail) = if fenced {
                    (
                        "#2044-inject-redeliver-suppressed",
                        "inject_redelivery_suppressed",
                        "redelivery admission fenced by a newer transport generation",
                    )
                } else {
                    (
                        "#2044-inject-redeliver-failed",
                        "inject_redelivery_failed",
                        "redelivery admission failed before adapter delivery",
                    )
                };
                tracing::warn!(agent = %agent, error = %error, tag, "{detail}");
                crate::event_log::log(home, kind, &agent, detail);
            }
        }
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
        arm_at_with_epoch(agent, text, injected_at_ms, 0);
    }

    fn arm_at_with_epoch(agent: &str, text: &str, injected_at_ms: u64, transport_epoch: u64) {
        store().lock().insert(
            agent.to_string(),
            Pending {
                injected_at_ms,
                text: text.to_string(),
                redelivered: false,
                transport_epoch,
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

    /// RED for #3303: verification state must follow the durable inbox row,
    /// not collapse every outstanding wake for an agent into one slot.
    #[test]
    fn distinct_durable_rows_have_independent_verification_latches() {
        let _g = test_guard();
        let agent = "row-keyed-2044";
        forget(agent);
        let now = now_ms();
        arm_at(
            agent,
            "[AGEND-MSG-PENDING] id=row-a kind=task from=lead inbox=1",
            now,
        );
        arm_at(
            agent,
            "[AGEND-MSG-PENDING] id=row-b kind=task from=lead inbox=1",
            now,
        );
        let guard = store().lock();
        assert!(guard.contains_key("row-a"), "row-a must have its own latch");
        assert!(guard.contains_key("row-b"), "row-b must have its own latch");
        drop(guard);
        forget(agent);
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

    /// The verifier decides to redeliver outside its store lock. If destructive
    /// teardown wins in that gap, the original epoch must fence the later
    /// enqueue even after cleanup has made the name admissible again. This
    /// proves the stale verifier cannot call an adapter, create a receipt, or
    /// leave an arm behind for a same-name successor.
    #[test]
    fn stale_redelivery_after_delete_is_fenced_before_adapter_or_receipt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _g = test_guard();
        let home = tmp_home("stale-redelivery-delete");
        let agent = "stale-redelivery-delete-2044";
        let _delivery_hook_guard = crate::transport::test_support::delivery_hook_guard();
        let _verify_hook_guard = test_support::verify_before_redelivery_hook_guard();
        forget(agent);
        let original_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let now = now_ms();
        arm_at_with_epoch(
            agent,
            "stale verifier wake",
            now - VERIFY_WINDOW_MS - 1,
            original_epoch,
        );

        let adapter_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let adapter_calls_hook = std::sync::Arc::clone(&adapter_calls);
        let expected_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
            move |called_home, called_agent, _body| {
                if called_home == expected_home.as_path() && called_agent == agent {
                    adapter_calls_hook.fetch_add(1, Ordering::SeqCst);
                    Some(Err(anyhow::anyhow!("stale verifier reached the adapter")))
                } else {
                    None
                }
            },
        )));

        let hook_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delete_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_ran_hook = std::sync::Arc::clone(&hook_ran);
        let delete_completed_hook = std::sync::Arc::clone(&delete_completed);
        let fence_home = home.clone();
        test_support::set_verify_before_redelivery_hook(Some(std::sync::Arc::new(
            move |hook_home, hook_agent, _epoch| {
                assert_eq!(hook_home, fence_home.as_path());
                assert_eq!(hook_agent, agent);
                hook_ran_hook.store(true, Ordering::SeqCst);
                let fence = crate::daemon::lifecycle::DeleteFence::new(hook_home, hook_agent, true);
                drop(fence);
                delete_completed_hook.store(true, Ordering::SeqCst);
            },
        )));

        verify_pass(&home);

        assert!(hook_ran.load(Ordering::SeqCst));
        assert!(delete_completed.load(Ordering::SeqCst));
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
        assert!(!is_armed_for_test(agent));
        assert!(
            !crate::transport::delivery_path_for_instance(&home, agent).exists(),
            "fenced verifier redelivery must not create a receipt"
        );
        assert!(
            !crate::transport::delivery_path_for_instance(&home, agent)
                .with_extension("jsonl.lock")
                .exists(),
            "fenced verifier redelivery must not create a receipt lock"
        );
        let event_log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(!event_log.contains("\"kind\":\"inject_redelivered\""));
        assert!(event_log.contains("inject_redelivery_suppressed"));
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
