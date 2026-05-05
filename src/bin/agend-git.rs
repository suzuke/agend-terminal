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
//! Platform: Unix only (macOS/Linux). Windows compiles a no-op stub.

#[cfg(not(unix))]
fn main() {
    eprintln!("agend-git: unix-only platform (macOS/Linux)");
    std::process::exit(1);
}

#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[derive(Default)]
struct Binding {
    task_id: Option<String>,
    branch: Option<String>,
    worktree: Option<String>,
}

#[cfg(unix)]
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
    Binding {
        task_id: v["task_id"].as_str().map(String::from),
        branch: v["branch"].as_str().map(String::from),
        worktree: v["worktree"].as_str().map(String::from),
    }
}

#[cfg(unix)]
fn is_bound(binding: &Binding) -> bool {
    binding.task_id.is_some()
}

// ── Classification ──────────────────────────────────────────────────────

#[cfg(unix)]
enum Action {
    Passthrough,
    ChdirPass(String),
    Deny(String),
}

#[cfg(unix)]
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

#[cfg(unix)]
fn exec_real_git(args: &[String], chdir: Option<&str>) -> ! {
    let git = resolve_real_git();
    let mut cmd = Command::new(&git);
    if let Some(dir) = chdir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let err = cmd.exec(); // replaces process on Unix
    eprintln!("agend-git: exec failed: {err}");
    std::process::exit(127);
}

#[cfg(unix)]
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
    let search: String = env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|p| !p.is_empty() && *p != agend_bin)
        .collect::<Vec<_>>()
        .join(":");
    which::which_in("git", Some(&search), ".")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/git".to_string())
}

// ── Error + Telemetry ───────────────────────────────────────────────────

#[cfg(unix)]
fn emit_deny_error(subcmd: &str, reason: &str, agent: &str) {
    eprintln!("agend-git: ERROR git {subcmd} denied");
    eprintln!("           agent={agent}, reason: {reason}");
    eprintln!("           HINT: use the task board to get a worktree assignment, or set AGEND_GIT_BYPASS=1 for emergency override");
}

#[cfg(unix)]
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
