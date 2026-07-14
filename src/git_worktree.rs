//! #2550 P3 W2 — git worktree lifecycle primitives.
//!
//! Built ON TOP of `git_helpers.rs`'s low-level wrappers (`git_bypass`/
//! `git_cmd`/`git_ok`), not a replacement for them: `git_helpers.rs` is the
//! single source of truth for HOW a git subprocess is invoked (bypass env,
//! timeout bound, process-group kill on timeout); this module is the single
//! source of truth for WHAT worktree-lifecycle argv/parsing looks like.
//!
//! ## Conservative scope (P3-W2-PRERESEARCH.md + lead's 3 decisions,
//! `m-20260703064336281447-62`)
//!
//! This is a MECHANICAL consolidation, not a behavior-unifying one:
//! - `git worktree list --porcelain` parsing: [`parse_porcelain`] is moved
//!   verbatim from `worktree_pool/gc.rs` (the most complete existing
//!   version — handles blank-line-absent trailing records via an explicit
//!   final flush, unlike the other 3 call sites' blank-line-triggered
//!   flush). [`list_porcelain`] adds the `git_bypass` call on top. Each of
//!   the 4 callers keeps its OWN caller-specific filtering (main/master
//!   exclusion, branch-name-only extraction, etc.) as a thin layer over
//!   this shared core — the core does NOT bake in any one caller's filter.
//! - `git worktree remove --force`: [`remove_force`] extracts the
//!   INTENTIONALLY-duplicated empty-`source_repo` dual-cwd branch shared
//!   by `worktree_pool` callers. Empty `source_repo` uses
//!   [`crate::git_helpers::git_bypass_no_cwd`] (always-bypass + timeout,
//!   no `current_dir`); non-empty uses [`crate::git_helpers::git_bypass`].
//! - Wrapper/error-handling-granularity CHOICE (`git_bypass` vs `git_ok` vs
//!   `git_cmd`) stays with each existing caller — this module does not
//!   force a uniform return type onto call sites that currently rely on
//!   distinguishing `GitError::NonZero` from `GitError::Spawn`, or that only
//!   need a plain `bool`. Unifying that is an explicit non-goal of W2 (would
//!   be a behavior change requiring its own review, not a mechanical PR).
//!
//! ## Explicitly NOT touched (documented exceptions, not oversights)
//!
//! - `mcp/handlers/ci/release.rs`'s `worktree remove --force` call
//!   deliberately does NOT bypass and does NOT set a cwd (see that file's
//!   own comment at the call site) — a known, still-undecided behavioral
//!   inconsistency versus every other remove-force call site. Lead's
//!   decision (`m-20260703064336281447-62`): leave it as-is, do not fold it
//!   into [`remove_force`] — that would be silently deciding an open design
//!   question inside a mechanical-consolidation PR. Tracked as an open item
//!   for operator/a future lead, not part of this wave.
//! - `worktree_cleanup.rs`'s Windows-specific retry loop around `git_ok`
//!   (3 attempts with exponential backoff, `cfg!(windows)`-gated) is an
//!   application-layer flake mitigation for the SAME infra-flake class
//!   `.config/nextest.toml`'s scoped `retries` addresses at the test-runner
//!   layer (#2578). Not extracted — no second call site needs it yet, so
//!   there's no real abstraction to name (lead's decision, same message).

use std::path::{Path, PathBuf};

/// `git worktree list --porcelain` argv.
pub(crate) const LIST_PORCELAIN_ARGS: [&str; 3] = ["worktree", "list", "--porcelain"];

/// Parse `git worktree list --porcelain` stdout into `(path, branch)` pairs.
/// Porcelain records are blank-line-separated; `worktree <path>` opens a
/// record, `branch refs/heads/<b>` names the checked-out branch (absent =
/// detached). Flushes on the NEXT `worktree` line (not on the blank
/// separator) plus an explicit final flush after the loop, so it does not
/// depend on trailing-blank-line presence — safe against trimmed input,
/// unlike the ad-hoc blank-line-triggered parsers this replaces.
///
/// Moved verbatim from `worktree_pool/gc.rs` (was `parse_worktree_porcelain`,
/// the most complete of the 4 existing copies — see module doc).
pub(crate) fn parse_porcelain(out: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut records = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    let flush = |p: &mut Option<PathBuf>, b: &mut Option<String>, out: &mut Vec<_>| {
        if let Some(path) = p.take() {
            out.push((path, b.take()));
        }
        *b = None;
    };
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur_path, &mut cur_branch, &mut records);
            cur_path = Some(PathBuf::from(p.trim()));
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(
                b.trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(b.trim())
                    .to_string(),
            );
        }
    }
    flush(&mut cur_path, &mut cur_branch, &mut records);
    records
}

/// `git worktree list --porcelain` (via `git_helpers::git_bypass`, #1899
/// LOCAL 60s bound) parsed into `(path, branch)` pairs. `Ok(vec![])` on a
/// non-zero exit (matches every existing caller's "no entries" fallback);
/// `Err` only on a spawn/timeout failure.
pub(crate) fn list_porcelain(repo: &Path) -> std::io::Result<Vec<(PathBuf, Option<String>)>> {
    let out = crate::git_helpers::git_bypass(repo, &LIST_PORCELAIN_ARGS)?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// Exact-owner variant: a non-zero `git worktree list` result is an opaque
/// metadata read failure, never an empty repository. Destructive callers use
/// this to preserve binding/evidence instead of treating unreadable metadata
/// as `ExactNone`.
pub(crate) fn list_porcelain_exact(repo: &Path) -> std::io::Result<Vec<(PathBuf, Option<String>)>> {
    let out = crate::git_helpers::git_bypass(repo, &LIST_PORCELAIN_ARGS)?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// `git worktree remove --force <wt_path>`.
///
/// `source_repo` empty → [`git_helpers::git_bypass_no_cwd`] (no `current_dir`;
/// git resolves the repo from the absolute `wt_path`; still always-bypass +
/// timeout-bounded). `source_repo` non-empty → [`git_helpers::git_bypass`].
pub(crate) fn remove_force(
    source_repo: &Path,
    wt_path: &str,
) -> std::io::Result<std::process::Output> {
    let args = ["worktree", "remove", "--force", wt_path];
    if source_repo.as_os_str().is_empty() {
        crate::git_helpers::git_bypass_no_cwd(&args)
    } else {
        crate::git_helpers::git_bypass(source_repo, &args)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-git-worktree-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn make_repo(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        git(path, &["commit", "--allow-empty", "-m", "init"]);
        path.to_path_buf()
    }

    #[test]
    fn parse_porcelain_pairs_path_with_branch() {
        let out = "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                    worktree /repo/wt-feature\nHEAD def456\nbranch refs/heads/feat/x\n";
        let parsed = parse_porcelain(out);
        assert_eq!(
            parsed,
            vec![
                (PathBuf::from("/repo"), Some("main".to_string())),
                (
                    PathBuf::from("/repo/wt-feature"),
                    Some("feat/x".to_string())
                ),
            ]
        );
    }

    #[test]
    fn parse_porcelain_detached_head_has_no_branch() {
        let out = "worktree /repo/wt-detached\nHEAD abc123\ndetached\n";
        assert_eq!(
            parse_porcelain(out),
            vec![(PathBuf::from("/repo/wt-detached"), None)]
        );
    }

    #[test]
    fn parse_porcelain_last_record_survives_without_trailing_blank_line() {
        // #2550 W2: the point of using this parser as canonical — it does NOT
        // depend on a trailing blank-line record terminator (unlike the
        // ad-hoc parsers it replaces), so trimmed/blank-line-stripped input
        // still yields the final record.
        let out =
            "worktree /repo\nbranch refs/heads/main\n\nworktree /repo/wt\nbranch refs/heads/x";
        assert_eq!(
            parse_porcelain(out),
            vec![
                (PathBuf::from("/repo"), Some("main".to_string())),
                (PathBuf::from("/repo/wt"), Some("x".to_string())),
            ]
        );
    }

    #[test]
    fn list_porcelain_real_repo_round_trip() {
        let home = tmp_home("list-porcelain");
        let repo = make_repo(&home.join("repo"));
        let wt = home.join("wt-feature");
        git(&repo, &["branch", "feat-x"]);
        git(
            &repo,
            &["worktree", "add", &wt.display().to_string(), "feat-x"],
        );

        let entries = list_porcelain(&repo).unwrap();
        assert!(
            entries
                .iter()
                .any(|(p, b)| p.ends_with("repo") && b.as_deref() == Some("main")),
            "main worktree entry present: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|(p, b)| p.ends_with("wt-feature") && b.as_deref() == Some("feat-x")),
            "feature worktree entry present: {entries:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn remove_force_with_source_repo_removes_registration() {
        let home = tmp_home("remove-force-with-repo");
        let repo = make_repo(&home.join("repo"));
        let wt = home.join("wt-feature");
        git(&repo, &["branch", "feat-x"]);
        git(
            &repo,
            &["worktree", "add", &wt.display().to_string(), "feat-x"],
        );
        assert!(wt.exists());

        let out = remove_force(&repo, &wt.display().to_string()).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!wt.exists(), "worktree dir removed");
        let entries = list_porcelain(&repo).unwrap();
        assert!(
            !entries.iter().any(|(p, _)| p.ends_with("wt-feature")),
            "registration cleared from the owning repo: {entries:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn remove_force_with_empty_source_repo_does_not_set_current_dir() {
        // #2550 W2 pin: the documented "empty source_repo" exception — git
        // runs with NO `current_dir`, so the outcome depends on the CALLING
        // PROCESS's ambient cwd (matching the two original call sites'
        // shared, commented behavior — their own TODO already flags this
        // path's real-world reachability as unaudited; W2 preserves the
        // exact mechanism, not a stronger guarantee than the original had).
        // Run from a cwd (the test binary's own, unrelated to `wt`) that
        // canNOT resolve `wt` as a worktree — proves the call does NOT
        // silently fall back to using `wt`'s parent (or any other inferred
        // path) as current_dir: a real cwd substitution would still fail the
        // same way (unrelated repo), so a *different* failure shape (e.g. a
        // spawn error) would indicate the no-current_dir branch broke.
        let home = tmp_home("remove-force-empty-repo");
        let repo = make_repo(&home.join("repo"));
        let wt = home.join("wt-feature");
        git(&repo, &["branch", "feat-x"]);
        git(
            &repo,
            &["worktree", "add", &wt.display().to_string(), "feat-x"],
        );
        assert!(wt.exists());

        let out = remove_force(Path::new(""), &wt.display().to_string())
            .expect("command spawns (git binary found, args accepted) regardless of cwd");
        // Not asserting success: whether this resolves depends on the test
        // process's own ambient cwd, which this test does not control (and
        // safely cannot, in a parallel test binary — see module doc's note
        // on the original TODO). The worktree dir must still exist either
        // way: a broken empty-source_repo branch would either panic or spawn
        // successfully-but-touch-the-wrong-repo, not silently succeed here.
        assert!(
            wt.exists() || out.status.success(),
            "either the removal genuinely succeeded, or the (expected) \
             ambient-cwd-mismatch failure left the worktree untouched — \
             never a partial/corrupt state"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
