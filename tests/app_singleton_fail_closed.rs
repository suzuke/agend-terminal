//! Split-brain regression (2026-07-21 incident): `agend-terminal app` must
//! REFUSE to boot when another process already holds the daemon singleton flock.
//!
//! Before the fix, `setup_app_bootstrap` funnelled every `bootstrap::prepare`
//! failure — including "another daemon holds `.daemon.lock`" — into a single
//! degraded arm that logged a warning and carried on with `attached_run_dir ==
//! None`. `run_app` reads that as **Owned** mode, so the app then spawned its own
//! copy of every fleet instance while the real daemon was still running: two live
//! sessions per agent name, memory dirs cross-written by duplicate identities,
//! and dispatch replies answered by whichever lead won the race.
//!
//! §3.20 SOP 1 — this reproduction is deterministic and timing-independent. The
//! test process itself holds the flock via `flock(LOCK_EX|LOCK_NB)`, so there is
//! no daemon to start, no race to lose and no window to hit: the contention is
//! already true before `app` is executed. The only bounded wait is on the child's
//! exit, polled (never `sleep(N)`-ed) per CONTRIBUTING.md.
//!
//! Unix-only: the reproduction needs a pty (the TUI refuses a non-tty stdio, so
//! a plain pipe would exit for the WRONG reason and the test would pass
//! vacuously).

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("agend-terminal")
}

fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-app-singleton-{}-{}",
        std::process::id(),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("mkdir temp home");
    dir
}

/// One shell instance: if the guard ever regresses and the app boots Owned, the
/// spawn is cheap — but the test still fails, because a booted TUI does not exit.
fn write_fleet(home: &Path) -> PathBuf {
    let path = home.join("fleet.yaml");
    std::fs::write(
        &path,
        "defaults:\n  backend: shell\ninstances:\n  probe:\n    backend: shell\n    command: /bin/sh\n",
    )
    .expect("write fleet.yaml");
    path
}

/// Take the daemon singleton flock and keep it for the returned handle's life.
///
/// This is the same inode `bootstrap::acquire_daemon_lock` targets, so the child
/// sees genuine contention rather than a simulated one.
fn hold_daemon_lock(home: &Path) -> File {
    let file = File::create(home.join(".daemon.lock")).expect("create .daemon.lock");
    // SAFETY: `file` owns a valid fd for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        rc, 0,
        "test harness must own the daemon lock to be meaningful"
    );
    file
}

/// Run `agend-terminal app` with its stdio wired to a real pty slave, so
/// `ratatui::init` / crossterm raw-mode succeed headlessly and the app gets far
/// enough to reach the singleton check.
fn boot_app_under_pty(home: &Path) -> Child {
    let mut winsize = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: openpty fills master+slave with fresh valid fds on success (rc==0).
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut::<libc::termios>(),
            &mut winsize as *mut libc::winsize,
        )
    };
    assert_eq!(rc, 0, "openpty must succeed");

    let mut cmd = Command::new(bin());
    cmd.env("AGEND_HOME", home)
        // Strip ambient fleet env: a stray AGEND_SUCCESSOR_HANDOFF would send the
        // process down the #1814 handoff path, which deliberately defers the
        // flock and would make this test assert the wrong thing.
        .env_remove("AGEND_SUCCESSOR_HANDOFF")
        .env_remove("AGEND_RESTART_HANDOFF")
        .env_remove("AGEND_WRAPPED")
        .env_remove("AGEND_SUPERVISED")
        .env_remove("AGEND_INSTANCE_NAME")
        .arg("app");
    // SAFETY: dup the slave fd for each std stream; std::process owns + closes
    // the dup'd fds, so the child's stdio is a real tty.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave)));
    }
    let child = cmd.spawn().expect("app must spawn under pty");
    // SAFETY: slave is a valid fd we opened above; the child holds its own dups.
    unsafe {
        libc::close(slave);
    }

    // Drain the master so the TUI's writes never block on a full pty buffer.
    // Detached: ends on EOF when the child exits, or on test-process exit.
    std::thread::spawn(move || {
        // SAFETY: master is a valid fd owned here; the File closes it on drop.
        let mut f = unsafe { File::from_raw_fd(master) };
        let mut buf = [0u8; 4096];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    child
}

/// Poll for exit up to `budget`. Returns `None` if the child is still running —
/// which is itself the regression signal: a booted TUI never exits on its own.
fn wait_for_exit(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            // Poll interval, not a sleep-as-synchronisation wait: the exit is
            // observed the moment it happens, the interval only bounds spin.
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    None
}

#[test]
fn app_refuses_to_boot_while_daemon_lock_is_held() {
    let home = tmp_home("held");
    write_fleet(&home);
    let _lock = hold_daemon_lock(&home);

    let mut child = boot_app_under_pty(&home);
    let status = wait_for_exit(&mut child, Duration::from_secs(30));

    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            std::fs::remove_dir_all(&home).ok();
            panic!(
                "`app` kept running while another process held .daemon.lock — it \
                 degraded to Owned mode and is spawning a second fleet (split-brain)"
            );
        }
    };

    assert!(
        !status.success(),
        "`app` must exit non-zero when it loses the singleton lock; got {status:?}"
    );

    // No run dir may be published: publishing one would let a later `app`/`start`
    // attach to a daemon that does not exist.
    let run = home.join("run");
    let published: Vec<_> = std::fs::read_dir(&run)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        published.is_empty(),
        "a refused app must not publish a run dir; found {published:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Control: with the lock free, the very same invocation gets PAST the singleton
/// check. Without this, the test above would still pass if `app` were broken in
/// some unrelated way that made it exit early for every reason.
///
/// Asserts only that the app does NOT die the way the refusal path dies — it is
/// left running (a healthy TUI does not exit) and then killed. This is what makes
/// the pair a real RED/GREEN discriminator rather than a one-sided assertion.
#[test]
fn app_proceeds_past_singleton_check_when_lock_is_free() {
    let home = tmp_home("free");
    write_fleet(&home);
    // Deliberately NO lock held.

    let mut child = boot_app_under_pty(&home);
    let early_exit = wait_for_exit(&mut child, Duration::from_secs(10));

    let still_running = early_exit.is_none();
    let _ = child.kill();
    let _ = child.wait();
    std::fs::remove_dir_all(&home).ok();

    assert!(
        still_running,
        "with the lock free the app must get past the singleton check and keep \
         running; it exited early ({early_exit:?}) — the refusal in the sibling \
         test would then prove nothing"
    );
}
