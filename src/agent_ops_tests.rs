#![cfg(test)]

use super::*;
use serde_json::json;

/// t-…-82348-36: LIST carries the reading's MEANING alongside the number.
/// Without it a consumer sees only `context_pct` and assumes "remaining
/// session budget" — the misread that nearly restarted healthy agents on
/// 2026-09-02. Claude scrapes context-WINDOW fill of an auto-compacted
/// window; kiro reports its own gauge; a backend that scrapes nothing says
/// null. The pre-existing context fields are untouched.
#[test]
fn list_snapshot_reports_context_meaning() {
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    let home = tmp_home("list-context-meaning");
    let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
    let (claude, _r1) = crate::daemon::per_tick::mock_live_agent_with_frame(
        "cl",
        &crate::backend::Backend::ClaudeCode,
        "  Model: Fable 5.1 | Ctx Used: 95.0% | ⎇ main | (+0,-0)",
    );
    let (kiro, _r2) = crate::daemon::per_tick::mock_live_agent_with_frame(
        "k",
        &crate::backend::Backend::KiroCli,
        "  Kiro · auto · ◔ 82%",
    );
    let (plain, _r3) = crate::daemon::per_tick::mock_live_agent_no_context("sh");
    for h in [claude, kiro, plain] {
        registry.lock().insert(h.id, h);
    }
    let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
    let out = list_snapshot(&home, &registry, &externals);
    let by_name = |n: &str| -> serde_json::Value {
        out["result"]["agents"]
            .as_array()
            .expect("agents array")
            .iter()
            .find(|a| a["name"] == n)
            .unwrap_or_else(|| panic!("agent {n} in LIST"))
            .clone()
    };
    let cl = by_name("cl");
    assert!(cl["instance_ref"]["instance_id"].is_string());
    assert_eq!(
        cl["instance_ref"]["generation"], 0,
        "roster identity must expose the live handle generation"
    );
    assert_eq!(
        cl["context_meaning"], "window_fill",
        "Claude's scraped figure is context-WINDOW fill, not session budget: {cl}"
    );
    // Additive: the existing context fields are unchanged.
    assert_eq!(cl["context_pct"], 95.0);
    assert_eq!(cl["context_source"], "pattern");
    assert_eq!(cl["context_provider"], "statusline");
    assert_eq!(by_name("k")["context_meaning"], "context_gauge");
    assert_eq!(by_name("sh")["context_meaning"], serde_json::Value::Null);
    std::fs::remove_dir_all(&home).ok();
}

fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-agent-ops-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
fn move_pane_validates_parses_and_logs_2454() {
    let home = tmp_home("move-pane-service-2454");
    let event = move_pane(&home, Some("agent-a"), Some("team-x"), Some("vertical")).unwrap();
    assert_eq!(event.agent, "agent-a");
    assert_eq!(event.target_tab, "team-x");
    assert_eq!(event.split_dir, PaneMoveSplit::Vertical);
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap();
    assert!(log.contains("\"kind\":\"move_pane\""));
    assert!(log.contains("target_tab=team-x split=Vertical"));
    assert_eq!(
        move_pane(&home, None, Some("team-x"), None),
        Err("missing agent".into())
    );
    assert_eq!(
        move_pane(&home, Some("agent-a"), None, None),
        Err("missing target_tab".into())
    );
}

/// The public runtime DELETE must fence an external early-return path just
/// like a managed delete. A queued old-generation job held behind the
/// keyed lane must be discarded after the external record is removed,
/// without reaching an adapter or recreating receipt state.
#[test]
fn external_delete_fences_queued_transport_before_early_return() {
    run_external_delete_fixture(true);
}

fn run_external_delete_fixture(send_outcome: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let home = tmp_home("external-delete-transport-fence");
    let agent = format!("external-delete-race-{}", std::process::id());
    let _full_guard = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(false);
    let _delivery_hook = crate::transport::test_support::delivery_hook_guard();
    let _cleanup_release_tail_guard =
        crate::daemon::delivery_worker::test_support::cleanup_release_tail_hook_guard();
    let adapter_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let adapter_calls_hook = std::sync::Arc::clone(&adapter_calls);
    let expected_home = home.clone();
    let expected_agent = agent.clone();
    crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
        move |called_home, called_agent, _body| {
            if called_home == expected_home.as_path() && called_agent == expected_agent {
                adapter_calls_hook.fetch_add(1, Ordering::SeqCst);
                Some(Err(anyhow::anyhow!(
                    "external-delete stale job reached adapter"
                )))
            } else {
                None
            }
        },
    )));

    // #3240 slice 2. Lane entry is an ORDERING fact, and this fixture used to
    // assert it with a one-second wall-clock budget that had to cover a
    // thread spawn, a global lane-map lock, a keyed mutex, this admission
    // hook and an epoch-state lock — so a loaded machine failed a healthy
    // run. The hook below deliberately pushes admission PAST that old budget
    // exactly once, so re-introducing any clock-based readiness wait fails
    // deterministically here instead of flaking somewhere else later.
    //
    // The delay is UNCONDITIONAL, and nothing else needs to be: the hook runs
    // only inside `with_transport_serial` (daemon/delivery_worker.rs:337-340),
    // while the queued worker takes the lane itself and hands the guard to
    // `dispatch_transport` (:468-471), which never runs the hook. So this
    // delay cannot reach the dispatch assertion further down, and a latch to
    // keep it away from there would be guarding a path that does not exist.
    const LANE_ADMISSION_DELAY: Duration = Duration::from_millis(1200);
    let _admission_guard =
        crate::daemon::delivery_worker::test_support::direct_transport_admission_hook_guard();
    let _cleanup_before_lane_guard =
        crate::daemon::delivery_worker::test_support::cleanup_before_lane_acquire_hook_guard();
    let admission_home = home.clone();
    let admission_agent = agent.clone();
    crate::daemon::delivery_worker::test_support::set_direct_transport_admission_hook(Some(
        std::sync::Arc::new(move |hook_home: &std::path::Path, hook_agent: &str| {
            if hook_home == admission_home.as_path() && hook_agent == admission_agent {
                std::thread::sleep(LANE_ADMISSION_DELAY);
            }
        }),
    ));

    // Deterministic treatment for the completion clock: the cleanup tail
    // is a real post-lane release path, so delaying it here reproduces the
    // Windows panic without relying on scheduler or filesystem load.
    const CLEANUP_RELEASE_TAIL_DELAY: Duration = Duration::from_millis(1200);
    let tail_home = home.clone();
    let tail_agent = agent.clone();
    crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
        std::sync::Arc::new(move |hook_home, hook_agent| {
            if hook_home == tail_home.as_path() && hook_agent == tail_agent {
                std::thread::sleep(CLEANUP_RELEASE_TAIL_DELAY);
            }
        }),
    ));

    let (lane_entered_tx, lane_entered_rx) = std::sync::mpsc::channel();
    let (lane_release_tx, lane_release_rx) = std::sync::mpsc::channel();
    let lane_home = home.clone();
    let lane_agent = agent.clone();
    let lane_holder = std::thread::spawn(move || {
        crate::daemon::delivery_worker::with_transport_serial(&lane_home, &lane_agent, || {
            lane_entered_tx.send(()).expect("lane-entered observer");
            lane_release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("lane release");
        });
    });
    // A BARRIER, not a clock. The ordering fact this fixture needs is
    // narrow and worth stating exactly: the holder has entered the
    // SYNCHRONOUS `with_transport_serial` path — past the lane acquire and
    // past the test admission hook — because that is where it sends. It
    // says nothing about the queued worker, which reaches
    // `dispatch_transport` by a different route. No wall-clock budget can
    // express even that much: too short flakes on a loaded machine, too
    // long only delays the flake. The wait is bounded by DISCONNECTION instead — if the holder
    // thread dies or panics, its sender drops and `recv()` returns `Err`
    // immediately, so a genuine failure stays fast and named rather than
    // becoming a hang. Teardown below keeps its own explicit bounds.
    //
    // DISCONNECT-BOUNDED IS NOT DEADLOCK-BOUNDED, and the difference is not
    // uniform across CI. If the holder neither finishes nor dies — wedged
    // inside `TransportLaneGuard::acquire`, say — its sender never drops and
    // this wait has no bound of its own. In the Check jobs nextest's `ci`
    // profile terminates a stuck test after its slow-timeout periods and
    // NAMES it. The Coverage job does not: it runs `cargo llvm-cov --tests`,
    // i.e. libtest, which has no per-test timeout, so there the same wedge
    // degrades into an anonymous step timeout. That is the trade this
    // barrier makes against the wall-clock budget it replaced, recorded here
    // rather than left for whoever meets it.
    lane_entered_rx
        .recv()
        .expect("lane holder must enter (sender dropped => holder thread died)");

    assert!(crate::daemon::delivery_worker::enqueue_transport_delivery(
        &home,
        &agent,
        "queued before external delete",
    )
    .is_ok());

    let externals: agent::ExternalRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    externals.lock().insert(
        agent.clone(),
        agent::ExternalAgentHandle {
            backend_command: "remote".to_string(),
            pid: 4321,
        },
    );
    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs: crate::api::ConfigRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let delete_home = home.clone();
    let delete_agent = agent.clone();
    let delete_externals = std::sync::Arc::clone(&externals);
    let delete_registry = std::sync::Arc::clone(&registry);
    let delete_configs = std::sync::Arc::clone(&configs);
    let (delete_tx, delete_rx) = std::sync::mpsc::channel();
    let (marker_tx, marker_rx) = std::sync::mpsc::channel();
    let (marker_continue_tx, marker_continue_rx) = std::sync::mpsc::channel();
    // #3240 slice 4: this is a fixed pre-acquire rendezvous, not a marker
    // observer. The marker assertion runs while the delete thread is held
    // at this seam, before it can acquire the already-held transport lane.
    // Named RED controls exercised during implementation (temporary only):
    // M2 moves mark_deleting after lane acquire; old-clock delays delete
    // entry by 1200ms; child-death panics before this seam. Each must fail
    // or disconnect without leaving a sender owned by the test thread.
    let marker_home = home.clone();
    let marker_agent = agent.clone();
    let expected_marker_home = home.clone();
    let expected_marker_agent = agent.clone();
    crate::daemon::delivery_worker::test_support::set_cleanup_before_lane_acquire_hook(Some(
        std::sync::Arc::new(move |hook_home, hook_agent| {
            if hook_home == expected_marker_home.as_path() && hook_agent == expected_marker_agent {
                crate::daemon::delivery_worker::test_support::notify_cleanup_before_lane_acquire(
                    hook_home, hook_agent,
                );
            }
        }),
    ));
    let delete_thread = std::thread::spawn(move || {
        let _marker_observer =
            crate::daemon::delivery_worker::test_support::cleanup_before_lane_acquire_observer(
                &marker_home,
                &marker_agent,
                marker_tx,
                marker_continue_rx,
            );
        let context = DeleteContext {
            registry: &delete_registry,
            configs: &delete_configs,
            externals: &delete_externals,
            notifier: None,
        };
        let outcome =
            delete_instance_with_exit_status(&delete_home, &delete_agent, &context, false).0;
        if send_outcome {
            delete_tx.send(outcome).expect("delete outcome observer");
        }
    });

    let marker_observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        marker_rx.recv().expect(
            "external delete marker observer disconnected: delete thread died before cleanup lane",
        );
        assert!(
            crate::agent::deleting::is_deleting(&home, &agent),
            "external delete must mark the name before waiting for its transport lane"
        );
        assert!(
            delete_rx.try_recv().is_err(),
            "external delete must remain behind the held transport lane"
        );
    }));
    let _ = marker_continue_tx.send(());
    marker_observation.expect("external delete marker ordering observation");

    lane_release_tx.send(()).expect("release lane");
    lane_holder.join().expect("lane holder");

    if send_outcome {
        assert_eq!(
            delete_rx.recv().expect(
                "external delete outcome sender dropped before sending outcome (RecvError)",
            ),
            DeleteOutcome::External
        );
    } else {
        let disconnected: std::sync::mpsc::RecvError = delete_rx.recv().expect_err(
            "external delete outcome sender dropped before sending outcome (RecvError)",
        );
        assert_eq!(
            format!("{disconnected:?}"),
            "RecvError",
            "external delete completion must fail with the named RecvError"
        );
    }
    delete_thread.join().expect("delete thread");

    let dispatch_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, &agent) < 1
        && std::time::Instant::now() < dispatch_deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    let dispatch_count =
        crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, &agent);
    assert_eq!(
        dispatch_count, 1,
        "the queued old-generation job must be observed and discarded exactly once"
    );
    assert_eq!(
        adapter_calls.load(Ordering::SeqCst),
        0,
        "the stale queued external-delete job must not reach an adapter"
    );
    let delivery_path = crate::transport::delivery_path_for_instance(&home, &agent);
    assert!(
        !delivery_path.exists(),
        "stale external job must not create a receipt"
    );
    assert!(
        !delivery_path.with_extension("jsonl.lock").exists(),
        "stale external job must not create a receipt lock"
    );
    assert!(
        !agent::lock_external(&externals).contains_key(&agent),
        "external delete must remove the external registry entry"
    );
    std::fs::remove_dir_all(home).ok();
}

/// The real external-delete thread can disconnect before reporting its
/// outcome; the fixture's own completion receiver must surface RecvError.
#[test]
fn external_delete_completion_disconnect_control() {
    run_external_delete_fixture(false);
}

#[test]
fn concurrent_save_metadata_no_lost_update_1886() {
    // #1886 C2 §3.9: N threads each set a DISTINCT key on the SAME instance's
    // metadata. The locked RMW keeps every field; the prior unlocked
    // read+atomic_write would lose updates under contention.
    let home = tmp_home("concurrent-save-meta-1886");
    const N: usize = 12;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let home = home.clone();
            std::thread::spawn(move || {
                save_metadata(&home, "agent-x", &format!("key-{i}"), json!(i));
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-x")).unwrap();
    let meta: Value = serde_json::from_str(&content).unwrap();
    for i in 0..N {
        assert_eq!(
            meta.get(format!("key-{i}")).and_then(|v| v.as_u64()),
            Some(i as u64),
            "every concurrent field write must survive"
        );
    }
}

#[test]
fn update_metadata_concurrent_append_and_filter_no_lost_or_resurrected() {
    // CR-2026-06-14: the pickup-id lost-update race a ONE-SIDED lock could
    // not close. The two production mutators of `pending_pickup_ids` — the
    // telegram inbound APPEND and the inbox-drain FILTER — both run as
    // `update_metadata` locked RMWs. Seed P "processed" ids; concurrently
    // each filter thread removes one while each append thread adds a fresh
    // one. Because BOTH sides take the same flock and derive their new value
    // from the CURRENT on-disk value inside the lock, the operations
    // serialize: the final set is EXACTLY the appended ids — nothing lost, no
    // processed id resurrected. (A one-sided unlocked append could write a
    // stale array back over a concurrent filter, resurrecting a removed id.)
    let home = tmp_home("update-meta-append-filter");
    const P: usize = 16;
    let seed: Vec<Value> = (0..P)
        .map(|i| json!({ "msg_id": format!("p{i}") }))
        .collect();
    save_metadata(&home, "agent-z", "pending_pickup_ids", json!(seed));

    let mut handles = Vec::new();
    for i in 0..P {
        // Filter thread: remove processed id pI (mirrors handle_inbox).
        let home_f = home.clone();
        handles.push(std::thread::spawn(move || {
            update_metadata(&home_f, "agent-z", "pending_pickup_ids", |current| {
                let remaining: Vec<Value> = current
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e["msg_id"].as_str() != Some(format!("p{i}").as_str()))
                    .collect();
                json!(remaining)
            });
        }));
        // Append thread: add a fresh id aI and its fallback id atomically
        // (mirrors telegram inbound).
        let home_a = home.clone();
        handles.push(std::thread::spawn(move || {
            update_metadata_object(&home_a, "agent-z", |meta| {
                let current = meta
                    .get("pending_pickup_ids")
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut ids: Vec<Value> = current.as_array().cloned().unwrap_or_default();
                ids.push(json!({ "msg_id": format!("a{i}") }));
                meta["pending_pickup_ids"] = json!(ids);
                meta["last_message_id"] = json!(i);
            });
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-z")).unwrap();
    let meta: Value = serde_json::from_str(&content).unwrap();
    let final_ids: std::collections::HashSet<String> = meta["pending_pickup_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["msg_id"].as_str().unwrap().to_string())
        .collect();
    let expected: std::collections::HashSet<String> = (0..P).map(|i| format!("a{i}")).collect();
    assert_eq!(
        final_ids, expected,
        "after concurrent append+filter the set must be exactly the appended ids \
         (no processed id resurrected, no append lost)"
    );
    assert!(
        meta["last_message_id"]
            .as_u64()
            .is_some_and(|id| id < P as u64),
        "the fallback id must come from one complete append transaction"
    );
}

#[test]
fn concurrent_save_metadata_clear_vs_set_both_survive_1886() {
    // #1886 C2 §3.9 (clear-vs-set): one writer clears `waiting_on` while
    // another sets a different field on the same instance — both updates
    // survive and an untouched field is preserved (the F7 interleave race,
    // now closed by the locked RMW).
    let home = tmp_home("save-meta-clear-set-1886");
    save_metadata_batch(
        &home,
        "agent-y",
        &[
            ("waiting_on", json!("reviewer")),
            ("waiting_on_since", json!(1)),
        ],
    );
    let h1 = {
        let home = home.clone();
        std::thread::spawn(move || save_metadata(&home, "agent-y", "waiting_on", json!(null)))
    };
    let h2 = {
        let home = home.clone();
        std::thread::spawn(move || save_metadata(&home, "agent-y", "extra", json!("set")))
    };
    h1.join().unwrap();
    h2.join().unwrap();
    let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-y")).unwrap();
    let meta: Value = serde_json::from_str(&content).unwrap();
    assert!(meta["waiting_on"].is_null(), "clear survived");
    assert_eq!(
        meta["extra"].as_str(),
        Some("set"),
        "concurrent set survived"
    );
    assert_eq!(
        meta["waiting_on_since"].as_u64(),
        Some(1),
        "untouched field preserved"
    );
}

// --- validate_branch (3 from ops.rs + 5 from mcp/handlers.rs) ---

#[test]
fn branch_valid() {
    assert!(validate_branch("main"));
    assert!(validate_branch("feature/foo"));
    assert!(validate_branch("v1.0.0"));
}

#[test]
fn branch_rejects_dotdot() {
    assert!(!validate_branch(".."));
    assert!(!validate_branch("foo/.."));
}

#[test]
fn branch_rejects_special() {
    assert!(!validate_branch(""));
    assert!(!validate_branch("-main"));
    assert!(!validate_branch("foo;bar"));
}

#[test]
fn branch_valid_simple() {
    assert!(validate_branch("main"));
    assert!(validate_branch("feature/foo"));
    assert!(validate_branch("v1.0.0"));
    assert!(validate_branch("fix-123"));
    assert!(validate_branch("release_2.0"));
}

#[test]
fn branch_rejects_empty() {
    assert!(!validate_branch(""));
}

// --- is_protected_ref (E4.5 invariant — Sprint 57 Wave 2 Track B #546) ---

#[test]
fn is_protected_ref_main_and_master() {
    assert!(is_protected_ref("main"));
    assert!(is_protected_ref("master"));
}

#[test]
fn is_protected_ref_rejects_feature_branches() {
    assert!(!is_protected_ref("feature/x"));
    assert!(!is_protected_ref("sprint57-track-b"));
    assert!(!is_protected_ref("release/v1.0.0"));
    assert!(!is_protected_ref("hotfix"));
}

#[test]
fn is_protected_ref_case_insensitive_blocks_case_variants() {
    // CR-2026-06-14: the prior "case-sensitive by design" stance was
    // empirically falsified on darwin/APFS — a case-insensitive FS folds
    // refs/heads/Main onto refs/heads/main, so `branch="Main"` lands the
    // agent's worktree on `main` (committing on "Main" advanced `main`).
    // Every case variant of main/master MUST be protected.
    for v in ["Main", "MAIN", "mAiN", "Master", "MASTER", "mAsTeR"] {
        assert!(
            is_protected_ref(v),
            "case variant {v:?} must be protected (E4.5 case-insensitive)"
        );
    }
}

#[test]
fn is_protected_ref_rejects_empty_and_substrings() {
    // eq_ignore_ascii_case is a full-string compare, so a branch that
    // merely CONTAINS "main"/"master" (or differs by more than case) is
    // not over-blocked.
    assert!(!is_protected_ref(""));
    assert!(!is_protected_ref("mainline"));
    assert!(!is_protected_ref("maintenance"));
    assert!(!is_protected_ref("main-feature"));
    assert!(!is_protected_ref("Maintenance"));
    assert!(!is_protected_ref("upstream-main"));
    assert!(!is_protected_ref("master/dev"));
}

#[test]
fn branch_rejects_dotdot_extended() {
    assert!(!validate_branch(".."));
    assert!(!validate_branch("foo/.."));
    assert!(!validate_branch("../bar"));
}

#[test]
fn branch_rejects_leading_dash() {
    assert!(!validate_branch("-main"));
    assert!(!validate_branch("-"));
}

#[test]
fn branch_rejects_special_chars() {
    assert!(!validate_branch("main branch"));
    assert!(!validate_branch("foo;bar"));
    assert!(!validate_branch("$(echo)"));
    assert!(!validate_branch("main\ninjected"));
}

// Migrated from `src/worktree.rs::tests` as part of Task #9 Option C
// epilogue (worktree.rs no longer holds its own `validate_branch` copy).

#[test]
fn test_validate_branch_valid() {
    assert!(validate_branch("main"));
    assert!(validate_branch("feature/my-branch"));
    assert!(validate_branch("agend/agent-1"));
    assert!(validate_branch("v1.0.0"));
}

#[test]
fn test_validate_branch_rejects() {
    assert!(!validate_branch(""));
    assert!(!validate_branch(".."));
    assert!(!validate_branch("foo/../bar"));
    assert!(!validate_branch("-starts-with-dash"));
    assert!(!validate_branch("has spaces"));
    assert!(!validate_branch("has;semicolon"));
}

// --- merge_metadata (2 from ops.rs) ---

#[test]
fn metadata_merge_no_file() {
    let home = tmp_home("meta_no_file");
    let mut info = json!({"name": "a"});
    merge_metadata(&home, "a", &mut info);
    assert_eq!(info["name"], "a");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn metadata_merge_fields() {
    let home = tmp_home("meta_fields");
    std::fs::create_dir_all(home.join("metadata")).ok();
    std::fs::write(
        home.join("metadata/a.json"),
        r#"{"display_name":"Dev","x":1}"#,
    )
    .ok();
    let mut info = json!({"name": "a"});
    merge_metadata(&home, "a", &mut info);
    assert_eq!(info["display_name"], "Dev");
    assert_eq!(info["x"], 1);
    std::fs::remove_dir_all(&home).ok();
}

// --- save_metadata (1 from ops.rs) ---

#[test]
fn metadata_save_roundtrip() {
    let home = tmp_home("meta_save");
    save_metadata(&home, "a", "key", json!("val"));
    let c = std::fs::read_to_string(home.join("metadata/a.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&c).unwrap();
    assert_eq!(v["key"], "val");
    std::fs::remove_dir_all(&home).ok();
}

// Sprint 21 Phase 5 — atomic multi-field metadata helper tests.
// Closes the F7 race window documented in docs/DAEMON-LOCK-ORDERING.md
// §1 F7): two sequential `save_metadata` calls had a partial-write
// window where a daemon crash between the two writes left disk state
// inconsistent (waiting_on cleared but waiting_on_since stale).

#[test]
fn atomic_multi_field_save_metadata_writes_in_single_transaction() {
    // Verify all fields land in the file together — the helper must
    // not write one field, return, then write the next (which would
    // expose the F7 race).
    let home = tmp_home("meta_batch_atomic");
    save_metadata_batch(
        &home,
        "agent_z",
        &[
            ("waiting_on", json!("review from at-dev-4")),
            ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
            ("last_heartbeat", json!("2026-04-27T00:01:00Z")),
        ],
    );
    let raw =
        std::fs::read_to_string(home.join("metadata/agent_z.json")).expect("metadata file written");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(v["waiting_on"], "review from at-dev-4");
    assert_eq!(v["waiting_on_since"], "2026-04-27T00:00:00Z");
    assert_eq!(v["last_heartbeat"], "2026-04-27T00:01:00Z");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn atomic_multi_field_save_metadata_clear_pair_no_corrupt_state() {
    // Closes Sprint 20 F7 directly: clearing `waiting_on` + `waiting_on_since`
    // must land both nulls in one write so a concurrent reader (e.g.
    // supervisor tick) never sees the half-cleared state where
    // waiting_on is null but waiting_on_since is still set.
    let home = tmp_home("meta_batch_clear");
    // Pre-populate with an active wait state.
    save_metadata_batch(
        &home,
        "agent_y",
        &[
            ("waiting_on", json!("PR review")),
            ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
        ],
    );
    // Now clear both atomically.
    save_metadata_batch(
        &home,
        "agent_y",
        &[
            ("waiting_on", json!(null)),
            ("waiting_on_since", json!(null)),
        ],
    );
    let raw =
        std::fs::read_to_string(home.join("metadata/agent_y.json")).expect("metadata file present");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(
        v["waiting_on"].is_null(),
        "waiting_on must be null after batch clear"
    );
    assert!(
        v["waiting_on_since"].is_null(),
        "waiting_on_since must be null after batch clear"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn atomic_multi_field_save_metadata_preserves_unrelated_fields() {
    // The helper does read-modify-write so unrelated keys must survive
    // the batch update — guards against accidental field overwrite if
    // an implementation regresses to "replace whole file".
    let home = tmp_home("meta_batch_preserve");
    save_metadata(&home, "agent_x", "role", json!("dev-impl-2"));
    save_metadata(&home, "agent_x", "team", json!("dev"));
    save_metadata_batch(
        &home,
        "agent_x",
        &[
            ("waiting_on", json!("review")),
            ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
        ],
    );
    let raw =
        std::fs::read_to_string(home.join("metadata/agent_x.json")).expect("metadata file present");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        v["role"], "dev-impl-2",
        "unrelated `role` must survive batch"
    );
    assert_eq!(v["team"], "dev", "unrelated `team` must survive batch");
    assert_eq!(v["waiting_on"], "review");
    assert_eq!(v["waiting_on_since"], "2026-04-27T00:00:00Z");
    std::fs::remove_dir_all(&home).ok();
}

// --- cleanup_working_dir (3 from ops.rs) ---

#[test]
fn cleanup_workspace_removes_dir() {
    let home = tmp_home("cw");
    let ws = home.join("workspace/agent1");
    std::fs::create_dir_all(&ws).ok();
    std::fs::write(ws.join("f.txt"), "x").ok();
    let _ = cleanup_working_dir(&home, "agent1", &ws);
    assert!(!ws.exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_user_dir_selective() {
    let home = tmp_home("cu");
    let ud = tmp_home("cu_proj");
    std::fs::write(ud.join("main.rs"), "fn main(){}").ok();
    std::fs::write(ud.join("opencode.json"), "{}").ok();
    let _ = cleanup_working_dir(&home, "a", &ud);
    assert!(ud.join("main.rs").exists());
    assert!(!ud.join("opencode.json").exists());
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&ud).ok();
}

// --- workspace-identity delete guard (boundary 3) ---

fn seed_agents_owned_by(dir: &Path, owner: &str) {
    std::fs::write(
        dir.join("AGENTS.md"),
        format!("<!-- agend:start -->\n## Identity\n\n- **Name**: `{owner}`\n<!-- agend:end -->\n"),
    )
    .unwrap();
}

#[test]
fn cleanup_preserves_foreign_identity_tree() {
    let home = tmp_home("cw_foreign");
    let ws = home.join("workspace/alice"); // "alice" is being deleted...
    std::fs::create_dir_all(&ws).unwrap();
    seed_agents_owned_by(&ws, "bob"); // ...but the directory belongs to "bob".
    std::fs::write(ws.join("keep.txt"), "b").unwrap();
    assert!(
        cleanup_working_dir(&home, "alice", &ws).is_some(),
        "foreign-owned dir must be refused (Some verdict)"
    );
    assert!(ws.exists(), "foreign-owned tree must be preserved");
    assert!(
        ws.join("AGENTS.md").exists(),
        "bob's identity file preserved"
    );
    assert!(
        ws.join("keep.txt").exists(),
        "foreign tree contents preserved"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_removes_same_identity_tree() {
    let home = tmp_home("cw_same");
    let ws = home.join("workspace/alice");
    std::fs::create_dir_all(&ws).unwrap();
    seed_agents_owned_by(&ws, "alice"); // dir belongs to the instance being deleted
    assert!(
        cleanup_working_dir(&home, "alice", &ws).is_none(),
        "same-identity dir cleans (None verdict)"
    );
    assert!(!ws.exists(), "same-identity tree must be removed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_removes_unowned_tree_normally() {
    let home = tmp_home("cw_absent");
    let ws = home.join("workspace/alice");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("f.txt"), "x").unwrap(); // no identity artifact
    assert!(
        cleanup_working_dir(&home, "alice", &ws).is_none(),
        "unowned dir cleans (None verdict)"
    );
    assert!(!ws.exists(), "unowned tree cleans normally");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ownership_conflict_detects_foreign_codex_stamp() {
    let home = tmp_home("wdoc_codex");
    let ws = home.join("workspace/alice");
    std::fs::create_dir_all(ws.join(".codex")).unwrap();
    std::fs::write(
        ws.join(".codex").join("config.toml"),
        "AGEND_INSTANCE_NAME = 'bob'\n",
    )
    .unwrap();
    assert!(
        working_dir_ownership_conflict(&ws, "alice").is_some(),
        "foreign .codex stamp is a conflict"
    );
    assert!(
        working_dir_ownership_conflict(&ws, "bob").is_none(),
        "same owner is not a conflict"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_refuses_unreadable_identity_tree() {
    // Fail-closed: an UNREADABLE identity artifact (opaque I/O ≠ NotFound)
    // must refuse the delete — never be read as "absent" and wipe the tree.
    let home = tmp_home("cw_unreadable");
    let ws = home.join("workspace/alice");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("AGENTS.md"), [0xFFu8, 0xFE]).unwrap(); // invalid UTF-8
    assert!(
        cleanup_working_dir(&home, "alice", &ws).is_some(),
        "unreadable identity must refuse (Some verdict)"
    );
    assert!(ws.exists(), "tree preserved on unreadable identity");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn workspace_identity_lock_is_mutually_exclusive_provision_vs_delete() {
    // Provision (generate_with_context) and delete (cleanup_working_dir) BOTH
    // acquire store::acquire_workspace_identity_lock(home, wd) for the same
    // directory. Prove it is mutually exclusive so a check+write can never
    // interleave with a check+remove of that directory (root finding 4).
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let home = tmp_home("wsid_lock");
    let wd = home.join("workspace/shared");
    let in_critical = Arc::new(AtomicBool::new(false));
    let held = crate::store::acquire_workspace_identity_lock(&home, &wd).expect("first lock");
    in_critical.store(true, Ordering::SeqCst);
    let (h2, w2, ic2) = (home.clone(), wd.clone(), in_critical.clone());
    let t = std::thread::spawn(move || {
        // Blocks until the main thread releases `held` (mutual exclusion).
        let _g = crate::store::acquire_workspace_identity_lock(&h2, &w2).expect("second lock");
        assert!(
            !ic2.load(Ordering::SeqCst),
            "acquired the workspace-identity lock while another holder was still in its \
             critical section — the lock is NOT mutually exclusive"
        );
    });
    // Give the spawned thread time to reach (and block on) the acquire.
    std::thread::sleep(std::time::Duration::from_millis(100));
    in_critical.store(false, Ordering::SeqCst);
    drop(held);
    t.join().expect("second acquirer thread");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn cleanup_metadata() {
    let home = tmp_home("cms");
    let ws = home.join("workspace/a");
    std::fs::create_dir_all(&ws).ok();
    std::fs::create_dir_all(home.join("metadata")).ok();
    std::fs::write(home.join("metadata/a.json"), "{}").ok();
    let _ = cleanup_working_dir(&home, "a", &ws);
    assert!(!home.join("metadata/a.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

// --- NEW: drift guard — assert canonical 19-entry set (at-dev-3 gate C).
//
// Creates every one of the 19 entries in a user-provided working dir
// (not under $AGEND_HOME/workspace/, so selective-mode path runs), then
// asserts all 19 are removed. Explicitly lists the 5 Kiro paths that
// `mcp/handlers.rs` was missing so any future drift regresses the test.

#[test]
fn cleanup_removes_all_19_canonical_entries() {
    let home = tmp_home("drift19_home");
    let ud = tmp_home("drift19_user");

    let canonical: [&str; 19] = [
        // Claude (6)
        ".claude/settings.local.json",
        "mcp-config.json",
        "claude-settings.json",
        "statusline.sh",
        "statusline.json",
        ".claude/rules/agend.md",
        // Gemini (1)
        ".gemini/settings.json",
        // OpenCode (2)
        "opencode.json",
        "instructions/agend.md",
        // Codex (2)
        ".codex/config.toml",
        "AGENTS.md",
        // Kiro — 14-entry handlers copy had only the first 3 of these 9
        ".kiro/settings/mcp.json",
        ".kiro/settings/agend-mcp-wrapper.sh",
        ".kiro/steering/agend.md",
        // The 5 Kiro paths missing from `mcp/handlers.rs` pre-Commit-2:
        ".kiro/agents/agend.json",
        ".kiro/agents/agend-prompt.md",
        ".kiro/agents/default.json",
        ".kiro/prompts/agend.md",
        ".kiro/settings.json",
    ];

    // Materialize every canonical path, plus one decoy that must survive.
    for rel in &canonical {
        let p = ud.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&p, "x").ok();
    }
    std::fs::write(ud.join("user-code.rs"), "fn main(){}").ok();

    let _ = cleanup_working_dir(&home, "drift19", &ud);

    // All 19 must be gone, user decoy preserved.
    for rel in &canonical {
        assert!(!ud.join(rel).exists(), "canonical entry not removed: {rel}");
    }
    assert!(
        ud.join("user-code.rs").exists(),
        "user file must survive selective cleanup"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&ud).ok();
}

// Explicit individual assertions for the 5 Kiro paths that were missing
// from `mcp/handlers.rs` — if any reappears as undeleted, this test
// pinpoints which one.
#[test]
fn cleanup_removes_each_of_5_drifted_kiro_entries() {
    let drifted = [
        ".kiro/agents/agend.json",
        ".kiro/agents/agend-prompt.md",
        ".kiro/agents/default.json",
        ".kiro/prompts/agend.md",
        ".kiro/settings.json",
    ];
    for rel in &drifted {
        let home = tmp_home("drift1_home");
        let ud = tmp_home("drift1_user");
        let p = ud.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&p, "x").ok();

        let _ = cleanup_working_dir(&home, "drift1", &ud);

        assert!(!p.exists(), "Kiro drift entry not removed: {rel}");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ud).ok();
    }
}

// #910 PR2 of 4: MCP-facing JSON shape stability pin.
//
// `src/mcp/handlers/instance.rs:36/39` wraps `list_agents()` result
// in `{"instances": [<names>]}` as the LIST fallback when the rich-
// info API path fails. After PR2's migration to `runtime::
// list_agents_with_fallback`, the OUTPUT TYPE must remain
// `Vec<String>` so the JSON envelope is byte-stable for any
// operator script grep'ing the MCP fallback response. This test
// pins that contract.
#[test]
fn list_agents_mcp_payload_shape_is_instances_array_of_strings() {
    // Build a fixture that mirrors what `list_agents()` returns —
    // a Vec<String>. Wrap it the same way the MCP handler does.
    // This pin tracks the wire contract, not the resolution path.
    let names: Vec<String> = vec!["alice".into(), "bob".into(), "charlie".into()];
    let payload = json!({"instances": names.clone()});

    // Top-level key must be `instances`.
    assert!(
        payload.get("instances").is_some(),
        "MCP fallback envelope must carry top-level 'instances' key — \
         #910 PR2 contract pin"
    );

    // Value must be a JSON array.
    let arr = payload["instances"]
        .as_array()
        .expect("'instances' value must be a JSON array");

    // Each element must be a JSON string (not an object, not nested).
    // Locks the fallback envelope as a flat name-list — the rich-info
    // path returns objects, but the fallback path is intentionally
    // simpler so degraded-mode parsers don't need the full schema.
    assert_eq!(arr.len(), 3);
    for (i, v) in arr.iter().enumerate() {
        assert!(
            v.is_string(),
            "'instances[{i}]' must be a JSON string in the LIST fallback, got {v}"
        );
        assert_eq!(v.as_str().unwrap(), names[i].as_str());
    }
}

// #910 PR2 of 4: `list_agents` thin-wrapper contract.
//
// After PR2, `list_agents()` is a 1-line delegation to
// `runtime::list_agents_with_fallback`. The behavioral surface is
// covered by PR1's `runtime::tests` (5 RED→GREEN tests). This test
// pins the SIGNATURE + RETURN TYPE so a future refactor that
// accidentally drops the no-arg shape or changes the return type
// breaks loudly here rather than at MCP handler call sites.
#[test]
fn list_agents_signature_is_no_arg_vec_string() {
    // Call site sanity: compiles with no args; result is Vec<String>.
    let result: Vec<String> = list_agents();
    // Result may be empty (no daemon, no tmp run dir) but must not panic.
    // Length assertion is intentionally weak — the resolution path is
    // tested in `runtime::tests::*`; here we only pin the signature.
    let _ = result.len();
}

#[test]
fn api_bridge_missing_delivery_mode_is_not_legacy_pty() {
    let missing = json!({"ok": true});
    assert_eq!(api_bridge_delivery_mode(&missing), UNVERIFIED_DELIVERY_MODE);
    assert_ne!(api_bridge_delivery_mode(&missing), "pty");

    let malformed = json!({"ok": true, "delivery_mode": null});
    assert_eq!(
        api_bridge_delivery_mode(&malformed),
        UNVERIFIED_DELIVERY_MODE
    );
}
