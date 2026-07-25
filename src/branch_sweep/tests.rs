use super::*;
use std::path::PathBuf;

#[path = "tests/branch_sweep_2999_tests.rs"]
mod tests_2999;

// ── #852 PR-C — reviewer_checkout pattern unit tests ──────────────

/// `tmp_pr_review` / `tmp/abc1234` / `tmp-merge` — the operator-
/// created scratch branches that show up after `cd canonical &&
/// git checkout -b tmp_<...>`. Must classify as reviewer_checkout.
#[test]
fn reviewer_checkout_pattern_matches_tmp_prefix() {
    assert!(
        is_reviewer_checkout("tmp_pr_review"),
        "`tmp_pr_review` must match"
    );
    assert!(
        is_reviewer_checkout("tmp/abc1234"),
        "`tmp/abc1234` must match (slash-separated tmp branch)"
    );
    assert!(
        is_reviewer_checkout("tmp-merge"),
        "`tmp-merge` must match (hyphen variant)"
    );
    assert!(is_reviewer_checkout("tmp"), "bare `tmp` must match");
}

/// `pr<N>_head` — the `gh pr fetch` / manual `git fetch origin
/// refs/pull/<N>/head:pr<N>_head` style. Common operator-typed
/// pattern when inspecting a PR locally. Must classify as
/// reviewer_checkout.
#[test]
fn reviewer_checkout_pattern_matches_pr_head_suffix() {
    assert!(
        is_reviewer_checkout("pr123_head"),
        "`pr123_head` must match"
    );
    assert!(
        is_reviewer_checkout("pr850_head"),
        "`pr850_head` must match (real example from operator's report)"
    );
    assert!(
        is_reviewer_checkout("pr1_head"),
        "single-digit pr1_head must match"
    );
}

/// `review/.*` — explicit `review/<n>` namespace. Some workflows
/// adopt this prefix for inspection refs.
#[test]
fn reviewer_checkout_pattern_matches_review_prefix() {
    assert!(
        is_reviewer_checkout("review/123"),
        "`review/123` must match"
    );
    assert!(
        is_reviewer_checkout("review/feat-x"),
        "`review/feat-x` must match"
    );
}

/// **CRITICAL** negative: legitimate working branch names must NOT
/// match. The pattern is narrow by design — only the three
/// observed pollution shapes. A false-positive here would have
/// the boot sweeper auto-deleting legitimate work.
#[test]
fn reviewer_checkout_pattern_does_not_match_main_or_fix_branches() {
    assert!(!is_reviewer_checkout("main"), "main must NOT match");
    assert!(!is_reviewer_checkout("master"), "master must NOT match");
    assert!(
        !is_reviewer_checkout("fix/123-real-work"),
        "fix/.* (legitimate fix branch) must NOT match"
    );
    assert!(
        !is_reviewer_checkout("feat/some-feature"),
        "feat/.* must NOT match"
    );
    assert!(
        !is_reviewer_checkout("temporary-work"),
        "`temporary-work` must NOT match — only `tmp.*` (3-letter \
             prefix) qualifies, not arbitrary 'temp' variants"
    );
    assert!(
        !is_reviewer_checkout("pr-merge-queue"),
        "`pr-merge-queue` must NOT match — pattern requires \
             `pr\\d+_head` shape specifically"
    );
    assert!(
        !is_reviewer_checkout(""),
        "empty string must NOT match (defensive)"
    );
}

/// Spawn a temp git repo scoped to `tag`. The repo has an initial
/// commit on `main` + pinned per-repo gitconfig (`user.name`/
/// `user.email`) so subsequent git subprocess calls don't fail
/// with "unable to auto-detect email address" under CI runners
/// that lack a global ~/.gitconfig. Mirrors #814 r1's CI
/// portability fix.
///
/// Returns the repo dir path.
pub(super) fn setup_repo(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("agend-817-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&base).ok();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).ok();
    git_run(&repo, &["init", "-b", "main"]);
    git_run(&repo, &["config", "user.name", "test"]);
    git_run(&repo, &["config", "user.email", "t@t"]);
    git_run(&repo, &["commit", "--allow-empty", "-m", "main: initial"]);
    repo
}

/// Run git with predictable env. `GIT_AUTHOR_DATE` /
/// `GIT_COMMITTER_DATE` callers use `git_run_dated` instead.
pub(super) fn git_run(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git ran")
}

/// Run git with explicit author + committer date for back-dating
/// commits. Used by stale_idle tests to plant commits N days in
/// the past without `chrono::Utc::now() - duration` arithmetic
/// (flaky near day boundaries).
pub(super) fn git_run_dated(dir: &Path, args: &[&str], date_rfc3339: &str) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", date_rfc3339)
        .env("GIT_COMMITTER_DATE", date_rfc3339)
        .output()
        .expect("git ran")
}

/// Helper: create a branch off main with one commit. Returns the
/// branch tip SHA.
pub(super) fn create_branch_with_commit(repo: &Path, branch: &str, commit_msg: &str) -> String {
    git_run(repo, &["checkout", "-b", branch]);
    let file = repo.join(format!("{branch}.txt"));
    std::fs::write(&file, format!("content for {branch}\n")).expect("write");
    git_run(repo, &["add", &format!("{branch}.txt")]);
    git_run(repo, &["commit", "-m", commit_msg]);
    let sha = String::from_utf8_lossy(&git_run(repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(repo, &["checkout", "main"]);
    sha
}

fn bind_handler_repo(home: &Path, repo: &Path, agent: &str) {
    let binding_dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&binding_dir).expect("mkdir binding");
    std::fs::write(
        binding_dir.join("binding.json"),
        serde_json::json!({
            "source_repo": repo.display().to_string(),
            "branch": "feature",
            "worktree": repo.display().to_string(),
        })
        .to_string(),
    )
    .expect("write binding");
}

fn handler_dry_run(home: &Path, repo: &Path, agent: &str) -> serde_json::Value {
    bind_handler_repo(home, repo, agent);
    crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        home,
        &serde_json::json!({"instance": agent}),
        agent,
    )
}

fn reviewer_candidate<'a>(response: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    response["categories"]["reviewer_checkout"]
        .as_array()
        .expect("reviewer_checkout array")
        .iter()
        .find(|candidate| candidate["name"] == name)
        .unwrap_or_else(|| panic!("missing reviewer candidate {name}: {response}"))
}

fn add_local_bare_origin(repo: &Path) -> PathBuf {
    let origin = repo.parent().expect("repo parent").join("origin.git");
    git_run(
        repo,
        &["init", "--bare", origin.to_str().expect("origin path")],
    );
    git_run(
        repo,
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("origin path"),
        ],
    );
    origin
}

// PR-A RED: preservation evidence is returned through the real
// cleanup_merged_branches dry-run handler. These assertions deliberately
// use JSON fields so the RED commit compiles against the pre-feature
// Candidate type and fails at the actual public response boundary.
#[test]
fn review_preservation_main_reachable_is_observability_only() {
    let repo = setup_repo("preservation_main");
    let home = repo.parent().unwrap().to_path_buf();
    add_local_bare_origin(&repo);
    git_run(&repo, &["branch", "tmp_main_reachable", "main"]);

    let response = handler_dry_run(&home, &repo, "preservation-main-agent");
    let candidate = reviewer_candidate(&response, "tmp_main_reachable");
    assert_eq!(
        candidate["preservation"]["classification"], "MAIN_REACHABLE",
        "main ancestry must be surfaced as current evidence: {response}"
    );
    assert_eq!(candidate["preservation"]["durable"], false);
    assert!(
            response["candidate_ids"]
                .as_array()
                .expect("candidate_ids")
                .iter()
                .any(|id| id == "tmp_main_reachable"),
            "PR-A is observability-only: classification must not remove the existing reviewer candidate"
        );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn review_preservation_external_ancestor_uses_hermetic_pull_ref() {
    let repo = setup_repo("preservation_external");
    let home = repo.parent().unwrap().to_path_buf();
    add_local_bare_origin(&repo);
    let candidate_sha =
        create_branch_with_commit(&repo, "tmp_external", "review work under inspection");
    git_run(
        &repo,
        &["checkout", "-b", "external-descendant", &candidate_sha],
    );
    std::fs::write(repo.join("external-descendant.txt"), "sync commit\n").expect("write");
    git_run(&repo, &["add", "external-descendant.txt"]);
    git_run(&repo, &["commit", "-m", "external descendant"]);
    let descendant_sha = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(
        &repo,
        &[
            "push",
            "origin",
            &format!("{descendant_sha}:refs/pull/7/head"),
        ],
    );
    git_run(&repo, &["checkout", "main"]);

    let response = handler_dry_run(&home, &repo, "preservation-external-agent");
    let candidate = reviewer_candidate(&response, "tmp_external");
    assert_eq!(
            candidate["preservation"]["classification"],
            "EXTERNALLY_REACHABLE_UNGUARANTEED",
            "candidate ancestor of a current pull head must be visible without claiming durability: {response}"
        );
    assert_eq!(candidate["preservation"]["durable"], false);

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn review_preservation_orphaned_reports_exact_unique_count() {
    let repo = setup_repo("preservation_orphan");
    let home = repo.parent().unwrap().to_path_buf();
    add_local_bare_origin(&repo); // successful, empty external inventory
    create_branch_with_commit(&repo, "tmp_orphan", "orphan commit one");
    git_run(&repo, &["checkout", "tmp_orphan"]);
    std::fs::write(repo.join("orphan-two.txt"), "second unique commit\n").expect("write");
    git_run(&repo, &["add", "orphan-two.txt"]);
    git_run(&repo, &["commit", "-m", "orphan commit two"]);
    git_run(&repo, &["checkout", "main"]);

    let response = handler_dry_run(&home, &repo, "preservation-orphan-agent");
    let candidate = reviewer_candidate(&response, "tmp_orphan");
    assert_eq!(
        candidate["preservation"]["classification"], "ORPHANED_UNIQUE",
        "orphan classification requires a successful external inventory: {response}"
    );
    assert_eq!(candidate["preservation"]["unique_commit_count"], 2);

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn review_preservation_external_failure_keeps_inventory_unknown() {
    let repo = setup_repo("preservation_unknown");
    let home = repo.parent().unwrap().to_path_buf();
    create_branch_with_commit(&repo, "tmp_unknown", "review work with offline origin");
    git_run(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "/definitely/missing/agend-origin.git",
        ],
    );

    let response = handler_dry_run(&home, &repo, "preservation-unknown-agent");
    assert_eq!(
        response["dry_run"], true,
        "local inventory must survive: {response}"
    );
    let candidate = reviewer_candidate(&response, "tmp_unknown");
    assert_eq!(
        candidate["preservation"]["classification"], "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
        "offline evidence must never fall through to ORPHANED: {response}"
    );
    assert_ne!(
        candidate["preservation"]["classification"],
        "ORPHANED_UNIQUE"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn review_preservation_unavailable_non_equal_external_object_is_unknown() {
    let repo = setup_repo("preservation_unavailable_object");
    let home = repo.parent().unwrap().to_path_buf();
    let origin = add_local_bare_origin(&repo);
    let candidate_sha =
        create_branch_with_commit(&repo, "tmp_unavailable", "locally available review tip");
    git_run(
        &repo,
        &[
            "push",
            "origin",
            &format!("{candidate_sha}:refs/heads/seed"),
        ],
    );

    // Create the external descendant in a separate clone, then remove the
    // seed ref. `ls-remote` can see the pull-head SHA, while the repository
    // being classified has never fetched that descendant object.
    let peer = repo.parent().unwrap().join("external-peer");
    git_run(
        &repo,
        &[
            "clone",
            origin.to_str().expect("origin path"),
            peer.to_str().expect("peer path"),
        ],
    );
    git_run(&peer, &["config", "user.name", "test"]);
    git_run(&peer, &["config", "user.email", "t@t"]);
    git_run(&peer, &["checkout", "seed"]);
    std::fs::write(peer.join("remote-only.txt"), "unfetched descendant\n").expect("write");
    git_run(&peer, &["add", "remote-only.txt"]);
    git_run(&peer, &["commit", "-m", "remote-only descendant"]);
    git_run(&peer, &["push", "origin", "HEAD:refs/pull/9/head"]);
    git_run(&repo, &["push", "origin", ":refs/heads/seed"]);

    let response = handler_dry_run(&home, &repo, "preservation-unavailable-agent");
    let candidate = reviewer_candidate(&response, "tmp_unavailable");
    assert_eq!(
        candidate["preservation"]["classification"], "UNKNOWN_EXTERNAL_LOOKUP_FAILED",
        "a non-equal remote-only object cannot prove ancestry: {response}"
    );
    assert_ne!(
        candidate["preservation"]["classification"],
        "ORPHANED_UNIQUE"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn spike_residue_is_separate_annotation_not_candidate() {
    let repo = setup_repo("preservation_spike");
    let home = repo.parent().unwrap().to_path_buf();
    add_local_bare_origin(&repo);
    git_run(&repo, &["checkout", "-b", "spike/preservation-probe"]);
    std::fs::write(repo.join("spike-probe.txt"), "analysis artifact\n").expect("write");
    git_run(&repo, &["add", "spike-probe.txt"]);
    git_run(&repo, &["commit", "-m", "spike artifact"]);
    git_run(&repo, &["checkout", "main"]);

    let response = handler_dry_run(&home, &repo, "preservation-spike-agent");
    let annotations = response["annotations"]["spike_residue"]
        .as_array()
        .expect("separate spike_residue annotations");
    assert!(
        annotations.iter().any(|entry| {
            entry["name"] == "spike/preservation-probe" && entry["annotation"] == "SPIKE_RESIDUE"
        }),
        "spike residue must be visible only as an annotation: {response}"
    );
    assert!(
        !response["candidate_ids"]
            .as_array()
            .expect("candidate_ids")
            .iter()
            .any(|id| id == "spike/preservation-probe"),
        "annotation must not add spike residue to candidate_ids"
    );
    assert!(
        !response["categories"]["reviewer_checkout"]
            .as_array()
            .expect("reviewer_checkout")
            .iter()
            .any(|candidate| candidate["name"] == "spike/preservation-probe"),
        "spike residue must remain outside reviewer_checkout"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_scan_categorizes_clean_merged() {
    // #817 RED 1: branch "feat-a" merged into main via a merge
    // commit lands in `clean_merged` (git branch --merged main
    // includes it). Stub returns empty Categories → assertion
    // fails. C2 lands the real scan that picks it up.
    let repo = setup_repo("clean_merged");
    create_branch_with_commit(&repo, "feat-a", "feat: a");
    // Merge via a no-fast-forward merge so a merge commit exists.
    git_run(&repo, &["merge", "--no-ff", "-m", "merge feat-a", "feat-a"]);
    // Branch still exists locally after merge.

    let now = chrono::Utc::now();
    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    assert!(
        cats.clean_merged.iter().any(|c| c.name == "feat-a"),
        "clean_merged must include feat-a, got: {cats:?}"
    );
    // Not in other buckets.
    assert!(!cats.squash_merged.iter().any(|c| c.name == "feat-a"));
    assert!(!cats.stale_idle.iter().any(|c| c.name == "feat-a"));
    assert!(!cats.active_unknown.iter().any(|c| c.name == "feat-a"));

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_scan_categorizes_squash_merged() {
    // #817 RED 2: branch "feat-b" whose commit was squash-applied
    // to main as a NEW commit with same patch-id but DIFFERENT
    // SHA (mirrors GitHub's "Squash and merge" semantics). The
    // detector must use `git cherry main feat-b` (patch-id based)
    // — `git branch --merged` would miss this case because the
    // feat-b SHA isn't reachable from main HEAD.
    //
    // To simulate the SHA-divergence: main advances by an
    // unrelated commit FIRST, then we cherry-pick feat-b with
    // `--no-commit` + commit with a different message. The
    // resulting main HEAD has feat-b's patch but a fresh SHA.
    let repo = setup_repo("squash_merged");
    create_branch_with_commit(&repo, "feat-b", "feat: b body");
    // Make main diverge first so cherry-pick doesn't fast-forward.
    std::fs::write(repo.join("unrelated.txt"), "main moves\n").expect("write");
    git_run(&repo, &["add", "unrelated.txt"]);
    git_run(&repo, &["commit", "-m", "main: unrelated work"]);
    // Squash-apply feat-b's diff to main as a separate commit.
    git_run(&repo, &["cherry-pick", "--no-commit", "feat-b"]);
    git_run(&repo, &["commit", "-m", "squash: feat-b body"]);

    let now = chrono::Utc::now();
    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    assert!(
        cats.squash_merged.iter().any(|c| c.name == "feat-b"),
        "squash_merged must include feat-b, got: {cats:?}"
    );
    // Not in clean_merged — feat-b's SHA is NOT in main's
    // ancestry post-squash (main has a different SHA with same
    // patch-id).
    assert!(!cats.clean_merged.iter().any(|c| c.name == "feat-b"));

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

// #2550 W6: cross-test pinning the behavior GAP between the
// ancestor-check (`git merge-base --is-ancestor`, used by
// worktree_cleanup.rs's is_branch_merged / worktree_pool.rs's
// cleanup_merged_branch via git_helpers::git_ok) and this file's own
// squash-merge detection (is_squash_merged, cherry + tree-diff based) —
// for a GitHub-style squash-merged branch. This is the decision input
// for whether the three merge-detection sites should unify onto
// branch_sweep.rs's heavier squash-aware method.
#[test]
fn ancestor_check_misses_squash_merge_that_branch_sweep_catches() {
    let repo = setup_repo("w6_ancestor_vs_squash");
    create_branch_with_commit(&repo, "feat-c", "feat: c body");
    // Make main diverge first so cherry-pick doesn't fast-forward
    // (mirrors test_branch_sweep_scan_categorizes_squash_merged above).
    std::fs::write(repo.join("unrelated2.txt"), "main moves\n").expect("write");
    git_run(&repo, &["add", "unrelated2.txt"]);
    git_run(&repo, &["commit", "-m", "main: unrelated work 2"]);
    git_run(&repo, &["cherry-pick", "--no-commit", "feat-c"]);
    git_run(&repo, &["commit", "-m", "squash: feat-c body"]);

    assert!(
        !crate::git_helpers::git_ok(&repo, &["merge-base", "--is-ancestor", "feat-c", "main"],),
        "ancestor-check must return false for a squash-merged branch — \
             squash produces a new commit on main with no direct ancestry \
             back to feat-c's tip, so this is a real (not incidental) gap, \
             not a bug to unify away lightly"
    );
    assert!(
        is_squash_merged(&repo, "main", "feat-c"),
        "is_squash_merged (this file's cherry/patch-id detection) must \
             still catch it — this is the gap ancestor-check callers close \
             via a DIFFERENT, cheaper signal instead (remote-tracking-ref-gone,\
             see worktree_cleanup.rs's is_remote_gone / worktree_pool.rs's \
             is_gone), not by adopting this file's cherry+diff(+gh API) method"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

// t-20260704054810920172-67777-3: `local_sha_matches_merged_head`
// regression coverage. main's now-default strict-up-to-date branch
// protection means a required "Update branch" sync commit lands on the
// remote HEAD before squash-merge, but this sweep's `fetch --prune`
// never fast-forwards the local branch ref — so `local_sha` (this
// sweep's only source of truth) stays one sync-commit behind the
// merged PR's real `head_ref_oid` forever once the remote branch is
// deleted. These pin the new is-ancestor acceptance without needing a
// live GitHub API / ScmProvider mock — `local_sha_matches_merged_head`
// takes both SHAs directly.

#[test]
fn local_sha_matches_merged_head_true_for_strict_ancestor_2637() {
    let repo = setup_repo("ancestor_true");
    let local_sha = create_branch_with_commit(&repo, "feat-d", "feat: d body");
    // Simulate the "Update branch" sync commit landing on the remote
    // HEAD after `local_sha` was last touched — one more commit on top,
    // never fetched back into the local branch ref.
    git_run(&repo, &["checkout", "feat-d"]);
    std::fs::write(repo.join("sync.txt"), "update-branch sync\n").expect("write");
    git_run(&repo, &["add", "sync.txt"]);
    git_run(&repo, &["commit", "-m", "sync with main"]);
    let head_ref_oid = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(&repo, &["checkout", "main"]);

    assert!(
        local_sha_matches_merged_head(&repo, &local_sha, &head_ref_oid),
        "local_sha strictly behind the actually-merged head_ref_oid (the \
             update-branch sync gap) must still match"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn local_sha_matches_merged_head_false_for_divergent_unmerged_commit_2637() {
    let repo = setup_repo("ancestor_false");
    let merged_base = create_branch_with_commit(&repo, "feat-e", "feat: e body");
    // `head_ref_oid`: what actually got merged (one commit past the
    // shared base).
    git_run(&repo, &["checkout", "feat-e"]);
    std::fs::write(repo.join("merged.txt"), "this landed in main\n").expect("write");
    git_run(&repo, &["add", "merged.txt"]);
    git_run(&repo, &["commit", "-m", "merged work"]);
    let head_ref_oid = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    // `local_sha`: a DIFFERENT, never-merged commit branched off the
    // same shared base — diverged, not an ancestor of head_ref_oid.
    git_run(&repo, &["checkout", "-b", "feat-e-local", &merged_base]);
    std::fs::write(repo.join("unmerged.txt"), "this never landed\n").expect("write");
    git_run(&repo, &["add", "unmerged.txt"]);
    git_run(&repo, &["commit", "-m", "unmerged local-only work"]);
    let local_sha = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(&repo, &["checkout", "main"]);

    assert!(
        !local_sha_matches_merged_head(&repo, &local_sha, &head_ref_oid),
        "a local_sha carrying real unmerged work outside head_ref_oid's \
             history must NOT match — this is the false-positive guard: \
             is-ancestor must never wrongly clear a branch with unpushed work"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn local_sha_matches_merged_head_true_for_exact_equal_2637() {
    let repo = setup_repo("ancestor_equal");
    let sha = create_branch_with_commit(&repo, "feat-f", "feat: f body");

    assert!(
        local_sha_matches_merged_head(&repo, &sha, &sha),
        "the pre-existing exact-SHA-match behavior must still hold"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn local_sha_matches_merged_head_false_when_head_ref_oid_unknown_locally_2637() {
    let repo = setup_repo("ancestor_missing_object");
    let local_sha = create_branch_with_commit(&repo, "feat-g", "feat: g body");

    assert!(
        !local_sha_matches_merged_head(&repo, &local_sha, "0000000000000000000000000000000000dead"),
        "an is-ancestor check against an object git doesn't have locally \
             (e.g. a deleted remote branch's newest commit, never fetched) \
             must fail CLOSED — not treated as a match"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_scan_categorizes_stale_idle() {
    // #817 RED 3: branch "old-wip" with committer-date 100 days
    // in the past, not merged, not squash-merged → stale_idle.
    // Uses GIT_AUTHOR_DATE/COMMITTER_DATE env to back-date the
    // commit (NOT chrono arithmetic — flaky near day boundary).
    let repo = setup_repo("stale_idle");
    // Back-date by 100 days from a fixed reference point.
    let now = chrono::DateTime::parse_from_rfc3339("2026-05-15T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let old_date = (now - chrono::Duration::days(100)).to_rfc3339();
    git_run(&repo, &["checkout", "-b", "old-wip"]);
    std::fs::write(repo.join("wip.txt"), "wip content\n").expect("write");
    git_run(&repo, &["add", "wip.txt"]);
    git_run_dated(&repo, &["commit", "-m", "WIP: stale work"], &old_date);
    git_run(&repo, &["checkout", "main"]);

    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    assert!(
        cats.stale_idle.iter().any(|c| c.name == "old-wip"),
        "stale_idle must include old-wip (100d > 90d threshold), got: {cats:?}"
    );
    // NOT merged + NOT squash-merged.
    assert!(!cats.clean_merged.iter().any(|c| c.name == "old-wip"));
    assert!(!cats.squash_merged.iter().any(|c| c.name == "old-wip"));

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

// ── #817 apply-path tests ──

/// #2011 regression: a branch checked out in a worktree whose physical
/// directory is GONE (crashed release / manual rm / pre-prune-era leak)
/// must still be deletable by the sweep — git counts it "checked out"
/// until the registration is pruned, and 14 such branches piled up live
/// on 2026-06-11. emit_delete_batch now prunes orphaned registrations in
/// the same transaction (delete dir → registration goes → branch
/// deletable). Pre-#2011 this test fails: `branch -D` refuses with
/// "checked out at".
#[test]
fn test_orphaned_worktree_registration_does_not_block_delete_2011() {
    let repo = setup_repo("orphan_wt_reg");
    let home = repo.parent().unwrap().to_path_buf();
    create_branch_with_commit(&repo, "feat-orphan", "feat: orphan");
    let _merge = git_run(
        &repo,
        &["merge", "--no-ff", "-m", "merge feat-orphan", "feat-orphan"],
    );
    // Check the branch out in a worktree, then vaporize ONLY the
    // physical directory — the registration survives (the leak shape).
    let wt_dir = repo.parent().unwrap().join("orphan-wt-dir");
    std::fs::remove_dir_all(&wt_dir).ok(); // stale residue from a prior run
    let wt_str = wt_dir.display().to_string();
    let out = git_run(&repo, &["worktree", "add", &wt_str, "feat-orphan"]);
    assert!(
        out.status.success(),
        "worktree add must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&wt_dir).expect("rm worktree dir");
    // Precondition pin: the registration is still there (prunable).
    let list = git_run(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("orphan-wt-dir"),
        "leak shape precondition: registration must survive the rm"
    );

    let now = chrono::Utc::now();
    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    let mut confirm = std::collections::HashSet::new();
    confirm.insert("feat-orphan".to_string());
    let applied = emit_delete_batch(&home, &repo, &cats, &confirm, "#2011 test").expect("emit");
    assert_eq!(
        applied, 1,
        "orphaned registration must not block the branch delete"
    );
    let post = enumerate_branches(&repo).expect("enumerate");
    assert!(
        !post.iter().any(|b| b.name == "feat-orphan"),
        "feat-orphan must be deleted after the in-transaction prune"
    );
}

#[test]
fn test_branch_sweep_apply_deletes_confirmed_subset() {
    // GREEN: emit_delete_batch runs `git branch -D <name>` for
    // each confirm_id and writes a `branch_sweep_apply` event-log
    // entry per success. Confirms double-opt-in actually deletes
    // the named branches AND records source SHA for restore.
    let repo = setup_repo("apply_subset");
    let home = repo.parent().unwrap().to_path_buf();
    // Create two clean-merged branches; only delete the first.
    create_branch_with_commit(&repo, "feat-keep", "feat: keep");
    git_run(
        &repo,
        &["merge", "--no-ff", "-m", "merge feat-keep", "feat-keep"],
    );
    create_branch_with_commit(&repo, "feat-delete", "feat: delete");
    git_run(
        &repo,
        &["merge", "--no-ff", "-m", "merge feat-delete", "feat-delete"],
    );

    let now = chrono::Utc::now();
    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    assert_eq!(
        cats.clean_merged.len(),
        2,
        "two branches expected: {cats:?}"
    );

    let mut confirm = std::collections::HashSet::new();
    confirm.insert("feat-delete".to_string());

    let applied =
        emit_delete_batch(&home, &repo, &cats, &confirm, "post-#817 test apply").expect("emit");
    assert_eq!(applied, 1, "exactly 1 deletion expected");

    // feat-delete is gone; feat-keep still exists.
    let post = enumerate_branches(&repo).expect("enumerate");
    let names: Vec<&str> = post.iter().map(|b| b.name.as_str()).collect();
    assert!(!names.contains(&"feat-delete"), "feat-delete must be gone");
    assert!(names.contains(&"feat-keep"), "feat-keep must remain");

    // Event-log entry per success.
    let log_path = home.join("event-log.jsonl");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("branch_sweep_apply"),
        "event-log must record branch_sweep_apply, got: {log}"
    );
    assert!(
        log.contains("feat-delete"),
        "event-log must name the deleted branch"
    );
    assert!(
        log.contains("post-#817 test apply"),
        "event-log must carry the audit_reason"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn branch_recovery_ref_records_source_sha_and_audit_identity() {
    let repo = setup_repo("branch_recovery_metadata");
    let home = repo.parent().unwrap().to_path_buf();
    git_run(&repo, &["checkout", "-b", "review/123"]);
    std::fs::write(repo.join("residue.txt"), "review residue\n").expect("write");
    git_run(&repo, &["add", "residue.txt"]);
    git_run(&repo, &["commit", "-m", "review residue"]);
    let tip = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    git_run(&repo, &["checkout", "main"]);

    let recovery_ref = prepare_branch_recovery(
        Some(&home),
        &repo,
        "review/123",
        &tip,
        "recovery metadata test",
    )
    .expect("recovery ref must be created");
    let resolved = String::from_utf8_lossy(&git_run(&repo, &["rev-parse", &recovery_ref]).stdout)
        .trim()
        .to_string();
    assert_eq!(resolved, tip, "recovery ref must preserve the source SHA");
    assert!(
        recovery_ref.starts_with("refs/agend/recovery/branch/review_123/"),
        "recovery identity must include the sanitized branch: {recovery_ref}"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("branch_cleanup_prepared"),
        "audit event missing: {log}"
    );
    assert!(
        log.contains(&format!("source_sha={tip}")),
        "source SHA missing: {log}"
    );
    assert!(
        log.contains(&format!("recovery_ref={recovery_ref}")),
        "recovery identity missing: {log}"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_apply_skips_unknown_confirm_id() {
    // GREEN: emit_delete_batch tolerates confirm_ids that aren't
    // in any category (e.g. operator typo). Skips silently (the
    // handler-level validator rejects these BEFORE calling this
    // function, so emit_delete_batch's contract is "do best-effort
    // for the candidates it recognizes"). Returns 0 deletions.
    let repo = setup_repo("apply_skip_unknown");
    let home = repo.parent().unwrap().to_path_buf();
    let cats = Categories::default(); // empty
    let mut confirm = std::collections::HashSet::new();
    confirm.insert("nonexistent-branch".to_string());
    let applied = emit_delete_batch(&home, &repo, &cats, &confirm, "unknown probe").expect("emit");
    assert_eq!(
        applied, 0,
        "unknown confirm_ids yield 0 deletions, not errors"
    );
    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// t-20260704115315460591-14440-0 (#817 behavior gap): a `reviewer_checkout`
/// candidate is NOT an unknown confirm_id — it's a recognized category, part
/// of `deletable_ids()`'s own documented default-deletable list, and it
/// survives the handler's `all_ids()` validation before ever reaching
/// `emit_delete_batch`. Unlike `test_branch_sweep_apply_skips_unknown_
/// confirm_id`'s intentional "typo confirm_id → silent no-op" contract,
/// this must behave like `clean_merged`/`squash_merged`: real delete-or-log,
/// never a silent no-op. Production incident (2026-07-04): dry-run listed
/// 24 `review/*` branches as candidates, apply=true confirmed them, but
/// `applied` didn't count them and no event-log entry appeared — the
/// operator had no signal the branches survived.
#[test]
fn test_branch_sweep_apply_deletes_reviewer_checkout_candidate_2620() {
    let repo = setup_repo("apply_reviewer_checkout");
    let home = repo.parent().unwrap().to_path_buf();
    // Unmerged on purpose — reviewer_checkout is classified by NAME
    // pattern alone (scan()'s bucket 0, checked before merge-status),
    // so an unmerged residue branch must still be a REAL candidate.
    create_branch_with_commit(&repo, "tmp_pr_review", "reviewer checkout residue");

    let now = chrono::Utc::now();
    let cats = scan(&repo, "main", STALE_IDLE_DEFAULT_DAYS, now).expect("scan");
    assert_eq!(
        cats.reviewer_checkout.len(),
        1,
        "precondition: review/123 must classify as reviewer_checkout: {cats:?}"
    );
    assert!(
        cats.deletable_ids().contains(&"tmp_pr_review".to_string()),
        "precondition: reviewer_checkout is in the default deletable list"
    );

    let mut confirm = std::collections::HashSet::new();
    confirm.insert("tmp_pr_review".to_string());
    let applied =
        emit_delete_batch(&home, &repo, &cats, &confirm, "reviewer-checkout probe").expect("emit");

    assert_eq!(
        applied, 1,
        "a confirmed reviewer_checkout candidate must actually be deleted, \
             not silently dropped like an unrecognized confirm_id"
    );
    let post = enumerate_branches(&repo).expect("enumerate");
    assert!(
        !post.iter().any(|b| b.name == "tmp_pr_review"),
        "review/123 must actually be gone from the repo"
    );
    let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("branch_sweep_apply") && log.contains("tmp_pr_review"),
        "a real delete must leave the same audit trail as any other category, \
             not silence: {log}"
    );
    assert!(
        log.contains("category=reviewer_checkout"),
        "the audit trail must name the correct category, not fall through to \
             active_unknown: {log}"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_handler_apply_requires_audit_reason_and_confirm_ids() {
    // GREEN: handler validator rejects apply=true with missing
    // confirm_ids OR missing audit_reason. Sets up a minimal
    // binding so the handler can resolve source_repo.
    let repo = setup_repo("handler_validators");
    let home = repo.parent().unwrap().to_path_buf();
    let agent = "test-agent";
    let binding_dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&binding_dir).expect("mkdir");
    std::fs::write(
        binding_dir.join("binding.json"),
        serde_json::json!({
            "source_repo": repo.display().to_string(),
            "branch": "feature",
            "worktree": repo.display().to_string(),
        })
        .to_string(),
    )
    .expect("write binding");

    // apply=true without confirm_ids → reject.
    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({"instance": agent, "apply": true}),
        agent,
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("confirm_ids"))
            .unwrap_or(false),
        "missing confirm_ids must reject: {r}"
    );
    assert_eq!(r["code"], "missing_confirm_ids");

    // apply=true with confirm_ids but no audit_reason → reject.
    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({
            "instance": agent,
            "apply": true,
            "confirm_ids": ["some-branch"],
        }),
        agent,
    );
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("audit_reason"))
            .unwrap_or(false),
        "missing audit_reason must reject: {r}"
    );
    assert_eq!(r["code"], "missing_audit_reason");

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

#[test]
fn test_branch_sweep_handler_active_unknown_remains_fail_closed() {
    // A branch in `active_unknown` (recent, unmerged, not
    // squash-applied) is NOT in `candidate_ids` and remains visible in
    // the dry-run response. Even an explicit confirm cannot delete it:
    // unknown provenance is a lifecycle KEEP decision.
    let repo = setup_repo("active_unknown_opt_in");
    let home = repo.parent().unwrap().to_path_buf();
    let agent = "test-agent";
    let binding_dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&binding_dir).expect("mkdir");
    std::fs::write(
        binding_dir.join("binding.json"),
        serde_json::json!({
            "source_repo": repo.display().to_string(),
            "branch": "feature",
            "worktree": repo.display().to_string(),
        })
        .to_string(),
    )
    .expect("write binding");

    // Create a recent unmerged branch → active_unknown.
    create_branch_with_commit(&repo, "wip-active", "feat: active wip");

    // Dry-run: candidate_ids should be empty for wip-active
    // (only deletable buckets); active_unknown is in categories
    // but not in candidate_ids.
    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({"instance": agent}),
        agent,
    );
    let candidate_ids: Vec<&str> = r["candidate_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
            !candidate_ids.contains(&"wip-active"),
            "wip-active must NOT be in candidate_ids (active_unknown is non-deletable), got: {candidate_ids:?}"
        );
    let active_unknown: Vec<&str> = r["categories"]["active_unknown"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        active_unknown.contains(&"wip-active"),
        "wip-active must appear in active_unknown bucket for visibility, got: {active_unknown:?}"
    );

    // Apply with wip-active in confirm_ids → handler reports no deletion
    // and preserves the branch, despite the name being in all_ids.
    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({
            "instance": agent,
            "apply": true,
            "confirm_ids": ["wip-active"],
            "audit_reason": "verify active_unknown remains preserved",
        }),
        agent,
    );
    assert_eq!(r["applied"], 0, "active_unknown must remain preserved: {r}");
    assert!(
        enumerate_branches(&repo)
            .expect("branches")
            .iter()
            .any(|candidate| candidate.name == "wip-active"),
        "active_unknown branch must remain after explicit confirmation"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// RED #2999: a non-terminal candidate is already a disposition KEEP, so
/// applying it must not gather binding, holder, or task evidence first.
#[test]
fn non_terminal_apply_skips_lifecycle_probes_2999() {
    CLEANUP_TEST_PROBE_MASK.with(|mask| mask.set(0));
    let repo = setup_repo("non-terminal-probes-2999");
    let home = repo.parent().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let branch = "wip-non-terminal";
    let tip_sha = create_branch_with_commit(&repo, branch, "non-terminal work");

    let categories = Categories {
        active_unknown: vec![Candidate {
            name: branch.to_string(),
            tip_sha,
            reason: "active_unknown".to_string(),
        }],
        ..Default::default()
    };
    let confirm_ids = std::iter::once(branch.to_string()).collect();

    let (applied, skipped) = emit_delete_batch_with_context(
        Some(&home),
        &repo,
        "main",
        &categories,
        &confirm_ids,
        "#2999 RED",
    )
    .expect("apply");

    assert_eq!(applied, 0, "non-terminal branch must remain preserved");
    assert_eq!(
        skipped.len(),
        1,
        "non-terminal branch must be reported skipped"
    );
    assert_eq!(
        cleanup_test_probe_mask(),
        0,
        "non-terminal branch must not inspect binding, holder, or task state"
    );
    assert!(
        enumerate_branches(&repo)
            .unwrap()
            .iter()
            .any(|candidate| candidate.name == branch),
        "non-terminal branch must remain present"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// RED: a clean-merged branch with a non-GitHub local origin causes
/// `open_pr_status` → `Unknown` → lifecycle Keep. The apply response
/// currently returns only `applied: 0` with zero structured explanation.
/// Assert that a `skipped` list surfaces the concrete blocker so
/// operators can distinguish "safely preserved because of open-PR
/// uncertainty" from "nothing matched".
#[test]
fn apply_skipped_surfaces_local_origin_unknown_pr_blocker() {
    let repo = setup_repo("skip-local-pr-unknown");
    let home = repo.parent().unwrap().to_path_buf();
    let agent = "skip-pr-agent";

    // Local bare origin → extract_github_repo returns None →
    // OpenPrStatus::Unknown for any terminal branch.
    add_local_bare_origin(&repo);

    // Create a branch and merge it → clean_merged (terminal provenance).
    create_branch_with_commit(&repo, "feat-merged-local", "feat: local work");
    git_run(
        &repo,
        &[
            "merge",
            "--no-ff",
            "-m",
            "merge feat-merged-local",
            "feat-merged-local",
        ],
    );

    bind_handler_repo(&home, &repo, agent);

    // Apply with the merged branch → lifecycle Keep (open_pr = Unknown).
    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({
            "instance": agent,
            "apply": true,
            "confirm_ids": ["feat-merged-local"],
            "audit_reason": "RED: verify skipped reason surfaces",
        }),
        agent,
    );
    assert_eq!(r["applied"], 0, "branch must be preserved: {r}");

    // The response MUST contain a structured skipped list.
    let skipped = r["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("apply response must contain 'skipped' array, got: {r}"));
    assert_eq!(skipped.len(), 1, "exactly one skipped entry expected: {r}");
    assert_eq!(
        skipped[0]["branch"], "feat-merged-local",
        "skipped entry must name the branch: {r}"
    );
    assert_eq!(
        skipped[0]["blocker"], "open_pr_status_unknown",
        "skipped entry must pin the exact blocker: {r}"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// RED: a clean-merged branch with a live binding on another agent
/// causes `active_holder` → `Some(true)` → lifecycle Keep. The apply
/// response must surface the concrete binding blocker, not just
/// `applied: 0`.
#[test]
fn apply_skipped_surfaces_active_binding_blocker() {
    let repo = setup_repo("skip-active-binding");
    let home = repo.parent().unwrap().to_path_buf();
    let caller = "skip-binding-caller";
    let holder = "holder-agent";

    // Create and merge a branch → clean_merged.
    create_branch_with_commit(&repo, "feat-held", "feat: held work");
    git_run(
        &repo,
        &["merge", "--no-ff", "-m", "merge feat-held", "feat-held"],
    );

    // Bind the caller agent so handle_cleanup finds source_repo.
    bind_handler_repo(&home, &repo, caller);

    // Create a SECOND agent's binding on the same branch + repo →
    // branch_has_active_binding returns Some(true).
    let holder_dir = home.join("runtime").join(holder);
    std::fs::create_dir_all(&holder_dir).expect("holder dir");
    std::fs::write(
        holder_dir.join("binding.json"),
        serde_json::json!({
            "source_repo": repo.display().to_string(),
            "branch": "feat-held",
            "worktree": repo.display().to_string(),
        })
        .to_string(),
    )
    .expect("holder binding");

    let r = crate::mcp::handlers::ci::handle_cleanup_merged_branches(
        &home,
        &serde_json::json!({
            "instance": caller,
            "apply": true,
            "confirm_ids": ["feat-held"],
            "audit_reason": "RED: verify binding blocker surfaces",
        }),
        caller,
    );
    assert_eq!(r["applied"], 0, "branch must be preserved: {r}");

    let skipped = r["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("apply response must contain 'skipped' array, got: {r}"));
    assert_eq!(skipped.len(), 1, "exactly one skipped entry expected: {r}");
    assert_eq!(
        skipped[0]["branch"], "feat-held",
        "skipped entry must name the branch: {r}"
    );
    assert_eq!(
        skipped[0]["blocker"], "active_holder",
        "skipped entry must pin the exact blocker: {r}"
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}

/// Bug 3 RED: dry_run_observability must not abort when a reviewer_checkout
/// branch from the categories no longer exists in a fresh branch enumeration.
/// This can happen when a concurrent cleanup or manual deletion removes the
/// branch between scan() and dry_run_observability(). Currently the code
/// calls ok_or_else(...) which returns Err, aborting the entire dry-run.
#[test]
fn dry_run_observability_skips_absent_reviewer_branch() {
    let repo = setup_repo("absent_reviewer");
    add_local_bare_origin(&repo);

    // Build categories with a reviewer_checkout entry for a branch that
    // does NOT exist in the repo (simulating concurrent deletion).
    let categories = Categories {
        reviewer_checkout: vec![Candidate {
            name: "tmp_gone".to_string(),
            tip_sha: "0000000000000000000000000000000000000000".to_string(),
            reason: "reviewer checkout pattern".to_string(),
        }],
        ..Default::default()
    };

    let result = dry_run_observability(&repo, "main", &categories);
    assert!(
        result.is_ok(),
        "dry_run_observability must not abort when a reviewer branch is absent, got: {:?}",
        result.err()
    );

    std::fs::remove_dir_all(repo.parent().unwrap()).ok();
}
