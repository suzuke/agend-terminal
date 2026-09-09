//! Operator recovery CLI must refuse ordinary agent contexts before API access.

#![allow(clippy::unwrap_used)]

#[test]
fn recovery_admin_cli_refuses_agent_environment_before_connecting() {
    let home =
        std::env::temp_dir().join(format!("agend-admin-job-recovery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .args([
            "admin",
            "resolve-job-recovery",
            "j-test",
            "--attempt",
            "1",
            "--cleanup-confirmed",
            "--result",
            "external cleanup confirmed",
        ])
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "job-worker-test")
        .current_dir(&home)
        .output()
        .unwrap();
    let mutated_store = home.join("schedule-jobs.json").exists();
    std::fs::remove_dir_all(&home).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("operator-only"),
        "unexpected refusal: {stderr}"
    );
    assert!(
        !stderr.contains("no active daemon"),
        "must refuse before API access: {stderr}"
    );
    assert!(!mutated_store);
}

#[test]
fn recovery_admin_cli_resolves_seeded_run_against_isolated_daemon() {
    let home = std::env::temp_dir().join(format!(
        "agend-admin-job-recovery-success-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("fleet.yaml"), "instances: {}\n").unwrap();
    std::fs::create_dir_all(home.join("artifacts")).unwrap();
    std::fs::write(
        home.join("schedule-jobs.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "watermarks": {},
            "overlap_skips": {},
            "runs": [{
                "id": "r-cli-success",
                "schedule_id": "s-cli-success",
                "scheduled_at": 1,
                "created_by": "operator",
                "message": "isolated recovery fixture",
                "config": {
                    "backends": ["codex"],
                    "artifact_directory": home.join("artifacts"),
                    "timeout_secs": 60,
                    "max_attempts": 1,
                    "retry_delay_secs": 1,
                    "output_context": "",
                    "notification": null
                },
                "phase": "stopping",
                "revision": 0,
                "attempt": {
                    "number": 1,
                    "name": "worker-cli-success",
                    "uuid": null,
                    "backend": "codex",
                    "started_at": 1
                },
                "previous_attempts": [],
                "task_id": null,
                "result": null,
                "error": "external worker stopped",
                "next_attempt_at": 1,
                "deadline": 2,
                "cleanup_pending": true,
                "recovery_required": true,
                "recovery_resolution": null,
                "task_settled": false,
                "notification": "not_requested",
                "notification_receipt": null,
                "notification_error": null
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .args(["start", "--foreground", "--fleet"])
        .arg(home.join("fleet.yaml"))
        .env("AGEND_HOME", &home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let run_dir = home.join("run");
    let mut api_ready = false;
    for _ in 0..100 {
        if std::fs::read_dir(&run_dir)
            .ok()
            .into_iter()
            .flatten()
            .any(|entry| entry.ok().is_some_and(|entry| entry.path().join("api.port").exists()))
        {
            api_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(api_ready, "isolated daemon did not publish api.port");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .args([
            "admin",
            "resolve-job-recovery",
            "r-cli-success",
            "--attempt",
            "1",
            "--cleanup-confirmed",
            "--result",
            "operator verified worker stopped and delivery reconciled",
        ])
        .env("AGEND_HOME", &home)
        .current_dir(&home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("schedule-jobs.json")).unwrap()).unwrap();
    assert_eq!(state["runs"][0]["recovery_required"], false);
    assert_eq!(state["runs"][0]["phase"], "failed");

    let _ = std::process::Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .arg("stop")
        .env("AGEND_HOME", &home)
        .output();
    let _ = daemon.wait();
    if daemon.try_wait().unwrap().is_none() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    std::fs::remove_dir_all(&home).unwrap();
}
