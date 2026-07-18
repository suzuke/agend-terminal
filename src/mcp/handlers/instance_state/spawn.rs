//! `spawn_single_instance` — the MCP-side caller path that adds a fleet.yaml
//! entry then issues the `SPAWN` RPC. The `spawn` submodule of the
//! `instance_state` concept, beside its sibling `lifecycle`.
//!
//! `spawn_single_instance` is `pub(super)` so `handle_create_instance` in
//! the parent `instance_state` module can call it; the test mock entry point
//! `spawn_single_instance_impl` is exported to `mcp::handlers` so
//! `mcp/handlers/instance_964_tests.rs` can inject a stub `spawn_fn` for the
//! #964 caller-path regression tests.

use crate::agent_ops::validate_branch;
use serde_json::{json, Value};
use std::path::Path;

/// #2454 S8 test seam: when Some(home), the delayed inject thread body
/// runs inline (no spawn, no sleep) for that specific home only.
#[cfg(test)]
pub(crate) static INJECT_INLINE: parking_lot::Mutex<Option<std::path::PathBuf>> =
    parking_lot::Mutex::new(None);

/// #2454 S8 test seam: when Some, `spawn_single_instance` uses this
/// instead of `crate::api::call` for the SPAWN RPC leaf only.
#[cfg(test)]
pub(crate) type SpawnRpcFn =
    fn(&std::path::Path, &serde_json::Value) -> anyhow::Result<serde_json::Value>;

#[cfg(test)]
pub(crate) static SPAWN_OVERRIDE: parking_lot::Mutex<Option<(std::path::PathBuf, SpawnRpcFn)>> =
    parking_lot::Mutex::new(None);

/// #2454 S8: synchronous inject routing — the testable core shared by the
/// fire-and-forget threads. Returns Ok(()) on success or Err(detail) on
/// failure. Extracted so routing tests exercise the real branch without
/// a 3s sleep.
pub(in crate::mcp::handlers) fn inject_with_routing(
    home: &Path,
    target: &str,
    data: &[u8],
    rt_arcs: Option<&(crate::agent::AgentRegistry, crate::agent::ExternalRegistry)>,
) -> Result<(), String> {
    if let Some((reg, ext)) = rt_arcs {
        crate::agent_ops::inject_input(reg, ext, home, target, data, false)
            .map(|_| ())
            .map_err(|e| e.to_string())
    } else {
        let resp = crate::api::call(
            home,
            &json!({"method": crate::api::method::INJECT, "params": {"name": target, "data": String::from_utf8_lossy(data)}}),
        );
        match resp {
            Ok(v) if v["ok"].as_bool() == Some(true) => Ok(()),
            Ok(v) => Err(v.to_string()),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub(super) fn spawn_single_instance(
    home: &Path,
    instance_name: &str,
    args: &Value,
    runtime: Option<&super::super::dispatch::RuntimeContext>,
) -> Value {
    #[cfg(test)]
    {
        if let Some((ref scope_home, f)) = *SPAWN_OVERRIDE.lock() {
            if home == scope_home.as_path() {
                return spawn_single_instance_impl(home, instance_name, args, &f, runtime);
            }
        }
    }
    spawn_single_instance_impl(home, instance_name, args, &crate::api::call, runtime)
}

/// Inner impl of [`spawn_single_instance`] parameterized on the SPAWN RPC for
/// `instance_964_tests`. Production passes [`crate::api::call`].
pub(in crate::mcp::handlers) fn spawn_single_instance_impl(
    home: &Path,
    instance_name: &str,
    args: &Value,
    spawn_fn: &dyn Fn(&Path, &Value) -> anyhow::Result<Value>,
    runtime: Option<&super::super::dispatch::RuntimeContext>,
) -> Value {
    let raw_name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    crate::validate_name_or_err!(raw_name);
    let name_owned = {
        // M4: AtomicU64 prevents 65536 wrap-around collision
        use std::sync::atomic::{AtomicU64, Ordering};
        static DEDUP_SEQ: AtomicU64 = AtomicU64::new(0);

        let existing: std::collections::HashSet<String> =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .map(|c| c.instance_names().into_iter().collect())
                .unwrap_or_default();
        if existing.contains(raw_name) {
            let seq = DEDUP_SEQ.fetch_add(1, Ordering::Relaxed);
            let deduped = format!("{raw_name}-{seq:04x}");
            tracing::info!(original = raw_name, deduped = %deduped, "name conflict, auto-deduped");
            deduped
        } else {
            raw_name.to_string()
        }
    };
    let name: &str = &name_owned;
    let command = args["backend"]
        .as_str()
        .or_else(|| args["command"].as_str())
        .unwrap_or("claude");
    let mut cmd_args = args
        .get("args")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_default();
    if let Some(model) = args
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
    {
        // #2744 r2 (root-review Blocker 1): route through the
        // Backend::push_model_arg chokepoint on the DECLARED identity — the
        // create wire's `backend` param is a declared NAME (parse_str is
        // exact alias resolution); a Raw/Shell parse has no capability and
        // the gate withholds the flag instead of breaking the spawn. The
        // former inline from_command + format!("--model …") assembly was a
        // capability-gate bypass.
        let declared = crate::backend::Backend::parse_str(command);
        let mut argv: Vec<String> = cmd_args.split_whitespace().map(String::from).collect();
        crate::backend::Backend::push_model_arg(&mut argv, &declared, model);
        cmd_args = argv.join(" ");
    }
    if let Some(dir) = args.get("working_directory").and_then(|v| v.as_str()) {
        if std::path::Path::new(dir)
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return json!({"error": "working_directory must not contain '..'"});
        }
        if !dir.starts_with('/') {
            return json!({"error": "working_directory must be an absolute path"});
        }
    }
    let mut work_dir = args
        .get("working_directory")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            crate::paths::workspace_dir(home)
                .join(name)
                .display()
                .to_string()
        });

    // #46776 fail-closed (root finding 2): refuse a colliding workspace identity
    // BEFORE creating any worktree or directory, so a refusal leaves NO partial
    // filesystem/git state. Compute the INTENDED final working directory (the
    // branch worktree path when a branch is set — computed, NOT created) and
    // preflight it against the registry. The atomic add_instance_to_yaml below
    // stays the race-safe authority (with rollback on the rare racing refusal).
    let intended_wd = match args.get("branch").and_then(|v| v.as_str()) {
        Some(branch) if validate_branch(branch) => {
            crate::worktree::worktree_path(home, name, branch)
        }
        _ => std::path::PathBuf::from(&work_dir),
    };
    if let Some(collider) = crate::fleet::workspace_identity_collision(home, name, &intended_wd) {
        return json!({"error": format!(
            "workspace identity collision: '{name}' would resolve to the same working directory as \
             existing instance '{collider}' ({}); refusing before creating any worktree/directory \
             (fail-closed).",
            intended_wd.display()
        )});
    }

    let mut worktree_created_by_attempt = false;
    if let Some(branch) = args.get("branch").and_then(|v| v.as_str()) {
        if !validate_branch(branch) {
            return json!({"error": format!("invalid branch name '{branch}'")});
        }
        // H6 (CR-2026-06-14): validate_branch ALLOWS main/master, so the spawn
        // path must also fire the E4.5 protected-branch gate — else
        // create_instance(branch="main") checks a protected branch into an agent
        // worktree, violating the system-wide "worktree never takes main"
        // invariant (the same guard bind_self / worktree_pool::lease enforce).
        if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
            return e;
        }
        let wd = std::path::PathBuf::from(&work_dir);
        // Sprint 57 Wave 4 (#546 Item 4): worktree creation now takes
        // `home` so the canonical external layout
        // `$AGEND_HOME/worktrees/<agent>/<branch>/` resolves correctly.
        if let Some(info) = crate::worktree::create(home, &wd, name, Some(branch)) {
            work_dir = info.path.display().to_string();
            worktree_created_by_attempt =
                info.provenance == crate::worktree::WorktreeProvenance::CreatedByThisAttempt;
        }
    }

    let work_dir_preexisted =
        !worktree_created_by_attempt && std::path::Path::new(&work_dir).exists();
    std::fs::create_dir_all(&work_dir).ok();

    let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
    let role = args.get("role").and_then(|v| v.as_str()).map(String::from);
    let backend_str = args
        .get("backend")
        .and_then(|v| v.as_str())
        .map(String::from);
    // #900: forward operator-supplied `env` through the SPAWN RPC AND
    // record it on the fleet.yaml entry. The runtime payload lets the
    // daemon's handle_spawn apply it directly (no second fleet.yaml
    // read); the persisted entry covers restart flows that re-resolve
    // from disk later. Non-string values are
    // filtered out at the daemon side via `parse_env_object`.
    let env_value: Option<Value> = args.get("env").filter(|v| v.is_object()).cloned();
    let env_for_entry: Option<std::collections::HashMap<String, String>> =
        env_value.as_ref().and_then(|v| {
            v.as_object().map(|obj| {
                obj.iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
        });
    let (layout, target_pane_owned) =
        super::resolve_team_layout(home, name, args.get("layout"), args.get("target_pane"));
    let target_pane = target_pane_owned.as_deref();

    // #1858: persist the spawn-intent `args` + `model` into the entry so a daemon
    // RESTART re-resolves the SAME backend invocation as the original spawn. At
    // boot, `agent_resolve::resolve_one` reads `entry.args` (None → empty argv) and
    // appends `--model` only from `entry.model` (None → no model flag) — so a
    // sparse entry boots the instance "less than" spawn (missing the user args and
    // the model flag → bare / stuck Starting). `instructions` is NOT lost (it is
    // regenerated from role+peers at boot, agent_resolve.rs); `command` is covered
    // by `backend`; `ready_pattern` is built-in — so ONLY these two need backfill.
    // Split matches `handle_spawn`'s `params["args"].split_whitespace()` so boot's
    // `entry.args` reproduces the create-path SPAWN argv (minus the model flag,
    // which boot re-derives from `entry.model` — same as create's cmd_args build).
    let entry_args: Option<Vec<String>> = args
        .get("args")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.split_whitespace().map(String::from).collect());
    let entry_model: Option<String> = args
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
        .map(String::from);
    let entry_model_tier: Option<String> = args
        .get("model_tier")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
        .map(String::from);
    // #964: ADD fleet.yaml entry BEFORE the SPAWN RPC so the instance
    // exists when SPAWN runs. Pre-fix SPAWN-then-add ordering caused
    // silent failures.
    let entry = crate::fleet::InstanceYamlEntry {
        backend: backend_str
            .or_else(|| {
                crate::backend::Backend::from_command(command).map(|b| b.name().to_string())
            })
            .or_else(|| Some(command.to_string())),
        working_directory: Some(work_dir.clone()),
        role: role.clone(),
        args: entry_args,
        model: entry_model,
        model_tier: entry_model_tier,
        env: env_for_entry,
        topic_binding_mode: args
            .get("topic_binding")
            .and_then(|v| v.as_str())
            .filter(|s| matches!(*s, "skip" | "deferred"))
            .map(String::from),
        // ACL: stamp the identified caller so `delete_instance` can later
        // allow the creator to reclaim its own spawn. Anonymous/operator-
        // direct creates (empty `instance_name`) leave this unset.
        created_by: (!instance_name.is_empty()).then(|| instance_name.to_string()),
        ..Default::default()
    };
    if let Err(e) = crate::fleet::add_instance_to_yaml(home, name, &entry) {
        // Only roll back filesystem state that THIS attempt created. A
        // pre-existing dir (reused worktree or occupied workspace) must survive.
        if !work_dir_preexisted {
            let _ = crate::agent_ops::cleanup_working_dir(
                home,
                name,
                &std::path::PathBuf::from(&work_dir),
            );
        }
        return json!({"error": format!("failed to register instance in fleet.yaml: {e}")});
    }

    let mut spawn_params = json!({
        "name": name, "backend": command, "args": &cmd_args,
        "working_directory": work_dir,
        "layout": layout, "spawner": instance_name,
        "target_pane": target_pane,
        "role": role,
    });
    if let Some(env) = env_value.as_ref() {
        spawn_params["env"] = env.clone();
    }
    if let Some(tb) = args.get("topic_binding").and_then(|v| v.as_str()) {
        spawn_params["topic_binding"] = json!(tb);
    }
    match spawn_fn(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": spawn_params}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let topic_id = resp["result"]["topic_id"].as_i64();
            if let Some(task_text) = task {
                let h = home.to_path_buf();
                let n = name.to_string();
                // fire-and-forget: single-agent task injection (M5 §10.5).
                let rt_arcs = runtime.map(|rt| {
                    (
                        std::sync::Arc::clone(&rt.registry),
                        std::sync::Arc::clone(&rt.externals),
                    )
                });
                #[cfg(test)]
                let run_inline = INJECT_INLINE.lock().as_ref() == Some(&h);
                #[cfg(not(test))]
                let run_inline = false;
                let inject_body = move || {
                    if let Err(detail) =
                        inject_with_routing(&h, &n, task_text.as_bytes(), rt_arcs.as_ref())
                    {
                        tracing::warn!(
                            agent = %n,
                            error = %detail,
                            "team-spawn task INJECT failed — member started without its task text (re-inject manually)"
                        );
                        crate::event_log::log(
                            &h,
                            "team_spawn_inject_failed",
                            &n,
                            &format!("task text inject failed after spawn: {detail}"),
                        );
                    }
                };
                if run_inline {
                    inject_body();
                } else {
                    std::thread::Builder::new()
                        .name("task_inject".into())
                        .spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            inject_body();
                        })
                        .ok();
                }
            }
            let mut result = json!({"name": name, "backend": command});
            if let Some(tid) = topic_id {
                result["topic_id"] = json!(tid);
            }
            result
        }
        Ok(resp) => {
            rollback_fleet_entry_on_failure(home, name, "SPAWN failed");
            json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
        }
        Err(e) => {
            rollback_fleet_entry_on_failure(home, name, "API unavailable");
            json!({"error": format!("API unavailable: {e}")})
        }
    }
}

/// #964 rollback helper: undo `add_instance_to_yaml` after a SPAWN/API
/// failure so create_instance is all-or-nothing. dev-2 cross-audit
/// Pushback 1 — surface rollback-failure via `tracing::error!` (NOT
/// `let _ = ...` — that would repeat the #962 antipattern). Operator
/// gets an audit trail on the rare double-failure case.
fn rollback_fleet_entry_on_failure(home: &Path, name: &str, primary_failure: &str) {
    if let Err(remove_err) = crate::fleet::remove_instance_from_yaml(home, name) {
        tracing::error!(
            name = %name,
            error = %remove_err,
            primary_failure = %primary_failure,
            "create_instance: rollback failed — fleet.yaml may have stale entry; \
             operator may need manual cleanup"
        );
    }
}
