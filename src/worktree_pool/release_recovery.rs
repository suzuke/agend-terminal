use std::path::Path;

use super::ReleaseOutcome;

fn stale_release() -> ReleaseOutcome {
    ReleaseOutcome {
        stale_fingerprint: true,
        error: Some(
            "release refused: binding fingerprint changed before destructive authority was reacquired"
                .to_string(),
        ),
        ..ReleaseOutcome::default()
    }
}

pub(crate) fn stale_release_after_snapshot(
    home: &Path,
    agent: &str,
    snapshot: &serde_json::Value,
    fingerprint: &crate::binding::BindingFingerprint,
) -> ReleaseOutcome {
    let mut outcome = stale_release();
    if let Err(error) = clear_if_matches_snapshot(home, agent, snapshot, fingerprint) {
        outcome.error = Some(format!(
            "{}; superseded recovery journal could not be cleared: {error}",
            outcome.error.as_deref().unwrap_or("release refused")
        ));
    }
    outcome
}

/// Clear a preflight journal only when the stale snapshot proves it owns that
/// exact signed generation. A replacement binding must never inherit a
/// predecessor's recovery fence.
pub(crate) fn clear_if_matches_snapshot(
    home: &Path,
    agent: &str,
    binding: &serde_json::Value,
    fingerprint: &crate::binding::BindingFingerprint,
) -> Result<(), String> {
    let Some(tombstone) = crate::agent::deletion_recovery::read(home, agent)? else {
        return Ok(());
    };
    if tombstone.binding_sha256 != fingerprint.digest
        || tombstone.branch != binding["branch"].as_str().unwrap_or_default()
        || tombstone.worktree != binding["worktree"].as_str().unwrap_or_default()
        || tombstone.source_repo != binding["source_repo"].as_str().unwrap_or_default()
    {
        return Ok(());
    }
    crate::agent::deletion_recovery::clear(home, agent)
}

pub(crate) fn clear_binding_state(
    home: &Path,
    agent: &str,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> crate::binding::BindingRemoval {
    crate::binding::unbind_with_permit(home, agent, permit)
}

/// Publish the exact signed binding fence before any normal release mutation.
/// The delete lifecycle already owns the same fence; its provenance skips this
/// helper and keeps ownership until the residual-store audit completes.
pub(crate) fn prepare_release_journal(home: &Path, agent: &str) -> Result<(), String> {
    let Some(binding) = crate::binding::read(home, agent) else {
        return Ok(());
    };
    // Pre-signature legacy bindings have no authenticated source identity and
    // remain on the existing marker/authority release path.  They cannot enter
    // this journal without inventing signed evidence; the normal release gates
    // still fail closed when the target itself is unsafe.
    if binding["source_repo"].as_str().is_none_or(str::is_empty) {
        return Ok(());
    }
    match crate::agent::deletion_recovery::begin_from_binding(home, agent)? {
        Some(_) => Ok(()),
        None => Err(
            "release refused: signed binding evidence is required before destructive cleanup"
                .to_string(),
        ),
    }
}

pub(crate) fn clear_release_recovery_journal(home: &Path, agent: &str) -> Result<(), String> {
    crate::agent::deletion_recovery::clear(home, agent)
}

pub(crate) fn mark_release_recovery_required(home: &Path, agent: &str) -> Result<(), String> {
    crate::agent::deletion_recovery::mark_recovery_required(home, agent, None)
}

pub(crate) fn record_binding_removal(
    out: &mut ReleaseOutcome,
    removal: crate::binding::BindingRemoval,
) {
    match removal {
        crate::binding::BindingRemoval::Removed => out.binding_removed = true,
        crate::binding::BindingRemoval::Absent => {
            if out.error.is_none() {
                out.error = Some("binding disappeared before removal".to_string());
            }
        }
        crate::binding::BindingRemoval::Failed(error) => {
            if let Some(existing) = &mut out.error {
                existing.push_str("; binding removal failed: ");
                existing.push_str(&error);
            } else {
                out.error = Some(format!("binding removal failed: {error}"));
            }
        }
    }
}
