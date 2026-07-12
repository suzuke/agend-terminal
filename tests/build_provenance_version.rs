//! Build provenance slice α (t-20260712073937810355-40783-34,
//! d-20260712073535548541-12) — deterministic RED→GREEN coverage.
//!
//! RED against main (no provenance capability): the resolver/probe/compose
//! logic in `build/provenance.rs` is stubbed to today's behavior, `build.rs`
//! emits no identity envs, and neither surface (CLI `--version`, MCP
//! serverInfo) carries a build SHA. Every test below encodes the slice-α
//! contract and fails on that state; GREEN implements it.
//!
//! Determinism: scratch git repos under a per-process temp dir (isolated
//! `HOME`/`GIT_CONFIG_*`, fixed committer, no network, no timing); the CLI
//! shape test spawns the actually-built binary via `CARGO_BIN_EXE_`; baked
//! env assertions use `option_env!` so the RED tree still compiles.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../build/provenance.rs"]
mod provenance;

use provenance::{
    compose_cli_version, compose_mcp_version, git_probe, rerun_paths, resolve_identity,
    valid_full_sha,
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

// ---- identity validation ----

#[test]
fn valid_full_sha_accepts_40_and_64_hex_only() {
    assert!(valid_full_sha(SHA_A));
    assert!(valid_full_sha(&"c".repeat(64)));
    assert!(!valid_full_sha("deadbeef")); // abbreviated
    assert!(!valid_full_sha(&"g".repeat(40))); // non-hex
    assert!(!valid_full_sha("")); // empty
    assert!(!valid_full_sha(&"a".repeat(41))); // wrong length
}

// ---- resolver hierarchy: env > probe > unknown ----

#[test]
fn resolve_env_commit_wins_over_probe() {
    let id = resolve_identity(Some(SHA_A), Some("1"), || Some((SHA_B.into(), false)));
    assert_eq!(id.sha, SHA_A, "explicit env commit must win over the probe");
    assert!(id.dirty, "explicit env dirty flag must be honored");
    assert!(id.warnings.is_empty());
}

#[test]
fn resolve_env_commit_is_lowercased() {
    let upper = SHA_A.to_uppercase();
    let id = resolve_identity(Some(&upper), None, || None);
    assert_eq!(id.sha, SHA_A, "env commit must be stored lowercase");
    assert!(!id.dirty, "absent env dirty flag means clean");
}

#[test]
fn resolve_malformed_env_falls_through_to_probe_with_warning() {
    let id = resolve_identity(Some("deadbeef"), None, || Some((SHA_B.into(), true)));
    assert_eq!(
        id.sha, SHA_B,
        "malformed env commit must fall through to the probe"
    );
    assert!(id.dirty, "probe dirtiness must be preserved");
    assert!(
        !id.warnings.is_empty(),
        "a malformed explicit commit must surface a warning"
    );
}

#[test]
fn resolve_probe_used_when_env_absent() {
    let id = resolve_identity(None, None, || Some((SHA_B.into(), false)));
    assert_eq!(id.sha, SHA_B);
    assert!(!id.dirty);
}

#[test]
fn resolve_unknown_when_no_source_available() {
    let id = resolve_identity(None, None, || None);
    assert_eq!(id.sha, "unknown", "no source must yield honest unknown");
    assert!(!id.dirty, "unknown is never dirty");
}

// ---- surface composition ----

#[test]
fn cli_version_shapes() {
    assert_eq!(
        compose_cli_version("0.10.0", SHA_A, false),
        format!("0.10.0 (build {SHA_A})")
    );
    assert_eq!(
        compose_cli_version("0.10.0", SHA_A, true),
        format!("0.10.0 (build {SHA_A} dirty)")
    );
    assert_eq!(
        compose_cli_version("0.10.0", "unknown", false),
        "0.10.0 (build unknown)"
    );
    // unknown never carries dirty, even if a caller passes true
    assert_eq!(
        compose_cli_version("0.10.0", "unknown", true),
        "0.10.0 (build unknown)"
    );
}

#[test]
fn mcp_version_shapes() {
    assert_eq!(
        compose_mcp_version("0.10.0", SHA_A, false),
        "0.10.0+gaaaaaaaaaaaa"
    );
    assert_eq!(
        compose_mcp_version("0.10.0", SHA_A, true),
        "0.10.0+gaaaaaaaaaaaa.dirty"
    );
    assert_eq!(
        compose_mcp_version("0.10.0", "unknown", false),
        "0.10.0",
        "unknown omits build metadata entirely (bare, still-valid semver)"
    );
    assert_eq!(compose_mcp_version("0.10.0", "unknown", true), "0.10.0");
}

// ---- git probe on scratch repos (worktree / detached / dirty / no-git) ----

/// Scratch repo helper: fully isolated from user/system git config and from
/// any daemon-bound worktree (cwd is always the scratch dir — never a
/// canonical-rooted path, so the agend-git shim pass-through is inert).
fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "agend-provenance-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(tag: &str) -> PathBuf {
    let d = scratch_dir(tag);
    git(&d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("f.txt"), "one\n").unwrap();
    git(&d, &["add", "f.txt"]);
    git(&d, &["commit", "-q", "--no-gpg-sign", "-m", "c1"]);
    d
}

#[test]
fn probe_reads_head_sha_in_normal_repo() {
    let d = init_repo("normal");
    let expect = git(&d, &["rev-parse", "HEAD"]).to_lowercase();
    let (sha, dirty) = git_probe(&d).expect("probe must resolve a git repo");
    assert_eq!(sha, expect);
    assert!(!dirty, "freshly committed repo must be clean");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn probe_reports_dirty_on_tracked_modification() {
    let d = init_repo("dirty");
    std::fs::write(d.join("f.txt"), "two\n").unwrap();
    let (_, dirty) = git_probe(&d).expect("probe must resolve");
    assert!(dirty, "a modified tracked file must read dirty");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn probe_resolves_detached_head() {
    let d = init_repo("detached");
    let expect = git(&d, &["rev-parse", "HEAD"]).to_lowercase();
    git(&d, &["checkout", "-q", "--detach"]);
    let (sha, _) = git_probe(&d).expect("probe must resolve detached HEAD");
    assert_eq!(sha, expect);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn probe_resolves_linked_worktree() {
    let d = init_repo("wt-main");
    let wt = scratch_dir("wt-linked");
    let _ = std::fs::remove_dir_all(&wt); // worktree add wants a fresh path
    git(
        &d,
        &["worktree", "add", "-q", "-b", "t2", wt.to_str().unwrap()],
    );
    let expect = git(&wt, &["rev-parse", "HEAD"]).to_lowercase();
    let (sha, dirty) = git_probe(&wt).expect("probe must resolve a linked worktree");
    assert_eq!(sha, expect);
    assert!(!dirty);
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn probe_returns_none_outside_any_repo() {
    let d = scratch_dir("norepo");
    assert!(
        git_probe(&d).is_none(),
        "a plain directory (source archive) must probe as unknown"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// ---- rerun trigger paths (cache-rebuild correctness) ----

#[test]
fn rerun_paths_exist_and_cover_head_in_normal_repo() {
    let d = init_repo("rerun-normal");
    let paths = rerun_paths(&d);
    assert!(
        !paths.is_empty(),
        "a git repo must yield at least the HEAD trigger"
    );
    for p in &paths {
        assert!(p.exists(), "trigger path must exist (else cargo re-runs every build): {p:?}");
    }
    assert!(
        paths.iter().any(|p| p.ends_with("HEAD")),
        "triggers must include the HEAD file: {paths:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rerun_paths_use_per_worktree_head_not_pointer_file() {
    let d = init_repo("rerun-wt-main");
    let wt = scratch_dir("rerun-wt-linked");
    let _ = std::fs::remove_dir_all(&wt);
    git(
        &d,
        &["worktree", "add", "-q", "-b", "t3", wt.to_str().unwrap()],
    );
    let paths = rerun_paths(&wt);
    let head = paths
        .iter()
        .find(|p| p.ends_with("HEAD"))
        .expect("worktree triggers must include a HEAD path");
    assert!(head.exists());
    assert!(
        head.to_string_lossy().contains("worktrees"),
        "worktree HEAD must live under <main>/.git/worktrees/<name>/ — \
         `<wt>/.git` is a pointer FILE and `<wt>/.git/HEAD` does not exist: {head:?}"
    );
    let _ = std::fs::remove_dir_all(&wt);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rerun_paths_empty_outside_any_repo() {
    let d = scratch_dir("rerun-norepo");
    assert!(
        rerun_paths(&d).is_empty(),
        "no repo ⇒ no file triggers (env triggers only; a nonexistent path would re-run every build)"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// ---- baked build-time envs (option_env! so the RED tree compiles) ----

#[test]
fn baked_identity_envs_present_and_well_formed() {
    let sha = option_env!("AGEND_BUILD_SHA")
        .expect("build.rs must emit AGEND_BUILD_SHA (sha | unknown)");
    assert!(
        sha == "unknown" || valid_full_sha(sha),
        "AGEND_BUILD_SHA must be a full lowercase hex SHA or 'unknown': {sha}"
    );
    assert_eq!(sha, sha.to_lowercase(), "baked SHA must be lowercase");
    let dirty = option_env!("AGEND_BUILD_DIRTY")
        .expect("build.rs must emit AGEND_BUILD_DIRTY (0|1)");
    assert!(dirty == "0" || dirty == "1", "AGEND_BUILD_DIRTY ∈ {{0,1}}: {dirty}");
    if sha == "unknown" {
        assert_eq!(dirty, "0", "unknown identity is never dirty");
    }
}

#[test]
fn baked_surface_envs_match_composition() {
    let sha = option_env!("AGEND_BUILD_SHA").expect("AGEND_BUILD_SHA");
    let dirty = option_env!("AGEND_BUILD_DIRTY").expect("AGEND_BUILD_DIRTY") == "1";
    let semver = env!("CARGO_PKG_VERSION");
    assert_eq!(
        option_env!("AGEND_CLI_VERSION").expect("build.rs must emit AGEND_CLI_VERSION"),
        compose_cli_version(semver, sha, dirty),
        "baked CLI version must equal the canonical composition"
    );
    assert_eq!(
        option_env!("AGEND_MCP_VERSION").expect("build.rs must emit AGEND_MCP_VERSION"),
        compose_mcp_version(semver, sha, dirty),
        "baked MCP version must equal the canonical composition"
    );
}

// ---- the real CLI surface ----

#[test]
fn cli_version_output_carries_build_identity_with_leading_compat() {
    let out = Command::new(env!("CARGO_BIN_EXE_agend-terminal"))
        .arg("--version")
        .output()
        .expect("spawn agend-terminal --version");
    assert!(out.status.success());
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    let re = regex::Regex::new(
        r"^agend-terminal \d+\.\d+\.\d+ \(build ([0-9a-f]{40}|[0-9a-f]{64}|unknown)( dirty)?\)$",
    )
    .unwrap();
    assert!(
        re.is_match(line),
        "--version must keep the leading `agend-terminal <semver>` tokens (§7.1) \
         and append `(build <sha|unknown>[ dirty])`; got: {line}"
    );
}

// ---- MCP serverInfo wiring pin (source-level; the bridge is a separate bin) ----

#[test]
fn mcp_bridge_serverinfo_uses_baked_build_metadata_version() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/agend-mcp-bridge.rs"),
    )
    .unwrap();
    assert!(
        src.contains(r#""version": env!("AGEND_MCP_VERSION")"#),
        "serverInfo.version must be the baked semver+build-metadata string \
         (AGEND_MCP_VERSION), not a bare CARGO_PKG_VERSION"
    );
}
