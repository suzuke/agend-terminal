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
    #[cfg(test)]
    test_pause_after_first_reservation: Mutex<Option<Arc<TestSequencePause>>>,
}

#[cfg(test)]
struct TestSequencePause {
    reserved: std::sync::atomic::AtomicBool,
    release: std::sync::atomic::AtomicBool,
    second_reserved: std::sync::atomic::AtomicBool,
    second_release: std::sync::atomic::AtomicBool,
}

impl EventHub {
    pub(crate) fn new(source: String, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            source,
            next_sequence: AtomicU64::new(0),
            capacity: capacity.max(1),
            subscribers: Mutex::new(Vec::new()),
            #[cfg(test)]
            test_pause_after_first_reservation: Mutex::new(None),
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
        let mut subscribers = self.subscribers.lock();
        let envelope = DaemonEvent {
            source: self.source.clone(),
            sequence: self.next_sequence.fetch_add(1, Ordering::AcqRel) + 1,
            event,
        };
        #[cfg(test)]
        if envelope.sequence == 1 {
            let pause = self.test_pause_after_first_reservation.lock().clone();
            if let Some(pause) = pause {
                pause.reserved.store(true, Ordering::Release);
                while !pause.release.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        }
        #[cfg(test)]
        if envelope.sequence == 2 {
            let pause = self.test_pause_after_first_reservation.lock().clone();
            if let Some(pause) = pause {
                pause.second_reserved.store(true, Ordering::Release);
                while !pause.second_release.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        }
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
            restart_id: None,
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

    #[test]
    fn concurrent_publish_preserves_sequence_order_per_subscriber() {
        let hub = EventHub::new("daemon-a".to_string(), 4);
        let rx = hub.subscribe();
        let pause = Arc::new(TestSequencePause {
            reserved: std::sync::atomic::AtomicBool::new(false),
            release: std::sync::atomic::AtomicBool::new(false),
            second_reserved: std::sync::atomic::AtomicBool::new(false),
            second_release: std::sync::atomic::AtomicBool::new(false),
        });
        *hub.test_pause_after_first_reservation.lock() = Some(pause.clone());

        let first_hub = Arc::clone(&hub);
        let first = std::thread::spawn(move || {
            first_hub.notify(ApiEvent::TeamCreated {
                name: "first".to_string(),
                members: vec![],
            });
        });
        while !pause.reserved.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let second_hub = Arc::clone(&hub);
        let second = std::thread::spawn(move || {
            second_hub.notify(ApiEvent::TeamCreated {
                name: "second".to_string(),
                members: vec![],
            });
        });

        if let Some(subscribers) = hub.subscribers.try_lock() {
            // Before the ordering fix, the first publisher has not acquired
            // the subscriber lock yet. Hold it while the second publisher
            // reserves sequence 2, then let sequence 2 publish first.
            while !pause.second_reserved.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            pause.second_release.store(true, Ordering::Release);
            drop(subscribers);
            second.join().expect("second publisher");
            pause.release.store(true, Ordering::Release);
            first.join().expect("first publisher");
        } else {
            // After the ordering fix, the first publisher holds the lock
            // while paused, so the second publisher cannot reserve sequence 2.
            pause.release.store(true, Ordering::Release);
            first.join().expect("first publisher");
            while !pause.second_reserved.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            pause.second_release.store(true, Ordering::Release);
            second.join().expect("second publisher");
        }

        assert_eq!(rx.recv().expect("first event").sequence, 1);
        assert_eq!(rx.recv().expect("second event").sequence, 2);
    }
}
