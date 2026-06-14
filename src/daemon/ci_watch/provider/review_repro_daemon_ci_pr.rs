//! Review-repro tests (scope: daemon-ci-pr) for `ci_watch::provider`.
//!
//! Finding: "Branch (and repo) names interpolated unencoded into CI provider
//! API query strings". `GitHubCiProvider::poll_runs` (and siblings) build the
//! REST URL by raw `format!` interpolation of `branch` into the query string
//! (`?branch={branch}&per_page=…`) with NO percent-encoding. Git ref names may
//! legally contain `&` and `=`, so a branch such as `feat&per_page=1` injects a
//! spurious query parameter: the intended `branch=` filter is corrupted and the
//! GitHub API silently drops/overrides it, returning unrelated runs → wrong CI
//! verdicts / false PR-terminal auto-clears.
//!
//! Method: behavioral_unit driven over the real production entry point
//! (`poll_runs`) against a local one-shot HTTP mock that captures the raw
//! request line. We assert the branch's `&`/`=` reach the wire percent-encoded
//! (`feat%26per_page%3D1`) — RED today (the raw `feat&per_page=1` injection is
//! sent verbatim), GREEN once the provider percent-encodes `branch` before
//! interpolation. Mirrors the existing `github_mock_server` /
//! `github_check_pr_terminal_uses_owner_prefix_in_head_query` harness in
//! `poller_tests.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// `poll_runs` is a `CiProvider` trait method — bring the trait into scope so it
// is callable on the concrete `GitHubCiProvider`.
use super::CiProvider;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One-shot HTTP mock server: accepts a single connection, captures the raw
/// HTTP request line (method + path+query), and returns the supplied JSON body
/// with a 200. Returns the bound port, the server thread handle, and the
/// shared capture slot holding the request-line path.
#[allow(clippy::type_complexity)]
fn capture_request_path(
    response_body: &str,
) -> (u16, std::thread::JoinHandle<()>, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured.clone();
    let body = response_body.to_string();

    // fire-and-forget: test-local one-shot TCP listener thread; its JoinHandle is
    // returned and joined by the caller — never escapes the test.
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).expect("read");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        *captured_clone.lock().expect("lock") = Some(path);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    (port, handle, captured)
}

#[test]
#[ignore = "branch-query-encoding: red until fix; remove #[ignore] after fix to confirm"]
fn github_poll_runs_percent_encodes_branch_in_query_daemon_ci_pr() {
    // A branch name that is a LEGAL git ref but contains query metacharacters.
    // Unencoded, the trailing `&per_page=1` injects a second `per_page` param
    // and the `=` corrupts the `branch=` filter value.
    let branch = "feat&per_page=1";

    // Empty-but-valid response body — the test only inspects the captured URL.
    let (port, handle, captured) = capture_request_path(r#"{"workflow_runs":[]}"#);
    let provider = super::GitHubCiProvider::with_base_url(format!("http://127.0.0.1:{port}"))
        .expect("provider");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    // Drive the real production entry point.
    let _ = rt.block_on(provider.poll_runs("acme/widgets", branch));

    handle.join().expect("mock thread join");
    let path = captured
        .lock()
        .expect("lock")
        .take()
        .expect("request captured");

    // Sanity: we hit the runs endpoint at all.
    assert!(
        path.contains("/repos/acme/widgets/actions/runs"),
        "URL must target the repo's actions/runs endpoint; got: {path}"
    );

    // CORRECT behavior: the branch's `&` and `=` must reach the wire
    // percent-encoded so the whole branch stays the VALUE of `branch=`.
    // RED today (raw `feat&per_page=1` is interpolated verbatim).
    assert!(
        path.contains("branch=feat%26per_page%3D1"),
        "branch must be percent-encoded into the query (`&`→%26, `=`→%3D) so the \
         filter value isn't corrupted; got: {path}"
    );

    // Defensive cross-check: the RAW injection (an unencoded `&per_page=1`
    // coming from the branch) must NOT appear — that is the corruption this
    // finding is about. A fixed encoder makes both `&` and `=` inside the
    // branch disappear from the literal form.
    assert!(
        !path.contains("branch=feat&per_page=1"),
        "raw unencoded branch injection corrupts the query string; got: {path}"
    );
}
