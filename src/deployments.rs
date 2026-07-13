//! Deployment tracking — batch instance creation from fleet templates.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub name: String,
    pub template: String,
    pub instances: Vec<String>,
    pub team: Option<String>,
    pub directory: String,
    pub created_at: String,
    /// #2764 R3: durable per-deployment creation-provenance generation nonce.
    /// Minted at deploy time and embedded in each freshly-created custom subdir's
    /// `.agend-deploy-created` marker. Custom-subdir whole-tree removal is
    /// authorized ONLY when a subdir's marker nonce matches this record. `None`
    /// on a legacy record (pre-R3) → its custom subdirs fail closed (preserved).
    #[serde(default)]
    pub provenance_nonce: Option<String>,
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

type DeployEntries = (
    Vec<String>,
    Vec<(String, crate::fleet::InstanceYamlEntry)>,
    Vec<crate::agent::deleting::CreateAdmission>,
);

fn create_instance_entries(
    home: &std::path::Path,
    params: &DeployParams,
) -> Result<DeployEntries, String> {
    let mut created = Vec::new();
    let mut yaml_entries = Vec::new();
    let dir = std::path::PathBuf::from(&params.directory);

    // #2764 R7 (codex P0-1): admission pre-pass for EVERY member name BEFORE
    // any mutation (prepare_work_dir below creates directories). Any name
    // mid-delete aborts the WHOLE deploy zero-side-effect; the returned
    // guards are held by the caller through fleet persist + spawn.
    let mut admissions = Vec::new();
    for (name_val, _) in &params.instances_def {
        let Some(inst_suffix) = name_val.as_str() else {
            continue;
        };
        if crate::agent::validate_name(inst_suffix).is_err() {
            continue;
        }
        let inst_name = format!("{}-{inst_suffix}", params.deploy_name);
        if crate::agent::validate_name(&inst_name).is_err() {
            continue;
        }
        match crate::agent::deleting::admit_create(home, &inst_name) {
            Ok(g) => admissions.push(g),
            Err(reason) => return Err(format!("deploy refused: {reason}")),
        }
    }

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
    Ok((created, yaml_entries, admissions))
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

    // #2764 D: mint a per-deployment provenance generation nonce — the durable
    // generation anchor bound into the cleanup-pending/audit record written at
    // teardown (teardown itself performs NO destructive cleanup this PR).
    let provenance_nonce = uuid::Uuid::new_v4().to_string();
    // #2764 R7: `_admissions` guards are HELD through fleet persist + spawn —
    // a same-name delete refuses to start while a deploy create is in flight.
    let (created, yaml_entries, _admissions) = match create_instance_entries(home, &params) {
        Ok(t) => t,
        Err(e) => return serde_json::json!({"error": e}),
    };

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
        provenance_nonce: Some(provenance_nonce),
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

/// #2764 D (decision d-20260713091213053694-25): deployment teardown performs
/// ZERO destructive or authority mutation this PR — no member DELETE, no
/// fleet/team/store/path/git mutation, no record pruning. Proving safe removal
/// of a deployment's (possibly custom, possibly operator-shared) directories
/// was found not implementable within this PR's safety bar, so teardown is an
/// EMBARGO: it only writes a durable, generation-bound, idempotent
/// cleanup-pending/audit record and reports that ownership-safe teardown is
/// temporarily unavailable. All authoritative state (deployment record, fleet
/// entries, team, instances, directories, provenance) remains intact for a
/// future generation-claimed teardown.
pub fn teardown(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let deployment = match load(home).deployments.iter().find(|d| d.name == name) {
        Some(d) => d.clone(),
        None => return serde_json::json!({"error": format!("deployment '{name}' not found")}),
    };

    // The ONLY permitted write: a durable, idempotent pending/audit record. A
    // write failure is LOUD but still mutates no authoritative state.
    match record_cleanup_pending(home, &deployment, "teardown") {
        Ok(()) => serde_json::json!({
            "name": name,
            "instances": deployment.instances,
            "torn_down": false,
            "cleanup_pending": true,
            "reason": "ownership_safe_teardown_temporarily_unavailable",
            "note": "Deployment teardown is embargoed pending a generation-claimed safe-ownership implementation (#2764). Recorded to deploy-cleanup-pending.jsonl; no instances, fleet entries, directories, or records were mutated.",
        }),
        Err(e) => serde_json::json!({
            "error": format!(
                "deployment '{name}' teardown is embargoed and the cleanup-pending record write FAILED: {e} — no authoritative state was mutated"
            ),
            "name": name,
            "torn_down": false,
            "cleanup_pending": false,
        }),
    }
}

pub fn list(home: &Path) -> Value {
    let store = load(home);
    serde_json::json!({"deployments": store.deployments})
}

fn cleanup_pending_path(home: &Path) -> PathBuf {
    home.join("deploy-cleanup-pending.jsonl")
}

/// #2764 D: the durable, generation-bound, IDEMPOTENT cleanup-pending/audit
/// ledger — the ONLY write teardown/reconcile perform. The identity binds the
/// deployment name, its instances, directory, the provenance generation nonce,
/// and the source (`teardown`/`reconcile`); the dedup key
/// `<name>@<provenance_nonce>` makes repeated writes for the SAME generation a
/// no-op (so boot-sweep reconciles never accumulate duplicates). Returns `Err`
/// when the record could not be durably persisted so the caller surfaces the
/// failure LOUDLY rather than claiming it was recorded — and never mutates any
/// authoritative state either way.
fn record_cleanup_pending(
    home: &Path,
    deployment: &Deployment,
    source: &str,
) -> Result<(), String> {
    let path = cleanup_pending_path(home);
    let nonce = deployment.provenance_nonce.clone().unwrap_or_default();
    let dedup_key = format!("{}@{}", deployment.name, nonce);
    // Serialize under a dedicated flock so concurrent reconcile / teardown
    // appends don't interleave-corrupt the ledger.
    let lock_path = store_path(home).with_extension("pending.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)
        .map_err(|e| format!("cleanup-pending lock failed: {e}"))?;
    // Idempotency: skip if this exact generation dedup key is already present.
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let already = existing.lines().any(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .map(|v| v["dedup_key"] == dedup_key)
                .unwrap_or(false)
        });
        if already {
            return Ok(());
        }
    }
    let line = serde_json::json!({
        "dedup_key": dedup_key,
        "deployment": deployment.name,
        "instances": deployment.instances,
        "directory": deployment.directory,
        "provenance_nonce": nonce,
        "source": source,
        "reason": "ownership_safe_teardown_temporarily_unavailable",
    })
    .to_string();
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    body.push_str(&line);
    body.push('\n');
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("cleanup-pending ledger write failed: {e}"))
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

    let store = load(home);
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

    // Orphan = a deployment with NO live instance in fleet.yaml.
    let orphans: Vec<&Deployment> = store
        .deployments
        .iter()
        .filter(|d| !d.instances.iter().any(|i| live_instances.contains(i)))
        .collect();

    // #2764 D: orphan reconcile performs ZERO destructive/authority mutation —
    // no record pruning, no team delete, no path/git cleanup. Its ONLY action is
    // to durably (idempotently) record each orphan to the cleanup-pending/audit
    // ledger for a future generation-claimed teardown, then return NO pruned
    // names (nothing was removed). The deployment/team/fleet/directory state all
    // remains intact.
    for dep in &orphans {
        if let Err(e) = record_cleanup_pending(home, dep, "reconcile") {
            tracing::warn!(
                deployment = %dep.name,
                error = %e,
                "#2764 deployments reconcile: cleanup-pending record write FAILED (state left intact)"
            );
        }
    }
    Vec::new()
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
