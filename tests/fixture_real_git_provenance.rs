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
#[cfg(unix)]
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
#[cfg(unix)]
fn real_git_dir() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let real = git_isolated::resolve_real_git_in(&path, &git_isolated::shim_dirs())
        .expect("real git must resolve in dev/CI env");
    real.parent().unwrap().to_path_buf()
}

#[cfg(unix)]
fn join(dirs: &[&PathBuf]) -> OsString {
    std::env::join_paths(dirs.iter().map(|p| p.as_path())).unwrap()
}

/// (1) a simulated deny-shim FIRST on PATH is excluded; the REAL git is resolved.
#[cfg(unix)]
#[test]
fn t122_simulated_deny_shim_is_excluded_real_git_resolved() {
    let shim = fake_shim_dir();
    let _g = TreeGuard(shim.clone());
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
}

/// (2) when ONLY the shim resolves, provenance FAILS LOUD (Err, never the shim).
#[cfg(unix)]
#[test]
fn t122_only_shim_fails_loud_never_skip() {
    let shim = fake_shim_dir();
    let _g = TreeGuard(shim.clone());
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
}

/// (4) real-git PATH and simulated-shim-prepended PATH yield the SAME real git.
#[cfg(unix)]
#[test]
fn t122_real_and_simulated_shim_paths_yield_same_git() {
    let shim = fake_shim_dir();
    let _g = TreeGuard(shim.clone());
    let real_dir = real_git_dir();
    let plain = git_isolated::resolve_real_git_in(&join(&[&real_dir]), &[]).unwrap();
    let with_shim =
        git_isolated::resolve_real_git_in(&join(&[&shim, &real_dir]), std::slice::from_ref(&shim))
            .unwrap();
    assert_eq!(
        plain, with_shim,
        "same real git regardless of a shim on PATH"
    );
}

/// End-to-end: a fixture built through the seam runs against REAL git even with a
/// deny-shim dir prepended to the child PATH — `setup_temp_repo` + a commit succeed
/// and land in the TEMP repo (not redirected). Proves child git procs inherit the
/// real-git-first PATH.
#[cfg(unix)]
#[test]
fn t122_fixture_uses_real_git_end_to_end() {
    let repo = git_isolated::setup_temp_repo("t122-e2e");
    let _g = TreeGuard(repo.clone());
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
}

/// (task122 RED #1) the #2770 suite (scripts/test_fmt_owned.sh) passes 10/10 through
/// the shell seam even when a DENY-SHIM occupies `$AGEND_HOME/bin` on the child PATH
/// — proof the migrated shell fixture + its child git procs use REAL git.
#[cfg(unix)]
#[test]
fn t122_test_fmt_owned_passes_under_simulated_deny_shim() {
    use std::os::unix::fs::PermissionsExt;
    let manifest = env!("CARGO_MANIFEST_DIR");
    // Temp AGEND_HOME whose bin/git is a deny-shim (exit 42) — must be EXCLUDED.
    let home = std::env::temp_dir().join(format!(
        "t122-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let _g = TreeGuard(home.clone());
    let shim_git = bin.join("git");
    std::fs::write(&shim_git, "#!/bin/sh\necho 'deny-shim' >&2\nexit 42\n").unwrap();
    std::fs::set_permissions(&shim_git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let real_dir = real_git_dir();
    let base = std::env::var("PATH").unwrap_or_default();
    // deny-shim FIRST, then real git, then the ambient PATH (for rustfmt etc.).
    let path = format!("{}:{}:{}", bin.display(), real_dir.display(), base);
    let out = std::process::Command::new("bash")
        .arg(format!("{manifest}/scripts/test_fmt_owned.sh"))
        .current_dir(manifest)
        .env("AGEND_HOME", &home)
        .env("PATH", &path)
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGEND_INSTANCE_NAME")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("10 passed, 0 failed"),
        "test_fmt_owned.sh must pass 10/10 via the seam under a deny-shim.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// (task122 RED — runtime fence) the chokepoint REFUSES to run real git against the
/// ENCLOSING worktree (`CARGO_MANIFEST_DIR`). This is the guard the src-symbol source
/// invariant CANNOT provide: a direct `git(Path::new(MANIFEST), …)` call must panic
/// fail-loud, never operate on the host repo.
#[test]
#[should_panic(expected = "NOT under the system temp dir")]
fn t122_rejects_enclosing_worktree_target() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let _ = git_isolated::git(&manifest, &["rev-parse", "--show-toplevel"]);
}

/// (task122 RED — runtime fence) a non-temp repo dir (the crate `src/`) is refused too;
/// the fence is "must be UNDER the system temp dir", not merely "not exactly MANIFEST".
#[test]
#[should_panic(expected = "NOT under the system temp dir")]
fn t122_rejects_non_temp_repo_target() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let _ = git_isolated::git(&src, &["status"]);
}

/// Panic-safe RAII cleanup of a manufactured temp tree (d-68: EVERY manufactured tree
/// must clean up even when the test PANICS — e.g. the `#[should_panic]` guards below,
/// which fire before any manual cleanup could run). Drop removes the tree during unwind.
struct TreeGuard(PathBuf);
impl Drop for TreeGuard {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A fresh unique temp root `<temp>/t122-<tag>-<pid>-<ns>` (wrap it in a [`TreeGuard`]).
fn fresh_temp_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "t122-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ))
}

/// (task122 RED — managed-worktree fence) even UNDER the system temp dir, a daemon-
/// MANAGED worktree (or a subdir of one) is REFUSED by the DEFAULT `git()`:
/// `starts_with(temp_dir())` alone is insufficient because test suites set AGEND_HOME
/// under temp, so a live `.agend-managed` worktree can sit under temp. The production
/// `git()` helper must PANIC via the ancestor-marker walk (root + Fable r1 blocker).
/// Targets a SUBDIR so the walk is exercised, not just the leaf.
#[test]
#[should_panic(expected = "daemon-MANAGED worktree")]
fn t122_rejects_managed_worktree_under_temp() {
    // <temp>/t122-managed-<pid>-<ns>/worktrees/dev/feat/{.agend-managed, src/}
    let root = fresh_temp_root("managed");
    let _g = TreeGuard(root.clone()); // drops during the (expected) unwind — no leak
    let wt = root.join("worktrees").join("dev").join("feat");
    let sub = wt.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(wt.join(".agend-managed"), "dev\n").unwrap(); // MANAGED_MARKER
                                                                 // reaches the production git() chokepoint against a subdir under a managed ancestor;
                                                                 // the ancestor-marker walk panics.
    let _ = git_isolated::git(&sub, &["status"]);
}

// ── git_managed_fixture — the d-68 sanctioned managed-worktree ESCAPE HATCH REDs ──
// The default git() fence above rejects EVERY `.agend-managed` ancestor. git_managed_fixture
// is the ONLY sanctioned way to run real git against a hermetic managed worktree, gated by
// process ownership of a temp home. RED-P proves the owned path SUCCEEDS; N1–N4 prove the
// fail-closed rejections. (This file is the d-68 guard SELF-TEST role.)

/// (RED-P) a VALID owned managed fixture SUCCEEDS: home under temp + repo under it +
/// `<home>/.agend-fixture-owned` = THIS PID → real git runs against a `.agend-managed`
/// worktree that the default `git()` would reject.
#[test]
fn t122_managed_fixture_valid_owned_succeeds() {
    let home = fresh_temp_root("mf-ok");
    let _g = TreeGuard(home.clone());
    let repo = home.join("worktrees").join("dev").join("feat");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join(".agend-managed"), "dev\n").unwrap(); // a MANAGED worktree
    git_isolated::write_fixture_owned_sentinel(&home);
    let init = git_isolated::git_managed_fixture(&home, &repo, &["init", "-q", "-b", "main"]);
    assert!(
        init.status.success(),
        "owned managed fixture `git init` failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let inside =
        git_isolated::git_managed_fixture(&home, &repo, &["rev-parse", "--is-inside-work-tree"]);
    assert_eq!(
        String::from_utf8_lossy(&inside.stdout).trim(),
        "true",
        "owned managed fixture must run REAL git inside the managed repo"
    );
}

/// (N1) a missing ownership sentinel → REFUSED.
#[test]
#[should_panic(expected = "absent/unreadable")]
fn t122_managed_fixture_missing_sentinel_refused() {
    let home = fresh_temp_root("mf-n1");
    let _g = TreeGuard(home.clone());
    let repo = home.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    // NO sentinel written
    let _ = git_isolated::git_managed_fixture(&home, &repo, &["status"]);
}

/// (N2) a stale/foreign sentinel PID (not this process) → REFUSED.
#[test]
#[should_panic(expected = "stale/foreign")]
fn t122_managed_fixture_foreign_pid_refused() {
    let home = fresh_temp_root("mf-n2");
    let _g = TreeGuard(home.clone());
    let repo = home.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    // a DIFFERENT, valid-looking PID — never the current process
    std::fs::write(
        home.join(".agend-fixture-owned"),
        std::process::id().wrapping_add(1).to_string(),
    )
    .unwrap();
    let _ = git_isolated::git_managed_fixture(&home, &repo, &["status"]);
}

/// (N3) a repo NOT under the owned home (sentinel from a DIFFERENT home) → REFUSED.
#[test]
#[should_panic(expected = "cross-home")]
fn t122_managed_fixture_cross_home_refused() {
    let home = fresh_temp_root("mf-n3-home");
    let _gh = TreeGuard(home.clone());
    std::fs::create_dir_all(&home).unwrap();
    git_isolated::write_fixture_owned_sentinel(&home);
    // a repo under a SIBLING temp home, not under `home`
    let other = fresh_temp_root("mf-n3-other");
    let _go = TreeGuard(other.clone());
    let repo = other.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    let _ = git_isolated::git_managed_fixture(&home, &repo, &["status"]);
}

/// (N4) a home NOT under the system temp dir → REFUSED.
#[test]
#[should_panic(expected = "NOT under the system temp dir")]
fn t122_managed_fixture_non_temp_home_refused() {
    // the enclosing crate — a real dir, never under temp
    let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = home.join("src");
    let _ = git_isolated::git_managed_fixture(&home, &repo, &["status"]);
}

/// (task122 RED — d-68 allowlist) `git_managed_fixture` is a sanctioned ESCAPE HATCH with
/// exactly THREE roles: the DEFINITION in tests/common/git_isolated.rs, exactly ONE real
/// consumer CALL-SITE in tests/e2e_workflow.rs, and the guard SELF-TEST in THIS file. Any
/// other file — or a second consumer call-site — fails LOUD, so the hatch cannot spread.
#[test]
fn t122_git_managed_fixture_allowlist_roles() {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&root.join("src"), &mut files);
    walk(&root.join("tests"), &mut files);
    let (mut def_seen, mut selftest_seen, mut consumer_calls) = (false, false, 0usize);
    for f in &files {
        let body = std::fs::read_to_string(f).unwrap_or_default();
        if !body.contains("git_managed_fixture") {
            continue;
        }
        if f.ends_with("common/git_isolated.rs") {
            assert!(
                body.contains("pub fn git_managed_fixture("),
                "the def file must DEFINE git_managed_fixture"
            );
            def_seen = true;
        } else if f.ends_with("fixture_real_git_provenance.rs") {
            selftest_seen = true; // exempt: the guard self-test exercises + names the hatch
        } else if f.ends_with("e2e_workflow.rs") {
            consumer_calls = body.matches("git_managed_fixture(").count();
        } else {
            panic!(
                "unauthorized git_managed_fixture reference in {} — the hatch is allowlisted to \
                 git_isolated.rs (def) + e2e_workflow.rs (ONE consumer) + \
                 fixture_real_git_provenance.rs (self-test) only",
                f.display()
            );
        }
    }
    assert!(def_seen, "git_managed_fixture definition not found");
    assert!(
        selftest_seen,
        "self-test file must reference git_managed_fixture"
    );
    assert_eq!(
        consumer_calls, 1,
        "tests/e2e_workflow.rs must have EXACTLY ONE git_managed_fixture call-site (found {consumer_calls})"
    );
}

/// (task122 RED — shell fence) the shell real-git seam is FIXTURE-ONLY: every script
/// under scripts/ that references scripts/lib/real-git.sh MUST be an audited fixture
/// script. A NEW (unaudited) consumer fails this LOUD — the shell analogue of the src/
/// source invariant, closing "future non-fixture shell consumers".
#[test]
fn t122_shell_real_git_consumers_are_audited_fixtures_only() {
    // Audited fixture scripts permitted to source lib/real-git.sh (by file name).
    const AUDITED: &[&str] = &["test_fmt_owned.sh"];
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "sh").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    let scripts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts");
    let mut shs = Vec::new();
    walk(&scripts, &mut shs);
    let unaudited: Vec<String> = shs
        .iter()
        // the seam definition itself is not a "consumer"
        .filter(|p| !p.ends_with("lib/real-git.sh"))
        .filter(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .contains("real-git.sh")
        })
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !AUDITED.contains(&name)
        })
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        unaudited.is_empty(),
        "unaudited shell consumer(s) of lib/real-git.sh — add to the audited fixture allowlist \
         only after review: {unaudited:?}"
    );
}

/// (task122 RED #3) the real-git seam is FIXTURE-ONLY — no production (`src/`)
/// consumer of the resolver symbols or the shell helper. The seam targets
/// test-created temp repos (`setup_temp_repo` → `std::env::temp_dir`), never the
/// enclosing worktree.
#[test]
fn t122_source_invariant_no_production_consumer() {
    fn walk(dir: &std::path::Path, needles: &[&str], hits: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, needles, hits);
            } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
                let c = std::fs::read_to_string(&p).unwrap_or_default();
                for n in needles {
                    if c.contains(n) {
                        hits.push(format!("{}: {n}", p.display()));
                    }
                }
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let needles = [
        "resolve_real_git_in",
        "real_git_first_path_from",
        "assert_real_git_or_die",
        "lib/real-git.sh",
    ];
    let mut hits = Vec::new();
    walk(&src, &needles, &mut hits);
    assert!(
        hits.is_empty(),
        "the real-git fixture seam must have NO production (src/) consumer: {hits:?}"
    );
}
