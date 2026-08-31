//! Bootstrap / respawn readiness waiting.
//!
//! Extracted verbatim from `agent/mod.rs` to restore that file under the
//! anti-monolith ceiling (`tests/src_file_size_invariant`). One cohesive
//! concern: decide when a freshly spawned or respawned agent is ready to
//! accept an injected first turn, and hand back one `InjectTarget` snapshot.
//! Behavior and public surface are unchanged — `agent/mod.rs` re-exports every
//! item, so all existing call sites keep their paths.

use super::{lock_registry, AgentCore, AgentRegistry, CoreMutex, InjectTarget};
use std::sync::Arc;

/// #CR-2026-06-14: lock the per-agent core (OFF the registry lock path) and
/// report whether it has reached Idle. Kept as a helper so the readiness check
/// snapshots `Arc::clone(&h.core)` under the registry lock, drops that guard, and
/// only then acquires the core lock here — never registry→core nested.
fn bootstrap_core_is_idle(core: &std::sync::Arc<CoreMutex<AgentCore>>) -> bool {
    core.lock().state.get_state() == crate::state::AgentState::Idle
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleInjectWaitTerminal {
    Shutdown,
    NeverRegisteredTimeout,
    NotIdleTimeout,
    DisappearedAfterSeen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapRegistrationState {
    #[cfg(test)]
    MayRegisterLater,
    AlreadyRegistered,
}

/// First-turn readiness authority plus registration contract. Legacy PTY waits
/// for raw prompt classification; structured adapters own their handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapWaitPolicy {
    RawPromptIdle(BootstrapRegistrationState),
    StructuredTransport(BootstrapRegistrationState),
}

impl IdleInjectWaitTerminal {
    fn label(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::NeverRegisteredTimeout => "never-registered-timeout",
            Self::NotIdleTimeout => "not-idle-timeout",
            Self::DisappearedAfterSeen => "disappeared-after-seen",
        }
    }
}

pub(crate) struct IdleInjectWaitResult {
    pub(crate) target: Option<InjectTarget>,
    pub(crate) terminal: Option<IdleInjectWaitTerminal>,
}

impl IdleInjectWaitResult {
    fn ready(target: InjectTarget) -> Self {
        Self {
            target: Some(target),
            terminal: None,
        }
    }

    fn terminal(reason: IdleInjectWaitTerminal) -> Self {
        Self {
            target: None,
            terminal: Some(reason),
        }
    }
}

/// Wait for the caller-selected first-turn readiness authority, then settle and
/// snapshot its [`InjectTarget`] with the registry lock released. `RawPromptIdle`
/// is used by Kiro instructions and LegacyPty self-kick delivery, whose bytes
/// would be swallowed while the TUI is still starting. `StructuredTransport`
/// waits only for the exact handle to register, then delegates handshake/thread
/// discovery/turn admission to the adapter. `None` => do not inject.
///
/// A restore can publish the exact UUID after this waiter starts. Absence is
/// therefore tolerated until the deadline before first observation; once the
/// handle has been observed, disappearance is an immediate terminal outcome.
///
/// #CR-2026-06-14 (concurrency): for raw-prompt readiness, snapshot the core Arc
/// under the tier-1 registry lock, DROP the registry guard, THEN lock the core
/// (in `bootstrap_core_is_idle`) — never registry→core nested. Structured
/// readiness needs no core lock.
pub(crate) fn wait_for_bootstrap_inject_target_with_clock<Now, Sleep>(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    timeout: std::time::Duration,
    policy: BootstrapWaitPolicy,
    shutdown: Option<&Arc<std::sync::atomic::AtomicBool>>,
    now: &mut Now,
    sleep: &mut Sleep,
) -> IdleInjectWaitResult
where
    Now: FnMut() -> std::time::Duration,
    Sleep: FnMut(std::time::Duration),
{
    let poll_interval = std::time::Duration::from_millis(200);
    let settle_delay = std::time::Duration::from_millis(500);
    let registration = match policy {
        BootstrapWaitPolicy::RawPromptIdle(state)
        | BootstrapWaitPolicy::StructuredTransport(state) => state,
    };
    let mut seen_handle = matches!(registration, BootstrapRegistrationState::AlreadyRegistered);

    loop {
        if let Some(s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                return IdleInjectWaitResult::terminal(IdleInjectWaitTerminal::Shutdown);
            }
        }
        if now() >= timeout {
            return IdleInjectWaitResult::terminal(if seen_handle {
                IdleInjectWaitTerminal::NotIdleTimeout
            } else {
                IdleInjectWaitTerminal::NeverRegisteredTimeout
            });
        }
        let core = {
            let reg = lock_registry(registry);
            match reg.get(&instance_id) {
                Some(h) => {
                    seen_handle = true;
                    Some(std::sync::Arc::clone(&h.core))
                }
                None if seen_handle => {
                    return IdleInjectWaitResult::terminal(
                        IdleInjectWaitTerminal::DisappearedAfterSeen,
                    );
                }
                None => None,
            }
        };
        let ready = match policy {
            BootstrapWaitPolicy::RawPromptIdle(_) => {
                core.as_ref().is_some_and(bootstrap_core_is_idle)
            }
            BootstrapWaitPolicy::StructuredTransport(_) => core.is_some(),
        };
        if ready {
            break;
        }
        sleep(poll_interval);
    }
    // Small settle delay for prompt paint or structured-session publication.
    sleep(settle_delay);
    // #3462-v3/F1: the loop-top check is the last shutdown observation before this
    // settle, so one landing during it would still hand back a live target and the
    // caller would write into an agent the daemon is tearing down. All three
    // callers of this waiter inherit the fence.
    if shutdown.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
        return IdleInjectWaitResult::terminal(IdleInjectWaitTerminal::Shutdown);
    }
    // #1530/F1: snapshot the inject target under the registry lock, release it,
    // THEN inject (caller side) — never hold the registry across the blocking write.
    let reg = lock_registry(registry);
    match reg.get(&instance_id) {
        Some(handle) => IdleInjectWaitResult::ready(InjectTarget::from_handle(handle)),
        None => IdleInjectWaitResult::terminal(IdleInjectWaitTerminal::DisappearedAfterSeen),
    }
}

pub(crate) fn wait_for_bootstrap_inject_target(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    name: &str,
    timeout: std::time::Duration,
    policy: BootstrapWaitPolicy,
    shutdown: Option<&Arc<std::sync::atomic::AtomicBool>>,
    what: &str,
) -> Option<InjectTarget> {
    let started = std::time::Instant::now();
    let mut now = || started.elapsed();
    let mut sleep = std::thread::sleep;
    let result = wait_for_bootstrap_inject_target_with_clock(
        registry,
        instance_id,
        timeout,
        policy,
        shutdown,
        &mut now,
        &mut sleep,
    );
    if let Some(reason) = result.terminal {
        tracing::warn!(
            agent = %name,
            what,
            reason = reason.label(),
            "bootstrap wait ended"
        );
    }
    result.target
}

/// #3462-v2: crash-respawn's readiness wait. The smallest reuse of the existing
/// bootstrap waiter: the respawned handle is ALREADY registered, so this only
/// selects the same transport-derived policy `spawn_self_kick_bootstrap` uses and
/// delegates. It adds no polling loop, no retry and no transport of its own —
/// every terminal outcome (timeout, disappearance, shutdown) already returns
/// `None` and is already logged by `wait_for_bootstrap_inject_target`, so the
/// caller simply does not inject and nothing is written.
pub(crate) fn wait_for_respawn_inject_target(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    name: &str,
    home: &std::path::Path,
    timeout: std::time::Duration,
    shutdown: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Option<InjectTarget> {
    let policy = if crate::transport::mode_for_instance(home, name)
        == crate::transport::TransportMode::LegacyPty
    {
        BootstrapWaitPolicy::RawPromptIdle(BootstrapRegistrationState::AlreadyRegistered)
    } else {
        BootstrapWaitPolicy::StructuredTransport(BootstrapRegistrationState::AlreadyRegistered)
    };
    wait_for_bootstrap_inject_target(
        registry,
        instance_id,
        name,
        timeout,
        policy,
        shutdown,
        "crash-respawn-notice",
    )
}
