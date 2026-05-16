//! #842 bridge↔daemon JSON-RPC idempotent retry — request_id + dedup cache.
//!
//! ## Symptom
//!
//! `agend-mcp-bridge::proxy_request` retries transport-level failures once
//! (`is_retriable_io` matches BrokenPipe / ConnectionReset / TimedOut / …)
//! by transparently re-sending the same JSON envelope on a fresh connection.
//! For idempotent reads (`list_*`, `binding_state`, …) this is harmless.
//! For side-effecting ops (`send`, channel inject, `task action=update`, …)
//! a retry can double-execute the handler when the daemon completed the
//! original request but the response delivery failed (slow channel IO + the
//! bridge's read timeout fires → TimedOut → retry → daemon re-executes).
//!
//! Phase 1 spike ([task t-20260516003843690911-2]) locked the design:
//!
//! ## Wire-format change
//!
//! The bridge generates a UUIDv4 `request_id` at envelope creation time
//! and reuses it on retry (the retry IS the same logical request, just
//! re-transported). The daemon parses `request_id` off the envelope at
//! dispatch entry. Missing field → skip dedup (legacy at-least-once
//! semantics for non-bridge clients).
//!
//! ## Daemon-side dedup cache (this module)
//!
//! A singleton [`DedupCache`] keyed by `request_id` stores three states:
//!
//! - **Fresh** — first sight of the id. Dispatch the handler, store the
//!   completion.
//! - **InProgress** — handler is still running. The second caller blocks
//!   on a shared [`std::sync::Condvar`] until the first thread stores
//!   its result, then returns the same response without re-executing.
//!   Synchronous wait (matches the existing thread-per-connection model
//!   in `src/api/mod.rs::serve` — no tokio runtime required).
//! - **Cached** / **Oversized** / **Errored** — terminal states. Subsequent
//!   lookups return the cached response (or an error marker for the
//!   pathological cases) without touching the handler.
//!
//! ## Bounds
//!
//! - TTL = 10 min (after `completed_at`); GC sweeps expired entries from
//!   the daemon supervisor tick (same pattern as `notification_dedup`).
//! - Per-entry payload cap = 64 KB (oversized responses are still delivered
//!   to the original requester; cache stores `Oversized` marker so retries
//!   get a deterministic error rather than a re-exec).
//! - Total cache ceiling = 64 MB (oldest-by-`completed_at` eviction on
//!   overflow; LRU not warranted because the workload has no hot-key
//!   re-hit pattern — each id is touched at most twice).
//! - Per-id waiter cap = 8 concurrent waiters (over-cap returns an
//!   immediate `in_progress` error rather than blocking, preventing a
//!   retry-storm from saturating the 32-slot api_handler thread pool).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Condvar, Mutex, OnceLock};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Time-to-live for completed entries before garbage collection.
pub const TTL: Duration = Duration::from_secs(600);

/// Per-entry payload cap. Responses larger than this are still delivered
/// to the original requester, but the cache stores an `Oversized` marker
/// instead of the payload — retries get a deterministic error rather than
/// re-exec.
pub const PER_ENTRY_CAP_BYTES: usize = 64 * 1024;

/// Total cache memory ceiling. On overflow, oldest-by-`completed_at`
/// entries are dropped.
pub const TOTAL_CAP_BYTES: usize = 64 * 1024 * 1024;

/// Maximum concurrent waiters on a single in-progress entry. Over-cap
/// callers receive an immediate `in_progress` error.
pub const WAITER_CAP: usize = 8;

/// Default wait timeout when a caller doesn't pass one. Covers the
/// longest per-tool budget (`mcp_proxy::tool_timeout` 60s for slow
/// spawn/deploy ops) plus a small margin.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(65);

// ---------------------------------------------------------------------------
// Inner state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum SlotResult {
    Cached(Value),
    Oversized,
    Errored(String),
}

#[derive(Default)]
struct Slot {
    result: Option<SlotResult>,
}

type NotifyHandle = Arc<(Mutex<Slot>, Condvar)>;

enum EntryState {
    InProgress(NotifyHandle),
    Cached(Value),
    Oversized,
    Errored(String),
}

struct Entry {
    state: EntryState,
    inserted_at: Instant,
    completed_at: Option<Instant>,
    response_bytes: usize,
    waiter_count: usize,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    total_bytes: usize,
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

pub struct DedupCache {
    inner: Mutex<Inner>,
    ttl: Duration,
    per_entry_cap: usize,
    total_cap: usize,
    waiter_cap: usize,
}

impl Default for DedupCache {
    fn default() -> Self {
        Self::with_caps(TTL, PER_ENTRY_CAP_BYTES, TOTAL_CAP_BYTES, WAITER_CAP)
    }
}

impl DedupCache {
    pub fn with_caps(
        ttl: Duration,
        per_entry_cap: usize,
        total_cap: usize,
        waiter_cap: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            per_entry_cap,
            total_cap,
            waiter_cap,
        }
    }

    /// Dispatch a handler with idempotent-retry semantics.
    ///
    /// - `request_id = None` → no dedup, handler always runs (legacy
    ///   at-least-once for clients that don't emit the field).
    /// - `request_id = Some(id)` → dedup. First call dispatches the handler
    ///   and caches the result. Concurrent duplicates wait on a shared
    ///   Condvar (up to `wait_timeout`) and observe the first thread's
    ///   response. Later duplicates within the TTL window return the
    ///   cached response without dispatching.
    pub fn dispatch<F>(
        &self,
        request_id: Option<&str>,
        wait_timeout: Duration,
        handler: F,
    ) -> Value
    where
        F: FnOnce() -> Value,
    {
        let _ = (
            request_id,
            wait_timeout,
            &self.inner,
            self.ttl,
            self.per_entry_cap,
            self.total_cap,
            self.waiter_cap,
        );
        // Silence the unused-FnOnce drop without invoking.
        drop(handler);
        unimplemented!("DedupCache::dispatch — C1 RED stub, C2 GREEN fills in")
    }

    /// Drop entries whose `completed_at` is older than `now - ttl`.
    /// Called per-tick from the daemon supervisor (mirror of #836's
    /// `notification_dedup::sweep_expired`).
    pub fn sweep_expired(&self) -> usize {
        self.sweep_expired_at(Instant::now())
    }

    pub fn sweep_expired_at(&self, _now: Instant) -> usize {
        unimplemented!("DedupCache::sweep_expired_at — C1 RED stub")
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("dedup inner mutex").entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-global cache used by `src/api/mod.rs::handle_session`.
pub fn global() -> &'static DedupCache {
    static CACHE: OnceLock<DedupCache> = OnceLock::new();
    CACHE.get_or_init(DedupCache::default)
}

/// Construct the over-cap `in_progress` error envelope. Exposed so the
/// daemon dispatch hook and the over-cap path stay in sync.
#[allow(dead_code)]
pub(crate) fn in_progress_error() -> Value {
    json!({
        "ok": false,
        "error": "in_progress (duplicate request_id still executing on another session)"
    })
}

#[allow(dead_code)]
pub(crate) fn oversized_error() -> Value {
    json!({
        "ok": false,
        "error": "duplicate request_id; original response exceeded cache size cap"
    })
}

#[allow(dead_code)]
pub(crate) fn handler_errored(detail: &str) -> Value {
    json!({
        "ok": false,
        "error": format!("duplicate request_id; original handler failed: {detail}")
    })
}

// ---------------------------------------------------------------------------
// Tests (C1 RED — all six fail against the `unimplemented!()` stubs above)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// (a) Same request_id submitted twice — handler must execute exactly
    /// once; the second caller must observe the first call's cached response.
    #[test]
    fn a_same_id_dedupes_handler_called_once() {
        let cache = DedupCache::default();
        let count = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&count);
        let resp1 = cache.dispatch(Some("req-A"), Duration::from_secs(5), move || {
            c1.fetch_add(1, Ordering::SeqCst);
            json!({"ok": true, "n": 1})
        });

        let c2 = Arc::clone(&count);
        let resp2 = cache.dispatch(Some("req-A"), Duration::from_secs(5), move || {
            c2.fetch_add(1, Ordering::SeqCst);
            json!({"ok": true, "n": 2})
        });

        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must execute exactly once for duplicate request_id"
        );
        assert_eq!(resp1, resp2, "duplicate must observe original response");
        assert_eq!(resp1["n"], 1);
    }

    /// (b) Different request_ids — both handlers execute. Cache must not
    /// collide across distinct ids.
    #[test]
    fn b_different_id_handler_called_twice() {
        let cache = DedupCache::default();
        let count = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&count);
        cache.dispatch(Some("req-B1"), Duration::from_secs(5), move || {
            c1.fetch_add(1, Ordering::SeqCst);
            json!({})
        });

        let c2 = Arc::clone(&count);
        cache.dispatch(Some("req-B2"), Duration::from_secs(5), move || {
            c2.fetch_add(1, Ordering::SeqCst);
            json!({})
        });

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// (c) In-progress race — S2 arrives while S1's handler is still
    /// executing. S2 must block on the shared Condvar and observe S1's
    /// result; handler runs once.
    #[test]
    fn c_in_progress_race_second_waits_for_first() {
        let cache = Arc::new(DedupCache::default());
        let count = Arc::new(AtomicUsize::new(0));

        let c = Arc::clone(&cache);
        let cnt = Arc::clone(&count);
        let t1 = thread::spawn(move || {
            c.dispatch(Some("req-C"), Duration::from_secs(5), move || {
                cnt.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(200));
                json!({"slow": "first"})
            })
        });

        // Give T1 enough head-start to install the InProgress entry.
        thread::sleep(Duration::from_millis(50));

        let c2 = Arc::clone(&cache);
        let cnt2 = Arc::clone(&count);
        let t2 = thread::spawn(move || {
            c2.dispatch(Some("req-C"), Duration::from_secs(5), move || {
                cnt2.fetch_add(1, Ordering::SeqCst);
                json!({"slow": "second-should-not-run"})
            })
        });

        let r1 = t1.join().expect("t1");
        let r2 = t2.join().expect("t2");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must run exactly once across the race"
        );
        assert_eq!(r1, r2, "S2 must observe S1's response");
        assert_eq!(r1["slow"], "first");
    }

    /// (d) Handler panic — waiters must wake with an error response
    /// (RAII `Drop` on the in-progress guard notifies the Condvar even
    /// during unwind). T2 must NOT hang and must NOT re-execute.
    #[test]
    fn d_handler_panic_notifies_waiters_with_error() {
        let cache = Arc::new(DedupCache::default());

        let c = Arc::clone(&cache);
        let t1 = thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                c.dispatch(Some("req-D"), Duration::from_secs(5), || {
                    panic!("simulated handler panic");
                })
            }))
        });

        thread::sleep(Duration::from_millis(50));

        let c2 = Arc::clone(&cache);
        let exec_count = Arc::new(AtomicUsize::new(0));
        let ec = Arc::clone(&exec_count);
        let t2 = thread::spawn(move || {
            c2.dispatch(Some("req-D"), Duration::from_secs(5), move || {
                ec.fetch_add(1, Ordering::SeqCst);
                json!({"never": "ran"})
            })
        });

        let _ = t1.join();
        let r2 = t2.join().expect("t2 must not panic");

        assert_eq!(
            exec_count.load(Ordering::SeqCst),
            0,
            "S2 handler must not execute on S1 panic — waiters get error response"
        );
        assert!(
            r2.get("error").is_some() || r2.get("ok") == Some(&json!(false)),
            "S2 expected an error response on S1 panic, got {r2:?}"
        );
    }

    /// (e) Oversized response — exceeds `per_entry_cap`. S1 still gets its
    /// full response, but the cache only stores an `Oversized` marker so
    /// the retry returns a deterministic error rather than re-executing.
    #[test]
    fn e_oversized_response_marker_blocks_re_exec() {
        let cache = DedupCache::with_caps(TTL, 100, TOTAL_CAP_BYTES, WAITER_CAP);
        let count = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&count);
        let resp1 = cache.dispatch(Some("req-E"), Duration::from_secs(5), move || {
            c1.fetch_add(1, Ordering::SeqCst);
            json!({"big": "x".repeat(500)})
        });
        assert_eq!(
            resp1["big"].as_str().map(|s| s.len()),
            Some(500),
            "S1 must still get its full response (cache policy doesn't truncate the wire)"
        );

        let c2 = Arc::clone(&count);
        let resp2 = cache.dispatch(Some("req-E"), Duration::from_secs(5), move || {
            c2.fetch_add(1, Ordering::SeqCst);
            json!({"never": "ran"})
        });
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "handler must not re-execute on retry of oversized response"
        );
        assert!(
            resp2.get("error").is_some() || resp2.get("ok") == Some(&json!(false)),
            "S2 expected oversized error, got {resp2:?}"
        );
    }

    /// (f) Missing request_id — legacy at-least-once. Every call executes
    /// the handler; no dedup state accumulates.
    #[test]
    fn f_missing_request_id_legacy_at_least_once() {
        let cache = DedupCache::default();
        let count = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let c = Arc::clone(&count);
            cache.dispatch(None, Duration::from_secs(5), move || {
                c.fetch_add(1, Ordering::SeqCst);
                json!({})
            });
        }

        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert!(
            cache.is_empty(),
            "missing request_id must not accumulate dedup state"
        );
    }
}
