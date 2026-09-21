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
    caller_generation: &crate::binding::BindingFingerprint,
) -> ReleaseOutcome {
    let mut outcome = stale_release();
    if let Err(error) = clear_if_matches_generation(home, agent, caller_generation) {
        outcome.error = Some(format!(
            "{}; superseded recovery journal could not be cleared: {error}",
            outcome.error.as_deref().unwrap_or("release refused")
        ));
    }
    outcome
}

/// Clear a preflight journal only when its exact signed generation belongs to
/// the stale caller. A replacement binding must never inherit a predecessor's
/// recovery fence, and a stale caller must never clear the replacement's.
pub(crate) fn clear_if_matches_generation(
    home: &Path,
    agent: &str,
    caller_generation: &crate::binding::BindingFingerprint,
) -> Result<(), String> {
    let Some(tombstone) = crate::agent::deletion_recovery::read(home, agent)? else {
        return Ok(());
    };
    if tombstone.binding_sha256 != caller_generation.digest {
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
    // Bindings written before the signed-source rollout may still carry a
    // source_repo field but have no signature sidecar. Preserve their legacy
    // release path; a present-but-invalid sidecar remains fail-closed below.
    let signature_path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json.sig");
    match std::fs::symlink_metadata(&signature_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "release refused: inspect binding signature sidecar {}: {error}",
                signature_path.display()
            ));
        }
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
    if crate::agent::deletion_recovery::read(home, agent)?.is_none() {
        // Unsigned legacy bindings do not enter the durable recovery lane and
        // retain the pre-#3696 absent-target release behavior.
        return Ok(());
    }
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
