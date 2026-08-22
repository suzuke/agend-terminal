//! Test-only HTTP stand-in for the pull-request list endpoint the sweep fetches.
//!
//! Re-homed out of `src/daemon/task_sweep.rs` (#3320): that file sits exactly on
//! the 2500-line anti-monolith ceiling `tests/src_file_size_invariant.rs`
//! enforces, so scaffolding living there blocked every further change to it.
//! Test scaffolding also simply belongs beside the tests, not inside the
//! production module.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration as StdDuration, Instant};

pub(super) struct PullListServer {
    pub(super) base_url: String,
    pub(super) body: Arc<Mutex<String>>,
    pub(super) requests: Arc<AtomicU32>,
    pub(super) stop: Arc<AtomicBool>,
    /// #3320: makes the non-`WouldBlock` accept arm reachable. Without it that
    /// arm is only entered on a real syscall failure (realistically `EMFILE`
    /// under a parallel run), which no test can schedule.
    pub(super) inject_accept_error: Arc<AtomicBool>,
    pub(super) thread: Option<JoinHandle<()>>,
}

impl Drop for PullListServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl PullListServer {
    pub(super) fn set_body(&self, body: String) {
        *self.body.lock().unwrap() = body;
    }
}

pub(super) fn pull_list_server(body: String) -> PullListServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let body = Arc::new(Mutex::new(body));
    let requests = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let inject_accept_error = Arc::new(AtomicBool::new(false));
    let body_for_thread = Arc::clone(&body);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let inject_for_thread = Arc::clone(&inject_accept_error);
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !stop_for_thread.load(Ordering::Acquire) && Instant::now() < deadline {
            let accepted = if inject_for_thread.load(Ordering::Acquire) {
                Err(std::io::Error::other("#3320 injected accept failure"))
            } else {
                listener.accept()
            };
            match accepted {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 2048];
                    let _ = stream.read(&mut request);
                    let body = body_for_thread.lock().unwrap().clone();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    requests_for_thread.fetch_add(1, Ordering::AcqRel);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(StdDuration::from_millis(5));
                }
                Err(error) => panic!("pull-list server accept failed: {error}"),
            }
        }
    });
    PullListServer {
        base_url,
        body,
        requests,
        stop,
        inject_accept_error,
        thread: Some(thread),
    }
}

/// Minimal blocking GET against the stand-in, so a test can ask it for service
/// directly instead of inferring liveness from a whole sweep tick.
fn get(base_url: &str) -> std::io::Result<String> {
    let addr = base_url.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")?;
    let mut out = String::new();
    stream.read_to_string(&mut out)?;
    Ok(out)
}

/// #3320 RED-A: the stand-in's lifetime must be bounded by `stop`, NOT by
/// elapsed time.
///
/// The deadline it replaces was `Instant::now() + 5s` fixed at CONSTRUCTION,
/// while every caller does substantial setup — config, `resolve_sweep_plan`,
/// task creation, permission changes — before the sweep ever issues its
/// request. On a loaded runner that setup outlives the window, the thread is
/// already gone, the fetch fails, and the task is never closed. That is the
/// confirmed mechanism behind the `left: Open, right: Done` flake.
///
/// SLOW BY DESIGN, NOT FLAKY: the gap deliberately outlasts the 5s deadline
/// being removed, and more elapsed time can never make this pass falsely. A
/// cheaper gap would go vacuous the moment anyone reintroduced a seconds-scale
/// bound, which is the failure mode this whole change exists to stop repeating.
#[test]
fn server_serves_after_outlasting_the_old_construction_deadline_3320() {
    let server = pull_list_server("[\"sentinel-3320\"]".to_string());
    std::thread::sleep(StdDuration::from_millis(5_400));

    let served = get(&server.base_url);
    assert!(
        served.is_ok(),
        "#3320: the stand-in stopped listening because time passed, not because \
         it was stopped — every caller's setup happens inside this window: {:?}",
        served.err()
    );
    assert!(
        served.as_deref().unwrap_or("").contains("sentinel-3320"),
        "#3320: still listening but did not serve the body; got {served:?}"
    );
    assert_eq!(
        server.requests.load(Ordering::Acquire),
        1,
        "#3320: exactly one request should have been served"
    );
}

/// #3320 RED-B: a dead server thread must not turn `Drop` into a panic.
///
/// The accept loop ends `Err(error) => panic!(...)` and `Drop` joins with
/// `.unwrap()`, so a thread that hit an accept error makes the JOIN panic. When
/// the test is already unwinding from its own failure that is a panic during
/// unwind — an abort, with the real assertion message lost behind it. The
/// trigger is not hypothetical: on a non-blocking listener `WouldBlock` is
/// handled, so that arm fires on a genuine syscall failure, realistically
/// `EMFILE`/`ENFILE` from a parallel run where every one of these tests binds
/// its own listener — the SAME load that produces RED-A's timing miss.
#[test]
fn dead_server_thread_does_not_panic_on_drop_3320() {
    let server = pull_list_server("[]".to_string());
    server.inject_accept_error.store(true, Ordering::Release);
    // Let the loop reach the injected error.
    std::thread::sleep(StdDuration::from_millis(50));

    // The assertion under test is that this returns rather than panicking.
    drop(server);
}
