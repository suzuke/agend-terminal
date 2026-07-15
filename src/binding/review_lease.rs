use std::path::Path;

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
