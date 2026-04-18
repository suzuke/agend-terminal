//! Shared synchronization helpers.
//!
//! `lock_poisoned` centralises what used to be `.lock().unwrap_or_else(|e|
//! e.into_inner())` sprinkled across daemon.rs. The silent recovery was
//! flagged in the 2026-04-18 review (P2-1) because panics inside a critical
//! section became invisible — a poisoned Mutex just kept running on whatever
//! state the panicked thread left behind, sometimes inconsistent. This helper
//! preserves the recovery (bailing the whole daemon on poison would itself
//! be worse, since the crash reaper thread depends on these locks to do its
//! job) but emits a structured `tracing::error!` every time we recover so
//! poisoning no longer masquerades as normal operation.

use std::sync::{Mutex, MutexGuard};

/// Acquire `mutex` and recover from poisoning with a logged warning.
///
/// `label` is a short, stable identifier (e.g. `"registry"` or `"configs"`)
/// that appears in the log message so operators can correlate which lock
/// tripped. Keep it static — callers pass string literals.
pub fn lock_poisoned<'a, T: ?Sized>(mutex: &'a Mutex<T>, label: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(
                lock = label,
                "mutex poisoned — a previous holder panicked; recovering with stale state"
            );
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn lock_poisoned_returns_guard_on_healthy_mutex() {
        let m = Mutex::new(5u32);
        let guard = lock_poisoned(&m, "healthy");
        assert_eq!(*guard, 5);
    }

    #[test]
    fn lock_poisoned_recovers_poisoned_mutex() {
        let m = Arc::new(Mutex::new(0u32));
        let m2 = Arc::clone(&m);
        let handle = std::thread::spawn(move || {
            let _g = m2.lock().expect("first lock");
            panic!("intentional panic to poison the mutex");
        });
        // We expect the spawned thread to panic.
        let _ = handle.join();
        assert!(m.is_poisoned(), "mutex should be poisoned after panic");
        let guard = lock_poisoned(&m, "poisoned_test");
        assert_eq!(*guard, 0, "poison recovery must expose last-known value");
    }
}
