use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
use portable_pty::PtySize;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Output of the synchronous TUI prep step. Carries the bound TCP
/// listener and the auth cookie so the async accept loop can resume
/// without re-reading either from disk.
///
/// #896 Option D: separating "bind + publish .port" (sync, returnable
/// failure) from "accept loop" (async, fire-and-forget) is what lets
/// `spawn_and_register_agent` block on the publish step.
pub(crate) struct TuiListenerMeta {
    listener: std::net::TcpListener,
    cookie: crate::auth_cookie::Cookie,
    /// Run dir holding `{name}.port` — the retirement signal this bridge polls.
    run_dir: std::path::PathBuf,
    /// The port this bridge published. A `{name}.port` that no longer names it
    /// means a successor generation owns the name and this bridge is stale.
    port: u16,
}

/// How often a bridge checks whether it has been retired. The accept loop is
/// non-blocking so it can notice; this bounds the notice delay.
const RETIREMENT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// True when `{name}.port` is gone or no longer names `my_port`.
///
/// #3373: `restart_instance(mode:"fresh")` is delete(no-wait) + spawn. The
/// delete already removes the port file (`ipc::remove_port`) and the successor
/// republishes it with a NEW port, so this is an existing signal, not a new one.
/// What was missing is a reader: the accept loop assumed removal closed its
/// socket, which is true for a socket FILE and false for the loopback TCP
/// listener this actually is. Without a reader the predecessor kept accepting
/// and kept its clients' sockets healthy, so a retained APP pane never went
/// `[DISCONNECTED]` and #3380's reconnect never fired.
fn is_retired(run_dir: &Path, name: &str, my_port: u16) -> bool {
    match std::fs::read_to_string(run_dir.join(format!("{name}.port"))) {
        Ok(text) => text
            .trim()
            .parse::<u16>()
            .map(|p| p != my_port)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Synchronously bind the agent's TUI loopback socket and publish
/// `{run_dir}/{name}.port`. Returns the listener + cookie so a
/// subsequent fire-and-forget accept loop (`serve_tui_accept_loop`)
/// can take over without redoing the io::Result-bearing setup.
///
/// #896 Option D contract: callers that need rollback semantics (the
/// daemon's startup loop via `spawn_and_register_agent`) MUST call
/// this directly and propagate the Err. Callers that don't need
/// rollback (CLI capture, agent shell-fallback, verify probe) can use
/// `serve_agent_tui` which wraps prep + accept-loop into one
/// best-effort entrypoint.
pub(crate) fn prepare_tui_listener_and_publish_port(
    name: &str,
    run_dir: &Path,
) -> std::io::Result<TuiListenerMeta> {
    // P1-10: load the per-daemon cookie once; every incoming TUI
    // client must present it as the first 32 bytes on the wire.
    let cookie = crate::auth_cookie::read_cookie(run_dir).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("api.cookie unavailable: {e}"),
        )
    })?;
    let listener = crate::ipc::bind_loopback()?;
    let port = crate::ipc::local_port(&listener);
    // #1935: refuse the port publish if the instance is mid-delete. `full_delete`
    // → `remove_port` deletes `run/<pid>/<name>.port`, but a boot-spawn / respawn
    // publish still in flight would re-create it AFTER the removal (the #1913
    // writer-vs-teardown race that left a residual the #1907 oracle didn't catch).
    // The #1915 DeletingGuard already refuses the spawn/register chokepoint; this
    // closes the narrower window where write_port runs past it. Cheap leaf-lock
    // read. `home` = run_dir's grandparent (run_dir is always `home/run/<pid>` via
    // run_dir_for_pid), so the key matches full_delete's `mark_deleting(home, …)`.
    if let Some(home) = run_dir.parent().and_then(|p| p.parent()) {
        if crate::agent::deleting::is_deleting(home, name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("instance '{name}' is being deleted; TUI port publish skipped"),
            ));
        }
    }
    crate::ipc::write_port(run_dir, name, port)?;
    tracing::info!(agent = name, port, "TUI socket ready");
    Ok(TuiListenerMeta {
        listener,
        cookie,
        run_dir: run_dir.to_path_buf(),
        port,
    })
}

/// All-in-one TUI server for callers that don't need rollback on
/// prep failure (CLI `capture`, agent crash shell-fallback, verify
/// probe). Internally runs the synchronous prep + the async accept
/// loop on the calling thread. Prep failure degrades to a warn-log
/// and early return, preserving the pre-#896 best-effort shape.
///
/// Blocks the calling thread on `incoming()` until the listener is
/// dropped or the agent is removed from the registry. Callers wanting
/// rollback semantics should call
/// [`prepare_tui_listener_and_publish_port`] + [`serve_tui_accept_loop`]
/// separately so they can react to prep failure.
pub fn serve_agent_tui(name: &str, run_dir: &Path, registry: &AgentRegistry) {
    let meta = match prepare_tui_listener_and_publish_port(name, run_dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                agent = name,
                error = %e,
                "TUI listener prep failed; server aborted"
            );
            return;
        }
    };
    serve_tui_accept_loop(name, meta, registry);
}

/// Run the TUI accept loop with a pre-bound listener + cookie. Blocks
/// the calling thread; intended to be spawned fire-and-forget after a
/// successful synchronous `prepare_tui_listener_and_publish_port`
/// step. Exits when the listener is dropped or accept errors
/// terminally (e.g. agent removal via `delete_transaction` closes the
/// underlying socket file).
/// Overall budget for a client's auth handshake. Unchanged from the inline
/// version this replaced.
const AUTH_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Read and verify one client's 32-byte TUI auth cookie.
///
/// Extracted from the accept loop so a test can drive the real production path
/// over a loopback socket. Behavior is currently verbatim: a single blocking
/// `read_exact` under one 10s timeout.
fn read_and_verify_tui_cookie(
    stream: &mut std::net::TcpStream,
    cookie: &crate::auth_cookie::Cookie,
    run_dir: &Path,
    name: &str,
    port: u16,
) -> bool {
    let _ = (run_dir, name, port);
    let _ = stream.set_read_timeout(Some(AUTH_BUDGET));
    crate::auth_cookie::read_and_verify_tui(stream, cookie).is_ok()
}

/// One client's output forwarder: pump broadcast frames at `write_stream`
/// until the client goes away, the generation's subscriber drops, or this
/// bridge is retired.
///
/// Extracted from the accept loop so a test can drive the REAL function over a
/// loopback socket pair with a real `crossbeam` receiver, and observe the actual
/// shutdown side effect rather than the shape of the source.
///
/// Socket ownership is the subtle part: the accept loop hands the INPUT thread
/// another handle on the same connection, so dropping `write_stream` here does
/// NOT close the connection. Only an explicit `shutdown` does — which is why
/// every terminal path that must be visible to the peer has to call it.
pub(crate) fn forward_tui_output(
    mut write_stream: std::net::TcpStream,
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    run_dir: std::path::PathBuf,
    name: String,
    port: u16,
) {
    loop {
        match rx.recv_timeout(RETIREMENT_POLL) {
            Ok(data) => {
                if framing::write_frame(&mut write_stream, &data).is_err() {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Closing the socket is the load-bearing act: it is what makes a
                // retained pane observe EOF, flip `connected` false, and become a
                // reconnect candidate.
                if is_retired(&run_dir, &name, port) {
                    let _ = write_stream.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

pub(crate) fn serve_tui_accept_loop(name: &str, meta: TuiListenerMeta, registry: &AgentRegistry) {
    let TuiListenerMeta {
        listener,
        cookie,
        run_dir,
        port,
    } = meta;
    // Fail closed: without non-blocking accept the retirement check below is
    // unreachable and this bridge would silently go back to parking forever
    // after a successor took the name — the exact #3373 shape.
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::error!(
            agent = name,
            port,
            error = %e,
            "TUI listener non-blocking mode unavailable; refusing to serve an unretirable bridge"
        );
        return;
    }

    loop {
        if is_retired(&run_dir, name, port) {
            // Stops the accept loop and drops the listener. Live client sockets
            // retire themselves in their own output thread below — that thread
            // owns the socket, so nothing here has to retain a handle to it.
            tracing::info!(
                agent = name,
                port,
                "TUI bridge retired — successor owns the name"
            );
            return;
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(RETIREMENT_POLL);
                continue;
            }
            Err(_) => continue,
        };
        // An accepted socket may inherit the listener's non-blocking mode; the
        // framing reads below are blocking by contract.
        if let Err(e) = stream.set_nonblocking(false) {
            tracing::warn!(agent = name, error = %e, "TUI client blocking mode unavailable");
            continue;
        }
        let _ = stream.set_nodelay(true);
        // Bound the auth read so a silent peer cannot pin this accept loop.
        // Framing read/write stays unbounded afterwards — the deadline is
        // reset once the cookie check passes.
        if !read_and_verify_tui_cookie(&mut stream, &cookie, &run_dir, name, port) {
            tracing::warn!(agent = name, "TUI client rejected (auth)");
            continue;
        }
        let _ = stream.set_read_timeout(None);
        tracing::info!(agent = name, "TUI client connected");

        // Protocol version handshake: send version byte before any framed data
        if stream.write_all(&[framing::PROTOCOL_VERSION]).is_err() {
            continue;
        }
        if stream.flush().is_err() {
            continue;
        }

        // #1617-class (mirror #1593 F1 snapshot→drop→IO): capture the rx +
        // initial dump + the Arcs UNDER the registry lock, then DROP the guard
        // before writing the dump to the client. `write_frame` is a blocking
        // socket write — a slow/non-draining TUI client would otherwise pin the
        // GLOBAL registry lock indefinitely (exactly the hung-peer stall #1617
        // closed for the PTY path), wedging the whole daemon. `dump` is an owned
        // Vec, so it survives the drop; intervening PTY output buffers in `rx`
        // and is sent by the tui_out thread after this initial frame.
        let (rx, dump, pty_writer, pty_master, core) = {
            let reg = agent::lock_registry(registry);
            // #1441: registry is UUID-keyed; this TUI-bridge server only knows
            // the display name, so locate the live handle by name.
            let agent = match reg.values().find(|h| h.name.as_str() == name) {
                Some(a) => a,
                None => continue,
            };
            let (rx, dump) = agent::subscribe_with_dump(agent);
            (
                rx,
                dump,
                Arc::clone(&agent.pty_writer),
                Arc::clone(&agent.pty_master),
                Arc::clone(&agent.core),
            )
        };
        // Registry lock released — the blocking initial-dump write runs lock-free.
        if framing::write_frame(&mut stream, &dump).is_err() {
            continue;
        }

        let write_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let n = name.to_string();
        let retire_dir = run_dir.clone();
        let retire_name = n.clone();
        // fire-and-forget: per-client TUI output forwarder. Loop exits when
        // the broadcast subscriber rx drops (agent removed via
        // delete_transaction), when frame write fails (client disconnect), or
        // when this bridge is retired. No graceful join needed — each client
        // connection is independent.
        // No leak: every exit path drops this thread's only socket handle, and
        // the disconnect path is reached by write_frame failing. #3373 adds the
        // retirement path — this thread owns the client socket, so retiring it
        // here needs no shared client list and cannot retain a dead clone.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("{n}_tui_out"))
            .spawn(move || {
                forward_tui_output(write_stream, rx, retire_dir, retire_name, port);
            })
        {
            tracing::warn!(agent = %n, error = %e, "failed to spawn TUI output thread");
        }

        let read_stream = stream;
        let n = name.to_string();
        let n_err = n.clone();
        // fire-and-forget: per-client TUI input forwarder. Loop exits on
        // socket disconnect (read_tagged_frame returns Err). Mirror of
        // tui_out above; same independent-per-client lifecycle.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("{n}_tui_in"))
            .spawn(move || {
                let mut reader = read_stream;
                loop {
                    match framing::read_tagged_frame(&mut reader) {
                        Ok((TAG_DATA, data)) => {
                            // CR-2026-06-14: route through the bounded
                            // `write_to_pty` (write_with_timeout) instead of a
                            // raw `pty_writer.lock().write_all`. This `pty_writer`
                            // is `Arc::clone(&agent.pty_writer)` — the SAME lock
                            // the inject path's `write_with_timeout` worker
                            // acquires. A raw blocking write here on a wedged PTY
                            // would hold that lock indefinitely, blocking the
                            // inject worker → leaving WRITE_IN_PROGRESS stuck →
                            // every subsequent inject to this agent fail-fasts
                            // (the H13 control-plane harm class). The bounded
                            // path never holds the lock past the timeout.
                            if agent::write_to_pty(&pty_writer, &data).is_err() {
                                break;
                            }
                        }
                        Ok((TAG_RESIZE, data)) if data.len() == 4 => {
                            let cols = u16::from_be_bytes([data[0], data[1]]);
                            let rows = u16::from_be_bytes([data[2], data[3]]);
                            let _ = pty_master.lock().resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            {
                                let mut c = core.lock();
                                c.vterm.resize(cols, rows);
                            }
                        }
                        _ => break,
                    }
                }
                tracing::info!(agent = %n, "TUI client disconnected");
            })
        {
            tracing::warn!(agent = %n_err, error = %e, "failed to spawn TUI input thread");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    /// A connected loopback pair plus a SECOND handle on the server side.
    ///
    /// The second handle is the point: in production the accept loop gives the
    /// INPUT thread another handle on the same connection, so dropping the
    /// forwarder's handle does not close anything. Holding one here makes these
    /// tests observe the same truth — only an explicit `shutdown` reaches the peer.
    struct SocketPair {
        server: TcpStream,
        _server_input_side: TcpStream,
        peer: TcpStream,
    }

    fn socket_pair() -> SocketPair {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let input_side = server.try_clone().unwrap();
        SocketPair {
            server,
            _server_input_side: input_side,
            peer,
        }
    }

    fn scratch_run_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-3373-fwd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn publish_port(run_dir: &std::path::Path, name: &str, port: u16) {
        std::fs::write(run_dir.join(format!("{name}.port")), port.to_string()).unwrap();
    }

    /// Drain `peer` until it reports EOF, or give up. Draining matters: an
    /// undrained socket would fill and stall the forwarder in `write_frame`,
    /// which would make the test pass for the wrong reason.
    fn peer_sees_eof(mut peer: TcpStream, budget: Duration) -> bool {
        peer.set_read_timeout(Some(budget)).unwrap();
        let deadline = Instant::now() + budget;
        let mut buf = [0u8; 8192];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return false
                }
                Err(_) => return true,
            }
        }
    }

    /// #3373 follow-up: a stalled peer must not hold the accept loop past
    /// retirement. The auth read is serialized in the accept loop, so a peer
    /// that sends 1 of the 32 cookie bytes pins the whole loop for the auth
    /// budget — a retired bridge keeps accepting for ~10s after the successor
    /// took the name. Measured at 8cd95769 by an independent reviewer: ~10.17s.
    #[test]
    fn stalled_auth_peer_does_not_outlive_retirement() {
        let run_dir = scratch_run_dir("stalled-auth");
        publish_port(&run_dir, "agent", 4242);
        let pair = socket_pair();
        let cookie: crate::auth_cookie::Cookie = [7u8; 32];

        let mut stalled = pair.peer;
        let mut server = pair.server;
        let dir = run_dir.clone();
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(1);
        let auth = std::thread::spawn(move || {
            started_tx.send(()).ok();
            super::read_and_verify_tui_cookie(&mut server, &cookie, &dir, "agent", 4242)
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("auth thread must start");

        // One byte of the 32, then silence: the read is now genuinely in flight.
        stalled.write_all(&[7u8]).unwrap();
        stalled.flush().unwrap();

        // Let the helper take several read timeouts INSIDE its loop before the
        // bridge is retired. This is what makes the test non-vacuous: at entry
        // the port file still named this bridge, so an entry-only precheck
        // cannot satisfy it — only a check between reads can.
        std::thread::sleep(super::RETIREMENT_POLL * 3);

        let retired_at = Instant::now();
        publish_port(&run_dir, "agent", 4343);

        let accepted = auth.join().unwrap();
        // Measured from the RETIREMENT, not from the start of the read.
        let elapsed = retired_at.elapsed();

        drop(stalled);
        std::fs::remove_dir_all(&run_dir).ok();
        assert!(!accepted, "a partial cookie must never authenticate");
        assert!(
            elapsed <= Duration::from_secs(1),
            "a retired bridge must abandon an in-flight stalled auth read promptly rather than \
             hold the serialized accept loop for the whole auth budget; took {elapsed:?} after \
             retirement"
        );
    }

    /// #3373 follow-up: retirement must not depend on the channel going quiet.
    /// A generation that is still producing output keeps `recv_timeout` in its
    /// `Ok` arm, so a retirement check that only lives in the timeout arm is
    /// never consulted and the stale pane socket stays open.
    #[test]
    fn retirement_is_observed_while_output_is_still_flowing() {
        let run_dir = scratch_run_dir("busy");
        publish_port(&run_dir, "agent", 4242);
        let pair = socket_pair();
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(64);

        // Keep the channel continuously ready for the whole test.
        let producer = std::thread::spawn(move || {
            while tx.send(vec![b'x'; 256]).is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let dir = run_dir.clone();
        let forwarder = std::thread::spawn(move || {
            super::forward_tui_output(pair.server, rx, dir, "agent".to_string(), 4242);
        });

        // Retire this bridge: the successor republishes the name on a new port.
        publish_port(&run_dir, "agent", 4343);

        let closed = peer_sees_eof(pair.peer, Duration::from_secs(5));
        let _ = forwarder.join();
        drop(producer);
        std::fs::remove_dir_all(&run_dir).ok();
        assert!(
            closed,
            "a retired bridge must close its client socket even while output is still flowing; \
             a check that only runs in the recv timeout arm never fires on a busy channel"
        );
    }

    /// #3373 follow-up: the generation's subscriber dropping is a terminal path
    /// too, and the peer must see it. Dropping the forwarder's handle is not
    /// enough — the input thread holds another handle on the same connection.
    #[test]
    fn subscriber_disconnect_closes_the_client_socket() {
        let run_dir = scratch_run_dir("disconnect");
        publish_port(&run_dir, "agent", 4242);
        let pair = socket_pair();
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(4);

        let dir = run_dir.clone();
        let forwarder = std::thread::spawn(move || {
            super::forward_tui_output(pair.server, rx, dir, "agent".to_string(), 4242);
        });

        // The agent is deleted: its broadcast senders go away.
        drop(tx);

        let closed = peer_sees_eof(pair.peer, Duration::from_secs(5));
        let _ = forwarder.join();
        std::fs::remove_dir_all(&run_dir).ok();
        assert!(
            closed,
            "a Disconnected subscriber must close the client socket; breaking without shutdown \
             leaves the pane attached to the dead generation (the original #3373 symptom)"
        );
    }

    /// #3373: the retirement predicate. A bridge is stale the moment
    /// `{name}.port` stops naming its own port — which is exactly what the
    /// restart delete (`ipc::remove_port`) plus the successor's republish do.
    #[test]
    fn retirement_tracks_the_published_port() {
        let dir = std::env::temp_dir().join(format!(
            "agend-3373-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let port_file = dir.join("agent.port");

        // No port file at all — the delete removed it.
        assert!(super::is_retired(&dir, "agent", 4242));

        // The successor republished under a different port.
        std::fs::write(&port_file, "4343").unwrap();
        assert!(super::is_retired(&dir, "agent", 4242));

        // Still ours.
        std::fs::write(&port_file, "4242").unwrap();
        assert!(!super::is_retired(&dir, "agent", 4242));

        // Unparseable is treated as not-ours rather than assumed live.
        std::fs::write(&port_file, "not-a-port").unwrap();
        assert!(super::is_retired(&dir, "agent", 4242));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fleet pane reaches this branch through `PaneSource::Remote`. A silent
    /// resize changes geometry but writes no bytes, so it must not invalidate a
    /// queued Claude development-channel confirmation. Any child repaint still
    /// invalidates through the PTY read loop, and `TAG_DATA` still goes through
    /// the normal write chokepoint.
    #[test]
    fn tui_resize_branch_does_not_bump_dev_modal_epoch() {
        let src = include_str!("tui_bridge.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = src.split_once(&cfg_test).map_or(src, |(before, _)| before);
        let resize_arm = prod
            .split_once("Ok((TAG_RESIZE, data)) if data.len() == 4 => {")
            .expect("production TAG_RESIZE arm must exist")
            .1
            .split_once("_ => break,")
            .expect("TAG_RESIZE arm must end before the fallback arm")
            .0;

        assert!(
            resize_arm.contains("pty_master.lock().resize"),
            "TAG_RESIZE must still resize the child PTY"
        );
        assert!(
            resize_arm.contains("c.vterm.resize"),
            "TAG_RESIZE must still resize the daemon VTerm"
        );
        assert!(
            !resize_arm.contains("dev_modal::") && !resize_arm.contains("note_pty_write"),
            "a byte-free TAG_RESIZE must not cancel the only queued startup-modal confirmation"
        );
    }

    /// #1617-class invariant: `serve_tui_accept_loop` must NEVER hold the
    /// global registry lock across the blocking initial-dump `write_frame`.
    /// A non-draining TUI client would otherwise pin the registry forever and
    /// wedge the whole daemon (same hung-peer stall #1617 closed for the PTY).
    ///
    /// Structural source-scan (mirrors #1593 F2 /
    /// `recovery_loop_never_holds_registry_across_blocking_io`): brace-match the
    /// dump-capture binding block and assert (a) `write_frame` is NOT inside it
    /// (i.e. not under the lock) and (b) a `write_frame` call DOES exist after
    /// the block closes (the dump is written lock-free). Needles are `concat`-
    /// built and the scan is sliced to the production region (before the
    /// `#[cfg(test)]` mod) so this test's own source can't self-satisfy it.
    #[test]
    fn tui_dump_write_not_held_across_registry_lock() {
        let src = include_str!("tui_bridge.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        // The fix marker: the lock block now captures `dump` into the outer
        // binding (was a 4-tuple without `dump` pre-fix), proving the dump is
        // moved out of the lock scope before it is written.
        let bind_needle = ["let (rx, dump, pty_writer", ", pty_master, core) = {"].concat();
        let bstart = prod
            .find(&bind_needle)
            .expect("dump-capture binding present (fix marker)");

        // Brace-match from the binding's opening `{` to find the locked region.
        let open_rel = prod[bstart..].find('{').expect("binding block opens");
        let block_start = bstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "binding block must close");

        let write_needle = ["write", "_frame"].concat();
        let locked_region = &prod[block_start..=block_end];
        assert!(
            !locked_region.contains(&write_needle),
            "tui_bridge must NOT write_frame while the registry lock is held (#1617 deadlock class)"
        );
        assert!(
            prod[block_end..].contains(&write_needle),
            "the initial dump must be written via write_frame AFTER the registry lock is dropped"
        );
    }

    /// #1935 §3.9: `prepare_tui_listener_and_publish_port` must NOT write
    /// `run/<pid>/<name>.port` while the instance is mid-delete (closes the
    /// publish-vs-teardown race where a boot-spawn republished the port AFTER
    /// `full_delete`'s `remove_port`), but MUST write it on a normal publish.
    #[test]
    fn publish_port_respects_deleting_guard() {
        let home = std::env::temp_dir().join(format!("agend-1935-pubguard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // run_dir MUST be `home/run/<pid>` so the fn derives `home` as its
        // grandparent (matching full_delete's `mark_deleting(home, …)` key).
        let run_dir = home.join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&run_dir).unwrap();
        crate::auth_cookie::issue(&run_dir).unwrap();
        let name = "victim-port";
        let port_file = crate::ipc::port_path(&run_dir, name);

        // (a) mid-delete → publish refused, no `.port` written.
        let guard = crate::agent::deleting::mark_deleting(&home, name);
        let refused = super::prepare_tui_listener_and_publish_port(name, &run_dir);
        assert!(refused.is_err(), "publish must be refused while deleting");
        assert!(
            !port_file.exists(),
            "no .port may be written while the instance is deleting"
        );
        drop(guard);

        // (b) not deleting → publish succeeds, `.port` written.
        let ok = super::prepare_tui_listener_and_publish_port(name, &run_dir);
        assert!(
            ok.is_ok(),
            "publish must succeed when not deleting (err: {:?})",
            ok.err()
        );
        assert!(
            port_file.exists(),
            ".port must be written on a normal (non-deleting) publish"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
