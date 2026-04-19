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
    crate::store::save(&store_path(home), store)
}

pub fn deploy(home: &Path, instance_name: &str, args: &Value) -> Value {
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

    let mut created = Vec::new();
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

        // Create worktree if branch specified
        let work_dir = if let Some(br) = branch {
            let wt = dir.join(&inst_name);
            let branch_name = format!("{deploy_name}/{inst_suffix}");
            match std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    &branch_name,
                    &wt.display().to_string(),
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
            wt.display().to_string()
        } else {
            directory.to_string()
        };

        // Spawn via API
        let _ = crate::api::call(
            home,
            &serde_json::json!({
                "method": crate::api::method::SPAWN,
                "params": {
                    "name": inst_name,
                    "backend": command,
                    "working_directory": work_dir,
                }
            }),
        );
        created.push(inst_name);
    }

    // Create team if multiple instances
    if created.len() > 1 {
        let _ = crate::teams::create(
            home,
            &serde_json::json!({
                "name": deploy_name,
                "members": created,
                "description": format!("Template deployment: {template}")
            }),
        );
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
