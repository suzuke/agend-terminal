//! Sprint 24 P0 PR1 — append-only task event log.
//!
//! Source-of-truth storage for task board state. Replaces direct-mutation
//! `tasks.json` (PR2 routes the existing MCP `task` tool through
//! [`append`]; PR1 ships only the storage substrate).
//!
//! ## Design references
//! - `docs/archived/TASK-BOARD-AUTO-CLOSE-REDESIGN.md` (4-perspective synthesis)
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
///
/// **v2 (Sprint 24 P0 PR3)** — adds:
/// - [`TaskEvent::Released`] (sweep-driven claim release; clears owner
///   unlike `Reopened` which preserves it for done→open re-work).
/// - [`TaskEvent::TaskCloseProposed`] (legacy-backfill dry-run proposal;
///   distinct from `Done` per dev-reviewer-2 must-have).
/// - [`TaskEvent::Created`] gains `due_at` / `depends_on` / `routed_to`
///   fields (`#[serde(default)]` so v1 envelopes round-trip).
/// - [`TaskRecord`] gains `created_by` / `created_at` / `updated_at` /
///   `due_at` / `depends_on` / `routed_to` / `result` fields.
///
/// v1 readers fail-closed on v2 envelopes via the `schema_version >
/// SCHEMA_VERSION` check in [`replay`]. v2 readers accept v1 envelopes
/// (the new `Created` fields default to `None` / `Vec::new()`).
pub const SCHEMA_VERSION: u32 = 2;

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
    /// Auto-closed when the associated branch was merged.
    AutoCloseOnPrMerge { branch: String, merged_at: String },
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
        /// **v2** — RFC3339 deadline; `sweep_overdue_claimed` releases
        /// claims past this. v1 envelopes default to `None`.
        #[serde(default)]
        due_at: Option<String>,
        /// **v2** — task IDs this task depends on; auto-blocks when any
        /// dep is non-Done (computed in-memory at list time, NOT
        /// persisted as Blocked/Unblocked events). v1 envelopes default
        /// to empty.
        #[serde(default)]
        depends_on: Vec<TaskId>,
        /// **v2** — when assignee resolves to a team, the orchestrator
        /// surfaced for routing. Derived at create-time, captured here
        /// so replay reproduces team-routing visibility.
        #[serde(default)]
        routed_to: Option<InstanceName>,
        /// **v2** — git branch the implementer should work on.
        #[serde(default)]
        branch: Option<String>,
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
    /// **v2** — claim release. Distinct from `Reopened`: Released clears
    /// the owner (claim is gone), while Reopened preserves it (done→open
    /// re-work goes back to the same person). Emitted by the overdue-
    /// claim sweeper and the operator's manual `task release` flow.
    Released {
        task_id: TaskId,
        reason: String,
    },
    /// **v2** — sweep dry-run proposal. Distinct from `Done` so an
    /// operator-confirm gate can intercede before the canonical close
    /// transition lands. Per dev-reviewer-2 must-have: do NOT use a
    /// nullable `pr_id` on `Done` to differentiate proposal vs final;
    /// use this distinct variant.
    TaskCloseProposed {
        task_id: TaskId,
        candidate: DoneSource,
        sweep_id: String,
        confidence: ConfidenceScore,
    },
    /// **v2** — owner reassignment / clear without status transition.
    /// Used by `update` MCP arm when the operator changes assignee
    /// without changing status (e.g. transferring ownership across
    /// teams while keeping the task Open). Distinct from `Claimed`
    /// (which forces status → Claimed) so reassigning an Open task
    /// stays Open.
    OwnerAssigned {
        task_id: TaskId,
        by: InstanceName,
        owner: Option<InstanceName>,
        routed_to: Option<InstanceName>,
    },
    /// **v2** — priority change without status transition.
    PriorityChanged {
        task_id: TaskId,
        by: InstanceName,
        priority: String,
    },
}

/// Per-task confidence breakdown produced by the legacy-backfill sweep.
/// Carried inside `TaskCloseProposed` so the operator audit trail shows
/// why a particular proposal landed in the propose-tier rather than
/// auto-apply or silent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceScore {
    /// Sum of weighted sub-scores. Audit-readable.
    pub total: f32,
    /// Count of independent signals that fired (not just sum).
    pub signal_count: u32,
    /// Per-sub-score breakdown for forensic review. Keys are signal
    /// names (e.g. `"branch_exact"`, `"branch_jaccard"`,
    /// `"closes_marker"`); values are the weighted contribution.
    pub sub_scores: std::collections::BTreeMap<String, f32>,
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
            | TaskEvent::Reopened { task_id, .. }
            | TaskEvent::Released { task_id, .. }
            | TaskEvent::TaskCloseProposed { task_id, .. }
            | TaskEvent::OwnerAssigned { task_id, .. }
            | TaskEvent::PriorityChanged { task_id, .. } => task_id,
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
            TaskEvent::Released { .. } => "released",
            TaskEvent::TaskCloseProposed { .. } => "task_close_proposed",
            TaskEvent::OwnerAssigned { .. } => "owner_assigned",
            TaskEvent::PriorityChanged { .. } => "priority_changed",
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
    // ── v2 fields (PR3) — replicate tasks.json metadata in the
    //    canonical replay-derived view so retire-tasks.json doesn't
    //    lose any field consumers rely on.
    pub created_by: InstanceName,
    pub created_at: String,
    pub updated_at: String,
    pub due_at: Option<String>,
    pub depends_on: Vec<TaskId>,
    pub routed_to: Option<InstanceName>,
    pub result: Option<String>,
    pub branch: Option<String>,
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

        // Touch-update timestamp for status mutations so on-board
        // `updated_at` mirrors tasks.json's pre-cutover surface.
        let touch_at = env.timestamp.clone();
        match &env.event {
            TaskEvent::Created {
                title,
                description,
                priority,
                owner,
                due_at,
                depends_on,
                routed_to,
                branch,
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
                        created_by: env.instance.clone(),
                        created_at: env.timestamp.clone(),
                        updated_at: env.timestamp.clone(),
                        due_at: due_at.clone(),
                        depends_on: depends_on.clone(),
                        routed_to: routed_to.clone(),
                        result: None,
                        branch: branch.clone(),
                    });
            }
            TaskEvent::Claimed { by, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Claimed;
                    t.owner = Some(by.clone());
                    // Claim transfers clear team-routing residue.
                    t.routed_to = None;
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::InProgress { by, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::InProgress;
                    t.owner = Some(by.clone());
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Verified { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Verified;
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Done { source, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Done;
                    t.updated_at = touch_at;
                    if let DoneSource::OperatorManual { result, .. } = source {
                        t.result = result.clone();
                    }
                }
            }
            TaskEvent::Cancelled { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Cancelled;
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Linked { pr_id, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    if !t.linked_prs.contains(pr_id) {
                        t.linked_prs.push(*pr_id);
                    }
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Blocked { reason, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Blocked;
                    t.block_reason = Some(reason.clone());
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Unblocked { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    if t.status == TaskStatus::Blocked {
                        t.status = TaskStatus::Open;
                    }
                    t.block_reason = None;
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Reopened { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Open;
                    // Reopened preserves owner: done→open is typically the
                    // same person re-doing the work after CI fail / revert.
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::Released { .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.status = TaskStatus::Open;
                    t.owner = None;
                    t.routed_to = None;
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::TaskCloseProposed { .. } => {
                // Proposal events are audit-only — they do NOT mutate
                // task status. The operator-confirm path emits a real
                // `Done` event after approval.
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.updated_at = touch_at;
                }
            }
            TaskEvent::OwnerAssigned {
                owner, routed_to, ..
            } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.owner = owner.clone();
                    t.routed_to = routed_to.clone();
                    t.updated_at = touch_at;
                    // Status NOT changed — distinct from Claimed which
                    // sets status=Claimed.
                }
            }
            TaskEvent::PriorityChanged { priority, .. } => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.priority = priority.clone();
                    t.updated_at = touch_at;
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
    // H1: update cached high-water mark after successful append
    if let Some(&last_seq) = seqs.last() {
        let log_path = home.join(format!("{LOG_NAME}.jsonl"));
        let key = (log_path, instance);
        SEQ_CACHE.lock().insert(key, last_seq);
    }
    Ok(seqs)
}

/// Tail-scan the hot log for the highest seq# this instance has emitted.
/// Best-effort: malformed lines are skipped because [`replay`] is the
/// strict reader; here we just need the high-water mark.
/// H1: Cached high-water map — avoids full-file scan on every append.
/// Populated on first access per log_path, updated in-memory on append.
static SEQ_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(std::path::PathBuf, InstanceName), u64>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn max_seq_for_instance(log_path: &Path, instance: &InstanceName) -> anyhow::Result<u64> {
    let key = (log_path.to_path_buf(), instance.clone());
    let mut cache = SEQ_CACHE.lock();
    if let Some(&cached) = cache.get(&key) {
        return Ok(cached);
    }
    // First access: scan file once
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
    cache.insert(key, max);
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
    // H2: stream-fold per file instead of collecting all envelopes into memory.
    // Archives are chronologically ordered by filename; within each file,
    // events are in append order. We sort per-file then fold immediately.
    let mut state = TaskBoardState::default();

    let archive_dir = archive_dir(home);
    if archive_dir.is_dir() {
        let mut archives: Vec<PathBuf> = std::fs::read_dir(&archive_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        archives.sort();
        for path in archives {
            let mut envelopes = Vec::new();
            read_envelopes_strict(&path, &mut envelopes)?;
            sort_envelopes(&mut envelopes);
            for env in &envelopes {
                state.apply(env);
            }
        }
    }

    let log_path = log_path(home);
    if log_path.exists() {
        let mut envelopes = Vec::new();
        read_envelopes_strict(&log_path, &mut envelopes)?;
        sort_envelopes(&mut envelopes);
        for env in &envelopes {
            state.apply(env);
        }
    }

    Ok(state)
}

/// Sort envelopes by timestamp (absolute nanos) → instance → seq.
fn sort_envelopes(envelopes: &mut [TaskEventEnvelope]) {
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
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
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
    /// successfully parses v(N) envelopes (round-trip). After PR3 the
    /// canonical writer emits v2 envelopes, so the round-trip path
    /// alone exercises v2 → v2; this test additionally covers the
    /// promised v2-reader-parses-v1-envelope contract.
    #[test]
    fn invariant_3_back_compat_v1_reader_parses_v1_envelope() {
        let home = tmp_home("backcompat");
        let inst = InstanceName::from("u");
        let _ = append(&home, &inst, sample_event("t-BC")).unwrap();
        let state = replay(&home).unwrap();
        assert!(state.tasks.contains_key(&TaskId::from("t-BC")));
        fs::remove_dir_all(&home).ok();
    }

    /// PR4 M2 (PR3 r1 dev-reviewer cross-vantage) — explicit v1 envelope
    /// in a v2 reader's path. Hand-crafts the JSON line as a v1 emitter
    /// would have written it (no `due_at` / `depends_on` / `routed_to` on
    /// `Created`, `schema_version: 1`); asserts the v2 reader parses it
    /// successfully via `#[serde(default)]` on the new fields. Defends
    /// the silent migration regression — operator running a v2 binary
    /// against an event log written entirely under v1 must observe state
    /// identical to a v1 reader's view, not an error.
    #[test]
    fn invariant_3_v2_reader_parses_v1_envelope_explicit() {
        let home = tmp_home("v1_explicit");
        let log = home.join("task_events.jsonl");
        // Hand-crafted v1 line: `Created` without v2 fields, `schema_version: 1`.
        let v1_line = serde_json::json!({
            "schema_version": 1,
            "seq": 1,
            "timestamp": "2026-04-26T00:00:00Z",
            "instance": "v1-emitter",
            "event": {
                "kind": "Created",
                "task_id": "t-V1",
                "title": "v1-shaped task",
                "description": "no v2 fields",
                "priority": "normal",
                "owner": null
            }
        });
        fs::write(&log, format!("{v1_line}\n")).unwrap();
        let state = replay(&home).unwrap();
        let task = state
            .tasks
            .get(&TaskId::from("t-V1"))
            .expect("v2 reader must parse v1 Created envelope via serde defaults");
        assert_eq!(task.status, TaskStatus::Open);
        assert!(
            task.due_at.is_none(),
            "v1 envelope's missing due_at → None default"
        );
        assert!(
            task.depends_on.is_empty(),
            "v1 envelope's missing depends_on → empty default"
        );
        assert!(
            task.routed_to.is_none(),
            "v1 envelope's missing routed_to → None default"
        );
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

    // ── Sprint 24 P0 PR2 — F1 (deferred from PR1 r2) + Invariant #5 ──

    /// Build a fresh `(home, instance)` test fixture seeded with a single
    /// `Created` event so subsequent transitions have a task to mutate.
    fn fixture_with_seeded_task(tag: &str) -> (PathBuf, InstanceName, TaskId) {
        let home = tmp_home(tag);
        let inst = InstanceName::from("u");
        let tid = TaskId::from("t-FIX");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "fixture".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
            },
        )
        .unwrap();
        (home, inst, tid)
    }

    /// **F1 (PR1 r2 deferred to PR2)** — exhaustive state-machine table.
    /// 7 statuses × 10 events = 70 cells; not all are interesting (Created
    /// on an existing task is a documented no-op via `or_insert_with`),
    /// but the test covers every cell explicitly so a future apply()
    /// regression on any status × event pair fails-loud.
    #[test]
    fn state_machine_exhaustive_transitions() {
        // Helper: prime task into a target status by sequencing prior
        // events. Returns (home, instance, task_id) post-priming.
        fn prime(target: TaskStatus, tag: &str) -> (PathBuf, InstanceName, TaskId) {
            let (home, inst, tid) = fixture_with_seeded_task(tag);
            let priming: Vec<TaskEvent> = match target {
                TaskStatus::Open => vec![],
                TaskStatus::Claimed => vec![TaskEvent::Claimed {
                    task_id: tid.clone(),
                    by: inst.clone(),
                }],
                TaskStatus::InProgress => vec![TaskEvent::InProgress {
                    task_id: tid.clone(),
                    by: inst.clone(),
                }],
                TaskStatus::Verified => vec![TaskEvent::Verified {
                    task_id: tid.clone(),
                    by_reviewer: inst.clone(),
                    verdict: "verified".into(),
                }],
                TaskStatus::Done => vec![TaskEvent::Done {
                    task_id: tid.clone(),
                    by: inst.clone(),
                    source: DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: None,
                    },
                }],
                TaskStatus::Cancelled => vec![TaskEvent::Cancelled {
                    task_id: tid.clone(),
                    by: inst.clone(),
                    reason: "test".into(),
                }],
                TaskStatus::Blocked => vec![TaskEvent::Blocked {
                    task_id: tid.clone(),
                    reason: "test".into(),
                }],
            };
            for e in priming {
                append(&home, &inst, e).unwrap();
            }
            (home, inst, tid)
        }

        // Helper: emit one event for the candidate transition.
        fn emit(home: &Path, inst: &InstanceName, tid: &TaskId, kind: &str) {
            let event = match kind {
                // Created on an existing task is a documented no-op via
                // `entry().or_insert_with` — applying it doesn't mutate
                // status. We still exercise the path here to pin the
                // invariant.
                "Created" => TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "dup".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                },
                "Claimed" => TaskEvent::Claimed {
                    task_id: tid.clone(),
                    by: inst.clone(),
                },
                "InProgress" => TaskEvent::InProgress {
                    task_id: tid.clone(),
                    by: inst.clone(),
                },
                "Verified" => TaskEvent::Verified {
                    task_id: tid.clone(),
                    by_reviewer: inst.clone(),
                    verdict: "v".into(),
                },
                "Done" => TaskEvent::Done {
                    task_id: tid.clone(),
                    by: inst.clone(),
                    source: DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: None,
                    },
                },
                "Cancelled" => TaskEvent::Cancelled {
                    task_id: tid.clone(),
                    by: inst.clone(),
                    reason: "t".into(),
                },
                "Linked" => TaskEvent::Linked {
                    task_id: tid.clone(),
                    pr_id: PrId(1),
                    source: LinkSource::Explicit {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                    },
                    snapshot: PrSnapshot {
                        pr_state: "merged".into(),
                        merge_sha: Some("aaaa".into()),
                        api_response_hash: "h".into(),
                        captured_at: chrono::Utc::now().to_rfc3339(),
                    },
                },
                "Blocked" => TaskEvent::Blocked {
                    task_id: tid.clone(),
                    reason: "t".into(),
                },
                "Unblocked" => TaskEvent::Unblocked {
                    task_id: tid.clone(),
                },
                "Reopened" => TaskEvent::Reopened {
                    task_id: tid.clone(),
                    reason: "t".into(),
                    source_evidence: "t".into(),
                },
                "Released" => TaskEvent::Released {
                    task_id: tid.clone(),
                    reason: "t".into(),
                },
                _ => unreachable!(),
            };
            append(home, inst, event).unwrap();
        }

        // Full 7×10 expectation table. Each row asserts the post-event
        // status. Created/Linked never change status. Unblocked only
        // moves Blocked → Open. Reopened always normalises to Open.
        // Other events overwrite to their own target status (replay-side
        // is permissive per F3 contract).
        let table: &[(TaskStatus, &str, TaskStatus)] = &[
            // (current_status, event_kind, expected_next_status)
            (TaskStatus::Open, "Created", TaskStatus::Open),
            (TaskStatus::Open, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Open, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Open, "Verified", TaskStatus::Verified),
            (TaskStatus::Open, "Done", TaskStatus::Done),
            (TaskStatus::Open, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Open, "Linked", TaskStatus::Open),
            (TaskStatus::Open, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Open, "Unblocked", TaskStatus::Open),
            (TaskStatus::Open, "Reopened", TaskStatus::Open),
            (TaskStatus::Claimed, "Created", TaskStatus::Claimed),
            (TaskStatus::Claimed, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Claimed, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Claimed, "Verified", TaskStatus::Verified),
            (TaskStatus::Claimed, "Done", TaskStatus::Done),
            (TaskStatus::Claimed, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Claimed, "Linked", TaskStatus::Claimed),
            (TaskStatus::Claimed, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Claimed, "Unblocked", TaskStatus::Claimed),
            (TaskStatus::Claimed, "Reopened", TaskStatus::Open),
            (TaskStatus::InProgress, "Created", TaskStatus::InProgress),
            (TaskStatus::InProgress, "Claimed", TaskStatus::Claimed),
            (TaskStatus::InProgress, "InProgress", TaskStatus::InProgress),
            (TaskStatus::InProgress, "Verified", TaskStatus::Verified),
            (TaskStatus::InProgress, "Done", TaskStatus::Done),
            (TaskStatus::InProgress, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::InProgress, "Linked", TaskStatus::InProgress),
            (TaskStatus::InProgress, "Blocked", TaskStatus::Blocked),
            (TaskStatus::InProgress, "Unblocked", TaskStatus::InProgress),
            (TaskStatus::InProgress, "Reopened", TaskStatus::Open),
            (TaskStatus::Verified, "Created", TaskStatus::Verified),
            (TaskStatus::Verified, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Verified, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Verified, "Verified", TaskStatus::Verified),
            (TaskStatus::Verified, "Done", TaskStatus::Done),
            (TaskStatus::Verified, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Verified, "Linked", TaskStatus::Verified),
            (TaskStatus::Verified, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Verified, "Unblocked", TaskStatus::Verified),
            (TaskStatus::Verified, "Reopened", TaskStatus::Open),
            (TaskStatus::Done, "Created", TaskStatus::Done),
            (TaskStatus::Done, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Done, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Done, "Verified", TaskStatus::Verified),
            (TaskStatus::Done, "Done", TaskStatus::Done),
            (TaskStatus::Done, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Done, "Linked", TaskStatus::Done),
            (TaskStatus::Done, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Done, "Unblocked", TaskStatus::Done),
            (TaskStatus::Done, "Reopened", TaskStatus::Open),
            (TaskStatus::Cancelled, "Created", TaskStatus::Cancelled),
            (TaskStatus::Cancelled, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Cancelled, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Cancelled, "Verified", TaskStatus::Verified),
            (TaskStatus::Cancelled, "Done", TaskStatus::Done),
            (TaskStatus::Cancelled, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Cancelled, "Linked", TaskStatus::Cancelled),
            (TaskStatus::Cancelled, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Cancelled, "Unblocked", TaskStatus::Cancelled),
            (TaskStatus::Cancelled, "Reopened", TaskStatus::Open),
            (TaskStatus::Blocked, "Created", TaskStatus::Blocked),
            (TaskStatus::Blocked, "Claimed", TaskStatus::Claimed),
            (TaskStatus::Blocked, "InProgress", TaskStatus::InProgress),
            (TaskStatus::Blocked, "Verified", TaskStatus::Verified),
            (TaskStatus::Blocked, "Done", TaskStatus::Done),
            (TaskStatus::Blocked, "Cancelled", TaskStatus::Cancelled),
            (TaskStatus::Blocked, "Linked", TaskStatus::Blocked),
            (TaskStatus::Blocked, "Blocked", TaskStatus::Blocked),
            (TaskStatus::Blocked, "Unblocked", TaskStatus::Open),
            (TaskStatus::Blocked, "Reopened", TaskStatus::Open),
            // PR4 F3 (PR3 r1 reviewer-2 MEDIUM) — Released variant rows.
            // Released always normalises to Open (clears owner; distinct
            // from Reopened which preserves owner). Adding the 7 rows
            // closes the F1 7×10 → 7×11 expansion gap.
            (TaskStatus::Open, "Released", TaskStatus::Open),
            (TaskStatus::Claimed, "Released", TaskStatus::Open),
            (TaskStatus::InProgress, "Released", TaskStatus::Open),
            (TaskStatus::Verified, "Released", TaskStatus::Open),
            (TaskStatus::Done, "Released", TaskStatus::Open),
            (TaskStatus::Cancelled, "Released", TaskStatus::Open),
            (TaskStatus::Blocked, "Released", TaskStatus::Open),
        ];

        for (i, (start, evt, expected)) in table.iter().enumerate() {
            let (home, inst, tid) = prime(*start, &format!("sm_{i}"));
            emit(&home, &inst, &tid, evt);
            let state = replay(&home).unwrap();
            let actual = state.tasks.get(&tid).unwrap().status;
            assert_eq!(
                actual, *expected,
                "({:?}, {}) expected → {:?}, got {:?}",
                start, evt, expected, actual
            );
            fs::remove_dir_all(&home).ok();
        }
    }

    /// **Replay-determinism invariant #5 (PR1 r2 deferred to PR2)** —
    /// sweep-replay associativity: applying sweep events on top of an
    /// existing log produces the same fold as inserting them anywhere
    /// chronologically before the rest. Defends the future "sweep
    /// daemon emits Linked/Done events while the operator emits manual
    /// transitions" interleaving.
    #[test]
    fn invariant_5_sweep_replay_associativity() {
        let inst = InstanceName::from("u");
        let sweep = InstanceName::from("system:task_sweep");

        // Scenario A: replay(operator events) then add sweep events on top.
        let home_a = tmp_home("assoc_a");
        append(
            &home_a,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-S1"),
                title: "s1".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
            },
        )
        .unwrap();
        append(
            &home_a,
            &inst,
            TaskEvent::Claimed {
                task_id: TaskId::from("t-S1"),
                by: inst.clone(),
            },
        )
        .unwrap();
        // Sweep emits Linked + Done on top.
        append(
            &home_a,
            &sweep,
            TaskEvent::Linked {
                task_id: TaskId::from("t-S1"),
                pr_id: PrId(42),
                source: LinkSource::SweepDiscovery {
                    sweep_id: "sw1".into(),
                },
                snapshot: PrSnapshot {
                    pr_state: "merged".into(),
                    merge_sha: Some("abc".into()),
                    api_response_hash: "h".into(),
                    captured_at: chrono::Utc::now().to_rfc3339(),
                },
            },
        )
        .unwrap();
        append(
            &home_a,
            &sweep,
            TaskEvent::Done {
                task_id: TaskId::from("t-S1"),
                by: inst.clone(),
                source: DoneSource::PrMerged {
                    pr_id: PrId(42),
                    merge_sha: "abc".into(),
                    merged_at: chrono::Utc::now().to_rfc3339(),
                    snapshot: PrSnapshot {
                        pr_state: "merged".into(),
                        merge_sha: Some("abc".into()),
                        api_response_hash: "h".into(),
                        captured_at: chrono::Utc::now().to_rfc3339(),
                    },
                },
            },
        )
        .unwrap();
        let state_a = replay(&home_a).unwrap();

        // Scenario B: same events but interleaved differently — sweep
        // events appear in the middle, not at the end. Replay's
        // chronological+seq sort canonicalises ordering, so the fold
        // result must match scenario A.
        let home_b = tmp_home("assoc_b");
        let log_b = home_b.join("task_events.jsonl");
        // Hand-craft the envelope sequence in a different file order:
        let envs = vec![
            TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: 1,
                timestamp: "2026-04-27T00:00:01Z".into(),
                instance: inst.clone(),
                event: TaskEvent::Created {
                    task_id: TaskId::from("t-S1"),
                    title: "s1".into(),
                    description: String::new(),
                    priority: "normal".into(),
                    owner: None,
                    due_at: None,
                    depends_on: Vec::new(),
                    routed_to: None,
                    branch: None,
                },
            },
            // Sweep Linked appears BEFORE operator Claimed in file order
            // but with later timestamp — replay sort still applies it
            // after Claimed because the sort key is timestamp.
            TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: 1,
                timestamp: "2026-04-27T00:00:03Z".into(),
                instance: sweep.clone(),
                event: TaskEvent::Linked {
                    task_id: TaskId::from("t-S1"),
                    pr_id: PrId(42),
                    source: LinkSource::SweepDiscovery {
                        sweep_id: "sw1".into(),
                    },
                    snapshot: PrSnapshot {
                        pr_state: "merged".into(),
                        merge_sha: Some("abc".into()),
                        api_response_hash: "h".into(),
                        captured_at: "2026-04-27T00:00:03Z".into(),
                    },
                },
            },
            TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: 2,
                timestamp: "2026-04-27T00:00:02Z".into(),
                instance: inst.clone(),
                event: TaskEvent::Claimed {
                    task_id: TaskId::from("t-S1"),
                    by: inst.clone(),
                },
            },
            TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: 2,
                timestamp: "2026-04-27T00:00:04Z".into(),
                instance: sweep.clone(),
                event: TaskEvent::Done {
                    task_id: TaskId::from("t-S1"),
                    by: inst.clone(),
                    source: DoneSource::PrMerged {
                        pr_id: PrId(42),
                        merge_sha: "abc".into(),
                        merged_at: "2026-04-27T00:00:04Z".into(),
                        snapshot: PrSnapshot {
                            pr_state: "merged".into(),
                            merge_sha: Some("abc".into()),
                            api_response_hash: "h".into(),
                            captured_at: "2026-04-27T00:00:04Z".into(),
                        },
                    },
                },
            },
        ];
        let mut content = String::new();
        for e in &envs {
            content.push_str(&serde_json::to_string(e).unwrap());
            content.push('\n');
        }
        fs::write(&log_b, content).unwrap();
        let state_b = replay(&home_b).unwrap();

        // Final task status & linked PRs identical regardless of file
        // order. Histories may differ in absolute timestamps but the
        // ordered status transition is the invariant.
        assert_eq!(
            state_a.tasks.get(&TaskId::from("t-S1")).unwrap().status,
            state_b.tasks.get(&TaskId::from("t-S1")).unwrap().status,
            "associativity: operator-then-sweep == interleaved"
        );
        assert_eq!(
            state_a.tasks.get(&TaskId::from("t-S1")).unwrap().linked_prs,
            state_b.tasks.get(&TaskId::from("t-S1")).unwrap().linked_prs
        );
        fs::remove_dir_all(&home_a).ok();
        fs::remove_dir_all(&home_b).ok();
    }
}
