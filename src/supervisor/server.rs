//! Supervisor server — the main loop of the `agend-supervisor` binary.
//!
//! # Responsibilities
//!
//! 1. **Hold the daemon child alive.** Spawn `$AGEND_HOME/bin/current`,
//!    watch for exit, respawn on crash (with bounded backoff).
//! 2. **Handle upgrade requests.** Coordinate: self-test → stop daemon →
//!    start new daemon → watch for ready ping → observe stability window
//!    → roll back on failure.
//! 3. **Be tiny and stable.** No teloxide, no ratatui, no PTY work. The
//!    supervisor is the one component we commit to not hot-upgrading; it
//!    should change rarely enough that crashes here are near-zero.
//!
//! # Threading & event model
//!
//! Single main thread owns `state` and drives the event loop. Helper
//! threads:
//!
//! - `supervisor-accept` — blocks on `UnixListener::accept`, forwards
//!   streams to the main loop via `ev_tx`.
//! - `supervisor-watch-<pid>` — polls `kill(pid, 0)` for the current
//!   daemon child and emits `Event::Exited` when it goes away.
//!
//! Upgrade orchestration runs on the main thread too (it borrows `state`
//! mutably). During an upgrade we keep draining `ev_rx` inline so Ready
//! pings and Exited events still flow; new Upgrade requests are rejected
//! while `upgrade_in_progress` is set.

#![cfg(unix)]

use super::ipc::{self, Request, Response, UpgradeArgs, UpgradeStage};
use super::paths;
use anyhow::{Context, Result};
use crossbeam::channel::{Receiver, Sender};
use std::collections::VecDeque;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Max child restarts we'll attempt in `RESTART_WINDOW` before giving up.
const RESTART_WINDOW: Duration = Duration::from_secs(120);
const MAX_RESTARTS_IN_WINDOW: usize = 5;
/// Grace period we give the daemon to exit cleanly on SIGTERM before we
/// escalate to SIGKILL. The daemon's own graceful shutdown gives itself
/// ~5s for child cleanup, so 10s is comfortable slack.
const DAEMON_STOP_GRACE: Duration = Duration::from_secs(10);

/// Entry point for the `agend-supervisor` binary.
///
/// `home` is resolved by the caller (mirrors the main binary's logic so
/// `AGEND_HOME` wins over user-home fallback).
pub fn run(home: &Path) -> Result<()> {
    std::fs::create_dir_all(paths::run_root(home))
        .with_context(|| format!("create run dir {}", paths::run_root(home).display()))?;
    std::fs::create_dir_all(paths::bin_dir(home))
        .with_context(|| format!("create bin dir {}", paths::bin_dir(home).display()))?;

    let current = paths::current_link(home);
    if !current.exists() {
        anyhow::bail!(
            "no daemon binary staged at {} — run `agend-terminal upgrade --install-supervisor` first",
            current.display()
        );
    }

    // Write pid file so CLI can probe for a live supervisor without opening
    // the socket. Removed on clean exit.
    let pid_file = paths::supervisor_pid_file(home);
    std::fs::write(&pid_file, std::process::id().to_string())
        .with_context(|| format!("write pid file {}", pid_file.display()))?;

    // Bind UDS socket. Stale file is cleaned by the bind helper.
    let sock_path = paths::supervisor_sock(home);
    let listener = ipc::uds::bind(&sock_path)
        .with_context(|| format!("bind supervisor socket {}", sock_path.display()))?;
    listener
        .set_nonblocking(false)
        .context("set listener blocking")?;
    tracing::info!(socket = %sock_path.display(), pid = std::process::id(), "supervisor listening");

    // Shutdown flag — flipped by signal handler.
    let shutdown = Arc::new(AtomicBool::new(false));
    install_signals(Arc::clone(&shutdown));

    // Event channel from helper threads into main loop.
    let (ev_tx, ev_rx) = crossbeam::channel::unbounded::<Event>();

    // Accept thread.
    {
        let ev_tx = ev_tx.clone();
        let shutdown = Arc::clone(&shutdown);
        thread::Builder::new()
            .name("supervisor-accept".into())
            .spawn(move || accept_loop(listener, ev_tx, shutdown))
            .context("spawn accept thread")?;
    }

    // Spawn initial daemon child.
    let mut state = SupervisorState {
        home: home.to_path_buf(),
        child: None,
        current_version: read_version_of(&current).unwrap_or_else(|_| "(unknown)".into()),
        restarts: VecDeque::new(),
        upgrade_in_progress: false,
        last_ready: None,
    };
    if let Err(e) = state.spawn_daemon(&ev_tx) {
        tracing::error!(error = %e, "initial daemon spawn failed");
        cleanup(&sock_path, &pid_file);
        return Err(e);
    }

    // Main loop.
    let result = main_loop(&mut state, &ev_rx, &ev_tx, &shutdown);

    // Cleanup on exit.
    if let Some(ref mut child) = state.child {
        tracing::info!(pid = child.id(), "supervisor exiting — stopping daemon");
        stop_child(child, DAEMON_STOP_GRACE);
    }
    cleanup(&sock_path, &pid_file);
    result
}

struct SupervisorState {
    home: PathBuf,
    child: Option<Child>,
    current_version: String,
    restarts: VecDeque<Instant>,
    upgrade_in_progress: bool,
    /// When the daemon last sent a Ready ping. Used by the upgrade path to
    /// confirm the new binary reached steady state.
    last_ready: Option<Instant>,
}

impl SupervisorState {
    fn spawn_daemon(&mut self, ev_tx: &Sender<Event>) -> Result<()> {
        let current = paths::current_link(&self.home);
        tracing::info!(binary = %current.display(), "spawning daemon");
        let mut cmd = Command::new(&current);
        cmd.arg("start")
            .env(
                "AGEND_SUPERVISOR_SOCK",
                paths::supervisor_sock(&self.home),
            )
            .env("AGEND_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn daemon {}", current.display()))?;
        let pid = child.id();
        self.child = Some(child);
        self.record_restart();

        // Watcher thread — polls pid liveness and emits Exited events.
        let ev_tx = ev_tx.clone();
        thread::Builder::new()
            .name(format!("supervisor-watch-{pid}"))
            .spawn(move || wait_for_child(pid, ev_tx))
            .context("spawn watcher thread")?;

        Ok(())
    }

    fn record_restart(&mut self) {
        let now = Instant::now();
        self.restarts.push_back(now);
        while let Some(&front) = self.restarts.front() {
            if now.duration_since(front) > RESTART_WINDOW {
                self.restarts.pop_front();
            } else {
                break;
            }
        }
    }

    fn restart_budget_exhausted(&self) -> bool {
        self.restarts.len() > MAX_RESTARTS_IN_WINDOW
    }
}

enum Event {
    /// New IPC connection accepted from the UDS listener.
    Connection(UnixStream),
    /// Daemon child exited.
    Exited { pid: u32, detail: String },
}

fn main_loop(
    state: &mut SupervisorState,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                dispatch_event(state, ev, ev_rx, ev_tx)?;
                if state.restart_budget_exhausted() {
                    tracing::error!(
                        restarts = state.restarts.len(),
                        "daemon keeps crashing — supervisor giving up"
                    );
                    return Err(anyhow::anyhow!(
                        "daemon crash-looped beyond supervisor budget"
                    ));
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                tracing::warn!("supervisor event channel disconnected");
                return Ok(());
            }
        }
    }
}

/// Top-level event dispatcher. Shared between `main_loop` and the drain
/// loops inside upgrade orchestration so behaviour is identical no matter
/// which loop observed the event.
fn dispatch_event(
    state: &mut SupervisorState,
    ev: Event,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
) -> Result<()> {
    match ev {
        Event::Connection(stream) => {
            if let Err(e) = handle_connection(state, stream, ev_rx, ev_tx) {
                tracing::warn!(error = %e, "supervisor IPC handler error");
            }
        }
        Event::Exited { pid, detail } => {
            handle_child_exit(state, pid, &detail, ev_tx);
        }
    }
    Ok(())
}

fn handle_child_exit(
    state: &mut SupervisorState,
    pid: u32,
    detail: &str,
    ev_tx: &Sender<Event>,
) {
    // Ignore if this isn't the current child (stale watcher thread).
    let is_current = state
        .child
        .as_ref()
        .map(|c| c.id() == pid)
        .unwrap_or(false);
    if !is_current {
        tracing::debug!(pid, "ignoring exit of non-current daemon");
        return;
    }

    // Reap to get status (non-blocking; the poll watcher may race us to
    // the kernel waitpid slot, which is fine — try_wait returns Ok(None)
    // if already reaped).
    let status = state.child.as_mut().and_then(|c| c.try_wait().ok().flatten());
    state.child = None;

    if state.upgrade_in_progress {
        // Upgrade path handles its own respawn logic.
        tracing::info!(pid, ?status, detail, "daemon exited during upgrade");
        return;
    }

    match status {
        Some(s) if s.success() => {
            tracing::info!(pid, "daemon exited cleanly — supervisor stopping");
            // Clean exit → stop supervising. Caller of main_loop exits
            // naturally because no more events arrive and shutdown is
            // orthogonal; simplest here is to flag budget-exhausted so
            // the loop bails with a non-error exit path. We do that by
            // returning from main_loop via the normal path — push no
            // respawn, set a marker the loop checks.
            //
            // Simpler trick: spawn NOTHING, let the loop idle; but the
            // CLI expects supervisor to stop when the daemon is stopped
            // via `agend-terminal stop`. We achieve that by explicitly
            // shutting down via an Exited event with pid=0 sentinel —
            // handled by handle_clean_shutdown below.
            handle_clean_shutdown(ev_tx);
        }
        _ => {
            tracing::warn!(pid, ?status, detail, "daemon crashed — respawning");
            if let Err(e) = state.spawn_daemon(ev_tx) {
                tracing::error!(error = %e, "respawn failed");
            }
        }
    }
}

fn handle_clean_shutdown(_ev_tx: &Sender<Event>) {
    // Simpler than a channel sentinel: raise SIGTERM on ourselves so the
    // handler installed in `install_signals` flips `shutdown` and the main
    // loop exits cleanly.
    // SAFETY: raise(SIGTERM) is async-signal-safe.
    unsafe {
        libc::raise(libc::SIGTERM);
    }
}

fn handle_connection(
    state: &mut SupervisorState,
    stream: UnixStream,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
    let mut writer = stream;

    let req: Request = match ipc::read_one::<Request, _>(&mut reader)? {
        Some(r) => r,
        None => return Ok(()),
    };

    match req {
        Request::Ping => {
            let mut data = serde_json::Map::new();
            data.insert("pid".into(), serde_json::json!(std::process::id()));
            data.insert(
                "daemon_pid".into(),
                serde_json::json!(state.child.as_ref().map(|c| c.id())),
            );
            data.insert(
                "version".into(),
                serde_json::json!(state.current_version.clone()),
            );
            ipc::write_one(
                &mut writer,
                &ipc::ok_final(Some("pong".into()), Some(serde_json::Value::Object(data))),
            )?;
        }
        Request::Status => {
            let data = serde_json::json!({
                "pid": std::process::id(),
                "daemon_pid": state.child.as_ref().map(|c| c.id()),
                "version": state.current_version,
                "upgrade_in_progress": state.upgrade_in_progress,
                "restart_count_window": state.restarts.len(),
                "last_ready_secs_ago": state.last_ready.map(|t| t.elapsed().as_secs()),
            });
            ipc::write_one(&mut writer, &ipc::ok_final(None, Some(data)))?;
        }
        Request::Ready { pid, version } => {
            tracing::info!(pid, version = %version, "daemon ready ping");
            state.last_ready = Some(Instant::now());
            // Only trust the version string if it came from the current
            // daemon child; a late Ready ping from a stale daemon must
            // not clobber our tracked version.
            if state.child.as_ref().map(|c| c.id()) == Some(pid) {
                state.current_version = version;
            }
            ipc::write_one(&mut writer, &ipc::ok_final(None, None))?;
        }
        Request::ShuttingDown { reason } => {
            tracing::info!(reason = %reason, "daemon shutting-down notice");
            ipc::write_one(&mut writer, &ipc::ok_final(None, None))?;
        }
        Request::Upgrade(args) => {
            if state.upgrade_in_progress {
                ipc::write_one(
                    &mut writer,
                    &ipc::err("another upgrade is already in progress"),
                )?;
                return Ok(());
            }
            state.upgrade_in_progress = true;
            let outcome = perform_upgrade(state, &args, &mut writer, ev_rx, ev_tx);
            state.upgrade_in_progress = false;
            if let Err(e) = outcome {
                tracing::error!(error = %e, "upgrade failed at orchestration level");
                let _ = ipc::write_one(&mut writer, &ipc::err(format!("{e:#}")));
            }
        }
    }
    Ok(())
}

/// Perform a supervised upgrade. Streams progress to `writer`; writes a
/// final terminal response before returning.
fn perform_upgrade(
    state: &mut SupervisorState,
    args: &UpgradeArgs,
    writer: &mut UnixStream,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
) -> Result<()> {
    let new_binary = paths::stored_binary(&state.home, &args.new_hash);
    let prev_binary = paths::stored_binary(&state.home, &args.prev_hash);
    if !new_binary.exists() {
        ipc::write_one(
            writer,
            &ipc::err(format!("new binary missing at {}", new_binary.display())),
        )?;
        return Ok(());
    }
    if !prev_binary.exists() {
        ipc::write_one(
            writer,
            &ipc::err(format!(
                "prev binary missing at {} (cannot guarantee rollback)",
                prev_binary.display()
            )),
        )?;
        return Ok(());
    }

    write_progress(writer, UpgradeStage::Accepted, "upgrade request accepted")?;

    // 1. Self-test the new binary BEFORE stopping the daemon.
    write_progress(
        writer,
        UpgradeStage::SelfTesting,
        "running self-test on new binary",
    )?;
    if let Err(e) = run_self_test(&new_binary, &state.home) {
        ipc::write_one(writer, &ipc::err(format!("self-test failed: {e:#}")))?;
        return Ok(());
    }

    // 2. Write the upgrade marker so the new daemon can tailor its system
    //    message on agent respawn.
    write_upgrade_marker(&state.home, args)?;

    // 3. Stop the current daemon.
    write_progress(writer, UpgradeStage::StoppingDaemon, "stopping current daemon")?;
    if let Some(ref mut child) = state.child {
        stop_child(child, DAEMON_STOP_GRACE);
    }
    state.child = None;
    // Drain any Exited event from the now-stopped daemon so wait_for_ready
    // doesn't mistake it for a new daemon crash.
    drain_exited_for(ev_rx, None, Duration::from_millis(250));

    // 4. Start the new daemon.
    write_progress(writer, UpgradeStage::StartingDaemon, "spawning new daemon")?;
    state.last_ready = None;
    let pre_spawn = Instant::now();
    state.spawn_daemon(ev_tx).with_context(|| {
        format!(
            "spawn new daemon from {}",
            paths::current_link(&state.home).display()
        )
    })?;
    let new_pid = state.child.as_ref().map(|c| c.id()).unwrap_or(0);

    // 5. Wait for the Ready ping (or give up).
    if args.ready_timeout_secs > 0 {
        write_progress(
            writer,
            UpgradeStage::WaitingReady,
            &format!(
                "waiting up to {}s for new daemon to report ready",
                args.ready_timeout_secs
            ),
        )?;
        match wait_for_ready(
            state,
            ev_rx,
            ev_tx,
            new_pid,
            args.ready_timeout_secs,
            pre_spawn,
        )? {
            ReadyOutcome::Ready => {}
            ReadyOutcome::Crashed(reason) => {
                return rollback(
                    state,
                    args,
                    writer,
                    ev_tx,
                    &format!("new daemon crashed before ready: {reason}"),
                );
            }
            ReadyOutcome::Timeout => {
                return rollback(
                    state,
                    args,
                    writer,
                    ev_tx,
                    "new daemon did not report ready within timeout",
                );
            }
        }
    }

    // 6. Stability window.
    if args.stability_secs > 0 {
        write_progress(
            writer,
            UpgradeStage::Stabilising,
            &format!("observing stability window ({}s)", args.stability_secs),
        )?;
        match stabilise(state, ev_rx, ev_tx, args.stability_secs)? {
            StabilityOutcome::Stable => {}
            StabilityOutcome::Unstable(reason) => {
                return rollback(
                    state,
                    args,
                    writer,
                    ev_tx,
                    &format!("unstable within window: {reason}"),
                );
            }
        }
    }

    // 7. Success.
    state.current_version = args
        .to_version
        .clone()
        .unwrap_or_else(|| state.current_version.clone());
    write_progress(writer, UpgradeStage::Succeeded, "upgrade succeeded")?;
    ipc::write_one(
        writer,
        &ipc::ok_final(Some("upgrade complete".into()), None),
    )?;
    Ok(())
}

enum ReadyOutcome {
    Ready,
    Crashed(String),
    Timeout,
}

/// Drain events until we either see the new daemon's Ready ping, its exit,
/// or hit the timeout. Non-Ready/Exit events (e.g. concurrent Status
/// queries) are dispatched through [`handle_connection`] inline so they
/// don't pile up in the channel.
fn wait_for_ready(
    state: &mut SupervisorState,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
    new_pid: u32,
    timeout_secs: u64,
    pre_spawn: Instant,
) -> Result<ReadyOutcome> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(ready_at) = state.last_ready {
            if ready_at >= pre_spawn {
                return Ok(ReadyOutcome::Ready);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(ReadyOutcome::Timeout);
        }
        let slice = (deadline - now).min(Duration::from_millis(250));
        match ev_rx.recv_timeout(slice) {
            Ok(Event::Exited { pid, detail }) => {
                if pid == new_pid {
                    state.child = None;
                    return Ok(ReadyOutcome::Crashed(detail));
                }
                // Stale watcher — ignore.
            }
            Ok(ev @ Event::Connection(_)) => {
                dispatch_event(state, ev, ev_rx, ev_tx)?;
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                return Ok(ReadyOutcome::Crashed("event channel disconnected".into()));
            }
        }
    }
}

enum StabilityOutcome {
    Stable,
    Unstable(String),
}

/// Watch for repeat crashes inside the stability window. One crash is
/// recoverable (we respawn and keep watching); a second crash fails
/// stability.
fn stabilise(
    state: &mut SupervisorState,
    ev_rx: &Receiver<Event>,
    ev_tx: &Sender<Event>,
    window_secs: u64,
) -> Result<StabilityOutcome> {
    let deadline = Instant::now() + Duration::from_secs(window_secs);
    let mut crashes = 0u32;

    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        let slice = remaining.min(Duration::from_millis(500));
        match ev_rx.recv_timeout(slice) {
            Ok(Event::Exited { pid, detail }) => {
                let is_current = state
                    .child
                    .as_ref()
                    .map(|c| c.id() == pid)
                    .unwrap_or(false);
                if !is_current {
                    continue;
                }
                state.child = None;
                crashes += 1;
                tracing::warn!(pid, detail, crashes, "crash during stability window");
                if crashes >= 2 {
                    return Ok(StabilityOutcome::Unstable(format!(
                        "{crashes} crashes in stability window"
                    )));
                }
                if let Err(e) = state.spawn_daemon(ev_tx) {
                    return Ok(StabilityOutcome::Unstable(format!(
                        "failed to respawn during stability: {e:#}"
                    )));
                }
            }
            Ok(ev @ Event::Connection(_)) => {
                dispatch_event(state, ev, ev_rx, ev_tx)?;
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                return Ok(StabilityOutcome::Unstable(
                    "event channel disconnected".into(),
                ));
            }
        }
    }
    Ok(StabilityOutcome::Stable)
}

/// Try to pull a matching `Exited` event out of the queue within `window`.
/// If `pid` is `None`, any Exited counts. Used after `stop_child` to
/// absorb the watcher-thread event for the stopped daemon.
fn drain_exited_for(ev_rx: &Receiver<Event>, pid: Option<u32>, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let remaining = deadline - Instant::now();
        match ev_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(Event::Exited { pid: got, .. }) => {
                if pid.is_none_or(|p| p == got) {
                    return;
                }
            }
            Ok(_) => {
                // Non-exit event during drain: push it back is non-trivial
                // with crossbeam (no peek-and-return). We just drop it —
                // rare during the 250ms post-stop window, and any dropped
                // Connection stream just gets RST on the client, which
                // already retries.
            }
            Err(_) => return,
        }
    }
}

/// Restore the `current` symlink to `prev`, restart the daemon, and
/// surface a rollback-final response.
fn rollback(
    state: &mut SupervisorState,
    args: &UpgradeArgs,
    writer: &mut UnixStream,
    ev_tx: &Sender<Event>,
    reason: &str,
) -> Result<()> {
    write_progress(
        writer,
        UpgradeStage::RollingBack,
        &format!("rolling back: {reason}"),
    )?;
    tracing::warn!(reason, "rolling back upgrade");

    // Stop whatever's running (likely nothing — we're here because it crashed).
    if let Some(ref mut child) = state.child {
        stop_child(child, DAEMON_STOP_GRACE);
        state.child = None;
    }

    // Swap current ← prev binary.
    if let Err(e) = swap_current_to(&state.home, &args.prev_hash) {
        ipc::write_one(
            writer,
            &ipc::err(format!(
                "rollback failed to repoint current symlink: {e:#}"
            )),
        )?;
        return Err(e);
    }
    // Clear the upgrade marker so the respawned old daemon doesn't emit a
    // "daemon upgraded" message.
    let _ = std::fs::remove_file(paths::upgrade_marker(&state.home));

    // Relaunch prev daemon.
    if let Err(e) = state.spawn_daemon(ev_tx) {
        ipc::write_one(
            writer,
            &ipc::err(format!("rollback failed to respawn prev daemon: {e:#}")),
        )?;
        return Err(e);
    }
    state.current_version = args
        .from_version
        .clone()
        .unwrap_or_else(|| state.current_version.clone());

    write_progress(writer, UpgradeStage::RolledBack, "rollback complete")?;
    ipc::write_one(writer, &ipc::err(format!("upgrade rolled back: {reason}")))?;
    Ok(())
}

/// Rewrite `bin/current` → `store/<hash>`.
fn swap_current_to(home: &Path, hash: &str) -> Result<()> {
    use std::os::unix::fs::symlink;
    let bin = paths::bin_dir(home);
    let tmp = bin.join(".current.rollback");
    let _ = std::fs::remove_file(&tmp);
    let target = PathBuf::from("store").join(hash);
    symlink(&target, &tmp)
        .with_context(|| format!("create rollback symlink {}", tmp.display()))?;
    std::fs::rename(&tmp, paths::current_link(home))
        .context("rename rollback symlink into place")?;
    Ok(())
}

fn write_progress(
    writer: &mut UnixStream,
    stage: UpgradeStage,
    message: &str,
) -> Result<()> {
    tracing::info!(?stage, message, "upgrade progress");
    ipc::write_one(writer, &ipc::progress(stage, message))?;
    Ok(())
}

fn write_upgrade_marker(home: &Path, args: &UpgradeArgs) -> Result<()> {
    let body = serde_json::json!({
        "from_version": args.from_version,
        "to_version": args.to_version,
        "new_hash": args.new_hash,
        "prev_hash": args.prev_hash,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    let path = paths::upgrade_marker(home);
    std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default())
        .with_context(|| format!("write upgrade marker {}", path.display()))?;
    Ok(())
}

fn run_self_test(binary: &Path, home: &Path) -> Result<()> {
    let out = Command::new(binary)
        .env("AGEND_SELF_TEST", "1")
        .env("AGEND_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn self-test for {}", binary.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("self-test exited {}: {}", out.status, stderr.trim());
    }
    Ok(())
}

fn read_version_of(binary: &Path) -> Result<String> {
    let out = Command::new(binary).arg("--version").output()?;
    if !out.status.success() {
        anyhow::bail!("--version exit {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// --- Accept loop + child waiter ------------------------------------------

fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    ev_tx: Sender<Event>,
    shutdown: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match stream {
            Ok(s) => {
                if ev_tx.send(Event::Connection(s)).is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "supervisor accept error");
            }
        }
    }
}

fn wait_for_child(pid: u32, ev_tx: Sender<Event>) {
    // Two ways a child can become unwatchable:
    //
    //   a) It was reaped elsewhere (e.g. `stop_child`'s `Child::try_wait`)
    //      — the PID is fully gone, `kill(pid, 0)` returns ESRCH.
    //   b) It exited but we (the parent) haven't reaped yet — a zombie. In
    //      this state `kill(pid, 0)` still returns success because the PID
    //      entry lingers until waitpid. Polling `kill(0)` alone misses
    //      this case and the stability/ready windows stay blind to
    //      crashes.
    //
    // We therefore try `waitpid(pid, WNOHANG)` first. When it returns a
    // positive value the child has just exited *and we reaped it*; when
    // it returns -1/ECHILD another call site already reaped (harmless
    // race with `Child::try_wait`). Only if neither happened do we fall
    // back to `kill(0)` for belt-and-braces defence.
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if r > 0 {
            let _ = ev_tx.send(Event::Exited {
                pid,
                detail: format!("exited (waitpid status {status:#x})"),
            });
            return;
        }
        if r < 0 {
            let last = std::io::Error::last_os_error();
            if last.raw_os_error() == Some(libc::ECHILD) {
                let _ = ev_tx.send(Event::Exited {
                    pid,
                    detail: "already reaped elsewhere".into(),
                });
                return;
            }
            // Other errors (EINTR) — retry after a tick.
        }
        if !is_alive(pid) {
            let _ = ev_tx.send(Event::Exited {
                pid,
                detail: "process gone (kill=ESRCH)".into(),
            });
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn is_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` sends no signal — pure liveness probe.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn stop_child(child: &mut Child, grace: Duration) {
    let pid = child.id();
    tracing::info!(pid, ?grace, "stopping daemon");
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "try_wait during stop");
                break;
            }
        }
        if Instant::now() >= deadline {
            tracing::warn!(pid, "grace expired — SIGKILL");
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn install_signals(shutdown: Arc<AtomicBool>) {
    extern "C" fn handler(_signum: libc::c_int) {
        FLAG.store(true, Ordering::Relaxed);
    }
    static FLAG: AtomicBool = AtomicBool::new(false);

    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }

    thread::Builder::new()
        .name("supervisor-signal-pump".into())
        .spawn(move || loop {
            if FLAG.load(Ordering::Relaxed) {
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
            thread::sleep(Duration::from_millis(250));
        })
        .ok();
}

fn cleanup(sock: &Path, pid_file: &Path) {
    let _ = std::fs::remove_file(sock);
    let _ = std::fs::remove_file(pid_file);
}

// Suppress unused-Response warning when the module is built without the
// `Response` ever being destructured here (the server always constructs
// via helpers).
#[allow(dead_code)]
type _ResponseAlias = Response;
