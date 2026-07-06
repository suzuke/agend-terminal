//! #2390 ③: resolve the push guards' range base (the remote's default branch)
//! instead of hardcoding `origin/main`. A non-main-default repo (master / trunk /
//! develop) otherwise makes `origin/main..HEAD` un-resolvable → the denylist
//! fails CLOSED → every push is blocked. Shared by both `push_range_files`
//! (denylist, hard) and `cleanup_init_pile_pre_push` (hygiene, soft).

use std::process::Command;

/// Resolve the range base for the push guards as a rev (`origin/<default>`), so
/// callers diff `origin/<default>..HEAD`.
///
/// Fallback order (conservative — always a TRUNK, never the branch's own upstream):
/// 1. `git symbolic-ref --short refs/remotes/origin/HEAD` — the authoritative
///    default branch WHEN SET. We use `symbolic-ref`, NOT
///    `git rev-parse --abbrev-ref origin/HEAD`: the latter LIES — it echoes the
///    literal `"origin/HEAD"` (looks like success) instead of failing when the
///    ref is unset, which is the common case in managed worktrees that never ran
///    `git remote set-head`.
/// 2. Existence-probe the conventional trunks — but only when EXACTLY ONE of
///    `origin/main` / `origin/master` exists (a normal clone with origin/HEAD
///    unset but one trunk present keeps working). BOTH present is ambiguous — we
///    can't tell the default and must NOT guess `main` (that could scan the wrong
///    base and miss a trust-root file), so it fails to path 3 (#2662).
/// 3. Otherwise `Err` — the caller fails CLOSED (denylist) / no-ops (cleanup), so
///    an undeterminable OR ambiguous base stays safe. Remedy (resolves both):
///    `git remote set-head origin -a`.
///
/// Deliberately does NOT consult the branch's `@{upstream}`: post-push it points
/// at the branch's OWN remote ref, so diffing against it would shrink the denylist
/// scan to "commits since the last push" and weaken the guardrail. The denylist
/// must scan the whole branch vs. trunk — conservative over precise.
pub fn resolve_default_branch_base(worktree: &str) -> Result<String, String> {
    // 1. origin/HEAD, only when explicitly set (symbolic-ref errors cleanly if not).
    if let Ok(o) = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        if o.status.success() {
            let head = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !head.is_empty() {
                return Ok(head);
            }
        }
    }
    // 2. Conventional-trunk existence probe — use it ONLY when EXACTLY ONE of
    //    origin/main / origin/master exists. If BOTH exist we cannot tell which is
    //    the default (origin/HEAD would have said, but it's unset), so we FAIL
    //    rather than guess: blindly picking the non-default trunk could scan
    //    `<wrong-trunk>..HEAD` and OMIT a trust-root commit reachable only from the
    //    true default — a fail-open false-negative in the denylist (#2662).
    match (
        trunk_exists(worktree, "origin/main"),
        trunk_exists(worktree, "origin/master"),
    ) {
        (true, false) => return Ok("origin/main".to_string()),
        (false, true) => return Ok("origin/master".to_string()),
        (true, true) => {
            return Err(
                "ambiguous default branch: both origin/main and origin/master \
                        exist and origin/HEAD is unset — cannot safely pick the trunk"
                    .to_string(),
            )
        }
        (false, false) => {}
    }
    // 3. Undeterminable.
    Err(
        "cannot resolve the remote default branch: origin/HEAD is unset and neither \
         origin/main nor origin/master exists"
            .to_string(),
    )
}

/// Does `<rev>` resolve to a commit in `worktree`? `.output()` (not `.status()`)
/// so the `rev-parse` SHA never leaks onto the shim's stdout.
fn trunk_exists(worktree: &str, rev: &str) -> bool {
    Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
