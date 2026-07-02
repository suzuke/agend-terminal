//! #2524 P6-r2 (#2537) — discharge ledger consumption tests for the two
//! chokepoints: `reclaim_renudge_worthy` (via `is_discharged_ci_fail`) and
//! `unread_count_after_discharge`. §3.9: exercised through the REAL entry
//! points (`enqueue`, `record_discharge`, real ci-watch JSON files on disk),
//! not synthetic mid-pipeline injection.
//!
//! Attached to `src/inbox/storage.rs`; private items are reached via `super::`.

use super::{enqueue, is_discharged_ci_fail, reclaim_renudge_worthy, unread_count_after_discharge};
use crate::daemon::ci_watch::{ci_watches_dir, watch_filename, WatchState};
use crate::daemon::discharge_ledger::record_discharge;
use crate::inbox::InboxMessage;
use std::fs;
use std::path::PathBuf;

fn tmp_home(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-p6-discharge-consume-{}-{}",
        suffix,
        std::process::id()
    ));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).ok();
    dir
}

/// A ci-fail body byte-identical in shape to `daemon/ci_watch/poller.rs`'s
/// `build_inbox_body` (`"{headline}\nDetail: {detail}\nURL: {run_url}"`).
fn ci_fail_body(repo: &str, branch: &str, short_sha: &str, job: &str) -> String {
    format!("[ci-fail] {repo}@{branch} ({short_sha}): failure\nDetail: {job}\nURL: https://example/run/1")
}

fn ci_fail_msg(repo: &str, branch: &str, short_sha: &str, job: &str) -> InboxMessage {
    InboxMessage::new_system(
        "system:ci",
        "ci-watch",
        ci_fail_body(repo, branch, short_sha, job),
    )
    .with_correlation_id(format!("{repo}@{branch}"))
}

/// Write a minimal ci-watch WatchState with the given CURRENT head_sha — the
/// on-disk file `is_discharged_ci_fail` reads to resolve "the watch's current
/// head" (not the message body's truncated short-sha).
fn write_watch(home: &std::path::Path, repo: &str, branch: &str, head_sha: &str) {
    let dir = ci_watches_dir(home);
    fs::create_dir_all(&dir).unwrap();
    let ws = WatchState {
        repo: repo.to_string(),
        branch: branch.to_string(),
        head_sha: Some(head_sha.to_string()),
        ..Default::default()
    };
    fs::write(
        dir.join(watch_filename(repo, branch)),
        serde_json::to_string_pretty(&ws).unwrap(),
    )
    .unwrap();
}

fn event_log_contents(home: &std::path::Path) -> String {
    fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default()
}

const REPO: &str = "o/r";
const BRANCH: &str = "feat/x";
const HEAD: &str = "abcdef1234567890abcdef1234567890abcdef12";
const SHORT: &str = "abcdef1";
const JOB: &str = "Coverage";

// ───────────────────── extract_ci_fail_job (pure parser) ─────────────────────

#[test]
fn extract_job_reads_detail_line() {
    let body = ci_fail_body(REPO, BRANCH, SHORT, JOB);
    assert_eq!(
        super::extract_ci_fail_job(&body).as_deref(),
        Some(JOB),
        "must extract the exact Detail: line content"
    );
}

#[test]
fn extract_job_none_when_no_detail_line() {
    // A ci-pass/ci-ended body has no `Detail:` line at all.
    let body = "[ci-pass] o/r@feat/x (abcdef1): passed ✓\nURL: https://example/run/1";
    assert_eq!(super::extract_ci_fail_job(body), None);
}

#[test]
fn extract_job_none_when_detail_empty() {
    let body = "[ci-fail] o/r@feat/x: failure\nDetail: \nURL: x";
    assert_eq!(super::extract_ci_fail_job(body), None);
}

// ───────────────────── is_discharged_ci_fail (pure predicate) ─────────────────────

#[test]
fn is_discharged_true_on_exact_signature_match() {
    let home = tmp_home("discharged-true");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", Some("flaky, reran")).unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        is_discharged_ci_fail(&home, &msg),
        "exact (head, job) match against the watch's CURRENT head must be discharged"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn is_discharged_false_wrong_kind() {
    let home = tmp_home("wrong-kind");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let mut msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    msg.kind = Some("report".to_string());
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "only kind=ci-watch is ever eligible for discharge absorption"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn is_discharged_false_different_job_2_of_5() {
    let home = tmp_home("diff-job");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, "audit", "dev-1", None).unwrap();

    // Same head, DIFFERENT job — must not be absorbed (fail-open to notify).
    let msg = ci_fail_msg(REPO, BRANCH, SHORT, "Coverage");
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "a different job at the same head must not be silently absorbed"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 3: head advanced past the discharged one — resumes notifying.
#[test]
fn is_discharged_false_after_head_advances_3_of_5() {
    let home = tmp_home("head-advanced");
    // Discharge was recorded against the OLD head...
    record_discharge(&home, "old-head-sha", JOB, "dev-1", None).unwrap();
    // ...but the watch's CURRENT head has since moved on.
    write_watch(&home, REPO, BRANCH, "new-head-sha");

    let msg = ci_fail_msg(REPO, BRANCH, "new-head", JOB);
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "a discharge recorded against a superseded head must not suppress the NEW head's failure"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 4: no ledger entry at all → regression/invariance (fail-open).
#[test]
fn is_discharged_false_no_ledger_entry_4_of_5() {
    let home = tmp_home("no-ledger");
    write_watch(&home, REPO, BRANCH, HEAD);
    // No `record_discharge` call at all — the discharge-ledger dir doesn't exist.
    assert!(!crate::daemon::discharge_ledger::discharge_ledger_dir(&home).exists());

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "no discharge ever recorded → never absorbed, byte-identical to pre-#2537"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 5: a corrupt ledger file fails OPEN (never panics, never
/// silently absorbs on a read error).
#[test]
fn is_discharged_false_on_corrupt_ledger_file_5_of_5() {
    let home = tmp_home("corrupt-ledger");
    write_watch(&home, REPO, BRANCH, HEAD);
    let dir = crate::daemon::discharge_ledger::discharge_ledger_dir(&home);
    fs::create_dir_all(&dir).unwrap();
    let sha_hex = crate::daemon::utils::sha256_hex(HEAD.as_bytes());
    fs::write(dir.join(format!("{sha_hex}.json")), b"{ not valid json").unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "a corrupt ledger file must fail OPEN (deliver), never panic, never silently absorb"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn is_discharged_false_no_correlation_id() {
    let home = tmp_home("no-correlation");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let mut msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    msg.correlation_id = None;
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "no correlation_id → can't resolve a watch → fail open"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn is_discharged_false_no_watch_file() {
    let home = tmp_home("no-watch-file");
    // No `write_watch` call — no ci-watch JSON exists for this repo@branch.
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "no on-disk watch to resolve the current head from → fail open"
    );

    fs::remove_dir_all(&home).ok();
}

/// #2524 P6-r2 r1 (gapfix-reviewer secondary coverage-gap finding): a
/// `correlation_id` PRESENT but not a parseable `repo@branch` pair (no `@`)
/// must fail open — distinct from the already-covered "correlation_id
/// entirely absent" case above.
#[test]
fn is_discharged_false_malformed_correlation_id() {
    let home = tmp_home("malformed-correlation");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let mut msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    msg.correlation_id = Some("not-a-repo-at-branch-pair".to_string());
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "a correlation_id with no '@' separator can't split into (repo, branch) → fail open"
    );

    fs::remove_dir_all(&home).ok();
}

/// #2524 P6-r2 r1 (gapfix-reviewer secondary coverage-gap finding): a
/// ci-watch file that EXISTS but has no `head_sha` yet (a fresh watch before
/// its first poll populates it) must fail open — distinct from the
/// already-covered "no watch file at all" case above.
#[test]
fn is_discharged_false_watch_file_with_no_head_sha_yet() {
    let home = tmp_home("watch-no-head-sha");
    let dir = ci_watches_dir(&home);
    fs::create_dir_all(&dir).unwrap();
    let ws = WatchState {
        repo: REPO.to_string(),
        branch: BRANCH.to_string(),
        head_sha: None, // fresh watch, first poll hasn't run yet
        ..Default::default()
    };
    fs::write(
        dir.join(watch_filename(REPO, BRANCH)),
        serde_json::to_string_pretty(&ws).unwrap(),
    )
    .unwrap();
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        !is_discharged_ci_fail(&home, &msg),
        "a watch file with head_sha=None can't resolve a current head → fail open"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn is_discharged_absorption_writes_audit_log_entry() {
    let home = tmp_home("audit-log");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", Some("flaky")).unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(is_discharged_ci_fail(&home, &msg));

    let log = event_log_contents(&home);
    assert!(
        log.contains("discharge_absorbed"),
        "absorption must leave an audit trail: {log}"
    );
    assert!(log.contains(HEAD) && log.contains(JOB));

    fs::remove_dir_all(&home).ok();
}

// ───────────────────── reclaim_renudge_worthy (chokepoint a) ─────────────────────

#[test]
fn reclaim_renudge_worthy_false_for_discharged_ci_fail() {
    let home = tmp_home("reclaim-discharged");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, JOB, "dev-1", None).unwrap();

    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(
        !reclaim_renudge_worthy(&home, &msg),
        "a discharged ci-fail is never worthy of reclaim re-nudge"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn reclaim_renudge_worthy_false_for_undischarged_ci_fail_regression() {
    // #2537's own honest finding: ci-watch was ALREADY non-worthy before this
    // change (obligation_reason=None, kind_is_unknown=false) — this pins that
    // pre-existing behavior is unchanged for the non-discharged case too.
    let home = tmp_home("reclaim-undischarged");
    let msg = ci_fail_msg(REPO, BRANCH, SHORT, JOB);
    assert!(!reclaim_renudge_worthy(&home, &msg));
    fs::remove_dir_all(&home).ok();
}

// ───────────────────── unread_count_after_discharge (chokepoint b) ─────────────────────

/// Requirement 1: after discharge, a same-signature duplicate no longer counts
/// (this is `collect_poll_reminders`'s literal observed bug — re-nudge on a
/// count bump the daemon can't otherwise tell is the same obligation).
#[test]
fn unread_count_after_discharge_absorbs_same_signature_duplicate_1_of_5() {
    let home = tmp_home("count-absorbs");
    write_watch(&home, REPO, BRANCH, HEAD);

    // First failure — not yet discharged, counts normally.
    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, SHORT, JOB)).unwrap();
    let (before, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(before, 1);

    // Agent triages it — discharge applies to the SIGNATURE, not to any one
    // message row, so it retroactively quiets the still-unread first row too
    // (there is nothing left to act on: the agent already explained it).
    record_discharge(&home, HEAD, JOB, "dev", None).unwrap();

    // A SECOND notification for the exact same (head, job) — a real new file
    // row (a naive count would go 1→2), but semantically the same
    // already-handled failure.
    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, SHORT, JOB)).unwrap();
    let (after, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(
        after, 0,
        "once (head, job) is discharged, BOTH the pre-discharge row and the \
         duplicate are absorbed — discharge is a signature-level fact, not a \
         per-message-row one"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 2: a different job's failure still counts (never silently
/// absorbed just because SOME job on this head was discharged).
#[test]
fn unread_count_after_discharge_still_counts_different_signature_2_of_5() {
    let home = tmp_home("count-diff-sig");
    write_watch(&home, REPO, BRANCH, HEAD);
    record_discharge(&home, HEAD, "audit", "dev", None).unwrap();

    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, SHORT, "Coverage")).unwrap();
    let (count, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(
        count, 1,
        "a different job at the same head must still be counted/notified"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 3: once the head advances, notifications resume even though an
/// old head's job was discharged.
#[test]
fn unread_count_after_discharge_resumes_after_head_advance_3_of_5() {
    let home = tmp_home("count-head-advance");
    record_discharge(&home, "old-head", JOB, "dev", None).unwrap();
    write_watch(&home, REPO, BRANCH, "new-head");

    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, "new-head", JOB)).unwrap();
    let (count, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(
        count, 1,
        "a NEW head's failure must count even if the SAME job was discharged \
         at an older head"
    );

    fs::remove_dir_all(&home).ok();
}

/// Requirement 4: regression/invariance — with no discharge ledger at all,
/// `unread_count_after_discharge` must be byte-identical to `unread_count`.
#[test]
fn unread_count_after_discharge_matches_unread_count_with_no_ledger_4_of_5() {
    let home = tmp_home("count-invariance");
    write_watch(&home, REPO, BRANCH, HEAD);
    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, SHORT, JOB)).unwrap();
    enqueue(
        &home,
        "dev",
        InboxMessage::new_system("peer", "report", "hi".to_string()),
    )
    .unwrap();

    let plain = super::unread_count(&home, "dev");
    let discharge_aware = unread_count_after_discharge(&home, "dev");
    assert_eq!(
        plain, discharge_aware,
        "with nothing ever discharged, the two counters must agree exactly"
    );
    assert_eq!(plain.0, 2);

    fs::remove_dir_all(&home).ok();
}

/// Requirement 5: a corrupt ledger file fails open — the row still counts
/// (never silently dropped on a read error).
#[test]
fn unread_count_after_discharge_counts_through_corrupt_ledger_5_of_5() {
    let home = tmp_home("count-corrupt-ledger");
    write_watch(&home, REPO, BRANCH, HEAD);
    let dir = crate::daemon::discharge_ledger::discharge_ledger_dir(&home);
    fs::create_dir_all(&dir).unwrap();
    let sha_hex = crate::daemon::utils::sha256_hex(HEAD.as_bytes());
    fs::write(dir.join(format!("{sha_hex}.json")), b"{ not valid json").unwrap();

    enqueue(&home, "dev", ci_fail_msg(REPO, BRANCH, SHORT, JOB)).unwrap();
    let (count, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(count, 1, "a corrupt ledger must never cause a silent drop");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn unread_count_after_discharge_non_ci_watch_rows_unaffected() {
    let home = tmp_home("count-non-ci-watch");
    enqueue(
        &home,
        "dev",
        InboxMessage::new_system("peer", "report", "hi".to_string()),
    )
    .unwrap();
    enqueue(
        &home,
        "dev",
        InboxMessage::new_system("peer", "update", "status".to_string()),
    )
    .unwrap();
    let (count, _) = unread_count_after_discharge(&home, "dev");
    assert_eq!(count, 2, "non-ci-watch rows are counted exactly as before");
    fs::remove_dir_all(&home).ok();
}
