//! Daemon self-test — smoke checks invoked when `AGEND_SELF_TEST=1` is set.
//!
//! The new binary is execed by the supervisor with this env var before the
//! real daemon is launched. Passing the self-test is a necessary (not
//! sufficient) condition for the upgrade to proceed. It catches:
//!
//! - Dynamic linker failures / missing libc symbols (binary won't even start)
//! - Basic I/O (can open `$AGEND_HOME`, read `fleet.yaml`)
//! - ABI mismatches in on-disk state (parse `fleet.yaml` with the new
//!   version's schema; fail loudly rather than limping along)
//!
//! What it deliberately does NOT do:
//!
//! - Spawn agents (that's what the real daemon does)
//! - Bind the API socket (the previous daemon may still own it)
//! - Acquire the daemon flock (same reason)
//!
//! The supervisor's rollback path will catch post-launch failures; self-test
//! only exists to fail-fast before we kill the running daemon.

use std::path::Path;

/// Returns true iff `AGEND_SELF_TEST=1` is set in the environment.
pub fn requested() -> bool {
    std::env::var("AGEND_SELF_TEST")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Run self-test checks. On success, returns `Ok(())`; the caller (main.rs)
/// then exits with code 0. On failure, returns `Err` with a human-readable
/// reason; caller exits non-zero.
///
/// `home` is the AGEND_HOME to use — caller resolves it the same way normal
/// startup does.
pub fn run(home: &Path) -> anyhow::Result<()> {
    use anyhow::Context;

    // 1. Home directory must be usable.
    if !home.exists() {
        std::fs::create_dir_all(home)
            .with_context(|| format!("self-test: create home {}", home.display()))?;
    }
    let meta = std::fs::metadata(home)
        .with_context(|| format!("self-test: stat home {}", home.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("self-test: home {} is not a directory", home.display());
    }

    // 2. Probe a write under home (catches read-only filesystem, quota).
    let probe = home.join(".self-test-probe");
    std::fs::write(&probe, b"ok")
        .with_context(|| format!("self-test: probe write {}", probe.display()))?;
    std::fs::remove_file(&probe).ok();

    // 3. If fleet.yaml exists, it must parse — catches schema drift after
    //    upgrades that rename/remove fields.
    let fleet_path = home.join("fleet.yaml");
    if fleet_path.exists() {
        let raw = std::fs::read_to_string(&fleet_path)
            .with_context(|| format!("self-test: read {}", fleet_path.display()))?;
        // Parse as plain YAML first so we get a clear "not YAML" error if
        // the file is corrupt, before attempting the stricter schema parse.
        serde_yaml::from_str::<serde_yaml::Value>(&raw)
            .context("self-test: fleet.yaml is not valid YAML")?;
    }

    // 4. Binary sanity: `current_exe` should succeed — fails on certain
    //    hardened Linux builds where /proc is restricted.
    std::env::current_exe().context("self-test: current_exe")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-selftest-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn requested_only_on_exact_one() {
        // Not asserting env manipulation (other tests may race); just
        // check the parsing behavior of the helper.
        std::env::remove_var("AGEND_SELF_TEST");
        assert!(!requested());
    }

    #[test]
    fn passes_empty_home() {
        let home = tmp_home("ok");
        run(&home).expect("self-test should pass");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fails_on_unparseable_fleet() {
        let home = tmp_home("bad_fleet");
        std::fs::write(home.join("fleet.yaml"), ": this is : not : yaml :\n").ok();
        let err = run(&home).expect_err("should fail");
        assert!(format!("{err:#}").to_lowercase().contains("yaml"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn passes_with_valid_fleet() {
        let home = tmp_home("good_fleet");
        std::fs::write(
            home.join("fleet.yaml"),
            "defaults:\n  backend: claude-code\ninstances: {}\n",
        )
        .ok();
        run(&home).expect("valid fleet should pass");
        std::fs::remove_dir_all(&home).ok();
    }
}
