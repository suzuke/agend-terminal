use crate::api::{ApiEvent, ApiNotifier};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Wire envelope for the daemon-to-TUI lifecycle stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DaemonEvent {
    pub source: String,
    pub sequence: u64,
    pub event: ApiEvent,
}

/// Per-daemon fan-out hub. Publishing is deliberately non-blocking: a slow
/// or gone TUI is removed and must rebuild from a subsequent Live snapshot.
pub(crate) struct EventHub {
    source: String,
    next_sequence: AtomicU64,
    capacity: usize,
    subscribers: Mutex<Vec<Sender<DaemonEvent>>>,
}

impl EventHub {
    pub(crate) fn new(source: String, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            source,
            next_sequence: AtomicU64::new(0),
            capacity: capacity.max(1),
            subscribers: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn subscribe(&self) -> Receiver<DaemonEvent> {
        let (tx, rx) = bounded(self.capacity);
        self.subscribers.lock().push(tx);
        rx
    }

    fn publish(&self, event: ApiEvent) {
        let envelope = DaemonEvent {
            source: self.source.clone(),
            sequence: self.next_sequence.fetch_add(1, Ordering::AcqRel) + 1,
            event,
        };
        let mut subscribers = self.subscribers.lock();
        subscribers.retain(|tx| match tx.try_send(envelope.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

impl ApiNotifier for EventHub {
    fn notify(&self, event: ApiEvent) {
        self.publish(event);
    }
}

/// The `.daemon` record is atomically published before the API listener starts.
/// Keeping its complete record as the source token fences successor daemons.
pub(crate) fn source_id(run_dir: &std::path::Path) -> String {
    std::fs::read_to_string(run_dir.join(".daemon"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn for_run_dir(home: &std::path::Path) -> Arc<EventHub> {
    EventHub::new(source_id(&crate::daemon::run_dir(home)), 128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiEvent, ApiNotifier};
    use crate::types::{InstanceId, InstanceRef};

    #[test]
    fn publishes_source_and_monotonic_sequence() {
        let hub = EventHub::new("daemon-a".to_string(), 4);
        let rx = hub.subscribe();
        hub.notify(ApiEvent::InstanceDeleted {
            name: "worker".to_string(),
            instance_ref: Some(InstanceRef::new(InstanceId::new(), 7)),
        });
        hub.notify(ApiEvent::TeamCreated {
            name: "team".to_string(),
            members: vec!["worker".to_string()],
        });
        let first = rx.recv().expect("first event");
        let second = rx.recv().expect("second event");
        assert_eq!(first.source, "daemon-a");
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
    }

    #[test]
    fn full_subscriber_is_closed_without_blocking_producer() {
        let hub = EventHub::new("daemon-a".to_string(), 1);
        let rx = hub.subscribe();
        hub.notify(ApiEvent::TeamCreated {
            name: "team".to_string(),
            members: vec![],
        });
        hub.notify(ApiEvent::TeamCreated {
            name: "team-2".to_string(),
            members: vec![],
        });
        assert!(rx.recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
