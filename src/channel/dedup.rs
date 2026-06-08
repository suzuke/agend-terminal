//! #969: short-TTL content-hash dedup for outbound channel sends.
//!
//! Catches duplicate emissions from any source:
//!
//! - **RC1**: app-side + daemon-side `check_ci_watches` racing to send the
//!   same CI notification when `agend-terminal start` (detached) +
//!   `agend-terminal app` (owned-by-mistake) both poll
//! - **RC2**: PTY mirror dispatcher + `handle_reply` MCP tool racing to
//!   send the same agent reply text (RC2 root-cause fix moves
//!   `mirror_skip` earlier in this same PR; the dedup catches any
//!   residual window the flag-order fix leaves open)
//! - **RC3**: future channel retry sites that re-send without
//!   distinguishing "ack lost" from "actually failed" (current production
//!   retry is narrow + safe — see `notify::notify_telegram_inner`'s
//!   topic-deleted recovery — but dedup is forward-defense for any new
//!   retry caller that lands without sufficient care)
//!
//! ## Design
//!
//! Per-process singleton (`DEDUP`). Each outbound send computes a
//! [`DedupKey`] from `(channel_kind, instance_name, topic_id, content_hash)`
//! and calls [`DedupCache::record_and_check`] BEFORE actually sending. The
//! cache returns `false` if this exact triple was sent within the TTL
//! window (default 5 s, configurable via `channel.dedup_ttl_secs` in
//! `fleet.yaml`), in which case the caller skips the send and emits a
//! `tracing::info!` audit line.
//!
//! ## §3.20 SOP 1 determinism
//!
//! Clock is injected via the [`Clock`] trait. Production uses [`SystemClock`]
//! which delegates to `Instant::now()`; tests use [`TestClock`] which holds
//! a `parking_lot::Mutex<Instant>` so tests can `advance(...)` without
//! sleeping. The cache itself has no internal threads — no spawn rationale
//! needed.
//!
//! ## Memory bound
//!
//! Cache is capped at [`MAX_ENTRIES`] (1024) — once full, insertion evicts
//! the OLDEST entry (insertion-order LRU via `VecDeque`). Eviction emits
//! `tracing::debug!`. Hash collisions are mitigated by `DefaultHasher` +
//! the full key tuple (`channel_kind` + `instance_name` + `topic_id`
//! disambiguate when content hashes happen to match).

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Default TTL window when `fleet.yaml channel.dedup_ttl_secs` is unset.
const DEFAULT_TTL_SECS: u64 = 5;

/// Maximum number of in-flight entries. Beyond this, oldest evicted on
/// insert. 1024 covers ~3.4 minutes of activity at 5/sec sustained rate;
/// generous for the dedup window's actual purpose (catching same-second
/// duplicates).
pub const MAX_ENTRIES: usize = 1024;

/// Composite key used to compare two outbound sends. dev-2 cross-audit
/// Pushback 5: `topic_id` MUST be part of the key so legitimate repeats
/// to distinct topics don't suppress each other. Reviewer concur.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub channel_kind: &'static str,
    pub instance_name: String,
    pub topic_id: Option<i64>,
    pub content_hash: u64,
}

impl DedupKey {
    pub fn new(
        channel_kind: &'static str,
        instance_name: &str,
        topic_id: Option<i64>,
        content: &str,
    ) -> Self {
        Self {
            channel_kind,
            instance_name: instance_name.to_string(),
            topic_id,
            content_hash: hash_content(content),
        }
    }
}

fn hash_content(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Injectable clock for §3.20 SOP 1 deterministic tests. Production
/// uses [`SystemClock`] which calls `Instant::now()`. Tests use
/// [`TestClock`] which advances explicitly without sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock — delegates to `Instant::now()`. Zero overhead;
/// stateless.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock — holds a controllable `Instant`. `advance(d)` moves it
/// forward without sleeping. Used only in tests; production never
/// constructs this.
#[cfg(test)]
#[derive(Debug)]
pub struct TestClock {
    now: Mutex<Instant>,
}

#[cfg(test)]
impl TestClock {
    pub fn new() -> Self {
        Self {
            now: Mutex::new(Instant::now()),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut guard = self.now.lock();
        *guard += by;
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn now(&self) -> Instant {
        *self.now.lock()
    }
}

/// One cached (key, observed_at) entry.
#[derive(Debug)]
struct Entry {
    key: DedupKey,
    seen_at: Instant,
}

/// The dedup cache. Bounded VecDeque with insertion-order LRU eviction.
/// Lookups are O(N) because we don't maintain a hash index alongside; at
/// MAX_ENTRIES=1024 this is fine for the call rates we see (low
/// hundreds/sec peak). If hot-path lookup ever becomes a measured
/// bottleneck, swap to `IndexMap` (same insertion-order property,
/// O(1) lookup).
pub struct DedupCache {
    entries: Mutex<VecDeque<Entry>>,
    /// Operator-visible suppression counter. Surfaced via doctor /
    /// debug endpoint (dev-2 Pushback 1b).
    suppressed: AtomicU64,
    ttl: Duration,
    clock: Box<dyn Clock>,
}

impl DedupCache {
    pub fn new(ttl_secs: u64, clock: Box<dyn Clock>) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(MAX_ENTRIES)),
            suppressed: AtomicU64::new(0),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            clock,
        }
    }

    /// Records `key` and returns `true` if it's a fresh send (caller
    /// should proceed) or `false` if a matching key was seen within the
    /// TTL window (caller should skip). Tracing + counter side effects
    /// fire on the suppression branch.
    ///
    /// Stale entries (older than TTL) are lazily swept on each call —
    /// keeps memory bounded without a background thread.
    pub fn record_and_check(&self, key: DedupKey) -> bool {
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        // Lazy sweep: drop everything past TTL from the front (oldest).
        // VecDeque preserves insertion order; older entries are always
        // at the front (we push_back).
        while let Some(front) = entries.front() {
            if now.duration_since(front.seen_at) > self.ttl {
                entries.pop_front();
            } else {
                break;
            }
        }
        // Lookup: O(N) scan from front. Bounded by MAX_ENTRIES.
        if let Some(existing) = entries.iter().find(|e| e.key == key) {
            // dev-2 Pushback 1b: emit per-suppression tracing with all
            // key fields + age-of-original so operators can grep
            // suppression events back to the originator.
            let age_ms = now.duration_since(existing.seen_at).as_millis();
            tracing::info!(
                channel = key.channel_kind,
                instance = %key.instance_name,
                topic = ?key.topic_id,
                content_hash = key.content_hash,
                age_ms = age_ms as u64,
                "#969 channel send deduped (suppressed)"
            );
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // Fresh send: enforce cap before insert (insertion-order LRU).
        if entries.len() >= MAX_ENTRIES {
            if let Some(evicted) = entries.pop_front() {
                tracing::debug!(
                    channel = evicted.key.channel_kind,
                    instance = %evicted.key.instance_name,
                    "#969 dedup: evicting oldest entry (cap reached)"
                );
            }
        }
        entries.push_back(Entry { key, seen_at: now });
        true
    }

    /// HIGH-2: roll back a [`record_and_check`] claim — remove `key` if present
    /// (no-op otherwise). Used when the send that the claim guarded ultimately
    /// FAILED: leaving the key recorded would suppress a legitimate retry of the
    /// same content within the TTL (returning a synthesized success that gets
    /// mis-recorded as delivered). Recording stays BEFORE the send so the RC2
    /// race against a concurrent duplicate is still caught atomically; only a
    /// terminal failure evicts. A SUCCESSFUL send keeps its key.
    pub fn evict(&self, key: &DedupKey) {
        let mut entries = self.entries.lock();
        if let Some(pos) = entries.iter().position(|e| &e.key == key) {
            entries.remove(pos);
        }
    }

    /// Total suppressions observed for the lifetime of this cache.
    /// Surfaced via doctor / debug — operator-visible health signal.
    pub fn suppressed_count(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    /// Current cached-entry count (testing + observability).
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

// ─── per-home singleton registry ───────────────────────────────────────

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Per-home cache registry. The dedup cache must be per-`AGEND_HOME`
/// rather than per-process so tests using distinct `tmp_home` paths
/// stay isolated. Production typically has one home; this map sees a
/// single entry. Tests using distinct homes get distinct caches.
static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, &'static DedupCache>>> = OnceLock::new();

/// Returns the dedup cache for the given home, initializing on first
/// access. Cache is leaked (`Box::leak`) so the returned reference can
/// have `'static` lifetime — acceptable because the home set is bounded
/// (one in production, a handful in tests; never grows unboundedly).
pub fn global(home: &std::path::Path) -> &'static DedupCache {
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let home_buf = home.to_path_buf();
    let mut guard = registry.lock();
    if let Some(existing) = guard.get(&home_buf) {
        return existing;
    }
    let ttl = read_ttl_from_fleet_yaml(home).unwrap_or(DEFAULT_TTL_SECS);
    let cache = Box::leak(Box::new(DedupCache::new(ttl, Box::new(SystemClock))));
    guard.insert(home_buf, cache);
    cache
}

/// Best-effort fleet.yaml read for `channel.dedup_ttl_secs`. Failure
/// modes (missing fleet.yaml, missing field, parse error, wrong type)
/// all fall through to `None` so caller uses `DEFAULT_TTL_SECS`.
fn read_ttl_from_fleet_yaml(home: &std::path::Path) -> Option<u64> {
    let path = crate::fleet::fleet_yaml_path(home);
    let content = std::fs::read_to_string(&path).ok()?;
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content).ok()?;
    doc.get("channel")?.get("dedup_ttl_secs")?.as_u64()
}

// ─── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(name: &str, topic: Option<i64>, content: &str) -> DedupKey {
        DedupKey::new("telegram", name, topic, content)
    }

    /// T1: dedup-module unit — first call returns true (proceed); second
    /// call with same key returns false (suppress); after TTL advances,
    /// repeat call returns true again. Deterministic via TestClock.
    #[test]
    fn t1_record_and_check_ttl_window() {
        let clock = std::sync::Arc::new(TestClock::new());
        // The cache owns its Clock as Box<dyn Clock>; for the test we
        // need to advance the clock externally, so wrap it.
        struct ArcClock(std::sync::Arc<TestClock>);
        impl Clock for ArcClock {
            fn now(&self) -> Instant {
                self.0.now()
            }
        }
        let cache = DedupCache::new(5, Box::new(ArcClock(std::sync::Arc::clone(&clock))));

        let k = key("agent-A", Some(42), "hello");
        assert!(cache.record_and_check(k.clone()), "first call: fresh");
        assert!(!cache.record_and_check(k.clone()), "second call: duplicate");
        assert_eq!(cache.suppressed_count(), 1);

        // Advance past TTL — same key now fresh.
        clock.advance(Duration::from_secs(6));
        assert!(cache.record_and_check(k.clone()), "post-TTL: fresh again");
        assert_eq!(cache.suppressed_count(), 1, "counter still 1");
    }

    /// HIGH-2: `evict` rolls back a claim so a retry-after-failure isn't
    /// suppressed, while a NON-evicted claim (the success path) stays deduped —
    /// preserving the RC2 guarantee. Calls are immediate (< the 5s TTL).
    #[test]
    fn evict_rolls_back_claim_but_kept_claim_stays_deduped_high2() {
        let cache = DedupCache::new(5, Box::new(SystemClock));
        let k = key("agent-A", Some(42), "hello");

        // RC2 preserved: a kept claim (the success path) still suppresses a dup.
        assert!(cache.record_and_check(k.clone()), "first: fresh");
        assert!(
            !cache.record_and_check(k.clone()),
            "duplicate of a kept claim must be suppressed (RC2)"
        );

        // The failed-send rollback un-blocks a retry of the same key.
        cache.evict(&k);
        assert!(
            cache.record_and_check(k.clone()),
            "after evict, a retry must NOT be suppressed"
        );

        // Evicting an absent key is a harmless no-op.
        let other = key("agent-A", Some(42), "different");
        cache.evict(&other);
        assert!(
            cache.record_and_check(other),
            "absent-key evict must not corrupt the cache"
        );
    }

    /// T2: LRU eviction — insert MAX_ENTRIES + 1 distinct keys; assert
    /// cache caps at MAX_ENTRIES and oldest evicted in FIFO order.
    #[test]
    fn t2_lru_eviction_at_capacity() {
        let cache = DedupCache::new(60, Box::new(TestClock::new()));
        for i in 0..MAX_ENTRIES {
            assert!(cache.record_and_check(key("a", None, &format!("content-{i}"))));
        }
        assert_eq!(cache.len(), MAX_ENTRIES);
        // Push one more new key: causes eviction of the OLDEST
        // (content-0, which sat at the front).
        assert!(cache.record_and_check(key("a", None, "content-new")));
        assert_eq!(cache.len(), MAX_ENTRIES, "still capped");
        // content-0 must have been evicted (fresh on re-record).
        assert!(
            cache.record_and_check(key("a", None, "content-0")),
            "oldest content-0 evicted to make room for content-new"
        );
        // Re-recording content-0 above triggered ANOTHER eviction (now
        // content-1 evicted). Cache stays at cap.
        assert_eq!(cache.len(), MAX_ENTRIES, "still capped after second push");
        // content-2 (next-oldest after content-1's eviction) is still
        // present — re-record returns false.
        assert!(
            !cache.record_and_check(key("a", None, "content-2")),
            "content-2 should still be present (only content-0 and content-1 evicted)"
        );
    }

    /// T3 caller-path RC1-like: simulate two senders trying to publish
    /// the same CI notification — second is suppressed.
    #[test]
    fn t3_rc1_dual_sender_suppressed() {
        let cache = DedupCache::new(5, Box::new(TestClock::new()));
        let ci_msg = "[ci-pass] owner/repo@feat/x (sha-A): passed ✓";
        let k1 = key("ci-watch:owner/repo:feat/x", None, ci_msg);
        let k2 = key("ci-watch:owner/repo:feat/x", None, ci_msg);
        assert!(cache.record_and_check(k1), "first sender: send");
        assert!(!cache.record_and_check(k2), "second sender: deduped");
        assert_eq!(cache.suppressed_count(), 1);
    }

    /// T4 caller-path RC2-like: PTY mirror + reply tool race — both
    /// would send the same reply text via the same (channel, instance,
    /// topic). Second suppressed.
    #[test]
    fn t4_rc2_pty_mirror_and_reply_tool_collision() {
        let cache = DedupCache::new(5, Box::new(TestClock::new()));
        // PTY mirror fires first.
        assert!(cache.record_and_check(key("dev", Some(100), "VERIFIED")));
        // Reply tool also tries to send the same text to the same topic.
        assert!(!cache.record_and_check(key("dev", Some(100), "VERIFIED")));
        assert_eq!(cache.suppressed_count(), 1);
    }

    /// T7 scope safety: same content but DIFFERENT topic must NOT be
    /// suppressed (dev-2 Pushback 5 + reviewer concur). Operator
    /// scenario: legitimate status update repeating same body across
    /// distinct topics for distinct agents.
    #[test]
    fn t7_distinct_topic_not_suppressed() {
        let cache = DedupCache::new(5, Box::new(TestClock::new()));
        let body = "[ci-pass] owner/repo@feat/x (sha-A): passed ✓";
        assert!(cache.record_and_check(key("dev", Some(100), body)));
        // Same body, DIFFERENT topic — must pass through.
        assert!(cache.record_and_check(key("dev", Some(200), body)));
        // Same body + same topic + DIFFERENT instance — also passes
        // (dedup is per-instance).
        assert!(cache.record_and_check(key("reviewer", Some(100), body)));
        assert_eq!(cache.suppressed_count(), 0);
    }

    /// Auxiliary T8: distinct channel_kind values keep separate
    /// suppression slots (e.g., a future "discord" channel doesn't
    /// dedup against telegram).
    #[test]
    fn t8_distinct_channel_kind_not_suppressed() {
        let cache = DedupCache::new(5, Box::new(TestClock::new()));
        let dk1 = DedupKey::new("telegram", "dev", None, "hi");
        let dk2 = DedupKey::new("discord", "dev", None, "hi");
        assert!(cache.record_and_check(dk1));
        assert!(cache.record_and_check(dk2));
    }

    /// T9: empty cache initial state.
    #[test]
    fn t9_initial_state() {
        let cache = DedupCache::new(5, Box::new(TestClock::new()));
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.suppressed_count(), 0);
    }

    /// T10: TTL read from fleet.yaml override.
    #[test]
    fn t10_fleet_yaml_ttl_override() {
        let dir = std::env::temp_dir().join(format!("agend-969-ttl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&dir),
            "channel:\n  dedup_ttl_secs: 30\ninstances: {}\n",
        )
        .unwrap();
        let ttl = read_ttl_from_fleet_yaml(&dir);
        assert_eq!(ttl, Some(30));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T11: TTL absent → None (caller uses DEFAULT_TTL_SECS).
    #[test]
    fn t11_fleet_yaml_ttl_missing_field() {
        let dir = std::env::temp_dir().join(format!("agend-969-ttlmiss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&dir),
            "channel:\n  type: telegram\ninstances: {}\n",
        )
        .unwrap();
        let ttl = read_ttl_from_fleet_yaml(&dir);
        assert_eq!(ttl, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
