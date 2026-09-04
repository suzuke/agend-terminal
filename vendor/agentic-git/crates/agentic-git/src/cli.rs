//! agentic-git session mode — the CLI surface reached when argv[0] does NOT
//! look like `git` (see `is_git_invocation` in `main.rs`). This absorbs the
//! **minimal** orchestrator role for a standalone user: provisioning a
//! worktree, a signed binding, and hooks, then launching the agent inside the
//! guarded session (see `agentic-git` issue #1 — this module implements that
//! design literally; do not re-derive it from first principles here).
//!
//! Subcommands: `run`, `version`. Anything else — including no subcommand —
//! is a hard `exit(2)` with usage (Δ1): CLI mode never silently falls back to
//! shim behavior.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentic_git_core::{binding, integrity_core};

pub fn cli_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run_cmd(&args[1..]),
        Some("snapshots") => snapshots_cmd(&args[1..]),
        Some("version") => {
            println!("agentic-git {} (cli)", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Some(other) => {
            print_usage(Some(other));
            std::process::exit(2);
        }
        None => {
            print_usage(None);
            std::process::exit(2);
        }
    }
}

fn print_usage(bad: Option<&str>) {
    match bad {
        Some(cmd) => eprintln!("agentic-git: unknown subcommand `{cmd}`"),
        None => eprintln!("agentic-git: no subcommand given"),
    }
    eprintln!(
        "Usage:\n  \
         agentic-git run [--agent <name>] [--branch <branch>] [--base <ref>] -- <cmd...>\n  \
         agentic-git snapshots list [--repo <path>]\n  \
         agentic-git snapshots restore [<snapshot-ref>] [--repo <path>] [--yes] [--staged]\n  \
         agentic-git snapshots prune [--older-than <N>d] [--repo <path>]\n  \
         agentic-git version\n\n\
         To use the shim, invoke via <home>/bin/git (see README)."
    );
}

// ── `snapshots` (issue #4 P2 recovery layer) ────────────────────────────

fn print_snapshots_usage() {
    eprintln!(
        "Usage:\n  \
         agentic-git snapshots list [--repo <path>]\n  \
         agentic-git snapshots restore [<snapshot-ref>] [--repo <path>] [--yes] [--staged]\n  \
         agentic-git snapshots prune [--older-than <N>d] [--repo <path>]\n\n\
         restore with no ref uses the only snapshot (or --yes for the newest of\n  \
         several); it writes the snapshot's files back to the working tree\n  \
         without deleting anything created since, and saves your current state\n  \
         first so the restore is itself undoable."
    );
}

fn snapshots_cmd(args: &[String]) -> ! {
    match args.first().map(String::as_str) {
        Some("list") => snapshots_list_cmd(&args[1..]),
        Some("prune") => snapshots_prune_cmd(&args[1..]),
        Some("restore") => snapshots_restore_cmd(&args[1..]),
        Some(other) => {
            eprintln!("agentic-git: snapshots: unknown subcommand `{other}`");
            print_snapshots_usage();
            std::process::exit(2);
        }
        None => {
            eprintln!("agentic-git: snapshots: no subcommand given");
            print_snapshots_usage();
            std::process::exit(2);
        }
    }
}

fn parse_repo_flag(args: &[String], caller: &str) -> Result<PathBuf, String> {
    let mut i = 0;
    let mut repo: Option<PathBuf> = None;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                i += 1;
                let v = args.get(i).ok_or("`--repo` requires a value")?;
                repo = Some(PathBuf::from(v));
            }
            other => return Err(format!("unknown flag `{other}` for `{caller}`")),
        }
        i += 1;
    }
    Ok(repo.unwrap_or_else(|| PathBuf::from(".")))
}

fn parse_prune_args(args: &[String]) -> Result<(PathBuf, u64), String> {
    let mut i = 0;
    let mut repo: Option<PathBuf> = None;
    let mut ttl_secs = super::snapshot::DEFAULT_TTL_SECS;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                i += 1;
                let v = args.get(i).ok_or("`--repo` requires a value")?;
                repo = Some(PathBuf::from(v));
            }
            "--older-than" => {
                i += 1;
                let v = args.get(i).ok_or("`--older-than` requires a value")?;
                ttl_secs = parse_older_than(v)?;
            }
            other => return Err(format!("unknown flag `{other}` for `snapshots prune`")),
        }
        i += 1;
    }
    Ok((repo.unwrap_or_else(|| PathBuf::from(".")), ttl_secs))
}

/// `<N>d` (days) — the only unit the issue's CLI surface specifies.
fn parse_older_than(s: &str) -> Result<u64, String> {
    let days_str = s
        .strip_suffix(['d', 'D'])
        .ok_or_else(|| format!("`--older-than` value {s:?} must look like `<N>d`"))?;
    let days: u64 = days_str
        .parse()
        .map_err(|_| format!("`--older-than` value {s:?} must look like `<N>d`"))?;
    Ok(days * 24 * 60 * 60)
}

fn snapshots_list_cmd(args: &[String]) -> ! {
    let repo = match parse_repo_flag(args, "snapshots list") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentic-git: snapshots list: {e}");
            print_snapshots_usage();
            std::process::exit(2);
        }
    };
    let git = super::resolve_real_git();
    match super::snapshot::list_snapshots(&git, &repo) {
        Ok(rows) => {
            if rows.is_empty() {
                println!("(no snapshots)");
            } else {
                println!(
                    "{:<58} {:<21} {:<12} {:<12} SUBJECT",
                    "REF", "WHEN", "OP", "WHO"
                );
                for r in rows {
                    println!(
                        "{:<58} {:<21} {:<12} {:<12} {}",
                        r.refname, r.when, r.op, r.who, r.subject
                    );
                }
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("agentic-git: snapshots list: {e}");
            std::process::exit(1);
        }
    }
}

fn snapshots_prune_cmd(args: &[String]) -> ! {
    let (repo, ttl_secs) = match parse_prune_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agentic-git: snapshots prune: {e}");
            print_snapshots_usage();
            std::process::exit(2);
        }
    };
    let git = super::resolve_real_git();
    match super::snapshot::prune_refs(&git, &repo, ttl_secs, None) {
        Ok(pruned) => {
            for r in &pruned {
                println!("pruned {r}");
            }
            println!("{} snapshot ref(s) pruned", pruned.len());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("agentic-git: snapshots prune: {e}");
            std::process::exit(1);
        }
    }
}

/// `[<ref>] [--repo <path>] [--yes] [--staged]` — at most one positional
/// (the snapshot ref). Hand-rolled to match the sibling commands' discipline.
fn parse_restore_args(args: &[String]) -> Result<(PathBuf, Option<String>, bool, bool), String> {
    let mut repo: Option<PathBuf> = None;
    let mut target: Option<String> = None;
    let mut assume_yes = false;
    let mut staged = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => {
                i += 1;
                let v = args.get(i).ok_or("`--repo` requires a value")?;
                repo = Some(PathBuf::from(v));
            }
            "--yes" | "-y" => assume_yes = true,
            "--staged" => staged = true,
            flag if flag.starts_with('-') && flag != "-" => {
                return Err(format!("unknown flag `{flag}` for `snapshots restore`"));
            }
            other => {
                if target.is_some() {
                    return Err(format!(
                        "unexpected extra argument `{other}` (at most one snapshot ref)"
                    ));
                }
                target = Some(other.to_string());
            }
        }
        i += 1;
    }
    Ok((repo.unwrap_or_else(|| PathBuf::from(".")), target, assume_yes, staged))
}

fn snapshots_restore_cmd(args: &[String]) -> ! {
    let (repo, target, assume_yes, staged) = match parse_restore_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agentic-git: snapshots restore: {e}");
            print_snapshots_usage();
            std::process::exit(2);
        }
    };
    let git = super::resolve_real_git();
    let opts = super::snapshot::RestoreOpts {
        target_ref: target.as_deref(),
        assume_yes,
        staged,
        agent: "",
    };
    match super::snapshot::restore_snapshot(&git, &repo, &opts) {
        Ok(o) => {
            print_restore_outcome(&o);
            std::process::exit(0);
        }
        Err(e) => std::process::exit(print_restore_error(&e)),
    }
}

fn print_restore_outcome(o: &super::snapshot::RestoreOutcome) {
    if o.already_current {
        println!(
            "Working tree already matches {} \u{2014} nothing to restore.",
            o.restored_from
        );
        return;
    }
    let staging = if o.staged {
        "left staged in the index"
    } else {
        "left unstaged (working tree only)"
    };
    println!("Restored {} path(s) from {}", o.paths_written, o.restored_from);
    if !o.when.is_empty() {
        println!("  snapshot taken {} (before a `{}` operation)", o.when, o.op);
    }
    println!("  files created after the snapshot were left untouched; changes {staging}");
    if let Some(pre) = &o.pre_restore_ref {
        println!("  \u{21a9} your pre-restore state was saved \u{2014} undo with:");
        println!("      agentic-git snapshots restore {pre}");
    }
}

/// Map a restore failure to its user message + process exit code. Arg/usage
/// problems exit 2 (mirrors the parse layer); operational failures exit 1.
fn print_restore_error(e: &super::snapshot::RestoreError) -> i32 {
    use super::snapshot::RestoreError::*;
    match e {
        NoSnapshots => {
            eprintln!("agentic-git: snapshots restore: no snapshots to restore from");
            1
        }
        NotOurRef(r) => {
            eprintln!(
                "agentic-git: snapshots restore: `{r}` is not an agentic-git snapshot ref \
                 (must start with refs/agentic-git/snapshots/); see `snapshots list`"
            );
            2
        }
        NoSuchSnapshot(r) => {
            eprintln!("agentic-git: snapshots restore: no such snapshot `{r}`");
            1
        }
        Ambiguous(rows) => {
            eprintln!(
                "agentic-git: snapshots restore: {} snapshots exist \u{2014} refusing to guess.",
                rows.len()
            );
            eprintln!("Pass an explicit ref (newest first):");
            for r in rows.iter().take(3) {
                eprintln!("  {}   ({}, before `{}`)", r.refname, r.when, r.op);
            }
            eprintln!("\u{2026}or re-run with --yes to restore the newest.");
            2
        }
        PreRestoreFailed(msg) => {
            eprintln!(
                "agentic-git: snapshots restore: could not save current state before restoring \
                 ({msg}); aborted to avoid overwriting unsaved work. Commit or stash, then retry."
            );
            1
        }
        Git(msg) => {
            eprintln!("agentic-git: snapshots restore: {msg}");
            1
        }
    }
}

// ── `run` argument parsing (hand-rolled — no clap, dependency discipline) ──

#[derive(Debug, Default, PartialEq, Eq)]
struct RunArgs {
    agent: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    cmd: Vec<String>,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut out = RunArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                out.cmd = args[i + 1..].to_vec();
                if out.cmd.is_empty() {
                    return Err("`run` requires a command after `--`".to_string());
                }
                return Ok(out);
            }
            "--agent" => {
                i += 1;
                out.agent = Some(
                    args.get(i)
                        .ok_or("`--agent` requires a value")?
                        .to_string(),
                );
            }
            "--branch" => {
                i += 1;
                out.branch = Some(
                    args.get(i)
                        .ok_or("`--branch` requires a value")?
                        .to_string(),
                );
            }
            "--base" => {
                i += 1;
                out.base = Some(args.get(i).ok_or("`--base` requires a value")?.to_string());
            }
            other => return Err(format!("unknown flag `{other}` for `run`")),
        }
        i += 1;
    }
    Err("`run` requires `-- <cmd...>`".to_string())
}

// ── Δ3 v3: `--agent` validation — pure fn, Windows/case-insensitive-safe ──

const RESERVED_WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

const AGENT_NAME_RULE: &str = "--agent must match ^[a-z0-9][a-z0-9._-]{0,63}$ (lowercase only), \
     must not end with '.', and its stem before the first '.' (uppercased) must not be a \
     reserved Windows device name (CON, PRN, AUX, NUL, COM1-9, LPT1-9)";

/// Δ3 v3 agent-name contract. Pure function — no filesystem access — so it is
/// unit-testable in isolation. `Err` carries the rule text verbatim (the CLI
/// quotes it back to the user on violation).
fn validate_agent_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 64 {
        return Err(AGENT_NAME_RULE);
    }
    let bytes = name.as_bytes();
    let is_lower_alnum = |b: u8| b.is_ascii_digit() || b.is_ascii_lowercase();
    let is_allowed = |b: u8| is_lower_alnum(b) || matches!(b, b'.' | b'_' | b'-');
    if !is_lower_alnum(bytes[0]) {
        return Err(AGENT_NAME_RULE);
    }
    if !bytes.iter().all(|&b| is_allowed(b)) {
        return Err(AGENT_NAME_RULE);
    }
    if name.ends_with('.') {
        return Err(AGENT_NAME_RULE);
    }
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED_WINDOWS_DEVICE_NAMES.contains(&stem.to_ascii_uppercase().as_str()) {
        return Err(AGENT_NAME_RULE);
    }
    Ok(())
}

fn default_agent_name() -> String {
    let mut buf = [0u8; 6];
    match getrandom::fill(&mut buf) {
        Ok(()) => format!("run-{}", hex_lower(&buf)),
        Err(_) => {
            // Extremely unlikely (OS RNG unavailable); fall back to a
            // monotonic-ish, still lowercase-alnum, still-valid identity
            // rather than hard-failing session start over cosmetic naming.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("run-{:x}", nanos as u64)
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn default_branch(agent: &str) -> String {
    format!(
        "agent/{agent}/{}",
        chrono::Utc::now().format("%Y%m%d-%H%M")
    )
}

// ── Home provisioning ───────────────────────────────────────────────────

fn resolve_home() -> PathBuf {
    if let Ok(h) = super::env_compat("AGENTIC_GIT_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home_dir = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home_dir.join(".agentic-git")
}

/// Provision `bin/`, `runtime/`, `worktrees/`, `hooks/`, and the integrity
/// key (iff missing). A key that can't be written is a hard error — a
/// guarded session without a signable binding must not silently degrade.
fn provision_home(home: &Path) -> Result<(), String> {
    for d in ["bin", "runtime", "worktrees", "hooks"] {
        let p = home.join(d);
        std::fs::create_dir_all(&p).map_err(|e| format!("create {}: {e}", p.display()))?;
    }
    integrity_core::ensure_key(home)
}

// ── Preconditions (step 1) ─────────────────────────────────────────────

fn git_bypass_cmd(git: &str) -> Command {
    let mut cmd = Command::new(git);
    cmd.env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1");
    cmd
}

/// A filename-safe, cross-process-stable key for a source repo (FNV-1a over the
/// path bytes — deterministic across processes/Rust versions, unlike
/// `DefaultHasher`, so two racing `run`s pick the SAME lock file).
fn provision_lock_key(source_repo: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in source_repo.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Acquire the per-source-repo provisioning lock, returning a guard held until
/// dropped. The whole provisioning critical section — `git worktree add` AND the
/// `git config --worktree` hooks wiring — is NOT concurrency-safe: each reads the
/// shared worktree list and can fatal on a sibling's half-written admin metadata
/// (`failed to read .git/worktrees/<other>/commondir`). An advisory `flock`,
/// keyed by the source repo and kept under the home (never inside `.git`), makes
/// two racing `run`s take turns; it auto-releases when the fd closes (process
/// exit included), so a crashed holder never wedges it. Best-effort: any setup
/// failure returns `None` and provisioning proceeds unserialized (the
/// pre-existing behavior) rather than blocking a session start. MUST be dropped
/// before the agent spawns, or the whole SESSION would serialize.
#[cfg(unix)]
fn acquire_provision_lock(home: &Path, source_repo: &Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let dir = home.join("locks");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("wt-{}.lock", provision_lock_key(source_repo)));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .ok()?;
    // Blocking exclusive advisory lock; released when the returned File drops.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(file)
}

#[cfg(not(unix))]
fn acquire_provision_lock(_home: &Path, _source_repo: &Path) -> Option<std::fs::File> {
    // v1: no cross-process advisory lock on Windows (already an unverified
    // platform); provisioning there proceeds unserialized.
    None
}

fn resolve_source_repo(git: &str) -> Result<PathBuf, String> {
    let out = git_bypass_cmd(git)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(
            "cwd is not inside a git repository — `run` requires a non-bare git repo".to_string(),
        );
    }
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if top.is_empty() {
        return Err("git rev-parse --show-toplevel returned no path".to_string());
    }

    let bare_out = git_bypass_cmd(git)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    let is_bare = String::from_utf8_lossy(&bare_out.stdout).trim() == "true";
    if is_bare {
        return Err("cwd is inside a bare git repository — `run` requires a non-bare repo".into());
    }
    Ok(PathBuf::from(top))
}

// ── Δ4: binding reuse predicate ─────────────────────────────────────────

fn canonical_or_lexical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `git rev-parse --git-common-dir` run inside `dir`, canonicalized. Used to
/// prove a worktree actually belongs to the repo we think it does (Δ4).
fn git_common_dir(git: &str, dir: &Path) -> Option<PathBuf> {
    let out = git_bypass_cmd(git)
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(&raw);
    let abs = if p.is_absolute() { p } else { dir.join(p) };
    Some(canonical_or_lexical(&abs))
}

/// `None` → no existing binding, proceed with fresh provisioning.
/// `Some(Ok(()))` → reuse the existing worktree/binding as-is.
/// `Some(Err(reason))` → hard error; caller must NOT provision anything.
#[allow(clippy::too_many_arguments)]
fn check_reuse(
    home: &Path,
    agent: &str,
    requested_branch: &str,
    requested_worktree: &Path,
    source_repo: &Path,
    git: &str,
) -> Option<Result<(), String>> {
    let dir = home.join("runtime").join(agent);
    let content = std::fs::read_to_string(dir.join("binding.json")).ok()?;
    let existing: serde_json::Value = serde_json::from_str(&content).ok()?;

    let remedy = format!(
        "stale binding at {} — remove it, and if the worktree still exists remove it too \
         (`git worktree remove <path>`), then re-run",
        dir.display()
    );
    let eb_branch = existing["branch"].as_str().unwrap_or_default();
    let eb_worktree = existing["worktree"].as_str().unwrap_or_default();
    let eb_source = existing["source_repo"].as_str().unwrap_or_default();

    if eb_branch != requested_branch {
        return Some(Err(format!(
            "agent {agent:?} is already bound to branch {eb_branch:?}, which does not match the \
             requested branch {requested_branch:?}; {remedy}"
        )));
    }
    let requested_worktree_str = requested_worktree.to_string_lossy();
    if eb_worktree != requested_worktree_str {
        return Some(Err(format!(
            "agent {agent:?} is already bound to worktree {eb_worktree:?}, which does not match \
             the requested worktree {requested_worktree_str:?}; {remedy}"
        )));
    }
    if canonical_or_lexical(Path::new(eb_source)) != canonical_or_lexical(source_repo) {
        return Some(Err(format!(
            "agent {agent:?}'s binding names a different source_repo ({eb_source:?}) than the \
             current repo ({}); {remedy}",
            source_repo.display()
        )));
    }
    if !requested_worktree.exists() {
        return Some(Err(format!(
            "agent {agent:?}'s bound worktree no longer exists on disk ({requested_worktree_str}); \
             {remedy}"
        )));
    }
    let wt_common = git_common_dir(git, requested_worktree);
    let repo_common = git_common_dir(git, source_repo);
    match (wt_common, repo_common) {
        (Some(a), Some(b)) if a == b => Some(Ok(())),
        _ => Some(Err(format!(
            "agent {agent:?}'s bound worktree does not resolve back into this repo's gitdir \
             (ownership check failed); {remedy}"
        ))),
    }
}

// ── Hooks (step 6) ───────────────────────────────────────────────────────

const HOOK_PREPARE_COMMIT_MSG: &str = include_str!("../assets/hooks/prepare-commit-msg");
const HOOK_REFERENCE_TRANSACTION: &str = include_str!("../assets/hooks/reference-transaction");
#[cfg(windows)]
const HOOK_PREPARE_COMMIT_MSG_PS1: &str =
    include_str!("../assets/hooks/prepare-commit-msg.ps1");

fn write_hook(path: &Path, content: &str) -> Result<(), String> {
    std::fs::write(path, content).map_err(|e| format!("write hook {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod hook {}: {e}", path.display()))?;
    }
    Ok(())
}

fn install_hooks(home: &Path) -> Result<(), String> {
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir).map_err(|e| format!("mkdir hooks: {e}"))?;
    write_hook(
        &hooks_dir.join("prepare-commit-msg"),
        HOOK_PREPARE_COMMIT_MSG,
    )?;
    write_hook(
        &hooks_dir.join("reference-transaction"),
        HOOK_REFERENCE_TRANSACTION,
    )?;
    #[cfg(windows)]
    write_hook(
        &hooks_dir.join("prepare-commit-msg.ps1"),
        HOOK_PREPARE_COMMIT_MSG_PS1,
    )?;
    Ok(())
}

/// Amended step 6: `extensions.worktreeConfig` is a repo-wide switch (must
/// land in the SHARED config), `core.hooksPath` is set `--worktree`-scoped so
/// only THIS worktree gets it — the user's own checkout keeps its hooks
/// (Δ5 hook noninterference).
///
/// `extensions.worktreeConfig` writes the SHARED `$GIT_DIR/config` (it isn't
/// per-worktree until it's set), so two `run`s racing against the SAME source
/// repo (different agents/branches, same first-use) can collide on git's own
/// `config.lock`. That's a plain transient lock, not a correctness issue —
/// retry briefly instead of hard-failing session start over it.
fn configure_worktree_hooks(git: &str, worktree: &Path, hooks_dir: &Path) -> Result<(), String> {
    let run = |args: &[&str]| -> Result<(), String> {
        let mut last_err = String::new();
        for attempt in 0..20 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let out = git_bypass_cmd(git)
                .args(args)
                .current_dir(worktree)
                .output()
                .map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok(());
            }
            last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !last_err.contains("could not lock config file") {
                break;
            }
        }
        Err(format!("git {} failed: {last_err}", args.join(" ")))
    };
    run(&["config", "extensions.worktreeConfig", "true"])?;
    run(&[
        "config",
        "--worktree",
        "core.hooksPath",
        &hooks_dir.to_string_lossy(),
    ])?;
    Ok(())
}

// ── Shim wiring (step 7) ────────────────────────────────────────────────

fn ensure_shim_symlink(home: &Path) -> Result<(), String> {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("mkdir bin: {e}"))?;
    let target_name = if cfg!(windows) { "git.exe" } else { "git" };
    let bin_git = bin_dir.join(target_name);
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    #[cfg(unix)]
    {
        if let Ok(existing) = std::fs::read_link(&bin_git) {
            if existing == exe {
                return Ok(()); // already correct — idempotent.
            }
        }
        let _ = std::fs::remove_file(&bin_git);
        std::os::unix::fs::symlink(&exe, &bin_git)
            .map_err(|e| format!("symlink {}: {e}", bin_git.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Windows: copy, not symlink (symlinks need elevated privileges by
        // default). Re-copy unconditionally — cheap, and always correct.
        std::fs::copy(&exe, &bin_git).map_err(|e| format!("copy to {}: {e}", bin_git.display()))?;
    }
    Ok(())
}

// ── Signal passthrough (best-effort; step 9 "signals forwarded") ───────

/// The terminal delivers SIGINT/SIGTERM to the whole foreground process
/// group already (parent AND child, since we don't put the child in its own
/// group) — so the child sees the signal without us doing anything. What we
/// DO need is for the PARENT to survive it long enough to `wait()` the child
/// and print the summary, instead of dying immediately under the default
/// disposition. A real (non-`SIG_IGN`) handler achieves that: it interrupts
/// the blocking `waitpid` with `EINTR` (which `Child::wait` retries) without
/// terminating us, and — unlike `SIG_IGN` — does NOT propagate across the
/// child's `exec` (so the child keeps normal default signal behavior).
#[cfg(unix)]
fn install_signal_passthrough() {
    extern "C" fn noop_handler(_: i32) {}
    unsafe {
        libc::signal(libc::SIGINT, noop_handler as *const () as usize);
        libc::signal(libc::SIGTERM, noop_handler as *const () as usize);
    }
}
#[cfg(not(unix))]
fn install_signal_passthrough() {
    // v1 known limitation: Windows Ctrl+C handling is left to the platform
    // default (the parent may exit before printing the summary).
}

// ── Spawn (step 8) ───────────────────────────────────────────────────────

fn spawn_agent(cmd: &[String], worktree: &Path, home: &Path, agent: &str, real_git: &str) -> i32 {
    let bin_dir = home.join("bin");
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let new_path_parts: Vec<PathBuf> = std::iter::once(bin_dir)
        .chain(std::env::split_paths(&path_env))
        .collect();
    // Review finding: a home path containing the PATH-list separator (':' on
    // Unix, ';' on Windows) makes `join_paths` fail. Falling back to the
    // ORIGINAL PATH would silently launch the agent UNGUARDED (its `git`
    // resolves to the real binary, bypassing routing/deny/snapshots). Refuse
    // loudly instead — a guarded session we can't guard is not a session.
    let new_path: OsString = match std::env::join_paths(new_path_parts) {
        Ok(p) => p,
        Err(e) => {
            let sep = if cfg!(windows) { ';' } else { ':' };
            eprintln!(
                "agentic-git: cannot build a guarded PATH — AGENTIC_GIT_HOME '{}' \
                 contains a path-list separator ('{sep}'), which PATH cannot represent. \
                 Refusing to launch the agent unguarded. Use a home path without '{sep}'. ({e})",
                home.display()
            );
            return 78; // EX_CONFIG
        }
    };

    let (prog, rest) = match cmd.split_first() {
        Some(pair) => pair,
        None => {
            eprintln!("agentic-git: run: internal error — empty command");
            return 2;
        }
    };

    install_signal_passthrough();

    let mut child = Command::new(prog);
    child
        .args(rest)
        .current_dir(worktree)
        .env("PATH", new_path)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", agent)
        .env("AGENTIC_GIT_REAL_GIT", real_git);

    // #4 Δc: `run` opts standalone sessions INTO the recovery-layer snapshot
    // net by default — but never override an explicit user setting (either
    // env name): `AGENTIC_GIT_SNAPSHOTS=0|off` must still force-disable
    // inside a run session. `env_compat` checks both the primary AND legacy
    // name in THIS process's own (inherited) environment, so injecting `=1`
    // only when it's unset can never clobber a user's `=0`.
    if super::env_compat("AGENTIC_GIT_SNAPSHOTS").is_err() {
        child.env("AGENTIC_GIT_SNAPSHOTS", "1");
    }

    match child.status() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("agentic-git: run: failed to spawn agent command {prog:?}: {e}");
            127
        }
    }
}

// ── `run` orchestration ──────────────────────────────────────────────────

fn run_cmd(raw_args: &[String]) -> ! {
    let parsed = match parse_run_args(raw_args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentic-git: run: {e}");
            print_usage(Some("run"));
            std::process::exit(2);
        }
    };

    let real_git = super::resolve_real_git();

    // Step 1: preconditions.
    let source_repo = match resolve_source_repo(&real_git) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentic-git: run: {e}");
            std::process::exit(1);
        }
    };

    // Step 2: home.
    let home = resolve_home();
    if let Err(e) = provision_home(&home) {
        eprintln!("agentic-git: run: {e}");
        std::process::exit(1);
    }

    // Step 3: identity.
    let agent = parsed.agent.clone().unwrap_or_else(default_agent_name);
    if let Err(rule) = validate_agent_name(&agent) {
        eprintln!("agentic-git: run: invalid --agent {agent:?}: {rule}");
        std::process::exit(2);
    }

    // Step 4: worktree (branch/base defaults + Δ4 reuse-or-fresh decision).
    let branch = parsed.branch.clone().unwrap_or_else(|| default_branch(&agent));
    let base = parsed.base.clone().unwrap_or_else(|| "HEAD".to_string());
    let wt_path = home.join("worktrees").join(&agent).join(&branch);

    let reused = match check_reuse(&home, &agent, &branch, &wt_path, &source_repo, &real_git) {
        Some(Ok(())) => true,
        Some(Err(reason)) => {
            eprintln!("agentic-git: run: {reason}");
            std::process::exit(1);
        }
        None => false,
    };

    let issued_at = chrono::Utc::now().to_rfc3339();

    // Serialize the whole worktree-metadata-touching critical section — the
    // `worktree add` (fresh) AND the `git config --worktree` hooks wiring
    // (Step 6, fresh or reused) — per source repo. Both read the shared worktree
    // list and race a sibling mid-provision (git reports `failed to read
    // .git/worktrees/<other>/commondir`). Held until dropped just before spawn.
    let provision_lock = acquire_provision_lock(&home, &source_repo);

    if !reused {
        if let Some(parent) = wt_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let wt_path_str = wt_path.to_string_lossy().into_owned();
        let wt_out = git_bypass_cmd(&real_git)
            .args(["worktree", "add", &wt_path_str, "-b", &branch, &base])
            .current_dir(&source_repo)
            .output();
        match wt_out {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                // Surface git's own refusal verbatim (e.g. a second `run` on the
                // same branch — natural mutual exclusion, no lease machinery).
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
                std::process::exit(out.status.code().unwrap_or(1));
            }
            Err(e) => {
                eprintln!("agentic-git: run: failed to spawn git worktree add: {e}");
                std::process::exit(1);
            }
        }

        let _ = std::fs::write(
            wt_path.join(".agend-managed"),
            format!("agent={agent}\nleased_at={issued_at}\n"),
        );

        // Step 5: binding (schema v1) + HMAC sidecar — built through the
        // core-owned typed codec (#26), so the reference writer and the shim
        // reader share one representation by construction.
        let binding_doc = binding::BindingV1 {
            agent: Some(agent.clone()),
            task_id: Some(format!(
                "run-session-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            )),
            branch: Some(branch.clone()),
            issued_at: Some(issued_at.clone()),
            worktree: Some(wt_path.to_string_lossy().into_owned()),
            source_repo: Some(source_repo.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let content = match binding::encode(&binding_doc) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("agentic-git: run: serialize binding: {e}");
                std::process::exit(1);
            }
        };
        let runtime_dir = home.join("runtime").join(&agent);
        if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
            eprintln!("agentic-git: run: mkdir {}: {e}", runtime_dir.display());
            std::process::exit(1);
        }
        if let Err(e) = std::fs::write(runtime_dir.join("binding.json"), &content) {
            eprintln!("agentic-git: run: write binding.json: {e}");
            std::process::exit(1);
        }
        let sig = match integrity_core::sign_binding(&home, content.as_bytes()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("agentic-git: run: sign binding: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = std::fs::write(runtime_dir.join("binding.json.sig"), sig) {
            eprintln!("agentic-git: run: write binding.json.sig: {e}");
            std::process::exit(1);
        }
    }

    // Step 6: hooks — embed + wire, always (idempotent whether fresh or reused).
    if let Err(e) = install_hooks(&home) {
        eprintln!("agentic-git: run: {e}");
        std::process::exit(1);
    }
    if let Err(e) = configure_worktree_hooks(&real_git, &wt_path, &home.join("hooks")) {
        eprintln!("agentic-git: run: {e}");
        std::process::exit(1);
    }

    // Provisioning is done — release the lock BEFORE the (long-lived) agent
    // spawns, so concurrent sessions serialize only their provisioning, not
    // their whole run.
    drop(provision_lock);

    // Step 7: shim wiring.
    if let Err(e) = ensure_shim_symlink(&home) {
        eprintln!("agentic-git: run: {e}");
        std::process::exit(1);
    }

    // Step 8: spawn.
    let code = spawn_agent(&parsed.cmd, &wt_path, &home, &agent, &real_git);

    // Step 9: teardown = keep. Print the session summary regardless of the
    // child's exit code — the work must outlive the agent.
    eprintln!(
        "\nagentic-git: session ended (exit {code}).\n  \
         worktree:  {}\n  \
         branch:    {branch}\n  \
         snapshots: pre-destructive-op safety net is ON by default in `run` sessions \
         (AGENTIC_GIT_SNAPSHOTS=0 to disable; `agentic-git snapshots list/prune` to inspect)\n  \
         re-enter:  cd {} && git status\n  \
         remove:    git -C {} worktree remove {} && rm -rf {}",
        wt_path.display(),
        wt_path.display(),
        source_repo.display(),
        wt_path.display(),
        home.join("runtime").join(&agent).display(),
    );
    std::process::exit(code);
}

#[cfg(test)]
mod tests;
