//! channel-HIGH-1 repro (static invariant): `notify_telegram_inner` must NOT
//! schedule the actual Telegram send with `telegram_runtime().spawn(...)` and
//! then discard the `JoinHandle`.
//!
//! WHY THIS IS A BUG: `telegram_runtime()` (src/channel/telegram/state.rs) is a
//! `new_current_thread` runtime. A current_thread tokio runtime makes NO
//! progress on `spawn`ed tasks unless some thread is actively inside
//! `Runtime::block_on()` on that SAME runtime. The only code that ever calls
//! `telegram_runtime().block_on()` is the outside-runtime branch of
//! `spawn_or_block_on` / `block_on_value` — there is no persistent driver thread
//! for `telegram_runtime()`. The public wrappers `notify_telegram` /
//! `notify_telegram_silent` discard the returned `JoinHandle`
//! (`let _ = notify_telegram_inner(...)`), so a daemon stall / recovery / crash /
//! CI notification reached synchronously from the supervisor is only delivered
//! opportunistically — IF some later sync-context `block_on` happens to
//! cooperatively poll the queued task. Otherwise the message is queued and never
//! sent, while the dedup claim stays recorded (suppressing a re-emit for the
//! whole TTL).
//!
//! CORRECT BEHAVIOR (the fix): drive the send synchronously via
//! `block_on_value(...)` inside `notify_telegram_inner` (matching how
//! `reply.rs::send_reply` works), OR own a dedicated long-lived driver thread —
//! never `spawn` onto a passively-held current_thread runtime and drop the
//! handle. After either fix, the `telegram_runtime().spawn(` fire-and-forget call
//! disappears from `notify.rs`.
//!
//! METHOD: a SOURCE-SCANNING invariant (mirrors
//! `tests/block_on_runtime_guard_invariant.rs`). The behavioral path can't be
//! observed without the fix, because the production runtime is never driven — so
//! this guard pins the structural property the fix establishes. RED now (the
//! `spawn` is present at notify.rs:64); GREEN once the send is driven instead.

use std::path::PathBuf;

#[test]
#[ignore = "channel-HIGH-1: red until fix; remove #[ignore] after fix to confirm"]
fn notify_does_not_fire_and_forget_onto_undriven_runtime_channel() {
    let notify = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/channel/telegram/notify.rs");
    let content = std::fs::read_to_string(&notify)
        .expect("channel-HIGH-1: src/channel/telegram/notify.rs must exist");

    // The fire-and-forget anti-pattern: scheduling the send on the passively
    // held current_thread `telegram_runtime()` via `spawn` (whose JoinHandle the
    // public wrappers then discard). `block_on_value(...)` — the fix — does NOT
    // contain `telegram_runtime().spawn(`.
    let mut violations = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment / doc lines that merely mention the pattern.
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if line.contains("telegram_runtime().spawn(") {
            violations.push(format!("{}:{}: {}", notify.display(), i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "channel-HIGH-1: `notify_telegram_inner` schedules the Telegram send with \
         `telegram_runtime().spawn(...)` onto a passively-held current_thread \
         runtime that has NO driver thread, then discards the JoinHandle — the \
         notification is queued and may never be sent while the dedup claim \
         suppresses a re-emit for the TTL. Drive the send synchronously via \
         `block_on_value(...)` (like reply.rs::send_reply) instead:\n{}",
        violations.join("\n")
    );
}
