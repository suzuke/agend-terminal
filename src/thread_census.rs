//! Thread census — lightweight counter-only thread lifecycle tracking.
//!
//! Sprint 26 PR-B: per operator pick A (counter-only, not JoinHandle root).

// Stub — impl lands in next commit.

/// Register a thread of the given kind. Returns a guard that decrements
/// the count on drop.
pub fn register(_kind: &'static str) -> ThreadGuard {
    todo!("thread census register not yet implemented")
}

/// Snapshot of current thread counts by kind.
pub fn snapshot() -> Vec<(&'static str, u32)> {
    todo!("thread census snapshot not yet implemented")
}

/// RAII guard — decrements the census count for its kind on drop.
pub struct ThreadGuard;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_increments_and_drop_decrements() {
        let guard = register("test_register");
        let snap = snapshot();
        let count = snap
            .iter()
            .find(|(k, _)| *k == "test_register")
            .map(|(_, v)| *v)
            .unwrap_or(0);
        assert!(count >= 1, "register must increment");
        drop(guard);
        let snap2 = snapshot();
        let count2 = snap2
            .iter()
            .find(|(k, _)| *k == "test_register")
            .map(|(_, v)| *v)
            .unwrap_or(0);
        assert!(count2 < count, "drop must decrement");
    }

    #[test]
    fn multi_thread_census_accurate() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut handles = vec![];
        for _ in 0..3 {
            let b = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let _guard = register("test_multi");
                b.wait();
                b.wait();
            }));
        }
        barrier.wait();
        let snap = snapshot();
        let count = snap
            .iter()
            .find(|(k, _)| *k == "test_multi")
            .map(|(_, v)| *v)
            .unwrap_or(0);
        assert_eq!(count, 3, "3 threads registered");
        barrier.wait();
        for h in handles {
            h.join().expect("join");
        }
        let snap2 = snapshot();
        let count2 = snap2
            .iter()
            .find(|(k, _)| *k == "test_multi")
            .map(|(_, v)| *v)
            .unwrap_or(0);
        assert_eq!(count2, 0, "all threads exited");
    }
}
