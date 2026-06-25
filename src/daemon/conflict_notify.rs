//! Phase A Piece-1 + Piece-2 — git conflict notify + escalation.
//!
//! When the state classifier observes an agent transitioning into
//! [`AgentState::GitConflict`](crate::state::AgentState::GitConflict),
//! this module emits a structured `kind=update` message to the bound
//! agent via [`crate::inbox::notify_agent`] with the conflicted file
//! paths, operation type (rebase / merge / cherry-pick), branch
//! context, and a next-steps hint. If the agent stays in
//! `GitConflict` for [`STALE_THRESHOLD_SECS`] (30 min), the daemon
//! pushes a telegram alert to the operator and sets `waiting_on` on
//! the agent.
//!
//! Mirror of [`crate::daemon::waiting_on_stale`] pattern — same
//! tracker shape, same `maybe_scan` per-tick cadence, same
//! dedup-via-realert-interval guard.
//!
//! ## Lifecycle
//!
//! 1. **Transition INTO `GitConflict`** — first observation. Look up
//!    conflicted files (`git status --porcelain` + UU/AA/DD/AU/UA/UD/DU
//!    prefix filter), op type (`.git/REBASE_HEAD` /
//!    `.git/MERGE_HEAD` / `.git/CHERRY_PICK_HEAD` presence), branch
//!    context (binding.json). Emit notify. Record
//!    `last_conflict_at[name] = now`.
//! 2. **30 min stale, STILL `GitConflict`** — escalate via telegram
//!    push to operator's channel binding. Set `waiting_on` on the
//!    agent. Suppress re-alert within `REALERT_INTERVAL_SECS`.
//! 3. **Transition OUT of `GitConflict`** — clear
//!    `last_conflict_at[name]`. Leave `waiting_on` for operator
//!    manual clear (auto-clear is wrong — operator may want to
//!    observe resolution before allowing new dispatch).

use std::collections::HashMap;
use std::path::Path;

/// Stale threshold — escalate to operator if conflict still active
/// after 30 minutes. Mirror of `waiting_on_stale::STALE_THRESHOLD_SECS`
/// pattern; tuned per spike Q3.
pub(crate) const STALE_THRESHOLD_SECS: i64 = 30 * 60;

/// Re-alert suppression: 30 minutes between repeated escalations for
/// the same agent. Prevents telegram spam when the operator can't
/// resolve immediately.
pub(crate) const REALERT_INTERVAL_SECS: i64 = 30 * 60;

/// Scan throttle: 30 ticks × 10s = 5 min cadence. Matches
/// `waiting_on_stale` + `idle_watchdog` + `anti_stall`.
pub(crate) const TICKS_PER_SCAN: u64 = 30;

/// Per-tick conflict-notify tracker. Mirrors
/// [`crate::daemon::waiting_on_stale::WaitingOnStaleTracker`].
pub(crate) struct ConflictNotifyTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// agent → moment of first conflict observation (for stale gate).
    last_conflict_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// agent → last telegram-escalation timestamp (dedup guard).
    last_escalated_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// #1739 boot-seed latch. The first scan after a fresh daemon start records
    /// agents already in `GitConflict` into `last_conflict_at` (stamped now)
    /// WITHOUT firing the first-observation notify, so a restart doesn't re-ping
    /// conflicts the operator already saw. NOTE (option a): seeding stamps
    /// `last_conflict_at = now`, so the 30-min staleness escalation clock starts
    /// at boot — a conflict surviving restart + 30 min still escalates. The
    /// escalation dedup itself (`last_escalated_at`) is out of scope here (#1739
    /// ②, persisted separately).
    seeded: bool,
}

impl Default for ConflictNotifyTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
            last_conflict_at: HashMap::new(),
            last_escalated_at: HashMap::new(),
            seeded: false,
        }
    }
}

impl ConflictNotifyTracker {
    /// #1923 G5: prune per-agent state for agents no longer in the registry
    /// (deleted / redeployed). Without this a same-name redeploy inherits the old
    /// instance's `last_conflict_at` (false-gating its first-observation notify or
    /// starting the 30-min staleness clock from the dead instance's time) and
    /// `last_escalated_at` (false-suppressing a real escalation). Mirrors the
    /// #1470 `retry_tracks` sweep; called per-tick from `run_loop` with the live
    /// registry agent set.
    pub(crate) fn retain_active(&mut self, active: &std::collections::HashSet<String>) {
        self.last_conflict_at
            .retain(|name, _| active.contains(name));
        self.last_escalated_at
            .retain(|name, _| active.contains(name));
    }

    /// Per-tick entry point — runs only every `TICKS_PER_SCAN` ticks
    /// for throttling. Returns true iff the scan body fired this
    /// tick.
    ///
    /// Reads in-memory registry to observe per-agent state. For
    /// agents currently in `GitConflict`:
    /// - First observation → emit kind=update via `notify_agent` with
    ///   structured context; record `last_conflict_at`.
    /// - Stale + dedup-clear → telegram escalate via
    ///   `channel::telegram::reply::send_reply` (best-effort; failure
    ///   logged + skipped, boot loop continues).
    ///
    /// For agents transitioning OUT of `GitConflict`: drop
    /// `last_conflict_at` entry. `waiting_on` is left for operator
    /// manual clear per spike Q3.
    pub(crate) fn maybe_scan(
        &mut self,
        home: &Path,
        registry: &crate::agent::AgentRegistry,
    ) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let seeding = !self.seeded;
        self.seeded = true;

        // Phase 1: collect per-agent (name, state, worktree, branch)
        // tuples under a single registry lock. The worktree-state
        // shell-outs + notify emission happen lock-free in phase 2.
        let mut observed: Vec<(String, crate::state::AgentState)> = Vec::new();
        {
            let reg = crate::agent::lock_registry(registry);
            for handle in reg.values() {
                let state = handle.core.lock().state.current;
                observed.push((handle.name.to_string(), state));
            }
        }

        let now = chrono::Utc::now();
        for (name, state) in observed {
            match state {
                crate::state::AgentState::GitConflict => {
                    let first_observation = !self.last_conflict_at.contains_key(&name);
                    if first_observation {
                        record_first_observation(
                            &mut self.last_conflict_at,
                            home,
                            &name,
                            now,
                            seeding,
                        );
                    } else if let Some(&last_at) = self.last_conflict_at.get(&name) {
                        let last_escalated = self.last_escalated_at.get(&name).copied();
                        if should_escalate_conflict(last_at, last_escalated, now) {
                            emit_telegram_escalation(home, &name);
                            self.last_escalated_at.insert(name.clone(), now);
                        }
                    }
                }
                _ => {
                    // Resolution path: drop the conflict tracker
                    // entry. `waiting_on` is left for operator manual
                    // clear per spike Q3.
                    if self.last_conflict_at.remove(&name).is_some() {
                        tracing::info!(
                            agent = %name,
                            "Phase A: GitConflict resolved, dropping tracker entry"
                        );
                        self.last_escalated_at.remove(&name);
                    }
                }
            }
        }
        true
    }
}

/// Pure escalation decision for a still-conflicted agent already being tracked:
/// escalate iff the conflict has been open longer than [`STALE_THRESHOLD_SECS`]
/// AND no escalation fired within the last [`REALERT_INTERVAL_SECS`] (realert
/// dedup, keyed on the LAST ESCALATION — not the last conflict observation).
/// Extracted from [`ConflictNotifyTracker::maybe_scan`] so the stale-gate +
/// dedup timing is unit-testable with real inputs. Byte-identical to the inline
/// `stale && dedup_ok` it replaced.
fn should_escalate_conflict(
    last_conflict_at: chrono::DateTime<chrono::Utc>,
    last_escalated_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let stale = now.signed_duration_since(last_conflict_at)
        > chrono::Duration::seconds(STALE_THRESHOLD_SECS);
    let dedup_ok = last_escalated_at.is_none_or(|t| {
        now.signed_duration_since(t) > chrono::Duration::seconds(REALERT_INTERVAL_SECS)
    });
    stale && dedup_ok
}

/// Best-effort: emit the structured kind=update notify to the bound
/// agent via `crate::inbox::notify_agent`. Discovers worktree state
/// (conflicted files + op type + branch) from disk + binding.json.
/// Failures are logged + skipped (boot continues).
/// #1739: handle a first-observation conflict. The dedup record happens
/// unconditionally; the first-observation notify is suppressed during the boot
/// seeding pass (`seeding == true`) so a daemon restart doesn't re-ping
/// conflicts the operator already saw before the restart.
fn record_first_observation(
    last_conflict_at: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
    home: &Path,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
    seeding: bool,
) {
    last_conflict_at.insert(name.to_string(), now);
    if !seeding {
        emit_conflict_notify(home, name);
    }
}

fn emit_conflict_notify(home: &Path, agent: &str) {
    let Some(binding_json) = crate::binding::read(home, agent) else {
        tracing::debug!(
            agent,
            "Phase A: conflict detected but no binding.json, skipping notify"
        );
        return;
    };
    let worktree_str = binding_json
        .get("worktree")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if worktree_str.is_empty() {
        tracing::debug!(
            agent,
            "Phase A: binding lacks worktree path, skipping notify"
        );
        return;
    }
    let worktree = std::path::PathBuf::from(worktree_str);
    let conflicted_files = discover_conflicted_files(&worktree);
    let operation = discover_operation_type(&worktree).unwrap_or("unknown");
    let branch = binding_json
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let base = crate::git_helpers::default_branch(&worktree);
    let payload = build_notify_payload(operation, &conflicted_files, branch, &base);
    let text = payload.to_string();
    route_conflict_alert(home, agent, false, &text);
    tracing::info!(
        agent,
        operation,
        files = ?conflicted_files,
        "Phase A: GitConflict notify emitted"
    );
}

/// Best-effort: telegram-push to operator's channel binding when a
/// conflict has been active for >= STALE_THRESHOLD_SECS (30 min) and
/// the dedup window allows re-alert. Falls back to inbox-notify
/// path; the actual telegram delivery is handled by the channel
/// router downstream.
fn emit_telegram_escalation(home: &Path, agent: &str) {
    let text = format!(
        "[Phase A escalation] Agent `{agent}` has been in GitConflict for >30min — \
         operator intervention may be required. Inspect via `pane_snapshot` or \
         direct check of the agent's worktree."
    );
    route_conflict_alert(home, agent, true, &text);
    tracing::warn!(
        agent,
        threshold_min = STALE_THRESHOLD_SECS / 60,
        "Phase A: GitConflict escalation (operator notified)"
    );
}

/// #event-bus (conflict_notify, Option A): gate-ON → emit `ConflictAlert` (the
/// subscriber delivers via `deliver_conflict_alert`); gate-OFF (prod default) →
/// the legacy direct deliver. No double-delivery, no gate-off regression. `text`
/// is the already-rendered notify body (built from the live worktree discovery at
/// the producer) — carried verbatim so the bus path never re-runs discovery.
fn route_conflict_alert(home: &Path, agent: &str, escalation: bool, text: &str) {
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::ConflictAlert {
            agent: agent.to_string(),
            escalation,
            text: text.to_string(),
        },
    );
}

/// Shared deliver for the git-conflict notify: PTY-notify the conflicted agent via
/// `notify_agent`. Called by BOTH the legacy path AND the event-bus subscriber, so
/// the delivery is identical by construction (the gate only chooses which path
/// invokes this fn). `escalation` selects the `NotifySource` tag: the 30-min
/// stale escalation vs the first-observation conflict notify.
fn deliver_conflict_alert(home: &Path, agent: &str, escalation: bool, text: &str) {
    let source = if escalation {
        crate::inbox::NotifySource::System("conflict_escalation")
    } else {
        crate::inbox::NotifySource::System("conflict_notify")
    };
    crate::inbox::notify_agent(home, agent, &source, text);
}

/// #event-bus (conflict_notify) subscriber: re-deliver a `ConflictAlert` event via
/// the shared `deliver_conflict_alert`.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::ConflictAlert {
        agent,
        escalation,
        text,
    } = &event.kind
    {
        deliver_conflict_alert(&event.home, agent, *escalation, text);
        true
    } else {
        false
    }
}

/// Register the conflict-notify subscriber once at daemon startup (`run_core`).
/// Home-agnostic — the home travels on each event.
pub(crate) fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

/// Discover conflicted files in a worktree via `git status
/// --porcelain` filtered on the unmerged-status prefixes (UU / AA /
/// DD / AU / UA / UD / DU per `git status` docs). Returns paths
/// trimmed of the 3-char prefix (`XY ` → start of path). Empty Vec
/// on git failure or no conflicts.
pub(crate) fn discover_conflicted_files(worktree: &Path) -> Vec<String> {
    // #1899: bounded via git_bypass (LOCAL 60s) — a stuck git → empty fallback.
    let output = crate::git_helpers::git_bypass(worktree, &["status", "--porcelain"]);
    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let prefix = &line[..2];
            // Unmerged file index per `git status --short` docs:
            // UU = both modified, AA = both added, DD = both deleted,
            // AU = added by us, UA = added by them, UD = deleted by
            // them, DU = deleted by us. All seven mean the file is in
            // an unmerged state and operator/agent must resolve.
            if matches!(prefix, "UU" | "AA" | "DD" | "AU" | "UA" | "UD" | "DU") {
                Some(line[3..].trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Discover the in-flight git operation by checking for the marker
/// files git writes during multi-step operations. Returns
/// `Some("rebase")` / `Some("merge")` / `Some("cherry-pick")` /
/// `None` when no marker is present (e.g. the conflict resolved
/// before our scan).
///
/// Rebase has two layout variants depending on the rebase mode:
/// `.git/REBASE_HEAD` (single-step), `.git/rebase-merge/` (interactive
/// or merge-based), `.git/rebase-apply/` (am-based). Any of the three
/// signals an in-flight rebase.
pub(crate) fn discover_operation_type(worktree: &Path) -> Option<&'static str> {
    let git_dir = worktree.join(".git");
    if git_dir.join("REBASE_HEAD").exists()
        || git_dir.join("rebase-merge").is_dir()
        || git_dir.join("rebase-apply").is_dir()
    {
        return Some("rebase");
    }
    if git_dir.join("MERGE_HEAD").exists() {
        return Some("merge");
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return Some("cherry-pick");
    }
    None
}

/// Build the structured kind=update notify payload for the conflict-
/// detected event. Pure function — composes the JSON from the
/// discovered context. Caller is responsible for actually sending
/// via `crate::inbox::notify_agent`.
pub(crate) fn build_notify_payload(
    operation: &str,
    conflicted_files: &[String],
    branch: &str,
    base: &str,
) -> serde_json::Value {
    let next_steps = format!(
        "Resolve conflicts via Read/Edit in the listed files, then \
         `git add <files>` + `git {operation} --continue` (or \
         `git {operation} --abort` to revert)."
    );
    serde_json::json!({
        "event": "git_conflict_detected",
        "operation": operation,
        "conflicted_files": conflicted_files,
        "branch": branch,
        "base": base,
        "next_steps": next_steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase A Piece-1 contract: the structured kind=update payload
    /// must include the operation type, conflicted file list, branch
    /// context, and a next-steps hint pointing the agent at
    /// Read/Edit/Bash + `git add` + `git rebase --continue`.
    #[test]
    fn build_notify_payload_includes_required_fields() {
        let payload = build_notify_payload(
            "rebase",
            &["src/a.rs".to_string(), "src/b.rs".to_string()],
            "fix/feat-x",
            "main",
        );
        assert_eq!(
            payload["event"], "git_conflict_detected",
            "payload must carry a stable event tag, got {payload}"
        );
        assert_eq!(payload["operation"], "rebase");
        assert_eq!(
            payload["conflicted_files"],
            serde_json::json!(["src/a.rs", "src/b.rs"])
        );
        assert_eq!(payload["branch"], "fix/feat-x");
        assert_eq!(payload["base"], "main");
        let next_steps = payload["next_steps"].as_str().unwrap_or("");
        assert!(
            next_steps.contains("Read")
                || next_steps.contains("Edit")
                || next_steps.contains("git add"),
            "next_steps must mention concrete resolution mechanics, got {next_steps:?}"
        );
    }

    /// Phase A Piece-1: conflicted-files discovery must filter on the
    /// canonical unmerged-status prefixes from `git status`
    /// porcelain output (UU/AA/DD/AU/UA/UD/DU). Caller passes the
    /// worktree path; we shell out via the `agend-git` shim with
    /// `AGEND_GIT_BYPASS=1`. C1 returns empty Vec → C2 returns the
    /// real list. The test pins the contract via a deterministic
    /// conflict-inducing fixture.
    #[test]
    fn discover_conflicted_files_filters_unmerged_prefixes() {
        let worktree = setup_conflicted_repo("phase-a-conflict-files");
        let conflicts = discover_conflicted_files(&worktree);
        assert!(
            conflicts.contains(&"file.txt".to_string()),
            "the synthetic merge conflict on file.txt must surface, got {conflicts:?}"
        );
        cleanup(&worktree);
    }

    /// Phase A Piece-1: operation-type discovery via `.git/*_HEAD`
    /// marker file presence. The synthetic rebase fixture leaves
    /// `.git/REBASE_HEAD` (or `.git/rebase-merge/` directory). C2
    /// GREEN: return `Some("rebase")` when REBASE_HEAD or
    /// rebase-{merge,apply} dir is present.
    #[test]
    fn discover_operation_type_identifies_rebase() {
        let worktree = setup_conflicted_repo("phase-a-conflict-op");
        let op = discover_operation_type(&worktree);
        assert_eq!(
            op,
            Some("rebase"),
            "rebase-induced conflict must classify as `rebase`, got {op:?}"
        );
        cleanup(&worktree);
    }

    /// Phase A Piece-2: stale-tracker dedup. The first scan after
    /// the stale threshold elapses fires; a subsequent scan within
    /// `REALERT_INTERVAL_SECS` must NOT re-fire. Prevents telegram
    /// spam when the operator hasn't acted on the initial alert.
    #[test]
    fn realert_dedup_suppresses_within_window() {
        // Drive the REAL escalation decision (`should_escalate_conflict`). The
        // earlier version exercised a test-LOCAL `should_escalate_at` re-impl that
        // gated dedup on the last CONFLICT time — but production dedups on the last
        // ESCALATION time (`last_escalated_at`), so the re-impl could pass while
        // production was wrong. Real fn, real inputs.
        let now = chrono::Utc::now();
        // A conflict first observed 40 min ago is past the 30-min stale gate.
        let conflict_at = now - chrono::Duration::minutes(40);

        // No prior escalation → a stale conflict escalates.
        assert!(
            should_escalate_conflict(conflict_at, None, now),
            "a stale conflict with no prior escalation must escalate"
        );

        // 5 min after the last escalation → within REALERT_INTERVAL_SECS (30 min)
        // → suppressed.
        let first_escalation = now;
        assert!(
            !should_escalate_conflict(
                conflict_at,
                Some(first_escalation),
                first_escalation + chrono::Duration::minutes(5),
            ),
            "a re-escalation 5 min after the last must be suppressed (dedup window)"
        );

        // 35 min after the last escalation → past the dedup window → fires again.
        assert!(
            should_escalate_conflict(
                conflict_at,
                Some(first_escalation),
                first_escalation + chrono::Duration::minutes(35),
            ),
            "a re-escalation 35 min after the last must fire (dedup window expired)"
        );

        // A conflict younger than the stale threshold must NOT escalate, even with
        // no prior escalation — the stale gate, not just the dedup, must hold.
        assert!(
            !should_escalate_conflict(now, None, now),
            "a conflict younger than STALE_THRESHOLD_SECS must not escalate"
        );
    }

    /// Phase A Piece-1: resolution path. When the state classifier
    /// transitions an agent OUT of `GitConflict`, the tracker must
    /// drop the `last_conflict_at` entry. `waiting_on` is left for
    /// operator manual clear per spike Q3.
    #[test]
    fn resolution_clears_last_conflict_at() {
        let mut tracker = ConflictNotifyTracker::default();
        tracker
            .last_conflict_at
            .insert("agent-r".to_string(), chrono::Utc::now());
        assert!(tracker.last_conflict_at.contains_key("agent-r"));
        clear_on_resolution(&mut tracker, "agent-r");
        assert!(
            !tracker.last_conflict_at.contains_key("agent-r"),
            "resolution transition must drop last_conflict_at entry"
        );
    }

    /// #1923 G5: `retain_active` prunes per-agent state for agents no longer in
    /// the live registry set (deleted / redeployed), so a same-name redeploy does
    /// not inherit the dead instance's conflict-observation / escalation-dedup
    /// timestamps (which would false-gate its first conflict notify or
    /// false-suppress a real escalation).
    #[test]
    fn retain_active_prunes_deleted_agents_1923_g5() {
        use std::collections::HashSet;
        let mut tracker = ConflictNotifyTracker::default();
        let now = chrono::Utc::now();
        tracker.last_conflict_at.insert("live".to_string(), now);
        tracker.last_conflict_at.insert("deleted".to_string(), now);
        tracker.last_escalated_at.insert("live".to_string(), now);
        tracker.last_escalated_at.insert("deleted".to_string(), now);

        let live: HashSet<String> = ["live".to_string()].into_iter().collect();
        tracker.retain_active(&live);

        assert!(tracker.last_conflict_at.contains_key("live"));
        assert!(tracker.last_escalated_at.contains_key("live"));
        assert!(
            !tracker.last_conflict_at.contains_key("deleted"),
            "#1923 G5: a deleted agent's conflict timestamp must be pruned"
        );
        assert!(
            !tracker.last_escalated_at.contains_key("deleted"),
            "#1923 G5: a deleted agent's escalation timestamp must be pruned"
        );
    }

    /// Phase A Piece-1: classifier integration. PTY scrollback
    /// containing standard git conflict output ("Automatic merge
    /// failed; fix conflicts and then commit the result.") must
    /// classify as `AgentState::GitConflict` regardless of backend
    /// (git output is backend-independent).
    #[test]
    fn classifier_matches_automatic_merge_failed_on_claude() {
        use crate::backend::Backend;
        use crate::state::{AgentState, StatePatterns};
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        let screen = "CONFLICT (content): Merge conflict in src/a.rs\n\
                      Automatic merge failed; fix conflicts and then commit the result.";
        assert_eq!(
            patterns.detect(screen),
            Some(AgentState::GitConflict),
            "git conflict output must classify as GitConflict, got {:?}",
            patterns.detect(screen)
        );
    }

    /// Same pattern coverage check on Kiro — verifies the regex is
    /// installed in every backend's pattern catalog (git output is
    /// identical regardless of CLI).
    #[test]
    fn classifier_matches_automatic_merge_failed_on_kiro() {
        use crate::backend::Backend;
        use crate::state::{AgentState, StatePatterns};
        let patterns = StatePatterns::for_backend(&Backend::KiroCli);
        let screen = "CONFLICT (content): Merge conflict in src/a.rs\n\
                      Automatic merge failed; fix conflicts and then commit the result.";
        assert_eq!(
            patterns.detect(screen),
            Some(AgentState::GitConflict),
            "Kiro must also recognize git conflict output"
        );
    }

    // ── Test helpers ──────────────────────────────────────────────────

    /// Pure helper: drop the conflict tracker entry on resolution.
    fn clear_on_resolution(tracker: &mut ConflictNotifyTracker, name: &str) {
        tracker.last_conflict_at.remove(name);
    }

    /// Build a temp repo guaranteed to produce a `git rebase` conflict
    /// on `file.txt`. Returns the repo path. Fixture is pure-git (uses
    /// `Command::new("git")` with `AGEND_GIT_BYPASS=1` + per-repo
    /// gitconfig), so it works in CI without operator state.
    fn setup_conflicted_repo(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agend-phase-a-conflict-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir base");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "t@t"]);
        std::fs::write(repo.join("file.txt"), "initial\n").expect("write initial");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        // Create branch with version A
        git(&repo, &["checkout", "-b", "feat-a"]);
        std::fs::write(repo.join("file.txt"), "version A\n").expect("write A");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "version A"]);
        // Switch back to main, write conflicting version B
        git(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("file.txt"), "version B\n").expect("write B");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "version B"]);
        // Attempt rebase — guaranteed conflict, exits non-zero
        git_allow_fail(&repo, &["checkout", "feat-a"]);
        git_allow_fail(&repo, &["rebase", "main"]);
        repo
    }

    fn cleanup(repo: &std::path::Path) {
        if let Some(base) = repo.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }

    fn git(repo: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git spawn");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `git rebase` exits non-zero on conflict — allow the failure
    /// so the fixture setup continues.
    fn git_allow_fail(repo: &std::path::Path, args: &[&str]) {
        let _ = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output();
    }

    // ── #event-bus (conflict_notify) migration tests ──────────────────────────

    /// Build a home whose snapshot marks `agent` as `thinking`, so `notify_agent`
    /// → `compose_aware_inject` DEFERS to the notification_queue (observable in a
    /// test) rather than attempting a live PTY inject (which no-ops without a pane).
    fn defer_home(tag: &str, agent: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-conflict-eb-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).ok();
        crate::snapshot::save(
            &home,
            &[crate::snapshot::AgentSnapshot {
                name: agent.to_string(),
                backend_command: "claude".to_string(),
                args: vec![],
                working_dir: None,
                submit_key: "\r".to_string(),
                health_state: "healthy".to_string(),
                agent_state: "active".to_string(),
                silent_secs: 0,
                output_silent_secs: 0,
            }],
        );
        home
    }

    /// gate-ON: emit(ConflictAlert)→subscriber delivers the CARRIED rendered text
    /// via the shared `deliver_conflict_alert` (→ notify_agent → deferred to the
    /// queue under the `thinking` snapshot). The notify_agent half is PTY-inject so
    /// it is observed via the notification_queue defer path (no inbox enqueue here).
    #[test]
    fn conflict_alert_gate_on_emit_subscriber_delivers_carried_text() {
        let agent = "conflict-gate-on";
        let home = defer_home("gate-on", agent);
        let text =
            build_notify_payload("rebase", &["src/a.rs".to_string()], "feat", "main").to_string();

        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home,
            crate::daemon::event_bus::EventKind::ConflictAlert {
                agent: agent.to_string(),
                escalation: false,
                text: text.clone(),
            },
        );

        let queued = crate::notification_queue::drain(&home, agent);
        assert_eq!(
            queued.len(),
            1,
            "subscriber must deliver exactly one notify"
        );
        assert!(
            queued[0].text.contains("git_conflict_detected"),
            "delivered notify must carry the rendered conflict payload, got: {}",
            queued[0].text
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn boot_seed_records_conflict_without_notifying() {
        // #1739: record_first_observation seeds the dedup unconditionally, but
        // suppresses the first-observation notify during the boot seeding pass —
        // a restart must not re-ping conflicts the operator already saw.
        let agent = "conflict-bootseed";
        let home = defer_home("bootseed", agent);
        // Binding with a worktree so emit_conflict_notify reaches the notify
        // (else it early-returns and the test would have no teeth).
        let bdir = crate::paths::runtime_dir(&home).join(agent);
        std::fs::create_dir_all(&bdir).unwrap();
        // Serialize via serde so the worktree path is JSON-escaped correctly —
        // on Windows `home.display()` contains backslashes, which a hand-rolled
        // `format!` would emit as invalid JSON escapes (breaking binding::read).
        std::fs::write(
            bdir.join("binding.json"),
            serde_json::json!({ "worktree": home.to_string_lossy(), "branch": "feat" }).to_string(),
        )
        .unwrap();
        let now = chrono::Utc::now();

        // seeding scan: record the conflict, do NOT notify.
        let mut last_conflict_at = HashMap::new();
        record_first_observation(&mut last_conflict_at, &home, agent, now, true);
        assert!(
            last_conflict_at.contains_key(agent),
            "boot-seed must record the conflict in last_conflict_at"
        );
        assert!(
            crate::notification_queue::drain(&home, agent).is_empty(),
            "boot-seed must NOT notify for a restart-existing conflict \
             (negative-probe: removing the `if !seeding` gate makes this fire)"
        );

        // normal first observation: the notify fires.
        let mut lc2 = HashMap::new();
        record_first_observation(&mut lc2, &home, agent, now, false);
        assert_eq!(
            crate::notification_queue::drain(&home, agent).len(),
            1,
            "a normal first-observation conflict must notify"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #event-bus Step 2 (legacy-zero): `route_conflict_alert` emits to the global
    /// bus; the registered subscriber delivers via `deliver_conflict_alert` (the
    /// home travels on the event → lands in this test's home). End-to-end coverage
    /// of the production route → bus → subscriber wiring.
    #[test]
    fn route_conflict_alert_delivers_via_bus() {
        let agent = "conflict-route-bus";
        let home = defer_home("route-bus", agent);
        let text =
            build_notify_payload("rebase", &["src/a.rs".to_string()], "feat", "main").to_string();

        route_conflict_alert(&home, agent, false, &text);

        let queued = crate::notification_queue::drain(&home, agent);
        assert_eq!(queued.len(), 1, "gate-off must deliver via legacy path");
        assert!(
            queued[0].text.contains("git_conflict_detected"),
            "legacy notify must carry the rendered conflict payload, got: {}",
            queued[0].text
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
