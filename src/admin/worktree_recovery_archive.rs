use std::path::{Path, PathBuf};

#[allow(clippy::too_many_arguments)]
pub(super) fn write_archive_metadata(
    directory: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    source_repo: &Path,
    original_worktree: &Path,
    archived_worktree: &Path,
    binding_body: &[u8],
    binding_signature: &[u8],
) -> Result<(), String> {
    let binding_path = directory.join(".agend-recovery-binding.json");
    if let Ok(existing) = std::fs::read(&binding_path) {
        if existing != binding_body {
            return Err(format!(
                "recovery metadata collision at {}",
                binding_path.display()
            ));
        }
    }
    let signature_path = directory.join(".agend-recovery-binding.json.sig");
    if let Ok(existing) = std::fs::read(&signature_path) {
        if existing != binding_signature {
            return Err(format!(
                "recovery signature metadata collision at {}",
                signature_path.display()
            ));
        }
    }
    let manifest_path = directory.join(".agend-recovery-manifest.json");
    let manifest = serde_json::json!({
        "schema_version": 1,
        "actor": actor,
        "audit_reason": audit_reason,
        "instance": instance,
        "branch": branch,
        "source_repo": source_repo,
        "original_worktree": original_worktree,
        "archived_worktree": archived_worktree,
        "binding_sha256": crate::daemon::utils::sha256_hex(binding_body),
    });
    if let Ok(existing) = std::fs::read(&manifest_path) {
        let existing: serde_json::Value = serde_json::from_slice(&existing)
            .map_err(|e| format!("parse existing recovery manifest: {e}"))?;
        if existing["instance"] != instance
            || existing["archived_worktree"] != archived_worktree.to_string_lossy().as_ref()
        {
            return Err(format!(
                "recovery manifest metadata collision at {}",
                manifest_path.display()
            ));
        }
    }
    if !binding_path.is_file() {
        crate::store::atomic_write(&binding_path, binding_body).map_err(|e| e.to_string())?;
    }
    if !signature_path.is_file() {
        crate::store::atomic_write(&signature_path, binding_signature)
            .map_err(|e| e.to_string())?;
    }
    if !manifest_path.is_file() {
        crate::store::atomic_write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

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
    let binding_path = crate::paths::binding_path(home, instance);
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let binding_exists = binding_path.is_file();
    let signature_exists = signature_path.is_file();
    if binding_exists != signature_exists {
        return Err("recovery refused: binding evidence is only partially present".to_string());
    }
    let live_binding = if binding_exists {
        let body = std::fs::read(&binding_path)
            .map_err(|e| format!("read binding metadata {}: {e}", binding_path.display()))?;
        let signature = std::fs::read(&signature_path)
            .map_err(|e| format!("read binding signature {}: {e}", signature_path.display()))?;
        if crate::daemon::utils::sha256_hex(&body) != tombstone.binding_sha256
            || crate::daemon::utils::sha256_hex(&signature) != tombstone.binding_signature_sha256
        {
            return Err(
                "recovery refused: live binding evidence does not match tombstone".to_string(),
            );
        }
        if !crate::binding::signature_valid(home, instance) {
            return Err(
                "recovery refused: binding signature for recovery is not valid".to_string(),
            );
        }
        Some((body, signature))
    } else {
        None
    };
    let archived_binding = match std::fs::read(archive.join(".agend-recovery-binding.json")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read archived binding evidence: {error}")),
    };
    let archived_signature = match std::fs::read(archive.join(".agend-recovery-binding.json.sig")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("read archived binding signature evidence: {error}")),
    };
    if archived_binding.is_some() != archived_signature.is_some() {
        return Err(
            "recovery refused: archived binding evidence is only partially present".to_string(),
        );
    }
    if let Some(binding) = archived_binding.as_ref() {
        let signature = archived_signature
            .as_ref()
            .expect("archived binding/signature presence checked above");
        if crate::daemon::utils::sha256_hex(binding) != tombstone.binding_sha256
            || crate::daemon::utils::sha256_hex(signature) != tombstone.binding_signature_sha256
        {
            return Err(
                "recovery refused: archived binding evidence does not match tombstone".to_string(),
            );
        }
        if let Some((live_body, live_signature)) = live_binding.as_ref() {
            if live_body != binding || live_signature != signature {
                return Err(
                    "recovery refused: live binding evidence does not match archive".to_string(),
                );
            }
        }
    }
    let (evidence_binding, evidence_signature) = live_binding
        .as_ref()
        .map(|(body, signature)| (body.as_slice(), signature.as_slice()))
        .or_else(|| {
            archived_binding
                .as_ref()
                .zip(archived_signature.as_ref())
                .map(|(body, signature)| (body.as_slice(), signature.as_slice()))
        })
        .ok_or_else(|| "recovery refused: archive has no signed binding evidence".to_string())?;
    let binding: serde_json::Value = serde_json::from_slice(evidence_binding)
        .map_err(|e| format!("recovery refused: archived binding is invalid JSON: {e}"))?;
    if binding["branch"].as_str() != Some(tombstone.branch.as_str())
        || binding["worktree"].as_str() != Some(tombstone.worktree.as_str())
        || binding["source_repo"].as_str() != Some(tombstone.source_repo.as_str())
    {
        return Err(
            "recovery refused: archived binding identity does not match tombstone".to_string(),
        );
    }
    let task_id = binding["task_id"].as_str().unwrap_or_default();
    if !task_id.is_empty() {
        let routed = crate::tasks::load_routed(home, task_id)
            .map_err(|e| format!("recovery refused: task '{task_id}' is unreadable: {e}"))?;
        if !routed.task.status.is_terminal() {
            return Err(format!(
                "recovery refused: task '{task_id}' is still active"
            ));
        }
    }
    if !binding_exists && crate::worktree_pool::is_agent_alive(home, instance) {
        return Err(format!(
            "recovery refused: instance '{instance}' still shows liveness"
        ));
    }
    write_archive_metadata(
        &archive,
        actor,
        audit_reason,
        instance,
        branch,
        source_repo,
        worktree,
        &archive,
        evidence_binding,
        evidence_signature,
    )?;
    if binding_exists {
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
