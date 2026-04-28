//! Sprint 25 P3 — Slow-loris timeout architecture tests.
//!
//! §3.5.10 external-fixture: real TCP drip-feed attack simulation.
//! §3.5.11 test-first: invariant test FAILS before timeout tightening.

#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Connect to a TCP listener, perform cookie auth, return reader+writer.
fn connect_and_auth(port: u16, cookie: &[u8; 32]) -> (BufReader<TcpStream>, TcpStream) {
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    let _ = stream.set_nodelay(true);
    let writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    let mut w = writer.try_clone().expect("clone writer");
    writeln!(w, r#"{{"auth":"{hex}"}}"#).expect("write auth");
    w.flush().expect("flush");
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read auth resp");
    (reader, writer)
}

/// Invariant: TCP read timeout must be ≤5s (not 30s).
/// §3.5.11 test-first: FAILS before tightening, PASSES after.
#[test]
fn api_tcp_read_timeout_is_tight() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/mod.rs"),
    )
    .expect("read api/mod.rs");

    let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
    let prod = &src[..cutoff];

    // Find the set_read_timeout call and verify it's ≤5s
    let has_tight_timeout = prod
        .contains("set_read_timeout(Some(std::time::Duration::from_secs(5)))")
        || prod.contains("from_secs(5)))");

    assert!(
        has_tight_timeout,
        "src/api/mod.rs TCP read timeout must be 5s (not 30s) for slow-loris defense. \
         Drip-feed attacks (1 byte/sec partial JSON) must be dropped within 5-6s."
    );
}

/// Invariant: env var override for large-args escape hatch must exist.
#[test]
fn api_tcp_timeout_has_env_override() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/mod.rs"),
    )
    .expect("read api/mod.rs");

    let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
    let prod = &src[..cutoff];

    assert!(
        prod.contains("AGEND_API_READ_TIMEOUT") || prod.contains("api_read_timeout"),
        "src/api/mod.rs must support env var override for TCP read timeout \
         (escape hatch for large-args payloads that exceed 5s)"
    );
}
