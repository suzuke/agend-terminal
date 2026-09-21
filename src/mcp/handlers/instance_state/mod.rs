pub(crate) mod set_model;
#[cfg(test)]
mod set_model_tests;

use serde_json::{json, Value};
use std::path::Path;

mod delete;
mod instance_layout;
pub(crate) mod lifecycle;
mod restart_prep;
mod topic;
#[cfg(test)]
pub(super) use delete::handle_delete_instance;
pub(super) use delete::handle_delete_instance_with_runtime;
pub(super) use instance_layout::resolve_team_layout;
pub(crate) use restart_prep::restart_instance_autonomic;
use restart_prep::{await_unsent_draft_or_grace, restart_spawn_params};
#[cfg(test)]
use restart_prep::{restart_draft_gate, DraftGate, RESTART_DRAFT_GRACE};
pub(super) use topic::handle_bind_topic;
#[cfg(not(test))]
pub(super) mod spawn;
#[cfg(test)]
pub(crate) mod spawn;

/// CR-2026-06-14 (resource-leak): upper bound on a team-mode spawn count. A
/// caller-supplied `count` flows into `vec![backend; count]`, so an unbounded
/// value (e.g. a few billion) triggers an enormous allocation → OOM/abort DoS.
/// 64 is already far beyond any real team size; reject above it at the MCP
/// boundary, before the allocation and the CREATE_TEAM RPC.
const MAX_TEAM_COUNT: usize = 64;

fn record_creator_force_delete(
    home: &Path,
    caller: &str,
    name: &str,
    reason: &str,
    has_binding: bool,
    has_active_task: bool,
) -> Result<(), String> {
    let event = serde_json::json!({
        "kind": "creator_force_delete",
        "agent": caller,
        "target": name,
        "force_reason": reason,
        "has_binding": has_binding,
        "has_active_task": has_active_task,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    agentic_audit_append::append_audit_line_bounded(
        home,
        &event,
        agentic_audit_append::DEFAULT_BOUNDED_BUDGET,
    )
    .map_err(|e| e.to_string())
}

pub(super) fn handle_create_instance(
    home: &Path,
    args: &Value,
    instance_name: &str,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    // #2037 (6): name + team = spawn THIS name, then join the team — team-mode
    // used to silently rename to `<team>-N` (the fixup-1 incident). With
    // count>1/backends the names are generated, so an explicit name errors.
    if let (Some(team_name), Some(explicit)) = (
        args.get("team").and_then(|v| v.as_str()),
        args.get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()),
    ) {
        // H7 (high/security): validate the team name at the MCP boundary. It
        // becomes member names (`<team>-N`) + `workspace_dir(home).join(name)`
        // downstream; `PathBuf::join` keeps `..`, so an unvalidated traversal
        // name like "../../tmp/evil" escapes the workspace root. Reject here,
        // exactly as the single-instance path does.
        crate::validate_name_or_err!(team_name);
        if args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) > 1
            || args.get("backends").is_some()
        {
            return json!({"error": "explicit 'name' with count>1/backends is ambiguous — drop 'name' (generated <team>-N names) or spawn one instance at a time"});
        }
        // Normal single path keeps the explicit name + all single-spawn behavior.
        let mut single = args.clone();
        if let Some(obj) = single.as_object_mut() {
            obj.remove("team");
            obj.remove("count");
        }
        let mut spawned = handle_create_instance(home, &single, instance_name, runtime);
        if spawned.get("error").is_some() {
            return spawned;
        }
        let team_resp = crate::teams::update(home, &json!({"name": team_name, "add": [explicit]}));
        if team_resp.get("error").is_some() {
            // Instance EXISTS — surface the partial state honestly.
            return json!({"name": explicit, "spawned": true, "team": team_name,
                "team_join_error": team_resp["error"].clone()});
        }
        spawned["team"] = json!(team_name);
        spawned["joined_team"] = json!(true);
        return spawned;
    }
    // Team mode: spawn count instances and group them
    if let Some(team_name) = args.get("team").and_then(|v| v.as_str()) {
        // H7 (high/security): validate the team name BEFORE the CREATE_TEAM RPC.
        // `create_team` derives member names `<team>-N` and `workspace_dir(home)
        // .join(name)`; `PathBuf::join` preserves `..`, so an unvalidated name
        // like "../../tmp/evil" creates + registers fleet entries outside the
        // workspace root. The single-instance path already validates; this
        // forwarded the raw name straight to the daemon.
        crate::validate_name_or_err!(team_name);
        let default_backend = args["backend"].as_str().unwrap_or("claude");
        let per_member_backends: Vec<String> = match args.get("backends").and_then(|v| v.as_array())
        {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => {
                let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                // CR-2026-06-14 (resource-leak): cap BEFORE the `vec!` allocation
                // — a huge `count` would OOM the daemon at the allocation itself.
                if count > MAX_TEAM_COUNT {
                    return json!({"error": format!(
                        "team count {count} exceeds the maximum {MAX_TEAM_COUNT}"
                    )});
                }
                vec![default_backend.to_string(); count]
            }
        };
        if per_member_backends.is_empty() {
            return json!({"error": "count must be >= 1 (or backends must be non-empty)"});
        }
        // CR-2026-06-14 (resource-leak): also bound the explicit-`backends` path
        // (already materialized by serde, so no OOM here, but enforce the same
        // team-size limit consistently at the boundary).
        if per_member_backends.len() > MAX_TEAM_COUNT {
            return json!({"error": format!(
                "team size {} exceeds the maximum {MAX_TEAM_COUNT}",
                per_member_backends.len()
            )});
        }
        let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
        let resp = if let Some(rt) = runtime {
            crate::team_ops::create(
                home,
                crate::team_ops::CreateTeamRequest {
                    name: team_name.to_string(),
                    per_member_backends: per_member_backends.clone(),
                    existing_members: Vec::new(),
                    topic_binding_mode: args["topic_binding"]
                        .as_str()
                        .filter(|s| matches!(*s, "skip" | "deferred"))
                        .map(String::from),
                    orchestrator: None,
                    description: args
                        .get("description")
                        .and_then(Value::as_str)
                        .map(String::from),
                    repository_path: None,
                    project_id: None,
                    accept_from: Vec::new(),
                },
                &rt.registry,
                &rt.configs,
                rt.notifier.as_deref(),
            )
        } else {
            // Standalone bridge calls retain the legacy API transport. Reuse
            // the existing SPAWN compatibility leaf so this loopback remains
            // isolated to RuntimeContext=None without adding another socket
            // call site.
            match spawn::legacy_spawn(
                home,
                &json!({"method": crate::api::method::CREATE_TEAM, "params": {
                    "name": team_name,
                    "backends": per_member_backends.clone(),
                    "description": args.get("description"),
                    // #991 PR-B: team-level default (all spawned members share
                    // it) — forwarded to handle_create_team, which persists +
                    // gates topic creation the same way handle_spawn does for
                    // single-instance create_instance.
                    "topic_binding": args.get("topic_binding"),
                }}),
            ) {
                Ok(resp) => resp,
                Err(e) => return json!({"error": format!("API unavailable: {e}")}),
            }
        };
        match resp {
            resp if resp["ok"].as_bool() == Some(true) => {
                let spawned: Vec<String> = resp["spawned"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                if let Some(task_text) = task {
                    let home = home.to_path_buf();
                    let names = spawned.clone();
                    // fire-and-forget: team task injection waits 3s for agents to
                    // initialize, then injects task text. No JoinHandle needed —
                    // losing the injection on shutdown is acceptable (M5 §10.5).
                    let rt_arcs = runtime.map(|rt| {
                        (
                            std::sync::Arc::clone(&rt.registry),
                            std::sync::Arc::clone(&rt.externals),
                        )
                    });
                    std::thread::Builder::new()
                        .name("team_task_inject".into())
                        .spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            for inst_name in &names {
                                let _ = spawn::inject_with_routing(
                                    &home,
                                    inst_name,
                                    task_text.as_bytes(),
                                    rt_arcs.as_ref(),
                                );
                            }
                        })
                        .ok();
                }
                let mut result = json!({
                    "team": team_name,
                    "spawned": spawned,
                    "backends": per_member_backends,
                });
                if let Some(failed) = resp.get("failed") {
                    result["failed"] = failed.clone();
                }
                result
            }
            resp => {
                json!({"error": resp["error"].as_str().unwrap_or("team creation failed")})
            }
        }
    } else {
        spawn::spawn_single_instance(home, instance_name, args, runtime)
    }
}

/// #991 Phase 2: retrofit a Telegram topic for a `deferred`/`auto`-without-
/// topic instance. See `bind_topic_for_instance` for the core logic and
/// `BindTopicOutcome`'s variants for the exact result shapes below.
///
/// `channel` is optional and defaults to `"telegram"` — the only channel
/// this action supports today. An explicit non-telegram value gets a clear
/// "not yet supported" error rather than silently misrouting or falling back
/// to the ambiguous `active_channel()` (ARCH note: BIND-TOPIC-PRERESEARCH.md
/// §4 — that resolver returns `None` whenever 0 OR MULTIPLE channels are
/// registered, a pre-existing, separately-tracked bug this action avoids by
/// never calling it).
pub(super) fn handle_start_instance_with_runtime(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this start re-pages.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        return json!({"error": "No fleet.yaml"});
    }
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
    };
    match config.resolve_instance_checked(name) {
        Ok(Some(resolved)) => {
            let cmd_args = resolved.args.join(" ");
            // #900: forward the resolved env explicitly so the daemon's
            // SPAWN handler doesn't have to re-read fleet.yaml for what
            // we already have in hand. params.env wins over the fleet
            // fallback in handle_spawn, which keeps this RPC the
            // single-source-of-truth for the instance start.
            let env_json = serde_json::to_value(&resolved.env).unwrap_or(serde_json::Value::Null);
            let spawn_request = json!({"method": crate::api::method::SPAWN, "params": {
                "name": name, "backend": resolved.backend_command, "args": cmd_args,
                "mode": "resume",
                "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                "env": env_json,
            }});
            match spawn::spawn_runtime_or_legacy(
                home,
                &spawn_request,
                runtime,
                &spawn::legacy_spawn,
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"name": name}),
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
                }
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        Ok(None) => json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
        Err(error) => json!({
            "error": error.to_string(),
            "code": "env_source_missing",
            "instance": error.instance,
            "destination": error.destination,
            "source": error.source,
        }),
    }
}

pub(super) fn handle_restart_instance(home: &Path, args: &Value) -> Value {
    handle_restart_instance_with_runtime(home, args, None)
}

pub(super) fn handle_restart_instance_with_runtime(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    let reason = args["reason"].as_str().unwrap_or("manual restart");
    let mode = args["mode"].as_str().unwrap_or("resume");
    let restart_id = args["restart_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::types::InstanceId::new().full());
    let requested_old_instance_ref = args
        .get("old_instance_ref")
        .and_then(|value| serde_json::from_value(value.clone()).ok());

    // #2476: a `fresh` restart DROPS the agent's in-memory context (that is its
    // value — it releases a stale prompt cache while a dev idles waiting on
    // review/CI). But fresh-restart-as-routine must not silently discard
    // UNCOMMITTED groundwork in the agent's bound worktree. Pre-flight: if the
    // bound worktree has uncommitted changes, refuse unless `force:true`, telling
    // the caller to push / leave a board handoff first. `resume` is unaffected
    // (it keeps context), and an unbound agent has no worktree to protect.
    if mode != "resume" && !args["force"].as_bool().unwrap_or(false) {
        if let Some(wt) = crate::binding::read(home, name)
            .and_then(|b| b["worktree"].as_str().map(std::path::PathBuf::from))
        {
            if wt.exists() && crate::worktree_pool::worktree_has_work_at_risk(&wt) {
                return json!({
                    "error": "refusing fresh restart: bound worktree has uncommitted changes \
                              that a context drop would strand. Commit/push (or leave a task-board \
                              handoff) first, then retry — or pass force:true to drop context anyway.",
                    "name": name,
                    "worktree": wt.display().to_string(),
                    "code": "uncommitted_work_at_risk",
                });
            }
        }
    }

    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
    };
    let resolved = match config.resolve_instance_checked(name) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
        Err(error) => {
            return json!({
                "error": error.to_string(),
                "code": "env_source_missing",
                "instance": error.instance,
                "destination": error.destination,
                "source": error.source,
            })
        }
    };

    // #3414 PREFLIGHT — must stay HERE: after `resolve_instance` (it needs the
    // DECLARED backend + stored args) and before EVERY destructive step below
    // (draft wait, inbox requeue, DELETE, SPAWN, self-kick arming). Fixing this
    // at `restart_spawn_params` is too late: by then the instance is gone.
    //
    // `mode=fresh` is a statement about the SESSION, not merely about the
    // preset. `SpawnMode` only selects `Backend::preset_spawn_args`, so a
    // session pin living in the CALLER args (the observed `--resume <uuid>`)
    // was never inspected and survived every fresh restart.
    //
    // Grammar comes from `resolved.backend` — the DECLARED identity — never
    // from `backend_command`, whose basename misclassifies wrappers (#2744).
    // Unresolvable grammar fails closed: guessing would either strand the
    // operator on the old session or silently drop a real value, and the live
    // instance is still running at this point.
    let sanitized_args = if mode == "resume" {
        resolved.args.clone()
    } else {
        match crate::backend_session::sanitize_for_fresh(&resolved.backend, &resolved.args) {
            Ok(args) => args,
            Err(error) => {
                return json!({
                    "error": format!(
                        "refusing fresh restart: {error}. Fix the instance's stored args \
                         (or restart with mode=resume) — the running instance was left untouched."
                    ),
                    "name": name,
                    "code": "fresh_session_args_invalid",
                    "backend": error.backend,
                    "token": error.token,
                    "reason": error.reason.as_str(),
                });
            }
        }
    };

    // #3538: resume-availability gate — must stay HERE: after `resolve_instance`
    // (it needs the DECLARED backend) and before EVERY destructive or mutating
    // step below. Body lives in `restart_prep` (same 750-LOC split as #3414).
    let codex_thread =
        match restart_prep::resume_availability_gate(home, name, reason, mode, &resolved.backend) {
            restart_prep::ResumeGate::Proceed { codex_thread } => codex_thread,
            restart_prep::ResumeGate::Refused { response } => return response,
        };

    // Correlation is authoritative only when the daemon can snapshot the live
    // predecessor. A caller-supplied ref is accepted only for legacy/no-runtime
    // paths; an in-process runtime with no registry identity must fail closed.
    let old_instance_ref = if let Some(runtime) = runtime {
        match crate::agent::instance_ref_for_name(&runtime.registry, home, name) {
            Some(instance_ref) => Some(instance_ref),
            None => {
                if requested_old_instance_ref.is_some() {
                    return json!({
                        "error": format!("runtime registry has no live identity for '{name}'"),
                        "code": "restart_identity_unavailable",
                        "name": name,
                        "restart_id": restart_id,
                    });
                }
                None
            }
        }
    } else {
        requested_old_instance_ref
    };
    let _restart_admission = match restart_prep::try_admit_restart(home, name, &restart_id) {
        Ok(admission) => admission,
        Err(error) => {
            return json!({
                "error": error,
                "code": "restart_in_progress",
                "name": name,
                "restart_id": restart_id,
            })
        }
    };

    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this restart re-pages.
    // Keep this AFTER typed session preflight: a refused fresh restart must not
    // mutate persisted escalation state or any other runtime state.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);

    // t-95913-5: the operator's unsent keystrokes live ONLY in the input line of
    // the process we're about to kill — a fresh OR resume restart destroys them
    // (`--continue` restores the conversation, not the input line). If the pane
    // has a live draft, defer the kill until the operator submits (draft clears)
    // or a grace ceiling elapses (so continuous typing can't defer forever).
    // Mode-agnostic; explicit restart callers bypass with the dedicated marker,
    // while `force:true` retains its existing worktree-protection meaning and
    // also bypasses this gate. Safe to block here: each api tool call
    // runs on its own `api_handler` thread (`api::serve` per-session spawn), and
    // the operator's submit arrives via the TUI write path, not this thread.
    let skip_unsent_draft_gate = args["skip_unsent_draft_gate"].as_bool().unwrap_or(false);
    await_unsent_draft_or_grace(
        home,
        name,
        skip_unsent_draft_gate || args["force"].as_bool().unwrap_or(false),
    );

    // Session-reset inbox handoff: for a FRESH restart (context-lost), requeue
    // all unconfirmed DELIVERING rows before killing the old instance. The
    // successor must be able to recover them; durable delivery history lets a
    // later targeted ack close exactly one requeued row. #159's old settle
    // rationale avoided stale re-injection by stamping read_at, but could
    // silently lose an unconfirmed message; #3228 intentionally chooses
    // visible redelivery and recovery. Resume restarts preserve context → the
    // implicit next-drain ack (A) handles it.
    let delete_context = runtime.map(|runtime| crate::agent_ops::DeleteContext {
        registry: &runtime.registry,
        configs: &runtime.configs,
        externals: &runtime.externals,
        notifier: runtime.notifier.as_ref(),
    });

    // #3414: the runtime DELETE goes through `delete_instance_under_guard`,
    // whose contract is that the CALLER already owns the `DeleteFence`. Restart
    // owned none, so neither the deleting mark that makes a concurrent
    // `dispatch_transport` refuse a job aimed at the dying session nor the
    // transport delivery cleanup that fence fronts ever ran. The legacy API
    // DELETE builds its own fence inside the daemon
    // (`agent_ops::delete_instance_with_exit_status`) and the transport lane is a
    // non-reentrant mutex, so nesting a second fence around that call would block
    // the serving thread on this lane while we block on its response: the fence is
    // scoped to exactly the path that lacks one. It opens after the draft gate so
    // a long grace never marks a still-live instance.
    let fence = delete_context
        .is_some()
        .then(|| crate::daemon::lifecycle::DeleteFence::new(home, name, true));

    if mode != "resume" {
        crate::inbox::requeue_delivering_for_session_reset(home, name);
    }

    // Restart intentionally uses no-wait deletion: admission of the kill signal,
    // followed by the replacement spawn, is this path's existing contract.
    let torn_down = lifecycle::delete_with_runtime_or_legacy_for_restart(
        home,
        name,
        delete_context.as_ref(),
        true,
        Some(&restart_id),
    );
    // A fresh restart destroys the session, so every transport receipt keyed
    // to it is unresolvable — the consumer that owed the acknowledgement no
    // longer exists. Left behind, the self-kick watchdog escalates it to every
    // channel as an unacknowledged delivery on an instance the operator
    // deliberately restarted. A resume restores the SAME session, so its
    // receipts stay resolvable and are preserved. Only under the fence:
    // `remove_instance_delivery_state` requires the cleanup guard so no
    // in-flight transport job can recreate the files behind it. Only after a
    // CONFIRMED teardown: `remove_instance_delivery_state` is an audited
    // policy discharge legitimate solely because no recipient survives to be
    // owed anything, so it must not run ahead of that outcome. Restart's
    // `skip_exit_wait=true` above makes the teardown call always report `Ok`
    // today (the refusal branch inside it is unreachable while that argument
    // stays `true`), but gating on the result here keeps the invariant local
    // instead of depending on that argument never changing.
    if mode != "resume" && fence.is_some() && torn_down.is_ok() {
        if let Err(error) = crate::transport::remove_instance_delivery_state(home, name) {
            tracing::warn!(
                agent = %name,
                error = %error,
                "restart: transport delivery cleanup failed"
            );
        }
    }
    // SPAWN refuses a name that is mid-delete (`agent::spawn_agent`), so the fence
    // must close before the replacement spawn.
    drop(fence);

    let spawn_params = restart_spawn_params(
        name,
        &resolved.backend_command,
        &sanitized_args,
        resolved.working_directory.as_deref(),
        &resolved.env,
        mode,
        &restart_id,
        old_instance_ref,
    );

    let spawn_request = json!({
        "method": crate::api::method::SPAWN,
        "params": spawn_params,
    });
    let spawn_result =
        spawn::spawn_runtime_or_legacy(home, &spawn_request, runtime, &spawn::legacy_spawn);
    let spawned = spawn_result
        .as_ref()
        .map(|r| r["ok"].as_bool() == Some(true))
        .unwrap_or(false);

    if !spawned {
        let error = spawn_result
            .as_ref()
            .ok()
            .and_then(|result| result["error"].as_str())
            .map(str::to_string)
            .or_else(|| spawn_result.as_ref().err().map(ToString::to_string))
            .unwrap_or_else(|| "restart spawn failed".to_string());
        if let Some(runtime) = runtime {
            if let Some(notifier) = runtime.notifier.as_ref() {
                notifier.notify(crate::api::ApiEvent::InstanceRestartFailed {
                    name: name.to_string(),
                    restart_id: restart_id.clone(),
                    old_instance_ref,
                    error,
                });
            }
        }
    }

    // Restart/TUI 交接確認見 restart_prep::settle_tui_handoff。
    let (tui_handoff, handoff_warning) = restart_prep::settle_tui_handoff(home, name, spawned);

    tracing::info!(%name, %reason, %mode, %spawned, tui_handoff, "restart_instance");
    let successor_instance_ref = runtime
        .and_then(|runtime| crate::agent::instance_ref_for_name(&runtime.registry, home, name));
    let mut resp = json!({"name": name, "reason": reason, "mode": mode, "spawned": spawned, "tui_handoff": tui_handoff, "restart_id": restart_id, "old_instance_ref": old_instance_ref, "successor_instance_ref": successor_instance_ref});
    if !spawned {
        resp["error"] = spawn_result
            .as_ref()
            .ok()
            .and_then(|result| result.get("error"))
            .cloned()
            .unwrap_or_else(|| json!("restart spawn failed"));
    }
    if let Some(warning) = handoff_warning {
        resp["tui_handoff_warning"] = json!(warning);
    }
    // #3538: exact-thread resume signal (boolean only — never leaks the id).
    if codex_thread {
        resp["resumed_thread"] = json!(true);
    }
    resp
}

#[cfg(test)]
mod tests;
