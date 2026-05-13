//! Integration test for AGEND_CAPTURE_FIXTURES passive tee (issue #704).
#![allow(clippy::unwrap_used)]
//!
//! Tests CaptureSink behaviour directly without requiring a running daemon.
//! Gate the full spawn variant behind AGEND_LIVE_BACKEND_TEST=1.

use std::path::PathBuf;

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-capture-test-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn capture_sink_writes_cap_and_meta_when_env_set() {
    let home = tmp_home("sink");
    let agent = "test-capture-agent";
    let backend = "shell";

    std::env::set_var("AGEND_CAPTURE_FIXTURES", "1");
    let mut sink = agend_terminal::capture::CaptureSink::new_if_enabled(&home, agent, backend)
        .expect("sink must be created when AGEND_CAPTURE_FIXTURES=1");

    let payload = b"hello capture world\r\n";
    sink.write(payload);
    assert_eq!(sink.byte_count, payload.len() as u64);

    // Drop triggers meta sidecar flush.
    drop(sink);
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
fn capture_sink_is_none_when_env_unset() {
    std::env::remove_var("AGEND_CAPTURE_FIXTURES");
    let home = tmp_home("no-sink");
    let result = agend_terminal::capture::CaptureSink::new_if_enabled(&home, "agent", "shell");
    assert!(result.is_none(), "sink must be None when env var is unset");
    std::fs::remove_dir_all(&home).ok();
}
