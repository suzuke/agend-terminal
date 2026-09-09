//! #3541: team-member spawn-argv tests — `team_ops.rs` has no inline test
//! module, so these live in a sibling file (same pattern as
//! `set_model_tests.rs`).

use super::team_ops::{member_spawn_args, CreateTeamRequest};
use crate::backend::Backend;
use crate::fleet::FleetConfig;

fn write_fleet(dir: &std::path::Path, yaml: &str) {
    std::fs::create_dir_all(dir).ok();
    std::fs::write(dir.join("fleet.yaml"), yaml).expect("write fleet.yaml");
}

/// #3541 (reviewer F1): a team member built from defaults (no per-instance
/// effort — `build_member_entries` leaves it `None`) inherits
/// `defaults.effort` through `resolve_instance`, and the team spawn path
/// injects it.
#[test]
fn team_member_inherits_defaults_effort_3541() {
    let dir = std::env::temp_dir().join(format!(
        "agend-team-effort-keep-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    write_fleet(
        &dir,
        "defaults:\n  backend: claude\n  effort: high\ninstances:\n  crew-1:\n    backend: claude\n",
    );
    let config = FleetConfig::load(&crate::fleet::fleet_yaml_path(&dir)).expect("load");
    let resolved = config.resolve_instance("crew-1").expect("resolve");
    assert_eq!(resolved.effort, Some("high".to_string()));

    let argv = member_spawn_args(Some(&resolved), &Backend::ClaudeCode, Vec::new());
    assert_eq!(argv, vec!["--effort", "high"]);

    // Instance-level effort wins over defaults on the team path too.
    write_fleet(
        &dir,
        "defaults:\n  backend: claude\n  effort: high\ninstances:\n  crew-1:\n    backend: claude\n    effort: low\n",
    );
    // Bypass the mtime cache: FleetConfig::load caches per path+mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let config = FleetConfig::load(&crate::fleet::fleet_yaml_path(&dir)).expect("reload");
    let resolved = config.resolve_instance("crew-1").expect("resolve");
    let argv = member_spawn_args(Some(&resolved), &Backend::ClaudeCode, Vec::new());
    assert_eq!(argv, vec!["--effort", "low"]);
    std::fs::remove_dir_all(&dir).ok();
}

/// #3541: a team member on an effort-unsupported backend drops the intent
/// (fail-soft) instead of breaking the spawn.
#[test]
fn team_member_unsupported_backend_drops_effort_3541() {
    let dir = std::env::temp_dir().join(format!(
        "agend-team-effort-drop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    write_fleet(
        &dir,
        "defaults:\n  backend: kiro-cli\n  effort: high\ninstances:\n  crew-1:\n    backend: kiro-cli\n",
    );
    let config = FleetConfig::load(&crate::fleet::fleet_yaml_path(&dir)).expect("load");
    let resolved = config.resolve_instance("crew-1").expect("resolve");
    let argv = member_spawn_args(Some(&resolved), &Backend::KiroCli, Vec::new());
    assert!(
        argv.is_empty(),
        "unsupported backend must drop effort, got {argv:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn team_member_missing_env_source_refuses_spawn_3540_r2() {
    let dir = std::env::temp_dir().join(format!(
        "agend-team-missing-env-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    write_fleet(
        &dir,
        "defaults:\n  env:\n    DESTINATION_TOKEN:\n      from_env: AGEND_TEST_MISSING_TEAM_ENV_3540_R2\ninstances: {}\n",
    );
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();

    let result = super::team_ops::create(
        &dir,
        CreateTeamRequest {
            name: "missing-env-team".to_string(),
            per_member_backends: vec!["agend-test-missing-team-command-3540-r2".to_string()],
            existing_members: Vec::new(),
            topic_binding_mode: Some("skip".to_string()),
            orchestrator: None,
            description: None,
            repository_path: None,
            project_id: None,
            accept_from: Vec::new(),
        },
        &registry,
        &configs,
        None,
    );

    assert_eq!(result["ok"], serde_json::json!(false), "{result}");
    assert!(crate::agent::lock_registry(&registry).is_empty());
    assert!(configs.lock().is_empty());
    let error = result["error"].as_str().expect("structured error");
    assert!(error.contains("AGEND_TEST_MISSING_TEAM_ENV_3540_R2"));
    assert!(!error.contains("secret-value-must-not-leak"));
    std::fs::remove_dir_all(dir).ok();
}
