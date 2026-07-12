//! S2 additive `ci_watches_detail` — binding_state projection matrix (test-first).
//!
//! Contract (task t-…19288-4): `ci_watches` strings stay byte-for-byte; the new
//! `ci_watches_detail` array carries identity (repo/branch/target_head_sha),
//! raw expires_at + last_terminal_seen_at, current_binding (repo+branch — NOT
//! branch alone), and a polling|expired lifecycle + expiry_reason derived from
//! the SAME `classify_subscribed_watch_expiry` predicates the GC reaps on. A
//! non-current binding is `polling`, not stale (#931). Rows GC's protected-
//! migration arm would remove (generic/malformed protected watches) are excluded.

use super::handle_binding_state;
use serde_json::{json, Value};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-s2-detail-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&h).unwrap();
    h
}

/// Bind `agent` to `repo_slug`@`branch` with a GitHub HTTPS origin (the common
/// case). Delegates to [`write_binding_with_origin`].
fn write_binding(home: &std::path::Path, agent: &str, repo_slug: &str, branch: &str) {
    write_binding_with_origin(
        home,
        agent,
        branch,
        &format!("https://github.com/{repo_slug}.git"),
    );
}

/// Bind `agent`@`branch` with `origin_url` as the source-repo origin remote.
/// Creates a REAL git repo so `current_repo` derivation resolves (or, for a
/// non-GitHub forge, fails to resolve) exactly as production would — the
/// current_binding projection needs the bound repo identity, not just the branch.
fn write_binding_with_origin(home: &std::path::Path, agent: &str, branch: &str, origin_url: &str) {
    let src = home.join(format!("src-{agent}"));
    std::fs::create_dir_all(&src).unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&src)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
    };
    git(&["init", "-b", "main"]);
    git(&["remote", "add", "origin", origin_url]);

    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    let wt = home.join(format!("wt-{agent}"));
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".agend-managed"), "x").unwrap();
    let payload = json!({
        "version": 1, "agent": agent, "task_id": "t", "branch": branch,
        "worktree": wt.to_str().unwrap(), "source_repo": src.to_str().unwrap(),
        "issued_at": "2026-05-09T00:00:00Z",
    });
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn write_watch(
    home: &std::path::Path,
    name: &str,
    repo: &str,
    branch: &str,
    agent: &str,
    subscribed_at: &str,
    expires_at: &str,
    last_terminal_seen_at: Option<&str>,
    target_head_sha: Option<&str>,
) {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = json!({
        "repo": repo, "branch": branch,
        "subscribers": [{"instance": agent, "subscribed_at": subscribed_at}],
        "expires_at": expires_at,
    });
    if let Some(ts) = last_terminal_seen_at {
        w["last_terminal_seen_at"] = json!(ts);
    }
    if let Some(sha) = target_head_sha {
        w["target_head_sha"] = json!(sha);
    }
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(&w).unwrap(),
    )
    .unwrap();
}

fn recent() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
}
fn future() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339()
}
fn past() -> String {
    (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
}
const FULL_SHA_A: &str = "aaaa000000000000000000000000000000000000";
const FULL_SHA_B: &str = "bbbb000000000000000000000000000000000000";

fn row<'a>(resp: &'a Value, branch: &str) -> &'a Value {
    resp["ci_watches_detail"]
        .as_array()
        .expect("ci_watches_detail array")
        .iter()
        .find(|e| e["branch"] == json!(branch))
        .unwrap_or_else(|| panic!("no detail for branch {branch}: {resp}"))
}

/// Current binding (repo+branch) → current_binding=true, polling. Non-current
/// live watch → false, polling (NOT stale). `ci_watches` strings intact.
#[test]
fn detail_current_and_noncurrent_are_polling() {
    let home = tmp_home("cur-noncur");
    write_binding(&home, "dev", "o/r", "feat/cur");
    write_watch(
        &home,
        "w_cur",
        "o/r",
        "feat/cur",
        "dev",
        &recent(),
        &future(),
        None,
        None,
    );
    write_watch(
        &home,
        "w_other",
        "o/r",
        "feat/other",
        "dev",
        &recent(),
        &future(),
        None,
        None,
    );

    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let strings: Vec<&str> = r["ci_watches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        strings,
        vec!["o/r:feat/cur", "o/r:feat/other"],
        "ci_watches strings: {r}"
    );

    let cur = row(&r, "feat/cur");
    assert_eq!(cur["current_binding"], json!(true), "{cur}");
    assert_eq!(cur["lifecycle"], json!("polling"), "{cur}");
    assert_eq!(cur["expiry_reason"], json!(null), "{cur}");
    assert_eq!(
        cur["target_head_sha"],
        json!(null),
        "ordinary watch has no pin: {cur}"
    );

    let other = row(&r, "feat/other");
    assert_eq!(other["current_binding"], json!(false), "{other}");
    assert_eq!(
        other["lifecycle"],
        json!("polling"),
        "non-current live watch is polling, NOT stale: {other}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// FINDING 1: `current_binding` is repo+branch. A same-branch watch on a DIFFERENT
/// repo must be current_binding=false (binding identity is not branch alone).
#[test]
fn detail_current_binding_requires_matching_repo() {
    let home = tmp_home("cross-repo");
    write_binding(&home, "dev", "o/r", "feat/x");
    write_watch(
        &home,
        "w_same",
        "o/r",
        "feat/x",
        "dev",
        &recent(),
        &future(),
        None,
        None,
    );
    // Same branch, DIFFERENT repo — must NOT be current.
    write_watch(
        &home,
        "w_other_repo",
        "o/other",
        "feat/x",
        "dev",
        &recent(),
        &future(),
        None,
        None,
    );

    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let same = r["ci_watches_detail"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["repo"] == json!("o/r"))
        .unwrap();
    let other = r["ci_watches_detail"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["repo"] == json!("o/other"))
        .unwrap();
    assert_eq!(
        same["current_binding"],
        json!(true),
        "same repo+branch is current: {same}"
    );
    assert_eq!(
        other["current_binding"],
        json!(false),
        "a same-BRANCH watch on a DIFFERENT repo must NOT be current_binding (finding 1): {other}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// FINDING 2: rows GC's protected-migration arm would remove — a GENERIC main
/// watch (no target SHA) and a MALFORMED exact-head main watch — must be ABSENT
/// from detail (out of per-agent scope); a VALID exact-head main watch remains.
#[test]
fn detail_excludes_protected_migration_rows_keeps_valid_exact_head() {
    let home = tmp_home("prot-mig");
    write_binding(&home, "dev", "o/r", "feat/cur");
    write_watch(
        &home,
        "generic_main",
        "o/r",
        "main",
        "dev",
        &recent(),
        &future(),
        None,
        None,
    );
    write_watch(
        &home,
        "malformed_main",
        "o/r",
        "main",
        "dev",
        &recent(),
        &future(),
        None,
        Some("not-a-sha"),
    );
    write_watch(
        &home,
        "valid_main",
        "o/r",
        "main",
        "dev",
        &recent(),
        &future(),
        None,
        Some(FULL_SHA_A),
    );

    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let mains: Vec<&Value> = r["ci_watches_detail"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["branch"] == json!("main"))
        .collect();
    assert_eq!(
        mains.len(),
        1,
        "only the VALID exact-head main survives: {r}"
    );
    assert_eq!(mains[0]["target_head_sha"], json!(FULL_SHA_A), "{r}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn detail_expired_absolute_ttl() {
    let home = tmp_home("abs-ttl");
    write_binding(&home, "dev", "o/r", "feat/cur");
    write_watch(
        &home,
        "w",
        "o/r",
        "feat/x",
        "dev",
        &recent(),
        &past(),
        None,
        None,
    );
    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let d = row(&r, "feat/x");
    assert_eq!(d["lifecycle"], json!("expired"), "{d}");
    assert_eq!(d["expiry_reason"], json!("absolute_ttl"), "{d}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn detail_expired_terminal_inactivity_ttl() {
    let home = tmp_home("inact-ttl");
    write_binding(&home, "dev", "o/r", "feat/cur");
    let ancient = (chrono::Utc::now() - chrono::Duration::hours(80)).to_rfc3339();
    write_watch(
        &home,
        "w",
        "o/r",
        "feat/x",
        "dev",
        &recent(),
        &future(),
        Some(&ancient),
        None,
    );
    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let d = row(&r, "feat/x");
    assert_eq!(d["lifecycle"], json!("expired"), "{d}");
    assert_eq!(d["expiry_reason"], json!("terminal_inactivity_ttl"), "{d}");
    assert_eq!(
        d["last_terminal_seen_at"],
        json!(ancient),
        "raw timestamp surfaced: {d}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn detail_expired_max_age() {
    let home = tmp_home("max-age");
    write_binding(&home, "dev", "o/r", "feat/cur");
    let old_sub = (chrono::Utc::now() - chrono::Duration::hours(7 * 24 + 1)).to_rfc3339();
    write_watch(
        &home,
        "w",
        "o/r",
        "feat/x",
        "dev",
        &old_sub,
        &future(),
        None,
        None,
    );
    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let d = row(&r, "feat/x");
    assert_eq!(d["lifecycle"], json!("expired"), "{d}");
    assert_eq!(d["expiry_reason"], json!("max_age"), "{d}");
    std::fs::remove_dir_all(&home).ok();
}

/// Two exact-head watches on the SAME repo+branch → DISTINCT rows keyed by
/// target_head_sha, sorted by (repo, branch, target_head_sha).
#[test]
fn detail_multiple_exact_head_same_branch_distinct_by_sha() {
    let home = tmp_home("multi-exact");
    write_binding(&home, "lead", "o/r", "feat/cur");
    write_watch(
        &home,
        "w_a",
        "o/r",
        "main",
        "lead",
        &recent(),
        &future(),
        None,
        Some(FULL_SHA_A),
    );
    write_watch(
        &home,
        "w_b",
        "o/r",
        "main",
        "lead",
        &recent(),
        &future(),
        None,
        Some(FULL_SHA_B),
    );
    let r = handle_binding_state(&home, &json!({"instance": "lead"}), &None);
    let rows: Vec<&Value> = r["ci_watches_detail"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["branch"] == json!("main"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "two distinct exact-head rows on the same branch: {r}"
    );
    assert_eq!(rows[0]["target_head_sha"], json!(FULL_SHA_A), "{r}");
    assert_eq!(rows[1]["target_head_sha"], json!(FULL_SHA_B), "{r}");
    std::fs::remove_dir_all(&home).ok();
}

/// Unbound agent → detail present, every current_binding=false.
#[test]
fn detail_unbound_all_noncurrent() {
    let home = tmp_home("unbound");
    write_watch(
        &home,
        "w",
        "o/r",
        "feat/x",
        "ghost",
        &recent(),
        &future(),
        None,
        None,
    );
    let r = handle_binding_state(&home, &json!({"instance": "ghost"}), &None);
    assert_eq!(r["bound"], json!(false), "{r}");
    let arr = r["ci_watches_detail"]
        .as_array()
        .expect("detail array (unbound)");
    assert_eq!(arr.len(), 1, "{r}");
    assert_eq!(
        arr[0]["current_binding"],
        json!(false),
        "unbound ⇒ nothing current: {r}"
    );
    assert_eq!(arr[0]["lifecycle"], json!("polling"), "{r}");
    std::fs::remove_dir_all(&home).ok();
}

/// r2 (codex #2746): a Bitbucket-Cloud watch is reachable via the PUBLIC `ci`
/// schema — `repository=owner/repo` is a bare, provider-blind slug that
/// `canonicalize_repo_slug` accepts, and `ci_provider=bitbucket_cloud` on a
/// NON-protected branch is not rejected (only `bitbucket_server` is; the
/// `provider_kind=="github"` gate applies to exact-head PROTECTED watches only).
/// r1 derived `current_repo` with a GitHub-only origin canonicalizer, so a
/// Bitbucket-origin binding resolved current_repo="" and the agent's OWN watch
/// row projected current_binding=false. PRODUCTION-PATH RED: the real
/// `handle_watch_ci` creates the watch (no synthetic sidecar injection).
#[test]
fn detail_current_binding_bitbucket_cloud_origin_production_path() {
    let home = tmp_home("bitbucket-cur");
    write_binding_with_origin(&home, "dev", "feat/x", "https://bitbucket.org/o/r.git");
    // Public sequence: ci watch on a feature branch, explicit bare slug + provider.
    let watch_resp = crate::mcp::handlers::ci::handle_watch_ci(
        &home,
        &json!({
            "action": "watch",
            "repository": "o/r",
            "branch": "feat/x",
            "ci_provider": "bitbucket_cloud",
        }),
        "dev",
    );
    assert!(
        watch_resp.get("error").is_none(),
        "a bitbucket_cloud feature-branch watch is reachable and must be accepted: {watch_resp}"
    );

    let r = handle_binding_state(&home, &json!({"instance": "dev"}), &None);
    let d = row(&r, "feat/x");
    assert_eq!(d["repo"], json!("o/r"), "watch stored the bare slug: {d}");
    assert_eq!(
        d["current_binding"],
        json!(true),
        "the agent's OWN Bitbucket-Cloud-origin watch must be current_binding=true; \
         a GitHub-only current_repo derivation regressed it to false: {d}"
    );
    std::fs::remove_dir_all(&home).ok();
}
