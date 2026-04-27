//! Sprint 24 P0 PR1 — append-only task event log.
//!
//! Source-of-truth storage for task board state. Replaces direct-mutation
//! `tasks.json` (PR2 routes the existing MCP `task` tool through
//! [`append`]; PR1 ships only the storage substrate).
//!
//! ## Design references
//! - `docs/TASK-BOARD-AUTO-CLOSE-REDESIGN.md` (4-perspective synthesis)
//! - dev-reviewer-2 must-haves (newtype IDs, `deny_unknown_fields`,
//!   monotonic seq, schema-version, forensic snapshots, replay aborts on
//!   unknown variant, distinct `DoneSource` variants over `Option<...>`)
//! - F7 atomic-batch lesson — multi-event append goes through one fsync
//!   so partial-write windows can't surface to readers.
//!
//! ## Forward-compat fail-closed
//! [`replay`] rejects envelopes whose `schema_version` exceeds
//! [`SCHEMA_VERSION`] (older binary observing newer-than-supported on-disk
//! data) and rejects unknown event variants (per `deny_unknown_fields`).
//! Sister-module [`crate::event_log::append`] handles the locked write +
//! rotation; this module owns the seq computation.

// PR1 ships the storage substrate only; the only in-tree caller is the
// unit-test suite. PR2 (`src/tasks.rs` migration) and the sweep daemon
// (PR2/PR3) consume this surface. `dead_code` allow lifts here as those
// consumers land — see the anti-bypass invariant in
// `tests/task_events_invariant.rs` for the contract that no other
// caller may reference the log directly.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Persisted-envelope schema version. Bumped only on changes that older
/// readers cannot interpret. Forward-compat: v(N) reader rejects v(N+1)
/// envelopes (fail-closed). Backward-compat: v(N) reader accepts v(<=N).
pub const SCHEMA_VERSION: u32 = 1;

/// Hot-file event count above which [`compact`] archives the older slice.
pub const COMPACTION_KEEP: usize = 10_000;

/// Sister-module log basename used with [`crate::event_log::append`] +
/// friends. The on-disk file is `<home>/task_events.jsonl`.
const LOG_NAME: &str = "task_events";

// ── Newtype IDs (type-system swap-prevention per dev-reviewer-2) ─────

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> Self {
        TaskId(s.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PrId(pub u64);

impl std::fmt::Display for PrId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct InstanceName(pub String);

impl InstanceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InstanceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for InstanceName {
    fn from(s: &str) -> Self {
        InstanceName(s.to_string())
    }
}

// ── Forensic snapshot embedded in events ────────────────────────────

/// GitHub PR state captured at the moment an event was emitted. Provides
/// provenance for replay correlation: when sweep concludes a PR closed a
/// task, the embedded snapshot lets a future audit reconstruct what the
/// daemon actually saw on the wire.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrSnapshot {
    /// Lifecycle state at capture time: `merged` / `closed` / `open`.
    pub pr_state: String,
    /// Squash-merge SHA captured at decision-time. Survives squash deletion.
    pub merge_sha: Option<String>,
    /// SHA-256 of the GitHub API response body (hex). Lets a forensic
    /// replay correlate against archived response bodies if the daemon
    /// stored them, without bloating every event with the full payload.
    pub api_response_hash: String,
    pub captured_at: String,
}

// ── DoneSource — distinct variants over `Option<reason>` ambiguity ──

/// Why a task transitioned to Done. Kept as an inner enum (not three
/// top-level events) so `Done { source }` is one state transition with
/// pluggable provenance — and so `DoneSource::OperatorManual` can be
/// distinguished from `DoneSource::PrMerged` at the type level rather
/// than via a nullable `pr_id` field (per dev-reviewer-2 must-have:
/// collapse `Option<X>` carrying implicit semantic to distinct variants).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "via", deny_unknown_fields)]
pub enum DoneSource {
    PrMerged {
        pr_id: PrId,
        merge_sha: String,
        merged_at: String,
        snapshot: PrSnapshot,
    },
    OperatorManual {
        authored_at: String,
        result: Option<String>,
    },
    LegacyBackfill {
        sweep_id: String,
        reasoning: String,
        snapshot: Option<PrSnapshot>,
    },
}

/// How a `Linked` event was discovered: explicit operator/agent action
/// vs the Phase 2 sweep daemon's PR-body parser.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "via", deny_unknown_fields)]
pub enum LinkSource {
    Explicit { authored_at: String },
    SweepDiscovery { sweep_id: String },
}

// ── TaskEvent — 10 variants, exhaustive match enforces forward audit ─

/// One state transition on a task. `kind` tag at the top level; inner
/// `via` tags on `DoneSource` / `LinkSource` keep the sub-provenance
/// type-system enforced. `deny_unknown_fields` rejects on-disk payloads
/// with extra keys (forward-compat: a v2 writer adding fields surfaces
/// as a parse error in v1 readers, which abort replay rather than
/// silently drop fields).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum TaskEvent {
    Created {
        task_id: TaskId,
        title: String,
        description: String,
        priority: String,
        owner: Option<InstanceName>,
    },
    Claimed {
        task_id: TaskId,
        by: InstanceName,
    },
    InProgress {
        task_id: TaskId,
        by: InstanceName,
    },
    Verified {
        task_id: TaskId,
        by_reviewer: InstanceName,
        verdict: String,
    },
    Done {
        task_id: TaskId,
        by: InstanceName,
        source: DoneSource,
    },
    Cancelled {
        task_id: TaskId,
        by: InstanceName,
        reason: String,
    },
    Linked {
        task_id: TaskId,
        pr_id: PrId,
        source: LinkSource,
        snapshot: PrSnapshot,
    },
    Blocked {
        task_id: TaskId,
        reason: String,
    },
    Unblocked {
        task_id: TaskId,
    },
    Reopened {
        task_id: TaskId,
        reason: String,
        source_evidence: String,
    },
}

impl TaskEvent {
    pub fn task_id(&self) -> &TaskId {
        match self {
            TaskEvent::Created { task_id, .. }
            | TaskEvent::Claimed { task_id, .. }
            | TaskEvent::InProgress { task_id, .. }
            | TaskEvent::Verified { task_id, .. }
            | TaskEvent::Done { task_id, .. }
            | TaskEvent::Cancelled { task_id, .. }
            | TaskEvent::Linked { task_id, .. }
            | TaskEvent::Blocked { task_id, .. }
            | TaskEvent::Unblocked { task_id }
            | TaskEvent::Reopened { task_id, .. } => task_id,
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            TaskEvent::Created { .. } => "created",
            TaskEvent::Claimed { .. } => "claimed",
            TaskEvent::InProgress { .. } => "in_progress",
            TaskEvent::Verified { .. } => "verified",
            TaskEvent::Done { .. } => "done",
            TaskEvent::Cancelled { .. } => "cancelled",
            TaskEvent::Linked { .. } => "linked",
            TaskEvent::Blocked { .. } => "blocked",
            TaskEvent::Unblocked { .. } => "unblocked",
            TaskEvent::Reopened { .. } => "reopened",
        }
    }
}

// ── Envelope (header + payload as one JSONL line) ──────────────────

/// One JSONL line. Header carries provenance + ordering so replay can
/// reconstruct state independent of file order: events are sorted by
/// `(timestamp, instance, seq)` then folded.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEventEnvelope {
    pub schema_version: u32,
    /// Monotonic per-instance sequence — disambiguates events sharing
    /// a clock-second timestamp from the same emitter.
    pub seq: u64,
    /// RFC3339 UTC.
    pub timestamp: String,
    /// Which agent emitted this event.
    pub instance: InstanceName,
    pub event: TaskEvent,
}

// ── Folded board state (output of replay; not persisted) ───────────

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum TaskStatus {
    Open,
    Claimed,
    InProgress,
    Verified,
    Done,
    Cancelled,
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryEntry {
    pub seq: u64,
    pub timestamp: String,
    pub instance: InstanceName,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub priority: String,
    pub status: TaskStatus,
    pub owner: Option<InstanceName>,
    pub linked_prs: Vec<PrId>,
    pub block_reason: Option<String>,
    pub history: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct TaskBoardState {
    /// BTreeMap so iteration order is stable across processes — required
    /// for cross-process determinism invariant.
    pub tasks: BTreeMap<TaskId, TaskRecord>,
    /// High-water seq# observed per emitter. Used both for dedupe (replay
    /// idempotency: re-applying an envelope with `seq <= last_seen` is a
    /// no-op) and as the seed value when computing the next seq# in
    /// [`append`].
    pub last_seq_per_instance: BTreeMap<InstanceName, u64>,
    /// Total envelopes applied (post-dedupe). Useful for replay sanity
    /// asserts.
    pub events_folded: u64,
}

impl TaskBoardState {
    /// Apply one envelope. Returns `false` if the envelope was a duplicate
    /// (already-applied seq for this instance) and was skipped.
    ///
    /// **F3 (PR1 r2 dev-reviewer-2)** — replay-side application is
    /// intentionally permissive on illegal state transitions (e.g. a
    /// `Done` arriving on a task that never saw `Created`, or `Verified`
    /// after `Cancelled`). The sister emit layer — `tasks.rs` MCP handler
    /// in PR2 + sweep daemon in PR3 — validates `(current_status, event)`
    /// legitimacy BEFORE calling [`append`], so the log only ever holds
    /// transitions that were valid at emit time. Replay must accept any
    /// valid envelope from the log (including pre-validation history that
    /// may exist if validation rules ever tighten), while emit can refuse
    /// new transitions. The split is deliberate: replay determinism first,
    /// state-machine policy later.
    fn apply(&mut self, env: &TaskEventEnvelope) -> bool {
        let prev = self
            .last_seq_per_instance
            .get(&env.instance)
            .copied()
            .unwrap_or(0);
        if env.seq <= prev {
            // Idempotent skip — replay of a duplicated line produces the
            // same state as the original. This is the fundamental
            // invariant the storage layer depends on.
            return false;
        }
        self.last_seq_per_instance
            .insert(env.instance.clone(), env.seq);
        self.events_folded += 1;

        let kind = env.event.kind_str();
        let task_id = env.event.task_id().clone();
        let history_entry = HistoryEntry {
            seq: env.seq,
            timestamp: env.timestamp.clone(),
            instance: env.instance.clone(),
            kind,
        };

        match &env.event {
            TaskEvent::Created {
                title,
                description,
                priority,
                owner,
                ..
            } => {
                self.tasks
                    .entry(task_id.clone())
                    .or_insert_with(|| TaskRecord {
                        id: task_id,
                        title: title.clone(),
                        description: description.clone(),
                        priority: priority.clone(),
                        status: TaskStatus::Open,
                        owner: owner.clone(),
                        linked_prs: Vec::new(),
                        block_reason: None,
                        history: Vec::new(),
                    });
            }
            TaskEvent::Claimed { by, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Claimed;
                    t.owner = Some(by.clone());
                }
            }
            TaskEvent::InProgress { by, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::InProgress;
                    t.owner = Some(by.clone());
                }
            }
            TaskEvent::Verified { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Verified;
                }
            }
            TaskEvent::Done { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Done;
                }
            }
            TaskEvent::Cancelled { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Cancelled;
                }
            }
            TaskEvent::Linked { pr_id, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    if !t.linked_prs.contains(pr_id) {
                        t.linked_prs.push(*pr_id);
                    }
                }
            }
            TaskEvent::Blocked { reason, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Blocked;
                    t.block_reason = Some(reason.clone());
                }
            }
            TaskEvent::Unblocked { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    if t.status == TaskStatus::Blocked {
                        t.status = TaskStatus::Open;
                    }
                    t.block_reason = None;
                }
            }
            TaskEvent::Reopened { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Open;
                }
            }
        }
        if let Some(t) = self.tasks.get_mut(env.event.task_id()) {
            t.history.push(history_entry);
        }
        true
    }
}

// ── Append: single + batch ─────────────────────────────────────────

fn log_path(home: &Path) -> PathBuf {
    home.join(format!("{LOG_NAME}.jsonl"))
}

fn archive_dir(home: &Path) -> PathBuf {
    home.join("task_events_archive")
}

/// Append one event, returning the newly assigned monotonic seq#.
///
/// The seq is computed by tail-scanning the hot log under the same lock
/// as the write — concurrent appenders observe a totally-ordered seq
/// stream per instance.
pub fn append(home: &Path, instance: &InstanceName, event: TaskEvent) -> anyhow::Result<u64> {
    let seqs = append_batch(home, instance, vec![event])?;
    Ok(seqs.into_iter().next().unwrap_or(0))
}

/// Append multiple events as one fsync (F7 atomic-batch). All events
/// receive consecutive seq#s starting at the current high-water + 1 for
/// this emitter.
pub fn append_batch(
    home: &Path,
    instance: &InstanceName,
    events: Vec<TaskEvent>,
) -> anyhow::Result<Vec<u64>> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let instance = instance.clone();
    let count = events.len();
    let mut seqs: Vec<u64> = Vec::with_capacity(count);
    let now = chrono::Utc::now().to_rfc3339();

    crate::event_log::append_lines_under_lock(home, LOG_NAME, |log_path| {
        let start_seq = max_seq_for_instance(log_path, &instance)? + 1;
        let mut lines = Vec::with_capacity(count);
        for (i, event) in events.into_iter().enumerate() {
            let seq = start_seq + i as u64;
            seqs.push(seq);
            let envelope = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq,
                timestamp: now.clone(),
                instance: instance.clone(),
                event,
            };
            lines.push(serde_json::to_string(&envelope)?);
        }
        Ok(lines)
    })?;
    Ok(seqs)
}

/// Tail-scan the hot log for the highest seq# this instance has emitted.
/// Best-effort: malformed lines are skipped because [`replay`] is the
/// strict reader; here we just need the high-water mark.
fn max_seq_for_instance(log_path: &Path, instance: &InstanceName) -> anyhow::Result<u64> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut max = 0u64;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(env) = serde_json::from_str::<TaskEventEnvelope>(line) {
            if &env.instance == instance && env.seq > max {
                max = env.seq;
            }
        }
    }
    Ok(max)
}

// ── Replay: strict reader (forward-compat fail-closed) ─────────────

/// Fold the entire on-disk event history (archive + hot file) into a
/// `TaskBoardState`. Strict: any envelope whose `schema_version` exceeds
/// [`SCHEMA_VERSION`] aborts the replay (forward-compat fail-closed),
/// and any line that fails to deserialize as a known [`TaskEvent`]
/// variant aborts (per dev-reviewer-2 must-have: replay must NOT silently
/// skip unknown envelopes).
pub fn replay(home: &Path) -> anyhow::Result<TaskBoardState> {
    let mut envelopes: Vec<TaskEventEnvelope> = Vec::new();

    let archive_dir = archive_dir(home);
    if archive_dir.is_dir() {
        let mut archives: Vec<PathBuf> = std::fs::read_dir(&archive_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        archives.sort();
        for path in archives {
            read_envelopes_strict(&path, &mut envelopes)?;
        }
    }

    let log_path = log_path(home);
    if log_path.exists() {
        read_envelopes_strict(&log_path, &mut envelopes)?;
    }

    // Cross-process-deterministic ordering: same input → same fold output.
    // F2 (PR1 r2 dev-reviewer-2): RFC3339 strings are lexically-sortable
    // ONLY when they share UTC offset format. Today every emitter goes
    // through `chrono::Utc::now().to_rfc3339()` (Z suffix), so lexical
    // would be safe — but a future emitter passing a non-UTC offset
    // (e.g. `+09:00`) would silently drift state across replay readers,
    // a worst-class bug. Parse to absolute nanoseconds-since-epoch so
    // ordering is chronological regardless of source offset. Unparseable
    // timestamps fold to 0 (placed first); they would also fail the
    // strict reader, so reaching this branch implies a programmer-
    // injected sentinel rather than on-disk data.
    envelopes.sort_by(|a, b| {
        let a_ts = chrono::DateTime::parse_from_rfc3339(&a.timestamp)
            .map(|d| d.timestamp_nanos_opt().unwrap_or(0))
            .unwrap_or(0);
        let b_ts = chrono::DateTime::parse_from_rfc3339(&b.timestamp)
            .map(|d| d.timestamp_nanos_opt().unwrap_or(0))
            .unwrap_or(0);
        a_ts.cmp(&b_ts)
            .then_with(|| a.instance.0.cmp(&b.instance.0))
            .then_with(|| a.seq.cmp(&b.seq))
    });

    let mut state = TaskBoardState::default();
    for env in &envelopes {
        state.apply(env);
    }
    Ok(state)
}

fn read_envelopes_strict(path: &Path, out: &mut Vec<TaskEventEnvelope>) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    for (lineno, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let env: TaskEventEnvelope = serde_json::from_str(line).map_err(|e| {
            anyhow::anyhow!(
                "{}:{}: replay aborts on unparseable envelope (forward-compat fail-closed): {e}",
                path.display(),
                lineno + 1
            )
        })?;
        if env.schema_version > SCHEMA_VERSION {
            anyhow::bail!(
                "{}:{}: schema_version {} > supported {} (forward-compat fail-closed)",
                path.display(),
                lineno + 1,
                env.schema_version,
                SCHEMA_VERSION
            );
        }
        out.push(env);
    }
    Ok(())
}

// ── Compaction ─────────────────────────────────────────────────────

/// Archive all but the last [`COMPACTION_KEEP`] envelopes from the hot
/// log into a timestamped file under `task_events_archive/`. Idempotent:
/// short-circuits when the hot log already fits the threshold. Holds the
/// same lock as [`append`] so concurrent appenders see a consistent
/// hot-file at all times.
pub fn compact(home: &Path) -> anyhow::Result<()> {
    let log_path = log_path(home);
    if !log_path.exists() {
        return Ok(());
    }
    let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();

    crate::event_log::append_lines_under_lock(home, LOG_NAME, |log_path| {
        let content = std::fs::read_to_string(log_path)?;
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() <= COMPACTION_KEEP {
            return Ok(Vec::new());
        }
        let split = lines.len() - COMPACTION_KEEP;
        let archived: String = lines[..split].iter().map(|l| format!("{l}\n")).collect();
        let kept: String = lines[split..].iter().map(|l| format!("{l}\n")).collect();

        let archive = archive_dir(
            log_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("log_path has no parent"))?,
        );
        std::fs::create_dir_all(&archive)?;
        let archive_path = archive.join(format!("task_events.{suffix}.jsonl"));
        crate::store::atomic_write(&archive_path, archived.as_bytes())?;
        crate::store::atomic_write(log_path, kept.as_bytes())?;
        // We've already rewritten the hot file — return no extra lines
        // to append.
        Ok(Vec::new())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-task-events-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_event(id: &str) -> TaskEvent {
        TaskEvent::Created {
            task_id: id.into(),
            title: format!("title for {id}"),
            description: "desc".to_string(),
            priority: "normal".to_string(),
            owner: None,
        }
    }

    #[test]
    fn append_assigns_monotonic_seq_per_instance() {
        let home = tmp_home("seq");
        let inst = InstanceName::from("dev-impl-1");
        let s1 = append(&home, &inst, sample_event("t-A")).unwrap();
        let s2 = append(&home, &inst, sample_event("t-B")).unwrap();
        let s3 = append(&home, &inst, sample_event("t-C")).unwrap();
        assert_eq!((s1, s2, s3), (1, 2, 3));
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn append_batch_atomic_consecutive_seqs() {
        let home = tmp_home("batch");
        let inst = InstanceName::from("a");
        let seqs = append_batch(
            &home,
            &inst,
            vec![
                sample_event("t-1"),
                sample_event("t-2"),
                sample_event("t-3"),
            ],
        )
        .unwrap();
        assert_eq!(seqs, vec![1, 2, 3]);
        let content = fs::read_to_string(home.join("task_events.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 3);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn seq_is_per_instance_isolated() {
        let home = tmp_home("isolate");
        let a = InstanceName::from("agent-a");
        let b = InstanceName::from("agent-b");
        let _ = append(&home, &a, sample_event("t-A1")).unwrap();
        let _ = append(&home, &a, sample_event("t-A2")).unwrap();
        let s_b1 = append(&home, &b, sample_event("t-B1")).unwrap();
        assert_eq!(s_b1, 1, "agent-b's seq is independent of agent-a");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn replay_folds_basic_lifecycle() {
        let home = tmp_home("fold");
        let inst = InstanceName::from("u");
        append(&home, &inst, sample_event("t-X")).unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: "t-X".into(),
                by: "agent".into(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Done {
                task_id: "t-X".into(),
                by: "agent".into(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: Some("ok".into()),
                },
            },
        )
        .unwrap();
        let state = replay(&home).unwrap();
        let task = state.tasks.get(&TaskId::from("t-X")).unwrap();
        assert_eq!(task.status, TaskStatus::Done);
        assert_eq!(task.history.len(), 3);
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #4 — a v(N) reader rejects v(N+k)
    /// envelopes (k>0) rather than dropping unknown fields. Operators
    /// running an older binary against a newer log fail loud.
    #[test]
    fn invariant_4_forward_compat_fail_closed() {
        let home = tmp_home("future");
        let log = home.join("task_events.jsonl");
        // Hand-craft a v999 envelope.
        let line = serde_json::json!({
            "schema_version": 999,
            "seq": 1,
            "timestamp": "2026-04-27T00:00:00Z",
            "instance": "test",
            "event": {"kind": "Unblocked", "task_id": "t-X"}
        });
        fs::write(&log, format!("{line}\n")).unwrap();
        let err = replay(&home).expect_err("must fail-closed on future schema");
        assert!(
            err.to_string().contains("forward-compat fail-closed"),
            "got: {err}"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn replay_rejects_unknown_event_variant() {
        let home = tmp_home("unknown");
        let log = home.join("task_events.jsonl");
        let line = serde_json::json!({
            "schema_version": 1,
            "seq": 1,
            "timestamp": "2026-04-27T00:00:00Z",
            "instance": "test",
            "event": {"kind": "TotallyMadeUpVariant", "task_id": "t-X"}
        });
        fs::write(&log, format!("{line}\n")).unwrap();
        let err = replay(&home).expect_err("must fail-closed on unknown variant");
        assert!(err.to_string().contains("replay aborts"), "got: {err}");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compact_archives_older_than_keep_threshold() {
        let home = tmp_home("compact");
        let inst = InstanceName::from("u");
        // Synthesise lines past the threshold without actually appending
        // 10001 events (slow). Instead bypass append, write directly.
        let log = home.join("task_events.jsonl");
        let mut lines = String::new();
        for i in 1..=(COMPACTION_KEEP + 5) {
            let env = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: i as u64,
                timestamp: format!("2026-04-27T{:02}:00:00Z", i % 24),
                instance: inst.clone(),
                event: TaskEvent::Unblocked {
                    task_id: format!("t-{i}").as_str().into(),
                },
            };
            lines.push_str(&serde_json::to_string(&env).unwrap());
            lines.push('\n');
        }
        fs::write(&log, lines).unwrap();
        compact(&home).unwrap();
        let kept = fs::read_to_string(&log).unwrap();
        assert_eq!(kept.lines().count(), COMPACTION_KEEP);
        let arc = archive_dir(&home);
        let entries: Vec<_> = fs::read_dir(&arc).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "exactly one archive file expected");
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #1 — re-applying the same envelope
    /// (e.g. duplicated line in a corrupted log) folds to identical state
    /// as applying it once. Implemented via the `seq <= last_seen` skip
    /// in [`TaskBoardState::apply`].
    #[test]
    fn invariant_1_idempotency() {
        let home = tmp_home("dedupe");
        let inst = InstanceName::from("u");
        append(&home, &inst, sample_event("t-D")).unwrap();
        // Manually duplicate the line to simulate a corrupted log.
        let log = home.join("task_events.jsonl");
        let content = fs::read_to_string(&log).unwrap();
        fs::write(&log, format!("{content}{content}")).unwrap();
        let state = replay(&home).unwrap();
        // Idempotency invariant: two copies of the same envelope fold to
        // identical state as one copy.
        assert_eq!(state.events_folded, 1);
        let task = state.tasks.get(&TaskId::from("t-D")).unwrap();
        assert_eq!(task.history.len(), 1);
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #2 — two readers fed the identical
    /// log produce bit-identical state. Stronger than ordering: the test
    /// JSON-serialises the entire fold and asserts equality, so any field
    /// drift (timestamp, history shape, per-instance seq) surfaces.
    #[test]
    fn invariant_2_cross_process_determinism() {
        let home = tmp_home("xproc");
        let inst = InstanceName::from("u");
        let _ = append(&home, &inst, sample_event("t-1")).unwrap();
        let _ = append(&home, &inst, sample_event("t-2")).unwrap();
        let _ = append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: "t-1".into(),
                by: "u".into(),
            },
        )
        .unwrap();
        let s1 = replay(&home).unwrap();
        let s2 = replay(&home).unwrap();
        let j1 = serde_json::to_string(&s1).unwrap();
        let j2 = serde_json::to_string(&s2).unwrap();
        assert_eq!(j1, j2);
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #3 — back-compat: a v(N) reader
    /// successfully parses v(N) envelopes (round-trip). When v2 ships,
    /// extend this to assert the v2 reader still parses v1 envelopes.
    #[test]
    fn invariant_3_back_compat_v1_reader_parses_v1_envelope() {
        let home = tmp_home("backcompat");
        let inst = InstanceName::from("u");
        let _ = append(&home, &inst, sample_event("t-BC")).unwrap();
        let state = replay(&home).unwrap();
        assert!(state.tasks.contains_key(&TaskId::from("t-BC")));
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #6 — replaying any prefix of the log
    /// (events 1..N for every N) yields a valid state with predictable
    /// status transitions. Future Phase 3 backfill emits dry-run snapshots
    /// at arbitrary cursor points; this asserts every cursor is safe.
    #[test]
    fn invariant_6_snapshot_prefix_is_valid_state() {
        let home = tmp_home("prefix");
        let inst = InstanceName::from("u");
        append(&home, &inst, sample_event("t-P")).unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: "t-P".into(),
                by: "u".into(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Done {
                task_id: "t-P".into(),
                by: "u".into(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
        )
        .unwrap();

        let log = home.join("task_events.jsonl");
        let full = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = full.lines().collect();
        assert_eq!(lines.len(), 3);

        for n in 1..=lines.len() {
            let prefix: String = lines[..n].iter().map(|l| format!("{l}\n")).collect();
            fs::write(&log, &prefix).unwrap();
            let state = replay(&home).unwrap_or_else(|_| panic!("prefix len {n} invalid"));
            assert_eq!(state.events_folded, n as u64);
            let task = state.tasks.get(&TaskId::from("t-P")).unwrap();
            let expected = match n {
                1 => TaskStatus::Open,
                2 => TaskStatus::Claimed,
                3 => TaskStatus::Done,
                _ => unreachable!(),
            };
            assert_eq!(task.status, expected, "prefix len {n}");
        }
        fs::remove_dir_all(&home).ok();
    }

    /// Replay-determinism invariant #7 — N readers on the same log all
    /// observe identical state. Defends the "operator runs `task list`
    /// and `task get` concurrently with daemon's MCP handler" workflow.
    #[test]
    fn invariant_7_concurrent_reader_coherence() {
        use std::sync::Arc;
        let home = Arc::new(tmp_home("concurrent"));
        let inst = InstanceName::from("u");
        for i in 1..=20 {
            append(&home, &inst, sample_event(&format!("t-{i}"))).unwrap();
        }
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let h = Arc::clone(&home);
                std::thread::spawn(move || serde_json::to_string(&replay(&h).unwrap()).unwrap())
            })
            .collect();
        let results: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let first = &results[0];
        for r in &results[1..] {
            assert_eq!(r, first, "concurrent readers must observe identical state");
        }
        fs::remove_dir_all(&*home).ok();
    }

    /// F2 (PR1 r2) — mixed-timezone envelopes must sort chronologically,
    /// not lexically. `+09:00` carries an earlier absolute instant than
    /// `Z`, even though the lexical comparison (`+` ≈ 0x2B vs `Z` ≈ 0x5A)
    /// goes the other way. This test pins the regression: if the sort
    /// ever reverts to string compare, the asserted Done-status flips.
    #[test]
    fn replay_sorts_chronologically_across_timezone_offsets() {
        let home = tmp_home("tz");
        let log = home.join("task_events.jsonl");
        // Event A: 2026-04-27T01:00:00+09:00 == 2026-04-26T16:00:00Z
        // Event B: 2026-04-27T00:00:00Z
        // Chronological: A precedes B by 8 hours.
        // Lexical (broken): "2026-04-27T00..." < "2026-04-27T01..." → B before A.
        let env_a = TaskEventEnvelope {
            schema_version: 1,
            seq: 1,
            timestamp: "2026-04-27T01:00:00+09:00".into(),
            instance: InstanceName::from("u"),
            event: sample_event("t-TZ"),
        };
        let env_b = TaskEventEnvelope {
            schema_version: 1,
            seq: 2,
            timestamp: "2026-04-27T00:00:00Z".into(),
            instance: InstanceName::from("u"),
            event: TaskEvent::Done {
                task_id: "t-TZ".into(),
                by: "u".into(),
                source: DoneSource::OperatorManual {
                    authored_at: "2026-04-27T00:00:00Z".into(),
                    result: None,
                },
            },
        };
        // Write in the wrong (lexical) order — replay must still produce
        // the chronologically-correct fold (Created → Done).
        let mut content = String::new();
        content.push_str(&serde_json::to_string(&env_b).unwrap());
        content.push('\n');
        content.push_str(&serde_json::to_string(&env_a).unwrap());
        content.push('\n');
        fs::write(&log, content).unwrap();

        let state = replay(&home).unwrap();
        let task = state.tasks.get(&TaskId::from("t-TZ")).unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Done,
            "chronological sort: Created (16:00Z) precedes Done (00:00Z next-UTC-day) → final Done"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn replay_ordering_is_deterministic() {
        let home = tmp_home("order");
        let log = home.join("task_events.jsonl");
        // Hand-shuffle order on disk; replay must sort by (timestamp,
        // instance, seq) and produce a stable fold regardless.
        let envs = [
            TaskEventEnvelope {
                schema_version: 1,
                seq: 2,
                timestamp: "2026-04-27T00:00:02Z".into(),
                instance: InstanceName::from("u"),
                event: TaskEvent::Claimed {
                    task_id: "t-O".into(),
                    by: "u".into(),
                },
            },
            TaskEventEnvelope {
                schema_version: 1,
                seq: 1,
                timestamp: "2026-04-27T00:00:01Z".into(),
                instance: InstanceName::from("u"),
                event: sample_event("t-O"),
            },
        ];
        let mut content = String::new();
        for e in &envs {
            content.push_str(&serde_json::to_string(e).unwrap());
            content.push('\n');
        }
        fs::write(&log, content).unwrap();
        let state = replay(&home).unwrap();
        let task = state.tasks.get(&TaskId::from("t-O")).unwrap();
        // Created applied before Claimed → final status Claimed.
        assert_eq!(task.status, TaskStatus::Claimed);
        fs::remove_dir_all(&home).ok();
    }
}
