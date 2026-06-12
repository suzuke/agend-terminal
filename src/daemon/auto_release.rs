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

/// t-worktree-leak (PR-1) must-fix #4 + codex gap ①c: ALL tasks on (repo, branch)
/// are terminal (Done | Cancelled). Tasks are branch-keyed (no `repo` field), so a
/// same-named branch in a DIFFERENT repo could pollute the aggregation. We scope
/// by deriving each PENDING task's repo from its owner's live binding: a pending
/// task blocks release ONLY if it is confirmed to belong to THIS repo; a pending
/// task confirmed in a DIFFERENT repo does not block; an UNresolvable pending task
/// (owner unbound) blocks (conservative — never mis-release).
fn all_branch_tasks_done(home: &Path, repo: &str, branch: &str) -> bool {
    use crate::task_events::TaskStatus;
    crate::tasks::list_all(home)
        .iter()
        .filter(|t| t.branch.as_deref() == Some(branch))
        // Only non-terminal (pending) tasks can block; Done/Cancelled never do.
        .filter(|t| !matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled))
        .all(|t| {
            let owner_repo = t
                .assignee
                .as_deref()
                .and_then(|a| crate::binding::read(home, a))
                .and_then(|b| repo_slug_from_binding(&b));
            // "does NOT block" ⟺ this pending task is CONFIRMED in a different repo.
            matches!(&owner_repo, Some(r) if r != repo)
        })
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
pub(crate) fn is_terminal_verdict_text(text: &str) -> bool {
    let t = strip_report_wrapper(text);
    t.starts_with("VERIFIED") || t.starts_with("REJECTED") || t.starts_with("UNVERIFIED")
}

/// Predicate helper used by the `handle_send` hook to decide whether the
/// message represents an actionable terminal verdict that should enqueue a
/// release intent. Pulled out so the unit test can assert the matching
/// contract without spinning up the full handler stack.
///
/// #2010 2a: widened from `VERIFIED`-only to ALL THREE terminal verdicts. A
/// REJECTED / UNVERIFIED reviewer holds the same kind of worktree binding (from
/// a worktree-align inspection) and must be able to release it the same way once
/// their review task is terminal — pre-fix the non-VERIFIED cases never even
/// enqueued an intent, so `releasable_by_invariant`'s open-PR gate held the
/// reviewer's binding to PR-terminal (the lease-conflict the lead re-dispatch
/// then hit). The `reviewed_head` gate is kept: we only ever release a binding
/// tied to an actually-reviewed head (UNVERIFIED that couldn't run/cite carries
/// no head and never worktree-aligned, so there is no binding to leak).
pub(crate) fn is_verdict_message(msg: &crate::inbox::InboxMessage) -> bool {
    msg.kind.as_deref() == Some("report")
        && is_terminal_verdict_text(&msg.text)
        && msg.reviewed_head.is_some()
        && msg.correlation_id.is_some()
}

#[derive(Debug, Default)]
pub(crate) struct AutoReleaseTracker {
    tick_count: u64,
}

impl AutoReleaseTracker {
    /// Per-tick entry. Returns `true` when the scan actually fired
    /// (test signal); `false` for pre-throttle ticks and the post-fire
    /// reset.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
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
        match process_intent(home, &intent) {
            // Released / terminal skip → delete.
            IntentOutcome::Done => {
                let _ = std::fs::remove_file(&path);
            }
            // Not-yet-releasable (PR open / dirty) → retain for the next sweep.
            IntentOutcome::Retry => {}
        }
    }
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
}

fn process_intent(home: &Path, intent: &AutoReleaseIntent) -> IntentOutcome {
    let event = intent.event_kind.as_deref().unwrap_or("verdict");
    let tasks = crate::tasks::list_all(home);
    let task = tasks.iter().find(|t| t.id == intent.task_id).cloned();
    let Some(assignee) = task.as_ref().and_then(|t| t.assignee.clone()) else {
        tracing::debug!(task_id = %intent.task_id, event, "auto_release: task missing / no assignee — dropping intent");
        return IntentOutcome::Done;
    };
    let Some(binding) = crate::binding::read(home, &assignee) else {
        // Already released / never bound → nothing to do (idempotent).
        tracing::debug!(agent = %assignee, task_id = %intent.task_id, event, "auto_release: agent unbound — dropping intent");
        return IntentOutcome::Done;
    };

    // ── must-fix #5: eligibility — only dispatch leases get invariant-release.
    if !is_dispatch_lease(&binding) {
        tracing::debug!(agent = %assignee, "auto_release: not a dispatch lease (no task_id) — left to force-reclaim backstop (PR-2)");
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
        tracing::debug!(agent = %assignee, task_id = %intent.task_id, event, "auto_release: repo/branch unresolved — retaining for retry");
        return IntentOutcome::Retry;
    }
    let (releasable, confidence) = releasable_by_invariant(home, &repo, &branch);
    if !releasable {
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
        if reviewer_binding_release_bypass(intent, task.as_ref(), &assignee, sender_role.as_deref())
        {
            tracing::info!(agent = %assignee, repo = %repo, branch = %branch, event, ?confidence, role = ?sender_role, "auto_release: reviewer-binding bypass — reviewer verdict + review task terminal, releasing if clean (#2010 2a)");
        } else {
            tracing::debug!(agent = %assignee, repo = %repo, branch = %branch, event, ?confidence, "auto_release: invariant not yet satisfied — retaining for retry");
            return IntentOutcome::Retry;
        }
    }

    // ── final gate (dirty / opt-out / bound), must-fix #6 — unchanged decide_release.
    let worktree_dirty = binding
        .get("worktree")
        .and_then(|v| v.as_str())
        .map(|w| !is_worktree_clean(Path::new(w)));
    match decide_release(task.as_ref(), Some(&binding), worktree_dirty) {
        ReleaseDecision::Release => {
            // codex gap ②: CAS+release must be ONE atomic critical section under
            // `.binding.json.lock` — the same lock `bind_full` holds (binding.rs:67)
            // and the GC path uses (worktree_pool.rs:717). The pre-lock CAS above is
            // only a cheap early-out; a concurrent bind_full from ANOTHER thread
            // (MCP handler) could interleave between it and the release. So re-read
            // + re-validate the FULL lease identity under the lock, then release in
            // the same section. (`release_full` → `unbind` does NOT take this lock —
            // binding.rs:119 just removes the file — so no deadlock, mirroring the
            // pre-PR-1 merge path.)
            let lock_path = crate::paths::runtime_dir(home)
                .join(&assignee)
                .join(".binding.json.lock");
            let _lock = match crate::store::acquire_file_lock(&lock_path) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(agent = %assignee, error = %e, "auto_release: binding lock failed — retaining for retry");
                    return IntentOutcome::Retry;
                }
            };
            let Some(live) = crate::binding::read(home, &assignee) else {
                // Unbound under the lock → already released. Nothing to do.
                return IntentOutcome::Done;
            };
            if let Some(snap) = intent.lease.as_ref() {
                if &LeaseIdentity::from_binding(&assignee, &live) != snap {
                    tracing::info!(agent = %assignee, task_id = %intent.task_id, "auto_release: lease identity changed under lock (re-leased) — skip (TOCTOU CAS)");
                    return IntentOutcome::Done;
                }
            }
            let outcome = crate::worktree_pool::release_full(home, &assignee, false);
            tracing::info!(agent = %assignee, task_id = %intent.task_id, event, ?confidence, outcome = ?outcome, "auto_release: released worktree (release invariant satisfied)");
            IntentOutcome::Done
        }
        // Dirty is transient (operator commits / stashes later) → retry.
        ReleaseDecision::SkipDirtyWorktree => {
            tracing::warn!(agent = %assignee, repo = %repo, branch = %branch, "auto_release: worktree dirty — retaining for retry (operator WIP protection)");
            IntentOutcome::Retry
        }
        ReleaseDecision::SkipOptOut => {
            tracing::info!(agent = %assignee, task_id = %intent.task_id, "auto_release: opted out (auto_release_on_verdict=false) — dropping intent");
            IntentOutcome::Done
        }
        other => {
            tracing::debug!(agent = %assignee, task_id = %intent.task_id, decision = ?other, "auto_release: terminal skip — dropping intent");
            IntentOutcome::Done
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
    let out = match crate::git_helpers::git_bypass(worktree, &["status", "--porcelain"]) {
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
        crate::binding::scan_existing_branch_binding(home, branch, /* exclude */ "")
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agend-test-auto-release-{tag}-{}-{id}",
            std::process::id()
        ))
    }

    fn sample_intent(task_id: &str) -> AutoReleaseIntent {
        AutoReleaseIntent {
            task_id: task_id.to_string(),
            reviewer: "reviewer-1".to_string(),
            verdict_msg_id: Some("m-test".to_string()),
            reviewed_head: Some("deadbeef".to_string()),
            enqueued_at: chrono::Utc::now().to_rfc3339(),
            event_kind: None,
            repo: None,
            branch: None,
            lease: None,
        }
    }

    fn sample_task(id: &str, assignee: Option<&str>) -> crate::tasks::Task {
        crate::tasks::Task {
            id: id.to_string(),
            title: "t".into(),
            description: "d".into(),
            status: crate::task_events::TaskStatus::Claimed,
            priority: crate::task_events::TaskPriority::Normal,
            assignee: assignee.map(String::from),
            routed_to: None,
            created_by: "lead".into(),
            depends_on: vec![],
            result: None,
            created_at: "2026-05-17T00:00:00Z".into(),
            updated_at: "2026-05-17T00:00:00Z".into(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            auto_release_on_verdict: None,
            tags: vec![],
            parent_id: None,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    /// VERIFIED report with reviewed_head + correlation_id matches.
    #[test]
    fn verdict_send_pattern_matches_canonical_verdict() {
        let mut msg = canonical_verdict_message();
        assert!(is_verdict_message(&msg), "canonical verdict must match");
        // Surrounding whitespace tolerated via trim_start.
        msg.text = "   VERIFIED — all green".into();
        assert!(is_verdict_message(&msg), "leading whitespace tolerated");
    }

    /// #2010 2a: REJECTED and UNVERIFIED reports (with reviewed_head +
    /// correlation) NOW match — the gate was widened from VERIFIED-only so the
    /// non-VERIFIED reviewer's binding also gets a release intent.
    #[test]
    fn all_terminal_verdicts_match_2010() {
        let base = canonical_verdict_message();
        for verdict in ["VERIFIED", "REJECTED", "UNVERIFIED"] {
            let mut m = base.clone();
            m.text = format!("{verdict} — evidence block follows");
            assert!(
                is_verdict_message(&m),
                "{verdict} report must enqueue a release intent (#2010 2a)"
            );
            // Leading whitespace tolerated via trim_start.
            m.text = format!("   {verdict} — indented");
            assert!(is_verdict_message(&m), "{verdict} with indent must match");
        }
    }

    /// #2059: the strip is idempotent — a BARE verdict (a raw `send` report with
    /// no `[report_result] ` wrapper) still matches, and a non-verdict report is
    /// NOT a false positive. (The PRODUCER-FED fixture — feeding the real
    /// `build_report_text` output through this matcher — lives in
    /// `comms_inbox.rs` next to the producer, #1493 discipline.)
    #[test]
    fn strip_report_wrapper_idempotent_and_no_false_positive_2059() {
        // Bare verdict (unwrapped) still resolves.
        assert!(is_terminal_verdict_text("VERIFIED — all green"));
        assert_eq!(strip_report_wrapper("VERIFIED — x"), "VERIFIED — x");
        // Wrapper stripped to the bare word (incl. the trailing space).
        assert_eq!(
            strip_report_wrapper("[report_result] VERIFIED — x"),
            "VERIFIED — x"
        );
        // A non-verdict report must NOT be a false positive (wrapped or bare).
        assert!(!is_terminal_verdict_text(
            "[report_result] Done — pushed PR #2058"
        ));
        assert!(!is_terminal_verdict_text("just a plain message"));
    }

    /// Non-verdict text, non-report kinds, or missing reviewed_head /
    /// correlation_id still skip detection — the widening (2010 2a) only added
    /// the two extra verdict WORDS, not a relaxation of the other gates.
    #[test]
    fn non_verdict_kinds_skip_detection() {
        let base = canonical_verdict_message();
        let mut m = base.clone();
        m.text = "looks good to me, merging".into();
        assert!(!is_verdict_message(&m), "non-verdict prose must not match");
        m = base.clone();
        m.kind = Some("task".into());
        assert!(!is_verdict_message(&m), "kind=task must not match");
        m = base.clone();
        m.kind = Some("update".into());
        assert!(!is_verdict_message(&m), "kind=update must not match");
        m = base.clone();
        m.text = "REJECTED — r1 needed".into();
        m.reviewed_head = None;
        assert!(
            !is_verdict_message(&m),
            "missing reviewed_head must not match even for a terminal verdict"
        );
        m = base.clone();
        m.text = "UNVERIFIED — re-run CI".into();
        m.correlation_id = None;
        assert!(
            !is_verdict_message(&m),
            "missing correlation_id must not match"
        );
    }

    /// #2010 2a §3.9 — the reviewer-binding release bypass: all FOUR conditions
    /// (verdict intent + bound agent IS the verdict sender + reviewer fleet role
    /// + review task terminal) must hold; dropping any one keeps the binding.
    #[test]
    fn reviewer_binding_bypass_requires_all_four_conditions_2010() {
        use crate::task_events::TaskStatus;
        let mut intent = sample_intent("t-rev");
        intent.event_kind = Some("verdict".to_string());
        intent.reviewer = "reviewer-1".to_string();
        let rev = Some("reviewer"); // fleet role of the verdict sender

        let mut done_task = sample_task("t-rev", Some("reviewer-1"));
        done_task.status = TaskStatus::Done;
        // All four hold → bypass (REJECTED/UNVERIFIED/VERIFIED all enqueue a
        // "verdict" intent, so the kind is irrelevant here).
        assert!(
            reviewer_binding_release_bypass(&intent, Some(&done_task), "reviewer-1", rev),
            "all four conditions → bypass the open-PR invariant"
        );
        // Cancelled is also terminal; descriptive template role still counts.
        let mut cancelled = done_task.clone();
        cancelled.status = TaskStatus::Cancelled;
        assert!(
            reviewer_binding_release_bypass(
                &intent,
                Some(&cancelled),
                "reviewer-1",
                Some("Code reviewer — independent review")
            ),
            "cancelled review task + descriptive reviewer role → bypass"
        );

        // (1) not a verdict intent (merge/task_done event) → no bypass.
        let mut merge_intent = intent.clone();
        merge_intent.event_kind = Some("merge".to_string());
        assert!(
            !reviewer_binding_release_bypass(&merge_intent, Some(&done_task), "reviewer-1", rev),
            "non-verdict event must not bypass (only the verdict-sender path does)"
        );

        // (2) bound agent is NOT the verdict sender → no bypass.
        assert!(
            !reviewer_binding_release_bypass(&intent, Some(&done_task), "other-agent", rev),
            "binding whose agent != verdict sender must NOT bypass"
        );

        // (4) verdict sender's role is NOT a reviewer → no bypass. This is the
        // #2010 codex-r1 gate: an implementer's self-verdict never bypasses.
        assert!(
            !reviewer_binding_release_bypass(&intent, Some(&done_task), "reviewer-1", None),
            "no role → not a reviewer → no bypass"
        );
        assert!(
            !reviewer_binding_release_bypass(
                &intent,
                Some(&done_task),
                "reviewer-1",
                Some("Implementer — build features")
            ),
            "implementer role must NOT bypass"
        );

        // (3) review task not terminal (still claimed/in_review) → no bypass yet.
        let claimed = sample_task("t-rev", Some("reviewer-1")); // default Claimed
        assert!(
            !reviewer_binding_release_bypass(&intent, Some(&claimed), "reviewer-1", rev),
            "non-terminal review task must not bypass (retry until done)"
        );
        // Missing task → no bypass.
        assert!(
            !reviewer_binding_release_bypass(&intent, None, "reviewer-1", rev),
            "missing task must not bypass"
        );
    }

    /// #2010 codex-r1 §3.9 — the implementer self-verdict EXPLOIT must not
    /// bypass. An implementer opens a report with "VERIFIED" on its OWN task
    /// (correlation = own task); the #1228 reporter==assignee auto-close marks
    /// that task Done in the same message — so conditions 1 (verdict intent),
    /// 2 (`intent.reviewer == assignee`, since the implementer verdicts its own
    /// task) and 3 (task Done) ALL hold. Only condition 4 (the fleet role gate)
    /// stops the implementer's binding from releasing on an open PR.
    #[test]
    fn implementer_self_verdict_does_not_bypass_2010_r1() {
        use crate::task_events::TaskStatus;
        let mut intent = sample_intent("t-dev");
        intent.event_kind = Some("verdict".to_string());
        intent.reviewer = "dev-1".to_string(); // the verdict SENDER is the implementer

        let mut self_done = sample_task("t-dev", Some("dev-1")); // own task, assignee == sender
        self_done.status = TaskStatus::Done; // #1228 auto-closed it

        // Conditions 1–3 all hold (the exploit shape). Role gate must veto it.
        assert!(
            !reviewer_binding_release_bypass(&intent, Some(&self_done), "dev-1", None),
            "implementer self-verdict (no reviewer role) must NOT release its own \
             binding on an open PR"
        );
        assert!(
            !reviewer_binding_release_bypass(
                &intent,
                Some(&self_done),
                "dev-1",
                Some("Implementer — build features, run tests")
            ),
            "implementer role string must NOT satisfy the reviewer gate"
        );
        // #2010 codex-r2: an implementer whose description (serde alias for role)
        // MENTIONS a review activity must still be rejected — the old
        // contains("review") gate let exactly this revive the bypass.
        assert!(
            !reviewer_binding_release_bypass(
                &intent,
                Some(&self_done),
                "dev-1",
                Some("Implementer — build features and submit changes for review")
            ),
            "an implementer description mentioning 'review' must NOT bypass (codex-r2)"
        );
    }

    /// `is_reviewer_role` matches both production reviewer role shapes by exact
    /// form and rejects implementer / orchestrator / absent roles — INCLUDING an
    /// implementer description that merely MENTIONS a review activity (the #2010
    /// codex-r2 counter-probe: a bare `contains("review")` let it through).
    #[test]
    fn is_reviewer_role_exact_forms_only_2010_r2() {
        // Accept: the two real reviewer shapes (the 3 live fixup reviewers are
        // exactly `reviewer`; the deploy template is `Code reviewer — …`).
        assert!(is_reviewer_role(Some("reviewer")), "fixup-team short tag");
        assert!(is_reviewer_role(Some("REVIEWER")), "case-insensitive");
        assert!(is_reviewer_role(Some("  reviewer  ")), "trimmed");
        assert!(
            is_reviewer_role(Some(
                "Code reviewer — independent review from a non-Claude vantage, \
                 verdicts VERIFIED/REJECTED/UNVERIFIED"
            )),
            "template descriptive role (exact production string)"
        );

        // Reject: implementer / orchestrator, incl. ones that mention review.
        assert!(
            !is_reviewer_role(Some(
                "Implementer — build features and submit changes for review"
            )),
            "#2010 codex-r2: an implementer description mentioning a review \
             ACTIVITY must NOT pass (this revived the self-verdict bypass under \
             the old contains(\"review\") gate)"
        );
        assert!(!is_reviewer_role(Some(
            "Implementer — pick up tasks from the board, build features, run tests"
        )));
        assert!(!is_reviewer_role(Some(
            "Team orchestrator — break work into tasks, dispatch, gate merges after reviewer approval"
        )));
        assert!(!is_reviewer_role(None), "no role → not a reviewer");
        assert!(!is_reviewer_role(Some("")), "empty role → not a reviewer");
    }

    /// `enqueue_intent` is atomic: temp file is renamed into place;
    /// after success no `.tmp` file remains in the queue dir.
    #[test]
    fn enqueue_intent_writes_atomic_file() {
        let home = tmp_home("enqueue");
        std::fs::create_dir_all(&home).unwrap();
        let intent = sample_intent("t-enqueue-1");
        enqueue_intent(&home, &intent).expect("enqueue");
        let final_path = queue_dir(&home).join("t-enqueue-1.json");
        assert!(
            final_path.exists(),
            "final intent file must exist after enqueue"
        );
        // No `.tmp` file left behind.
        let stragglers: Vec<_> = std::fs::read_dir(queue_dir(&home))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(
            stragglers.is_empty(),
            "no .tmp stragglers should remain after atomic rename"
        );
        // Round-trip the body.
        let body = std::fs::read_to_string(&final_path).unwrap();
        let parsed: AutoReleaseIntent = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, intent);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Tracker throttle: TICKS_PER_SCAN=3 means tick 1+2 return false,
    /// tick 3 fires (returns true), tick 4 resets to false.
    #[test]
    fn tracker_throttles_to_tick_per_scan() {
        let home = tmp_home("throttle");
        std::fs::create_dir_all(&home).unwrap();
        let mut tracker = AutoReleaseTracker::default();
        for i in 0..(TICKS_PER_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&home),
                "tick {i} (pre-throttle) must return false"
            );
        }
        assert!(
            tracker.maybe_scan(&home),
            "{TICKS_PER_SCAN}th tick must fire and return true"
        );
        assert!(
            !tracker.maybe_scan(&home),
            "post-fire tick must reset counter and return false"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `decide_release`: dirty worktree → `SkipDirtyWorktree`
    /// regardless of opt-out / assignee presence (operator WIP
    /// protection takes precedence over the release decision).
    #[test]
    fn decide_release_skips_dirty_worktree() {
        let task = sample_task("t-1", Some("dev-1"));
        let binding = serde_json::json!({ "worktree": "/tmp/x" });
        let d = decide_release(Some(&task), Some(&binding), Some(true));
        assert_eq!(d, ReleaseDecision::SkipDirtyWorktree);
    }

    /// `decide_release`: explicit `Some(false)` opt-out flag short-
    /// circuits even when assignee + binding + clean tree present.
    #[test]
    fn decide_release_skips_opt_out_flag() {
        let mut task = sample_task("t-2", Some("dev-2"));
        task.auto_release_on_verdict = Some(false);
        let binding = serde_json::json!({ "worktree": "/tmp/y" });
        let d = decide_release(Some(&task), Some(&binding), Some(false));
        assert_eq!(d, ReleaseDecision::SkipOptOut);
    }

    /// `decide_release`: happy path — task + assignee + binding +
    /// clean tree all green, decision is `Release`.
    #[test]
    fn decide_release_happy_path() {
        let task = sample_task("t-3", Some("dev-3"));
        let binding = serde_json::json!({ "worktree": "/tmp/z" });
        let d = decide_release(Some(&task), Some(&binding), Some(false));
        assert_eq!(d, ReleaseDecision::Release);
    }

    /// `decide_release`: missing task → drop intent.
    #[test]
    fn decide_release_skips_missing_task() {
        let d = decide_release(None, None, None);
        assert_eq!(d, ReleaseDecision::SkipTaskMissing);
    }

    /// `decide_release`: task has no assignee → nothing to release.
    #[test]
    fn decide_release_skips_no_assignee() {
        let task = sample_task("t-4", None);
        let d = decide_release(Some(&task), None, None);
        assert_eq!(d, ReleaseDecision::SkipNoAssignee);
    }

    /// `decide_release`: assignee present but binding gone (already
    /// released, never bound) → idempotent skip.
    #[test]
    fn decide_release_skips_not_bound() {
        let task = sample_task("t-5", Some("dev-5"));
        let d = decide_release(Some(&task), None, None);
        assert_eq!(d, ReleaseDecision::SkipNotBound);
    }

    /// `drain_queue` drops a file whose JSON is malformed and emits
    /// a warn — but the file is removed so the tracker doesn't keep
    /// retrying the same broken record (poison-message handling).
    #[test]
    fn drain_queue_drops_malformed_intents() {
        let home = tmp_home("malformed");
        std::fs::create_dir_all(queue_dir(&home)).unwrap();
        let bad_path = queue_dir(&home).join("garbage.json");
        std::fs::write(&bad_path, b"{not json").unwrap();
        drain_queue(&home);
        assert!(
            !bad_path.exists(),
            "malformed intent must be dropped on drain"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1244: no binding for branch → silent no-op.
    #[test]
    fn auto_release_on_merge_no_binding_is_noop() {
        let home = tmp_home("1244-no-bind");
        std::fs::create_dir_all(crate::paths::runtime_dir(&home)).unwrap();
        auto_release_for_merged_branch(&home, "owner/repo", "feat/gone");
        // No panic, no crash — silent skip.
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1244: dirty worktree → skip auto-release, binding preserved.
    #[test]
    fn auto_release_on_merge_skips_dirty_worktree() {
        let home = tmp_home("1244-dirty");
        let agent = "dev-dirty";
        let branch = "feat/dirty-branch";
        let rt = crate::paths::runtime_dir(&home).join(agent);
        std::fs::create_dir_all(&rt).unwrap();
        // Create a real git worktree dir with uncommitted changes
        let wt =
            std::env::temp_dir().join(format!("agend-test-1244-dirty-wt-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Create initial commit so git status works
        std::fs::write(wt.join("initial.txt"), "init").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Now create an uncommitted file → dirty
        std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();
        let binding = serde_json::json!({
            "version": 1,
            "branch": branch,
            "task_id": "t-test",
            "worktree": wt.to_str().unwrap(),
        });
        std::fs::write(
            rt.join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        auto_release_for_merged_branch(&home, "owner/repo", branch);
        assert!(
            rt.join("binding.json").exists(),
            "binding.json must be preserved when worktree is dirty"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// t-worktree-leak (PR-1): merge no longer releases directly — it ENQUEUES a
    /// release-invariant recompute intent (event_kind="merge") carrying the CAS
    /// lease snapshot, which the sweeper processes. (The full enqueue→sweep→
    /// release path is covered by the invariant tests below.)
    #[test]
    fn auto_release_on_merge_enqueues_recompute_intent() {
        let home = tmp_home("1244-release");
        // Real source repo whose origin resolves to "owner/repo" so the codex ①a
        // cross-repo guard (binding repo == event repo) passes.
        let repo = itest_source_repo(&home, "owner/repo");
        let agent = "dev-merge";
        let branch = "feat/merged-branch";
        let rt = crate::paths::runtime_dir(&home).join(agent);
        std::fs::create_dir_all(&rt).unwrap();
        let binding = serde_json::json!({
            "version": 1,
            "branch": branch,
            "task_id": "t-test",
            "worktree": "",
            "source_repo": repo.to_str().unwrap(),
            "issued_at": "2026-06-05T00:00:00Z",
        });
        std::fs::write(
            rt.join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        auto_release_for_merged_branch(&home, "owner/repo", branch);
        let queued = std::fs::read_dir(queue_dir(&home))
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        assert_eq!(queued, 1, "merge must enqueue exactly one recompute intent");
        let content = std::fs::read_to_string(queue_dir(&home).join("t-test.json")).unwrap();
        let intent: AutoReleaseIntent = serde_json::from_str(&content).unwrap();
        assert_eq!(intent.event_kind.as_deref(), Some("merge"));
        assert_eq!(intent.branch.as_deref(), Some(branch));
        assert!(
            intent.lease.is_some(),
            "merge intent must carry the CAS lease snapshot"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    fn canonical_verdict_message() -> crate::inbox::InboxMessage {
        crate::inbox::InboxMessage {
            id: Some("m-verdict-1".into()),
            task_id: Some("t-x".into()),
            correlation_id: Some("t-x".into()),
            reviewed_head: Some("deadbeef".into()),
            from: "from:reviewer-1".into(),
            text: "VERIFIED — clean baseline + 5 platforms green".into(),
            kind: Some("report".into()),
            timestamp: "2026-05-17T00:00:00Z".into(),
            ..Default::default()
        }
    }

    // ── t-worktree-leak (PR-1): release-invariant tests ──

    fn write_pr(
        home: &Path,
        branch: &str,
        ms: crate::daemon::pr_state::MergeState,
        pr_number: u64,
        polled: bool,
    ) {
        use crate::daemon::pr_state;
        let mut s =
            pr_state::new_for_branch("o/r", branch, "headsha", pr_state::ReviewClass::Single);
        s.merge_state = ms;
        s.pr_number = pr_number;
        if polled {
            s.last_gh_poll_at = Some("2026-06-05T00:00:00Z".to_string());
        }
        pr_state::save(home, &s).unwrap();
    }

    use crate::daemon::pr_state::MergeState;

    #[test]
    fn invariant_merged_is_releasable() {
        let home = tmp_home("inv-merged");
        write_pr(
            &home,
            "feat/x",
            MergeState::Merged {
                merge_commit: "c0ffee".into(),
                merged_at: "2026-06-05T00:00:00Z".into(),
            },
            5,
            true,
        );
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(r, "merged PR is releasable");
        assert_eq!(c, PrConfidence::ObservedTerminal);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_open_pr_is_not_releasable() {
        // The Q1(b) behavior: a VERIFIED on an OPEN PR must NOT release.
        let home = tmp_home("inv-open");
        write_pr(&home, "feat/x", MergeState::MergeReady, 7, true);
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(
            !r,
            "open PR must not be releasable (release waits for terminal)"
        );
        assert_eq!(c, PrConfidence::ObservedOpen);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_no_pr_polled_is_releasable_when_tasks_done() {
        // gh-poll ran, no PR found (pr_number 0) + no pending tasks (vacuous) →
        // releasable via the no-PR branch (covers tasks that never produce a PR).
        let home = tmp_home("inv-nopr");
        write_pr(&home, "feat/x", MergeState::NotReady, 0, true);
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(r, "no-PR + all-tasks-done is releasable");
        assert_eq!(c, PrConfidence::QueriedNone);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_never_polled_is_unknown_not_releasable() {
        // pr_state exists (ci-watch armed) but never gh-polled → cannot positively
        // confirm no-PR (absence ≠ no-PR, must-fix #3).
        let home = tmp_home("inv-unknown");
        write_pr(&home, "feat/x", MergeState::NotReady, 0, false);
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(!r);
        assert_eq!(c, PrConfidence::Unknown);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn queried_none_requires_successful_poll_986() {
        // #986: QueriedNone (positively-no-PR → release) requires a SUCCESSFUL poll
        // (gh_poll_failures == 0). The Err path (scanner.rs:387) ALSO sets
        // last_gh_poll_at, so a failed / cold-cache poll (failures>0) must be
        // ambiguous (Unknown), never a false "no PR" that releases the worktree.
        let home = tmp_home("qn-986");
        // Successful poll, pr_number 0 → positively no PR → releasable.
        write_pr(&home, "feat/x", MergeState::NotReady, 0, true);
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(r, "success + no PR → releasable");
        assert_eq!(c, PrConfidence::QueriedNone);
        // Failed / cold-cache poll: failures>0 → ambiguous → NOT releasable.
        let mut s = crate::daemon::pr_state::load(&home, "o/r", "feat/x").unwrap();
        s.gh_poll_failures = 1;
        crate::daemon::pr_state::save(&home, &s).unwrap();
        let (r2, c2) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(!r2, "failed/cold poll (failures>0) must NOT release");
        assert_eq!(c2, PrConfidence::Unknown);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_absent_pr_state_is_unknown() {
        let home = tmp_home("inv-absent");
        let (r, c) = releasable_by_invariant(&home, "o/r", "feat/missing");
        assert!(!r);
        assert_eq!(c, PrConfidence::Unknown);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invariant_closed_unmerged_respects_grace() {
        let home = tmp_home("inv-closed");
        // Within grace (just closed) → not releasable.
        let fresh = chrono::Utc::now().to_rfc3339();
        write_pr(
            &home,
            "feat/x",
            MergeState::ClosedUnmerged { closed_at: fresh },
            9,
            true,
        );
        let (r, _) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(
            !r,
            "closed-unmerged within grace must NOT release (may rework)"
        );
        // Past grace → releasable.
        let old =
            (chrono::Utc::now() - chrono::Duration::hours(CLOSE_GRACE_HOURS + 1)).to_rfc3339();
        write_pr(
            &home,
            "feat/x",
            MergeState::ClosedUnmerged { closed_at: old },
            9,
            true,
        );
        let (r2, c2) = releasable_by_invariant(&home, "o/r", "feat/x");
        assert!(r2, "closed-unmerged past grace is releasable");
        assert_eq!(c2, PrConfidence::ObservedTerminal);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn eligibility_requires_dispatch_task_id() {
        assert!(is_dispatch_lease(&serde_json::json!({ "task_id": "t-1" })));
        // Fail-safe: empty / missing task_id → NOT eligible.
        assert!(!is_dispatch_lease(&serde_json::json!({ "task_id": "" })));
        assert!(!is_dispatch_lease(
            &serde_json::json!({ "branch": "feat/x" })
        ));
    }

    #[test]
    fn cas_lease_identity_detects_release() {
        let snap = LeaseIdentity::from_binding(
            "dev",
            &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1" }),
        );
        // Same binding → matches.
        let same = LeaseIdentity::from_binding(
            "dev",
            &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1" }),
        );
        assert_eq!(snap, same);
        // Re-leased (new task / issued_at) → mismatch → CAS skips.
        let relesed = LeaseIdentity::from_binding(
            "dev",
            &serde_json::json!({ "task_id": "t-2", "branch": "feat/x", "worktree": "/w", "issued_at": "T2" }),
        );
        assert_ne!(snap, relesed);
        // codex gap ①b: re-leased to the SAME branch name in a DIFFERENT repo →
        // source_repo differs → CAS catches it.
        let snap_repo = LeaseIdentity::from_binding(
            "dev",
            &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1", "source_repo": "/repos/a" }),
        );
        let other_repo = LeaseIdentity::from_binding(
            "dev",
            &serde_json::json!({ "task_id": "t-1", "branch": "feat/x", "worktree": "/w", "issued_at": "T1", "source_repo": "/repos/b" }),
        );
        assert_ne!(
            snap_repo, other_repo,
            "CAS must catch a re-lease to a different repo"
        );
    }

    #[test]
    fn cross_repo_same_branch_enqueue_skips_codex_1a() {
        // codex gap ①a: a bound branch in repo B must NOT be released by an event
        // for the same branch name in repo A.
        let home = tmp_home("itest-xrepo");
        let repo_b = itest_source_repo(&home, "owner/repo-b");
        itest_lease(&home, &repo_b, "dev", "feat/shared", "t-b", false);
        // Event for repo-A (a DIFFERENT repo) on the same branch name.
        enqueue_release_recompute(&home, "owner/repo-a", "feat/shared", "merge");
        assert_eq!(
            queue_len(&home),
            0,
            "cross-repo same-branch event must not enqueue against repo-b's lease"
        );
        assert!(bound(&home, "dev"), "repo-b's binding untouched");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn intent_expiry_after_7_days() {
        let mut intent = sample_intent("t-exp");
        intent.enqueued_at =
            (chrono::Utc::now() - chrono::Duration::days(INTENT_EXPIRY_DAYS + 1)).to_rfc3339();
        assert!(intent_expired(&intent), "intent older than 7d expires");
        intent.enqueued_at = chrono::Utc::now().to_rfc3339();
        assert!(!intent_expired(&intent), "fresh intent does not expire");
    }

    // ── t-worktree-leak (PR-1): enqueue→sweep→release INTEGRATION tests ──
    // §3.9 / #1799: drive the REAL entry (the scanner's merge call /
    // enqueue_release_recompute), provision real state (managed git worktree +
    // dispatch binding + board task + pr_state), run the sweeper, and assert the
    // worktree is actually released or retained — not an injected-input unit test.

    fn itest_source_repo(home: &Path, slug: &str) -> std::path::PathBuf {
        let dir = home.join("source-repo");
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("https://github.com/{slug}.git");
        for args in [
            vec!["init", "-b", "main"],
            vec!["remote", "add", "origin", url.as_str()],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .ok();
        }
        // Mirror the real repo's .gitignore (line 29) so the `.agend-managed`
        // marker the lease writes into the worktree does NOT show as untracked —
        // otherwise `git status --porcelain` is non-empty and the dirty-guard
        // refuses to release (exactly how production stays clean).
        std::fs::write(dir.join(".gitignore"), ".agend-managed\n").unwrap();
        std::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        dir
    }

    fn seed_task(home: &Path, id: &str, owner: &str, branch: &str, done: bool) {
        use crate::task_events::{append, DoneSource, InstanceName, TaskEvent, TaskId};
        append(
            home,
            &InstanceName::from("test:lead"),
            TaskEvent::Created {
                task_id: TaskId(id.into()),
                title: "t".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from(owner)),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: Some(branch.into()),
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        if done {
            append(
                home,
                &InstanceName::from(owner),
                TaskEvent::Done {
                    task_id: TaskId(id.into()),
                    by: InstanceName::from(owner),
                    source: DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: Some("ok".into()),
                    },
                },
            )
            .unwrap();
        }
    }

    /// Provision a real managed worktree + dispatch binding (non-empty task_id) +
    /// a board task owned by the agent. Returns the worktree path.
    fn itest_lease(
        home: &Path,
        repo: &Path,
        agent: &str,
        branch: &str,
        task_id: &str,
        done: bool,
    ) -> std::path::PathBuf {
        let lease = crate::worktree_pool::lease(home, repo, agent, branch).expect("lease");
        crate::binding::bind_full(home, agent, task_id, branch, &lease.path, repo)
            .expect("bind_full");
        seed_task(home, task_id, agent, branch, done);
        lease.path
    }

    fn write_pr_slug(
        home: &Path,
        repo: &str,
        branch: &str,
        ms: MergeState,
        pr_number: u64,
        polled: bool,
    ) {
        use crate::daemon::pr_state;
        let mut s =
            pr_state::new_for_branch(repo, branch, "headsha", pr_state::ReviewClass::Single);
        s.merge_state = ms;
        s.pr_number = pr_number;
        if polled {
            s.last_gh_poll_at = Some("2026-06-05T00:00:00Z".to_string());
        }
        pr_state::save(home, &s).unwrap();
    }

    fn bound(home: &Path, agent: &str) -> bool {
        crate::binding::read(home, agent).is_some()
    }
    fn queue_len(home: &Path) -> usize {
        std::fs::read_dir(queue_dir(home))
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }

    fn write_fleet(home: &Path, agent: &str) {
        let p = crate::fleet::fleet_yaml_path(home);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&p, format!("instances:\n  {agent}:\n    backend: claude\n")).unwrap();
    }

    /// REAL task-done entry: the MCP handler (`task action=done`) — exercises the
    /// handler→enqueue wiring (codex gap ③ / §3.9). Asserts no error.
    fn task_done_via_handler(home: &Path, agent: &str, task_id: &str) {
        let r = crate::tasks::handle(
            home,
            agent,
            &serde_json::json!({ "action": "done", "id": task_id }),
        );
        assert!(r.get("error").is_none(), "task action=done failed: {r}");
    }

    #[test]
    fn integration_merge_releases_via_real_scanner() {
        // codex gap ③: drive the REAL scanner entry `scan_and_emit_with` (not the
        // helper) so the scanner→enqueue wiring is under test (breaks → fails).
        let home = tmp_home("itest-merge");
        let repo = itest_source_repo(&home, "owner/repo");
        itest_lease(&home, &repo, "dev", "feat/m", "t-m", false);
        write_pr_slug(
            &home,
            "owner/repo",
            "feat/m",
            MergeState::Merged {
                merge_commit: "c0ffee".into(),
                merged_at: "2026-06-05T00:00:00Z".into(),
            },
            5,
            true,
        );
        assert!(bound(&home, "dev"), "pre: agent is bound");
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Mock poller returns no PRs; the stored Merged state is sticky and drives
        // the scanner's terminal-merge arm → auto_release_for_merged_branch → enqueue.
        let poller = crate::daemon::pr_state::gh_poll::tests::MockGhPoller::new(vec![Ok(vec![])]);
        crate::daemon::pr_state::scan_and_emit_with(&home, &registry, &poller);
        assert_eq!(
            queue_len(&home),
            1,
            "real scanner→enqueue wiring produced an intent"
        );
        drain_queue(&home);
        assert!(
            !bound(&home, "dev"),
            "real scanner → enqueue → sweep → released (binding gone)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn integration_open_pr_retains_via_real_handler() {
        let home = tmp_home("itest-open");
        write_fleet(&home, "dev");
        let repo = itest_source_repo(&home, "owner/repo");
        itest_lease(&home, &repo, "dev", "feat/o", "t-o", false);
        write_pr_slug(
            &home,
            "owner/repo",
            "feat/o",
            MergeState::MergeReady,
            7,
            true,
        );
        // REAL entry: the task-done handler marks done + enqueues.
        task_done_via_handler(&home, "dev", "t-o");
        assert_eq!(queue_len(&home), 1, "task-done handler→enqueue wiring");
        drain_queue(&home);
        assert!(
            bound(&home, "dev"),
            "open PR → NOT released (binding stays)"
        );
        assert_eq!(queue_len(&home), 1, "open-PR intent retained for retry");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn integration_no_pr_task_done_releases_via_real_handler() {
        let home = tmp_home("itest-nopr");
        write_fleet(&home, "dev");
        let repo = itest_source_repo(&home, "owner/repo");
        itest_lease(&home, &repo, "dev", "feat/n", "t-n", false);
        write_pr_slug(&home, "owner/repo", "feat/n", MergeState::NotReady, 0, true); // polled, no PR
                                                                                     // REAL entry: task-done handler.
        task_done_via_handler(&home, "dev", "t-n");
        drain_queue(&home);
        assert!(
            !bound(&home, "dev"),
            "no-PR + task done (via real handler) → released"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn integration_cas_skips_re_leased_binding() {
        let home = tmp_home("itest-cas");
        let repo = itest_source_repo(&home, "owner/repo");
        let wt = itest_lease(&home, &repo, "dev", "feat/c", "t-c", false);
        write_pr_slug(
            &home,
            "owner/repo",
            "feat/c",
            MergeState::Merged {
                merge_commit: "c".into(),
                merged_at: "2026-06-05T00:00:00Z".into(),
            },
            5,
            true,
        );
        // Enqueue snapshots the CURRENT lease (task_id=t-c).
        crate::daemon::auto_release::enqueue_release_recompute(
            &home,
            "owner/repo",
            "feat/c",
            "merge",
        );
        // Re-lease the SAME agent to a new task → snapshot is now stale.
        crate::binding::bind_full(&home, "dev", "t-c2", "feat/c", &wt, &repo).expect("rebind");
        drain_queue(&home);
        assert!(
            bound(&home, "dev"),
            "CAS: a stale (re-leased) intent must NOT release the new lease"
        );
        assert_eq!(
            queue_len(&home),
            0,
            "stale intent dropped (CAS skip is terminal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn integration_expired_intent_dropped() {
        let home = tmp_home("itest-exp");
        std::fs::create_dir_all(queue_dir(&home)).unwrap();
        let mut intent = sample_intent("t-exp");
        intent.enqueued_at =
            (chrono::Utc::now() - chrono::Duration::days(INTENT_EXPIRY_DAYS + 1)).to_rfc3339();
        enqueue_intent(&home, &intent).unwrap();
        assert_eq!(queue_len(&home), 1);
        drain_queue(&home);
        assert_eq!(
            queue_len(&home),
            0,
            "expired intent dropped (force-reclaim backstop takes over)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
