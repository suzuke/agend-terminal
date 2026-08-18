use std::path::Path;

pub(crate) fn forget_reclaim_dedup(canonical_agent: &str, legacy_agent: &str, id: &str) {
    let ledger = crate::daemon::notification_dedup::global();
    ledger.forget(canonical_agent, id);
    if canonical_agent != legacy_agent {
        ledger.forget(legacy_agent, id);
    }
}

/// Re-arm one reverted inbox row only after transport admission succeeds.
/// The reservation keeps the durable structured re-arm budget transactional:
/// a queue-full or fenced delivery restores the terminal latch instead of
/// consuming the one bounded retry.
pub(crate) fn rearm_reclaimed_message(home: &Path, agent: &str, id: &str, kind: &str, from: &str) {
    let Some(reservation) =
        crate::daemon::inject_delivery::reserve_rearm_after_reclaim(home, agent, id)
    else {
        return;
    };
    match crate::inbox::notify::rearm_persisted_pointer(home, agent, id, kind, from) {
        crate::inbox::notify::ComposeInjectOutcome::Deferred => {
            if !crate::daemon::inject_delivery::defer_rearm_after_reclaim(home, &reservation) {
                tracing::error!(agent = %agent, msg_id = %id,
                    "deferred reclaim re-arm could not persist its pending marker");
                let _ = crate::daemon::inject_delivery::rollback_rearm_after_reclaim(
                    home,
                    &reservation,
                );
            }
        }
        crate::inbox::notify::ComposeInjectOutcome::TransportAccepted => {
            if !crate::daemon::inject_delivery::commit_rearm_after_reclaim(home, &reservation) {
                tracing::error!(agent = %agent, msg_id = %id,
                    "reclaim re-arm was admitted but its durable commit failed");
            }
        }
        crate::inbox::notify::ComposeInjectOutcome::Suppressed
        | crate::inbox::notify::ComposeInjectOutcome::Failed => {
            tracing::warn!(agent = %agent, msg_id = %id,
                "reclaim re-arm admission failed; restoring terminal latch");
            let _ =
                crate::daemon::inject_delivery::rollback_rearm_after_reclaim(home, &reservation);
        }
    }
}
