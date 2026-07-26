//! The release gate must read the published root package version from Cargo's
//! package metadata, not the first `version =` line in a workspace manifest.

use std::process::Command;

const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

#[test]
fn release_gate_selects_agend_terminal_package_version() {
    assert!(
        RELEASE_WORKFLOW.contains("cargo metadata --no-deps --format-version=1"),
        "release gate must resolve versions through Cargo package metadata"
    );
    assert!(
        RELEASE_WORKFLOW.contains(r#"select(.name == "agend-terminal") | .version"#),
        "release gate must select the published agend-terminal package by name"
    );
    assert!(
        !RELEASE_WORKFLOW.contains("grep -m1 '^version = ' Cargo.toml"),
        "release gate must not let workspace.package shadow the root package version"
    );

    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .expect("cargo metadata must be runnable in the release workspace");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata must emit valid JSON");
    let root = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package["name"].as_str() == Some("agend-terminal"))
        })
        .expect("cargo metadata must include the agend-terminal package");
    assert_eq!(
        root["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "the release gate must compare the root package version, not workspace.package"
    );
}
