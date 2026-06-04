//! Invariant: the committed pre-push hook enforces local CI-parity.
//!
//! #t-ci-parity-prepush-guard. Closes the recurring miss where an agent ran
//! only `cargo test --bin` — which SKIPS the `tests/` integration targets —
//! declared CI-ready, and CI then rejected the push (#1734 stale "ready" string
//! in tests/integration.rs; #1735 block_on invariant under tests/). The hook
//! delegates CI-parity to the canonical preflight so it can never drift from
//! CI; these pins keep the gate wired so a future edit can't silently drop it.
//!
//! (This file is itself a `tests/` target — so the very gate it pins would also
//! catch a regression here, the same way it catches #1734/#1735.)

const PRE_PUSH: &str = include_str!("../scripts/hooks/pre-push");

#[test]
fn pre_push_runs_ci_parity_via_preflight() {
    assert!(
        PRE_PUSH.contains("scripts/preflight.sh --quick"),
        "pre-push must delegate CI-parity to the canonical preflight \
         (fmt --check / clippy --all-targets --features tray / cargo test --tests)"
    );
    assert!(
        PRE_PUSH.contains("exit 1"),
        "pre-push must BLOCK the push (exit 1) when CI-parity fails"
    );
}

#[test]
fn pre_push_gates_on_code_changes() {
    // Docs-only pushes skip the (~minutes) build for latency; code pushes run
    // the gate. Pin both the diff detection and the watched code paths.
    assert!(
        PRE_PUSH.contains("git diff --name-only"),
        "pre-push must detect whether the push range touches code"
    );
    for path in ["src/", "tests/", "Cargo.toml", "Cargo.lock"] {
        assert!(
            PRE_PUSH.contains(path),
            "pre-push code-change gate must watch '{path}'"
        );
    }
}

#[test]
fn pre_push_documents_emergency_override() {
    assert!(
        PRE_PUSH.contains("--no-verify"),
        "pre-push must document the emergency override"
    );
}
