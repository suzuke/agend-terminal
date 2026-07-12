// Build provenance resolution (slice α, d-20260712073535548541-12).
//
// Pure-std logic shared by `build.rs` (via `include!` — which is why this
// header is `//` not `//!`: inner doc comments cannot be spliced mid-file) and
// its tests (via a `#[path]` module in `tests/build_provenance_version.rs`).
// Resolves the build identity through a strict hierarchy — explicit
// `AGEND_BUILD_COMMIT` env > worktree-safe git probe > honest `unknown` — and
// composes the two surface strings (CLI `--version`, MCP serverInfo semver
// build-metadata). No dependencies beyond std; no network; never fails the
// build.

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
pub fn valid_full_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolve the identity hierarchy: explicit env commit (validated, lowercased,
/// with optional env dirty flag `1`/`true`) > `probe` (the git probe, injected
/// for determinism) > honest `unknown`. A malformed explicit commit falls
/// through to the probe with a warning — the most honest identity still
/// available beats both a hard build failure and a silently-trusted bad pin.
pub fn resolve_identity(
    env_commit: Option<&str>,
    env_dirty: Option<&str>,
    probe: impl FnOnce() -> Option<(String, bool)>,
) -> Identity {
    let mut warnings = Vec::new();
    if let Some(raw) = env_commit {
        let c = raw.trim().to_ascii_lowercase();
        if valid_full_sha(&c) {
            let dirty = matches!(env_dirty.map(str::trim), Some("1") | Some("true"));
            return Identity {
                sha: c,
                dirty,
                warnings,
            };
        }
        warnings.push(format!(
            "AGEND_BUILD_COMMIT is not a full 40/64-hex commit SHA (got `{}`); \
             falling back to the git probe",
            sanitize_for_warning(raw)
        ));
    }
    if let Some((sha, dirty)) = probe() {
        return Identity {
            sha: sha.to_ascii_lowercase(),
            dirty,
            warnings,
        };
    }
    Identity {
        sha: "unknown".to_string(),
        dirty: false,
        warnings,
    }
}

/// Bound, single-line rendering of an untrusted env value for `cargo:warning`
/// output. Control characters (esp. `\n`/`\r`) are replaced — cargo parses
/// each build-script stdout LINE as a directive, so a raw multiline echo
/// would let a crafted value inject e.g. a forged `cargo:rustc-env=`.
pub fn sanitize_for_warning(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .take(80)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if raw.chars().count() > 80 {
        s.push('…');
    }
    s
}

/// Run git in `dir`, returning trimmed stdout on success and `None` on any
/// spawn/exit failure — every failure collapses to "this source is
/// unavailable", never a partial identity.
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Worktree-safe git probe: `git rev-parse HEAD` in `dir` (works in linked
/// worktrees and detached HEAD) + dirtiness via `git status --porcelain` —
/// tracked modifications AND non-ignored untracked files both count (r1: the
/// initial `-uno` hid untracked files, a false-clean); gitignored build
/// products never count. Any git failure — including a status failure after
/// a successful rev-parse — returns `None` (unknown), never a false-clean
/// identity.
pub fn git_probe(dir: &Path) -> Option<(String, bool)> {
    let sha = git_out(dir, &["rev-parse", "HEAD"])?.to_ascii_lowercase();
    if !valid_full_sha(&sha) {
        return None;
    }
    let status = git_out(dir, &["status", "--porcelain"])?;
    Some((sha, !status.is_empty()))
}

/// Worktree-safe `cargo:rerun-if-changed` trigger paths:
///
/// - git metadata — the per-worktree HEAD file (`git rev-parse --git-path
///   HEAD`; NOT `<dir>/.git/HEAD`, which does not exist in a worktree where
///   `.git` is a pointer file), the current branch's loose ref file (absent
///   when detached or packed), and `packed-refs` (absent until refs are
///   packed);
/// - TRACKED files (`git ls-files`, regular files only) — r1: without these,
///   a tracked edit with no HEAD movement rebuilt the crate WITHOUT
///   re-running the build script, baking a stale clean identity into the
///   incremental build (false-clean). Build products stay excluded by
///   construction: they are untracked/gitignored, and gitlink directory
///   entries are filtered by the is_file() check — no target-loop.
///
/// Only paths that exist are returned — cargo re-runs the build script on
/// EVERY build when a registered trigger path is missing. Residuals, accepted
/// for slice α: a loose ref that later becomes packed loses its trigger until
/// the next rebuild re-registers paths; an untracked-only change (new file,
/// nothing tracked touched) has no trigger to fire — its dirtiness lands on
/// the next build-script run, not instantly.
pub fn rerun_paths(dir: &Path) -> Vec<PathBuf> {
    // `--git-path` output is relative to the invocation cwd when possible;
    // since every git call runs with `current_dir(dir)`, join relative paths
    // back onto `dir`.
    let absolutize = |p: String| -> PathBuf {
        let pb = PathBuf::from(p);
        if pb.is_absolute() {
            pb
        } else {
            dir.join(pb)
        }
    };
    let mut v = Vec::new();
    let Some(head) = git_out(dir, &["rev-parse", "--git-path", "HEAD"]) else {
        return v; // not a repo ⇒ no file triggers (env triggers only)
    };
    let head = absolutize(head);
    if head.exists() {
        v.push(head);
    }
    // `symbolic-ref -q` exits non-zero when detached ⇒ git_out None ⇒ skipped.
    if let Some(refname) = git_out(dir, &["symbolic-ref", "-q", "HEAD"]) {
        if !refname.is_empty() {
            if let Some(ref_path) = git_out(dir, &["rev-parse", "--git-path", &refname]) {
                let ref_path = absolutize(ref_path);
                if ref_path.exists() {
                    v.push(ref_path);
                }
            }
        }
    }
    if let Some(packed) = git_out(dir, &["rev-parse", "--git-path", "packed-refs"]) {
        let packed = absolutize(packed);
        if packed.exists() {
            v.push(packed);
        }
    }
    // r1: tracked files, NUL-delimited (paths may contain anything but NUL).
    // is_file() drops deleted-but-tracked entries and gitlink (submodule)
    // directory entries — registering a directory would make cargo watch its
    // whole tree.
    if let Some(tracked) = git_out(dir, &["ls-files", "-z"]) {
        for rel in tracked.split('\0').filter(|s| !s.is_empty()) {
            let p = dir.join(rel);
            if p.is_file() {
                v.push(p);
            }
        }
    }
    v
}

/// CLI `--version` value handed to clap: `<semver> (build <sha>[ dirty])` /
/// `<semver> (build unknown)`. clap prepends the bin name, so the printed line
/// keeps the leading `agend-terminal <semver>` compatibility tokens (§7.1).
/// `unknown` never carries `dirty`.
pub fn compose_cli_version(semver: &str, sha: &str, dirty: bool) -> String {
    if sha == "unknown" {
        format!("{semver} (build unknown)")
    } else if dirty {
        format!("{semver} (build {sha} dirty)")
    } else {
        format!("{semver} (build {sha})")
    }
}

/// MCP serverInfo version: semver build-metadata (semver §10, `+` suffix with
/// `[0-9A-Za-z-]` dot-separated idents — existing semver parsers keep working)
/// — `<semver>+g<sha12>[.dirty]`; `unknown` yields the bare semver (honest
/// absence, still valid).
pub fn compose_mcp_version(semver: &str, sha: &str, dirty: bool) -> String {
    if sha == "unknown" {
        semver.to_string()
    } else {
        let short = &sha[..12];
        if dirty {
            format!("{semver}+g{short}.dirty")
        } else {
            format!("{semver}+g{short}")
        }
    }
}
