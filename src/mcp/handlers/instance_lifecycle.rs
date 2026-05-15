//! Sprint 54 P1-B Bug 1: instance-deletion lifecycle + residual-store
//! audit. Extracted from `src/mcp/handlers/instance.rs` to keep the
//! parent file under the `tests/file_size_invariant.rs` 700-LOC ceiling
//! — the audit + transactional-or-loud refactor pushed instance.rs over
//! the limit, and a sibling module is the proper response per that
//! invariant's design intent (split, don't bypass).
//!
//! Public surface re-exported here:
//! - `full_delete_instance` — used by the MCP `delete_instance` handler
//!   (`super::instance::handle_delete_instance`) and the TUI close
//!   path (`crate::app::overlay`). Returns `Result<(), String>`; `Err`
//!   carries the residual-store audit so callers can surface partial
//!   state instead of letting `auto_start_fleet` resurrect the
//!   half-deleted instance on next reconcile.
//! - `name_residual_anywhere` — pure-function audit fn, exposed for
//!   future callers (e.g. `handle_spawn` rejection-message enrichment)
//!   that want to surface the divergent-store list to the operator.

use crate::agent_ops::cleanup_working_dir;
use crate::channel::telegram;
use serde_json::json;
use std::path::Path;

/// Sprint 53 Smoke 2 r1: shared full single-instance teardown used by both
/// the MCP `delete_instance` handler and the TUI close path
/// (`app/overlay.rs::Overlay::ConfirmClose`). Covers everything
/// `handle_delete_instance` historically did EXCEPT the channel-singleton
/// guard, which stays MCP-only — TUI close is operator-driven and we don't
/// want to refuse a close because of channel routing.
///
/// Side effects, all expected for both call sites:
/// - **PTY kill + child-tree reap** via `daemon::lifecycle::delete_transaction`
///   (process-tree kill, synchronous wait-for-exit, registry remove,
///   active-channel binding drop, configs map remove, IPC port remove,
///   event log).
/// - **fleet.yaml entry removal** so daemon restart's `auto_start_fleet`
///   doesn't resurrect the dead agent.
/// - **Telegram topic delete** for the resolved per-instance topic — leaving
///   it would orphan the topic on the chat side.
/// - **Working-dir cleanup** via `cleanup_working_dir` (the shared
///   `home/workspace/<name>` whole-tree branch + the user-dir agend-files
///   branch). Custom-directory deployment subdirs are still cleaned by the
///   reconcile path's `cleanup_deployment_dirs` after this — see
///   `app/overlay.rs` for the layering.
/// - **Team membership removal** so a closed instance doesn't leave a
///   dangling team-member reference.
///
/// Returns `Ok(())` when every fleet store is verified clean post-delete.
/// Returns `Err(detail)` (Sprint 54 P1-B Bug 1 fix — transactional-or-loud)
/// when any store still holds the name after the cleanup run, so the
/// caller can surface the residual rather than silently leaving partial
/// state for `auto_start_fleet` to resurrect on next reconcile. `detail`
/// is a human-readable string listing the residual stores plus any
/// per-step error captured during cleanup.
pub(crate) fn full_delete_instance(home: &Path, name: &str) -> Result<(), String> {
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let (topic_id, working_dir) = fleet
        .as_ref()
        .and_then(|c| {
            c.resolve_instance(name)
                .map(|r| (r.topic_id, r.working_directory))
        })
        .unwrap_or((None, None));
    let topic_id = topic_id.or_else(|| telegram::lookup_topic_for_instance(home, name));

    // Sprint 54 P1-B Bug 1: collect per-step errors instead of silently
    // swallowing them. Each cleanup step runs best-effort so even when
    // earlier steps fail the later ones still get a chance, but every
    // surfaced error feeds the final audit so the caller knows which
    // stores left residual state.
    let mut step_errors: Vec<String> = Vec::new();

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, name) {
        step_errors.push(format!("fleet.yaml removal: {e}"));
        tracing::error!(name, error = %e, "full_delete_instance: fleet.yaml removal failed");
    }
    if let Some(tid) = topic_id {
        telegram::delete_topic(home, tid);
    } else {
        tracing::warn!(%name, "no topic_id found for full_delete_instance — possible orphan");
    }
    if let Some(ref wd) = working_dir {
        cleanup_working_dir(home, name, wd);
    }
    crate::teams::remove_member_from_all(home, name);

    // #808: orphan tasks whose owner is the deleted instance so the
    // ACL gate (`tasks::can_mutate_record`) doesn't lock survivors
    // out. Best-effort like the other cleanup steps — a failure
    // feeds the residual audit but doesn't abort the teardown.
    if let Err(e) = crate::tasks::orphan_tasks_for_owner(home, name) {
        step_errors.push(format!("task orphan: {e}"));
        tracing::error!(name, error = %e, "full_delete_instance: task orphan failed");
    }

    // Sprint 54 P1-B Bug 1 audit: enumerate every store that still holds
    // the name. If any do, surface a loud error instead of returning
    // success — `auto_start_fleet` revival of a half-deleted instance is
    // exactly the silent-drop class pattern this fix prevents.
    let residual = name_residual_anywhere(home, name);
    if residual.is_empty() && step_errors.is_empty() {
        return Ok(());
    }
    let detail = match (residual.is_empty(), step_errors.is_empty()) {
        (true, _) => format!("step errors: {}", step_errors.join("; ")),
        (false, true) => format!("residual stores: {}", residual.join(", ")),
        (false, false) => format!(
            "step errors: {}; residual stores: {}",
            step_errors.join("; "),
            residual.join(", ")
        ),
    };
    tracing::error!(
        name,
        residual = ?residual,
        step_errors = ?step_errors,
        "full_delete_instance left residual state — silent-drop class pattern blocked"
    );
    Err(detail)
}

/// Sprint 54 P1-B Bug 1: enumerate every fleet store that still holds
/// `name` after a delete attempt. Returns the list of store identifiers
/// (`"fleet.yaml"`, `"metadata"`, etc.) so callers can surface the
/// residual loudly. Per the P1-B RCA doc (PR #509 squash 66682d2):
/// three primary stores plus four auxiliary on-disk artefacts where
/// instance-name-bearing state survives delete; this audit covers them
/// all.
///
/// Daemon-process-internal stores (`agent::registry`,
/// `agent::externals`) are NOT post-audited here — they require live
/// registry handles to inspect, and the DELETE step inside
/// `full_delete_instance` clears them as part of its cleanup. The audit
/// fn is positioned for stores whose state survives daemon restart
/// (disk-backed) where the silent-drop revival risk lives.
pub(crate) fn name_residual_anywhere(home: &Path, name: &str) -> Vec<&'static str> {
    let mut sources: Vec<&'static str> = Vec::new();
    if let Ok(cfg) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        if cfg.instances.contains_key(name) {
            sources.push("fleet.yaml");
        }
        if cfg
            .teams
            .values()
            .any(|t| t.members.iter().any(|m| m == name))
        {
            sources.push("fleet.yaml/teams");
        }
    }
    if home.join("metadata").join(format!("{name}.json")).exists() {
        sources.push("metadata");
    }
    if home.join("inbox").join(format!("{name}.jsonl")).exists() {
        sources.push("inbox");
    }
    if home
        .join("runtime")
        .join(name)
        .join("binding.json")
        .exists()
    {
        sources.push("runtime/binding.json");
    }
    if home
        .join("notification-queue")
        .join(format!("{name}.jsonl"))
        .exists()
    {
        sources.push("notification-queue");
    }
    sources
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Sprint 54 P1-B Bug 1 fix: residual-store audit + transactional-or-loud
    //! `full_delete_instance`. Tests cover the audit fn's per-store
    //! detection (clean / each-store-positive / multi-source) and the
    //! delete fn's Result-return contract (Err on residual,
    //! Ok on clean). `full_delete_instance` reaches into the daemon's
    //! `api::call` which fails harmlessly with no daemon — we exercise
    //! the post-cleanup audit branch by pre-seeding residual state
    //! directly, mirroring the silent-drop class production scenario.

    use super::name_residual_anywhere;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-p1b-bug1-test-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn name_residual_anywhere_clean_home_returns_empty() {
        let home = tmp_home("clean");
        assert!(name_residual_anywhere(&home, "ghost").is_empty());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_fleet_yaml_instance_residual() {
        let home = tmp_home("fleet_yaml_inst");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  zombie:\n    backend: claude\n",
        )
        .unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(
            sources.contains(&"fleet.yaml"),
            "fleet.yaml instances residual must surface, got {sources:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_fleet_yaml_team_member_residual() {
        // Sprint 54 PR #507 unification: teams live in fleet.yaml; a
        // delete that misses team membership cleanup leaves the name
        // resolvable as a team member, which the audit must surface
        // separately from the instances: stanza.
        let home = tmp_home("fleet_yaml_team");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  ops:\n    members: [zombie, alice]\n    orchestrator: alice\n",
        )
        .unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(
            sources.contains(&"fleet.yaml/teams"),
            "team-member residual must surface, got {sources:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_metadata_residual() {
        let home = tmp_home("metadata");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(meta_dir.join("zombie.json"), "{}").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"metadata"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_inbox_residual() {
        let home = tmp_home("inbox");
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).unwrap();
        std::fs::write(inbox_dir.join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"inbox"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_runtime_binding_residual() {
        let home = tmp_home("binding");
        let dir = crate::paths::runtime_dir(&home).join("zombie");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("binding.json"), "{}").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"runtime/binding.json"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_notification_queue_residual() {
        let home = tmp_home("nq");
        let dir = home.join("notification-queue");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"notification-queue"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_returns_multi_source_when_several_stores_dirty() {
        // Regression-proof: dropping the per-store check must surface
        // as a missing entry in this list, not as a silent skip.
        let home = tmp_home("multi");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  zombie:\n    backend: claude\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join("metadata")).unwrap();
        std::fs::write(home.join("metadata").join("zombie.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        std::fs::write(home.join("inbox").join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        for expected in ["fleet.yaml", "metadata", "inbox"] {
            assert!(
                sources.contains(&expected),
                "multi-source audit must include {expected}, got {sources:?}"
            );
        }
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn full_delete_instance_returns_err_when_residual_remains_post_cleanup() {
        // Pre-seed metadata + inbox files before delete; daemon API is
        // unreachable in the test process, so `api::call` fails
        // (silently). fleet.yaml removal is also a no-op (no fleet.yaml
        // present). The post-cleanup audit must surface the
        // metadata/inbox residual and the fn must return Err.
        let home = tmp_home("full_residual");
        std::fs::create_dir_all(home.join("metadata")).unwrap();
        std::fs::write(home.join("metadata").join("zombie.json"), "{}").unwrap();
        let result = super::full_delete_instance(&home, "zombie");
        let err = result.expect_err(
            "metadata residual after cleanup must surface as Err — silent-drop class blocked",
        );
        assert!(
            err.contains("metadata"),
            "Err detail must name the residual store, got: {err:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn full_delete_instance_returns_ok_when_no_residual() {
        // Clean home: no fleet.yaml, no metadata, no inbox — every
        // cleanup step is a no-op AND the audit reports clean.
        // `api::call` failure during DELETE is harmless because there's
        // nothing to clean and the audit returns empty.
        let home = tmp_home("full_clean");
        let result = super::full_delete_instance(&home, "ghost");
        assert!(
            result.is_ok(),
            "clean home + clean post-audit must return Ok, got: {result:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn full_delete_instance_orphans_owned_tasks() {
        // #808 GREEN test 2: deleting an instance must orphan the
        // tasks it owns so the ACL gate (`tasks::can_mutate_record`)
        // doesn't lock survivors out. Pre-fix: tasks keep ghost
        // owner → operator gets "not authorized" on cancel.
        // Post-fix: orphan_tasks_for_owner clears assignee before
        // the residual audit so the survivor can mutate.
        let home = tmp_home("orphan_on_delete");
        // Create a task owned by the doomed instance via the public
        // handle entry — this exercises the same event-log flow the
        // MCP `task` tool uses in production.
        let r = crate::tasks::handle(
            &home,
            "doomed",
            &serde_json::json!({"action": "create", "title": "owned task", "assignee": "doomed"}),
        );
        let task_id = r["id"].as_str().expect("task id").to_string();
        // Sanity: pre-delete ownership recorded.
        let pre_tasks = crate::tasks::list_all(&home);
        let pre = pre_tasks
            .iter()
            .find(|t| t.id == task_id)
            .expect("task exists");
        assert_eq!(
            pre.assignee.as_deref(),
            Some("doomed"),
            "pre-delete sanity: task owner must be 'doomed'"
        );
        // Run the full teardown. `api::call` is unreachable in test
        // context (harmless) and there's no fleet.yaml / metadata so
        // the residual audit returns clean.
        let result = super::full_delete_instance(&home, "doomed");
        assert!(
            result.is_ok(),
            "delete on clean home must return Ok, got: {result:?}"
        );
        // Orphan side-effect: assignee cleared.
        let post_tasks = crate::tasks::list_all(&home);
        let post = post_tasks
            .iter()
            .find(|t| t.id == task_id)
            .expect("task still exists post-delete");
        assert!(
            post.assignee.is_none(),
            "owned task must be orphaned after full_delete_instance, got assignee={:?}",
            post.assignee
        );
        std::fs::remove_dir_all(home).ok();
    }
}
