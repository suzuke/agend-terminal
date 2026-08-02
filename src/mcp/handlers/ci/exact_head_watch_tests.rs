//! S1 exact-head protected-main watch — handler gate tests (test-first).
//!
//! Contract (decision d-20260712033954660984-4): a watch on a protected ref
//! (`main`/`master`) is accepted ONLY as an exact-head post-merge watch —
//! full immutable `head_sha` + `task_id` + explicit `next_after_ci`, created
//! by the target team orchestrator or operator, on a GitHub repo. A generic
//! protected watch stays E4.5-rejected. These pin the handler gate; the
//! poller-freshness + sweep behaviors are pinned in their own modules.

use super::watch::handle_watch_ci;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = std::env::temp_dir().join(format!(
        "agend-exact-head-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&h).unwrap();
    h
}

/// Seed a team where `orchestrator` orchestrates `member`, so
/// `teams::is_orchestrator_of(home, orchestrator, member)` is true.
fn seed_team(home: &std::path::Path, orchestrator: &str, member: &str) {
    let yaml = format!(
        "teams:\n  post-merge-team:\n    members:\n      - {member}\n    orchestrator: {orchestrator}\n    created_at: \"2026-01-01T00:00:00Z\"\n"
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

const FULL_SHA: &str = "c4206950c4206950c4206950c4206950c4206950"; // 40-hex

fn exact_head_args(head_sha: &str) -> serde_json::Value {
    json!({
        "repository": "suzuke/agend-terminal",
        "branch": "main",
        "head_sha": head_sha,
        "task_id": "t-1",
        "next_after_ci": ["reviewer-x"],
    })
}

/// Generic protected watch (no head_sha) stays E4.5-rejected — the exact-head
/// path must NOT open a generic bypass.
#[test]
fn generic_main_watch_still_e4_5_rejected() {
    let home = tmp_home("generic-reject");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main"}),
        "lead",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("e4_5_protected_branch"),
        "generic main (no head_sha) must stay E4.5-rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Authorized orchestrator + full SHA + task_id + next_after_ci → accepted.
/// RED today (E4.5 rejects everything on main); GREEN after the gate lands.
#[test]
fn exact_head_main_accepted_for_orchestrator() {
    let home = tmp_home("accept-orch");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "lead");
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "authorized exact-head main watch must be accepted: {r}"
    );
    assert!(r.get("error").is_none(), "no error on accept: {r}");
    assert_eq!(r["subscribers"], json!(["lead"]));
    std::fs::remove_dir_all(&home).ok();
}

/// Operator (empty caller) bypasses the orchestrator check but still needs the
/// full triple (SHA + task_id + next_after_ci).
#[test]
fn exact_head_main_accepted_for_operator() {
    let home = tmp_home("accept-op");
    // No team seeded — operator authority is caller-identity, not membership.
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "");
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "operator exact-head main watch must be accepted: {r}"
    );
    assert_eq!(r["subscribers"], json!(["reviewer-x"]));
    std::fs::remove_dir_all(&home).ok();
}

/// A caller who is NOT the target team orchestrator (nor operator) is rejected
/// on AUTHORITY — distinct from the generic E4.5 rejection.
#[test]
fn exact_head_main_rejected_for_unauthorized_caller() {
    let home = tmp_home("reject-unauth");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args(FULL_SHA), "dev");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_unauthorized"),
        "a non-orchestrator caller must be rejected on authority: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Missing task_id → rejected (exact-head requires the close-loop triple).
#[test]
fn exact_head_main_rejected_without_task_id() {
    let home = tmp_home("reject-no-task");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().remove("task_id");
    let r = handle_watch_ci(&home, &args, "lead");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_missing_requirements"),
        "exact-head without task_id must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Missing next_after_ci → rejected.
#[test]
fn exact_head_main_rejected_without_next_after_ci() {
    let home = tmp_home("reject-no-next");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().remove("next_after_ci");
    let r = handle_watch_ci(&home, &args, "lead");
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_missing_requirements"),
        "exact-head without next_after_ci must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Abbreviated / non-full SHA → rejected (immutable-target requirement).
#[test]
fn exact_head_main_rejected_for_abbreviated_sha() {
    let home = tmp_home("reject-shortsha");
    seed_team(&home, "lead", "reviewer-x");
    let r = handle_watch_ci(&home, &exact_head_args("c420695"), "lead"); // 7-hex
    assert_eq!(
        r["code"].as_str(),
        Some("protected_watch_invalid_sha"),
        "abbreviated SHA must be rejected: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A non-GitHub repository can never arm an exact-head watch. Repo resolution
/// (`canonicalize_repo_slug` / `derive_repo_from_remote`) already rejects every
/// non-GitHub remote before the gate (the daemon only polls GitHub Actions), so
/// the observable contract is "no exact-head watch for a non-GitHub repo". The
/// handler's `detect_provider_from_remote != github` check is a backstop for the
/// binding-derived edge. (Reported to codex: the upstream layer makes the backstop
/// effectively unreachable — candidate for removal per KISS.)
#[test]
fn exact_head_main_rejected_for_non_github_repo() {
    let home = tmp_home("reject-nongh");
    seed_team(&home, "lead", "reviewer-x");
    let mut args = exact_head_args(FULL_SHA);
    args.as_object_mut().unwrap().insert(
        "repository".to_string(),
        json!("https://gitlab.com/suzuke/agend-terminal"),
    );
    let r = handle_watch_ci(&home, &args, "lead");
    assert_ne!(
        r["watching"].as_bool(),
        Some(true),
        "a non-GitHub repo must not arm an exact-head watch: {r}"
    );
    assert!(
        r.get("error").is_some() && r.get("code").is_some(),
        "non-GitHub repo must reject with a structured error+code: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A non-protected branch is unaffected by the exact-head gate: `head_sha` is
/// simply ignored and a normal branch watch is created.
#[test]
fn non_protected_branch_watch_unaffected_by_head_sha() {
    let home = tmp_home("nonprot");
    let r = handle_watch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "feat/x", "head_sha": FULL_SHA}),
        "dev",
    );
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "non-protected branch watch must still work: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// An anonymous caller on a feature branch is not the privileged protected-ref
/// path; explicit next_after_ci targets must not be leaked into subscribers.
#[test]
fn anonymous_feature_branch_does_not_subscribe_next_after_ci_target() {
    let home = tmp_home("nonprot-anonymous-no-leak");
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": "suzuke/agend-terminal",
            "branch": "feat/x",
            "next_after_ci": ["reviewer-x"],
        }),
        "",
    );
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "feature watch must arm: {r}"
    );
    assert_eq!(r["subscribers"], json!([]));
    std::fs::remove_dir_all(&home).ok();
}

/// Auto-watch guard (decision point 3): `head_sha` is INERT on a non-protected
/// branch — `target_head_sha` is only ever persisted behind the protected-ref
/// exact-head gate. The dispatch auto-arm path arms feature branches (never
/// main/master — E4.5 rejects that at dispatch) and passes no head_sha, so it can
/// never mint an exact-head watch; this pins that even a leaked head_sha stays inert.
#[test]
fn head_sha_inert_on_non_protected_branch_no_target_persisted() {
    let home = std::env::temp_dir().join(format!(
        "agend-exact-head-nonprot-no-persist-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&home).expect("create unique temp home");
    let r = handle_watch_ci(
        &home,
        &json!({
            "repository": "suzuke/agend-terminal", "branch": "feat/x",
            "head_sha": FULL_SHA, "task_id": "t-1", "next_after_ci": ["reviewer-x"],
        }),
        "dev",
    );
    assert_eq!(r["watching"].as_bool(), Some(true), "{r}");
    // The single persisted watch file must NOT carry target_head_sha.
    let ci_dir = home.join("ci-watches");
    let entry = std::fs::read_dir(&ci_dir)
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .expect("a watch file was written");
    let raw = std::fs::read(entry.path()).unwrap_or_else(|error| {
        panic!(
            "watch JSON read failed: file={} error={error}",
            entry.path().display()
        )
    });
    let watch: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_else(|error| {
        panic!(
            "watch JSON parse failed: file={} bytes={} raw={raw:?} error={error}",
            entry.path().display(),
            raw.len()
        )
    });
    assert!(
        watch.get("target_head_sha").is_none(),
        "a non-protected-branch watch must never carry target_head_sha: {watch}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #3159: authorized exact-head unwatch (test-first) ─────────────────
//
// `ci unwatch` could address only the generic `repo:branch` key, so a
// post-merge exact-head watch had NO manual disarm and the response still
// claimed `watching:false`. These pin the addressed-disarm contract:
// exact identity only, never a generic fallback, authority no weaker than
// the arm that created it, and an honest scoped response.

use super::unwatch::handle_unwatch_ci;
use super::watch::handle_status_ci;

const SHA_A: &str = "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111";
const SHA_B: &str = "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222";

/// Count live (non-tombstoned) exact-head watch files for repo@branch.
fn armed_exact_heads(home: &std::path::Path) -> Vec<String> {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let mut out: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .filter(|w| w["auto_arm_optout"].as_bool() != Some(true))
                .filter_map(|w| w["target_head_sha"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn arm_two(home: &std::path::Path) {
    seed_team(home, "lead", "reviewer-x");
    for sha in [SHA_A, SHA_B] {
        let r = handle_watch_ci(home, &exact_head_args(sha), "");
        assert_eq!(r["watching"].as_bool(), Some(true), "arm {sha}: {r}");
    }
}

/// The core gap: an authorized orchestrator disarms exactly ONE addressed
/// exact-head watch; the sibling SHA stays armed.
#[test]
fn exact_head_unwatch_disarms_only_the_addressed_sha_3159() {
    let home = tmp_home("eh-unwatch-selective");
    arm_two(&home);
    assert_eq!(armed_exact_heads(&home), vec![SHA_A, SHA_B]);

    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": SHA_A}),
        "lead",
    );
    assert!(
        r.get("error").is_none(),
        "authorized disarm must succeed: {r}"
    );
    assert_eq!(r["scope"].as_str(), Some("exact_head"), "{r}");
    assert_eq!(r["head_sha"].as_str(), Some(SHA_A), "{r}");
    assert_eq!(r["disarmed"].as_bool(), Some(true), "{r}");
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_B],
        "only the addressed SHA may be disarmed"
    );
    assert_eq!(
        r["exact_head_remaining"].as_u64(),
        Some(1),
        "response must report the remaining armed exact-head count: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A malformed SHA must fail closed — never silently fall back to the
/// generic branch watch (that would unwatch the whole branch on a typo).
#[test]
fn exact_head_unwatch_malformed_sha_fails_closed_3159() {
    let home = tmp_home("eh-unwatch-malformed");
    arm_two(&home);
    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": "not-a-sha"}),
        "lead",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("exact_head_unwatch_invalid_sha"),
        "{r}"
    );
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_A, SHA_B],
        "no watch may change"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A well-formed but unknown SHA must fail closed, not fall back.
#[test]
fn exact_head_unwatch_unknown_sha_fails_closed_3159() {
    let home = tmp_home("eh-unwatch-unknown");
    arm_two(&home);
    let unknown = "cccc3333cccc3333cccc3333cccc3333cccc3333";
    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": unknown}),
        "lead",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("exact_head_unwatch_not_found"),
        "{r}"
    );
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_A, SHA_B],
        "no watch may change"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Authority must be no weaker than the arm: a caller who is not the
/// orchestrator of every privileged continuation target is rejected.
#[test]
fn exact_head_unwatch_unauthorized_is_rejected_3159() {
    let home = tmp_home("eh-unwatch-unauth");
    arm_two(&home);
    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": SHA_A}),
        "stranger",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("exact_head_unwatch_unauthorized"),
        "{r}"
    );
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_A, SHA_B],
        "no watch may change"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Generic (no head_sha) unwatch keeps its semantics but stops claiming the
/// repo is unwatched while exact-head watches are still armed.
#[test]
fn generic_unwatch_reports_remaining_exact_heads_3159() {
    let home = tmp_home("eh-unwatch-generic-honest");
    arm_two(&home);
    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main"}),
        "lead",
    );
    assert_eq!(r["scope"].as_str(), Some("generic"), "{r}");
    assert_eq!(
        r["exact_head_remaining"].as_u64(),
        Some(2),
        "generic unwatch must disclose still-armed exact-head watches: {r}"
    );
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_A, SHA_B],
        "generic path must not touch them"
    );
    let st = handle_status_ci(&home, &json!({"repository": "suzuke/agend-terminal"}), "");
    assert_eq!(st["watches"].as_array().map(|a| a.len()), Some(2), "{st}");
    std::fs::remove_dir_all(&home).ok();
}

/// Authority is TIERED: a notification_only merge-receipt assignee owns only
/// its OWN subscription. It must remove itself and leave every co-subscriber
/// (and the watch itself) intact — full disarm stays with operator/orchestrator.
///
/// Control on the durable authority: by disarm time the assignee has moved on
/// and is bound to a LATER, unrelated task. That current binding must NOT block
/// unsubscribing this older receipt-backed watch — the receipt (repo + SHA +
/// task_id + `task_assignee == caller`) is the authority, not the live binding.
#[test]
fn notification_only_assignee_unsubscribes_without_erasing_co_subscribers_3159() {
    let home = tmp_home("eh-unwatch-tiered");
    seed_team(&home, "lead", "dev");
    let task_id = "t-notif-1";
    crate::merge_receipt::persist(
        &home,
        &crate::merge_receipt::MergeReceipt {
            repo: "suzuke/agend-terminal".into(),
            merge_sha: SHA_A.into(),
            task_id: task_id.into(),
            task_assignee: "dev".into(),
            merge_authority: "lead".into(),
            pr_number: 7,
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: (chrono::Utc::now() + chrono::TimeDelta::try_hours(1).unwrap())
                .to_rfc3339(),
        },
    )
    .unwrap();
    let repo_dir = home.join("srcrepo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    crate::binding::bind_full(&home, "dev", task_id, "feat/x", &repo_dir, &repo_dir, false)
        .unwrap();

    let armed = handle_watch_ci(
        &home,
        &json!({
            "repository": "suzuke/agend-terminal",
            "branch": "main",
            "head_sha": SHA_A,
            "task_id": task_id,
            "notification_only": true,
        }),
        "dev",
    );
    assert_eq!(armed["watching"].as_bool(), Some(true), "arm: {armed}");

    // The assignee has since moved on: its CURRENT binding now names a LATER,
    // unrelated task (same worktree — #2158 forbids a silent cross-branch
    // rebind, and the branch is irrelevant to the removed task_id gate).
    crate::binding::bind_full(
        &home,
        "dev",
        "t-later-unrelated",
        "feat/x",
        &repo_dir,
        &repo_dir,
        false,
    )
    .unwrap();
    assert_eq!(
        crate::binding::read(&home, "dev")
            .and_then(|b| b["task_id"].as_str().map(String::from))
            .as_deref(),
        Some("t-later-unrelated"),
        "fixture: current binding must name a task OTHER than the watch's"
    );

    // Inject a co-subscriber directly into the persisted watch (a second
    // interested party); the assignee's unsubscribe must not touch it.
    let path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename_exact_head("suzuke/agend-terminal", "main", SHA_A),
    );
    let mut w: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut subs = w["subscribers"].as_array().cloned().unwrap_or_default();
    subs.push(json!({"instance": "other-watcher", "subscribed_at": "2026-01-01T00:00:00Z"}));
    w["subscribers"] = json!(subs);
    std::fs::write(&path, serde_json::to_string_pretty(&w).unwrap()).unwrap();

    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": SHA_A}),
        "dev",
    );
    assert!(
        r.get("error").is_none(),
        "assignee unsubscribe must succeed: {r}"
    );
    assert_eq!(
        r["disarmed"].as_bool(),
        Some(false),
        "must NOT full-disarm: {r}"
    );
    assert_eq!(r["unsubscribed"].as_bool(), Some(true), "{r}");
    assert_eq!(
        r["subscribers"].as_array().map(|a| a.len()),
        Some(1),
        "co-subscriber must remain: {r}"
    );
    assert_eq!(r["subscribers"][0].as_str(), Some("other-watcher"), "{r}");
    assert_eq!(
        r["watching"].as_bool(),
        Some(true),
        "watch stays armed for the remaining subscriber: {r}"
    );
    // The watch itself is still armed (not tombstoned).
    assert_eq!(
        armed_exact_heads(&home),
        vec![SHA_A],
        "watch must remain armed"
    );
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        persisted["auto_arm_optout"].as_bool() != Some(true),
        "assignee unsubscribe must not tombstone: {persisted}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #3159 correction 1: PRESENCE of `head_sha` selects the exact-head path, so an
/// empty string / non-string must fail closed — never degrade into a generic
/// whole-branch unwatch.
#[test]
fn exact_head_unwatch_present_but_malformed_never_falls_back_3159() {
    for bad in [json!(""), json!(42), json!(true), json!(["x"])] {
        let home = tmp_home("eh-unwatch-present-bad");
        arm_two(&home);
        let r = handle_unwatch_ci(
            &home,
            &json!({
                "repository": "suzuke/agend-terminal",
                "branch": "main",
                "head_sha": bad,
            }),
            "lead",
        );
        assert_eq!(
            r["code"].as_str(),
            Some("exact_head_unwatch_invalid_sha"),
            "head_sha={bad} must fail closed: {r}"
        );
        assert_eq!(
            armed_exact_heads(&home),
            vec![SHA_A, SHA_B],
            "no watch may change for head_sha={bad}"
        );
        // And the generic watch key must be untouched (no tombstone written).
        let generic = crate::daemon::ci_watch::ci_watches_dir(&home).join(
            crate::daemon::ci_watch::watch_filename("suzuke/agend-terminal", "main"),
        );
        assert!(
            !generic.exists(),
            "generic watch must not be created/tombstoned by a bad exact-head selector"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

/// #3159 correction 2: on a notification_only watch the ORCHESTRATOR still holds
/// full-disarm authority (proven via the receipt's merge authority), while the
/// assignee remains limited to its own subscription.
#[test]
fn orchestrator_full_disarms_notification_only_watch_3159() {
    let home = tmp_home("eh-unwatch-orch-notif");
    seed_team(&home, "lead", "dev");
    let task_id = "t-notif-orch";
    crate::merge_receipt::persist(
        &home,
        &crate::merge_receipt::MergeReceipt {
            repo: "suzuke/agend-terminal".into(),
            merge_sha: SHA_A.into(),
            task_id: task_id.into(),
            task_assignee: "dev".into(),
            merge_authority: "lead".into(),
            pr_number: 9,
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: (chrono::Utc::now() + chrono::TimeDelta::try_hours(1).unwrap())
                .to_rfc3339(),
        },
    )
    .unwrap();
    let repo_dir = home.join("srcrepo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    crate::binding::bind_full(&home, "dev", task_id, "feat/x", &repo_dir, &repo_dir, false)
        .unwrap();
    let armed = handle_watch_ci(
        &home,
        &json!({
            "repository": "suzuke/agend-terminal",
            "branch": "main",
            "head_sha": SHA_A,
            "task_id": task_id,
            "notification_only": true,
        }),
        "dev",
    );
    assert_eq!(armed["watching"].as_bool(), Some(true), "arm: {armed}");

    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": SHA_A}),
        "lead",
    );
    assert!(
        r.get("error").is_none(),
        "orchestrator must be authorized: {r}"
    );
    assert_eq!(
        r["disarmed"].as_bool(),
        Some(true),
        "orchestrator holds FULL-disarm authority even on notification_only: {r}"
    );
    assert!(
        armed_exact_heads(&home).is_empty(),
        "watch must be disarmed"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #3159 correction 3: a persisted `target_head_sha` that disagrees with the
/// addressed filename fails closed and mutates nothing (no generic fallback).
#[test]
fn exact_head_unwatch_identity_mismatch_fails_closed_3159() {
    let home = tmp_home("eh-unwatch-identity");
    arm_two(&home);
    // Corrupt ONLY the addressed file's persisted identity.
    let path = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename_exact_head("suzuke/agend-terminal", "main", SHA_A),
    );
    let mut w: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    w["target_head_sha"] = json!(SHA_B);
    std::fs::write(&path, serde_json::to_string_pretty(&w).unwrap()).unwrap();

    let r = handle_unwatch_ci(
        &home,
        &json!({"repository": "suzuke/agend-terminal", "branch": "main", "head_sha": SHA_A}),
        "lead",
    );
    assert_eq!(
        r["code"].as_str(),
        Some("exact_head_unwatch_identity_mismatch"),
        "{r}"
    );
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        after["auto_arm_optout"].as_bool() != Some(true),
        "mismatched file must not be tombstoned: {after}"
    );
    let generic = crate::daemon::ci_watch::ci_watches_dir(&home).join(
        crate::daemon::ci_watch::watch_filename("suzuke/agend-terminal", "main"),
    );
    assert!(!generic.exists(), "no generic fallback mutation");
    std::fs::remove_dir_all(&home).ok();
}
