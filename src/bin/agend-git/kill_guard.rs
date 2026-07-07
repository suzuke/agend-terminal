//! agend kill-family shim — footgun-guard for the OS/shell command layer (#t-…777-1).
//!
//! Intercepts `pkill` / `killall` / `kill` via the SAME PATH-shadow the git shim uses
//! (`binding.rs::symlink_shim` links each name → the agend-git binary; `main` dispatches
//! on argv[0]). Motive: a broad unscoped pattern-kill from one agent's shell scans EVERY
//! process on the host by command line and can wipe sibling agents / the daemon. This is
//! the 2026-06-24 incident: `pkill -f "resume --last --dangerously-bypass"` (legit cleanup
//! intent, pattern over-matched) killed 3 live agents.
//!
//! Policy v1 (operator-approved — COMMAND-AUTHORITY-SPIKE.md §2.2, task 777-1 metadata):
//!  - `pkill` / `killall` (inherently pattern-scan) → DENY with a one-shot-fixable teaching
//!    message (`pgrep` to confirm the matches, then `kill` the explicit pids).
//!  - `kill`: explicit positive pids pass through — the sanctioned scoped escape — with two
//!    DETERMINISTIC guards (no ancestry mechanism): (i) a target == the daemon pid → DENY;
//!    (ii) a negative `-<pgid>` group-kill target (incl. `-1` = all your processes) → DENY.
//!    `kill 0` (own process group) is NOT guarded.
//!
//! HONEST SCOPE — footgun-guard, NOT a security boundary. Bypassable: an absolute path
//! (`/usr/bin/pkill`), a re-implemented kill, or `AGEND_SAFETY_BYPASS=1` (same caveat class
//! as agend-git's `AGEND_GIT_BYPASS`). Two inherent limits worth naming: (a) `kill` is a
//! SHELL BUILTIN in bash/zsh/sh, so a bare `kill` typed in an agent shell hits the builtin
//! and never reaches this PATH shim — the `kill` guards fire only for EXTERNAL `kill`
//! (`/bin/kill`, `command kill`, `xargs kill`, script `exec`); `pkill`/`killall` are not
//! builtins, so the shim reliably catches them, and those ARE the incident vector. (b) It
//! stops the ACCIDENTAL over-match class (the whole observed threat), not a determined
//! malicious agent — an agent with a shell has unbounded destructive primitives.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of classifying a kill-family invocation. `Deny.event` is the audit label;
/// `Deny.msg` is the one-shot-fixable message printed to the agent.
pub enum Decision {
    Allow,
    Deny { event: &'static str, msg: String },
}

/// Map argv[0]'s basename to the shimmed tool, or `None` for anything else (→ git path).
pub fn shim_tool(argv0: &str) -> Option<&'static str> {
    match basename(argv0) {
        "pkill" => Some("pkill"),
        "killall" => Some("killall"),
        "kill" => Some("kill"),
        _ => None,
    }
}

/// PURE policy classifier — testable without touching the process. `daemon_pid` is the
/// resolved daemon pid (`None` → guard (i) is skipped; the pattern/group guards still apply).
pub fn classify(tool: &str, args: &[String], daemon_pid: Option<i32>) -> Decision {
    match tool {
        "pkill" | "killall" => {
            // #2683 B1: FAIL-CLOSED. pkill/killall are pattern-scan by design; exempt ONLY a
            // pure informational invocation (all args are no-kill flags), DENY everything else.
            // Detecting a target by its FORM is a losing enumeration (`--` operands, `--uid=`/
            // `--newest` selectors, future procps flags all specify targets while every arg
            // starts with `-`), so we invert to an allowlist — a new selector fails to DENY.
            if is_informational_only(args) {
                Decision::Allow
            } else {
                Decision::Deny {
                    event: "deny_unscoped_pattern_kill",
                    msg: deny_pattern_kill(tool, args),
                }
            }
        }
        "kill" => {
            for op in kill_operands(args) {
                if let Ok(n) = op.parse::<i64>() {
                    if n < 0 {
                        return Decision::Deny {
                            event: "deny_group_kill",
                            msg: deny_group_kill(args, op),
                        };
                    }
                    if n > 0 && daemon_pid.map(i64::from) == Some(n) {
                        return Decision::Deny {
                            event: "deny_kill_daemon",
                            msg: deny_kill_daemon(args, n),
                        };
                    }
                }
            }
            Decision::Allow
        }
        _ => Decision::Allow,
    }
}

/// Entry from `main` — never returns. Bypass → audit + exec real. Otherwise classify;
/// Deny → print message + audit + exit 1; Allow → exec the real binary.
pub fn run(tool: &'static str, args: &[String]) -> ! {
    if should_bypass() {
        append_safety_event(tool, args, "bypass");
        exec_real(tool, args);
    }
    match classify(tool, args, daemon_pid()) {
        Decision::Allow => exec_real(tool, args),
        Decision::Deny { event, msg } => {
            eprintln!("{msg}");
            append_safety_event(tool, args, event);
            std::process::exit(1);
        }
    }
}

// ── policy helpers (pure) ────────────────────────────────────────────────────

/// The ONLY pkill/killall invocations exempt from the guard: pure informational flags that
/// perform no kill. Deliberately a small allowlist (#2683 B1 fail-closed) — anything not here
/// (a pattern, a `--uid=`/`-u`/`--newest` selector, `--` + operand) DENIES. Empty args are
/// vacuously informational (`pkill` alone errors with no criteria — no kill).
const KILL_INFO_FLAGS: &[&str] = &["--help", "-h", "--version", "-V", "-l", "--list", "-L"];

fn is_informational_only(args: &[String]) -> bool {
    args.iter().all(|a| KILL_INFO_FLAGS.contains(&a.as_str()))
}

/// The target operands of a `kill` invocation, after stripping an optional leading signal
/// spec (`-s SIG` / `-n NUM` / `-SIG`) and `--` separators. Per POSIX `kill` grammar the
/// signal, if present, is the FIRST argument; every later token is a target (positive pid,
/// `0`, or a negative `-pgid`).
fn kill_operands(args: &[String]) -> Vec<&str> {
    let skip = match args.first().map(String::as_str) {
        Some("-s" | "--signal" | "-n") => 2, // signal flag + its value
        Some("--") => 1,                     // explicit end-of-options
        Some(s) if s.starts_with('-') && s.len() > 1 => 1, // -9 / -TERM / -SIGTERM / info flag
        _ => 0,
    };
    args.iter()
        .skip(skip.min(args.len()))
        .map(String::as_str)
        .filter(|a| *a != "--")
        .collect()
}

/// argv[0] basename with any `.exe` suffix removed.
fn basename(invoked: &str) -> &str {
    let base = Path::new(invoked)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(invoked);
    base.strip_suffix(".exe").unwrap_or(base)
}

// ── one-shot-fixable deny messages (#2677 convention) ────────────────────────

/// The full argv, joined for echoing back what the agent typed.
fn argv_display(args: &[String]) -> String {
    args.iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The pattern tokens for a copy-pasteable `pgrep` hint. Respects `--` (everything after it is
/// the pattern, so a `-`-leading pattern like `--dangerously-bypass` is NOT dropped — #2683 B1);
/// otherwise the non-flag operands. Rendered inside `-- '<pat>'` so pgrep can't parse it as flags.
fn pattern_for_hint(args: &[String]) -> String {
    let toks: Vec<&str> = match args.iter().position(|a| a == "--") {
        Some(pos) => args[pos + 1..].iter().map(String::as_str).collect(),
        None => args
            .iter()
            .filter(|a| !a.starts_with('-'))
            .map(String::as_str)
            .collect(),
    };
    toks.join(" ")
}

fn deny_pattern_kill(tool: &str, args: &[String]) -> String {
    let argv = argv_display(args);
    let pat = pattern_for_hint(args);
    // M1 (#2683): the hint is `pgrep -f -- '<pat>'` — `--` stops pgrep parsing the pattern as
    // flags (`--last` etc.) and the single-quotes survive copy-paste with spaces.
    format!(
        "agend-safety: `{tool} {argv}` denied — a pattern-kill scans EVERY process on the host \
by command line and can match sibling agents or the daemon, not just your own processes \
(2026-06-24: a `pkill -f` over-matched and killed 3 live agents this way). To do what you \
intended safely:\n  1) pgrep -fl -- '{pat}'      # list what matches — confirm they are all yours\n  \
2) kill <pid> [<pid> …]     # kill only the pids you confirmed (explicit pids are allowed)\n\
Deliberate teardown? Re-run with AGEND_SAFETY_BYPASS=1."
    )
}

fn deny_group_kill(args: &[String], op: &str) -> String {
    let argv = argv_display(args);
    format!(
        "agend-safety: `kill {argv}` denied — the target `{op}` is NEGATIVE, which signals an \
entire process GROUP and can reach beyond your own subtree (`kill -- -1` hits every process \
you own). Kill explicit positive pids instead:\n  1) pgrep -fl <your-pattern>   # find the pids\n  \
2) kill <pid> [<pid> …]       # one signal per confirmed pid\nDeliberate? Re-run with AGEND_SAFETY_BYPASS=1."
    )
}

fn deny_kill_daemon(args: &[String], pid: i64) -> String {
    let argv = argv_display(args);
    format!(
        "agend-safety: `kill {argv}` denied — target pid {pid} is the agend daemon; killing it \
takes down the whole fleet. Daemon lifecycle goes through the operator (`restart_daemon`), not an \
agent shell. To act on your OWN processes, kill their pids instead. Deliberate? Re-run with \
AGEND_SAFETY_BYPASS=1."
    )
}

// ── environment / exec (impure) ──────────────────────────────────────────────

fn should_bypass() -> bool {
    // Mirrors agend-git `should_bypass` (any value present = bypass).
    env::var("AGEND_SAFETY_BYPASS").is_ok()
}

/// The daemon pid, injected by the daemon at agent spawn (`agent/mod.rs`). `None` when
/// absent (non-daemon-spawned shell) → the daemon-pid guard is skipped; the others hold.
fn daemon_pid() -> Option<i32> {
    env::var("AGEND_DAEMON_PID").ok()?.trim().parse().ok()
}

/// Resolve the REAL binary for `tool`, never this shim. Mirrors `resolve_real_git`:
/// env override → `which` over PATH with `$AGEND_HOME/bin` (the shim dir) excluded →
/// absolute fallback (which can never be the shim).
fn resolve_real_binary(tool: &str) -> String {
    let env_key = format!("AGEND_REAL_{}", tool.to_uppercase());
    if let Ok(p) = env::var(&env_key) {
        if !p.is_empty() && Path::new(&p).exists() {
            return p;
        }
    }
    let shim_dir: Option<PathBuf> = env::var_os("AGEND_HOME").map(|h| PathBuf::from(h).join("bin"));
    let path_os = env::var_os("PATH").unwrap_or_default();
    let search: Vec<PathBuf> = env::split_paths(&path_os)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !crate::same_dir(p, shim_dir.as_deref()))
        .collect();
    if let Ok(joined) = env::join_paths(&search) {
        if let Ok(p) = which::which_in(tool, Some(&joined), ".") {
            // Belt-and-suspenders: never exec back into THIS binary. If the shim-dir PATH
            // exclusion above ever missed (AGEND_HOME unset/mismatched), `which` could resolve
            // e.g. `kill` to the `$AGEND_HOME/bin/kill` symlink → infinite self-exec. Reject a
            // match that canonicalizes to our own exe and fall through to the absolute path.
            if !resolves_to_self(&p) {
                return p.display().to_string();
            }
        }
    }
    match tool {
        "kill" => "/bin/kill",
        "pkill" => "/usr/bin/pkill",
        "killall" => "/usr/bin/killall",
        other => other,
    }
    .to_string()
}

/// True when `candidate` canonicalizes to this running binary — a recursion guard for the
/// allow-path exec (see `resolve_real_binary`).
fn resolves_to_self(candidate: &Path) -> bool {
    match (
        env::current_exe().and_then(|p| p.canonicalize()),
        candidate.canonicalize(),
    ) {
        (Ok(me), Ok(other)) => me == other,
        _ => false,
    }
}

fn exec_real(tool: &str, args: &[String]) -> ! {
    let real = resolve_real_binary(tool);
    let mut cmd = Command::new(&real);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("agend-safety: exec {tool} failed: {err}");
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("agend-safety: exec {tool} failed: {e}");
                std::process::exit(127);
            }
        }
    }
}

/// Best-effort append to the daemon-observable `fleet_events.jsonl`. NEVER blocks the
/// deny/exec path (same sink + discipline as the git shim's audit events).
fn append_safety_event(tool: &str, args: &[String], event: &str) {
    let home = match env::var("AGEND_HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => return,
    };
    let rec = serde_json::json!({
        "kind": "safety_event",
        "event": event,
        "agent": env::var("AGEND_INSTANCE_NAME").unwrap_or_default(),
        "tool": tool,
        "argv": args,
        "cwd": env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "ppid": crate::parent_pid(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let path = PathBuf::from(home).join("fleet_events.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{rec}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    fn denied(tool: &str, a: &[&str], daemon: Option<i32>) -> bool {
        matches!(classify(tool, &v(a), daemon), Decision::Deny { .. })
    }

    #[test]
    fn shim_tool_maps_only_the_kill_family() {
        assert_eq!(
            shim_tool("/home/u/.agend-terminal/bin/pkill"),
            Some("pkill")
        );
        assert_eq!(shim_tool("killall"), Some("killall"));
        assert_eq!(shim_tool("kill.exe"), Some("kill"));
        assert_eq!(shim_tool("/usr/bin/git"), None);
        assert_eq!(shim_tool("pgrep"), None); // pgrep stays ALLOWED (the sanctioned list tool)
    }

    #[test]
    fn pkill_killall_pattern_kills_are_denied() {
        // The incident form + common variants: bare pattern, -f, bundled signal flags.
        assert!(denied(
            "pkill",
            &["-f", "resume --last --dangerously-bypass"],
            None
        ));
        assert!(denied("pkill", &["claude"], None));
        assert!(denied("pkill", &["-9", "-f", "agend-terminal"], None));
        assert!(denied("pkill", &["--signal", "TERM", "codex"], None));
        assert!(denied("killall", &["claude"], None));
        assert!(denied("killall", &["-9", "codex"], None));
    }

    #[test]
    fn pkill_killall_informational_only_is_allowed() {
        // The fail-closed allowlist: pure informational flags perform no kill → allow.
        assert!(!denied("pkill", &["--help"], None));
        assert!(!denied("pkill", &["-h"], None));
        assert!(!denied("killall", &["--version"], None));
        assert!(!denied("killall", &["-V"], None));
        assert!(!denied("killall", &["-l"], None)); // list signal names, no kill
        assert!(!denied("pkill", &[], None)); // bare pkill: no criteria → errors, no kill
    }

    #[test]
    fn b1_flag_form_and_end_of_options_targets_are_denied() {
        // #2683 review B1 (dev3 CONFIRMED bypass): a `!starts_with('-')` target test is fail-OPEN.
        // A pattern after `--`, or a flag-form selector, has EVERY arg starting with `-` yet still
        // makes procps kill host-wide. All must DENY (RED against the pre-fix has_kill_target).
        assert!(denied("pkill", &["-f", "--", "--dangerously-bypass"], None)); // the live incident substring
        assert!(denied("pkill", &["-f", "--", "-claude"], None));
        assert!(denied("killall", &["--", "-codex"], None));
        assert!(denied("pkill", &["--uid=501"], None)); // selector, no positional
        assert!(denied("pkill", &["--euid=0"], None));
        assert!(denied("pkill", &["--newest", "--uid=501"], None));
        assert!(denied("pkill", &["-u", "root"], None)); // -u <user> selector
                                                         // Incomplete/selector-only pkill is still a kill attempt in spirit → fail-closed DENY.
        assert!(denied("pkill", &["-f"], None));
    }

    #[test]
    fn kill_explicit_positive_pids_pass_through() {
        // The sanctioned scoped escape — must NOT be blocked.
        assert!(!denied("kill", &["1234"], Some(999)));
        assert!(!denied("kill", &["-9", "1234"], Some(999)));
        assert!(!denied("kill", &["-TERM", "1234", "5678"], Some(999)));
        assert!(!denied("kill", &["-s", "KILL", "1234"], Some(999)));
        assert!(!denied("kill", &["0"], Some(999))); // kill 0 = own pgroup → NOT guarded
        assert!(!denied("kill", &["%1"], Some(999))); // job spec → not our concern
    }

    #[test]
    fn kill_daemon_pid_is_denied() {
        assert!(denied("kill", &["4242"], Some(4242)));
        assert!(denied("kill", &["-9", "4242"], Some(4242)));
        assert!(denied("kill", &["-s", "TERM", "1111", "4242"], Some(4242))); // daemon among many
                                                                              // No daemon pid known → guard (i) skipped, positive pid allowed.
        assert!(!denied("kill", &["4242"], None));
    }

    #[test]
    fn kill_negative_group_targets_are_denied() {
        assert!(denied("kill", &["--", "-1234"], None)); // explicit group-kill
        assert!(denied("kill", &["-9", "-1234"], None)); // signal + group target
        assert!(denied("kill", &["-TERM", "--", "-1"], None)); // -1 = ALL your processes
        assert!(denied("kill", &["-s", "KILL", "-1"], None));
        // A bare `-9` (signal, no target) is not a group-kill → allow (harmless usage error).
        assert!(!denied("kill", &["-9"], None));
    }

    #[test]
    fn deny_messages_are_one_shot_fixable() {
        let m = deny_pattern_kill("pkill", &v(&["-f", "codex"]));
        assert!(m.contains("`pkill -f codex` denied"), "echoes argv: {m}");
        // M1: quoted, `--`-guarded pgrep hint (copy-paste-safe; pattern not parsed as flags).
        assert!(m.contains("pgrep -fl -- 'codex'"), "quoted pgrep hint: {m}");
        assert!(m.contains("kill <pid>"), "must teach explicit kill: {m}");
        assert!(
            m.contains("AGEND_SAFETY_BYPASS=1"),
            "must name the bypass: {m}"
        );
        // killall by name → the pgrep hint carries only the operand (no leaked `-9`),
        // even though the echoed argv line legitimately shows `killall -9 codex`.
        let k = deny_pattern_kill("killall", &v(&["-9", "codex"]));
        assert!(
            k.contains("pgrep -fl -- 'codex'"),
            "quoted killall hint: {k}"
        );
        // B1 + M1: a `--`-guarded `-`-leading pattern must survive into the hint, quoted.
        let b = deny_pattern_kill("pkill", &v(&["-f", "--", "--dangerously-bypass"]));
        assert!(
            b.contains("pgrep -fl -- '--dangerously-bypass'"),
            "post-`--` pattern preserved + quoted: {b}"
        );
        let g = deny_group_kill(&v(&["--", "-1"]), "-1");
        assert!(g.contains("AGEND_SAFETY_BYPASS=1") && g.contains("kill <pid>"));
        let d = deny_kill_daemon(&v(&["4242"]), 4242);
        assert!(d.contains("restart_daemon") && d.contains("AGEND_SAFETY_BYPASS=1"));
    }
}
