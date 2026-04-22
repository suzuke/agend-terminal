//! Global registry of [`UxEventSink`] implementations.
//!
//! The MCP handler layer emits `UxEvent::Fleet(..)` events (Q2 fleet
//! visibility) through [`registry`]; the daemon boots channel adapters
//! (e.g. `TelegramChannel`) and registers them once at startup. At emit
//! time, every registered sink receives a reference to the event.
//!
//! ## Why a crate-level singleton
//!
//! The registry is deliberately a plain `OnceLock<T>` behind a module
//! accessor — matching the established pattern in the codebase:
//! `src/mcp/mod.rs::ACL`, `src/schedules.rs::DETECTED_TZ`,
//! `src/channel/telegram.rs::RT`, `src/vterm.rs::CACHE`. We do NOT
//! introduce a DI framework or service-locator abstraction — a small
//! crate-wide resource handled by the same pattern as its neighbors
//! beats a bespoke abstraction. Decision: `docs/DESIGN-stage-b-ux.md`
//! §9 Q3, by general.
//!
//! ## Scope of PR-A
//!
//! The registry and the producer-side hooks in `src/mcp/handlers.rs`
//! land here. The Telegram fleet renderer (the sink that actually
//! formats and forwards Fleet events to the `fleet_binding` topic)
//! lands in PR-B and will register `TelegramChannel` at bootstrap.
//! Until then, emissions fan out to whatever sinks tests register;
//! production runs see zero registered sinks and emissions are
//! effectively no-ops.

use std::sync::{Arc, Mutex, OnceLock};

use super::ux_event::{UxEvent, UxEventSink};
use crate::sync::lock_poisoned;

/// Crate-level singleton. Lazily initialized on first access so neither
/// the daemon nor tests need an explicit init step.
static REGISTRY: OnceLock<UxSinkRegistry> = OnceLock::new();

/// Borrow the process-wide [`UxSinkRegistry`]. Callers use this to
/// either [`UxSinkRegistry::register`] a sink at startup or
/// [`UxSinkRegistry::emit`] an event from an MCP handler.
pub fn registry() -> &'static UxSinkRegistry {
    REGISTRY.get_or_init(UxSinkRegistry::default)
}

/// Fan-out container for [`UxEventSink`] impls. Registration is
/// bootstrap-only in production code paths (Telegram adapter registers
/// itself once) and emit is fire-and-forget, so a plain `Mutex` matches
/// the rest of the crate's locking style (`src/sync.rs::lock_poisoned`)
/// — no reason to reach for `RwLock` when there's no contention.
#[derive(Default)]
pub struct UxSinkRegistry {
    sinks: Mutex<Vec<Arc<dyn UxEventSink>>>,
}

impl UxSinkRegistry {
    /// Add a sink to receive all future [`emit`](Self::emit) calls.
    /// Registration order is preserved. Adapters typically register
    /// themselves once during bootstrap and never deregister.
    pub fn register(&self, sink: Arc<dyn UxEventSink>) {
        lock_poisoned(&self.sinks, "ux_sink_registry").push(sink);
    }

    /// Fan out `event` to every registered sink in insertion order.
    /// Sinks are expected to be fire-and-forget and must not panic
    /// (per [`UxEventSink`]'s contract); this loop does not catch or
    /// propagate per-sink failures.
    pub fn emit(&self, event: &UxEvent) {
        // Snapshot under the lock, then release before invoking sinks
        // so a slow sink can't block registration or other emits. The
        // `Arc` clone is cheap.
        let snapshot: Vec<Arc<dyn UxEventSink>> =
            lock_poisoned(&self.sinks, "ux_sink_registry").clone();
        for sink in &snapshot {
            sink.emit(event);
        }
    }

    /// Number of currently registered sinks. Exposed for tests that
    /// need to assert registration state without reaching into
    /// internals.
    pub fn len(&self) -> usize {
        lock_poisoned(&self.sinks, "ux_sink_registry").len()
    }

    /// True when no sinks are registered. Production callers never
    /// depend on this — the emit path is oblivious to sink count —
    /// but tests that exercise the "no sinks registered" fallback
    /// read cleaner with an `is_empty()` assertion.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove every registered sink. Intended for tests that share
    /// the process-wide singleton across cases and need a clean slate
    /// between them. Production code has no use for this and should
    /// not call it.
    #[cfg(test)]
    pub fn clear_for_test(&self) {
        lock_poisoned(&self.sinks, "ux_sink_registry").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ux_event::FleetEvent;

    /// Test-only sink that records every event it receives. Used in
    /// both this module and `src/mcp/handlers.rs` tests to assert that
    /// MCP handlers emit the expected `UxEvent::Fleet(..)` shape.
    pub struct RecordingSink {
        pub events: Mutex<Vec<UxEvent>>,
    }

    impl RecordingSink {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }
    }

    impl UxEventSink for RecordingSink {
        fn emit(&self, event: &UxEvent) {
            lock_poisoned(&self.events, "recording_sink").push(event.clone());
        }
    }

    fn make_fleet_event() -> UxEvent {
        UxEvent::Fleet(FleetEvent::DelegateTask {
            from: "a".into(),
            to: "b".into(),
            summary: "hi".into(),
            task_id: None,
        })
    }

    /// Registering a sink and emitting an event propagates the event
    /// to that sink. Basic wiring pin.
    #[test]
    fn register_then_emit_delivers_event() {
        let reg = UxSinkRegistry::default();
        let sink = super::tests::RecordingSink::new();
        reg.register(sink.clone() as Arc<dyn UxEventSink>);
        reg.emit(&make_fleet_event());
        assert_eq!(lock_poisoned(&sink.events, "test").len(), 1);
    }

    /// Multiple sinks all see every emission in insertion order.
    #[test]
    fn multiple_sinks_all_receive() {
        let reg = UxSinkRegistry::default();
        let s1 = super::tests::RecordingSink::new();
        let s2 = super::tests::RecordingSink::new();
        reg.register(s1.clone() as Arc<dyn UxEventSink>);
        reg.register(s2.clone() as Arc<dyn UxEventSink>);
        reg.emit(&make_fleet_event());
        reg.emit(&make_fleet_event());
        assert_eq!(lock_poisoned(&s1.events, "test").len(), 2);
        assert_eq!(lock_poisoned(&s2.events, "test").len(), 2);
    }

    /// An emit into an empty registry is a harmless no-op. This is
    /// the production state for PR-A (no TelegramChannel registered
    /// yet) — handlers still emit and must not panic.
    #[test]
    fn emit_with_no_sinks_is_noop() {
        let reg = UxSinkRegistry::default();
        assert!(reg.is_empty());
        reg.emit(&make_fleet_event()); // must not panic
        assert_eq!(reg.len(), 0);
    }

    /// The `registry()` accessor returns the same instance across
    /// calls — it's a real singleton, not a lazy-per-call factory.
    #[test]
    fn registry_accessor_is_singleton() {
        let a = registry() as *const UxSinkRegistry;
        let b = registry() as *const UxSinkRegistry;
        assert_eq!(a, b);
    }
}
