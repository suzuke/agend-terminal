use serde_json::{json, Value};
use std::path::Path;

pub(crate) mod lifecycle;
pub(super) mod spawn;

/// CR-2026-06-14 (resource-leak): upper bound on a team-mode spawn count. A
/// caller-supplied `count` flows into `vec![backend; count]`, so an unbounded
/// value (e.g. a few billion) triggers an enormous allocation → OOM/abort DoS.
/// 64 is already far beyond any real team size; reject above it at the MCP
/// boundary, before the allocation and the CREATE_TEAM RPC.
const MAX_TEAM_COUNT: usize = 64;

pub(super) fn handle_create_instance(home: &Path, args: &Value, instance_name: &str) -> Value {
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
        let mut spawned = handle_create_instance(home, &single, instance_name);
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
        let default_backend = args["backend"]
            .as_str()
            .or_else(|| args["command"].as_str())
            .unwrap_or("claude");
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
        match crate::api::call(
            home,
            &json!({"method": crate::api::method::CREATE_TEAM, "params": {
                "name": team_name,
                "backends": per_member_backends,
                "description": args.get("description"),
                // #991 PR-B: team-level default (all spawned members share
                // it) — forwarded to handle_create_team, which persists +
                // gates topic creation the same way handle_spawn does for
                // single-instance create_instance.
                "topic_binding": args.get("topic_binding"),
            }}),
        ) {
            Ok(resp) if resp["ok"].as_bool() == Some(true) => {
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
                    std::thread::Builder::new()
                        .name("team_task_inject".into())
                        .spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            for inst_name in &names {
                                let _ = crate::api::call(
                                    &home,
                                    &json!({"method": crate::api::method::INJECT, "params": {"name": inst_name, "data": task_text}}),
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
            Ok(resp) => {
                json!({"error": resp["error"].as_str().unwrap_or("team creation failed")})
            }
            Err(e) => json!({"error": format!("API unavailable: {e}")}),
        }
    } else {
        spawn::spawn_single_instance(home, instance_name, args)
    }
}

pub(super) fn handle_delete_instance(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // AUDIT2-002: deleting an instance tears down its PTY, inbox and worktree and
    // orphans its tasks. Restrict an identified caller to deleting itself or a
    // member of a team it orchestrates — a peer can no longer remove another
    // agent by naming it. Anonymous (no sender: operator-direct / standalone)
    // keeps full authority (the TUI close path calls `full_delete_instance`).
    //
    // ACL improvement: also allow the instance's CREATOR (the caller that ran
    // `create_instance` for it, stamped as `created_by` in fleet.yaml — the
    // "為 ACL 建 team" pain point: a creator wanting to redo/retire its own
    // spawn shouldn't have to build a team just to gain orchestrator
    // authority). Guarded by an in-flight safety valve: if the target has an
    // active worktree binding or a claimed/in_progress task, the creator path
    // requires `force=true` + a non-empty `force_reason` (audit-logged), so a
    // creator can't casually reap an agent mid-work. Self/orchestrator deletes
    // are unaffected by the valve — this only gates the NEW creator path.
    if let Some(caller) = sender.as_ref().map(|s| s.as_str()) {
        if caller != name && !crate::teams::is_orchestrator_of(home, caller, name) {
            let is_creator = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.created_by.clone()))
                .as_deref()
                == Some(caller);
            if !is_creator {
                return serde_json::json!({
                    "error": format!(
                        "permission denied: '{caller}' cannot delete '{name}' \
                         (only the instance itself, its team orchestrator, or its creator may)"
                    ),
                    "code": "not_owner_or_orchestrator"
                });
            }
            let has_binding = crate::binding::read(home, name).is_some();
            let has_active_task = crate::tasks::list_all(home).iter().any(|t| {
                t.assignee.as_deref() == Some(name)
                    && matches!(
                        t.status,
                        crate::task_events::TaskStatus::Claimed
                            | crate::task_events::TaskStatus::InProgress
                    )
            });
            if has_binding || has_active_task {
                let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                let force_reason = args
                    .get("force_reason")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                match force_reason {
                    Some(reason) if force => {
                        tracing::warn!(
                            caller,
                            target = name,
                            reason,
                            has_binding,
                            has_active_task,
                            "creator force-deleting instance with in-flight work"
                        );
                        // Durable audit trail — a permission override deleting
                        // in-flight work needs more than a process log line.
                        // Mirrors ci/merge.rs's merge_force_bypass: fail-closed
                        // if the write itself fails (an unrecordable override
                        // must not proceed).
                        let event = serde_json::json!({
                            "kind": "creator_force_delete",
                            "agent": caller,
                            "target": name,
                            "force_reason": reason,
                            "has_binding": has_binding,
                            "has_active_task": has_active_task,
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                        });
                        let events_path = home.join("fleet_events.jsonl");
                        let audit_written = (|| -> std::io::Result<()> {
                            use std::io::Write;
                            let mut f = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(events_path)?;
                            writeln!(f, "{event}")?;
                            Ok(())
                        })();
                        if let Err(e) = audit_written {
                            return serde_json::json!({
                                "error": format!(
                                    "creator force-delete refused: audit log write failed: {e}"
                                ),
                                "code": "creator_force_delete_audit_failed"
                            });
                        }
                    }
                    _ => {
                        return serde_json::json!({
                            "error": format!(
                                "'{name}' has in-flight work (binding={has_binding}, \
                                 active_task={has_active_task}) — creator delete requires \
                                 force=true and a non-empty force_reason"
                            ),
                            "code": "creator_delete_requires_force"
                        });
                    }
                }
            }
        }
    }
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some()
            && config.instances.contains_key(name)
            && config.instances.len() <= 1
        {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    // Full multi-store teardown lives in the `lifecycle` submodule of this
    // `instance_state` concept (Sprint 54 P1-B Bug 1).
    match lifecycle::full_delete_instance(home, name) {
        Ok(()) => json!({"name": name}),
        Err(detail) => json!({
            "name": name,
            "error": format!(
                "delete completed with residual state — fleet may resurrect on next reconcile: {detail}"
            ),
        }),
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
pub(super) fn handle_bind_topic(home: &Path, args: &Value) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    if let Some(channel) = args["channel"].as_str() {
        if channel != "telegram" {
            return json!({
                "error": format!("bind_topic: channel '{channel}' not yet supported (only 'telegram')"),
                "code": "channel_not_supported"
            });
        }
    }
    use crate::channel::telegram::BindTopicOutcome;
    match crate::channel::telegram::bind_topic_for_instance(home, name) {
        BindTopicOutcome::Bound(tid) => json!({"bound": true, "topic_id": tid}),
        BindTopicOutcome::AlreadyBound(tid) => {
            json!({"bound": true, "topic_id": tid, "already_bound": true})
        }
        BindTopicOutcome::NotEligible { reason } => {
            json!({"error": reason, "code": "not_eligible"})
        }
        BindTopicOutcome::InstanceNotFound => {
            json!({"error": format!("instance '{name}' not found"), "code": "instance_not_found"})
        }
        BindTopicOutcome::ChannelUnavailable => json!({
            "error": "telegram channel not ready yet — retry in a few seconds",
            "code": "channel_unavailable"
        }),
        BindTopicOutcome::ApiError(e) => json!({"error": e, "code": "api_error"}),
    }
}

pub(super) fn handle_start_instance(home: &Path, args: &Value) -> Value {
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
    match config.resolve_instance(name) {
        Some(resolved) => {
            let cmd_args = resolved.args.join(" ");
            // #900: forward the resolved env explicitly so the daemon's
            // SPAWN handler doesn't have to re-read fleet.yaml for what
            // we already have in hand. params.env wins over the fleet
            // fallback in handle_spawn, which keeps this RPC the
            // single-source-of-truth for the instance start.
            let env_json = serde_json::to_value(&resolved.env).unwrap_or(serde_json::Value::Null);
            match crate::api::call(
                home,
                &json!({"method": crate::api::method::SPAWN, "params": {
                    "name": name, "backend": resolved.backend_command, "args": cmd_args,
                    "mode": "resume",
                    "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                    "env": env_json,
                }}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"name": name}),
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
                }
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        None => json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
    }
}

/// #1625: assemble the SPAWN params for a restart. Tags `layout: same-tab` so
/// the respawned pane returns to the tab the killed pane occupied (recorded
/// on its DELETE) instead of opening a fresh tab. `mode` only toggles backend
/// resume args — placement is identical for resume and fresh restarts — so
/// the hint is applied unconditionally.
fn restart_spawn_params(
    name: &str,
    backend_command: &str,
    args: &[String],
    working_directory: Option<&Path>,
    env: &std::collections::HashMap<String, String>,
    mode: &str,
) -> Value {
    let mut spawn_params = json!({
        "name": name,
        "backend": backend_command,
        "args": args.join(" "),
        "working_directory": working_directory.map(|p| p.display().to_string()),
        "env": serde_json::to_value(env).unwrap_or(serde_json::Value::Null),
        "layout": "same-tab",
    });
    if mode == "resume" {
        spawn_params["mode"] = json!("resume");
    } else {
        // fresh restart only: arm the daemon's first-turn self-kick so the
        // respawned (context-lost) instance runs its recovery sequence instead of
        // sitting idle until an operator happens to type (the overnight
        // restart-strands-the-fleet failure). INDEPENDENT flag — the SPAWN handler
        // must NOT derive self-kick from SpawnMode::Fresh, which initial fleet
        // spawns also map to; only THIS restart-fresh path sets it.
        spawn_params["self_kick_on_ready"] = json!(true);
    }
    spawn_params
}

pub(super) fn handle_restart_instance(home: &Path, args: &Value) -> Value {
    let name = match super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // #1744-PR-B (latch-scope): operator-initiated recovery resets the terminal
    // self-orch once-off latch, so a fresh terminal death after this restart re-pages.
    crate::daemon::escalation_persist::clear_failed_escalated(home, name);
    let reason = args["reason"].as_str().unwrap_or("manual restart");
    let mode = args["mode"].as_str().unwrap_or("resume");

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
            if wt.exists() && crate::worktree::has_uncommitted_changes(&wt) {
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
    let resolved = match config.resolve_instance(name) {
        Some(r) => r,
        None => return json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
    };

    // Session-reset inbox settle: for a FRESH restart (context-lost), settle
    // all DELIVERING rows to PROCESSED before killing the old instance.
    // Resume restarts preserve context → the implicit next-drain ack (A)
    // handles it; settle would prematurely close messages the resumed agent
    // still has in context. (agend-customization#159)
    if mode != "resume" {
        crate::inbox::settle_delivering_for_session_reset(home, name);
    }

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name, "no_wait": true}}),
    );

    let spawn_params = restart_spawn_params(
        name,
        &resolved.backend_command,
        &resolved.args,
        resolved.working_directory.as_deref(),
        &resolved.env,
        mode,
    );

    let spawn_result = crate::api::call(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": spawn_params}),
    );
    let spawned = spawn_result
        .as_ref()
        .map(|r| r["ok"].as_bool() == Some(true))
        .unwrap_or(false);

    tracing::info!(%name, %reason, %mode, %spawned, "restart_instance");
    json!({"name": name, "reason": reason, "mode": mode, "spawned": spawned})
}

/// #t-777-3: daemon-autonomic self-heal entry — the respawn-stuck watchdog's
/// narrow path to a **Fresh** restart. Wraps `handle_restart_instance(mode=fresh)`,
/// which round-trips the PROVEN direct `DELETE`(no_wait)+`SPAWN` api::calls →
/// `ApiEvent::InstanceCreated` → app pane Fresh respawn (the same path the
/// operator's manual `restart_instance fresh` takes, working in the live
/// app-mode daemon where the crash_tx→respawn machinery is inert).
///
/// **Gate-exempt BY CONSTRUCTION** (no new operator-gate surface): the inner
/// `DELETE`/`SPAWN` are DIRECT api methods — operator-transport, which
/// `operator_gate::check_operation_allowed` returns `Ok` for before `classify`
/// is consulted. Reached ONLY from the per-tick hang-detection watchdog (never
/// agent-invocable), so the narrowness is enforced by the trigger, exactly like
/// crash-respawn / hang-recovery (`operator_gate` module scope note). Returns
/// whether the SPAWN succeeded so the caller can escalate a failed recovery.
pub(crate) fn restart_instance_autonomic(home: &Path, name: &str, reason: &str) -> bool {
    let result = handle_restart_instance(
        home,
        &json!({"name": name, "mode": "fresh", "reason": reason}),
    );
    result
        .get("spawned")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn resolve_team_layout(
    home: &Path,
    name: &str,
    layout_arg: Option<&serde_json::Value>,
    target_pane_arg: Option<&serde_json::Value>,
) -> (&'static str, Option<String>) {
    let caller_set_layout = layout_arg.and_then(|v| v.as_str()).is_some();
    let caller_set_target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .is_some();
    if !caller_set_layout && !caller_set_target {
        if let Some(team) = crate::teams::find_team_for(home, name) {
            let anchor = team.orchestrator.or_else(|| team.members.first().cloned());
            return ("split-right", anchor);
        }
    }
    let layout = layout_arg.and_then(|v| v.as_str()).unwrap_or("tab");
    let target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let layout = match layout {
        "split-right" => "split-right",
        "split-below" => "split-below",
        _ => "tab",
    };
    (layout, target)
}

#[cfg(test)]
mod tests;
