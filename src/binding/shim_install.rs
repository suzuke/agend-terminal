//! Git/kill PATH-shim installation for `$AGEND_HOME/bin` (#2524 P2).
//!
//! Homed out of `binding.rs` so the flag-gated git-shim backend-swap logic and
//! its unit tests stay off that core file's anti-monolith LOC budget. The public
//! entry [`symlink_shim`] is re-exported from `crate::binding`, so callers and
//! the app-mode wiring invariant keep referring to `binding::symlink_shim`.

use std::path::{Path, PathBuf};

/// Symlink the guard shim binaries into `$AGEND_HOME/bin` under every name they
/// shadow (so the shims win over the real tools via PATH). Called at daemon
/// startup. #t-…777-1: the binaries are MULTIPLEXED on argv[0] — `git` = git
/// shim; `pkill`/`killall`/`kill` = kill_guard.rs.
///
/// #2524 P2 (agentic-git migration, design §5-Phase-2): the `git` name is
/// FLAG-GATED. When `use_agentic_git` is true and the self-built `agentic-git`
/// sibling exists, `git` points at it (the vendored successor shim, whose git
/// guard matrix is parity-equivalent to agend-git's); otherwise `git` stays on
/// `agend-git`. The `{pkill,killall,kill}` kill-shim links ALWAYS point at
/// `agend-git` (D1=A — agentic-git is git-only and has no process-kill guard,
/// so it must never shadow the kill family). Rollback = flip the flag back to
/// false + restart; the on-disk binding format is unchanged, so a rolled-back
/// agend-git shim still verifies core-signed bindings (no rebind storm).
pub fn symlink_shim(home: &Path, use_agentic_git: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    symlink_shim_at(home, &exe, use_agentic_git);
}

/// Testable core of [`symlink_shim`]: the daemon-exe path is injected so unit
/// tests can point it at a fixture directory holding fake sibling binaries,
/// instead of the real `current_exe()`.
fn symlink_shim_at(home: &Path, exe: &Path, use_agentic_git: bool) {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).ok();

    // Resolve a self-built sibling binary next to the daemon exe, .exists()-gated.
    let sibling = |stem: &str| -> Option<PathBuf> {
        let name = if cfg!(windows) {
            format!("{stem}.exe")
        } else {
            stem.to_string()
        };
        let candidate = exe.with_file_name(name);
        candidate.exists().then_some(candidate)
    };

    // agend-git is the mandatory floor: it owns the kill family AND is the git
    // default/fallback. Absent → nothing to install (byte-identical to the
    // pre-P2 early-return when the shim binary was missing).
    let Some(agend_git) = sibling("agend-git") else {
        return;
    };

    // Flag-gated git target. Fail-SAFE: flag on but the agentic-git sibling
    // missing (build-together did not ship it) → fall back to agend-git so `git`
    // is NEVER left unguarded, and WARN loudly so the drift is diagnosable.
    let git_src = if use_agentic_git {
        match sibling("agentic-git") {
            Some(agentic) => agentic,
            None => {
                tracing::warn!(
                    "use_agentic_git_shim=true but the agentic-git binary is missing next \
                     to the daemon exe; falling back to agend-git for the git shim (run the \
                     build-together step). git stays guarded."
                );
                agend_git.clone()
            }
        }
    } else {
        agend_git.clone()
    };

    let git_name = if cfg!(windows) { "git.exe" } else { "git" };
    let kill_names: &[&str] = if cfg!(windows) {
        &["pkill.exe", "killall.exe", "kill.exe"]
    } else {
        &["pkill", "killall", "kill"]
    };

    // (link_name, source): `git` → flag-selected; kill family → always agend-git.
    let mut links: Vec<(&str, &Path)> = vec![(git_name, git_src.as_path())];
    for name in kill_names {
        links.push((name, agend_git.as_path()));
    }

    for (name, src) in links {
        let link_path = bin_dir.join(name);
        // Remove stale symlink/file first.
        let _ = std::fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(src, &link_path);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::copy(src, &link_path);
        }
    }
}

// The target-resolution assertions read symlink targets, which is a unix
// semantic (the `cfg(not(unix))` path uses `fs::copy` instead). Windows shim
// wiring is advisory-only in this repo; parity there is covered by the
// source-inspection + build tests in `tests/agentic_git_shim_swap.rs`.
#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Build a fixture: an exe dir containing the requested fake sibling
    /// binaries plus a separate empty home. Returns `(home, fake_exe_path)`.
    fn fixture(tag: &str, siblings: &[&str]) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("agend-p2-shim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let exe_dir = base.join("exe");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        for s in siblings {
            std::fs::write(exe_dir.join(s), b"#!/bin/sh\n").unwrap();
        }
        (base.join("home"), exe_dir.join("agend-terminal"))
    }

    fn target_of(home: &Path, name: &str) -> PathBuf {
        std::fs::read_link(home.join("bin").join(name)).unwrap()
    }

    #[test]
    fn flag_off_git_points_agend_git() {
        let (home, exe) = fixture("off", &["agend-git", "agentic-git"]);
        symlink_shim_at(&home, &exe, false);
        let agend = exe.with_file_name("agend-git");
        assert_eq!(target_of(&home, "git"), agend, "flag off: git → agend-git");
        for n in ["pkill", "killall", "kill"] {
            assert_eq!(target_of(&home, n), agend, "kill family → agend-git");
        }
    }

    #[test]
    fn flag_on_git_points_agentic_git() {
        // The P2 swap: flag on + both siblings present → git retargets to
        // agentic-git while the kill family stays on agend-git. Pre-P2 this
        // function always pointed git → agend-git (the RED baseline).
        let (home, exe) = fixture("on", &["agend-git", "agentic-git"]);
        symlink_shim_at(&home, &exe, true);
        assert_eq!(
            target_of(&home, "git"),
            exe.with_file_name("agentic-git"),
            "flag on: git → agentic-git"
        );
        let agend = exe.with_file_name("agend-git");
        for n in ["pkill", "killall", "kill"] {
            assert_eq!(
                target_of(&home, n),
                agend,
                "D1=A: kill family stays on agend-git even with the flag on"
            );
        }
    }

    #[test]
    fn flag_on_missing_agentic_falls_back_to_agend_git() {
        // Fail-safe: flag on but agentic-git not shipped → git must NOT be left
        // unguarded; it falls back to the agend-git floor.
        let (home, exe) = fixture("fallback", &["agend-git"]);
        symlink_shim_at(&home, &exe, true);
        let agend = exe.with_file_name("agend-git");
        assert_eq!(target_of(&home, "git"), agend, "fallback: git → agend-git");
        for n in ["pkill", "killall", "kill"] {
            assert_eq!(target_of(&home, n), agend);
        }
    }

    #[test]
    fn no_agend_git_floor_installs_nothing() {
        // No kill-shim floor present → install nothing (pre-P2 early-return),
        // even though an agentic-git sibling exists.
        let (home, exe) = fixture("noop", &["agentic-git"]);
        symlink_shim_at(&home, &exe, true);
        assert!(
            !home.join("bin").join("git").exists(),
            "no agend-git floor → no git link installed"
        );
        assert!(!home.join("bin").join("pkill").exists());
    }
}
