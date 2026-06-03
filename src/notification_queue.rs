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
/// Overridable via `AGEND_DRAFT_ESCAPE_SECS`; default 300s (5 min).
fn draft_escape_timeout_ms() -> i64 {
    std::env::var("AGEND_DRAFT_ESCAPE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|s| *s > 0)
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(300_000)
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

/// #1457: pop and return the single OLDEST queued notification, leaving the
/// rest queued. The escape valve uses this so an abandoned-draft pane trickles
/// its backlog one-per-tick instead of clobbering the draft with a full batch.
pub fn drain_one(home: &Path, agent_name: &str) -> Option<QueuedNotification> {
    let path = queue_path(home, agent_name);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut lines = content.lines();
    let first = lines.next()?;
    let oldest = serde_json::from_str::<QueuedNotification>(first).ok();
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        // Best-effort rewrite of the remaining lines (matches enqueue's
        // non-atomic append model — notifications are best-effort, and the
        // #911 dedup ledger absorbs a rare re-inject on crash mid-rewrite).
        let _ = std::fs::write(&path, format!("{}\n", rest.join("\n")));
    }
    oldest
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

pub fn pending_count(home: &Path, agent_name: &str) -> usize {
    let mut count = 0;
    for path in [
        queue_path(home, agent_name),
        draining_path(home, agent_name),
    ] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        count += content.lines().count();
    }
    count
}

pub fn drain(home: &Path, agent_name: &str) -> Vec<QueuedNotification> {
    let path = queue_path(home, agent_name);
    let tmp = draining_path(home, agent_name);
    if tmp.exists() {
        return read_drain_file(&tmp);
    }
    if !path.exists() {
        return Vec::new();
    }
    if std::fs::rename(&path, &tmp).is_err() {
        return Vec::new();
    }
    read_drain_file(&tmp)
}

pub fn requeue_all(home: &Path, agent_name: &str, notifications: &[QueuedNotification]) {
    for notification in notifications {
        // #1513: preserve actionable + deferred_since_ms verbatim so the
        // MAX_DEFER cap keeps counting from the original defer.
        let _ = append_queued(home, agent_name, notification);
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

    #[test]
    fn enqueue_classified_round_trips_actionable_and_deferred_since_1513() {
        let home = tmp_home("classified");
        enqueue_classified(&home, "a", "work", true).expect("enqueue actionable");
        enqueue(&home, "a", "ambient").expect("enqueue ambient"); // actionable=false default
        let drained = drain(&home, "a");
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
        let again = drain(&home, "a");
        assert_eq!(
            again[0].deferred_since_ms, since,
            "requeue preserves deferred_since"
        );
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
        let drained = drain(&home, "agent1");
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
}
