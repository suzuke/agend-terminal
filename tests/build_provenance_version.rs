//! Build provenance slice α (t-20260712073937810355-40783-34,
//! d-20260712073535548541-12) — deterministic RED→GREEN coverage.
//!
//! r0 RED against main (no provenance capability): resolver hierarchy, probe,
//! composition, surfaces. r1 RED against the r0 GREEN (codex exact-head
//! review m-…-412): false-clean incremental build (tracked edit without HEAD
//! move kept a stale clean identity — build.rs watched only git metadata),
//! `-uno` hiding untracked files, multiline `AGEND_BUILD_COMMIT` values able
//! to inject cargo directives through `cargo:warning`, and a source-string
//! scan standing in for the real MCP initialize handshake.
//!
//! Determinism: scratch git repos via `common::git_isolated` (#821) under
//! per-process temp dirs; the CLI/bridge shape tests spawn the actually-built
//! binaries via `CARGO_BIN_EXE_`; the cached-build repro drives a hermetic
//! nested-cargo fixture crate; baked env assertions use `option_env!` so the
//! RED tree still compiles.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod common;
use common::git_isolated;

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

/// r1: `cargo:warning={w}` interprets embedded newlines as fresh cargo
/// directives — a crafted multiline `AGEND_BUILD_COMMIT` could inject e.g.
/// `cargo:rustc-env=AGEND_BUILD_SHA=<forged>`. The echoed value must be
/// single-line and bounded.
#[test]
fn malformed_env_warning_is_single_line_and_bounded() {
    let evil = format!(
        "bad\ncargo:rustc-env=AGEND_BUILD_SHA={SHA_B}\r{}",
        "x".repeat(500)
    );
    let id = resolve_identity(Some(&evil), None, || None);
    assert_eq!(id.sha, "unknown", "evil env must not resolve");
    let w = id.warnings.first().expect("warning must be surfaced");
    assert!(
        !w.contains('\n') && !w.contains('\r'),
        "warning must be single-line (cargo directive injection vector): {w}"
    );
    assert!(w.len() <= 200, "warning must be bounded, len={}", w.len());
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

/// The production `git_probe`/`rerun_paths` under test spawn `git` themselves
/// (not via the #821 `git_isolated` fixture helper), so the shim bypass must
/// also hold process-wide: in a fleet-agent shell the `agend-git` shim's
/// ChdirPass would answer a scratch-repo `rev-parse` for the BOUND worktree
/// (the CLAUDE.md lesson-10.7 / #820 class). Inert where the shim is absent
/// (CI, operator shells). Fixture git calls go through `git_isolated::git`,
/// which pins the same bypass per-child.
fn isolate_from_git_shim() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("AGEND_GIT_BYPASS", "1");
        std::env::set_var("AGENTIC_GIT_BYPASS", "1");
    });
}

/// Scratch dir helper: per-process temp path, never a canonical-rooted repo.
fn scratch_dir(tag: &str) -> PathBuf {
    isolate_from_git_shim(); // every git-touching test builds a scratch dir first
    let d = std::env::temp_dir().join(format!("agend-provenance-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// #821-conformant fixture git: `git_isolated::git` (cwd pin + shim bypass +
/// pinned committer identity) with success assertion + trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_isolated::git(dir, args);
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

/// r1: a NON-IGNORED untracked file is real working-tree divergence and must
/// read dirty — the r0 `--untracked-files=no` hid it (false-clean).
#[test]
fn probe_reports_dirty_on_untracked_non_ignored_file() {
    let d = init_repo("untracked");
    std::fs::write(d.join("new-file.txt"), "x\n").unwrap();
    let (_, dirty) = git_probe(&d).expect("probe must resolve");
    assert!(dirty, "a non-ignored untracked file must read dirty");
    let _ = std::fs::remove_dir_all(&d);
}

/// r1 boundary: gitignored files (e.g. target dirs) must NOT read dirty —
/// this is what keeps build products out of the dirtiness signal.
#[test]
fn probe_stays_clean_on_ignored_file() {
    let d = init_repo("ignored");
    std::fs::write(d.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&d, &["add", ".gitignore"]);
    git(&d, &["commit", "-q", "--no-gpg-sign", "-m", "ignore"]);
    std::fs::write(d.join("ignored.txt"), "x\n").unwrap();
    let (_, dirty) = git_probe(&d).expect("probe must resolve");
    assert!(!dirty, "an ignored file must stay clean");
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
        assert!(
            p.exists(),
            "trigger path must exist (else cargo re-runs every build): {p:?}"
        );
    }
    assert!(
        paths.iter().any(|p| p.ends_with("HEAD")),
        "triggers must include the HEAD file: {paths:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// r1: TRACKED files must be triggers too — with git-metadata-only triggers a
/// tracked edit (no HEAD move) rebuilt the crate WITHOUT re-running build.rs,
/// leaving a stale clean identity (codex r0-review deterministic repro).
#[test]
fn rerun_paths_include_tracked_files() {
    let d = init_repo("rerun-tracked");
    let paths = rerun_paths(&d);
    assert!(
        paths
            .iter()
            .any(|p| p.file_name().is_some_and(|f| f == "f.txt")),
        "tracked files must re-trigger the build script (false-clean fix): {paths:?}"
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

// ---- r1 empirical cached-build repro (codex r0-review finding 1) ----

/// The exact false-clean sequence, driven through REAL cargo builds of a
/// hermetic fixture crate whose build.rs `include!`s the SAME provenance.rs
/// under test: clean build shows the sha; a TRACKED edit without any HEAD
/// move must flip the baked identity to dirty on the next cached build; a
/// revert must flip it back to clean. r0 failed the middle step (build.rs
/// watched only git metadata, so the incremental rebuild kept the stale
/// clean identity).
#[test]
fn cached_build_refreshes_dirty_without_head_move() {
    let repo = init_repo("nested-cargo");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::copy(
        manifest.join("build/provenance.rs"),
        repo.join("provenance.rs"),
    )
    .unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"prov-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("build.rs"),
        r#"include!("provenance.rs");
fn main() {
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let id = resolve_identity(None, None, || git_probe(&dir));
    for w in &id.warnings {
        println!("cargo:warning={w}");
    }
    for p in rerun_paths(&dir) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!(
        "cargo:rustc-env=FIXTURE_CLI={}",
        compose_cli_version("0.1.0", &id.sha, id.dirty)
    );
}
"#,
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/main.rs"),
        "fn main() {\n    println!(\"{}\", env!(\"FIXTURE_CLI\"));\n}\n",
    )
    .unwrap();
    // Build products are gitignored so the build itself never reads dirty —
    // the "exclude target loops" boundary. Cargo.lock: cargo writes it before
    // the build script runs, so an untracked lockfile would dirty the very
    // first build.
    std::fs::write(repo.join(".gitignore"), "/target-fixture\nCargo.lock\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "--no-gpg-sign", "-m", "fixture"]);
    let head = git(&repo, &["rev-parse", "HEAD"]).to_lowercase();

    let run_fixture = |repo: &Path| -> String {
        let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["run", "-q"])
            .current_dir(repo)
            // Hermetic vs the outer test invocation: coverage/instrumentation
            // flags must not leak into the fixture build.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("LLVM_PROFILE_FILE")
            .env("CARGO_TARGET_DIR", repo.join("target-fixture"))
            .output()
            .expect("nested cargo run");
        assert!(
            out.status.success(),
            "fixture build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert_eq!(
        run_fixture(&repo),
        format!("0.1.0 (build {head})"),
        "clean cached build must bake the exact HEAD, clean"
    );
    // Tracked edit, NO HEAD movement — the r0 false-clean scenario.
    std::fs::write(
        repo.join("src/main.rs"),
        "fn main() {\n    println!(\"{}\", env!(\"FIXTURE_CLI\"));\n    // edited\n}\n",
    )
    .unwrap();
    assert_eq!(
        run_fixture(&repo),
        format!("0.1.0 (build {head} dirty)"),
        "a tracked edit without HEAD move must flip the cached build to dirty"
    );
    git(&repo, &["checkout", "--", "src/main.rs"]);
    assert_eq!(
        run_fixture(&repo),
        format!("0.1.0 (build {head})"),
        "reverting the edit must flip the cached build back to clean"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

// ---- baked build-time envs (option_env! so the RED tree compiles) ----

/// RED-compilable required-env accessor: `option_env!` keeps the RED tree
/// compiling (the env doesn't exist until GREEN's build.rs emits it) and the
/// runtime panic makes the RED failure a test FAILURE, not a build error.
/// A plain `option_env!(..).expect(..)` would trip the deny-by-default
/// `clippy::option_env_unwrap`; routing through a fn keeps the identical
/// semantics without the linted adjacency.
#[track_caller]
fn required_env(value: Option<&'static str>, what: &str) -> &'static str {
    match value {
        Some(v) => v,
        None => panic!("build.rs must emit {what}"),
    }
}

#[test]
fn baked_identity_envs_present_and_well_formed() {
    let sha = required_env(
        option_env!("AGEND_BUILD_SHA"),
        "AGEND_BUILD_SHA (sha | unknown)",
    );
    assert!(
        sha == "unknown" || valid_full_sha(sha),
        "AGEND_BUILD_SHA must be a full lowercase hex SHA or 'unknown': {sha}"
    );
    assert_eq!(sha, sha.to_lowercase(), "baked SHA must be lowercase");
    let dirty = required_env(option_env!("AGEND_BUILD_DIRTY"), "AGEND_BUILD_DIRTY (0|1)");
    assert!(
        dirty == "0" || dirty == "1",
        "AGEND_BUILD_DIRTY ∈ {{0,1}}: {dirty}"
    );
    if sha == "unknown" {
        assert_eq!(dirty, "0", "unknown identity is never dirty");
    }
}

#[test]
fn baked_surface_envs_match_composition() {
    let sha = required_env(option_env!("AGEND_BUILD_SHA"), "AGEND_BUILD_SHA");
    let dirty = required_env(option_env!("AGEND_BUILD_DIRTY"), "AGEND_BUILD_DIRTY") == "1";
    let semver = env!("CARGO_PKG_VERSION");
    assert_eq!(
        required_env(option_env!("AGEND_CLI_VERSION"), "AGEND_CLI_VERSION"),
        compose_cli_version(semver, sha, dirty),
        "baked CLI version must equal the canonical composition"
    );
    assert_eq!(
        required_env(option_env!("AGEND_MCP_VERSION"), "AGEND_MCP_VERSION"),
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

// ---- the real MCP surface: zero-daemon initialize handshake (r1) ----

/// Spawn the actually-built bridge and perform the real NDJSON `initialize`
/// handshake (handled bridge-locally — no daemon needed): serverInfo.version
/// must equal the canonical semver+build-metadata composition. Replaces the
/// r0 source-string scan (codex r0-review finding: scan pins the source text,
/// not the wire behavior).
#[test]
fn mcp_bridge_initialize_handshake_reports_build_metadata_version() {
    use std::io::{BufRead, BufReader, Write};
    let home = scratch_dir("bridge-home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agend-mcp-bridge"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("AGEND_HOME", &home)
        .env_remove("AGEND_INSTANCE_NAME")
        .spawn()
        .expect("spawn agend-mcp-bridge");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();
    drop(child.stdin.take()); // EOF ⇒ bridge answers then exits its read loop
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = child.wait();
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("one NDJSON response");
    let sha = required_env(option_env!("AGEND_BUILD_SHA"), "AGEND_BUILD_SHA");
    let dirty = required_env(option_env!("AGEND_BUILD_DIRTY"), "AGEND_BUILD_DIRTY") == "1";
    assert_eq!(
        v["result"]["serverInfo"]["version"].as_str().unwrap_or(""),
        compose_mcp_version(env!("CARGO_PKG_VERSION"), sha, dirty),
        "real initialize handshake must report the baked build-metadata version; got: {line}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
