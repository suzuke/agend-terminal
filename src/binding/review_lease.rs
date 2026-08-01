use std::path::Path;

/// Repair the one legitimate task-id drift for a disposable review binding:
/// assignment authority replaced a cancelled predecessor review task with a new
/// task for the same reviewer, PR subject, and exact head while the reviewer kept
/// the already-provisioned worktree. Every other mismatch fails closed; the
/// ordinary task-completion guard still validates the repaired binding afterward.
pub(crate) fn retarget_disposable_review_binding_for_receipt(
    home: &Path,
    summary: &crate::review_receipt::ReviewReceiptSummary,
) -> Result<bool, String> {
    // Fast no-op preserves the established exact-task path byte-for-byte. This
    // helper owns only replacement-task drift; it is not a new prerequisite for
    // every already-correct validated-review completion.
    let Some(observed_binding) = crate::binding::read(home, &summary.reviewer_name) else {
        return Ok(false);
    };
    if observed_binding["task_id"].as_str() == Some(summary.task_id.as_str()) {
        return Ok(false);
    }
    if !crate::review_receipt::assignment_still_authorizes(home, summary) {
        return Err("review assignment no longer authorizes the receipt".to_string());
    }

    let agent = &summary.reviewer_name;
    let _agent_lock = super::acquire_agent_mutation_lock(home, agent)?;
    let _binding_lock = super::acquire_binding_file_lock(home, agent)?;
    let mut binding = match super::guarded_binding_disk_fresh(home, agent) {
        super::GuardedBinding::Known { value, .. } => value,
        super::GuardedBinding::Absent => return Ok(false),
        super::GuardedBinding::Opaque(reason) => {
            return Err(format!("review binding is opaque: {reason}"))
        }
    };
    if !super::signature_valid(home, agent) {
        return Err("review binding signature is invalid".to_string());
    }
    if binding["agent"].as_str() != Some(agent) {
        return Err("review binding agent does not match the validated reviewer".to_string());
    }

    let Some(binding_task) = binding["task_id"].as_str() else {
        return Ok(false);
    };
    if binding_task == summary.task_id {
        return Ok(false);
    }
    let predecessor_task = binding_task.to_string();
    let expected_head = summary.reviewed_head.as_str();
    if binding["checkout_purpose"].as_str() != Some("disposable_review")
        || binding["provenance"].as_str() != Some("DaemonProvisionedReview")
        || binding["lease_kind"].as_str() != Some("review")
        || binding["provisioned_head"].as_str() != Some(expected_head)
        || binding["expected_head"].as_str() != Some(expected_head)
        || binding["review_assignment_id"]
            .as_str()
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .is_none()
    {
        return Err("stale binding is not an exact typed disposable-review lease".to_string());
    }
    let source_repo = binding["source_repo"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map(Path::new)
        .ok_or_else(|| "stale review binding has no source repository".to_string())?;
    let remote_url = crate::git_helpers::git_cmd(source_repo, &["remote", "get-url", "origin"])
        .map_err(|e| format!("stale review binding source repository is unreadable: {e}"))?;
    if crate::branch_sweep::extract_github_repo_for_intent(&remote_url).as_deref()
        != Some(summary.repo.as_str())
    {
        return Err("stale review binding repository does not match the receipt".to_string());
    }

    let predecessor = crate::tasks::load_routed(home, &predecessor_task)
        .map_err(|e| format!("predecessor review task is not uniquely routed: {e}"))?;
    if predecessor.record().status != crate::task_events::TaskStatus::Cancelled
        || predecessor
            .record()
            .owner
            .as_ref()
            .map(|owner| owner.0.as_str())
            != Some(agent)
        || predecessor.record().branch.as_deref() != Some(summary.branch.as_str())
    {
        return Err(
            "stale binding predecessor is not a cancelled task owned by the reviewer".to_string(),
        );
    }
    let successor = crate::tasks::load_routed(home, &summary.task_id)
        .map_err(|e| format!("successor review task is not uniquely routed: {e}"))?;
    if successor
        .record()
        .owner
        .as_ref()
        .map(|owner| owner.0.as_str())
        != Some(agent)
        || successor.record().branch.as_deref() != Some(summary.branch.as_str())
        || !matches!(
            successor.record().status,
            crate::task_events::TaskStatus::Open
                | crate::task_events::TaskStatus::Claimed
                | crate::task_events::TaskStatus::InProgress
                | crate::task_events::TaskStatus::InReview
                | crate::task_events::TaskStatus::Blocked
        )
    {
        return Err(
            "receipt successor is not an active review task owned by the reviewer".to_string(),
        );
    }
    if !crate::review_receipt::assignment_still_authorizes(home, summary) {
        return Err("review assignment changed during binding repair".to_string());
    }

    binding["task_id"] = serde_json::json!(summary.task_id);
    binding["review_assignment_id"] = serde_json::json!(summary.assignment_id.to_string());
    binding["expected_head"] = serde_json::json!(summary.reviewed_head);
    binding["issued_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    let body = serde_json::to_string_pretty(&binding)
        .map_err(|e| format!("serialize retargeted review binding: {e}"))?;
    let tag = agentic_git_core::integrity_core::sign_binding(home, body.as_bytes())
        .map_err(|e| format!("sign retargeted review binding: {e}"))?;
    let dir = crate::paths::runtime_dir(home).join(agent);
    crate::store::atomic_write(&dir.join("binding.json"), body.as_bytes())
        .map_err(|e| format!("write retargeted review binding: {e}"))?;
    crate::store::atomic_write(&super::binding_sig_path(&dir), tag.as_bytes())
        .map_err(|e| format!("write retargeted review binding signature: {e}"))?;
    if let Ok(mut map) = super::binding_index().write() {
        map.insert(super::index_key(home, agent), binding);
    }
    crate::event_log::log(
        home,
        "review_binding_task_retargeted",
        agent,
        &format!(
            "validated replacement assignment {} moved disposable binding task {} -> {} at {}",
            summary.assignment_id, predecessor_task, summary.task_id, summary.reviewed_head
        ),
    );
    Ok(true)
}

pub(crate) fn augment_binding_with_lease(
    home: &Path,
    agent: &str,
    lease_kind: &str,
    review_assignment_id: &str,
    expected_head: &str,
) -> Result<(), String> {
    let _agent_lock = super::acquire_agent_mutation_lock(home, agent)?;
    let _binding_lock = super::acquire_binding_file_lock(home, agent)?;
    let dir = crate::paths::runtime_dir(home).join(agent);
    let path = dir.join("binding.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("read binding for lease augment: {e}"))?;
    let mut binding: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("parse binding for lease augment: {e}"))?;
    binding["lease_kind"] = serde_json::json!(lease_kind);
    binding["review_assignment_id"] = serde_json::json!(review_assignment_id);
    binding["expected_head"] = serde_json::json!(expected_head);
    let body = serde_json::to_string_pretty(&binding).unwrap_or_default();
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write binding for lease augment: {e}"))?;
    match agentic_git_core::integrity_core::sign_binding(home, body.as_bytes()) {
        Ok(tag) => {
            if let Err(e) =
                crate::store::atomic_write(&super::binding_sig_path(&dir), tag.as_bytes())
            {
                tracing::warn!(%agent, error = %e,
                    "lease augment sig write failed — shim fails closed (deny) until re-bind");
            }
        }
        Err(e) => tracing::warn!(%agent, error = %e,
            "lease augment HMAC sign failed — shim fails closed (deny) until re-bind"),
    }
    if let Ok(mut map) = super::binding_index().write() {
        map.insert(super::index_key(home, agent), binding);
    }
    Ok(())
}

pub(crate) fn try_augment_review_lease(
    home: &Path,
    agent: &str,
    task_id: &str,
    checkout_branch: &str,
    source_repo: &Path,
) {
    if task_id.is_empty() {
        return;
    }
    let Ok(remote_url) = crate::git_helpers::git_cmd(source_repo, &["remote", "get-url", "origin"])
    else {
        return;
    };
    let Some(slug) = crate::branch_sweep::extract_github_repo_for_intent(&remote_url) else {
        return;
    };
    let task = match crate::tasks::load_routed(home, task_id) {
        Ok(rt) => rt.task,
        Err(_) => return,
    };
    let Some(subject_branch) = task.branch.as_deref().filter(|b| !b.is_empty()) else {
        return;
    };
    let Some(assignment) =
        crate::daemon::assignment_authority::get(home, &slug, subject_branch, agent)
    else {
        return;
    };
    if !assignment.is_receipt_capable() {
        return;
    }
    if assignment.task_id != task_id {
        return;
    }
    let current_id = crate::fleet::resolve_uuid(home, agent);
    match (&assignment.target_instance_id, &current_id) {
        (Some(assign_id), Some(cur_id)) if assign_id == cur_id => {}
        _ => return,
    }
    let Some(reviewed_head) = assignment
        .reviewed_head
        .as_deref()
        .filter(|h| !h.is_empty())
    else {
        return;
    };
    let Ok(tip) = crate::git_helpers::git_cmd(source_repo, &["rev-parse", checkout_branch]) else {
        return;
    };
    if tip.trim() != reviewed_head {
        return;
    }
    if let Err(e) = augment_binding_with_lease(
        home,
        agent,
        "review",
        &assignment.assignment_id.to_string(),
        tip.trim(),
    ) {
        tracing::warn!(
            %agent, %task_id, error = %e,
            "review lease augmentation failed — review branch preserved on release"
        );
    }
}
