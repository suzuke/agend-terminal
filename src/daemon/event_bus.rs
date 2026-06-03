//! Structured event bus for daemon-internal notifications.
//!
//! Phase 1: parallel emit spine. Existing `enqueue_with_idle_hint` calls
//! remain untouched; this module emits a structured `Event` alongside them
//! so future subscribers can react without per-module plumbing.
//!
//! Gated by `AGEND_EVENT_BUS=1`. When disabled, `emit()` is a no-op.

#![allow(dead_code)]

use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum EventKind {
    TaskStateChanged {
        task_id: String,
        title: String,
        assignee: Option<String>,
        reason: String,
        // #event-bus first-pattern (anti_stall): carry the task's `started_at` +
        // `eta_secs` so a subscriber can rebuild the BYTE-IDENTICAL notification
        // text the legacy direct enqueue formats from them.
        started_at: Option<String>,
        eta_secs: Option<i64>,
    },
    CiReady {
        repo: String,
        branch: String,
        target: String,
    },
    CiFail {
        repo: String,
        branch: String,
        target: String,
    },
    DispatchIdleExceeded {
        dispatcher: String,
        target: String,
        elapsed_secs: i64,
    },
    MemberStateChanged {
        agent: String,
        team: String,
        from_state: String,
        to_state: String,
    },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub timestamp: std::time::Instant,
}

impl Event {
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            timestamp: std::time::Instant::now(),
        }
    }
}

pub fn global() -> &'static EventBus {
    static BUS: std::sync::OnceLock<EventBus> = std::sync::OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

type Subscriber = Arc<dyn Fn(&Event) + Send + Sync>;

pub struct EventBus {
    subscribers: parking_lot::Mutex<Vec<Subscriber>>,
    enabled: bool,
}

impl EventBus {
    pub fn new() -> Self {
        let enabled = std::env::var("AGEND_EVENT_BUS")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Test-only: a bus with the gate forced ON (bypasses the env var), so other
    /// modules' tests can exercise the `emit`→subscriber path deterministically
    /// without touching the process-global `global()` bus.
    #[cfg(test)]
    pub(crate) fn new_enabled_for_test() -> Self {
        Self {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        }
    }

    pub fn subscribe(&self, f: impl Fn(&Event) + Send + Sync + 'static) {
        self.subscribers.lock().push(Arc::new(f));
    }

    pub fn emit(&self, kind: EventKind) {
        if !self.enabled {
            return;
        }
        let event = Event::new(kind);
        let subs = self.subscribers.lock();
        for sub in subs.iter() {
            sub(&event);
        }
        tracing::debug!(kind = ?event.kind, "event_bus: emitted");
    }

    pub fn emit_lazy(&self, f: impl FnOnce() -> EventKind) {
        if !self.enabled {
            return;
        }
        self.emit(f());
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("enabled", &self.enabled)
            .field("subscriber_count", &self.subscribers.lock().len())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emit_disabled_by_default() {
        let bus = EventBus::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        bus.subscribe(move |e| r.lock().unwrap().push(e.kind.clone()));
        bus.emit(EventKind::CiReady {
            repo: "owner/repo".into(),
            branch: "main".into(),
            target: "dev".into(),
        });
        assert!(
            received.lock().unwrap().is_empty(),
            "emit must be no-op when AGEND_EVENT_BUS != 1"
        );
    }

    #[test]
    fn emit_delivers_to_subscribers_when_enabled() {
        let bus = EventBus {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        };
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        bus.subscribe(move |e| r.lock().unwrap().push(e.kind.clone()));
        bus.emit(EventKind::TaskStateChanged {
            task_id: "t-1".into(),
            title: "test".into(),
            assignee: Some("dev".into()),
            reason: "stalled".into(),
            started_at: None,
            eta_secs: None,
        });
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], EventKind::TaskStateChanged { task_id, .. } if task_id == "t-1")
        );
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let bus = EventBus {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        };
        let count_a = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_b = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ca = Arc::clone(&count_a);
        let cb = Arc::clone(&count_b);
        bus.subscribe(move |_| {
            ca.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        bus.subscribe(move |_| {
            cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        bus.emit(EventKind::CiFail {
            repo: "o/r".into(),
            branch: "feat".into(),
            target: "dev".into(),
        });
        assert_eq!(count_a.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(count_b.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn all_event_kinds_constructible() {
        let kinds = vec![
            EventKind::TaskStateChanged {
                task_id: "t-1".into(),
                title: "t".into(),
                assignee: None,
                reason: "r".into(),
                started_at: None,
                eta_secs: None,
            },
            EventKind::CiReady {
                repo: "o/r".into(),
                branch: "b".into(),
                target: "t".into(),
            },
            EventKind::CiFail {
                repo: "o/r".into(),
                branch: "b".into(),
                target: "t".into(),
            },
            EventKind::DispatchIdleExceeded {
                dispatcher: "lead".into(),
                target: "dev".into(),
                elapsed_secs: 600,
            },
            EventKind::MemberStateChanged {
                agent: "dev".into(),
                team: "fixup".into(),
                from_state: "Ready".into(),
                to_state: "ServerRateLimit".into(),
            },
        ];
        for k in kinds {
            let _ = format!("{k:?}");
        }
    }
}
