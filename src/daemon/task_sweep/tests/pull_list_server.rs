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
use std::time::Duration as StdDuration;

pub(super) struct PullListServer {
    pub(super) base_url: String,
    pub(super) body: Arc<Mutex<String>>,
    pub(super) requests: Arc<AtomicU32>,
    pub(super) stop: Arc<AtomicBool>,
    /// #3320: makes the non-`WouldBlock` accept arm reachable. Without it that
    /// arm is only entered on a real syscall failure (realistically `EMFILE`
    /// under a parallel run), which no test can schedule.
    pub(super) inject_accept_error: Arc<AtomicBool>,
    /// Set when the loop ended on a genuine accept failure, so a test can assert
    /// on it instead of discovering it as an abort.
    pub(super) accept_error: Arc<Mutex<Option<String>>>,
    pub(super) thread: Option<JoinHandle<()>>,
}

impl Drop for PullListServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // #3320: teardown stays deterministic — we still JOIN, so the thread
            // is gone before the test returns. But a thread that panicked must
            // not panic us in turn: that would be a panic during unwind, i.e. an
            // abort that buries the real failure.
            let _ = thread.join();
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
    let accept_error = Arc::new(Mutex::new(None));
    let body_for_thread = Arc::clone(&body);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let inject_for_thread = Arc::clone(&inject_accept_error);
    let accept_error_for_thread = Arc::clone(&accept_error);
    let thread = std::thread::spawn(move || {
        // #3320: `stop` is the ONLY termination condition. The wall clock that
        // used to bound this started at construction, so a caller whose setup
        // ran long lost its server before it ever asked for one. `Drop` sets
        // `stop` and joins, so every dropped server is reaped; a leaked one
        // outlives the test binary, which is the right trade for scaffolding —
        // a leak is a bug to fix, not something to paper over with a timeout
        // that silently breaks working tests.
        while !stop_for_thread.load(Ordering::Acquire) {
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
                // #3320: BREAK, do not panic. A panicking server thread turns
                // `Drop`'s join into a panic; during a test's own unwind that is
                // an abort that hides the real assertion. A dead stand-in should
                // fail the assertion that depended on it, not replace that
                // failure with an abort. The error is recorded so a test can
                // still assert it happened.
                Err(error) => {
                    *accept_error_for_thread.lock().unwrap() = Some(error.to_string());
                    break;
                }
            }
        }
    });
    PullListServer {
        base_url,
        body,
        requests,
        stop,
        inject_accept_error,
        accept_error,
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

    // Without this the test would pass even if the injection did nothing, and a
    // pin that cannot tell those apart is the exact defect class this branch is
    // here to remove.
    assert!(
        server.accept_error.lock().unwrap().is_some(),
        "#3320 fixture check: the loop must have OBSERVED the injected accept \
         failure and recorded it, not simply carried on"
    );

    // The property under test: this returns rather than panicking.
    drop(server);
}
