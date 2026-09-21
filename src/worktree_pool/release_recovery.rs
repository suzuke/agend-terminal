use std::path::Path;

use super::ReleaseOutcome;

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
