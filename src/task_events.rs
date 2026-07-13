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
// #1164: module-level #![allow(dead_code)] removed; targeted allows below.

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
/// - [`TaskEvent::OwnerAssigned`] (explicit owner assignment without claim).
/// - [`TaskEvent::PriorityChanged`] (priority mutation tracking).
/// - [`TaskEvent::Created`] gains `due_at` / `depends_on` / `routed_to`
///   fields (`#[serde(default)]` so v1 envelopes round-trip).
/// - [`TaskRecord`] gains `created_by` / `created_at` / `updated_at` /
///   `due_at` / `depends_on` / `routed_to` / `result` fields.
///
/// v1 readers fail-closed on v2 envelopes via the `schema_version >
/// SCHEMA_VERSION` check in [`replay`]. v2 readers accept v1 envelopes
/// (the new `Created` fields default to `None` / `Vec::new()`).
pub const SCHEMA_VERSION: u32 = 2;

/// Hot-file event count [`compact`] trims the hot log back down to.
pub const COMPACTION_KEEP: usize = 10_000;

/// #2389 E1 follow-up: compaction hysteresis high-water. [`maybe_compact_events`]
/// triggers a compaction only once the hot log exceeds this (= 2×
/// [`COMPACTION_KEEP`]), then trims back to `COMPACTION_KEEP` in one pass. This
/// amortizes compaction to once per `COMPACTION_HIGH_WATER - COMPACTION_KEEP`
/// appends (vs once per append past `COMPACTION_KEEP` previously) — eliminating
/// the steady-state write amplification (rewriting the whole hot log + emitting a
/// 1-line archive segment on EVERY append). The hot log stays bounded by
/// `COMPACTION_HIGH_WATER`.
pub const COMPACTION_HIGH_WATER: usize = COMPACTION_KEEP * 2;

/// Sister-module log basename used with [`crate::event_log::append`] +
/// friends. The on-disk file is `<home>/task_events.jsonl`.
const LOG_NAME: &str = "task_events";

// ── Newtype IDs (type-system swap-prevention per dev-reviewer-2) ─────

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    #[allow(dead_code)]
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
    /// #1228: Auto-closed when assignee sent kind=report with matching correlation_id.
    ReportAutoClose {
        report_summary: String,
        closed_at: String,
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
        /// **Sprint 55 P0-C** — opt-out flag for daemon auto-bind on
        /// dispatch. `Some(false)` skips `dispatch_auto_bind_lease` so
        /// read-only RCA/audit/design tasks don't waste a worktree.
        /// `None` (absent) or `Some(true)` preserves current auto-bind
        /// behavior. v1 envelopes default to `None` via serde.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bind: Option<bool>,
        /// **Sprint 59 Wave 1 PR-1 (#9 task stall watchdog)** —
        /// optional operator-supplied estimate of seconds to
        /// completion. Anti-stall scanner emits `task_stalled` inbox
        /// event when elapsed since `last_progress_at` exceeds
        /// `eta_secs * 1.5`. `None` disables stall detection. v1
        /// envelopes default to `None` via serde.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        eta_secs: Option<i64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<TaskId>,
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
    /// #1265: transition to backlog status.
    MovedToBacklog {
        task_id: TaskId,
    },
    /// #1265: transition to in_review status.
    MovedToReview {
        task_id: TaskId,
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
    /// Description update after creation.
    DescriptionUpdated {
        task_id: TaskId,
        by: InstanceName,
        description: String,
    },
    /// Tags update without status transition.
    TagsSet {
        task_id: TaskId,
        tags: Vec<String>,
    },
    /// Set/replace a task's free-text `result` after creation — e.g. an owner or
    /// orchestrator backfilling the outcome on a done task. Additive variant
    /// (mirrors `TagsSet`); the `update` MCP arm emits it. `depends_on` has NO
    /// analogous variant by design — it is create-only/immutable.
    ResultSet {
        task_id: TaskId,
        by: InstanceName,
        result: String,
    },
    /// Metadata KV bag update — set one key-value pair on a task.
    MetadataSet {
        task_id: TaskId,
        by: InstanceName,
        key: String,
        value: serde_json::Value,
    },
    /// #1942: link a git branch to a task after creation. The dispatch
    /// (`send kind=task`) carries `branch=`, but a separately-created task starts
    /// with `branch: None`; this event fills it in so `auto_close_merged_tasks`
    /// can find the task↔branch link when the branch merges.
    BranchLinked {
        task_id: TaskId,
        by: InstanceName,
        branch: String,
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
            | TaskEvent::MovedToBacklog { task_id }
            | TaskEvent::MovedToReview { task_id }
            | TaskEvent::TaskCloseProposed { task_id, .. }
            | TaskEvent::OwnerAssigned { task_id, .. }
            | TaskEvent::PriorityChanged { task_id, .. }
            | TaskEvent::DescriptionUpdated { task_id, .. }
            | TaskEvent::TagsSet { task_id, .. }
            | TaskEvent::ResultSet { task_id, .. }
            | TaskEvent::MetadataSet { task_id, .. }
            | TaskEvent::BranchLinked { task_id, .. } => task_id,
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
            TaskEvent::MovedToBacklog { .. } => "moved_to_backlog",
            TaskEvent::MovedToReview { .. } => "moved_to_review",
            TaskEvent::TaskCloseProposed { .. } => "task_close_proposed",
            TaskEvent::OwnerAssigned { .. } => "owner_assigned",
            TaskEvent::PriorityChanged { .. } => "priority_changed",
            TaskEvent::DescriptionUpdated { .. } => "description_updated",
            TaskEvent::TagsSet { .. } => "tags_set",
            TaskEvent::ResultSet { .. } => "result_set",
            TaskEvent::MetadataSet { .. } => "metadata_set",
            TaskEvent::BranchLinked { .. } => "branch_linked",
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
    /// Sprint 46 P3: emitter's InstanceId for audit trail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emitter_id: Option<String>,
    pub event: TaskEvent,
}

// ── Folded board state (output of replay; not persisted) ───────────

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Open,
    Claimed,
    InProgress,
    InReview,
    Verified,
    Done,
    Cancelled,
    Blocked,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backlog => write!(f, "backlog"),
            Self::Open => write!(f, "open"),
            Self::Claimed => write!(f, "claimed"),
            Self::InProgress => write!(f, "in_progress"),
            Self::InReview => write!(f, "in_review"),
            Self::Verified => write!(f, "verified"),
            Self::Done => write!(f, "done"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Blocked => write!(f, "blocked"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Urgent,
}

#[allow(clippy::derivable_impls)]
impl Default for TaskPriority {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::High => write!(f, "high"),
            Self::Urgent => write!(f, "urgent"),
        }
    }
}

impl TaskStatus {
    /// Parse a status string (MCP-facing).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "backlog" => Some(Self::Backlog),
            "open" => Some(Self::Open),
            "claimed" => Some(Self::Claimed),
            "in_progress" => Some(Self::InProgress),
            "in_review" => Some(Self::InReview),
            "verified" => Some(Self::Verified),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }

    /// #1265: allowed transitions table. Returns true if transitioning
    /// from `self` to `target` is valid.
    pub fn can_transition_to(self, target: Self) -> bool {
        use TaskStatus::*;
        // Cancelled and Blocked are reachable from any non-terminal state.
        if target == Cancelled {
            return self != Done && self != Cancelled;
        }
        if target == Blocked {
            return !matches!(self, Done | Cancelled | Blocked);
        }
        matches!(
            (self, target),
            // Forward lifecycle
            (Backlog, Open)
                | (Open, Claimed)
                | (Claimed, InProgress)
                | (InProgress, InReview)
                | (InReview, Verified)
                | (Verified, Done)
                // Skip paths (common shortcuts)
                | (Open, InProgress)
                | (Open, Done)
                | (Claimed, InReview)
                | (Claimed, Done)
                | (InProgress, Done)
                | (InReview, Done)
                | (InProgress, Verified)
                // Backward (rejection / rework)
                | (InReview, InProgress)
                | (Verified, InProgress)
                | (Claimed, Open)
                | (InProgress, Open)
                // Unblock
                | (Blocked, Open)
                | (Blocked, Claimed)
                | (Blocked, InProgress)
                | (Blocked, InReview)
                // Reopen
                | (Done, Open)
                | (Cancelled, Open)
        )
    }
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
    /// Sprint 55 P0-C — opt-out flag for daemon auto-bind on dispatch.
    /// `Some(false)` means RCA/audit/design class; auto-bind was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<bool>,
    /// RFC3339 timestamp captured when the task first transitions
    /// to `in_progress`. Set in the `TaskEvent::InProgress` arm of
    /// [`TaskBoardState::apply`]; idempotent (only first transition
    /// records the timestamp).
    ///
    /// #807 Item 3: renamed `dispatched_at` → `started_at`. The
    /// stamp lands on InProgress (post-claim), not at `send()`
    /// dispatch. `serde(alias)` preserves replay of legacy
    /// task_events.jsonl carrying the prior field name.
    #[serde(alias = "dispatched_at", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — operator-
    /// supplied estimate of seconds to completion, sourced from the
    /// `eta_secs` field on `TaskEvent::Created` (or a future
    /// `EtaUpdated` event if the contract evolves).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
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
        self.apply_event(&env.event, &task_id, &env.instance, &touch_at);
        if let Some(t) = self.tasks.get_mut(env.event.task_id()) {
            t.history.push(history_entry);
        }
        true
    }

    fn apply_event(
        &mut self,
        event: &TaskEvent,
        task_id: &TaskId,
        instance: &InstanceName,
        touch_at: &str,
    ) {
        match event {
            TaskEvent::Created {
                title,
                description,
                priority,
                owner,
                due_at,
                depends_on,
                routed_to,
                branch,
                bind,
                eta_secs,
                tags,
                parent_id,
                ..
            } => {
                self.apply_created(
                    task_id,
                    instance,
                    touch_at,
                    title,
                    description,
                    priority,
                    owner,
                    due_at,
                    depends_on,
                    routed_to,
                    branch,
                    bind,
                    eta_secs,
                    tags,
                    parent_id,
                );
            }
            TaskEvent::Claimed { by, .. } => {
                self.apply_status_with_owner(task_id, touch_at, TaskStatus::Claimed, Some(by), true)
            }
            TaskEvent::InProgress { by, .. } => self.apply_in_progress(task_id, touch_at, by),
            TaskEvent::Verified { .. } => {
                self.apply_simple_status(task_id, touch_at, TaskStatus::Verified)
            }
            TaskEvent::Done { source, .. } => self.apply_done(task_id, touch_at, source),
            TaskEvent::Cancelled { .. } => self.apply_cancelled(task_id, touch_at),
            TaskEvent::Linked { pr_id, .. } => self.apply_linked(task_id, touch_at, *pr_id),
            TaskEvent::Blocked { reason, .. } => self.apply_blocked(task_id, touch_at, reason),
            TaskEvent::Unblocked { .. } => self.apply_unblocked(task_id, touch_at),
            TaskEvent::Reopened { .. } => {
                self.apply_simple_status(task_id, touch_at, TaskStatus::Open)
            }
            TaskEvent::Released { .. } => {
                self.apply_status_with_owner(task_id, touch_at, TaskStatus::Open, None, true)
            }
            TaskEvent::MovedToBacklog { .. } => {
                self.apply_simple_status(task_id, touch_at, TaskStatus::Backlog)
            }
            TaskEvent::MovedToReview { .. } => {
                self.apply_simple_status(task_id, touch_at, TaskStatus::InReview)
            }
            TaskEvent::TaskCloseProposed { .. } => self.apply_touch_only(task_id, touch_at),
            TaskEvent::OwnerAssigned {
                owner, routed_to, ..
            } => self.apply_owner_assigned(task_id, touch_at, owner, routed_to),
            TaskEvent::PriorityChanged { priority, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.priority = priority.clone();
                    t.updated_at = touch_at.to_string();
                }
            }
            TaskEvent::DescriptionUpdated { description, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.description = description.clone();
                    t.updated_at = touch_at.to_string();
                }
            }
            TaskEvent::TagsSet { tags, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.tags = tags.clone();
                    t.updated_at = touch_at.to_string();
                }
            }
            TaskEvent::ResultSet { result, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.result = Some(result.clone());
                    t.updated_at = touch_at.to_string();
                }
            }
            TaskEvent::BranchLinked { branch, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.branch = Some(branch.clone());
                    t.updated_at = touch_at.to_string();
                }
            }
            TaskEvent::MetadataSet { key, value, .. } => {
                if let Some(t) = self.tasks.get_mut(task_id) {
                    t.metadata.insert(key.clone(), value.clone());
                    t.updated_at = touch_at.to_string();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_created(
        &mut self,
        task_id: &TaskId,
        instance: &InstanceName,
        touch_at: &str,
        title: &str,
        description: &str,
        priority: &str,
        owner: &Option<InstanceName>,
        due_at: &Option<String>,
        depends_on: &[TaskId],
        routed_to: &Option<InstanceName>,
        branch: &Option<String>,
        bind: &Option<bool>,
        eta_secs: &Option<i64>,
        tags: &[String],
        parent_id: &Option<TaskId>,
    ) {
        self.tasks
            .entry(task_id.clone())
            .or_insert_with(|| TaskRecord {
                id: task_id.clone(),
                title: title.to_string(),
                description: description.to_string(),
                priority: priority.to_string(),
                status: TaskStatus::Open,
                owner: owner.clone(),
                linked_prs: Vec::new(),
                block_reason: None,
                history: Vec::new(),
                created_by: instance.clone(),
                created_at: touch_at.to_string(),
                updated_at: touch_at.to_string(),
                due_at: due_at.clone(),
                depends_on: depends_on.to_vec(),
                routed_to: routed_to.clone(),
                result: None,
                branch: branch.clone(),
                bind: *bind,
                started_at: None,
                eta_secs: *eta_secs,
                tags: tags.to_vec(),
                parent_id: parent_id.clone(),
                metadata: BTreeMap::new(),
            });
    }

    fn apply_simple_status(&mut self, task_id: &TaskId, touch_at: &str, status: TaskStatus) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = status;
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_status_with_owner(
        &mut self,
        task_id: &TaskId,
        touch_at: &str,
        status: TaskStatus,
        owner: Option<&InstanceName>,
        clear_routed: bool,
    ) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = status;
            t.owner = owner.cloned();
            if clear_routed {
                t.routed_to = None;
            }
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_in_progress(&mut self, task_id: &TaskId, touch_at: &str, by: &InstanceName) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = TaskStatus::InProgress;
            t.owner = Some(by.clone());
            t.updated_at = touch_at.to_string();
            if t.started_at.is_none() {
                t.started_at = Some(touch_at.to_string());
            }
        }
    }

    fn apply_done(&mut self, task_id: &TaskId, touch_at: &str, source: &DoneSource) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = TaskStatus::Done;
            t.updated_at = touch_at.to_string();
            match source {
                DoneSource::OperatorManual { result, .. } => t.result = result.clone(),
                // F1: project the terminal auto-close report body into `result`
                // when the task has no explicit result yet — never overwrite an
                // operator-set result. Purely read-model; replay backfills
                // historical ReportAutoClose logs. (PrMerged / AutoCloseOnPrMerge
                // are deliberately NOT projected — out of the witnessed scope; add
                // an arm here if a case surfaces.)
                DoneSource::ReportAutoClose { report_summary, .. } if t.result.is_none() => {
                    t.result = Some(report_summary.clone());
                }
                _ => {}
            }
        }
    }

    fn apply_cancelled(&mut self, task_id: &TaskId, touch_at: &str) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = TaskStatus::Cancelled;
            t.updated_at = touch_at.to_string();
        }
        let children: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.parent_id.as_ref() == Some(task_id))
            .filter(|(_, t)| matches!(t.status, TaskStatus::Open | TaskStatus::Claimed))
            .map(|(id, _)| id.clone())
            .collect();
        for child_id in children {
            if let Some(child) = self.tasks.get_mut(&child_id) {
                child.status = TaskStatus::Cancelled;
                child.updated_at = touch_at.to_string();
            }
        }
    }

    fn apply_linked(&mut self, task_id: &TaskId, touch_at: &str, pr_id: PrId) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            if !t.linked_prs.contains(&pr_id) {
                t.linked_prs.push(pr_id);
            }
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_blocked(&mut self, task_id: &TaskId, touch_at: &str, reason: &str) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.status = TaskStatus::Blocked;
            t.block_reason = Some(reason.to_string());
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_unblocked(&mut self, task_id: &TaskId, touch_at: &str) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            if t.status == TaskStatus::Blocked {
                t.status = TaskStatus::Open;
            }
            t.block_reason = None;
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_touch_only(&mut self, task_id: &TaskId, touch_at: &str) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.updated_at = touch_at.to_string();
        }
    }

    fn apply_owner_assigned(
        &mut self,
        task_id: &TaskId,
        touch_at: &str,
        owner: &Option<InstanceName>,
        routed_to: &Option<InstanceName>,
    ) {
        if let Some(t) = self.tasks.get_mut(task_id) {
            t.owner = owner.clone();
            t.routed_to = routed_to.clone();
            t.updated_at = touch_at.to_string();
        }
    }
}

// ── Board root (#2117 P0 project-isolation seam) ───────────────────
//
// The task board's on-disk files (the hot `task_events.jsonl`, the archive,
// and the replay/seq caches) live under a *board root*. P0 introduces the seam
// only: every storage fn now has a `_at(board, …)` variant, and the public
// `fn x(home, …)` delegates to `x_at(&board_root(home, DEFAULT_PROJECT), …)`.
// For the default/fleet project `board_root` returns `home` UNCHANGED, so the
// public API, every caller, and every test are byte-identical (P0 has NO
// multi-board caller — routing is P1). A real project gets its own subtree.

/// Sentinel project id for the single, fleet-wide board — maps to `home`.
pub(crate) const DEFAULT_PROJECT: &str = "default";

/// Resolve the on-disk root for a project's task board.
///
/// `default`/`fleet`/empty → `home` itself (single-project byte-identical).
/// Any other project id → `home/boards/<slug>/`, where the slug is derived
/// from the project id (a source_repo / `owner/repo`). P1 owns the routing
/// that picks a non-default project id; P0 only needs the mapping to exist.
pub(crate) fn board_root(home: &Path, project_id: &str) -> PathBuf {
    if project_id.is_empty() || project_id == DEFAULT_PROJECT || project_id == "fleet" {
        return home.to_path_buf();
    }
    home.join("boards").join(project_slug(project_id))
}

/// Derive a filesystem-safe directory name for a project id. Prefers a
/// canonical `owner__repo` for an `owner/repo` slug; otherwise sanitizes any
/// remaining path/URL separators. (P0: deterministic + round-trippable enough
/// for an isolated subtree; P1 may refine the canonicalization.)
pub(crate) fn project_slug(project_id: &str) -> String {
    let trimmed = project_id
        .trim()
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| project_id.trim().trim_end_matches('/'));
    let slug: String = trimmed
        .chars()
        .map(|c| match c {
            '/' => '_', // an `owner/repo` becomes `owner_repo` after the pass below
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') => c,
            _ => '_',
        })
        .collect();
    // CR-2026-06-14 (security): a project id that sanitizes to a path-special
    // component (`.`, `..`, or any all-dots string) escapes its subtree —
    // `board_root` joins it so `home/boards/..` collapses back to `home` and
    // `home/boards/.` to `home/boards`. A caller-supplied `project` override
    // could thus redirect a create onto the home/default board. Defang any
    // all-dots (or empty) slug to a single safe sentinel, matching how the pass
    // above maps every other unsafe char to `_`.
    if slug.is_empty() || slug.bytes().all(|b| b == b'.') {
        return "_".to_string();
    }
    slug
}

// ── Append: single + batch ─────────────────────────────────────────

fn log_path(board: &Path) -> PathBuf {
    board.join(format!("{LOG_NAME}.jsonl"))
}

fn archive_dir(board: &Path) -> PathBuf {
    board.join("task_events_archive")
}

/// CR-2026-06-14 (#2212 orphan-prune follow-up): does `board` have ANY on-disk
/// event bytes — a non-empty hot log or a non-empty archive segment?
///
/// `replay_at` SKIPS corrupt (non-JSON) lines (#1988 half-write tolerance), so a
/// board whose ENTIRE log is garbage replays to `Ok(empty)` — indistinguishable
/// BY STATE from a board that genuinely holds no tasks (terminal tasks stay in
/// `state.tasks`, so a readable non-empty board always replays ≥1 task). The
/// orphan-prune ([`crate::tasks::board_router::live_task_ids`]) uses this to
/// disambiguate: an empty replay WITH on-disk bytes is an unreadable/corrupt
/// board, not an empty one, so its index entries must NOT be treated as orphans.
/// Cheap O(1) metadata stats; no parse.
pub(crate) fn board_has_event_bytes(board: &Path) -> bool {
    let nonempty = |p: &Path| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false);
    if nonempty(&log_path(board)) {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir(archive_dir(board)) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") && nonempty(&p) {
                return true;
            }
        }
    }
    false
}

/// Append one event, returning the newly assigned monotonic seq#.
///
/// The seq is computed by tail-scanning the hot log under the same lock
/// as the write — concurrent appenders observe a totally-ordered seq
/// stream per instance.
pub fn append(home: &Path, instance: &InstanceName, event: TaskEvent) -> anyhow::Result<u64> {
    append_at(&board_root(home, DEFAULT_PROJECT), instance, event)
}

/// #2117 board-root variant of [`append`].
pub(crate) fn append_at(
    board: &Path,
    instance: &InstanceName,
    event: TaskEvent,
) -> anyhow::Result<u64> {
    let seqs = append_batch_at(board, instance, vec![event])?;
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
    append_batch_at(&board_root(home, DEFAULT_PROJECT), instance, events)
}

/// #2117 board-root variant of [`append_batch`]. The board root carries the hot
/// log, lock, and seq cache. NOTE (P1 seam): emitter-id resolution + the audit
/// log are passed the same `board` arg here (byte-identical while
/// `board == home`); P1 routing decides whether instance-resolution / fleet
/// audit stay home-scoped — emitter resolution already degrades to `None` (a
/// best-effort audit field) if the board can't resolve it.
pub(crate) fn append_batch_at(
    board: &Path,
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

    // Sprint 46 P3: resolve emitter's InstanceId for audit trail.
    let emitter_id = match crate::agent::resolve_instance(board, instance.as_str()) {
        Ok((id, _)) => Some(id.full()),
        Err(e) => {
            tracing::debug!(instance = %instance, error = %e, "emitter ID resolution failed");
            None
        }
    };

    let mut hot_lines = 0usize;
    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        let (start_seq, pre_lines) = next_seq_under_lock(board, log_path, &instance, count as u64)?;
        hot_lines = pre_lines;
        let mut lines = Vec::with_capacity(count);
        for (i, event) in events.into_iter().enumerate() {
            let seq = start_seq + i as u64;
            seqs.push(seq);
            let envelope = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq,
                timestamp: now.clone(),
                instance: instance.clone(),
                emitter_id: emitter_id.clone(),
                event,
            };
            lines.push(serde_json::to_string(&envelope)?);
        }
        Ok(lines)
    })?;
    invalidate_replay_cache();
    maybe_compact_events(board, hot_lines + count);
    Ok(seqs)
}

/// Batch variant of [`append_checked`] (#1868): append ALL `events` atomically
/// iff `precondition` — evaluated under the append lock against a FRESH on-disk
/// replay — returns `Ok`. Closes the same TOCTOU as [`append_checked`] for the
/// multi-event `update` arm (a status transition plus priority/desc/tags/owner
/// events emitted as one batch). On precondition failure NO event is written and
/// the reason is returned as `Ok(Err(reason))`; the outer `Err` is reserved for
/// IO / replay failures.
// #2117 P1: every non-test in-tree caller now routes via `append_batch_checked_at`
// (the task command handlers resolve a board first). This home-default wrapper is
// retained for the symmetric storage API (home + `_at` pair) and its test callers;
// `allow(dead_code)` because non-test builds have no remaining caller.
#[allow(dead_code)]
pub fn append_batch_checked<F>(
    home: &Path,
    instance: &InstanceName,
    events: Vec<TaskEvent>,
    precondition: F,
) -> anyhow::Result<Result<Vec<u64>, String>>
where
    F: FnOnce(&TaskBoardState) -> Result<(), String>,
{
    append_batch_checked_at(
        &board_root(home, DEFAULT_PROJECT),
        instance,
        events,
        precondition,
    )
}

/// #2117 board-root variant of [`append_batch_checked`].
pub(crate) fn append_batch_checked_at<F>(
    board: &Path,
    instance: &InstanceName,
    events: Vec<TaskEvent>,
    precondition: F,
) -> anyhow::Result<Result<Vec<u64>, String>>
where
    F: FnOnce(&TaskBoardState) -> Result<(), String>,
{
    if events.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let instance = instance.clone();
    let count = events.len();
    let mut seqs: Vec<u64> = Vec::with_capacity(count);
    let now = chrono::Utc::now().to_rfc3339();
    let emitter_id = match crate::agent::resolve_instance(board, instance.as_str()) {
        Ok((id, _)) => Some(id.full()),
        Err(e) => {
            tracing::debug!(instance = %instance, error = %e, "emitter ID resolution failed");
            None
        }
    };

    let mut rejection: Option<String> = None;
    let mut hot_lines = 0usize;
    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        // FRESH replay under the lock — authoritative committed history.
        let state = replay_uncached(board)?;
        if let Err(reason) = precondition(&state) {
            rejection = Some(reason);
            return Ok(Vec::new()); // empty ⇒ no write
        }
        let (start_seq, pre_lines) = next_seq_under_lock(board, log_path, &instance, count as u64)?;
        hot_lines = pre_lines;
        let mut lines = Vec::with_capacity(count);
        for (i, event) in events.into_iter().enumerate() {
            let seq = start_seq + i as u64;
            seqs.push(seq);
            let envelope = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq,
                timestamp: now.clone(),
                instance: instance.clone(),
                emitter_id: emitter_id.clone(),
                event,
            };
            lines.push(serde_json::to_string(&envelope)?);
        }
        Ok(lines)
    })?;

    if let Some(reason) = rejection {
        return Ok(Err(reason));
    }
    invalidate_replay_cache();
    maybe_compact_events(board, hot_lines + count);
    Ok(Ok(seqs))
}

/// Append `event` atomically iff `precondition` — evaluated under the append
/// lock against a FRESH on-disk replay — returns `Ok`. This closes the TOCTOU
/// where a caller validates task state, then appends after a concurrent writer
/// has already mutated it (e.g. #t-21: two agents claiming the same Open task,
/// both seeing it Open before either appends, both succeeding).
///
/// The precondition runs inside the SAME critical section as the write, so the
/// state it inspects is the authoritative committed history: any racing writer
/// either committed before us (and is visible) or is blocked waiting for the
/// lock (and will re-validate against OUR committed event next).
///
/// On precondition failure NO event is written; the rejection reason is
/// returned as `Ok(Err(reason))`. The outer `Err` is reserved for IO / replay
/// failures. Replay/append semantics are otherwise unchanged — this reuses
/// `replay_uncached` (lock-free) for the read and the same seq/cache plumbing
/// as [`append_batch`].
// #2117 P1: non-test callers route via `append_checked_at` (board-resolved); this
// home-default wrapper is retained for the symmetric storage API + test callers.
#[allow(dead_code)]
pub fn append_checked<F>(
    home: &Path,
    instance: &InstanceName,
    event: TaskEvent,
    precondition: F,
) -> anyhow::Result<Result<u64, String>>
where
    F: FnOnce(&TaskBoardState) -> Result<(), String>,
{
    append_checked_at(
        &board_root(home, DEFAULT_PROJECT),
        instance,
        event,
        precondition,
    )
}

/// #2117 board-root variant of [`append_checked`].
pub(crate) fn append_checked_at<F>(
    board: &Path,
    instance: &InstanceName,
    event: TaskEvent,
    precondition: F,
) -> anyhow::Result<Result<u64, String>>
where
    F: FnOnce(&TaskBoardState) -> Result<(), String>,
{
    let instance = instance.clone();
    let now = chrono::Utc::now().to_rfc3339();
    let emitter_id = match crate::agent::resolve_instance(board, instance.as_str()) {
        Ok((id, _)) => Some(id.full()),
        Err(e) => {
            tracing::debug!(instance = %instance, error = %e, "emitter ID resolution failed");
            None
        }
    };

    let mut assigned_seq: Option<u64> = None;
    let mut rejection: Option<String> = None;
    let mut hot_lines = 0usize;

    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        // FRESH replay under the lock — authoritative committed history.
        let state = replay_uncached(board)?;
        if let Err(reason) = precondition(&state) {
            rejection = Some(reason);
            return Ok(Vec::new()); // empty ⇒ no write
        }
        let (seq, pre_lines) = next_seq_under_lock(board, log_path, &instance, 1)?;
        assigned_seq = Some(seq);
        hot_lines = pre_lines;
        let envelope = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: now.clone(),
            instance: instance.clone(),
            emitter_id: emitter_id.clone(),
            event,
        };
        Ok(vec![serde_json::to_string(&envelope)?])
    })?;

    if let Some(reason) = rejection {
        return Ok(Err(reason));
    }
    invalidate_replay_cache();
    maybe_compact_events(board, hot_lines + 1);
    if let Some(seq) = assigned_seq {
        return Ok(Ok(seq));
    }
    Ok(Ok(0))
}

/// #1873: append a DAEMON-originated →Done write (auto-close / sweep) iff the task
/// can still legally transition to Done UNDER the append lock against fresh
/// committed state. #1868 hardened the user `done`/`update` handlers, but daemon
/// auto-close paths still bare-appended — so a daemon racing a peer/operator
/// `cancel` could flip a Cancelled (terminal) task to Done. `events` is the
/// →Done batch (the `Done` event alone, or a preceding `Linked` + `Done`).
/// Returns `Ok(true)` if written, `Ok(false)` if SKIPPED because the task can no
/// longer transition to Done (logged at info — the operator already holds the
/// canonical terminal signal; not an error).
#[allow(dead_code)]
pub fn append_done_if_legal(
    home: &Path,
    instance: &InstanceName,
    task_id: &str,
    events: Vec<TaskEvent>,
) -> anyhow::Result<bool> {
    append_done_if_legal_at(
        &board_root(home, DEFAULT_PROJECT),
        instance,
        task_id,
        events,
    )
}

/// #2117 board-root variant of [`append_done_if_legal`].
pub(crate) fn append_done_if_legal_at(
    board: &Path,
    instance: &InstanceName,
    task_id: &str,
    events: Vec<TaskEvent>,
) -> anyhow::Result<bool> {
    let tid = TaskId(task_id.to_string());
    let label = task_id.to_string();
    let outcome = append_batch_checked_at(board, instance, events, move |state| {
        match state.tasks.get(&tid).map(|r| r.status) {
            Some(status) if status.can_transition_to(TaskStatus::Done) => Ok(()),
            Some(status) => Err(format!(
                "task '{label}' is '{status}' — cannot transition to done"
            )),
            None => Err(format!("task '{label}' not found")),
        }
    })?;
    match outcome {
        Ok(_) => Ok(true),
        Err(reason) => {
            tracing::info!(
                target: "task_board",
                reason = %reason,
                "#1873: daemon →Done write skipped — task no longer transitionable to done"
            );
            Ok(false)
        }
    }
}

/// Tail-scan the hot log for the highest seq# this instance has emitted.
/// Returns `(max_seq, nonblank_line_count)` — the line count is folded into the
/// SAME scan (free) so the append path can hand it to [`maybe_compact_events`] as
/// a hysteresis hint, avoiding a second full read of the hot log per append.
/// Best-effort: malformed lines are skipped because [`replay`] is the
/// strict reader; here we just need the high-water mark.
///
/// H10 (CR-2026-06-14): ALWAYS scan the on-disk hot log — never short-circuit on
/// a process-local cache. Every caller runs inside an `append_lines_under_lock`
/// closure (under the cross-process append flock), but a cache is process-local:
/// a high-water mark cached before ANOTHER process (e.g. the daemon's
/// auto_close/sweep/lifecycle vs the MCP `tasks::handle`) appended the same
/// instance is a STALE high-water → the next append here would mint a seq `<=` an
/// already-persisted one, and replay's idempotency skip (`seq <= last_seen`)
/// would SILENTLY DROP the real task transition. The on-disk file is the only
/// source of truth all appenders share. The scan is cheap because task-event
/// appends are agent/human-paced (not a hot loop), batches share one scan, and
/// `compact_at` (now wired via [`maybe_compact_events`]) bounds the hot log to
/// `COMPACTION_KEEP`. Because compaction ARCHIVES the older slice OUT of the hot
/// log, this hot-only scan no longer sees an instance whose events were all
/// archived — so callers go through [`next_seq_under_lock`], which maxes this
/// scan with the per-instance seq sidecar (which survives compaction) to keep
/// the high-water correct. A cross-process-correct approach must re-read on
/// change anyway; the previous process-local cache was O(1) only by trusting
/// stale cross-process state — the exact bug avoided here.
fn max_seq_for_instance(log_path: &Path, instance: &InstanceName) -> anyhow::Result<(u64, usize)> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e.into()),
    };
    let mut max = 0u64;
    let mut lines = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        if let Ok(env) = serde_json::from_str::<TaskEventEnvelope>(line) {
            if &env.instance == instance && env.seq > max {
                max = env.seq;
            }
        }
    }
    Ok((max, lines))
}

// ── Per-instance seq high-water sidecar (retention seq-safety) ────────
//
// `compact_at` archives the older slice of the hot log, but
// `max_seq_for_instance` scans ONLY the hot log (H10's cross-process
// contract). So once ALL of an instance's events have been archived, the
// hot scan returns 0 and the next append would mint a seq `<=` an
// already-persisted one — replay's idempotency skip (`seq <= last_seen`)
// would then SILENTLY DROP the real transition. (This is exactly why
// `compact` was left dead: an unbounded hot log keeps the full history in
// one file, so the hot scan was always complete.)
//
// Fix: persist a per-instance high-water in a small sidecar that survives
// compaction. It is a DERIVED CACHE of the authoritative event log, not a
// new source of truth — any load failure (missing / corrupt) rebuilds it
// from a full hot+archive scan, so it can never wedge. The committed
// high-water is `max(sidecar, hot-scan)`: the hot-scan term keeps it
// crash-safe (a crash between the sidecar write and the hot append leaves
// the sidecar AHEAD, never behind → at worst a seq gap, never a collision).
fn seq_sidecar_path(board: &Path) -> PathBuf {
    board.join(format!("{LOG_NAME}_seq.json"))
}

/// Best-effort scan of hot log + every archive segment for the max seq per
/// instance. Lenient parse (skip torn lines — replay is the strict reader),
/// mirroring [`max_seq_for_instance`]. Used to (re)build the sidecar.
fn scan_seq_highwater(board: &Path) -> BTreeMap<String, u64> {
    let mut hw: BTreeMap<String, u64> = BTreeMap::new();
    let mut absorb = |path: &Path| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(env) = serde_json::from_str::<TaskEventEnvelope>(line) {
                let slot = hw.entry(env.instance.as_str().to_string()).or_insert(0);
                *slot = (*slot).max(env.seq);
            }
        }
    };
    if let Ok(entries) = std::fs::read_dir(archive_dir(board)) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                absorb(&p);
            }
        }
    }
    absorb(&log_path(board));
    hw
}

/// Load the per-instance high-water sidecar, rebuilding from a full
/// hot+archive scan on any failure (missing / unparseable). The rebuild is
/// persisted so the O(history) scan happens at most once per board.
fn load_seq_highwater(board: &Path) -> BTreeMap<String, u64> {
    if let Ok(content) = std::fs::read_to_string(seq_sidecar_path(board)) {
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, u64>>(&content) {
            return map;
        }
    }
    // Missing or corrupt → rebuild from the authoritative log + persist.
    let rebuilt = scan_seq_highwater(board);
    let _ = write_seq_highwater(board, &rebuilt);
    rebuilt
}

/// Atomic-write the sidecar (crash-safe tmp + rename via [`crate::store::atomic_write`]).
fn write_seq_highwater(board: &Path, hw: &BTreeMap<String, u64>) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(hw)?;
    crate::store::atomic_write(&seq_sidecar_path(board), &bytes)
}

/// Compute the start seq for `instance`'s batch of `count` events and persist
/// the bumped high-water — all under the caller's append lock. The high-water
/// is `max(sidecar, hot-scan)` (archive-safe + crash-safe). The sidecar is
/// written BEFORE the caller appends the lines, so a crash can only leave it
/// ahead of the hot log (a harmless seq gap), never behind (a collision →
/// silent replay drop).
///
/// Returns `(start_seq, hot_lines_pre_append)`: the second value is the hot-log
/// line count from the same scan, which the caller adds `count` to and hands to
/// [`maybe_compact_events`] as the post-append hysteresis hint (so the common
/// path needs no extra hot-log read).
fn next_seq_under_lock(
    board: &Path,
    log_path: &Path,
    instance: &InstanceName,
    count: u64,
) -> anyhow::Result<(u64, usize)> {
    let mut hw = load_seq_highwater(board);
    let (hot_max, hot_lines) = max_seq_for_instance(log_path, instance)?;
    let prev = hw.get(instance.as_str()).copied().unwrap_or(0).max(hot_max);
    let start = prev + 1;
    hw.insert(instance.as_str().to_string(), start + count - 1);
    write_seq_highwater(board, &hw)?;
    Ok((start, hot_lines))
}

// ── Replay cache ─────────────────────────────────────────────────────
// Read-side cache: avoids full-file replay when nothing has changed.
// Keyed on (home, generation, file_len, mtime_ns):
// - generation: process-wide monotonic counter, catches in-process
//   concurrent appends (fixes Linux ext4 mtime ms-granularity flake)
// - file_len + mtime_ns: catch external modifications (cross-process
//   writes, compaction, tests truncating the log)

static REPLAY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

type ReplayCacheKey = (std::path::PathBuf, u64, u64, i64);

struct ReplayCacheEntry {
    key: ReplayCacheKey,
    state: TaskBoardState,
}

// #2117 P0: per-board-keyed map (was a single global `Option`). The map key is
// the board root; the entry's `key` field still carries the full freshness tuple
// (path, generation, len, mtime). Single-board (default) keeps exactly one entry
// → byte-identical; P1 multi-board can't cross-contaminate state between boards.
static REPLAY_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<PathBuf, ReplayCacheEntry>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn replay_cache_key(board: &Path) -> ReplayCacheKey {
    let gen = REPLAY_GENERATION.load(std::sync::atomic::Ordering::Acquire);
    let log = log_path(board);
    let (log_len, mtime_ns) = std::fs::metadata(&log)
        .ok()
        .map(|m| {
            let len = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            (len, mtime)
        })
        .unwrap_or((0, 0));
    (board.to_path_buf(), gen, log_len, mtime_ns)
}

pub fn invalidate_replay_cache() {
    REPLAY_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

// ── Replay: strict reader (forward-compat fail-closed) ─────────────

/// Fold the entire on-disk event history (archive + hot file) into a
/// `TaskBoardState`. Strict: any envelope whose `schema_version` exceeds
/// [`SCHEMA_VERSION`] aborts the replay (forward-compat fail-closed),
/// and any line that fails to deserialize as a known [`TaskEvent`]
/// variant aborts (per dev-reviewer-2 must-have: replay must NOT silently
/// skip unknown envelopes).
/// #1990 item 4: process-global once-per-boot latch. The per-tick task-board
/// readers (cron gate in `cron_tick.rs`, idle watchdog) swallow a fail-closed
/// replay error into a read-gate, so without this the board silently freezes and
/// the operator has no cause to look at. Boot-scoped (a restart re-alerts — the
/// cause either healed or still blocks).
static REPLAY_FAILCLOSED_EVENT_EMITTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// #1990 item 4 (follows the #1972 crash-budget surface pattern): when [`replay`]
/// fail-closes on a forward-incompatible record (#1992 — a future-version or
/// unknown-variant envelope), make it OPERATOR-VISIBLE. The daemon keeps running
/// while the per-tick callers swallow the `Err`, so an operator otherwise sees a
/// frozen task board with no explanation. The fix is dual: ERROR-log every
/// occurrence (greppable) and emit ONE `event_log` entry per boot (latched so
/// per-tick callers can't spam). Observation only: the fail-closed `Err` still
/// propagates unchanged, so no recovery semantics change (same discipline as
/// #1972). A transient IO error (not the "fail-closed" class) is left alone.
///
/// Classification is by the `"fail-closed"` substring of the error — a contract
/// pinned at `read_envelopes_strict`'s two `bail!` sites; keep them in lockstep.
/// Known limitation (#1990 item 4, reviewer-2 minor 2): only failures that flow
/// through [`replay`] are surfaced; the timeline queries `envelopes_for_task` /
/// `stream_envelopes` fail-close without surfacing. The board-freeze alert here
/// is the primary operator signal, so that narrower timeline-query gap is
/// accepted rather than expanding scope.
fn surface_failclosed_replay_once(board: &Path, err: &anyhow::Error) {
    let msg = err.to_string();
    // stringly-allow: `err` is an `anyhow::Error` from replay with no typed
    // variant for the fail-closed condition; the message text is the only signal
    // for the once-per-boot board-freeze alert gate.
    if !msg.contains("fail-closed") {
        return;
    }
    tracing::error!(
        error = %msg,
        "task-board replay FAIL-CLOSED — the board will not advance until resolved (upgrade the daemon to a version that understands this log, or quarantine the offending record)"
    );
    if !REPLAY_FAILCLOSED_EVENT_EMITTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::event_log::log(
            board,
            "task_replay_fail_closed",
            "task-board",
            &format!(
                "task-board replay fail-closed — the board is frozen until resolved: {msg}. \
                 Fix: upgrade the daemon to a version that understands this log, or quarantine \
                 the offending record. Further failures this boot log at error level only."
            ),
        );
    }
}

pub fn replay(home: &Path) -> anyhow::Result<TaskBoardState> {
    replay_at(&board_root(home, DEFAULT_PROJECT))
}

/// #2117 board-root variant of [`replay`].
pub(crate) fn replay_at(board: &Path) -> anyhow::Result<TaskBoardState> {
    let key = replay_cache_key(board);
    {
        let cache = REPLAY_CACHE.lock();
        if let Some(entry) = cache.get(board) {
            if entry.key == key {
                return Ok(entry.state.clone());
            }
        }
    }

    let state = match replay_uncached(board) {
        Ok(s) => s,
        Err(e) => {
            // #1990 item 4: surface the fail-closed stall before the per-tick
            // caller swallows the Err into a read-gate.
            surface_failclosed_replay_once(board, &e);
            return Err(e);
        }
    };

    REPLAY_CACHE.lock().insert(
        board.to_path_buf(),
        ReplayCacheEntry {
            key,
            state: state.clone(),
        },
    );

    Ok(state)
}

fn replay_uncached(board: &Path) -> anyhow::Result<TaskBoardState> {
    let mut state = TaskBoardState::default();

    let archive_dir = archive_dir(board);
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

    let log_path = log_path(board);
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

// ── #2760 item 1: router-only STRICT replay (route-local complete-record proof) ──
//
// `replay_uncached` (above) is the FLEET-WIDE reader: it SKIPS a non-JSON line as a
// tolerated half-write (#1988) and serves the rest. That leniency is right for a
// display/list read but WRONG for the per-id ROUTER authority: a skipped record
// could be the very `Created`/`Cancelled` event that decides whether the target id
// lives on THIS board, so a silent skip turns a real hit into a false miss (or a
// real duplicate into a false unique). `replay_strict_at` is the router-ONLY reader
// that fails closed instead:
//   - ANY complete (newline-terminated) malformed record — non-JSON, a
//     `schema_version` newer than supported, or a well-formed-but-undeserializable
//     envelope — is a hard `StrictReplayError` (the router maps it to `Unreadable`).
//   - ONLY a final unterminated EOF fragment on the LIVE log is tolerable: a crash
//     mid-`append_lines_under_lock` (append-in-place, no tmp+rename) leaves exactly
//     such a torn tail. It is repaired ONCE under the SAME writer lock (quarantine +
//     truncate-to-last-newline + fsync), then the scan re-runs on the clean file.
//   - Archives are written tmp+rename (atomic), so a torn tail there is real
//     corruption, NOT a repairable fragment → `StrictReplayError`.
// The fleet-wide `replay`/`replay_uncached`/`read_envelopes_strict` path is
// deliberately UNCHANGED (this is additive — no fleet-wide behaviour change).

/// #2760 item 1: why the router-only strict replay refused to anchor a route — the
/// board's committed history could not be proven complete. The router maps this to
/// [`crate::tasks::TaskRouteError::Unreadable`].
#[derive(Debug)]
pub(crate) struct StrictReplayError {
    pub path: PathBuf,
    pub cause: String,
}

/// Split raw file `content` into COMPLETE (newline-terminated, non-blank) records
/// and an optional trailing UNTERMINATED fragment (the last record with no final
/// `\n` — the shape a crash mid-append leaves). A file ending in `\n` has no
/// fragment; a blank/whitespace-only tail is not a fragment.
pub(crate) fn split_complete_and_fragment(content: &str) -> (Vec<&str>, Option<&str>) {
    if content.is_empty() {
        return (Vec::new(), None);
    }
    let terminated = content.ends_with('\n');
    let mut lines: Vec<&str> = content.lines().collect();
    let fragment = if terminated { None } else { lines.pop() };
    let complete: Vec<&str> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
    let fragment = fragment.filter(|f| !f.trim().is_empty());
    (complete, fragment)
}

/// Strict single-line parse: the router rejects every shape `read_envelopes_strict`
/// either skips (non-JSON) or aborts on (future-version / undeserializable) — here
/// they ALL become a typed cause the caller turns into `Unreadable`.
fn parse_envelope_strict(line: &str) -> Result<TaskEventEnvelope, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("non-JSON task-event record: {e}"))?;
    let version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if version > SCHEMA_VERSION as u64 {
        return Err(format!(
            "schema_version {version} > supported {SCHEMA_VERSION} (forward-compat fail-closed)"
        ));
    }
    serde_json::from_value(value)
        .map_err(|e| format!("undeserializable envelope at supported schema: {e}"))
}

/// Outcome of one strict scan pass (no repair). `LiveFragment` = the live log ended
/// in an unterminated, unparseable torn tail — the ONE repairable case.
enum StrictScan {
    Complete(TaskBoardState),
    LiveFragment,
}

/// One strict scan of `board` (archives then live log), no repair. Mirrors
/// [`replay_uncached`]'s per-file sort+apply so a corruption-free board yields the
/// SAME state as the lenient reader.
fn replay_strict_scan(board: &Path) -> Result<StrictScan, StrictReplayError> {
    let mut state = TaskBoardState::default();
    let malformed = |path: &Path, cause: String| StrictReplayError {
        path: path.to_path_buf(),
        cause,
    };

    // Archives (atomic tmp+rename): no fragment tolerance — any torn tail or
    // malformed record is real corruption.
    let archive_dir = archive_dir(board);
    if archive_dir.is_dir() {
        let mut archives: Vec<PathBuf> = std::fs::read_dir(&archive_dir)
            .map_err(|e| malformed(&archive_dir, format!("archive read_dir failed: {e}")))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        archives.sort();
        for path in archives {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| malformed(&path, format!("archive read failed: {e}")))?;
            let (complete, fragment) = split_complete_and_fragment(&content);
            if fragment.is_some() {
                return Err(malformed(
                    &path,
                    "archive has an unterminated final record (atomic writes are always \
                     newline-terminated — this is corruption, not a repairable tail)"
                        .to_string(),
                ));
            }
            let mut envelopes = Vec::with_capacity(complete.len());
            for line in complete {
                envelopes.push(parse_envelope_strict(line).map_err(|c| malformed(&path, c))?);
            }
            sort_envelopes(&mut envelopes);
            for env in &envelopes {
                state.apply(env);
            }
        }
    }

    // Live log: a trailing unterminated, unparseable fragment is the ONE repairable
    // case; a complete malformed record is not.
    let log_path = log_path(board);
    if log_path.exists() {
        let content = std::fs::read_to_string(&log_path)
            .map_err(|e| malformed(&log_path, format!("live log read failed: {e}")))?;
        let (complete, fragment) = split_complete_and_fragment(&content);
        let mut envelopes = Vec::with_capacity(complete.len());
        for line in complete {
            envelopes.push(parse_envelope_strict(line).map_err(|c| malformed(&log_path, c))?);
        }
        if let Some(frag) = fragment {
            // A trailing record without a newline: if it PARSES it is a whole, valid
            // event (just missing its terminator) — apply it, no repair. If it does
            // NOT parse, it is a torn tail → signal a repair.
            match parse_envelope_strict(frag) {
                Ok(env) => envelopes.push(env),
                Err(_) => return Ok(StrictScan::LiveFragment),
            }
        }
        sort_envelopes(&mut envelopes);
        for env in &envelopes {
            state.apply(env);
        }
    }

    Ok(StrictScan::Complete(state))
}

/// #2760 item 1: the router-only strict replay. See the module comment above.
pub(crate) fn replay_strict_at(board: &Path) -> Result<TaskBoardState, StrictReplayError> {
    match replay_strict_scan(board)? {
        StrictScan::Complete(state) => Ok(state),
        StrictScan::LiveFragment => {
            // Repair the torn tail under the writer lock, then re-scan ONCE. A
            // fragment that survives one repair (e.g. a racing writer left a fresh
            // torn tail) is hard corruption — bounded, never a repair loop.
            repair_live_trailing_fragment(board)?;
            match replay_strict_scan(board)? {
                StrictScan::Complete(state) => Ok(state),
                StrictScan::LiveFragment => Err(StrictReplayError {
                    path: log_path(board),
                    cause: "live-log trailing fragment persisted after one repair".to_string(),
                }),
            }
        }
    }
}

/// Repair a live-log torn tail: under the SAME `task_events.jsonl.lock` the append
/// path holds, re-read, and — only if the file STILL ends in an unterminated,
/// unparseable fragment — quarantine that fragment and truncate the log to the last
/// newline (fsync + durable parent). A concurrent writer that already completed or
/// replaced the tail short-circuits to a no-op (the re-scan then sees clean state).
fn repair_live_trailing_fragment(board: &Path) -> Result<(), StrictReplayError> {
    let path = log_path(board);
    let err = |cause: String| StrictReplayError {
        path: path.clone(),
        cause,
    };
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)
        .map_err(|e| err(format!("acquire writer lock for EOF repair: {e}")))?;

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Vanished under the lock (compaction/rewrite) → nothing to repair; re-scan.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(err(format!("re-read under lock: {e}"))),
    };
    // A concurrent append completed the tail (now newline-terminated) → no-op.
    if content.is_empty() || content.ends_with('\n') {
        return Ok(());
    }
    let cut = content.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let fragment = &content[cut..];
    if fragment.trim().is_empty() {
        return Ok(());
    }
    // The tail is now a valid whole record (a writer flushed it without a newline)
    // → do NOT drop a real event; leave it for the re-scan to apply.
    if parse_envelope_strict(fragment).is_ok() {
        return Ok(());
    }

    // Quarantine the torn bytes (forensics — never silently destroy), then truncate.
    quarantine_torn_tail(board, fragment);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| err(format!("open for truncate: {e}")))?;
    f.set_len(cut as u64)
        .map_err(|e| err(format!("truncate torn tail: {e}")))?;
    f.sync_all()
        .map_err(|e| err(format!("fsync after truncate: {e}")))?;
    crate::store::fsync_parent_dir(&path);
    invalidate_replay_cache();
    tracing::warn!(
        tag = "#2760-eof-fragment-repaired",
        board = %board.display(),
        bytes = fragment.len(),
        "router repaired a live task_events torn tail (truncated to last newline; quarantined)"
    );
    Ok(())
}

/// Quarantine a torn tail under `task_events.recovery/<ts>/` (mirrors
/// [`recover_half_writes_at`]) before it is truncated out of the live log.
fn quarantine_torn_tail(board: &Path, fragment: &str) {
    use std::io::Write;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let recovery_dir = board.join("task_events.recovery").join(&ts);
    let _ = std::fs::create_dir_all(&recovery_dir);
    if let Ok(mut rf) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(recovery_dir.join(format!("{LOG_NAME}.jsonl")))
    {
        let _ = writeln!(rf, "{fragment}");
        let _ = rf.sync_all();
    }
}

/// Return all task event envelopes for a given `task_id` on a board, sorted
/// chronologically. Used by `task action=activity` to build a timeline. (#2117
/// P1: the activity handler resolves the task's board and calls this directly,
/// so the former `home`-default wrapper — which had no other callers — was
/// removed; `board_root(home, DEFAULT)` is `home` for a default-board read.)
pub(crate) fn envelopes_for_task_at(
    board: &Path,
    task_id: &str,
) -> anyhow::Result<Vec<TaskEventEnvelope>> {
    let tid = TaskId(task_id.to_string());
    let mut all = Vec::new();

    let archive = archive_dir(board);
    if archive.is_dir() {
        let mut archives: Vec<std::path::PathBuf> = std::fs::read_dir(&archive)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        archives.sort();
        for path in archives {
            let mut envs = Vec::new();
            read_envelopes_strict(&path, &mut envs)?;
            all.extend(envs.into_iter().filter(|e| *e.event.task_id() == tid));
        }
    }

    let lp = log_path(board);
    if lp.exists() {
        let mut envs = Vec::new();
        read_envelopes_strict(&lp, &mut envs)?;
        all.extend(envs.into_iter().filter(|e| *e.event.task_id() == tid));
    }

    sort_envelopes(&mut all);
    Ok(all)
}

/// #1077: stream every persisted envelope (archive + live log), sorted by
/// timestamp → instance → seq. Unlike [`replay`] this preserves per-event
/// timestamps (replay folds to state and drops them), which the token
/// time-join needs to build per-task `[start, end)` windows. Read-only; no
/// schema change. Fails closed on an unparseable envelope, same as replay.
pub fn stream_envelopes(home: &Path) -> anyhow::Result<Vec<TaskEventEnvelope>> {
    stream_envelopes_at(&board_root(home, DEFAULT_PROJECT))
}

/// #2117 board-root variant of [`stream_envelopes`].
pub(crate) fn stream_envelopes_at(board: &Path) -> anyhow::Result<Vec<TaskEventEnvelope>> {
    let mut all = Vec::new();

    let archive = archive_dir(board);
    if archive.is_dir() {
        let mut archives: Vec<std::path::PathBuf> = std::fs::read_dir(&archive)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect();
        archives.sort();
        for path in archives {
            read_envelopes_strict(&path, &mut all)?;
        }
    }

    let lp = log_path(board);
    if lp.exists() {
        read_envelopes_strict(&lp, &mut all)?;
    }

    sort_envelopes(&mut all);
    Ok(all)
}

/// Sort envelopes by timestamp (absolute nanos) → instance → seq.
/// Schwartzian transform: parse timestamps once into a parallel key vec,
/// then sort both in lockstep — avoids re-parsing on every comparison.
fn sort_envelopes(envelopes: &mut [TaskEventEnvelope]) {
    let keys: Vec<i64> = envelopes
        .iter()
        .map(|e| {
            chrono::DateTime::parse_from_rfc3339(&e.timestamp)
                .map(|d| d.timestamp_nanos_opt().unwrap_or(0))
                .unwrap_or(0)
        })
        .collect();
    let mut indices: Vec<usize> = (0..envelopes.len()).collect();
    indices.sort_by(|&a, &b| {
        keys[a]
            .cmp(&keys[b])
            .then_with(|| envelopes[a].instance.0.cmp(&envelopes[b].instance.0))
            .then_with(|| envelopes[a].seq.cmp(&envelopes[b].seq))
    });
    let gathered: Vec<TaskEventEnvelope> = indices.iter().map(|&i| envelopes[i].clone()).collect();
    envelopes.clone_from_slice(&gathered);
}

fn read_envelopes_strict(path: &Path, out: &mut Vec<TaskEventEnvelope>) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    for (lineno, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // #1988: distinguish three failure shapes by their *cause* — only the
        // first is skippable; the other two stay deliberately fail-closed.
        //
        // #1990 item 4 CONTRACT: both fail-closed `bail!` messages below MUST
        // contain the substring "fail-closed". `surface_failclosed_replay_once`
        // classifies a replay error as the operator-surfaceable forward-incompat
        // class by exactly that substring (so a frozen board raises an alert,
        // while a transient IO error does not). Reword these messages only in
        // lockstep with that classifier, or the board-freeze alert goes silent.
        //
        // (1) CORRUPT line — not even valid JSON: a torn/half-written tail from a
        //     crash mid-append (`append_lines_under_lock` appends in place), or a
        //     disk glitch. It carries no recoverable event, so SKIP it and keep
        //     replaying the rest — one bad byte must not brick the whole task
        //     board (the old behaviour aborted the entire replay → board
        //     unreadable). Warned here on every replay and, at boot, quarantined
        //     + rewritten out of the log by `recover_half_writes`, so it is
        //     neither accumulated nor silently lost. Mirrors `inbox`'s read
        //     paths, which already skip unparseable lines.
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    line = lineno + 1,
                    error = %e,
                    "#1988: skipping corrupt (non-JSON) task-event line (half-write/disk glitch) — replay continues"
                );
                continue;
            }
        };
        // (2) FUTURE-VERSION line — valid JSON, but `schema_version` exceeds what
        //     this binary understands: a VALID event a NEWER daemon wrote. Keep
        //     the WHOLE-FILE fail-closed ABORT (deliberately NOT a per-record
        //     skip) on CORRECTNESS grounds: an event we cannot decode may change
        //     task state in ways we cannot see (a newer done/reassign/close
        //     variant), so skipping it and serving the rest would have this
        //     daemon act on a PARTIAL, MISREAD board — e.g. re-dispatching a task
        //     a newer event already closed. Aborting refuses to operate until the
        //     operator runs a binary new enough to read the whole log. This is
        //     the intended forward-compat protection.
        let version = value
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if version > SCHEMA_VERSION as u64 {
            anyhow::bail!(
                "{}:{}: schema_version {} > supported {} (forward-compat fail-closed — a newer daemon wrote this log; refusing to operate on a partial board)",
                path.display(),
                lineno + 1,
                version,
                SCHEMA_VERSION
            );
        }
        // (3) WELL-FORMED-but-undeserializable line — valid JSON at a supported
        //     version that still won't decode into a `TaskEventEnvelope`: an
        //     unknown event `kind` or a missing required field (a newer daemon
        //     that added an event variant WITHOUT bumping the version, or a
        //     hand-edit). Same hazard as case (2) — a real event we cannot apply
        //     — so ABORT, not skip: serving a board that silently omits it is the
        //     same partial/misread-state risk. Garbage from a half-write fails
        //     case (1)'s JSON parse first and never reaches here.
        let env: TaskEventEnvelope = serde_json::from_value(value).map_err(|e| {
            anyhow::anyhow!(
                "{}:{}: replay aborts on undeserializable envelope at supported schema (fail-closed): {e}",
                path.display(),
                lineno + 1
            )
        })?;
        out.push(env);
    }
    Ok(())
}

/// #1988: scan the live `task_events.jsonl` for corrupt (unparseable) lines — a
/// torn/half-written tail from a crash mid-append (`append_lines_under_lock`
/// appends in place, not via tmp+rename) — quarantine them under
/// `task_events.recovery/<ts>/` and rewrite the log keeping only the good lines.
/// Mirrors [`crate::inbox::recover_half_writes`]; call once at daemon startup.
///
/// "Corrupt" here means strictly NON-JSON (a half-write tail or disk glitch) —
/// the only case [`read_envelopes_strict`] skips. A future-version or unknown
/// event-variant line is still valid JSON and is KEPT, so the fail-closed gate
/// in [`read_envelopes_strict`] keeps owning those cases (recovery must never
/// auto-drop a newer daemon's events). Only the live log is scanned: archives
/// under `task_events_archive/` are written atomically (tmp+rename) so they
/// cannot hold a half-write, and the read-path skip is the backstop for any
/// other archive corruption.
pub fn recover_half_writes(home: &Path) {
    recover_half_writes_at(&board_root(home, DEFAULT_PROJECT))
}

/// #2117 board-root variant of [`recover_half_writes`].
pub(crate) fn recover_half_writes_at(board: &Path) {
    use std::io::Write;
    let path = log_path(board);
    if !path.exists() {
        return;
    }
    // Same lock `append` takes for this physical log (event_log derives
    // `<log>.jsonl.lock`), so a concurrent early-boot append cannot race the
    // rewrite.
    let lock_path = path.with_extension("jsonl.lock");
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_path) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut bad: Vec<&str> = Vec::new();
    for l in content.lines() {
        // Keep anything that is valid JSON (incl. future-version / unknown-variant
        // lines — those are owned by the read-path fail-closed gate). Quarantine
        // only true non-JSON garbage (half-write tails, disk glitches).
        if l.trim().is_empty() || serde_json::from_str::<serde_json::Value>(l).is_ok() {
            kept.push(l);
        } else {
            bad.push(l);
        }
    }
    if bad.is_empty() {
        return;
    }
    // Forensics: quarantine only the corrupt line(s) — never silently destroy.
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let recovery_dir = board.join("task_events.recovery").join(&ts);
    let _ = std::fs::create_dir_all(&recovery_dir);
    if let Ok(mut rf) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(recovery_dir.join(format!("{LOG_NAME}.jsonl")))
    {
        for l in &bad {
            let _ = writeln!(rf, "{l}");
        }
        // fsync the forensic copy before the hot log is rewritten below: if we
        // crash between the two, the quarantined line must already be durable or
        // it is lost for good (the rewrite drops it from the live log).
        let _ = rf.sync_all();
    }
    // Rewrite the hot log with only the good lines via tmp + fsync + atomic
    // rename (mirrors compaction's write-back) so every valid event survives.
    let tmp = path.with_extension("jsonl.tmp");
    let rewrite = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        for l in &kept {
            writeln!(f, "{l}")?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        crate::store::fsync_parent_dir(&path); // AUDIT2-015: durable rename
        Ok(())
    })();
    if rewrite.is_ok() {
        tracing::warn!(
            tag = "#1988-task-events-recovered",
            quarantined = bad.len(),
            kept = kept.len(),
            recovery_dir = %recovery_dir.display(),
            "recovered corrupt task-event line(s) — quarantined + hot log rewritten with good lines only"
        );
    }
}

// ── Compaction ─────────────────────────────────────────────────────

/// Archive all but the last [`COMPACTION_KEEP`] envelopes from the hot
/// log into a timestamped file under `task_events_archive/`. Idempotent:
/// short-circuits when the hot log already fits the threshold. Holds the
/// same lock as [`append`] so concurrent appenders see a consistent
/// hot-file at all times.
/// Home-default wrapper; production compaction runs through [`compact_at`]
/// (board-scoped) via [`maybe_compact_events`]. Retained for API symmetry +
/// test use.
#[allow(dead_code)]
pub fn compact(home: &Path) -> anyhow::Result<()> {
    compact_at(&board_root(home, DEFAULT_PROJECT))
}

/// #2117 board-root variant of [`compact`]. Wired into the append path via
/// [`maybe_compact_events`] (this was dead code — the source of the unbounded
/// hot-log growth this change fixes).
pub(crate) fn compact_at(board: &Path) -> anyhow::Result<()> {
    compact_at_with_keep(board, COMPACTION_KEEP)
}

/// Threshold-injected core of [`compact_at`] (#2135-style testability — tests
/// pass a small `keep` rather than generating `COMPACTION_KEEP` events).
/// Byte-identical to the prior `compact_at` body when `keep == COMPACTION_KEEP`.
/// The older slice is ARCHIVED (replay folds archive + hot → no data loss); the
/// per-instance seq sidecar is independent of this rewrite, so an instance whose
/// events are all archived keeps a correct seq high-water ([`next_seq_under_lock`]).
fn compact_at_with_keep(board: &Path, keep: usize) -> anyhow::Result<()> {
    let log_path = log_path(board);
    if !log_path.exists() {
        return Ok(());
    }
    let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();

    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        let content = std::fs::read_to_string(log_path)?;
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() <= keep {
            return Ok(Vec::new());
        }
        let split = lines.len() - keep;
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
        // S1: compaction just rewrote (shrank) the hot log. Invalidate the
        // replay cache so the next reader replays the compacted file instead of
        // a stale entry — mirrors every append path (:1124/:1211/:1297). Today
        // this is correct-by-ACCIDENT (the shorter file changes the
        // `(len, mtime)` cache key → key miss); the explicit bump makes it
        // correct-by-CONTRACT and survives any future cache-key change.
        invalidate_replay_cache();
        // We've already rewritten the hot file — return no extra lines
        // to append. (H10: no SEQ_CACHE to invalidate — `max_seq_for_instance`
        // always re-scans the on-disk file, so the atomic replace is observed.)
        Ok(Vec::new())
    })
}

/// Opportunistic, non-fatal hot-log compaction after an append (mirrors
/// `tasks::board_router::maybe_compact_index`). [`compact_at`] ARCHIVES — never
/// drops — the older slice, so replay (archive + hot) is unaffected. A failure
/// never propagates: the append already committed; compaction is opportunistic
/// cleanup.
///
/// #2389 E1 hysteresis: `hot_lines` is the post-append hot-log line count handed
/// down from the append path's existing scan ([`next_seq_under_lock`] + `count`).
/// Compaction is SKIPPED entirely (no read, no rewrite) until the hot log exceeds
/// [`COMPACTION_HIGH_WATER`], then [`compact_at`] trims it back to
/// [`COMPACTION_KEEP`] in one pass — amortizing the rewrite + archive-segment
/// churn to once per `HIGH_WATER - KEEP` appends instead of every append.
///
/// The hint is lock-internal-exact for THIS appender; a concurrent cross-process
/// append can only make the real count higher, so at worst compaction fires one
/// batch late (the next append's larger hint catches it). `compact_at` re-reads
/// the real file and self-gates on `COMPACTION_KEEP`, so a stale hint can never
/// mis-trim — it only decides whether to bother reading.
fn maybe_compact_events(board: &Path, hot_lines: usize) {
    if hot_lines <= COMPACTION_HIGH_WATER {
        return;
    }
    if let Err(e) = compact_at(board) {
        tracing::warn!(error = %e, "task_events compaction failed (non-fatal; append already durable)");
    }
}

#[cfg(test)]
mod review_repro_tasks;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
