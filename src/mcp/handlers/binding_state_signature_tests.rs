//! PR2 — `binding_state.signature_valid` pins (architecture F2 / d-20260711000700914571-1).
//!
//! Loaded via `#[path]` from `binding_state.rs` so the production handler file
//! stays under the MCP-handler LOC ceiling.
//!
//! Exercises the **real** production entry `handle_binding_state` (not a
//! reimplemented matcher). Cases:
//! - valid bind_full-signed binding → signature_valid true
//! - tampered body with stale sidecar → signature_valid false (still bound)
//! - missing sidecar → signature_valid false
//!
//! RED against pre-PR2: the key is absent → as_bool() is None.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::handle_binding_state;
use serde_json::json;
use std::path::{Path, PathBuf};

fn tmp_home(suffix: &str) -> PathBuf {
    let h = std::env::temp_dir().join(format!(
        "agend-binding-sig-{}-{}-{}",
        std::process::id(),
        suffix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn binding_dir(home: &Path, agent: &str) -> PathBuf {
    crate::paths::runtime_dir(home).join(agent)
}

/// Cold-load a body (+ optional sidecar) after clearing the process-global
/// binding index via `unbind` (which also deletes files).
fn plant_binding(home: &Path, agent: &str, body: &str, sig: Option<&str>) {
    crate::binding::unbind(home, agent);
    let dir = binding_dir(home, agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("binding.json"), body.as_bytes()).unwrap();
    if let Some(s) = sig {
        std::fs::write(dir.join("binding.json.sig"), s.as_bytes()).unwrap();
    }
}

/// RED: production binding_state must surface whether the HMAC sidecar verifies
/// against the on-disk binding body — same primitive/tag/sidecar contract as
/// the shim (`agentic_git_core::integrity_core::verify` / `binding.json.sig`).
#[test]
fn binding_state_signature_valid_true_false_missing() {
    let home = tmp_home("sig");
    let agent = "sig-agent";
    let wt = home.join("wt");
    let src = home.join("src");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::create_dir_all(&src).unwrap();

    // Happy path: bind_full issues body + sidecar via the production signer.
    crate::binding::bind_full(&home, agent, "t-sig", "feat/sig", &wt, &src, false)
        .expect("bind_full must succeed for hermetic home");

    let dir = binding_dir(&home, agent);
    assert!(
        dir.join("binding.json.sig").is_file(),
        "fixture: bind_full must write binding.json.sig"
    );
    let original_body = std::fs::read_to_string(dir.join("binding.json")).unwrap();
    let original_sig = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap();

    let r = handle_binding_state(&home, &json!({"instance": agent}), &None);
    assert_eq!(r["bound"].as_bool(), Some(true), "bound: {r}");
    assert_eq!(
        r["signature_valid"].as_bool(),
        Some(true),
        "valid daemon-signed binding must report signature_valid=true: {r}"
    );

    // Tamper body, keep original sidecar.
    let mut body: serde_json::Value = serde_json::from_str(&original_body).unwrap();
    body["branch"] = json!("feat/tampered");
    let tampered = serde_json::to_string_pretty(&body).unwrap();
    plant_binding(&home, agent, &tampered, Some(&original_sig));

    let r_tampered = handle_binding_state(&home, &json!({"instance": agent}), &None);
    assert_eq!(
        r_tampered["bound"].as_bool(),
        Some(true),
        "daemon binding_state still bound on parseable body (observability): {r_tampered}"
    );
    assert_eq!(
        r_tampered["signature_valid"].as_bool(),
        Some(false),
        "tampered body with stale sig must report signature_valid=false: {r_tampered}"
    );

    // Missing sidecar.
    plant_binding(&home, agent, &original_body, None);
    let r_missing = handle_binding_state(&home, &json!({"instance": agent}), &None);
    assert_eq!(r_missing["bound"].as_bool(), Some(true), "{r_missing}");
    assert_eq!(
        r_missing["signature_valid"].as_bool(),
        Some(false),
        "missing sidecar must report signature_valid=false: {r_missing}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
