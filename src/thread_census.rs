//! Thread census — lightweight counter-only thread lifecycle tracking.
//!
//! Sprint 26 PR-B: per operator pick A (counter-only, not JoinHandle root).
//! Doctor/bugreport reports active thread counts by kind.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

fn census() -> &'static Mutex<HashMap<&'static str, AtomicU32>> {
    static CENSUS: OnceLock<Mutex<HashMap<&'static str, AtomicU32>>> = OnceLock::new();
    CENSUS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a thread of the given kind. Returns a guard that decrements
/// the count on drop. Call at the start of a spawned thread's body.
pub fn register(kind: &'static str) -> ThreadGuard {
    let mut map = census().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(kind)
        .or_insert_with(|| AtomicU32::new(0))
        .fetch_add(1, Ordering::Relaxed);
    ThreadGuard { kind }
}

/// Snapshot of current thread counts by kind.
pub fn snapshot() -> Vec<(&'static str, u32)> {
    let map = census().lock().unwrap_or_else(|e| e.into_inner());
    map.iter()
        .map(|(k, v)| (*k, v.load(Ordering::Relaxed)))
        .filter(|(_, count)| *count > 0)
        .collect()
}

/// RAII guard — decrements the census count for its kind on drop.
pub struct ThreadGuard {
    kind: &'static str,
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        let map = census().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(counter) = map.get(self.kind) {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

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
                b.wait(); // all 3 threads alive
                b.wait(); // wait for main to snapshot
            }));
        }
        barrier.wait(); // all 3 alive
        let snap = snapshot();
        let count = snap
            .iter()
            .find(|(k, _)| *k == "test_multi")
            .map(|(_, v)| *v)
            .unwrap_or(0);
        assert_eq!(count, 3, "3 threads registered");
        barrier.wait(); // release threads
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
