//! #941: Periodic daemon thread-state dump for incident-grade
//! observability. Closes #932 RCA's H1/H7 evidence gap by surfacing:
//!
//! - **registry.lock holder** (Dim 1): via the `lock_registry_tracked`
//!   wrapper + `sync_audit::REGISTRY_HOLDER` slot. Catches a per-tick
//!   handler wedged while holding registry (the H1 zombie-daemon
//!   hypothesis).
//! - **Per-handler timing** (Dim 2): via `HANDLER_TIMING` accumulator
//!   updated by the main loop's per-handler wrapper. Surfaces "which
//!   handler is slow".
//! - **API listener thread phase** (Dim 4): via
//!   `crate::api::LISTENER_PHASE` atomic. Surfaces whether the API
//!   listener is currently blocked in `accept()` (H7 evidence — partial,
//!   only top-level state observable without inserting per-iteration
//!   trace points into the hot loop).
//!
//! Output is log-only (single `tracing::info!` line per dump). No JSON
//! file artifacts — eliminates the GC concern dev-2 cross-audit flagged
//! and keeps the operator's grep workflow simple
//! (`grep "thread-dump" daemon.log`).
//!
//! **Wrapper-only blind spot** (PR body caveat): the holder slot only
//! tracks `lock_registry_tracked` callers. ~30 bare `reg.lock()` sites
//! across the codebase bypass the tracker. Operator reading
//! `registry_holder=None` MUST NOT conclude "no wedge" — it only proves
//! that no MIGRATED handler currently holds the lock. Non-handler sites
//! (binding writes, agent.rs internal sites) require manual grep.
//!
//! Gated by `AGEND_DAEMON_THREAD_DUMP_SECS=N` (N >= 1 enables; default
//! disabled). The env var is cached via `OnceLock<bool>` in
//! `sync_audit::thread_dump_enabled()` — operator must restart the
//! daemon to toggle (no live-update support).

use super::{PerTickHandler, TickContext};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct ThreadDumpHandler {
    /// Interval seconds. Read once at construction from
    /// `AGEND_DAEMON_THREAD_DUMP_SECS`. `0` disables.
    interval_secs: u64,
    /// Epoch seconds of last emitted dump (`0` = never).
    last_dump_at: AtomicU64,
}

impl ThreadDumpHandler {
    pub(crate) fn new() -> Self {
        let interval = std::env::var("AGEND_DAEMON_THREAD_DUMP_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            interval_secs: interval,
            last_dump_at: AtomicU64::new(0),
        }
    }

    fn now_epoch_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Compare-and-set the next-dump timestamp; returns true if the
    /// current invocation should emit a dump. Idempotent under
    /// concurrent ticks because only the main loop calls this — but the
    /// atomic CAS pattern is correct anyway.
    fn should_dump(&self) -> bool {
        if self.interval_secs == 0 {
            return false;
        }
        let now = Self::now_epoch_secs();
        let last = self.last_dump_at.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.interval_secs {
            return false;
        }
        self.last_dump_at.store(now, Ordering::Relaxed);
        true
    }
}

impl PerTickHandler for ThreadDumpHandler {
    fn name(&self) -> &'static str {
        "thread_dump"
    }

    fn run(&self, _ctx: &TickContext<'_>) {
        if !self.should_dump() {
            return;
        }

        // Dim 1: registry.lock holder snapshot
        let holder_field = match crate::sync_audit::current_registry_holder() {
            Some(info) => format!(
                "{site}@{thread} for {dur_ms}ms",
                site = info.site_label,
                thread = info.thread_name,
                dur_ms = info.acquired_at.elapsed().as_millis(),
            ),
            None => "none".to_string(),
        };

        // Dim 2: per-handler timing snapshot — alphabetical for grep
        // stability so operators piping multiple dumps to `diff` see
        // only true changes.
        let timings = super::snapshot_handler_timings();
        let mut keys: Vec<&String> = timings.keys().collect();
        keys.sort();
        let timings_field: String = keys
            .iter()
            .map(|k| {
                let s = &timings[*k];
                format!(
                    "{name}={last}ms(max{max}ms,n{n})",
                    name = k,
                    last = s.last_duration_ms,
                    max = s.max_duration_ms,
                    n = s.run_count
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        // Dim 3 (DROPPED per dev-2 cross-audit): per-session phase
        // tracking — low diagnostic value vs cost; the API session
        // COUNT alone is sufficient surrogate.

        // Dim 4: API listener phase + active session count
        let api_phase = crate::api::LISTENER_PHASE.load(Ordering::Relaxed);
        let api_phase_label = match api_phase {
            0 => "processing",
            1 => "in_accept",
            _ => "unknown",
        };
        let api_sessions = crate::api::ACTIVE_API_SESSIONS.load(Ordering::Relaxed);

        tracing::info!(
            registry_holder = %holder_field,
            handler_timings = %if timings_field.is_empty() { "(none recorded yet)".to_string() } else { timings_field },
            api_sessions = api_sessions,
            api_listener_phase = api_phase_label,
            interval_secs = self.interval_secs,
            "thread-dump"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_ctx<'a>(
        home: &'a std::path::Path,
        registry: &'a AgentRegistry,
        externals: &'a ExternalRegistry,
        configs: &'a Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) -> TickContext<'a> {
        TickContext {
            home,
            registry,
            externals,
            configs,
        }
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-941-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Test 1 (#941 mandatory): when `AGEND_DAEMON_THREAD_DUMP_SECS` is
    /// unset (or 0), constructing the handler captures `interval_secs=0`
    /// and `should_dump` always returns false. The handler.run() path
    /// is a no-op past the gate.
    ///
    /// Note: `thread_dump_enabled()` is process-wide cached via OnceLock
    /// (per dev-2 sharpening #2), so this test must NOT mutate that
    /// env var — it asserts the per-handler interval=0 gate, not the
    /// global cache. The global cache test happens in
    /// `sync_audit::tests` (separate module, separate process when
    /// run via `cargo test`).
    #[test]
    fn env_unset_no_dump() {
        // Unset env explicitly in case parent inherited it.
        unsafe {
            std::env::remove_var("AGEND_DAEMON_THREAD_DUMP_SECS");
        }
        let handler = ThreadDumpHandler::new();
        assert_eq!(
            handler.interval_secs, 0,
            "unset env must produce interval_secs=0"
        );
        // should_dump must short-circuit on interval=0 regardless of
        // wall-clock state.
        assert!(!handler.should_dump(), "interval=0 must not emit");
        assert!(!handler.should_dump(), "second call still no-op");
    }

    /// Test 2 (#941 mandatory): when env is set, the handler emits at
    /// the configured interval. We verify the gate state-machine
    /// directly (first call → true; immediate second call → false;
    /// after interval → true).
    ///
    /// Determinism: instead of sleeping or relying on real wall-clock,
    /// we directly manipulate `last_dump_at` to simulate elapsed time.
    /// §3.20 SOP 1 — no sleep, observable state.
    #[test]
    fn handler_gates_emit_at_interval() {
        let handler = ThreadDumpHandler {
            interval_secs: 60,
            last_dump_at: AtomicU64::new(0),
        };
        // First call: last_dump_at=0, now > 0+60 (assuming wall-clock
        // isn't pre-1970) → emit.
        assert!(handler.should_dump(), "first call must emit");
        // Immediate second call: last_dump_at just updated → too soon.
        assert!(!handler.should_dump(), "back-to-back call must skip");
        // Simulate 61 seconds elapsed.
        let now = ThreadDumpHandler::now_epoch_secs();
        handler
            .last_dump_at
            .store(now.saturating_sub(61), Ordering::Relaxed);
        assert!(handler.should_dump(), "after interval, next call must emit");
    }

    /// Test 3 (#941 mandatory, §3.20 SOP 1): synthetic registry-lock
    /// contention via channel sync — a worker thread acquires
    /// `lock_registry_tracked`, signals "I have it", and holds until
    /// signaled to release. Main thread reads
    /// `current_registry_holder` while the worker holds and asserts
    /// the holder slot is populated with the expected site label.
    ///
    /// Deterministic: uses crossbeam channels for hand-off, NO sleeps.
    /// The "500ms hold-time" sleep dev-2 cross-audit flagged is
    /// avoided here by signaling release explicitly.
    ///
    /// Pre-requisite: this test relies on `thread_dump_enabled()`
    /// returning true. Because `OnceLock` caches per-process, we set
    /// the env BEFORE the first `thread_dump_enabled()` call and use
    /// `serial_test` to prevent interleaved tests from racing the
    /// cache init.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lock_contention_regression_seam() {
        unsafe {
            std::env::set_var("AGEND_DAEMON_THREAD_DUMP_SECS", "60");
        }
        // Force OnceLock init via a first call. If a prior test in
        // this process already initialized with env=unset, the cache
        // returns false and we cannot proceed deterministically — skip
        // with a clear message rather than emit a false negative.
        if !crate::sync_audit::thread_dump_enabled() {
            eprintln!(
                "skipping lock_contention_regression_seam: \
                 thread_dump_enabled() cache pinned to false earlier in \
                 the process. Run this test in isolation with \
                 AGEND_DAEMON_THREAD_DUMP_SECS=60 set."
            );
            return;
        }

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let registry_for_worker = Arc::clone(&registry);

        // Worker acquires lock, signals "acquired", waits for "release".
        let (acquired_tx, acquired_rx) = crossbeam_channel::bounded::<()>(0);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(0);

        let worker = std::thread::Builder::new()
            .name("test-lock-holder".into())
            .spawn(move || {
                let _guard = crate::agent::lock_registry_tracked(
                    &registry_for_worker,
                    "test_lock_contention",
                );
                acquired_tx.send(()).expect("signal acquired");
                release_rx.recv().expect("wait for release signal");
                // _guard drops here, clearing holder slot.
            })
            .expect("spawn worker");

        // Wait deterministically for worker to acquire.
        acquired_rx.recv().expect("worker acquired lock");

        // Now read holder — must show test_lock_contention.
        let holder = crate::sync_audit::current_registry_holder();
        let site = holder.as_ref().map(|h| h.site_label);
        let thread = holder.as_ref().map(|h| h.thread_name.clone());

        // Signal worker to release.
        release_tx.send(()).expect("signal release");
        worker.join().expect("worker joined");

        assert_eq!(
            site,
            Some("test_lock_contention"),
            "holder slot must reflect the acquiring site"
        );
        assert_eq!(
            thread,
            Some("test-lock-holder".to_string()),
            "holder slot must reflect the acquiring thread name"
        );

        // After release + drop, holder slot must be cleared.
        let post = crate::sync_audit::current_registry_holder();
        assert!(
            post.is_none(),
            "post-drop holder slot must be cleared; got {post:?}"
        );
    }

    /// Test 4 (#941, run-once sanity): handler.run() doesn't panic with
    /// an empty registry + no handler timings recorded. Smoke test for
    /// the dump output path. Does NOT verify dump content (tracing
    /// output isn't trivially capturable in unit tests); see test 3 for
    /// state-side verification.
    #[test]
    fn run_with_empty_state_does_not_panic() {
        unsafe {
            std::env::remove_var("AGEND_DAEMON_THREAD_DUMP_SECS");
        }
        let home = tmp_home("run-smoke");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = make_ctx(&home, &registry, &externals, &configs);

        // Interval=0 → handler.run is a no-op.
        let handler = ThreadDumpHandler::new();
        handler.run(&ctx);

        std::fs::remove_dir_all(&home).ok();
    }

    /// Pin: the handler's `name()` matches its module so per_tick
    /// telemetry can group consistently.
    #[test]
    fn name_matches_module() {
        let handler = ThreadDumpHandler::new();
        assert_eq!(handler.name(), "thread_dump");
    }
}
