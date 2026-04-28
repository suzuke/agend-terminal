use crate::agent::{self, AgentRegistry};
use crate::framing::{self, TAG_DATA, TAG_RESIZE};
use portable_pty::PtySize;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Start the TUI socket server for an agent (blocks the calling thread).
///
/// Binds a TCP loopback port, publishes it to `{run_dir}/{name}.port`, then
/// accepts connections. Removes the port file when the listener exits.
pub fn serve_agent_tui(name: &str, run_dir: &Path, registry: &AgentRegistry) {
    // P1-10: load the per-daemon cookie once; every incoming TUI client must
    // present it as the first 32 bytes on the wire. If the cookie file isn't
    // there yet, the caller (daemon::run / verify::run) skipped its issuance
    // step — fail closed rather than serve an unauthenticated TUI.
    let cookie = match crate::auth_cookie::read_cookie(run_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(agent = name, error = %e, "api.cookie missing; TUI server aborted");
            return;
        }
    };

    let listener = match crate::ipc::bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(agent = name, error = %e, "failed to bind TUI socket");
            return;
        }
    };
    let port = crate::ipc::local_port(&listener);
    if let Err(e) = crate::ipc::write_port(run_dir, name, port) {
        tracing::warn!(agent = name, error = %e, "failed to publish TUI port");
        return;
    }
    tracing::info!(agent = name, port, "TUI socket ready");

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

        let (rx, pty_writer, pty_master, core) = {
            let reg = agent::lock_registry(registry);
            let agent = match reg.get(name) {
                Some(a) => a,
                None => continue,
            };
            let (rx, dump) = agent::subscribe_with_dump(agent);
            if framing::write_frame(&mut stream, &dump).is_err() {
                continue;
            }
            (
                rx,
                Arc::clone(&agent.pty_writer),
                Arc::clone(&agent.pty_master),
                Arc::clone(&agent.core),
            )
        };

        let mut write_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let n = name.to_string();
        // fire-and-forget: per-client TUI output forwarder. Loop exits when
        // the broadcast subscriber rx drops (agent removed via
        // delete_transaction) or when frame write fails (client disconnect).
        // No graceful join needed — each client connection is independent.
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
                            if pty_writer.lock().write_all(&data).is_err() {
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
