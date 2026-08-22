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
    let body_for_thread = Arc::clone(&body);
    let requests_for_thread = Arc::clone(&requests);
    let stop_for_thread = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !stop_for_thread.load(Ordering::Acquire) && Instant::now() < deadline {
            match listener.accept() {
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
        thread: Some(thread),
    }
}
