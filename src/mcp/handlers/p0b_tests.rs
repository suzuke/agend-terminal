//! Sprint 55 P0-B IMPL — unified bind dynamic binding tests.
//!
//! Located in this sibling module per Sprint 54 PR #517 / Sprint 55 PR
//! #522/#526 cycle-10 file_size_invariant precedent. Covers 15 edge cases
//! (10 dev RCA + 5 reviewer-added per design doc
//! `docs/DESIGN-sprint55-p0b-unified-bind.md` §4). EC2/3/5/8/10/13/14
//! are covered by existing `dispatch_hook::tests` (Sprint 53 prior-art);
//! P0-B tests below focus on the deltas:
//!   EC1   ci(watch) without binding → no_binding_no_repo
//!   EC4   fleet.yaml `repo:` override field schema + resolution
//!   EC6   3-tier source_repo resolution observability
//!   EC7   release_full ci-watch unsubscribe (+ scope correctness)
//!   EC9   bind_self ambiguous_args / dual-arg deprecation
//!   EC11  per-(home,agent) bind in-flight guard
//!   EC12  no remote configured (relies on existing parser None path)
//!   EC15  source_repo path deleted post-bind → ci(watch) errors
//!   binding.json corrupt → ci(watch) errors

use serde_json::json;
use std::path::Path;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-p0b-{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn write_binding(home: &Path, agent: &str, source_repo: &str, branch: &str) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).ok();
    let v = json!({
        "version": 1,
        "agent": agent,
        "branch": branch,
        "source_repo": source_repo,
    });
    std::fs::write(dir.join("binding.json"), serde_json::to_string(&v).unwrap()).ok();
}

fn write_ci_watch(home: &Path, repo: &str, branch: &str, subs: &[&str]) -> std::path::PathBuf {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&ci_dir).ok();
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = ci_dir.join(&filename);
    let subs_json: Vec<_> = subs.iter().map(|s| json!({"instance": s})).collect();
    let watch = json!({
        "repo": repo,
        "branch": branch,
        "subscribers": subs_json,
        "instance": subs.first().copied().unwrap_or(""),
    });
    std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).ok();
    path
}

// ── EC1: ci(watch) without binding → no_binding_no_repo ─────────────────

#[test]
fn ec1_ci_watch_no_binding_no_repo_returns_error() {
    let home = tmp_home("ec1");
    let result = super::ci::handle_watch_ci(&home, &json!({}), "no-such-agent");
    assert_eq!(result["code"], "no_binding_no_repo");
    std::fs::remove_dir_all(&home).ok();
}

// ── EC15: source_repo path deleted post-bind ────────────────────────────

#[test]
fn ec15_ci_watch_source_repo_path_deleted_returns_error() {
    let home = tmp_home("ec15");
    write_binding(
        &home,
        "alpha",
        "/nonexistent/path/that/will/never/exist",
        "feat-x",
    );
    let result = super::ci::handle_watch_ci(&home, &json!({}), "alpha");
    assert_eq!(result["code"], "source_repo_path_deleted");
    std::fs::remove_dir_all(&home).ok();
}

// ── binding.json corrupt → structured error ─────────────────────────────

#[test]
fn ci_watch_binding_corrupt_returns_error() {
    let home = tmp_home("corrupt");
    let dir = crate::paths::runtime_dir(&home).join("alpha");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join("binding.json"), "this is not json").ok();
    let result = super::ci::handle_watch_ci(&home, &json!({}), "alpha");
    assert_eq!(result["code"], "binding_corrupt");
    std::fs::remove_dir_all(&home).ok();
}

// ── EC9: bind_self ambiguous_args ───────────────────────────────────────

#[test]
fn ec9_bind_self_both_args_rejected_as_ambiguous() {
    let home = tmp_home("ec9");
    let sender = crate::identity::Sender::new("alpha");
    let args = json!({"branch": "feat-x", "repo": "owner/name", "source_repo": "/tmp/x"});
    let result = super::worktree::handle_bind_self(&home, &args, &sender);
    assert_eq!(result["code"], "ambiguous_args");
    std::fs::remove_dir_all(&home).ok();
}

// ── EC7: release_full ci-watch unsubscribe scope ────────────────────────
// These tests use a real git source-repo + lease + release_full path so
// the unsubscribe loop sees an actual binding.json with `branch`.

fn setup_git_repo(home: &Path, agent: &str) -> std::path::PathBuf {
    setup_git_repo_with_remote(home, agent, "https://github.com/o/r.git")
}

/// #781 Phase 3 r1: helper used by tests that inline a `git init`
/// (instead of going through `setup_git_repo*`). Populates
/// `refs/remotes/origin/main` so the strict
/// `dispatch_hook::ensure_branch_exists` `git branch X origin/main`
/// fast path resolves locally without a real fetch. Caller is
/// responsible for any prior `git remote add origin <url>` — this
/// helper just writes the ref.
fn populate_origin_main_for_strict_ensure_branch(repo: &Path) {
    let head_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !head_sha.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &head_sha])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
    }
}

fn setup_git_repo_with_remote(home: &Path, agent: &str, origin_url: &str) -> std::path::PathBuf {
    let repo = crate::paths::workspace_dir(home).join(agent);
    std::fs::create_dir_all(&repo).ok();
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    // Sprint 55 P0-B EC7 r1 fixup: configure origin remote so
    // `release_full`'s repo derivation (via `derive_repo_from_remote_pub`)
    // can resolve to a GitHub `owner/repo` for the unsubscribe predicate.
    let _ = std::process::Command::new("git")
        .args(["remote", "add", "origin", origin_url])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    // #781 Phase 3 r1 (Path A — strict mode): populate
    // `refs/remotes/origin/main` so strict `ensure_branch_exists` in
    // dispatch_auto_bind_lease resolves `origin/main` without network.
    // Required because #781 moves branch provisioning from
    // `worktree::create -b` (current-HEAD-based) to the dispatch layer.
    let main_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !main_sha.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &main_sha])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
    }
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "instances:\n  {agent}:\n    backend: claude\n    working_directory: {}\n",
            repo.display()
        ),
    )
    .ok();
    repo
}

// #931 INVERTED (was EC7 r1 binding-branch unsubscribe pin).
//
// Pre-#931: release_full would remove the released agent from the
// binding-branch's subscriber list. Post-#931 (Direction A.1 pure):
// release_full never mutates ci-watch state. Subscriptions persist
// across release per operator intent in #931 body.
#[test]
fn ec7_release_full_unsubscribes_matching_branch() {
    let home = tmp_home("ec7-match");
    setup_git_repo(&home, "alpha");
    let _ = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-ec7m",
        Some("o/r"),
    );
    let watch_path = write_ci_watch(&home, "o/r", "feat-ec7m", &["alpha", "bob"]);
    assert!(watch_path.exists());

    crate::worktree_pool::release_full(&home, "alpha", false);

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&watch_path).unwrap()).unwrap();
    let subs = crate::daemon::ci_watch::parse_subscribers(&v);
    assert!(
        subs.contains(&"alpha".to_string()),
        "#931 GREEN: alpha persists on binding-branch watch across release — got {subs:?}"
    );
    assert!(
        subs.contains(&"bob".to_string()),
        "bob co-subscriber untouched — got {subs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// #931 INVERTED (was Sprint 57 Wave 2 Track B / #546 Item 2 cross-branch sweep pin).
//
// The Sprint 57 broad sweep was reverted in #931: ad-hoc cross-branch
// watches the agent subscribed to MUST persist across release. Hygiene
// is delegated to TTL + PR-terminal + explicit unwatch.
#[test]
fn release_full_unsubscribes_agent_from_cross_branch_watches_too() {
    let home = tmp_home("931-cross-branch-persists");
    setup_git_repo(&home, "alpha");
    let _ = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-ec7u",
        Some("o/r"),
    );
    let other_path = write_ci_watch(&home, "o/r", "dev", &["alpha"]);

    crate::worktree_pool::release_full(&home, "alpha", false);

    assert!(
        other_path.exists(),
        "#931 GREEN: ad-hoc cross-branch watch persists across release (TTL handles hygiene)"
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&other_path).unwrap()).unwrap();
    let subs = crate::daemon::ci_watch::parse_subscribers(&v);
    assert!(
        subs.contains(&"alpha".to_string()),
        "#931 GREEN: alpha persists on ad-hoc cross-branch watch — got {subs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// #931 INVERTED (was Sprint 57 Wave 2 Track B cross-repo broad sweep pin).
//
// The agent-keyed cross-repo unsubscribe was reverted in #931. Watches
// on other repos that the agent subscribed to remain intact across
// release — same persistence rule as cross-branch.
#[test]
fn release_full_unsubscribes_agent_from_cross_repo_watches_too() {
    let home = tmp_home("931-cross-repo-persists");
    setup_git_repo_with_remote(&home, "alpha", "https://github.com/o/repo-a.git");
    let _ = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-x",
        Some("o/repo-a"),
    );
    let cross_repo_path = write_ci_watch(&home, "o/repo-b", "feat-x", &["alpha", "bob"]);
    let own_path = write_ci_watch(&home, "o/repo-a", "feat-x", &["alpha", "bob"]);

    crate::worktree_pool::release_full(&home, "alpha", false);

    let cross: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cross_repo_path).unwrap()).unwrap();
    let cross_subs = crate::daemon::ci_watch::parse_subscribers(&cross);
    assert!(
        cross_subs.contains(&"alpha".to_string()),
        "#931 GREEN: alpha persists on cross-repo watch — got {cross_subs:?}"
    );

    let own: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&own_path).unwrap()).unwrap();
    let own_subs = crate::daemon::ci_watch::parse_subscribers(&own);
    assert!(
        own_subs.contains(&"alpha".to_string()),
        "#931 GREEN: alpha persists on own (binding) watch — got {own_subs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// #931 INVERTED (was EC7 r1 last-subscriber delete-on-empty pin).
//
// Pre-#931: when the released agent was the sole subscriber,
// release_full deleted the watch file entirely. Post-#931: the file
// persists across release. The file preserves `next_after_ci` chain +
// polling state for any next agent who re-subscribes.
#[test]
fn ec7_release_full_removes_watch_file_when_last_subscriber() {
    let home = tmp_home("ec7-last");
    setup_git_repo(&home, "alpha");
    let _ = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-ec7l",
        Some("o/r"),
    );
    let watch_path = write_ci_watch(&home, "o/r", "feat-ec7l", &["alpha"]);
    assert!(watch_path.exists());

    crate::worktree_pool::release_full(&home, "alpha", false);

    assert!(
        watch_path.exists(),
        "#931 GREEN: sole-subscriber watch file persists across release"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── EC11: per-(home,agent) bind in-flight guard ─────────────────────────
// Production semantics validated indirectly via the test-isolation key
// scoping (parallel tests share process but each uses unique home,
// preventing cross-test guard collisions). Direct concurrency proof
// requires threading + barrier; we validate the structural property:
// the guard keying is `(home, agent)`, so two SAME-home SAME-agent
// dispatches in sequence both succeed (RAII releases between calls).
#[test]
fn ec11_sequential_same_agent_same_home_succeeds_via_raii_release() {
    let home = tmp_home("ec11-seq");
    setup_git_repo(&home, "alpha");
    let r1 = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-seq",
        Some("o/r"),
    );
    assert!(r1.is_ok(), "first dispatch ok: {r1:?}");
    crate::worktree_pool::release_full(&home, "alpha", false);
    let r2 = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-2",
        "feat-seq2",
        Some("o/r"),
    );
    assert!(r2.is_ok(), "second dispatch (post-release) ok: {r2:?}");
    std::fs::remove_dir_all(&home).ok();
}

// ── EC4: fleet.yaml `repo:` override field round-trip ───────────────────

#[test]
fn ec4_instance_config_repo_field_round_trips() {
    use crate::fleet::FleetConfig;
    let yaml = r#"
instances:
  alpha:
    backend: claude
    source_repo: /tmp/alpha-src
    repo: owner/canonical
"#;
    let dir = tmp_home("ec4-rt");
    std::fs::write(dir.join("fleet.yaml"), yaml).unwrap();
    let cfg = FleetConfig::load(&dir.join("fleet.yaml")).expect("parse");
    let resolved = cfg.resolve_instance("alpha").expect("resolve");
    assert_eq!(resolved.repo.as_deref(), Some("owner/canonical"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ec4_instance_config_default_repo_none_v1_compat() {
    // Pre-Sprint-55 fleet.yaml lacks `repo:` field; must default to None
    // via serde so existing deployments parse cleanly.
    use crate::fleet::FleetConfig;
    let yaml = r#"
instances:
  alpha:
    backend: claude
"#;
    let dir = tmp_home("ec4-v1");
    std::fs::write(dir.join("fleet.yaml"), yaml).unwrap();
    let cfg = FleetConfig::load(&dir.join("fleet.yaml")).expect("parse");
    let resolved = cfg.resolve_instance("alpha").expect("resolve");
    assert_eq!(resolved.repo, None);
    std::fs::remove_dir_all(&dir).ok();
}

// ── EC6: 3-tier source_repo resolution observability ────────────────────
// The observability log levels are validated by inspection (info/warn
// per tier per impl). Here we structurally verify the resolution chain
// returns the expected path at each tier when `dispatch_auto_bind_lease`
// is invoked.

#[test]
fn ec6_dispatch_uses_fleet_source_repo_tier_when_present() {
    let home = tmp_home("ec6-fleet");
    let src = home.join("src-tier2");
    std::fs::create_dir_all(&src).ok();
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    populate_origin_main_for_strict_ensure_branch(&src);
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  alpha:\n    backend: claude\n    source_repo: {}\n",
            src.display()
        ),
    )
    .ok();
    let r = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-ec6",
        Some("o/r"),
    );
    assert!(r.is_ok(), "dispatch via fleet source_repo tier ok: {r:?}");
    let binding_path = crate::paths::runtime_dir(&home)
        .join("alpha")
        .join("binding.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).unwrap()).unwrap();
    assert_eq!(
        v["source_repo"].as_str(),
        Some(src.display().to_string().as_str()),
        "binding source_repo reflects fleet tier value"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── EC4 (cont.) repo-override-wins-over-derivation through dispatch ─────

#[test]
fn ec4_fleet_repo_override_wins_over_derive() {
    let home = tmp_home("ec4-override");
    let src = home.join("src-noremote");
    std::fs::create_dir_all(&src).ok();
    // git init but NO origin remote registered → derive returns None.
    // #781 Phase 3 r1: populate `refs/remotes/origin/main` so strict
    // `ensure_branch_exists` resolves locally; we intentionally skip
    // `git remote add origin <url>` so `derive_repo_from_remote`
    // returns None (this is the assertion under test — fleet.yaml
    // `repo:` override wins over remote-URL derivation).
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let head_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !head_sha.is_empty() {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &head_sha])
            .current_dir(&src)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
    }
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  alpha:\n    backend: claude\n    source_repo: {}\n    repo: explicit/override\n",
            src.display()
        ),
    )
    .ok();
    // Caller passes repo=None → fleet.yaml `repo:` override wins, ci-watch
    // file lands under "explicit/override".
    let _ =
        super::dispatch_hook::dispatch_auto_bind_lease(&home, "alpha", "T-1", "feat-ec4o", None);
    let watch_filename = crate::daemon::ci_watch::watch_filename("explicit/override", "feat-ec4o");
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(&home).join(&watch_filename);
    assert!(
        watch_path.exists(),
        "ci-watch landed under fleet.yaml `repo:` override path: {}",
        watch_path.display()
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── bind_self with source_repo arg succeeds (positive new-shape proof) ──

#[test]
fn bind_self_with_source_repo_arg_succeeds() {
    let home = tmp_home("bs-src-arg");
    let src = setup_git_repo(&home, "alpha");
    let sender = crate::identity::Sender::new("alpha");
    let args = json!({
        "branch": "feat-bs",
        "source_repo": src.display().to_string(),
    });
    let result = super::worktree::handle_bind_self(&home, &args, &sender);
    assert_eq!(result["bound"], true, "new-shape bind succeeds: {result}");
    std::fs::remove_dir_all(&home).ok();
}

// ── #1044: bind_self task_id plumbing ──────────────────────────────────

#[test]
fn bind_self_with_task_id_arg_persists_real_task_id() {
    let home = tmp_home("bs-tid");
    let src = setup_git_repo(&home, "beta");
    let sender = crate::identity::Sender::new("beta");
    let args = json!({
        "branch": "feat-tid",
        "source_repo": src.display().to_string(),
        "task_id": "t-20260521-real-task",
    });
    let result = super::worktree::handle_bind_self(&home, &args, &sender);
    assert_eq!(result["bound"], true, "bind must succeed: {result}");
    let binding_path = crate::paths::runtime_dir(&home)
        .join("beta")
        .join("binding.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).unwrap()).unwrap();
    assert_eq!(
        v["task_id"].as_str(),
        Some("t-20260521-real-task"),
        "#1044: bind_self with task_id arg must persist real task_id, not 'self'"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bind_self_without_task_id_arg_uses_empty_not_self() {
    let home = tmp_home("bs-no-tid");
    let src = setup_git_repo(&home, "gamma");
    let sender = crate::identity::Sender::new("gamma");
    let args = json!({
        "branch": "feat-no-tid",
        "source_repo": src.display().to_string(),
    });
    let result = super::worktree::handle_bind_self(&home, &args, &sender);
    assert_eq!(result["bound"], true, "bind must succeed: {result}");
    let binding_path = crate::paths::runtime_dir(&home)
        .join("gamma")
        .join("binding.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).unwrap()).unwrap();
    assert_ne!(
        v["task_id"].as_str(),
        Some("self"),
        "#1044: bind_self without task_id must NOT use 'self' sentinel"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── source_repo override via bind_self_with_source param ────────────────

#[test]
fn dispatch_with_source_repo_override_wins_over_fleet() {
    let home = tmp_home("override-wins");
    let stub_src = crate::paths::workspace_dir(&home).join("alpha");
    let real_src = home.join("override-src");
    std::fs::create_dir_all(&real_src).ok();
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&real_src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&real_src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    populate_origin_main_for_strict_ensure_branch(&real_src);
    // fleet.yaml points to a different (stub) source_repo
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  alpha:\n    backend: claude\n    source_repo: {}\n",
            stub_src.display()
        ),
    )
    .ok();
    let r = super::dispatch_hook::dispatch_auto_bind_lease_with_source(
        &home,
        "alpha",
        "T-1",
        "feat-ovr",
        Some("o/r"),
        Some(&real_src), // override wins over fleet stub
    );
    assert!(r.is_ok(), "override dispatch ok: {r:?}");
    let binding_path = crate::paths::runtime_dir(&home)
        .join("alpha")
        .join("binding.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&binding_path).unwrap()).unwrap();
    assert_eq!(
        v["source_repo"].as_str(),
        Some(real_src.display().to_string().as_str()),
        "explicit override path wins over fleet.yaml source_repo"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── ci(watch) auto-derive from binding ──────────────────────────────────

#[test]
fn ci_watch_uses_binding_source_repo_when_repo_arg_absent() {
    let home = tmp_home("ci-auto");
    setup_git_repo(&home, "alpha"); // configures origin → derive succeeds
    let _ = super::dispatch_hook::dispatch_auto_bind_lease(
        &home,
        "alpha",
        "T-1",
        "feat-auto",
        Some("autoderived/repo"),
    );
    // ci(watch) with NO repo arg — handler reads binding's source_repo + derives
    // owner/repo via `derive_repo_from_remote_pub`. Origin is configured to
    // `https://github.com/o/r.git` so derive returns `o/r`.
    //
    // Sprint 57 Wave 2 Track B (#546 Item 3): branch field is now
    // explicit because the default-to-"main" path is rejected by the
    // new E4.5 gate. Any non-protected branch exercises the auto-derive
    // logic this test pins.
    let result = super::ci::handle_watch_ci(&home, &json!({"branch": "feat-auto"}), "alpha");
    assert_eq!(
        result["repo"], "o/r",
        "auto-derive must resolve to 'o/r' from origin URL: {result}"
    );
    assert_eq!(result["watching"], true);
    std::fs::remove_dir_all(&home).ok();
}

// ── ci(watch) without binding nor origin → non_github_remote_no_override ─

#[test]
fn ci_watch_no_origin_remote_returns_non_github_error() {
    // Distinct from EC1 (no binding at all): here a binding exists but
    // its source_repo has no origin remote → derive returns None.
    let home = tmp_home("ci-no-origin");
    let src = crate::paths::workspace_dir(&home).join("alpha");
    std::fs::create_dir_all(&src).ok();
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .current_dir(&src)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    write_binding(
        &home,
        "alpha",
        src.display().to_string().as_str(),
        "feat-no",
    );
    let result = super::ci::handle_watch_ci(&home, &json!({}), "alpha");
    assert_eq!(result["code"], "non_github_remote_no_override");
    std::fs::remove_dir_all(&home).ok();
}
