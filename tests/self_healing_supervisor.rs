//! End-to-end integration tests for the self-healing supervisor.
//!
//! Covers the two paths that code review flagged as untested: a successful
//! upgrade (accept → self-test → swap → ready → stabilise → commit) and a
//! rollback triggered by the new daemon crashing repeatedly inside the
//! stability window.
//!
//! The tests spawn the real `agend-supervisor` binary against a temp
//! `AGEND_HOME` and use the `agend-mock-daemon` helper binary (also built by
//! cargo) as the daemon child. Mock behaviour is driven by env + files so
//! the same executable stands in for both "v1" and "v2" — a v2 copy is
//! fabricated by appending padding bytes to change the sha256 without
//! altering behaviour.
//!
//! These tests are Unix-only (the whole upgrade path is).

#![cfg(unix)]

use agend_terminal::supervisor::{client, ipc, paths};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

// --- binary locations ------------------------------------------------------

fn target_bin_dir() -> PathBuf {
    // current_exe = target/debug/deps/self_healing_supervisor-<hash>
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // debug/
    p
}

fn supervisor_bin() -> PathBuf {
    target_bin_dir().join("agend-supervisor")
}

fn mock_daemon_bin() -> PathBuf {
    target_bin_dir().join("agend-mock-daemon")
}

// --- temp home -------------------------------------------------------------

fn temp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agend-sh-it-{}-{}-{}", std::process::id(), tag, id));
    std::fs::create_dir_all(&dir).expect("mkdir temp home");
    dir
}

/// Drop guard that SIGTERMs the supervisor (escalating to SIGKILL) and
/// wipes the home dir, so a failing assert doesn't leak processes or tmp
/// files.
struct TestEnv {
    home: PathBuf,
    supervisor: Child,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.supervisor.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.supervisor.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if matches!(self.supervisor.try_wait(), Ok(None)) {
            let _ = self.supervisor.kill();
            let _ = self.supervisor.wait();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

// --- setup helpers ---------------------------------------------------------

/// Stage the mock-daemon binary as v1 and point `bin/current` at it.
/// Returns the v1 sha256.
fn seed_initial_install(home: &Path) -> String {
    let hash = client::stage_binary(home, &mock_daemon_bin()).expect("stage v1");
    let bin = paths::bin_dir(home);
    std::fs::create_dir_all(&bin).expect("mkdir bin");
    let target = PathBuf::from("store").join(&hash);
    let link = paths::current_link(home);
    let _ = std::fs::remove_file(&link);
    symlink(&target, &link).expect("seed current symlink");
    hash
}

/// Fabricate a "v2" binary by copying mock-daemon and appending padding
/// bytes. Trailing bytes after ELF/Mach-O are ignored by the loader, so the
/// binary runs identically — only its sha256 differs.
fn make_v2_binary(home: &Path) -> PathBuf {
    let mut bytes = std::fs::read(mock_daemon_bin()).expect("read mock-daemon");
    bytes.extend_from_slice(b"\n# agend-mock-daemon v2 padding\n");
    let dest = home.join("mock-daemon-v2");
    std::fs::write(&dest, &bytes).expect("write v2");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).expect("chmod v2");
    dest
}

fn start_supervisor(home: &Path) -> Child {
    Command::new(supervisor_bin())
        .args(["--home", home.to_str().expect("home path utf-8")])
        // Clear AGEND_HOME from outer env so supervisor honours --home.
        .env_remove("AGEND_HOME")
        .env("AGEND_SUPERVISOR_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn agend-supervisor")
}

// --- polling helpers -------------------------------------------------------

fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn wait_for_supervisor_ready(home: &Path, timeout: Duration) -> bool {
    wait_until(|| matches!(client::probe(home), Ok(Some(_))), timeout)
}

fn count_ready_sentinels(home: &Path) -> usize {
    std::fs::read_dir(home)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("mock-ready-"))
                .count()
        })
        .unwrap_or(0)
}

fn clear_ready_sentinels(home: &Path) {
    if let Ok(entries) = std::fs::read_dir(home) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("mock-ready-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Shared prologue: seed v1, spawn supervisor, wait until v1 booted, stage v2
/// and swap the symlinks. Returns the env guard and both hashes.
fn setup_upgrade_scenario(tag: &str) -> (TestEnv, String, String) {
    let home = temp_home(tag);
    let v1_hash = seed_initial_install(&home);

    let env = TestEnv {
        home: home.clone(),
        supervisor: start_supervisor(&home),
    };

    assert!(
        wait_for_supervisor_ready(&env.home, Duration::from_secs(10)),
        "supervisor didn't answer Ping within 10s"
    );
    assert!(
        wait_until(
            || count_ready_sentinels(&env.home) >= 1,
            Duration::from_secs(10),
        ),
        "v1 daemon never wrote a ready-sentinel"
    );

    let v2_path = make_v2_binary(&env.home);
    let v2_hash = client::stage_binary(&env.home, &v2_path).expect("stage v2");
    client::swap_current(&env.home, &v2_hash, &v1_hash).expect("swap symlinks");

    (env, v1_hash, v2_hash)
}

// --- tests -----------------------------------------------------------------

#[test]
fn supervisor_upgrade_success_path() {
    let (env, v1_hash, v2_hash) = setup_upgrade_scenario("success");
    assert_ne!(v1_hash, v2_hash, "v1 and v2 must have distinct hashes");

    // Send the Upgrade request. Short stability window keeps the test fast;
    // it's still long enough to catch an immediate post-ready crash.
    let args = ipc::UpgradeArgs {
        new_hash: v2_hash.clone(),
        prev_hash: v1_hash.clone(),
        from_version: Some("v1".into()),
        to_version: Some("v2".into()),
        stability_secs: 2,
        ready_timeout_secs: 10,
    };
    clear_ready_sentinels(&env.home);
    let resp = client::send_upgrade(&env.home, args, |stage, msg| {
        eprintln!("[test success] stage={stage:?} {msg}");
    })
    .expect("send_upgrade");

    match resp {
        ipc::Response::Ok { r#final: true, .. } => {}
        other => panic!("expected terminal Ok, got {other:?}"),
    }

    // current now points at v2; prev at v1.
    let cur = std::fs::read_link(paths::current_link(&env.home)).expect("read current");
    assert_eq!(cur, PathBuf::from("store").join(&v2_hash));
    let prev = std::fs::read_link(paths::prev_link(&env.home)).expect("read prev");
    assert_eq!(prev, PathBuf::from("store").join(&v1_hash));

    // v2 actually booted — at least one new ready-sentinel appeared.
    assert!(
        count_ready_sentinels(&env.home) >= 1,
        "v2 daemon never wrote a ready-sentinel after upgrade"
    );
}

#[test]
fn supervisor_upgrade_rolls_back_on_repeated_crash() {
    let (env, v1_hash, v2_hash) = setup_upgrade_scenario("rollback");

    // Arm the crash counter BEFORE sending Upgrade. The mock-daemon
    // decrements it once per boot; two boots = two post-ready crashes,
    // which is the stability window's failure threshold (>=2).
    std::fs::write(env.home.join("mock-crashes-remaining"), "2").expect("write crash counter");
    clear_ready_sentinels(&env.home);

    let args = ipc::UpgradeArgs {
        new_hash: v2_hash.clone(),
        prev_hash: v1_hash.clone(),
        from_version: Some("v1".into()),
        to_version: Some("v2".into()),
        stability_secs: 10,
        ready_timeout_secs: 10,
    };
    let resp = client::send_upgrade(&env.home, args, |stage, msg| {
        eprintln!("[test rollback] stage={stage:?} {msg}");
    })
    .expect("send_upgrade");

    match resp {
        ipc::Response::Err { ref error, .. } => {
            assert!(
                error.contains("rolled back") || error.contains("unstable"),
                "expected rollback error, got: {error}"
            );
        }
        other => panic!("expected Err, got {other:?}"),
    }

    // Supervisor should have repointed current → store/<v1>.
    let cur = std::fs::read_link(paths::current_link(&env.home)).expect("read current");
    assert_eq!(
        cur,
        PathBuf::from("store").join(&v1_hash),
        "rollback did not repoint current symlink"
    );

    // Upgrade marker must be deleted on rollback so a later unrelated
    // crash-respawn doesn't misreport as "daemon upgraded".
    assert!(
        !paths::upgrade_marker(&env.home).exists(),
        "upgrade marker should be deleted after rollback"
    );

    // The respawned v1 daemon should eventually write a new ready sentinel.
    assert!(
        wait_until(
            || count_ready_sentinels(&env.home) >= 1,
            Duration::from_secs(10),
        ),
        "post-rollback v1 daemon never wrote a ready-sentinel"
    );

    // Crash counter was fully consumed by v2 — v1's post-rollback boot must
    // not have crashed on it.
    let remaining = std::fs::read_to_string(env.home.join("mock-crashes-remaining"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    assert_eq!(
        remaining, "0",
        "expected crash counter fully consumed; got {remaining:?}"
    );
}
