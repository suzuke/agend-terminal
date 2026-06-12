//! #964 regression tests — caller-side integration coverage for the
//! SPAWN-then-add ordering bug that was latent since PR #417 (May 4)
//! and only surfaced after #962 made the silent no-op observable.
//!
//! T1 + T2 inject a mock `spawn_fn` into `spawn_single_instance_impl`
//! that mimics `handle_spawn`'s side effects (`register_topic` writes
//! `topic_id` to `topics.json`) WITHOUT standing up a real daemon.
//! This isolates the ordering invariant: the fleet.yaml entry MUST
//! exist when SPAWN runs, otherwise the mock's `register_topic` call
//! fails and the assertion fails.
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

use super::instance_state::spawn::spawn_single_instance_impl;
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
/// topics.json on the happy path. The mock `spawn_fn` mimics
/// `handle_spawn`'s `register_topic` chain: it calls
/// `register_topic(home, topic_id, name)` then returns the topic_id
/// in the SPAWN response. After the swap fix, the fleet.yaml entry
/// exists by the time the mock fires, so the register succeeds.
///
/// Pre-#964 (SPAWN-then-add ordering): mock would fire BEFORE the
/// caller added the entry. Post-#994 Phase 1, topic_id is persisted
/// to topics.json (not fleet.yaml), so the ordering constraint is
/// relaxed — but the test anchors the caller-path contract.
#[test]
fn t1_create_instance_persists_topic_id() {
    let home = tmp_home("t1");
    let target_topic_id = 5198_i32;

    let spawn_fn = |h: &Path, req: &Value| -> anyhow::Result<Value> {
        let name = req["params"]["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing name"))?;
        crate::channel::telegram::register_topic(h, target_topic_id, name)?;
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

    let persisted = crate::channel::telegram::lookup_topic_for_instance(&home, "t1-instance");
    assert_eq!(
        persisted,
        Some(target_topic_id),
        "#964 regression: topic_id must be persisted to topics.json \
         after MCP create_instance returns. Got {persisted:?}"
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

// ── #991 topic_binding tests ──────────────────────────────────────

#[test]
fn t3_topic_binding_skip_persists_to_fleet_yaml_and_spawn_params() {
    let home = tmp_home("t3-tb");

    let spawn_fn = |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        assert_eq!(
            req["params"]["topic_binding"].as_str(),
            Some("skip"),
            "SPAWN RPC must carry topic_binding param"
        );
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t3-skip", "backend": "claude", "topic_binding": "skip"}),
        &spawn_fn,
    );
    assert!(result.get("error").is_none(), "must succeed: {result}");

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let inst = cfg.instances.get("t3-skip").expect("instance exists");
    assert_eq!(
        inst.topic_binding_mode.as_deref(),
        Some("skip"),
        "fleet.yaml must persist topic_binding_mode=skip"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn t4_topic_binding_omitted_defaults_to_auto() {
    let home = tmp_home("t4-tb");

    let spawn_fn = |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        assert!(
            req["params"]["topic_binding"].is_null(),
            "SPAWN params must NOT carry topic_binding when omitted"
        );
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t4-auto", "backend": "claude"}),
        &spawn_fn,
    );
    assert!(result.get("error").is_none(), "must succeed: {result}");

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let inst = cfg.instances.get("t4-auto").expect("instance exists");
    assert!(
        inst.topic_binding_mode.is_none(),
        "fleet.yaml must NOT have topic_binding_mode when omitted (auto default)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn t5_topic_binding_deferred_persists_and_forwards() {
    let home = tmp_home("t5-tb");

    let spawn_fn = |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        assert_eq!(
            req["params"]["topic_binding"].as_str(),
            Some("deferred"),
            "SPAWN RPC must carry topic_binding=deferred"
        );
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t5-deferred", "backend": "claude", "topic_binding": "deferred"}),
        &spawn_fn,
    );
    assert!(result.get("error").is_none(), "must succeed: {result}");

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let inst = cfg.instances.get("t5-deferred").expect("instance exists");
    assert_eq!(
        inst.topic_binding_mode.as_deref(),
        Some("deferred"),
        "fleet.yaml must persist topic_binding_mode=deferred"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn t6_topic_binding_invalid_value_treated_as_auto() {
    let home = tmp_home("t6-tb");

    let spawn_fn = |_h: &Path, _req: &Value| -> anyhow::Result<Value> {
        Ok(json!({"ok": true, "result": {}}))
    };

    let result = spawn_single_instance_impl(
        &home,
        "test-spawner",
        &json!({"name": "t6-invalid", "backend": "claude", "topic_binding": "bogus"}),
        &spawn_fn,
    );
    assert!(result.get("error").is_none(), "must succeed: {result}");

    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let inst = cfg.instances.get("t6-invalid").expect("instance exists");
    assert!(
        inst.topic_binding_mode.is_none(),
        "invalid topic_binding value must be treated as auto (not persisted)"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #1858: create_instance must PERSIST `args` + `model` into the fleet entry so a
/// daemon RESTART (re-resolve from disk) reproduces the SAME backend argv as the
/// original spawn — not a "bare" instance missing the user args and the --model
/// flag. `instructions` is regenerated from role+peers at boot, so it is not the
/// lost field; `args`/`model` were left None pre-fix.
#[test]
fn create_instance_persists_args_and_model_for_restart_parity_1858() {
    let home = tmp_home("1858");
    // Capture the spawn-time argv the backend received (the SPAWN RPC params).
    let captured = std::cell::RefCell::new(String::new());
    let spawn_fn = |_h: &Path, req: &Value| -> anyhow::Result<Value> {
        *captured.borrow_mut() = req["params"]["args"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(json!({"ok": true, "result": {}}))
    };
    let _ = spawn_single_instance_impl(
        &home,
        "spawner",
        &json!({"name": "dev-x", "backend": "claude", "args": "--foo bar", "model": "opus"}),
        &spawn_fn,
    );

    // The persisted entry (what a restart re-resolves) must carry args + model.
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let cfg = fleet
        .instances
        .get("dev-x")
        .expect("instance persisted to fleet.yaml");
    assert_eq!(
        cfg.args,
        vec!["--foo".to_string(), "bar".to_string()],
        "#1858: user args must persist (was None → bare argv on restart)"
    );
    assert_eq!(
        cfg.model.as_deref(),
        Some("opus"),
        "#1858: model must persist (was None → no --model flag on restart)"
    );

    // Restart parity: boot reconstructs argv = entry.args + a --model flag derived
    // from entry.model; it must EQUAL the spawn-time argv (not be "less than").
    let model_token = crate::backend::Backend::from_command("claude")
        .map(|b| b.format_model_arg("opus"))
        .unwrap_or_else(|| "opus".to_string());
    let mut boot_argv = cfg.args.clone();
    boot_argv.push("--model".to_string());
    boot_argv.push(model_token);
    let spawn_argv: Vec<String> = captured
        .borrow()
        .split_whitespace()
        .map(String::from)
        .collect();
    assert_eq!(
        boot_argv, spawn_argv,
        "#1858: restart re-resolve must reproduce the spawn argv (not bare)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2037 (6): explicit `name` + `team` with count>1/backends is ambiguous —
/// loud error instead of the pre-#2037 silent rename to `<team>-N`.
#[test]
fn create_instance_name_plus_team_count_conflict_errors_2037() {
    let home = tmp_home("2037-name-team");
    let resp = super::instance::handle_create_instance(
        &home,
        &serde_json::json!({"name": "exact-name", "team": "squad", "count": 3}),
        "lead",
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ambiguous"),
        "count>1 + name must be a loud error, not a silent rename: {resp}"
    );
    let resp = super::instance::handle_create_instance(
        &home,
        &serde_json::json!({"name": "exact-name", "team": "squad", "backends": ["claude", "codex"]}),
        "lead",
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ambiguous"),
        "backends + name must be a loud error: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}
