//! Shared synchronization helpers.
//!
//! `lock_poisoned` was the centralised poison-recovery wrapper for
//! `std::sync::Mutex`. With the migration to `parking_lot::Mutex`
//! (which never poisons), the function has been removed. All call-sites
//! now use `.lock()` directly.

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    #[test]
    fn lock_returns_guard_on_healthy_mutex() {
        let m = Mutex::new(5u32);
        let guard = m.lock();
        assert_eq!(*guard, 5);
    }
}
