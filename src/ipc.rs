//! Cross-platform IPC over TCP loopback.
//!
//! The daemon and each agent bind an OS-assigned port on 127.0.0.1 and write
//! it to `{run_dir}/{name}.port` (api.port for the daemon's control socket).
//! Clients discover ports by reading those files.
//!
//! Rationale: Unix domain sockets are not available on stable Rust for
//! Windows; named pipes would require a separate code path. TCP loopback is
//! portable and keeps a single code path across platforms. Binding is
//! restricted to 127.0.0.1 so the ports are never reachable off-host.

use anyhow::{Context, Result};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

pub const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Name used for the daemon API port file (`api.port`).
pub const API_NAME: &str = "api";

/// Bind a TCP listener on 127.0.0.1 with an OS-assigned port.
pub fn bind_loopback() -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((LOOPBACK, 0)))
}

/// Return the port a listener is bound to.
pub fn local_port(listener: &TcpListener) -> u16 {
    listener.local_addr().map(|a| a.port()).unwrap_or(0)
}

/// Path for a named port file inside run_dir.
fn port_path(run_dir: &Path, name: &str) -> std::path::PathBuf {
    run_dir.join(format!("{name}.port"))
}

/// Write `port` to `{run_dir}/{name}.port` atomically (tmp + rename).
pub fn write_port(run_dir: &Path, name: &str, port: u16) -> io::Result<()> {
    let final_path = port_path(run_dir, name);
    let tmp = run_dir.join(format!(".{name}.port.tmp"));
    std::fs::write(&tmp, port.to_string())?;
    std::fs::rename(&tmp, &final_path)
}

/// Read a port from `{run_dir}/{name}.port`. Returns None if missing/malformed.
pub fn read_port(run_dir: &Path, name: &str) -> Option<u16> {
    std::fs::read_to_string(port_path(run_dir, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Best-effort removal of a port file.
pub fn remove_port(run_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(port_path(run_dir, name));
}

/// Connect a TcpStream to `127.0.0.1:port`, applying TCP_NODELAY.
fn connect_port(port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(SocketAddr::from((LOOPBACK, port)))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Connect to the active daemon's API port.
pub fn connect_api(home: &Path) -> Result<TcpStream> {
    let run =
        crate::daemon::find_active_run_dir(home).context("no active daemon (run dir not found)")?;
    let port = read_port(&run, API_NAME).context("daemon api.port missing or invalid")?;
    connect_port(port).map_err(Into::into)
}

/// Connect to a named agent's TUI port.
pub fn connect_agent(home: &Path, name: &str) -> Result<TcpStream> {
    let run =
        crate::daemon::find_active_run_dir(home).context("no active daemon (run dir not found)")?;
    let port = read_port(&run, name)
        .with_context(|| format!("agent '{name}' port file missing or invalid"))?;
    connect_port(port).map_err(Into::into)
}

/// Enumerate agent names whose `*.port` files exist in `run`, excluding the
/// daemon's own `api.port`. Returned in filesystem order — callers that need
/// a stable list should sort.
pub fn list_agent_ports(run: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(run) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".port").map(str::to_string)
        })
        .filter(|n| n != API_NAME)
        .collect()
}

/// Probe whether an agent's TCP listener is reachable (for `doctor`).
pub fn probe_agent(run: &Path, name: &str) -> bool {
    match read_port(run, name) {
        Some(port) => TcpStream::connect_timeout(
            &SocketAddr::from((LOOPBACK, port)),
            Duration::from_millis(200),
        )
        .is_ok(),
        None => false,
    }
}

/// Probe whether a run_dir's daemon API is reachable. Used to distinguish a
/// live daemon from a stale run_dir whose PID has been reused: `is_pid_alive`
/// may say true, but only a real agend daemon answers on the api port.
///
/// 200ms timeout matches `probe_agent`. A refused connection on a loopback
/// port returns much faster than the timeout, so this is cheap in the common
/// "dead daemon" case.
pub fn probe_api(run: &Path) -> bool {
    match read_port(run, API_NAME) {
        Some(port) => TcpStream::connect_timeout(
            &SocketAddr::from((LOOPBACK, port)),
            Duration::from_millis(200),
        )
        .is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ipc-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).expect("create tmp");
        dir
    }

    #[test]
    fn bind_loopback_assigns_port() {
        let listener = bind_loopback().expect("bind");
        let port = local_port(&listener);
        assert!(port > 0);
        let addr = listener.local_addr().expect("addr");
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn write_and_read_port_roundtrip() {
        let dir = tmp_dir("roundtrip");
        write_port(&dir, "api", 12345).expect("write");
        assert_eq!(read_port(&dir, "api"), Some(12345));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_port_missing_returns_none() {
        let dir = tmp_dir("missing");
        assert_eq!(read_port(&dir, "nope"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_port_malformed_returns_none() {
        let dir = tmp_dir("malformed");
        std::fs::write(dir.join("x.port"), "not-a-port").expect("write");
        assert_eq!(read_port(&dir, "x"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_port_is_best_effort() {
        let dir = tmp_dir("remove");
        remove_port(&dir, "absent"); // must not panic
        write_port(&dir, "a", 1).expect("write");
        remove_port(&dir, "a");
        assert_eq!(read_port(&dir, "a"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_api_matches_api_port_listener() {
        // probe_api is the gatekeeper that distinguishes a live daemon from
        // a stale run dir whose PID got recycled. It must return true when
        // api.port points at an actual listener and false when the file is
        // missing (stale daemon, port never written, or written but daemon
        // exited so the OS released the port).
        let dir = tmp_dir("probe_api");
        let listener = bind_loopback().expect("bind");
        let port = local_port(&listener);
        write_port(&dir, API_NAME, port).expect("write api.port");

        let handle = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        assert!(probe_api(&dir), "must succeed while listener is live");
        handle.join().ok();

        remove_port(&dir, API_NAME);
        assert!(
            !probe_api(&dir),
            "must fail once api.port file is gone (stale run dir)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_agent_connects_then_fails_after_close() {
        let dir = tmp_dir("probe");
        let listener = bind_loopback().expect("bind");
        let port = local_port(&listener);
        write_port(&dir, "dev", port).expect("write");

        // Accept once in background so connect_timeout succeeds cleanly.
        let handle = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        assert!(probe_agent(&dir, "dev"));
        handle.join().ok();

        // After listener dropped (and accepted), connects may still succeed to
        // the closed port on some OS buffers; the key invariant we exercise is
        // that the file-missing case returns false.
        remove_port(&dir, "dev");
        assert!(!probe_agent(&dir, "dev"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
