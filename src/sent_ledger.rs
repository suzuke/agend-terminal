//! Telegram reply-to correlation: the sent-message ledger.
//!
//! Records, per agent, the `message_id` of each message the bot SENDS to the
//! operator channel together with that message's task context
//! (`task_id` / `correlation_id`) and a short excerpt. When the operator later
//! "reply-to"s one of those messages, the inbound path looks the quoted
//! `message_id` up here and enriches the [`crate::inbox::InboxMessage`] with a
//! `reply_target` so the agent knows EXACTLY which prior message — and which
//! task — the operator is responding to (even when they jump back to a very old
//! message).
//!
//! ## Why persisted (unlike `notification_dedup`)
//! `notification_dedup` is in-memory only because its entries are short-lived
//! (10-min window). This ledger's whole point is "the operator may quote a
//! message hours or days later", and the daemon can restart (upgrade /
//! watchdog) in between — so the mapping MUST survive restarts. It is stored as
//! append-only JSONL (`<home>/sent_ledger.jsonl`), one line per sent message,
//! loaded into an in-memory `HashMap` on first access for O(1) lookup, and
//! compacted by a periodic GC (rewrite). This mirrors the existing inbox-JSONL /
//! event-log style — human-readable and auditable — without pulling in an
//! embedded DB.
//!
//! ## IRON RULE (same as `reply_ledger`)
//! Every op is infallible. A failed disk write is `warn!`-logged and swallowed;
//! it NEVER blocks or changes the return value of a reply/send. Losing a ledger
//! line only degrades a future reply-to to the pre-existing
//! `in_reply_to_excerpt` behaviour (graceful), never breaks delivery.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Retain sent-message records for 14 days. This bounds how far back the
/// operator can reply-to and still get precise correlation; older quotes fall
/// back to the `in_reply_to_excerpt`-only path. Pinned by the operator/general.
pub const TTL_SECS: i64 = 14 * 24 * 60 * 60;

/// Per-agent cap on retained records (FIFO — oldest dropped first). A second
/// defence against unbounded growth on a high-frequency fleet, alongside the
/// TTL and the per-entry excerpt truncation. Pinned by the operator/general.
pub const MAX_PER_AGENT: usize = 2000;

/// Minimum wall-clock interval between actual GC rewrites. The supervisor calls
/// [`SentLedger::maybe_gc`] on its 10s tick, but an O(N) JSONL rewrite every 10s
/// is wasteful; throttle it to hourly.
const GC_MIN_INTERVAL_SECS: u64 = 3600;

/// Excerpt cap — store only the leading slice of a sent message so the ledger
/// stays small and never holds full message bodies.
const EXCERPT_MAX_CHARS: usize = 200;

/// One recorded sent message. Serialised one-per-line into the JSONL ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SentEntry {
    /// The channel's message id for the sent message (Telegram returns `i32`;
    /// stored as `String` so it compares directly with
    /// `InboxMessage.in_reply_to_msg_id`).
    pub message_id: String,
    /// Which agent sent it.
    pub agent: String,
    /// Channel kind, e.g. `"telegram"`.
    pub channel: String,
    /// Chat/group id the message was sent into. Part of the lookup key:
    /// Telegram `message_id` is unique only WITHIN a chat (§5 of the design),
    /// so `(message_id, chat_id)` is the composite key.
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Telegram topic / forum-thread id (informational; not part of the key).
    #[serde(default)]
    pub topic_id: Option<i32>,
    /// Leading excerpt of the sent text (≤ [`EXCERPT_MAX_CHARS`] chars).
    pub excerpt: String,
    /// Task context, when the send carried one (reply tool may pass it; many
    /// interactive replies legitimately have none → `None`).
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// Send time (RFC3339, UTC).
    pub ts: String,
}

impl SentEntry {
    /// Build an entry, truncating the excerpt and stamping `ts = now` (UTC).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        message_id: impl Into<String>,
        agent: impl Into<String>,
        channel: impl Into<String>,
        chat_id: Option<String>,
        topic_id: Option<i32>,
        text: &str,
        task_id: Option<String>,
        correlation_id: Option<String>,
    ) -> Self {
        Self {
            message_id: message_id.into(),
            agent: agent.into(),
            channel: channel.into(),
            chat_id,
            topic_id,
            excerpt: truncate_excerpt(text),
            task_id,
            correlation_id,
            ts: chrono::Utc::now().to_rfc3339(),
        }
    }
}

fn truncate_excerpt(text: &str) -> String {
    text.chars().take(EXCERPT_MAX_CHARS).collect()
}

/// Composite lookup key. `chat_id` is `Option` because legacy/edge records may
/// lack it; see [`SentLedger::lookup`] for the precision contract.
type Key = (String, Option<String>);

fn key_of(message_id: &str, chat_id: Option<&str>) -> Key {
    (message_id.to_string(), chat_id.map(str::to_string))
}

/// In-memory map of sent messages, backed by the JSONL file. Whole-map mutex
/// (entries are small, ops are infrequent relative to the daemon tick).
#[derive(Default)]
pub struct SentLedger {
    state: Mutex<HashMap<Key, SentEntry>>,
    /// Last time [`maybe_gc`](Self::maybe_gc) actually ran a rewrite.
    last_gc: Mutex<Option<Instant>>,
}

impl SentLedger {
    /// Path of the JSONL ledger for a given home.
    fn path(home: &Path) -> PathBuf {
        home.join("sent_ledger.jsonl")
    }

    /// Record a sent message: append one JSONL line AND update the in-memory
    /// map. Infallible — a write error is logged and swallowed (IRON RULE), the
    /// in-memory entry is still inserted so the current process can resolve it.
    pub fn record(&self, home: &Path, entry: SentEntry) {
        self.append_line(home, &entry);
        if let Ok(mut s) = self.state.lock() {
            s.insert(key_of(&entry.message_id, entry.chat_id.as_deref()), entry);
        }
    }

    fn append_line(&self, home: &Path, entry: &SentEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "sent_ledger: serialize failed; entry not persisted");
                return;
            }
        };
        let path = Self::path(home);
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %path.display(),
                "sent_ledger: append failed; reply-to correlation for this message may be lost");
        }
    }

    /// Look up the context for a quoted message.
    ///
    /// Precision contract (§5 of the design): when `chat_id` is `Some`, an
    /// EXACT `(message_id, chat_id)` match is required — a record stored under a
    /// different chat is NOT returned (Telegram `message_id` repeats across
    /// chats). When `chat_id` is `None` (caller couldn't determine it), fall
    /// back to a `message_id`-only match.
    pub fn lookup(&self, message_id: &str, chat_id: Option<&str>) -> Option<SentEntry> {
        let s = self.state.lock().ok()?;
        match chat_id {
            Some(_) => s.get(&key_of(message_id, chat_id)).cloned(),
            None => s
                .iter()
                .find(|((mid, _), _)| mid == message_id)
                .map(|(_, e)| e.clone()),
        }
    }

    /// Load the JSONL ledger into memory. Called once per home on first
    /// [`global`] access. Unparseable lines are skipped (forward-compat).
    pub fn load(&self, home: &Path) {
        let path = Self::path(home);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // no ledger yet — fresh start
        };
        if let Ok(mut s) = self.state.lock() {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<SentEntry>(line) {
                    Ok(e) => {
                        s.insert(key_of(&e.message_id, e.chat_id.as_deref()), e);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "sent_ledger: skipping unparseable line");
                    }
                }
            }
        }
    }

    /// Throttled GC for the supervisor tick — runs an actual rewrite at most
    /// once per [`GC_MIN_INTERVAL_SECS`].
    pub fn maybe_gc(&self, home: &Path) {
        {
            let mut last = match self.last_gc.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            let now = Instant::now();
            if let Some(prev) = *last {
                if now.duration_since(prev).as_secs() < GC_MIN_INTERVAL_SECS {
                    return;
                }
            }
            *last = Some(now);
        }
        self.gc(home);
    }

    /// Drop expired (TTL) and over-cap (per-agent FIFO) entries, then rewrite
    /// the JSONL compacted. Infallible. Public for the supervisor's forced path
    /// and for tests; production cadence goes through [`maybe_gc`](Self::maybe_gc).
    pub fn gc(&self, home: &Path) {
        self.gc_at(home, chrono::Utc::now(), TTL_SECS, MAX_PER_AGENT);
    }

    /// Test/observability core of [`gc`](Self::gc) — clock and limits injected.
    /// Returns the number of entries dropped.
    pub fn gc_at(
        &self,
        home: &Path,
        now: chrono::DateTime<chrono::Utc>,
        ttl_secs: i64,
        max_per_agent: usize,
    ) -> usize {
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let before = s.len();

        // 1. TTL: drop entries older than the window (or with an unparseable ts).
        s.retain(|_, e| match chrono::DateTime::parse_from_rfc3339(&e.ts) {
            Ok(ts) => {
                now.signed_duration_since(ts.with_timezone(&chrono::Utc))
                    .num_seconds()
                    < ttl_secs
            }
            Err(_) => false, // corrupt timestamp → drop
        });

        // 2. Per-agent FIFO cap: for any agent over the cap, drop its oldest.
        let mut by_agent: HashMap<String, Vec<(i64, Key)>> = HashMap::new();
        for (k, e) in s.iter() {
            let ord = chrono::DateTime::parse_from_rfc3339(&e.ts)
                .map(|t| t.timestamp_millis())
                .unwrap_or(0);
            by_agent
                .entry(e.agent.clone())
                .or_default()
                .push((ord, k.clone()));
        }
        let mut to_drop: Vec<Key> = Vec::new();
        for (_agent, mut entries) in by_agent {
            if entries.len() > max_per_agent {
                entries.sort_by_key(|(ord, _)| *ord); // oldest first
                let overflow = entries.len() - max_per_agent;
                for (_, k) in entries.into_iter().take(overflow) {
                    to_drop.push(k);
                }
            }
        }
        for k in to_drop {
            s.remove(&k);
        }

        let dropped = before.saturating_sub(s.len());
        self.rewrite(home, &s);
        dropped
    }

    /// Rewrite the JSONL file from the in-memory map, compacted. Best-effort:
    /// writes to a temp file then renames, so a crash mid-write can't corrupt
    /// the live ledger. Errors are logged and swallowed.
    fn rewrite(&self, home: &Path, entries: &HashMap<Key, SentEntry>) {
        let path = Self::path(home);
        let tmp = path.with_extension("jsonl.tmp");
        let mut buf = String::new();
        for e in entries.values() {
            match serde_json::to_string(e) {
                Ok(l) => {
                    buf.push_str(&l);
                    buf.push('\n');
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sent_ledger: gc serialize failed; line skipped")
                }
            }
        }
        if let Err(e) = std::fs::write(&tmp, &buf).and_then(|_| std::fs::rename(&tmp, &path)) {
            tracing::warn!(error = %e, path = %path.display(),
                "sent_ledger: gc rewrite failed; ledger left as-is");
        }
    }

    /// Test-only: entry count.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.state.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Test-only: true when empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-home singleton registry. Like `channel::dedup`, the ledger is keyed by
/// `AGEND_HOME` so tests with distinct temp homes stay isolated; production has
/// a single home. The ledger is loaded from disk on first access for that home
/// and leaked for a `'static` reference (home set is bounded).
pub fn global(home: &Path) -> &'static SentLedger {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, &'static SentLedger>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let home_buf = home.to_path_buf();
    let mut guard = match registry.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(existing) = guard.get(&home_buf) {
        return existing;
    }
    let ledger: &'static SentLedger = Box::leak(Box::new(SentLedger::default()));
    ledger.load(home);
    guard.insert(home_buf, ledger);
    ledger
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let id = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "agend-sent-ledger-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&d).ok();
        d
    }

    fn entry(message_id: &str, agent: &str, chat: Option<&str>) -> SentEntry {
        SentEntry::new(
            message_id,
            agent,
            "telegram",
            chat.map(str::to_string),
            Some(7),
            "hello operator",
            None,
            None,
        )
    }

    fn entry_ts(message_id: &str, agent: &str, chat: Option<&str>, ts: &str) -> SentEntry {
        let mut e = entry(message_id, agent, chat);
        e.ts = ts.to_string();
        e
    }

    #[test]
    fn record_then_lookup_hits_and_misses() {
        let home = tmp_home("hitmiss");
        let l = SentLedger::default();
        l.record(&home, entry("100", "general", Some("-100")));
        // Hit on the recorded (message_id, chat_id).
        let got = l.lookup("100", Some("-100")).expect("must hit");
        assert_eq!(got.agent, "general");
        assert_eq!(got.excerpt, "hello operator");
        // Miss on a different message_id.
        assert!(l.lookup("999", Some("-100")).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn lookup_chat_scope_does_not_cross_chats() {
        // Same message_id, different chats — must not mis-hit (§5 precision).
        let home = tmp_home("chatscope");
        let l = SentLedger::default();
        l.record(&home, entry("42", "agentA", Some("-1001")));
        l.record(&home, entry("42", "agentB", Some("-1002")));
        assert_eq!(l.lookup("42", Some("-1001")).unwrap().agent, "agentA");
        assert_eq!(l.lookup("42", Some("-1002")).unwrap().agent, "agentB");
        // A chat with no such record misses, even though message_id 42 exists.
        assert!(l.lookup("42", Some("-9999")).is_none());
        // chat-less lookup falls back to message_id-only (returns SOME record).
        assert!(l.lookup("42", None).is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_drops_expired_by_ttl() {
        let home = tmp_home("ttl");
        let l = SentLedger::default();
        let now = chrono::Utc::now();
        let fresh = (now - chrono::Duration::seconds(10)).to_rfc3339();
        let old = (now - chrono::Duration::days(30)).to_rfc3339();
        l.record(&home, entry_ts("fresh", "g", Some("-1"), &fresh));
        l.record(&home, entry_ts("old", "g", Some("-1"), &old));
        let dropped = l.gc_at(&home, now, TTL_SECS, MAX_PER_AGENT);
        assert_eq!(dropped, 1, "the 30-day-old entry must be dropped");
        assert!(l.lookup("fresh", Some("-1")).is_some());
        assert!(l.lookup("old", Some("-1")).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_enforces_per_agent_fifo_cap() {
        let home = tmp_home("fifo");
        let l = SentLedger::default();
        let now = chrono::Utc::now();
        // 3 entries for one agent, distinct ascending timestamps; cap = 2.
        for (i, secs) in [(0, 300), (1, 200), (2, 100)] {
            let ts = (now - chrono::Duration::seconds(secs)).to_rfc3339();
            l.record(&home, entry_ts(&format!("m{i}"), "busy", Some("-1"), &ts));
        }
        let dropped = l.gc_at(&home, now, TTL_SECS, 2);
        assert_eq!(dropped, 1, "one over-cap entry dropped (FIFO)");
        // m0 is the OLDEST (300s ago) → it is the one evicted.
        assert!(l.lookup("m0", Some("-1")).is_none(), "oldest evicted first");
        assert!(l.lookup("m1", Some("-1")).is_some());
        assert!(l.lookup("m2", Some("-1")).is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fifo_cap_is_per_agent_not_global() {
        let home = tmp_home("fifo-peragent");
        let l = SentLedger::default();
        let now = chrono::Utc::now();
        // Two agents, 2 entries each; cap = 2 per agent → nothing dropped.
        for agent in ["a", "b"] {
            for i in 0..2 {
                let ts = (now - chrono::Duration::seconds(100 - i)).to_rfc3339();
                l.record(
                    &home,
                    entry_ts(&format!("{agent}{i}"), agent, Some("-1"), &ts),
                );
            }
        }
        let dropped = l.gc_at(&home, now, TTL_SECS, 2);
        assert_eq!(dropped, 0, "cap is per-agent; neither agent exceeds it");
        assert_eq!(l.len(), 4);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn persistence_round_trips_across_reload() {
        let home = tmp_home("reload");
        {
            let l = SentLedger::default();
            l.record(&home, entry("777", "general", Some("-500")));
        }
        // Simulate a daemon restart: a brand-new ledger loads the same home.
        let l2 = SentLedger::default();
        l2.load(&home);
        let got = l2.lookup("777", Some("-500")).expect("must survive reload");
        assert_eq!(got.agent, "general");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_rewrite_compacts_file_on_disk() {
        let home = tmp_home("compact");
        let l = SentLedger::default();
        let now = chrono::Utc::now();
        let old = (now - chrono::Duration::days(30)).to_rfc3339();
        l.record(&home, entry("keep", "g", Some("-1")));
        l.record(&home, entry_ts("drop", "g", Some("-1"), &old));
        l.gc_at(&home, now, TTL_SECS, MAX_PER_AGENT);
        // Reload from disk: only the kept entry should remain persisted.
        let l2 = SentLedger::default();
        l2.load(&home);
        assert!(l2.lookup("keep", Some("-1")).is_some());
        assert!(l2.lookup("drop", Some("-1")).is_none());
        assert_eq!(l2.len(), 1, "gc must have compacted the on-disk JSONL");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn record_is_infallible_on_unwritable_home() {
        // A non-existent, non-creatable nested path: append fails, but record
        // must not panic and the in-memory entry must still be queryable.
        let bogus = PathBuf::from("/no/such/agend/home/\0invalid");
        let l = SentLedger::default();
        l.record(&bogus, entry("x", "g", Some("-1")));
        assert!(
            l.lookup("x", Some("-1")).is_some(),
            "in-memory record survives even when the disk append fails"
        );
    }

    #[test]
    fn load_ignores_unparseable_lines() {
        let home = tmp_home("badlines");
        let path = SentLedger::path(&home);
        let good = serde_json::to_string(&entry("ok", "g", Some("-1"))).unwrap();
        std::fs::write(&path, format!("not json\n{good}\n\n{{bad\n")).unwrap();
        let l = SentLedger::default();
        l.load(&home);
        assert_eq!(l.len(), 1, "only the one valid line loads");
        assert!(l.lookup("ok", Some("-1")).is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn entry_truncates_long_excerpt() {
        let long = "x".repeat(500);
        let e = SentEntry::new("1", "g", "telegram", None, None, &long, None, None);
        assert_eq!(e.excerpt.chars().count(), EXCERPT_MAX_CHARS);
    }
}
