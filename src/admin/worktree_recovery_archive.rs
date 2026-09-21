use std::path::{Path, PathBuf};

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_recorded_archive(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    worktree: &Path,
    source_repo: &Path,
    tombstone: &crate::agent::deletion_recovery::Tombstone,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Result<super::RecoveryReport, String> {
    if !matches!(
        tombstone.state,
        crate::agent::deletion_recovery::State::Deleting
            | crate::agent::deletion_recovery::State::RecoveryRequired
    ) {
        return Err("recovery refused: tombstone is not pending recovery".to_string());
    }
    if tombstone.instance != instance
        || tombstone.branch != branch
        || Path::new(&tombstone.worktree) != worktree
        || Path::new(&tombstone.source_repo) != source_repo
    {
        return Err(
            "recovery refused: supplied identity does not match the delete tombstone".to_string(),
        );
    }
    if worktree.exists() {
        return Err(format!(
            "recovery refused: original worktree reappeared while archive is pending: {}",
            worktree.display()
        ));
    }
    let archive = tombstone
        .archive
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| "recovery refused: pending tombstone has no archive".to_string())?;
    let archive_root = home
        .join(".trash")
        .join("worktrees")
        .canonicalize()
        .map_err(|e| format!("recovery archive root is unavailable: {e}"))?;
    let archive_target = archive
        .canonicalize()
        .map_err(|e| format!("recorded recovery archive is unavailable: {e}"))?;
    if !archive_target.starts_with(&archive_root) || archive_target == archive_root {
        return Err("recovery refused: recorded archive is outside the recovery root".to_string());
    }
    let archived_binding = std::fs::read(archive.join(".agend-recovery-binding.json"))
        .map_err(|e| format!("read archived binding evidence: {e}"))?;
    let archived_signature = std::fs::read(archive.join(".agend-recovery-binding.json.sig"))
        .map_err(|e| format!("read archived binding signature evidence: {e}"))?;
    if crate::daemon::utils::sha256_hex(&archived_binding) != tombstone.binding_sha256
        || crate::daemon::utils::sha256_hex(&archived_signature)
            != tombstone.binding_signature_sha256
    {
        return Err(
            "recovery refused: archived binding evidence does not match tombstone".to_string(),
        );
    }

    let binding_path = crate::paths::binding_path(home, instance);
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let binding_exists = binding_path.is_file();
    let signature_exists = signature_path.is_file();
    if binding_exists != signature_exists {
        return Err("recovery refused: binding evidence is only partially present".to_string());
    }
    if binding_exists {
        let binding_body = std::fs::read(&binding_path)
            .map_err(|e| format!("read binding metadata {}: {e}", binding_path.display()))?;
        let binding_signature = std::fs::read(&signature_path)
            .map_err(|e| format!("read binding signature {}: {e}", signature_path.display()))?;
        if binding_body != archived_binding || binding_signature != archived_signature {
            return Err(
                "recovery refused: live binding evidence does not match tombstone".to_string(),
            );
        }
        if !crate::binding::signature_valid(home, instance) {
            return Err(
                "recovery refused: binding signature for recovery is not valid".to_string(),
            );
        }
        match crate::binding::unbind_with_permit(home, instance, permit) {
            crate::binding::BindingRemoval::Removed | crate::binding::BindingRemoval::Absent => {}
            crate::binding::BindingRemoval::Failed(error) => {
                return Err(format!(
                    "binding removal failed; archive remains at {}: {error}",
                    archive.display()
                ));
            }
        }
    }
    super::remove_empty_dir_tree(
        &crate::worktree_pool::daemon_managed_worktree_root(home).join(instance),
    );
    let _ = std::fs::remove_dir_all(crate::paths::runtime_dir(home).join(instance));
    crate::agent::deletion_recovery::mark_recovered(home, instance, &archive)?;
    crate::event_log::log(
        home,
        "markerless_worktree_recovered",
        instance,
        &format!(
            "actor={actor}; branch={branch}; archive={}; reason={audit_reason}",
            archive.display()
        ),
    );
    Ok(super::RecoveryReport { archive })
}
