//! agend-git — KILL-family footgun-guard shim (kill-only residual, #2524 P3b).
//!
//! HISTORY: this binary was formerly the transparent git shim. As of the
//! agentic-git migration (#2524 P3), git interception moved to the vendored
//! `agentic-git` binary — the `git` PATH-shadow points there when the migration
//! flag is on (binding/shim_install.rs::symlink_shim). This binary survives ONLY
//! as the kill-family guard (pkill/killall/kill), multiplexed via argv[0] by
//! `symlink_shim`. It NO LONGER intercepts git at all: any non-kill invocation
//! fails closed (loud, non-zero exit) instead of touching real git.
//!
//! Kill dispatch + guards live in `kill_guard`; the `same_dir` / `lexical_path_eq`
//! / `parent_pid` helpers below are the shim-local surface `kill_guard` depends on
//! (via `crate::`), kept here so the kill guard stays self-contained.

use std::env;

// #t-…777-1: kill-family footgun-guard (pkill/killall/kill) — dispatched on
// argv[0] basename via the PATH-shadow symlinks (binding/shim_install.rs). This is
// the ONLY live behavior of this binary post-P3b.
#[path = "agend-git/kill_guard.rs"]
mod kill_guard;

fn main() {
    // #t-…777-1: kill-family shim dispatch. When invoked via the pkill/killall/kill
    // PATH-shadow symlink (binding/shim_install.rs::symlink_shim), argv[0]'s basename
    // routes here and NEVER returns (deny+exit(1) or exec the real kill binary).
    if let Some(tool) = kill_guard::shim_tool(&env::args().next().unwrap_or_default()) {
        let kargs: Vec<String> = env::args().skip(1).collect();
        kill_guard::run(tool, &kargs); // -> ! : deny+exit(1) or exec the real binary
    }

    // #2524 P3b fail-closed: agend-git no longer intercepts git. The `git`
    // PATH-shadow points at the vendored `agentic-git` when the migration flag is
    // on; the only ways execution reaches HERE with a non-kill argv[0] are (a) the
    // flag is OFF, or (b) the agentic-git binary was missing at install so
    // `symlink_shim` fell `git` back onto this binary (shim_install.rs fail-safe).
    // Either way this binary can no longer guard git, so we REFUSE LOUDLY with a
    // non-zero exit rather than silently exec real git (which would bypass ALL
    // agentic-git enforcement). Every agent may hit this wall, so the message is
    // the operator's first-line diagnosis (actionable breadcrumb per lead vet).
    let argv0 = env::args().next().unwrap_or_default();
    let rest: Vec<String> = env::args().skip(1).collect();
    eprintln!(
        "agend-git: FATAL this binary no longer serves git (P3b). Expected route for git is \
         `agentic-git` (the vendored shim). Execution reached agend-git for a git-style \
         invocation (argv0={argv0:?}, args={rest:?}) — this means the use_agentic_git_shim flag \
         is OFF or the agentic-git binary is missing next to the daemon exe. Fix: check the \
         vendor/agentic-git build (`cargo build` produces the `agentic-git` [[bin]] target) or \
         revert the P3b commit. agend-git now guards only the kill family (pkill/killall/kill)."
    );
    std::process::exit(70); // EX_SOFTWARE — fail closed, never passthrough to real git.
}

// ── Path helpers used by `kill_guard` via `crate::` (kept from the pre-P3b file
//    verbatim so the kill guard's dedup/self-exclusion behavior is unchanged) ──

fn same_dir(a: &std::path::Path, b: Option<&std::path::Path>) -> bool {
    let Some(b) = b else { return false };
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => lexical_path_eq(a, b),
    }
}

/// Lexical directory equality: normalize backslashes to `/`, strip trailing
/// separators, compare case-insensitively on Windows. Fallback only.
fn lexical_path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
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

// ── Parent-pid lookup used by `kill_guard` via `crate::` (the daemon-pid guard
//    forensics). Unix-only; other platforms report -1 (best-effort). ──

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parent_pid() -> i32 {
    unsafe { libc::getppid() }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid() -> i32 {
    -1
}
