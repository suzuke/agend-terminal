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
