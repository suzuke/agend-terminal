//! Interim guard for: "Zombie kill primitive has TOCTOU between liveness check
//! and signal — can kill a recycled PID" (src/admin/cleanup_zombies.rs
//! `cleanup_zombie_daemon`).
//!
//! `cleanup_zombie_daemon(pid, …)` checks `is_alive(pid)` then sends
//! SIGTERM/SIGKILL. Between the `is_alive` check and `libc::kill` the OS can
//! recycle the PID onto an unrelated process, which then receives the signal.
//! The candidate is selected purely by run-dir name + `.daemon` mtime; nothing
//! re-confirms at kill time that the PID still belongs to an agend daemon
//! (matching the `.daemon`-recorded PID/start identity or argv).
//!
//! METHOD: redesign_required, with this static_invariant interim guard.
//! The function's CURRENT signature — `cleanup_zombie_daemon(pid: u32,
//! term_grace: Duration, kill_grace: Duration)` — receives ONLY a bare PID. It
//! has no recorded-identity material to compare the live process against, so the
//! PID-reuse window CANNOT be closed without threading the run-dir's recorded
//! identity (the `.daemon` pid:start_time, or the run_dir path) into the kill
//! primitive. That is an architectural change (see redesign_note in the
//! manifest). This guard scans the `cleanup_zombie_daemon` body and asserts an
//! identity re-verification token appears between the entry liveness check and
//! the destructive signal.
//!
//! RED now: the function body contains NO identity-recheck token (it only sends
//! SIGTERM then SIGKILL after the single entry `is_alive`). GREEN after fix: the
//! fix re-confirms the live PID is still the intended daemon (argv/comm/
//! start-time vs the `.daemon` identity) before signalling.

#[test]
#[ignore = "zombie-kill-toctou: red until fix; remove #[ignore] after fix to confirm"]
fn cleanup_zombie_daemon_must_recheck_identity_before_signal_bootstrap_config_cli() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/admin/cleanup_zombies.rs");
    let text = std::fs::read_to_string(&path).expect("read src/admin/cleanup_zombies.rs");

    // Extract the `cleanup_zombie_daemon` function body: from its `pub fn`
    // header up to the doc comment that introduces the next item
    // (`/// Poll `is_alive...`). This bounds the scan to the kill primitive and
    // excludes the unrelated `poll_until_dead` / `find_zombie_candidates` code.
    let start = text
        .find("pub fn cleanup_zombie_daemon(")
        .expect("cleanup_zombie_daemon must exist");
    let rest = &text[start..];
    let end = rest
        .find("/// Poll `is_alive")
        .expect("poll_until_dead doc boundary must follow cleanup_zombie_daemon");
    let body = &rest[..end];

    // The destructive SIGKILL path must be present (sanity: we are scanning the
    // right function) and the body must re-verify the PID still belongs to the
    // intended daemon before signalling. Tokens a correct fix would introduce
    // when comparing the live process identity against the run-dir's recorded
    // `.daemon` pid:start_time / argv / comm.
    assert!(
        body.contains("libc::kill") || body.contains("crate::process::terminate"),
        "scan boundary wrong — cleanup_zombie_daemon body no longer contains a kill call"
    );

    let identity_tokens = [
        "start_time",
        "start-time",
        ".daemon",
        "identity",
        "verify_identity",
        "recheck",
        "re-check",
        "argv",
        "comm",
        "run_dir",
    ];
    let has_recheck = identity_tokens.iter().any(|tok| body.contains(tok));

    assert!(
        has_recheck,
        "cleanup_zombie_daemon sends SIGTERM/SIGKILL after a single entry \
         `is_alive(pid)` with NO identity re-verification — a PID recycled onto an \
         unrelated process between the check and the signal would be killed. Re-confirm \
         the live PID is still the intended agend daemon (argv/comm/start-time vs the \
         run-dir's recorded `.daemon` identity) before signalling. NOTE: this requires \
         threading the recorded identity into the primitive — its signature currently \
         takes only a bare `pid: u32` (see redesign_note)."
    );
}
