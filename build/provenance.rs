//! Build provenance resolution (slice α, d-20260712073535548541-12).
//!
//! Pure-std logic shared by `build.rs` (via `include!`) and its tests (via a
//! `#[path]` module in `tests/build_provenance_version.rs`). Resolves the build
//! identity through a strict hierarchy — explicit `AGEND_BUILD_COMMIT` env >
//! worktree-safe git probe > honest `unknown` — and composes the two surface
//! strings (CLI `--version`, MCP serverInfo semver build-metadata). No
//! dependencies beyond std; no network; never fails the build.

use std::path::{Path, PathBuf};

/// A resolved build identity. `sha` is a full lowercase 40/64-hex commit OID or
/// the literal `"unknown"`; `dirty` is only ever true when `sha` is known.
/// `warnings` carries operator-actionable notes (e.g. a malformed explicit env)
/// for `build.rs` to surface as `cargo:warning` lines.
pub struct Identity {
    pub sha: String,
    pub dirty: bool,
    pub warnings: Vec<String>,
}

/// Full commit OID: 40 or 64 hex digits (mirrors the runtime
/// `is_full_commit_sha` invariant; build.rs cannot import the crate).
pub fn valid_full_sha(_s: &str) -> bool {
    false // RED stub: no provenance capability exists yet
}

/// Resolve the identity hierarchy: explicit env commit (validated, lowercased,
/// with optional env dirty flag) > `probe` (the git probe, injected for
/// determinism) > honest `unknown`. A malformed explicit commit falls through
/// to the probe with a warning — the most honest identity still available.
pub fn resolve_identity(
    _env_commit: Option<&str>,
    _env_dirty: Option<&str>,
    _probe: impl FnOnce() -> Option<(String, bool)>,
) -> Identity {
    // RED stub: current behavior — no provenance is ever resolved.
    Identity {
        sha: "unknown".to_string(),
        dirty: false,
        warnings: Vec::new(),
    }
}

/// Worktree-safe git probe: `git rev-parse HEAD` in `dir` (works in worktrees
/// and detached HEAD) + dirtiness via `git status --porcelain -uno`. Any git
/// failure — including a status failure after a successful rev-parse — returns
/// `None` (unknown), never a false-clean identity.
pub fn git_probe(_dir: &Path) -> Option<(String, bool)> {
    None // RED stub
}

/// Worktree-safe `cargo:rerun-if-changed` trigger paths: the per-worktree HEAD
/// file (`git rev-parse --git-path HEAD` — NOT `<dir>/.git/HEAD`, which does not
/// exist in a worktree where `.git` is a pointer file), the current branch's
/// loose ref file, and `packed-refs`. Only paths that exist are returned (a
/// missing path would make cargo re-run the build script on every build).
pub fn rerun_paths(_dir: &Path) -> Vec<PathBuf> {
    Vec::new() // RED stub
}

/// CLI `--version` value handed to clap: `<semver> (build <sha>[ dirty])` /
/// `<semver> (build unknown)`. clap prepends the bin name, so the printed line
/// keeps the leading `agend-terminal <semver>` compatibility tokens (§7.1).
/// `unknown` never carries `dirty`.
pub fn compose_cli_version(semver: &str, _sha: &str, _dirty: bool) -> String {
    semver.to_string() // RED stub: current surface is the bare semver
}

/// MCP serverInfo version: semver build-metadata (semver §10) —
/// `<semver>+g<sha12>[.dirty]`; `unknown` yields the bare semver (honest
/// absence, still valid semver for existing parsers).
pub fn compose_mcp_version(semver: &str, _sha: &str, _dirty: bool) -> String {
    semver.to_string() // RED stub: current surface is the bare semver
}
