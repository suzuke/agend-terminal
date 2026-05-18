//! Recursion-guard for `agend-terminal`-spawns-`agend-terminal`.
//!
//! Tracks how deep into a self-spawn chain we are via the
//! `AGEND_SPAWN_DEPTH` env var. Every legitimate spawn site reads the
//! current depth, bails if it has reached [`THRESHOLD`], and otherwise
//! sets `current + 1` on the child's env.
//!
//! Why: #882 shipped a fork-bomb on cold start because `spawn_detached`'s
//! child re-entered main's `Start` arm without `--foreground`, recursively
//! calling `spawn_detached` again. 800f1cc + 446b9ed plugged the two known
//! callers via `--foreground`, but those are content-fixes; the guard here
//! is the structural fix that catches any future regression at any new
//! spawn site.
//!
//! Threshold rationale (from #879v3 E2 spike): the deepest legitimate
//! agend-spawns-agend chain is parent (app | tray | OS service) → daemon
//! (`start --foreground`, no further self-spawn). That tops out at depth 1.
//! Threshold = 2 gives one frame of headroom for that legitimate case
//! and hard-stops the grandchild that would mark a fork bomb.

use anyhow::{bail, Result};

/// Env var name the guard reads/writes.
pub const ENV_KEY: &str = "AGEND_SPAWN_DEPTH";

/// Depth at which a self-spawn must bail. See module doc for rationale.
pub const THRESHOLD: u8 = 2;

/// Current depth as observed from the process env. Missing or malformed
/// values are treated as 0 — defense-in-depth so an attacker / accidental
/// env-poisoning that sets `AGEND_SPAWN_DEPTH=abc` does not silently
/// disable the guard.
pub fn current() -> u8 {
    std::env::var(ENV_KEY)
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0)
}

/// Check guard at a spawn site or Start-arm entry. Returns the depth value
/// the child should run at (`current + 1`) on the legitimate path, or an
/// `Err` carrying the fork-bomb-signature message if depth has reached
/// [`THRESHOLD`].
pub fn check() -> Result<u8> {
    let depth = current();
    if depth >= THRESHOLD {
        bail!(
            "AGEND_SPAWN_DEPTH={depth} reached threshold {THRESHOLD} — \
             refusing recursive self-spawn (fork-bomb guard, see #882 RCA). \
             If you reached this legitimately, that means a new agend-spawns-agend \
             code path needs a different mechanism; do not raise the threshold."
        );
    }
    Ok(depth + 1)
}

/// Apply `next_depth` to a child [`std::process::Command`]'s env. Pairs
/// with [`check`] — the typical sequence is
/// `let next = check()?; cmd.env(ENV_KEY, next.to_string());`
pub fn set_on_child(cmd: &mut std::process::Command, next_depth: u8) {
    cmd.env(ENV_KEY, next_depth.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env mutations across tests in this module — `std::env::set_var`
    // is process-global, and cargo runs tests in the same process in parallel
    // by default. A mutex (not `serial_test`) keeps this module self-contained.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<R>(value: Option<&str>, body: impl FnOnce() -> R) -> R {
        // PoisonError is recoverable here — a poisoned lock means a prior
        // test panicked while holding it. We restore env after the body
        // regardless, so taking the value through poison is safe.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(ENV_KEY).ok();
        match value {
            Some(v) => std::env::set_var(ENV_KEY, v),
            None => std::env::remove_var(ENV_KEY),
        }
        let out = body();
        match prior {
            Some(p) => std::env::set_var(ENV_KEY, p),
            None => std::env::remove_var(ENV_KEY),
        }
        out
    }

    #[test]
    fn unset_env_starts_at_zero_and_child_gets_one() {
        with_env(None, || {
            assert_eq!(current(), 0);
            let next = check().expect("legit first spawn");
            assert_eq!(next, 1, "child env AGEND_SPAWN_DEPTH should be 1");
        });
    }

    #[test]
    fn depth_zero_explicit_allows_legit_first_spawn() {
        with_env(Some("0"), || {
            let next = check().expect("legit");
            assert_eq!(next, 1);
        });
    }

    #[test]
    fn depth_one_allows_legit_daemon_layer() {
        // Daemon child sees AGEND_SPAWN_DEPTH=1 (set by parent). If the
        // daemon entry attempts a further self-spawn (the pre-hotfix #882
        // path), the guard allows one more frame; the grandchild then
        // bails. Threshold=2 leaves exactly this frame of headroom.
        with_env(Some("1"), || {
            let next = check().expect("daemon layer allowed");
            assert_eq!(next, 2);
        });
    }

    /// LOAD-BEARING — proves the fork-bomb guard fires deterministically at
    /// the boundary that mattered for #882. Reviewer §3.20 SOP 3 RED protocol
    /// will revert the `check()` body to `Ok(0)` and observe this test FAIL,
    /// then re-apply and observe PASS.
    #[test]
    fn depth_two_bails_recursive_fork_bomb() {
        with_env(Some("2"), || {
            let err = check().expect_err("must bail at threshold");
            let msg = format!("{err}");
            assert!(
                msg.contains("AGEND_SPAWN_DEPTH=2"),
                "error must surface the observed depth (got: {msg})"
            );
            assert!(
                msg.contains("threshold 2"),
                "error must surface the threshold (got: {msg})"
            );
            assert!(
                msg.contains("fork-bomb guard"),
                "error must reference the #882 RCA (got: {msg})"
            );
        });
    }

    #[test]
    fn malformed_env_treated_as_unset() {
        // Defense-in-depth: an attacker / accidental env-poisoning that
        // sets `AGEND_SPAWN_DEPTH=abc` MUST NOT silently disable the
        // guard. Parse failure → 0 → legitimate first spawn allowed.
        with_env(Some("not-a-number"), || {
            assert_eq!(current(), 0, "malformed env defaults to 0, not to ∞");
            let next = check().expect("malformed = treat as 0");
            assert_eq!(next, 1);
        });
    }

    #[test]
    fn high_value_above_threshold_bails() {
        with_env(Some("99"), || {
            let err = check().expect_err("any depth >= threshold must bail");
            assert!(format!("{err}").contains("AGEND_SPAWN_DEPTH=99"));
        });
    }

    #[test]
    fn set_on_child_writes_env_var() {
        let mut cmd = std::process::Command::new("/bin/true");
        set_on_child(&mut cmd, 1);
        // Command's get_envs is the only way to introspect; check the entry
        // we just set is present with the expected stringified value.
        let found = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(ENV_KEY))
            .and_then(|(_, v)| v.map(|os| os.to_string_lossy().into_owned()));
        assert_eq!(found.as_deref(), Some("1"));
    }
}
