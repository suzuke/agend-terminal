//! Sprint 57 Wave 3 PR-2 (#548 Phase 2) source-text invariant pins.
//!
//! Tier-1 baseline regression-proof against Phase 2 IMPL drift.
//! Each test here is paired with a structural invariant from the
//! Phase 1 RCA (PR #554 / `bae2afd`) and the corresponding Q (Q1,
//! Q6, Q7) that this Phase 2 IMPL bakes into code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

const MAIN_RS: &str = include_str!("../src/main.rs");
const TRAY_MOD_RS: &str = include_str!("../src/tray/mod.rs");
const DAEMON_MOD_RS: &str = include_str!("../src/daemon/mod.rs");

// ---------------------------------------------------------------------
// Audit 1 (Q1): `Commands::Start` default flips to detached.
// ---------------------------------------------------------------------

#[test]
fn commands_start_default_is_detached() {
    // Pin the default-flip: the field is named `foreground`, NOT
    // `detached`. The new opt-in flag is `--foreground`; the absence
    // of the flag IS the detached default per Q1.
    assert!(
        MAIN_RS.contains("foreground: bool,"),
        "Wave 3 PR-2 (#548 Q1): Commands::Start must declare `foreground: bool` \
         (the new opt-in field; absence == detached default)"
    );
    // The pre-Wave-3-PR-2 field name must be gone — pin against
    // accidental revert.
    assert!(
        !MAIN_RS.contains("detached: bool,"),
        "Wave 3 PR-2 (#548 Q1): Commands::Start must NOT declare `detached: bool` \
         — that's the pre-Wave-3-PR-2 opt-in field name. Q1 inverted the default."
    );
}

#[test]
fn commands_start_foreground_opt_in_works() {
    // The dispatch site must consult `foreground` (the new field) and
    // `force_foreground` for the agents-implies-foreground path. Pin
    // both so future refactors keep the logic readable.
    assert!(
        MAIN_RS.contains("foreground,"),
        "dispatch must destructure `foreground` from Commands::Start"
    );
    assert!(
        MAIN_RS.contains("force_foreground"),
        "dispatch must compute force_foreground for the --agents path"
    );
}

#[test]
fn commands_start_no_legacy_foreground_default_optout() {
    // Hard cut-over per Q5: NO actual `--legacy-foreground-default`
    // CLI flag exists. A future refactor that adds the flag (i.e.
    // the field in the Commands::Start struct) would silently undo
    // the cut-over semantic. Match on the field declaration pattern
    // (`legacy_foreground_default:`) — clap derive macro converts
    // hyphenated CLI flags to snake_case fields, so the field decl
    // is the load-bearing structural marker. The doc-comment
    // referring to the flag name as part of explaining the
    // cut-over rationale is fine and intentional.
    assert!(
        !MAIN_RS.contains("legacy_foreground_default:"),
        "Q5 hard cut-over: no legacy_foreground_default field allowed in Commands::Start"
    );
}

// ---------------------------------------------------------------------
// Audit 7 (Q7): tray::bootstrap_daemon removed; spawn entry
// consolidates to CLI `start`.
// ---------------------------------------------------------------------

#[test]
fn tray_bootstrap_daemon_function_removed() {
    // Pre-Wave-3-PR-2 there was a `fn bootstrap_daemon(home: &Path)`
    // that called `daemon_spawn::spawn_detached` directly. Q7 deletes
    // it. Pin against re-introduction — the spawn-entry consolidation
    // is the load-bearing invariant.
    assert!(
        !TRAY_MOD_RS.contains("fn bootstrap_daemon("),
        "Wave 3 PR-2 (#548 Q7): tray::bootstrap_daemon function MUST be deleted \
         — daemon spawn consolidates to CLI `start` only"
    );
    // The tray must NOT import daemon_spawn either; that's the
    // structural signal it's no longer spawning daemons directly.
    assert!(
        !TRAY_MOD_RS.contains("bootstrap::daemon_spawn"),
        "Wave 3 PR-2 (#548 Q7): tray must NOT import bootstrap::daemon_spawn \
         — direct daemon spawning belongs to CLI `start` per Q7"
    );
}

#[test]
fn tray_check_daemon_state_uses_api_call_list() {
    // Replacement for the deleted bootstrap_daemon: a
    // `check_daemon_state` probe that returns Online/Offline via
    // `api::call(LIST)`. Pin both the function name and the LIST
    // method choice so a future refactor doesn't silently drop the
    // status surface.
    assert!(
        TRAY_MOD_RS.contains("fn check_daemon_state("),
        "Wave 3 PR-2 (#548 Q7): tray must define check_daemon_state probe"
    );
    assert!(
        TRAY_MOD_RS.contains(r#"&json!({"method": api::method::LIST})"#),
        "Wave 3 PR-2 (#548 Q7): tray probe must use api::method::LIST \
         (LIST is the lightweight handshake; SHUTDOWN would tear down a daemon)"
    );
}

#[test]
fn tray_menu_start_command_shells_out_to_cli() {
    // The new "Start daemon" menu item must shell out to
    // `agend-terminal start`, NOT call daemon_spawn directly. Pin
    // both the helper name and the `.arg("start")` invocation.
    assert!(
        TRAY_MOD_RS.contains("fn start_daemon_via_cli("),
        "Wave 3 PR-2 (#548 Q7): start_daemon_via_cli helper must exist"
    );
    assert!(
        TRAY_MOD_RS.contains(r#".arg("start")"#),
        "Wave 3 PR-2 (#548 Q7): tray must shell out to `agend-terminal start` \
         (consolidates daemon spawn entry to CLI per Q7)"
    );
    // env::current_exe is the cross-platform way to find the
    // running binary. Pin it so a future refactor doesn't fall
    // back to PATH-resolution which could pick up a stale install.
    assert!(
        TRAY_MOD_RS.contains("std::env::current_exe()")
            || TRAY_MOD_RS.contains("env::current_exe()"),
        "Wave 3 PR-2 (#548 Q7): tray must resolve start binary via current_exe()"
    );
}

// ---------------------------------------------------------------------
// Audit 6 (Q6): shutdown sequence enrichment + staged TERM/KILL.
// ---------------------------------------------------------------------

#[test]
fn shutdown_sequence_sigterm_then_sigkill_2s_grace() {
    // Source-text pin for the staged termination contract:
    // SIGTERM stage runs in parallel, then a SHUTDOWN_GRACE wait,
    // then SIGKILL stage for survivors. Each component is named
    // explicitly so a future refactor can't silently collapse the
    // staging.
    assert!(
        DAEMON_MOD_RS.contains("libc::SIGTERM"),
        "Wave 3 PR-2 (#548 Q6): shutdown_sequence must SIGTERM agents on Unix"
    );
    assert!(
        DAEMON_MOD_RS.contains("SHUTDOWN_GRACE"),
        "Wave 3 PR-2 (#548 Q6): shutdown_sequence must use SHUTDOWN_GRACE constant \
         (parameterized so the grace window is auditable)"
    );
    assert!(
        DAEMON_MOD_RS.contains("kill_process_tree"),
        "Wave 3 PR-2 (#548 Q6): SIGKILL stage must escalate via kill_process_tree"
    );
    assert!(
        DAEMON_MOD_RS.contains("Duration::from_secs(2)"),
        "Wave 3 PR-2 (#548 Q6): grace window MUST be 2s per Phase A RCA recommendation"
    );
}

#[test]
fn shutdown_sequence_emits_summary_metrics() {
    // The enriched `daemon_stop` payload must carry the four
    // documented fields. Pin each by source-text so they can't
    // silently disappear.
    for field in &[
        "reason=",
        "agents_total=",
        "agents_killed_after_grace=",
        "uptime_secs=",
    ] {
        assert!(
            DAEMON_MOD_RS.contains(field),
            "Wave 3 PR-2 (#548 Q6): daemon_stop payload MUST carry `{field}` field"
        );
    }
}

#[test]
fn shutdown_reason_taxonomy_recorded_at_each_trigger_site() {
    // Each shutdown trigger must record its reason taxonomy via
    // `record_shutdown_reason` BEFORE flipping the flag. The
    // signal handler + API SHUTDOWN method are the two production
    // sites; pin both via source-text invariant.
    let signals_rs = include_str!("../src/bootstrap/signals.rs");
    assert!(
        signals_rs.contains("ShutdownReason::Signal"),
        "Wave 3 PR-2 (#548 Q6): signal handler must record ShutdownReason::Signal"
    );
    let api_rs = include_str!("../src/api/mod.rs");
    assert!(
        api_rs.contains("ShutdownReason::ApiShutdown"),
        "Wave 3 PR-2 (#548 Q6): API SHUTDOWN handler must record \
         ShutdownReason::ApiShutdown"
    );
}
