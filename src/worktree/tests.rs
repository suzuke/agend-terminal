use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Original AGEND_HOME captured once before any test can modify it.
/// The git shim uses AGEND_HOME as fallback for AGENTIC_GIT_HOME to locate
/// its own bin/ directory for self-exclusion from PATH (#1504). Tests that
/// override AGEND_HOME must pin AGENTIC_GIT_HOME to this value so the shim
/// can still resolve the real git binary.
#[cfg(unix)]
static DAEMON_HOME: std::sync::LazyLock<Option<String>> =
    std::sync::LazyLock::new(|| std::env::var("AGEND_HOME").ok());

fn tmp_repo(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    // git init
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .ok();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@test",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&dir)
        .output()
        .ok();
    dir
}

/// Sprint 57 Wave 4 (#546 Item 4): test home dir distinct from
/// the test repo dir so the new external worktree layout
/// `<home>/worktrees/<agent>/<branch>/` is verifiable in isolation.
fn tmp_home(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-home-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
fn test_is_git_repo() {
    let repo = tmp_repo("is_git");
    assert!(is_git_repo(&repo));
    assert!(!is_git_repo(&std::env::temp_dir()));
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn test_create_worktree() {
    let home = tmp_home("create");
    let repo = tmp_repo("create");
    let info = create(&home, &repo, "agent1", None);
    assert!(info.is_some());
    let info = info.expect("worktree created");
    assert!(info.path.exists());
    assert_eq!(info.branch, "agend/agent1");
    // arch14: create snapshots the CANONICAL source at entry, so
    // info.source_repo is the canonicalized repo — not the raw fixture path
    // (macOS: /var -> /private/var; Windows: \\?\-prefixed canonical form).
    assert_eq!(
        info.source_repo,
        std::fs::canonicalize(&repo).expect("fixture repo canonicalizes"),
        "info.source_repo must be the entry-snapshotted CANONICAL source"
    );
    // Sprint 57 Wave 4 (#546 Item 4): worktree must live under
    // `<home>/worktrees/<agent>/<branch>/`, NOT `<repo>/.worktrees/`.
    let expected = home.join("worktrees").join("agent1").join("agend/agent1");
    assert_eq!(
        info.path, expected,
        "worktree path must follow new external layout"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Fixture helper: init a bare-ish local git repo with identity + one commit.
fn tmp_repo_with_file(name: &str, rel: &str, body: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-subfix-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    git_run_ok(&dir, &["init", "-b", "main"], /*allow_file*/ false);
    git_run_ok(&dir, &["config", "user.email", "test@test"], false);
    git_run_ok(&dir, &["config", "user.name", "test"], false);
    if let Some(parent) = Path::new(rel).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(dir.join(parent)).unwrap();
        }
    }
    std::fs::write(dir.join(rel), body).unwrap();
    git_run_ok(&dir, &["add", rel], false);
    git_run_ok(&dir, &["commit", "-m", "init"], false);
    dir
}

/// Run git; panic with stderr on non-zero. When `allow_file`, sets
/// `protocol.file.allow=always` so local-path submodule fixtures work.
fn git_run_ok(dir: &Path, args: &[&str], allow_file: bool) {
    let mut cmd = std::process::Command::new("git");
    cmd.env("AGEND_GIT_BYPASS", "1").current_dir(dir);
    if allow_file {
        cmd.args(["-c", "protocol.file.allow=always"]);
    }
    cmd.args(args);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Hermetic superproject with **two** submodule levels:
///   super → `vendor/mid` (A) → `nested` (B, holds `nested_b.txt`)
/// Proves fresh `worktree::create` initializes submodules **recursively**
/// (`--init --recursive`). A single-level fixture would not pin recursion.
///
/// Fixture `submodule add` uses `-c protocol.file.allow=always` (via
/// `git_run_ok(..., true)`). Production `init_submodules_after_create`
/// also passes that `-c` on `git_cmd` — local-path clone helpers ignore
/// repo-stored `protocol.file.allow` alone. No stored config required.
fn tmp_super_with_nested_submodules(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agend-wt-nest-root-{}-{}-{}",
        std::process::id(),
        name,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();

    // Level B (innermost)
    let b = tmp_repo_with_file(&format!("{name}-b"), "nested_b.txt", "level-b-payload\n");

    // Level A: depends on B at nested/
    let a = {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("a-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        git_run_ok(
            &dir,
            &["submodule", "add", &b.display().to_string(), "nested"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "A with nested B"], false);
        dir
    };

    // Super: depends on A at vendor/mid/
    let super_repo = {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("super-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        git_run_ok(
            &dir,
            &["submodule", "add", &a.display().to_string(), "vendor/mid"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "super with A→B nest"], false);
        dir
    };

    // Keep root alive via super_repo living under it; B and A are siblings.
    // Drop path to B/A is fine — git objects live in their dirs which remain.
    let _ = (b, a);
    super_repo
}

/// Fresh daemon worktree provision must materialize nested submodule
/// content (level B file) without a manual `git submodule update`.
#[test]
fn create_initializes_nested_submodules_recursively() {
    let home = tmp_home("submod-rec");
    let super_repo = tmp_super_with_nested_submodules("submod-rec");

    // Sanity: super has .gitmodules and the nested path is recorded.
    assert!(
        super_repo.join(".gitmodules").is_file(),
        "fixture super must have .gitmodules"
    );
    // Nested content must be present in the *source* super (already inited
    // by `submodule add`); the bug is only on the *fresh worktree* side.
    assert!(
        super_repo.join("vendor/mid/nested/nested_b.txt").is_file()
            || super_repo.join("vendor/mid").join(".gitmodules").is_file(),
        "fixture: A must be present under super (init by submodule add)"
    );

    let info = create(&home, &super_repo, "agent-sub", Some("feat/submod-rec"))
        .expect("worktree::create must succeed for hermetic super");

    // Decisive pin: level-B file exists inside the fresh worktree.
    // Without --recursive init after worktree add, vendor/mid is empty.
    let nested_b = info.path.join("vendor/mid/nested/nested_b.txt");
    assert!(
        nested_b.is_file(),
        "fresh worktree must recursively init submodules so level-B file \
             exists at {}; worktree add alone leaves submodule dirs empty",
        nested_b.display()
    );
    // Windows git may rewrite LF→CRLF on checkout (core.autocrlf); pin payload only.
    let body = std::fs::read_to_string(&nested_b).unwrap();
    assert_eq!(
        body.trim_end_matches(['\r', '\n']),
        "level-b-payload",
        "payload text must match regardless of CRLF vs LF"
    );

    std::fs::remove_dir_all(&home).ok();
    // super_repo's parent root holds A/B siblings — best-effort clean.
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Soft-warn contract: when a committed submodule's source is unavailable
/// (optional/private/offline), `create` must still return `Some` with a
/// managed worktree — never hard-fail the lease. Nested content stays empty.
#[test]
fn create_soft_fails_when_submodule_source_unavailable() {
    let home = tmp_home("submod-soft");
    let root = std::env::temp_dir().join(format!(
        "agend-wt-soft-root-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();

    let sub = tmp_repo_with_file("soft-sub", "payload.txt", "should-not-appear\n");
    let super_repo = {
        let dir = root.join("super");
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        git_run_ok(
            &dir,
            &["submodule", "add", &sub.display().to_string(), "vendor/dep"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "super with dep"], false);
        dir
    };
    assert!(super_repo.join(".gitmodules").is_file());

    // Make the recorded submodule URL unusable BEFORE production create.
    std::fs::remove_dir_all(&sub).expect("remove submodule source");

    let info = create(&home, &super_repo, "agent-soft", Some("feat/submod-soft"))
        .expect("create must soft-warn and still return Some when submodule init fails");

    assert!(
        info.path.join(".agend-managed").is_file(),
        "managed marker must still land on soft-fail path"
    );
    assert!(
        !info.path.join("vendor/dep/payload.txt").is_file(),
        "nested content must remain unavailable when source is gone"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&root).ok();
}

// ── #2158-adjacent: dirty-WIP preservation helpers ──────────────────
// (reuses the module's existing `git_out`/`git_run` bypass helpers below)

fn recovery_ref_names(repo: &Path, branch: &str) -> Vec<String> {
    git_out(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/agend/recovery/{branch}/"),
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect()
}

fn pres_kind(p: &WipPreservation) -> &'static str {
    match p {
        WipPreservation::Clean => "Clean",
        WipPreservation::Preserved => "Preserved",
        WipPreservation::Blocked(_) => "Blocked",
        WipPreservation::UnpreservableNestedDirty(_) => "UnpreservableNestedDirty",
    }
}

#[test]
fn preserve_dirty_worktree_captures_untracked_wip() {
    let home = tmp_home("preserve-untracked");
    let repo = tmp_repo("preserve-untracked");
    let info = create(&home, &repo, "agent1", None).expect("worktree created");
    // Untracked WIP — the loss-prone case (`clean -fd` would delete it).
    std::fs::write(info.path.join("scratch-wip.txt"), b"unsaved work").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None);
    assert!(
        matches!(outcome, WipPreservation::Preserved),
        "dirty worktree must be Preserved, got {}",
        pres_kind(&outcome)
    );
    // Verify the ref via git (authoritative — the ref name is not returned).
    let refs = recovery_ref_names(&repo, &info.branch);
    assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
    let tree = git_out(&repo, &["ls-tree", "-r", "--name-only", &refs[0]]);
    assert!(
        tree.contains("scratch-wip.txt"),
        "untracked WIP captured in recovery ref tree: {tree}"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn preserve_dirty_worktree_clean_is_noop() {
    let home = tmp_home("preserve-clean");
    let repo = tmp_repo("preserve-clean");
    let info = create(&home, &repo, "agent1", None).expect("worktree created");
    // No real WIP (a freshly-created worktree carries at most the daemon
    // marker, which is not preservable) → helper must report Clean.
    assert!(
        matches!(
            preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None),
            WipPreservation::Clean
        ),
        "clean worktree must be Clean (no recovery ref)"
    );
    assert!(
        recovery_ref_names(&repo, &info.branch).is_empty(),
        "no recovery ref for a clean release"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// The linked worktree's private index lives at `<gitdir>/index` (gitdir read
/// from the `.git` gitlink file). Planting `<gitdir>/index.lock` makes any
/// index write (`git add -A`) fail — reviewer4's #2672 counterexample for a
/// contended index.
fn plant_index_lock(wt_path: &Path) -> PathBuf {
    let gitlink = std::fs::read_to_string(wt_path.join(".git")).expect("read .git gitlink");
    let gitdir = gitlink
        .strip_prefix("gitdir:")
        .expect("gitlink form")
        .trim();
    let lock = Path::new(gitdir).join("index.lock");
    std::fs::write(&lock, b"").expect("plant index.lock");
    lock
}

#[test]
fn preserve_dirty_worktree_blocks_when_index_locked() {
    // reviewer4 #2672 fail-OPEN counterexample: dirty untracked WIP + a
    // contended index (index.lock) → `git add -A` fails. The old code returned
    // a silently-ignored `None` and the caller removed the worktree, evaporating
    // the WIP. It must now be Blocked (fail-closed) with NO recovery ref.
    let home = tmp_home("preserve-blocked");
    let repo = tmp_repo("preserve-blocked");
    let info = create(&home, &repo, "agent1", None).expect("worktree created");
    std::fs::write(info.path.join("precious-wip.txt"), b"must not vanish").unwrap();
    let lock = plant_index_lock(&info.path);

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None);
    assert!(
        outcome.blocked_reason().is_some(),
        "unpreservable WIP must be Blocked (fail-closed), got {}",
        pres_kind(&outcome)
    );
    assert!(
        recovery_ref_names(&repo, &info.branch).is_empty(),
        "Blocked must not leave a (partial) recovery ref"
    );
    std::fs::remove_file(&lock).ok();
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Bug1 (#t-…61315-2): a known caller is the notice recipient — never the
/// hardcoded `general` the pre-fix `notify_agent(home, "general", …)` used.
#[test]
fn wip_notice_recipient_prefers_the_caller() {
    let home = tmp_home("wip-recipient-caller");
    assert_eq!(
        wip_notice_recipient(&home, "agent1", Some("lead-x")),
        "lead-x",
        "a known caller must be the recipient, not a hardcoded one"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Bug1 last-resort fallback: no caller AND no team → operator inbox
/// (`general`). This is the ONLY path that may legitimately use `general`.
#[test]
fn wip_notice_recipient_no_team_falls_back_to_general() {
    let home = tmp_home("wip-recipient-no-team");
    assert_eq!(
        wip_notice_recipient(&home, "agent1", None),
        "general",
        "no caller and no team → operator inbox as last resort"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── release RCA: unpreservable nested-submodule WIP (data-safety) ─────────
//
// A super-repo worktree whose ONLY dirt lives inside a submodule's working
// tree (gitlink unchanged) MUST NOT be reported `Preserved`: a parent
// recovery ref (tree = super's `add -A`) captures nothing of the nested WIP,
// so removing the worktree silently loses it. The fix classifies this via a
// dual-tree compare and refuses removal (`UnpreservableNestedDirty`), while
// ordinary parent WIP + submodule-pointer moves still preserve+release.

/// Superproject with `commit_marker_gitignore` + one committed submodule at
/// `vendor/dep` holding `sub_file`. `create()` inits it recursively.
// Only used by `#[cfg(unix)]` nested tests; the fixture root is owned by one
// test so its cleanup cannot remove sibling repositories from another test.
#[cfg(unix)]
fn tmp_super_one_sub_file(name: &str, sub_file: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "agend-wt-super1-root-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&root).unwrap();
    let sub_tmp = tmp_repo_with_file(&format!("{name}-sub"), sub_file, "vendored-v1\n");
    let sub = root.join("sub");
    std::fs::rename(&sub_tmp, &sub).expect("move submodule fixture under owned root");
    let dir = root.join("super");
    std::fs::create_dir_all(&dir).unwrap();
    git_run_ok(&dir, &["init", "-b", "main"], false);
    git_run_ok(&dir, &["config", "user.email", "test@test"], false);
    git_run_ok(&dir, &["config", "user.name", "test"], false);
    commit_marker_gitignore(&dir); // marker gitignored, like a real repo
    git_run_ok(
        &dir,
        &["submodule", "add", &sub.display().to_string(), "vendor/dep"],
        true,
    );
    git_run_ok(&dir, &["commit", "-m", "add vendor/dep submodule"], false);
    dir
}

#[cfg(unix)]
fn tmp_super_one_sub(name: &str) -> PathBuf {
    tmp_super_one_sub_file(name, "vendored.txt")
}

/// Run git with piped stdin; panic with stderr on non-zero.
#[cfg(unix)]
fn git_stdin_ok(dir: &Path, args: &[&str], input: &[u8]) {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(input)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Count inbox messages for `recipient` under `home` whose `from` matches
/// `from_marker` (reads the JSONL file directly — fresh test home ⇒ no
/// id-redirect, so the file is `home/inbox/<recipient>.jsonl`).
fn inbox_count_from(home: &Path, recipient: &str, from_marker: &str) -> usize {
    let path = home.join("inbox").join(format!("{recipient}.jsonl"));
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| l.contains(from_marker))
        .count()
}

const NESTED_FROM: &str = "system:release_unpreservable_nested_dirty";

/// (1) Nested-only dirt (tracked file inside the submodule, gitlink unchanged)
/// ⇒ `UnpreservableNestedDirty`, `blocked_reason().is_some()`, NO recovery ref,
/// and the LIVE index/worktree are UNCHANGED by classification.
#[cfg(unix)]
#[test]
fn preserve_refuses_nested_only_dirt() {
    let home = tmp_home("nested-refuse");
    let super_repo = tmp_super_one_sub("nested-refuse");
    let info = create(&home, &super_repo, "agent1", Some("feat/nest")).expect("worktree");
    let vendored = info.path.join("vendor/dep/vendored.txt");
    assert!(
        vendored.is_file(),
        "fixture: submodule file present in worktree"
    );

    std::fs::write(&vendored, b"DIRTY-nested-edit\n").unwrap();
    // Capture the DIRTY status immediately before the call: classification must
    // leave the live index + working tree byte-untouched, so this is identical
    // afterwards (still the nested dirt, nothing staged).
    let status_before = git_out(&info.path, &["status", "--porcelain"]);
    assert!(
        status_before.contains("vendor/dep"),
        "precondition: nested submodule reads dirty: {status_before}"
    );

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/nest", None);
    assert_eq!(
        pres_kind(&outcome),
        "UnpreservableNestedDirty",
        "nested-only dirt must refuse (not falsely Preserved)"
    );
    assert!(
        outcome.blocked_reason().is_some(),
        "refusal must be fail-closed (blocked_reason Some)"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/nest").is_empty(),
        "no recovery ref may be minted for unpreservable nested dirt"
    );
    // Non-destructive: identical porcelain status before/after the call.
    let status_after = git_out(&info.path, &["status", "--porcelain"]);
    assert_eq!(
        status_before, status_after,
        "classification must not mutate the live index or working tree"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// (2) Staged-only WIP (index=v2, working tree reverted to HEAD=v1) ⇒
/// `Preserved`, a ref exists, the staged content is recoverable, and the LIVE
/// index still has v2 staged afterwards (non-destructive).
#[cfg(unix)]
#[test]
fn preserve_keeps_staged_only_wip_recoverable() {
    let home = tmp_home("staged-only");
    let repo = tmp_repo_with_file("staged-only", "f", "v1\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/staged")).expect("worktree");
    let f = info.path.join("f");
    std::fs::write(&f, b"v2\n").unwrap();
    git_run_ok(&info.path, &["add", "f"], false); // stage v2
    std::fs::write(&f, b"v1\n").unwrap(); // revert working tree to HEAD

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/staged", None);
    assert_eq!(
        pres_kind(&outcome),
        "Preserved",
        "staged-only WIP is preservable (parent tree captures it)"
    );
    let refs = recovery_ref_names(&repo, "feat/staged");
    assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
    // Staged v2 must be recoverable: dual-parent (staged ≠ worktree) ⇒ `^2`.
    let staged = git_out(&repo, &["show", &format!("{}^2:f", refs[0])]);
    assert_eq!(
        staged, "v2",
        "staged snapshot recoverable at ref^2: got {staged:?}"
    );
    // CRUCIAL non-destructive: the LIVE index still has v2 staged.
    let live_staged = git_out(&info.path, &["show", ":f"]);
    assert_eq!(
        live_staged, "v2",
        "live index must still hold staged v2 after call"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (3) Staged (v2) DIFFERS from working tree (v3), both ≠ HEAD (v1) ⇒ ONE ref,
/// dual-parent: `ref:f`==v3, `ref^2:f`==v2, and both commits are reachable.
#[cfg(unix)]
#[test]
fn preserve_dual_parent_when_staged_differs_from_worktree() {
    let home = tmp_home("dual-parent");
    let repo = tmp_repo_with_file("dual-parent", "f", "v1\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/dual")).expect("worktree");
    let f = info.path.join("f");
    std::fs::write(&f, b"v2\n").unwrap();
    git_run_ok(&info.path, &["add", "f"], false); // stage v2
    std::fs::write(&f, b"v3\n").unwrap(); // working tree v3

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/dual", None);
    assert_eq!(pres_kind(&outcome), "Preserved");
    let refs = recovery_ref_names(&repo, "feat/dual");
    assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
    assert_eq!(
        git_out(&repo, &["show", &format!("{}:f", refs[0])]),
        "v3",
        "ref tree captures the WORKING tree (v3)"
    );
    assert_eq!(
        git_out(&repo, &["show", &format!("{}^2:f", refs[0])]),
        "v2",
        "ref^2 captures the STAGED index (v2)"
    );
    let staged_commit = git_out(&repo, &["rev-parse", &format!("{}^2", refs[0])]);
    let reachable = git_out(&repo, &["rev-list", &refs[0]]);
    assert!(
        reachable.lines().any(|l| l == staged_commit),
        "staged parent must be reachable from the recovery ref"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (4) Nested submodule INTERNAL dirt co-occurring with parent dirt must REFUSE
/// (fail-closed), NOT falsely Preserve. A parent snapshot (`git add -A`) records
/// the gitlink only, never the submodule's internal edits — so preserving here
/// would mint a recovery ref that silently DROPS the nested WIP on removal. The
/// prior `both-trees == HEAD` classifier caught only the SOLE-nested case; any
/// co-occurring parent dirt made `worktree_tree != HEAD` and skipped the refusal
/// → silent nested loss in the common mixed case (2nd-seat blocker @ fc8481d3).
#[cfg(unix)]
#[test]
fn preserve_mixed_parent_and_nested_internal_refuses_no_silent_loss() {
    let home = tmp_home("mixed");
    let super_repo = tmp_super_one_sub("mixed");
    let info = create(&home, &super_repo, "agent1", Some("feat/mixed")).expect("worktree");
    // Dirty the submodule INTERNAL file AND drop an untracked parent file.
    let vendored = info.path.join("vendor/dep/vendored.txt");
    std::fs::write(&vendored, b"nested-dirty\n").unwrap();
    std::fs::write(info.path.join("parent-wip.txt"), b"parent untracked WIP\n").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/mixed", None);
    assert_eq!(
        pres_kind(&outcome),
        "UnpreservableNestedDirty",
        "mixed parent+nested-internal dirt must refuse (a parent ref cannot capture nested WIP)"
    );
    assert!(
        outcome.blocked_reason().is_some(),
        "refusal must be fail-closed (blocked_reason Some) so the caller retains the worktree"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/mixed").is_empty(),
        "no recovery ref may be minted — it would falsely claim the nested WIP was preserved"
    );
    // The nested WIP must survive in place (classification is non-destructive and
    // the caller must NOT remove the worktree).
    assert_eq!(
        std::fs::read(&vendored).unwrap(),
        b"nested-dirty\n",
        "the nested edit must remain on disk for in-place recovery"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// (5) A submodule POINTER move (new commit inside the submodule ⇒ gitlink
/// changes) is a real, preservable parent change ⇒ `Preserved`, not refused.
#[cfg(unix)]
#[test]
fn preserve_submodule_pointer_move_is_preserved() {
    let home = tmp_home("ptr-move");
    let super_repo = tmp_super_one_sub("ptr-move");
    let info = create(&home, &super_repo, "agent1", Some("feat/ptr")).expect("worktree");
    let sub = info.path.join("vendor/dep");
    // Commit INSIDE the submodule so its HEAD (the gitlink) moves.
    std::fs::write(sub.join("vendored.txt"), b"vendored-v2\n").unwrap();
    git_run_ok(&sub, &["add", "vendored.txt"], false);
    git_run_ok(
        &sub,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "bump inside submodule",
        ],
        false,
    );

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/ptr", None);
    assert_eq!(
        pres_kind(&outcome),
        "Preserved",
        "submodule gitlink move is preservable via the parent tree"
    );
    assert_eq!(
        recovery_ref_names(&super_repo, "feat/ptr").len(),
        1,
        "one recovery ref for the pointer move"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// (5b) P3b (reviewer5 r6 blocker): an UNTRACKED EMBEDDED git repo (in-worktree
/// `git init`/clone, NOT a registered submodule — porcelain `?` row, no `S` field,
/// so the submodule walk never saw it) WITH a commit + internal WIP must REFUSE.
/// A superproject `add -A` records it only as a gitlink; removal destroys the
/// embedded WIP AND its `.git` object store → the recovery ref's gitlink dangles
/// and the "preserved" notice lies. Same no-silent-loss invariant as the headline
/// bug. RED at d47beca2 (returned Preserved + minted a ref).
#[cfg(unix)]
#[test]
fn preserve_refuses_untracked_embedded_git_repo_with_wip() {
    let home = tmp_home("embed");
    let repo = tmp_repo_with_file("embed", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/embed")).expect("worktree");
    // Untracked embedded repo inside the worktree: init + commit + internal WIP.
    let embed = info.path.join("cloned-dep");
    std::fs::create_dir_all(&embed).unwrap();
    git_run_ok(&embed, &["init", "-b", "main"], false);
    std::fs::write(embed.join("committed.txt"), b"committed\n").unwrap();
    git_run_ok(&embed, &["add", "committed.txt"], false);
    git_run_ok(
        &embed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "embed init",
        ],
        false,
    );
    let wip = embed.join("wip.txt");
    std::fs::write(&wip, b"UNCOMMITTED embedded WIP\n").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/embed", None);
    assert_eq!(
        pres_kind(&outcome),
        "UnpreservableNestedDirty",
        "an untracked embedded git repo's WIP cannot be captured by a parent ref — must refuse"
    );
    assert!(
        outcome.blocked_reason().is_some(),
        "refusal must be fail-closed (blocked_reason Some)"
    );
    assert!(
        recovery_ref_names(&repo, "feat/embed").is_empty(),
        "no recovery ref may falsely claim the embedded WIP was preserved"
    );
    assert_eq!(
        std::fs::read(&wip).unwrap(),
        b"UNCOMMITTED embedded WIP\n",
        "the embedded WIP must survive on disk for in-place recovery"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (5c) P3a (pin): a commit-LESS embedded repo already fail-closes — the temp-index
/// `git add -A` errors ("does not have a commit checked out") ⇒ `Blocked`. Keep
/// that data-preserving behavior pinned (must never fall through to Preserved).
#[cfg(unix)]
#[test]
fn preserve_blocks_commitless_embedded_git_repo() {
    let home = tmp_home("embed-empty");
    let repo = tmp_repo_with_file("embed-empty", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/embed0")).expect("worktree");
    let embed = info.path.join("empty-clone");
    std::fs::create_dir_all(&embed).unwrap();
    git_run_ok(&embed, &["init", "-b", "main"], false); // no commit
    std::fs::write(embed.join("wip.txt"), b"wip\n").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/embed0", None);
    assert_eq!(
        pres_kind(&outcome),
        "Blocked",
        "a commit-less embedded repo fails the temp-index add -A => Blocked (fail-closed)"
    );
    assert!(outcome.blocked_reason().is_some());
    assert!(recovery_ref_names(&repo, "feat/embed0").is_empty());

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (5d) P4 (reviewer5 r6 re-verdict): the embedded-repo class ONE LEVEL DEEPER.
/// `git status` collapses an entirely-untracked TREE to a single `?? junk/` row, so
/// an embedded repo at `junk/deep/repo/` never gets its own row and the r6 depth-1
/// `<junk>/.git` check misses it → Preserved + ref minted → the same silent-loss
/// invariant at depth ≥2. The r7 `--untracked-files=all` walk lists the deep embed
/// as its own row (git never descends INTO a foreign repo), so it refuses. RED at
/// 27c4063a.
#[cfg(unix)]
#[test]
fn preserve_refuses_deep_untracked_embedded_git_repo() {
    let home = tmp_home("embed-deep");
    let repo = tmp_repo_with_file("embed-deep", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/embed-deep")).expect("worktree");
    // Embedded repo TWO levels down inside an otherwise-untracked tree.
    let embed = info.path.join("junk/deep/repo");
    std::fs::create_dir_all(&embed).unwrap();
    git_run_ok(&embed, &["init", "-b", "main"], false);
    std::fs::write(embed.join("committed.txt"), b"committed\n").unwrap();
    git_run_ok(&embed, &["add", "committed.txt"], false);
    git_run_ok(
        &embed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "embed init",
        ],
        false,
    );
    let wip = embed.join("wip.txt");
    std::fs::write(&wip, b"DEEP embedded WIP\n").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/embed-deep", None);
    assert_eq!(
        pres_kind(&outcome),
        "UnpreservableNestedDirty",
        "a deep untracked embedded git repo must refuse (depth >=2 still unpreservable)"
    );
    assert!(outcome.blocked_reason().is_some());
    assert!(recovery_ref_names(&repo, "feat/embed-deep").is_empty());
    assert_eq!(
        std::fs::read(&wip).unwrap(),
        b"DEEP embedded WIP\n",
        "the deep embedded WIP must survive on disk"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (5e) BOUNDARY (documented, disposition a2): an embedded repo inside a
/// GITIGNORE'd dir is OUT of the no-silent-loss contract. `git status` (and thus the
/// walk, even with `-uall`) never lists ignored paths, and `git add -A` never
/// captures them — so NO gitlink is recorded and NO recovery ref falsely claims to
/// preserve it. Ignored content is universal accepted-loss on every release path
/// (a plain ignored file is dropped on removal identically); preserving it would be
/// an ignore-semantics change, not this data-safety fix. This test PINS that
/// boundary: ignored embed (sole "dirt") ⇒ Clean (no dirt visible, safe remove),
/// and NO recovery ref is minted for it (no false-preserved claim).
#[cfg(unix)]
#[test]
fn preserve_ignored_dir_embedded_repo_is_out_of_contract_no_false_ref() {
    let home = tmp_home("embed-ignored");
    let repo = tmp_repo_with_file("embed-ignored", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/embed-ign")).expect("worktree");
    // gitignore the dir, then embed a repo inside it with WIP.
    std::fs::write(info.path.join(".gitignore"), b"/junk/\n").unwrap();
    git_run_ok(&info.path, &["add", ".gitignore"], false);
    git_run_ok(
        &info.path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "ignore junk",
        ],
        false,
    );
    let embed = info.path.join("junk/repo");
    std::fs::create_dir_all(&embed).unwrap();
    git_run_ok(&embed, &["init", "-b", "main"], false);
    std::fs::write(embed.join("committed.txt"), b"c\n").unwrap();
    git_run_ok(&embed, &["add", "committed.txt"], false);
    git_run_ok(
        &embed,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "e",
        ],
        false,
    );
    std::fs::write(embed.join("wip.txt"), b"ignored embedded WIP\n").unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/embed-ign", None);
    // Ignored content is invisible to `git status` ⇒ no preservable dirt ⇒ Clean.
    assert_eq!(
        pres_kind(&outcome),
        "Clean",
        "ignored content is invisible to git status ⇒ nothing to preserve (documented boundary)"
    );
    // The load-bearing property: no recovery ref falsely claims the ignored embed.
    assert!(
        recovery_ref_names(&repo, "feat/embed-ign").is_empty(),
        "no recovery ref may be minted for ignored content (no false-preserved claim)"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (5f) B (symlink probe): a symlink to a git repo OUTSIDE the worktree root must
/// fail-closed via the canonicalize+containment guard (escape ⇒ `[skipped:
/// containment]` line ⇒ non-empty walk ⇒ refuse), never a false Preserve. Proves the
/// embedded-repo detection can't be dodged by pointing a symlink out of the tree.
#[cfg(unix)]
#[test]
fn preserve_refuses_symlink_to_out_of_root_embedded_repo() {
    let home = tmp_home("embed-symlink");
    let repo = tmp_repo_with_file("embed-symlink", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/embed-sym")).expect("worktree");
    // A real git repo OUTSIDE the worktree (under the test home, not the worktree).
    let outside = home.join("outside-repo");
    std::fs::create_dir_all(&outside).unwrap();
    git_run_ok(&outside, &["init", "-b", "main"], false);
    std::fs::write(outside.join("wip.txt"), b"outside WIP\n").unwrap();
    // Symlink it into the worktree (untracked).
    std::os::unix::fs::symlink(&outside, info.path.join("linked-repo")).unwrap();

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/embed-sym", None);
    assert_ne!(
        pres_kind(&outcome),
        "Preserved",
        "a symlink to an out-of-root repo must not be falsely Preserved (fail-closed)"
    );
    assert!(
        outcome.blocked_reason().is_some(),
        "must be fail-closed (refuse / retain)"
    );
    assert!(recovery_ref_names(&repo, "feat/embed-sym").is_empty());

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// (6) An UNMERGED live index makes `git write-tree` fail ⇒ `Blocked`
/// (fail-closed), no ref. Planted deterministically via `update-index
/// --index-info` with stage 1/2/3 entries for one path.
#[cfg(unix)]
#[test]
fn preserve_unmerged_index_is_blocked() {
    let home = tmp_home("unmerged");
    let repo = tmp_repo_with_file("unmerged", "f.txt", "base\n");
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/unmerged")).expect("worktree");
    // Object for the conflict path (hash-object on a real file → no stdin).
    std::fs::write(info.path.join("c.txt"), b"conflicted\n").unwrap();
    let blob = git_out(&info.path, &["hash-object", "-w", "c.txt"]);
    let index_info =
        format!("100644 {blob} 1\tc.txt\n100644 {blob} 2\tc.txt\n100644 {blob} 3\tc.txt\n");
    git_stdin_ok(
        &info.path,
        &["update-index", "--index-info"],
        index_info.as_bytes(),
    );
    // Sanity: the live index is genuinely unmerged (write-tree fails).
    assert!(
        crate::git_helpers::git_cmd(&info.path, &["write-tree"]).is_err(),
        "precondition: unmerged index ⇒ write-tree fails"
    );

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/unmerged", None);
    assert_eq!(
        pres_kind(&outcome),
        "Blocked",
        "unmerged index (snapshot failure) is Blocked, not nested-refused"
    );
    assert!(
        recovery_ref_names(&repo, "feat/unmerged").is_empty(),
        "Blocked must not mint a recovery ref"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Marker directory for the per-worktree refusal notice (test-visible mirror
/// of the production path).
fn refusal_marker_dir(home: &Path) -> PathBuf {
    crate::paths::runtime_dir(home).join("release_refusal_notices")
}

/// (7) `release_full` on a nested-only-dirty bound worktree ⇒ refused:
/// NOT released, NOT removed, path intact, no ref, binding retained. Then a
/// CLEAN re-release removes the worktree AND clears the refusal marker.
#[cfg(unix)]
#[test]
fn release_full_refuses_nested_only() {
    let home = tmp_home("release-refuse");
    let super_repo = tmp_super_one_sub("release-refuse");
    let info = create(&home, &super_repo, "agent1", Some("feat/rel")).expect("worktree");
    crate::binding::bind_full(
        &home,
        "agent1",
        "",
        "feat/rel",
        &info.path,
        &super_repo,
        false,
    )
    .expect("bind");
    let vendored = info.path.join("vendor/dep/vendored.txt");
    std::fs::write(&vendored, b"nested-dirty\n").unwrap();

    let out = crate::worktree_pool::release_full(&home, "agent1", false);
    assert!(
        !out.released,
        "release must be refused for unpreservable nested WIP"
    );
    assert!(
        !out.worktree_removed,
        "worktree must NOT be removed on refusal"
    );
    assert!(
        info.path.exists(),
        "worktree dir must remain for in-place recovery"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/rel").is_empty(),
        "no recovery ref for refused nested dirt"
    );
    assert!(
        crate::binding::read(&home, "agent1").is_some(),
        "binding must be retained on refusal"
    );
    // The refusal wrote a per-worktree marker.
    let markers: Vec<_> = std::fs::read_dir(refusal_marker_dir(&home))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) != Some("lock"))
        .collect();
    assert!(!markers.is_empty(), "refusal must persist a de-dup marker");

    // Now make the worktree clean and re-release: it removes + clears marker.
    std::fs::write(&vendored, b"vendored-v1\n").unwrap(); // revert nested dirt
    assert!(
        !has_uncommitted_changes(&info.path),
        "precondition: worktree is clean before re-release"
    );
    let out2 = crate::worktree_pool::release_full(&home, "agent1", false);
    assert!(out2.released, "clean re-release must succeed");
    assert!(out2.worktree_removed, "clean worktree must be removed");
    // P2 (codex R2): clean release removes the MARKER but keeps the `.lock`
    // inode durable (never unlink a lock a concurrent notice may hold). So the
    // only residue is the `.lock`; no non-lock marker survives.
    let residue: Vec<_> = std::fs::read_dir(refusal_marker_dir(&home))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    let markers = residue
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) != Some("lock"))
        .count();
    assert_eq!(
        markers, 0,
        "clean release must clear this worktree's marker"
    );
    assert!(
        residue
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("lock")),
        "the per-worktree `.lock` must remain durable after clear"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// (7b) Real-entry MIXED case through `release_full`: nested-submodule INTERNAL
/// dirt co-occurring with parent dirt must be REFUSED — the worktree is NOT
/// removed and the nested WIP survives on disk. Before the fix, the parent dirt
/// made `worktree_tree != HEAD`, the preserve path snapshotted the gitlink only,
/// and the caller removed the worktree → the nested edit was SILENTLY LOST.
#[cfg(unix)]
#[test]
fn release_full_refuses_mixed_parent_and_nested_internal_preserving_nested() {
    let home = tmp_home("release-mixed");
    let super_repo = tmp_super_one_sub("release-mixed");
    let info = create(&home, &super_repo, "agent1", Some("feat/relmix")).expect("worktree");
    crate::binding::bind_full(
        &home,
        "agent1",
        "",
        "feat/relmix",
        &info.path,
        &super_repo,
        false,
    )
    .expect("bind");
    let vendored = info.path.join("vendor/dep/vendored.txt");
    std::fs::write(&vendored, b"nested-dirty\n").unwrap();
    // Co-occurring PARENT dirt (the trigger that bypassed the sole-nested refusal).
    std::fs::write(info.path.join("parent-wip.txt"), b"parent untracked WIP\n").unwrap();

    let out = crate::worktree_pool::release_full(&home, "agent1", false);
    assert!(
        !out.released,
        "mixed nested-internal dirt must refuse release"
    );
    assert!(
        !out.worktree_removed,
        "worktree must NOT be removed — removal would discard the nested WIP"
    );
    assert!(
        info.path.exists(),
        "worktree dir must remain for in-place recovery"
    );
    assert_eq!(
        std::fs::read(&vendored).unwrap(),
        b"nested-dirty\n",
        "the nested edit must survive on disk (no silent loss)"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/relmix").is_empty(),
        "no recovery ref for the refused mixed case"
    );
    assert!(
        crate::binding::read(&home, "agent1").is_some(),
        "binding must be retained on refusal"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// (8) The refusal notice de-dups per (worktree, nested-status) and re-notifies
/// on a status TRANSITION: A,A ⇒ 1 msg; then B ⇒ 2; then A again ⇒ 3.
#[cfg(unix)]
#[test]
fn nested_notice_dedups_and_transition_renotifies() {
    let home = tmp_home("notice-dedup");
    let wt = tmp_repo("notice-dedup-wt"); // any stable path for the marker key
    let notify = |status: &str| {
        notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, status, None)
    };
    notify("nested-A");
    notify("nested-A");
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        1,
        "identical status must be notified once"
    );
    notify("nested-B");
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        2,
        "a changed nested status must re-notify"
    );
    notify("nested-A");
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        3,
        "A→B→A: returning to A differs from the last-seen B ⇒ re-notify"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// (9) Two threads racing the SAME (worktree, status) notice ⇒ EXACTLY one
/// inbox message (the per-worktree file lock serializes claim+enqueue+marker).
#[cfg(unix)]
#[test]
fn nested_notice_concurrent_claim_single_notify() {
    let home = tmp_home("notice-concurrent");
    let wt = tmp_repo("notice-concurrent-wt");
    std::thread::scope(|s| {
        for _ in 0..2 {
            let home = &home;
            let wt = &wt;
            s.spawn(move || {
                notify_unpreservable_nested_dirty(
                    home,
                    "agent1",
                    "feat/x",
                    wt,
                    "same-status",
                    None,
                );
            });
        }
    });
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        1,
        "concurrent identical notices must collapse to one message"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// (10) `enumerate_nested_dirty` handles a nested tracked file whose name has a
/// SPACE (porcelain-v2 `-z` NUL parsing) — the exact path appears in the output.
#[cfg(unix)]
#[test]
fn nested_dirty_enumeration_includes_paths_with_spaces() {
    let home = tmp_home("enum-spaces");
    let super_repo = tmp_super_one_sub_file("enum-spaces", "my file.txt");
    let info = create(&home, &super_repo, "agent1", Some("feat/space")).expect("worktree");
    let spaced = info.path.join("vendor/dep/my file.txt");
    assert!(spaced.is_file(), "fixture: spaced submodule file present");
    std::fs::write(&spaced, b"dirtied\n").unwrap();

    let listing = enumerate_nested_dirty(&info.path);
    assert!(
        listing.contains("my file.txt"),
        "enumeration must preserve the spaced nested path: {listing:?}"
    );
    assert!(
        listing.contains("vendor/dep"),
        "enumeration must name the dirty submodule: {listing:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
    std::fs::remove_dir_all(&super_repo).ok();
}

/// P0 (codex R2): `submodule.<name>.ignore=all` — set in EITHER `.git/config`
/// OR a committed `.gitmodules` — makes a PLAIN `git status` HIDE submodule
/// working-tree dirt. Without `--ignore-submodules=none` the safety classifier
/// reads the worktree as Clean and would REMOVE it, silently losing the nested
/// WIP. Both sources must be OVERRIDDEN: config-hidden nested dirt must still
/// classify `UnpreservableNestedDirty` and refuse release.
#[cfg(unix)]
#[test]
fn preserve_refuses_config_ignored_submodule_dirt() {
    for (label, use_gitmodules) in [("repo-config", false), ("gitmodules", true)] {
        let home = tmp_home(&format!("cfg-ignore-{label}"));
        let super_repo = tmp_super_one_sub(&format!("cfg-ignore-{label}"));
        // Lease first so submodule initialization cannot honor ignore=all and
        // legitimately leave the fixture unpopulated. Then configure the
        // requested source; `.gitmodules` is committed so the worktree itself
        // stays clean before nested dirt is introduced.
        let info = create(&home, &super_repo, "agent1", Some("feat/cfgig")).expect("worktree");
        if use_gitmodules {
            git_run_ok(
                &info.path,
                &[
                    "config",
                    "-f",
                    ".gitmodules",
                    "submodule.vendor/dep.ignore",
                    "all",
                ],
                false,
            );
            git_run_ok(&info.path, &["add", ".gitmodules"], false);
            git_run_ok(
                &info.path,
                &["commit", "-m", "gitmodules ignore=all"],
                false,
            );
        } else {
            git_run_ok(
                &info.path,
                &["config", "submodule.vendor/dep.ignore", "all"],
                false,
            );
        }
        crate::binding::bind_full(
            &home,
            "agent1",
            "",
            "feat/cfgig",
            &info.path,
            &super_repo,
            false,
        )
        .expect("bind");
        let vendored = info.path.join("vendor/dep/vendored.txt");
        assert!(
            vendored.is_file(),
            "{label}: fixture submodule file present"
        );
        std::fs::write(&vendored, b"DIRTY-nested-edit\n").unwrap();

        // Mechanism: plain status is BLIND to the dirt; forced status reveals it.
        let plain = git_out(&info.path, &["status", "--porcelain"]);
        assert!(
            plain.is_empty(),
            "{label}: precondition — ignore=all hides dirt from PLAIN status: {plain:?}"
        );
        let forced = git_out(
            &info.path,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--ignore-submodules=none",
            ],
        );
        assert!(
            forced.contains("vendor/dep"),
            "{label}: --ignore-submodules=none must reveal the dirt: {forced:?}"
        );

        // Direct classification MUST refuse (not falsely Clean → remove).
        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/cfgig", None);
        assert_eq!(
            pres_kind(&outcome),
            "UnpreservableNestedDirty",
            "{label}: config-hidden submodule dirt must refuse, not read Clean"
        );
        assert!(
            outcome.blocked_reason().is_some(),
            "{label}: refusal must be fail-closed (blocked_reason Some)"
        );
        assert!(
            recovery_ref_names(&super_repo, "feat/cfgig").is_empty(),
            "{label}: no recovery ref may be minted for unpreservable nested dirt"
        );

        // Integration: release_full must refuse and RETAIN the worktree.
        let out = crate::worktree_pool::release_full(&home, "agent1", false);
        assert!(!out.released, "{label}: release must be refused");
        assert!(
            !out.worktree_removed,
            "{label}: worktree must not be removed"
        );
        assert!(
            info.path.exists(),
            "{label}: worktree dir retained for recovery"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }
}

/// P1 (codex R2): a plain `git status` opportunistically REFRESHES the stat
/// cache and REWRITES the live index; `git write-tree` persists the cache-tree.
/// `preserve_dirty_worktree` must leave the LIVE index BYTE-IDENTICAL. Setup
/// pre-persists the cache-tree (a `write-tree` in-fixture) so the ONLY residual
/// mutator is the plain-status stat refresh that `--no-optional-locks` removes.
/// `a` is made STAT-dirty (mtime changed, content identical) to arm that refresh;
/// `b` carries a distinct STAGED change so preserve runs its full classify path.
#[cfg(unix)]
#[test]
fn preserve_leaves_live_index_bytes_identical() {
    let home = tmp_home("index-identity");
    let repo = tmp_repo_with_file("index-identity", "a", "va\n");
    std::fs::write(repo.join("b"), "vb\n").unwrap();
    git_run_ok(&repo, &["add", "b"], false);
    git_run_ok(&repo, &["commit", "-m", "add b"], false);
    commit_marker_gitignore(&repo);
    let info = create(&home, &repo, "agent1", Some("feat/idx")).expect("worktree");

    // Stage a distinct change to `b` (index != HEAD ⇒ full preserve path).
    std::fs::write(info.path.join("b"), "vb2\n").unwrap();
    git_run_ok(&info.path, &["add", "b"], false);
    // Arm the stat-cache refresh: bump `a`'s mtime far into the past, content
    // unchanged. A plain `status` would then refresh + rewrite the index.
    let fa = std::fs::OpenOptions::new()
        .write(true)
        .open(info.path.join("a"))
        .unwrap();
    fa.set_modified(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800),
    )
    .unwrap();
    drop(fa);
    // Pre-persist the cache-tree so `write-tree` inside preserve is a pure no-op;
    // this isolates the status stat-refresh as the sole index mutator under test.
    let _ = git_out(&info.path, &["write-tree"]);

    let git_dir = git_out(&info.path, &["rev-parse", "--absolute-git-dir"]);
    let index_path = Path::new(&git_dir).join("index");
    let before = std::fs::read(&index_path).expect("read index before");

    let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/idx", None);
    assert_eq!(
        pres_kind(&outcome),
        "Preserved",
        "staged change is real, preservable parent WIP"
    );

    let after = std::fs::read(&index_path).expect("read index after");
    assert_eq!(
        before, after,
        "preserve must NOT mutate the live index (byte-identical)"
    );
    // And the staged change genuinely survived (non-destructive read-only path).
    assert_eq!(
        git_out(&info.path, &["show", ":b"]),
        "vb2",
        "staged content must remain in the live index"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// P2 (codex R2): `clear_nested_refusal_marker` must remove ONLY the marker and
/// KEEP the `.lock` inode durable — flock is per-inode, so unlinking the lock a
/// concurrent notice may hold breaks serialization. Assert the marker is gone,
/// the `.lock` remains, and the notice path still serializes (re-notifies once,
/// then de-dups) afterwards — proving the lock path is intact.
#[test]
fn clear_nested_refusal_marker_keeps_lock_and_serializes() {
    let home = tmp_home("clear-keeps-lock");
    let wt = tmp_repo("clear-keeps-lock-wt");
    let dir = refusal_marker_dir(&home);
    let has_lock = || {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("lock"))
    };
    let markers = || {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) != Some("lock"))
            .count()
    };

    // Drive one refusal notice ⇒ writes marker + lock.
    notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
    assert_eq!(markers(), 1, "notify writes exactly one refusal marker");
    assert!(has_lock(), "notify creates the per-worktree lock file");

    clear_nested_refusal_marker(&home, &wt);
    assert_eq!(markers(), 0, "clear must remove the marker");
    assert!(
        has_lock(),
        "clear must KEEP the .lock inode durable (never unlink the held lock)"
    );

    // The lock path still serializes: identical status after clear re-notifies
    // exactly once (marker gone), then de-dups (marker re-created under lock).
    let base = inbox_count_from(&home, "general", NESTED_FROM);
    notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        base + 1,
        "after clear, the same status must notify once more"
    );
    notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
    assert_eq!(
        inbox_count_from(&home, "general", NESTED_FROM),
        base + 1,
        "an immediate identical repeat must de-dup (lock+marker path intact)"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&wt).ok();
}

/// P2 (codex R3): cleanup must NEVER remove the marker without HOLDING the
/// per-worktree lock. While an in-flight notice holds the lock,
/// `clear_nested_refusal_marker` must FAIL SAFE (leave the marker); the marker
/// is only removed by a later cleanup that actually owns the lock.
#[test]
fn clear_nested_refusal_marker_fails_safe_while_lock_held() {
    let home = tmp_home("clear-contended");
    let wt = home.join("wt-contended");
    let dir = refusal_marker_dir(&home);
    std::fs::create_dir_all(&dir).unwrap();
    let key = hash_hex(&wt);
    let marker = dir.join(&key);
    let lock = dir.join(format!("{key}.lock"));
    std::fs::write(&marker, b"h").unwrap();

    // An in-flight notice holds the per-worktree lock.
    let held = crate::store::acquire_file_lock(&lock).expect("hold lock");
    // Cleanup while the lock is held elsewhere MUST leave the marker (fail-safe),
    // and must never block (non-blocking try-lock).
    clear_nested_refusal_marker(&home, &wt);
    assert!(
        marker.exists(),
        "cleanup must NOT remove the marker while the lock is held elsewhere (fail-safe)"
    );
    // The `.lock` inode is untouched either way.
    assert!(lock.exists(), "the .lock inode must remain durable");

    // Once the lock frees, a later lock-owning cleanup removes the marker.
    drop(held);
    clear_nested_refusal_marker(&home, &wt);
    assert!(
        !marker.exists(),
        "a later cleanup that owns the lock removes the marker"
    );
    assert!(lock.exists(), "the .lock inode is never unlinked");

    std::fs::remove_dir_all(&home).ok();
}

/// Bug1 middle fallback: no caller but the agent belongs to a team → route to
/// that team's ORCHESTRATOR, not the hardcoded `general`.
#[test]
fn wip_notice_recipient_no_caller_uses_team_orchestrator() {
    let home = tmp_home("wip-recipient-team");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  gapfix:\n    members: [agent1, lead-y]\n    orchestrator: lead-y\n",
    )
    .expect("write fleet.yaml");
    assert_eq!(
        wip_notice_recipient(&home, "agent1", None),
        "lead-y",
        "no caller but a team → the team orchestrator, not hardcoded general"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Bug2 (#t-…61315-2): the `[system:…]` marker is added exactly once by the
/// notify layer (`NotifySource::System`); the body builder must NOT embed a
/// second copy — else the delivered message double-prefixes.
#[test]
fn wip_preserved_notice_has_no_embedded_marker() {
    let notice = wip_preserved_notice("agent1", "feat/x", "refs/agend-wip/agent1/x-1", false);
    assert!(
        !notice.contains("[system:"),
        "notice body must not embed a [system:…] marker (notify layer adds it): {notice}"
    );
    // Single-parent: no staged-snapshot sentence. Dual-parent: mentions `^2`.
    assert!(
        !notice.contains("^2"),
        "single-parent notice must not mention ^2"
    );
    let dual = wip_preserved_notice("agent1", "feat/x", "refs/agend-wip/agent1/x-1", true);
    assert!(
        dual.contains("refs/agend-wip/agent1/x-1^2"),
        "dual-parent notice must point at the staged snapshot ref^2: {dual}"
    );
}

/// Seed a recovery ref dated `days_ago` (distinct days → distinct names).
fn seed_recovery_ref(repo: &Path, branch: &str, days_ago: i64) -> String {
    let ts = (chrono::Utc::now() - chrono::Duration::days(days_ago))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let name = format!("refs/agend/recovery/{branch}/{ts}");
    git_run(repo, &["update-ref", &name, "HEAD"]);
    name
}

#[test]
fn prune_recovery_refs_enforces_per_branch_cap() {
    let repo = tmp_repo("prune-cap");
    let branch = "feat/prune-cap";
    // 5 recent refs (all within TTL); each day is a distinct date so no ts
    // collision. names[0] = day-1 (newest) … names[4] = day-5 (oldest). Capture
    // the returned names and assert on THEM (never recompute ts from `now()` —
    // seed vs assert straddling a second boundary would spuriously mismatch).
    let names: Vec<String> = [1, 2, 3, 4, 5]
        .iter()
        .map(|&d| seed_recovery_ref(&repo, branch, d))
        .collect();
    assert_eq!(recovery_ref_names(&repo, branch).len(), 5, "seeded 5");
    prune_recovery_refs(&repo, branch);
    let survivors = recovery_ref_names(&repo, branch);
    assert_eq!(survivors.len(), 3, "cap=3 enforced: {survivors:?}");
    for keep in &names[0..3] {
        assert!(survivors.contains(keep), "newest ref must survive: {keep}");
    }
    for gone in &names[3..5] {
        assert!(
            !survivors.contains(gone),
            "over-cap ref must be pruned: {gone}"
        );
    }
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_recovery_refs_ttl_deletes_expired_within_cap() {
    let repo = tmp_repo("prune-ttl");
    let branch = "feat/prune-ttl";
    let recent = seed_recovery_ref(&repo, branch, 1);
    let _expired = seed_recovery_ref(&repo, branch, 15); // > 14d TTL
    prune_recovery_refs(&repo, branch);
    let survivors = recovery_ref_names(&repo, branch);
    assert_eq!(
        survivors,
        vec![recent],
        "expired (>14d) ref pruned even under the per-branch cap: {survivors:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn recovery_ref_expired_parses_timestamp() {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(RECOVERY_TTL_DAYS);
    assert!(
        recovery_ref_expired("refs/agend/recovery/b/20200101T000000Z", cutoff),
        "a year-2020 ref is well past the 14d cutoff"
    );
    let now_ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    assert!(
        !recovery_ref_expired(&format!("refs/agend/recovery/b/{now_ts}"), cutoff),
        "a just-created ref is not expired"
    );
    assert!(
        !recovery_ref_expired("refs/agend/recovery/b/not-a-timestamp", cutoff),
        "unparseable name is fail-safe (NOT expired)"
    );
}

#[test]
fn test_reuse_existing_worktree() {
    let home = tmp_home("reuse");
    let repo = tmp_repo("reuse");
    let info1 = create(&home, &repo, "agent1", None);
    assert!(info1.is_some());
    let info2 = create(&home, &repo, "agent1", None);
    assert!(info2.is_some());
    assert_eq!(info1.expect("i1").path, info2.expect("i2").path);
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn test_non_git_returns_none() {
    let home = tmp_home("nongit");
    let dir = std::env::temp_dir().join(format!("agend-wt-test-nongit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    assert!(create(&home, &dir, "agent1", None).is_none());
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_custom_branch() {
    let home = tmp_home("custom_branch");
    let repo = tmp_repo("custom_branch");
    let info = create(&home, &repo, "agent1", Some("my-feature"));
    assert!(info.is_some());
    assert_eq!(info.expect("i").branch, "my-feature");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn test_list_residual() {
    let home = tmp_home("residual");
    let repo = tmp_repo("residual");
    create(&home, &repo, "agent1", None);
    create(&home, &repo, "agent2", None);
    // Sprint 57 Wave 4 (#546 Item 4): list_residual now scans the
    // CENTRAL `$AGEND_HOME/worktrees/` location (repo-independent).
    let residual = list_residual(&home);
    assert_eq!(residual.len(), 2);
    assert!(residual.contains(&"agent1".to_string()));
    assert!(residual.contains(&"agent2".to_string()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn test_empty_repo_gets_initial_commit() {
    // git init without any commit — should auto-create initial commit
    let home = tmp_home("empty");
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-test-empty-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .ok();
    // No commit — HEAD is invalid
    assert!(!has_commits(&dir));
    // create() should handle this gracefully
    let info = create(&home, &dir, "agent1", None);
    assert!(info.is_some(), "worktree should be created in empty repo");
    assert!(has_commits(&dir), "initial commit should exist now");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&dir).ok();
}

// `test_validate_branch_valid` + `test_validate_branch_rejects` migrated
// to `src/agent_ops.rs::tests` as part of Task #9 Option C epilogue — the
// `validate_branch` fn itself lives in `agent_ops.rs` now, so tests are
// colocated with their subject.

#[test]
#[allow(clippy::unwrap_used)]
fn checkout_branch_creates_new_branch() {
    let dir = std::env::temp_dir().join(format!("agend-wt-checkout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();

    // Checkout a new branch
    assert!(checkout_branch(&dir, "feat/test-branch").is_ok());

    // Verify we're on the new branch
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["branch", "--show-current"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(branch, "feat/test-branch");

    std::fs::remove_dir_all(&dir).ok();
}

// ── P0-1.6: actual HEAD verification on reuse ─────────────────────

/// Smoke test 2 regression: same agent, different branch → must reject.
/// Pre-fix this returned Some with `branch = requested`, falsely echoing
/// the requested branch back even though the worktree HEAD was unchanged.
///
/// Sprint 57 Wave 4 (#546 Item 4): the new external layout puts each
/// (agent, branch) at a distinct path, so a different branch creates a
/// different worktree dir. The "reject on mismatch" semantic still
/// applies WHEN the same path is reused — but with branch in the path,
/// the second `create` lands at a NEW location and the conflict check
/// (which fires only when `wt_dir.exists()`) doesn't trigger. Pin the
/// updated semantic: same-agent-different-branch creates a SECOND
/// worktree at the second branch's path, leaving the first untouched.
#[test]
fn reuse_rejects_when_branch_mismatch() {
    let home = tmp_home("reuse-mismatch");
    let repo = tmp_repo("reuse-mismatch");
    let first = create(&home, &repo, "agent1", Some("feat/A")).expect("first lease");
    assert!(first.path.exists());
    // Second lease, same instance, DIFFERENT branch → lands at a
    // distinct path under the new layout; the first remains intact.
    let second = create(&home, &repo, "agent1", Some("feat/B"));
    assert!(
        second.is_some(),
        "Wave 4: same agent on a different branch lands at a distinct path"
    );
    let second = second.expect("second lease");
    assert_ne!(
        first.path, second.path,
        "different-branch worktrees must occupy different paths"
    );
    assert!(first.path.exists(), "first worktree must remain intact");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Idempotent path: same agent, same custom branch → reuse OK.
/// Confirms the actual-HEAD check does not break the idempotent re-lease
/// semantics that P0-1.5 relies on.
#[test]
fn reuse_idempotent_same_custom_branch() {
    let home = tmp_home("reuse-idem");
    let repo = tmp_repo("reuse-idem");
    let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
    let second = create(&home, &repo, "agent1", Some("feat/X")).expect("second lease idempotent");
    assert_eq!(first.path, second.path);
    assert_eq!(second.branch, "feat/X");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #2010 2b: clean-guarded detached-HEAD reattach on reuse ──────────

/// Commit a `.gitignore` that ignores the `.agend-managed` lease marker.
/// `create()` writes that marker into every worktree (worktree.rs ~255), and
/// every REAL source repo gitignores it (this repo's own .gitignore line 29),
/// so production worktrees read CLEAN. Without it the marker shows as an
/// untracked `??` and a freshly-created worktree would falsely read "dirty" —
/// the fixture must represent production (representative-fixture rule). Adds
/// one commit on top of `tmp_repo`'s init, before any worktree is created.
fn commit_marker_gitignore(repo: &std::path::Path) {
    std::fs::write(repo.join(".gitignore"), ".agend-managed\n").unwrap();
    for args in [
        vec!["add", ".gitignore"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "gitignore marker",
        ],
    ] {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(&args)
            .current_dir(repo)
            .output()
            .expect("git");
    }
}

/// Detach the worktree's HEAD (the `git branch --show-current` ⇒ `Some("")`
/// shape the issue describes — e.g. a reviewer's detached `repo checkout`).
fn detach_head(wt: &std::path::Path) {
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["checkout", "--detach", "HEAD"])
        .current_dir(wt)
        .output()
        .expect("detach HEAD");
}

/// §3.9: a CLEAN detached worktree is reattached to the requested branch and
/// REUSED — pre-#2010 the empty `branch --show-current` mismatched and
/// returned None (LeaseConflict), forcing a manual release before re-dispatch.
#[test]
fn reuse_reattaches_clean_detached_worktree_2010() {
    let home = tmp_home("reattach-clean");
    let repo = tmp_repo("reattach-clean");
    commit_marker_gitignore(&repo); // representative: marker is gitignored in prod
    let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
    // Sanity: it really is on feat/X before we detach.
    detach_head(&first.path);
    let cur = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["branch", "--show-current"])
        .current_dir(&first.path)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&cur.stdout).trim().is_empty(),
        "precondition: HEAD is detached (empty show-current)"
    );

    // Re-lease the same (agent, branch): clean-guarded reattach → reuse.
    let second = create(&home, &repo, "agent1", Some("feat/X"))
        .expect("clean detached worktree must reattach + reuse (#2010 2b)");
    assert_eq!(second.path, first.path, "same worktree reused");
    assert_eq!(second.branch, "feat/X");
    // HEAD is back on the requested branch.
    let after = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["branch", "--show-current"])
        .current_dir(&second.path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&after.stdout).trim(),
        "feat/X",
        "reattach must put HEAD back on the requested branch"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// §3.9: a DIRTY detached worktree still conflicts (returns None) — the
/// clean-guard protects in-flight review WIP, unchanged from pre-#2010.
#[test]
fn reuse_rejects_dirty_detached_worktree_2010() {
    let home = tmp_home("reattach-dirty");
    let repo = tmp_repo("reattach-dirty");
    commit_marker_gitignore(&repo); // representative: marker is gitignored in prod
    let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
    detach_head(&first.path);
    // A REAL uncommitted change (not the gitignored marker) → dirty.
    std::fs::write(first.path.join("wip.txt"), "review notes in progress").unwrap();
    assert!(
        has_uncommitted_changes(&first.path),
        "precondition: worktree is dirty"
    );

    let second = create(&home, &repo, "agent1", Some("feat/X"));
    assert!(
        second.is_none(),
        "dirty detached worktree must still conflict (protect review WIP)"
    );
    // And the WIP is untouched.
    assert!(
        first.path.join("wip.txt").exists(),
        "the dirty WIP file must be left intact"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #2115: force-sync reused worktree to HEAD (review-integrity) ─────────

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_run(dir: &std::path::Path, args: &[&str]) {
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
}

/// #2115 (r6 #2196/#2223 repro): when the branch ref is fast-forwarded
/// (#869 `update-ref`) between leases, the reused worktree's HEAD points at
/// the new SHA but its index + working tree are stale → DIRTY on hand-off.
/// The same-branch reuse path must force-sync to HEAD so the new occupant
/// gets a clean tree at the current SHA (else reviewers run on a polluted
/// tree → false verdicts).
#[test]
fn reuse_syncs_stale_worktree_to_head_after_ref_advance_2115() {
    let home = tmp_home("sync-on-reuse");
    let repo = tmp_repo("sync-on-reuse");
    commit_marker_gitignore(&repo); // representative: marker gitignored in prod

    // First lease lands the worktree on feat/X at c1.
    let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
    let wt = first.path.clone();

    // Advance feat/X to a NEW commit c2 WITHOUT touching the worktree's tree
    // — exactly what ensure_branch_exists (#869) does via `update-ref`. Build
    // c2 on the repo's own checkout, then repoint the branch ref at it.
    std::fs::write(repo.join("feature.txt"), "c2-content\n").unwrap();
    git_run(&repo, &["add", "feature.txt"]);
    git_run(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "c2",
        ],
    );
    let c2 = git_out(&repo, &["rev-parse", "HEAD"]);
    git_run(&repo, &["update-ref", "refs/heads/feat/X", &c2]);

    // The worktree is now stale (HEAD=c2 via the symref, tree=c1) → dirty —
    // and add a stray untracked file to prove `clean -fd` runs too.
    std::fs::write(wt.join("scratch.txt"), "stray").unwrap();
    assert!(
        has_uncommitted_changes(&wt),
        "precondition: reused worktree is dirty after ref advance"
    );
    assert_eq!(
        git_out(&wt, &["branch", "--show-current"]),
        "feat/X",
        "precondition: HEAD symref still on feat/X (update-ref does not detach)"
    );

    // Re-lease the same (agent, branch): same-branch reuse → force-sync.
    let second = create(&home, &repo, "agent1", Some("feat/X")).expect("reuse lease");
    assert_eq!(second.path, wt, "same worktree reused");

    // The tree is now CLEAN at the advanced HEAD (c2).
    assert_eq!(
        git_out(&wt, &["status", "--porcelain"]),
        "",
        "worktree must be clean after sync-on-reuse"
    );
    assert_eq!(
        git_out(&wt, &["rev-parse", "HEAD"]),
        c2,
        "HEAD must be the advanced commit c2"
    );
    let feature = std::fs::read_to_string(wt.join("feature.txt")).expect("feature.txt synced");
    // trim_end: Windows git checkout rewrites the LF to CRLF (`c2-content\r\n`)
    // — assert on content, not the platform line ending.
    assert_eq!(
        feature.trim_end(),
        "c2-content",
        "tracked content synced to HEAD (c2)"
    );
    assert!(
        !wt.join("scratch.txt").exists(),
        "untracked stray file must be removed by clean -fd"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ─────────────────────────────────────────────────────────────
// Sprint 57 Wave 4 (#546 Item 4) — path layout invariants.
// ─────────────────────────────────────────────────────────────

#[test]
fn worktree_path_resolves_to_agend_terminal_external_location() {
    // Pin the canonical layout: `<home>/worktrees/<agent>/<branch>/`.
    let home = std::path::Path::new("/test/home");
    let path = worktree_path(home, "dev", "feat/track-x");
    assert_eq!(
        path,
        std::path::Path::new("/test/home/worktrees/dev/feat/track-x")
    );
}

#[test]
fn worktree_path_handles_simple_branch_without_slash() {
    let home = std::path::Path::new("/test/home");
    let path = worktree_path(home, "dev", "feat-test");
    assert_eq!(
        path,
        std::path::Path::new("/test/home/worktrees/dev/feat-test")
    );
}

#[test]
fn path_layout_invariant_against_regression() {
    // Regression-proof: ensure the new path is NOT under the
    // source repo. This is the load-bearing invariant Wave 4
    // ships — re-introducing `<repo>/.worktrees/<agent>/` as the
    // production path would silently undo the migration.
    let home = std::env::temp_dir().join(format!(
        "agend-wt-invariant-home-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let repo = std::env::temp_dir().join(format!(
        "agend-wt-invariant-repo-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let path = worktree_path(&home, "agent-x", "feat-x");
    assert!(
        path.starts_with(&home),
        "new layout MUST live under home, got: {}",
        path.display()
    );
    assert!(
        !path.starts_with(&repo),
        "new layout MUST NOT live under source_repo, got: {}",
        path.display()
    );
    let path_str = path.display().to_string();
    assert!(
        !path_str.contains(".worktrees"),
        "Wave 4: path must NOT contain `.worktrees` (legacy layout marker), got: {}",
        path_str
    );
}

#[test]
fn list_residual_scans_central_worktrees_dir_not_legacy() {
    // Defensive: list_residual MUST scan `<home>/worktrees/`, not
    // `<repo>/.worktrees/`. Plant entries in BOTH locations and
    // verify only the central one is reported.
    let home = tmp_home("residual-scan");
    let repo = tmp_repo("residual-scan");

    // Central (new layout) — should be reported.
    std::fs::create_dir_all(home.join("worktrees").join("dev").join("feat-a")).unwrap();
    std::fs::create_dir_all(home.join("worktrees").join("lead").join("main-mirror")).unwrap();

    // Legacy (old layout) entry on disk — must NOT be reported by
    // list_residual (which only scans the central new layout).
    std::fs::create_dir_all(repo.join(".worktrees").join("ghost-agent")).unwrap();

    let new_residual = list_residual(&home);
    assert_eq!(
        new_residual.len(),
        2,
        "central scan must surface both new-layout entries, got: {new_residual:?}"
    );
    assert!(new_residual.contains(&"dev".to_string()));
    assert!(new_residual.contains(&"lead".to_string()));
    assert!(
        !new_residual.contains(&"ghost-agent".to_string()),
        "legacy entries must NOT be reported by central scan"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

fn mk_resolved(
    working_directory: PathBuf,
    source_repo: Option<PathBuf>,
    git_branch: Option<String>,
    worktree: Option<bool>,
) -> crate::fleet::ResolvedInstance {
    crate::fleet::ResolvedInstance {
        name: "agent".into(),
        backend: crate::backend::Backend::ClaudeCode,
        backend_command: "claude".into(),
        args: vec![],
        env: std::collections::HashMap::new(),
        working_directory: Some(working_directory),
        ready_pattern: None,
        submit_key: "\r".into(),
        role: None,
        cols: None,
        rows: None,
        topic_id: None,
        git_branch,
        model: None,
        worktree,
        instructions: None,
        source_repo,
        repo: None,
    }
}

/// §3.9 (b)+(c) (#1858): the shared auto-worktree gate must (b) still create
/// a worktree for an EXPLICIT real-repo `working_directory` + `source_repo`
/// (no over-kill of legitimate opt-in), and (c) SKIP the daemon-managed
/// default `workspace/<name>` dir even when it has been git-init'd and
/// `source_repo` is set (the deploy non-branch shape — `deployments.rs`
/// writes exactly `source_repo` + a `workspace/<name>` working_directory).
#[test]
fn resolve_auto_worktree_skips_workspace_default_allows_explicit_repo_1858() {
    // (b) explicit real repo as working_directory → worktree still created.
    let home_b = tmp_repo("1858-b-home");
    let repo = tmp_repo("1858-b-repo");
    let resolved_b = mk_resolved(repo.clone(), Some(repo.clone()), None, None);
    let got_b = resolve_auto_worktree(&home_b, "agent", &resolved_b);
    assert!(
        got_b
            .as_ref()
            .is_some_and(|p| p.to_string_lossy().contains("worktrees")),
        "#1858 (b): explicit real-repo working_directory must still auto-worktree, got {got_b:?}"
    );

    // (c) deploy non-branch shape: source_repo set + working_directory is the
    // default workspace dir (even git-init'd) → NO worktree.
    let home_c = tmp_repo("1858-c-home");
    let work_dir = crate::paths::workspace_dir(&home_c).join("team-dev");
    std::fs::create_dir_all(&work_dir).unwrap();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&work_dir)
        .output()
        .ok();
    assert!(is_git_repo(&work_dir), "fixture: workspace dir git-init'd");
    let resolved_c = mk_resolved(work_dir.clone(), Some(home_c.join("realrepo")), None, None);
    assert!(
        resolve_auto_worktree(&home_c, "team-dev", &resolved_c).is_none(),
        "#1858 (c): deploy non-branch (source_repo + default workspace dir) must not auto-worktree"
    );

    // (d) #1919 team-deploy: the per-instance default NESTED under a team subdir
    // (`<home>/workspace/<team>/<instance>`). The old exact `== workspace/<name>`
    // check missed this (workspace/member1 ≠ workspace/myteam/member1), so the
    // git-init'd default fell through to auto-worktree and broke `claude
    // --continue` session resume on restart. The `starts_with` gate catches the
    // whole `workspace/` subtree. (This case FAILS on the pre-#1919 exact match.)
    let home_d = tmp_repo("1919-d-home");
    let nested = crate::paths::workspace_dir(&home_d)
        .join("myteam")
        .join("member1");
    std::fs::create_dir_all(&nested).unwrap();
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["init", "-b", "main"])
        .current_dir(&nested)
        .output()
        .ok();
    assert!(
        is_git_repo(&nested),
        "fixture: team-nested workspace dir git-init'd"
    );
    let resolved_d = mk_resolved(nested.clone(), Some(home_d.join("realrepo")), None, None);
    assert!(
            resolve_auto_worktree(&home_d, "member1", &resolved_d).is_none(),
            "#1919 (d): team-nested default workspace (workspace/<team>/<instance>) must not auto-worktree"
        );

    for d in [home_b, repo, home_c, home_d] {
        std::fs::remove_dir_all(&d).ok();
    }
}

/// #2234 cure-(B): with the flag OFF (default), a default workspace dir
/// resolves to `None` exactly as pre-(B) — byte-identical, no reconcile.
#[test]
fn resolve_auto_worktree_flag_off_workspace_none_2234() {
    let _flag = crate::worktree_pool::workspace_worktree_test_seam::force(false);
    let home = tmp_repo("2234-off-home");
    let repo = tmp_repo("2234-off-repo");
    let ws = crate::paths::workspace_dir(&home).join("agent");
    let resolved = mk_resolved(ws.clone(), Some(repo.clone()), None, None);
    assert!(
        resolve_auto_worktree(&home, "agent", &resolved).is_none(),
        "flag OFF → workspace stays a non-worktree (byte-identical)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 cure-(B): with the flag ON + a `source_repo`, the gate reconciles
/// the workspace dir into a worktree and returns that SAME path (stable cwd).
#[test]
fn resolve_auto_worktree_flag_on_workspace_reconciles_2234() {
    let home = tmp_repo("2234-on-home");
    let repo = tmp_repo("2234-on-repo");
    let ws = crate::paths::workspace_dir(&home).join("agent");
    let resolved = mk_resolved(ws.clone(), Some(repo.clone()), None, None);

    // Thread-local seam (not process-global set_var) → no cross-test leak.
    let got = {
        let _flag = crate::worktree_pool::workspace_worktree_test_seam::force(true);
        resolve_auto_worktree(&home, "agent", &resolved)
    };

    assert_eq!(
        got.as_deref(),
        Some(ws.as_path()),
        "flag ON → gate returns the workspace path itself (cwd == worktree)"
    );
    assert!(
        ws.join(".git").is_file(),
        "workspace reconciled into a gitlink worktree"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── #2234 Phase 2: remove_worktree binding-driven (destroy-work-safe) ──
fn write_test_binding(home: &Path, agent: &str, branch: &str, worktree: &Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let v = serde_json::json!({
        "version": 1, "agent": agent, "task_id": "T-test",
        "branch": branch, "worktree": worktree.display().to_string(),
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string_pretty(&v).unwrap(),
    )
    .unwrap();
}

/// ① OFF/legacy: the derived `worktrees/<agent>/<branch>` exists → resolve to
/// it (byte-identical with the pre-#2234 behavior).
#[test]
fn resolve_removable_derived_exists_off_byte_identical_2234() {
    let home = tmp_home("rrw-derived");
    let derived = worktree_path(&home, "dev", "fix/x");
    std::fs::create_dir_all(&derived).unwrap();
    assert_eq!(
        resolve_removable_worktree(&home, "dev", "fix/x"),
        Some(derived)
    );
    std::fs::remove_dir_all(&home).ok();
}

/// ② cure-(B): derived path gone, binding bound to the SAME branch → resolve
/// to the binding's `workspace/<agent>` worktree.
#[test]
fn resolve_removable_b_same_branch_uses_binding_2234() {
    let home = tmp_home("rrw-b-same");
    let ws = crate::paths::workspace_dir(&home).join("devb");
    std::fs::create_dir_all(&ws).unwrap();
    write_test_binding(&home, "devb", "feat/y", &ws);
    assert_eq!(
        resolve_removable_worktree(&home, "devb", "feat/y"),
        Some(ws)
    );
    std::fs::remove_dir_all(&home).ok();
}

/// ③ branch-mismatch (the destroy-work guard): derived gone + binding bound
/// to a DIFFERENT branch → None. A stale `remove(branchX)` after the agent
/// rebound to branchY must NOT resolve (and thus must not delete) the live
/// branchY workspace.
#[test]
fn resolve_removable_branch_mismatch_is_noop_no_destroy_2234() {
    let home = tmp_home("rrw-mismatch");
    let ws = crate::paths::workspace_dir(&home).join("devm");
    std::fs::create_dir_all(&ws).unwrap();
    write_test_binding(&home, "devm", "feat/Y", &ws);
    assert_eq!(
            resolve_removable_worktree(&home, "devm", "feat/X"),
            None,
            "#2234: stale remove(branchX) after rebind to branchY must NOT resolve the live branchY workspace"
        );
    assert!(
        ws.exists(),
        "the live workspace must be untouched by resolution"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// ④ derive-miss + no binding → None (already gone).
#[test]
fn resolve_removable_no_binding_is_noop_2234() {
    let home = tmp_home("rrw-none");
    assert_eq!(resolve_removable_worktree(&home, "devn", "feat/z"), None);
    std::fs::remove_dir_all(&home).ok();
}

/// End-to-end destroy-work prevention: a REAL `workspace/<agent>` worktree on
/// branchY + a binding to branchY; a stale `remove_worktree(agent, branchX)`
/// must be a graceful no-op and leave the live workspace intact (the critical
/// #2234-cluster guard — `git worktree remove --force` is destructive).
#[test]
fn remove_worktree_stale_branch_does_not_destroy_live_workspace_2234() {
    let home = tmp_home("rrw-e2e");
    let repo = tmp_repo("rrw-e2e-repo");
    let ws = crate::paths::workspace_dir(&home).join("deve");
    std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "feat/Y", &ws.display().to_string()])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    write_test_binding(&home, "deve", "feat/Y", &ws);

    let r = remove_worktree(&home, &repo, "deve", "feat/X");

    assert!(
        r.is_ok(),
        "stale-branch remove must be a graceful no-op: {r:?}"
    );
    assert!(
        ws.exists(),
        "#2234: the live branchY workspace must NOT be destroyed by a stale remove(branchX)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2234 Phase 2: list_residual also surfaces cure-(B) `workspace/<agent>`
/// gitlink worktrees (the worktrees_root first-level scan is unchanged →
/// byte-identical OFF; this adds the workspace coverage when (B) is on).
#[test]
fn list_residual_includes_workspace_gitlink_2234() {
    let home = tmp_home("lr-ws");
    let repo = tmp_repo("lr-ws-repo");
    let ws = crate::paths::workspace_dir(&home).join("devw");
    std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "feat/y", &ws.display().to_string()])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        list_residual(&home).contains(&"devw".to_string()),
        "#2234: cure-(B) workspace gitlink agent must appear in list_residual"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

// ── Phase 1 RED: nested-submodule discard release seam (#arch14-nested-dirt) ──
//
// These tests define the contract for `discard_nested_dirt` on the real
// release handler. They deliberately call the MCP entry point with a real
// managed-marker worktree and legacy absent-binding state; there is no
// helper-level production shim.

/// Resolve the source repository recorded by a linked worktree's git common
/// directory so the fixture can provide the same source identity that release
/// uses in production.
#[cfg(unix)]
fn source_repo_for_worktree(wt_path: &Path) -> PathBuf {
    let common = PathBuf::from(git_out(wt_path, &["rev-parse", "--git-common-dir"]));
    let common = if common.is_absolute() {
        common
    } else {
        wt_path.join(common)
    };
    common
        .canonicalize()
        .expect("linked worktree common git dir")
        .parent()
        .expect("git common dir parent")
        .to_path_buf()
}

#[cfg(unix)]
fn release_tmp_home(name: &str) -> PathBuf {
    let root = PathBuf::from(std::env::var("HOME").expect("HOME for release fixture"));
    let dir = root.join(format!(
        ".agend-nested-release-{}-{name}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("release fixture home");
    dir
}

/// Invoke the actual path-addressed `repo action=release` route. The fixture
/// uses the legacy managed-marker shape with an absent binding so this reaches
/// the production absent-binding release transaction rather than a helper.
#[cfg(unix)]
fn release_entry(
    home: &Path,
    agent: &str,
    wt_path: &Path,
    branch: &str,
    discard_nested_dirt: bool,
    force: bool,
    expected_digest: Option<&str>,
) -> serde_json::Value {
    release_entry_with_reason(
        home,
        agent,
        wt_path,
        branch,
        discard_nested_dirt,
        force,
        expected_digest,
        "nested discard release confirmation",
    )
}

#[allow(clippy::too_many_arguments)]
fn release_entry_with_reason(
    home: &Path,
    agent: &str,
    wt_path: &Path,
    branch: &str,
    discard_nested_dirt: bool,
    force: bool,
    expected_digest: Option<&str>,
    audit_reason: &str,
) -> serde_json::Value {
    std::fs::write(
        wt_path.join(crate::worktree_pool::MANAGED_MARKER),
        format!("agent={agent}\nbranch={branch}\n"),
    )
    .expect("legacy managed marker");
    crate::binding::unbind(home, agent);
    let source_repo = source_repo_for_worktree(wt_path);
    let mut args = serde_json::json!({
        "action": "release",
        "path": wt_path,
        "repository_path": source_repo,
        "discard_nested_dirt": discard_nested_dirt,
        "force": force,
        "audit_reason": audit_reason,
    });
    if let Some(digest) = expected_digest {
        args["expected_nested_dirt_digest"] = serde_json::json!(digest);
    }
    let _guard = crate::mcp::handlers::fleet_test_guard();
    if let Some(ref h) = *DAEMON_HOME {
        std::env::set_var("AGENTIC_GIT_HOME", h);
    }
    std::env::set_var("AGEND_HOME", home);
    let result = crate::mcp::handlers::handle_tool("repo", &args, "");
    match &*DAEMON_HOME {
        Some(h) => std::env::set_var("AGEND_HOME", h),
        None => std::env::remove_var("AGEND_HOME"),
    }
    std::env::remove_var("AGENTIC_GIT_HOME");
    result
}

/// Compute the nested-dirt digest a caller must supply to authorize discard.
#[cfg(unix)]
fn nested_dirt_digest(wt_path: &Path) -> String {
    nested_dirt_digest_sha256(&enumerate_nested_dirty(wt_path))
}

/// (a) Without discard authorization, nested-only dirt still refuses through
/// the real handler and keeps the managed target/binding intact.
#[cfg(unix)]
#[test]
fn discard_seam_no_authorization_still_refuses() {
    let home = release_tmp_home("discard-no-auth");
    let super_repo = tmp_super_one_sub("discard-no-auth");
    let info = create(&home, &super_repo, "agent1", Some("feat/no-auth")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();
    let digest = nested_dirt_digest(&info.path);

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/no-auth",
        true,
        false,
        Some(&digest),
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("discard") && error.contains("force"),
        "(a) refusal must identify the missing discard authorization: {result}"
    );
    assert!(info.path.exists(), "(a) target must remain on refusal");
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "(a) absent binding must remain absent on refusal"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Explicit discard without a confirmation digest is not authorization.
#[cfg(unix)]
#[test]
fn discard_seam_force_without_digest_refuses() {
    let home = release_tmp_home("discard-no-digest");
    let super_repo = tmp_super_one_sub("discard-no-digest");
    let info = create(&home, &super_repo, "agent1", Some("feat/no-digest")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/no-digest",
        true,
        true,
        None,
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("digest"),
        "missing confirmation digest must refuse authorization: {result}"
    );
    assert!(info.path.exists(), "missing digest must preserve target");
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "missing digest must preserve the absent binding"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// (b) RED: authorization with matching digest must succeed for nested-only dirt.
#[cfg(unix)]
#[test]
fn discard_seam_matching_digest_succeeds() {
    let home = release_tmp_home("discard-match");
    let super_repo = tmp_super_one_sub("discard-match");
    let info = create(&home, &super_repo, "agent1", Some("feat/match")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();

    let digest = nested_dirt_digest(&info.path);
    assert!(!digest.is_empty(), "precondition: digest is non-empty");

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/match",
        true,
        true,
        Some(&digest),
    );
    assert_eq!(
        result["released"].as_bool(),
        Some(true),
        "(b) authorized discard with matching digest must release: {result}"
    );
    assert!(!info.path.exists(), "(b) released target must be removed");
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "(b) release must leave the already-absent binding absent"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// (c) RED: stale/wrong digest must refuse with a TOCTOU-specific reason.
#[cfg(unix)]
#[test]
fn discard_seam_wrong_digest_refuses_toctou() {
    let home = release_tmp_home("discard-toctou");
    let super_repo = tmp_super_one_sub("discard-toctou");
    let info = create(&home, &super_repo, "agent1", Some("feat/toctou")).expect("worktree");
    std::fs::write(
        info.path.join("vendor/dep/vendored.txt"),
        b"nested-edit-v1\n",
    )
    .unwrap();
    let stale_digest = nested_dirt_digest(&info.path);

    // Mutate further so the real enumeration digest no longer matches the
    // stale digest (the digest intentionally covers status/path, not bytes).
    std::fs::write(
        info.path.join("vendor/dep/vendored.txt"),
        b"nested-edit-v2-changed\n",
    )
    .unwrap();
    std::fs::write(
        info.path.join("vendor/dep/toctou-race.txt"),
        b"appeared-after-confirmation\n",
    )
    .unwrap();
    let fresh_digest = nested_dirt_digest(&info.path);
    assert_ne!(
        stale_digest, fresh_digest,
        "precondition: digest changed after mutation"
    );

    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "legacy TOCTOU fixture starts without a binding"
    );
    let nested_before = std::fs::read(info.path.join("vendor/dep/vendored.txt"))
        .expect("nested bytes before TOCTOU release");
    let race_before = std::fs::read(info.path.join("vendor/dep/toctou-race.txt"))
        .expect("TOCTOU race file before release");
    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/toctou",
        true,
        true,
        Some(&stale_digest),
    );
    let reason = result["error"].as_str().unwrap_or("");
    assert!(
        reason.contains("digest"),
        "(c) TOCTOU refusal must mention 'digest' in reason, got: {result}"
    );
    assert!(info.path.exists(), "(c) stale digest must preserve target");
    assert_eq!(
        std::fs::read(info.path.join("vendor/dep/vendored.txt")).unwrap(),
        nested_before,
        "(c) stale digest must not reset nested bytes"
    );
    assert_eq!(
        std::fs::read(info.path.join("vendor/dep/toctou-race.txt")).unwrap(),
        race_before,
        "(c) stale digest must not remove newly appeared nested dirt"
    );
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "(c) stale digest must not create or mutate a binding"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/toctou").is_empty(),
        "(c) stale digest must not mint a recovery ref"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// (d) RED: if targeted reset leaves residual untracked dirt, must still refuse.
#[cfg(unix)]
#[test]
fn discard_seam_residual_dirt_after_reset_refuses() {
    let home = release_tmp_home("discard-residual");
    let super_repo = tmp_super_one_sub("discard-residual");
    let info = create(&home, &super_repo, "agent1", Some("feat/residual")).expect("worktree");
    // Tracked modification (would be reset by `git submodule update --force`)
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"tracked-mod\n").unwrap();
    // Untracked file (persists after `git submodule update --force`)
    std::fs::write(info.path.join("vendor/dep/rogue-untracked.txt"), b"rogue\n").unwrap();

    let digest = nested_dirt_digest(&info.path);

    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "legacy residual fixture starts without a binding"
    );
    let nested_before = std::fs::read(info.path.join("vendor/dep/vendored.txt"))
        .expect("nested bytes before residual release");
    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/residual",
        true,
        true,
        Some(&digest),
    );
    let reason = result["error"].as_str().unwrap_or("");
    assert!(
        reason.contains("residual"),
        "(d) post-reset residual refusal must mention 'residual', got: {result}"
    );
    assert!(info.path.exists(), "(d) residual dirt must preserve target");
    assert_eq!(
        std::fs::read(info.path.join("vendor/dep/vendored.txt")).unwrap(),
        nested_before,
        "(d) residual refusal must not reset tracked nested bytes"
    );
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "(d) residual refusal must not create or mutate a binding"
    );
    assert!(
        recovery_ref_names(&super_repo, "feat/residual").is_empty(),
        "(d) residual refusal must not mint a recovery ref"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// (e) RED: parent-level dirt + nested dirt + authorized discard → parent Preserved.
#[cfg(unix)]
#[test]
fn discard_seam_preserves_parent_wip() {
    let home = release_tmp_home("discard-parent");
    let super_repo = tmp_super_one_sub("discard-parent");
    let info = create(&home, &super_repo, "agent1", Some("feat/parent")).expect("worktree");
    // Parent-level WIP (must be preserved to recovery ref)
    std::fs::write(info.path.join("parent-wip.txt"), b"parent-work\n").unwrap();
    // Nested submodule dirt (must be discarded)
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();

    let digest = nested_dirt_digest(&info.path);

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/parent",
        true,
        true,
        Some(&digest),
    );
    assert_eq!(
        result["released"].as_bool(),
        Some(true),
        "(e) parent WIP must be recovered while nested dirt is discarded: {result}"
    );
    assert!(!info.path.exists(), "(e) released target must be removed");
    assert!(
        crate::binding::read(&home, "agent1").is_none(),
        "(e) release must leave the already-absent binding absent"
    );
    let refs = recovery_ref_names(&super_repo, "feat/parent");
    assert!(
        !refs.is_empty(),
        "(e) recovery ref must exist for parent WIP"
    );
    let tree = git_out(&super_repo, &["ls-tree", "-r", "--name-only", &refs[0]]);
    assert!(
        tree.lines().any(|line| line == "parent-wip.txt"),
        "(e) recovery ref must retain parent WIP, got {tree}"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// (f) RED: successful discard must use the existing append-only event log.
#[cfg(unix)]
#[test]
fn discard_seam_records_audit_evidence() {
    let home = release_tmp_home("discard-audit");
    let super_repo = tmp_super_one_sub("discard-audit");
    let info = create(&home, &super_repo, "agent1", Some("feat/audit")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();

    let digest = nested_dirt_digest(&info.path);

    let event_log = home.join("event-log.jsonl");
    let before = std::fs::read_to_string(&event_log).unwrap_or_default();
    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/audit",
        true,
        true,
        Some(&digest),
    );
    assert_eq!(
        result["released"].as_bool(),
        Some(true),
        "(f) nested-only discard must succeed before audit check: {result}"
    );
    assert!(!info.path.exists(), "(f) successful discard removes target");
    let after = std::fs::read_to_string(&event_log).expect("existing event log");
    let new_lines = after
        .lines()
        .skip(before.lines().count())
        .collect::<Vec<_>>();
    assert!(
        new_lines.iter().any(|line| {
            line.contains("agent1")
                && line.contains("feat/audit")
                && (line.contains("release") || line.contains("discard"))
        }),
        "(f) discard must append agent/branch evidence to event-log.jsonl: {after}"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Trimmed-empty digest must refuse (not silently pass through).
#[cfg(unix)]
#[test]
fn discard_seam_empty_digest_refuses() {
    let home = release_tmp_home("discard-empty-digest");
    let super_repo = tmp_super_one_sub("discard-empty-digest");
    let info = create(&home, &super_repo, "agent1", Some("feat/empty-digest")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"edit\n").unwrap();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/empty-digest",
        true,
        true,
        Some("  "),
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("digest"),
        "trimmed-empty digest must refuse: {result}"
    );
    assert!(
        info.path.exists(),
        "trimmed-empty digest must preserve target"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Trimmed-empty audit_reason must refuse.
#[cfg(unix)]
#[test]
fn discard_seam_empty_audit_reason_refuses() {
    let home = release_tmp_home("discard-empty-reason");
    let super_repo = tmp_super_one_sub("discard-empty-reason");
    let info = create(&home, &super_repo, "agent1", Some("feat/empty-reason")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"edit\n").unwrap();
    let digest = nested_dirt_digest(&info.path);

    let result = release_entry_with_reason(
        &home,
        "agent1",
        &info.path,
        "feat/empty-reason",
        true,
        true,
        Some(&digest),
        "  ",
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("audit_reason"),
        "trimmed-empty audit_reason must refuse: {result}"
    );
    assert!(
        info.path.exists(),
        "trimmed-empty audit_reason must preserve target"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Ordinary nested-dirt refusal (no discard) must return the daemon-computed
/// digest for the confirmation round-trip.
#[cfg(unix)]
#[test]
fn discard_seam_refusal_returns_digest() {
    let home = release_tmp_home("discard-refusal-digest");
    let super_repo = tmp_super_one_sub("discard-refusal-digest");
    let info = create(&home, &super_repo, "agent1", Some("feat/refusal-digest")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"edit\n").unwrap();
    let expected = nested_dirt_digest(&info.path);

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/refusal-digest",
        false,
        false,
        None,
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(!error.is_empty(), "nested dirt must refuse: {result}");
    let returned_digest = result["nested_dirt_digest"].as_str().unwrap_or("");
    assert_eq!(
        returned_digest, expected,
        "refusal must return daemon-computed digest for round-trip: {result}"
    );
    assert!(info.path.exists(), "refusal must preserve target");

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// SHA-256 digest must be 64 hex chars and stable across calls.
#[cfg(unix)]
#[test]
fn discard_seam_sha256_shape_and_stability() {
    let home = release_tmp_home("discard-sha256");
    let super_repo = tmp_super_one_sub("discard-sha256");
    let info = create(&home, &super_repo, "agent1", Some("feat/sha256")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"sha256-test\n").unwrap();

    let d1 = nested_dirt_digest(&info.path);
    let d2 = nested_dirt_digest(&info.path);
    assert_eq!(d1.len(), 64, "SHA-256 hex digest must be 64 chars: {d1}");
    assert!(
        d1.chars().all(|c| c.is_ascii_hexdigit()),
        "digest must be pure hex: {d1}"
    );
    assert_eq!(d1, d2, "digest must be deterministic across calls");

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Successful discard must NOT record audit before parent preservation succeeds.
/// Verify: on success the event is "nested_dirt_discard_release" (not _aborted).
#[cfg(unix)]
#[test]
fn discard_seam_audit_records_after_parent_preservation() {
    let home = release_tmp_home("discard-audit-order");
    let super_repo = tmp_super_one_sub("discard-audit-order");
    let info = create(&home, &super_repo, "agent1", Some("feat/audit-order")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();
    std::fs::write(info.path.join("parent-wip.txt"), b"parent-work\n").unwrap();

    let digest = nested_dirt_digest(&info.path);
    let event_log = home.join("event-log.jsonl");
    let before = std::fs::read_to_string(&event_log).unwrap_or_default();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/audit-order",
        true,
        true,
        Some(&digest),
    );
    assert_eq!(
        result["released"].as_bool(),
        Some(true),
        "must succeed: {result}"
    );

    let after = std::fs::read_to_string(&event_log).expect("event log");
    let new_lines: Vec<&str> = after.lines().skip(before.lines().count()).collect();
    assert!(
        new_lines
            .iter()
            .any(|l| l.contains("nested_dirt_discard_release")),
        "success audit must be nested_dirt_discard_release: {new_lines:?}"
    );
    assert!(
        !new_lines
            .iter()
            .any(|l| l.contains("nested_dirt_discard_aborted")),
        "success must NOT have an aborted audit: {new_lines:?}"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// The refusal digest from an ordinary (no-discard) refusal must be SHA-256
/// shaped (64 hex chars) and usable in the confirmation round-trip.
#[cfg(unix)]
#[test]
fn discard_seam_refusal_digest_is_sha256_round_trippable() {
    let home = release_tmp_home("discard-sha-rt");
    let super_repo = tmp_super_one_sub("discard-sha-rt");
    let info = create(&home, &super_repo, "agent1", Some("feat/sha-rt")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"edit\n").unwrap();

    let refusal = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/sha-rt",
        false,
        false,
        None,
    );
    let returned_digest = refusal["nested_dirt_digest"]
        .as_str()
        .expect("refusal must return digest");
    assert_eq!(
        returned_digest.len(),
        64,
        "refusal digest must be SHA-256 (64 hex): {returned_digest}"
    );

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/sha-rt",
        true,
        true,
        Some(returned_digest),
    );
    assert_eq!(
        result["released"].as_bool(),
        Some(true),
        "round-trip with refusal digest must succeed: {result}"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Deep-nested dirty submodule (sub-within-sub has edits) must be caught by
/// preflight before any target is mutated.
#[cfg(unix)]
#[test]
fn discard_seam_deep_nested_preflight_refuses() {
    let home = release_tmp_home("discard-deep-nested");
    let super_repo = tmp_super_with_nested_submodules("discard-deep-nested");
    commit_marker_gitignore(&super_repo);
    let info = create(&home, &super_repo, "agent1", Some("feat/deep-nested")).expect("worktree");
    let deep_file = info.path.join("vendor/mid/nested/nested_b.txt");
    std::fs::write(&deep_file, b"deep-nested-edit\n").unwrap();

    let digest = nested_dirt_digest(&info.path);
    let dirty_content = std::fs::read(&deep_file).unwrap();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/deep-nested",
        true,
        true,
        Some(&digest),
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("deep-nested"),
        "preflight must refuse deep-nested dirty submodules: {result}"
    );
    assert!(
        info.path.exists(),
        "worktree must be preserved on preflight refusal"
    );
    assert_eq!(
        std::fs::read(&deep_file).unwrap(),
        dirty_content,
        "deep-nested file must not be modified by preflight refusal"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Two dirty submodules: vendor/alpha (valid) and vendor/beta (deep-nested).
/// Preflight must reject the deep-nested one WITHOUT mutating the valid one.
#[cfg(unix)]
#[test]
fn discard_seam_later_invalid_target_no_earlier_mutation() {
    let home = release_tmp_home("discard-multi-preflight");
    let root = std::env::temp_dir().join(format!(
        "agend-wt-multi-pre-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();

    let simple = tmp_repo_with_file("multi-alpha", "alpha.txt", "simple-v1\n");
    let inner = tmp_repo_with_file("multi-inner", "inner.txt", "inner-v1\n");
    let beta_dir = root.join("beta-src");
    std::fs::create_dir_all(&beta_dir).unwrap();
    git_run_ok(&beta_dir, &["init", "-b", "main"], false);
    git_run_ok(&beta_dir, &["config", "user.email", "t@t"], false);
    git_run_ok(&beta_dir, &["config", "user.name", "t"], false);
    git_run_ok(
        &beta_dir,
        &["submodule", "add", &inner.display().to_string(), "inner"],
        true,
    );
    git_run_ok(&beta_dir, &["commit", "-m", "beta with inner"], false);

    let super_dir = root.join("super");
    std::fs::create_dir_all(&super_dir).unwrap();
    git_run_ok(&super_dir, &["init", "-b", "main"], false);
    git_run_ok(&super_dir, &["config", "user.email", "t@t"], false);
    git_run_ok(&super_dir, &["config", "user.name", "t"], false);
    commit_marker_gitignore(&super_dir);
    git_run_ok(
        &super_dir,
        &[
            "submodule",
            "add",
            &simple.display().to_string(),
            "vendor/alpha",
        ],
        true,
    );
    git_run_ok(
        &super_dir,
        &[
            "submodule",
            "add",
            &beta_dir.display().to_string(),
            "vendor/beta",
        ],
        true,
    );
    git_run_ok(&super_dir, &["commit", "-m", "two submodules"], false);

    let info = create(&home, &super_dir, "agent1", Some("feat/multi-pre")).expect("worktree");

    let alpha_file = info.path.join("vendor/alpha/alpha.txt");
    std::fs::write(&alpha_file, b"alpha-dirty\n").unwrap();
    let beta_deep = info.path.join("vendor/beta/inner/inner.txt");
    std::fs::write(&beta_deep, b"deep-dirty\n").unwrap();

    let digest = nested_dirt_digest(&info.path);
    let alpha_dirty = std::fs::read(&alpha_file).unwrap();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/multi-pre",
        true,
        true,
        Some(&digest),
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains("deep-nested"),
        "preflight must catch deep-nested target: {result}"
    );
    assert_eq!(
        std::fs::read(&alpha_file).unwrap(),
        alpha_dirty,
        "earlier valid target must NOT be mutated when later target fails preflight"
    );
    assert!(
        info.path.exists(),
        "worktree must be preserved on preflight refusal"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// When worktree removal fails after successful discard, the event log must
/// contain `nested_dirt_discard_aborted` (with release_failed), NOT the
/// success event `nested_dirt_discard_release`.
#[cfg(unix)]
#[test]
fn discard_seam_removal_failure_no_success_audit() {
    use std::os::unix::fs::PermissionsExt;

    let home = release_tmp_home("discard-rm-fail");
    let super_repo = tmp_super_one_sub("discard-rm-fail");
    let gi = super_repo.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    std::fs::write(&gi, format!("{existing}.trap\n")).unwrap();
    git_run_ok(&super_repo, &["add", ".gitignore"], false);
    git_run_ok(
        &super_repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "gitignore trap",
        ],
        false,
    );

    let info = create(&home, &super_repo, "agent1", Some("feat/rm-fail")).expect("worktree");
    std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-edit\n").unwrap();

    let locked = info.path.join(".trap/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("content.txt"), b"trapped\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let digest = nested_dirt_digest(&info.path);
    let event_log = home.join("event-log.jsonl");
    let before = std::fs::read_to_string(&event_log).unwrap_or_default();

    let result = release_entry(
        &home,
        "agent1",
        &info.path,
        "feat/rm-fail",
        true,
        true,
        Some(&digest),
    );
    assert!(
        result["error"].as_str().is_some(),
        "removal failure must surface an error: {result}"
    );

    let after = std::fs::read_to_string(&event_log).unwrap_or_default();
    let new_lines: Vec<&str> = after.lines().skip(before.lines().count()).collect();
    assert!(
        !new_lines
            .iter()
            .any(|l| l.contains("nested_dirt_discard_release")),
        "removal failure must NOT emit success audit: {new_lines:?}"
    );
    assert!(
        new_lines
            .iter()
            .any(|l| l.contains("nested_dirt_discard_aborted") && l.contains("release_failed")),
        "removal failure must emit aborted/release_failed audit: {new_lines:?}"
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();
    std::fs::remove_dir_all(&info.path).ok();
    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}
