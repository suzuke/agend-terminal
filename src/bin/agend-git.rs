//! agend-git — transparent git shim for fleet-managed worktrees.
//!
//! Intercepts git commands via PATH shadowing. Reads binding.json to
//! determine the active worktree, then either:
//! - passthrough (unbound read-only commands)
//! - chdir + pass (bound commands routed to worktree)
//! - silent-exempt (gh post-merge cleanup checkout — Sprint 57 Wave 2 Track D)
//! - deny (forbidden operations with LLM-friendly error)
//!
//! Bypass: AGEND_GIT_BYPASS=1 | AGEND_GIT_BYPASS_AGENT=<name> | AGEND_GIT_BYPASS_UNTIL=<epoch>
//!
//! Cross-platform: Unix uses exec() for process replacement; Windows uses
//! status() + exit(code) for equivalent behavior.

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // Bypass checks (3-layer per §7).
    if should_bypass() {
        exec_real_git(&args, None);
    }

    let agent = env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let home = env::var("AGEND_HOME").unwrap_or_default();

    if agent.is_empty() || home.is_empty() {
        exec_real_git(&args, None);
    }

    // Read binding.
    let binding = read_binding(&home, &agent);
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");

    // Sprint 57 Wave 2 Track D: resolve parent-process-is-gh signal once.
    // Used by `classify` to recognize gh-driven post-merge cleanup
    // checkouts and silently exempt them from the E4.5 cross-branch
    // fence. See `invocation_is_gh_post_merge` for the rationale.
    let parent_is_gh = invocation_is_gh_post_merge();

    // #778 Option 3 + #852 residual PR-A: resolve cwd-is-canonical-
    // rooted once, pass into classify as a pure bool so the leniency
    // rule is unit-testable without a real filesystem fixture. Detects
    // BOTH daemon-provisioned worktrees AND the canonical source repo
    // (post-#852-residual; pre-fix only matched worktrees).
    let canonical_cwd = cwd_is_canonical_rooted();

    // #852: resolve agent-vs-operator caller identity once. Agents are
    // daemon-spawned subprocesses (AGEND_INSTANCE_NAME set); operators
    // are interactive shells with no such env. Used by classify's
    // canonical-checkout gate to prevent reviewer-style PR-inspection
    // from polluting canonical worktrees with stale refs.
    let is_agent_caller = env::var_os("AGEND_INSTANCE_NAME").is_some();

    match classify(
        subcommand,
        &args,
        &binding,
        parent_is_gh,
        canonical_cwd,
        is_agent_caller,
    ) {
        Action::Passthrough => exec_real_git(&args, None),
        Action::ChdirPass(worktree) => exec_real_git(&args, Some(&worktree)),
        Action::SilentExempt {
            target_branch,
            reason,
        } => {
            // Sprint 57 Wave 2 Track D: gh-driven post-merge cleanup
            // checkout. Already-merged PR + already-deleted remote
            // branch — the local checkout is purely cosmetic from
            // gh's perspective. Skip the actual git invocation
            // (preserves E4.5: no real checkout to main happens),
            // log the exemption for security review, exit 0 so gh
            // continues its post-merge cleanup quietly.
            write_git_event_typed(
                &home,
                &agent,
                subcommand,
                "post_merge_cleanup_exempt",
                Some(&target_branch),
                Some(&reason),
            );
            std::process::exit(0);
        }
        Action::Deny(reason) => {
            emit_deny_error(subcommand, &reason, &agent);
            write_git_event_typed(&home, &agent, subcommand, "deny", None, Some(&reason));
            std::process::exit(1);
        }
    }
}

// ── Bypass ──────────────────────────────────────────────────────────────

fn should_bypass() -> bool {
    if env::var("AGEND_GIT_BYPASS").is_ok() {
        return true;
    }
    if let Ok(agent_bypass) = env::var("AGEND_GIT_BYPASS_AGENT") {
        if let Ok(current) = env::var("AGEND_INSTANCE_NAME") {
            if agent_bypass == current {
                return true;
            }
        }
    }
    if let Ok(until_str) = env::var("AGEND_GIT_BYPASS_UNTIL") {
        if let Ok(until) = until_str.parse::<u64>() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now < until {
                return true;
            }
        }
    }
    false
}

// ── Binding ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Binding {
    task_id: Option<String>,
    branch: Option<String>,
    worktree: Option<String>,
}

fn read_binding(home: &str, agent: &str) -> Binding {
    let path = PathBuf::from(home)
        .join("runtime")
        .join(agent)
        .join("binding.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Binding::default(),
    };
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Binding::default(), // parse failure = unbound (fail-safe)
    };
    let b = Binding {
        task_id: v["task_id"].as_str().map(String::from),
        branch: v["branch"].as_str().map(String::from),
        worktree: v["worktree"].as_str().map(String::from),
    };
    // P0-1.6: orphan binding defense.
    // If binding points to a worktree path that no longer exists (e.g. operator
    // ran `git worktree remove` after the daemon wrote the binding, or a stale
    // binding survived a daemon restart), treat the agent as unbound rather
    // than letting chdir fatal at exec time. Daemon-side reconcile will
    // eventually clean the stale file; this guard is only a fail-safe.
    if let Some(ref wt) = b.worktree {
        if !std::path::Path::new(wt).exists() {
            return Binding::default();
        }
    }
    b
}

fn is_bound(binding: &Binding) -> bool {
    binding.task_id.is_some()
}

// ── Classification ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Passthrough,
    ChdirPass(String),
    /// Sprint 57 Wave 2 Track D: gh post-merge cleanup checkout
    /// recognized — exit 0 without invoking real git. E4.5 protection
    /// is preserved (no actual checkout to main happens) and the
    /// `gh pr merge --delete-branch` cleanup proceeds silently.
    SilentExempt {
        target_branch: String,
        reason: String,
    },
    Deny(String),
}

/// Local mirror of `agent_ops::is_protected_ref`. The wrapper binary
/// is intentionally self-contained (no `crate::*` imports) so it
/// builds standalone without the full library surface. Sprint 57
/// Wave 2 Track B introduced the lib-side helper; the literal here
/// MUST stay in sync.
fn is_protected_ref(branch: &str) -> bool {
    matches!(branch, "main" | "master")
}

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
fn cwd_is_canonical_rooted() -> bool {
    let cwd = match env::current_dir() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let dot_git = cwd.join(".git");
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

fn classify(
    subcmd: &str,
    args: &[String],
    binding: &Binding,
    parent_is_gh: bool,
    canonical_cwd: bool,
    is_agent_caller: bool,
) -> Action {
    let bound = is_bound(binding);

    match subcmd {
        // Read-only commands: passthrough when unbound, chdir when bound.
        "status" | "log" | "diff" | "show" | "blame" | "ls-files" | "ls-tree" | "rev-parse"
        | "fetch" | "remote" | "branch" | "tag" | "describe" | "shortlog" | "reflog" => {
            if bound {
                if let Some(ref wt) = binding.worktree {
                    return Action::ChdirPass(wt.clone());
                }
            }
            Action::Passthrough
        }
        // Config/help: always passthrough.
        "config" | "help" | "version" | "init" | "clone" => Action::Passthrough,
        // Mutating commands: deny when unbound.
        "commit" | "push" | "pull" | "reset" | "revert" | "cherry-pick" | "stash" | "merge"
        | "rebase" | "am" | "add" | "rm" | "mv" => {
            if !bound {
                return Action::Deny("unbound — no active task assignment".into());
            }
            if let Some(ref wt) = binding.worktree {
                Action::ChdirPass(wt.clone())
            } else {
                Action::Deny("bound but no worktree path".into())
            }
        }
        // Checkout/switch: deny unbound, deny cross-branch.
        "checkout" | "switch" => {
            let target_branch = args.get(1).map(|s| s.as_str()).unwrap_or("");
            if !bound {
                // #852: agent callers must NOT use the #778 Option-3
                // leniency below. The leniency was designed for the
                // operator-typed validation-canary flow (operator runs
                // `repo action=checkout` to provision a worktree in
                // detached-HEAD, then `git switch <branch>` to land on
                // the branch; that follow-up needs to pass without a
                // BYPASS). But the gate wasn't agent-aware, so
                // reviewer agents whose binding lookup failed for the
                // canonical-rooted cwd fell through to the same
                // leniency — and the resulting `git checkout <sha>` /
                // `git checkout -b tmp_review` calls polluted
                // canonical's branch list with stale `pr*_head` /
                // `tmp*` / `review/*` refs. Operator surfaced the
                // recurrence on PR #805 morning + PR #850 afternoon.
                // Fix: route agents to either `repo action=checkout
                // bind=true` (gives them a properly-bound worktree)
                // or `gh pr diff/view` (read-only). Operator path
                // unchanged.
                if is_agent_caller && canonical_cwd {
                    return Action::Deny(
                        "agent callers must not checkout in canonical \
                         (use `repo action=checkout` for PR inspection or \
                         `gh pr diff/view` for read-only). #852."
                            .into(),
                    );
                }
                // #778 Option 3: shim leniency for canonical-rooted
                // unbound worktrees. When cwd is inside a worktree whose
                // `.git` pointer resolves to a source repo carrying a
                // `[remote "origin"]` config entry (i.e. a canonical
                // repo, not the orphan workspace-placeholder daemon
                // startup leaves), allow `git checkout`/`git switch
                // <branch>` as a Passthrough. Closes the chicken-and-egg
                // surfaced by validation canary 2026-05-14:
                // `repo action=checkout` provisions the worktree in
                // detached-HEAD but doesn't bind, so the natural
                // follow-up `git switch <branch>` would otherwise need
                // a BYPASS. Narrow by design — `target_branch` must be
                // a positional argument (not a flag) and the worktree
                // must be daemon-provisioned canonical-rooted, so the
                // surface is limited to navigation within an already-
                // materialized worktree.
                if !target_branch.is_empty() && !target_branch.starts_with('-') && canonical_cwd {
                    return Action::Passthrough;
                }
                return Action::Deny("unbound — no active task assignment".into());
            }
            // Check for cross-branch attempt.
            if let Some(ref assigned) = binding.branch {
                if !target_branch.is_empty()
                    && target_branch != assigned
                    && !target_branch.starts_with('-')
                {
                    // Sprint 57 Wave 2 Track D: gh post-merge cleanup
                    // exemption. Trigger requires ALL of:
                    //   - target is a protected ref (main / master)
                    //   - parent process is `gh` (signal that this
                    //     invocation is from `gh pr merge --delete-branch`
                    //     post-merge local-state cleanup)
                    //   - we're in the agent-invoked path (AGEND_INSTANCE_NAME
                    //     was set; bound binding is the consequence of that)
                    // Heuristic robustness: a non-gh parent (interactive
                    // shell, script, IDE) reaches the cross-branch deny
                    // unchanged, preserving E4.5 protection for the
                    // operator-typed case the rule was originally built
                    // for.
                    if is_protected_ref(target_branch) && parent_is_gh {
                        return Action::SilentExempt {
                            target_branch: target_branch.to_string(),
                            reason: format!(
                                "gh post-merge cleanup checkout to '{target_branch}' \
                                 from binding-branch '{assigned}' \
                                 (parent process detected as `gh`); \
                                 PR merge already succeeded — silent exit avoids \
                                 noisy false-positive deny on the operator's terminal"
                            ),
                        };
                    }
                    return Action::Deny(format!(
                        "cross-branch — assigned to '{assigned}', cannot switch to '{target_branch}'"
                    ));
                }
            }
            if let Some(ref wt) = binding.worktree {
                Action::ChdirPass(wt.clone())
            } else {
                Action::Deny("bound but no worktree path".into())
            }
        }
        // Worktree management: always deny (fleet-managed).
        "worktree" => Action::Deny("fleet-managed — use agend-terminal worktree tools".into()),
        // Default: passthrough when unbound, chdir when bound.
        _ => {
            if bound {
                if let Some(ref wt) = binding.worktree {
                    return Action::ChdirPass(wt.clone());
                }
            }
            Action::Passthrough
        }
    }
}

// ── Parent-process detection (gh post-merge cleanup heuristic) ──────────

/// Sprint 57 Wave 2 Track D: detect that this `agend-git` invocation
/// is a child of `gh`. Returns `true` only when AGEND_INSTANCE_NAME is
/// set (i.e. we're inside the agent-invoked path the cross-branch
/// fence guards) AND the parent process name is `gh`. Conservative
/// by design: any platform-specific lookup failure returns `false`,
/// letting the fence fire as it would have pre-Track-D rather than
/// silently weakening E4.5.
fn invocation_is_gh_post_merge() -> bool {
    // Operator-shell invocations don't have AGEND_INSTANCE_NAME set;
    // those already hit the early passthrough at the top of `main()`,
    // so the cross-branch fence never fires for them. Restricting the
    // exemption to AGEND_INSTANCE_NAME-set invocations keeps the
    // surface tight.
    if env::var("AGEND_INSTANCE_NAME")
        .ok()
        .is_none_or(|s| s.is_empty())
    {
        return false;
    }
    parent_process_name()
        .map(|n| process_basename_is_gh(&n))
        .unwrap_or(false)
}

/// Pure helper for testability — accepts any process-name string and
/// returns whether it looks like the `gh` binary (basename match).
/// Handles common platform formats: "gh", "/usr/local/bin/gh",
/// "C:\\Program Files\\GitHub CLI\\gh.exe".
fn process_basename_is_gh(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Strip any trailing newline/whitespace and split off the basename.
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    // Match either `gh` or `gh.exe` (case-insensitive on Windows
    // semantics, but case-sensitive paths are universal — the gh CLI
    // ships its binary lower-case).
    last == "gh" || last.eq_ignore_ascii_case("gh.exe")
}

#[cfg(target_os = "linux")]
fn parent_process_name() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    let path = format!("/proc/{ppid}/comm");
    std::fs::read_to_string(&path).ok().map(|s| {
        s.trim_end_matches(['\n', '\r', '\0', ' '])
            .trim()
            .to_string()
    })
}

#[cfg(target_os = "macos")]
fn parent_process_name() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    let output = std::process::Command::new("ps")
        .args(["-p", &ppid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "windows")]
fn parent_process_name() -> Option<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let parent_pid = sys.process(pid)?.parent()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[parent_pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(parent_pid)
        .map(|p| p.name().to_string_lossy().to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn parent_process_name() -> Option<String> {
    None
}

// ── Exec ────────────────────────────────────────────────────────────────

fn exec_real_git(args: &[String], chdir: Option<&str>) -> ! {
    let git = resolve_real_git();
    let mut cmd = Command::new(&git);
    if let Some(dir) = chdir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);

    // Unix: exec() replaces process. Windows: status() + exit(code).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("agend-git: exec failed: {err}");
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("agend-git: exec failed: {e}");
                std::process::exit(127);
            }
        }
    }
}

fn resolve_real_git() -> String {
    // Priority 1: AGEND_REAL_GIT env (injected by daemon at spawn).
    if let Ok(path) = env::var("AGEND_REAL_GIT") {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return path;
        }
    }
    // Priority 2: which excluding $AGEND_HOME/bin/.
    let agend_bin = env::var("AGEND_HOME")
        .map(|h| format!("{h}/bin"))
        .unwrap_or_default();
    let path_sep = if cfg!(windows) { ';' } else { ':' };
    let search: String = env::var("PATH")
        .unwrap_or_default()
        .split(path_sep)
        .filter(|p| !p.is_empty() && *p != agend_bin)
        .collect::<Vec<_>>()
        .join(&path_sep.to_string());
    which::which_in("git", Some(&search), ".")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/git".to_string())
}

// ── Error + Telemetry ───────────────────────────────────────────────────

fn emit_deny_error(subcmd: &str, reason: &str, agent: &str) {
    for line in format_deny_error(subcmd, reason, agent) {
        eprintln!("{line}");
    }
}

/// Sprint 54 P2-4: build the deny-error block as a `Vec<String>` so the
/// 3-form bypass hint can be unit-tested for env-var-name presence
/// without capturing stderr. `emit_deny_error` is a thin wrapper that
/// `eprintln!`s each line. Per `should_bypass` (above), three bypass
/// forms exist; the hint enumerates all of them so operators don't
/// have to grep the source to discover the agent-specific or
/// time-limited variants.
fn format_deny_error(subcmd: &str, reason: &str, agent: &str) -> Vec<String> {
    vec![
        format!("agend-git: ERROR git {subcmd} denied"),
        format!("           agent={agent}, reason: {reason}"),
        "           HINT: use the task board for a worktree assignment, or bypass with one of:".to_string(),
        "             AGEND_GIT_BYPASS=1               one-shot emergency override".to_string(),
        "             AGEND_GIT_BYPASS_AGENT=<name>    agent-specific exemption (matches AGEND_INSTANCE_NAME)".to_string(),
        "             AGEND_GIT_BYPASS_UNTIL=<epoch>   time-limited exemption (Unix seconds, not ISO)".to_string(),
    ]
}

/// Sprint 57 Wave 2 Track D: structured audit-event writer with an
/// explicit event-type discriminator. Replaces the previous untyped
/// `write_git_event` that hardcoded `event="deny"`. `event_type` is
/// the new `kind`-style discriminator (`"deny"` or
/// `"post_merge_cleanup_exempt"`); `target_branch` carries the
/// resolved checkout target when relevant for the exemption case;
/// `detail` mirrors the human-readable reason string.
fn write_git_event_typed(
    home: &str,
    agent: &str,
    subcmd: &str,
    event_type: &str,
    target_branch: Option<&str>,
    detail: Option<&str>,
) {
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    let event = serde_json::json!({
        "kind": "git_event",
        "event": event_type,
        "agent": agent,
        "subcommand": subcmd,
        "target_branch": target_branch,
        "reason": detail,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    // Best-effort append (don't block on failure).
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", event);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn bound_binding(branch: &str, worktree: &str) -> Binding {
        Binding {
            task_id: Some("T-test".into()),
            branch: Some(branch.into()),
            worktree: Some(worktree.into()),
        }
    }

    #[test]
    fn deny_hint_lists_all_three_bypass_forms() {
        let lines = format_deny_error("commit", "unbound", "dev");
        let joined = lines.join("\n");
        for var in [
            "AGEND_GIT_BYPASS=1",
            "AGEND_GIT_BYPASS_AGENT=",
            "AGEND_GIT_BYPASS_UNTIL=",
        ] {
            assert!(
                joined.contains(var),
                "deny hint must list {var}, got:\n{joined}"
            );
        }
        assert!(
            joined.contains("epoch") && joined.contains("Unix seconds"),
            "AGEND_GIT_BYPASS_UNTIL hint must clarify epoch wording (not ISO), got:\n{joined}"
        );
    }

    // ----- Sprint 57 Wave 2 Track D — gh post-merge exemption -----

    #[test]
    fn gh_post_merge_checkout_exempted_from_e45_fence() {
        // Happy path: agent is bound to a feat branch, gh just merged
        // it + deleted remote, now runs `git checkout main` to clean
        // up local state. parent=gh signal fires → SilentExempt.
        let binding = bound_binding("sprint57-track-x", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "main".into()],
            &binding,
            true, // parent_is_gh = true
            false,
            false, // is_agent_caller — operator default; the gh-exemption
                   // is independent of #852's agent-vs-operator gate
        );
        match action {
            Action::SilentExempt {
                target_branch,
                reason,
            } => {
                assert_eq!(target_branch, "main");
                assert!(
                    reason.contains("gh post-merge"),
                    "reason must label the exemption: {reason}"
                );
            }
            other => panic!("expected SilentExempt for gh post-merge cleanup, got {other:?}"),
        }
    }

    #[test]
    fn gh_post_merge_exemption_also_covers_master() {
        // master is part of the protected set per `is_protected_ref`;
        // legacy repos using `master` as default branch must also
        // trigger the exemption.
        let binding = bound_binding("sprint57-track-y", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "master".into()],
            &binding,
            true,
            false,
            false, // is_agent_caller — operator default
        );
        assert!(
            matches!(action, Action::SilentExempt { .. }),
            "master target must also be exempted, got {action:?}"
        );
    }

    #[test]
    fn interactive_checkout_to_main_still_blocked() {
        // Regression-proof of E4.5 normal protection: when parent is
        // NOT gh (interactive shell, script, IDE), the cross-branch
        // fence must still fire. Without this guarantee Track D
        // would silently weaken the rule.
        let binding = bound_binding("sprint57-track-z", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "main".into()],
            &binding,
            false, // parent_is_gh = false (interactive shell)
            false,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("cross-branch"),
                    "interactive case must still trip the cross-branch fence: {reason}"
                );
                assert!(
                    reason.contains("'main'"),
                    "deny message must mention target branch: {reason}"
                );
            }
            other => panic!("interactive checkout to main MUST be denied, got {other:?}"),
        }
    }

    #[test]
    fn switch_subcommand_also_routes_through_gate() {
        // `git switch main` is the modern equivalent of `git checkout
        // main`; the gate must apply to both subcommands so the
        // exemption + the normal block both work via either spelling.
        let binding = bound_binding("sprint57-track-q", "/tmp/.worktrees/dev");
        // gh path → exempt
        let action_gh = classify(
            "switch",
            &["switch".into(), "main".into()],
            &binding,
            true,
            false,
            false, // is_agent_caller — operator default
        );
        assert!(matches!(action_gh, Action::SilentExempt { .. }));
        // interactive path → deny
        let action_interactive = classify(
            "switch",
            &["switch".into(), "main".into()],
            &binding,
            false,
            false,
            false, // is_agent_caller — operator default
        );
        match action_interactive {
            Action::Deny(_) => {}
            other => panic!("interactive `switch main` must deny, got {other:?}"),
        }
    }

    #[test]
    fn cross_branch_to_non_protected_target_never_exempted() {
        // Heuristic correctness: even with parent_is_gh=true, a
        // checkout to a NON-protected branch must still be denied.
        // The exemption is narrow by design — protected refs only.
        // gh in normal operation never checks out feature branches
        // post-merge, so this case represents a heuristic false-
        // positive boundary we explicitly guard.
        let binding = bound_binding("sprint57-track-r", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "feat-other".into()],
            &binding,
            true, // parent_is_gh — but target isn't protected.
            false,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("cross-branch"),
                    "non-protected cross-branch must deny even with parent=gh: {reason}"
                );
            }
            other => panic!(
                "non-protected cross-branch with parent=gh must deny (NOT exempt), got {other:?}"
            ),
        }
    }

    #[test]
    fn gh_invocation_detection_robust_against_simulated_external_invocation() {
        // The detection helper must reject `gh`-lookalike basenames
        // that aren't the canonical CLI binary. This pins the
        // basename matcher: only the literal `gh` (or `gh.exe`)
        // qualifies — common false-positives like `github`,
        // `gh-cli-helper`, or empty strings must NOT.
        assert!(process_basename_is_gh("gh"));
        assert!(process_basename_is_gh("/usr/local/bin/gh"));
        assert!(process_basename_is_gh("/opt/homebrew/bin/gh"));
        assert!(process_basename_is_gh(
            "C:\\Program Files\\GitHub CLI\\gh.exe"
        ));
        assert!(process_basename_is_gh("gh.exe"));

        // Negative cases — must NOT fire the heuristic.
        assert!(!process_basename_is_gh(""));
        assert!(!process_basename_is_gh("github"));
        assert!(!process_basename_is_gh("/usr/bin/github"));
        assert!(!process_basename_is_gh("gh-cli-helper"));
        assert!(!process_basename_is_gh("not-gh"));
        assert!(!process_basename_is_gh("/path/to/gh.sh")); // shell wrapper
        assert!(!process_basename_is_gh("agh")); // adjacent letters
    }

    #[test]
    fn audit_event_logged_when_exemption_fires() {
        // Round-trip: classify produces SilentExempt → main() writes a
        // structured `post_merge_cleanup_exempt` event. We can't run
        // main() in a unit test (it calls std::process::exit), but
        // we can call the underlying `write_git_event_typed` writer
        // directly and assert the on-disk shape, which is what main
        // would emit.
        let home = std::env::temp_dir().join(format!(
            "agend-git-d-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();

        write_git_event_typed(
            home.to_str().unwrap(),
            "dev",
            "checkout",
            "post_merge_cleanup_exempt",
            Some("main"),
            Some("gh post-merge cleanup checkout — test fixture"),
        );

        let events_path = home.join("fleet_events.jsonl");
        assert!(events_path.exists(), "audit event file must be created");

        let content = std::fs::read_to_string(&events_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["kind"], "git_event");
        assert_eq!(v["event"], "post_merge_cleanup_exempt");
        assert_eq!(v["agent"], "dev");
        assert_eq!(v["subcommand"], "checkout");
        assert_eq!(v["target_branch"], "main");
        assert!(
            v["reason"]
                .as_str()
                .map(|s| s.contains("post-merge"))
                .unwrap_or(false),
            "reason must record the exemption rationale"
        );
        assert!(
            v["timestamp"].as_str().is_some(),
            "timestamp must be RFC3339 string"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deny_event_still_uses_typed_writer() {
        // Defensive bonus pin: the legacy `event="deny"` shape must
        // continue to work via the new `write_git_event_typed`
        // function. Previously the wrapper had a separate
        // `write_git_event` for deny-only; consolidating to a typed
        // writer must not change the on-disk shape for the deny
        // event-type so downstream parsers keep working.
        let home = std::env::temp_dir().join(format!(
            "agend-git-d-deny-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();

        write_git_event_typed(
            home.to_str().unwrap(),
            "dev",
            "checkout",
            "deny",
            None,
            Some("cross-branch — assigned to 'feat-x', cannot switch to 'main'"),
        );

        let events_path = home.join("fleet_events.jsonl");
        let content = std::fs::read_to_string(&events_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["event"], "deny");
        assert_eq!(v["target_branch"], serde_json::Value::Null);
        assert!(
            v["reason"]
                .as_str()
                .map(|s| s.contains("cross-branch"))
                .unwrap_or(false),
            "deny reason must round-trip"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- #778 Option 3 — canonical-worktree leniency for unbound -----

    #[test]
    fn p778_unbound_canonical_worktree_checkout_branch_passes_through() {
        // Empirical regression-proof anchor for #778 Option 3:
        // commenting out the `if !target_branch.is_empty() && ...
        // canonical_cwd { Action::Passthrough }` block makes this
        // FAIL with Action::Deny.
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &Binding::default(), // unbound
            false,               // parent_is_gh = no
            true,                // canonical_cwd = yes
            false,               // is_agent_caller — operator default; the
                                 // #778 leniency must still fire for the
                                 // operator-driven validation-canary flow
        );
        assert!(
            matches!(action, Action::Passthrough),
            "unbound + canonical worktree + positional branch must Passthrough, got {action:?}"
        );
    }

    #[test]
    fn p778_unbound_canonical_switch_subcommand_also_passes() {
        // `git switch` is the modern equivalent and must benefit from
        // the same leniency — otherwise the rule is partial and the
        // validation-canary workflow stays broken on the recommended
        // `switch` path.
        let action = classify(
            "switch",
            &["switch".into(), "feat/p778".into()],
            &Binding::default(),
            false,
            true,
            false, // is_agent_caller — operator default
        );
        assert!(
            matches!(action, Action::Passthrough),
            "switch must also benefit from the leniency, got {action:?}"
        );
    }

    #[test]
    fn p778_unbound_non_canonical_worktree_still_denied() {
        // Negative: when cwd is not a canonical worktree (placeholder
        // repo with no origin, or no worktree at all), the original
        // unbound deny must still fire — this is the security
        // guarantee that keeps the leniency narrow.
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &Binding::default(),
            false,
            false, // canonical_cwd = no
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(
                reason.contains("unbound"),
                "non-canonical cwd must keep the unbound deny: {reason}"
            ),
            other => panic!("non-canonical unbound must deny, got {other:?}"),
        }
    }

    #[test]
    fn p778_unbound_canonical_flag_arg_still_denied() {
        // Heuristic safety: when the next arg is a flag (`-b
        // newbranch`, `-B foo`, `--orphan`) the leniency must NOT
        // fire — those create branches or detach in ways that aren't
        // "just navigation". Keep the deny for the unbound case so
        // we don't accidentally widen the surface.
        let action = classify(
            "checkout",
            &["checkout".into(), "-b".into(), "evil".into()],
            &Binding::default(),
            false,
            true,  // canonical_cwd = yes, but arg is a flag
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(
                reason.contains("unbound"),
                "flag arg in unbound canonical must deny: {reason}"
            ),
            other => panic!("flag arg leniency leak: {other:?}"),
        }
    }

    #[test]
    fn p778_unbound_canonical_no_branch_arg_still_denied() {
        // `git checkout` with no positional branch (just to inspect
        // status) shouldn't even hit the leniency block — keep the
        // existing unbound deny for the no-target case.
        let action = classify(
            "checkout",
            &["checkout".into()],
            &Binding::default(),
            false,
            true,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(reason.contains("unbound"), "got {reason}"),
            other => panic!("no-arg unbound must deny, got {other:?}"),
        }
    }

    #[test]
    fn p778_bound_path_unchanged_when_canonical_cwd_true() {
        // Regression-proof of the bound path: canonical_cwd must NOT
        // alter behavior when the agent is bound. The existing
        // cross-branch check + ChdirPass dispatch are the source of
        // truth; the leniency only opens when bound=false.
        let binding = bound_binding("feat/p778", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &binding,
            false,
            true,  // canonical_cwd — should NOT route through leniency
            false, // is_agent_caller — operator default
        );
        match action {
            Action::ChdirPass(ref wt) => assert_eq!(wt, "/tmp/.worktrees/dev"),
            other => panic!("bound same-branch must ChdirPass, got {other:?}"),
        }
    }

    // ----- #852 PR-B — agent caller + canonical cwd → Deny -----
    //
    // The pre-#852 `!bound + canonical_cwd + positional non-flag arg →
    // Passthrough` leniency was designed for the operator-typed
    // validation-canary flow (`repo action=checkout` provisions a
    // worktree in detached-HEAD; operator's natural `git switch
    // <branch>` follow-up needed to pass). It accidentally also
    // covered agent callers whose binding lookup failed for the
    // current cwd — reviewers especially, who inspect PRs via
    // canonical-rooted worktrees and end up creating `pr*_head` /
    // `tmp*` / `review/*` refs that pollute the canonical's branch
    // list. PR-B gates the leniency on agent-vs-operator identity:
    // operators keep the leniency, agents are routed to the
    // `repo action=checkout bind=true` MCP tool (which gives them a
    // properly-bound worktree) or `gh pr diff/view` (read-only).

    /// #852 PR-B core: when caller is an agent (AGEND_INSTANCE_NAME
    /// set) AND cwd is a canonical-rooted worktree, the leniency must
    /// NOT fire — checkout is denied with an actionable hint pointing
    /// to the supported alternatives.
    #[test]
    fn shim_denies_agent_checkout_in_canonical() {
        let action = classify(
            "checkout",
            &["checkout".into(), "abc1234".into()], // SHA — reviewer's
            // "let me see this
            // PR's tree" workflow
            &Binding::default(), // unbound (binding lookup failed for
            // canonical cwd)
            false, // parent_is_gh = no
            true,  // canonical_cwd = yes
            true,  // is_agent_caller = yes
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("agent"),
                    "deny reason must explicitly call out the agent-caller \
                     identity so reviewers see WHY their workflow is rejected: \
                     {reason}"
                );
                assert!(
                    reason.contains("repo action=checkout") || reason.contains("gh pr diff"),
                    "deny reason must surface the supported alternative \
                     (repo action=checkout MCP or gh pr diff): {reason}"
                );
                assert!(
                    reason.contains("#852"),
                    "deny reason should reference the issue for operator \
                     traceability: {reason}"
                );
            }
            other => panic!(
                "agent caller in canonical worktree must Deny, not {other:?} \
                 — that's the reviewer-pollution bug fix"
            ),
        }
    }

    /// #852 PR-B operator preservation: when caller is NOT an agent
    /// (operator's interactive shell, no AGEND_INSTANCE_NAME), the
    /// existing #778 leniency must continue to fire — the validation-
    /// canary flow must not regress.
    #[test]
    fn shim_allows_operator_checkout_in_canonical() {
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/canary".into()],
            &Binding::default(),
            false, // parent_is_gh = no
            true,  // canonical_cwd = yes
            false, // is_agent_caller = no (operator shell)
        );
        assert!(
            matches!(action, Action::Passthrough),
            "operator in canonical worktree must keep the #778 leniency, \
             got {action:?}"
        );
    }

    /// #852 PR-B narrowness check: when the agent IS a caller but cwd
    /// is NOT canonical (e.g. agent invoked git from a non-worktree
    /// path), the gate must NOT fire — only the canonical-pollution
    /// surface is targeted. Operator's `unbound + non-canonical →
    /// Deny` outcome is preserved (different code path).
    #[test]
    fn shim_agent_outside_canonical_unchanged() {
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/x".into()],
            &Binding::default(),
            false, // parent_is_gh = no
            false, // canonical_cwd = NO — gate must NOT fire
            true,  // is_agent_caller = yes
        );
        // Falls through to the existing `unbound — no active task
        // assignment` Deny (different from the new #852 Deny). The
        // pre-existing safety net stays intact.
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("unbound"),
                    "non-canonical agent path must keep the original \
                     unbound deny (not the new #852 agent-canonical deny): \
                     {reason}"
                );
                assert!(
                    !reason.contains("#852"),
                    "non-canonical agent path must NOT trigger the #852 \
                     gate (gate is narrow by design): {reason}"
                );
            }
            other => panic!(
                "non-canonical unbound must keep the pre-existing deny, \
                 got {other:?}"
            ),
        }
    }

    // ----- #852 residual PR-A — cwd_is_canonical_rooted detection -----
    //
    // Pre-#852-residual, the detection helper required `.git` to be a
    // FILE (worktree marker). This excluded canonical source repos
    // where `.git` is a DIRECTORY — reviewers `cd`'ing into source
    // sailed past the `is_agent_caller && canonical_cwd` deny because
    // canonical_cwd was always false for source. Operator's reflog
    // evidenced two such checkouts (21:46 + 22:24 today) AFTER
    // #858+#859 shipped. PR-A broadens the helper to cover BOTH
    // shapes.
    //
    // Tests use `std::env::set_current_dir` to position cwd inside
    // the synthetic fixtures. Mutex-serialized so parallel test
    // threads don't race the process-global cwd.

    fn with_cwd<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static CWD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = CWD_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::current_dir().expect("snapshot cwd");
        std::env::set_current_dir(dir).expect("set test cwd");
        let result = f();
        std::env::set_current_dir(&prior).expect("restore cwd");
        result
    }

    fn make_source_repo_with_origin(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agend-852-pr-a-source-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir source-base");
        let repo = base.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        // Synthetic config: matches the canonical-detection criterion
        // (contains `[remote "origin"]`).
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
        )
        .expect("write .git/config");
        repo
    }

    fn make_source_repo_without_origin(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agend-852-pr-a-no-origin-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir no-origin-base");
        let repo = base.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        // Orphan workspace-placeholder shape: `.git` directory but
        // no `[remote "origin"]`. Daemon startup creates these
        // before fleet config resolves; they must NOT trigger the
        // canonical-rooted gate.
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("write .git/config");
        repo
    }

    fn make_canonical_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        // Two-step: build a source repo with origin, then a synthetic
        // worktree pointing into it via the gitdir: marker. Mirrors
        // git's real worktree layout at <source>/.git/worktrees/<name>.
        let base =
            std::env::temp_dir().join(format!("agend-852-pr-a-wt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir wt-base");
        let source = base.join("source");
        let source_git = source.join(".git");
        let worktrees_dir = source_git.join("worktrees").join("agent-1");
        std::fs::create_dir_all(&worktrees_dir).expect("mkdir worktree entry");
        std::fs::write(
            source_git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
        )
        .expect("write source .git/config");
        // Worktree dir with `.git` FILE pointing at the worktrees entry.
        let wt = base.join("worktree-cwd");
        std::fs::create_dir_all(&wt).expect("mkdir worktree dir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", worktrees_dir.display()),
        )
        .expect("write worktree .git pointer");
        (wt, base)
    }

    fn cleanup_base(repo: &std::path::Path) {
        if let Some(base) = repo.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }

    /// #852 residual core: canonical source repo (`.git` directory +
    /// `[remote "origin"]`) must classify as canonical-rooted. This
    /// is the path that pre-#852-residual missed entirely.
    #[test]
    fn cwd_is_canonical_rooted_returns_true_for_source_repo_with_origin() {
        let repo = make_source_repo_with_origin("with-origin");
        let result = with_cwd(&repo, cwd_is_canonical_rooted);
        cleanup_base(&repo);
        assert!(
            result,
            "canonical source repo with `[remote \"origin\"]` must classify \
             as canonical-rooted (this is the #852 residual fix)"
        );
    }

    /// Defense against orphan workspace-placeholder repos: `.git`
    /// directory present but no remote configured. Daemon startup
    /// creates these before fleet config resolves; the canonical-
    /// rooted gate must NOT fire on them.
    #[test]
    fn cwd_is_canonical_rooted_returns_false_for_source_repo_without_origin() {
        let repo = make_source_repo_without_origin("no-origin");
        let result = with_cwd(&repo, cwd_is_canonical_rooted);
        cleanup_base(&repo);
        assert!(
            !result,
            "orphan workspace-placeholder (`.git` directory but no \
             `[remote \"origin\"]`) must NOT classify as canonical-rooted"
        );
    }

    /// Preserves the #858 contract: canonical-rooted worktree
    /// (`.git` FILE with `gitdir:` pointer to source carrying origin)
    /// still classifies. This is the pre-PR-A path; the broadening
    /// must NOT regress it.
    #[test]
    fn cwd_is_canonical_rooted_returns_true_for_canonical_worktree() {
        let (wt, _base) = make_canonical_worktree("worktree");
        let result = with_cwd(&wt, cwd_is_canonical_rooted);
        cleanup_base(&wt);
        assert!(
            result,
            "canonical worktree (`.git` FILE + gitdir: pointer to source \
             with origin) must still classify (pre-#852-residual contract)"
        );
    }
}
