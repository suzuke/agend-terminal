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
//! Phase 1 spike (task t-20260516003843690911-2) locked the design:
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
use std::sync::Arc;
use std::sync::{Condvar, Mutex, OnceLock};
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
    /// `None` while the handler is still in flight; set to `Some(now)`
    /// when the in-progress guard transitions the entry to a terminal
    /// state. Drives both TTL eviction and overflow-policy ordering.
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
    pub fn dispatch<F>(&self, request_id: Option<&str>, wait_timeout: Duration, handler: F) -> Value
    where
        F: FnOnce() -> Value,
    {
        let id = match request_id.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => return handler(),
        };

        // First lookup — either we register Fresh (and run the handler),
        // or we observe a terminal state, or we attach as a waiter.
        let action = self.check_or_register(&id);
        match action {
            CheckOutcome::Cached(v) => v,
            CheckOutcome::Oversized => oversized_error(),
            CheckOutcome::Errored(detail) => handler_errored(&detail),
            CheckOutcome::OverCap => in_progress_error(),
            CheckOutcome::Wait(handle) => self.wait_for(&id, handle, wait_timeout),
            CheckOutcome::Fresh => {
                let mut guard = InProgressGuard {
                    cache: self,
                    request_id: id,
                    completed: false,
                };
                let response = handler();
                guard.complete(response.clone());
                response
            }
        }
    }

    fn check_or_register(&self, id: &str) -> CheckOutcome {
        let mut inner = self.inner.lock().expect("dedup inner mutex");
        match inner.entries.get_mut(id) {
            None => {
                let handle: NotifyHandle = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
                inner.entries.insert(
                    id.to_string(),
                    Entry {
                        state: EntryState::InProgress(handle),
                        completed_at: None,
                        response_bytes: 0,
                        waiter_count: 0,
                    },
                );
                CheckOutcome::Fresh
            }
            Some(entry) => match &entry.state {
                EntryState::Cached(v) => CheckOutcome::Cached(v.clone()),
                EntryState::Oversized => CheckOutcome::Oversized,
                EntryState::Errored(detail) => CheckOutcome::Errored(detail.clone()),
                EntryState::InProgress(handle) => {
                    if entry.waiter_count >= self.waiter_cap {
                        tracing::warn!(
                            request_id = id,
                            waiter_count = entry.waiter_count,
                            cap = self.waiter_cap,
                            "request_dedup waiter cap reached — returning in_progress error"
                        );
                        return CheckOutcome::OverCap;
                    }
                    entry.waiter_count += 1;
                    CheckOutcome::Wait(Arc::clone(handle))
                }
            },
        }
    }

    fn wait_for(&self, id: &str, handle: NotifyHandle, timeout: Duration) -> Value {
        let (mutex, condvar) = (&handle.0, &handle.1);
        let mut slot = mutex.lock().expect("notify slot mutex");
        let deadline = Instant::now().checked_add(timeout);
        while slot.result.is_none() {
            let remaining = match deadline {
                Some(d) => d.checked_duration_since(Instant::now()),
                None => Some(timeout),
            };
            match remaining {
                Some(dur) if !dur.is_zero() => {
                    let (g, wt) = condvar
                        .wait_timeout(slot, dur)
                        .expect("condvar wait_timeout");
                    slot = g;
                    if wt.timed_out() && slot.result.is_none() {
                        break;
                    }
                }
                _ => break,
            }
        }
        let result = slot.result.clone();
        drop(slot);

        // Decrement waiter count — entry may already be terminal but the
        // counter is harmless once nobody is reading it.
        {
            let mut inner = self.inner.lock().expect("dedup inner mutex");
            if let Some(entry) = inner.entries.get_mut(id) {
                if entry.waiter_count > 0 {
                    entry.waiter_count -= 1;
                }
            }
        }

        match result {
            Some(SlotResult::Cached(v)) => v,
            Some(SlotResult::Oversized) => oversized_error(),
            Some(SlotResult::Errored(detail)) => handler_errored(&detail),
            None => in_progress_error(),
        }
    }

    /// Used by `InProgressGuard::complete` and its `Drop` to swap an
    /// InProgress entry into a terminal state and notify waiters.
    fn finalize(&self, id: &str, outcome: SlotResult) {
        let handle_to_notify = {
            let mut inner = self.inner.lock().expect("dedup inner mutex");
            let mut bytes_delta: usize = 0;
            let handle = {
                let Some(entry) = inner.entries.get_mut(id) else {
                    return;
                };
                let handle = match &entry.state {
                    EntryState::InProgress(h) => Some(Arc::clone(h)),
                    _ => None,
                };
                match &outcome {
                    SlotResult::Cached(v) => {
                        let bytes = estimated_bytes(v);
                        entry.state = EntryState::Cached(v.clone());
                        entry.response_bytes = bytes;
                        bytes_delta = bytes;
                    }
                    SlotResult::Oversized => {
                        entry.state = EntryState::Oversized;
                        entry.response_bytes = 0;
                    }
                    SlotResult::Errored(detail) => {
                        entry.state = EntryState::Errored(detail.clone());
                        entry.response_bytes = 0;
                    }
                }
                entry.completed_at = Some(Instant::now());
                handle
            };
            if bytes_delta > 0 {
                inner.total_bytes = inner.total_bytes.saturating_add(bytes_delta);
            }
            self.evict_to_fit(&mut inner);
            handle
        };

        if let Some(handle) = handle_to_notify {
            let mut slot = handle.0.lock().expect("notify slot mutex");
            slot.result = Some(outcome);
            drop(slot);
            handle.1.notify_all();
        }
    }

    fn evict_to_fit(&self, inner: &mut Inner) {
        if inner.total_bytes <= self.total_cap {
            return;
        }
        // Collect terminal entries ordered by completed_at ascending.
        // InProgress entries (completed_at = None) are skipped — they're
        // not "old" in any meaningful sense and can't be replayed safely.
        let mut victims: Vec<(String, Instant, usize)> = inner
            .entries
            .iter()
            .filter_map(|(k, e)| {
                if e.response_bytes == 0 {
                    return None;
                }
                let completed = e.completed_at?;
                Some((k.clone(), completed, e.response_bytes))
            })
            .collect();
        victims.sort_by_key(|(_, t, _)| *t);
        for (id, _, bytes) in victims {
            if inner.total_bytes <= self.total_cap {
                break;
            }
            if inner.entries.remove(&id).is_some() {
                inner.total_bytes = inner.total_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Drop entries whose `completed_at` is older than `now - ttl`.
    /// Called per-tick from the daemon supervisor (mirror of #836's
    /// `notification_dedup::sweep_expired`).
    pub fn sweep_expired(&self) -> usize {
        self.sweep_expired_at(Instant::now())
    }

    pub fn sweep_expired_at(&self, now: Instant) -> usize {
        let mut inner = self.inner.lock().expect("dedup inner mutex");
        let ttl = self.ttl;
        let mut reclaimed_bytes: usize = 0;
        let mut dropped = 0usize;
        inner.entries.retain(|_, entry| {
            let keep = match entry.completed_at {
                Some(t) => now
                    .checked_duration_since(t)
                    .map(|d| d < ttl)
                    .unwrap_or(true),
                None => true,
            };
            if !keep {
                reclaimed_bytes = reclaimed_bytes.saturating_add(entry.response_bytes);
                dropped += 1;
            }
            keep
        });
        inner.total_bytes = inner.total_bytes.saturating_sub(reclaimed_bytes);
        dropped
    }

    #[allow(dead_code)] // introspection helper (used by tests + future operator endpoints)
    pub fn len(&self) -> usize {
        self.inner.lock().expect("dedup inner mutex").entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

enum CheckOutcome {
    Fresh,
    Wait(NotifyHandle),
    Cached(Value),
    Oversized,
    Errored(String),
    OverCap,
}

// ---------------------------------------------------------------------------
// RAII completion guard
// ---------------------------------------------------------------------------

struct InProgressGuard<'a> {
    cache: &'a DedupCache,
    request_id: String,
    completed: bool,
}

impl InProgressGuard<'_> {
    fn complete(&mut self, response: Value) {
        self.completed = true;
        let outcome = if estimated_bytes(&response) > self.cache.per_entry_cap {
            SlotResult::Oversized
        } else {
            SlotResult::Cached(response)
        };
        self.cache.finalize(&self.request_id, outcome);
    }
}

impl Drop for InProgressGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // Handler panicked or returned without calling complete().
        // Promote the entry to Errored so concurrent waiters wake with
        // a deterministic error instead of stalling until wait_timeout.
        self.cache.finalize(
            &self.request_id,
            SlotResult::Errored("handler aborted (panic or early return)".to_string()),
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn estimated_bytes(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

/// Process-global cache used by `src/api/mod.rs::handle_session`.
pub fn global() -> &'static DedupCache {
    static CACHE: OnceLock<DedupCache> = OnceLock::new();
    CACHE.get_or_init(DedupCache::default)
}

/// Per-API-method dedup wait budget. Aligned to the handler's own time
/// budget (the spike Q3 contract: "timeout = method's tool_timeout") so
/// a waiter never sleeps past the point where the in-flight handler
/// would itself time out. For `mcp_tool`, defers to
/// [`crate::api::handlers::mcp_proxy::tool_timeout`] keyed on
/// `params["tool"]` — shared source of truth across the dispatch budget
/// and the dedup wait budget. Unmapped methods fall back to
/// [`DEFAULT_WAIT_TIMEOUT`] so adding a new API method without updating
/// this map degrades to a longer-than-needed wait rather than a panic
/// or hang.
pub fn method_wait_timeout(method: &str, params: &Value) -> Duration {
    use crate::api::method as m;
    match method {
        // Fast read-only / atomic-flip operations.
        m::LIST
        | m::STATUS
        | m::PANE_SNAPSHOT
        | m::MCP_TOOLS_LIST
        | m::SHUTDOWN
        | m::SET_BLOCKED_REASON
        | m::CLEAR_BLOCKED_REASON
        | m::REGISTER_EXTERNAL
        | m::DEREGISTER_EXTERNAL
        | m::MOVE_PANE => Duration::from_secs(5),

        // Slow operations that spawn processes / write a lot of state.
        m::SPAWN | m::CREATE_TEAM | m::UPDATE_TEAM => Duration::from_secs(60),

        // `mcp_tool` dispatches the actual tool inside the daemon with
        // its OWN per-tool budget (see `mcp_proxy::tool_timeout`).
        // Mirror that map exactly so the dedup wait can never outrun
        // the handler's own deadline.
        m::MCP_TOOL => {
            let tool = params.get("tool").and_then(|v| v.as_str()).unwrap_or("");
            crate::api::handlers::mcp_proxy::tool_timeout(tool)
        }

        // Middle band — covers `send`, `inject`, `kill`, `delete`,
        // `verify_push`, and any future method not yet classified.
        m::SEND | m::INJECT | m::KILL | m::DELETE | m::VERIFY_PUSH => Duration::from_secs(30),

        // Unknown / unmapped — fall back to the conservative default.
        _ => DEFAULT_WAIT_TIMEOUT,
    }
}

/// Construct the over-cap `in_progress` error envelope. Exposed so the
/// daemon dispatch hook and the over-cap path stay in sync.
pub(crate) fn in_progress_error() -> Value {
    json!({
        "ok": false,
        "error": "in_progress (duplicate request_id still executing on another session)"
    })
}

pub(crate) fn oversized_error() -> Value {
    json!({
        "ok": false,
        "error": "duplicate request_id; original response exceeded cache size cap"
    })
}

pub(crate) fn handler_errored(detail: &str) -> Value {
    json!({
        "ok": false,
        "error": format!("duplicate request_id; original handler failed: {detail}")
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// #868 — spin-wait helper used by the in-progress / over-cap
    /// coordination tests below. Replaces the old `thread::sleep(50ms)`
    /// gating which flaked on slow macOS GH-runners (#856 + #866 + #867).
    /// Observes the production `cache.inner` state directly — no
    /// production code changes, no test hook injection. Panics with a
    /// clear deadline message if the predicate never holds.
    fn wait_until<F: FnMut() -> bool>(deadline: Duration, mut predicate: F) {
        let started = Instant::now();
        while !predicate() {
            if started.elapsed() > deadline {
                panic!("#868 wait_until: predicate did not hold within {deadline:?}");
            }
            thread::yield_now();
        }
    }

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

        // #868 hardening — wait for T1 to install the InProgress entry
        // by observing `cache.inner` directly instead of betting on
        // 50ms scheduler latency. Mirror the over-cap test's pattern.
        wait_until(Duration::from_secs(2), || {
            cache
                .inner
                .lock()
                .expect("inner mutex")
                .entries
                .contains_key("req-C")
        });

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

    /// Sweep eviction — entries past TTL are reclaimed; in-flight entries
    /// (no completed_at) are preserved.
    #[test]
    fn sweep_expired_drops_old_terminal_entries_only() {
        let cache = DedupCache::with_caps(
            Duration::from_secs(1),
            PER_ENTRY_CAP_BYTES,
            TOTAL_CAP_BYTES,
            WAITER_CAP,
        );
        cache.dispatch(Some("old-1"), Duration::from_secs(5), || json!({"k": "v"}));
        cache.dispatch(Some("old-2"), Duration::from_secs(5), || json!({"k": "v"}));
        assert_eq!(cache.len(), 2);

        // Move time forward past TTL.
        let future = Instant::now() + Duration::from_secs(5);
        let dropped = cache.sweep_expired_at(future);
        assert_eq!(dropped, 2);
        assert_eq!(cache.len(), 0);
    }

    /// Over-cap waiters — once `waiter_cap` is reached, additional
    /// callers fail fast with `in_progress` rather than blocking.
    #[test]
    fn over_cap_waiters_get_in_progress_error_fast() {
        let cache = Arc::new(DedupCache::with_caps(
            TTL,
            PER_ENTRY_CAP_BYTES,
            TOTAL_CAP_BYTES,
            2,
        ));

        // T1 holds the entry InProgress for a while. The handler's
        // 300ms sleep is intentional substantive work — it represents
        // T1 holding InProgress while T4 dispatches, NOT coordination.
        let c = Arc::clone(&cache);
        let t1 = thread::spawn(move || {
            c.dispatch(Some("req-cap"), Duration::from_secs(5), || {
                thread::sleep(Duration::from_millis(300));
                json!({"first": true})
            })
        });
        // #868 hardening — wait for T1 to install the InProgress entry
        // by observing `cache.inner` directly. Old `thread::sleep(50ms)`
        // flaked on slow macOS GH-runners (#856, #866, #867).
        wait_until(Duration::from_secs(2), || {
            cache
                .inner
                .lock()
                .expect("inner mutex")
                .entries
                .contains_key("req-cap")
        });

        // T2 + T3 fill the waiter slots.
        let c2 = Arc::clone(&cache);
        let t2 = thread::spawn(move || {
            c2.dispatch(Some("req-cap"), Duration::from_secs(5), || json!({}))
        });
        let c3 = Arc::clone(&cache);
        let t3 = thread::spawn(move || {
            c3.dispatch(Some("req-cap"), Duration::from_secs(5), || json!({}))
        });
        // #868 hardening — wait for T2 + T3 to attach as waiters
        // (increment `waiter_count` to the cap) before dispatching T4.
        // Old `thread::sleep(50ms)` flaked on slow macOS GH-runners;
        // T4 would otherwise become a 3rd waiter and block 5s on the
        // Condvar instead of fast-failing with `OverCap`.
        wait_until(Duration::from_secs(2), || {
            cache
                .inner
                .lock()
                .expect("inner mutex")
                .entries
                .get("req-cap")
                .map(|e| e.waiter_count >= 2)
                .unwrap_or(false)
        });

        // T4 — over cap — should fail fast.
        let started = Instant::now();
        let resp4 = cache.dispatch(
            Some("req-cap"),
            Duration::from_secs(5),
            || json!({"never": "ran"}),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "over-cap caller should not block (took {elapsed:?})"
        );
        assert_eq!(
            resp4["error"].as_str().unwrap_or(""),
            "in_progress (duplicate request_id still executing on another session)"
        );

        // Wait for the legitimate waiters to finish.
        let _ = t1.join();
        let r2 = t2.join().expect("t2");
        let r3 = t3.join().expect("t3");
        assert_eq!(r2, r3);
        assert_eq!(r2["first"], true);
    }

    /// Total-cap eviction — oldest-by-completed_at terminal entry drops
    /// when a fresh completion pushes us over the ceiling.
    #[test]
    fn total_cap_overflow_evicts_oldest_terminal_entry() {
        // Set caps so two 50-byte responses fit but the third forces an
        // eviction. Each json!({"k": "v"}) string-encodes to 9 bytes
        // (`{"k":"v"}`), so a per-entry/total budget around 20 bytes is
        // tight enough to trigger eviction on the third insert.
        let cache = DedupCache::with_caps(TTL, 64 * 1024, 20, WAITER_CAP);
        cache.dispatch(Some("e1"), Duration::from_secs(5), || json!({"k": "v"}));
        thread::sleep(Duration::from_millis(2));
        cache.dispatch(Some("e2"), Duration::from_secs(5), || json!({"k": "v"}));
        thread::sleep(Duration::from_millis(2));
        cache.dispatch(Some("e3"), Duration::from_secs(5), || json!({"k": "v"}));
        // After eviction we expect at most 2 entries.
        let len = cache.len();
        assert!(
            len <= 2,
            "expected total-cap eviction to drop at least one entry, got len={len}"
        );
        // The oldest entry (e1) should be the one evicted; e3 must be
        // present.
        let inner = cache.inner.lock().expect("inner mutex");
        assert!(
            inner.entries.contains_key("e3"),
            "newest entry must survive"
        );
        assert!(
            !inner.entries.contains_key("e1") || inner.entries.contains_key("e2"),
            "if e1 still present, e2 must also be present (oldest-first eviction)"
        );
    }

    /// Per-method wait budget MUST differentiate by method (spike Q3
    /// contract): fast read-only methods get 5s, slow spawn-class methods
    /// get 60s, and `mcp_tool` defers to `mcp_proxy::tool_timeout` keyed
    /// on `params["tool"]`. A single global constant violates the spike
    /// design.
    #[test]
    fn method_wait_timeout_differs_per_method() {
        use crate::api::method;
        let empty = json!({});
        // Fast read-only → 5s
        assert_eq!(
            method_wait_timeout(method::LIST, &empty),
            Duration::from_secs(5),
            "list (read-only) → 5s"
        );
        assert_eq!(
            method_wait_timeout(method::PANE_SNAPSHOT, &empty),
            Duration::from_secs(5),
            "pane_snapshot (read-only) → 5s"
        );
        // Slow spawn-class → 60s
        assert_eq!(
            method_wait_timeout(method::SPAWN, &empty),
            Duration::from_secs(60),
            "spawn → 60s"
        );
        // Default middle band → 30s
        assert_eq!(
            method_wait_timeout(method::SEND, &empty),
            Duration::from_secs(30),
            "send (middle) → 30s"
        );
        // mcp_tool defers to mcp_proxy::tool_timeout keyed on params["tool"]
        let create = json!({"tool": "create_instance"});
        assert_eq!(
            method_wait_timeout(method::MCP_TOOL, &create),
            Duration::from_secs(60),
            "mcp_tool(create_instance) → 60s via tool_timeout"
        );
        let inbox = json!({"tool": "inbox"});
        assert_eq!(
            method_wait_timeout(method::MCP_TOOL, &inbox),
            Duration::from_secs(5),
            "mcp_tool(inbox) → 5s via tool_timeout"
        );
        // Unknown method → DEFAULT_WAIT_TIMEOUT fallback
        assert_eq!(
            method_wait_timeout("not-a-real-method", &empty),
            DEFAULT_WAIT_TIMEOUT,
            "unknown method falls back to DEFAULT_WAIT_TIMEOUT"
        );
    }
}
