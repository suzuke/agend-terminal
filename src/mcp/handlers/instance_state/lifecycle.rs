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

use crate::agent_ops::cleanup_working_dir;
use crate::channel::telegram;
use serde_json::json;
use std::path::Path;

/// #1907: remove `dir` and any empty subdirectories bottom-up, stopping at any
/// non-empty dir. Used to drop a deleted agent's worktree root including the
/// intermediate dirs a slash-containing branch nests (`worktrees/<name>/feat/x/`),
/// while preserving a refused unmanaged worktree (its real files keep its dir
/// non-empty, so it's left for the residual audit to surface).
fn remove_empty_dir_tree(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                remove_empty_dir_tree(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir); // succeeds only if now-empty
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
    // #1915: mark this instance "deleting" for the ENTIRE teardown — the guard is
    // held until this fn returns (after workspace cleanup + the residual audit
    // below), so any concurrent spawn (boot-stagger / crash-respawn worker /
    // stage2) is refused at the `spawn_agent` / `spawn_and_register_agent`
    // chokepoints. `DeletingGuard::drop` un-marks on EVERY path (normal return,
    // early `Err`, panic), so the name is always re-creatable afterwards — a
    // leaked mark would make it un-spawnable for the daemon's lifetime.
    let _delete_guard = crate::agent::deleting::mark_deleting(home, name);
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let (topic_id, working_dir) = fleet
        .as_ref()
        .and_then(|c| {
            c.resolve_instance(name)
                .map(|r| (r.topic_id, r.working_directory))
        })
        .unwrap_or((None, None));
    // #1157: extract InstanceId before fleet.yaml removal for id-based metadata cleanup.
    let instance_id = fleet
        .as_ref()
        .and_then(|c| c.instances.get(name))
        .and_then(|i| i.id.clone());

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
    // #1907: default to `workspace/<name>` when the fleet entry has no explicit
    // `working_directory`. Otherwise the daemon-created default workspace dir
    // (per-backend skills/config under `$AGEND_HOME/workspace/<name>`) LEAKS on
    // delete — `cleanup_working_dir` only ran when the resolver returned a path,
    // but it returns `None` for the (common) entries that never set
    // `working_directory:` explicitly. `cleanup_working_dir` only `remove_dir_all`s
    // paths UNDER `$AGEND_HOME/workspace/`, so a user-provided dir is never nuked —
    // the default fed here is always under workspace/, the path it's safe to remove.
    let wd = working_dir
        .clone()
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(name));
    cleanup_working_dir(home, name, &wd);
    // #1157: clean id-based metadata. Fleet.yaml is already removed above,
    // so cleanup_working_dir's best-effort lookup may miss the id path.
    // #1682: construct the id path via agent_ops (fleet.yaml is gone, so the
    // name→id resolver can't be used here — feed the captured id directly).
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
