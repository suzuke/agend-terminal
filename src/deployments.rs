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
    deployments: Vec<Deployment>,
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "deployments.json")
}

fn load(home: &Path) -> DeploymentStore {
    crate::store::load(&store_path(home))
}

fn save(home: &Path, store: &DeploymentStore) -> anyhow::Result<()> {
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
        let inst_name = format!("{deploy_name}-{inst_suffix}");
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
    let _ = save(home, &store);

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
    let _ = save(home, &store);

    serde_json::json!({"status": "torn_down", "name": name, "instances": deployment.instances})
}

pub fn list(home: &Path) -> Value {
    let store = load(home);
    serde_json::json!({"deployments": store.deployments})
}
