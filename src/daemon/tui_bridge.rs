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
    Ok(TuiListenerMeta { listener, cookie })
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
pub(crate) fn serve_tui_accept_loop(name: &str, meta: TuiListenerMeta, registry: &AgentRegistry) {
    let TuiListenerMeta { listener, cookie } = meta;

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = stream.set_nodelay(true);
        // Bound the auth read so a silent peer cannot pin this accept loop.
        // Framing read/write stays unbounded afterwards — the deadline is
        // reset once the cookie check passes.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        if let Err(e) = crate::auth_cookie::read_and_verify_tui(&mut stream, &cookie) {
            tracing::warn!(agent = name, error = %e, "TUI client rejected (auth)");
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

        let mut write_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let n = name.to_string();
        // fire-and-forget: per-client TUI output forwarder. Loop exits when
        // the broadcast subscriber rx drops (agent removed via
        // delete_transaction) or when frame write fails (client disconnect).
        // No graceful join needed — each client connection is independent.
        // M4 analysis: no leak — write_frame failure on disconnect breaks
        // the loop immediately; rx.recv() Err on agent deletion also exits.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("{n}_tui_out"))
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if framing::write_frame(&mut write_stream, &data).is_err() {
                        break;
                    }
                }
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
