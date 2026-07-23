//! Integration test for AGEND_CAPTURE_FIXTURES passive tee (issue #704).
#![allow(clippy::unwrap_used)]

use agend_terminal::capture::make_capture_writer;
use serial_test::serial;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-capture-test-{tag}"));
    let _ = std::fs::remove_dir_all(&dir); // clear any leftovers from previous run
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[serial(capture_env)]
fn capture_writer_writes_cap_and_meta_when_env_set() {
    let home = tmp_home("writer");
    let agent = "test-capture-agent";
    let backend = "shell";

    std::env::set_var("AGEND_CAPTURE_FIXTURES", "1");
    let mut writer = make_capture_writer(Some(&home), agent, backend);

    let payload = b"hello capture world\r\n";
    writer.write(payload);

    drop(writer);
    std::env::remove_var("AGEND_CAPTURE_FIXTURES");

    let cap_dir = home.join("captures").join(agent);
    let cap_files: Vec<_> = std::fs::read_dir(&cap_dir)
        .expect("captures dir must exist")
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("cap"))
        .collect();
    assert_eq!(cap_files.len(), 1, "exactly one .cap file expected");

    let cap_path = cap_files[0].path();
    let cap_bytes = std::fs::read(&cap_path).unwrap();
    assert_eq!(cap_bytes, payload, ".cap content must match written bytes");

    let meta_path = {
        let mut s = cap_path.clone().into_os_string();
        s.push(".meta.json");
        PathBuf::from(s)
    };
    assert!(meta_path.exists(), ".meta.json sidecar must exist");
    let meta_json = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta["backend"].as_str(), Some(backend));
    assert_eq!(meta["agent_name"].as_str(), Some(agent));
    assert_eq!(meta["byte_count"].as_u64(), Some(payload.len() as u64));

    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[serial(capture_env)]
fn capture_writer_is_noop_when_env_unset() {
    std::env::remove_var("AGEND_CAPTURE_FIXTURES");
    let home = tmp_home("noop");
    let mut writer = make_capture_writer(Some(&home), "agent", "shell");
    writer.write(b"should be ignored");
    drop(writer);
    assert!(
        !home.join("captures").exists(),
        "no captures dir when env unset"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn capture_backend_cli_writes_cap_and_meta() {
    let home = tmp_home("backend-cli");
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();

    let fake_claude = bin_dir.join("claude");
    std::fs::write(
        &fake_claude,
        "#!/bin/sh\nprintf 'capture-cli-regression\\n'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_claude).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_claude, permissions).unwrap();

    let path = std::env::join_paths(std::iter::once(bin_dir).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .args([
            "capture",
            "backend",
            "--backend",
            "claude",
            "--seconds",
            "1",
        ])
        .env("AGEND_HOME", &home)
        .env("AGEND_CAPTURE_FIXTURES", "1")
        .env("PATH", path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "capture backend failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cap_dir = home.join("captures").join("capture-claude");
    let cap_files: Vec<_> = std::fs::read_dir(&cap_dir)
        .expect("capture backend must create its capture directory")
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("cap"))
        .collect();
    assert_eq!(cap_files.len(), 1, "exactly one .cap file expected");

    let mut meta_path = cap_files[0].path().into_os_string();
    meta_path.push(".meta.json");
    assert!(
        PathBuf::from(meta_path).exists(),
        ".meta.json sidecar must exist"
    );

    std::fs::remove_dir_all(&home).ok();
}
