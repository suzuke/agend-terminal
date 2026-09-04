//! #3517 follow-up: the head-drift window the merge-retirement ordering leaves
//! open, and the three controls that keep the fix from becoming a row factory.
//!
//! Re-homed out of `cleanup_intents.rs` so the anti-monolith ceiling stays met:
//! the parent is a grandfathered 2437-line file and these cases would have
//! pushed it to 2651 against a 2500 cap.

use super::*;

/// Open (non-terminal) branch-retention rows for one lane, with their
/// retention-key tags — enough to say both "how many" and "at which head".
fn open_lane_rows(home: &Path, repo: &str, branch: &str) -> Vec<crate::task_events::TaskRecord> {
    let project = crate::tasks::resolve_repository_project(home, Path::new(repo));
    let board = crate::task_events::board_root(home, &project);
    let lane = crate::worktree_pool::retention_lane_key(repo, branch);
    crate::task_events::projected_state_at(&board)
        .expect("board state")
        .tasks
        .values()
        .filter(|t| {
            t.tags
                .iter()
                .any(|tag| tag == crate::worktree_pool::RETENTION_TAG)
                && t.tags.contains(&lane)
                && !t.status.is_terminal()
        })
        .cloned()
        .collect()
}

/// Add a commit to `branch` and return the new tip — the post-merge commit
/// that opens the window.
fn commit_onto(repo: &Path, branch: &str, file: &str) -> String {
    git_in(repo, &["checkout", branch]);
    std::fs::write(repo.join(file), "late").ok();
    git_in(repo, &["add", "."]);
    git_in(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "post-merge work",
        ],
    );
    let tip = branch_tip(repo, branch);
    git_in(repo, &["checkout", "main"]);
    tip
}

/// #3517 follow-up RED: the window #3517 left open.
///
/// `retire_merged_lane` runs BEFORE the head-drift CAS in `settle_intent`,
/// deliberately — refusing to retire on drift is what produced #3503's noise,
/// where a post-merge CI commit held the row open forever. The cost is this:
/// a lane that merges and THEN gains a commit has its owner row closed while
/// branch and intent both survive. The self-healing argument ("a later
/// release at a new head raises a fresh row") only holds if a release ever
/// happens again — if nobody opens a worktree on that branch again, nothing
/// raises anything and the residue is owned by no one.
///
/// The merge itself is not re-observed here: the sweep learns it from
/// `branch_sweep::merged_pr_number`, which requires a GitHub remote and a
/// live SCM call. The state that matters is what the merge LEFT BEHIND — an
/// auto-retired row — and that is produced here by the real retirement
/// function, not a stub.
#[test]
fn sweep_reraises_a_merge_retired_lane_whose_branch_moved_3517() {
    let home = tmp_dir("drift-reraise");
    let repo = tmp_repo("drift-reraise-repo");
    let branch = "feat/drifted-after-merge";
    let rs = repo.display().to_string();
    let merged_head = make_branch(&repo, branch);
    persist_intent(&home, &rs, branch, &merged_head, "t-origin", None, None).expect("persist");
    let merged_row = seed_obligation(&home, &rs, branch, &merged_head, Some("agent-owner"));

    // #3517: the merge retires the row. Branch and intent survive.
    owner_attestation::retire_merged_lane(&home, &rs, branch);
    assert!(
        open_lane_rows(&home, &rs, branch).is_empty(),
        "precondition: the merge must have closed the owner's row"
    );

    // …and then the branch gains a commit that no merge ever settles.
    let drifted_head = commit_onto(&repo, branch, "late.txt");
    assert_ne!(drifted_head, merged_head, "the branch must have moved");

    sweep_settle_merged(&home);

    let rows = open_lane_rows(&home, &rs, branch);
    assert_eq!(
        rows.len(),
        1,
        "a merge-retired lane whose branch then moved must get ONE fresh \
         obligation from the sweep — nothing else will ever raise it"
    );
    assert!(
        rows[0].tags.contains(&crate::worktree_pool::retention_key(
            &rs,
            branch,
            &drifted_head
        )),
        "the fresh obligation must be keyed at the DRIFTED head, not the \
         merged one — that is what makes it a new question and what keeps \
         re-raising idempotent"
    );
    assert!(
        branch_exists(&repo, branch) && has_intent(&home, &rs, branch),
        "re-raising must not disturb the preserved branch or its intent"
    );
    // #3503 stays fixed: merge still retires, and the row it retired stays
    // retired. The fresh question is a NEW row at the new head, never the old
    // one held open — that distinction is the whole reason the call order in
    // `settle_intent` does not have to change.
    assert_ne!(
        rows[0].id.0, merged_row,
        "the merge-retired row must not be the one reopened"
    );
    assert!(
        obligation(&home, &merged_row).status.is_terminal(),
        "#3503: the row the merge retired must STAY retired"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Negative control 1 — the ordinary lane. Merge retires the row, the branch
/// does NOT move: there is nothing left unattested, so the sweep must raise
/// nothing. A fix that manufactures an obligation here is worse than the gap
/// it closes, and it would re-open #3503's symptom by another door.
#[test]
fn sweep_raises_nothing_for_a_merge_retired_lane_that_never_drifted_3517() {
    let home = tmp_dir("drift-none");
    let repo = tmp_repo("drift-none-repo");
    let branch = "feat/clean-after-merge";
    let rs = repo.display().to_string();
    let merged_head = make_branch(&repo, branch);
    persist_intent(&home, &rs, branch, &merged_head, "t-origin", None, None).expect("persist");
    seed_obligation(&home, &rs, branch, &merged_head, Some("agent-owner"));
    owner_attestation::retire_merged_lane(&home, &rs, branch);

    sweep_settle_merged(&home);

    assert!(
        open_lane_rows(&home, &rs, branch).is_empty(),
        "no drift means no unattested work — the sweep must not invent an obligation"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Negative control 2 — standing drift must cost ONE row, not one per tick.
/// The sweep runs forever; an obligation raised on every pass would be a
/// worse board problem than the silence it replaces.
#[test]
fn sweep_reraises_once_not_once_per_tick_3517() {
    let home = tmp_dir("drift-idempotent");
    let repo = tmp_repo("drift-idempotent-repo");
    let branch = "feat/standing-drift";
    let rs = repo.display().to_string();
    let merged_head = make_branch(&repo, branch);
    persist_intent(&home, &rs, branch, &merged_head, "t-origin", None, None).expect("persist");
    seed_obligation(&home, &rs, branch, &merged_head, Some("agent-owner"));
    owner_attestation::retire_merged_lane(&home, &rs, branch);
    commit_onto(&repo, branch, "late.txt");

    sweep_settle_merged(&home);
    sweep_settle_merged(&home);
    sweep_settle_merged(&home);

    assert_eq!(
        open_lane_rows(&home, &rs, branch).len(),
        1,
        "a standing drift is one question, however many times the sweep asks it"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Negative control 3 — the row a PERSON answered is not the row this fix is
/// about. #3517's window is specifically a row the SYSTEM closed on merge
/// while the branch moved on; an answered row means an owner already decided
/// about this lane, and re-asking on drift is a different policy question
/// that this change does not get to make on its own.
#[test]
fn sweep_does_not_reraise_a_lane_its_owner_answered_3517() {
    let home = tmp_dir("drift-answered");
    let repo = tmp_repo("drift-answered-repo");
    let branch = "feat/answered-then-drifted";
    let rs = repo.display().to_string();
    let answered_head = make_branch(&repo, branch);
    persist_intent(&home, &rs, branch, &answered_head, "t-origin", None, None).expect("persist");
    let id = seed_obligation(&home, &rs, branch, &answered_head, Some("agent-owner"));
    answer(&home, &id, "agent-owner", "keep: still working on it");
    commit_onto(&repo, branch, "late.txt");

    sweep_settle_merged(&home);

    assert!(
        open_lane_rows(&home, &rs, branch).is_empty(),
        "an answered lane is not #3517's window — the sweep must leave it alone"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #3517 follow-up RED 2 — the history the negative control above did not
/// imagine.
///
/// Negative control 3 answers a lane's FIRST and only row, which is the
/// simplest history rather than the normal one. Once this sweep starts raising
/// rows, a lane accumulates generations: the merge closes row A, the first
/// drift raises row B, the owner answers B. What closed the LAST row is the
/// whole question — a backward search for any result-less row walks past
/// answered B, lands on system-closed A, and re-asks an owner who has already
/// answered.
///
/// With this case pinned the lane is covered for every way its last row can
/// stand: still open (the duplicate guard above returns early), closed by an
/// owner (here), or closed by the system (the RED at the top of this file).
#[test]
fn a_later_owner_answer_outranks_an_older_system_retirement_3517() {
    let home = tmp_dir("drift-generations");
    let repo = tmp_repo("drift-generations-repo");
    let branch = "feat/answered-after-reraise";
    let rs = repo.display().to_string();
    let merged_head = make_branch(&repo, branch);
    persist_intent(&home, &rs, branch, &merged_head, "t-origin", None, None).expect("persist");
    let retired_row = seed_obligation(&home, &rs, branch, &merged_head, Some("agent-owner"));

    // Generation 1: the merge closes A, the first drift makes the sweep raise B.
    owner_attestation::retire_merged_lane(&home, &rs, branch);
    commit_onto(&repo, branch, "late.txt");
    sweep_settle_merged(&home);
    let raised = open_lane_rows(&home, &rs, branch);
    assert_eq!(
        raised.len(),
        1,
        "precondition: the first drift raises exactly one fresh row"
    );
    assert_ne!(
        raised[0].id.0, retired_row,
        "precondition: that row is a new question, not the retired one reopened"
    );

    // The owner answers the row the sweep raised — the newest word on this lane.
    answer(
        &home,
        &raised[0].id.0,
        "agent-owner",
        "keep: still working on it",
    );

    // Generation 2: the branch moves again.
    commit_onto(&repo, branch, "later.txt");
    sweep_settle_merged(&home);

    assert!(
        open_lane_rows(&home, &rs, branch).is_empty(),
        "the owner answered the newest question on this lane, so a further \
         drift must not reach past that answer to an older system-closed row \
         and ask again"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #3517 follow-up — the reachability fact the whole fix rests on, pinned rather
/// than argued.
///
/// `merged_pr_number` matches a merged PR only when the local tip IS the merged
/// head or is an ANCESTOR of it (`branch_sweep::local_sha_matches_merged_head`).
/// A drifted branch is neither — it is one commit AHEAD — so the moment the
/// window this fix exists for opens, the merge stops being observable and the
/// sweep routes to `settle_by_owner_attestation` instead of `settle_intent`.
///
/// Two things follow, and both matter:
///   1. `retire_merged_lane` cannot run a second time on a drifted lane, so it
///      cannot close the obligation this fix raises. (I reported the opposite
///      before measuring it; this test is the correction, kept so nobody has to
///      re-derive it.)
///   2. The other tests here reach `settle_by_owner_attestation` because their
///      fixture repo has no remote — the SAME branch production takes for a
///      drifted lane, for a different reason. The seam does not send the tests
///      down a path production would not take.
#[test]
fn a_drifted_lane_is_no_longer_merge_observable_3517() {
    let repo = tmp_repo("drift-observability");
    let branch = "feat/observability";
    let merged_head = make_branch(&repo, branch);
    git_in(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );

    // At the merged head the merge IS observable — otherwise this test could
    // pass because the mock never fires, which would prove nothing.
    let guard = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::MergedHead {
            base: "main".to_string(),
            head_oid: merged_head.clone(),
        },
    ));
    assert!(
        crate::branch_sweep::merged_pr_number(&repo, "main", branch).is_some(),
        "control: a branch still at its merged head must resolve its merged PR"
    );
    drop(guard);

    // One post-merge commit, and the same lookup goes dark.
    let drifted = commit_onto(&repo, branch, "late.txt");
    assert_ne!(drifted, merged_head);
    let _guard = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::MergedHead {
            base: "main".to_string(),
            head_oid: merged_head.clone(),
        },
    ));
    assert!(
        crate::branch_sweep::merged_pr_number(&repo, "main", branch).is_none(),
        "a drifted branch is AHEAD of the merged head, so it matches neither \
         equality nor the is-ancestor arm — the merge is no longer observable, \
         which is exactly why nothing retires the obligation raised for it"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// #3517 follow-up — the ONE reachable trigger of the head-blind retirement
/// filter, and why leaving that filter alone is right.
///
/// `retire_merged_lane` closes every open row on the lane tag without looking at
/// which head the row is about. That looks over-broad, and the first version of
/// this work proposed narrowing it. It cannot be reached by drift — a drifted tip
/// is AHEAD of the merged head, so the merge stops being observable (see
/// `a_drifted_lane_is_no_longer_merge_observable_3517`) and retirement never runs
/// again. It CAN be reached by rewinding the tip back to the merged head, with a
/// force-push or a reset, after a later-head row was raised.
///
/// And that is precisely the case where closing the row is the RIGHT answer: the
/// post-merge commits no longer exist, so there is nothing left for the owner to
/// attest to. The branch is deleted and the question is retired — asking it would
/// be asking about work that is gone. So the only way to reach the head-blind
/// filter is a case where today's behaviour is correct, which is a stronger
/// reason to leave `retire_merged_lane` untouched than "you cannot reach it".
#[test]
fn rewinding_to_the_merged_head_correctly_retires_the_later_head_row_3517() {
    let home = tmp_dir("drift-rewind");
    let repo = tmp_repo("drift-rewind-repo");
    let branch = "feat/rewound";
    let rs = repo.display().to_string();
    let merged_head = make_branch(&repo, branch);
    git_in(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    persist_intent(&home, &rs, branch, &merged_head, "t-origin", None, None).expect("persist");

    // A row about a LATER head — the shape the narrowing was meant to protect.
    let later_head = commit_onto(&repo, branch, "late.txt");
    let later_row = seed_obligation(&home, &rs, branch, &later_head, Some("agent-owner"));
    assert_eq!(
        open_lane_rows(&home, &rs, branch).len(),
        1,
        "precondition: the later-head row is open"
    );

    // The rewind: the post-merge commits are discarded and the tip is the merged
    // head again, so the merge becomes observable once more.
    git_in(&repo, &["branch", "-f", branch, &merged_head]);
    let _guard = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_pr_list(
        crate::scm::MockPrList::MergedHead {
            base: "main".to_string(),
            head_oid: merged_head.clone(),
        },
    ));

    sweep_settle_merged(&home);

    assert!(
        obligation(&home, &later_row).status.is_terminal(),
        "the later-head row must be retired — the work it was about no longer exists"
    );
    assert!(
        !branch_exists(&repo, branch),
        "and the lane settles for real: tip back at the merged head passes the CAS \
         and the branch is deleted"
    );
    assert!(
        open_lane_rows(&home, &rs, branch).is_empty(),
        "nothing is left open to ask about"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}
