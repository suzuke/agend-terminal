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
