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

pub(super) fn spawn_single_instance(home: &Path, instance_name: &str, args: &Value) -> Value {
    spawn_single_instance_impl(home, instance_name, args, &crate::api::call)
}

/// Inner impl of [`spawn_single_instance`] parameterized on the SPAWN RPC for
/// `instance_964_tests`. Production passes [`crate::api::call`].
pub(in crate::mcp::handlers) fn spawn_single_instance_impl(
    home: &Path,
    instance_name: &str,
    args: &Value,
    spawn_fn: &dyn Fn(&Path, &Value) -> anyhow::Result<Value>,
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
        let model_val = crate::backend::Backend::from_command(command)
            .map(|b| b.format_model_arg(model))
            .unwrap_or_else(|| model.to_string());
        if !cmd_args.is_empty() {
            cmd_args.push(' ');
        }
        cmd_args.push_str(&format!("--model {model_val}"));
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
        }
    }

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
    // read); the persisted entry covers replace_instance / restart
    // flows that re-resolve from disk later. Non-string values are
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
        env: env_for_entry,
        topic_binding_mode: args
            .get("topic_binding")
            .and_then(|v| v.as_str())
            .filter(|s| matches!(*s, "skip" | "deferred"))
            .map(String::from),
        ..Default::default()
    };
    if let Err(e) = crate::fleet::add_instance_to_yaml(home, name, &entry) {
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
                std::thread::Builder::new()
                    .name("task_inject".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        // #2004: a swallowed INJECT failure left a slow-starting
                        // member idle with ZERO task context, invisibly. Pure
                        // surfacing — the spawn itself already succeeded, so a
                        // failed inject stays non-fatal (operator re-injects).
                        let resp = crate::api::call(
                            &h,
                            &json!({"method": crate::api::method::INJECT, "params": {"name": n, "data": task_text}}),
                        );
                        let failed = match &resp {
                            Ok(v) => v["ok"].as_bool() != Some(true),
                            Err(_) => true,
                        };
                        if failed {
                            let detail = match resp {
                                Ok(v) => v.to_string(),
                                Err(e) => e.to_string(),
                            };
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
                    })
                    .ok();
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
