//! Test isolation invariant — verifies AGEND_TEST_ISOLATION is set
//! in all test helpers that spawn MCP subprocesses or set AGEND_HOME.
//!
//! Sprint 31 P0: prevents cargo test from polluting the real fleet.

#[test]
fn mcp_characterization_sets_test_isolation() {
    let src = std::fs::read_to_string("tests/mcp_characterization.rs")
        .expect("read mcp_characterization.rs");
    assert!(
        src.contains("AGEND_TEST_ISOLATION"),
        "mcp_characterization.rs must set AGEND_TEST_ISOLATION=1 for subprocess spawns"
    );
}

#[test]
fn mcp_roundtrip_sets_test_isolation() {
    let src = std::fs::read_to_string("tests/mcp_roundtrip.rs").expect("read mcp_roundtrip.rs");
    assert!(
        src.contains("AGEND_TEST_ISOLATION"),
        "mcp_roundtrip.rs must set AGEND_TEST_ISOLATION=1 for subprocess spawns"
    );
}

#[test]
fn handler_tests_set_test_isolation() {
    let src = std::fs::read_to_string("src/mcp/handlers/tests.rs").expect("read handler tests");
    assert!(
        src.contains("AGEND_TEST_ISOLATION"),
        "handler tests must set AGEND_TEST_ISOLATION=1 in setup_recorder"
    );
}

#[test]
fn proxy_or_local_checks_test_isolation() {
    let src = std::fs::read_to_string("src/mcp/mod.rs").expect("read mcp/mod.rs");
    assert!(
        src.contains("AGEND_TEST_ISOLATION"),
        "proxy_or_local must check AGEND_TEST_ISOLATION to prevent fleet pollution"
    );
}
