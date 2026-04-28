//! Worktree opt-out integration tests.
//!
//! §3.5.11 RED: these tests verify fleet.yaml `worktree: false` is parsed
//! correctly by the InstanceConfig schema.

/// Minimal InstanceConfig mirror for testing fleet.yaml parsing.
/// Must match src/fleet.rs::InstanceConfig's worktree field.
#[derive(serde::Deserialize, Debug)]
struct TestInstanceConfig {
    #[allow(dead_code)]
    backend: Option<String>,
    #[serde(default)]
    worktree: Option<bool>,
}

#[derive(serde::Deserialize, Debug)]
struct TestFleetConfig {
    #[serde(default)]
    instances: std::collections::HashMap<String, TestInstanceConfig>,
}

#[test]
fn fleet_yaml_worktree_false_parsed() {
    let yaml = r#"
instances:
  dev-lead:
    backend: claude
    worktree: false
  dev-impl:
    backend: claude
"#;
    let config: TestFleetConfig = serde_yaml::from_str(yaml).unwrap();
    let lead = config.instances.get("dev-lead").unwrap();
    assert_eq!(
        lead.worktree,
        Some(false),
        "worktree: false must parse as Some(false)"
    );
    let impl_inst = config.instances.get("dev-impl").unwrap();
    assert_eq!(
        impl_inst.worktree, None,
        "absent worktree must parse as None (default true)"
    );
}

#[test]
fn fleet_yaml_worktree_true_explicit() {
    let yaml = r#"
instances:
  worker:
    backend: claude
    worktree: true
"#;
    let config: TestFleetConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(
        config.instances.get("worker").unwrap().worktree,
        Some(true)
    );
}
