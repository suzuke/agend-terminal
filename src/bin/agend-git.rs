//! agend-git — transparent git shim for fleet-managed worktrees.
//!
//! Intercepts git commands via PATH shadowing. Reads binding.json to
//! determine the active worktree, then either:
//! - passthrough (unbound read-only commands)
//! - chdir + pass (bound commands routed to worktree)
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

    match classify(subcommand, &args, &binding) {
        Action::Passthrough => exec_real_git(&args, None),
        Action::ChdirPass(worktree) => exec_real_git(&args, Some(&worktree)),
        Action::Deny(reason) => {
            emit_deny_error(subcommand, &reason, &agent);
            write_git_event(&home, &agent, subcommand, &reason);
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

enum Action {
    Passthrough,
    ChdirPass(String),
    Deny(String),
}

fn classify(subcmd: &str, args: &[String], binding: &Binding) -> Action {
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

fn write_git_event(home: &str, agent: &str, subcmd: &str, reason: &str) {
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    let event = serde_json::json!({
        "kind": "git_event",
        "event": "deny",
        "agent": agent,
        "subcommand": subcmd,
        "reason": reason,
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
mod tests {
    use super::format_deny_error;

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
}
