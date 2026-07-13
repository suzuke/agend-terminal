//! Instance-deletion lifecycle + residual-store audit (Sprint 54 P1-B
//! Bug 1). The `lifecycle` submodule of the `instance_state` concept:
//! the transactional-or-loud teardown that `handle_delete_instance`
//! delegates to, kept beside its sibling `spawn` submodule.
//!
//! Public surface:
//! - `full_delete_instance` — used by the MCP `delete_instance` handler
//!   (`super::handle_delete_instance`) and the TUI close
//!   path (`crate::app::overlay`). Returns `Result<(), String>`; `Err`
//!   carries the residual-store audit so callers can surface partial
//!   state instead of letting `auto_start_fleet` resurrect the
//!   half-deleted instance on next reconcile.
//! - `name_residual_anywhere` — pure-function audit fn, exposed for
//!   future callers (e.g. `handle_spawn` rejection-message enrichment)
//!   that want to surface the divergent-store list to the operator.

use crate::channel::telegram;
use serde_json::json;
use std::path::Path;

/// #1907: remove `dir` and any empty subdirectories bottom-up, stopping at any
/// non-empty dir. Used to drop a deleted agent's worktree root including the
/// intermediate dirs a slash-containing branch nests (`worktrees/<name>/feat/x/`),
/// while preserving a refused unmanaged worktree (its real files keep its dir
/// non-empty, so it's left for the residual audit to surface).
pub(crate) fn remove_empty_dir_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_empty_dir_tree(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir); // succeeds only if now-empty
}

/// #2764 R6 seam: fires after the pure authority gate and BEFORE the first
/// mutation (process stop) — the reviewer's "replacement before authority
/// acquisition" injection point. Thread-local one-shot (nextest =
/// process-per-test keeps it hermetic).
#[cfg(test)]
pub(crate) mod full_delete_test_seam {
    use std::cell::RefCell;
    thread_local! {
        static AFTER_GATE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        static POST_COMMIT: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        // #2764 R7/R8: forced RESPONSE for the process-stop DELETE call
        // (P0-2 REDs + pinned-success injection for daemon-less tests).
        // PERSISTENT until the thread ends — multi-delete flows (team
        // cascades) re-read it per delete.
        static STOP_CALL: RefCell<Option<serde_json::Value>> = const { RefCell::new(None) };
    }
    pub(crate) fn set_after_gate(f: Box<dyn FnOnce()>) {
        AFTER_GATE.with(|h| *h.borrow_mut() = Some(f));
    }
    pub(crate) fn fire_after_gate() {
        if let Some(f) = AFTER_GATE.with(|h| h.borrow_mut().take()) {
            f();
        }
    }
    /// Fires after a Clean destructive commit (or the vacuous-ghost
    /// fall-through), BEFORE the tail cleanup — the reviewer's
    /// "post-CAS / pre-tail" injection point.
    pub(crate) fn set_post_commit(f: Box<dyn FnOnce()>) {
        POST_COMMIT.with(|h| *h.borrow_mut() = Some(f));
    }
    pub(crate) fn fire_post_commit() {
        if let Some(f) = POST_COMMIT.with(|h| h.borrow_mut().take()) {
            f();
        }
    }
    pub(crate) fn set_stop_call(v: serde_json::Value) {
        STOP_CALL.with(|h| *h.borrow_mut() = Some(v));
    }
    pub(crate) fn take_stop_call() -> Option<anyhow::Result<serde_json::Value>> {
        STOP_CALL.with(|h| h.borrow().clone().map(Ok))
    }
}

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
///   doesn't resurrect the dead agent. #2764 E: removal happens ONLY via the
///   exact-id generation-CAS inside the destructive phase, and ONLY after the
///   working-dir cleanup came back Clean — a preserved/failed cleanup keeps
///   the entry and surfaces as `Err`.
/// - **Telegram topic delete** for the resolved per-instance topic — leaving
///   it would orphan the topic on the chat side.
/// - **Working-dir cleanup** via `workspace_cleanup::plan_full_delete` →
///   `execute_full_delete` (#2764 E/R6): id-anchored whole-tree removal of the
///   exclusive canonical default dir, or agend-file scrub of an exclusive
///   user-provided dir, committed under the fleet-flock generation fence;
///   every ownership ambiguity aborts the WHOLE delete as a complete
///   fail-closed no-op (no process kill, no store cleanup).
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
    // #1915: mark this instance "deleting" for the ENTIRE teardown — the guard is
    // held until this fn returns (after workspace cleanup + the residual audit
    // below), so any concurrent spawn (boot-stagger / crash-respawn worker /
    // stage2) is refused at the `spawn_agent` / `spawn_and_register_agent`
    // chokepoints. `DeletingGuard::drop` un-marks on EVERY path (normal return,
    // early `Err`, panic), so the name is always re-creatable afterwards — a
    // leaked mark would make it un-spawnable for the daemon's lifetime.
    // #2764 R7: refuses while a same-name CREATE admission is in flight —
    // deleting a half-created generation is neither refuse-clean nor survive.
    let _delete_guard = match crate::agent::deleting::mark_deleting(home, name) {
        Ok(g) => g,
        Err(reason) => {
            return Err(format!("delete refused before any mutation: {reason}"));
        }
    };
    // #2764 E: the RAW immutable pre-removal snapshot is the path authority.
    // Distinguish MISSING (no roster → empty survivor set, ghost-delete flow)
    // from UNREADABLE/CORRUPT (a roster may exist that we cannot read → the
    // destructive phase fails closed on `None`).
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let fleet = if fleet_path.exists() {
        crate::fleet::FleetConfig::load(&fleet_path).ok()
    } else {
        Some(crate::fleet::FleetConfig::default())
    };
    let topic_id = fleet
        .as_ref()
        .and_then(|c| c.resolve_instance(name))
        .and_then(|r| r.topic_id);
    // #1157: extract InstanceId before fleet.yaml removal for id-based metadata cleanup.
    let instance_id = fleet
        .as_ref()
        .and_then(|c| c.instances.get(name))
        .and_then(|i| i.id.clone());

    // #2764 R6 (codex blocker 1 at 650e24e0): PURE authority gate BEFORE ANY
    // mutation. A `Preserve` verdict (fleet unreadable, no entry for an
    // existing dir, legacy/no-id, raw dotdot, shared/ambiguous plan) makes the
    // ENTIRE full delete a complete production no-op — no process kill, no
    // topic delete, no name-keyed store cleanup. A stale delete must never
    // gut a same-name replacement's stores while "only" preserving its dir.
    use crate::agent_ops::workspace_cleanup::{
        execute_full_delete, plan_full_delete, CleanupOutcome, FullDeletePlan,
    };
    let plan = plan_full_delete(home, name, fleet.as_ref());
    if let FullDeletePlan::Preserve { reason } = &plan {
        tracing::warn!(name, %reason,
            "full_delete_instance: authority gate preserved — complete no-op");
        return Err(format!(
            "workspace ownership preserved (fail-closed) — full delete aborted with no mutation: {reason}"
        ));
    }

    #[cfg(test)]
    full_delete_test_seam::fire_after_gate();

    // Sprint 54 P1-B Bug 1: collect per-step errors instead of silently
    // swallowing them. Each cleanup step runs best-effort so even when
    // earlier steps fail the later ones still get a chance, but every
    // surfaced error feeds the final audit so the caller knows which
    // stores left residual state.
    let mut step_errors: Vec<String> = Vec::new();

    // Process stop (PTY kill + child reap). Generation fence for this step is
    // the `_delete_guard` held above, NOT the fleet flock: `mark_deleting`
    // makes every same-name spawn refuse at the `spawn_agent` /
    // `spawn_and_register_agent` chokepoints AND every create path refuse at
    // its `admit_create` admission for the lifetime of this fn, so the
    // registry cannot hold a REPLACEMENT process under this name. The DELETE
    // call additionally pins `expected_id` so the daemon refuses a
    // generation-mismatched stop outright. (Holding the fleet flock across
    // this IPC round-trip instead would risk a self-deadlock: the daemon-side
    // DELETE handler loads fleet.yaml, whose id-backfill write re-acquires
    // the flock.)
    //
    // #2764 R7 (codex P0-2): the destructive commit REQUIRES a proven
    // process-stop disposition. `{ok:true}` without `stopped:false` proves the
    // stop (or that nothing was registered to stop); `stopped:false` = the
    // child did not exit within the kill timeout → abort; an unreachable
    // daemon API is provably-nothing-to-stop ONLY when no live-agent port
    // evidence exists for the name. Nothing destructive has happened yet, so
    // an abort here is a complete no-op beyond the kill attempt itself.
    let stop_params = match &instance_id {
        Some(id) => json!({"name": name, "expected_id": id}),
        None => json!({"name": name}),
    };
    #[allow(unused_mut)]
    let mut stop_resp = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": stop_params}),
    );
    #[cfg(test)]
    if let Some(forced) = full_delete_test_seam::take_stop_call() {
        stop_resp = forced;
    }
    let disposition: Result<(), String> = match &stop_resp {
        // #2764 R8: a PINNED managed delete requires the EXPLICIT
        // `stopped:true` verdict — a missing field (older daemon, unexpected
        // arm) is unproven, not implicit success. Only the unpinned ghost
        // flow (no generation to protect) accepts a bare ok:true.
        Ok(v) if v["ok"].as_bool() == Some(true) && v["stopped"].as_bool() == Some(true) => Ok(()),
        Ok(v)
            if v["ok"].as_bool() == Some(true)
                && instance_id.is_none()
                && v["stopped"].as_bool() != Some(false) =>
        {
            Ok(())
        }
        Ok(v) if v["ok"].as_bool() == Some(true) && v["stopped"].as_bool() == Some(false) => {
            Err("child did not exit within the kill timeout (stopped=false)".to_string())
        }
        Ok(v) if v["ok"].as_bool() == Some(true) => {
            Err("pinned delete requires an explicit stopped:true verdict (missing)".to_string())
        }
        Ok(v) => Err(format!(
            "DELETE refused: {}",
            v["error"].as_str().unwrap_or("unknown")
        )),
        Err(e) => {
            // R8': for a PINNED managed delete an unreachable daemon API is
            // ALWAYS unproven — a missing `.port` sidecar is not OS/registry/
            // generation evidence of a dead child. Only the unpinned ghost
            // flow (no generation to protect) proceeds, and even it fails
            // closed when live port evidence exists.
            if instance_id.is_some() {
                Err(format!(
                    "daemon API unreachable ({e}) — a pinned delete requires a live stop verdict"
                ))
            } else if crate::ipc::port_path(&crate::daemon::run_dir(home), name).exists() {
                Err(format!(
                    "daemon API unreachable ({e}) while live port evidence exists for '{name}'"
                ))
            } else {
                Ok(())
            }
        }
    };
    if let Err(reason) = disposition {
        tracing::error!(name, %reason,
            "full_delete_instance: process-stop disposition unproven — aborting before any destruction");
        return Err(format!(
            "process-stop disposition unproven — full delete aborted before any destruction: {reason}"
        ));
    }

    // #2764 E/R6: the id-anchored destructive commit — the ONLY path that may
    // destroy the victim's working dir AND the ONLY remover of its fleet.yaml
    // entry (exact-id generation-CAS). The whole commit runs under the fleet
    // flock (codex blocker 2): fresh raw read → exact-id verify → re-plan →
    // final pre-destruction verify → execute → CAS, so no in-model writer can
    // interleave a same-name replacement between check and commit. Preserved /
    // Failed STOPS the delete here: no topic, metadata, inbox, team, task,
    // watch, worktree, binding, runtime or port cleanup runs — a surviving
    // generation's stores must never be erased past a non-Clean phase.
    if let FullDeletePlan::Destructive(intent) = plan {
        match execute_full_delete(home, name, intent) {
            CleanupOutcome::Clean => {}
            CleanupOutcome::Preserved { reason } => {
                tracing::warn!(name, %reason,
                    "full_delete_instance: fenced commit preserved — remaining cleanup skipped");
                return Err(format!(
                    "workspace ownership preserved at commit (fail-closed) — remaining cleanup skipped: {reason}"
                ));
            }
            CleanupOutcome::Failed { reason } => {
                tracing::error!(name, %reason,
                    "full_delete_instance: destructive commit failed — remaining cleanup skipped");
                return Err(format!(
                    "workspace cleanup failed — remaining cleanup skipped: {reason}"
                ));
            }
        }
    }
    // (FullDeletePlan::VacuousGhost falls through: no entry, nothing on disk —
    // the remaining name-keyed cleanup below is orphan-remnant reaping.)

    #[cfg(test)]
    full_delete_test_seam::fire_post_commit();

    if let Some(tid) = topic_id {
        // #2550 identity-confusion root cause: `delete_topic` only unregisters
        // `topics.json` on its own `Deleted` outcome. A `PermissionDenied` /
        // `ApiError` / `ChannelUnavailable` result left the mapping in place —
        // our own bookkeeping said this instance still owned `tid` even though
        // we'd already decided to tear it down. A LATER instance created under
        // the same name then silently inherited that stale mapping via
        // `create_topic_for_instance`'s reuse-if-found check, with no
        // verification the topic still meant what the mapping claimed.
        // Unregister unconditionally: our registry reflects our own intent
        // (this instance is gone) regardless of whether the Telegram-side API
        // call could complete. A Telegram-side topic left dangling after a
        // failed delete becomes a genuine orphan (no daemon-side mapping to
        // anything) for the existing orphan-sweep paths (`doctor_topics`,
        // `bootstrap::init_from_config`) to reap later — not a landmine for
        // the next same-name instance.
        match telegram::delete_topic(home, tid) {
            telegram::DeleteTopicOutcome::Deleted => {}
            other => {
                telegram::unregister_topic(home, tid);
                let detail = format!("telegram topic {tid} cleanup: {other:?}");
                tracing::warn!(%name, topic_id = tid, %detail, "full_delete_instance: topic cleanup incomplete — registry unregistered anyway");
                step_errors.push(detail);
            }
        }
    } else {
        tracing::warn!(%name, "no topic_id found for full_delete_instance — possible orphan");
    }
    // Metadata tail (never touches the workspace tree): the name-keyed
    // metadata file + the non-hidden agy workspace link. Runs only past a
    // Clean destructive commit (or the vacuous-ghost flow) — see the R6
    // authority gate + fenced commit above.
    let _ = std::fs::remove_file(crate::agent_ops::metadata_path(home, name));
    crate::agy_workspace::remove_link(home, name);
    // #1157: clean id-based metadata. The fleet entry may already be removed
    // by the CAS above, so a name→id resolver can't be used here.
    // #1682: construct the id path via agent_ops — feed the captured id
    // directly.
    // #1907: also drop the `<…>.lock` flock sidecar that `with_json_state` /
    // `acquire_file_lock` leaves next to the data file. The data removal above
    // (and the inbox removal below) left the advisory-lock files behind, and the
    // whole-home audit correctly flags a `<uuid>.lock` / `<uuid>.jsonl.lock` as a
    // name/uuid-bearing residual. The lock is stateless (re-acquired on next use),
    // but a complete teardown leaves nothing name/uuid-keyed on disk.
    let _ =
        std::fs::remove_file(crate::agent_ops::metadata_path(home, name).with_extension("lock"));
    if let Some(ref id) = instance_id {
        if let Some(id) = crate::types::InstanceId::parse(id) {
            let mp = crate::agent_ops::metadata_path_for_id(home, &id);
            let _ = std::fs::remove_file(&mp);
            let _ = std::fs::remove_file(mp.with_extension("lock"));
        }
    }
    // #1902: delete the inbox file — this step was MISSING entirely, so a deleted
    // instance's inbox leaked. Cover BOTH paths: the name-based `inbox/{name}.jsonl`
    // AND the UUID-based `inbox/{uuid}.jsonl` (the current default). fleet.yaml is
    // already removed above, so `inbox_path_resolved` can't resolve the UUID — feed
    // the captured id directly (same #1682 name↔UUID-resolution trap the metadata
    // cleanup above sidesteps). The residual audit only saw the name path, so the
    // UUID inbox was leaking even past the audit.
    let inbox_name = crate::inbox::storage::inbox_path(home, name);
    let _ = std::fs::remove_file(&inbox_name);
    let _ = std::fs::remove_file(inbox_name.with_extension("jsonl.lock")); // #1907 flock sidecar
    if let Some(ref id) = instance_id {
        if let Some(id) = crate::types::InstanceId::parse(id) {
            let inbox_id = crate::inbox::storage::inbox_path_for_id(home, &id);
            let _ = std::fs::remove_file(&inbox_id);
            let _ = std::fs::remove_file(inbox_id.with_extension("jsonl.lock"));
        }
    }
    // #2764 R11: purge the child-lifecycle ledger record for the EXACT deleted
    // generation (`runtime/child-pids/<uuid>.json`). Sound ONLY here — past the
    // proven stop disposition AND the Clean destructive commit above — where
    // the whole generation is erased: `Absent` stays a durable negative
    // (Running-before-exposure invariant) and `Exited` remains the terminal
    // proof for every NON-full-delete stop. An ambiguous/failed delete returns
    // before this tail, retaining the record as the retry's evidence. Keyed by
    // the captured generation id, so a newer same-name generation's record
    // (different uuid) is never touched.
    if let Some(ref id) = instance_id {
        if let Some(id) = crate::types::InstanceId::parse(id) {
            crate::agent::child_ledger::purge(home, &id);
        }
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

    // #1018 (C): clear pending dispatch sidecars targeting the deleted
    // instance. The agent can never deliver `kind=report` so every
    // sidecar would otherwise fire `dispatch_idle_threshold_exceeded`
    // noise indefinitely. Best-effort: count is logged inside the
    // helper; failures are silently swallowed (matches the rest of
    // this function's cleanup contract).
    let _ = crate::daemon::dispatch_idle::cleanup_pending_for_instance(home, name);

    // #1022: remove activity sidecar so fleet_idle_watchdog stops
    // tracking the deleted instance. Without this, ghost agents
    // accumulate in the tracking list and inflate alert text.
    crate::daemon::idle_watchdog::remove_agent_activity(home, name);

    // CR-2026-06-14: drop the in-memory hook-shadow store entry — same
    // per-agent-residual / stale-state-on-redeploy class as the sidecars above,
    // but this global `HashMap<name, HookShadow>` had no eviction path (it only
    // ever inserted via `record_event`), leaking one entry per ever-seen agent.
    crate::daemon::hook_shadow::forget(name);
    // #2413 Shadow Observer (local plane): drop this agent's session token(s) so the
    // registry doesn't grow across churn and a recycled name can't inherit a dead
    // session's token binding. Same name↔uuid residual class as the hook-shadow store.
    crate::daemon::shadow::forget_agent(name);

    // #1744-H2: drop the persisted escalation state so a later agent reusing
    // this name does not rehydrate the deleted instance's crash budget /
    // cooldowns / hung anchor (the #1680 stale-state lesson).
    crate::daemon::escalation_persist::remove(home, name);

    // #1906 (Leak 2): drop the usage-limit notify-dedup entry — the SAME
    // stale-state-on-redeploy class as escalation_persist above, but this store
    // was missed. Without it, a same-name redeploy inherits the deleted
    // instance's suppression record and silently eats its first real usage_limit
    // notify (until the #1894/#1895 stale-unlock window).
    crate::daemon::supervisor::remove_usage_limit_notify(home, name);

    // #1488: cascade-clean the services bound to the deleted instance.
    // Without these, an orphaned schedule keeps firing into the cron self-IPC
    // fallback (this morning's deadlock trigger), dispatch_tracking entries
    // re-warn forever (the ~81 stuck-check messages), and CI watches route
    // [ci-ready-for-action] to a ghost. All best-effort like the steps above.
    // - schedules: DISABLED + marked orphaned (never deleted — operator may
    //   re-target a still-useful cron at a surviving instance).
    let _ = crate::schedules::orphan_schedules_for_target(home, name);
    // - dispatch_tracking: GC'd (no re-target value; removal stops stuck noise).
    let _ = crate::dispatch_tracking::cleanup_for_instance(home, name);
    // - ci_watch: drop from subscribers + clear next_after_ci (+ remove empty).
    let _ = crate::daemon::ci_watch::cleanup_watches_for_instance(home, name);
    // #1907: scrub the deleted instance from pr_state subscriber lists — same
    // per-instance-residual class as ci_watch above, but this store had no
    // teardown cleanup before (PR events would route at a vacant/redeployed slot).
    let _ = crate::daemon::pr_state::cleanup_subscribers_for_instance(home, name);
    // t-…-17: REVOKE the deleted instance's active reviewer assignments. Once the
    // durable authority store is LIVE, a ghost reviewer's record would keep the
    // per-tick reconciler deriving a `reserved_assignments` entry for it, holding
    // `is_merge_ready` CLOSED forever — the same per-instance-residual class as the
    // ci_watch / pr_state subscriber cleanups above. Best-effort; supersedes the
    // reviewer's outbox row (+ a revocation notice if already read).
    let _ = crate::daemon::assignment_authority::revoke_all_for_target(
        home,
        name,
        &chrono::Utc::now().to_rfc3339(),
    );
    // #1519: GC the per-instance opencode data dir ($AGEND_HOME/backend-data/
    // opencode/<name> — the per-instance XDG_DATA_HOME holding its isolated
    // session DB + copied auth). No-op for non-opencode instances (the dir was
    // never created). Best-effort like the steps above.
    let _ = std::fs::remove_dir_all(crate::agent::opencode_data_dir(home, name));

    // #1906 (Leak 1): release the PHYSICAL worktree (`worktrees/<name>/<branch>/`),
    // not just the binding. The agent is already dead here (the `api::call(DELETE)`
    // at the top runs `delete_transaction` → `wait_for_child_exit`), so an eager
    // release is safe — #1885 GC only existed as a backstop for crash/missed
    // releases, and full_delete forgetting this WAS the leak. Reuses the vetted
    // clean-release path (marker-checked `remove_worktree`, branch-aware: deletes
    // MERGED branches, PRESERVES UNMERGED — `false` is `dry_run`, NOT a
    // force-delete, so an operator's unmerged WIP is never nuked at teardown).
    //
    // ORDER IS LOAD-BEARING: this MUST run BEFORE the `binding::unbind` below —
    // `release_full` reads `binding::read` to locate the worktree path, so once
    // the binding is cleared it can't find the worktree (no-op). `release_full`
    // also clears the binding itself on success, so the unbind below is a
    // defensive idempotent no-op for the bound-agent case (still needed for the
    // never-bound / partial-state cases).
    let _ = crate::worktree_pool::release_full(home, name, false);
    // release_full removes the leaf worktree `worktrees/<name>/<branch>/`; drop the
    // now-empty agent dir `worktrees/<name>/` too so the audit below reads clean.
    // #1907: a slash-containing branch (e.g. `feat/x`) nests intermediate dirs
    // (`worktrees/<name>/feat/x/`), so a single `remove_dir` of `<name>/` failed
    // (still held the empty `feat/`) and leaked the agent dir. `remove_empty_dir_tree`
    // removes empty dirs bottom-up and STOPS at any non-empty dir — so a refused
    // unmanaged worktree (real files) stays put and is correctly surfaced by the
    // residual audit, exactly like the old `remove_dir` intent.
    remove_empty_dir_tree(&crate::worktree_pool::daemon_managed_worktree_root(home).join(name));

    // #1879 (BIND-1): clear the worktree binding (`runtime/<name>/binding.json`
    // + its HMAC sidecar + the bind-in-flight flag). Every OTHER store above is
    // cleaned here, but binding was missed — so deleting a bound agent (one that
    // ran bind_self / repo checkout) without a prior release both leaked the
    // binding (blocking a same-name re-bind) AND tripped the residual audit below
    // → the whole teardown returned Err despite otherwise succeeding.
    crate::binding::unbind(home, name);
    crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(home, name);
    // #1907: `unbind` drops binding.json + its HMAC sidecar but leaves the now-empty
    // `runtime/<name>/` dir + its `.binding.json.lock` flock behind. Remove the whole
    // dir so teardown is fully clean (the residual audit below now checks it). Safe:
    // the agent is dead (api::call(DELETE) waited for exit), so no concurrent bind
    // re-creates it; `runtime/<name>/` holds only this agent's binding artefacts.
    let _ = std::fs::remove_dir_all(crate::paths::runtime_dir(home).join(name));
    // #1935: explicitly remove the per-agent TUI port file `run/<pid>/<name>.port`.
    // The `api::call(DELETE)` stop-path already calls `remove_port` for a live
    // agent, but a boot-spawn that published the port without the agent reaching a
    // stoppable state would leave it behind. Remove it unconditionally before the
    // audit; the `_delete_guard` held above makes any concurrent publish skip its
    // `write_port` (tui_bridge.rs), so it stays gone past this point.
    crate::ipc::remove_port(&crate::daemon::run_dir(home), name);

    // Sprint 54 P1-B Bug 1 audit: enumerate every store that still holds
    // the name. If any do, surface a loud error instead of returning
    // success — `auto_start_fleet` revival of a half-deleted instance is
    // exactly the silent-drop class pattern this fix prevents.
    let residual = name_residual_anywhere(home, name, instance_id.as_deref());
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
///
/// #1682 (defect-1): `id` is the instance's captured `InstanceId` (taken BEFORE
/// fleet.yaml removal). It is required because the metadata residual lives at the
/// id-resolved `<uuid>.json`, and once fleet.yaml is gone the name→id resolver
/// can no longer find it — so the audit must check the id path DIRECTLY (no fleet
/// reload) or it goes blind to exactly the stale file a delete is meant to clear.
/// `None` when the instance had no id mapping (then only the legacy name path
/// can carry metadata).
pub(crate) fn name_residual_anywhere(
    home: &Path,
    name: &str,
    id: Option<&str>,
) -> Vec<&'static str> {
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
    // #1682 (defect-1): name path OR the id-direct path. `metadata_exists` resolves
    // via fleet.yaml, which is already gone at delete-audit time — the explicit
    // id check below is what keeps the `<uuid>.json` residual visible.
    let metadata_residual = crate::agent_ops::metadata_exists(home, name)
        || id
            .and_then(crate::types::InstanceId::parse)
            .is_some_and(|id| crate::agent_ops::metadata_path_for_id(home, &id).exists());
    if metadata_residual {
        sources.push("metadata");
    }
    // #1902: name path OR the id-direct path. fleet.yaml is gone at delete-audit
    // time, so the explicit captured-id check is what keeps a `<uuid>.jsonl`
    // inbox residual visible — the audit previously checked ONLY the name path
    // and silently missed UUID inboxes (the current default).
    let inbox_residual = crate::inbox::storage::inbox_path(home, name).exists()
        || id
            .and_then(crate::types::InstanceId::parse)
            .is_some_and(|id| crate::inbox::storage::inbox_path_for_id(home, &id).exists());
    if inbox_residual {
        sources.push("inbox");
    }
    // #1906 (Leak 2): usage-limit notify-dedup entry (was a teardown blind spot).
    if crate::daemon::supervisor::usage_limit_notify_has(home, name) {
        sources.push("usage_limit_notify");
    }
    if crate::paths::binding_path(home, name).exists() {
        sources.push("runtime/binding.json");
    }
    if home
        .join("notification-queue")
        .join(format!("{name}.jsonl"))
        .exists()
    {
        sources.push("notification-queue");
    }
    // #1906 (Leak 1): the PHYSICAL worktree dir (`worktrees/<name>/<branch>/`).
    // The audit previously only saw `runtime/<name>/binding.json` (above), so a
    // teardown that cleared the binding but left the worktree read as "clean" —
    // the same-name+branch redeploy then collided with the orphan worktree.
    if crate::worktree_pool::daemon_managed_worktree_root(home)
        .join(name)
        .exists()
    {
        sources.push("worktree");
    }
    // #1907 teardown-completeness: the remaining per-instance stores that
    // `full_delete_instance` cleans but the audit previously did not check. Each
    // detector mirrors its cleanup's predicate exactly (see the `has_*` helpers).
    // Allowlisted/intentional-retention stores (schedules, tasks, deployments,
    // telegram topics.json) are DELIBERATELY absent — they retain state by design,
    // so checking them here would make every delete return a false `Err`.
    if home
        .join("agent-activity")
        .join(format!("{name}.json"))
        .exists()
    {
        sources.push("agent-activity");
    }
    if crate::daemon::escalation_persist::load_for(home, name).is_some() {
        sources.push("escalation");
    }
    if crate::dispatch_tracking::has_for_instance(home, name) {
        sources.push("dispatch_tracking");
    }
    if crate::daemon::dispatch_idle::has_pending_for_instance(home, name) {
        sources.push("pending-dispatch");
    }
    if crate::daemon::ci_watch::has_instance_anywhere(home, name) {
        sources.push("ci-watch");
    }
    if crate::agent::opencode_data_dir(home, name).exists() {
        sources.push("opencode-data");
    }
    // #1907: the `runtime/<name>/` dir itself (empty dir + `.binding.json.lock`
    // left behind by `unbind`; full_delete now removes the whole dir).
    if crate::paths::runtime_dir(home).join(name).exists() {
        sources.push("runtime-dir");
    }
    // #1935: the per-agent TUI port file `run/<pid>/<name>.port` (daemon-LIFECYCLE
    // dir — a different family from `runtime/<name>/`, which is why #1907's oracle
    // missed it). full_delete → remove_port deletes it, but a publish racing
    // teardown could re-create it; scanning here makes the production delete LOUD
    // if it lingers, and the #1935 publish deleting-guard closes the write window.
    if crate::ipc::port_path(&crate::daemon::run_dir(home), name).exists() {
        sources.push("tui-port");
    }
    if crate::daemon::pr_state::has_subscriber(home, name) {
        sources.push("pr-state");
    }
    // #2764 R11: the child-lifecycle ledger record (`runtime/child-pids/
    // <uuid>.json`) — mirrors the exact-generation `child_ledger::purge` in the
    // teardown tail. Uuid-keyed, so like the metadata/inbox checks above it
    // needs the captured id (fleet.yaml is already gone at audit time).
    if id
        .and_then(crate::types::InstanceId::parse)
        .is_some_and(|id| crate::agent::child_ledger::record_exists(home, &id))
    {
        sources.push("child-ledger");
    }
    // #1907: the daemon-created default workspace dir (`workspace/<name>`). Only a
    // residual when the entry had no explicit `working_directory` AND the cleanup
    // above failed — a user-provided working dir resolves elsewhere and never
    // materialises here.
    if crate::paths::workspace_dir(home).join(name).exists() {
        sources.push("workspace");
    }
    sources
}

#[cfg(test)]
#[path = "lifecycle/tests.rs"]
mod tests;
