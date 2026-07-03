//! #2549 W3 — collapses the three stale-backlog NOTIFICATION watchdogs
//! (`PollReminderHandler`, `InboxStuckHandler`, `HandoffTimeoutHandler`) into
//! ONE registered [`PerTickHandler`] slot (`NotificationWatchdogsHandler`,
//! 38 → 36 handlers in `build_default_handlers`).
//!
//! This is a pure COMPOSITION wrapper, not a rewrite: each inner handler
//! keeps its own `PerTickHandler` impl, `CadenceGate` (each with its own
//! `NOTIFICATION_BOOT_GRACE`-suppressed cadence and `every_n_ticks` — 30/30/12,
//! genuinely different, so they are NOT hoisted onto one shared gate), and
//! extra state (`InboxStuckHandler`'s shared `AlertLatch`, `HandoffTimeoutHandler`'s
//! two dedup maps) completely unchanged. `poll_reminder.rs`, `inbox_stuck.rs`,
//! and `handoff_timeout.rs` are untouched beyond bumping their `#[cfg(test)]`
//! `new_at` constructors to `pub(crate)` (visibility only, zero behavior
//! change) so this file's own tests can construct them past boot-grace
//! (P2-2549-SPIKE.md §3b).
//!
//! Panic isolation moves from PER-HANDLER to PER-CHECK (mirrors
//! `hourly_gc::run_sweep_isolated` / `supervisor_trackers::run_scan_isolated`):
//! this handler wraps each of its 3 inner `.run()` calls in its own
//! `catch_unwind`, so the pre-merge invariant — one watchdog panicking never
//! blocks the other two in the same tick — survives the collapse into a
//! single registered handler.
//!
//! ## InboxStuck ↔ Reclaim latch (hard constraint, §3b)
//!
//! `InboxStuckHandler`'s dedup latch (`AlertLatch`) is shared with
//! `ReclaimHandler` so a successful reclaim clears an agent's repeat stuck-
//! alert entry (#2127 Phase 1). [`NotificationWatchdogsHandler::inbox_stuck_latch`]
//! preserves the exact clone-out interface `build_default_handlers` already
//! used on the standalone `InboxStuckHandler` — construct the merged handler
//! first, clone the latch, then move the handler into the Vec, same shape as
//! before. The end-to-end wiring pin lives in `reclaim.rs`'s own test module
//! (`latch_shared_with_notification_watchdogs_merge_2549_w3`) since it already
//! owns `ReclaimHandler`'s test conventions.

use super::handoff_timeout::HandoffTimeoutHandler;
use super::inbox_stuck::{AlertLatch, InboxStuckHandler};
use super::poll_reminder::PollReminderHandler;
use super::{PerTickHandler, TickContext};

pub(crate) struct NotificationWatchdogsHandler {
    poll_reminder: PollReminderHandler,
    inbox_stuck: InboxStuckHandler,
    handoff_timeout: HandoffTimeoutHandler,
}

impl NotificationWatchdogsHandler {
    pub(crate) fn new(
        poll_reminder_ticks: u64,
        inbox_stuck_ticks: u64,
        handoff_timeout_ticks: u64,
    ) -> Self {
        Self {
            poll_reminder: PollReminderHandler::new(poll_reminder_ticks),
            inbox_stuck: InboxStuckHandler::new(inbox_stuck_ticks),
            handoff_timeout: HandoffTimeoutHandler::new(handoff_timeout_ticks),
        }
    }

    /// Test-only constructor with an explicit shared `created_at` so all
    /// three sub-handlers' boot-grace gates can be put past their window in
    /// one call (mirrors each sub-handler's own `new_at`).
    #[cfg(test)]
    pub(crate) fn new_at(
        poll_reminder_ticks: u64,
        inbox_stuck_ticks: u64,
        handoff_timeout_ticks: u64,
        created_at: std::time::Instant,
    ) -> Self {
        Self {
            poll_reminder: PollReminderHandler::new_at(poll_reminder_ticks, created_at),
            inbox_stuck: InboxStuckHandler::new_at(inbox_stuck_ticks, created_at),
            handoff_timeout: HandoffTimeoutHandler::new_at(handoff_timeout_ticks, created_at),
        }
    }

    /// A clone of the InboxStuck sub-handler's shared dedup latch — see
    /// `InboxStuckHandler::latch`. Reclaim clears an agent's entry after
    /// reclaiming its board work.
    pub(crate) fn inbox_stuck_latch(&self) -> AlertLatch {
        self.inbox_stuck.latch()
    }
}

impl PerTickHandler for NotificationWatchdogsHandler {
    fn name(&self) -> &'static str {
        "notification_watchdogs"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        run_check_isolated("poll_reminder", || self.poll_reminder.run(ctx));
        run_check_isolated("inbox_stuck_watchdog", || self.inbox_stuck.run(ctx));
        run_check_isolated("handoff_timeout_watchdog", || self.handoff_timeout.run(ctx));
    }
}

/// Run one sub-check isolated from its siblings: a panic inside `f` is
/// caught and logged, never propagated — the per-check equivalent of the
/// outer per-tick loop's per-HANDLER `catch_unwind`. Preserves "one
/// notification watchdog panicking doesn't block the other two" now that all
/// three run inside a single registered handler's `run()` call.
fn run_check_isolated(name: &'static str, f: impl FnOnce()) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        test_hooks::record_and_maybe_force_panic(name);
        f()
    }));
    if let Err(payload) = outcome {
        tracing::error!(
            check = name,
            error = %super::panic_payload_str(&payload),
            "notification_watchdogs: sub-check panicked — isolated, the other checks in this tick still ran"
        );
    }
}

/// Test-only fault-injection seam: proves the per-check isolation property
/// against the REAL merged handler (not a mock). Mirrors `hourly_gc`'s
/// identically-shaped `test_hooks`.
#[cfg(test)]
mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static FORCE_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
        static INVOKED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_and_maybe_force_panic(name: &'static str) {
        INVOKED.with(|v| v.borrow_mut().push(name));
        if FORCE_PANIC.with(|p| p.get()) == Some(name) {
            panic!("fault-injection: forced panic in check '{name}'");
        }
    }

    pub(super) fn force_panic(name: &'static str) {
        FORCE_PANIC.with(|p| p.set(Some(name)));
    }

    pub(super) fn clear_force_panic() {
        FORCE_PANIC.with(|p| p.set(None));
    }

    pub(super) fn take_invoked() -> Vec<&'static str> {
        INVOKED.with(|v| std::mem::take(&mut *v.borrow_mut()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-notification-watchdogs-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn past_grace() -> std::time::Instant {
        std::time::Instant::now()
            - super::super::NOTIFICATION_BOOT_GRACE
            - std::time::Duration::from_secs(1)
    }

    fn tick_ctx<'a>(
        home: &'a std::path::Path,
        registry: &'a crate::agent::AgentRegistry,
        externals: &'a crate::agent::ExternalRegistry,
        configs: &'a std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        >,
    ) -> TickContext<'a> {
        TickContext {
            home,
            registry,
            externals,
            configs,
        }
    }

    #[test]
    fn name_is_notification_watchdogs() {
        assert_eq!(
            NotificationWatchdogsHandler::new(30, 30, 12).name(),
            "notification_watchdogs"
        );
    }

    /// #2549 W3 pin (mirrors `hourly_gc`'s panic-isolation tests): the outer
    /// per-tick loop used to isolate panics PER-HANDLER — 3 separately-
    /// registered handlers meant a panic in one never touched the other 2's
    /// invocation this tick. After collapsing all 3 into
    /// `NotificationWatchdogsHandler`, that guarantee must be reproduced
    /// INSIDE `run()` at per-check granularity. Force the FIRST check
    /// (`poll_reminder`) to panic and assert (a) `run()` itself does not
    /// propagate the panic, and (b) all three checks were still reached, in
    /// order.
    #[test]
    fn one_check_panic_does_not_block_the_other_two() {
        let home = tmp_home("panic-isolation-first");
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = tick_ctx(&home, &registry, &externals, &configs);

        let handler = NotificationWatchdogsHandler::new_at(1, 1, 1, past_grace());
        test_hooks::force_panic("poll_reminder");

        handler.run(&ctx);

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "poll_reminder",
                "inbox_stuck_watchdog",
                "handoff_timeout_watchdog"
            ],
            "all three checks must be attempted, in order, even though \
             'poll_reminder' (the first) panicked — per-check isolation (#2549 W3)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Same property, forcing the MIDDLE check — the ones on either side both
    /// still ran, closing the "only proved trailing checks survive" gap the
    /// first test alone would leave.
    #[test]
    fn middle_check_panic_does_not_block_its_neighbors() {
        let home = tmp_home("panic-isolation-middle");
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = tick_ctx(&home, &registry, &externals, &configs);

        let handler = NotificationWatchdogsHandler::new_at(1, 1, 1, past_grace());
        test_hooks::force_panic("inbox_stuck_watchdog");

        handler.run(&ctx);

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "poll_reminder",
                "inbox_stuck_watchdog",
                "handoff_timeout_watchdog"
            ],
            "'inbox_stuck_watchdog' panicking must not stop \
             'handoff_timeout_watchdog' (after it) from running, nor does it \
             retroactively un-run 'poll_reminder' (before it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Baseline (no forced panic): all three still run in order, on a single
    /// `run()` call — the composition itself doesn't drop or reorder any of
    /// the three sub-checks.
    #[test]
    fn no_panic_all_three_run_in_order() {
        let home = tmp_home("baseline");
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = tick_ctx(&home, &registry, &externals, &configs);

        let handler = NotificationWatchdogsHandler::new_at(1, 1, 1, past_grace());
        handler.run(&ctx);

        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "poll_reminder",
                "inbox_stuck_watchdog",
                "handoff_timeout_watchdog"
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
