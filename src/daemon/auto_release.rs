//! #870: daemon auto-release worktree on reviewer VERIFIED verdict.
//!
//! Eliminates the lease-conflict cycle observed across PR-A/B/C of the
//! #852 residual series (every cycle hit "branch already checked out
//! at reviewer's worktree" requiring manual pre-release before the dev
//! could re-bind for r1 or the reviewer could re-attach for review).
//!
//! Trigger contract (locked at spike — [#870] Q1):
//!
//! - **VERIFIED verdict only.** REJECTED / UNVERIFIED leave the
//!   binding intact so the dev can push r1 on the same branch.
//! - Detection in [`crate::api::handlers::messaging::handle_send`]
//!   post-success path:
//!   - `kind == "report"`
//!   - `text.trim_start().starts_with("VERIFIED")` (§3.12 literal)
//!   - `reviewed_head.is_some()` (§4.2 SHA-staleness gate present)
//!   - `correlation_id.is_some()` (task linkage present)
//! - On match: write a disk-backed intent record under
//!   `<home>/auto_release_queue/<task_id>.json` via [`enqueue_intent`].
//!   Disk-backed for restart resilience.
//!
//! Drain contract:
//!
//! - Supervisor hosts [`AutoReleaseTracker::maybe_scan`] in the
//!   per-tick loop, sibling to `conflict_notify` and `canonical_drift`.
//!   `TICKS_PER_SCAN = 3` (~30s at 10s/tick — faster than the 30-tick
//!   siblings because release responsiveness directly affects the
//!   next-cycle lease-conflict surface this whole module exists to
//!   eliminate).
//! - Each intent is processed at most once; the file is removed after
//!   processing regardless of outcome. The decision to release is
//!   gated by [`decide_release`] (pure helper, unit-tested).
//! - Dirty-worktree refusal: if the bound agent's worktree has
//!   uncommitted changes (`git status --porcelain` non-empty), the
//!   tracker **refuses** to release and emits a warn log — mirror of
//!   the operator-WIP-protection philosophy from #852 PR-C's
//!   `StashAndSwitchToDefault → emit_dirty_detached_warning` fall-back.
//!
//! Manual `release_worktree` MCP still works; the auto-release is
//! idempotent on an already-released binding.
//!
//! ── t-worktree-leak (PR-1): unified release invariant ──
//!
//! The trigger above is generalised from "VERIFIED verdict only" to THREE events
//! — merge, close-unmerged, and task-done (plus the verdict path, now gated by
//! the invariant) — all routed through the same queue. The sweeper releases iff
//! the release INVARIANT holds:
//!
//!   `releasable ⟺ PR-terminal ∨ (no-PR ∧ all branch tasks done)`,
//!   AND not-dirty AND not-opt-out, scoped to (repo, branch).
//!
//! So a VERIFIED on an OPEN PR no longer releases (it waits for the terminal
//! merge/close — fixes the premature-release class where #1795/#1804 needed the
//! worktree AFTER VERIFIED). Merge releases the worktree ORTHOGONALLY to the task.
//! Drain is no longer one-shot: an intent that is not-yet-releasable (PR open,
//! dirty) is RETAINED and retried, with a 7-day expiry handing off to the
//! force-reclaim backstop (PR-2). Each intent carries a lease-identity snapshot
//! for a TOCTOU CAS (skip if the lease was re-leased), and only dispatch-lease
//! worktrees (binding has a `task_id`) are invariant-released.

use std::path::{Path, PathBuf};

/// Sub-directory under `<home>` where pending release intents live.
const QUEUE_DIR: &str = "auto_release_queue";

/// Scan throttle in supervisor ticks. 3 ≈ 30s at the 10s tick rate —
/// faster than the 30-tick siblings because release latency directly
/// gates the next-cycle lease-conflict surface this module exists to
/// eliminate.
pub(crate) const TICKS_PER_SCAN: u64 = 3;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct AutoReleaseIntent {
    pub task_id: String,
    pub reviewer: String,
    pub verdict_msg_id: Option<String>,
    pub reviewed_head: Option<String>,
    pub enqueued_at: String,
    // ── t-worktree-leak (PR-1): event-driven release-invariant recompute ──
    // These default to None so legacy verdict-only intents still deserialize.
    /// The event that enqueued this intent: "verdict" | "merge" |
    /// "close_unmerged" | "task_done". Absent ⟹ legacy verdict intent.
    #[serde(default)]
    pub event_kind: Option<String>,
    /// (repo, branch) the invariant is scoped to (must-fix #2).
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Lease-identity snapshot taken at enqueue time, for the TOCTOU CAS
    /// (must-fix #1). If the live binding no longer matches this, the lease was
    /// re-leased to a different task between enqueue and sweep → skip.
    #[serde(default)]
    pub lease: Option<LeaseIdentity>,
    /// #3005: RFC3339 deadline before which this intent's next probe is skipped.
    /// Stamped ONLY when the expensive Unknown-only terminal-resolution fallback
    /// actually ran and still could not prove release — the one case that
    /// otherwise re-spawns git + two gh queries every sweep until the 7-day
    /// expiry. Absent ⟹ probe on the next sweep (legacy intents deserialize this
    /// way). Every fresh enqueue writes `None`, so new evidence wakes the probe
    /// immediately instead of waiting out the interval.
    #[serde(default)]
    pub unknown_retry_after: Option<String>,
}

/// t-worktree-leak (PR-1): the stable identity of a worktree lease, snapshotted
/// into an intent so the sweeper can detect a re-lease (TOCTOU, must-fix #1).
/// All fields are read from the agent's `binding.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
pub(crate) struct LeaseIdentity {
    pub agent: String,
    pub task_id: String,
    pub branch: String,
    pub worktree: String,
    /// `binding.json` `source_repo` (the repo path) — t-worktree-leak codex gap ①b:
    /// included so the CAS catches a re-lease to a DIFFERENT repo at the same
    /// branch name (cross-repo same-branch collision).
    #[serde(default)]
    pub source_repo: String,
    /// `binding.json` `issued_at` — changes on every fresh lease.
    pub issued_at: String,
}

impl LeaseIdentity {
    /// Read the current lease identity for `agent` from its live binding.
    /// `None` when the agent is unbound.
    pub(crate) fn from_binding(agent: &str, binding: &serde_json::Value) -> Self {
        let s = |k: &str| {
            binding
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        LeaseIdentity {
            agent: agent.to_string(),
            task_id: s("task_id"),
            branch: s("branch"),
            worktree: s("worktree"),
            source_repo: s("source_repo"),
            issued_at: s("issued_at"),
        }
    }
}

/// t-worktree-leak (PR-1): confidence in the PR-state determination, surfaced in
/// logs + the force-reclaim ALERT (PR-2) so we never blindly trust pr_state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrConfidence {
    /// pr_state positively shows a TERMINAL PR (Merged | ClosedUnmerged).
    ObservedTerminal,
    /// pr_state shows a real OPEN PR (pr_number > 0, non-terminal).
    ObservedOpen,
    /// gh-poll RAN and found no PR for the branch (positive no-PR, not absence).
    QueriedNone,
    /// No pr_state, or never gh-polled → cannot confirm (absence ≠ no-PR).
    Unknown,
}

/// t-worktree-leak (PR-1) must-fix #5: eligibility gate. Only worktrees
/// provisioned via a dispatch lease (their `binding.json` carries a non-empty
/// `task_id`) are subject to invariant-release. Operator-created / PR-inspection
/// worktrees (no task_id) are left to the conservative force-reclaim backstop
/// (PR-2). Fail-safe: provenance unclear ⟹ NOT eligible.
fn is_dispatch_lease(binding: &serde_json::Value) -> bool {
    binding
        .get("task_id")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// t-worktree-leak (PR-1) must-fix #3: the PR half of the release invariant, with
/// a confidence level. Allows release only on a POSITIVE signal — a terminal PR,
/// or a gh-poll that ran and found no PR — never on mere pr_state absence.
fn evaluate_pr_for_release(home: &Path, repo: &str, branch: &str) -> (bool, PrConfidence) {
    let Some(state) = crate::daemon::pr_state::load(home, repo, branch) else {
        // No pr_state at all → absence is ambiguous (gh-poll may not have run).
        return (false, PrConfidence::Unknown);
    };
    use crate::daemon::pr_state::MergeState;
    match state.merge_state {
        // Merged → release immediately (terminal, no rework path).
        MergeState::Merged { .. } => (true, PrConfidence::ObservedTerminal),
        // Closed-unmerged is MORE conservative than merge: a committed-but-
        // unmerged branch may be reworked, so the worktree only becomes releasable
        // CLOSE_GRACE_HOURS after the PR closed. (Dirty WIP is already protected by
        // decide_release; this grace covers clean-but-being-reworked branches.)
        MergeState::ClosedUnmerged { ref closed_at } => (
            close_grace_passed(closed_at),
            PrConfidence::ObservedTerminal,
        ),
        // Non-terminal: a real open PR blocks release; "polled, none found" allows.
        MergeState::NotReady | MergeState::MergeReady => {
            if state.pr_number > 0 {
                (false, PrConfidence::ObservedOpen)
            } else if state.last_gh_poll_at.is_some() && state.gh_poll_failures == 0 {
                // A SUCCESSFUL gh-poll ran (failures==0), pr_number still 0 ⟹
                // positively no PR. #986: gate on `gh_poll_failures == 0` — the Err
                // path (scanner.rs:387) ALSO sets `last_gh_poll_at` (for backoff),
                // so a FAILED or cold-cache poll (failures>0) must NOT be misread as
                // "no PR found". Also closes a pre-existing latent bug where a
                // transient gh-poll failure could false-release a worktree whose PR
                // was simply not observed.
                (true, PrConfidence::QueriedNone)
            } else {
                // pr_state exists (ci-watch armed) but never successfully gh-polled
                // (never polled, or last poll failed / cold cache) → ambiguous.
                (false, PrConfidence::Unknown)
            }
        }
    }
}

/// t-worktree-leak (PR-1): close-unmerged grace ceiling. A closed (unmerged) PR's
/// worktree only becomes releasable this long after `closed_at`.
const CLOSE_GRACE_HOURS: i64 = 24;

fn close_grace_passed(closed_at: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(closed_at)
        .map(|t| {
            chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc))
                > chrono::Duration::hours(CLOSE_GRACE_HOURS)
        })
        // Unparseable closed_at → conservative: NOT yet (wait / force-reclaim).
        .unwrap_or(false)
}

/// t-worktree-leak (PR-1) must-fix #4 + codex gap ①c + P0 cross-lease fix:
/// ALL tasks on (repo, branch) across EVERY project board are terminal
/// (Done | Cancelled). Uses the strict cross-board aggregate that fails
/// closed on enumeration/replay errors and rejects duplicate task ids.
///
/// Fails closed (returns false) when:
///   - board enumeration or replay fails (cannot guarantee completeness)
///   - zero tasks match the branch (no evidence of completion)
///
/// Tasks are branch-keyed (no `repo` field), so a same-named branch in a
/// DIFFERENT repo could pollute the aggregation. We scope by deriving each
/// PENDING task's repo from its owner's live binding: a pending task blocks
/// release ONLY if it is confirmed to belong to THIS repo; a pending task
/// confirmed in a DIFFERENT repo does not block; an UNresolvable pending
/// task (owner unbound) blocks (conservative — never mis-release).
fn all_branch_tasks_done(home: &Path, repo: &str, branch: &str) -> bool {
    use crate::task_events::TaskStatus;
    let all_tasks = match crate::tasks::list_all_strict(home) {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                branch = %branch, error = %e,
                "all_branch_tasks_done: strict cross-board aggregate failed — fail closed"
            );
            return false;
        }
    };
    let branch_tasks: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.branch.as_deref() == Some(branch))
        .collect();
    if branch_tasks.is_empty() {
        tracing::debug!(
            branch = %branch,
            "all_branch_tasks_done: zero tasks match branch — fail closed (no evidence of completion)"
        );
        return false;
    }
    branch_tasks
        .iter()
        .filter(|t| !matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled))
        .all(|t| {
            let owner_repo = t
                .assignee
                .as_deref()
                .and_then(|a| crate::binding::read(home, a))
                .and_then(|b| repo_slug_from_binding(&b));
            matches!(&owner_repo, Some(r) if r != repo)
        })
}

/// #t-…24962-7: positively confirm a branch NEVER had any PR — `gh pr list
/// --state all --head <branch>` returns empty. Branch-delete-immune (queries by
/// head-branch name, not a live ref). `Some(true)` = confirmed never-PR;
/// `Some(false)` = a PR exists/existed; `None` = gh failed (cannot confirm — the
/// caller must NOT release, #986 transient-failure convention). Routed through
/// `make_scm_provider` so the crate-wide test seam can drive it without gh.
fn branch_never_had_pr(repo: &str, branch: &str) -> Option<bool> {
    match crate::scm::make_scm_provider(repo, None).pr_list(
        repo,
        &crate::scm::ListFilter {
            state: Some("all"),
            head: Some(branch.to_string()),
            ..Default::default()
        },
        &["number"],
        None,
    ) {
        Ok(prs) => Some(prs.is_empty()),
        Err(_) => None,
    }
}

/// t-worktree-leak (PR-1): the unified release invariant (must-fix #2/#3/#4),
/// repo+branch scoped. `releasable ⟺ PR-terminal ∨ (no-PR ∧ all branch tasks
/// done)`. Returns (releasable_per_invariant, confidence). The dirty / opt-out /
/// bound gates stay in `decide_release` (must-fix #6).
fn releasable_by_invariant(home: &Path, repo: &str, branch: &str) -> (bool, PrConfidence) {
    let (pr_releasable, confidence) = evaluate_pr_for_release(home, repo, branch);
    let releasable = match confidence {
        // Terminal PR releases the worktree ORTHOGONALLY to task state (the
        // team-resolved T1: merge releases the worktree, doesn't touch the task).
        // `pr_releasable` is true for merged, grace-gated for closed-unmerged.
        PrConfidence::ObservedTerminal => pr_releasable,
        // No PR found → release only once the branch's tasks (this repo) are done.
        PrConfidence::QueriedNone => all_branch_tasks_done(home, repo, branch),
        // Open PR or unknown → not releasable (the sweeper retries until merge).
        PrConfidence::ObservedOpen | PrConfidence::Unknown => false,
    };
    (releasable, confidence)
}

/// True iff `role` (the verdict sender's resolved fleet.yaml role) is a reviewer
/// role. Structural — sourced from the operator-set `role:` config, never from
/// message text. Matches the two reviewer role shapes in production by EXACT
/// form, not a loose substring:
///   - the short fixup-team tag `reviewer` (exact, case-insensitive);
///   - the descriptive template role `Code reviewer — …` (prefix).
///
/// #2010 codex-r2: a bare `contains("review")` was too wide. `description` is a
/// serde alias for `role` (fleet/mod.rs), so a perfectly normal IMPLEMENTER
/// description such as "Implementer — build features and submit changes for
/// review" contains "review" and would re-open the self-verdict bypass. The
/// exact tag + `code reviewer` prefix admit every real reviewer (the three live
/// fixup reviewers are exactly `reviewer`; the deploy template is `Code reviewer
/// — …`) while rejecting any implementer/orchestrator description that merely
/// mentions a review ACTIVITY.
fn is_reviewer_role(role: Option<&str>) -> bool {
    let Some(r) = role else { return false };
    let t = r.trim().to_lowercase();
    t == "reviewer" || t.starts_with("code reviewer")
}

/// #2010 2a: the reviewer-binding-release bypass. A reviewer that ran a full
/// (worktree-align) inspection binds to the branch; once it submits a terminal
/// verdict AND its review task is terminal, its binding must be released even
/// though the PR is still open — otherwise `releasable_by_invariant`'s open-PR
/// gate holds the binding to PR-terminal and the lead's rework re-dispatch hits
/// a lease conflict. This bypass is the ONLY way the open-PR invariant is
/// skipped, and it is scoped with FOUR independent conditions so it can never
/// release an implementer's worktree (which legitimately waits for the terminal
/// PR per t-worktree-leak PR-1):
///
///   1. the intent was enqueued by a terminal verdict (`event_kind == "verdict"`);
///   2. the bound agent IS the verdict sender (`intent.reviewer == assignee`) —
///      scopes the release strictly to the verdict-sender's own binding;
///   3. the review task itself is terminal (Done | Cancelled);
///   4. the verdict sender's fleet ROLE is a reviewer (`is_reviewer_role`).
///
/// #2010 codex-r1: condition 2 alone is NOT a reviewer-vs-implementer
/// discriminator — an IMPLEMENTER that opens a report with "VERIFIED" on its
/// OWN task satisfies `intent.reviewer == assignee` (self-verdict), and the
/// #1228 reporter==assignee auto-close marks that task Done in the SAME message,
/// so conditions 1–3 all pass and the implementer's binding would release on an
/// open PR. Condition 4 (the structural fleet-role gate) closes that hole: an
/// implementer's role never reads as a reviewer, so its self-verdict never
/// bypasses. `sender_role` is resolved by the caller from fleet.yaml.
///
/// Cleanliness (the lead's clean-only condition) is enforced downstream by
/// [`decide_release`]'s `SkipDirtyWorktree` arm: a dirty reviewer worktree
/// retries (binding held) rather than releasing, protecting in-flight review WIP.
fn reviewer_binding_release_bypass(
    intent: &AutoReleaseIntent,
    task: Option<&crate::tasks::Task>,
    assignee: &str,
    sender_role: Option<&str>,
) -> bool {
    use crate::task_events::TaskStatus;
    intent.event_kind.as_deref() == Some("verdict")
        && intent.reviewer == assignee
        && is_reviewer_role(sender_role)
        && matches!(
            task.map(|t| &t.status),
            Some(TaskStatus::Done | TaskStatus::Cancelled)
        )
}

/// Outcome of [`decide_release`] — pure helper unit-tested without
/// touching disk / subprocess. The tracker dispatches by variant; the
/// `Skip` variants distinguish operator-visible reasons in the audit
/// log without conflating them with the happy path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseDecision {
    /// Release the worktree.
    Release,
    /// Task has no assignee — nothing to release.
    SkipNoAssignee,
    /// Task is not in the board (e.g. deleted between verdict + drain).
    SkipTaskMissing,
    /// Task's `auto_release_on_verdict` is explicitly `Some(false)`.
    /// r0 NOTE: there is no operator-write surface for this flag yet
    /// (deferred follow-up); the default `None` semantic = release.
    SkipOptOut,
    /// Agent is not currently bound (already released, never bound).
    SkipNotBound,
    /// Binding present but worktree has uncommitted changes.
    /// **Operator WIP protection** — mirror of #852 PR-C's
    /// `emit_dirty_detached_warning` fall-back; refuses to auto-release
    /// until the operator commits / stashes / explicitly releases.
    SkipDirtyWorktree,
}

/// Pure helper: given an intent and the resolved task plus worktree
/// state, decide whether to release. All inputs are pre-fetched by
/// the tracker so this fn is `unit testable` without disk / subprocess.
pub(crate) fn decide_release(
    task_lookup: Option<&crate::tasks::Task>,
    binding: Option<&serde_json::Value>,
    worktree_dirty: Option<bool>,
) -> ReleaseDecision {
    let Some(task) = task_lookup else {
        return ReleaseDecision::SkipTaskMissing;
    };
    if task.assignee.as_deref().unwrap_or("").is_empty() {
        return ReleaseDecision::SkipNoAssignee;
    }
    if task.auto_release_on_verdict == Some(false) {
        return ReleaseDecision::SkipOptOut;
    }
    let Some(_binding) = binding else {
        return ReleaseDecision::SkipNotBound;
    };
    match worktree_dirty {
        Some(true) => ReleaseDecision::SkipDirtyWorktree,
        Some(false) => ReleaseDecision::Release,
        // Couldn't determine dirty state (e.g. worktree path missing
        // on disk) → fail-safe to "not bound" rather than risk
        // releasing a binding pointing to legitimate operator WIP.
        None => ReleaseDecision::SkipNotBound,
    }
}

/// PR-D · D2 (#2711): the bound-path release gate, delegated to the unified
/// [`crate::worktree::disposition::terminal_disposition`] classifier (D1) instead
/// of an inline boolean. Pure, so the equivalence pin can lock it exhaustively.
///
/// The classifier owns the normal L1/L2 invariant — release only on a POSITIVE
/// PR-invariant, for both the clean-`Release` and the #2697 dirty-but-releasable
/// (`SkipDirtyWorktree`) arms. The auto-release path exercises only L0+L1/L2, so
/// the L0 protection fields are fed pass-through values (the pre-D2 gate never
/// consulted `.agend-managed` / `.agend-pinned` / occupancy) and the L3 (GC)
/// fields are inert — with `binding_present=true` the classifier never reaches the
/// binding-absent branch that reads them.
///
/// `reviewer_bypassed` (the #2010 2a reviewer-binding release) is `releasable ==
/// false` BY CONSTRUCTION (it is only ever set inside the `!releasable` block), so
/// the classifier keeps it. The bypass therefore releases a CLEAN reviewer
/// worktree via a SEPARATE explicit arm; a DIRTY one still retains (protecting
/// review WIP). This reproduces the pre-D2
/// `matches!(Release) || (matches!(SkipDirtyWorktree) && releasable)` expression
/// byte-for-byte on every reachable `(decision, releasable, reviewer_bypassed)` —
/// locked by [`tests::should_release_now_equals_pre_d2_gate`].
fn should_release_now(
    decision: &ReleaseDecision,
    releasable: bool,
    reviewer_bypassed: bool,
) -> bool {
    use crate::worktree::disposition::{
        terminal_disposition, Disposition, DispositionInput, ReclaimState,
    };
    let input = DispositionInput {
        // L0 pass-through: the pre-D2 auto-release gate never consulted these.
        daemon_managed: true,
        pinned: false,
        in_use: Some(false),
        // L1/L2: the real sub-verdicts the classifier composes.
        binding_present: true,
        release_decision: decision.clone(),
        releasable_by_invariant: releasable,
        // L3 (GC) inert on the bound path — never read when binding_present.
        agent_alive: Some(false),
        reclaim: ReclaimState::NotEligible,
    };
    matches!(terminal_disposition(&input), Disposition::Release)
        || (reviewer_bypassed && matches!(decision, ReleaseDecision::Release))
}

/// Return the queue directory path. Caller is responsible for ensuring
/// it exists before reading; [`enqueue_intent`] handles creation on
/// the write side.
pub(crate) fn queue_dir(home: &Path) -> PathBuf {
    home.join(QUEUE_DIR)
}

/// Atomic disk write: write-temp + rename. Hook-side caller (see
/// `handle_send` in `src/api/handlers/messaging.rs`) invokes this
/// post-success when the verdict predicate matches; failures are
/// logged at warn but do NOT propagate to the send caller (verdict
/// delivery must remain non-fragile even if the auto-release queue
/// can't be written — operator can always release manually).
pub(crate) fn enqueue_intent(home: &Path, intent: &AutoReleaseIntent) -> std::io::Result<()> {
    let dir = queue_dir(home);
    std::fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec_pretty(intent)?;
    let final_path = dir.join(format!("{}.json", intent.task_id));
    let tmp_path = dir.join(format!(".{}.tmp", intent.task_id));
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// #2059: strip the `[report_result] ` wrapper that
/// `comms::handle_report_result` prepends, so verdict detection sees the bare
/// verdict word. The reviewer's verdict is sent via `send request_kind=report`,
/// which routes through `handle_report_result` and wraps the summary — so the
/// downstream `msg.text` is `"[report_result] VERIFIED …"`, NOT `"VERIFIED …"`.
/// Without this strip, every `starts_with("VERIFIED")`-style check is dead
/// against real wire text (the pipeline-wide silence #2059 RCA'd). Idempotent on
/// already-bare text (a raw `send` report), so both shapes resolve.
///
/// This is the SINGLE strip mechanism — both verdict consumers
/// (`is_terminal_verdict_text` here, and `process_verdicts` in
/// `api/handlers/messaging.rs`) route through it, so the two never drift.
pub(crate) fn strip_report_wrapper(text: &str) -> &str {
    let t = text.trim_start();
    t.strip_prefix("[report_result] ")
        .map(str::trim_start)
        .unwrap_or(t)
}

/// The three terminal review verdicts. A reviewer's report opens with exactly
/// one of these (§3.12 / #1666 §3.3). True iff `text` (the message body, with or
/// without the `[report_result] ` wrapper) begins with one of them.
#[cfg(test)]
pub(crate) fn is_terminal_verdict_text(text: &str) -> bool {
    let t = strip_report_wrapper(text);
    t.starts_with("VERIFIED") || t.starts_with("REJECTED") || t.starts_with("UNVERIFIED")
}

/// Code-review release authority is a server-validated receipt, never visible
/// text, reviewed_head, a name, or a correlation string. `process_verdicts`
/// calls this only after PR-state ingestion accepted the receipt exactly once.
pub(crate) fn is_verdict_message(msg: &crate::inbox::InboxMessage) -> bool {
    msg.kind.as_deref() == Some("report") && msg.validated_code_review.is_some()
}

pub(crate) struct AutoReleaseTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl Default for AutoReleaseTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
        }
    }
}

impl AutoReleaseTracker {
    /// Per-tick entry. Returns `true` when the scan actually fired
    /// (test signal); `false` for pre-throttle ticks and the post-fire
    /// reset.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        drain_queue(home);
        true
    }
}

/// Drain every JSON file under `<home>/auto_release_queue/`. Each
/// file is processed at most once; the file is removed after
/// processing regardless of outcome. Malformed JSON is logged + the
/// file is dropped (poison-message handling — don't keep retrying).
fn drain_queue(home: &Path) {
    let dir = queue_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // Skip the in-progress `.<task_id>.tmp` files that
        // `enqueue_intent` uses for atomic rename. Defensive — the
        // rename should have moved them already, but a crash between
        // `write` and `rename` could leave them behind.
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let intent = match serde_json::from_str::<AutoReleaseIntent>(&content) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "auto_release: malformed intent JSON, dropping"
                );
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        // t-worktree-leak (PR-1) Q2: an intent that has been retrying past the
        // expiry is dropped — the force-reclaim backstop (PR-2) takes over (clean
        // handoff, aligned with the force-reclaim age cap).
        if intent_expired(&intent) {
            tracing::info!(
                task_id = %intent.task_id,
                "auto_release: intent past {INTENT_EXPIRY_DAYS}d expiry — dropping (force-reclaim backstop takes over)"
            );
            let _ = std::fs::remove_file(&path);
            continue;
        }
        // #3005: an intent whose Unknown-only probe already ran without proving
        // release carries a deferral stamp — leave the file untouched until the
        // deadline instead of re-spawning git + two gh queries every sweep.
        if unknown_probe_deferred(&intent) {
            continue;
        }
        match process_intent(home, &intent) {
            // Released / terminal skip → delete.
            IntentOutcome::Done => {
                let _ = std::fs::remove_file(&path);
            }
            // Not-yet-releasable (PR open / dirty) → retain for the next sweep.
            IntentOutcome::Retry => {}
            // #3005: the expensive Unknown-only fallback ran and still could not
            // prove release → retain, but defer its next probe by a fixed
            // interval. Rewrites the SAME queue file (`enqueue_intent` is keyed
            // on task_id and renames atomically).
            IntentOutcome::RetryAfterUnknownProbe => {
                let mut deferred = intent.clone();
                deferred.unknown_retry_after = Some(
                    (chrono::Utc::now() + chrono::Duration::minutes(UNKNOWN_RETRY_MINS))
                        .to_rfc3339(),
                );
                if let Err(e) = enqueue_intent(home, &deferred) {
                    // Stamp write failed → the intent stays as-is and is simply
                    // probed again next sweep (today's behavior). Never fatal.
                    tracing::warn!(task_id = %intent.task_id, error = %e,
                        "auto_release: could not stamp Unknown-probe deferral — will re-probe next sweep");
                }
            }
        }
    }
}

/// #3005: fixed deferral between Unknown-only probes that could not prove
/// release. One flat interval — deliberately not exponential, not configurable,
/// and not a cache: the 7-day `enqueued_at` expiry still bounds the whole retry
/// life, and any fresh enqueue clears the stamp.
const UNKNOWN_RETRY_MINS: i64 = 5;

/// #3005: true while a stamped intent is still inside its deferral window. An
/// absent or unparseable stamp probes now (fail-open toward liveness).
fn unknown_probe_deferred(intent: &AutoReleaseIntent) -> bool {
    intent
        .unknown_retry_after
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|deadline| chrono::Utc::now() < deadline.with_timezone(&chrono::Utc))
        .unwrap_or(false)
}

/// t-worktree-leak (PR-1) Q2: retry-intent age ceiling. Past this, the intent is
/// dropped and the force-reclaim backstop (PR-2) handles the worktree.
const INTENT_EXPIRY_DAYS: i64 = 7;

fn intent_expired(intent: &AutoReleaseIntent) -> bool {
    chrono::DateTime::parse_from_rfc3339(&intent.enqueued_at)
        .map(|t| {
            chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc))
                > chrono::Duration::days(INTENT_EXPIRY_DAYS)
        })
        // Unparseable enqueued_at → don't expire (conservative; a real intent
        // always carries a valid RFC3339 timestamp).
        .unwrap_or(false)
}

/// t-worktree-leak (PR-1) Q2: the sweeper's per-intent verdict — whether to
/// delete the intent or retain it for a later retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntentOutcome {
    /// Released, or a terminal skip (no assignee / unbound / not-eligible /
    /// lease-changed / opt-out) → delete the intent.
    Done,
    /// Not-yet-releasable but might become so (PR still open, dirty worktree) →
    /// retain for retry on a later sweep (subject to the queue expiry).
    Retry,
    /// #3005: same as `Retry`, but the expensive Unknown-only terminal-resolution
    /// fallback just ran and still could not prove release — so the caller stamps
    /// a fixed deferral before the next probe.
    RetryAfterUnknownProbe,
}

fn process_intent(home: &Path, intent: &AutoReleaseIntent) -> IntentOutcome {
    let event = intent.event_kind.as_deref().unwrap_or("verdict");
    // #2760 Slice A: resolve the intent's task via the STRICT router (the old
    // `list_all(home)` saw ONLY the default board). Typed policy — `Found` → continue
    // the assignee + lease CAS; `Found`/no-assignee OR proven `NotFound` → drop the
    // intent (`Done`); `Unreadable`/`Ambiguous` → RETAIN for retry (never release OR
    // drop on an unprovable route).
    let task = match crate::tasks::load_routed(home, &intent.task_id) {
        Ok(rt) => rt.task,
        Err(crate::tasks::TaskRouteError::NotFound) => {
            tracing::info!(task_id = %intent.task_id, event, "auto_release: task not found on any board — dropping intent");
            return IntentOutcome::Done;
        }
        Err(route_err) => {
            tracing::info!(task_id = %intent.task_id, event, %route_err, "auto_release: task route unresolved — retaining for retry");
            return IntentOutcome::Retry;
        }
    };
    let Some(assignee) = task.assignee.clone() else {
        tracing::info!(task_id = %intent.task_id, event, "auto_release: task has no assignee — dropping intent");
        return IntentOutcome::Done;
    };
    let (binding, binding_fingerprint) = match crate::binding::snapshot_guarded_binding(
        home, &assignee,
    ) {
        Ok(crate::binding::GuardedBinding::Absent) => {
            // Already released / never bound → nothing to do (idempotent).
            tracing::debug!(agent = %assignee, task_id = %intent.task_id, event, "auto_release: agent unbound — dropping intent");
            return IntentOutcome::Done;
        }
        Ok(crate::binding::GuardedBinding::Opaque(reason)) => {
            tracing::warn!(agent = %assignee, %reason, "auto_release: opaque binding state — retaining for retry");
            return IntentOutcome::Retry;
        }
        Ok(crate::binding::GuardedBinding::Known { value, fingerprint }) => (value, fingerprint),
        Err(e) => {
            tracing::warn!(agent = %assignee, error = %e, "auto_release: binding snapshot failed — retaining for retry");
            return IntentOutcome::Retry;
        }
    };

    // ── must-fix #5: eligibility — only dispatch leases get invariant-release.
    if !is_dispatch_lease(&binding) {
        tracing::info!(agent = %assignee, "auto_release: not a dispatch lease (no task_id) — left to force-reclaim backstop (PR-2)");
        return IntentOutcome::Done;
    }

    // ── must-fix #1: TOCTOU CAS — the live lease must still match the snapshot.
    if let Some(snap) = intent.lease.as_ref() {
        let live = LeaseIdentity::from_binding(&assignee, &binding);
        if &live != snap {
            tracing::info!(agent = %assignee, task_id = %intent.task_id, "auto_release: lease identity changed since enqueue (re-leased) — skipping (TOCTOU CAS)");
            return IntentOutcome::Done;
        }
    }

    // ── the release invariant (must-fix #2/#3/#4), repo+branch scoped. repo/
    // branch come from the intent; legacy verdict intents fall back to the
    // binding (branch directly, repo derived from the source_repo remote).
    let branch = intent
        .branch
        .clone()
        .or_else(|| {
            binding
                .get("branch")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default();
    let repo = intent
        .repo
        .clone()
        .or_else(|| repo_slug_from_binding(&binding))
        .unwrap_or_default();
    if branch.is_empty() || repo.is_empty() {
        // Cannot scope the invariant → cannot positively confirm. Retry (a later
        // gh-poll / event may resolve repo/branch) rather than release blindly.
        tracing::info!(agent = %assignee, task_id = %intent.task_id, event, "auto_release: repo/branch unresolved — retaining for retry");
        return IntentOutcome::Retry;
    }
    let (mut releasable, mut confidence) = releasable_by_invariant(home, &repo, &branch);
    // #3005: witness for "the expensive Unknown-only fallback below actually ran".
    let mut unknown_probe_ran = false;
    // #P1b (t-…24962-1): the fleet-standard merge flow is `--delete-branch`, after
    // which the pr_state scanner deletes the pr_state doc (scanner.rs
    // `ScanAction::Remove`) → `evaluate_pr_for_release` falls to `Unknown` and the
    // worktree would retain FOREVER (an immortal worktree, until the 7d
    // force-reclaim). Positively re-confirm a terminal merge via the
    // branch-delete-immune squash-merge check (`git cherry`/patch-id locally, gh
    // headRefOid fallback), gated to COMPLETED work (all branch tasks done) so a
    // transient pr_state absence on an OPEN PR is never false-released.
    if !releasable
        && confidence == PrConfidence::Unknown
        && all_branch_tasks_done(home, &repo, &branch)
    {
        if let Some(src) = binding.get("source_repo").and_then(|v| v.as_str()) {
            // #3005: from here the sweep spawns git (`default_branch`,
            // `is_squash_merged`) and may query gh (`branch_never_had_pr`).
            unknown_probe_ran = true;
            let src = Path::new(src);
            let default = crate::git_helpers::default_branch(src);
            if crate::branch_sweep::is_squash_merged(src, &default, &branch) {
                releasable = true;
                confidence = PrConfidence::ObservedTerminal;
                tracing::info!(agent = %assignee, repo = %repo, branch = %branch,
                    "auto_release: pr_state absent but branch confirmed merged (post --delete-branch) — treating as terminal");
            } else if branch_never_had_pr(&repo, &branch) == Some(true) {
                // #t-…24962-7: no pr_state AND gh positively confirms the branch
                // never had ANY PR (review/spike/design worktrees that produce no
                // PR of their own) AND all tasks are done → releasable via the
                // no-PR invariant. A transient gh failure returns None → NOT
                // released (retain for a later sweep, #986 convention).
                releasable = true;
                confidence = PrConfidence::QueriedNone;
                tracing::info!(agent = %assignee, repo = %repo, branch = %branch,
                    "auto_release: pr_state absent, gh confirms branch never had a PR + all tasks done — releasable (no-PR)");
            }
        }
    }
    let reviewer_bypassed = if !releasable {
        // #2010 2a: the open-PR invariant holds an IMPLEMENTER's worktree until
        // the PR is terminal (correct — it may be needed for rework/merge), but
        // it must NOT hold a REVIEWER's binding once the reviewer's own review
        // task is terminal — that leaks the binding and makes the lead's rework
        // re-dispatch hit a lease conflict. Bypass the invariant ONLY for the
        // verdict-sender's own binding when it is a REVIEWER (fleet role) with
        // its review task terminal; the dirty gate below still protects review
        // WIP (dirty → retry, not release). The role gate (#2010 codex-r1) is
        // the structural reviewer-vs-implementer discriminator that stops an
        // implementer's self-"VERIFIED" + #1228 auto-close from releasing its
        // own binding on an open PR.
        let sender_role = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|f| f.resolve_instance(&assignee))
            .and_then(|r| r.role);
        if reviewer_binding_release_bypass(intent, Some(&task), &assignee, sender_role.as_deref()) {
            tracing::info!(agent = %assignee, repo = %repo, branch = %branch, event, ?confidence, role = ?sender_role, "auto_release: reviewer-binding bypass — reviewer verdict + review task terminal, releasing if clean (#2010 2a)");
            true
        } else {
            tracing::debug!(agent = %assignee, repo = %repo, branch = %branch, event, ?confidence, "auto_release: invariant not yet satisfied — retaining for retry");
            // #3005: only the Unknown-only probe path is deferred. Every other
            // retain reason (open PR, unresolved route, repo/branch unresolved)
            // keeps its unchanged every-sweep cadence.
            return if unknown_probe_ran {
                IntentOutcome::RetryAfterUnknownProbe
            } else {
                IntentOutcome::Retry
            };
        }
    } else {
        false
    };

    // ── final gate (dirty / opt-out / bound), must-fix #6.
    let worktree_dirty = binding
        .get("worktree")
        .and_then(|v| v.as_str())
        .map(|w| !is_worktree_clean(Path::new(w)));
    let decision = decide_release(Some(&task), Some(&binding), worktree_dirty);
    // #P1b (t-…24962-1): on a RELEASABLE branch (PR-terminal, or no-PR + all tasks
    // done), a dirty worktree is stale build/handoff artifacts — a gitignored
    // SESSION-HANDOFF, a build-dirtied submodule — that never self-clears. The old
    // dirty→refuse-retry immortalized the worktree (retry every sweep, dirt stays,
    // never releases). Treat releasable-dirty like Release: `release_full`
    // WIP-preserves the dirty tree to a durable recovery ref (fail-closed, #2672)
    // AND notifies (wip_notice_recipient → team orchestrator, #2696) before
    // removing, so no WIP is lost. A NON-releasable dirty worktree (open PR /
    // Unknown / reviewer-bypass) still retains — that WIP may be active work.
    // PR-D · D2 (#2711): delegate this gate to the unified `terminal_disposition`
    // classifier instead of the inline boolean. `reviewer_bypassed` carries the
    // #2010 clean-reviewer release the classifier can't encode (releasable=false by
    // construction). Byte-identical — `should_release_now` + its equivalence pin.
    let release_now = should_release_now(&decision, releasable, reviewer_bypassed);
    if release_now {
        let dirty = worktree_dirty.unwrap_or(false);
        // S1: auto-release no longer carries a caller-held binding flock into the
        // mechanism. It supplies the exact disk generation evaluated above; the
        // shared release transaction owns A→L→A and the final raw fingerprint CAS.
        let crate::daemon::janitor::DispositionOutcome::Released(outcome) =
            crate::daemon::janitor::dispose_release_exact(home, &assignee, &binding_fingerprint)
        else {
            unreachable!("exact Release disposition yields Released");
        };
        if outcome.released {
            tracing::info!(agent = %assignee, task_id = %intent.task_id, event, ?confidence, dirty, outcome = ?outcome, "auto_release: released worktree (release invariant satisfied)");
            IntentOutcome::Done
        } else if outcome.stale_fingerprint {
            tracing::info!(agent = %assignee, task_id = %intent.task_id, "auto_release: exact binding fingerprint moved — dropping stale intent");
            IntentOutcome::Done
        } else {
            // #P1b: release_full is FAIL-CLOSED — dirty WIP that could not be
            // snapshotted (e.g. a contended index.lock) leaves the binding intact
            // (#2672). Retain the intent so a later sweep retries rather than
            // dropping it and leaking the worktree.
            tracing::warn!(agent = %assignee, task_id = %intent.task_id, error = ?outcome.error, "auto_release: release_full did not release (fail-closed) — retaining for retry");
            IntentOutcome::Retry
        }
    } else {
        match decision {
            // Non-terminal dirty: active-WIP protection. Debug because it repeats
            // every sweep until the branch terminates (per-cycle info = log spam);
            // the immortal case is gone — a terminal branch now takes the release
            // arm above and logs once.
            ReleaseDecision::SkipDirtyWorktree => {
                tracing::debug!(agent = %assignee, repo = %repo, branch = %branch, ?confidence, "auto_release: worktree dirty on non-terminal branch — retaining for retry");
                IntentOutcome::Retry
            }
            ReleaseDecision::SkipOptOut => {
                tracing::info!(agent = %assignee, task_id = %intent.task_id, "auto_release: opted out (auto_release_on_verdict=false) — dropping intent");
                IntentOutcome::Done
            }
            other => {
                tracing::info!(agent = %assignee, task_id = %intent.task_id, decision = ?other, "auto_release: terminal skip — dropping intent");
                IntentOutcome::Done
            }
        }
    }
}

/// t-worktree-leak (PR-1): derive the gh `owner/repo` slug from a binding's
/// `source_repo` path (via its `origin` remote). `None` if not resolvable.
fn repo_slug_from_binding(binding: &serde_json::Value) -> Option<String> {
    let src = binding.get("source_repo").and_then(|v| v.as_str())?;
    crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(Path::new(src))
}

/// Return `true` when `git status --porcelain` produces no output for
/// the given worktree. Failure (spawn / non-zero exit / worktree
/// missing) returns `false` — fail-safe to "dirty" so we refuse to
/// release when we can't confirm cleanliness.
fn is_worktree_clean(worktree: &Path) -> bool {
    if !worktree.is_dir() {
        return false;
    }
    // #1899: bounded via git_bypass (LOCAL 60s) — a stuck git → false fallback.
    // P0/P1 (codex R2): `--ignore-submodules=none` overrides any
    // `submodule.<name>.ignore=all|dirty` that would HIDE submodule dirt (this gates
    // the auto-release dirty decision that can route to remove — a false "clean" here
    // loses nested WIP); global `--no-optional-locks` (first) keeps the live index
    // byte-untouched. Both are git GLOBAL options, so they precede `status`.
    let out = match crate::git_helpers::git_bypass(
        worktree,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignore-submodules=none",
        ],
    ) {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    out.stdout.is_empty()
}

/// t-worktree-leak (PR-1): enqueue a release-invariant recompute intent for the
/// worktree bound to `branch`. Shared by the merge / close-unmerged / task-done
/// events. The sweeper re-checks the invariant + TOCTOU CAS + dirty/opt-out
/// before releasing, so this is lock-free (just a queue write) and safe to call
/// from inside the pr_state scanner's post-flock region (#1617).
///
/// Eligibility (must-fix #5): only dispatch leases (binding carries a non-empty
/// `task_id`) are enqueued; operator / inspection worktrees are left to the
/// conservative force-reclaim backstop (PR-2).
pub(crate) fn enqueue_release_recompute(home: &Path, repo: &str, branch: &str, event_kind: &str) {
    let Some(agent) =
        // #2117 P3b: branch-only scan (source_repo="") — this caller does its own
        // slug-based cross-repo guard below (repo_slug_from_binding vs event repo).
        crate::binding::scan_existing_branch_binding(home, "", branch, /* exclude */ "")
    else {
        return; // no bound agent → nothing to release.
    };
    let Some(binding) = crate::binding::read(home, &agent) else {
        return;
    };
    // codex gap ①a: cross-repo same-branch guard. `scan_existing_branch_binding`
    // matches by BRANCH only, so for a same-named branch in a different repo it can
    // resolve the WRONG agent. Verify the resolved binding's repo == the event's
    // repo (when the caller supplied one) and skip the mismatch.
    if !repo.is_empty() {
        let binding_repo = repo_slug_from_binding(&binding);
        if binding_repo.as_deref() != Some(repo) {
            tracing::debug!(agent = %agent, branch = %branch, event = %event_kind,
                event_repo = %repo, binding_repo = ?binding_repo,
                "auto_release: bound branch's repo != event repo (cross-repo same-branch) — skip");
            return;
        }
    }
    let task_id = binding
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if task_id.is_empty() {
        tracing::debug!(agent = %agent, branch = %branch, event = %event_kind,
            "auto_release: lease has no task_id (not a dispatch lease) — left to force-reclaim backstop");
        return;
    }
    let intent = AutoReleaseIntent {
        task_id,
        reviewer: String::new(),
        verdict_msg_id: None,
        reviewed_head: None,
        enqueued_at: chrono::Utc::now().to_rfc3339(),
        event_kind: Some(event_kind.to_string()),
        // Empty repo (e.g. the task-done caller, which lacks the gh slug) → None
        // so the sweeper derives it from the binding's source_repo.
        repo: (!repo.is_empty()).then(|| repo.to_string()),
        branch: Some(branch.to_string()),
        lease: Some(LeaseIdentity::from_binding(&agent, &binding)),
        // #3005: a fresh enqueue IS new evidence — never inherit a deferral.
        unknown_retry_after: None,
    };
    if let Err(e) = enqueue_intent(home, &intent) {
        tracing::warn!(repo = %repo, branch = %branch, event = %event_kind, error = %e,
            "auto_release: enqueue recompute intent failed");
    }
}

/// #1244 + t-worktree-leak (PR-1): on PR merge, enqueue a release-invariant
/// recompute. Merge is a terminal PR state → the sweeper releases the worktree
/// ORTHOGONALLY to the task (team-resolved T1). Routed through the HYBRID queue
/// so the CAS / dirty / opt-out gates all apply uniformly.
///
/// #1339 DAEMON-AUTONOMIC, GATE-EXEMPT BY DESIGN: reached ONLY from the per-tick
/// daemon loop on an internal PR-merge trigger (`ci_watch::poller` /
/// `pr_state::scanner`), never from the API socket — daemon self-heal.
pub(crate) fn auto_release_for_merged_branch(home: &Path, repo: &str, branch: &str) {
    enqueue_release_recompute(home, repo, branch, "merge");
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "auto_release_tests.rs"]
mod tests;
