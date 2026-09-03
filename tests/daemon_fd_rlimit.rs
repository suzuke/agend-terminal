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
    let fd_line = stdout
        .lines()
        .find(|line| {
            line.trim_start()
                .starts_with("File descriptors (this process):")
        })
        .unwrap_or_else(|| panic!("file-descriptor line missing:\n{stdout}"));
    let usage = regex::Regex::new(
        r"^  File descriptors \(this process\): [0-9]+/[0-9]+ \([0-9]+\.[0-9]%\)(?: — WARNING: near file descriptor limit)?$",
    )
    .expect("fd usage regex");
    assert!(
        usage.is_match(fd_line),
        "malformed fd usage line: {fd_line}"
    );
    std::fs::remove_dir_all(home).ok();
}
