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

/// Hot-file event count above which [`compact`] archives the older slice.
#[allow(dead_code)]
pub const COMPACTION_KEEP: usize = 10_000;

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
            if let DoneSource::OperatorManual { result, .. } = source {
                t.result = result.clone();
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
    trimmed
        .chars()
        .map(|c| match c {
            '/' => '_', // an `owner/repo` becomes `owner_repo` after the pass below
            c if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') => c,
            _ => '_',
        })
        .collect()
}

// ── Append: single + batch ─────────────────────────────────────────

fn log_path(board: &Path) -> PathBuf {
    board.join(format!("{LOG_NAME}.jsonl"))
}

fn archive_dir(board: &Path) -> PathBuf {
    board.join("task_events_archive")
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

    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
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
                emitter_id: emitter_id.clone(),
                event,
            };
            lines.push(serde_json::to_string(&envelope)?);
        }
        Ok(lines)
    })?;
    invalidate_replay_cache();
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
    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        // FRESH replay under the lock — authoritative committed history.
        let state = replay_uncached(board)?;
        if let Err(reason) = precondition(&state) {
            rejection = Some(reason);
            return Ok(Vec::new()); // empty ⇒ no write
        }
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

    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
        // FRESH replay under the lock — authoritative committed history.
        let state = replay_uncached(board)?;
        if let Err(reason) = precondition(&state) {
            rejection = Some(reason);
            return Ok(Vec::new()); // empty ⇒ no write
        }
        let seq = max_seq_for_instance(log_path, &instance)? + 1;
        assigned_seq = Some(seq);
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
/// appends are agent/human-paced (not a hot loop) and batches share one scan —
/// NOT because the file is bounded: `compact` (which would cap it at
/// `COMPACTION_KEEP`) is currently dead code with no production caller, so the
/// hot log grows unbounded. Any cross-process-correct approach must re-read the
/// file when it changes anyway; the previous cache was O(1) only by trusting
/// stale cross-process state — the exact bug fixed here.
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
#[allow(dead_code)]
pub fn compact(home: &Path) -> anyhow::Result<()> {
    compact_at(&board_root(home, DEFAULT_PROJECT))
}

/// #2117 board-root variant of [`compact`].
pub(crate) fn compact_at(board: &Path) -> anyhow::Result<()> {
    let log_path = log_path(board);
    if !log_path.exists() {
        return Ok(());
    }
    let suffix = chrono::Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string();

    crate::event_log::append_lines_under_lock(board, LOG_NAME, |log_path| {
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
        // to append. (H10: no SEQ_CACHE to invalidate — `max_seq_for_instance`
        // always re-scans the on-disk file, so the atomic replace is observed.)
        Ok(Vec::new())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
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
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
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
    #[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
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
    #[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
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

    // ── #1988: corrupt-line resilience ──────────────────────────────────

    /// #1988 shape 1 — a CORRUPT (non-JSON) line in the MIDDLE of a real board
    /// lifecycle (create → claim → update → done) must be SKIPPED, not abort the
    /// whole replay. Goes through the real producer (`append`) for the good lines
    /// and the real consumer (`replay`) — not a unit-injected `read_envelopes_strict`.
    #[test]
    fn replay_skips_corrupt_midfile_line_keeps_full_lifecycle() {
        let home = tmp_home("corrupt-skip");
        let inst = InstanceName::from("dev-impl-1");
        let log = home.join("task_events.jsonl");

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
        // Simulate a crash-torn / disk-glitched line landing mid-log.
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
            writeln!(f, "this is not valid json {{{{ truncated").unwrap();
        }
        append(
            &home,
            &inst,
            TaskEvent::DescriptionUpdated {
                task_id: "t-X".into(),
                by: "agent".into(),
                description: "updated desc".into(),
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

        // The garbage really is in the log...
        let raw = fs::read_to_string(&log).unwrap();
        assert!(
            raw.contains("not valid json"),
            "fixture must contain the bad line"
        );
        // ...yet replay folds the full lifecycle, skipping it (no abort).
        let state = replay(&home).expect("corrupt mid-line must NOT brick replay");
        let task = state
            .tasks
            .get(&TaskId::from("t-X"))
            .expect("task survives the corrupt line");
        assert_eq!(task.status, TaskStatus::Done);
        fs::remove_dir_all(&home).ok();
    }

    /// #1988 shape 2 — a half-written TAIL line (crash mid-append) is quarantined
    /// and rewritten out of the hot log by `recover_half_writes` (the real boot
    /// entry), preserving every good event and leaving a forensic copy.
    #[test]
    fn recover_half_writes_quarantines_torn_tail() {
        let home = tmp_home("recover-tail");
        let inst = InstanceName::from("u");
        let log = home.join("task_events.jsonl");

        append(&home, &inst, sample_event("t-Y")).unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Done {
                task_id: "t-Y".into(),
                by: "agent".into(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
        )
        .unwrap();
        // A torn trailing fragment from a crash mid-append (not valid JSON).
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
            // Unique sentinel — a generic fragment like "timesta" is a substring
            // of the good lines' "timestamp" field and would give a false match.
            write!(f, "{{\"schema_version\":2,\"seq\":99,\"TORN_SENTINEL_ZZ").unwrap();
        }

        recover_half_writes(&home);

        // The torn line is gone from the hot log; every remaining line is valid JSON.
        let rewritten = fs::read_to_string(&log).unwrap();
        assert!(
            !rewritten.contains("TORN_SENTINEL_ZZ"),
            "torn tail must be removed"
        );
        for l in rewritten.lines().filter(|l| !l.trim().is_empty()) {
            serde_json::from_str::<serde_json::Value>(l).expect("every kept line is valid JSON");
        }
        // It is quarantined, not silently destroyed.
        let rec_root = home.join("task_events.recovery");
        let sub = fs::read_dir(&rec_root)
            .unwrap()
            .next()
            .expect("a recovery subdir exists")
            .unwrap()
            .path();
        let quarantined = fs::read_to_string(sub.join("task_events.jsonl")).unwrap();
        assert!(
            quarantined.contains("TORN_SENTINEL_ZZ"),
            "torn tail preserved for forensics"
        );
        // The board still replays cleanly with the good events.
        let state = replay(&home).expect("replay clean after recovery");
        assert_eq!(
            state.tasks.get(&TaskId::from("t-Y")).unwrap().status,
            TaskStatus::Done
        );
        fs::remove_dir_all(&home).ok();
    }

    /// #1988 shape 3 — recovery must NOT auto-drop a newer daemon's events: a
    /// FUTURE-VERSION line is valid JSON, so `recover_half_writes` KEEPS it and the
    /// read-path fail-closed gate still fires on replay. (Proves the corrupt-skip
    /// and forward-compat-abort responsibilities stay cleanly separated.)
    #[test]
    #[serial(task_replay_latch)] // #1990 item 4: shares the global fail-closed-emit latch
    fn recover_keeps_future_version_line_replay_still_fail_closed() {
        let home = tmp_home("recover-future");
        let inst = InstanceName::from("u");
        let log = home.join("task_events.jsonl");
        append(&home, &inst, sample_event("t-Z")).unwrap();
        let future = serde_json::json!({
            "schema_version": 999,
            "seq": 2,
            "timestamp": "2026-04-27T00:00:00Z",
            "instance": "newer-daemon",
            "event": {"kind": "Unblocked", "task_id": "t-Z"}
        });
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&log).unwrap();
            writeln!(f, "{future}").unwrap();
        }

        recover_half_writes(&home);

        let after = fs::read_to_string(&log).unwrap();
        assert!(
            after.contains("\"schema_version\":999"),
            "recovery must keep the future-version line (it is valid JSON, not garbage)"
        );
        let err = replay(&home).expect_err("future-version still fail-closed after recovery");
        assert!(
            err.to_string().contains("forward-compat fail-closed"),
            "got: {err}"
        );
        fs::remove_dir_all(&home).ok();
    }

    /// #1990 item 4: a fail-closed replay (the board freezes while the per-tick
    /// callers swallow the Err) must surface ONE operator-visible event_log entry
    /// per boot — and a second fail-closed replay in the same boot must NOT emit a
    /// duplicate (latched). Serialized with the other latch-tripping tests so the
    /// process-global latch reset is uninterrupted.
    #[test]
    #[serial(task_replay_latch)]
    fn replay_fail_closed_surfaces_operator_event_once() {
        let home = tmp_home("failclosed-visible");
        REPLAY_FAILCLOSED_EVENT_EMITTED.store(false, std::sync::atomic::Ordering::Relaxed);
        // A future-version record → replay fail-closes (#1992 forward-compat).
        let line = serde_json::json!({
            "schema_version": 999,
            "seq": 1,
            "timestamp": "2026-04-27T00:00:00Z",
            "instance": "newer-daemon",
            "event": {"kind": "Unblocked", "task_id": "t-X"}
        });
        fs::write(home.join("task_events.jsonl"), format!("{line}\n")).unwrap();
        // Two fail-closed replays in the same boot (Err is not cached, so both
        // re-run replay_uncached and reach the surface helper).
        assert!(replay(&home).is_err());
        assert!(replay(&home).is_err());
        // Exactly one operator event was emitted (latched).
        let elog = fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert_eq!(
            elog.matches("task_replay_fail_closed").count(),
            1,
            "fail-closed replay must surface exactly one operator event per boot, got: {elog}"
        );
        fs::remove_dir_all(&home).ok();
    }

    /// #1990 item 4 (reviewer-2 minor 1): the boundary that keeps disk jitter from
    /// becoming a false alarm — a transient IO-class replay error (no "fail-closed"
    /// substring) must NOT surface an operator event. Guards the substring
    /// classifier against over-firing.
    #[test]
    #[serial(task_replay_latch)]
    fn transient_io_error_does_not_surface_operator_event() {
        let home = tmp_home("io-no-surface");
        REPLAY_FAILCLOSED_EVENT_EMITTED.store(false, std::sync::atomic::Ordering::Relaxed);
        // A bare IO-class error (what a vanished/locked file yields) — not the
        // forward-compat "fail-closed" class.
        let io_err = anyhow::anyhow!("No such file or directory (os error 2)");
        surface_failclosed_replay_once(&home, &io_err);
        let elog = fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            !elog.contains("task_replay_fail_closed"),
            "a transient IO error must NOT surface an operator event (false-alarm guard): {elog}"
        );
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
                emitter_id: None,
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
            emitter_id: None,
            event: sample_event("t-TZ"),
        };
        let env_b = TaskEventEnvelope {
            schema_version: 1,
            seq: 2,
            timestamp: "2026-04-27T00:00:00Z".into(),
            instance: InstanceName::from("u"),
            emitter_id: None,
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
                emitter_id: None,
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
                emitter_id: None,
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
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
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
                TaskStatus::Backlog => vec![TaskEvent::MovedToBacklog {
                    task_id: tid.clone(),
                }],
                TaskStatus::InReview => vec![TaskEvent::MovedToReview {
                    task_id: tid.clone(),
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
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
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
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
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
                emitter_id: None,
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
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
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
                emitter_id: None,
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
                emitter_id: None,
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
                emitter_id: None,
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

    // ── Sprint 46 P3: audit trail round-trip tests ──────────────────

    #[test]
    fn emitter_id_round_trips_through_serde() {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 1,
            timestamp: "2026-05-04T00:00:00Z".into(),
            instance: InstanceName::from("dev"),
            emitter_id: Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
            event: sample_event("t-rt"),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("a1b2c3d4"));
        let deser: TaskEventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            deser.emitter_id.as_deref(),
            Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890")
        );
    }

    #[test]
    fn emitter_id_none_omitted_from_json() {
        let env = TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq: 1,
            timestamp: "2026-05-04T00:00:00Z".into(),
            instance: InstanceName::from("dev"),
            emitter_id: None,
            event: sample_event("t-rt2"),
        };
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(
            !json.contains("emitter_id"),
            "None emitter_id must be omitted"
        );
        let deser: TaskEventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser.emitter_id, None);
    }

    // ── Sprint 55 P0-C — bind opt-out flag schema tests ─────────────

    #[test]
    fn created_event_v1_envelope_default_bind_none() {
        // Pre-P0-C envelopes have no `bind` field; serde must default to
        // None so existing tasks.json migrations + any v1 log replay
        // continue to work exactly as before.
        let v1_json = r#"{
            "kind": "Created",
            "task_id": "t-v1",
            "title": "v1 task",
            "description": "",
            "priority": "normal",
            "owner": null,
            "due_at": null,
            "depends_on": [],
            "routed_to": null,
            "branch": null
        }"#;
        let event: TaskEvent = serde_json::from_str(v1_json).expect("v1 envelope must deserialize");
        match event {
            TaskEvent::Created { bind, .. } => assert_eq!(bind, None),
            _ => panic!("expected Created variant"),
        }
    }

    #[test]
    fn created_event_round_trips_bind_some_false_through_replay() {
        // Append a Created event with bind=Some(false), replay the log,
        // and verify TaskRecord.bind preserves the opt-out signal end to
        // end (event log → apply → in-memory record).
        let home = tmp_home("p0c_bind_round_trip");
        let inst = InstanceName::from("dev");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-rca"),
                title: "rca task".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: Some(false),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        let state = replay(&home).expect("replay");
        let task = state
            .tasks
            .get(&TaskId::from("t-rca"))
            .expect("task in state");
        assert_eq!(task.bind, Some(false));
        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────────────────────────────────────────────────────
    // Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — schema field
    // tests pinning eta_secs round-trip + dispatched_at semantics.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn task_schema_dispatched_at_set_on_status_in_progress_transition() {
        // Lead spec name: dispatched_at must be auto-set the FIRST
        // time the task transitions to in_progress.
        let home = tmp_home("schema-dispatched-at");
        let inst = InstanceName::from("test");
        let tid = TaskId::from("t-disp");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "x".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: Some(60),
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        // Pre-claim: dispatched_at is None.
        let pre = replay(&home).unwrap();
        let pre_t = pre.tasks.get(&tid).unwrap();
        assert!(pre_t.started_at.is_none(), "pre-claim: no dispatched_at");

        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        // Post-claim, pre-in_progress: still None.
        let mid = replay(&home).unwrap();
        assert!(mid.tasks.get(&tid).unwrap().started_at.is_none());

        append(
            &home,
            &inst,
            TaskEvent::InProgress {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        // Post-in_progress: dispatched_at is set.
        let post = replay(&home).unwrap();
        let post_t = post.tasks.get(&tid).unwrap();
        assert!(
            post_t.started_at.is_some(),
            "in_progress must set dispatched_at: {post_t:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_schema_dispatched_at_idempotent_on_subsequent_in_progress() {
        // Defensive: a Released → Claimed → InProgress cycle must
        // NOT overwrite the original dispatched_at — anti-stall
        // scanner cares about "when did work first start", not the
        // latest checkpoint.
        let home = tmp_home("schema-disp-idem");
        let inst = InstanceName::from("test");
        let tid = TaskId::from("t-disp-idem");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "x".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: Some(60),
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::InProgress {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        let first_dispatched = replay(&home)
            .unwrap()
            .tasks
            .get(&tid)
            .unwrap()
            .started_at
            .clone();
        assert!(first_dispatched.is_some());

        // Release → Claim → InProgress again. dispatched_at must
        // remain unchanged.
        append(
            &home,
            &inst,
            TaskEvent::Released {
                task_id: tid.clone(),
                reason: "test".into(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        // Sleep briefly so a hypothetical overwrite would surface
        // as a different timestamp.
        std::thread::sleep(std::time::Duration::from_millis(50));
        append(
            &home,
            &inst,
            TaskEvent::InProgress {
                task_id: tid.clone(),
                by: inst.clone(),
            },
        )
        .unwrap();
        let second_dispatched = replay(&home)
            .unwrap()
            .tasks
            .get(&tid)
            .unwrap()
            .started_at
            .clone();
        assert_eq!(
            first_dispatched, second_dispatched,
            "dispatched_at must NOT be overwritten on re-entry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_schema_eta_secs_round_trips_from_created_event() {
        // Defensive: eta_secs supplied at Created event must
        // surface on TaskRecord post-replay.
        let home = tmp_home("schema-eta-rt");
        let inst = InstanceName::from("test");
        let tid = TaskId::from("t-eta-rt");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "x".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: Some(7200),
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        let task = replay(&home).unwrap().tasks.get(&tid).cloned().unwrap();
        assert_eq!(task.eta_secs, Some(7200), "eta_secs must round-trip");
        std::fs::remove_dir_all(&home).ok();
    }

    /// H10: after compaction atomically replaces the hot file, the next append
    /// must still produce the correct monotonic seq and replay must fold both
    /// events. `max_seq_for_instance` always re-scans the on-disk file, so the
    /// replace is observed with no stale high-water mark.
    #[test]
    fn append_after_compaction_produces_correct_monotonic_seq() {
        let home = tmp_home("compact-seq");
        let inst = InstanceName::from("dev");

        // Append events past the compaction threshold
        let total = COMPACTION_KEEP + 10;
        let log = home.join("task_events.jsonl");
        let mut lines = String::new();
        for i in 1..=total {
            let env = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq: i as u64,
                timestamp: format!("2026-05-24T{:02}:{:02}:00Z", (i / 60) % 24, i % 60),
                instance: inst.clone(),
                emitter_id: None,
                event: TaskEvent::Unblocked {
                    task_id: format!("t-{i}").as_str().into(),
                },
            };
            lines.push_str(&serde_json::to_string(&env).unwrap());
            lines.push('\n');
        }
        fs::write(&log, &lines).unwrap();

        let seq_before = append(&home, &inst, sample_event("t-pre-compact")).unwrap();
        assert_eq!(seq_before, total as u64 + 1);

        // Compact — atomically rewrites the hot file (keeps the latest events).
        compact(&home).unwrap();

        // Post-compaction append must still produce correct monotonic seq
        let seq_after = append(&home, &inst, sample_event("t-post-compact")).unwrap();
        assert_eq!(
            seq_after,
            seq_before + 1,
            "post-compaction seq must be monotonically next"
        );

        // Replay sees both events
        let state = replay(&home).unwrap();
        assert!(state.tasks.contains_key(&TaskId::from("t-pre-compact")));
        assert!(state.tasks.contains_key(&TaskId::from("t-post-compact")));
        fs::remove_dir_all(&home).ok();
    }

    // ── parent_id tree structure tests ──────────────────────────────

    #[test]
    fn parent_id_round_trips_through_replay() {
        let home = tmp_home("parent-rt");
        let inst = InstanceName::from("dev");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-parent"),
                title: "parent".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-child"),
                title: "child".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: Some(TaskId::from("t-parent")),
            },
        )
        .unwrap();
        let state = replay(&home).unwrap();
        let parent = state.tasks.get(&TaskId::from("t-parent")).unwrap();
        assert_eq!(parent.parent_id, None);
        let child = state.tasks.get(&TaskId::from("t-child")).unwrap();
        assert_eq!(child.parent_id, Some(TaskId::from("t-parent")));
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn parent_id_v1_envelope_defaults_to_none() {
        let home = tmp_home("parent-v1");
        let log = home.join("task_events.jsonl");
        let v1_line = serde_json::json!({
            "schema_version": 1,
            "seq": 1,
            "timestamp": "2026-05-25T00:00:00Z",
            "instance": "v1-emitter",
            "event": {
                "kind": "Created",
                "task_id": "t-old",
                "title": "old task",
                "description": "",
                "priority": "normal",
                "owner": null
            }
        });
        fs::write(&log, format!("{v1_line}\n")).unwrap();
        let state = replay(&home).unwrap();
        let task = state.tasks.get(&TaskId::from("t-old")).unwrap();
        assert_eq!(task.parent_id, None, "v1 envelope missing parent_id → None");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cascade_cancel_cancels_open_and_claimed_children() {
        let home = tmp_home("cascade");
        let inst = InstanceName::from("dev");
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-root"),
                title: "root".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-child-open"),
                title: "open child".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: Some(TaskId::from("t-root")),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-child-claimed"),
                title: "claimed child".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: Some(TaskId::from("t-root")),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Claimed {
                task_id: TaskId::from("t-child-claimed"),
                by: inst.clone(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-child-done"),
                title: "done child".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: Some(TaskId::from("t-root")),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Done {
                task_id: TaskId::from("t-child-done"),
                by: inst.clone(),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-child-inprog"),
                title: "in-progress child".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: Some(TaskId::from("t-root")),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::InProgress {
                task_id: TaskId::from("t-child-inprog"),
                by: inst.clone(),
            },
        )
        .unwrap();
        append(
            &home,
            &inst,
            TaskEvent::Created {
                task_id: TaskId::from("t-unrelated"),
                title: "unrelated".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();

        // Cancel the parent
        append(
            &home,
            &inst,
            TaskEvent::Cancelled {
                task_id: TaskId::from("t-root"),
                by: inst.clone(),
                reason: "test cascade".into(),
            },
        )
        .unwrap();

        let state = replay(&home).unwrap();
        assert_eq!(
            state.tasks.get(&TaskId::from("t-root")).unwrap().status,
            TaskStatus::Cancelled
        );
        assert_eq!(
            state
                .tasks
                .get(&TaskId::from("t-child-open"))
                .unwrap()
                .status,
            TaskStatus::Cancelled,
            "open child must be cascade-cancelled"
        );
        assert_eq!(
            state
                .tasks
                .get(&TaskId::from("t-child-claimed"))
                .unwrap()
                .status,
            TaskStatus::Cancelled,
            "claimed child must be cascade-cancelled"
        );
        assert_eq!(
            state
                .tasks
                .get(&TaskId::from("t-child-done"))
                .unwrap()
                .status,
            TaskStatus::Done,
            "done child must NOT be cascade-cancelled"
        );
        assert_eq!(
            state
                .tasks
                .get(&TaskId::from("t-child-inprog"))
                .unwrap()
                .status,
            TaskStatus::InProgress,
            "in-progress child must NOT be cascade-cancelled"
        );
        assert_eq!(
            state
                .tasks
                .get(&TaskId::from("t-unrelated"))
                .unwrap()
                .status,
            TaskStatus::Open,
            "unrelated task must NOT be affected"
        );
        fs::remove_dir_all(&home).ok();
    }

    // ── Perf Group 1A: replay cache tests ──────────────────────────

    #[test]
    fn board_root_default_is_home_real_project_is_subtree() {
        // #2117 P0 seam: the default/fleet/empty project maps to `home` itself
        // (this is what makes the whole refactor byte-identical), while a real
        // project id resolves to its own isolated subtree under `home/boards/`.
        let home = tmp_home("board-root");
        assert_eq!(board_root(&home, DEFAULT_PROJECT), home);
        assert_eq!(board_root(&home, "fleet"), home);
        assert_eq!(board_root(&home, ""), home);

        let proj = board_root(&home, "owner/repo");
        assert_ne!(
            proj, home,
            "a real project must not collide with the home board"
        );
        assert!(proj.starts_with(home.join("boards")));
        let slug = proj.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !slug.contains('/'),
            "slug must be filesystem-safe, got {slug:?}"
        );
        // Deterministic / round-trippable: same id → same root; distinct ids → distinct roots.
        assert_eq!(board_root(&home, "owner/repo"), proj);
        assert_ne!(board_root(&home, "other/repo"), proj);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn replay_cache_hit_returns_same_result() {
        let home = tmp_home("cache-hit");
        let inst = InstanceName::from("a");
        append(&home, &inst, sample_event("t-1")).unwrap();

        let r1 = replay(&home).unwrap();
        let r2 = replay(&home).unwrap();
        assert_eq!(r1.tasks.len(), r2.tasks.len());
        assert_eq!(r1.events_folded, r2.events_folded);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn replay_cache_invalidated_after_append() {
        let home = tmp_home("cache-inv");
        let inst = InstanceName::from("a");
        append(&home, &inst, sample_event("t-1")).unwrap();

        let r1 = replay(&home).unwrap();
        assert_eq!(r1.tasks.len(), 1);

        append(&home, &inst, sample_event("t-2")).unwrap();

        let r2 = replay(&home).unwrap();
        assert_eq!(r2.tasks.len(), 2, "cache must be invalidated after append");
        fs::remove_dir_all(&home).ok();
    }

    // ── Perf Group 1B: sort_envelopes Schwartzian consistency ──────

    #[test]
    fn sort_envelopes_schwartzian_matches_naive() {
        let mk = |ts: &str, inst: &str, seq: u64| TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: ts.to_string(),
            instance: InstanceName::from(inst),
            emitter_id: None,
            event: sample_event("t-sort"),
        };
        let mut envelopes = vec![
            mk("2026-05-26T03:00:00Z", "b", 2),
            mk("2026-05-26T01:00:00Z", "a", 1),
            mk("2026-05-26T02:00:00Z", "a", 1),
            mk("2026-05-26T02:00:00Z", "b", 1),
            mk("2026-05-26T02:00:00Z", "a", 2),
        ];
        sort_envelopes(&mut envelopes);
        let order: Vec<(&str, &str, u64)> = envelopes
            .iter()
            .map(|e| (e.timestamp.as_str(), e.instance.0.as_str(), e.seq))
            .collect();
        assert_eq!(
            order,
            vec![
                ("2026-05-26T01:00:00Z", "a", 1),
                ("2026-05-26T02:00:00Z", "a", 1),
                ("2026-05-26T02:00:00Z", "a", 2),
                ("2026-05-26T02:00:00Z", "b", 1),
                ("2026-05-26T03:00:00Z", "b", 2),
            ]
        );
    }

    #[test]
    fn sort_envelopes_reverse_and_interleaved() {
        let mk = |ts: &str, inst: &str, seq: u64| TaskEventEnvelope {
            schema_version: SCHEMA_VERSION,
            seq,
            timestamp: ts.to_string(),
            instance: InstanceName::from(inst),
            emitter_id: None,
            event: sample_event("t-perm"),
        };
        // Reverse order input
        let mut rev = vec![
            mk("2026-05-26T04:00:00Z", "z", 3),
            mk("2026-05-26T03:00:00Z", "y", 2),
            mk("2026-05-26T02:00:00Z", "x", 1),
            mk("2026-05-26T01:00:00Z", "w", 1),
        ];
        sort_envelopes(&mut rev);
        let ts: Vec<&str> = rev.iter().map(|e| e.timestamp.as_str()).collect();
        assert_eq!(
            ts,
            vec![
                "2026-05-26T01:00:00Z",
                "2026-05-26T02:00:00Z",
                "2026-05-26T03:00:00Z",
                "2026-05-26T04:00:00Z",
            ]
        );

        // Interleaved: same timestamp, different instances and seqs
        let mut interleaved = vec![
            mk("2026-05-26T01:00:00Z", "c", 2),
            mk("2026-05-26T01:00:00Z", "a", 3),
            mk("2026-05-26T01:00:00Z", "b", 1),
            mk("2026-05-26T01:00:00Z", "a", 1),
            mk("2026-05-26T01:00:00Z", "a", 2),
            mk("2026-05-26T01:00:00Z", "c", 1),
        ];
        sort_envelopes(&mut interleaved);
        let order: Vec<(&str, u64)> = interleaved
            .iter()
            .map(|e| (e.instance.0.as_str(), e.seq))
            .collect();
        assert_eq!(
            order,
            vec![("a", 1), ("a", 2), ("a", 3), ("b", 1), ("c", 1), ("c", 2)]
        );
    }
}

#[cfg(test)]
mod review_repro_tasks;
