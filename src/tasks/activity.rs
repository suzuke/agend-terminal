//! Task activity timeline rendering.

pub(super) fn summarize_event(
    env: &crate::task_events::TaskEventEnvelope,
) -> (&str, String, String) {
    use crate::task_events::TaskEvent;
    let actor = env.instance.0.clone();
    match &env.event {
        TaskEvent::Created {
            title,
            branch,
            owner,
            ..
        } => {
            let assignee = owner.as_ref().map(|o| o.0.as_str()).unwrap_or("unassigned");
            let br = branch.as_deref().unwrap_or("");
            let summary = if br.is_empty() {
                format!("created task: {title} (assignee: {assignee})")
            } else {
                format!("created task: {title} (assignee: {assignee}, branch: {br})")
            };
            ("created", actor, summary)
        }
        TaskEvent::Claimed { by, .. } => ("claimed", by.0.clone(), "claimed task".to_string()),
        TaskEvent::InProgress { by, .. } => {
            ("in_progress", by.0.clone(), "started work".to_string())
        }
        TaskEvent::Verified { by_reviewer, .. } => {
            ("verified", by_reviewer.0.clone(), "verified".to_string())
        }
        TaskEvent::Done { source, .. } => {
            let detail = match source {
                crate::task_events::DoneSource::PrMerged { pr_id, .. } => {
                    format!("done (PR {} merged)", pr_id)
                }
                crate::task_events::DoneSource::OperatorManual { result, .. } => {
                    format!(
                        "done{}",
                        result
                            .as_ref()
                            .map(|r| format!(": {r}"))
                            .unwrap_or_default()
                    )
                }
                _ => "done".to_string(),
            };
            ("done", actor, detail)
        }
        TaskEvent::Cancelled { by, reason, .. } => {
            ("cancelled", by.0.clone(), format!("cancelled: {reason}"))
        }
        TaskEvent::Superseded {
            by, successor_id, ..
        } => (
            "superseded",
            by.0.clone(),
            format!("superseded by {}", successor_id.0),
        ),
        TaskEvent::Blocked { reason, .. } => ("blocked", actor, format!("blocked: {reason}")),
        TaskEvent::Unblocked { .. } => ("unblocked", actor, "unblocked".to_string()),
        TaskEvent::Reopened { reason, .. } => ("reopened", actor, format!("reopened: {reason}")),
        TaskEvent::Released { reason, .. } => {
            ("released", actor, format!("released claim: {reason}"))
        }
        TaskEvent::Linked { pr_id, .. } => ("linked", actor, format!("linked PR {pr_id}")),
        TaskEvent::TaskCloseProposed { .. } => (
            "close_proposed",
            actor,
            "close proposed by sweep".to_string(),
        ),
        TaskEvent::OwnerAssigned { owner, .. } => {
            let o = owner.as_ref().map(|n| n.0.as_str()).unwrap_or("none");
            ("owner_assigned", actor, format!("assigned to {o}"))
        }
        TaskEvent::PriorityChanged { priority, .. } => {
            ("priority_changed", actor, format!("priority → {priority}"))
        }
        TaskEvent::DescriptionUpdated { .. } => (
            "description_updated",
            actor,
            "description updated".to_string(),
        ),
        TaskEvent::TagsSet { tags, .. } => ("tags_set", actor, format!("tags → {tags:?}")),
        TaskEvent::ResultSet { by, .. } => ("result_set", by.0.clone(), "result set".to_string()),
        TaskEvent::MetadataSet { key, value, by, .. } => (
            "metadata_set",
            by.0.clone(),
            format!("metadata[{key}] = {value}"),
        ),
        TaskEvent::MovedToBacklog { .. } => ("moved_to_backlog", actor, "→ backlog".to_string()),
        TaskEvent::MovedToReview { .. } => ("moved_to_review", actor, "→ in_review".to_string()),
        TaskEvent::BranchLinked { branch, by, .. } => {
            ("branch_linked", by.0.clone(), format!("branch → {branch}"))
        }
    }
}
