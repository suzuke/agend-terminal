//! Real-git handoff: `resolve_real_git`, `exec_real_git`, and the
//! conflict-guidance wrapper emitted around conflict-capable commands.

use std::env;
use std::process::Command;

use super::*;

// ── Exec ────────────────────────────────────────────────────────────────

pub(crate) fn exec_with_conflict_guidance(
    args: &[String],
    worktree: &str,
    home: &str,
    agent: &str,
    subcmd: &str,
) -> ! {
    let git = resolve_real_git();
    // #1504 L3: propagate incremented depth (rebase/merge/pull/cherry-pick reach
    // here and also spawn real git — same recursion vector as exec_real_git).
    let status = Command::new(&git)
        .env("AGENTIC_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string())
        .arg("-C")
        .arg(worktree)
        .args(args)
        .status();
    match status {
        Ok(st) => {
            if !st.success() && has_unmerged_files(&git, worktree) {
                emit_conflict_guidance(home, agent, subcmd);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = st.signal() {
                    std::process::exit(128 + sig);
                }
            }
            std::process::exit(st.code().unwrap_or(1))
        }
        Err(e) => {
            eprintln!("agentic-git: exec failed: {e}");
            std::process::exit(127);
        }
    }
}

pub(crate) fn exec_real_git(args: &[String], chdir: Option<&str>) -> ! {
    let git = resolve_real_git();
    let mut cmd = Command::new(&git);
    // #1504 L3: propagate the incremented depth so a self-resolution loop trips
    // the recursion guard at the next entry instead of spawning unbounded.
    cmd.env("AGENTIC_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string());
    if let Some(dir) = chdir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);

    // Unix: exec() replaces process. Windows: status() + exit(code).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("agentic-git: exec failed: {err}");
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("agentic-git: exec failed: {e}");
                std::process::exit(127);
            }
        }
    }
}

pub(crate) fn resolve_real_git() -> String {
    // Priority 1: AGENTIC_GIT_REAL_GIT env (injected by daemon at spawn).
    // Review-1 hardening: REJECT the env value when it resolves to THIS
    // binary. A standalone user who prepends `<home>/bin` to PATH *before*
    // running `command -v git` captures the shim itself here; trusting it
    // verbatim guarantees a self-exec loop that only dies at the #1504
    // depth cap (exit 70). Detecting the foot-gun at resolution time lets
    // the Priority-2 self-excluding PATH search below do its job instead.
    if let Ok(path) = env_compat("AGENTIC_GIT_REAL_GIT") {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            let points_at_self = match (
                std::fs::canonicalize(&path),
                std::env::current_exe().and_then(std::fs::canonicalize),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if !points_at_self {
                return path;
            }
        }
    }
    // Priority 2: which excluding $AGENTIC_GIT_HOME/bin/ (the shim dir).
    // #1504 L2: exclude via canonicalized Path comparison, not a string compare.
    // `format!("{h}/bin")` (forward slash) never matched a Windows PATH entry
    // (backslash / case / trailing-slash), so the shim failed to exclude itself
    // and `which_in` resolved git back to THIS binary → recursive-spawn storm.
    // With L1 fixed the daemon injects AGENTIC_GIT_REAL_GIT and Priority 1 above
    // short-circuits, so this fallback rarely runs — but it must be correct when
    // it does. `split_paths` also gives the right separator + drive-colon handling.
    let agend_bin: Option<PathBuf> =
        env_compat_os("AGENTIC_GIT_HOME").map(|h| PathBuf::from(h).join("bin"));
    let path_os = env::var_os("PATH").unwrap_or_default();
    let search_paths: Vec<PathBuf> = std::env::split_paths(&path_os)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !same_dir(p, agend_bin.as_deref()))
        .collect();
    let search = std::env::join_paths(&search_paths).unwrap_or_default();
    which::which_in("git", Some(&search), ".")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/git".to_string())
}

pub(crate) fn is_conflict_capable(subcmd: &str) -> bool {
    matches!(subcmd, "rebase" | "merge" | "pull" | "cherry-pick")
}

pub(crate) fn has_unmerged_files(git: &str, worktree: &str) -> bool {
    Command::new(git)
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// #2379 ②: a merge conflict is a WARN, not a deny — the op ran, git left conflict
/// markers, and the agent RESOLVES + continues (it must NOT abandon/redo). Previously
/// this guidance was stderr-only → invisible to fleet observers; mirror it into
/// `fleet_events.jsonl` as a `git_conflict` event (disposition=warn via `disposition_for`)
/// for parity with deny events, then print the unchanged stderr guidance.
pub(crate) fn emit_conflict_guidance(home: &str, agent: &str, subcmd: &str) {
    write_git_event_typed(
        home,
        agent,
        subcmd,
        "git_conflict",
        None,
        Some("merge conflict — resolve the markers and continue (do not abandon/redo)"),
    );
    eprint!("{}", format_conflict_guidance());
}

pub(crate) fn format_conflict_guidance() -> &'static str {
    "\n\u{26a0} Merge conflict detected. To resolve:\n\
     1. Edit the conflicted files listed above \u{2014} resolve all <<<<<<< / ======= / >>>>>>> markers\n\
     2. git add <resolved-files>\n\
     3. git rebase --continue (or git merge --continue / git cherry-pick --continue)\n\
     Do NOT abandon and redo from scratch unless the conflict involves complex semantic changes you cannot resolve.\n"
}
