//! Deployment tracking — batch instance creation from fleet templates.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub name: String,
    pub template: String,
    pub instances: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_instances: Vec<String>,
    pub team: Option<String>,
    pub directory: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeploymentStore {
    #[serde(default)]
    schema_version: u32,
    deployments: Vec<Deployment>,
}

/// Runtime-owned state forwarded by the in-process MCP adapter.  The
/// deployments owner deliberately knows only these neutral registries and the
/// lifecycle notifier; MCP transport context stays at the adapter boundary.
pub(crate) struct DeploymentRuntime<'a> {
    pub registry: &'a crate::agent::AgentRegistry,
    pub configs: &'a crate::api::ConfigRegistry,
    pub externals: &'a crate::agent::ExternalRegistry,
    pub notifier: Option<&'a std::sync::Arc<dyn crate::api::ApiNotifier>>,
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
    // #3624 症狀 2：前後 dash 的 deploy name 會拼出雙 dash 實例名
    // （`eo-team-` + `lead` → `eo-team--lead`），讓 create_deployment_team
    // 的 orchestrator 匹配失敗、team 無主。`validate_name` 本身允許 dash，
    // 這裡額外拒絕前後 dash（早於任何 side-effect）。
    if deploy_name.starts_with('-') || deploy_name.ends_with('-') {
        return Err(serde_json::json!({"error": format!(
            "invalid deploy name '{deploy_name}': leading/trailing dash is not allowed (it produces double-dash instance names)"
        )}));
    }

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

fn validate_context_threshold(
    inst_val: &serde_yaml_ng::Value,
    field: &str,
) -> Result<Option<f32>, String> {
    let Some(val) = inst_val.get(field) else {
        return Ok(None);
    };
    let f64_val = val
        .as_f64()
        .ok_or_else(|| format!("`{field}` must be a YAML number, got: {val:?}"))?;
    if !f64_val.is_finite() {
        return Err(format!("`{field}` must be finite, got: {f64_val}"));
    }
    let f32_val = f64_val as f32;
    if !f32_val.is_finite() {
        return Err(format!("`{field}` overflows f32 precision: {f64_val}"));
    }
    Ok(Some(f32_val))
}

#[allow(clippy::type_complexity)]
fn create_instance_entries(
    params: &DeployParams,
    generation_id: &str,
) -> Result<(Vec<String>, Vec<(String, crate::fleet::InstanceYamlEntry)>), serde_json::Value> {
    for (name_val, inst_val) in &params.instances_def {
        let inst_suffix = name_val.as_str().unwrap_or("?");
        let inst_name = format!("{}-{inst_suffix}", params.deploy_name);
        for field in [
            "context_alert_pct",
            "context_handoff_pct",
            "context_handoff_escalate_pct",
        ] {
            validate_context_threshold(inst_val, field).map_err(|e| {
                serde_json::json!({
                    "error": format!("deploy_template: instance `{inst_name}` — {e}"),
                    "code": "deploy_invalid_threshold",
                })
            })?;
        }
    }

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
        // Sprint 61 follow-up: per-instance skills allowlist from a template stanza.
        // Unlike `args` extraction this does NOT `.filter(|v| !v.is_empty())` —
        // `skills: []` (explicit opt-out of all skills) is a meaningful value per
        // `InstanceConfig::skills` docstring, and filtering it would silently flip
        // opt-out → install-all. `None` (field absent) means install every skill.
        //
        // Presence-with-invalid-type must fail closed: a non-sequence value
        // (e.g. scalar `skills: code-review`) or a sequence with non-string
        // members (e.g. `skills: [42]`) is rejected at deployment time rather
        // than silently resolving to `None` or a partial list. Only absent
        // (None), empty `[]`, and valid string sequences pass through.
        let template_skills = match inst_val.get("skills") {
            None => None,
            Some(skills_val) => {
                let seq = match skills_val.as_sequence() {
                    Some(s) => s,
                    None => {
                        tracing::warn!(
                            deploy_name = %params.deploy_name,
                            inst = %inst_name,
                            "skipping template instance: `skills` must be a YAML sequence (list)"
                        );
                        continue;
                    }
                };
                let mut skills = Vec::with_capacity(seq.len());
                let mut invalid = false;
                for x in seq {
                    match x.as_str() {
                        Some(s) => skills.push(s.to_string()),
                        None => {
                            tracing::warn!(
                                deploy_name = %params.deploy_name,
                                inst = %inst_name,
                                value = ?x,
                                "skipping template instance: every `skills` entry must be a string"
                            );
                            invalid = true;
                            break;
                        }
                    }
                }
                if invalid {
                    continue;
                }
                Some(skills)
            }
        };
        let source_repo = inst_val
            .get("source_repo")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or(params.template_source_repo.clone());

        let inst_dir = dir.join(&inst_name);
        // Keep entry construction side-effect free. Fleet admission below is
        // the authority for workspace identity; materialize only after it has
        // accepted every intended working directory.
        let work_dir = inst_dir.display().to_string();

        yaml_entries.push((
            inst_name.clone(),
            crate::fleet::InstanceYamlEntry {
                backend: Some(backend_label.to_string()),
                working_directory: Some(work_dir),
                role,
                instructions: yaml_str(inst_val, "instructions"),
                source_repo,
                skills_path: yaml_str(inst_val, "skills_path"),
                skills: template_skills,
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
                effort: yaml_str(inst_val, "effort"),
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
                deployment_generation: Some(generation_id.to_string()),
                context_alert_pct: validate_context_threshold(inst_val, "context_alert_pct")
                    .map_err(|e| {
                        serde_json::json!({
                            "error": format!("deploy_template: instance `{inst_name}` — {e}"),
                            "code": "deploy_invalid_threshold",
                        })
                    })?,
                context_handoff_pct: validate_context_threshold(inst_val, "context_handoff_pct")
                    .map_err(|e| {
                        serde_json::json!({
                            "error": format!("deploy_template: instance `{inst_name}` — {e}"),
                            "code": "deploy_invalid_threshold",
                        })
                    })?,
                context_handoff_escalate_pct: validate_context_threshold(
                    inst_val,
                    "context_handoff_escalate_pct",
                )
                .map_err(|e| {
                    serde_json::json!({
                        "error": format!("deploy_template: instance `{inst_name}` — {e}"),
                        "code": "deploy_invalid_threshold",
                    })
                })?,
            },
        ));
        created.push(inst_name);
    }
    Ok((created, yaml_entries))
}

enum WorkDirPreparation {
    Ready,
    Failed { residual: Option<String> },
}

fn prepare_work_dir(
    inst_dir: &std::path::Path,
    parent_dir: &std::path::Path,
    deploy_name: &str,
    generation_id: &str,
    inst_suffix: &str,
    inst_name: &str,
    branch: Option<&str>,
) -> WorkDirPreparation {
    let absent_before_create = matches!(
        std::fs::symlink_metadata(inst_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    if !absent_before_create {
        tracing::warn!(%inst_name, path = %inst_dir.display(), "deployment workdir already exists; preserving unowned path");
        return WorkDirPreparation::Failed {
            residual: Some(inst_dir.display().to_string()),
        };
    }
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
                if write_deployment_owner_marker(
                    inst_dir,
                    deploy_name,
                    inst_name,
                    Some(generation_id),
                ) {
                    return WorkDirPreparation::Ready;
                }
                return WorkDirPreparation::Failed {
                    residual: Some(failed_workdir_residual(inst_dir, Some(&branch_name))),
                };
            }
            Err(crate::git_helpers::GitError::NonZero { stderr, .. }) => {
                tracing::warn!(%inst_name, error = %stderr, "worktree failed");
            }
            Err(crate::git_helpers::GitError::Spawn(e)) => {
                tracing::warn!(error = %e, "git not available");
            }
        }
    } else {
        if let Some(parent) = inst_dir.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                tracing::warn!(%inst_name, %error, "deployment workdir parent creation failed");
                return WorkDirPreparation::Failed { residual: None };
            }
        }
        if let Err(error) = std::fs::create_dir(inst_dir) {
            tracing::warn!(%inst_name, %error, "deployment workdir creation failed");
            return WorkDirPreparation::Failed { residual: None };
        }
        if write_deployment_owner_marker(inst_dir, deploy_name, inst_name, Some(generation_id)) {
            return WorkDirPreparation::Ready;
        }
        return WorkDirPreparation::Failed {
            residual: Some(failed_workdir_residual(inst_dir, None)),
        };
    }
    WorkDirPreparation::Failed { residual: None }
}

fn failed_workdir_residual(inst_dir: &Path, branch_name: Option<&str>) -> String {
    if cfg!(test) {
        run_before_failed_workdir_residual_test_hook();
    }
    let path = inst_dir.display().to_string();
    if let Some(branch_name) = branch_name {
        tracing::error!(%path, %branch_name, "deployment owner marker failed; preserving worktree and branch residual");
        format!("{path} (branch {branch_name}; inspect/remove worktree and branch)")
    } else {
        tracing::error!(%path, "deployment owner marker failed; preserving directory residual");
        path
    }
}

const DEPLOYMENT_OWNER_MARKER: &str = ".agend-deployment-owner";

fn deployment_owner_marker_contents(
    directory: &Path,
    deploy_name: &str,
    instance: &str,
    generation_id: Option<&str>,
) -> Option<Vec<u8>> {
    let canonical = crate::paths::canonical_workspace_path(directory).ok()?;
    let mut marker = serde_json::json!({
        "schema_version": if generation_id.is_some() { 2 } else { 1 },
        "deployment": deploy_name,
        "instance": instance,
        "directory": canonical,
    });
    if let Some(generation_id) = generation_id {
        marker["generation_id"] = serde_json::Value::String(generation_id.to_string());
    }
    serde_json::to_vec(&marker).ok()
}

fn write_deployment_owner_marker(
    directory: &Path,
    deploy_name: &str,
    instance: &str,
    generation_id: Option<&str>,
) -> bool {
    use std::io::Write;
    if cfg!(test) && fail_deployment_owner_marker_test_hook() {
        return false;
    }
    let Some(contents) =
        deployment_owner_marker_contents(directory, deploy_name, instance, generation_id)
    else {
        return false;
    };
    let marker = directory.join(DEPLOYMENT_OWNER_MARKER);
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .and_then(|mut file| file.write_all(&contents));
    if let Err(error) = result {
        tracing::warn!(path = %marker.display(), %error, "deployment ownership marker creation failed; cleanup will preserve path");
        return false;
    }
    true
}

fn deployment_owner_marker_matches(
    directory: &Path,
    deploy_name: &str,
    instance: &str,
    generation_id: Option<&str>,
) -> bool {
    let Some(expected) =
        deployment_owner_marker_contents(directory, deploy_name, instance, generation_id)
    else {
        return false;
    };
    match std::fs::read(directory.join(DEPLOYMENT_OWNER_MARKER)) {
        Ok(actual) => actual == expected,
        Err(_) => false,
    }
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
    crate::fleet::insert_new_instances_to_yaml(home, &refs).map_err(|e| {
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
    runtime: Option<&DeploymentRuntime<'_>>,
) {
    if let Some(runtime) = runtime {
        for (inst_name, entry) in yaml_entries {
            let args = entry.args.as_ref().map(|values| values.join(" "));
            let work_dir = entry.working_directory.as_deref().unwrap_or(directory);
            let working_directory = std::path::Path::new(work_dir);
            let backend = entry.command.as_deref().or(entry.backend.as_deref());
            let params = crate::agent_ops::spawn::SpawnParams {
                name: inst_name,
                backend,
                args: args.as_deref(),
                model: entry.model.as_deref(),
                model_tier: entry.model_tier.as_deref(),
                working_directory: Some(working_directory),
                env: entry.env.as_ref(),
                mode: crate::backend::SpawnMode::Fresh,
                explicit_role: entry.role.as_deref(),
                self_kick_on_ready: false,
                topic_binding: entry.topic_binding_mode.as_deref().unwrap_or("auto"),
                layout: "tab",
                spawner: None,
                target_pane: None,
                restart_id: None,
                old_instance_ref: None,
            };
            let request = match crate::agent_ops::spawn::resolve_spawn_request(home, &params) {
                Ok(request) => request,
                Err(error) => {
                    tracing::error!(instance = %inst_name, error = %error, "deploy: Phase 3 spawn refused");
                    continue;
                }
            };
            let context = crate::agent_ops::spawn::SpawnContext {
                home,
                registry: runtime.registry,
                configs: runtime.configs,
                externals: runtime.externals,
                notifier: runtime.notifier,
            };
            if let Err(error) = crate::agent_ops::spawn::spawn_instance(&context, &request) {
                tracing::error!(instance = %inst_name, error = %error, "deploy: Phase 3 spawn failed");
            }
        }
        return;
    }

    spawn_instances_legacy(home, yaml_entries, directory);
}

fn spawn_instances_legacy(
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
    runtime: Option<&DeploymentRuntime<'_>>,
) -> bool {
    if created.len() <= 1 {
        return false;
    }
    let description = format!("Template deployment: {template}");
    // #3327: team-level `project_id` override (#2509) was unreachable from a
    // template deploy — this used to hardcode None, so teams whose clone path
    // mis-slugs lost their board override on every redeploy (teardown deletes
    // the whole team entry). Read the optional field from the same
    // template_def the sibling `orchestrator` parse uses.
    let template_project_id = yaml_str(template_def, "project_id");
    let orchestrator = template_def
        .get("orchestrator")
        .and_then(|value| value.as_str())
        .and_then(|suffix| {
            let full = format!("{deploy_name}-{suffix}");
            if created.contains(&full) {
                Some(full)
            } else {
                tracing::warn!(
                    template,
                    suffix,
                    "template orchestrator not among spawned instances; team created without one"
                );
                None
            }
        });
    if let Some(runtime) = runtime {
        let request = crate::team_ops::CreateTeamRequest {
            name: deploy_name.to_string(),
            per_member_backends: Vec::new(),
            existing_members: created.to_vec(),
            topic_binding_mode: None,
            orchestrator,
            description: Some(description),
            repository_path: template_source_repo.clone(),
            project_id: template_project_id,
            accept_from: Vec::new(),
        };
        let result = crate::team_ops::create(
            home,
            request,
            runtime.registry,
            runtime.configs,
            runtime.notifier.map(|notifier| notifier.as_ref()),
        );
        return result.get("ok").and_then(Value::as_bool) == Some(true);
    }

    create_deployment_team_legacy(
        home,
        deploy_name,
        template_source_repo,
        template_project_id.as_ref(),
        created,
        &description,
        orchestrator.as_deref(),
    )
}

fn create_deployment_team_legacy(
    home: &Path,
    deploy_name: &str,
    template_source_repo: &Option<String>,
    template_project_id: Option<&String>,
    created: &[String],
    description: &str,
    orchestrator: Option<&str>,
) -> bool {
    let mut team_args = serde_json::json!({
        "name": deploy_name,
        "members": created,
        "description": description,
    });
    if let Some(ref sr) = template_source_repo {
        team_args["repository_path"] = serde_json::Value::String(sr.clone());
    }
    // #3327: the legacy fallback must carry the field too — teams::create
    // reads `project_id` from raw args (#2509), so both creation paths
    // land the same override.
    if let Some(pid) = template_project_id {
        team_args["project_id"] = serde_json::Value::String(pid.clone());
    }
    if let Some(orchestrator) = orchestrator {
        team_args["orchestrator"] = serde_json::Value::String(orchestrator.to_string());
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

#[allow(dead_code)]
pub fn deploy(home: &Path, instance_name: &str, args: &Value) -> Value {
    deploy_with_runtime(home, instance_name, args, None)
}

pub(crate) fn deploy_with_runtime(
    home: &Path,
    instance_name: &str,
    args: &Value,
    runtime: Option<&DeploymentRuntime<'_>>,
) -> Value {
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

    let generation_id = uuid::Uuid::new_v4().to_string();
    let (created, yaml_entries) = match create_instance_entries(&params, &generation_id) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if let Err(e) =
        persist_to_fleet_yaml(home, &yaml_entries, &params.template, &params.deploy_name)
    {
        return e;
    }

    let directory = std::path::PathBuf::from(&params.directory);
    let mut materialized = Vec::with_capacity(yaml_entries.len());
    for (inst_name, _) in &yaml_entries {
        let suffix = inst_name
            .strip_prefix(&format!("{}-", params.deploy_name))
            .unwrap_or(inst_name);
        let inst_dir = directory.join(inst_name);
        let preparation = prepare_work_dir(
            &inst_dir,
            &directory,
            &params.deploy_name,
            &generation_id,
            suffix,
            inst_name,
            params.branch.as_deref(),
        );
        if let WorkDirPreparation::Failed { residual } = preparation {
            let cleanup = Deployment {
                name: params.deploy_name.clone(),
                template: params.template.clone(),
                instances: materialized,
                cleanup_instances: Vec::new(),
                team: None,
                directory: params.directory.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                generation_id: Some(generation_id.clone()),
            };
            let mut residuals = residual.clone().into_iter().collect::<Vec<_>>();
            let rollback_entries = created
                .iter()
                .map(|name| (name.as_str(), generation_id.as_str()))
                .collect::<Vec<_>>();
            match crate::fleet::remove_instances_from_yaml_for_generation(home, &rollback_entries) {
                Ok(preserved) => residuals.extend(preserved.into_iter().map(|name| {
                    format!(
                        "preserved newer fleet generation for '{name}' during deployment rollback"
                    )
                })),
                Err(error) => residuals.push(format!(
                    "fleet rollback failed for deployment '{}': {error}",
                    params.deploy_name
                )),
            }
            residuals.extend(cleanup_deployment_dirs_impl(home, &cleanup, false));
            return serde_json::json!({
                "error": format!("deployment '{}' could not safely materialize working directory for '{inst_name}'", params.deploy_name),
                "code": "deploy_workdir_materialization_failed",
                "residual": residual,
                "residuals": residuals,
            });
        }
        materialized.push(inst_name.clone());
    }

    // #3624 症狀 1：先 CREATE_TEAM（members 用 entries 建好的預期名單）
    // 後 spawn。舊順序 spawn → team 讓 TUI roster sync 在 team 建好前的
    // tick 把先出現的成員歸進 standalone tab（實測 lead 落單）。team 建在
    // persist 之後（persist 失敗不留孤 team）、spawn 之前；spawn 失敗的
    // 成員由既有 stale-member 機制承接（#785）。CREATE_TEAM 與 SPAWN 同為
    // self-IPC，仍在下方 store flock 之外（#1617/#1629 不變）。
    let team_created = create_deployment_team(
        home,
        &params.deploy_name,
        &params.template,
        &params.template_def,
        &params.template_source_repo,
        &created,
        runtime,
    );

    spawn_instances(home, &yaml_entries, &params.directory, runtime);

    let deployment = Deployment {
        name: params.deploy_name.to_string(),
        template: params.template.to_string(),
        instances: created.clone(),
        cleanup_instances: Vec::new(),
        team: if team_created {
            Some(params.deploy_name.to_string())
        } else {
            None
        },
        directory: params.directory,
        created_at: chrono::Utc::now().to_rfc3339(),
        generation_id: Some(generation_id),
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

#[allow(dead_code)]
pub fn teardown(home: &Path, args: &Value) -> Value {
    teardown_with_runtime(home, args, None)
}

pub(crate) fn teardown_with_runtime(
    home: &Path,
    args: &Value,
    runtime: Option<&DeploymentRuntime<'_>>,
) -> Value {
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
    // #3505 P0(a): the runtime path names refused deletes instead of
    // reporting a silent clean `torn_down`. `delete_instance` alone cannot
    // observe the child-exit refusal (the instance stays alive, retained
    // for retry) — `delete_instance_with_exit_status` exposes it, and the
    // refused names are returned as `residuals` for an actionable retry.
    // The legacy (runtime=None) transport path cannot observe refusal, so
    // its contract is unchanged.
    let mut residuals: Vec<String> = Vec::new();
    let mut preserved_generations: Vec<String> = Vec::new();
    let remove_legacy_fleet_rows =
        runtime.is_none() && crate::daemon::find_active_run_dir(home).is_none();
    if let Some(runtime) = runtime {
        let delete_context = crate::agent_ops::DeleteContext {
            registry: runtime.registry,
            configs: runtime.configs,
            externals: runtime.externals,
            notifier: runtime.notifier,
        };
        for inst in &deployment.instances {
            let mut fleet_remove_error = None;
            let generation = deployment.generation_id.as_deref();
            let delete_result =
                crate::agent_ops::delete_instance_with_exit_status_and_post_for_deployment_generation(
                    home,
                    inst,
                    &delete_context,
                    false,
                    generation,
                    |observed_exit| {
                        if observed_exit {
                            if let Some(generation) = generation {
                                match crate::fleet::remove_instances_from_yaml_for_generation(
                                    home,
                                    &[(inst.as_str(), generation)],
                                ) {
                                    Ok(preserved) if preserved.is_empty() => {}
                                    Ok(_) => {
                                        fleet_remove_error = Some(
                                            "a newer fleet generation was preserved".into(),
                                        );
                                    }
                                    Err(error) => {
                                        fleet_remove_error = Some(error.to_string())
                                    }
                                }
                            }
                        }
                    },
                );
            let Some((_outcome, observed_exit)) = delete_result else {
                preserved_generations.push(inst.clone());
                residuals.push(inst.clone());
                continue;
            };
            if let Some(error) = fleet_remove_error {
                tracing::warn!(instance = %inst, %error, "failed to remove deleted fleet row under delete fence");
                residuals.push(inst.clone());
            } else if !observed_exit {
                residuals.push(inst.clone());
            }
        }
        if cfg!(test) {
            run_after_runtime_instance_deletes_test_hook();
        }
    } else {
        delete_instances_legacy(home, &deployment.instances);
    }

    // #3505 P0(a): partial-teardown partition. Refused instances stay fully
    // intact (registry + fleet.yaml + team + store record) so a retry stays
    // possible; only fully-deleted instances proceed to file cleanup.
    let deleted: Vec<String> = deployment
        .instances
        .iter()
        .filter(|i| !residuals.contains(i))
        .cloned()
        .collect();
    // Managed runtime deletes removed each confirmed-deleted row while its
    // DeleteFence remained held, so a same-name generation cannot be erased
    // by a later name-only batch mutation. Offline cleanup has no active
    // spawner; it retains the legacy batch removal. An active legacy daemon's
    // DELETE path owns row removal under its lifecycle fence.
    if remove_legacy_fleet_rows {
        if let Err(e) = crate::fleet::remove_instances_from_yaml(home, &deleted) {
            tracing::warn!(error = %e, "failed to clean up fleet.yaml on teardown");
        }
    }

    // Smoke 2 fix: filesystem cleanup of every spawned subdir, including
    // custom-`directory` deployments that the prior inline
    // `home/workspace/<inst>` loop missed.
    // #3505: only clean what was actually deleted — a refused instance is
    // still alive and its workspace must survive for the retry.
    let cleanup_instances = deployment
        .cleanup_instances
        .iter()
        .chain(deleted.iter())
        .cloned()
        .collect::<Vec<_>>();
    let cleanup_deployment = Deployment {
        instances: cleanup_instances.clone(),
        ..deployment.clone()
    };
    let cleanup_residuals = cleanup_deployment_dirs_impl(home, &cleanup_deployment, true);

    // Delete team if exists — but only on FULL teardown. A partial teardown
    // still has live residual members; deleting their team would strand them.
    if residuals.is_empty() {
        if let Some(ref team) = deployment.team {
            let _ = crate::teams::delete(home, &serde_json::json!({"name": team}));
        }
    }

    // #1629 (C1 lost-update): flock ONLY the store record-removal load-modify-save.
    // Re-load under the flock so a concurrent deploy/teardown isn't lost-updated.
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => return serde_json::json!({"error": format!("deployment lock failed: {e}")}),
    };
    let mut store = load(home);
    // #3505: partial teardown NARROWS the record to the residuals (retry
    // stays possible via the same name); full teardown removes it.
    if residuals.is_empty() && cleanup_residuals.is_empty() {
        // Remove from store
        store.deployments.retain(|d| d.name != name);
    } else {
        for d in store.deployments.iter_mut().filter(|d| d.name == name) {
            d.instances = residuals.clone();
            d.cleanup_instances = if cleanup_residuals.is_empty() {
                Vec::new()
            } else {
                cleanup_instances.clone()
            };
        }
    }
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

    // #3505 P0(a): partial teardown surfaces the refused names instead of
    // a silent clean `torn_down` — the operator can retry the same name
    // once the residual exits. Full teardown keeps the exact prior shape.
    if residuals.is_empty() && cleanup_residuals.is_empty() {
        let mut result = serde_json::json!({"status": "torn_down", "name": name, "instances": deployment.instances});
        if !preserved_generations.is_empty() {
            result["preserved_generations"] = serde_json::json!(preserved_generations);
        }
        result
    } else {
        let mut result = serde_json::json!({
            "status": "torn_down_partial",
            "name": name,
            "instances": deployment.instances,
            "deleted": deleted,
            "residuals": residuals,
            "cleanup_residuals": cleanup_residuals,
            "hint": format!(
                "{}; retry `teardown name={name}` after addressing the residual",
                if residuals.is_empty() {
                    "workspace cleanup left residuals"
                } else {
                    "teardown refused for live instances; their registry, port, and workspace are retained"
                },
            ),
        });
        if !preserved_generations.is_empty() {
            result["preserved_generations"] = serde_json::json!(preserved_generations);
        }
        result
    }
}

fn delete_instances_legacy(home: &Path, instances: &[String]) {
    for inst in instances {
        let _ = crate::api::call(
            home,
            &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": inst}}),
        );
    }
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
/// All filesystem ops are best-effort; failures are returned as actionable
/// residuals, and a single per-instance failure doesn't abort the sweep.
#[cfg(test)]
fn cleanup_deployment_dirs(home: &Path, deployment: &Deployment) {
    for residual in cleanup_deployment_dirs_impl(home, deployment, true) {
        tracing::warn!(%residual, deployment = %deployment.name, "deployment cleanup left a residual");
    }
}

fn cleanup_deployment_dirs_impl(
    home: &Path,
    deployment: &Deployment,
    remove_parent: bool,
) -> Vec<String> {
    // Serialize the fleet snapshot + filesystem deletion with all workspace
    // admissions. Otherwise a newly admitted instance could claim a path after
    // this snapshot and have its directory removed based on stale ownership.
    cleanup_deployment_dirs_with_wait_hook(home, deployment, remove_parent, || {})
}

fn cleanup_deployment_dirs_with_wait_hook(
    home: &Path,
    deployment: &Deployment,
    remove_parent: bool,
    mut on_lock_contention: impl FnMut(),
) -> Vec<String> {
    let mut residuals = Vec::new();
    let lock_path = home.join(".fleet.yaml.lock");
    let _fleet_lock = loop {
        match crate::store::try_acquire_file_lock(&lock_path) {
            Ok(Some(lock)) => break lock,
            Ok(None) => {
                on_lock_contention();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                tracing::warn!(%error, "deployment cleanup refused: fleet lock unavailable");
                return vec![format!(
                    "deployment cleanup could not acquire fleet lock: {error}"
                )];
            }
        }
    };
    let custom_root = std::path::Path::new(&deployment.directory);
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let fleet = match crate::fleet::FleetConfig::load_snapshot_under_lock(&fleet_path) {
        Ok(config) => Some(config),
        Err(_)
            if std::fs::symlink_metadata(&fleet_path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Some(crate::fleet::FleetConfig::default())
        }
        Err(error) => {
            return vec![format!(
                "deployment cleanup could not read fleet snapshot; preserved members {:?}: {error}",
                deployment.instances
            )];
        }
    };
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
    let registered_worktrees = if dir_is_repo {
        crate::git_worktree::list_porcelain_exact(custom_root)
            .map(|entries| {
                entries
                    .into_iter()
                    .filter_map(|(path, _)| path.canonicalize().ok())
                    .collect::<Vec<_>>()
            })
            .map_err(|error| error.to_string())
    } else {
        Ok(Vec::new())
    };
    for inst in &deployment.instances {
        // Custom-directory branch: deploy()'s `inst_dir = dir.join(&inst_name)`.
        let custom_subdir = custom_root.join(inst);
        let custom_owned = deployment_owner_marker_matches(
            &custom_subdir,
            &deployment.name,
            inst,
            deployment.generation_id.as_deref(),
        );
        let custom_admitted = custom_owned
            && deployment_member_cleanup_admitted(
                fleet.as_ref(),
                home,
                inst,
                &custom_subdir,
                &custom_subdir,
            );
        if !custom_admitted && custom_subdir.exists() {
            residuals.push(format!(
                "deployment cleanup preserved workspace for '{inst}' at {} because ownership or cleanup admission refused it",
                custom_subdir.display()
            ));
        }
        let mut remove_custom_tree = true;
        if custom_admitted && dir_is_repo {
            // Instances are named `{deploy_name}-{suffix}`; the worktree branch
            // is `{deploy_name}/{suffix}` (see prepare_work_dir).
            let suffix = inst
                .strip_prefix(&format!("{}-", deployment.name))
                .unwrap_or(inst);
            let branch = format!("{}/{}", deployment.name, suffix);
            let subdir_str = custom_subdir.display().to_string();
            match &registered_worktrees {
                Err(error) => {
                    remove_custom_tree = false;
                    residuals.push(format!(
                        "deployment cleanup preserved '{inst}' at {subdir_str}: could not inspect Git worktrees: {error}"
                    ));
                }
                Ok(paths) => {
                    let is_registered = custom_subdir
                        .canonicalize()
                        .ok()
                        .is_some_and(|candidate| paths.iter().any(|path| path == &candidate));
                    if is_registered {
                        // Worktree removal unregisters and deletes the directory;
                        // only remove the branch after Git confirms that step.
                        match crate::git_helpers::git_bypass(
                            custom_root,
                            &["worktree", "remove", "--force", &subdir_str],
                        ) {
                            Ok(output) if output.status.success() => {
                                for (args, operation) in [
                                    (
                                        &["branch", "-D", branch.as_str()][..],
                                        "remove deployment branch",
                                    ),
                                    (&["worktree", "prune"][..], "prune worktree metadata"),
                                ] {
                                    match crate::git_helpers::git_bypass(custom_root, args) {
                                        Ok(output) if output.status.success() => {}
                                        Ok(output) => residuals.push(format!(
                                            "deployment cleanup could not {operation} for '{inst}' (exit {})",
                                            output.status
                                        )),
                                        Err(error) => residuals.push(format!(
                                            "deployment cleanup could not {operation} for '{inst}': {error}"
                                        )),
                                    }
                                }
                                remove_custom_tree = false;
                            }
                            Ok(output) => {
                                remove_custom_tree = false;
                                residuals.push(format!(
                                    "deployment cleanup preserved worktree for '{inst}' at {subdir_str}: git worktree remove exited {}",
                                    output.status
                                ));
                            }
                            Err(error) => {
                                remove_custom_tree = false;
                                residuals.push(format!(
                                    "deployment cleanup preserved worktree for '{inst}' at {subdir_str}: {error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        if custom_admitted && remove_custom_tree && custom_subdir.exists() {
            match std::fs::remove_dir_all(&custom_subdir) {
                Ok(()) => tracing::info!(
                    inst = %inst,
                    path = %custom_subdir.display(),
                    "deployment cleanup: removed custom subdir"
                ),
                Err(e) => {
                    tracing::warn!(
                        inst = %inst,
                        path = %custom_subdir.display(),
                        error = %e,
                        "deployment cleanup: remove_dir_all failed"
                    );
                    residuals.push(format!(
                        "deployment cleanup could not remove workspace for '{inst}' at {}: {e}",
                        custom_subdir.display()
                    ));
                }
            }
        }
        // Default-path branch: covers deployments whose `directory` defaulted
        // to `home/workspace/<deploy_name>` (subdir lands at
        // `home/workspace/<deploy_name>/<inst>`) AND covers historical
        // teardown semantics that cleaned `home/workspace/<inst>` directly.
        let default_subdir = crate::paths::workspace_dir(home).join(inst);
        let default_owned = default_subdir.exists()
            && deployment_owner_marker_matches(
                &default_subdir,
                &deployment.name,
                inst,
                deployment.generation_id.as_deref(),
            );
        let default_admitted = default_owned
            && deployment_member_cleanup_admitted(
                fleet.as_ref(),
                home,
                inst,
                &default_subdir,
                &default_subdir,
            );
        if default_subdir.exists() && !default_admitted {
            residuals.push(format!(
                "deployment cleanup preserved workspace for '{inst}' at {} because ownership or cleanup admission refused it",
                default_subdir.display()
            ));
        }
        if default_admitted {
            if let Ok(canonical) = crate::paths::canonical_workspace_path(&default_subdir) {
                let admission =
                    crate::agent_ops::cleanup_admission::CleanupAdmission::RemoveOwned {
                        canonical,
                    };
                if let Some(error) = crate::agent_ops::cleanup_working_dir_admitted(
                    home,
                    inst,
                    &default_subdir,
                    &admission,
                ) {
                    residuals.push(format!(
                        "deployment cleanup could not remove default workspace for '{inst}' at {}: {error}",
                        default_subdir.display()
                    ));
                }
            }
        }
    }
    // Sprint 54 P1-5: best-effort rmdir of the custom-directory parent.
    // If every member subdir was just removed AND the operator left no
    // unrelated files there, the parent is now empty — strip it so we
    // don't leak `/tmp/team-foo/` shells behind. `remove_dir` (NOT
    // `remove_dir_all`) errors on non-empty, which is exactly what we
    // want: any operator-dropped file preserves the parent.
    if remove_parent
        && deployment_path_cleanup_admitted(
            fleet.as_ref(),
            home,
            custom_root,
            custom_root,
            &deployment.instances,
        )
    {
        rmdir_if_empty(custom_root);
    }
    residuals
}

/// Admit a deployment-owned child only when its exact canonical location is
/// still disjoint from every effective fleet workspace. This also works after
/// orphan reconciliation has already removed the member from fleet.yaml.
fn deployment_member_cleanup_admitted(
    fleet: Option<&crate::fleet::FleetConfig>,
    home: &Path,
    instance: &str,
    candidate: &Path,
    owned_path: &Path,
) -> bool {
    if fleet.is_some_and(|fleet| fleet.instances.contains_key(instance)) {
        tracing::warn!(instance, path = %candidate.display(), "deployment cleanup refused: instance name has been re-admitted");
        return false;
    }
    if !deployment_path_cleanup_admitted(fleet, home, candidate, owned_path, &[]) {
        return false;
    }
    true
}

fn deployment_path_cleanup_admitted(
    fleet: Option<&crate::fleet::FleetConfig>,
    home: &Path,
    candidate: &Path,
    owned_path: &Path,
    ignored_instances: &[String],
) -> bool {
    let canonical = match crate::paths::canonical_workspace_path(candidate) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(path = %candidate.display(), %error, "deployment cleanup refused: candidate path is ambiguous");
            return false;
        }
    };
    let expected_canonical = match crate::paths::canonical_workspace_path(owned_path) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if canonical != expected_canonical {
        return false;
    }
    let reserved_roots = [
        crate::paths::canonical_workspace_path(home),
        crate::paths::canonical_workspace_path(&crate::paths::workspace_dir(home)),
    ];
    if reserved_roots
        .iter()
        .any(|root| root.as_ref().is_ok_and(|root| root == &canonical))
    {
        tracing::warn!(path = %canonical.display(), "deployment cleanup refused: reserved root");
        return false;
    }
    let Some(fleet) = fleet else {
        tracing::warn!(path = %canonical.display(), "deployment cleanup refused: fleet snapshot unavailable");
        return false;
    };
    for (name, entry) in &fleet.instances {
        if ignored_instances.iter().any(|ignored| ignored == name) {
            continue;
        }
        let survivor = entry
            .working_directory
            .as_deref()
            .map(crate::fleet::resolve::expand_tilde_path)
            .unwrap_or_else(|| crate::paths::workspace_dir(home).join(name));
        match crate::paths::canonical_workspace_path(&survivor) {
            Ok(path)
                if matches!(
                    crate::paths::workspace_paths_overlap(&canonical, &path),
                    Ok(false)
                ) => {}
            Ok(path) => {
                tracing::warn!(path = %canonical.display(), survivor = %path.display(), "deployment cleanup preserved overlapping active workspace");
                return false;
            }
            Err(error) => {
                tracing::warn!(survivor = %survivor.display(), %error, "deployment cleanup refused: survivor path is ambiguous");
                return false;
            }
        }
    }
    true
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

    // Persist cleanup-only state before touching directories. If filesystem
    // cleanup fails, the deployment remains addressable and can be retried by
    // name; successful cleanup removes the record in the second save.
    let mut cleanup_candidates = Vec::new();
    for deployment in &mut store.deployments {
        let any_live = deployment
            .instances
            .iter()
            .any(|instance| live_instances.contains(instance));
        if any_live {
            continue;
        }
        let mut cleanup_instances = deployment.cleanup_instances.clone();
        cleanup_instances.extend(deployment.instances.iter().cloned());
        cleanup_instances.sort();
        cleanup_instances.dedup();
        deployment.instances.clear();
        deployment.cleanup_instances = cleanup_instances.clone();
        cleanup_candidates.push(Deployment {
            instances: cleanup_instances,
            ..deployment.clone()
        });
    }

    if cleanup_candidates.is_empty() {
        return Vec::new();
    }
    if let Err(e) = save(home, &mut store) {
        tracing::warn!(
            error = %e,
            "deployments reconcile: pending cleanup save failed — skipping filesystem cleanup"
        );
        return Vec::new();
    }

    let mut pruned_names = Vec::new();
    let mut pruned_teams = Vec::new();
    for deployment in &cleanup_candidates {
        let residuals = cleanup_deployment_dirs_impl(home, deployment, true);
        if residuals.is_empty() {
            pruned_names.push(deployment.name.clone());
            if let Some(team) = deployment.team.as_ref() {
                pruned_teams.push(team.clone());
            }
            store
                .deployments
                .retain(|record| record.name != deployment.name);
        } else {
            for residual in residuals {
                tracing::warn!(%residual, deployment = %deployment.name, "deployment reconciliation cleanup left a residual");
            }
        }
    }
    if let Err(e) = save(home, &mut store) {
        tracing::warn!(
            error = %e,
            pruned = ?pruned_names,
            "deployments reconcile: final cleanup save failed — cleanup-only records remain for retry"
        );
        return Vec::new();
    }
    for team in &pruned_teams {
        let _ = crate::teams::delete(home, &serde_json::json!({"name": team}));
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
#[cfg(test)]
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
std::thread_local! {
    static AFTER_RUNTIME_INSTANCE_DELETES_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static FAIL_NEXT_DEPLOYMENT_OWNER_MARKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BEFORE_FAILED_WORKDIR_RESIDUAL_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

fn fail_deployment_owner_marker_test_hook() -> bool {
    #[cfg(test)]
    return FAIL_NEXT_DEPLOYMENT_OWNER_MARKER.with(|fail| fail.replace(false));
    #[cfg(not(test))]
    false
}

#[cfg(test)]
fn fail_next_deployment_owner_marker_for_test() {
    FAIL_NEXT_DEPLOYMENT_OWNER_MARKER.with(|fail| fail.set(true));
}

#[cfg(test)]
fn set_before_failed_workdir_residual_hook_for_test(hook: impl FnOnce() + 'static) {
    BEFORE_FAILED_WORKDIR_RESIDUAL_HOOK.with(|slot| {
        slot.borrow_mut().replace(Box::new(hook));
    });
}

#[cfg(test)]
fn run_before_failed_workdir_residual_test_hook() {
    BEFORE_FAILED_WORKDIR_RESIDUAL_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_before_failed_workdir_residual_test_hook() {}

#[cfg(test)]
fn run_after_runtime_instance_deletes_test_hook() {
    AFTER_RUNTIME_INSTANCE_DELETES_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_after_runtime_instance_deletes_test_hook() {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

#[cfg(test)]
mod review_repro_deployments_health_teams;
