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

/// Predicate helper used by the `handle_send` hook to decide whether
/// the message represents an actionable VERIFIED verdict. Pulled out
/// so the unit test can assert the matching contract without spinning
/// up the full handler stack.
pub(crate) fn is_verdict_message(msg: &crate::inbox::InboxMessage) -> bool {
    msg.kind.as_deref() == Some("report")
        && msg.text.trim_start().starts_with("VERIFIED")
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
        match serde_json::from_str::<AutoReleaseIntent>(&content) {
            Ok(intent) => process_intent(home, &intent),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#870 auto_release: malformed intent JSON, dropping"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn process_intent(home: &Path, intent: &AutoReleaseIntent) {
    let tasks = crate::tasks::list_all(home);
    let task = tasks.iter().find(|t| t.id == intent.task_id).cloned();
    let assignee_opt = task.as_ref().and_then(|t| t.assignee.clone());
    let binding = assignee_opt
        .as_ref()
        .and_then(|a| crate::binding::read(home, a));
    let worktree_dirty = binding.as_ref().and_then(|b| {
        b.get("worktree")
            .and_then(|v| v.as_str())
            .map(|w| !is_worktree_clean(Path::new(w)))
    });
    let decision = decide_release(task.as_ref(), binding.as_ref(), worktree_dirty);
    match decision {
        ReleaseDecision::Release => {
            // Safe to unwrap: decision Release requires task + assignee.
            let assignee = assignee_opt.expect("assignee present per decide_release");
            let outcome = crate::worktree_pool::release_full(home, &assignee, false);
            tracing::info!(
                agent = %assignee,
                task_id = %intent.task_id,
                reviewer = %intent.reviewer,
                verdict_msg_id = ?intent.verdict_msg_id,
                outcome = ?outcome,
                "#870 auto_release: released worktree on VERIFIED verdict"
            );
        }
        ReleaseDecision::SkipDirtyWorktree => {
            tracing::warn!(
                agent = ?assignee_opt,
                task_id = %intent.task_id,
                "#870 auto_release: worktree has uncommitted changes — \
                 refusing to auto-release (operator WIP protection). \
                 Manual release_worktree still available."
            );
        }
        ReleaseDecision::SkipOptOut => {
            tracing::info!(
                agent = ?assignee_opt,
                task_id = %intent.task_id,
                "#870 auto_release: task opted out via auto_release_on_verdict=false, skipping"
            );
        }
        ReleaseDecision::SkipNotBound => {
            tracing::debug!(
                agent = ?assignee_opt,
                task_id = %intent.task_id,
                "#870 auto_release: agent not bound (already released or never bound), skipping"
            );
        }
        ReleaseDecision::SkipNoAssignee => {
            tracing::debug!(
                task_id = %intent.task_id,
                "#870 auto_release: task has no assignee, skipping"
            );
        }
        ReleaseDecision::SkipTaskMissing => {
            tracing::warn!(
                task_id = %intent.task_id,
                "#870 auto_release: task not found in board, dropping intent"
            );
        }
    }
}

/// Return `true` when `git status --porcelain` produces no output for
/// the given worktree. Failure (spawn / non-zero exit / worktree
/// missing) returns `false` — fail-safe to "dirty" so we refuse to
/// release when we can't confirm cleanliness.
fn is_worktree_clean(worktree: &Path) -> bool {
    if !worktree.is_dir() {
        return false;
    }
    let out = match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    out.stdout.is_empty()
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
            enqueued_at: "2026-05-17T00:00:00Z".to_string(),
        }
    }

    fn sample_task(id: &str, assignee: Option<&str>) -> crate::tasks::Task {
        crate::tasks::Task {
            id: id.to_string(),
            title: "t".into(),
            description: "d".into(),
            status: "claimed".into(),
            priority: "normal".into(),
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

    /// REJECTED, UNVERIFIED, kind=task / update / query, missing
    /// reviewed_head, or missing correlation_id all skip detection.
    #[test]
    fn non_verdict_kinds_skip_detection() {
        let base = canonical_verdict_message();
        let mut m = base.clone();
        m.text = "REJECTED — r1 needed".into();
        assert!(!is_verdict_message(&m), "REJECTED must not match");
        m = base.clone();
        m.text = "UNVERIFIED — re-run CI".into();
        assert!(!is_verdict_message(&m), "UNVERIFIED must not match");
        m = base.clone();
        m.kind = Some("task".into());
        assert!(!is_verdict_message(&m), "kind=task must not match");
        m = base.clone();
        m.kind = Some("update".into());
        assert!(!is_verdict_message(&m), "kind=update must not match");
        m = base.clone();
        m.reviewed_head = None;
        assert!(
            !is_verdict_message(&m),
            "missing reviewed_head must not match"
        );
        m = base.clone();
        m.correlation_id = None;
        assert!(
            !is_verdict_message(&m),
            "missing correlation_id must not match"
        );
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
}
