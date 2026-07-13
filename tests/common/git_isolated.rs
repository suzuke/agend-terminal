//! #821 / task122 — isolated git subprocess invocation for test fixtures.
//!
//! Test fixtures that shell out to `git` MUST use this module instead of
//! `std::process::Command::new("git")` directly. On an agent's PATH, `git`
//! resolves to the `agend-git` SHIM, which (a) denies `git -c … <subcommand>`
//! forms, (b) reroutes a `-C <tmp>` fixture into the agent's BOUND worktree, and
//! (c) is inherited by CHILD git processes (`git submodule` → `git clone`). The
//! prior `AGEND_GIT_BYPASS=1` reliance did not fully neutralize it.
//!
//! task122 — **real-git provenance seam** (fixture-only). Resolve the REAL git
//! binary (excluding the shim dir[s]), export a real-git-FIRST PATH to the entire
//! spawned fixture process tree, and FAIL LOUD (panic, never SKIP) when only the
//! shim resolves. The seam targets test-created temp repos only; it is NOT a
//! production consumer and does NOT change shim policy or `AGEND_GIT_BYPASS`.
//!
//! Author/committer env is pinned (CI runners lack a global `~/.gitconfig`), and
//! `current_dir(repo_dir)` pins cwd so git's upward `.git` discovery can't leak
//! into the host worktree.
//!
//! ## Pattern to USE
//!
//! ```rust,ignore
//! use crate::common::git_isolated;
//! let repo = git_isolated::setup_temp_repo("my-tag");
//! git_isolated::git(&repo, &["checkout", "-b", "feat-b"]);
//! ```
//!
//! ## Allowlist
//!
//! Pre-existing test files using raw `Command::new("git")` are grandfathered via
//! `tests/git_subprocess_invariant.rs`. New tests MUST use this helper.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Actionable message for a provenance failure — a fixture must run against REAL
/// git, so we panic rather than silently proceed on the shim or SKIP.
const PROVENANCE_HINT: &str = "a fixture must run against REAL git, but after excluding the \
    agend-git shim dir(s) no real git resolved on PATH. Set AGEND_REAL_GIT to a real git binary \
    or put one on PATH. This is a fail-loud provenance guard (task122) — never a SKIP.";

/// Marker file the daemon places in a managed worktree. MIRRORS
/// `worktree_pool::MANAGED_MARKER` (bin-private → not importable from an integration
/// test); it is the SAME authoritative signal `worktree_pool::is_daemon_managed` uses.
const MANAGED_MARKER: &str = ".agend-managed";

/// Ownership sentinel for [`git_managed_fixture`] (d-68). Placed by the sanctioned e2e
/// at `<home>/.agend-fixture-owned` BEFORE the daemon starts; it contains THIS test
/// process's PID. The managed-fixture escape hatch runs real git against a daemon
/// managed worktree ONLY when this proves the current process owns the home.
pub const FIXTURE_OWNED_SENTINEL: &str = ".agend-fixture-owned";

/// Directories whose `git` is the agend-git shim (or its default install), to be
/// EXCLUDED from real-git resolution: `$AGEND_HOME/bin` and `~/.agend-terminal/bin`.
#[allow(dead_code)]
pub fn shim_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(h) = std::env::var_os("AGEND_HOME") {
        dirs.push(PathBuf::from(h).join("bin"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".agend-terminal").join("bin"));
    }
    dirs
}

/// True when `a` and `b` name the same directory. Prefers `canonicalize` (resolves
/// symlink/slash form, case-folds on Windows); falls back to a lexical compare when
/// a path doesn't exist on disk. Mirrors `agent/mod.rs::same_dir` (#1504).
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => {
            let norm = |p: &Path| {
                p.to_string_lossy()
                    .replace('\\', "/")
                    .trim_end_matches('/')
                    .to_string()
            };
            let (na, nb) = (norm(a), norm(b));
            if cfg!(windows) {
                na.eq_ignore_ascii_case(&nb)
            } else {
                na == nb
            }
        }
    }
}

/// Resolve the REAL git binary by scanning `path`, EXCLUDING every entry in
/// `shim_dirs`, then PROVE the candidate answers `git version`. Returns an
/// actionable `Err` when nothing but the shim resolves — callers fail LOUD, never
/// SKIP (task122 provenance contract). PURE w.r.t. process env (only the resolved
/// binary is spawned for the probe), so REDs drive it with a simulated PATH + fake
/// shim dir. `AGEND_REAL_GIT` honoring is layered on in [`cached_real`], NOT here,
/// so the "only shim on PATH" case is deterministically testable.
#[allow(dead_code)]
pub fn resolve_real_git_in(path: &OsStr, shim_dirs: &[PathBuf]) -> Result<PathBuf, String> {
    let search: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !shim_dirs.iter().any(|s| same_dir(p, s)))
        .collect();
    // An empty search set must FAIL, not fall back to the ambient PATH (which
    // `which_in` would do for an empty string) — else "only shim" resolves the shim.
    if search.is_empty() {
        return Err(format!("real-git provenance FAILED — {PROVENANCE_HINT}"));
    }
    let joined =
        std::env::join_paths(&search).map_err(|e| format!("real-git PATH join failed: {e}"))?;
    let cand = which::which_in("git", Some(&joined), ".")
        .map_err(|_| format!("real-git provenance FAILED — {PROVENANCE_HINT}"))?;
    if !probe_is_git(&cand) {
        return Err(format!(
            "real-git provenance FAILED — {} did not respond to `git version`. {PROVENANCE_HINT}",
            cand.display()
        ));
    }
    Ok(cand)
}

/// Prove a candidate binary is a working git (`git version` → `git version …`).
fn probe_is_git(cand: &Path) -> bool {
    Command::new(cand)
        .arg("version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).starts_with("git version"))
        .unwrap_or(false)
}

/// A PATH with the resolved real git's DIRECTORY first, so the whole spawned
/// fixture process tree (incl. child `git submodule` → `git`) uses real git.
#[allow(dead_code)]
pub fn real_git_first_path_from(path: &OsStr, shim_dirs: &[PathBuf]) -> Result<OsString, String> {
    let real = resolve_real_git_in(path, shim_dirs)?;
    let dir = real
        .parent()
        .ok_or_else(|| format!("resolved real git has no parent dir: {}", real.display()))?;
    let mut entries = vec![dir.to_path_buf()];
    entries.extend(std::env::split_paths(path));
    std::env::join_paths(entries).map_err(|e| format!("real-git PATH join failed: {e}"))
}

/// An explicitly-injected `AGEND_REAL_GIT` (existing production allowlist plumbing —
/// agent/mod.rs), accepted only when it is a real git OUTSIDE the shim dir(s).
fn explicit_real_git(shim_dirs: &[PathBuf]) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("AGEND_REAL_GIT")?);
    let in_shim = p
        .parent()
        .is_some_and(|par| shim_dirs.iter().any(|s| same_dir(par, s)));
    (p.is_file() && !in_shim && probe_is_git(&p)).then_some(p)
}

/// Process-cached (real git binary, real-git-first PATH). Resolved once: an
/// explicit `AGEND_REAL_GIT` (if a real git outside the shim) else a shim-excluding
/// PATH scan. Panics fail-loud on a provenance failure (never SKIP).
fn cached_real() -> &'static (PathBuf, OsString) {
    static CACHE: OnceLock<(PathBuf, OsString)> = OnceLock::new();
    CACHE.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs = shim_dirs();
        let real = match explicit_real_git(&dirs) {
            Some(p) => p,
            None => {
                resolve_real_git_in(&path, &dirs).unwrap_or_else(|e| panic!("git_isolated: {e}"))
            }
        };
        let dir = real
            .parent()
            .unwrap_or_else(|| panic!("git_isolated: resolved real git has no parent dir"));
        let mut entries = vec![dir.to_path_buf()];
        entries.extend(std::env::split_paths(&path));
        let first =
            std::env::join_paths(entries).unwrap_or_else(|e| panic!("git_isolated: PATH join {e}"));
        (real, first)
    })
}

/// Fail-loud RUNTIME provenance fence: a fixture repo dir MUST live UNDER the system
/// temp dir. The seam targets ONLY test-created temp repos (`setup_temp_repo` →
/// `std::env::temp_dir`) and must NEVER run real git against the enclosing
/// agend-terminal worktree (`CARGO_MANIFEST_DIR`) or any managed worktree — the src-
/// symbol source invariant cannot catch a runtime `git(Path::new(MANIFEST), …)` call,
/// so this closes it AT the chokepoint (task122: "prevents use against the enclosing
/// repo"). Canonicalizes both sides so the macOS `/var`→`/private/var` symlink and
/// `.`/symlink path forms cannot defeat the check.
fn assert_temp_repo(repo_dir: &Path) {
    let tmp = std::env::temp_dir();
    let canon_tmp = tmp.canonicalize().unwrap_or(tmp);
    let canon_repo = repo_dir.canonicalize().unwrap_or_else(|e| {
        panic!(
            "git_isolated: fixture repo_dir {} cannot be canonicalized ({e}); the real-git \
             seam targets ONLY test-created temp repos under {}",
            repo_dir.display(),
            canon_tmp.display()
        )
    });
    assert!(
        canon_repo.starts_with(&canon_tmp),
        "git_isolated: refusing to run real git against {} — it is NOT under the system temp \
         dir ({}). The fixture seam is fenced to test-created temp repos and must NEVER operate \
         on the enclosing worktree / CARGO_MANIFEST_DIR (task122 source invariant).",
        canon_repo.display(),
        canon_tmp.display(),
    );
    // `starts_with(temp)` is necessary but NOT sufficient: a daemon-MANAGED worktree can
    // live UNDER the system temp dir — test suites set AGEND_HOME under temp_dir, so
    // `<temp>/…/worktrees/<agent>/<branch>/.agend-managed` is a real topology. Running real
    // git (shim stripped) against live managed state is the exact pollution class this fence
    // prevents (root + Fable r1 blocker). Reject if `repo_dir` OR any ancestor up to
    // `canon_tmp` carries the `.agend-managed` marker — the SAME signal the daemon uses
    // (worktree_pool::is_daemon_managed). Ancestor walk bounded at canon_tmp; fail-CLOSED on
    // an un-statable marker (`try_exists` Err → treat as present → panic).
    let mut cur: &Path = &canon_repo;
    loop {
        if cur.join(MANAGED_MARKER).try_exists().unwrap_or(true) {
            panic!(
                "git_isolated: refusing to run real git against {} — it (or an ancestor at {}) is \
                 a daemon-MANAGED worktree ({MANAGED_MARKER} marker). The seam targets ONLY \
                 test-created temp repos, NEVER a managed worktree, even under the temp dir \
                 (task122 source invariant).",
                canon_repo.display(),
                cur.display(),
            );
        }
        if cur == canon_tmp {
            break;
        }
        cur = match cur.parent() {
            Some(p) => p,
            None => break,
        };
    }
}

/// Build the REAL-git command (real-git-first PATH children inherit, cwd pin, agent-
/// session env scrubbed, pinned author/committer). NO fence — the shared spawn primitive
/// behind the two FENCED public entries: `base_cmd` (→ `git`/`git_dated`, temp-repo fence)
/// and [`git_managed_fixture`] (ownership fence). Private; never call it directly.
fn real_git_cmd(repo_dir: &Path, args: &[&str]) -> Command {
    let (real, real_path) = cached_real();
    let mut c = Command::new(real);
    c.args(args)
        .current_dir(repo_dir)
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env("PATH", real_path)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example");
    c
}

/// Base command for the default fixture entries: temp-repo fenced (rejects any
/// `.agend-managed` ancestor), then the shared real-git command.
fn base_cmd(repo_dir: &Path, args: &[&str]) -> Command {
    assert_temp_repo(repo_dir);
    real_git_cmd(repo_dir, args)
}

/// Fail-CLOSED ownership fence for [`git_managed_fixture`] (d-68). Proves the CURRENT
/// process owns a hermetic temp home before letting real git touch a managed worktree:
/// (N4) `home` canonicalizes UNDER the system temp dir; (N3) `repo_dir` canonicalizes
/// under that EXACT `home`; (N1/N2) `<home>/.agend-fixture-owned` is readable and holds
/// THIS process's PID — reject absent / unreadable / malformed / stale-or-foreign PID.
fn assert_owned_managed_fixture(home: &Path, repo_dir: &Path) {
    let tmp = std::env::temp_dir();
    let canon_tmp = tmp.canonicalize().unwrap_or(tmp);
    let canon_home = home.canonicalize().unwrap_or_else(|e| {
        panic!(
            "git_managed_fixture: home {} cannot be canonicalized ({e}) — refusing",
            home.display()
        )
    });
    let canon_repo = repo_dir.canonicalize().unwrap_or_else(|e| {
        panic!(
            "git_managed_fixture: repo {} cannot be canonicalized ({e}) — refusing",
            repo_dir.display()
        )
    });
    // (N4) home under the system temp dir
    assert!(
        canon_home.starts_with(&canon_tmp),
        "git_managed_fixture: home {} is NOT under the system temp dir ({}) — refusing",
        canon_home.display(),
        canon_tmp.display(),
    );
    // (N3) repo under that EXACT home (no cross-home reuse of a sentinel)
    assert!(
        canon_repo.starts_with(&canon_home),
        "git_managed_fixture: repo {} is NOT under the owned home {} — refusing (cross-home)",
        canon_repo.display(),
        canon_home.display(),
    );
    // (N1) sentinel present + readable
    let sentinel = canon_home.join(FIXTURE_OWNED_SENTINEL);
    let raw = std::fs::read_to_string(&sentinel).unwrap_or_else(|e| {
        panic!(
            "git_managed_fixture: ownership sentinel {} absent/unreadable ({e}) — refusing",
            sentinel.display()
        )
    });
    // malformed → refuse
    let owner: u32 = raw.trim().parse().unwrap_or_else(|_| {
        panic!(
            "git_managed_fixture: ownership sentinel {} is malformed ({raw:?}) — refusing",
            sentinel.display()
        )
    });
    // (N2) stale/foreign PID → refuse
    let me = std::process::id();
    assert!(
        owner == me,
        "git_managed_fixture: ownership sentinel {} PID {owner} != current process {me} \
         (stale/foreign) — refusing",
        sentinel.display(),
    );
}

/// Sanctioned ESCAPE HATCH (d-68) for the ONE hermetic e2e workflow test: run real git
/// against a daemon-created MANAGED worktree — which the default [`git`] fence rejects —
/// gated by explicit process ownership of the hermetic temp `home`.
///
/// This is a NON-ADVERSARIAL FOOTGUN GUARD, NOT same-UID containment: a same-UID
/// adversary can forge `<home>/.agend-fixture-owned`. Its job is to stop a fixture from
/// ACCIDENTALLY driving real git against the wrong (or a live/foreign) managed worktree,
/// while still letting the sanctioned e2e drive its OWN hermetic mock-dev worktree.
///
/// Source-allowlisted to `tests/e2e_workflow.rs` (enforced by the allowlist invariant in
/// `tests/fixture_real_git_provenance.rs`). Fail-closed via [`assert_owned_managed_fixture`].
///
/// allow: raw-git-subprocess  // canonical helper module — exempt
#[allow(dead_code)]
pub fn git_managed_fixture(home: &Path, repo_dir: &Path, args: &[&str]) -> Output {
    assert_owned_managed_fixture(home, repo_dir);
    real_git_cmd(repo_dir, args)
        .output()
        .expect("git subprocess spawn")
}

/// Write the [`git_managed_fixture`] ownership sentinel into `home` for THIS process
/// (the single source of the exact PID format the guard verifies). The sanctioned e2e
/// calls this once, BEFORE the daemon starts — a synchronous write that completes before
/// any reader exists (no atomic rename needed; malformed contents fail closed in the guard).
#[allow(dead_code)]
pub fn write_fixture_owned_sentinel(home: &Path) {
    std::fs::write(
        home.join(FIXTURE_OWNED_SENTINEL),
        std::process::id().to_string(),
    )
    .unwrap_or_else(|e| {
        panic!(
            "git_isolated: write fixture-owned sentinel in {}: {e}",
            home.display()
        )
    });
}

/// Run git in `repo_dir` against REAL git (shim excluded), cwd-isolated, pinned
/// author/committer. The canonical entry for test fixtures.
///
/// allow: raw-git-subprocess  // canonical helper module — exempt
#[allow(dead_code)]
pub fn git(repo_dir: &Path, args: &[&str]) -> Output {
    base_cmd(repo_dir, args)
        .output()
        .expect("git subprocess spawn")
}

/// Variant accepting an explicit committer date for back-dating commits (stale-
/// detection tests per #817 — `chrono::now() - duration` is flaky near day
/// boundaries, so date-sensitive tests pin both author and committer dates).
///
/// allow: raw-git-subprocess  // canonical helper module — exempt
#[allow(dead_code)]
pub fn git_dated(repo_dir: &Path, args: &[&str], date_rfc3339: &str) -> Output {
    base_cmd(repo_dir, args)
        .env("GIT_AUTHOR_DATE", date_rfc3339)
        .env("GIT_COMMITTER_DATE", date_rfc3339)
        .output()
        .expect("git subprocess spawn")
}

/// Create a temp git repo with `main` branch + pinned per-repo gitconfig. Standard
/// entry for fixtures needing a fresh git repo. The per-repo `user.name`/`.email`
/// means CI runners without a global `~/.gitconfig` can still commit (#814 r1).
#[allow(dead_code)]
pub fn setup_temp_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-test-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir test temp repo");
    git(&dir, &["init", "-b", "main"]);
    git(&dir, &["config", "user.name", "test"]);
    git(&dir, &["config", "user.email", "test@example"]);
    dir
}
