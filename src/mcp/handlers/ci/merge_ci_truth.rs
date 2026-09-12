//! #3589: provider-backed CI truth recovery for the merge gate.

use std::path::Path;

pub(super) fn load_exact_merge_state(
    home: &Path,
    repo: &str,
    pr: u64,
    branch: &str,
    head: &str,
) -> Option<crate::daemon::pr_state::PrState> {
    let state = crate::daemon::pr_state::load(home, repo, branch)?;
    (state.repo == repo
        && state.pr_number == pr
        && state.branch == branch
        && state.head_sha == head)
        .then_some(state)
}

pub(super) fn refresh_ci_truth(
    home: &Path,
    repo: &str,
    pr: u64,
    branch: &str,
    head: &str,
) -> Option<crate::daemon::pr_state::PrState> {
    // The caller has already completed the provider and final identity fences.
    // Reload here so hydration uses the latest review class and subscribers.
    let state = load_exact_merge_state(home, repo, pr, branch, head)?;
    if !matches!(
        &state.ci_state,
        crate::daemon::pr_state::CiState::Green { sha, .. } if sha == head
    ) {
        crate::daemon::pr_state::record_ci_result(
            home,
            repo,
            branch,
            head,
            crate::daemon::pr_state::CiConclusion::Green,
            state.subscribers.clone(),
            state.review_class,
        );
    }
    load_exact_merge_state(home, repo, pr, branch, head)
}
