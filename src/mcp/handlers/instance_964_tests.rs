//! #964 regression tests — caller-side integration coverage for the
//! SPAWN-then-add ordering bug that was latent since PR #417 (May 4)
//! and only surfaced after #962 made the silent no-op observable.
//!
//! T1 + T2 inject a mock `spawn_fn` into `spawn_single_instance_impl`
//! that mimics `handle_spawn`'s side effects (`register_topic` writes
//! `topic_id` back via `update_instance_field`) WITHOUT standing up a
//! real daemon. This isolates the ordering invariant: the fleet.yaml
//! entry MUST exist when SPAWN runs, otherwise the mock's
//! `update_instance_field` call hits #962 Path 2 (silent no-op) and
//! the assertion fails — which is exactly what HEAD-pre-fix did.
//!
//! Class lesson (per dev-2 cross-audit): the existing test
//! `topic_id_persists_to_fleet_yaml_via_update_instance_field`
//! (api/handlers/instance.rs:712, added with PR #417) is helper-
//! level and never exercised the caller-path ordering. These tests
//! pin the caller-path contract — what PR #417 should have shipped.
//!
//! Lives in a sibling file (rather than inline `#[cfg(test)] mod`)
//! to satisfy the 750-LOC file-size invariant on
//! `src/mcp/handlers/instance.rs`. Same precedent as
//! `channel_p0a_tests.rs`, `p0b_tests.rs`, `comms_p0c_tests.rs`.

use super::instance_spawn::spawn_single_instance_impl;
use serde_json::{json, Value};
use std::path::Path;

fn tmp_home(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agend-964-{}-{}-{}", slug, std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&dir), "instances: {}\n").unwrap();
    dir
}

/// T1 (load-bearing): MCP create_instance must persist `topic_id` to
/// fleet.yaml on the happy path. The mock `spawn_fn` mimics
/// `handle_spawn`'s `register_topic` chain: it calls
/// `update_instance_field("topic_id", ...)` then returns the topic_id
/// in the SPAWN response. After the swap fix, the fleet.yaml entry
/// exists by the time the mock fires, so the helper write succeeds and
/// the post-state has `topic_id = Some(5198)`.
///
/// Pre-#964 (SPAWN-then-add ordering): mock would fire BEFORE the
/// caller added the entry → helper hits #962 Path 2 silent no-op →
/// `topic_id` stays `None` → assertion fails. This test is the
/// regression anchor.
#[test]
fn t1_create_instance_persists_topic_id_to_fleet_yaml() {
    let home = tmp_home("t1");
    let target_topic_id = 5198_i32;

    let spawn_fn = |h: &Path, req: &Value| -> anyhow::Result<Value> {
        let name = req["params"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing name"))?;
        // Mimic handle_spawn → register_topic → update_instance_field.
        // With the swap fix, fleet.yaml entry exists here, so the
        // helper persists topic_id. Pre-fix, the entry didn't exist,
        // so the helper silently no-op'd.
        crate::fleet::update_instance_field(
            h,
            name,
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(target_topic_id)),
        )?;
        Ok(json!({
            "ok": true,
            "result": {"topic_id": target_topic_id}
        }))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t1-instance", "backend": "claude"}),
        &spawn_fn,
    );

    assert_eq!(
        result["topic_id"].as_i64(),
        Some(target_topic_id as i64),
        "MCP response must carry topic_id; got {result}"
    );

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let persisted = cfg.instances.get("t1-instance").and_then(|i| i.topic_id);
    assert_eq!(
        persisted,
        Some(target_topic_id),
        "#964 regression: topic_id must be persisted to fleet.yaml \
         after MCP create_instance returns. Got {persisted:?} for \
         entry={:?}",
        cfg.instances.get("t1-instance")
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// T2 (rollback): when SPAWN reports failure, the fleet.yaml entry
/// that was added pre-SPAWN MUST be rolled back so create_instance is
/// all-or-nothing. Without rollback, a failed create would leave a
/// stale entry that confuses subsequent list/replace flows.
#[test]
fn t2_spawn_failure_rolls_back_fleet_yaml_entry() {
    let home = tmp_home("t2");

    let spawn_fn = |_h: &Path, _req: &Value| -> anyhow::Result<Value> {
        Ok(json!({"ok": false, "error": "backend rejected spawn"}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t2-instance", "backend": "claude"}),
        &spawn_fn,
    );

    assert!(
        result.get("error").is_some(),
        "SPAWN failure must surface as error; got {result}"
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("backend rejected spawn"),
        "error must propagate inner SPAWN error; got {result}"
    );

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    assert!(
        !cfg.instance_names().iter().any(|n| n == "t2-instance"),
        "#964 rollback: fleet.yaml must NOT contain t2-instance \
         after SPAWN failure. Got instances={:?}",
        cfg.instance_names()
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// T2b (rollback on API-unavailable): same contract as T2, but the
/// spawn_fn returns `Err` (API-unavailable path) instead of an
/// `Ok(json!{"ok": false})` response.
#[test]
fn t2b_api_unavailable_rolls_back_fleet_yaml_entry() {
    let home = tmp_home("t2b");

    let spawn_fn =
        |_h: &Path, _req: &Value| -> anyhow::Result<Value> { Err(anyhow::anyhow!("ipc dead")) };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t2b-instance", "backend": "claude"}),
        &spawn_fn,
    );

    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("API unavailable"),
        "Err path must surface as 'API unavailable'; got {result}"
    );

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    assert!(
        !cfg.instance_names().iter().any(|n| n == "t2b-instance"),
        "#964 rollback on API-unavailable: fleet.yaml must NOT \
         contain t2b-instance; got instances={:?}",
        cfg.instance_names()
    );

    let _ = std::fs::remove_dir_all(&home);
}
