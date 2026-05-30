//! Per-tick handlers — first cut of #694 BLOCK 1.
//!
//! The daemon main loop (`run_core` in `src/daemon/mod.rs`) historically
//! inlined every periodic concern in a single 200-line block. This module
//! introduces a thin trait, [`PerTickHandler`], so each periodic concern
//! can be moved into its own file, owned state and all, then invoked from
//! the main loop in the same position it occupied before. The trait is
//! deliberately minimal — pattern relocation, not abstraction.
//!
//! Cumulative extraction state (handlers grow per PR):
//!
//! - [`SnapshotRotationHandler`] (T-B2, was `mod.rs:644-680`) — owns the
//!   `last_snapshot_json` dedup string that used to live as a loop-local.
//! - [`PollReminderHandler`] (T-B2, was `mod.rs:748-758`) — owns the
//!   every-N tick counter that used to live as a function-local `static`.
//! - [`InboxMaintenanceHandler`] (T-B3, was `mod.rs:667-728`) — the
//!   every-60-tick composite of 6 sub-ops; counter moves from
//!   function-local `static AtomicU64` onto the struct.
//! - [`ExternalLivenessHandler`] (T-B3, was `mod.rs:647-658`) — picked
//!   over the watchdog block for blast-radius reasons documented in the
//!   T-B3 PR body. Stateless wrapper around the `externals.retain`
//!   liveness sweep.
//!
//! Follow-up PRs (T-B4+) will move further subsystems behind the same
//! trait. Until then, the daemon loop holds the handlers as named locals
//! and calls them at their original sites — a single `Vec<Box<dyn …>>`
//! iteration would reorder execution and is deferred until enough
//! handlers exist for the uniform iteration to be the natural shape.

use crate::agent::{AgentRegistry, ExternalRegistry};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(crate) mod check_schedules;
pub(crate) mod ci_watch_poll;
pub(crate) mod external_liveness;
pub(crate) mod gc_tick;
pub(crate) mod hang_detection;
pub(crate) mod inbox_maintenance;
pub(crate) mod inbox_stuck;
pub(crate) mod log_rotation;
pub(crate) mod poll_reminder;
pub(crate) mod pr_state_scan;
pub(crate) mod recovery_dispatcher;
pub(crate) mod snapshot;
pub(crate) mod thread_dump;
pub(crate) mod watchdog;

pub(crate) use check_schedules::CheckSchedulesHandler;
pub(crate) use ci_watch_poll::CiWatchPollHandler;
pub(crate) use external_liveness::ExternalLivenessHandler;
pub(crate) use gc_tick::GcTickHandler;
pub(crate) use hang_detection::HangDetectionHandler;
pub(crate) use inbox_maintenance::InboxMaintenanceHandler;
pub(crate) use inbox_stuck::InboxStuckHandler;
pub(crate) use log_rotation::LogRotationHandler;
pub(crate) use poll_reminder::PollReminderHandler;
pub(crate) use pr_state_scan::PrStateScanHandler;
pub(crate) use recovery_dispatcher::RecoveryDispatcherHandler;
pub(crate) use snapshot::SnapshotRotationHandler;
pub(crate) use thread_dump::ThreadDumpHandler;
pub(crate) use watchdog::WatchdogHandler;

/// Shared per-tick context. Field types match what the daemon main loop
/// holds verbatim — the trait is pure relocation, not abstraction. New
/// fields are added as a handler's extraction lands; existing handlers
/// are unaffected because all fields are borrowed references.
pub(crate) struct TickContext<'a> {
    pub home: &'a Path,
    pub registry: &'a AgentRegistry,
    pub externals: &'a ExternalRegistry,
    pub configs: &'a Arc<Mutex<HashMap<String, super::AgentConfig>>>,
}

/// One periodic concern in the daemon main loop. `run` takes `&self`
/// because handlers are held by reference for the daemon's lifetime;
/// state that needs to mutate across ticks must use interior mutability
/// (`AtomicU64`, `Mutex<…>`, etc.).
pub(crate) trait PerTickHandler: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self, ctx: &TickContext<'_>);
}

// ── #941: per-handler timing observability ─────────────────────────────
//
// `HANDLER_TIMING` accumulates per-handler wall-clock stats so the
// periodic thread-dump (see `thread_dump::ThreadDumpHandler`) can
// surface "which handler is slow". Zero overhead when
// `AGEND_DAEMON_THREAD_DUMP_SECS` is unset: `record_handler_timing`
// early-returns after one cached atomic load.
//
// `RwLock<HashMap>` is the right shape: many writers (one per handler
// per tick — sequential, never contended in practice) + few readers
// (the periodic dump). HashMap key is `&'static str` so no allocation
// per record.

#[derive(Debug, Clone, Default)]
pub(crate) struct HandlerStats {
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_duration_ms: u64,
    pub max_duration_ms: u64,
    pub run_count: u64,
}

static HANDLER_TIMING: std::sync::OnceLock<std::sync::RwLock<HashMap<&'static str, HandlerStats>>> =
    std::sync::OnceLock::new();

/// Record one handler's run duration. Called by the main loop's
/// per-handler timing wrapper. No-op when thread-dump is disabled.
pub(crate) fn record_handler_timing(name: &'static str, elapsed: std::time::Duration) {
    if !crate::sync_audit::thread_dump_enabled() {
        return;
    }
    let lock = HANDLER_TIMING.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let Ok(mut guard) = lock.write() else {
        return;
    };
    let stats = guard.entry(name).or_default();
    stats.last_run_at = Some(chrono::Utc::now());
    stats.last_duration_ms = elapsed.as_millis() as u64;
    if elapsed.as_millis() as u64 > stats.max_duration_ms {
        stats.max_duration_ms = elapsed.as_millis() as u64;
    }
    stats.run_count = stats.run_count.saturating_add(1);
}

/// Snapshot the current handler-timing map for the periodic dump
/// handler. Cloned because the dump runs on the same thread as the
/// recorders and we want to avoid holding the read lock across
/// formatting.
pub(crate) fn snapshot_handler_timings() -> HashMap<String, HandlerStats> {
    let Some(lock) = HANDLER_TIMING.get() else {
        return HashMap::new();
    };
    let Ok(guard) = lock.read() else {
        return HashMap::new();
    };
    guard
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// #1002 Phase 1 — per-handler panic isolation for the main-loop
/// dispatch.
///
/// Drives the per-tick handler iteration in two steps:
/// 1. `std::panic::catch_unwind` wraps each `handler.run(...)` call so
///    a panic in handler N does not abort the iteration before
///    handlers N+1..end run on this tick.
/// 2. On a caught panic, log handler identity + panic payload (best-
///    effort `Debug`-formatting of `Box<dyn Any>`) so future
///    `grep "panic"` over `app.log` surfaces the silent failure that
///    #1002 was filed against.
///
/// The handler trait object itself is `Send + Sync`, but `tick_ctx`
/// holds borrowed references — `AssertUnwindSafe` is required.
/// Borrows are not invalidated by unwinding because the catch boundary
/// is per-handler (no shared mutable state crosses the boundary).
///
/// Test seam: `handlers` accepts any `&[Box<dyn PerTickHandler>]`, so
/// the unit test (`per_tick::tests::panicking_handler_does_not_skip_siblings`)
/// constructs a fixture vec with a panicking handler in the middle and
/// asserts the trailing handler runs.
pub(crate) fn run_handlers_with_panic_guard(
    handlers: &[Box<dyn PerTickHandler>],
    ctx: &TickContext<'_>,
) {
    for handler in handlers {
        let start = std::time::Instant::now();
        let name = handler.name();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler.run(ctx);
        }));
        record_handler_timing(name, start.elapsed());
        if let Err(payload) = outcome {
            // Best-effort payload Debug; tracing crate stringifies via Debug.
            let payload_str = panic_payload_str(&payload);
            tracing::error!(
                handler = name,
                payload = %payload_str,
                "#1002 per_tick handler panicked — subsequent handlers \
                 in this tick continue; next tick re-invokes this handler"
            );
        }
    }
}

/// Reduce a panic payload (`Box<dyn Any + Send>`) to a printable
/// string. Mirrors `std::panic::panic_any` conventions: `String` and
/// `&'static str` are the only payloads `panic!()` produces by
/// default; other payloads fall through to a placeholder.
fn panic_payload_str(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PLMutex;
    use std::sync::Arc;

    /// Mock handler with arbitrary `run` behaviour: either records a
    /// hit on a shared counter or panics with a fixed message.
    struct MockHandler {
        name_: &'static str,
        on_run: Box<dyn Fn(&TickContext<'_>) + Send + Sync>,
    }

    impl PerTickHandler for MockHandler {
        fn name(&self) -> &'static str {
            self.name_
        }
        fn run(&self, ctx: &TickContext<'_>) {
            (self.on_run)(ctx);
        }
    }

    #[test]
    fn panicking_handler_does_not_skip_siblings() {
        // #1002 Phase 1 RED→GREEN pin: a panic in handler N MUST NOT
        // abort handler N+1's invocation on this tick. Pre-fix, the
        // panic propagated up the for-loop and silently killed the
        // entire tick (the daemon's `run_core` had no `catch_unwind`
        // around `handler.run(&tick_ctx)`).
        let home = std::env::temp_dir().join(format!("agend-pertick-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let before_count = Arc::new(PLMutex::new(0u32));
        let after_count = Arc::new(PLMutex::new(0u32));
        let before_clone = before_count.clone();
        let after_clone = after_count.clone();

        let handlers: Vec<Box<dyn PerTickHandler>> = vec![
            Box::new(MockHandler {
                name_: "before",
                on_run: Box::new(move |_| *before_clone.lock() += 1),
            }),
            Box::new(MockHandler {
                name_: "panicker",
                on_run: Box::new(|_| panic!("#1002 test panic")),
            }),
            Box::new(MockHandler {
                name_: "after",
                on_run: Box::new(move |_| *after_clone.lock() += 1),
            }),
        ];

        run_handlers_with_panic_guard(&handlers, &ctx);

        assert_eq!(*before_count.lock(), 1, "pre-panic handler ran");
        assert_eq!(
            *after_count.lock(),
            1,
            "post-panic handler MUST still run — catch_unwind isolates the panic"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn panic_payload_str_handles_common_panic_types() {
        let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("string panic"));
        let static_payload: Box<dyn std::any::Any + Send> = Box::new("static str panic");
        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42u32);

        assert_eq!(panic_payload_str(&string_payload), "string panic");
        assert_eq!(panic_payload_str(&static_payload), "static str panic");
        assert_eq!(
            panic_payload_str(&other_payload),
            "<non-string panic payload>"
        );
    }
}
