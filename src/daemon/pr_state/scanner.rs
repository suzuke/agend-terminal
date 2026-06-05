use std::path::Path;

use super::gh_poll;
use super::{
    apply, format_ready_body, pr_state_dir, remove, resolve_author, resolve_notify_recipient,
    with_pr_state, DraftState, Event, MergeState, PrState,
};

enum ScanAction {
    None,
    Saved,
    Remove,
}

pub fn scan_and_emit(home: &Path, registry: &crate::agent::AgentRegistry) {
    scan_and_emit_with(home, registry, &gh_poll::CliGhPoller);
}

/// Per-tick scanner: walks `<home>/pr-state/*.json`, emits any newly-
/// eligible `[pr-ready-for-merge]` events (debounced via
/// `ready_emitted_for_sha`), and sweeps terminal-state files.
///
/// gh-poll for pr_number/pr_author/draft/merge state is fired here
/// (rate-limited — at most one gh call per scanner tick per file).
pub fn scan_and_emit_with(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    poller: &dyn gh_poll::GhPoller,
) {
    let dir = pr_state_dir(home);
    // #986: Phase 1 — batched gh-poll per repo for files due.
    apply_gh_poll(home, &dir, poller);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "#1002 pr_state: scan_and_emit_with read_dir failed — skipping tick"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 pr_state: scan_and_emit_with read_to_string failed — skipping file"
                );
                continue;
            }
        };
        let snapshot: PrState = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 pr_state: scan_and_emit_with json parse failed — skipping file"
                );
                continue;
            }
        };
        let repo = snapshot.repo.clone();
        let branch = snapshot.branch.clone();

        // #1342: all emit + flag-set under flock to prevent lost-update race.
        // #bughunt3 (#1617 lock-while-blocking class): the worktree auto-release
        // does a `git` subprocess + acquires a SECOND (binding) flock — it must
        // NOT run under the PR-state flock. The closure only records the branch
        // to release; the actual release runs after `with_pr_state` returns and
        // the flock is dropped (see below).
        // t-worktree-leak (PR-1): (branch, event_kind) of a terminal PR transition
        // observed this scan — the release-invariant recompute runs post-flock.
        let mut release_after_unlock: Option<(String, &'static str)> = None;
        // #1629: collect deferred inbox emits here under the flock; drain them
        // AFTER `with_pr_state` returns so enqueue_with_idle_hint (self-IPC via
        // loopback api::call) runs lock-free (#1617 lock-while-blocking class).
        let mut pending_emits: Vec<(String, crate::inbox::InboxMessage)> = Vec::new();
        let result = with_pr_state(home, &repo, &branch, |state| {
            let mut dirty = false;

            // Emit [pr-ready-for-merge] if eligible and not already fired.
            if matches!(state.merge_state, MergeState::MergeReady)
                && state.ready_emitted_for_sha.as_deref() != Some(state.head_sha.as_str())
            {
                // #2 (t-verdict-to-author-routing) Gap C: route to the BINDING-
                // resolved author (shared-account-proof), not `resolve_author`'s
                // gh-login chain whose last resort is a hard-coded "fixup-lead".
                let author = resolve_notify_recipient(home, state);
                let body = format_ready_body(state);
                let msg = build_event_message("pr-ready-for-merge", &author, state, body);
                // #1629: defer the enqueue (see top of fn). Set the dedup flag
                // optimistically under the flock. NOTE: this is a behavior change
                // for the pr-ready arm — previously the flag was set only on
                // enqueue success, so a failed enqueue retried next scan. Now a
                // post-unlock enqueue failure leaves the flag already set → next
                // scan skips → the pr-ready signal is lost (warn-logged at the
                // drain, not silent) until head_sha changes. The flag IS the dedup
                // ledger; it suppresses re-emit, it does not back-stop. Accepted:
                // enqueue failure is near-zero (local inbox write), this matches
                // the Merged/ClosedUnmerged arms, and the flock-free emit prevents
                // the #1617 deadlock.
                state.ready_emitted_for_sha = Some(state.head_sha.clone());
                dirty = true;
                tracing::info!(
                    repo = %state.repo,
                    branch = %state.branch,
                    head = %state.head_sha,
                    author = %author,
                    "#972 pr_state: [pr-ready-for-merge] queued (emit after flock drop)"
                );
                pending_emits.push((author, msg));
            }

            // Terminal-state sweep.
            let already_emitted =
                state.ready_emitted_for_sha.as_deref() == Some(state.head_sha.as_str());
            match &state.merge_state {
                MergeState::Merged {
                    merge_commit,
                    merged_at,
                } => {
                    if !already_emitted {
                        // #1344/#bughunt3: defer the worktree auto-release until
                        // AFTER this flock is dropped (the git subprocess +
                        // nested binding flock are the #1617 lock-while-blocking
                        // class). Record the branch; release runs post-unlock.
                        release_after_unlock = Some((state.branch.clone(), "merge"));
                        let author = resolve_author(state);
                        let body = format!(
                            "[pr-merged] {}@{} (merge_commit {}, merged_at {})\n\n\
                             ⚠ Action checklist:\n\
                             1. `gh issue close` (if linked issue)\n\
                             2. `task action=done` (if correlation_id present)\n\
                             3. Report completion to lead",
                            state.repo,
                            state.branch,
                            &merge_commit[..8.min(merge_commit.len())],
                            merged_at,
                        );
                        // #1629: defer the enqueue (see top of fn) — run lock-free
                        // after the flock drops.
                        let msg = build_event_message("pr-merged", &author, state, body);
                        pending_emits.push((author, msg));
                        state.ready_emitted_for_sha = Some(state.head_sha.clone());
                        dirty = true;
                    } else {
                        tracing::debug!(
                            repo = %state.repo,
                            branch = %state.branch,
                            head = %state.head_sha,
                            "#1017 pr_state: stale Merged replay suppressed at scan"
                        );
                        return ScanAction::Remove;
                    }
                }
                MergeState::ClosedUnmerged { closed_at } => {
                    if !already_emitted {
                        let author = resolve_author(state);
                        let body = format!(
                            "[pr-closed-unmerged] {}@{} (closed_at {})\n\n\
                             ⚠ Action checklist:\n\
                             1. `release_worktree` for branch `{}`\n\
                             2. Investigate closure reason (operator decision? superseded?)\n\
                             3. Report to lead with context",
                            state.repo, state.branch, closed_at, state.branch,
                        );
                        // #1629: defer the enqueue (see top of fn) — run lock-free
                        // after the flock drops.
                        let msg = build_event_message("pr-closed-unmerged", &author, state, body);
                        pending_emits.push((author, msg));
                        state.ready_emitted_for_sha = Some(state.head_sha.clone());
                        // t-worktree-leak (PR-1): close-unmerged is a terminal PR
                        // transition → recompute the release invariant post-flock
                        // (the sweeper applies the conservative close-grace).
                        release_after_unlock = Some((state.branch.clone(), "close_unmerged"));
                        dirty = true;
                    } else {
                        tracing::debug!(
                            repo = %state.repo,
                            branch = %state.branch,
                            head = %state.head_sha,
                            "#1017 pr_state: stale ClosedUnmerged replay suppressed at scan"
                        );
                        return ScanAction::Remove;
                    }
                }
                _ => {}
            }

            if dirty {
                ScanAction::Saved
            } else {
                ScanAction::None
            }
        });

        // #1629 (#1617 class): PR-state flock is now released — drain the deferred
        // inbox emits lock-free (enqueue_with_idle_hint self-IPCs via loopback
        // api::call). Emit BEFORE auto_release to preserve the prior
        // under-flock-emit → post-unlock-release ordering.
        for (author, msg) in pending_emits {
            if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &author, msg) {
                tracing::warn!(
                    author = %author,
                    error = %e,
                    "#1629 pr_state: deferred emit enqueue failed"
                );
            }
        }

        // #bughunt3 (#1617 class): PR-state flock is now released — run the
        // release-invariant recompute lock-free. t-worktree-leak (PR-1): merge
        // keeps the named entry (the #1617 invariant test pins it); close-unmerged
        // enqueues through the same HYBRID path with its own event kind.
        if let Some((branch, event_kind)) = release_after_unlock {
            match event_kind {
                "merge" => crate::daemon::auto_release::auto_release_for_merged_branch(
                    home, &repo, &branch,
                ),
                _ => crate::daemon::auto_release::enqueue_release_recompute(
                    home, &repo, &branch, event_kind,
                ),
            }
        }

        match result {
            Ok(Some(ScanAction::Remove)) => {
                let _ = remove(home, &repo, &branch);
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo,
                    branch = %branch,
                    error = %e,
                    "#972 pr_state: post-emit save failed"
                );
            }
            _ => {}
        }
        let _ = registry; // reserved for future gh-poll author lookup hook
    }
}

/// #986: batched gh-poll feeder. Groups PrState files by repo, issues
/// ONE `gh pr list` per repo with at least one file due for refresh,
/// then applies each PR's metadata back to its matching file by
/// `head_ref → branch`. Tiered cadence (15s armed / 60s default) +
/// exponential backoff on failures (`2^failures × tick` capped 300s).
///
/// Failures bump per-PrState `gh_poll_failures` (per-repo failures
/// would over-suppress); success clears the counter. Idempotent:
/// re-applying the same metadata is a no-op for the reducer if state
/// already matches.
fn apply_gh_poll(home: &Path, dir: &Path, poller: &dyn gh_poll::GhPoller) {
    use std::collections::{HashMap, HashSet};

    // PR-3 (t-ci-ready-pr3-arm-not-armed): the pr_state dir may be ABSENT (cold
    // start / no PR tracked yet). That must NOT skip the gh-poll — the
    // bound-branch seed below still drives discovery of unwatched open PRs. So
    // tolerate a missing/unreadable dir (empty iterator) instead of early
    // returning, which previously bypassed the whole auto-arm path.
    let entries = std::fs::read_dir(dir).ok();
    let now = chrono::Utc::now().to_rfc3339();
    // Group files by repo + collect those due for refresh.
    let mut by_repo: HashMap<String, Vec<PrState>> = HashMap::new();
    // PR-3 (t-ci-ready-pr3-arm-not-armed): every repo with a non-terminal
    // pr-state is already cadence-managed (it polls on its own `should_poll`
    // schedule). Track them so the bound-branch seed below does NOT force an
    // extra poll on an already-known repo (which would defeat `should_poll`).
    let mut seen_repos: HashSet<String> = HashSet::new();
    let mut skipped_terminal = 0u32;
    let mut skipped_should_poll = 0u32;
    for entry in entries.into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 apply_gh_poll read_to_string failed — skipping file"
                );
                continue;
            }
        };
        let state: PrState = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 apply_gh_poll json parse failed — skipping file"
                );
                continue;
            }
        };
        // Skip already-terminal states — they'll be swept by the
        // main scanner loop on this pass.
        if matches!(
            state.merge_state,
            MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
        ) {
            skipped_terminal = skipped_terminal.saturating_add(1);
            continue;
        }
        // Non-terminal pr-state → this repo is cadence-managed already.
        seen_repos.insert(state.repo.clone());
        if gh_poll::should_poll(&state, &now) {
            by_repo.entry(state.repo.clone()).or_default().push(state);
        } else {
            skipped_should_poll = skipped_should_poll.saturating_add(1);
        }
    }

    // PR-3 (t-ci-ready-pr3-arm-not-armed): seed the poll-repo list from LIVE
    // BOUND BRANCHES whose repo has NO non-terminal pr-state yet. This is the
    // discovery path #1782 needs — a bypass / non-dispatch PR has neither a
    // ci-watch nor a pr-state, so its repo would never be polled from pr-state
    // alone. Each bound agent's binding.json carries the `source_repo`; resolve
    // it to a gh slug and poll it (empty `due_states` → the poll just feeds
    // `auto_arm_unwatched_open_prs`). `seen_repos` keeps this from re-polling a
    // repo the cadence already manages.
    for src_path in crate::binding::bound_source_repos(home) {
        if let Some(slug) =
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(&src_path)
        {
            if !seen_repos.contains(&slug) {
                by_repo.entry(slug).or_default();
            }
        }
    }
    if !by_repo.is_empty() || skipped_terminal > 0 || skipped_should_poll > 0 {
        tracing::debug!(
            repos_to_poll = by_repo.len(),
            skipped_terminal,
            skipped_should_poll_cadence = skipped_should_poll,
            "#1002 apply_gh_poll grouping done"
        );
    }
    for (repo, due_states) in by_repo {
        match poller.poll(&repo) {
            Ok(prs) => {
                for state in due_states {
                    let branch = state.branch.clone();
                    if let Err(e) = with_pr_state(home, &repo, &branch, |s| {
                        apply_gh_observations(home, s, &prs, &now);
                        s.gh_poll_failures = 0;
                        s.last_gh_poll_at = Some(now.clone());
                    }) {
                        tracing::warn!(
                            repo = %repo,
                            branch = %branch,
                            error = %e,
                            "#986 pr_state: post-gh-poll save failed"
                        );
                    }
                }
                // #1750-B4: piggyback remote-orphan branch GC on the poll just
                // done — `prs` already carries every PR's {state, head_ref,
                // merged_at}, so no second poller. Best-effort; never blocks the
                // scanner.
                super::remote_gc::gc_remote_orphans(&repo, &prs);
                // PR-3 (t-ci-ready-pr3-arm-not-armed): same piggyback — auto-arm a
                // ci-watch for any OPEN PR with no armed watch. Closes the
                // bypass/non-dispatch arm-not-armed gap (#1782) server-side, the
                // only place a `--no-verify` bypass push is observable.
                super::auto_arm::auto_arm_unwatched_open_prs(home, &repo, &prs);
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "#986 gh-poll failed");
                for state in due_states {
                    let branch = state.branch.clone();
                    let _ = with_pr_state(home, &repo, &branch, |s| {
                        s.gh_poll_failures = s.gh_poll_failures.saturating_add(1);
                        s.last_gh_poll_at = Some(now.clone());
                    });
                }
            }
        }
    }
}

/// Apply gh-poll observations to a single PrState. Detects state
/// transitions and dispatches the appropriate reducer events:
/// - `state=MERGED + mergedAt!=None` → `MergedObserved`
/// - `state=CLOSED + mergedAt=None` → `ClosedUnmergedObserved`
/// - `isDraft` toggle → `DraftTransition`
/// - First observation: populate `pr_number` + `pr_author` from the
///   matching metadata (no Event needed; direct field assignment).
fn apply_gh_observations(
    home: &Path,
    state: &mut PrState,
    prs: &[gh_poll::GhPrMetadata],
    now: &str,
) {
    let Some(meta) = prs.iter().find(|m| m.head_ref == state.branch) else {
        return;
    };

    // First observation — populate identity fields.
    if state.pr_author.is_empty() {
        state.pr_number = meta.number;
        state.pr_author = gh_poll::resolve_author_with_gh(home, Some(&meta.author_login), state);
        tracing::info!(
            repo = %state.repo,
            branch = %state.branch,
            pr_number = state.pr_number,
            pr_author = %state.pr_author,
            "#986 pr_state: first-observation populated PR identity"
        );
    }

    // Draft transition.
    let new_draft = meta.is_draft;
    let old_draft = matches!(state.draft_state, DraftState::Draft);
    if new_draft != old_draft {
        apply(
            state,
            Event::DraftTransition {
                is_draft: new_draft,
            },
        );
    }

    // Terminal state transitions.
    if let Some(prev) = state.last_gh_state.as_ref() {
        if prev.state != meta.state {
            match (meta.state, meta.merged_at.as_deref()) {
                (gh_poll::GhPrState::Merged, Some(merged_at)) => {
                    apply(
                        state,
                        Event::MergedObserved {
                            // gh CLI doesn't return the merge commit hash
                            // in `pr list`; use head_sha as best-effort
                            // identifier. Operator can `gh pr view` for
                            // the real commit hash.
                            merge_commit: &state.head_sha.clone(),
                            merged_at: merged_at.to_string(),
                        },
                    );
                }
                (gh_poll::GhPrState::Closed, _) if meta.merged_at.is_none() => {
                    apply(
                        state,
                        Event::ClosedUnmergedObserved {
                            closed_at: now.to_string(),
                        },
                    );
                }
                _ => {}
            }
        }
    } else {
        // First observation — also catches case where PR was already
        // merged before we started watching.
        match (meta.state, meta.merged_at.as_deref()) {
            (gh_poll::GhPrState::Merged, Some(merged_at)) => {
                apply(
                    state,
                    Event::MergedObserved {
                        merge_commit: &state.head_sha.clone(),
                        merged_at: merged_at.to_string(),
                    },
                );
            }
            (gh_poll::GhPrState::Closed, _) if meta.merged_at.is_none() => {
                apply(
                    state,
                    Event::ClosedUnmergedObserved {
                        closed_at: now.to_string(),
                    },
                );
            }
            _ => {}
        }
    }

    state.last_gh_state = Some(meta.clone());
}

fn build_event_message(
    kind: &str,
    _author: &str,
    state: &PrState,
    body: String,
) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage::new_system("system:pr-state", kind, body)
        // #946 grep target: `{repo}@{branch}` canonical form
        .with_correlation_id(format!("{}@{}", state.repo, state.branch))
        .with_reviewed_head(state.head_sha.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    /// #bughunt3 invariant (#1617 lock-while-blocking class): the worktree
    /// auto-release does a `git` subprocess + acquires a second (binding) flock,
    /// so it must NEVER run inside the `with_pr_state` closure — that closure
    /// runs under the PR-state flock. Structural source-scan (mirrors #1593 F2):
    /// brace-match the `|state| { ... }` closure body and assert
    /// `auto_release_for_merged_branch` is NOT called inside it, and IS called
    /// after the closure (lock-free, post-unlock). Needle is `concat`-built and
    /// the scan is prod-sliced so this test can't self-satisfy.
    #[test]
    fn auto_release_not_called_under_pr_state_flock() {
        let src = include_str!("scanner.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        let closure_needle = [", |state|", " {"].concat();
        let cstart = prod
            .find(&closure_needle)
            .expect("with_pr_state closure present");

        // Brace-match from the closure's opening `{` to find its body span.
        let open_rel = prod[cstart..].find('{').expect("closure block opens");
        let block_start = cstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "closure block must close");

        let release_needle = ["auto_release_for", "_merged_branch"].concat();
        let closure_body = &prod[block_start..=block_end];
        assert!(
            !closure_body.contains(&release_needle),
            "auto_release_for_merged_branch must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
        assert!(
            prod[block_end..].contains(&release_needle),
            "auto_release_for_merged_branch must run AFTER the PR-state flock is dropped"
        );
    }

    /// #1629 invariant (#1617 lock-while-blocking class): the inbox emit
    /// (`enqueue_with_idle_hint` → loopback `api::call`) must NEVER run inside
    /// the `with_pr_state` closure, which holds the PR-state flock. The emits are
    /// collected under the flock and drained after it drops. Same structural
    /// source-scan as the auto_release invariant above: brace-match the closure
    /// body and assert `enqueue_with_idle_hint` is NOT inside it and IS called
    /// after. Needle is `concat`-built and the scan is prod-sliced so this test
    /// can't self-satisfy.
    #[test]
    fn deferred_emit_not_called_under_pr_state_flock() {
        let src = include_str!("scanner.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        let closure_needle = [", |state|", " {"].concat();
        let cstart = prod
            .find(&closure_needle)
            .expect("with_pr_state closure present");
        let open_rel = prod[cstart..].find('{').expect("closure block opens");
        let block_start = cstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "closure block must close");

        let emit_needle = ["enqueue_with", "_idle_hint"].concat();
        let closure_body = &prod[block_start..=block_end];
        assert!(
            !closure_body.contains(&emit_needle),
            "enqueue_with_idle_hint must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
        assert!(
            prod[block_end..].contains(&emit_needle),
            "enqueue_with_idle_hint must run AFTER the PR-state flock is dropped (deferred drain)"
        );
    }
}
