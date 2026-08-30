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
    /// Why the serving loop ended, when it ended on a failure rather than on
    /// `stop`. Covers every arm that can end it: a failed `accept`, a failed
    /// request read, a poisoned body lock, and a failed response write. Without
    /// a cause recorded here the arm's failure is discarded by `Drop`'s quiet
    /// join and the dependent test fails with no cause attached.
    pub(super) thread_error: Arc<Mutex<Option<String>>>,
    /// #3320: makes the write arm fail. Otherwise only a real socket error
    /// reaches it, which no test can schedule.
    pub(super) inject_write_error: Arc<AtomicBool>,
    /// #3320: makes the request-read arm fail. TWO seams, because the two
    /// outcomes must be told apart: a genuine read error ends the exchange and
    /// has to be reported, while a would-block means only that the request has
    /// not arrived yet and must be waited out.
    pub(super) inject_read_error: Arc<AtomicBool>,
    pub(super) inject_read_would_block: Arc<AtomicBool>,
    /// #3320: forces the LINUX shape — a BLOCKING accepted socket — on any
    /// platform, so the normalisation guard below is reachable off CI.
    pub(super) simulate_blocking_accept: Arc<AtomicBool>,
    /// #3320: makes the mode normalisation itself fail. Otherwise only a real
    /// `fcntl` failure reaches that arm, which no test can schedule.
    pub(super) inject_mode_error: Arc<AtomicBool>,
    /// #3320: set by the serving thread on its way out, so a test can ask
    /// whether the loop ENDED without joining it. Joining a parked thread hangs
    /// the binary instead of reporting the failure.
    pub(super) exited: Arc<AtomicBool>,
    /// #3320: connections accepted AND mode-normalised, i.e. the loop has
    /// reached the read wait. A test that only SLEEPS before acting cannot tell
    /// "parked in the read wait" from "not scheduled yet", and would pass
    /// without exercising anything.
    pub(super) accepted: Arc<AtomicU32>,
    /// #3320: times the INJECTED would-block was produced and retried. Proves
    /// the arm under test was actually taken rather than missed.
    pub(super) injected_would_block_hits: Arc<AtomicU32>,
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
    let inject_read_error = Arc::new(AtomicBool::new(false));
    let inject_read_would_block = Arc::new(AtomicBool::new(false));
    let simulate_blocking_accept = Arc::new(AtomicBool::new(false));
    let inject_mode_error = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicU32::new(0));
    let injected_would_block_hits = Arc::new(AtomicU32::new(0));
    let body_for_thread = Arc::clone(&body);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let inject_for_thread = Arc::clone(&inject_accept_error);
    let thread_error_for_thread = Arc::clone(&thread_error);
    let inject_write_for_thread = Arc::clone(&inject_write_error);
    let inject_read_for_thread = Arc::clone(&inject_read_error);
    let inject_read_would_block_for_thread = Arc::clone(&inject_read_would_block);
    let simulate_blocking_for_thread = Arc::clone(&simulate_blocking_accept);
    let inject_mode_error_for_thread = Arc::clone(&inject_mode_error);
    let exited_for_thread = Arc::clone(&exited);
    let accepted_for_thread = Arc::clone(&accepted);
    let would_block_hits_for_thread = Arc::clone(&injected_would_block_hits);
    let thread = std::thread::spawn(move || {
        // #3320: `stop` is the ONLY termination condition. The wall clock that
        // used to bound this started at construction, so a caller whose setup
        // ran long lost its server before it ever asked for one. `Drop` sets
        // `stop` and joins, so every dropped server is reaped; a leaked one
        // outlives the test binary, which is the right trade for scaffolding —
        // a leak is a bug to fix, not something to paper over with a timeout
        // that silently breaks working tests.
        'serving: while !stop_for_thread.load(Ordering::Acquire) {
            let accepted = if inject_for_thread.load(Ordering::Acquire) {
                Err(std::io::Error::other("#3320 injected accept failure"))
            } else {
                listener.accept()
            };
            match accepted {
                Ok((mut stream, _)) => {
                    if simulate_blocking_for_thread.load(Ordering::Acquire) {
                        // The Linux shape, reproduced anywhere.
                        let _ = stream.set_nonblocking(false);
                    }
                    // #3320: NORMALISE the accepted socket, never inherit it.
                    // The read wait below can only observe `stop` if `read`
                    // returns, and the mode decides whether it does: macOS
                    // inherits the listener's non-blocking flag, Linux does not
                    // (std's `accept4` never asks for `SOCK_NONBLOCK`), and std
                    // normalises neither. Inheriting means the wait is
                    // `stop`-bounded on one platform and unbounded on the other.
                    let normalised = if inject_mode_error_for_thread.load(Ordering::Acquire) {
                        Err(std::io::Error::other("#3320 injected mode failure"))
                    } else {
                        stream.set_nonblocking(true)
                    };
                    // Fail CLOSED: after a failed normalisation the socket's mode
                    // is unknown, and that is precisely the state that can park
                    // the read forever.
                    if let Err(error) = normalised {
                        record_thread_error(
                            &thread_error_for_thread,
                            format!("accepted socket mode normalisation failed: {error}"),
                        );
                        break;
                    }
                    accepted_for_thread.fetch_add(1, Ordering::AcqRel);
                    // #3320: WAIT for the request instead of answering blind.
                    // A single attempt raced every request, and losing that race
                    // STRANDS the response: an unread request makes the close
                    // send an RST rather than a FIN, so the client loses what was
                    // written. `stop` still wins, so `Drop` cannot hang behind a
                    // client that never speaks.
                    let mut request = [0_u8; 2048];
                    let read = loop {
                        if stop_for_thread.load(Ordering::Acquire) {
                            break 'serving;
                        }
                        let attempt = if inject_read_for_thread.load(Ordering::Acquire) {
                            Err(std::io::Error::other("#3320 injected read failure"))
                        } else if inject_read_would_block_for_thread.load(Ordering::Acquire) {
                            would_block_hits_for_thread.fetch_add(1, Ordering::AcqRel);
                            Err(std::io::Error::new(
                                std::io::ErrorKind::WouldBlock,
                                "#3320 injected read would-block",
                            ))
                        } else {
                            stream.read(&mut request)
                        };
                        match attempt {
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                std::thread::sleep(StdDuration::from_millis(5));
                            }
                            settled => break settled,
                        }
                    };
                    // Anything else IS a failure, and ends the exchange with a
                    // cause — the arm the review found swallowed.
                    if let Err(error) = read {
                        record_thread_error(
                            &thread_error_for_thread,
                            format!("request read failed: {error}"),
                        );
                        break;
                    }
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
                        // #3320: this socket is non-blocking, so a response
                        // larger than the send buffer comes back as a PARTIAL
                        // write plus `WouldBlock` — which `write_all` reports as
                        // a failure, stranding the rest of the body. Resume from
                        // the remaining bytes and wait the transient kinds out,
                        // exactly as the read wait above does. `stop` stays the
                        // ONLY bound, and every other error still ends the
                        // exchange with its cause recorded.
                        let bytes = response.as_bytes();
                        let mut sent = 0;
                        loop {
                            if stop_for_thread.load(Ordering::Acquire) {
                                break 'serving;
                            }
                            match stream.write(&bytes[sent..]) {
                                Ok(0) => {
                                    break Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                                }
                                Ok(n) => {
                                    sent += n;
                                    if sent == bytes.len() {
                                        break Ok(());
                                    }
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::Interrupted
                                    ) =>
                                {
                                    std::thread::sleep(StdDuration::from_millis(5));
                                }
                                Err(error) => break Err(error),
                            }
                        }
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
        exited_for_thread.store(true, Ordering::Release);
    });
    PullListServer {
        base_url,
        body,
        requests,
        stop,
        inject_accept_error,
        thread_error,
        inject_write_error,
        inject_read_error,
        inject_read_would_block,
        simulate_blocking_accept,
        inject_mode_error,
        exited,
        accepted,
        injected_would_block_hits,
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

/// #3320: wait for a counter the SERVING THREAD advances, so a test acts on a
/// proven event instead of on elapsed time. A fixed sleep cannot tell "the
/// thread reached the state under test" from "the thread has not run yet", and a
/// pin that cannot tell those apart passes when nothing happened — the defect
/// class this branch exists to remove, one level up in the test itself.
fn wait_for_at_least(counter: &Arc<AtomicU32>, target: u32, budget: StdDuration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if counter.load(Ordering::Acquire) >= target {
            return true;
        }
        std::thread::sleep(StdDuration::from_millis(5));
    }
    counter.load(Ordering::Acquire) >= target
}

/// #3320: wait for the serving thread to RECORD why it died. Same reason as
/// `wait_for_at_least`, mirrored: a fixed sleep here makes a slow thread a FALSE
/// FAILURE rather than a false pass.
fn wait_for_cause(slot: &Arc<Mutex<Option<String>>>, budget: StdDuration) -> Option<String> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(cause) = slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Some(cause);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(StdDuration::from_millis(5));
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
/// task creation, permission changes — before the sweep issues its request. On a
/// loaded runner that setup outlives the window: the thread is already gone, the
/// fetch fails, the task is never closed. The confirmed mechanism behind the
/// `left: Open, right: Done` flake.
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
/// The accept loop ended `Err(error) => panic!(...)` while `Drop` joined with
/// `.unwrap()`, so an accept error made the JOIN panic — during a test's own
/// unwind that is an abort, with the real assertion lost behind it. The trigger
/// is not hypothetical: `WouldBlock` is handled, so that arm fires on a genuine
/// syscall failure, realistically `EMFILE`/`ENFILE` from a parallel run where
/// every one of these tests binds its own listener — the SAME load that produces
/// RED-A's timing miss.
#[test]
fn dead_server_thread_does_not_panic_on_drop_3320() {
    let server = pull_list_server("[]".to_string());
    server.inject_accept_error.store(true, Ordering::Release);

    // Without this the test would pass even if the injection did nothing, and a
    // pin that cannot tell those apart is the exact defect class this branch is
    // here to remove. WAIT for the record rather than sleeping a fixed amount:
    // a sleep short enough to keep the suite fast is also short enough to expire
    // before a loaded machine schedules the serving thread.
    assert!(
        wait_for_cause(&server.thread_error, StdDuration::from_secs(5)).is_some(),
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
/// Since the accepted socket is now normalised to non-blocking on every
/// platform, `write_all` can return `WouldBlock` here for real — measured as
/// unreachable at these response sizes, and tracked separately.
#[test]
fn write_side_failure_is_reported_not_swallowed_3320() {
    let server = pull_list_server("[\"body-3320\"]".to_string());
    server.inject_write_error.store(true, Ordering::Release);

    // Drive one request so the write arm is entered. The response is expected to
    // fail; what is under test is whether that failure is VISIBLE afterwards.
    let _ = get(&server.base_url);

    let cause = wait_for_cause(&server.thread_error, StdDuration::from_secs(5));
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

/// #3320 RED-E: a response too large for the socket send buffer must be
/// RESUMED, not abandoned.
///
/// The note on RED-C above deferred exactly this. The accepted socket is
/// normalised to NON-BLOCKING, so a response bigger than the send buffer makes
/// the write return `WouldBlock` after a PARTIAL write — and `write_all`
/// reports that as a failure. The exchange then ends with a recorded cause and
/// the client receives a truncated body: the same swallowed-write class this
/// file already guards on the read side, arriving through the OS rather than
/// through a seam.
///
/// Nothing is injected. The client sends its request and then STALLS before
/// draining, so the send buffer genuinely fills and the write genuinely blocks.
#[test]
fn a_would_block_write_is_resumed_not_abandoned_3320() {
    // Comfortably larger than the socket send buffer plus the client receive
    // buffer, so the block is a certainty rather than a race.
    let body = "x".repeat(4 * 1024 * 1024);
    let server = pull_list_server(body.clone());

    let addr = server.base_url.trim_start_matches("http://").to_string();
    let mut stream = std::net::TcpStream::connect(&addr).expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("request");
    // Stall so the server reaches its write arm and fills the buffer before
    // anything is drained.
    std::thread::sleep(StdDuration::from_millis(250));
    let mut out = String::new();
    stream.read_to_string(&mut out).expect("response");

    let cause = server
        .thread_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        cause.is_none(),
        "#3320: a would-block on a large response is not a failure — it means \
         the rest of the body still has to go out; got {cause:?}"
    );
    assert!(
        out.ends_with(&body),
        "#3320: the response was truncated — received {} bytes, the body alone \
         is {}",
        out.len(),
        body.len()
    );

    drop(server);
}

/// #3320 RED-D: a REQUEST-READ failure must be reported, not swallowed.
///
/// The accept, body-lock and write arms all record why the loop ended; the read
/// did not — its `Result` was discarded outright, and the `thread_error` doc
/// claimed a coverage this arm never had. Found by the secondary review of
/// `ac9a2372`.
///
/// The failure is INJECTED. What is pinned is this branch's behaviour, not the
/// OS's willingness to fail a read at that instant.
#[test]
fn read_side_failure_is_reported_not_swallowed_3320() {
    let server = pull_list_server("[\"body-3320\"]".to_string());
    server.inject_read_error.store(true, Ordering::Release);

    // Drive one request so the read arm is entered. What is under test is
    // whether the failure is VISIBLE afterwards, not what the client received.
    let _ = get(&server.base_url);

    let cause = wait_for_cause(&server.thread_error, StdDuration::from_secs(5));
    assert!(
        cause
            .as_deref()
            .is_some_and(|c| c.contains("injected read failure")),
        "#3320: a request-read failure must be RECORDED, so the test that \
         depended on this server can say why it died; got {cause:?}"
    );

    // And teardown must still be quiet.
    drop(server);
}

/// #3320 RED-E: a would-block read means the request has NOT ARRIVED YET, and
/// the stand-in must wait for it instead of answering a request it never read.
///
/// Not a hypothetical arm: macOS returns `EAGAIN` here from a REAL `read`, so
/// one discarded attempt raced every request — and losing that race STRANDS the
/// response (see the accept loop). A second, independent producer of the same
/// `left: Open, right: Done` flake, in the arm nobody was looking at. It also
/// answers the reviewer's Windows concern by measurement rather than guess:
/// waiting is correct on every platform, and never waiting is already wrong on
/// this one.
#[test]
fn a_would_block_read_is_waited_out_not_answered_blind_3320() {
    let server = pull_list_server("[\"sentinel-3320\"]".to_string());
    server
        .inject_read_would_block
        .store(true, Ordering::Release);

    // The client's request is on the wire, and the stand-in can only see a
    // would-block. `get` blocks until the response, so it has to run beside us.
    let base_url = server.base_url.clone();
    let client = std::thread::spawn(move || get(&base_url));

    // Wait for the INJECTED would-block to actually fire. A fixed sleep here
    // would pass even if the serving thread had not run at all, proving nothing
    // about the arm under test.
    assert!(
        wait_for_at_least(
            &server.injected_would_block_hits,
            1,
            StdDuration::from_secs(5)
        ),
        "#3320 fixture check: the injected would-block never fired, so nothing \
         below is evidence about waiting one out"
    );

    assert_eq!(
        server.requests.load(Ordering::Acquire),
        0,
        "#3320: the request has not been read, so nothing may be counted as \
         served — answering here is what strands the response behind an RST"
    );

    // Now let the read succeed: service must RESUME, not have died.
    server
        .inject_read_would_block
        .store(false, Ordering::Release);
    let served = client.join().expect("#3320: client thread");
    assert!(
        served.as_deref().unwrap_or("").contains("sentinel-3320"),
        "#3320: after the request arrives the stand-in must serve it; got \
         {served:?}"
    );
    assert_eq!(
        server.requests.load(Ordering::Acquire),
        1,
        "#3320: exactly one request should have been served"
    );
    let cause = server.thread_error.lock().unwrap().clone();
    assert!(
        cause.is_none(),
        "#3320: waiting for a request is not a death and must not be recorded \
         as one — a wrong cause is worse than none, because it gets believed; \
         got {cause:?}"
    );
}

/// #3320: waiting for a request must not wedge teardown — on EITHER platform.
///
/// The wait is unbounded by time on purpose (a clock is what this branch
/// removes), so `stop` has to end it — which is only reachable if `read`
/// RETURNS, making the accepted socket's mode load-bearing (see the accept
/// loop). CI killed this exact case at 120s.
///
/// So the test FORCES the Linux shape, and deliberately does not use `Drop` to
/// observe the outcome: joining a parked thread hangs the binary instead of
/// reporting the failure, which is the one thing worse than the flake this
/// branch replaced. It asks the thread whether it ENDED, and on failure leaks
/// the server rather than joining it.
#[test]
fn a_silent_client_does_not_wedge_teardown_3320() {
    let server = pull_list_server("[]".to_string());
    server
        .simulate_blocking_accept
        .store(true, Ordering::Release);
    let addr = server.base_url.trim_start_matches("http://").to_string();

    // Connect and say NOTHING, so the stand-in is parked in the read wait.
    let _silent = std::net::TcpStream::connect(&addr).expect("#3320: connect");

    // PROVE it is parked there before releasing `stop`. Sleeping instead would
    // let this pass on a loaded machine where the serving thread simply had not
    // run yet: it would then see `stop` before ever accepting, set `exited`, and
    // report success without touching the mode normalisation under test.
    assert!(
        wait_for_at_least(&server.accepted, 1, StdDuration::from_secs(5)),
        "#3320 fixture check: the stand-in never accepted the connection, so \
         the teardown below would prove nothing about the read wait"
    );

    // What `Drop` does, minus the join we cannot safely perform yet.
    server.stop.store(true, Ordering::Release);
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while !server.exited.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(StdDuration::from_millis(10));
    }

    if !server.exited.load(Ordering::Acquire) {
        // Leak rather than join: `Drop` joins, and joining a thread parked in a
        // blocking read hangs the whole binary in place of this message.
        std::mem::forget(server);
        panic!(
            "#3320: the read wait never observed `stop` — the accepted socket's \
             blocking mode was inherited rather than normalised, so `read` never \
             returned and `Drop`'s join would have wedged the test binary"
        );
    }

    // The thread is gone, so the join in `Drop` is now a formality.
    drop(server);
}

/// #3320: if the mode normalisation itself fails, say so and stop.
///
/// It is the guard the whole read wait rests on. A failure there leaves a socket
/// whose blocking mode is unknown, which is precisely the state that can park
/// the read forever — so it must end the exchange with a cause, never carry on
/// hoping.
#[test]
fn accepted_socket_mode_failure_is_reported_not_ignored_3320() {
    let server = pull_list_server("[\"body-3320\"]".to_string());
    server.inject_mode_error.store(true, Ordering::Release);

    let _ = get(&server.base_url);

    let cause = wait_for_cause(&server.thread_error, StdDuration::from_secs(5));
    assert!(
        cause
            .as_deref()
            .is_some_and(|c| c.contains("injected mode failure")),
        "#3320: a failed mode normalisation must be RECORDED — the read wait \
         cannot be trusted after it; got {cause:?}"
    );
    assert_eq!(
        server.requests.load(Ordering::Acquire),
        0,
        "#3320: the exchange ended before service, so nothing may be counted"
    );
}

/// #3320: the recorder's own two documented properties, pinned.
///
/// `record_thread_error` claims it never panics and that the FIRST cause wins.
/// Nothing tested either — a documented property with no defender, the same
/// shape this branch exists to remove — and both failure modes are silent: a
/// recorder that panics IS the failure it reports, and one that overwrites hands
/// the reader the wrong reason for the death it is explaining.
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
