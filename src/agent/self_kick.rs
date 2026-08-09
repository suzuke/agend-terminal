use super::{deleting, inject_with_target, lock_registry, AgentRegistry, InjectTarget};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelfKickDelivery {
    ProtocolAccepted,
    LegacyPty,
}

/// Deliver the fresh-restart prompt exactly once on the transport lane. The
/// target identity is captured after the readiness wait and checked against the
/// current registry generation before the registry lock is released for I/O.
/// `legacy_pty_ready` is the raw-prompt proof carried across that wait; a
/// structured-ready caller cannot later fall through to a PTY write.
pub(super) fn deliver_self_kick(
    registry: &AgentRegistry,
    home: &std::path::Path,
    target: &InjectTarget,
    prompt: &str,
    legacy_pty_ready: bool,
) -> anyhow::Result<SelfKickDelivery> {
    crate::daemon::delivery_worker::with_transport_serial(home, &target.name, || {
        if deleting::is_deleting(home, target.name.as_str()) {
            return Err(anyhow::anyhow!(
                "self-kick target is being deleted: {}",
                target.name
            ));
        }

        let target_is_current = {
            let reg = lock_registry(registry);
            reg.get(&target.instance_id).is_some_and(|handle| {
                handle.id == target.instance_id
                    && handle.name.as_str() == target.name.as_str()
                    && handle.generation == target.generation
                    && !handle.deleted.load(std::sync::atomic::Ordering::Acquire)
            })
        };
        if !target_is_current || target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return Err(anyhow::anyhow!(
                "self-kick target is stale: {} ({})",
                target.name,
                target.instance_id
            ));
        }

        let legacy_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let legacy_called_by_closure = Arc::clone(&legacy_called);
        let target_for_legacy = target.clone();
        let prompt = prompt.to_string();
        let receipt = crate::transport::deliver_notification(
            home,
            target.name.as_str(),
            &prompt,
            move |_home, _agent, text| {
                // The readiness mode was selected before the bounded wait. If
                // transport configuration changes from structured to LegacyPty
                // meanwhile, fail closed instead of converting registration-only
                // readiness into an unchecked terminal write.
                if !legacy_pty_ready {
                    return Err(anyhow::anyhow!(
                        "LegacyPty self-kick lacks raw-prompt-idle readiness"
                    ));
                }
                if legacy_called_by_closure.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    return Err(anyhow::anyhow!("LegacyPty closure invoked more than once"));
                }
                inject_with_target(&target_for_legacy, text.as_bytes())
                    .map_err(|error| anyhow::anyhow!("LegacyPty self-kick failed: {error}"))
            },
        )?;

        if receipt.state == crate::transport::DeliveryState::ProtocolAccepted {
            return Ok(SelfKickDelivery::ProtocolAccepted);
        }
        if receipt.state == crate::transport::DeliveryState::Ambiguous
            && legacy_called.load(std::sync::atomic::Ordering::Acquire)
        {
            // Capture and arm while with_transport_serial still owns the
            // same-key lane. A subsequent delete therefore cannot clear the
            // pending state and then be followed by a late arm from the
            // bootstrap caller.
            let epoch =
                crate::daemon::delivery_worker::current_transport_epoch(home, target.name.as_str());
            let _ = crate::daemon::delivery_worker::arm_transport_verification_if_current(
                home,
                target.name.as_str(),
                epoch,
                &prompt,
            );
            return Ok(SelfKickDelivery::LegacyPty);
        }
        Err(anyhow::anyhow!(
            "self-kick transport did not accept delivery: {:?}",
            receipt.state
        ))
    })
}
