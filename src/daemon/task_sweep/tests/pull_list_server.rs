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
    /// Why the serving loop ended, when it ended on a failure rather than on
    /// `stop`. Covers BOTH arms: a failed `accept`, and a failed body read or
    /// response write. Without the write arm recorded here, its panic is
    /// discarded by `Drop`'s quiet join and the dependent test fails with no
    /// cause attached.
    pub(super) thread_error: Arc<Mutex<Option<String>>>,
    /// #3320: makes the write arm fail. Otherwise only a real socket error
    /// reaches it, which no test can schedule.
    pub(super) inject_write_error: Arc<AtomicBool>,
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
    let thread_error = Arc::new(Mutex::new(None));
    let inject_write_error = Arc::new(AtomicBool::new(false));
    let body_for_thread = Arc::clone(&body);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let inject_for_thread = Arc::clone(&inject_accept_error);
    let thread_error_for_thread = Arc::clone(&thread_error);
    let inject_write_for_thread = Arc::clone(&inject_write_error);
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
                    // #3320: record-and-break, never panic. A panic here dies
                    // inside the server thread, and `Drop`'s quiet join discards
                    // it — the dependent test then fails with no cause.
                    let body = match body_for_thread.lock() {
                        Ok(guard) => guard.clone(),
                        Err(poisoned) => {
                            record_thread_error(
                                &thread_error_for_thread,
                                format!("body lock poisoned: {poisoned}"),
                            );
                            break;
                        }
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let written = if inject_write_for_thread.load(Ordering::Acquire) {
                        Err(std::io::Error::other("#3320 injected write failure"))
                    } else {
                        stream.write_all(response.as_bytes())
                    };
                    if let Err(error) = written {
                        record_thread_error(
                            &thread_error_for_thread,
                            format!("response write failed: {error}"),
                        );
                        break;
                    }
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
                    record_thread_error(
                        &thread_error_for_thread,
                        format!("accept failed: {error}"),
                    );
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
        thread_error,
        inject_write_error,
        thread: Some(thread),
    }
}

/// Record why the serving loop ended. Never panics — a panic in the error path
/// would be the very failure this exists to report. The FIRST cause wins: it is
/// the one that ended the loop.
fn record_thread_error(slot: &Arc<Mutex<Option<String>>>, cause: String) {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(cause);
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
        server.thread_error.lock().unwrap().is_some(),
        "#3320 fixture check: the loop must have OBSERVED the injected accept \
         failure and recorded it, not simply carried on"
    );

    // The property under test: this returns rather than panicking.
    drop(server);
}

/// #3320 RED-C: a WRITE-side failure must be reported, not swallowed.
///
/// The accept arm records its cause; the write arm did not. Its `unwrap` panics
/// the server thread, and `Drop`'s quiet join then discards that panic — so the
/// dependent test fails on `served.is_ok()` or a request count with NO cause
/// attached. This is not simply "worse than the abort it replaced": the abort
/// BURIED the real assertion, while this leaves the assertion visible but
/// STRIPS its cause. Both are wrong, in opposite directions, and recording the
/// cause is what gets both right.
///
/// It matters most on Windows, where these tests have never yet run: an accepted
/// socket may inherit the listener's non-blocking mode there, in which case
/// `write_all` can return `WouldBlock` — a real, uninjected instance of exactly
/// this arm, on the one platform with no evidence either way.
#[test]
fn write_side_failure_is_reported_not_swallowed_3320() {
    let server = pull_list_server("[\"body-3320\"]".to_string());
    server.inject_write_error.store(true, Ordering::Release);

    // Drive one request so the write arm is entered. The response is expected to
    // fail; what is under test is whether that failure is VISIBLE afterwards.
    let _ = get(&server.base_url);
    std::thread::sleep(StdDuration::from_millis(150));

    let cause = server.thread_error.lock().unwrap().clone();
    assert!(
        cause
            .as_deref()
            .is_some_and(|c| c.contains("injected write failure")),
        "#3320: a write-side failure must be RECORDED, so the test that depended \
         on this server can say why it died; got {cause:?}"
    );

    // And teardown must still be quiet.
    drop(server);
}

/// #3320: the recorder's own two documented properties, pinned.
///
/// `record_thread_error` claims it never panics and that the FIRST cause wins.
/// Nothing tested either, which is the same shape of defect this branch exists
/// to remove — a documented property with no defender — and it matters here
/// because both failure modes are silent: a recorder that panics IS the failure
/// it exists to report, and one that overwrites hands the reader the wrong
/// reason for the death it is explaining.
#[test]
fn recorded_cause_keeps_the_first_and_never_panics_3320() {
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    record_thread_error(&slot, "the cause that ended the loop".to_string());
    record_thread_error(&slot, "a later, misleading cause".to_string());
    assert_eq!(
        slot.lock().unwrap().as_deref(),
        Some("the cause that ended the loop"),
        "#3320: the FIRST cause must win — it is the one that ended the loop; a \
         later one describes the aftermath and would misattribute the failure"
    );

    // A poisoned slot must still record, not panic: the poisoning is itself a
    // symptom of the failure being reported.
    let poisoned: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let holder = Arc::clone(&poisoned);
    let _ = std::thread::Builder::new()
        .name("poison-3320".into())
        .spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("#3320: poison the slot");
        })
        .expect("spawn")
        .join();
    record_thread_error(&poisoned, "recorded despite poison".to_string());
    assert_eq!(
        poisoned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref(),
        Some("recorded despite poison"),
        "#3320: the recorder must survive a poisoned slot — a panic in the error \
         path would be the very failure it exists to report"
    );
}
