//! Path predicates: canonical-root detection, git-dir/commondir
//! resolution, object-store identity, and directory equality helpers.

use std::env;
use std::path::{Path, PathBuf};

/// #778 Option 3 (originally) + #852 residual PR-A: detect that cwd
/// is rooted inside a canonical-origin git repo — either a
/// daemon-provisioned worktree (`.git` file with `gitdir:` pointer)
/// OR the canonical source repo itself (`.git` directory with
/// `[remote "origin"]` in config). The `origin` remote is what
/// distinguishes a canonical-rooted cwd from an orphan workspace-
/// placeholder repo (daemon startup creates these before fleet
/// config resolves; they have no remote and no project files).
///
/// **#852 residual fix**: the pre-PR-A logic required `.git` to be a
/// FILE (worktree marker shape only), which returned FALSE when the
/// caller was inside the canonical SOURCE REPO (`.git` is a
/// directory there). That gap let reviewer agents who `cd
/// canonical && git checkout <sha>` slip past the `is_agent_caller
/// && canonical_cwd` deny at line ~297-303, producing the
/// detached-HEAD pollution operator observed at 21:46 + 22:24 today
/// (`checkout: moving from main to <sha>` reflog entries post-21:23
/// daemon restart). The broadened detection covers BOTH shapes.
///
/// Renamed from `cwd_is_canonical_worktree` to `cwd_is_canonical_rooted`
/// — the previous name's "worktree" suffix was misleading after
/// broadening since the canonical source repo isn't a worktree.
pub(crate) fn cwd_is_canonical_rooted() -> bool {
    match env::current_dir() {
        Ok(cwd) => path_is_canonical_rooted(&cwd),
        Err(_) => false,
    }
}

/// #2234 Patch A: the canonical-rooted test for an ARBITRARY directory, so a
/// `git -C <path>` op can be judged against the dir git WOULD operate in rather
/// than the shim's process cwd. Extracted verbatim from `cwd_is_canonical_rooted`
/// (which now delegates with the process cwd — its behavior is byte-identical).
/// `<dir>/.git` is a DIR (canonical source repo) or a FILE (linked worktree)
/// whose origin config carries `[remote "origin"]`.
pub(crate) fn path_is_canonical_rooted(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    let meta = match std::fs::metadata(&dot_git) {
        Ok(m) => m,
        Err(_) => return false,
    };

    if meta.is_file() {
        // Worktree case (pre-#852-residual logic, unchanged).
        // `.git` file carries a `gitdir:` pointer to
        // `<source>/.git/worktrees/<entry>`; grandparent is
        // `<source>/.git` which carries the source repo's config.
        let content = match std::fs::read_to_string(&dot_git) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let gitdir_str = match content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
        {
            Some(s) => s,
            None => return false,
        };
        let gitdir = PathBuf::from(gitdir_str);
        let source_git_dir = match gitdir.parent().and_then(|p| p.parent()) {
            Some(p) => p.to_path_buf(),
            None => return false,
        };
        let config = match std::fs::read_to_string(source_git_dir.join("config")) {
            Ok(c) => c,
            Err(_) => return false,
        };
        config.contains("[remote \"origin\"]")
    } else if meta.is_dir() {
        // Canonical source repo case (#852 residual broadening).
        // `.git` is a directory; read `.git/config` directly to check
        // for `[remote "origin"]`. Same defense against orphan
        // workspace-placeholder repos that have no remote.
        let config = match std::fs::read_to_string(dot_git.join("config")) {
            Ok(c) => c,
            Err(_) => return false,
        };
        config.contains("[remote \"origin\"]")
    } else {
        // Unknown `.git` shape (symlink to neither file nor dir,
        // etc.) — fail closed.
        false
    }
}

/// #1463: resolve the `.git` directory for `start` the way git does — walk up
/// to the first `.git`, following a `.git` FILE's `gitdir:` pointer (linked
/// worktree) but NOT a `.git` symlink (fail-closed against pointer-craft).
/// Returns the canonicalized gitdir, or `None` on any IO/parse ambiguity.
/// Kept separate from `cwd_is_canonical_rooted` (which keys off `[remote
/// "origin"]` presence) per the #1463 ruling — same `.git`-resolve spirit,
/// different semantics (object-store identity vs canonical-origin membership).
pub(crate) fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(start).ok()?;
    let mut dir: &Path = &start;
    loop {
        let dot_git = dir.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return std::fs::canonicalize(&dot_git).ok(),
            Ok(meta) if meta.is_file() => {
                let content = std::fs::read_to_string(&dot_git).ok()?;
                let ptr = content
                    .lines()
                    .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))?;
                let p = PathBuf::from(ptr);
                let gitdir = if p.is_absolute() { p } else { dir.join(p) };
                return std::fs::canonicalize(&gitdir).ok();
            }
            // `.git` is a symlink or other irregular shape → fail-closed.
            Ok(_) => return None,
            // No `.git` here → walk up.
            Err(_) => dir = dir.parent()?,
        }
    }
}

/// #1463: resolve the COMMON git dir (shared object store + refs) for `start`,
/// mirroring git's `commondir` resolution — a linked worktree's gitdir carries
/// a `commondir` file pointing at the shared `<source>/.git`. Two paths that
/// resolve to the SAME commondir share one object store. Canonicalized; `None`
/// on any ambiguity. Defeats `.git`-pointer craft: a `.git` file pointing
/// `gitdir: <canonical>/.git` resolves to canonical's commondir (no `commondir`
/// file in the main gitdir → gitdir IS the common dir), so it is NOT seen as
/// foreign.
pub(crate) fn resolve_commondir(start: &Path) -> Option<PathBuf> {
    let gitdir = find_git_dir(start)?;
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(s) => {
            let rel = s.trim();
            if rel.is_empty() {
                gitdir.clone()
            } else {
                let p = PathBuf::from(rel);
                if p.is_absolute() {
                    p
                } else {
                    gitdir.join(p)
                }
            }
        }
        Err(_) => gitdir.clone(),
    };
    std::fs::canonicalize(&common).ok()
}

/// #1463 (A): the pure object-store-identity comparison behind
/// `cwd_is_foreign_repo` — `true` iff both paths resolve to a commondir AND the
/// two commondirs differ. Fail-closed (`false`) if either side is unresolvable.
/// Split out so the adversarial matrix is hermetically testable without
/// touching the process cwd.
pub(crate) fn paths_are_foreign(cwd: &Path, worktree: &Path) -> bool {
    match (resolve_commondir(cwd), resolve_commondir(worktree)) {
        (Some(c), Some(w)) => c != w,
        _ => false,
    }
}
/// #1504: directory equality via `canonicalize` (resolves slash form, case-folds
/// on Windows NTFS, follows symlinks), with a lexical fallback when a path can't
/// be canonicalized (e.g. `$AGENTIC_GIT_HOME/bin` not yet created). NEVER unwraps
/// `canonicalize` — it `Err`s on missing paths.
pub(crate) fn same_dir(a: &std::path::Path, b: Option<&std::path::Path>) -> bool {
    let Some(b) = b else { return false };
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => lexical_path_eq(a, b),
    }
}

/// Lexical directory equality: normalize backslashes to `/`, strip trailing
/// separators, compare case-insensitively on Windows. Fallback only.
pub(crate) fn lexical_path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
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
