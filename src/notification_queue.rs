use crate::agent_ops;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;

/// #1457: the old fixed compose-idle window. The notification-delivery guards
/// no longer use it (replaced by the input-vs-submit `DraftState`); it now only
/// backs `Pane::is_composing`, a test-only helper — hence `#[cfg(test)]`.
#[cfg(test)]
pub const COMPOSE_IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPOSE_METADATA_KEY: &str = "last_input_epoch_ms";
/// Sprint 54 P2-3: epoch-ms timestamp of the most recent submit-key
/// keystroke (e.g. `\r` for claude). Distinct from
/// `COMPOSE_METADATA_KEY` which records ANY input keystroke. Used by
/// the daemon supervisor to detect "typed but not submitted" state.
const SUBMIT_METADATA_KEY: &str = "last_submit_epoch_ms";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedNotification {
    pub text: String,
    pub timestamp: String,
    /// #1513: actionable work-delivery wake (ci-ready / task / query) vs ambient.
    /// Actionable items drain FIRST and carry a tighter MAX_DEFER cap. `serde
    /// default` keeps pre-#1513 queue lines (no field) deserializing as ambient.
    #[serde(default)]
    pub actionable: bool,
    /// #1513: epoch-ms when this item was FIRST deferred. Drives the MAX_DEFER
    /// anti-starvation cap (release after the cap even if the agent stays busy).
    /// Preserved across requeue so the cap counts from the original defer.
    #[serde(default)]
    pub deferred_since_ms: i64,
}

fn queue_path(home: &Path, agent_name: &str) -> PathBuf {
    home.join("notification-queue")
        .join(format!("{agent_name}.jsonl"))
}

/// Legacy fixed-name draining file written by pre-claim-atomic binaries.
/// Production code no longer writes it (claims use unique per-process names),
/// but `list_draining_files` still matches it so stale-claim recovery covers
/// an upgrade-over-crash. Tests use it to simulate a peer's claim.
#[cfg(test)]
fn draining_path(home: &Path, agent_name: &str) -> PathBuf {
    queue_path(home, agent_name).with_extension("draining")
}

pub fn record_input_activity(home: &Path, agent_name: &str) {
    agent_ops::save_metadata(
        home,
        agent_name,
        COMPOSE_METADATA_KEY,
        json!(chrono::Utc::now().timestamp_millis()),
    );
}

/// Sprint 54 P2-3: record a submit-key keystroke (e.g. claude `\r`).
/// Caller (`app::write_to_focused`) is responsible for the backend
/// allowlist + submit-key match — this helper only persists the
/// timestamp. The daemon supervisor tick reads it via
/// `last_submit_at_ms` and compares against `last_input_at_ms` for
/// the typed-but-not-submitted detection.
pub fn record_submit_activity(home: &Path, agent_name: &str) {
    agent_ops::save_metadata(
        home,
        agent_name,
        SUBMIT_METADATA_KEY,
        json!(chrono::Utc::now().timestamp_millis()),
    );
}

/// Sprint 54 P2-3: read the last input/submit timestamps. Returns
/// `(typed_ms, submit_ms)` tuple; either component is `0` when missing
/// (legacy data, agent never typed, or non-submit-detected backend).
/// Used by the daemon supervisor tick for typed-but-not-submitted
/// detection — keeps the read inline-cheap (single file read, single
/// JSON parse) so per-tick overhead stays bounded.
pub fn read_input_submit_timestamps(home: &Path, agent_name: &str) -> (i64, i64) {
    // #1680: resolve via the SAME path resolver the write side uses
    // (`save_metadata` → `metadata_path_resolved`). The previous hand-coded
    // `metadata/<name>.json` read the never-written name file while the write
    // landed on `metadata/<uuid>.json`, so `draft_state` was permanently stale
    // (`None`) and the inject path force-submitted the operator's unsent draft.
    let meta_path = agent_ops::metadata_path_resolved(home, agent_name);
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return (0, 0);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (0, 0);
    };
    let typed_ms = value[COMPOSE_METADATA_KEY].as_i64().unwrap_or(0);
    let submit_ms = value[SUBMIT_METADATA_KEY].as_i64().unwrap_or(0);
    (typed_ms, submit_ms)
}

/// #1457: how long an unsent draft defers notification delivery before the
/// escape valve releases it (operator likely walked away mid-draft).
/// Fixed const 300s / 5 min (#env-cleanup: was env-overridable via
/// `AGEND_DRAFT_ESCAPE_SECS`; demoted to YAGNI for single-user deploys).
fn draft_escape_timeout_ms() -> i64 {
    const DRAFT_ESCAPE_MS: i64 = 300_000;
    DRAFT_ESCAPE_MS
}

/// #1457: draft state used to gate notification delivery. Derived from the
/// relative ORDER of the last input vs last submit keystroke (not a fixed idle
/// window) — fixes the `is_composing` false-negative where a >3s pause mid-draft
/// was misread as "no draft" and a notification clobbered the operator's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftState {
    /// No unsent draft: everything typed has been submitted (or never typed).
    None,
    /// Unsent draft present and operator likely still composing → defer all.
    Drafting,
    /// Unsent draft present but idle past the escape window → trickle-release.
    Abandoned,
}

/// #1457: classify the focused pane's draft state for delivery gating.
/// `typed > submit` means keystrokes were entered but not submitted (a live
/// draft); `typed <= submit` (or never typed) means the buffer is clean.
pub fn draft_state(home: &Path, agent_name: &str) -> DraftState {
    let (typed_ms, submit_ms) = read_input_submit_timestamps(home, agent_name);
    if typed_ms == 0 || typed_ms <= submit_ms {
        return DraftState::None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    if now_ms.saturating_sub(typed_ms) < draft_escape_timeout_ms() {
        DraftState::Drafting
    } else if submit_ms == 0 {
        // #1473 (scoped fix): an unsent draft idle past the escape window with
        // NO submit ever recorded is not evidence of a real operator draft —
        // e.g. the operator poked an agent pane once but never composed a
        // message there. Treat as None so notifications deliver normally,
        // rather than trapping the pane in Abandoned forever. The Drafting
        // branch above (recent typing = active draft) is untouched, so #1457's
        // "don't clobber an in-progress draft" protection is preserved.
        // NOTE: scoped to the Abandoned branch ON PURPOSE — a naive top-level
        // `submit_ms == 0 → None` would mis-classify the operator's first-ever
        // draft (recent typing, no prior submit) and re-introduce the #1457 bug.
        DraftState::None
    } else {
        DraftState::Abandoned
    }
}

/// #1944: is the backend's input box (located by its prompt `marker` in the
/// rendered screen `tail`) actually EMPTY? This refines `draft_state`'s
/// timestamp-only heuristic, which reads a type-then-clear (typed then deleted
/// to empty, or typed-but-not-submitted) as a live `Drafting` for up to 5 min
/// even though the input line is visibly empty.
///
/// Returns:
/// - `Some(true)` — the input prompt is present and nothing non-whitespace
///   follows the marker → the box is empty (a stale draft; deliver normally).
/// - `Some(false)` — content follows the marker → a real live draft (protect).
/// - `None` — no prompt line found (agent mid-output, or a backend with no
///   marker) → the caller cannot tell, so it FAILS TOWARD PROTECTION (keep
///   deferring), never risking a clobber of a real draft.
///
/// Robustness: the input prompt is the BOTTOM-MOST line whose first non-blank
/// char is the marker (the input box sits at the screen bottom, below any
/// conversation/prose), so a `>`/`❯` appearing mid-prose above it is not matched
/// (the #1944 prose-false-positive guard).
pub fn input_box_is_empty(tail: &str, marker: &str) -> Option<bool> {
    let prompt_line = tail
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with(marker))?;
    let after_marker = prompt_line
        .trim_start()
        .strip_prefix(marker)
        .unwrap_or_default();
    Some(after_marker.trim().is_empty())
}

/// #1948 v2: unified per-backend empty-input-box probe. Tries the prompt-line
/// `marker` first (claude/codex/agy — content after the marker == non-empty);
/// if that can't decide (no prompt line found / no marker) AND a `placeholder`
/// is supplied (kiro), treats the placeholder being VISIBLE in the tail as the
/// box being empty (the TUI shows it only while empty). Returns:
/// - `Some(true)` — box verifiably empty (deliver),
/// - `Some(false)` — box has content (protect),
/// - `None` — undeterminable → caller FAILS TOWARD PROTECTION (keep deferring).
///
/// A backend has at most one of (marker, placeholder); the order is just a safe
/// precedence. Placeholder ABSENCE returns `None` (not `Some(false)`): it could
/// mean "typed" OR "agent mid-output / placeholder string changed across
/// versions" — either way fail toward protection (a placeholder-text change
/// disables the kiro path without ever risking a clobber of a real draft).
pub fn input_box_empty_probe(
    tail: &str,
    marker: Option<&str>,
    placeholder: Option<&str>,
) -> Option<bool> {
    if let Some(m) = marker {
        if let Some(empty) = input_box_is_empty(tail, m) {
            return Some(empty);
        }
    }
    if let Some(p) = placeholder {
        if tail.contains(p) {
            return Some(true);
        }
    }
    None
}

/// #1948(b): empty-box check for a backend (codex) whose EMPTY box renders a
/// rotating ghost/placeholder phrase after `marker` in the DIM attribute — which
/// a plain marker probe mis-reads as typed content (the v1 codex blind spot).
/// `text`/`dim` come from [`crate::vterm::VTerm::tail_lines_with_dim`] and are
/// 1:1 char-aligned. The marker's own glyph is excluded (codex renders `›` BOLD,
/// the ghost DIM; the operator's real input is normal intensity).
///
/// Returns `Some(true)` if the BOTTOM-MOST marker line has, after the marker,
/// only whitespace OR only DIM text (ghost → box empty); `Some(false)` if ANY
/// non-whitespace non-DIM glyph follows (a real live draft → protect); `None` if
/// no marker line is present (agent mid-output → fail toward protection).
pub fn input_box_dim_aware_empty(text: &str, dim: &[bool], marker: &str) -> Option<bool> {
    let chars: Vec<char> = text.chars().collect();
    // Find the BOTTOM-MOST line whose first non-blank char is the marker, and the
    // char range of its content AFTER the marker. Char indices align 1:1 with
    // `dim` (each `\n` contributes one char + one `dim` entry).
    let mut line_start = 0usize;
    let mut after_marker: Option<(usize, usize)> = None;
    for line in text.split('\n') {
        let line_len = line.chars().count();
        let line_end = line_start + line_len;
        if line.trim_start().starts_with(marker) {
            let leading_ws = line.chars().take_while(|c| c.is_whitespace()).count();
            let start = line_start + leading_ws + marker.chars().count();
            after_marker = Some((start, line_end));
        }
        line_start = line_end + 1; // +1 for the '\n' separator char
    }
    let (start, end) = after_marker?;
    for i in start..end {
        if chars.get(i).copied().unwrap_or(' ').is_whitespace() {
            continue;
        }
        if !dim.get(i).copied().unwrap_or(false) {
            return Some(false); // a normal-intensity glyph after the marker = real input
        }
    }
    Some(true) // only whitespace / only DIM ghost after the marker
}

/// #1457: pop and return the single OLDEST queued notification, leaving the
/// rest queued. The escape valve uses this so an abandoned-draft pane trickles
/// its backlog one-per-tick instead of clobbering the draft with a full batch.
/// Routes through the claim-atomic `drain` (then requeues the tail) so a
/// concurrent flusher can never read the same lines mid-rewrite.
pub fn drain_one(home: &Path, agent_name: &str) -> Option<QueuedNotification> {
    // #2028: drain_one is a SINGLE-SHOT caller (the Abandoned-trickle path and
    // tests) — unlike the flushers it has no "next tick" to absorb a transient
    // false-empty, so a lock/claim hiccup must be retried, not reported as
    // "queue empty". Bounded (no double-drain dead-wait regression): a live
    // peer holds the drain lock for microseconds (rename + read), so a few
    // short retries comfortably outlast any healthy contention window.
    const RETRIES: u32 = 5;
    const RETRY_SLEEP: std::time::Duration = std::time::Duration::from_millis(10);
    for attempt in 0..=RETRIES {
        match try_drain_with_stale_threshold(home, agent_name, STALE_DRAINING_MS) {
            DrainAttempt::Drained(mut all) => {
                if all.is_empty() {
                    return None; // TRUE empty — claim succeeded, nothing queued.
                }
                let oldest = all.remove(0);
                if !all.is_empty() {
                    requeue_all(home, agent_name, &all);
                }
                return Some(oldest);
            }
            DrainAttempt::Unavailable => {
                if attempt < RETRIES {
                    std::thread::sleep(RETRY_SLEEP);
                }
            }
        }
    }
    tracing::warn!(
        agent = agent_name,
        "#2028: drain_one gave up after {RETRIES} contended attempts — \
         deferring to the next trickle cycle (not claiming empty)"
    );
    None
}

pub fn enqueue(home: &Path, agent_name: &str, text: &str) -> anyhow::Result<()> {
    enqueue_classified(home, agent_name, text, false)
}

/// #1513: enqueue a notification tagged actionable/ambient. `deferred_since_ms`
/// is stamped now (first defer). Actionable items drain first and carry a
/// tighter MAX_DEFER cap; ambient retains the legacy contract.
pub fn enqueue_classified(
    home: &Path,
    agent_name: &str,
    text: &str,
    actionable: bool,
) -> anyhow::Result<()> {
    let msg = QueuedNotification {
        text: text.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        actionable,
        deferred_since_ms: chrono::Utc::now().timestamp_millis(),
    };
    append_queued(home, agent_name, &msg)
}

/// #1513: append a fully-formed `QueuedNotification` verbatim, preserving its
/// `actionable` + `deferred_since_ms` (used by `requeue_all` so the MAX_DEFER
/// cap counts from the ORIGINAL defer, not the requeue).
fn append_queued(home: &Path, agent_name: &str, msg: &QueuedNotification) -> anyhow::Result<()> {
    let path = queue_path(home, agent_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(msg)?)?;
    Ok(())
}

/// #t-3558 P2: the `[AGEND-AUTO kind=X]` coalesce key for `text`, or `None` when
/// `text` is not an auto-inject nudge. Two nudges coalesce iff they share this
/// EXACT prefix — so a different `kind=` (e.g. `progress-backstop` vs
/// `ratelimit-retry`) and any ordinary notification never match.
fn agend_auto_kind_prefix(text: &str) -> Option<String> {
    if !text.starts_with(crate::agent::DAEMON_AUTO_INJECT_MARKER) {
        return None;
    }
    let close = text.find(']')?;
    Some(text[..=close].to_string())
}

/// #t-3558 P2: enqueue an `[AGEND-AUTO]` auto-inject nudge, COALESCING it with any
/// already-queued nudge of the SAME `[AGEND-AUTO kind=X]` kind (keep-latest). A
/// non-draining agent otherwise accumulates a stack of identical retry nudges and
/// replays the whole pile on its next wake (the operator-visible noise this
/// fixes). ONLY same-kind AGEND-AUTO lines are dropped — a different `kind=` and
/// EVERY ordinary notification are preserved verbatim (byte-for-byte, including a
/// line that fails to parse).
///
/// No message loss: the read-modify-write is serialized against the drainer by
/// the SAME per-agent `drain.lock`, and snapshots the queue via the drainer's
/// atomic rename-claim — a lock-free [`append_queued`] racing in AFTER the claim
/// lands in a fresh queue file and is preserved by the re-append, never clobbered.
/// If the drain lock is held (a drainer is mid-delivery) the coalesce is SKIPPED
/// and we fall back to a plain append: skipping a round cannot lose a message (at
/// worst a transient duplicate the drainer is already consuming). On any IO error
/// after the claim, the claim file is LEFT for stale-recovery rather than removed
/// (re-delivered, never dropped).
pub fn enqueue_coalesced_auto(home: &Path, agent_name: &str, text: &str) -> anyhow::Result<()> {
    let new_msg = QueuedNotification {
        text: text.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        actionable: false,
        deferred_since_ms: chrono::Utc::now().timestamp_millis(),
    };
    let Some(kind_key) = agend_auto_kind_prefix(text) else {
        // Not an AGEND-AUTO nudge (defensive — the caller only routes those
        // here): nothing to coalesce on, append unchanged.
        return append_queued(home, agent_name, &new_msg);
    };
    // Serialize vs the drainer; lock held → plain append (no coalesce, no loss).
    let Ok(Some(_lock)) = crate::store::try_acquire_file_lock(&drain_lock_path(home, agent_name))
    else {
        return append_queued(home, agent_name, &new_msg);
    };
    let path = queue_path(home, agent_name);
    if path.exists() {
        let claim = draining_claim_path(home, agent_name);
        if std::fs::rename(&path, &claim).is_ok() {
            // Re-append every claimed RAW line EXCEPT same-kind AGEND-AUTO nudges.
            // Raw lines (not re-serialized structs) so a non-matching OR
            // unparseable line survives byte-for-byte. Remove the claim only
            // after a successful re-append; on IO error leave it for the
            // drainer's stale-claim recovery (re-delivered, never lost).
            if let Ok(content) = std::fs::read_to_string(&claim) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
                    for line in content.lines() {
                        let drop_it = serde_json::from_str::<QueuedNotification>(line)
                            .map(|m| m.text.starts_with(&kind_key))
                            .unwrap_or(false);
                        if !drop_it {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    let _ = std::fs::remove_file(&claim);
                }
            }
        }
        // rename failed (queue vanished mid-claim) → nothing to coalesce.
    }
    append_queued(home, agent_name, &new_msg)
    // `_lock` drops here → drain lock released.
}

pub fn pending_count(home: &Path, agent_name: &str) -> usize {
    let mut count = 0;
    let mut paths = list_draining_files(home, agent_name);
    paths.push(queue_path(home, agent_name));
    for path in paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        count += content.lines().count();
    }
    count
}

/// A foreign draining file older than this is a crashed drainer's leftover and
/// gets recovered into the next drain. A healthy in-flight claim lives for
/// milliseconds (rename → read → inject), so 30s is comfortably past any live
/// window while still bounding how long a crash can strand its claimed lines.
const STALE_DRAINING_MS: u128 = 30_000;

/// Monotonic per-process suffix so every claim file is unique even within one
/// millisecond (two flushers in the same process, e.g. TUI loop + per-tick
/// handler in app-mode).
static CLAIM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn draining_claim_path(home: &Path, agent_name: &str) -> PathBuf {
    let seq = CLAIM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    queue_path(home, agent_name).with_extension(format!("draining-{}-{}", std::process::id(), seq))
}

/// Every draining file for `agent_name`, regardless of claim suffix. Also
/// matches the legacy fixed `<agent>.draining` name written by older binaries
/// (crash recovery must still pick those up after an upgrade).
fn list_draining_files(home: &Path, agent_name: &str) -> Vec<PathBuf> {
    let dir = home.join("notification-queue");
    let prefix = format!("{agent_name}.draining");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(prefix.as_str()))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

fn draining_file_is_stale(path: &Path, stale_ms: u128) -> bool {
    // Metadata anomalies (file vanished mid-scan, future mtime after a clock
    // step) are treated as STALE: this check only runs under the per-agent
    // drain lock, where no live peer can own the file — recovering it is
    // safe, while skipping would strand it forever and permanently inflate
    // `pending_count` (reviewer challenge 3, PR #1).
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_millis() >= stale_ms)
        .unwrap_or(true)
}

/// Per-agent drain mutex file. Sibling of the queue file; the name shares the
/// `<agent>.` prefix but NOT the `<agent>.draining` prefix, so neither
/// `list_draining_files` nor `pending_count` ever picks it up as queue content.
fn drain_lock_path(home: &Path, agent_name: &str) -> PathBuf {
    queue_path(home, agent_name).with_extension("drain.lock")
}

/// Claim-exclusive drain. The TUI flush loop and the daemon's per-tick
/// `notification_flush` handler run in DIFFERENT processes and may drain the
/// same agent concurrently, so the whole critical section is serialized by a
/// per-agent OS file lock (`store::try_acquire_file_lock` — the #1629
/// FLOCK_DEPTH chokepoint):
///
/// 1. Try-lock on `<agent>.drain.lock` — held means a peer flusher is
///    draining this agent right now; walk away empty (the holder delivers,
///    and our caller retries next tick). The lock releases on drop, including
///    on crash (the OS releases file locks with the process).
/// 2. Inside the lock: a FRESH foreign draining file is a recently-crashed
///    peer's in-flight claim — leave it alone until the STALE window (≥30s)
///    passes, then crash-recover its lines. (A LIVE peer is excluded by the
///    lock, so any foreign claim file seen here belongs to a dead drainer.)
/// 3. The live queue is claimed by renaming it to a unique per-process claim
///    file, then read + removed.
///
/// Plain rename-arbitration without the lock double-delivered under CI
/// concurrency (windows-latest, run 27248027241): two racing renames of the
/// same source can interleave (open-source → set-rename-info), re-renaming
/// the winner's just-claimed file so BOTH drainers read the same lines. The
/// OS lock makes single-drainer-per-agent a structural invariant instead of
/// a rename race.
pub fn drain(home: &Path, agent_name: &str) -> Vec<QueuedNotification> {
    drain_with_stale_threshold(home, agent_name, STALE_DRAINING_MS)
}

/// #2028: outcome of one drain attempt. The flusher callers collapse
/// `Unavailable` to empty (they retry next tick — unchanged behavior);
/// single-shot callers (`drain_one`) retry instead of trusting a transient
/// hiccup as "queue empty".
pub(crate) enum DrainAttempt {
    Drained(Vec<QueuedNotification>),
    /// Could not claim: drain lock held/unopenable, or the queue file exists
    /// but the claim rename failed. "Not sure" — NEVER "empty".
    Unavailable,
}

/// `stale_ms` injected for deterministic tests (0 = recover any leftover now).
/// Flusher-facing wrapper: `Unavailable` collapses to empty (the holder
/// delivers; this caller retries next tick).
pub(crate) fn drain_with_stale_threshold(
    home: &Path,
    agent_name: &str,
    stale_ms: u128,
) -> Vec<QueuedNotification> {
    match try_drain_with_stale_threshold(home, agent_name, stale_ms) {
        DrainAttempt::Drained(v) => v,
        DrainAttempt::Unavailable => Vec::new(),
    }
}

/// #1629: routed through the store chokepoint so the flock bumps
/// FLOCK_DEPTH for the self-IPC deadlock guard. Non-blocking on purpose:
/// a held lock means a live peer flusher is mid-delivery — we must not
/// dead-wait on it. #2028 made the non-blocking outcome HONEST: lock
/// unavailable (or unopenable, or a failed claim rename over an existing
/// queue) is `Unavailable`, not an empty vec — under llvm-cov-grade load a
/// transient open/lock hiccup was reported as "queue empty" and single-shot
/// callers believed it. No inject/self-IPC happens while the guard is held:
/// drain only touches files and returns; injection runs after the guard
/// drops.
pub(crate) fn try_drain_with_stale_threshold(
    home: &Path,
    agent_name: &str,
    stale_ms: u128,
) -> DrainAttempt {
    let Ok(Some(_drain_lock)) =
        crate::store::try_acquire_file_lock(&drain_lock_path(home, agent_name))
    else {
        return DrainAttempt::Unavailable;
    };
    let mut out = Vec::new();
    for leftover in list_draining_files(home, agent_name) {
        if draining_file_is_stale(&leftover, stale_ms) {
            out.extend(read_drain_file(&leftover));
        }
    }
    let path = queue_path(home, agent_name);
    if path.exists() {
        let claim = draining_claim_path(home, agent_name);
        match std::fs::rename(&path, &claim) {
            Ok(()) => out.extend(read_drain_file(&claim)),
            Err(_) if path.exists() => {
                // The queue is RIGHT THERE but we couldn't claim it. Stale
                // leftovers (if any) were already consumed above and must be
                // DELIVERED, not dropped — so this is Unavailable only when
                // we'd otherwise return a false "empty".
                if out.is_empty() {
                    return DrainAttempt::Unavailable;
                }
            }
            Err(_) => {} // queue vanished mid-claim — genuinely nothing left for us
        }
    }
    // `_drain_lock` drops here → OS lock released + FLOCK_DEPTH decremented.
    DrainAttempt::Drained(out)
}

pub fn requeue_all(home: &Path, agent_name: &str, notifications: &[QueuedNotification]) {
    for notification in notifications {
        // #1513: preserve actionable + deferred_since_ms verbatim so the
        // MAX_DEFER cap keeps counting from the original defer.
        // #2028: a swallowed append here is MESSAGE LOSS (the items were
        // already claimed out of the queue) — surface it loudly; the next
        // drain honestly reports the smaller queue either way.
        if let Err(e) = append_queued(home, agent_name, notification) {
            tracing::error!(
                agent = agent_name,
                error = %e,
                text = %notification.text.chars().take(80).collect::<String>(),
                "notification requeue FAILED — this queued message is LOST"
            );
        }
    }
}

fn read_drain_file(path: &Path) -> Vec<QueuedNotification> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let notifications = content
        .lines()
        .filter_map(|line| serde_json::from_str::<QueuedNotification>(line).ok())
        .collect::<Vec<_>>();
    let _ = std::fs::remove_file(path);
    notifications
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-notification-queue-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Retry-accumulate `drain` to absorb #2028's transient `Unavailable→empty`.
    /// `drain()` is contractually allowed to return an empty vec when
    /// `try_acquire_file_lock` hits a transient open/lock hiccup under heavy
    /// parallel load (llvm-cov-grade fd pressure) — production's flusher simply
    /// retries next tick, so a one-shot drain is NOT authoritative. A test that
    /// trusts it indexes an empty vec → index panic (the #2072 coverage flake,
    /// notification_queue.rs `again[0]`). This helper models the retry: it keeps
    /// draining (accumulating, since `drain` is destructive) until it has `want`
    /// items or the bounded budget elapses. The happy path returns on the first
    /// attempt — zero behavior change when the drain succeeds immediately.
    fn drain_settled(home: &Path, agent_name: &str, want: usize) -> Vec<QueuedNotification> {
        drain_settled_with_stale(home, agent_name, want, STALE_DRAINING_MS)
    }

    fn drain_settled_with_stale(
        home: &Path,
        agent_name: &str,
        want: usize,
        stale_ms: u128,
    ) -> Vec<QueuedNotification> {
        let mut acc = Vec::new();
        for _ in 0..200 {
            acc.extend(drain_with_stale_threshold(home, agent_name, stale_ms));
            if acc.len() >= want {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        acc
    }

    // #t-3558 P2 — coalesce keep-latest. Pins (lead's hard conditions): the
    // rewrite drops ONLY same-kind AGEND-AUTO lines, never a normal message nor a
    // different AGEND-AUTO kind.
    #[test]
    fn coalesce_keeps_latest_same_kind_and_preserves_others() {
        let home = tmp_home("coalesce-keep");
        let a = "agent";
        let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
        let pb = "[AGEND-AUTO kind=progress-backstop] continue";
        enqueue(&home, a, "hello world").expect("normal");
        enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
        enqueue_coalesced_auto(&home, a, pb).expect("different kind");
        enqueue_coalesced_auto(&home, a, rl).expect("rl#2 coalesces rl#1");

        let drained = drain_settled(&home, a, 3);
        let texts: Vec<&str> = drained.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            drained.len(),
            3,
            "hello + 1 ratelimit + 1 progress-backstop; got {texts:?}"
        );
        assert_eq!(
            texts
                .iter()
                .filter(|t| t.contains("kind=ratelimit-retry"))
                .count(),
            1,
            "only the LATEST ratelimit nudge kept; got {texts:?}"
        );
        assert!(
            texts.contains(&"hello world"),
            "normal message preserved; got {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("kind=progress-backstop")),
            "a DIFFERENT AGEND-AUTO kind is never coalesced away; got {texts:?}"
        );
    }

    // #t-3558 P2 — coalesce operates on RAW lines, so a non-JSON/unparseable
    // queue line is preserved byte-for-byte (never dropped by the filter rewrite).
    #[test]
    fn coalesce_preserves_unparseable_lines() {
        let home = tmp_home("coalesce-raw");
        let a = "agent";
        let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
        let qp = queue_path(&home, a);
        std::fs::create_dir_all(qp.parent().unwrap()).unwrap();
        {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&qp)
                .unwrap();
            writeln!(f, "GARBAGE not json").unwrap();
        }
        enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
        enqueue_coalesced_auto(&home, a, rl).expect("rl#2 coalesces");

        let raw = std::fs::read_to_string(&qp).unwrap();
        assert!(
            raw.lines().any(|l| l == "GARBAGE not json"),
            "unparseable line preserved; got:\n{raw}"
        );
        assert_eq!(
            raw.lines()
                .filter(|l| l.contains("kind=ratelimit-retry"))
                .count(),
            1,
            "ratelimit coalesced to the latest; got:\n{raw}"
        );
    }

    // #t-3558 P2 — drain-lock contention: when a drainer holds the lock, coalesce
    // SKIPS (no read-modify-write) and falls back to a plain append → no message
    // loss while contended; coalesce resumes once the lock frees.
    #[test]
    fn coalesce_falls_back_to_append_when_drain_lock_held_no_loss() {
        let home = tmp_home("coalesce-fallback");
        let a = "agent";
        let rl = "[AGEND-AUTO kind=ratelimit-retry] continue";
        enqueue_coalesced_auto(&home, a, rl).expect("rl#1");
        {
            // Simulate a drainer mid-claim by holding the per-agent drain lock.
            let _held = crate::store::try_acquire_file_lock(&drain_lock_path(&home, a))
                .expect("lock op")
                .expect("lock acquired");
            enqueue_coalesced_auto(&home, a, rl).expect("rl#2 under held lock → fallback append");
            let raw = std::fs::read_to_string(queue_path(&home, a)).unwrap();
            assert_eq!(
                raw.lines().count(),
                2,
                "lock held → fallback append keeps BOTH nudges (no loss); got:\n{raw}"
            );
        }
        // Lock freed → a fresh coalesce now collapses to keep-latest.
        enqueue_coalesced_auto(&home, a, rl).expect("rl#3 coalesces once free");
        let drained = drain_settled(&home, a, 1);
        assert_eq!(
            drained.len(),
            1,
            "after the lock frees, coalesce keeps exactly the latest"
        );
    }

    #[test]
    fn enqueue_classified_round_trips_actionable_and_deferred_since_1513() {
        let home = tmp_home("classified");
        enqueue_classified(&home, "a", "work", true).expect("enqueue actionable");
        enqueue(&home, "a", "ambient").expect("enqueue ambient"); // actionable=false default
        let drained = drain_settled(&home, "a", 2);
        assert_eq!(drained.len(), 2);
        let actionable = drained
            .iter()
            .find(|q| q.text == "work")
            .expect("find actionable");
        assert!(
            actionable.actionable,
            "actionable flag preserved across serde"
        );
        assert!(actionable.deferred_since_ms > 0, "deferred_since stamped");
        let ambient = drained
            .iter()
            .find(|q| q.text == "ambient")
            .expect("find ambient");
        assert!(!ambient.actionable, "ambient default false");
        // requeue preserves the original deferred_since (cap counts from first defer)
        let since = actionable.deferred_since_ms;
        requeue_all(&home, "a", std::slice::from_ref(actionable));
        let again = drain_settled(&home, "a", 1);
        assert_eq!(
            again[0].deferred_since_ms, since,
            "requeue preserves deferred_since"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn drain_settled_retries_through_transient_lock_contention_2072() {
        // Deterministic reproduction of the #2072 coverage-flake MECHANISM:
        // while a peer holds the drain lock, a one-shot `drain()` returns empty
        // (#2028 `Unavailable→empty`), so a test that trusts it indexes an empty
        // vec → index panic (the live failure at `again[0]`). `drain_settled`
        // must RETRY across the contention window and recover the item once the
        // lock frees — exactly what the production flusher does next tick.
        use std::sync::mpsc;
        let home = tmp_home("transient_lock_2072");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).ok();
        enqueue(&home, "a", "delayed").expect("enqueue");

        let (held_tx, held_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let lock_path = drain_lock_path(&home, "a");
        let peer = std::thread::spawn(move || {
            let guard = crate::store::try_acquire_file_lock(&lock_path)
                .expect("lock op")
                .expect("peer acquires drain lock");
            held_tx.send(()).expect("signal held");
            // Hold until the test has observed the contention, then release
            // inside `drain_settled`'s retry window.
            go_rx.recv().expect("await go");
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(guard);
        });

        held_rx.recv().expect("peer holds the lock");
        // A one-shot drain while the lock is held is empty — the exact trap.
        assert!(
            drain(&home, "a").is_empty(),
            "one-shot drain under contention returns empty (#2028 Unavailable→empty)"
        );
        go_tx.send(()).expect("let the peer schedule its release");
        let got = drain_settled(&home, "a", 1);
        peer.join().ok();
        assert_eq!(
            got.len(),
            1,
            "drain_settled recovers the item after the lock frees"
        );
        assert_eq!(got[0].text, "delayed");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn pending_count_tracks_enqueued_notifications() {
        let home = tmp_home("count");
        enqueue(&home, "agent1", "a").expect("enqueue a");
        enqueue(&home, "agent1", "b").expect("enqueue b");
        assert_eq!(pending_count(&home, "agent1"), 2);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn drain_roundtrip() {
        let home = tmp_home("drain");
        enqueue(&home, "agent1", "a").expect("enqueue a");
        enqueue(&home, "agent1", "b").expect("enqueue b");
        let drained = drain_settled(&home, "agent1", 2);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "a");
        assert_eq!(pending_count(&home, "agent1"), 0);
        std::fs::remove_dir_all(home).ok();
    }

    /// Sprint 54 P2-3: round-trip both timestamps; ensure
    /// `read_input_submit_timestamps` returns paired values and
    /// `record_submit_activity` writes a value strictly newer than the
    /// preceding `record_input_activity` call.
    #[test]
    fn record_and_read_input_submit_timestamps_round_trip() {
        let home = tmp_home("ts_round_trip");
        // Fresh agent → both 0.
        let (typed0, submit0) = read_input_submit_timestamps(&home, "agent1");
        assert_eq!((typed0, submit0), (0, 0));
        record_input_activity(&home, "agent1");
        std::thread::sleep(Duration::from_millis(2));
        record_submit_activity(&home, "agent1");
        let (typed1, submit1) = read_input_submit_timestamps(&home, "agent1");
        assert!(typed1 > 0, "typed timestamp must be set after record");
        assert!(submit1 > 0, "submit timestamp must be set after record");
        assert!(
            submit1 >= typed1,
            "submit (called second) must be ≥ typed (called first), got typed={typed1} submit={submit1}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1680 regression: the keystroke WRITE (`record_input_activity` →
    /// `save_metadata` → `metadata_path_resolved` → `<uuid>.json`) and the
    /// draft-gate READ (`read_input_submit_timestamps`) MUST land on the SAME
    /// file when fleet.yaml maps the name → a UUID. Pre-#1680 the read hand-coded
    /// `<name>.json` (bypassing the resolver) while the write went to
    /// `<uuid>.json`, so they never intersected → `draft_state` was permanently
    /// stale (`None`) → the inject path force-submitted the operator's unsent
    /// draft. Every prior test used a home with NO fleet.yaml (the resolver falls
    /// back to the name path, so write/read happen to converge) and so could not
    /// catch the split. This pins the id-mapped path: it FAILS before the read
    /// resolver alignment and PASSES after.
    #[test]
    fn draft_gate_read_resolves_uuid_like_write_1680() {
        let home = tmp_home("draft_gate_uuid_1680");
        // Isolate from any prior run sharing the same (suffix,pid) dir.
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).ok();
        // fleet.yaml maps the fleet NAME → a UUID, so the resolver routes
        // metadata to `<uuid>.json` — the production shape that exposed the split.
        let id = crate::types::InstanceId::new();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  fixup-x:\n    id: {}\n", id.full()),
        )
        .expect("write fleet.yaml");

        // WRITE a compose keystroke (no submit) → `<uuid>.json` via the resolver.
        record_input_activity(&home, "fixup-x");

        // READ must see it through the SAME resolver. Pre-fix this reads the
        // never-written `<name>.json` and returns 0 → assert fails (RED).
        let (typed, submit) = read_input_submit_timestamps(&home, "fixup-x");
        assert!(
            typed > 0,
            "#1680: read must resolve the same UUID file the write used \
             (got typed=0 → it read the stale name-path)"
        );
        assert_eq!(submit, 0, "no submit recorded yet");
        // End-to-end gate signal: a recent unsent draft must read as Drafting,
        // NOT None — `None` is precisely what let the inject clobber the draft.
        assert_eq!(
            draft_state(&home, "fixup-x"),
            DraftState::Drafting,
            "#1680: a live unsent operator draft must gate (Drafting), not read as None"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// Sprint 54 P2-3: typed-only (no submit) must read as
    /// `submit_ms == 0`. This is the daemon-supervisor's signal for
    /// "user typed but never pressed Enter" — it MUST distinguish
    /// from "user typed AND submitted" otherwise the dedup logic
    /// degrades to never firing.
    #[test]
    fn typed_only_leaves_submit_zero() {
        let home = tmp_home("typed_only");
        record_input_activity(&home, "agent1");
        let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
        assert!(typed > 0);
        assert_eq!(
            submit, 0,
            "submit must stay 0 until record_submit_activity is called"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1457: write raw input/submit timestamps so draft-state tests are
    /// deterministic (no sleeps / no process-global env).
    fn write_ts(home: &Path, agent: &str, typed_ms: i64, submit_ms: i64) {
        if typed_ms != 0 {
            agent_ops::save_metadata(home, agent, COMPOSE_METADATA_KEY, json!(typed_ms));
        }
        if submit_ms != 0 {
            agent_ops::save_metadata(home, agent, SUBMIT_METADATA_KEY, json!(submit_ms));
        }
    }

    // ── #1944: input_box_is_empty (buffer-content draft refinement) ──

    #[test]
    fn input_box_empty_when_only_prompt_marker() {
        // claude `❯ ` with nothing typed (real capture: claude-discussion-text.raw
        // ends in exactly this) → empty.
        assert_eq!(input_box_is_empty("some output\n❯ ", "❯"), Some(true));
        assert_eq!(input_box_is_empty("output\n> ", ">"), Some(true));
        // trailing whitespace / wrapped blank only
        assert_eq!(input_box_is_empty("❯   \n", "❯"), Some(true));
    }

    #[test]
    fn input_box_nonempty_when_text_after_marker() {
        // a real live draft → protect (defer).
        assert_eq!(
            input_box_is_empty("output\n❯ hello world", "❯"),
            Some(false)
        );
        assert_eq!(input_box_is_empty("> draft text", ">"), Some(false));
    }

    #[test]
    fn input_box_none_when_marker_absent() {
        // agent mid-output / no prompt rendered → cannot determine → None
        // (caller fails toward protection).
        assert_eq!(input_box_is_empty("just output, no prompt", "❯"), None);
        assert_eq!(input_box_is_empty("", "❯"), None);
    }

    #[test]
    fn input_box_uses_bottom_most_marker_not_prose() {
        // #1944 prose-FP guard: a `❯`/`>` that appears MID-prose above the input
        // box must not be matched — only the bottom-most line whose first
        // non-blank char IS the marker counts as the live input prompt.
        let screen = "the agent printed ❯ in its output\nmore prose with > inside\n❯ ";
        assert_eq!(input_box_is_empty(screen, "❯"), Some(true));
        // a markdown blockquote above, real empty input below
        let screen2 = "> quoted line from agent output\nplain text\n> ";
        assert_eq!(input_box_is_empty(screen2, ">"), Some(true));
        // typed input below a blockquote → non-empty (still the bottom-most)
        let screen3 = "> quoted output\n> my actual draft";
        assert_eq!(input_box_is_empty(screen3, ">"), Some(false));
    }

    // ── #1948 v2: input_box_empty_probe (marker → placeholder → fallback) ──

    #[test]
    fn probe_marker_path_decides_directly() {
        // claude/codex/agy: marker present → decided by content after marker;
        // placeholder is ignored (None) for these backends.
        assert_eq!(
            input_box_empty_probe("out\n❯ ", Some("❯"), None),
            Some(true)
        );
        assert_eq!(
            input_box_empty_probe("out\n❯ typed", Some("❯"), None),
            Some(false)
        );
        // marker present but no prompt line in the tail (mid-output) → None
        // (fail-protect), NOT silently falling through to a non-existent placeholder.
        assert_eq!(input_box_empty_probe("just output", Some("❯"), None), None);
    }

    #[test]
    fn probe_placeholder_path_for_kiro() {
        // kiro: no marker, placeholder VISIBLE → empty (deliver). Uses the real
        // captured placeholder text (live pane_snapshot of a cleared kiro pane).
        let ph = "ask a question or describe a task";
        let empty_screen = "  Kiro · auto · 13%\n\n ask a question or describe a task ↵\n  /copy";
        assert_eq!(
            input_box_empty_probe(empty_screen, None, Some(ph)),
            Some(true)
        );
        // typed → placeholder replaced/absent → None (fail toward protection).
        let typed_screen = "  Kiro · auto · 13%\n\n half-typed reply\n  /copy";
        assert_eq!(input_box_empty_probe(typed_screen, None, Some(ph)), None);
    }

    #[test]
    fn probe_none_when_neither_marker_nor_placeholder() {
        // Shell/Raw / markerless backend → fall back to timestamp behavior.
        assert_eq!(input_box_empty_probe("$ ", None, None), None);
    }

    // ── #1948(b): input_box_dim_aware_empty (codex DIM-ghost) ──

    #[test]
    fn dim_aware_empty_when_ghost_is_dim() {
        // codex empty box: `› <dim ghost>` — the prompt is bold (idx 0, not dim),
        // every non-ws char after it is the DIM ghost → box empty (deliver).
        let text = "› Use /skills to list available skills";
        let n = text.chars().count();
        let dim: Vec<bool> = (0..n).map(|i| i != 0).collect();
        assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(true));
    }

    #[test]
    fn dim_aware_nonempty_when_input_is_normal_intensity() {
        // a real draft: `› my draft` rendered at NORMAL intensity (no dim) → a
        // non-dim glyph after the marker → real input (protect).
        let text = "› my half-typed reply";
        let dim = vec![false; text.chars().count()];
        assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(false));
    }

    #[test]
    fn dim_aware_empty_when_only_whitespace_after_marker() {
        let text = "some output\n› ";
        let dim = vec![false; text.chars().count()];
        assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(true));
    }

    #[test]
    fn dim_aware_none_when_no_marker_line() {
        let text = "just output, no prompt";
        let dim = vec![false; text.chars().count()];
        assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), None);
    }

    #[test]
    fn dim_aware_uses_bottom_most_marker_line() {
        // a `›` in DIM prose above + the real input box below at NORMAL intensity
        // → the bottom-most marker line (real input) decides → Some(false).
        let text = "› a dim quote in output\n› my real draft";
        let n = text.chars().count();
        let nl_char = text.split('\n').next().unwrap_or("").chars().count();
        // first line (+ its `\n`) dim; second line (real input) normal.
        let dim: Vec<bool> = (0..n).map(|i| i <= nl_char).collect();
        assert_eq!(input_box_dim_aware_empty(text, &dim, "›"), Some(false));
    }

    /// #1457: submitted (or never-typed) buffer → None → notifications deliver.
    /// This is the submit-then-flush release path.
    #[test]
    fn draft_state_none_when_submitted_or_clean() {
        let home = tmp_home("draft_none");
        let now = chrono::Utc::now().timestamp_millis();
        // never typed
        assert_eq!(draft_state(&home, "fresh"), DraftState::None);
        // typed then submitted (submit newer) → clean
        write_ts(&home, "submitted", now - 1000, now - 100);
        assert_eq!(draft_state(&home, "submitted"), DraftState::None);
        std::fs::remove_dir_all(home).ok();
    }

    /// #1457: unsent draft, typed recently → Drafting (defer). Crucially this
    /// holds regardless of how long the pause is (no 3s window) — the old
    /// `is_composing` would have false-negatived after 3s of thinking.
    #[test]
    fn draft_state_drafting_when_typed_after_submit() {
        let home = tmp_home("draft_drafting");
        let now = chrono::Utc::now().timestamp_millis();
        // typed AFTER last submit, well within the escape window — but also
        // older than the old 3s window, proving we no longer false-negative.
        write_ts(&home, "a", now - 60_000, now - 120_000);
        assert_eq!(draft_state(&home, "a"), DraftState::Drafting);
        std::fs::remove_dir_all(home).ok();
    }

    /// #1457: unsent draft idle past the escape window → Abandoned (release).
    /// Covers the "typed then deleted the draft / walked away" edge — the
    /// escape valve is what bounds the otherwise-indefinite defer.
    #[test]
    fn draft_state_abandoned_past_escape_window() {
        let home = tmp_home("draft_abandoned");
        let now = chrono::Utc::now().timestamp_millis();
        // typed 400s ago, with a PRIOR submit (submit_ms>0) → past the 300s
        // escape → genuine "typed then walked away" → Abandoned (trickle kept).
        write_ts(&home, "a", now - 400_000, now - 500_000);
        assert_eq!(draft_state(&home, "a"), DraftState::Abandoned);
        std::fs::remove_dir_all(home).ok();
    }

    /// #1473: stale typed + NEVER submitted (submit_ms==0) → None, NOT
    /// Abandoned. This is the regression case: an agent pane the operator
    /// poked once but never composed in (e.g. codex reviewer) was trapped in
    /// Abandoned, deferring its wakes forever. Must deliver normally now.
    #[test]
    fn draft_state_never_submitted_stale_is_none() {
        let home = tmp_home("draft_never_submit");
        let now = chrono::Utc::now().timestamp_millis();
        // typed 400s ago (past escape), submit_ms == 0 (never submitted).
        write_ts(&home, "a", now - 400_000, 0);
        assert_eq!(
            draft_state(&home, "a"),
            DraftState::None,
            "never-submitted stale pane must be None, not Abandoned (#1473)"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1473 guard: the scoped fix must NOT weaken the active-draft protection.
    /// A RECENT first-ever draft (typed now, submit_ms==0) is still Drafting —
    /// proving a naive top-level `submit==0→None` (which would regress #1457)
    /// was avoided.
    #[test]
    fn draft_state_first_draft_recent_still_drafting() {
        let home = tmp_home("draft_first");
        let now = chrono::Utc::now().timestamp_millis();
        write_ts(&home, "a", now - 1_000, 0); // typed 1s ago, never submitted
        assert_eq!(
            draft_state(&home, "a"),
            DraftState::Drafting,
            "recent first draft must stay protected (Drafting), not None (#1457 preserved)"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// #1457: escape valve releases ONE oldest notification, leaving the rest
    /// queued (no clobbering batch).
    #[test]
    fn drain_one_pops_oldest_leaves_rest() {
        let home = tmp_home("drain_one");
        enqueue(&home, "a", "first").expect("enqueue first");
        enqueue(&home, "a", "second").expect("enqueue second");
        enqueue(&home, "a", "third").expect("enqueue third");
        let popped = drain_one(&home, "a").expect("one popped");
        assert_eq!(popped.text, "first", "oldest must be released first");
        assert_eq!(pending_count(&home, "a"), 2, "rest stay queued");
        assert_eq!(drain_one(&home, "a").expect("second pop").text, "second");
        assert_eq!(drain_one(&home, "a").expect("third pop").text, "third");
        assert!(drain_one(&home, "a").is_none(), "empty after draining all");
        std::fs::remove_dir_all(home).ok();
    }

    /// Concurrent-claim contract: a FRESH foreign draining file belongs to a
    /// LIVE concurrent drain (e.g. the TUI flush mid-drain while the daemon's
    /// per-tick flush scans the same agent). Re-reading it double-delivers
    /// every line it contains. `drain` must claim work ONLY by atomically
    /// renaming the live queue file; a fresh foreign draining file is left
    /// untouched (only STALE ones — a crashed drainer's leftovers — are
    /// recovered).
    #[test]
    fn drain_does_not_steal_fresh_foreign_draining_file() {
        let home = tmp_home("foreign_draining");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).ok();
        enqueue(&home, "a", "claimed-by-peer").expect("enqueue");
        // Simulate a concurrent drainer that has JUST claimed the queue
        // (renamed it to its draining file and is about to inject).
        std::fs::rename(queue_path(&home, "a"), draining_path(&home, "a"))
            .expect("simulate peer claim");
        let got = drain(&home, "a");
        assert!(
            got.is_empty(),
            "a fresh foreign draining file must NOT be re-read — that \
             double-delivers the peer's claimed items: {got:?}"
        );
        assert_eq!(
            pending_count(&home, "a"),
            1,
            "the peer's claimed item still counts as pending (it owns delivery)"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// Crash recovery: a STALE draining file (its drainer died mid-flight —
    /// including the legacy fixed-name file from a pre-claim-atomic binary)
    /// must be folded into the next drain rather than stranding forever.
    /// `stale_ms = 0` makes "stale" deterministic without mtime manipulation.
    #[test]
    fn drain_recovers_stale_draining_leftover() {
        let home = tmp_home("stale_draining");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).ok();
        enqueue(&home, "a", "crashed-claim").expect("enqueue");
        std::fs::rename(queue_path(&home, "a"), draining_path(&home, "a"))
            .expect("simulate crashed drainer's leftover claim");
        let got = drain_settled_with_stale(&home, "a", 1, 0);
        assert_eq!(got.len(), 1, "stale leftover must be recovered");
        assert_eq!(got[0].text, "crashed-claim");
        assert_eq!(pending_count(&home, "a"), 0, "leftover consumed");
        std::fs::remove_dir_all(home).ok();
    }

    /// Reviewer challenge 3 (PR #1): a metadata anomaly (vanished file /
    /// future mtime after a clock step) must read as STALE — skipping would
    /// strand the leftover forever and permanently inflate pending_count.
    /// Safe because the check only runs under the per-agent drain lock.
    #[test]
    fn stale_check_treats_metadata_anomaly_as_stale() {
        let missing = std::env::temp_dir()
            .join("agend-notification-queue-anomaly")
            .join("never-created.draining");
        assert!(
            draining_file_is_stale(&missing, 30_000),
            "unreadable metadata must classify as stale (recoverable), not strand"
        );
    }

    /// §3.9 concurrent-state harness: exactly-once delivery under racing
    /// drainers. N threads drain the same agent concurrently; every enqueued
    /// line must be delivered EXACTLY once across all threads. Serialized by
    /// the per-agent OS drain lock — plain rename arbitration double-delivered
    /// on Windows (PR #1 CI run 27248027241; both racing renames of one source
    /// can succeed because handles survive renames).
    #[test]
    fn concurrent_drains_deliver_exactly_once() {
        let home = tmp_home("concurrent_drain");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).ok();
        const ITEMS: usize = 50;
        for i in 0..ITEMS {
            enqueue(&home, "a", &format!("msg-{i}")).expect("enqueue");
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut joins = Vec::new();
        for _ in 0..4 {
            let home = home.clone();
            let barrier = barrier.clone();
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                let mut got = Vec::new();
                for _ in 0..8 {
                    got.extend(drain(&home, "a"));
                }
                got
            }));
        }
        let mut all: Vec<String> = joins
            .into_iter()
            .flat_map(|j| j.join().expect("thread join"))
            .map(|n| n.text)
            .collect();
        all.sort();
        let unique: std::collections::HashSet<&String> = all.iter().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "no line may be delivered twice across concurrent drains"
        );
        assert_eq!(
            all.len(),
            ITEMS,
            "every enqueued line must be delivered exactly once"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // ── #2028: false-empty under contention — single-shot caller honesty ──

    /// Lock held by a peer → `Unavailable`, NEVER `Drained(empty)`. The
    /// pre-#2028 collapse to an empty vec is what made `drain_one` report
    /// "queue empty" under llvm-cov-grade load.
    #[test]
    fn try_drain_reports_unavailable_while_lock_held_2028() {
        let home = tmp_home("unavail-lock");
        enqueue(&home, "a", "queued").expect("enqueue");
        let guard = crate::store::try_acquire_file_lock(&drain_lock_path(&home, "a"))
            .expect("lock open")
            .expect("lock acquired");
        assert!(
            matches!(
                try_drain_with_stale_threshold(&home, "a", STALE_DRAINING_MS),
                DrainAttempt::Unavailable
            ),
            "held lock must read as Unavailable, not empty"
        );
        drop(guard);
        match try_drain_with_stale_threshold(&home, "a", STALE_DRAINING_MS) {
            DrainAttempt::Drained(v) => {
                assert_eq!(v.len(), 1, "after release the claim drains the queue")
            }
            DrainAttempt::Unavailable => panic!("lock released — must drain"),
        }
        std::fs::remove_dir_all(home).ok();
    }

    /// Flusher contract UNCHANGED: `drain` (the next-tick-tolerant API)
    /// still collapses contention to an empty vec — the holder delivers.
    #[test]
    fn flusher_drain_still_collapses_contention_to_empty_2028() {
        let home = tmp_home("flusher-collapse");
        enqueue(&home, "a", "queued").expect("enqueue");
        let _guard = crate::store::try_acquire_file_lock(&drain_lock_path(&home, "a"))
            .expect("lock open")
            .expect("lock acquired");
        assert!(
            drain(&home, "a").is_empty(),
            "flusher API keeps the walk-away-empty semantics"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// drain_one outlasts a SHORT contention window (the healthy-peer shape:
    /// a live flusher holds the lock for the duration of a rename+read). The
    /// hold here (15ms) is well inside drain_one's retry budget (5×10ms), so
    /// margins are wide, not timing-fragile.
    #[test]
    fn drain_one_retries_through_short_contention_2028() {
        let home = tmp_home("drain-one-retry");
        enqueue(&home, "a", "the-item").expect("enqueue");
        let guard = crate::store::try_acquire_file_lock(&drain_lock_path(&home, "a"))
            .expect("lock open")
            .expect("lock acquired");
        let h = home.clone();
        let worker = std::thread::spawn(move || drain_one(&h, "a"));
        std::thread::sleep(std::time::Duration::from_millis(15));
        drop(guard);
        let popped = worker.join().expect("join");
        assert_eq!(
            popped
                .expect("must retry past the contention, not report empty")
                .text,
            "the-item"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// True-empty stays a fast None — the retry loop only engages on
    /// Unavailable, an actually-empty queue answers immediately.
    #[test]
    fn drain_one_true_empty_is_immediate_none_2028() {
        let home = tmp_home("drain-one-empty");
        let start = std::time::Instant::now();
        assert!(drain_one(&home, "a").is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_millis(40),
            "true empty must not burn the retry budget"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
