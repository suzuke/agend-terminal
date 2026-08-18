//! Phase-2 protocol delivery parity RED→GREEN contract.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;

fn cmd() -> Command {
    Command::cargo_bin("agend-terminal").expect("agend-terminal binary")
}

fn isolated_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-protocol-delivery-{tag}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&home).expect("isolated home");
    home
}

#[test]
fn protocol_markdown_is_lf_pinned_and_translation_is_non_normative() {
    let attrs = fs::read_to_string(".gitattributes").expect("protocol LF attributes");
    assert!(
        attrs.lines().any(|line| {
            let line = line.trim();
            line == "docs/*.md text eol=lf" || line == "docs/**.md text eol=lf"
        }),
        ".gitattributes must pin protocol markdown to LF"
    );

    let translation =
        fs::read_to_string("docs/FLEET-DEV-PROTOCOL.zh-TW.md").expect("zh-TW protocol");
    let header = translation.lines().take(8).collect::<Vec<_>>().join("\n");
    assert!(
        header.to_ascii_lowercase().contains("non-normative") || header.contains("非規範"),
        "zh-TW protocol must identify itself as non-normative: {header}"
    );
}

#[test]
fn protocol_source_declares_exact_identity_and_atomic_result_contract() {
    let source = fs::read_to_string("src/protocol.rs").expect("protocol source");
    for needle in [
        "pub struct ProtocolIdentity",
        "source_kind",
        "content_sha256",
        "embedded_sha256",
        "build_sha",
        "build_dirty",
        "pub fn extract_default(home: &Path) -> anyhow::Result",
        "crate::store::atomic_write",
    ] {
        assert!(
            source.contains(needle),
            "protocol source must declare {needle}"
        );
    }
    assert!(
        !source.contains("let _ = std::fs::write"),
        "protocol extraction must not silently discard write errors"
    );
}

#[test]
fn production_heal_audit_documents_global_interest_cache_refresh() {
    let source = fs::read_to_string("src/protocol.rs").expect("protocol source");
    let body = source
        .split("fn emit_default_healed_audit")
        .nth(1)
        .and_then(|rest| rest.split("fn status_for_identity").next())
        .expect("default-heal audit function body");
    assert!(
        body.contains("rebuild_interest_cache")
            && body.contains("scoped subscriber can be installed")
            && body.contains("does not alter protocol state"),
        "production audit cache refresh must carry its scoped-subscriber rationale and state-isolation guarantee"
    );
}

#[test]
fn doctor_reports_protocol_identity_and_resolution_state() {
    let home = isolated_home("doctor");
    cmd()
        .env("AGEND_HOME", &home)
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("protocol"))
        .stdout(predicate::str::contains("content_sha256"))
        .stdout(predicate::str::contains("embedded_sha256"));
    fs::remove_dir_all(home).ok();
}
