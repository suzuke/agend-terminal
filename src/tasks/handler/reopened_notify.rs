use std::path::Path;

/// t-20260713015904150648-15764-33: after a terminal→open `Reopened` batch has
/// COMMITTED and the append flock has dropped, hand the preserved owner one
/// fresh durable `task_reopened` inbox row. A team-owned task routes to the
/// record's `routed_to` orchestrator; an ownerless task notifies no one. One
/// row per committed reopen generation — never a reuse of the completed
/// generation's dispatch/inbox rows, and ordinary create/dispatch semantics
/// are untouched. Best-effort: a notify failure never fails the update (same
/// contract as the #2305 decision-answered notify; handler context holds no
/// registry lock, satisfying the #1492 self-IPC guard).
pub(super) fn notify_reopened_owner(
    home: &Path,
    project: &str,
    id: &str,
    reopened_from: &'static str,
    by: &str,
    record: &crate::task_events::TaskRecord,
) {
    let Some(owner) = record.owner.as_ref() else {
        return;
    };
    let target = record.routed_to.as_ref().unwrap_or(owner).0.as_str();
    let result_line = match record.result.as_deref() {
        Some(r) if !r.is_empty() => {
            // Results can run to hundreds of chars — excerpt, don't dump.
            let excerpt: String = r.chars().take(200).collect();
            let ellipsis = if r.chars().count() > 200 { "…" } else { "" };
            format!("its recorded result was: {excerpt}{ellipsis}")
        }
        _ => "no result was recorded on the closed generation".to_string(),
    };
    let text = format!(
        "[task_reopened] task {id} on board '{project}' was reopened ({reopened_from} → open) by '{by}'.\n\
         Owner '{owner}' is preserved and the task needs action again:\n\
         1. Re-claim before working (task action=claim id={id}) — reopen does not re-claim for you.\n\
         2. Reconcile status/result: the prior {reopened_from} outcome no longer stands; {result_line}\n\
            Update or replace the result to reflect the re-work before closing again.",
        owner = owner.0,
    );
    if let Err(e) = crate::inbox::notify_system(
        home,
        target,
        "system:task_board",
        "task_reopened",
        text,
        Some(id),
        Some(id),
    ) {
        tracing::warn!(task = id, target, error = %e, "task_reopened owner notify failed");
    }
}
