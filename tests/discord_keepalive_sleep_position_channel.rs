//! channel-LOW-3 repro (static invariant): the Discord keepalive loop in
//! `start_keepalive` (src/channel/discord.rs) must NOT `std::thread::sleep` the
//! full `KEEPALIVE_INTERVAL_SECS` BEFORE doing any keepalive work on the first
//! iteration.
//!
//! WHY THIS IS A BUG: the loop body opens with
//! `std::thread::sleep(Duration::from_secs(KEEPALIVE_INTERVAL_SECS))` (30 min),
//! so on the first iteration the thread idles a full interval before issuing its
//! first `archived=false` PATCH for any bound thread. Freshly bound threads (and
//! any binding recorded shortly before the daemon starts) get less refresh
//! margin than the comment ("30 min refresh vs 60 min auto-archive is safe")
//! claims, because the very first refresh slips toward the 60-min archive
//! boundary instead of happening promptly.
//!
//! CORRECT BEHAVIOR (the fix): do one immediate keepalive pass before sleeping,
//! or move the `sleep` to the END of the loop body — so the first statement
//! inside `loop { ... }` is the keepalive WORK, not the interval sleep. Then the
//! 2x-margin assumption holds on the first cycle too.
//!
//! METHOD: a SOURCE-SCANNING invariant. The loop runs on a fire-and-forget
//! detached thread on an internal runtime that tests can't drive without a 30-min
//! real sleep, so we pin the structural property the fix establishes: the first
//! executable statement of the `start_keepalive` loop is not the interval sleep.
//! RED now (sleep is the first statement); GREEN once the work runs first / the
//! sleep moves to the loop tail.
//!
//! This is a source-text scan, so it does NOT require the `discord` feature to be
//! compiled.

use std::path::PathBuf;

#[test]
#[ignore = "channel-LOW-3: red until fix; remove #[ignore] after fix to confirm"]
fn discord_keepalive_does_not_sleep_before_first_refresh_channel() {
    let discord = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/channel/discord.rs");
    let content = std::fs::read_to_string(&discord)
        .expect("channel-LOW-3: src/channel/discord.rs must exist");
    let lines: Vec<&str> = content.lines().collect();

    // 1. Locate `start_keepalive`.
    let fn_idx = lines
        .iter()
        .position(|l| l.contains("fn start_keepalive"))
        .expect("channel-LOW-3: start_keepalive must exist in discord.rs");

    // 2. Find the loop opener after the fn (the keepalive refresh loop).
    let loop_idx = (fn_idx..lines.len())
        .find(|&i| lines[i].trim_start().starts_with("loop {"))
        .expect("channel-LOW-3: start_keepalive must contain a `loop {` body");

    // 3. The FIRST non-blank / non-comment statement inside the loop body.
    let first_stmt = (loop_idx + 1..lines.len())
        .map(|i| (i, lines[i].trim()))
        .find(|(_, t)| !t.is_empty() && !t.starts_with("//") && !t.starts_with('*'))
        .map(|(i, t)| (i, t.to_string()))
        .expect("channel-LOW-3: keepalive loop body must have a statement");

    let (stmt_line, stmt) = first_stmt;
    let sleeps_first = stmt.contains("thread::sleep") && stmt.contains("KEEPALIVE_INTERVAL_SECS");

    assert!(
        !sleeps_first,
        "channel-LOW-3: the Discord keepalive loop sleeps the full \
         KEEPALIVE_INTERVAL_SECS BEFORE its first refresh PATCH \
         ({}:{}: `{}`) — freshly bound threads idle up to a full 30-min interval \
         before their first archive-refresh, eroding the 30-min-vs-60-min margin. \
         Do an immediate pass first, or move the sleep to the END of the loop body.",
        discord.display(),
        stmt_line + 1,
        stmt
    );
}
