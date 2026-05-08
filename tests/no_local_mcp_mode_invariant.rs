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

// Sprint 56 Track I-Phase2c (#531): hard removal — `Commands::Mcp`
// enum variant + `mcp::run` function + supporting machinery deleted
// per operator escalation. The deprecation pin from Phase 2b is
// inverted: we now assert the variant + function are GONE, not
// merely deprecated.

#[test]
fn commands_mcp_enum_variant_removed() {
    // Sprint 56 Track I-Phase2c: the `Commands::Mcp` arm and its
    // enum variant declaration must be absent from main.rs. A
    // future regression that re-introduces the variant would
    // re-create the silent-drop class root path that #531 fought
    // through 4 phases of migration to remove.
    assert!(
        !MAIN_RS.contains("Some(Commands::Mcp)"),
        "Phase 2c hard-removal invariant: src/main.rs must not contain \
         a `Some(Commands::Mcp)` match arm. The `agend-terminal mcp` \
         subcommand was retired per operator directive — see \
         docs/RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md."
    );
    // Stricter pin: also no `Commands::Mcp` enum variant declaration.
    // This catches a subtler regression where someone adds the
    // variant back without immediately matching it.
    assert!(
        !MAIN_RS.contains("    Mcp,"),
        "The `Mcp` enum variant declaration in `Commands` must remain \
         removed. If a new MCP-related subcommand is needed, give it a \
         distinct name (e.g. `BridgeMcp`) so we don't shadow the retired \
         command."
    );
}

const MCP_MOD_RS: &str = include_str!("../src/mcp/mod.rs");

#[test]
fn mcp_run_function_removed() {
    // Sprint 56 Track I-Phase2c: the `pub fn run() -> anyhow::Result<()>`
    // stdio JSON-RPC server is deleted. The canonical MCP server is
    // now `agend-mcp-bridge` (`src/bin/agend-mcp-bridge.rs`).
    assert!(
        !MCP_MOD_RS.contains("pub fn run("),
        "Phase 2c hard-removal invariant: src/mcp/mod.rs must not \
         contain a `pub fn run()` stdio server. The bridge binary is \
         the canonical post-Sprint-56 MCP server entry point."
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

// ── Sprint 56 Track I-Phase2c (#531): runtime invariant ─────────────
//
// Spawn agend-mcp-bridge as a subprocess WITHOUT a daemon running and
// confirm it surfaces a daemon-related error rather than silently
// degrading. This pins the post-Phase-2c contract that the bridge is
// proxy-only — there is no local-handler fallback path it can fall
// into when the daemon is unreachable.

/// Find the agend-mcp-bridge binary cargo built for tests. Returns
/// `None` when the binary path isn't exposed (some test harness
/// configurations); test scope-skips rather than panicking so a CI
/// runner without the env-var still gives a clean signal.
fn bridge_binary() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_agend-mcp-bridge").map(std::path::PathBuf::from)
}

#[test]
fn bridge_emits_daemon_error_when_daemon_down() {
    let bridge = match bridge_binary() {
        Some(p) => p,
        None => {
            eprintln!(
                "skip: CARGO_BIN_EXE_agend-mcp-bridge not set — the runtime \
                 invariant requires `cargo test` (which exposes bin paths)."
            );
            return;
        }
    };

    // Point AGEND_HOME at a clean temp dir so we don't accidentally
    // talk to the operator's real running daemon.
    let tag = format!("agend-mcp-bridge-no-daemon-{}", std::process::id());
    let home = std::env::temp_dir().join(&tag);
    std::fs::create_dir_all(&home).expect("test home dir");

    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "no-daemon-runtime-test")
        // Disable any test-isolation env that might short-circuit the
        // daemon-down code path inside the bridge.
        .env_remove("AGEND_TEST_ISOLATION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agend-mcp-bridge");

    // Send a tools/call for `reply` — a daemon-state-required tool
    // (Sprint 54 PR #488 hotfix). With no daemon running, the bridge
    // must surface a clear daemon-related error.
    let req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
        \"params\":{\"name\":\"reply\",\"arguments\":{\"text\":\"hi\"}}}\n";
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(req.as_bytes());
        let _ = stdin.flush();
    }
    // Close stdin so the bridge's read loop exits after responding.
    drop(child.stdin.take());

    // Bound the wait so a hung subprocess can't stall the test.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    // Force-kill if still running.
    let _ = child.kill();
    let output = child.wait_with_output().expect("collect bridge output");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");

    // The daemon-down failure mode must surface SOME daemon-related
    // hint. Accept any of the canonical signals: literal `daemon`,
    // connect-failed wording, or the `mcp_proxy` API error path.
    let daemon_signal = combined.to_lowercase();
    assert!(
        daemon_signal.contains("daemon")
            || daemon_signal.contains("connect")
            || daemon_signal.contains("api")
            || daemon_signal.contains("unreachable"),
        "Phase 2c runtime invariant: bridge must surface a daemon-related \
         signal when no daemon is running. Got stdout/stderr:\n{combined}"
    );

    std::fs::remove_dir_all(&home).ok();
}
