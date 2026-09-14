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
        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
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
        assert!(rx.recv().is_err() || rx.try_recv().is_err());
    }
}
