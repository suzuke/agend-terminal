//! Sprint 56 Track I-Phase2a (#531) — packaging-regression invariant.
//!
//! Production builds must ship `agend-mcp-bridge` alongside
//! `agend-terminal` in every release artifact. The
//! `mcp_config::bridge_binary_path` fallback at `src/mcp_config.rs:34`
//! lands operators on the broken `agend-terminal mcp` path whenever
//! the bridge isn't found next to the main binary, which is the root
//! cause of issue #531 (Windows daemon overwrites mcp.json with the
//! local-mode command). The reporter's symptom resolves only when
//! both binaries land in the same package on every supported
//! platform.
//!
//! This test parses `.github/workflows/release.yml` and asserts the
//! 3 packaging steps (Unix tar, Windows zip, AppImage stage) all
//! reference the bridge. It catches release-workflow edits that
//! drop the bridge — the regression that originally produced #531.
//!
//! ## Why text-parse over shell-out
//!
//! An alternative would be to `cargo build --release` and verify the
//! bridge exists in `target/release/`. That's heavier (multi-minute
//! release build per test invocation) and tests the wrong
//! abstraction layer — the regression we care about is in the
//! workflow text, not the local cargo build (which already builds
//! the bridge by default per `cargo metadata`). Text-parse catches
//! the regression at the source and runs in milliseconds.
//!
//! ## Empirical regression-proof anchor
//!
//! Removing `agend-mcp-bridge` from any of the three packaging
//! steps in release.yml flips this test red. Restoring → green.
//! See PR #N (Track I-Phase2a) for the captured FAIL signature.

const RELEASE_YML: &str = include_str!("../.github/workflows/release.yml");

#[test]
fn release_workflow_packages_bridge_in_unix_tar_step() {
    // The Unix tar packaging step (line 77+ at the time of writing)
    // gathers the binaries into a tar.gz. The current operator-
    // visible artifact for Linux + macOS distributions.
    let tar_idx = RELEASE_YML
        .find("tar czf")
        .expect("release.yml must have a `tar czf` packaging step");
    // Bracket the section: from `tar czf` to the next workflow step
    // (lines beginning with `      - ` after a newline). Scope the
    // assertion to ONE packaging step so the workflow-author's
    // intent is unambiguous.
    let section_end = RELEASE_YML[tar_idx..]
        .find("\n      - ")
        .map(|n| tar_idx + n)
        .unwrap_or(RELEASE_YML.len());
    let section = &RELEASE_YML[tar_idx..section_end];
    assert!(
        section.contains("agend-mcp-bridge"),
        "Unix tar packaging step must include `agend-mcp-bridge` so \
         release tarballs ship both binaries — Phase 2a fix for #531. \
         Section was:\n{section}"
    );
}

#[test]
fn release_workflow_packages_bridge_in_windows_zip_step() {
    // The Windows zip packaging step uses PowerShell's
    // Compress-Archive. Same rationale as the tar arm.
    let zip_idx = RELEASE_YML
        .find("Compress-Archive")
        .expect("release.yml must have a `Compress-Archive` packaging step");
    let section_end = RELEASE_YML[zip_idx..]
        .find("\n      - ")
        .map(|n| zip_idx + n)
        .unwrap_or(RELEASE_YML.len());
    let section = &RELEASE_YML[zip_idx..section_end];
    assert!(
        section.contains("agend-mcp-bridge"),
        "Windows zip packaging step must include `agend-mcp-bridge.exe` \
         so the release zip ships both binaries — Phase 2a fix for #531. \
         Section was:\n{section}"
    );
}

#[test]
fn release_workflow_includes_bridge_in_appimage_stage() {
    // The AppImage stage copies binaries into AppDir/usr/bin/. The
    // bridge must land there too so AppImage users hit the same
    // post-Phase-2a fix as tar / zip users.
    let stage_idx = RELEASE_YML
        .find("AppDir/usr/bin/agend-terminal")
        .expect("release.yml must have an AppImage stage");
    // Bracket: from this cp line to the next blank line followed by
    // a non-indented `- ` step or the end of the script block.
    let section_end = RELEASE_YML[stage_idx..]
        .find("# Validate")
        .map(|n| stage_idx + n)
        .unwrap_or(RELEASE_YML.len());
    let section = &RELEASE_YML[stage_idx..section_end];
    assert!(
        section.contains("agend-mcp-bridge"),
        "AppImage stage must copy `agend-mcp-bridge` into \
         AppDir/usr/bin/ so AppImage installs ship both binaries — \
         Phase 2a fix for #531. Section was:\n{section}"
    );
}

#[test]
fn cargo_declares_agend_mcp_bridge_bin_target() {
    // Belt-and-suspenders: even with the workflow correctly
    // packaging the bridge, the build only produces it if the bin
    // target is declared. `src/bin/agend-mcp-bridge.rs` is the
    // automatic discovery path; some packages also declare an
    // explicit `[[bin]]` block. Either is fine; both missing is
    // the regression this guards against.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let cargo_toml = std::path::Path::new(manifest_dir).join("Cargo.toml");
    let src_bin = std::path::Path::new(manifest_dir).join("src/bin/agend-mcp-bridge.rs");
    let cargo_content = std::fs::read_to_string(&cargo_toml).expect("Cargo.toml must exist");
    assert!(
        cargo_content.contains("agend-mcp-bridge") || src_bin.exists(),
        "agend-mcp-bridge bin target must be declared either in \
         Cargo.toml [[bin]] block or as src/bin/agend-mcp-bridge.rs — \
         the workflow assertions above would silently pass while the \
         build itself failed if both are missing."
    );
}
