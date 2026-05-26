use std::path::Path;

use super::gh_poll;
use super::{
    apply, format_ready_body, pr_state_dir, remove, resolve_author, save, DraftState, Event,
    MergeState, PrState,
};

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
        let mut state: PrState = match serde_json::from_str(&content) {
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
        let mut dirty = false;

        // Emit [pr-ready-for-merge] if eligible and not already fired.
        if matches!(state.merge_state, MergeState::MergeReady)
            && state.ready_emitted_for_sha.as_deref() != Some(state.head_sha.as_str())
        {
            let author = resolve_author(&state);
            let body = format_ready_body(&state);
            let msg = build_event_message("pr-ready-for-merge", &author, &state, body);
            if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &author, msg) {
                tracing::warn!(
                    repo = %state.repo,
                    branch = %state.branch,
                    error = %e,
                    "#972 pr_state: [pr-ready-for-merge] enqueue failed"
                );
            } else {
                state.ready_emitted_for_sha = Some(state.head_sha.clone());
                dirty = true;
                tracing::info!(
                    repo = %state.repo,
                    branch = %state.branch,
                    head = %state.head_sha,
                    author = %author,
                    "#972 pr_state: [pr-ready-for-merge] emitted"
                );
            }
        }

        // Terminal-state sweep.
        //
        // #1017: each terminal-state branch first checks the
        // `ready_emitted_for_sha` debounce gate. The startup hook
        // `suppress_stale_terminal_replay` sets this to `Some(head_sha)`
        // for files older than the replay-age threshold so they are
        // swept (file removed) without firing a stale event. Fresh
        // terminal-state files have `ready_emitted_for_sha == None`
        // and fire normally.
        let already_emitted =
            state.ready_emitted_for_sha.as_deref() == Some(state.head_sha.as_str());
        match &state.merge_state {
            MergeState::Merged {
                merge_commit,
                merged_at,
            } => {
                if !already_emitted {
                    let author = resolve_author(&state);
                    let body = format!(
                        "[pr-merged] {}@{} (merge_commit {}, merged_at {})",
                        state.repo,
                        state.branch,
                        &merge_commit[..8.min(merge_commit.len())],
                        merged_at,
                    );
                    let _ = crate::inbox::enqueue_with_idle_hint(
                        home,
                        &author,
                        build_event_message("pr-merged", &author, &state, body),
                    );
                    // #1287: set dedup flag and persist — file removal
                    // deferred to the next scan so the flag survives
                    // if gh_poll recreates the watch file.
                    state.ready_emitted_for_sha = Some(state.head_sha.clone());
                    dirty = true;
                } else {
                    tracing::debug!(
                        repo = %state.repo,
                        branch = %state.branch,
                        head = %state.head_sha,
                        "#1017 pr_state: stale Merged replay suppressed at scan"
                    );
                    let _ = remove(home, &state.repo, &state.branch);
                    continue;
                }
            }
            MergeState::ClosedUnmerged { closed_at } => {
                if !already_emitted {
                    let author = resolve_author(&state);
                    let body = format!(
                        "[pr-closed-unmerged] {}@{} (closed_at {})",
                        state.repo, state.branch, closed_at
                    );
                    let _ = crate::inbox::enqueue_with_idle_hint(
                        home,
                        &author,
                        build_event_message("pr-closed-unmerged", &author, &state, body),
                    );
                    state.ready_emitted_for_sha = Some(state.head_sha.clone());
                    dirty = true;
                } else {
                    tracing::debug!(
                        repo = %state.repo,
                        branch = %state.branch,
                        head = %state.head_sha,
                        "#1017 pr_state: stale ClosedUnmerged replay suppressed at scan"
                    );
                    let _ = remove(home, &state.repo, &state.branch);
                    continue;
                }
            }
            _ => {}
        }

        if dirty {
            if let Err(e) = save(home, &state) {
                tracing::warn!(
                    repo = %state.repo,
                    branch = %state.branch,
                    error = %e,
                    "#972 pr_state: post-emit save failed"
                );
            }
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
    use std::collections::HashMap;

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "#1002 apply_gh_poll read_dir failed — gh-poll skipped this tick"
            );
            return;
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    // Group files by repo + collect those due for refresh.
    let mut by_repo: HashMap<String, Vec<PrState>> = HashMap::new();
    let mut skipped_terminal = 0u32;
    let mut skipped_should_poll = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
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
        if gh_poll::should_poll(&state, &now) {
            by_repo.entry(state.repo.clone()).or_default().push(state);
        } else {
            skipped_should_poll = skipped_should_poll.saturating_add(1);
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
                for mut state in due_states {
                    apply_gh_observations(home, &mut state, &prs, &now);
                    state.gh_poll_failures = 0;
                    state.last_gh_poll_at = Some(now.clone());
                    if let Err(e) = save(home, &state) {
                        tracing::warn!(
                            repo = %state.repo,
                            branch = %state.branch,
                            error = %e,
                            "#986 pr_state: post-gh-poll save failed"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "#986 gh-poll failed");
                for mut state in due_states {
                    state.gh_poll_failures = state.gh_poll_failures.saturating_add(1);
                    state.last_gh_poll_at = Some(now.clone());
                    let _ = save(home, &state);
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
