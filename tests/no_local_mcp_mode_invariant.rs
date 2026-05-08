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

// ── Sprint 56 Track I-Phase2b (#531): no fallback to local mode ──

const MCP_CONFIG_RS: &str = include_str!("../src/mcp_config.rs");
const MAIN_RS: &str = include_str!("../src/main.rs");

#[test]
fn bridge_binary_path_does_not_fall_back_to_local_mode() {
    // Phase 2b removes the `(binary_path(), vec!["mcp"])` fallback
    // line that previously fired whenever the bridge binary wasn't
    // alongside the main binary. Operators on the broken Windows
    // path always landed there — that's the load-bearing #531 fix.
    //
    // We pin the absence of `vec!["mcp"]` (the fallback's tell) in
    // `mcp_config.rs::bridge_binary_path`. A future regression that
    // accidentally restores the fallback would re-introduce the
    // silent-drop class fault on Windows.
    assert!(
        !MCP_CONFIG_RS.contains("vec![\"mcp\"]"),
        "Phase 2b fail-loud invariant: src/mcp_config.rs must not \
         contain the `vec![\"mcp\"]` fallback that previously made \
         backends spawn `agend-terminal mcp` instead of the bridge. \
         See docs/RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md."
    );
}

#[test]
fn commands_mcp_emits_deprecation_warning_in_one_sprint_window() {
    // Phase 2b keeps `Commands::Mcp` enum + handler for one-Sprint
    // backwards compat (Sprint 57+ removes outright per dispatch).
    // The handler must emit a deprecation eprintln so operators with
    // hand-edited mcp.json see ONE clear signal before the Sprint 57
    // removal lands.
    let mcp_arm_idx = MAIN_RS
        .find("Some(Commands::Mcp)")
        .expect("main.rs must still carry the Commands::Mcp arm during deprecation window");
    let arm_end_relative = MAIN_RS[mcp_arm_idx + 1..]
        .find("Some(Commands::")
        .expect("at least one more Commands variant must follow Mcp");
    let arm_section = &MAIN_RS[mcp_arm_idx..mcp_arm_idx + 1 + arm_end_relative];
    assert!(
        arm_section.contains("DEPRECATED"),
        "Phase 2b deprecation invariant: Commands::Mcp arm must emit \
         a `DEPRECATED:`-prefixed eprintln so operators see one clear \
         deprecation signal before Sprint 57's removal."
    );
    assert!(
        arm_section.contains("Sprint 57"),
        "Deprecation message must name the removal Sprint so operators \
         have a concrete migration deadline."
    );
}

#[test]
fn mcp_config_emits_fatal_when_bridge_missing() {
    // Pin the FATAL log + eprintln pair in `bridge_binary_path` so a
    // future edit can't silently swap them back to a fallback path.
    let bbp_idx = MCP_CONFIG_RS
        .find("fn bridge_binary_path()")
        .expect("bridge_binary_path() helper must still exist");
    let bbp_end = MCP_CONFIG_RS[bbp_idx..]
        .find("\nfn ")
        .map(|n| bbp_idx + n)
        .unwrap_or(MCP_CONFIG_RS.len());
    let bbp_body = &MCP_CONFIG_RS[bbp_idx..bbp_end];
    assert!(
        bbp_body.contains("FATAL"),
        "bridge_binary_path must emit a FATAL log when the bridge is \
         missing — Phase 2b's fail-loud contract. Body was:\n{bbp_body}"
    );
    // Specific signal: tracing::error! at FATAL level for daemon log,
    // eprintln! for direct stderr (independent of tracing config —
    // mirrors Track H2 #525 item 5 pattern).
    assert!(
        bbp_body.contains("tracing::error!"),
        "Must emit tracing::error! for daemon-log audit trail."
    );
    assert!(
        bbp_body.contains("eprintln!"),
        "Must emit eprintln! for direct-stderr visibility."
    );
}
