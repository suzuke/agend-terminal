//! Sprint 25 P3 — Slow-loris timeout source-grep invariant guards.
//!
//! Behavioral tests live in src/api/mod.rs (unit tests with real
//! api::serve). These integration tests verify code-shape invariants.

#![allow(clippy::unwrap_used)]

#[test]
fn api_tcp_read_timeout_is_tight() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/mod.rs"),
    )
    .expect("read api/mod.rs");
    let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
    let prod = &src[..cutoff];
    assert!(prod.contains("unwrap_or(5)") && prod.contains("read_timeout"));
    assert!(!prod.contains("set_read_timeout(Some(std::time::Duration::from_secs(30)))"));
}

#[test]
fn api_tcp_timeout_has_env_override() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/mod.rs"),
    )
    .expect("read api/mod.rs");
    let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
    let prod = &src[..cutoff];
    assert!(prod.contains("AGEND_API_READ_TIMEOUT_SECS"));
}
