//! #969 RC1 — bounded-backoff attach detection for the App→Daemon race.
//!
//! The race: when operator runs `agend-terminal start` (detached daemon)
//! and then immediately `agend-terminal app` (interactive TUI) within
//! milliseconds, the App's `try_attach` calls
//! [`crate::daemon::find_active_run_dir`] before the daemon has finished
//! writing its `.daemon` file. `find_active_run_dir` returns `None`, App
//! bootstraps `Owned`, and BOTH processes end up polling
//! `check_ci_watches` → duplicate telegram notifications on CI completion.
//!
//! Fix: retry `find_active_run_dir` up to 3 times with 100ms backoff
//! before committing to `Owned`. 300ms total budget closes the race
//! window for typical daemon bootstrap latency (operator dispatches
//! report `.daemon` writes complete within ~150ms of process start).
//!
//! The helper does NOT cover all RC1 cracks (HOME divergence + zombie
//! daemon thread are out of scope for r0 per dispatch). The #984 dedup
//! module catches any RC1 duplicates that slip through this defense as
//! a wire-layer safety net.
//!
//! Contract-test-only: e2e race reproduction requires process orchestration
//! (a daemon mid-bootstrap with controlled `.daemon` write timing) that's
//! impractical for §3.20 SOP 1 deterministic tests. The contract test
//! covers the helper's retry behavior with a synthetic detect_fn callback;
//! the integration is exercised by manual canary verification.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default backoff schedule: 3 attempts × 100ms = 300ms total budget.
/// Picked per #969 PR 2 dispatch; pinning to operator data deferred
/// (dev-2 cross-audit Pushback 7).
pub(crate) const DEFAULT_MAX_ATTEMPTS: usize = 3;
pub(crate) const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);

/// Bounded-backoff wrapper around [`crate::daemon::find_active_run_dir`].
/// Calls the detection fn up to `max_attempts` times, sleeping
/// `backoff` between attempts. Returns the first `Some(path)` seen, or
/// `None` after exhausting attempts.
///
/// The `detect_fn` parameter is injected for testability — production
/// callers pass [`crate::daemon::find_active_run_dir`]; tests pass a
/// mock that returns canned values to verify retry semantics without
/// real filesystem state.
pub(crate) fn find_active_run_dir_with_backoff<F>(
    home: &Path,
    max_attempts: usize,
    backoff: Duration,
    mut detect_fn: F,
) -> Option<PathBuf>
where
    F: FnMut(&Path) -> Option<PathBuf>,
{
    for attempt in 0..max_attempts {
        if let Some(run_dir) = detect_fn(home) {
            if attempt > 0 {
                tracing::info!(
                    home = %home.display(),
                    run_dir = %run_dir.display(),
                    attempt = attempt + 1,
                    "#969 RC1: attach detected after backoff retry — closed the App/daemon race window"
                );
            }
            return Some(run_dir);
        }
        if attempt + 1 < max_attempts {
            std::thread::sleep(backoff);
        }
    }
    None
}

/// Production entry point: wraps [`crate::daemon::find_active_run_dir`]
/// with [`DEFAULT_MAX_ATTEMPTS`] × [`DEFAULT_BACKOFF`] retries.
pub(crate) fn find_active_run_dir_backoff(home: &Path) -> Option<PathBuf> {
    find_active_run_dir_with_backoff(
        home,
        DEFAULT_MAX_ATTEMPTS,
        DEFAULT_BACKOFF,
        crate::daemon::find_active_run_dir,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! #969 RC1 contract tests — verify retry semantics without depending
    //! on real daemon-bootstrap timing.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn tmp_home(slug: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agend-969-rc1-{}-{}-{}",
            slug,
            std::process::id(),
            id
        ))
    }

    /// T1 — happy path: detect_fn returns Some on first attempt. No
    /// retries fired; helper returns immediately.
    #[test]
    fn t1_returns_first_some_without_retry() {
        let home = tmp_home("t1");
        let expected = home.join("run").join("12345");
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);
        let expected_clone = expected.clone();

        let result =
            find_active_run_dir_with_backoff(&home, 3, Duration::from_millis(100), move |_h| {
                count_clone.fetch_add(1, Ordering::Relaxed);
                Some(expected_clone.clone())
            });

        assert_eq!(result, Some(expected));
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            1,
            "first Some must short-circuit retry loop"
        );
    }

    /// T2 — race-closed-on-retry: detect_fn returns None on first 2
    /// attempts, Some on 3rd. Verifies the helper retries up to
    /// max_attempts and surfaces the late-arriving Some.
    #[test]
    fn t2_closes_race_window_via_retry() {
        let home = tmp_home("t2");
        let expected = home.join("run").join("67890");
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);
        let expected_clone = expected.clone();

        let result = find_active_run_dir_with_backoff(
            &home,
            3,
            Duration::from_millis(10), // tight backoff for test speed
            move |_h| {
                let n = count_clone.fetch_add(1, Ordering::Relaxed);
                if n < 2 {
                    None
                } else {
                    Some(expected_clone.clone())
                }
            },
        );

        assert_eq!(
            result,
            Some(expected),
            "3rd attempt's Some must be returned"
        );
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            3,
            "all 3 attempts must fire when first 2 are None"
        );
    }

    /// T3 — bounded budget: detect_fn always returns None. After
    /// max_attempts the helper commits to None. No infinite loop.
    #[test]
    fn t3_bounded_attempts_then_returns_none() {
        let home = tmp_home("t3");
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        let start = Instant::now();
        let result =
            find_active_run_dir_with_backoff(&home, 3, Duration::from_millis(10), move |_h| {
                count_clone.fetch_add(1, Ordering::Relaxed);
                None
            });
        let elapsed = start.elapsed();

        assert_eq!(result, None);
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            3,
            "exhausts max_attempts before returning None"
        );
        // 2 inter-attempt sleeps × 10ms = ~20ms; allow 200ms slack for CI scheduling.
        assert!(
            elapsed < Duration::from_millis(200),
            "bounded budget — must not loop indefinitely; got elapsed={elapsed:?}"
        );
    }

    /// T4 — zero-attempt edge case: max_attempts=0 returns None
    /// immediately without calling detect_fn. Defensive contract pin.
    #[test]
    fn t4_zero_attempts_returns_none_without_calling_detect() {
        let home = tmp_home("t4");
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        let result =
            find_active_run_dir_with_backoff(&home, 0, Duration::from_millis(10), move |_h| {
                count_clone.fetch_add(1, Ordering::Relaxed);
                Some(PathBuf::from("/should/never/return"))
            });

        assert_eq!(result, None);
        assert_eq!(
            call_count.load(Ordering::Relaxed),
            0,
            "max_attempts=0 must not invoke detect_fn"
        );
    }

    /// T5 — default helper production wiring: `find_active_run_dir_backoff`
    /// returns None when home dir is empty (no run/ subdir). Anchors
    /// the production wiring against the underlying detection fn.
    #[test]
    fn t5_default_helper_returns_none_on_empty_home() {
        let home = tmp_home("t5");
        std::fs::create_dir_all(&home).unwrap();
        // No run/ subdir → find_active_run_dir returns None.
        let result = find_active_run_dir_backoff(&home);
        assert_eq!(result, None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
