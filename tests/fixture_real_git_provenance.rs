//! task122 — real-git provenance seam REDs (fixture-only).
//!
//! Proves `common::git_isolated`'s resolver: a simulated deny-shim on PATH is
//! EXCLUDED and the REAL git is used; when ONLY the shim resolves it FAILS LOUD
//! (never SKIP, never the shim); real-git PATH and simulated-shim PATH yield the
//! same real git. The source invariant (no production consumer, no enclosing-repo
//! use) is asserted separately.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::git_isolated;
use std::ffi::OsString;
use std::path::PathBuf;

/// Create a temp dir containing an EXECUTABLE `git` that is NOT real git (a stand-in
/// for the agend-git deny-shim). If the seam ever selected+ran it, the `git version`
/// probe would reject it — but the seam must EXCLUDE its dir outright.
#[cfg(unix)]
fn fake_shim_dir() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "t122-fakeshim-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let git = dir.join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\necho 'agentic-git: ERROR deny-shim' >&2\nexit 42\n",
    )
    .unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// The real git dir in this environment (control), resolved through the seam on the
/// live PATH. If this itself can't resolve real git, the harness has no real git —
/// which is itself the fail-loud contract, so we require it.
fn real_git_dir() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let real = git_isolated::resolve_real_git_in(&path, &git_isolated::shim_dirs())
        .expect("real git must resolve in dev/CI env");
    real.parent().unwrap().to_path_buf()
}

fn join(dirs: &[&PathBuf]) -> OsString {
    std::env::join_paths(dirs.iter().map(|p| p.as_path())).unwrap()
}

/// (1) a simulated deny-shim FIRST on PATH is excluded; the REAL git is resolved.
#[cfg(unix)]
#[test]
fn t122_simulated_deny_shim_is_excluded_real_git_resolved() {
    let shim = fake_shim_dir();
    let real_dir = real_git_dir();
    let path = join(&[&shim, &real_dir]);
    let got = git_isolated::resolve_real_git_in(&path, std::slice::from_ref(&shim))
        .expect("shim excluded → real git found");
    assert_eq!(
        got.parent().unwrap(),
        real_dir,
        "must resolve REAL git ({}), not the shim dir",
        real_dir.display()
    );
    // real-git-first PATH puts the real git dir ahead of the shim.
    let first = git_isolated::real_git_first_path_from(&path, std::slice::from_ref(&shim))
        .expect("real-git-first PATH");
    let entries: Vec<PathBuf> = std::env::split_paths(&first).collect();
    assert_eq!(entries.first().unwrap(), &real_dir, "real git dir is FIRST");
    std::fs::remove_dir_all(&shim).ok();
}

/// (2) when ONLY the shim resolves, provenance FAILS LOUD (Err, never the shim).
#[cfg(unix)]
#[test]
fn t122_only_shim_fails_loud_never_skip() {
    let shim = fake_shim_dir();
    let only_shim = join(&[&shim]);
    let r = git_isolated::resolve_real_git_in(&only_shim, std::slice::from_ref(&shim));
    assert!(
        r.is_err(),
        "only-shim PATH must FAIL LOUD (never resolve the shim, never SKIP): {r:?}"
    );
    assert!(
        r.unwrap_err().contains("provenance FAILED"),
        "error must be an actionable provenance failure"
    );
    std::fs::remove_dir_all(&shim).ok();
}

/// (4) real-git PATH and simulated-shim-prepended PATH yield the SAME real git.
#[cfg(unix)]
#[test]
fn t122_real_and_simulated_shim_paths_yield_same_git() {
    let shim = fake_shim_dir();
    let real_dir = real_git_dir();
    let plain = git_isolated::resolve_real_git_in(&join(&[&real_dir]), &[]).unwrap();
    let with_shim =
        git_isolated::resolve_real_git_in(&join(&[&shim, &real_dir]), std::slice::from_ref(&shim))
            .unwrap();
    assert_eq!(
        plain, with_shim,
        "same real git regardless of a shim on PATH"
    );
    std::fs::remove_dir_all(&shim).ok();
}

/// End-to-end: a fixture built through the seam runs against REAL git even with a
/// deny-shim dir prepended to the child PATH — `setup_temp_repo` + a commit succeed
/// and land in the TEMP repo (not redirected). Proves child git procs inherit the
/// real-git-first PATH.
#[cfg(unix)]
#[test]
fn t122_fixture_uses_real_git_end_to_end() {
    let repo = git_isolated::setup_temp_repo("t122-e2e");
    std::fs::write(repo.join("f.txt"), "x").unwrap();
    let add = git_isolated::git(&repo, &["add", "-A"]);
    assert!(add.status.success(), "git add via seam: {add:?}");
    let commit = git_isolated::git(&repo, &["commit", "-m", "c"]);
    assert!(commit.status.success(), "git commit via seam: {commit:?}");
    // HEAD resolves in the TEMP repo (not the enclosing worktree).
    let head = git_isolated::git(&repo, &["rev-parse", "--show-toplevel"]);
    let top = String::from_utf8_lossy(&head.stdout);
    let top_canon = std::path::Path::new(top.trim()).canonicalize().unwrap();
    assert_eq!(
        top_canon,
        repo.canonicalize().unwrap(),
        "fixture git operated on the temp repo, not the host worktree"
    );
    std::fs::remove_dir_all(&repo).ok();
}
