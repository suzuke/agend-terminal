//! Repro batch `deployments-health-teams` — Finding 1
//! ("deploy spawns before the lock").
//!
//! `deploy()` runs `spawn_instances` + `create_deployment_team` + the
//! fleet.yaml writes BEFORE acquiring the deployment-store flock
//! (src/deployments.rs ~451) and performs NO existing-name check: the
//! deployment record is `push`ed onto the store unconditionally. Deploying
//! the SAME name twice therefore appends a second record with the same
//! name (and re-spawns / re-writes fleet.yaml). The correct behavior is to
//! reject (or per-name lock) a duplicate name so the store holds exactly
//! one record per deploy name.
//!
//! This drives the real public entry point `deploy()` against a temp HOME
//! and asserts on the durable on-disk store via `list()`. It is RED now
//! (the store ends with TWO `dev` records) and GREEN once a duplicate-name
//! deploy is rejected under the flock.

#![allow(clippy::unwrap_used)]

use super::{deploy, list};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-deploy-dht-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).expect("create tmp home");
    dir
}

#[test]
fn deploy_rejects_duplicate_name_under_lock_deployments_health_teams() {
    let home = tmp_home("dup-name");
    // A two-instance template so the deploy exercises the full path
    // (spawn + team creation) before the store flock.
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
      impl:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).expect("write fleet.yaml");

    let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});

    // First deploy — should succeed and record exactly one deployment.
    let first = deploy(&home, "caller", &args);
    assert!(
        first.get("error").is_none(),
        "first deploy of 'dev' must succeed, got: {first}"
    );

    // Second deploy of the SAME name. The correct behavior rejects this (or
    // locks per deploy-name) so the store is NOT given a second 'dev'
    // record. The pre-fix code spawns + writes + pushes a second record
    // before ever taking the store flock.
    let _second = deploy(&home, "caller", &args);

    let listing = list(&home);
    let deployments = listing
        .get("deployments")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let dev_count = deployments
        .iter()
        .filter(|d| d.get("name").and_then(|n| n.as_str()) == Some("dev"))
        .count();

    assert_eq!(
        dev_count, 1,
        "deploying the same name twice must leave exactly ONE deployment record \
         named 'dev' (duplicate-name deploy must be rejected/locked under the flock, \
         not spawn + write + push a second record before the lock); store held {dev_count}: {listing}"
    );

    std::fs::remove_dir_all(&home).ok();
}
