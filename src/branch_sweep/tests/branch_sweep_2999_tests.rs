use std::path::Path;

use super::*;

fn four_merged_sweep_candidates(repo: &Path) -> Categories {
    for branch in [
        "feat-open",
        "feat-closed-a",
        "feat-closed-b",
        "feat-unknown",
    ] {
        create_branch_with_commit(repo, branch, &format!("merge candidate {branch}"));
        git_run(repo, &["merge", "--no-ff", "-m", "merge candidate", branch]);
    }
    scan(repo, "main", STALE_IDLE_DEFAULT_DAYS, chrono::Utc::now()).expect("scan")
}

/// #2999: an explicitly confirmed active_unknown batch is still non-terminal
/// and must not pay for an open-PR provider inventory that cannot affect its
/// fail-closed disposition.
#[test]
fn apply_active_unknown_batch_skips_open_pr_snapshot_2999() {
    let repo = setup_repo("2999-active-unknown-no-pr-probe");
    let home = repo.parent().unwrap().to_path_buf();
    git_run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    let tip_sha = create_branch_with_commit(&repo, "wip-active", "active unknown");
    let categories = Categories {
        active_unknown: vec![Candidate {
            name: "wip-active".into(),
            tip_sha,
            reason: "unmerged + fresh".into(),
        }],
        ..Categories::default()
    };
    let confirm_ids = ["wip-active".to_string()].into_iter().collect();
    let provider =
        crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Branches(vec![
            "wip-active".into(),
        ]));
    let _provider_guard = crate::scm::set_test_scm_provider(provider.clone());

    let (deleted, skipped) = emit_delete_batch_with_context(
        Some(&home),
        &repo,
        "main",
        &categories,
        &confirm_ids,
        "RED: active_unknown must skip provider probe",
    )
    .expect("apply");

    assert_eq!(
        provider.pr_list_calls(),
        0,
        "non-terminal batch must not probe open PRs"
    );
    assert_eq!(deleted, 0, "active_unknown must remain preserved");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["blocker"], "non_terminal");

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// #2999: the real multi-candidate apply path uses one bounded provider
/// inventory while preserving the open branch and deleting the three
/// closed branches.
#[test]
fn apply_batches_open_pr_snapshot_and_preserves_open_disposition_2999() {
    let repo = setup_repo("2999-open-pr-batch");
    let home = repo.parent().unwrap().to_path_buf();
    git_run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    let categories = four_merged_sweep_candidates(&repo);
    let confirm_ids = categories.deletable_ids().into_iter().collect();
    let provider =
        crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Branches(vec![
            "feat-open".into(),
        ]));
    let _provider_guard = crate::scm::set_test_scm_provider(provider.clone());

    let (deleted, skipped) = emit_delete_batch_with_context(
        Some(&home),
        &repo,
        "main",
        &categories,
        &confirm_ids,
        "RED: preserve open PR",
    )
    .expect("apply");

    assert_eq!(
        provider.pr_list_calls(),
        1,
        "one provider inventory must cover every terminal candidate"
    );
    assert_eq!(deleted, 3, "closed branches remain deletable: {skipped:?}");
    assert_eq!(skipped.len(), 1, "only the open branch should be skipped");
    assert_eq!(skipped[0]["branch"], "feat-open");
    assert_eq!(skipped[0]["blocker"], "open_pr");

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// #2999: a provider inventory failure preserves every terminal candidate
/// while the snapshot remains fail-closed for the whole sweep.
#[test]
fn apply_batches_open_pr_snapshot_and_fails_closed_on_provider_error_2999() {
    let repo = setup_repo("2999-open-pr-failure");
    let home = repo.parent().unwrap().to_path_buf();
    git_run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/repo.git",
        ],
    );
    let categories = four_merged_sweep_candidates(&repo);
    let confirm_ids = categories.deletable_ids().into_iter().collect();
    let provider =
        crate::scm::MockScmProvider::with_pr_list(crate::scm::MockPrList::Fail("offline".into()));
    let _provider_guard = crate::scm::set_test_scm_provider(provider.clone());

    let (deleted, skipped) = emit_delete_batch_with_context(
        Some(&home),
        &repo,
        "main",
        &categories,
        &confirm_ids,
        "RED: preserve on provider error",
    )
    .expect("apply");

    assert_eq!(
        provider.pr_list_calls(),
        1,
        "one failed provider inventory must cover the whole sweep"
    );
    assert_eq!(deleted, 0, "provider failure must fail closed");
    assert_eq!(skipped.len(), 4, "every candidate must be preserved");
    assert!(skipped
        .iter()
        .all(|entry| entry["blocker"] == "open_pr_status_unknown"));

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}
