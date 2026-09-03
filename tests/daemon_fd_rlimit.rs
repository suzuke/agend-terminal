//! Production-entry contracts for daemon file-descriptor limit self-healing.

#[test]
fn daemon_start_doctor_and_emfile_paths_are_wired() {
    let main = include_str!("../src/main.rs");
    let cli = include_str!("../src/cli.rs");
    let api = include_str!("../src/api/mod.rs");
    let persistence = include_str!("../src/macros.rs");

    assert!(main.contains("resource_limits::raise_daemon_nofile_limit"));
    assert!(cli.contains("resource_limits::doctor_fd_usage"));
    assert!(api.contains("resource_limits::record_fd_exhaustion"));
    assert!(persistence.contains("resource_limits::record_fd_exhaustion"));
}

#[cfg(unix)]
#[test]
fn doctor_reports_live_fd_usage_against_the_soft_limit() {
    let home = std::env::temp_dir().join(format!(
        "agend-fd-doctor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("temp home");
    let output = assert_cmd::Command::cargo_bin("agend-terminal")
        .expect("agend-terminal binary")
        .env("AGEND_HOME", &home)
        .arg("doctor")
        .output()
        .expect("run doctor");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("File descriptors:"), "stdout:\n{stdout}");
    assert!(stdout.contains('/'), "current/soft missing:\n{stdout}");
    std::fs::remove_dir_all(home).ok();
}
