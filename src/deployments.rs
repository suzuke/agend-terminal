//! Deployment tracking — batch instance creation from fleet templates.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub name: String,
    pub template: String,
    pub instances: Vec<String>,
    pub team: Option<String>,
    pub directory: String,
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeploymentStore {
    #[serde(default)]
    schema_version: u32,
    deployments: Vec<Deployment>,
}

impl crate::store::SchemaVersioned for DeploymentStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "deployments.json")
}

/// H14: the JSON error for a duplicate deploy name (used by both the pre-spawn
/// read-check and the authoritative under-flock re-check in `deploy`).
fn duplicate_deploy_error(deploy_name: &str) -> Value {
    serde_json::json!({
        "error": format!(
            "a deployment named '{deploy_name}' already exists — teardown it first or deploy a different name"
        ),
        "name": deploy_name,
    })
}

fn load(home: &Path) -> DeploymentStore {
    crate::store::load_versioned(
        &store_path(home),
        <DeploymentStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

fn save(home: &Path, store: &mut DeploymentStore) -> anyhow::Result<()> {
    use crate::store::SchemaVersioned;
    *store.version_mut() = DeploymentStore::CURRENT;
    crate::store::save_atomic(&store_path(home), store)
}

struct DeployParams {
    template: String,
    deploy_name: String,
    branch: Option<String>,
    directory: String,
    template_def: serde_yaml_ng::Value,
    template_source_repo: Option<String>,
    instances_def: serde_yaml_ng::Mapping,
}

fn validate_deploy_args(home: &Path, args: &Value) -> Result<DeployParams, Value> {
    let template = args["template"]
        .as_str()
        .ok_or_else(|| serde_json::json!({"error": "missing 'template'"}))?
        .to_string();
    let deploy_name = args["name"].as_str().unwrap_or(&template).to_string();
    let branch = args["branch"].as_str().map(String::from);

    crate::agent::validate_name(&template)
        .map_err(|e| serde_json::json!({"error": format!("invalid template name: {e}")}))?;
    crate::agent::validate_name(&deploy_name)
        .map_err(|e| serde_json::json!({"error": format!("invalid deploy name: {e}")}))?;

    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        return Err(serde_json::json!({"error": "No fleet.yaml"}));
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path)
        .map_err(|e| serde_json::json!({"error": format!("fleet.yaml: {e}")}))?;

    let templates = config
        .templates
        .as_ref()
        .ok_or_else(|| serde_json::json!({"error": "No templates defined in fleet.yaml"}))?;
    let template_def = templates
        .get(&template)
        .ok_or_else(|| serde_json::json!({"error": format!("Template '{template}' not found")}))?
        .clone();
    let instances_def = template_def
        .get("instances")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| serde_json::json!({"error": "Template has no instances"}))?
        .clone();

    let directory = if let Some(d) = args["directory"].as_str() {
        d.to_string()
    } else if let Some(d) = template_def.get("directory").and_then(|v| v.as_str()) {
        d.to_string()
    } else {
        crate::paths::workspace_dir(home)
            .join(&deploy_name)
            .display()
            .to_string()
    };

    let template_source_repo = template_def
        .get("source_repo")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(DeployParams {
        template,
        deploy_name,
        branch,
        directory,
        template_def,
        template_source_repo,
        instances_def,
    })
}

fn yaml_str(val: &serde_yaml_ng::Value, key: &str) -> Option<String> {
    val.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn create_instance_entries(
    params: &DeployParams,
) -> (Vec<String>, Vec<(String, crate::fleet::InstanceYamlEntry)>) {
    let mut created = Vec::new();
    let mut yaml_entries = Vec::new();
    let dir = std::path::PathBuf::from(&params.directory);

    for (name_val, inst_val) in &params.instances_def {
        let inst_suffix = match name_val.as_str() {
            Some(s) => s,
            None => continue,
        };
        if let Err(e) = crate::agent::validate_name(inst_suffix) {
            tracing::warn!(deploy_name = %params.deploy_name, suffix = %inst_suffix, error = %e,
                "skipping template instance with invalid name");
            continue;
        }
        let inst_name = format!("{}-{inst_suffix}", params.deploy_name);
        if let Err(e) = crate::agent::validate_name(&inst_name) {
            tracing::warn!(%inst_name, error = %e, "skipping: combined instance name fails validation");
            continue;
        }

        let backend_label = inst_val
            .get("backend")
            .and_then(|v| v.as_str())
            .unwrap_or("claude");
        let role = inst_val
            .get("role")
            .or_else(|| inst_val.get("description"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let template_args = inst_val
            .get("args")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            })
            .filter(|v| !v.is_empty());
        let template_env = inst_val
            .get("env")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                let mut out = std::collections::HashMap::new();
                for (k, v) in m {
                    if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                        out.insert(k.to_string(), v.to_string());
                    }
                }
                out
            })
            .filter(|m| !m.is_empty());
        let source_repo = inst_val
            .get("source_repo")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or(params.template_source_repo.clone());

        let inst_dir = dir.join(&inst_name);
        let work_dir = prepare_work_dir(
            &inst_dir,
            &dir,
            &params.deploy_name,
            inst_suffix,
            &inst_name,
            params.branch.as_deref(),
        );

        yaml_entries.push((
            inst_name.clone(),
            crate::fleet::InstanceYamlEntry {
                backend: Some(backend_label.to_string()),
                working_directory: Some(work_dir),
                role,
                instructions: yaml_str(inst_val, "instructions"),
                source_repo,
                skills_path: yaml_str(inst_val, "skills_path"),
                // #2104 (cheerc): both operator-controlled override fields were
                // hardcoded None here → templates that set them were silently
                // dropped. `repo` = explicit owner/name override (else daemon
                // derives from source_repo); github_login feeds task_sweep's
                // authorship gate (its absence false-fired D002). Read from the
                // template stanza like the sibling yaml_str fields above; a
                // template that omits them still yields None (unchanged).
                repo: yaml_str(inst_val, "repo"),
                github_login: yaml_str(inst_val, "github_login"),
                args: template_args,
                model: yaml_str(inst_val, "model"),
                model_tier: yaml_str(inst_val, "model_tier"),
                env: template_env,
                ready_pattern: yaml_str(inst_val, "ready_pattern"),
                command: yaml_str(inst_val, "command"),
                worktree: inst_val.get("worktree").and_then(|v| v.as_bool()),
                // #991 PR-B: was hardcoded None → a template's `topic_binding:
                // skip`/`deferred` was silently dropped. Same filter as the
                // `create_instance` MCP path (spawn.rs): only "skip"/"deferred"
                // persist, anything else (including "auto" or an invalid value)
                // is None — unchanged auto default.
                topic_binding_mode: inst_val
                    .get("topic_binding")
                    .and_then(|v| v.as_str())
                    .filter(|s| matches!(*s, "skip" | "deferred"))
                    .map(String::from),
                created_by: None, // no single ACL creator for templated instances
            },
        ));
        created.push(inst_name);
    }
    (created, yaml_entries)
}

fn prepare_work_dir(
    inst_dir: &std::path::Path,
    parent_dir: &std::path::Path,
    deploy_name: &str,
    inst_suffix: &str,
    inst_name: &str,
    branch: Option<&str>,
) -> String {
    if let Some(br) = branch {
        let branch_name = format!("{deploy_name}/{inst_suffix}");
        // W1.2: LOCAL `git worktree add` via the bypass+bounded helper. The 3-way
        // match maps onto GitError: NonZero (git ran, rejected) keeps the
        // stderr-bearing "worktree failed" warn; Spawn (git never produced a
        // status) keeps the "git not available" warn. GitError's stderr is already
        // trimmed, matching the prior `.trim()`.
        match crate::git_helpers::git_cmd(
            parent_dir,
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                &inst_dir.display().to_string(),
                br,
            ],
        ) {
            Ok(_) => {
                tracing::info!(%inst_name, %branch_name, "created worktree");
            }
            Err(crate::git_helpers::GitError::NonZero { stderr, .. }) => {
                tracing::warn!(%inst_name, error = %stderr, "worktree failed");
            }
            Err(crate::git_helpers::GitError::Spawn(e)) => {
                tracing::warn!(error = %e, "git not available");
            }
        }
    } else {
        std::fs::create_dir_all(inst_dir).ok();
    }
    inst_dir.display().to_string()
}

fn persist_to_fleet_yaml(
    home: &Path,
    yaml_entries: &[(String, crate::fleet::InstanceYamlEntry)],
    template: &str,
    deploy_name: &str,
) -> Result<(), Value> {
    if yaml_entries.is_empty() {
        return Ok(());
    }
    let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
        yaml_entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
    crate::fleet::add_instances_to_yaml(home, &refs).map_err(|e| {
        tracing::error!(error = %e, template, deploy_name, count = yaml_entries.len(),
            "deploy: Phase 2 add_instances_to_yaml failed — aborting before Phase 3 spawn");
        serde_json::json!({
            "error": format!(
                "deploy_template: failed to persist {} instance(s) to fleet.yaml: {e} \
                 — Phase 3 spawn aborted to prevent partial-success state (no agents spawned)",
                yaml_entries.len()
            ),
            "code": "deploy_yaml_persist_failed",
        })
    })
}

/// MED-2 (re-marshal allowlist-drop): the binary deploy's Phase-3 SPAWN should
/// run for `inst_name`. The SPAWN handler runs `params["backend"]` AS the
/// command, and a template's `command:` override is persisted to fleet.yaml in
/// Phase 2 (before Phase 3), so resolve via `FleetConfig` —
/// `resolved.backend_command` honors `command:` over the `backend:` preset,
/// mirroring every sibling spawn path (start/restart/replace/cold-boot). The
/// pre-fix code passed raw `entry.backend`, silently ignoring `command:` and
/// spawning the preset binary on first deploy. Falls back to `entry.backend`
/// (then `"claude"`) only if the entry can't be resolved.
fn resolve_spawn_backend(
    home: &Path,
    inst_name: &str,
    entry: &crate::fleet::InstanceYamlEntry,
) -> String {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|c| c.resolve_instance(inst_name))
        .map(|r| r.backend_command)
        .unwrap_or_else(|| {
            entry
                .backend
                .clone()
                .unwrap_or_else(|| "claude".to_string())
        })
}

fn spawn_instances(
    home: &Path,
    yaml_entries: &[(String, crate::fleet::InstanceYamlEntry)],
    directory: &str,
) {
    for (inst_name, entry) in yaml_entries {
        let backend_name = resolve_spawn_backend(home, inst_name, entry);
        let work_dir = entry.working_directory.as_deref().unwrap_or(directory);
        let mut params = serde_json::json!({
            "name": inst_name,
            "backend": backend_name,
            "working_directory": work_dir,
        });
        if let Some(ref model) = entry.model {
            params["model"] = serde_json::json!(model);
        }
        if let Some(ref args) = entry.args {
            if !args.is_empty() {
                params["args"] = serde_json::json!(args.join(" "));
            }
        }
        if let Some(ref env) = entry.env {
            if !env.is_empty() {
                params["env"] = serde_json::to_value(env).unwrap_or(serde_json::Value::Null);
            }
        }
        // #991 PR-B: forward the template-derived topic_binding_mode so
        // handle_spawn's existing skip/deferred gate (api/handlers/
        // instance.rs) actually honors it — without this, the field landed
        // correctly in fleet.yaml (create_instance_entries) but a topic got
        // created anyway (SPAWN defaults topic_binding to "auto" when absent).
        if let Some(ref tb) = entry.topic_binding_mode {
            params["topic_binding"] = serde_json::json!(tb);
        }
        let spawn_result = crate::api::call(
            home,
            &serde_json::json!({"method": crate::api::method::SPAWN, "params": params}),
        );
        match spawn_result {
            Ok(ref v) if v.get("ok").and_then(|b| b.as_bool()) == Some(false) => {
                let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                tracing::error!(instance = %inst_name, error = %err, "deploy: Phase 3 spawn failed");
            }
            Err(e) => {
                tracing::error!(instance = %inst_name, error = %e, "deploy: Phase 3 spawn call failed");
            }
            _ => {}
        }
    }
}

fn create_deployment_team(
    home: &Path,
    deploy_name: &str,
    template: &str,
    template_def: &serde_yaml_ng::Value,
    template_source_repo: &Option<String>,
    created: &[String],
) -> bool {
    if created.len() <= 1 {
        return false;
    }
    let mut team_args = serde_json::json!({
        "name": deploy_name,
        "members": &created,
        "description": format!("Template deployment: {template}"),
    });
    if let Some(ref sr) = template_source_repo {
        team_args["repository_path"] = serde_json::Value::String(sr.clone());
    }
    if let Some(suffix) = template_def.get("orchestrator").and_then(|v| v.as_str()) {
        let full = format!("{deploy_name}-{suffix}");
        if created.contains(&full) {
            team_args["orchestrator"] = serde_json::Value::String(full);
        } else {
            tracing::warn!(
                template,
                suffix,
                "template orchestrator not among spawned instances; team created without one"
            );
        }
    }
    // H15 (CR-2026-06-14): a daemon REJECTION comes back as `Ok(v)` with
    // `v["ok"] == false`, NOT as `Err` — the old catch-all Ok no-op arm swallowed
    // it as success, so `deploy` recorded a `team` that was never created. Inspect
    // the `ok` field (mirroring `spawn_instances`); on a rejection fall back to a
    // direct create, same as a transport error. Return whether a team actually
    // exists so `deploy` records `team: Some(..)` only when one was created.
    match crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::CREATE_TEAM, "params": &team_args}),
    ) {
        Ok(ref v) if v.get("ok").and_then(|b| b.as_bool()) == Some(false) => {
            crate::teams::create(home, &team_args)
                .get("error")
                .is_none()
        }
        Ok(_) => true,
        Err(_) => crate::teams::create(home, &team_args)
            .get("error")
            .is_none(),
    }
}

pub fn deploy(home: &Path, instance_name: &str, args: &Value) -> Value {
    let params = match validate_deploy_args(home, args) {
        Ok(p) => p,
        Err(e) => return e,
    };

    // H14 (CR-2026-06-14): reject a duplicate deploy name BEFORE any side-effect
    // (fleet.yaml write / spawn), so re-deploying an existing name doesn't re-spawn
    // + clobber fleet.yaml + push a second record. This is a plain READ, NOT a
    // flock: #1629 forbids holding ANY flock across the self-IPC `api::call` in
    // spawn_instances / create_deployment_team below (the FLOCK_DEPTH self-IPC
    // guard refuses on depth > 0, regardless of which lock is held), and the #1617
    // invariant forbids taking the store flock before spawn. The AUTHORITATIVE
    // re-check runs under the store flock at the load-modify-save below, closing
    // the narrow window where two deploys race before either persists its record.
    if load(home)
        .deployments
        .iter()
        .any(|d| d.name == params.deploy_name)
    {
        return duplicate_deploy_error(&params.deploy_name);
    }

    let (created, yaml_entries) = create_instance_entries(&params);

    if let Err(e) =
        persist_to_fleet_yaml(home, &yaml_entries, &params.template, &params.deploy_name)
    {
        return e;
    }

    spawn_instances(home, &yaml_entries, &params.directory);

    let team_created = create_deployment_team(
        home,
        &params.deploy_name,
        &params.template,
        &params.template_def,
        &params.template_source_repo,
        &created,
    );

    let deployment = Deployment {
        name: params.deploy_name.to_string(),
        template: params.template.to_string(),
        instances: created.clone(),
        team: if team_created {
            Some(params.deploy_name.to_string())
        } else {
            None
        },
        directory: params.directory,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    // #1629: narrow the deployment-store flock to JUST the load-modify-save (its
    // C1 lost-update purpose). spawn_instances (api::call SPAWN) and
    // create_deployment_team (api::call CREATE_TEAM) above now run lock-free — a
    // self-IPC (loopback api::call) held under this flock is the #1617
    // lock-while-blocking deadlock class. validate_deploy_args reads only
    // fleet.yaml, not the store, so it needs no lock either.
    let lock_path = store_path(home).with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => return serde_json::json!({"error": format!("deployment lock failed: {e}")}),
    };
    let mut store = load(home);
    // H14: authoritative duplicate-name re-check UNDER the flock — closes the race
    // where a concurrent same-name deploy passed the pre-spawn read above and
    // persisted its record first. The loser drops its record (its spawned instances
    // share the winner's names; the daemon SPAWN handler rejects duplicate names).
    if store
        .deployments
        .iter()
        .any(|d| d.name == params.deploy_name)
    {
        return duplicate_deploy_error(&params.deploy_name);
    }
    store.deployments.push(deployment);
    // #bughunt2: a deploy whose record never persisted is NOT "deployed" — the
    // instances are live in fleet.yaml but `teardown <name>` can't find them and
    // a daemon restart resurrects them untracked. Surface the failure (with the
    // spawned instances) so the operator can reconcile, instead of reporting
    // success.
    if let Err(e) = save(home, &mut store) {
        return serde_json::json!({
            "error": format!(
                "deployment '{}' spawned {} instance(s) but failed to persist the deployment record: {e} — teardown-by-name will not work until reconciled",
                params.deploy_name, created.len()
            ),
            "name": params.deploy_name,
            "instances": created,
        });
    }

    let _ = instance_name;
    serde_json::json!({"status": "deployed", "name": params.deploy_name, "instances": created})
}

pub fn teardown(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };

    let lock_path = store_path(home).with_extension("lock");
    // #1629: read the deployment record lock-free (load reads atomically-written
    // files, so no flock is needed) so the DELETE api::calls below run OUTSIDE any
    // flock — a self-IPC under the deployment flock is the #1617 deadlock class.
    let deployment = match load(home).deployments.iter().find(|d| d.name == name) {
        Some(d) => d.clone(),
        None => return serde_json::json!({"error": format!("deployment '{name}' not found")}),
    };

    // Delete all instances (full cleanup via DELETE instead of KILL — #456).
    for inst in &deployment.instances {
        let _ = crate::api::call(
            home,
            &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": inst}}),
        );
    }

    // Smoke 2 fix: filesystem cleanup of every spawned subdir, including
    // custom-`directory` deployments that the prior inline
    // `home/workspace/<inst>` loop missed.
    cleanup_deployment_dirs(home, &deployment);

    // Symmetrical with `deploy`: we wrote entries into fleet.yaml so
    // pane_factory could render identity; teardown must remove them or
    // daemon restart would resurrect dead agents via auto_start_fleet.
    if let Err(e) = crate::fleet::remove_instances_from_yaml(home, &deployment.instances) {
        tracing::warn!(error = %e, "failed to clean up fleet.yaml on teardown");
    }

    // Delete team if exists
    if let Some(ref team) = deployment.team {
        let _ = crate::teams::delete(home, &serde_json::json!({"name": team}));
    }

    // #1629 (C1 lost-update): flock ONLY the store record-removal load-modify-save.
    // Re-load under the flock so a concurrent deploy/teardown isn't lost-updated.
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => return serde_json::json!({"error": format!("deployment lock failed: {e}")}),
    };
    let mut store = load(home);
    // Remove from store
    store.deployments.retain(|d| d.name != name);
    // #bughunt2: if the record-removal save fails, the instances are already
    // gone from fleet.yaml but the stale record lingers on disk — `list` and a
    // re-`teardown` will still show it. Surface it rather than reporting a clean
    // torn_down.
    if let Err(e) = save(home, &mut store) {
        return serde_json::json!({
            "error": format!(
                "deployment '{name}' instances were removed but failed to persist the record cleanup: {e} — the stale record remains; retry teardown"
            ),
            "name": name,
            "instances": deployment.instances,
        });
    }

    serde_json::json!({"status": "torn_down", "name": name, "instances": deployment.instances})
}

pub fn list(home: &Path) -> Value {
    let store = load(home);
    serde_json::json!({"deployments": store.deployments})
}

/// Smoke 2 fix (post-#475): close-path + teardown both leave deployment
/// member subdirs on disk when the deployment used a custom `directory:`
/// arg outside `$AGEND_HOME/workspace/`. `cleanup_working_dir`'s
/// user-provided-dir branch only removes specific agend files (by design,
/// to protect user data), so a deployment with `directory: /tmp/foo` and
/// member `foo-lead` leaves `/tmp/foo/foo-lead/` behind even after the
/// agend files are stripped.
///
/// This helper is the single source of truth for "remove every subdir a
/// deployment spawned" — used by both `reconcile_orphan_deployments`
/// (close-path + boot-sweep) and `teardown` (operator action).
///
/// Removes:
/// - `<deployment.directory>/<inst_name>` — the actual spawned subdir per
///   `deploy()`'s `inst_dir = dir.join(&inst_name)`. Whole-tree removal
///   because the subdir is daemon-managed.
/// - `<home>/workspace/<inst_name>` — default-path fallback via
///   `cleanup_working_dir`, so the AGEND_HOME/workspace branch handles
///   default-directory deployments correctly.
///
/// All filesystem ops are best-effort (`let _ = ...` / matched and logged);
/// a single per-instance failure doesn't abort the rest of the sweep.
fn cleanup_deployment_dirs(home: &Path, deployment: &Deployment) {
    let custom_root = std::path::Path::new(&deployment.directory);
    // MED-4: a branch-mode deploy creates a git worktree per instance via
    // `git worktree add -b {deploy}/{suffix}` in the deploy directory (which IS
    // the source repo in branch mode). A bare `remove_dir_all` left a prunable
    // `.git/worktrees/<seg>` registry entry + the orphan branch behind, so a
    // same-name re-deploy failed ("already exists" / "already checked out").
    // When the deploy dir is a git repo, tear the worktree + branch down FIRST,
    // via the daemon's bypass git (mirrors `worktree_pool::release_full`). All
    // best-effort: harmless no-ops for a non-branch deploy (subdir isn't a
    // worktree, branch doesn't exist).
    let dir_is_repo = crate::worktree::is_git_repo(custom_root);
    for inst in &deployment.instances {
        // Custom-directory branch: deploy()'s `inst_dir = dir.join(&inst_name)`.
        let custom_subdir = custom_root.join(inst);
        if dir_is_repo {
            // Instances are named `{deploy_name}-{suffix}`; the worktree branch
            // is `{deploy_name}/{suffix}` (see prepare_work_dir).
            let suffix = inst
                .strip_prefix(&format!("{}-", deployment.name))
                .unwrap_or(inst);
            let branch = format!("{}/{}", deployment.name, suffix);
            let subdir_str = custom_subdir.display().to_string();
            // worktree remove unregisters + deletes the dir; branch -D drops the
            // orphan; prune sweeps any dangling entry if the remove failed.
            let _ = crate::git_helpers::git_bypass(
                custom_root,
                &["worktree", "remove", "--force", &subdir_str],
            );
            let _ = crate::git_helpers::git_bypass(custom_root, &["branch", "-D", &branch]);
            let _ = crate::git_helpers::git_bypass(custom_root, &["worktree", "prune"]);
        }
        if custom_subdir.exists() {
            match std::fs::remove_dir_all(&custom_subdir) {
                Ok(()) => tracing::info!(
                    inst = %inst,
                    path = %custom_subdir.display(),
                    "deployment cleanup: removed custom subdir"
                ),
                Err(e) => tracing::warn!(
                    inst = %inst,
                    path = %custom_subdir.display(),
                    error = %e,
                    "deployment cleanup: remove_dir_all failed"
                ),
            }
        }
        // Default-path branch: covers deployments whose `directory` defaulted
        // to `home/workspace/<deploy_name>` (subdir lands at
        // `home/workspace/<deploy_name>/<inst>`) AND covers historical
        // teardown semantics that cleaned `home/workspace/<inst>` directly.
        let default_subdir = crate::paths::workspace_dir(home).join(inst);
        if default_subdir.exists() {
            crate::agent_ops::cleanup_working_dir(home, inst, &default_subdir);
        }
    }
    // Sprint 54 P1-5: best-effort rmdir of the custom-directory parent.
    // If every member subdir was just removed AND the operator left no
    // unrelated files there, the parent is now empty — strip it so we
    // don't leak `/tmp/team-foo/` shells behind. `remove_dir` (NOT
    // `remove_dir_all`) errors on non-empty, which is exactly what we
    // want: any operator-dropped file preserves the parent.
    rmdir_if_empty(custom_root);
}

/// Best-effort rmdir of an empty directory (Sprint 54 P1-5).
///
/// Uses [`std::fs::remove_dir`] (not `remove_dir_all`) so the call
/// fails non-destructively when the directory still has contents the
/// daemon didn't put there. Logs an info-level event on success and
/// debug-level on skip — never returns an error to callers.
fn rmdir_if_empty(path: &Path) {
    match std::fs::remove_dir(path) {
        Ok(()) => tracing::info!(
            path = %path.display(),
            "deployment cleanup: removed empty parent dir"
        ),
        // Already gone — idempotency-friendly, common on second teardown.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Non-empty (operator dropped files) or permission denied — log
        // at debug because non-empty is the expected mixed-use case;
        // unexpected errors are still discoverable via `RUST_LOG=debug`.
        Err(e) => tracing::debug!(
            path = %path.display(),
            error = %e,
            "deployment cleanup: parent rmdir skipped (likely non-empty)"
        ),
    }
}

/// Issue #474: prune deployment entries whose instances no longer exist in
/// fleet.yaml. Shared core for the close-path hook (`reconcile_after_close`)
/// and the daemon-startup sweep (`reconcile_orphans`).
///
/// For each deployment:
/// - if NONE of its `instances` are present in fleet.yaml → prune the
///   deployment entry from the store, delete the associated team, log.
/// - otherwise → leave the deployment intact (multi-instance deployment
///   with at least one member still alive).
///
/// Returns the names of deployments that were pruned (empty when nothing
/// changed). The caller decides whether to log/event-report.
pub(crate) fn reconcile_orphan_deployments(home: &Path) -> Vec<String> {
    // Same lock as deploy/teardown — load-modify-save must be serialized.
    let lock_path = store_path(home).with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "deployments reconcile: lock acquire failed — skipping");
            return Vec::new();
        }
    };

    let mut store = load(home);
    if store.deployments.is_empty() {
        return Vec::new();
    }

    // Snapshot current fleet.yaml instance set. If fleet.yaml fails to load,
    // bail out with an empty result so a transient parse error doesn't wipe
    // the deployment store.
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let live_instances: std::collections::HashSet<String> = match crate::fleet::FleetConfig::load(
        &fleet_path,
    ) {
        Ok(cfg) => cfg.instance_names().into_iter().collect(),
        Err(e) => {
            tracing::warn!(error = %e, "deployments reconcile: fleet.yaml load failed — skipping");
            return Vec::new();
        }
    };

    // Collect the pruned `Deployment` records (not just names) so we can
    // hand each one to `cleanup_deployment_dirs` after the store is saved.
    // Smoke 2 fix: without this we lose the `directory` + `instances` info
    // needed to remove custom-directory subdirs.
    let mut pruned_names = Vec::new();
    let mut pruned_teams = Vec::new();
    let mut pruned_deployments: Vec<Deployment> = Vec::new();
    store.deployments.retain(|d| {
        let any_live = d.instances.iter().any(|i| live_instances.contains(i));
        if any_live {
            true
        } else {
            pruned_names.push(d.name.clone());
            if let Some(t) = d.team.clone() {
                pruned_teams.push(t);
            }
            pruned_deployments.push(d.clone());
            false
        }
    });

    if pruned_names.is_empty() {
        return pruned_names;
    }

    // Persist pruned store first so a crash between save and team-delete
    // doesn't leave the deployment-store in a more-stale state than the
    // teams-store; teams without their parent deployment is the safer
    // failure mode.
    if let Err(e) = save(home, &mut store) {
        tracing::warn!(
            error = %e,
            pruned = ?pruned_names,
            "deployments reconcile: save failed — entries may resurface on next load"
        );
        return Vec::new();
    }

    for team in &pruned_teams {
        let _ = crate::teams::delete(home, &serde_json::json!({"name": team}));
    }

    // Smoke 2 fix: clean each pruned deployment's spawned subdirs. Runs
    // AFTER the store save + team delete so a deployment store entry
    // doesn't survive its filesystem cleanup (the safer failure mode is
    // "files gone, store entry stays" not "store says clean, files leak").
    for dep in &pruned_deployments {
        cleanup_deployment_dirs(home, dep);
    }

    tracing::info!(
        pruned = ?pruned_names,
        teams = ?pruned_teams,
        "deployments reconcile: pruned orphan entries"
    );
    pruned_names
}

/// Option 1 (auto-cleanup) hook. Called from the TUI close path AFTER
/// `fleet::remove_instance(s)_from_yaml`, with the names that were just
/// removed. Triggers `reconcile_orphan_deployments`, which detects the
/// "last instance of this deployment was just closed" case generically.
///
/// `removed_names` is currently unused — the reconcile pass scans every
/// deployment against the post-close fleet.yaml, so it doesn't need to
/// know which specific names were removed. Kept in the signature so a
/// future optimization can target only deployments touching those names
/// without changing the call site.
pub fn reconcile_after_close(home: &Path, removed_names: &[String]) -> Vec<String> {
    let _ = removed_names;
    reconcile_orphan_deployments(home)
}

/// Option 3 (defensive) hook. Called once at daemon startup, before
/// `auto_start_fleet`, so a stale deployment-store entry left by a
/// previous unclean shutdown doesn't carry over.
pub fn reconcile_orphans(home: &Path) -> Vec<String> {
    reconcile_orphan_deployments(home)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

#[cfg(test)]
mod review_repro_deployments_health_teams;
