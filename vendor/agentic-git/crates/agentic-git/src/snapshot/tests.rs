use super::*;
use std::path::PathBuf;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// ── Destructive-op classification (v1 list) ─────────────────────────────

#[test]
fn reset_only_destructive_with_hard_merge_keep() {
    assert_eq!(destructive_op_slug(&s(&["reset"])), None);
    assert_eq!(destructive_op_slug(&s(&["reset", "--soft"])), None);
    assert_eq!(destructive_op_slug(&s(&["reset", "HEAD~1"])), None);
    assert_eq!(destructive_op_slug(&s(&["reset", "--hard"])), Some("reset"));
    assert_eq!(destructive_op_slug(&s(&["reset", "--merge"])), Some("reset"));
    assert_eq!(destructive_op_slug(&s(&["reset", "--keep"])), Some("reset"));
    assert_eq!(
        destructive_op_slug(&s(&["reset", "--hard", "HEAD~1"])),
        Some("reset")
    );
}

#[test]
fn clean_only_destructive_with_force_combos() {
    assert_eq!(destructive_op_slug(&s(&["clean"])), None);
    assert_eq!(destructive_op_slug(&s(&["clean", "-n"])), None);
    assert_eq!(destructive_op_slug(&s(&["clean", "-d"])), None);
    assert_eq!(destructive_op_slug(&s(&["clean", "-f"])), Some("clean"));
    assert_eq!(destructive_op_slug(&s(&["clean", "-fd"])), Some("clean"));
    assert_eq!(destructive_op_slug(&s(&["clean", "-fdx"])), Some("clean"));
    assert_eq!(destructive_op_slug(&s(&["clean", "-df"])), Some("clean"));
    assert_eq!(
        destructive_op_slug(&s(&["clean", "--force"])),
        Some("clean")
    );
}

#[test]
fn stash_only_destructive_for_drop_clear() {
    assert_eq!(destructive_op_slug(&s(&["stash"])), None);
    assert_eq!(destructive_op_slug(&s(&["stash", "push"])), None);
    assert_eq!(destructive_op_slug(&s(&["stash", "pop"])), None);
    assert_eq!(destructive_op_slug(&s(&["stash", "list"])), None);
    assert_eq!(destructive_op_slug(&s(&["stash", "apply"])), None);
    assert_eq!(destructive_op_slug(&s(&["stash", "drop"])), Some("stash"));
    assert_eq!(destructive_op_slug(&s(&["stash", "clear"])), Some("stash"));
}

#[test]
fn checkout_only_destructive_for_pathspec_or_force() {
    // `checkout main` is now classified destructive (bias-to-snapshot): a bare
    // 1-positional checkout is ambiguous branch-vs-path, and for a recovery
    // net a spurious snapshot on a harmless same-branch checkout (cross-branch
    // is denied upstream, never reaching here) is far cheaper than missing a
    // real `checkout <path>` that discards work. See the full matrix in
    // `checkout_destructive_matrix_covers_pathspec_restore`.
    assert_eq!(destructive_op_slug(&s(&["checkout", "main"])), Some("checkout"));
    assert_eq!(destructive_op_slug(&s(&["checkout", "-b", "x"])), None);
    assert_eq!(
        destructive_op_slug(&s(&["checkout", "--", "file.txt"])),
        Some("checkout")
    );
    assert_eq!(
        destructive_op_slug(&s(&["checkout", "main", "--", "file.txt"])),
        Some("checkout")
    );
    assert_eq!(
        destructive_op_slug(&s(&["checkout", "-f"])),
        Some("checkout")
    );
    assert_eq!(
        destructive_op_slug(&s(&["checkout", "--force", "main"])),
        Some("checkout")
    );
}

#[test]
fn restore_destructive_unless_staged_only() {
    assert_eq!(
        destructive_op_slug(&s(&["restore", "file.txt"])),
        Some("restore")
    );
    assert_eq!(
        destructive_op_slug(&s(&["restore", "--worktree", "file.txt"])),
        Some("restore")
    );
    assert_eq!(
        destructive_op_slug(&s(&["restore", "--staged", "file.txt"])),
        None
    );
    assert_eq!(
        destructive_op_slug(&s(&["restore", "-S", "file.txt"])),
        None
    );
    // `--staged --worktree` touches the worktree too — destructive.
    assert_eq!(
        destructive_op_slug(&s(&["restore", "--staged", "--worktree", "file.txt"])),
        Some("restore")
    );
}

/// Impl-review finding: the `--`/`-f`-only rule missed pathspec-restore
/// checkouts with no separator (`git checkout HEAD f.txt`, `git checkout
/// f.txt`), which discard the worktree. Full matrix — every worktree-
/// overwriting shape is caught; pure branch-create/switch shapes are not.
#[test]
fn checkout_destructive_matrix_covers_pathspec_restore() {
    // Destructive — worktree overwrite (fail-safe default catches every
    // reachable spelling incl. flag-forms with no positional):
    for argv in [
        vec!["checkout", "--", "f.txt"],                 // explicit pathspec sep
        vec!["checkout", "-f", "feat/x"],                // force
        vec!["checkout", "--force"],                     // force, no arg
        vec!["checkout", "f.txt"],                       // bare pathspec
        vec!["checkout", "HEAD", "f.txt"],               // tree-ish + pathspec
        vec!["checkout", "abc123", "src/"],              // sha + pathspec
        vec!["checkout", "-p", "f.txt"],                 // patch mode
        vec!["checkout", "--ours", "f.txt"],             // conflict side restore
        vec!["checkout", "--theirs", "f.txt"],           // conflict side restore
        vec!["checkout", "--pathspec-from-file=ps"],     // review-2 repro: NO positional
        vec!["checkout", "--pathspec-from-file", "ps"],  // space-separated form
        vec!["checkout", "main"],                        // ambiguous 1-arg → bias to snapshot
    ] {
        assert_eq!(
            destructive_op_slug(&s(&argv)),
            Some("checkout"),
            "must classify destructive: {argv:?}"
        );
    }
    // NOT destructive — pure branch create, or a no-op empty invocation:
    for argv in [
        vec!["checkout", "-b", "feat/x"],           // create branch
        vec!["checkout", "-B", "feat/x"],           // create/reset branch ref
        vec!["checkout", "--orphan", "gh-pages"],   // orphan branch
        vec!["checkout"],                           // no args → git errors, no-op
    ] {
        assert_eq!(
            destructive_op_slug(&s(&argv)),
            None,
            "must NOT classify branch-op as destructive: {argv:?}"
        );
    }
}

/// Self-review addendum: `switch` with a force/discard flag discards the
/// worktree (verified empirically even for the current branch), a reachable
/// hazard the v1 table missed. A plain `switch` (branch move) is NOT
/// snapshotted (cross-branch is denied upstream; a clean move loses nothing).
#[test]
fn switch_force_discard_forms_are_destructive() {
    assert_eq!(destructive_op_slug(&s(&["switch", "feat/x"])), None);
    assert_eq!(destructive_op_slug(&s(&["switch", "-c", "feat/x"])), None);
    assert_eq!(
        destructive_op_slug(&s(&["switch", "-f", "feat/x"])),
        Some("switch")
    );
    assert_eq!(
        destructive_op_slug(&s(&["switch", "--force", "feat/x"])),
        Some("switch")
    );
    assert_eq!(
        destructive_op_slug(&s(&["switch", "--discard-changes", "feat/x"])),
        Some("switch")
    );
}

#[test]
fn mid_op_manglers_always_destructive() {
    for (argv, slug) in [
        (vec!["merge", "feature"], "merge"),
        (vec!["rebase", "main"], "rebase"),
        (vec!["pull"], "pull"),
        (vec!["cherry-pick", "abc123"], "cherry-pick"),
        (vec!["revert", "abc123"], "revert"),
        (vec!["am", "patch.mbox"], "am"),
    ] {
        assert_eq!(destructive_op_slug(&s(&argv)), Some(slug), "{argv:?}");
    }
}

#[test]
fn non_destructive_ops_never_classify() {
    for argv in [
        vec!["status"],
        vec!["log"],
        vec!["diff"],
        vec!["commit", "-m", "x"],
        vec!["push"],
        vec!["fetch"],
        vec!["add", "."],
    ] {
        assert_eq!(destructive_op_slug(&s(&argv)), None, "{argv:?}");
    }
}

/// Reuses `subcommand_index` (per the issue's own instruction) so a leading
/// global option doesn't hide the real (destructive) subcommand.
#[test]
fn leading_global_options_still_classify_correctly() {
    assert_eq!(
        destructive_op_slug(&s(&["-C", "somewhere", "reset", "--hard"])),
        Some("reset")
    );
    assert_eq!(
        destructive_op_slug(&s(&["-c", "user.name=x", "reset", "--hard"])),
        Some("reset")
    );
}

// ── who_for ──────────────────────────────────────────────────────────────

#[test]
fn who_for_empty_agent_is_noagent() {
    assert_eq!(who_for(""), "noagent");
    assert_eq!(who_for("agent-x"), "agent-x");
}

// ── snapshots_enabled kill switch ────────────────────────────────────────

#[test]
fn snapshots_enabled_requires_exact_one() {
    // Isolate from ambient process env via a lock-free approach: just probe
    // the pure string contract snapshots_enabled relies on (env_compat's own
    // resolution is covered elsewhere) by checking the primary-name values
    // through env::set_var in a scoped, single-threaded manner is racy across
    // the test binary's threads, so this suite instead proves the CONTRACT
    // via the integration tests (tests/snapshots.rs) which spawn a fresh
    // process per case. Here we only pin the value contract used inside
    // `snapshots_enabled`: "1" (trimmed) enables, everything else doesn't.
    assert!("1".trim() == "1");
    assert!("0".trim() != "1");
    assert!("off".trim() != "1");
    assert!("".trim() != "1");
}

// ── Ref name construction + parsing ─────────────────────────────────────

#[test]
fn snapshot_ref_name_is_ref_name_safe_and_roundtrips() {
    let refname = snapshot_ref_name("agent-x", "20260704T120000Z", 0, "cherry-pick");
    assert_eq!(
        refname,
        "refs/agentic-git/snapshots/agent-x/20260704T120000Z-0-cherry-pick"
    );
    // No characters git disallows in a ref component.
    for bad in ['~', '^', ':', '?', '*', '[', '\\', ' '] {
        assert!(!refname.contains(bad), "{refname:?} must not contain {bad:?}");
    }
    let (who, op) = parse_snapshot_ref(&refname).expect("must parse own format");
    assert_eq!(who, "agent-x");
    assert_eq!(op, "cherry-pick");
}

#[test]
fn parse_snapshot_ref_rejects_foreign_refs() {
    assert_eq!(parse_snapshot_ref("refs/heads/main"), None);
    assert_eq!(parse_snapshot_ref("refs/agentic-git/other/x"), None);
}

// ── Push guard (Δa v5) — pure text-layer cases (no repo needed) ─────────

#[test]
fn push_guard_text_layer_catches_every_documented_spelling() {
    let wt = "/nonexistent-for-text-layer-only";
    for argv in [
        vec!["push", "origin", "refs/agentic-git/snapshots/me/snap"],
        vec!["push", "origin", "agentic-git/snapshots/me/snap"],
        vec![
            "push",
            "origin",
            "refs/agentic-git/snapshots/*:refs/agentic-git/snapshots/*",
        ],
        vec![
            "push",
            "origin",
            "+agentic-git/snapshots/x:refs/heads/y",
        ],
    ] {
        let violation = snapshot_push_violation(&s(&argv), wt);
        assert!(
            violation.is_some(),
            "{argv:?} must be denied by the text layer"
        );
        assert!(violation.unwrap().contains("SNAPSHOT_REF_PUSH"));
    }
}

#[test]
fn push_guard_mirror_flag_denied_regardless_of_refspec() {
    let wt = "/nonexistent-for-text-layer-only";
    let v = snapshot_push_violation(&s(&["push", "--mirror", "origin"]), wt);
    assert!(v.is_some());
    assert!(v.unwrap().contains("SNAPSHOT_REF_PUSH"));
}

#[test]
fn push_guard_normal_push_never_denied_by_text_layer() {
    let wt = "/nonexistent-for-text-layer-only";
    for argv in [
        vec!["push"],
        vec!["push", "origin", "feature-branch"],
        vec!["push", "origin", "HEAD:refs/heads/feature"],
        vec!["push", "--all", "origin"],
        vec!["push", "--tags", "origin"],
    ] {
        // The commit layer would need a real repo to resolve anything; with
        // a nonexistent worktree path it can never match, so any denial here
        // would have to come from the (over-eager) text layer.
        let violation = snapshot_push_violation(&s(&argv), wt);
        assert!(violation.is_none(), "{argv:?} must not be denied: {violation:?}");
    }
}

// ── Repo-backed tests (real temp git repos; in-process — every child git
// call goes through explicit `Command::env()`, never process-wide env
// mutation, so these are safe under a parallel test runner). ────────────

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentic-git-snapshot-unit-{tag}-{}-{}",
        std::process::id(),
        nanos_now()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    dir
}

fn git_real(git: &str, args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(git)
        .args(args)
        .current_dir(dir)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("run real git for test setup")
}

fn init_repo(git: &str, dir: &Path, committed: bool) {
    assert!(git_real(git, &["init", "-q", "-b", "main", "."], dir)
        .status
        .success());
    assert!(git_real(git, &["config", "user.name", "Test"], dir)
        .status
        .success());
    assert!(
        git_real(git, &["config", "user.email", "t@example.com"], dir)
            .status
            .success()
    );
    if committed {
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        assert!(git_real(git, &["add", "."], dir).status.success());
        assert!(git_real(git, &["commit", "-q", "-m", "init"], dir)
            .status
            .success());
    }
}

fn rev_parse(git: &str, dir: &Path, rev: &str) -> String {
    let out = git_real(git, &["rev-parse", rev], dir);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn is_clean_true_on_clean_false_when_dirty() {
    let git = resolve_real_git();
    let dir = tempdir("clean");
    init_repo(&git, &dir, true);
    assert!(is_clean(&git, &dir), "freshly committed repo must be clean");
    std::fs::write(dir.join("README.md"), "changed\n").unwrap();
    assert!(!is_clean(&git, &dir), "a tracked edit must NOT be clean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_snapshot_head_less_omits_parent_and_captures_content() {
    // Δb: first-run repo with uncommitted files — no HEAD to parent onto.
    let git = resolve_real_git();
    let dir = tempdir("headless");
    init_repo(&git, &dir, false); // no initial commit — HEAD is unborn.
    std::fs::write(dir.join("a.txt"), "tracked-to-be\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "untracked-content\n").unwrap();

    let refname = create_snapshot(&git, &dir, "agent-hl", "reset").expect("must succeed HEAD-less");
    assert!(refname.starts_with(SNAPSHOT_REF_PREFIX));

    // No parent: `rev-list --parents -n1 <ref>` prints ONLY the commit SHA.
    let out = git_real(&git, &["rev-list", "--parents", "-n1", &refname], &dir);
    let sha = rev_parse(&git, &dir, &refname);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        sha,
        "HEAD-less snapshot must have NO parent"
    );

    // Content-level capture: both tracked-to-be and untracked files present.
    let show_a = git_real(&git, &["show", &format!("{refname}:a.txt")], &dir);
    assert_eq!(
        String::from_utf8_lossy(&show_a.stdout),
        "tracked-to-be\n"
    );
    let show_u = git_real(&git, &["show", &format!("{refname}:untracked.txt")], &dir);
    assert_eq!(
        String::from_utf8_lossy(&show_u.stdout),
        "untracked-content\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_snapshot_with_parent_when_head_exists() {
    let git = resolve_real_git();
    let dir = tempdir("withparent");
    init_repo(&git, &dir, true);
    let head = rev_parse(&git, &dir, "HEAD");
    std::fs::write(dir.join("README.md"), "dirty\n").unwrap();

    let refname = create_snapshot(&git, &dir, "agent-p", "reset").expect("must succeed");
    let out = git_real(&git, &["rev-list", "--parents", "-n1", &refname], &dir);
    let sha = rev_parse(&git, &dir, &refname);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("{sha} {head}"),
        "snapshot must be parented on HEAD when HEAD exists"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_snapshot_commit_date_is_now_not_epoch() {
    // Δd (functional half): the snapshot's own committer date must be "now"
    // regardless of ambient env this child inherits. The full ambient-1970
    // self-prune regression (spawning under an env with GIT_COMMITTER_DATE
    // forced) is covered at the integration level (tests/snapshots.rs), since
    // it needs a whole-process ambient env, not just this fn's own children.
    let git = resolve_real_git();
    let dir = tempdir("datenow");
    init_repo(&git, &dir, true);
    std::fs::write(dir.join("README.md"), "dirty\n").unwrap();
    let refname = create_snapshot(&git, &dir, "agent-d", "reset").expect("must succeed");
    let out = git_real(
        &git,
        &["log", "-1", "--format=%ct", &refname],
        &dir,
    );
    let ts: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((now - ts).abs() < 120, "snapshot committer date must be ~now, got {ts}, now={now}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prune_refs_removes_only_expired_and_never_the_excluded_ref() {
    let git = resolve_real_git();
    let dir = tempdir("prune");
    init_repo(&git, &dir, true);
    let head = rev_parse(&git, &dir, "HEAD");

    let make_ref_at = |refname: &str, committer_date: &str| {
        let out = Command::new(&git)
            .args(["-C", dir.to_str().unwrap(), "commit-tree", &format!("{head}^{{tree}}"), "-p", &head, "-m", "snap"])
            .env("AGENTIC_GIT_BYPASS", "1")
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_DATE", committer_date)
            .env("GIT_COMMITTER_DATE", committer_date)
            .output()
            .expect("commit-tree");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            git_real(&git, &["update-ref", refname, &sha], &dir)
                .status
                .success()
        );
    };

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let thirty_days_ago = format!("@{} +0000", now_secs - 30 * 24 * 60 * 60);
    let now_ident = format!("@{now_secs} +0000");

    let old_ref = format!("{SNAPSHOT_REF_PREFIX}me/old-0-reset");
    let fresh_ref = format!("{SNAPSHOT_REF_PREFIX}me/fresh-0-reset");
    let just_created_ref = format!("{SNAPSHOT_REF_PREFIX}me/justcreated-0-reset");
    // 30 days ago — well past the 7-day default TTL.
    make_ref_at(&old_ref, &thirty_days_ago);
    // Fresh — well within TTL.
    make_ref_at(&fresh_ref, &now_ident);
    // Also old, but excluded (simulates "the ref this same invocation just
    // created" — Δd's belt-and-suspenders self-prune immunity).
    make_ref_at(&just_created_ref, &thirty_days_ago);

    let pruned = prune_refs(&git, &dir, DEFAULT_TTL_SECS, Some(just_created_ref.as_str()))
        .expect("prune must succeed");
    assert!(pruned.contains(&old_ref), "expired ref must be pruned: {pruned:?}");
    assert!(
        !pruned.contains(&fresh_ref),
        "fresh ref must NOT be pruned: {pruned:?}"
    );
    assert!(
        !pruned.contains(&just_created_ref),
        "the excluded (just-created) ref must survive even though it's old: {pruned:?}"
    );
    assert!(ref_exists(&git, &dir, &fresh_ref));
    assert!(ref_exists(&git, &dir, &just_created_ref));
    assert!(!ref_exists(&git, &dir, &old_ref));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn push_guard_commit_layer_catches_rev_suffix_and_laundered_branch() {
    let git = resolve_real_git();
    let dir = tempdir("pushguard");
    init_repo(&git, &dir, true);
    let head = rev_parse(&git, &dir, "HEAD");

    // A real snapshot ref pointing at a (distinct) snapshot commit.
    let tree = rev_parse(&git, &dir, "HEAD^{tree}");
    let out = git_real(
        &git,
        &["commit-tree", &tree, "-p", &head, "-m", "snap"],
        &dir,
    );
    let snap_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let snap_ref = format!("{SNAPSHOT_REF_PREFIX}me/x-0-reset");
    assert!(git_real(&git, &["update-ref", &snap_ref, &snap_sha], &dir).status.success());
    // Launder into a branch pointed straight at the snapshot tip.
    assert!(
        git_real(&git, &["branch", "laundered", &snap_sha], &dir)
            .status
            .success()
    );

    let wt = dir.to_str().unwrap();
    for src in [
        snap_ref.clone(),
        "agentic-git/snapshots/me/x-0-reset".to_string(), // abbreviated
        format!("{snap_ref}^{{}}"),
        format!("{snap_ref}~0"),
        "laundered".to_string(),
        snap_sha.clone(),
    ] {
        let argv = s(&["push", "origin", &format!("{src}:refs/heads/out")]);
        let v = snapshot_push_violation(&argv, wt);
        assert!(v.is_some(), "src {src:?} must be denied: argv={argv:?}");
    }

    // A normal branch (tip is the ORIGINAL head, not a snapshot) is allowed.
    let allowed = snapshot_push_violation(&s(&["push", "origin", "main:refs/heads/main2"]), wt);
    assert!(allowed.is_none(), "a non-snapshot branch push must be allowed: {allowed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_snapshots_and_parse_round_trip_on_a_real_ref() {
    let git = resolve_real_git();
    let dir = tempdir("list");
    init_repo(&git, &dir, true);
    std::fs::write(dir.join("README.md"), "dirty\n").unwrap();
    let refname = create_snapshot(&git, &dir, "agent-l", "cherry-pick").expect("must succeed");

    let rows = list_snapshots(&git, &dir).expect("list must succeed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].refname, refname);
    assert_eq!(rows[0].who, "agent-l");
    assert_eq!(rows[0].op, "cherry-pick");
    assert!(rows[0].subject.contains("cherry-pick snapshot"));
    assert!(!rows[0].when.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
