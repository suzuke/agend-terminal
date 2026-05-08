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

pub fn deploy(home: &Path, instance_name: &str, args: &Value) -> Value {
    // C1 fix: flock around load-modify-save to prevent lost-update race.
    let lock_path = store_path(home).with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => return serde_json::json!({"error": format!("deployment lock failed: {e}")}),
    };
    let template = match args["template"].as_str() {
        Some(t) => t,
        None => return serde_json::json!({"error": "missing 'template'"}),
    };
    let directory = match args["directory"].as_str() {
        Some(d) => d,
        None => return serde_json::json!({"error": "missing 'directory'"}),
    };
    let deploy_name = args["name"].as_str().unwrap_or(template);
    let branch = args["branch"].as_str();

    // Template / deploy name both feed into paths and shell-visible
    // identifiers (git branch `deploy_name/suffix`, worktree path,
    // deployment record). Enforce the same character class as
    // agent::validate_name. Check template first so an empty `name` that
    // defaults to `template` still surfaces a template-name error rather
    // than a confusing deploy-name error.
    if let Err(e) = crate::agent::validate_name(template) {
        return serde_json::json!({"error": format!("invalid template name: {e}")});
    }
    if let Err(e) = crate::agent::validate_name(deploy_name) {
        return serde_json::json!({"error": format!("invalid deploy name: {e}")});
    }

    // Load fleet.yaml to find template definition
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return serde_json::json!({"error": "No fleet.yaml"});
    }
    let config = match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"error": format!("fleet.yaml: {e}")}),
    };

    let templates = match &config.templates {
        Some(t) => t,
        None => return serde_json::json!({"error": "No templates defined in fleet.yaml"}),
    };

    let template_def = match templates.get(template) {
        Some(t) => t,
        None => return serde_json::json!({"error": format!("Template '{template}' not found")}),
    };

    let instances_def = match template_def.get("instances").and_then(|v| v.as_mapping()) {
        Some(m) => m,
        None => return serde_json::json!({"error": "Template has no instances"}),
    };

    // Phase 1 — validate every template entry, compute worktrees, and
    // collect the fleet.yaml records. No SPAWN happens here: handle_spawn
    // reads fleet.yaml to build the AgentContext (name + role + peers),
    // which means every instance's entry must exist before any member
    // spawns, otherwise early spawns would see a stale peer list and ship
    // an incomplete Identity block into the backend's agend.md.
    let mut created: Vec<String> = Vec::new();
    let mut yaml_entries: Vec<(String, crate::fleet::InstanceYamlEntry)> = Vec::new();
    let dir = std::path::PathBuf::from(directory);

    for (name_val, inst_val) in instances_def {
        let inst_suffix = match name_val.as_str() {
            Some(s) => s,
            None => continue,
        };
        // Template-supplied suffix flows into a git branch name
        // (`deploy_name/suffix`) and a worktree path segment (`inst_name`).
        // A hostile fleet.yaml could otherwise stuff `../../etc` or shell
        // metacharacters here. Skip the entry with a warn rather than
        // aborting the whole deploy.
        if let Err(e) = crate::agent::validate_name(inst_suffix) {
            tracing::warn!(
                %deploy_name,
                suffix = %inst_suffix,
                error = %e,
                "skipping template instance with invalid name"
            );
            continue;
        }
        let inst_name = format!("{deploy_name}-{inst_suffix}");
        if let Err(e) = crate::agent::validate_name(&inst_name) {
            tracing::warn!(
                %inst_name,
                error = %e,
                "skipping: combined instance name fails validation"
            );
            continue;
        }
        let command = inst_val
            .get("command")
            .or_else(|| inst_val.get("backend"))
            .and_then(|v| v.as_str())
            .unwrap_or("claude");
        // Accept `role:` (preferred) and `description:` (alias, mirrors
        // InstanceConfig's serde alias). Empty string → None so we don't
        // write a blank "Role:" line into agend.md.
        let role = inst_val
            .get("role")
            .or_else(|| inst_val.get("description"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let instructions = inst_val
            .get("instructions")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        // Every member gets its own `<directory>/<inst_name>` subdir —
        // same-backend teammates would otherwise clobber each other's
        // agend.md (and mcp-config.json) when writing into a shared dir.
        // `directory` is therefore treated as the parent for all members,
        // matching branch-mode behavior.
        let inst_dir = dir.join(&inst_name);
        let work_dir = if let Some(br) = branch {
            let branch_name = format!("{deploy_name}/{inst_suffix}");
            match std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    &branch_name,
                    &inst_dir.display().to_string(),
                    br,
                ])
                .current_dir(&dir)
                .output()
            {
                Ok(o) if o.status.success() => {
                    tracing::info!(%inst_name, %branch_name, "created worktree");
                }
                Ok(o) => {
                    tracing::warn!(%inst_name, error = %String::from_utf8_lossy(&o.stderr).trim(), "worktree failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "git not available");
                }
            }
            inst_dir.display().to_string()
        } else {
            // create_dir_all is a no-op if it already exists; safe to call
            // unconditionally. handle_spawn also runs one — this pre-create
            // is defensive for tests that inspect the path without going
            // through the daemon.
            std::fs::create_dir_all(&inst_dir).ok();
            inst_dir.display().to_string()
        };

        yaml_entries.push((
            inst_name.clone(),
            crate::fleet::InstanceYamlEntry {
                backend: Some(command.to_string()),
                working_directory: Some(work_dir),
                role,
                instructions,
                // Sprint 54 P1-B Bug 2 fix: see instance.rs:593.
                source_repo: None,
                // Sprint 55 P0-B EC4: see instance.rs (gradient).
                repo: None,
                github_login: None,
            },
        ));
        created.push(inst_name);
    }

    // Phase 2 — persist to fleet.yaml. Must happen before any SPAWN so
    // handle_spawn can read this instance's role and the full peer list
    // when building the AgentContext for its agend.md. Single lock take
    // for the whole batch.
    if !yaml_entries.is_empty() {
        let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
            yaml_entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
        if let Err(e) = crate::fleet::add_instances_to_yaml(home, &refs) {
            tracing::warn!(error = %e, "failed to persist deployment to fleet.yaml");
        }
    }

    // Phase 3 — SPAWN each instance. handle_spawn now reads fleet.yaml,
    // writes a full Identity/Role/Peers agend.md (or GEMINI.md for gemini),
    // then spawns the child so the backend's --append-system-prompt-file
    // flag resolves to an existing file.
    for (inst_name, entry) in &yaml_entries {
        let backend_name = entry.backend.as_deref().unwrap_or("claude");
        let work_dir = entry.working_directory.as_deref().unwrap_or(directory);
        let _ = crate::api::call(
            home,
            &serde_json::json!({
                "method": crate::api::method::SPAWN,
                "params": {
                    "name": inst_name,
                    "backend": backend_name,
                    "working_directory": work_dir,
                }
            }),
        );
    }

    // Phase 4 — create the team. Route through the CREATE_TEAM API (not a
    // direct teams::create call) so the handler emits the TeamCreated event
    // and the TUI moves all member panes into a single tab, matching the
    // behavior of `create_instance(team:...)`. Passing empty backends/count
    // tells handle_create_team to skip its own spawn phase — our members
    // already exist from Phase 3.
    //
    // Orchestrator can be nominated by *suffix* (`orchestrator: lead` →
    // `dev-lead`). Unknown or unspawned suffixes get dropped with a warn,
    // leaving the team orchestrator-less rather than failing deploy.
    if created.len() > 1 {
        let mut team_args = serde_json::json!({
            "name": deploy_name,
            "members": &created,
            "description": format!("Template deployment: {template}"),
        });
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
        // Route through API so the daemon can emit TeamCreated and the TUI
        // consolidates member panes into one tab. Fall back to a direct
        // teams::create when the daemon is unreachable (unit tests, or a
        // pre-daemon bootstrap) — no TUI means no consolidation anyway, so
        // just persist the record.
        match crate::api::call(
            home,
            &serde_json::json!({
                "method": crate::api::method::CREATE_TEAM,
                "params": &team_args,
            }),
        ) {
            Ok(_) => {}
            Err(_) => {
                let _ = crate::teams::create(home, &team_args);
            }
        }
    }

    // Track deployment
    let deployment = Deployment {
        name: deploy_name.to_string(),
        template: template.to_string(),
        instances: created.clone(),
        team: if created.len() > 1 {
            Some(deploy_name.to_string())
        } else {
            None
        },
        directory: directory.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut store = load(home);
    store.deployments.push(deployment);
    let _ = save(home, &mut store);

    let _ = instance_name; // suppress unused
    serde_json::json!({"status": "deployed", "name": deploy_name, "instances": created})
}

pub fn teardown(home: &Path, args: &Value) -> Value {
    // C1 fix: flock around load-modify-save to prevent lost-update race.
    let lock_path = store_path(home).with_extension("lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => return serde_json::json!({"error": format!("deployment lock failed: {e}")}),
    };
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };

    let mut store = load(home);
    let deployment = match store.deployments.iter().find(|d| d.name == name) {
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

    // Remove from store
    store.deployments.retain(|d| d.name != name);
    let _ = save(home, &mut store);

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
    for inst in &deployment.instances {
        // Custom-directory branch: deploy()'s `inst_dir = dir.join(&inst_name)`.
        let custom_subdir = custom_root.join(inst);
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
        let default_subdir = home.join("workspace").join(inst);
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
    let fleet_path = home.join("fleet.yaml");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let args = serde_json::json!({
            "template": "dev",
            "directory": home.display().to_string(),
        });
        let _ = deploy(&home, "caller", &args);

        let reloaded =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("reload fleet.yaml");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).unwrap();
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).unwrap();
        assert_eq!(
            reloaded
                .instances
                .get("dev-lead")
                .and_then(|i| i.instructions.as_deref()),
            Some("./instructions/lead.md")
        );
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
        let _ = deploy(&home, "caller", &args);

        let reloaded = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).unwrap();
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        // Sanity: deployed entries are there.
        let after_deploy =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("post-deploy");
        assert!(after_deploy.instances.contains_key("dev-lead"));
        assert!(after_deploy.instances.contains_key("dev-impl"));

        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        let after_teardown =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("post-teardown");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );

        let reloaded = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).unwrap();
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();

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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        // Create workspace dir (simulates what daemon would create).
        let workspace = home.join("workspace").join("dev-worker");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
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
        let workspace = home.join("workspace").join("dev-agent");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        let _ = deploy(
            &home,
            "caller",
            &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
        );
        let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

        let config = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).expect("load fleet");
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
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
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
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
            home.join("fleet.yaml"),
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
        std::fs::write(home.join("fleet.yaml"), "instances: {}\n").unwrap();
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
        let wd = home.join("workspace").join("tpl-worker");
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
            home.join("fleet.yaml"),
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
}
