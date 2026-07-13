//! #2755 real-entry RED: `repo action=checkout` must recursively initialize
//! submodules. The MCP entry `handle_checkout_repo` runs `git worktree add`
//! but (pre-fix) skips `submodule update --init --recursive`, so a
//! path-dependency submodule (e.g. `vendor/agentic-git`) is left EMPTY on the
//! provisioned worktree — the build then fails on missing nested content.
//!
//! Fixtures mirror `src/worktree/tests.rs::tmp_super_with_nested_submodules`
//! (that module's helpers are private); a two-level super→A→B nest pins that
//! the fix inits submodules RECURSIVELY, not just one level.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Run git with `AGEND_GIT_BYPASS` (skip the shim); panic on non-zero. When
/// `allow_file`, set `protocol.file.allow=always` so local-path submodule
/// fixtures clone (git's submodule helper ignores repo-stored config).
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

/// A committed local repo with one file at `rel` (the innermost submodule).
fn tmp_repo_with_file(name: &str, rel: &str, body: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-co-subfix-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    git_run_ok(&dir, &["init", "-b", "main"], false);
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

/// Hermetic superproject with two submodule levels:
///   super → `vendor/mid` (A) → `nested` (B, holds `nested_b.txt`).
fn tmp_super_with_nested_submodules(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agend-co-nest-root-{}-{}-{}",
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
        git_run_ok(&dir, &["commit", "-m", "super with A->B nest"], false);
        dir
    };

    let _ = (b, a);
    super_repo
}

fn tmp_home(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-co-home-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// #2755 RED: a real `repo action=checkout` of a repo with (nested) submodules
/// must leave the submodule CONTENT materialized in the provisioned worktree.
/// Pre-fix the handler runs `git worktree add` and skips `--init --recursive`,
/// so the level-B file is missing and the assert fails.
#[test]
fn checkout_initializes_nested_submodules_2755() {
    let home = tmp_home("co-submod");
    let super_repo = tmp_super_with_nested_submodules("co-submod");
    assert!(
        super_repo.join(".gitmodules").is_file(),
        "fixture: super must have .gitmodules"
    );

    // Real MCP entry. bind:false is the minimal materialization path (no
    // lease/bind_full/signing confounds); the `git worktree add` it runs is the
    // exact site that skips submodule init.
    let args = json!({
        "repository_path": super_repo.display().to_string(),
        "branch": "main",
        "bind": false,
    });
    let resp = super::checkout::handle_checkout_repo(&home, &args, "agent-co");
    assert!(resp.get("error").is_none(), "checkout errored: {resp}");
    let wt = PathBuf::from(
        resp["path"]
            .as_str()
            .unwrap_or_else(|| panic!("checkout must return a path: {resp}")),
    );

    // Decisive pin: the level-B file exists inside the provisioned worktree.
    // `git worktree add` alone leaves vendor/mid (and its nested/) empty.
    let nested_b = wt.join("vendor/mid/nested/nested_b.txt");
    assert!(
        nested_b.is_file(),
        "#2755: repo checkout must recursively init submodules so {} exists; \
         `git worktree add` alone leaves submodule dirs empty",
        nested_b.display()
    );
    // Windows git may rewrite LF→CRLF on checkout; pin payload only.
    let body = std::fs::read_to_string(&nested_b).unwrap();
    assert_eq!(
        body.trim_end_matches(['\r', '\n']),
        "level-b-payload",
        "nested submodule payload must match regardless of CRLF vs LF"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}
