//! CR-2026-06-14: `compute_next_poll_eta` extracted to this sibling module so
//! `ci/mod.rs` (at its grandfathered LOC ceiling) hosts only a re-export — the
//! same LOC-relief pattern already used for `merge_freshness`, NOT the larger
//! ci/mod.rs split tracked separately. Pure function; no IO / global state.

use serde_json::Value;

/// Sprint 54 P0-5 helper: estimate the next poll's epoch-millis tick from
/// `last_polled_at` + `effective_interval_secs` (or `interval_secs` when adaptive
/// backoff hasn't been computed yet). Returns `None` for fresh watches that
/// haven't polled yet. Shared with the `ci status` aggregator so the two surfaces
/// never disagree.
pub(crate) fn compute_next_poll_eta(watch: &Value) -> Option<i64> {
    let last_polled_at = watch["last_polled_at"].as_i64()?;
    let interval_secs = watch["effective_interval_secs"]
        .as_u64()
        .or_else(|| watch["interval_secs"].as_u64())
        .unwrap_or(60);
    // CR-2026-06-14: saturate — a huge interval_secs else overflows (panic/wrap).
    let ms = i64::try_from(interval_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000);
    Some(last_polled_at.saturating_add(ms))
}
