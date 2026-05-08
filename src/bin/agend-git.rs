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

    match classify(subcommand, &args, &binding, parent_is_gh) {
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

fn classify(subcmd: &str, args: &[String], binding: &Binding, parent_is_gh: bool) -> Action {
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
            if !bound {
                return Action::Deny("unbound — no active task assignment".into());
            }
            // Check for cross-branch attempt.
            let target_branch = args.get(1).map(|s| s.as_str()).unwrap_or("");
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
        let action_gh = classify("switch", &["switch".into(), "main".into()], &binding, true);
        assert!(matches!(action_gh, Action::SilentExempt { .. }));
        // interactive path → deny
        let action_interactive =
            classify("switch", &["switch".into(), "main".into()], &binding, false);
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
}
