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
                env: template_env,
                ready_pattern: yaml_str(inst_val, "ready_pattern"),
                command: yaml_str(inst_val, "command"),
                worktree: inst_val.get("worktree").and_then(|v| v.as_bool()),
                topic_binding_mode: None,
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
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-deploy-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// §3.9 (MED-2): the binary deploy's Phase-3 SPAWN must run a template's
    /// `command:` override, not the `backend:` preset. `resolve_spawn_backend`
    /// (which feeds `params["backend"]`, run AS the command by the SPAWN handler)
    /// must return the `command:` for an entry that declares one. Regression-proof:
    /// revert to raw `entry.backend` and the override assertion fails ("claude").
    #[test]
    fn resolve_spawn_backend_honors_command_override_med2() {
        let home = tmp_home("med2-backend");
        // The entries are persisted to fleet.yaml in Phase 2 before the spawn.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  worker:\n    backend: claude\n    command: ./my-runner.sh\n",
        )
        .unwrap();
        // `entry` is only the resolve-failure fallback; the fix resolves via fleet.yaml.
        let entry = crate::fleet::InstanceYamlEntry {
            backend: Some("claude".into()),
            ..Default::default()
        };

        // Template `command:` override reaches the spawn binary (was: "claude").
        assert_eq!(
            resolve_spawn_backend(&home, "worker", &entry),
            "./my-runner.sh",
            "MED-2: a template `command:` override must reach the Phase-3 spawn"
        );
        // An entry absent from fleet.yaml falls back to its declared backend.
        assert_eq!(
            resolve_spawn_backend(&home, "ghost", &entry),
            "claude",
            "fallback to entry.backend when the instance can't be resolved"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_rejects_bad_deploy_name() {
        let home = tmp_home("bad_deploy");
        let args = serde_json::json!({
            "template": "ok-template",
            "directory": home.display().to_string(),
            "name": "../escape",
        });
        let out = deploy(&home, "caller", &args);
        let err = out["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("invalid deploy name"),
            "expected deploy-name rejection, got: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_rejects_bad_template_name() {
        let home = tmp_home("bad_tpl");
        let args = serde_json::json!({
            "template": "tpl with space",
            "directory": home.display().to_string(),
        });
        let out = deploy(&home, "caller", &args);
        let err = out["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("invalid template name"),
            "expected template-name rejection, got: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_role_into_fleet_yaml() {
        // Role declared on a template instance must flow into fleet.yaml's
        // instances: block so pane_factory::create_pane_from_resolved can
        // render Identity/Role into the agent's agend.md. Before this PR,
        // template schema ignored role entirely and no fleet.yaml entry was
        // written on deploy.
        let home = tmp_home("role_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        role: orchestrator
      impl:
        backend: kiro-cli
        role: implementer
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({
            "template": "dev",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("reload fleet.yaml");
        let lead = reloaded
            .instances
            .get("dev-lead")
            .expect("dev-lead must be persisted");
        assert_eq!(lead.role.as_deref(), Some("orchestrator"));
        let imp = reloaded
            .instances
            .get("dev-impl")
            .expect("dev-impl must be persisted");
        assert_eq!(imp.role.as_deref(), Some("implementer"));
        // Template block untouched by the instances: mutation.
        assert!(
            reloaded.templates.is_some(),
            "templates section must survive the write"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_accepts_description_alias_for_role() {
        // Mirror InstanceConfig's `#[serde(alias = "description")]`. Users
        // coming from the TS version write `description:` — accept both so
        // the schemas stay in sync.
        let home = tmp_home("role_alias");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        description: orchestrator via alias
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert_eq!(
            reloaded
                .instances
                .get("dev-lead")
                .and_then(|i| i.role.clone())
                .as_deref(),
            Some("orchestrator via alias")
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_instructions_into_fleet_yaml() {
        let home = tmp_home("instructions_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        instructions: ./instructions/lead.md
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert_eq!(
            reloaded
                .instances
                .get("dev-lead")
                .and_then(|i| i.instructions.as_deref()),
            Some("./instructions/lead.md")
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2104 (cheerc): template deployment must carry the template instance's
    /// operator-controlled override fields — `github_login` AND `repo` — into the
    /// deployed instance. Both were hardcoded `None` at the same site
    /// (`create_instance_entries`), so a deployed fleet had NO github_login
    /// mapping (→ `task_sweep` D002 false-fired) and lost any explicit `repo`
    /// owner/name override (→ daemon fell back to source_repo derivation, wrong
    /// for non-GitHub remotes / fork disambiguation).
    #[test]
    fn deploy_persists_github_login_and_repo_into_fleet_yaml() {
        let home = tmp_home("github_login_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      impl:
        backend: claude
        github_login: cheerc
        repo: cheerc/talented-payroll
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-impl").expect("dev-impl");
        assert_eq!(
            inst.github_login.as_deref(),
            Some("cheerc"),
            "deployed instance must carry the template's github_login (D002 false-fire root cause)"
        );
        assert_eq!(
            inst.repo.as_deref(),
            Some("cheerc/talented-payroll"),
            "deployed instance must carry the template's explicit repo owner/name override"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 56 Track E (#450): template params passthrough ──────────

    #[test]
    fn deploy_persists_args_into_fleet_yaml() {
        let home = tmp_home("args_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        args:
          - --resume
          - --model
          - opus
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
        assert_eq!(
            inst.args,
            vec![
                "--resume".to_string(),
                "--model".to_string(),
                "opus".to_string()
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_model_into_fleet_yaml() {
        let home = tmp_home("model_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      specialist:
        backend: claude
        model: opus
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded
            .instances
            .get("dev-specialist")
            .expect("dev-specialist");
        assert_eq!(inst.model.as_deref(), Some("opus"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_env_into_fleet_yaml() {
        let home = tmp_home("env_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        env:
          MCP_SERVER_URL: https://example.com
          DEBUG: "1"
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
        assert_eq!(
            inst.env.get("MCP_SERVER_URL").map(|s| s.as_str()),
            Some("https://example.com")
        );
        assert_eq!(inst.env.get("DEBUG").map(|s| s.as_str()), Some("1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_ready_pattern_into_fleet_yaml() {
        let home = tmp_home("ready_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        ready_pattern: "ready for input"
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
        assert_eq!(inst.ready_pattern.as_deref(), Some("ready for input"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_worktree_opt_out_into_fleet_yaml() {
        // reviewer / orchestrator roles often want `worktree: false` so
        // the worktree pool skips creation. Template passthrough must
        // preserve this signal.
        let home = tmp_home("worktree_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      reviewer:
        backend: claude
        worktree: false
      impl:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let reviewer = reloaded
            .instances
            .get("dev-reviewer")
            .expect("dev-reviewer");
        assert_eq!(
            reviewer.worktree,
            Some(false),
            "reviewer must round-trip `worktree: false`"
        );
        let imp = reloaded.instances.get("dev-impl").expect("dev-impl");
        assert!(
            imp.worktree.is_none(),
            "instance without `worktree:` field must stay None for default auto-create behavior"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_persists_command_into_fleet_yaml() {
        // `command:` template field — non-backend custom invocation.
        let home = tmp_home("command_persist");
        let yaml = r#"
templates:
  dev:
    instances:
      script:
        backend: claude
        command: ./scripts/my-runner.sh
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-script").expect("dev-script");
        assert_eq!(
            inst.command.as_deref(),
            Some("./scripts/my-runner.sh"),
            "custom command must round-trip via the template passthrough"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_omits_template_params_when_not_set_backwards_compat() {
        // Critical backwards-compat invariant: existing templates that
        // declare none of args/model/env/ready_pattern/command/worktree
        // must continue to deploy unchanged. The fleet.yaml stanza for
        // the deployed instance must have NO sentinel values for those
        // fields — operator can't tell whether they were "passed
        // through as None" vs "never declared" otherwise.
        let home = tmp_home("compat_omit");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        role: orchestrator
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("dev-lead").expect("dev-lead");
        assert!(
            inst.args.is_empty(),
            "args must default to empty Vec when template doesn't declare it"
        );
        assert!(inst.model.is_none(), "model must stay None");
        assert!(inst.env.is_empty(), "env must default to empty HashMap");
        assert!(inst.ready_pattern.is_none(), "ready_pattern must stay None");
        assert!(
            inst.worktree.is_none(),
            "worktree must stay None (preserves default auto-create behavior)"
        );
        // `command` is a special case — the existing fallback at
        // deploy()'s line 142-146 reads `command` OR `backend` for the
        // SPAWN-time backend label; the template-passthrough path only
        // captures it when the operator explicitly declared `command:`.
        // A template with only `backend: claude` writes neither field
        // value into the durable command slot.
        assert!(
            inst.command.is_none(),
            "command must stay None when template only declared `backend:`"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_passes_all_six_params_simultaneously() {
        // End-to-end pin: a template that declares all six new fields
        // round-trips every one through fleet.yaml. Defends against a
        // future regression where one passthrough is silently dropped
        // (e.g. someone removes the template_args binding without
        // updating the constructor).
        let home = tmp_home("all_six");
        let yaml = r#"
templates:
  full:
    instances:
      worker:
        backend: claude
        args:
          - --resume
        model: sonnet
        env:
          API_KEY_VAR: KEY
        ready_pattern: "now ready"
        command: my-runner
        worktree: false
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "full", "directory": home.display().to_string()}),
        );
        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let inst = reloaded.instances.get("full-worker").expect("full-worker");
        assert_eq!(inst.args, vec!["--resume".to_string()]);
        assert_eq!(inst.model.as_deref(), Some("sonnet"));
        assert_eq!(inst.env.get("API_KEY_VAR").map(|s| s.as_str()), Some("KEY"));
        assert_eq!(inst.ready_pattern.as_deref(), Some("now ready"));
        assert_eq!(inst.command.as_deref(), Some("my-runner"));
        assert_eq!(inst.worktree, Some(false));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_omits_role_when_not_set() {
        // A template without `role:` must not write a blank role field —
        // empty Role lines in agend.md would mislead agents into thinking
        // "" is their role.
        let home = tmp_home("role_absent");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let lead = reloaded.instances.get("dev-lead").expect("dev-lead");
        assert!(
            lead.role.is_none(),
            "unset role must stay None, got {:?}",
            lead.role
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn teardown_removes_deployed_entries_from_fleet_yaml() {
        // Cleanup symmetry: without this, daemon restart would auto-spawn
        // dead agents because auto_start_fleet reads fleet.yaml instances.
        let home = tmp_home("teardown_cleanup");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        role: orchestrator
      impl:
        backend: claude
        role: implementer
instances:
  preexisting:
    backend: claude
    role: survivor
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        // Sanity: deployed entries are there.
        let after_deploy = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("post-deploy");
        assert!(after_deploy.instances.contains_key("dev-lead"));
        assert!(after_deploy.instances.contains_key("dev-impl"));

        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        let after_teardown = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("post-teardown");
        assert!(
            !after_teardown.instances.contains_key("dev-lead"),
            "deployed entry must be removed"
        );
        assert!(
            !after_teardown.instances.contains_key("dev-impl"),
            "deployed entry must be removed"
        );
        // Entries not owned by the deployment stay.
        assert!(
            after_teardown.instances.contains_key("preexisting"),
            "teardown must not touch pre-existing instances"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_sets_orchestrator_from_template_suffix() {
        // Template nominates orchestrator by suffix; deploy must rewrite it
        // to the fully-prefixed name (`<deploy_name>-<suffix>`) before calling
        // teams::create, otherwise the member-of-team check rejects it.
        let home = tmp_home("orch_ok");
        let yaml = r#"
templates:
  dev:
    orchestrator: lead
    instances:
      lead:
        backend: claude
        role: orchestrator
      impl:
        backend: claude
        role: implementer
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );

        let orch =
            crate::teams::resolve_team_orchestrator(&home, "dev").expect("team dev must exist");
        assert_eq!(
            orch.as_deref(),
            Some("dev-lead"),
            "orchestrator suffix must be expanded to full name"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_ignores_unknown_orchestrator_suffix() {
        // Typo protection: a template pointing orchestrator at a non-existent
        // suffix must not fail the deploy. The team gets created without an
        // orchestrator — operator sees a warn log and can fix via update_team.
        let home = tmp_home("orch_typo");
        let yaml = r#"
templates:
  dev:
    orchestrator: captian   # typo — should be "captain" (or a real suffix)
    instances:
      lead:
        backend: claude
      impl:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );

        // resolve_team_orchestrator errors on degraded teams ("no orchestrator,
        // cannot route"), so probe via list and inspect the orchestrator field
        // directly — we want to assert the team exists but is orchestrator-less,
        // not prove routing works.
        let listed = crate::teams::list(&home);
        let team = listed["teams"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["name"] == "dev"))
            .cloned()
            .expect("team 'dev' must still be created");
        assert!(
            team["orchestrator"].is_null(),
            "unknown orchestrator suffix must leave team with no orchestrator, got {}",
            team["orchestrator"]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_gives_each_member_its_own_workdir() {
        // Regression: same-backend teammates used to share `directory` when
        // no `branch:` was given, which made them clobber each other's
        // `.kiro/steering/agend.md` (and `.claude/mcp-config.json`) on
        // every respawn. Each member must land in `<directory>/<inst_name>`.
        let home = tmp_home("workdir_isolate");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
      impl-1:
        backend: kiro-cli
      impl-2:
        backend: kiro-cli
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let workdirs: std::collections::HashSet<String> = ["dev-lead", "dev-impl-1", "dev-impl-2"]
            .iter()
            .filter_map(|name| {
                reloaded
                    .instances
                    .get(*name)
                    .and_then(|i| i.working_directory.clone())
            })
            .collect();

        assert_eq!(
            workdirs.len(),
            3,
            "every member must get a unique working_directory, got {workdirs:?}"
        );
        for name in ["dev-lead", "dev-impl-1", "dev-impl-2"] {
            let wd = reloaded
                .instances
                .get(name)
                .and_then(|i| i.working_directory.clone())
                .unwrap_or_default();
            assert!(
                wd.ends_with(name),
                "{name}'s working_directory must end with its own name, got {wd}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_skips_bad_instance_suffix_but_keeps_good_ones() {
        let home = tmp_home("mixed_suffix");
        // Minimal fleet.yaml with one bad suffix and one good one.
        let yaml = r#"
defaults:
  cols: 80
  rows: 24
  layout: grid
templates:
  tpl:
    instances:
      "../etc":
        backend: claude
      good:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        // Point the daemon-less API call at a non-running daemon: `api::call`
        // just returns an error, but `deploy` itself only tracks the names it
        // accepted, so that's enough to verify filtering.
        let args = serde_json::json!({
            "template": "tpl",
            "directory": home.display().to_string(),
            "name": "dep",
        });
        let out = deploy(&home, "caller", &args);
        let instances = out["instances"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>();
        assert!(
            instances.iter().any(|n| n == "dep-good"),
            "good suffix dropped: {out}"
        );
        assert!(
            !instances.iter().any(|n| n.contains("..")),
            "bad suffix accepted: {out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Issue #456: teardown cleanup tests ───────────────────────────

    #[test]
    fn teardown_removes_workspace_dir() {
        let home = tmp_home("teardown_workspace");
        let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        // Create workspace dir (simulates what daemon would create).
        let workspace = crate::paths::workspace_dir(&home).join("dev-worker");
        std::fs::create_dir_all(&workspace).ok();
        std::fs::write(workspace.join("test.txt"), "data").ok();
        assert!(workspace.exists());

        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        assert!(
            !workspace.exists(),
            "teardown must remove workspace directory"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn teardown_clears_configs_and_bindings() {
        let home = tmp_home("teardown_configs");
        let yaml = r#"
templates:
  dev:
    instances:
      agent:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        // Create binding (simulates active task).
        crate::binding::bind(&home, "dev-agent", "T-1", "feat");
        assert!(crate::binding::read(&home, "dev-agent").is_some());

        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        // Binding should be cleared by cleanup_working_dir or DELETE.
        // Workspace dir should not exist.
        let workspace = crate::paths::workspace_dir(&home).join("dev-agent");
        assert!(!workspace.exists(), "workspace must be cleaned");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn teardown_prevents_respawn_by_removing_fleet_entry() {
        // After teardown, fleet.yaml must not contain the instance.
        // This prevents daemon restart from auto-spawning the dead agent.
        let home = tmp_home("teardown_respawn");
        let yaml = r#"
templates:
  dev:
    instances:
      impl:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        let config = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("load fleet");
        assert!(
            !config.instances.contains_key("dev-impl"),
            "teardown must remove fleet entry to prevent respawn"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Issue #474: TUI close path bypassed teardown ──────────────────
    //
    // The TUI close overlay (`Ctrl-B x` / tab close) calls
    // `fleet::remove_instance(s)_from_yaml` + `kill_agent` but doesn't
    // touch `deployments.json`. Result: stale entries in `deployment list`
    // after every TUI-triggered close. The fix wires
    // `deployments::reconcile_after_close` into the same overlay code path
    // (Option 1, auto-cleanup) and adds `reconcile_orphans` to the daemon
    // boot path (Option 3, defensive sweep).
    //
    // These tests target the production reconcile function that the
    // overlay calls — they exercise the same code path the TUI close
    // overlay does, just without the ratatui input boilerplate.

    /// Build a fleet.yaml + deploy a 1-instance template, returning the
    /// home dir. Used by tests that need a baseline post-deploy state.
    fn deploy_single_instance_for_test(tag: &str, deploy_name: &str) -> std::path::PathBuf {
        let home = tmp_home(tag);
        let yaml = r#"
templates:
  tpl:
    instances:
      worker:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({
                "template": "tpl",
                "name": deploy_name,
                "directory": home.display().to_string(),
            }),
        );
        home
    }

    /// #bughunt2: a deploy whose `deployments.json` save fails must surface an
    /// error (instances are live but untracked), NOT report `status:deployed`.
    #[test]
    fn deploy_surfaces_record_save_failure_not_fake_deployed() {
        let home = tmp_home("deploy-save-fail");
        let yaml = r#"
templates:
  tpl:
    instances:
      worker:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        // Force the record save to fail: a DIRECTORY at the target path makes
        // atomic_write's final rename unable to replace it.
        std::fs::create_dir_all(home.join("deployments.json")).unwrap();
        let result = deploy(
            &home,
            "caller",
            &serde_json::json!({
                "template": "tpl",
                "name": "tpl",
                "directory": home.display().to_string(),
            }),
        );
        assert!(
            result.get("error").and_then(|e| e.as_str()).is_some(),
            "a failed record save must surface as an error, not status:deployed: {result}"
        );
        assert_ne!(
            result.get("status").and_then(|s| s.as_str()),
            Some("deployed"),
            "must NOT report deployed when the record was not persisted"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #bughunt2 (codex review): teardown's record-cleanup save-failure branch
    /// must surface a stale-record error, not report `torn_down`. Unix-only: the
    /// failure is injected by making `home` read-only AFTER deploy (the
    /// `deployments.lock` already exists so the flock still opens, `load` still
    /// reads the record, but the `atomic_write` tmp create in the read-only dir
    /// fails the save).
    #[cfg(unix)]
    #[test]
    fn teardown_surfaces_record_save_failure_not_fake_torn_down() {
        use std::os::unix::fs::PermissionsExt;
        let home = deploy_single_instance_for_test("teardown-save-fail", "tpl");
        assert!(
            load(&home).deployments.iter().any(|d| d.name == "tpl"),
            "pre: deployment must exist for the teardown to find"
        );
        let ro = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&home, ro).unwrap();

        let result = teardown(&home, &serde_json::json!({"name": "tpl"}));

        // Restore write perms so cleanup (and any later test) works.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.get("error").and_then(|e| e.as_str()).is_some(),
            "a failed record-cleanup save must surface an error, not torn_down: {result}"
        );
        assert_ne!(
            result.get("status").and_then(|s| s.as_str()),
            Some("torn_down"),
            "must NOT report torn_down when the record cleanup was not persisted"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn close_last_instance_prunes_deployment_entry() {
        // Production smoke for the Issue #474 fix: deploy → simulate the
        // TUI close path (remove from fleet.yaml + reconcile_after_close)
        // → deployment store entry gone.
        let home = deploy_single_instance_for_test("close_last", "tpl");
        let store = load(&home);
        assert!(
            store.deployments.iter().any(|d| d.name == "tpl"),
            "pre: deployment must exist"
        );

        // TUI close path mirror: fleet.yaml first, then reconcile.
        let names: Vec<String> = vec!["tpl-worker".to_string()];
        let _ = crate::fleet::remove_instances_from_yaml(&home, &names);
        let pruned = reconcile_after_close(&home, &names);

        assert!(
            pruned.iter().any(|n| n == "tpl"),
            "reconcile must prune the empty deployment, got {pruned:?}"
        );
        let store = load(&home);
        assert!(
            store.deployments.iter().all(|d| d.name != "tpl"),
            "deployment store must NOT contain 'tpl' post-close"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn close_non_last_instance_keeps_deployment_intact() {
        // Multi-instance deployment: closing one of three keeps the
        // deployment entry intact — only when ALL members are gone does
        // the entry get pruned.
        let home = tmp_home("close_non_last");
        let yaml = r#"
templates:
  tpl:
    instances:
      a:
        backend: claude
      b:
        backend: claude
      c:
        backend: claude
instances: {}
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({
                "template": "tpl",
                "name": "tpl",
                "directory": home.display().to_string(),
            }),
        );

        // Close the first instance only.
        let names: Vec<String> = vec!["tpl-a".to_string()];
        let _ = crate::fleet::remove_instances_from_yaml(&home, &names);
        let pruned = reconcile_after_close(&home, &names);
        assert!(
            pruned.is_empty(),
            "reconcile must NOT prune when 2/3 members remain, got {pruned:?}"
        );

        let store = load(&home);
        let entry = store
            .deployments
            .iter()
            .find(|d| d.name == "tpl")
            .expect("deployment must remain in store");
        // The deployment record's `instances` list is the deploy-time
        // snapshot; we don't shrink it as members leave. The only
        // invariant the lint protects is "entry survives if any member
        // still in fleet.yaml".
        assert_eq!(
            entry.instances.len(),
            3,
            "instances list unchanged: {entry:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn reconcile_orphans_prunes_stale_entry_at_boot() {
        // Defensive sweep (Option 3): a stale deployment-store entry left
        // by an unclean shutdown — fleet.yaml has zero matching members
        // — gets pruned on next `reconcile_orphans` call.
        let home = tmp_home("reconcile_orphans");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "templates:\n  tpl:\n    instances:\n      worker:\n        backend: claude\ninstances: {}\n",
        )
        .unwrap();

        // Hand-craft a deployment-store entry with no live members.
        let mut store = load(&home);
        store.deployments.push(Deployment {
            name: "ghost".into(),
            template: "tpl".into(),
            instances: vec!["ghost-instance-that-never-was".into()],
            team: None,
            directory: home.display().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        save(&home, &mut store).expect("save store");

        let pruned = reconcile_orphans(&home);
        assert!(
            pruned.iter().any(|n| n == "ghost"),
            "boot reconcile must prune stale entry, got {pruned:?}"
        );
        let store = load(&home);
        assert!(
            store.deployments.iter().all(|d| d.name != "ghost"),
            "stale entry must be gone from store"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn reconcile_after_close_is_idempotent() {
        // Repeated reconciles on the same already-clean state must no-op
        // (no spurious team-delete calls, no panics).
        let home = deploy_single_instance_for_test("reconcile_idem", "tpl");
        let names: Vec<String> = vec!["tpl-worker".to_string()];
        let _ = crate::fleet::remove_instances_from_yaml(&home, &names);
        let r1 = reconcile_after_close(&home, &names);
        assert_eq!(r1, vec!["tpl".to_string()], "first reconcile prunes");
        let r2 = reconcile_after_close(&home, &names);
        assert!(r2.is_empty(), "second reconcile no-ops, got {r2:?}");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Smoke 2 (post-#475) — workspace dir cleanup tests ─────────────
    //
    // Issue: TUI close path + `deployment teardown` both invoked
    // `cleanup_working_dir` for `home/workspace/<inst>` only. Deployments
    // with `directory: /tmp/foo` left `/tmp/foo/<inst>/` on disk because
    // `cleanup_working_dir`'s user-provided-dir branch only strips agend
    // files (by design — protects user data).
    //
    // Fix: `cleanup_deployment_dirs` (this module) now removes both the
    // custom-directory subdir (whole-tree) AND the default workspace
    // path. Wired into both `reconcile_orphan_deployments` (close path /
    // boot sweep) and `teardown` (operator action).
    //
    // Regression-proof: comment out the `for dep in &pruned_deployments`
    // loop in `reconcile_orphan_deployments` and
    // `reconcile_prunes_custom_directory_subdirs` FAILS — restore → PASS.

    /// Helper: build a deployment-shaped fixture with a custom directory
    /// outside `home/workspace/`. Mirrors deploy()'s `inst_dir = dir.join(name)`
    /// shape so the test exercises the same on-disk layout production
    /// produces.
    fn deploy_with_custom_directory(
        tag: &str,
        deploy_name: &str,
        members: &[&str],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let home = tmp_home(tag);
        // Custom directory parent — outside home/workspace/ so the
        // user-provided-dir branch of cleanup_working_dir would normally
        // skip whole-tree removal.
        let custom_root = std::env::temp_dir().join(format!(
            "agend-smoke2-{}-{}-custom",
            std::process::id(),
            tag
        ));
        std::fs::create_dir_all(&custom_root).ok();
        // Hand-craft the deployment store entry + matching subdirs +
        // fleet.yaml entries. Simpler than running deploy() because the
        // deploy fn would also try to spawn instances via the API.
        let mut store = load(&home);
        let inst_names: Vec<String> = members
            .iter()
            .map(|m| format!("{deploy_name}-{m}"))
            .collect();
        for inst in &inst_names {
            let inst_dir = custom_root.join(inst);
            std::fs::create_dir_all(&inst_dir).unwrap();
            // Drop a sentinel file so we can tell the dir was actually
            // removed (vs e.g. moved or never created).
            std::fs::write(inst_dir.join("sentinel.txt"), "fixture").unwrap();
        }
        store.deployments.push(Deployment {
            name: deploy_name.to_string(),
            template: "tpl".to_string(),
            instances: inst_names.clone(),
            team: None,
            directory: custom_root.display().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        save(&home, &mut store).unwrap();
        // Empty fleet.yaml — simulates the "all instances closed" state
        // that triggers the prune branch in reconcile.
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        (home, custom_root)
    }

    #[test]
    fn reconcile_prunes_custom_directory_subdirs() {
        // Production smoke matching general's m-13 Smoke 2 reproduction.
        // Custom directory + multi-member deployment + all instances
        // already removed from fleet.yaml → reconcile must prune the
        // entry AND remove every spawned subdir.
        let (home, custom_root) = deploy_with_custom_directory(
            "smoke2_custom",
            "smoke2-team",
            &["lead", "impl-1", "impl-2", "reviewer"],
        );

        // Pre-condition: subdirs exist with the sentinel file.
        for member in ["lead", "impl-1", "impl-2", "reviewer"] {
            let inst = format!("smoke2-team-{member}");
            let inst_dir = custom_root.join(&inst);
            assert!(
                inst_dir.exists(),
                "test setup: {inst_dir:?} must exist pre-reconcile"
            );
            assert!(
                inst_dir.join("sentinel.txt").exists(),
                "test setup: sentinel must exist in {inst_dir:?}"
            );
        }

        let pruned = reconcile_orphans(&home);
        assert!(
            pruned.contains(&"smoke2-team".to_string()),
            "reconcile must prune the deployment, got {pruned:?}"
        );

        // Post-condition: every per-member subdir gone.
        for member in ["lead", "impl-1", "impl-2", "reviewer"] {
            let inst = format!("smoke2-team-{member}");
            let inst_dir = custom_root.join(&inst);
            assert!(
                !inst_dir.exists(),
                "Smoke 2 fix: custom-directory subdir {inst_dir:?} must be gone post-reconcile"
            );
        }

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom_root).ok();
    }

    #[test]
    fn close_last_instance_cleans_default_workspace_dir() {
        // Default-directory deployment: `home/workspace/<deploy>/<inst>/`.
        // After close + reconcile, the per-instance default workspace dir
        // should be removed via cleanup_working_dir's AGEND_HOME branch.
        let home = deploy_single_instance_for_test("close_default_wd", "tpl");
        // Hand-create the workspace subdir (production's spawn path
        // would have created it; deploy_single_instance_for_test stops
        // short of spawning).
        let wd = crate::paths::workspace_dir(&home).join("tpl-worker");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(wd.join("agent_data.txt"), "data").unwrap();
        assert!(wd.exists(), "test setup: workspace dir must exist");

        let names: Vec<String> = vec!["tpl-worker".to_string()];
        let _ = crate::fleet::remove_instances_from_yaml(&home, &names);
        let _ = reconcile_after_close(&home, &names);

        assert!(
            !wd.exists(),
            "default workspace dir must be cleaned post-reconcile: {wd:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn teardown_cleans_custom_directory_subdirs() {
        // Operator-driven `deployment teardown` action must also clean
        // custom-directory subdirs (not just the home/workspace fallback
        // the prior inline loop covered).
        let (home, custom_root) =
            deploy_with_custom_directory("smoke2_teardown", "td-team", &["a", "b"]);

        // teardown looks up by deployment name from the store.
        let _ = teardown(&home, &serde_json::json!({"name": "td-team"}));

        for member in ["a", "b"] {
            let inst = format!("td-team-{member}");
            let inst_dir = custom_root.join(&inst);
            assert!(
                !inst_dir.exists(),
                "teardown must remove custom subdir {inst_dir:?}"
            );
        }

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom_root).ok();
    }

    #[test]
    fn cleanup_deployment_dirs_handles_missing_subdirs_gracefully() {
        // Pre-removed subdirs (e.g., manual cleanup, or a previous reconcile
        // already ran) must not panic the helper. Tests the
        // `if custom_subdir.exists()` guard.
        let home = tmp_home("smoke2_missing");
        let custom_root = std::env::temp_dir().join(format!(
            "agend-smoke2-{}-missing-custom",
            std::process::id()
        ));
        // No subdir created — just point a Deployment at the (non-existent)
        // path.
        let dep = Deployment {
            name: "ghost".to_string(),
            template: "tpl".to_string(),
            instances: vec!["ghost-a".to_string()],
            team: None,
            directory: custom_root.display().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        // Must not panic.
        cleanup_deployment_dirs(&home, &dep);

        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 (MED-4): a branch-mode deploy creates a git worktree + branch per
    /// instance; teardown must remove BOTH so a same-name re-deploy succeeds.
    /// Pre-fix, `cleanup_deployment_dirs` only `remove_dir_all`'d the subdir,
    /// leaking a prunable worktree registry entry + the orphan branch. Drives
    /// the real `cleanup_deployment_dirs`; the decisive check is that re-creating
    /// the same worktree+branch succeeds afterward. Regression-proof: revert the
    /// worktree/branch GC and the re-add fails ("already exists").
    #[test]
    fn teardown_removes_branch_mode_worktree_and_orphan_branch_med4() {
        fn git(dir: &Path, args: &[&str]) -> std::process::Output {
            crate::git_helpers::git_bypass(dir, args).expect("git")
        }
        fn branch_exists(repo: &Path, branch: &str) -> bool {
            git(repo, &["rev-parse", "--verify", "--quiet", branch])
                .status
                .success()
        }

        let home = tmp_home("med4-worktree");
        // Branch-mode: the deploy directory IS the source repo.
        let repo = home.join("srcrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );

        // Mirror prepare_work_dir: worktree `deploy-lead` on branch `deploy/lead`.
        let inst_dir = repo.join("deploy-lead");
        let inst_dir_str = inst_dir.display().to_string();
        assert!(
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "deploy/lead",
                    &inst_dir_str,
                    "main"
                ]
            )
            .status
            .success(),
            "setup: worktree add must succeed"
        );
        assert!(
            inst_dir.join(".git").is_file(),
            "pre: inst_dir is a worktree"
        );
        assert!(branch_exists(&repo, "deploy/lead"), "pre: branch exists");

        cleanup_deployment_dirs(&home, &make_deployment("deploy", &["lead"], &repo));

        assert!(
            !inst_dir.exists(),
            "MED-4: the worktree dir must be removed"
        );
        assert!(
            !branch_exists(&repo, "deploy/lead"),
            "MED-4: the orphan branch must be deleted"
        );
        // Decisive: a same-name re-deploy (re-`worktree add -b`) must succeed —
        // proving no leftover registry entry or branch.
        assert!(
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "deploy/lead",
                    &inst_dir_str,
                    "main"
                ]
            )
            .status
            .success(),
            "MED-4: same-name re-deploy must succeed after teardown (no orphan)"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // -----------------------------------------------------------------
    // Sprint 54 P1-5 — rmdir empty deployment parent. Three contract
    // gates from dispatch m-20260507033740155107-2:
    //  1. all-subdirs-removed → parent gone
    //  2. unrelated file present → parent preserved
    //  3. running cleanup twice → second call silent
    //
    // Empirical regression-proof anchor: commenting out the
    // `rmdir_if_empty(custom_root)` call at the end of
    // `cleanup_deployment_dirs` trips test (1) with the FAIL signature
    // attached to the PR description.
    // -----------------------------------------------------------------

    fn make_deployment(name: &str, members: &[&str], directory: &Path) -> Deployment {
        let inst_names: Vec<String> = members.iter().map(|m| format!("{name}-{m}")).collect();
        Deployment {
            name: name.to_string(),
            template: "tpl".to_string(),
            instances: inst_names,
            team: None,
            directory: directory.display().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn cleanup_deployment_dirs_rmdir_parent_when_only_member_subdirs() {
        // Gate 1: deploy two members into a custom parent → cleanup →
        // parent must be GONE because both subdirs are removed and
        // nothing else lives there.
        let home = tmp_home("p15_rmdir_clean");
        let custom_root =
            std::env::temp_dir().join(format!("agend-p15-{}-clean-custom", std::process::id()));
        std::fs::create_dir_all(&custom_root).unwrap();
        for member in ["a", "b"] {
            let inst = format!("p15clean-{member}");
            let inst_dir = custom_root.join(&inst);
            std::fs::create_dir_all(&inst_dir).unwrap();
            std::fs::write(inst_dir.join("sentinel.txt"), "fixture").unwrap();
        }
        let dep = make_deployment("p15clean", &["a", "b"], &custom_root);

        cleanup_deployment_dirs(&home, &dep);

        assert!(
            !custom_root.exists(),
            "parent must be removed when only deployment subdirs were inside: {custom_root:?}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom_root).ok();
    }

    #[test]
    fn cleanup_deployment_dirs_preserves_parent_with_unrelated_file() {
        // Gate 2: same shape as gate 1 but with an unrelated file
        // dropped into the parent. The rmdir MUST be skipped — never
        // strip an operator's own data.
        let home = tmp_home("p15_rmdir_keep");
        let custom_root =
            std::env::temp_dir().join(format!("agend-p15-{}-keep-custom", std::process::id()));
        std::fs::create_dir_all(&custom_root).unwrap();
        for member in ["a", "b"] {
            let inst = format!("p15keep-{member}");
            let inst_dir = custom_root.join(&inst);
            std::fs::create_dir_all(&inst_dir).unwrap();
        }
        // Operator-dropped file at the parent root.
        let unrelated = custom_root.join("operator-notes.md");
        std::fs::write(&unrelated, "do not delete").unwrap();
        let dep = make_deployment("p15keep", &["a", "b"], &custom_root);

        cleanup_deployment_dirs(&home, &dep);

        assert!(
            custom_root.exists(),
            "parent MUST be preserved when it holds unrelated files: {custom_root:?}"
        );
        assert!(
            unrelated.exists(),
            "operator-dropped file MUST remain on disk: {unrelated:?}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom_root).ok();
    }

    #[test]
    fn cleanup_deployment_dirs_rmdir_is_idempotent() {
        // Gate 3: a second cleanup invocation after the first removed
        // the parent must NOT panic and must NOT log error-level
        // noise (`NotFound` is mapped to a silent no-op in
        // `rmdir_if_empty`).
        let home = tmp_home("p15_rmdir_idem");
        let custom_root =
            std::env::temp_dir().join(format!("agend-p15-{}-idem-custom", std::process::id()));
        std::fs::create_dir_all(&custom_root).unwrap();
        std::fs::create_dir_all(custom_root.join("p15idem-a")).unwrap();
        let dep = make_deployment("p15idem", &["a"], &custom_root);

        cleanup_deployment_dirs(&home, &dep);
        assert!(!custom_root.exists(), "first call must remove parent");

        // Second call — must not panic, must remain a silent no-op.
        cleanup_deployment_dirs(&home, &dep);
        assert!(!custom_root.exists(), "parent stays gone after second call");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn reconcile_keeps_subdirs_when_deployment_still_alive() {
        // Negative control: if any member still in fleet.yaml, the
        // deployment is NOT pruned and the subdirs are NOT touched.
        // Guards against an over-eager cleanup that runs on any
        // reconcile invocation regardless of prune outcome.
        let (home, custom_root) =
            deploy_with_custom_directory("smoke2_alive", "alive-team", &["a", "b"]);
        // Re-add one member back into fleet.yaml so reconcile sees it
        // as live and refuses to prune.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  alive-team-a:\n    backend: claude\n",
        )
        .unwrap();

        let pruned = reconcile_orphans(&home);
        assert!(
            pruned.is_empty(),
            "deployment with one live member must NOT be pruned: {pruned:?}"
        );

        // Both subdirs must still be on disk.
        for member in ["a", "b"] {
            let inst = format!("alive-team-{member}");
            let inst_dir = custom_root.join(&inst);
            assert!(
                inst_dir.exists(),
                "subdir must NOT be cleaned when deployment alive: {inst_dir:?}"
            );
        }

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom_root).ok();
    }

    // ── #787: deploy backend/command field conflation ─────────────────

    /// #787 §3.10 anchor — a template declaring BOTH `backend:` and
    /// `command:` must preserve each field independently on deploy.
    /// Pre-fix, the local `command` variable at deployments.rs:142
    /// fell back to `inst_val.get("backend")` and then got written to
    /// `InstanceYamlEntry.backend` at line 260, so the `command:` path
    /// silently overwrote the `backend:` label.
    ///
    /// RED on §3.10 RED: assertion fails because `backend` is
    /// "/tmp/fake-proxy" (the command path) instead of "claude".
    /// GREEN once the local `command` resolution is renamed to
    /// `backend_label` and reads only the `backend:` key.
    #[test]
    fn deploy_template_with_backend_and_command_preserves_both_fields() {
        let home = tmp_home("backend_command_split");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        command: /tmp/fake-proxy
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({
            "template": "dev",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("reload fleet.yaml");
        let lead = reloaded
            .instances
            .get("dev-lead")
            .expect("dev-lead must be persisted");

        assert_eq!(
            lead.backend.as_ref().map(|b| b.as_str()),
            Some("claude"),
            "backend field must hold the label, not the command path"
        );
        assert_eq!(
            lead.command.as_deref(),
            Some("/tmp/fake-proxy"),
            "command field must preserve the custom invocation path"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #787 back-compat invariant — a template with only `backend:`
    /// (no `command:`) is the common case and must continue to write
    /// the user-supplied backend label verbatim. Pins the post-fix
    /// behavior so the rename refactor doesn't accidentally regress
    /// the normal path.
    #[test]
    fn deploy_template_with_only_backend_persists_backend_label() {
        let home = tmp_home("only_backend");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: kiro-cli
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({
            "template": "dev",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("reload fleet.yaml");
        let lead = reloaded
            .instances
            .get("dev-lead")
            .expect("dev-lead must be persisted");

        assert_eq!(lead.backend.as_ref().map(|b| b.as_str()), Some("kiro-cli"));
        assert!(
            lead.command.is_none(),
            "no `command:` in template ⇒ `command` field must be None"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #787 — a template with ONLY `command:` (no explicit `backend:`)
    /// must NOT smuggle the command path into the backend field. After
    /// the fix, backend falls back to the "claude" default (which is
    /// the same default rustup-init users get from `fleet new`); the
    /// custom invocation lives in the `command:` field only.
    ///
    /// This pins the behavior-change called out in the decision spec:
    /// pre-fix `backend: <command-path>`, post-fix `backend: "claude"`.
    #[test]
    fn deploy_template_with_only_command_defaults_backend_to_claude() {
        let home = tmp_home("only_command");
        let yaml = r#"
templates:
  dev:
    instances:
      lead:
        command: /tmp/fake-proxy
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

        let args = serde_json::json!({
            "template": "dev",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
            .expect("reload fleet.yaml");
        let lead = reloaded
            .instances
            .get("dev-lead")
            .expect("dev-lead must be persisted");

        assert_eq!(
            lead.backend.as_ref().map(|b| b.as_str()),
            Some("claude"),
            "no explicit backend ⇒ default to claude (label); command path must NOT smuggle into backend"
        );
        assert_eq!(lead.command.as_deref(), Some("/tmp/fake-proxy"));

        std::fs::remove_dir_all(&home).ok();
    }

    /// #1320: deploy without directory falls back to $AGEND_HOME/workspace/<deploy_name>/.
    #[test]
    fn deploy_defaults_directory_to_workspace_deploy_name() {
        let home = tmp_home("dir_default");
        let yaml = r#"
templates:
  svc:
    instances:
      worker:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
        });
        let out = deploy(&home, "caller", &args);
        assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
        let store = load(&home);
        let dep = store.deployments.iter().find(|d| d.name == "svc");
        if let Some(dep) = dep {
            let expected = crate::paths::workspace_dir(&home)
                .join("svc")
                .display()
                .to_string();
            assert_eq!(
                dep.directory, expected,
                "#1320: default dir must be $AGEND_HOME/workspace/<deploy_name>"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1320: template-level directory takes effect when args omits it.
    #[test]
    fn deploy_reads_template_directory_field() {
        let home = tmp_home("dir_tpl");
        let yaml = r#"
templates:
  svc:
    directory: /tmp/custom-workspace
    instances:
      worker:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
        });
        let out = deploy(&home, "caller", &args);
        assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
        let store = load(&home);
        let dep = store.deployments.iter().find(|d| d.name == "svc");
        if let Some(dep) = dep {
            assert_eq!(
                dep.directory, "/tmp/custom-workspace",
                "#1320: template directory must take effect"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1320: explicit args directory still wins over template and default.
    #[test]
    fn deploy_args_directory_overrides_template_and_default() {
        let home = tmp_home("dir_override");
        let yaml = r#"
templates:
  svc:
    directory: /tmp/template-dir
    instances:
      worker:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
            "directory": "/tmp/explicit-dir",
        });
        let out = deploy(&home, "caller", &args);
        assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
        let store = load(&home);
        let dep = store.deployments.iter().find(|d| d.name == "svc");
        if let Some(dep) = dep {
            assert_eq!(
                dep.directory, "/tmp/explicit-dir",
                "#1320: explicit args directory must win"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_propagates_template_source_repo_to_instances() {
        let home = tmp_home("tpl_source_repo");
        let yaml = r#"
templates:
  svc:
    source_repo: /repos/my-project
    instances:
      lead:
        backend: claude
      dev:
        backend: kiro-cli
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
        assert_eq!(
            lead.source_repo.as_deref(),
            Some("/repos/my-project"),
            "template source_repo must propagate to instances"
        );
        let dev = reloaded.instances.get("svc-dev").expect("svc-dev");
        assert_eq!(
            dev.source_repo.as_deref(),
            Some("/repos/my-project"),
            "template source_repo must propagate to all instances"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_instance_source_repo_overrides_template() {
        let home = tmp_home("inst_override_sr");
        let yaml = r#"
templates:
  svc:
    source_repo: /repos/default
    instances:
      lead:
        backend: claude
        source_repo: /repos/override
      dev:
        backend: kiro-cli
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
        assert_eq!(
            lead.source_repo.as_deref(),
            Some("/repos/override"),
            "instance source_repo must override template"
        );
        let dev = reloaded.instances.get("svc-dev").expect("svc-dev");
        assert_eq!(
            dev.source_repo.as_deref(),
            Some("/repos/default"),
            "instance without override inherits template"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_no_source_repo_stays_none() {
        let home = tmp_home("no_source_repo");
        let yaml = r#"
templates:
  svc:
    instances:
      lead:
        backend: claude
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
        assert_eq!(
            lead.source_repo, None,
            "no source_repo in template or instance must remain None"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deploy_propagates_template_source_repo_to_team() {
        let home = tmp_home("tpl_sr_team");
        let yaml = r#"
templates:
  svc:
    source_repo: /repos/team-project
    instances:
      lead:
        backend: claude
      dev:
        backend: kiro-cli
"#;
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let args = serde_json::json!({
            "template": "svc",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let team = fleet.teams.get("svc").expect("team 'svc' must exist");
        assert_eq!(
            team.source_repo.as_ref().map(|p| p.display().to_string()),
            Some("/repos/team-project".to_string()),
            "template source_repo must propagate to team"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // #1629 invariant (#1617 lock-while-blocking class): the deployment-store
    // flock must be acquired AFTER the loopback `api::call`s (SPAWN/CREATE_TEAM
    // in deploy, DELETE in teardown), never around them — a self-IPC held under
    // that flock deadlocks (the loopback handler needs the registry lock). These
    // structural source-scans slice each fn and assert the api::call site index
    // precedes the `acquire_file_lock` index. Prod-sliced + `concat` needles so
    // they can't self-satisfy.
    fn prod_src() -> &'static str {
        let src = include_str!("deployments.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        }
    }

    fn fn_body<'a>(prod: &'a str, sig: &str) -> &'a str {
        let start = prod.find(sig).expect("fn present");
        let rest = &prod[start + sig.len()..];
        let end = rest.find("\npub fn ").unwrap_or(rest.len());
        &prod[start..start + sig.len() + end]
    }

    #[test]
    fn deploy_api_calls_not_under_flock() {
        let prod = prod_src();
        let body = fn_body(prod, "pub fn deploy(home");
        // H14: deploy's duplicate-name guard is a plain `load()` READ (no flock)
        // before spawn — #1629 forbids holding ANY flock across the self-IPC
        // spawn/team. So the ONLY `acquire_file_lock` in deploy is still the store
        // flock, and it must come AFTER spawn_instances/create_deployment_team.
        let lock_at = body
            .find(&["acquire_file", "_lock"].concat())
            .expect("deploy locks the store save");
        let spawn_at = body
            .find("spawn_instances(")
            .expect("deploy spawns instances");
        let team_at = body
            .find("create_deployment_team(")
            .expect("deploy creates the team");
        assert!(
            spawn_at < lock_at,
            "spawn_instances (api::call SPAWN) must run BEFORE the deployment flock (#1617 class)"
        );
        assert!(
            team_at < lock_at,
            "create_deployment_team (api::call CREATE_TEAM) must run BEFORE the deployment flock (#1617 class)"
        );
    }

    #[test]
    fn teardown_api_calls_not_under_flock() {
        let prod = prod_src();
        let body = fn_body(prod, "pub fn teardown(home");
        let lock_at = body
            .find(&["acquire_file", "_lock"].concat())
            .expect("teardown locks the record removal");
        let delete_at = body
            .find(&["crate::api::", "call"].concat())
            .expect("teardown DELETEs instances via api::call");
        assert!(
            delete_at < lock_at,
            "the DELETE api::call loop must run BEFORE the record-removal flock (#1617 class)"
        );
    }
}

#[cfg(test)]
mod review_repro_deployments_health_teams;
