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

    // Kill all instances
    for inst in &deployment.instances {
        let _ = crate::api::call(
            home,
            &serde_json::json!({"method": crate::api::method::KILL, "params": {"name": inst}}),
        );
    }

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
}
