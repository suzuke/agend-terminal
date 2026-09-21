//! Operator-only recovery for a bound review worktree whose marker and Git
//! pointer were lost during a timed-out release.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn remove_empty_dir_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_empty_dir_tree(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryReport {
    pub archive: PathBuf,
}

#[path = "worktree_recovery_archive.rs"]
mod archive;

/// Recover one exact markerless bound worktree.
///
#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_markerless_bound_worktree(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    instance: &str,
    branch: &str,
    worktree: &Path,
    source_repo: &Path,
) -> Result<RecoveryReport, String> {
    if actor.trim().is_empty() || audit_reason.trim().is_empty() {
        return Err("operator and audit_reason are required".to_string());
    }
    crate::agent::validate_name(instance)
        .map_err(|_| format!("invalid instance name '{instance}'"))?;
    if branch.trim().is_empty() {
        return Err("branch is required".to_string());
    }

    // #3696: a completed operator archive is a durable idempotency receipt.
    // It deliberately lives outside runtime/<instance>/, which the first
    // recovery removes after unbinding.  A retry returns the exact same
    // archive and never touches a same-name replacement binding.
    let mut durable_tombstone = crate::agent::deletion_recovery::read(home, instance)?;
    if let Some(tombstone) = durable_tombstone.as_ref() {
        if tombstone.state == crate::agent::deletion_recovery::State::Recovered {
            if tombstone.instance != instance
                || tombstone.branch != branch
                || Path::new(&tombstone.worktree) != worktree
                || Path::new(&tombstone.source_repo) != source_repo
            {
                return Err(
                    "recovery refused: supplied instance/branch/worktree/source does not match tombstone"
                        .to_string(),
                );
            }
            let archive = tombstone
                .archive
                .as_deref()
                .map(PathBuf::from)
                .ok_or_else(|| {
                    "recovery refused: recovered tombstone has no archive".to_string()
                })?;
            if !archive.is_dir() {
                return Err(format!(
                    "recovery refused: recorded archive is unavailable: {}",
                    archive.display()
                ));
            }
            return Ok(RecoveryReport { archive });
        }
    }

    // Lock order is deliberate and shared with normal lifecycle mutation:
    // lifecycle permit -> per-agent mutation lock -> binding-file lock.
    let permit = crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        instance,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    )
    .map_err(|e| format!("recovery refused: {e}"))?;
    let _agent_lock = crate::binding::acquire_agent_mutation_lock(home, instance)?;
    let _binding_lock = crate::binding::acquire_binding_file_lock(home, instance)?;

    let tombstone_recovery = durable_tombstone.as_ref().is_some_and(|tombstone| {
        tombstone.state == crate::agent::deletion_recovery::State::RecoveryRequired
    });
    if !tombstone_recovery && crate::worktree_pool::is_agent_alive(home, instance) {
        return Err(format!(
            "recovery refused: instance '{instance}' still shows liveness"
        ));
    }

    if let Some(tombstone) = durable_tombstone.as_ref() {
        let recorded_archive_exists = tombstone
            .archive
            .as_deref()
            .is_some_and(|archive| Path::new(archive).is_dir());
        if tombstone.archive.is_some() && recorded_archive_exists {
            return archive::recover_recorded_archive(
                home,
                actor,
                audit_reason,
                instance,
                branch,
                worktree,
                source_repo,
                tombstone,
                &permit,
            );
        }
        if !worktree.exists() {
            return archive::recover_absent_worktree(
                home,
                actor,
                audit_reason,
                instance,
                branch,
                worktree,
                source_repo,
                tombstone,
                &permit,
            );
        }
    }

    let root = crate::worktree_pool::daemon_managed_worktree_root(home)
        .canonicalize()
        .map_err(|e| format!("managed worktree root is unavailable: {e}"))?;
    let target = worktree
        .canonicalize()
        .map_err(|e| format!("worktree is unavailable: {e}"))?;
    if !target.starts_with(&root) || target == root {
        return Err(format!(
            "recovery target is outside the daemon worktree root: {}",
            target.display()
        ));
    }
    let source = source_repo
        .canonicalize()
        .map_err(|e| format!("source repository is unavailable: {e}"))?;

    let crate::binding::GuardedBinding::Known { value: binding, .. } =
        crate::binding::guarded_binding_disk_fresh(home, instance)
    else {
        return Err(format!(
            "recovery refused: binding for '{instance}' is absent or opaque"
        ));
    };
    if !crate::binding::signature_valid(home, instance) {
        return Err(format!(
            "recovery refused: binding signature for '{instance}' is not valid"
        ));
    }

    let bound_branch = binding["branch"].as_str().unwrap_or_default();
    let bound_worktree = binding["worktree"].as_str().unwrap_or_default();
    let bound_source = binding["source_repo"].as_str().unwrap_or_default();
    let bound_target = Path::new(bound_worktree)
        .canonicalize()
        .map_err(|e| format!("bound worktree is not canonicalizable: {e}"))?;
    let bound_repo = Path::new(bound_source)
        .canonicalize()
        .map_err(|e| format!("bound source repository is not canonicalizable: {e}"))?;
    if bound_branch != branch || bound_target != target || bound_repo != source {
        return Err(
            "recovery refused: supplied instance/branch/worktree/source does not match binding"
                .to_string(),
        );
    }
    if let Some(tombstone) = durable_tombstone.as_ref() {
        if tombstone.instance != instance
            || !matches!(
                tombstone.state,
                crate::agent::deletion_recovery::State::Deleting
                    | crate::agent::deletion_recovery::State::RecoveryRequired
            )
            || tombstone.branch != bound_branch
            || Path::new(&tombstone.worktree) != Path::new(bound_worktree)
            || Path::new(&tombstone.source_repo) != Path::new(bound_source)
        {
            return Err(
                "recovery refused: supplied identity does not match the delete tombstone"
                    .to_string(),
            );
        }
    }
    if crate::worktree_pool::is_daemon_managed(&target) {
        return Err(
            "recovery refused: worktree still has its managed marker; use normal release"
                .to_string(),
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

    let binding_path = crate::paths::binding_path(home, instance);
    let binding_body = std::fs::read(&binding_path)
        .map_err(|e| format!("read binding metadata {}: {e}", binding_path.display()))?;
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let binding_signature = std::fs::read(&signature_path)
        .map_err(|e| format!("read binding signature {}: {e}", signature_path.display()))?;
    if let Some(tombstone) = durable_tombstone.as_ref() {
        let binding_sha256 = crate::daemon::utils::sha256_hex(&binding_body);
        let signature_sha256 = crate::daemon::utils::sha256_hex(&binding_signature);
        if tombstone.binding_sha256 != binding_sha256
            || tombstone.binding_signature_sha256 != signature_sha256
        {
            return Err(
                "recovery refused: binding evidence does not match the delete tombstone"
                    .to_string(),
            );
        }
    }

    // Public CLI recovery can start without the delete handler having written
    // a tombstone.  All rejection-only preflight checks above intentionally
    // run before this journal write, so a bad operator request cannot leave a
    // blocking Deleting tombstone behind.  Once the signed binding evidence
    // has passed those checks, establish the same durable journal used by the
    // delete handler before any archive mutation so a crash is recoverable.
    if durable_tombstone.is_none() {
        durable_tombstone = crate::agent::deletion_recovery::begin_from_binding(home, instance)?;
        if durable_tombstone.is_none() {
            return Err(
                "recovery refused: signed binding evidence is required for operator recovery"
                    .to_string(),
            );
        }
    }

    let archive_root = home.join(".trash").join("worktrees");
    std::fs::create_dir_all(&archive_root)
        .map_err(|e| format!("create recovery archive root: {e}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let archive = archive_root.join(format!(
        "{instance}-recovery-{}-{}",
        stamp.as_secs(),
        stamp.subsec_nanos()
    ));
    if archive.exists() {
        return Err(format!("recovery archive collision: {}", archive.display()));
    }

    // Publish the planned archive path before the rename.  A daemon crash at
    // either side of the filesystem rename leaves a durable next action: the
    // retry can finish the rename when the source remains, or finish the
    // receipt when the archive already exists.
    if durable_tombstone.is_some() {
        crate::agent::deletion_recovery::mark_recovery_required(home, instance, Some(&archive))?;
    }

    // Prepare self-describing metadata inside the source before rename.  The
    // rename then carries the signed evidence and manifest atomically with the
    // payload, while retries can safely complete any interrupted metadata write.
    archive::write_archive_metadata(
        &target,
        actor,
        audit_reason,
        instance,
        branch,
        &source,
        &target,
        &archive,
        &binding_body,
        &binding_signature,
    )?;

    // Rename is the safety boundary: it is atomic on the normal same-filesystem
    // layout. Cross-device copy is deliberately refused so recovery never turns
    // into a partially copied destructive cleanup.
    std::fs::rename(&target, &archive).map_err(|e| {
        format!(
            "archive rename {} -> {} failed: {e}",
            target.display(),
            archive.display()
        )
    })?;

    match crate::binding::unbind_with_permit(home, instance, &permit) {
        crate::binding::BindingRemoval::Removed => {
            remove_empty_dir_tree(
                &crate::worktree_pool::daemon_managed_worktree_root(home).join(instance),
            );
            let _ = std::fs::remove_dir_all(crate::paths::runtime_dir(home).join(instance));
            if durable_tombstone.is_some() {
                if let Err(error) =
                    crate::agent::deletion_recovery::mark_recovered(home, instance, &archive)
                {
                    return Err(format!(
                        "archive created and binding cleared at {}, but recovery receipt failed: {error}",
                        archive.display()
                    ));
                }
            }
            crate::event_log::log(
                home,
                "markerless_worktree_recovered",
                instance,
                &format!(
                    "actor={actor}; branch={branch}; archive={}; reason={audit_reason}",
                    archive.display()
                ),
            );
            Ok(RecoveryReport { archive })
        }
        crate::binding::BindingRemoval::Absent => Err(format!(
            "binding disappeared during recovery; archive remains at {}",
            archive.display()
        )),
        crate::binding::BindingRemoval::Failed(error) => {
            // `unbind_with_permit` may have removed binding.json before a
            // sidecar cleanup error. Never put the payload back at its old
            // path without a known binding owner; leave the archive as the
            // durable recovery boundary for a later operator inspection.
            Err(format!(
                "binding removal failed; archive remains at {}: {error}",
                archive.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-worktree-recovery-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create temporary home");
        home
    }

    #[test]
    fn markerless_bound_worktree_is_archived_and_binding_cleared() {
        let home = temp_home("happy");
        let instance = format!("recovery-test-{}", std::process::id());
        let branch = "review/orphan";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-orphan");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        std::fs::write(worktree.join("leftover.txt"), b"preserve me")
            .expect("write residual worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");

        let report = recover_markerless_bound_worktree(
            &home,
            "operator",
            "recover timed-out review worktree",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect("known markerless binding should be recoverable");

        assert!(!worktree.exists());
        assert_eq!(
            std::fs::read(report.archive.join("leftover.txt")).expect("read archived fixture"),
            b"preserve me"
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(report.archive.join(".agend-recovery-manifest.json"))
                .expect("read recovery manifest"),
        )
        .expect("parse recovery manifest");
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["actor"], "operator");
        assert_eq!(manifest["instance"], instance);
        assert!(report
            .archive
            .join(".agend-recovery-binding.json")
            .is_file());
        assert!(report
            .archive
            .join(".agend-recovery-binding.json.sig")
            .is_file());
        assert!(crate::binding::read(&home, &instance).is_none());
        assert_eq!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery receipt")
                .expect("CLI recovery must journal signed binding")
                .state,
            crate::agent::deletion_recovery::State::Recovered
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn interrupted_deleting_tombstone_is_recoverable_after_restart() {
        let home = temp_home("interrupted-delete");
        let instance = format!("recovery-interrupted-{}", std::process::id());
        let branch = "review/interrupted";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-interrupted");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        std::fs::write(worktree.join("leftover.txt"), b"survive restart")
            .expect("write residual worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");

        crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
            .expect("persist interrupted deleting tombstone")
            .expect("signed binding must enter recovery lane");

        let report = recover_markerless_bound_worktree(
            &home,
            "operator",
            "recover interrupted delete after restart",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect("operator recovery must accept an interrupted deleting tombstone");

        assert!(!worktree.exists());
        assert_eq!(
            std::fs::read(report.archive.join("leftover.txt")).expect("read archived fixture"),
            b"survive restart"
        );
        assert!(crate::binding::read(&home, &instance).is_none());
        assert_eq!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery receipt")
                .expect("receipt must persist")
                .state,
            crate::agent::deletion_recovery::State::Recovered
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn archived_recovery_receipt_retry_survives_binding_cleanup() {
        let home = temp_home("receipt-retry");
        let instance = format!("recovery-receipt-{}", std::process::id());
        let branch = "review/receipt-retry";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-receipt-retry");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");
        let binding_body = std::fs::read(crate::paths::binding_path(&home, &instance))
            .expect("read binding fixture");
        let binding_signature = std::fs::read(
            crate::paths::runtime_dir(&home)
                .join(&instance)
                .join("binding.json.sig"),
        )
        .expect("read binding signature fixture");
        crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
            .expect("persist deleting tombstone")
            .expect("signed binding must enter recovery lane");

        let archive = home
            .join(".trash")
            .join("worktrees")
            .join(format!("{instance}-recovery-retry"));
        std::fs::create_dir_all(&archive).expect("create recovery archive fixture");
        std::fs::write(archive.join(".agend-recovery-binding.json"), &binding_body)
            .expect("write archived binding fixture");
        std::fs::write(
            archive.join(".agend-recovery-binding.json.sig"),
            &binding_signature,
        )
        .expect("write archived signature fixture");
        std::fs::write(archive.join("leftover.txt"), b"archived before receipt")
            .expect("write archived payload fixture");
        crate::agent::deletion_recovery::mark_recovery_required(&home, &instance, Some(&archive))
            .expect("persist archive before simulated crash");
        std::fs::remove_dir_all(&worktree).expect("simulate archived worktree");
        std::fs::remove_dir_all(crate::paths::runtime_dir(&home).join(&instance))
            .expect("simulate binding cleanup before receipt");

        let report = recover_markerless_bound_worktree(
            &home,
            "operator",
            "retry recovery receipt after restart",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect("operator retry must complete an archived recovery receipt");

        assert_eq!(report.archive, archive);
        assert_eq!(
            std::fs::read(report.archive.join("leftover.txt")).expect("read archived payload"),
            b"archived before receipt"
        );
        assert_eq!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery receipt")
                .expect("receipt must persist")
                .state,
            crate::agent::deletion_recovery::State::Recovered
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn archive_metadata_gap_is_recoverable_after_restart() {
        let home = temp_home("metadata-gap");
        let instance = format!("recovery-metadata-gap-{}", std::process::id());
        let branch = "review/metadata-gap";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-metadata-gap");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");
        crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
            .expect("persist deleting tombstone")
            .expect("signed binding must enter recovery lane");

        let archive = home
            .join(".trash")
            .join("worktrees")
            .join(format!("{instance}-metadata-gap"));
        std::fs::create_dir_all(&archive).expect("create partial archive fixture");
        std::fs::write(
            archive.join("leftover.txt"),
            b"archive survived metadata crash",
        )
        .expect("write archived payload fixture");
        crate::agent::deletion_recovery::mark_recovery_required(&home, &instance, Some(&archive))
            .expect("persist archive path before simulated crash");
        std::fs::remove_dir_all(&worktree).expect("simulate rename before metadata writes");

        let report = recover_markerless_bound_worktree(
            &home,
            "operator",
            "complete metadata after restart",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect("recovery must complete metadata after rename crash");

        assert_eq!(
            std::fs::read(report.archive.join("leftover.txt")).expect("read archived payload"),
            b"archive survived metadata crash"
        );
        assert!(report
            .archive
            .join(".agend-recovery-binding.json")
            .is_file());
        assert!(report
            .archive
            .join(".agend-recovery-binding.json.sig")
            .is_file());
        assert!(report
            .archive
            .join(".agend-recovery-manifest.json")
            .is_file());
        assert_eq!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery receipt")
                .expect("receipt must persist")
                .state,
            crate::agent::deletion_recovery::State::Recovered
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn partial_archive_evidence_repairs_each_written_prefix_3696() {
        for (case, write_binding) in [("binding", true), ("signature", false)] {
            let home = temp_home(&format!("partial-archive-{case}"));
            let instance = format!("recovery-partial-{case}-{}", std::process::id());
            let branch = format!("review/partial-{case}");
            let source_repo = home.join("source-repo");
            std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
            let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
                .join(&instance)
                .join("review-partial");
            std::fs::create_dir_all(&worktree).expect("create worktree fixture");
            crate::binding::bind_full(
                &home,
                &instance,
                "",
                &branch,
                &worktree,
                &source_repo,
                false,
            )
            .expect("bind recovery fixture");
            let binding_body = std::fs::read(crate::paths::binding_path(&home, &instance))
                .expect("read binding fixture");
            let binding_signature = std::fs::read(
                crate::paths::runtime_dir(&home)
                    .join(&instance)
                    .join("binding.json.sig"),
            )
            .expect("read binding signature fixture");
            crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
                .expect("persist deleting tombstone")
                .expect("signed binding must enter recovery lane");

            let archive = home
                .join(".trash")
                .join("worktrees")
                .join(format!("{instance}-archive"));
            std::fs::create_dir_all(&archive).expect("create partial archive fixture");
            std::fs::write(archive.join("leftover.txt"), b"partial archive payload")
                .expect("write archived payload fixture");
            let partial_path = if write_binding {
                archive.join(".agend-recovery-binding.json")
            } else {
                archive.join(".agend-recovery-binding.json.sig")
            };
            let partial_bytes = if write_binding {
                &binding_body
            } else {
                &binding_signature
            };
            std::fs::write(partial_path, partial_bytes).expect("write partial archive evidence");
            crate::agent::deletion_recovery::mark_recovery_required(
                &home,
                &instance,
                Some(&archive),
            )
            .expect("persist archive path before simulated crash");
            std::fs::remove_dir_all(&worktree).expect("simulate archived worktree");

            let report = recover_markerless_bound_worktree(
                &home,
                "operator",
                "repair partial archive evidence after restart",
                &instance,
                &branch,
                &worktree,
                &source_repo,
            )
            .expect("retry must repair a partial archive metadata prefix");
            assert_eq!(report.archive, archive);
            assert!(archive.join(".agend-recovery-binding.json").is_file());
            assert!(archive.join(".agend-recovery-binding.json.sig").is_file());
            assert!(archive.join(".agend-recovery-manifest.json").is_file());
            assert!(crate::binding::read(&home, &instance).is_none());
            assert_eq!(
                crate::agent::deletion_recovery::read(&home, &instance)
                    .expect("read recovered receipt")
                    .expect("recovery receipt must remain durable")
                    .state,
                crate::agent::deletion_recovery::State::Recovered
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn partial_live_binding_evidence_reconciles_from_recorded_archive_3696() {
        for (case, remove_binding) in [("binding", true), ("signature", false)] {
            let home = temp_home(&format!("partial-live-{case}"));
            let instance = format!("recovery-live-partial-{case}-{}", std::process::id());
            let branch = format!("review/live-partial-{case}");
            let source_repo = home.join("source-repo");
            std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
            let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
                .join(&instance)
                .join("review-live-partial");
            std::fs::create_dir_all(&worktree).expect("create worktree fixture");
            crate::binding::bind_full(
                &home,
                &instance,
                "",
                &branch,
                &worktree,
                &source_repo,
                false,
            )
            .expect("bind recovery fixture");
            let binding_body = std::fs::read(crate::paths::binding_path(&home, &instance))
                .expect("read binding fixture");
            let binding_signature = std::fs::read(
                crate::paths::runtime_dir(&home)
                    .join(&instance)
                    .join("binding.json.sig"),
            )
            .expect("read binding signature fixture");
            crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
                .expect("persist deleting tombstone")
                .expect("signed binding must enter recovery lane");

            let archive = home
                .join(".trash")
                .join("worktrees")
                .join(format!("{instance}-archive"));
            std::fs::create_dir_all(&archive).expect("create complete archive fixture");
            std::fs::write(archive.join(".agend-recovery-binding.json"), &binding_body)
                .expect("write archived binding evidence");
            std::fs::write(
                archive.join(".agend-recovery-binding.json.sig"),
                &binding_signature,
            )
            .expect("write archived signature evidence");
            crate::agent::deletion_recovery::mark_recovery_required(
                &home,
                &instance,
                Some(&archive),
            )
            .expect("persist archive path before simulated cleanup interruption");
            std::fs::remove_dir_all(&worktree).expect("simulate archived worktree");
            if remove_binding {
                std::fs::remove_file(crate::paths::binding_path(&home, &instance))
                    .expect("simulate binding cleanup before signature cleanup");
            } else {
                std::fs::remove_file(
                    crate::paths::runtime_dir(&home)
                        .join(&instance)
                        .join("binding.json.sig"),
                )
                .expect("simulate signature cleanup before binding cleanup");
            }

            let report = recover_markerless_bound_worktree(
                &home,
                "operator",
                "reconcile partial live recovery evidence after restart",
                &instance,
                &branch,
                &worktree,
                &source_repo,
            )
            .expect("complete archive evidence must reconcile partial live cleanup");
            assert_eq!(report.archive, archive);
            assert!(crate::binding::read(&home, &instance).is_none());
            assert_eq!(
                crate::agent::deletion_recovery::read(&home, &instance)
                    .expect("read recovered receipt")
                    .expect("recovery receipt must remain durable")
                    .state,
                crate::agent::deletion_recovery::State::Recovered
            );
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn marker_present_target_is_left_for_normal_release() {
        let home = temp_home("managed");
        let instance = format!("recovery-managed-{}", std::process::id());
        let branch = "review/managed";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-managed");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        std::fs::write(
            worktree.join(crate::worktree_pool::MANAGED_MARKER),
            "agent=recovery-managed\n",
        )
        .expect("write managed marker fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");

        let error = recover_markerless_bound_worktree(
            &home,
            "operator",
            "must refuse managed target",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect_err("managed targets belong to normal release");
        assert!(error.contains("managed marker"));
        assert!(worktree.exists());
        assert!(crate::binding::read(&home, &instance).is_some());
        assert!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery state")
                .is_none(),
            "a rejected preflight must not leave a blocking tombstone"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn binding_identity_mismatch_is_rejected_without_mutation() {
        let home = temp_home("mismatch");
        let instance = format!("recovery-mismatch-{}", std::process::id());
        let branch = "review/mismatch";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-mismatch");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind recovery fixture");

        let error = recover_markerless_bound_worktree(
            &home,
            "operator",
            "must reject wrong branch",
            &instance,
            "review/wrong",
            &worktree,
            &source_repo,
        )
        .expect_err("wrong binding identity must be rejected");
        assert!(error.contains("does not match binding"));
        assert!(worktree.exists());
        assert!(crate::binding::read(&home, &instance).is_some());
        assert!(
            crate::agent::deletion_recovery::read(&home, &instance)
                .expect("read recovery state")
                .is_none(),
            "a rejected preflight must not leave a blocking tombstone"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn absent_recovery_rejects_target_outside_managed_root() {
        let home = temp_home("absent-outside-root");
        let instance = format!("recovery-outside-root-{}", std::process::id());
        let branch = "review/outside-root";
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        std::fs::create_dir_all(crate::worktree_pool::daemon_managed_worktree_root(&home))
            .expect("create managed worktree root");
        let worktree = home
            .join("outside-worktrees")
            .join(&instance)
            .join("review-outside-root");
        std::fs::create_dir_all(&worktree).expect("create outside worktree fixture");
        crate::binding::bind_full(&home, &instance, "", branch, &worktree, &source_repo, false)
            .expect("bind outside-root recovery fixture");
        crate::agent::deletion_recovery::begin_from_binding(&home, &instance)
            .expect("persist outside-root tombstone")
            .expect("signed binding must enter recovery lane");
        std::fs::remove_dir_all(&worktree).expect("simulate removed outside worktree");

        let error = recover_markerless_bound_worktree(
            &home,
            "operator",
            "reject outside managed root",
            &instance,
            branch,
            &worktree,
            &source_repo,
        )
        .expect_err("absent recovery must prove the managed target root");
        assert!(error.contains("daemon worktree root"), "{error}");
        assert!(crate::binding::read(&home, &instance).is_some());
        assert!(crate::agent::deletion_recovery::read(&home, &instance)
            .expect("read retained tombstone")
            .is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn concurrent_recovery_archives_once_and_clears_once() {
        let home = temp_home("race");
        let instance = format!("recovery-race-{}", std::process::id());
        let branch = "review/race".to_string();
        let source_repo = home.join("source-repo");
        std::fs::create_dir_all(&source_repo).expect("create source repository fixture");
        let worktree = crate::worktree_pool::daemon_managed_worktree_root(&home)
            .join(&instance)
            .join("review-race");
        std::fs::create_dir_all(&worktree).expect("create worktree fixture");
        std::fs::write(worktree.join("leftover.txt"), b"race-safe")
            .expect("write residual worktree fixture");
        crate::binding::bind_full(
            &home,
            &instance,
            "",
            &branch,
            &worktree,
            &source_repo,
            false,
        )
        .expect("bind recovery fixture");

        let first_home = home.clone();
        let first_instance = instance.clone();
        let first_branch = branch.clone();
        let first_worktree = worktree.clone();
        let first_source = source_repo.clone();
        let first = std::thread::spawn(move || {
            recover_markerless_bound_worktree(
                &first_home,
                "operator-a",
                "concurrent recovery",
                &first_instance,
                &first_branch,
                &first_worktree,
                &first_source,
            )
        });
        let second_home = home.clone();
        let second_instance = instance.clone();
        let second_branch = branch.clone();
        let second_worktree = worktree.clone();
        let second_source = source_repo.clone();
        let second = std::thread::spawn(move || {
            recover_markerless_bound_worktree(
                &second_home,
                "operator-b",
                "concurrent recovery",
                &second_instance,
                &second_branch,
                &second_worktree,
                &second_source,
            )
        });
        let results = [
            first.join().expect("join first recovery attempt"),
            second.join().expect("join second recovery attempt"),
        ];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(crate::binding::read(&home, &instance).is_none());
        assert!(!worktree.exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
