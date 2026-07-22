#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::gh_poll::tests::MockGhPoller;
use super::super::gh_poll::{GhPrMetadata, GhPrState};
use super::super::{
    freshness_gate, load, new_for_branch, save, CiState, FreshnessGate, MergeState, ReviewClass,
};
use super::scan_and_emit_with;

fn empty_registry() -> crate::agent::AgentRegistry {
    std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn add_typed_receipt(
    state: &mut super::super::PrState,
    verdict: crate::review_receipt::ReviewVerdict,
) {
    let source_id = format!("scanner-test-source-{}", uuid::Uuid::new_v4());
    super::super::apply_receipt_to_state(
        state,
        crate::review_receipt::ReviewReceiptSummary {
            receipt_id: format!("review-receipt:{source_id}"),
            source_id,
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: crate::types::InstanceId::new(),
            reviewer_name: "r".into(),
            repo: state.repo.clone(),
            pr_number: state.pr_number,
            branch: state.branch.clone(),
            task_id: "t-scanner-review".into(),
            reviewed_head: state.head_sha.clone(),
            review_class: state.review_class,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict,
        },
    );
}

/// #2749 test helper: an otherwise fully MergeReady PR (CI green + VERIFIED
/// at `head`, not draft) with an EMPTY freshness cache. Callers stamp /
/// mutate the freshness+observed fields to exercise each gate branch.
fn merge_ready_state(repo: &str, branch: &str, head: &str, pr: u64) -> super::super::PrState {
    let mut s = new_for_branch(repo, branch, head, ReviewClass::Single);
    s.pr_number = pr;
    s.ci_state = CiState::Green {
        sha: head.into(),
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    add_typed_receipt(&mut s, crate::review_receipt::ReviewVerdict::Verified);
    s
}

/// Stamp a VALID freshness tuple onto `s`: three heads agree at `head`,
/// checked base == observed base == `base`, no error, both timestamps == now,
/// the given `behind_by` — so `freshness_gate` returns `Fresh` (behind_by==0)
/// or `Behind` (behind_by>0).
fn stamp_fresh_tuple(s: &mut super::super::PrState, head: &str, base: &str, behind_by: u64) {
    // Stamp 1s in the PAST. In production the off-tick populator / gh_poll
    // writes these timestamps BEFORE the scanner tick reads them, so the gate
    // sees a positive age. It also keeps the pure-classifier unit tests robust
    // under the strict `0 <= age <= ttl` gate (they capture the gate's `now`
    // up front, before this stamp — a same-instant stamp would otherwise read
    // marginally in the future and fail closed).
    let now = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    s.observed_head_sha = Some(head.into());
    s.observed_base_sha = Some(base.into());
    s.observed_at = Some(now.clone());
    s.observed_error = false;
    s.freshness_checked_head_sha = Some(head.into());
    s.freshness_checked_base_sha = Some(base.into());
    s.freshness_checked_at = Some(now);
    s.freshness_behind_by = Some(behind_by);
    s.freshness_error = false;
}

fn open_pr_meta(number: u64, branch: &str) -> GhPrMetadata {
    // A live, open, non-draft PR matching the tracked branch — keeps the
    // snapshot OPEN through `apply_gh_poll` (an EMPTY poll would drive it
    // terminal and resolve the track via the wrong path). gh metadata never
    // carries the review verdict, so verdict_state is untouched.
    GhPrMetadata {
        number,
        author_login: "dev".into(),
        head_ref: branch.into(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Open,
        merged_at: None,
        head_ref_oid: None,
        base_ref_oid: None,
    }
}

/// #t-92758 P1(a): a REJECTED-but-open PR resolves its pending ci-handoff
/// track on the next scan — the #2297 noise root cause (REJECTED is not a
/// terminal PR state, so none of the prior resolvers fired and the watchdog
/// re-nudged every ~2 min).
#[test]
fn scan_evicts_ci_handoff_track_for_rejected_pr() {
    let home = std::env::temp_dir().join(format!(
        "agend-92758-scan-evict-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let mut s = new_for_branch("o/r", "b", "abcdef0", ReviewClass::Single);
    s.pr_number = 42;
    add_typed_receipt(&mut s, crate::review_receipt::ReviewVerdict::Rejected);
    save(&home, &s).unwrap();
    crate::daemon::ci_handoff_track::record(
        &home,
        "lead",
        "o/r@b",
        &chrono::Utc::now().to_rfc3339(),
        Some("abcdef0"),
        None,
    );

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(42, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    assert!(
        crate::daemon::ci_handoff_track::list(&home).is_empty(),
        "REJECTED PR must evict the ci-handoff track (#2297 noise fix)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #t-92758 IRON RULE (end-to-end): a VERIFIED PR must KEEP its ci-handoff
/// track — the normal "your turn / should-merge" handoff + re-nudge survives
/// the new eviction path.
#[test]
fn scan_keeps_ci_handoff_track_for_verified_pr() {
    let home = std::env::temp_dir().join(format!(
        "agend-92758-scan-keep-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let s = merge_ready_state("o/r", "b", "abcdef1", 43);
    save(&home, &s).unwrap();
    crate::daemon::ci_handoff_track::record(
        &home,
        "lead",
        "o/r@b",
        &chrono::Utc::now().to_rfc3339(),
        Some("abcdef1"),
        None,
    );

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(43, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    assert!(
        crate::daemon::ci_handoff_track::list(&home)
            .iter()
            .any(|(_, t)| t.correlation == "o/r@b"),
        "IRON RULE: VERIFIED PR must KEEP its ci-handoff track"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #bughunt3 invariant (#1617 lock-while-blocking class): the worktree
/// auto-release does a `git` subprocess + acquires a second (binding) flock,
/// so it must NEVER run inside the `with_pr_state` closure — that closure
/// runs under the PR-state flock. Structural source-scan (mirrors #1593 F2):
/// brace-match the `|state| { ... }` closure body and assert
/// `auto_release_for_merged_branch` is NOT called inside it, and IS called
/// after the closure (lock-free, post-unlock). Needle is `concat`-built and
/// the scan is prod-sliced so this test can't self-satisfy.
#[test]
fn auto_release_not_called_under_pr_state_flock() {
    let src = include_str!("../scanner.rs");
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => src,
    };

    let closure_needle = [", |state|", " {"].concat();
    let cstart = prod
        .find(&closure_needle)
        .expect("with_pr_state closure present");

    // Brace-match from the closure's opening `{` to find its body span.
    let open_rel = prod[cstart..].find('{').expect("closure block opens");
    let block_start = cstart + open_rel;
    let mut depth = 0usize;
    let mut block_end = block_start;
    for (i, c) in prod[block_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    block_end = block_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(block_end > block_start, "closure block must close");

    let release_needle = ["auto_release_for", "_merged_branch"].concat();
    let closure_body = &prod[block_start..=block_end];
    assert!(
            !closure_body.contains(&release_needle),
            "auto_release_for_merged_branch must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
    assert!(
        prod[block_end..].contains(&release_needle),
        "auto_release_for_merged_branch must run AFTER the PR-state flock is dropped"
    );
}

/// #1629 invariant (#1617 lock-while-blocking class): the inbox emit
/// (`enqueue_with_idle_hint` → loopback `api::call`) must NEVER run inside
/// the `with_pr_state` closure, which holds the PR-state flock. The emits are
/// collected under the flock and drained after it drops. Same structural
/// source-scan as the auto_release invariant above: brace-match the closure
/// body and assert `enqueue_with_idle_hint` is NOT inside it and IS called
/// after. Needle is `concat`-built and the scan is prod-sliced so this test
/// can't self-satisfy.
#[test]
fn deferred_emit_not_called_under_pr_state_flock() {
    let src = include_str!("../scanner.rs");
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => src,
    };

    let closure_needle = [", |state|", " {"].concat();
    let cstart = prod
        .find(&closure_needle)
        .expect("with_pr_state closure present");
    let open_rel = prod[cstart..].find('{').expect("closure block opens");
    let block_start = cstart + open_rel;
    let mut depth = 0usize;
    let mut block_end = block_start;
    for (i, c) in prod[block_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    block_end = block_start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(block_end > block_start, "closure block must close");

    let emit_needle = ["enqueue_with", "_idle_hint"].concat();
    let closure_body = &prod[block_start..=block_end];
    assert!(
            !closure_body.contains(&emit_needle),
            "enqueue_with_idle_hint must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
    assert!(
        prod[block_end..].contains(&emit_needle),
        "enqueue_with_idle_hint must run AFTER the PR-state flock is dropped (deferred drain)"
    );
}

/// #2749 RED (fail-closed anchor, first small increment): an otherwise
/// fully MergeReady PR (CI green at head, VERIFIED at head, not draft) whose
/// deterministic-ancestry freshness tuple is UNKNOWN — `freshness_checked_*`
/// all `None`, the first-observation / pre-populator state — must NOT emit
/// `[pr-ready-for-merge]`. Ancestry is unproven, so the read-only gate fails
/// CLOSED: it suppresses ready and emits NOTHING (never mislabels as
/// pr-needs-rebase), leaving #2747's exact-head merge gate as the hard
/// backstop while the off-tick populator stamps the tuple on a later cycle.
///
/// Against the CURRENT gate-less scanner — which emits pr-ready whenever
/// `merge_state == MergeReady` — this FAILS (ready_emitted_for_sha becomes
/// `Some(head)`), which is the intended RED. The GREEN three-way gate makes
/// it pass. Emission is asserted via the persisted `ready_emitted_for_sha`
/// dedup flag (set under the flock exactly when pr-ready is queued, and only
/// reset on a post-flock enqueue FAILURE — which does not happen against a
/// real temp home).
#[test]
fn merge_ready_without_freshness_tuple_suppresses_pr_ready() {
    let home = std::env::temp_dir().join(format!(
        "agend-2749-fail-closed-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    // Drive is_merge_ready → MergeReady with a server-validated receipt.
    let s = merge_ready_state("o/r", "b", head, 77);
    // Freshness tuple left UNKNOWN (all None) — the fail-closed case.
    assert!(
        s.freshness_checked_head_sha.is_none()
            && s.freshness_checked_base_sha.is_none()
            && s.freshness_behind_by.is_none(),
        "precondition: freshness tuple is unknown"
    );
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(77, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let reloaded = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha, None,
        "#2749 fail-closed: a MergeReady PR with NO freshness tuple must NOT \
             emit [pr-ready-for-merge] (ancestry unproven ⇒ suppress; #2747 is \
             the backstop)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2749 no-regression guard (RED #6): a MergeReady PR whose freshness tuple
/// is VALID and FRESH (three heads agree, checked base == observed base, no
/// error, within TTL, behind_by == 0) must STILL emit [pr-ready-for-merge] —
/// deterministic ancestry proven fresh WINS. This pins the gate so a GREEN
/// implementation cannot degenerate into "never emit" (which would satisfy
/// the fail-closed RED alone). Ancestry-fresh ⇒ ready fires unchanged.
#[test]
fn merge_ready_with_fresh_tuple_still_emits_pr_ready() {
    let home = std::env::temp_dir().join(format!(
        "agend-2749-fresh-emits-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    let mut s = merge_ready_state("o/r", "b", head, 78);
    stamp_fresh_tuple(&mut s, head, "beef0001", 0);
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(78, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let reloaded = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha,
        Some(head.to_string()),
        "#2749 no-regression: a MergeReady PR with a valid FRESH tuple \
             (behind_by=0) must STILL emit [pr-ready-for-merge]"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2749 A5 (Fable pinning test): a PERSISTED MergeReady state whose
/// `review_class` is Unresolved must emit NOTHING from the freshness arm —
/// even with a VALID FRESH tuple that would otherwise open pr-ready. Guards
/// against a future off-tick populator reviving a legacy stale MergeReady
/// whose class was never resolved (the reducer's `is_merge_ready` refuses
/// Unresolved, but a persisted MergeReady bypasses that path). Fail closed.
#[test]
fn merge_ready_unresolved_class_suppresses_freshness_delivery() {
    let home = std::env::temp_dir().join(format!(
        "agend-2749-a5-unresolved-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    let mut s = merge_ready_state("o/r", "b", head, 79);
    // Legacy/torn: persisted MergeReady, but the class was never resolved.
    s.review_class = ReviewClass::Unresolved;
    stamp_fresh_tuple(&mut s, head, "beef0001", 0);
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(79, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let reloaded = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha, None,
        "#2749 A5: a MergeReady state with review_class Unresolved must NOT \
             emit pr-ready even with a fresh tuple (fail closed before delivery)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2749 the pure read-only three-way classifier. Fresh only when the whole
/// tuple agrees and is within TTL at behind_by==0; behind_by>0 ⇒ Behind;
/// every unknown/torn/stale/error input ⇒ Suppress (fail closed). Exercised
/// directly (populator-independent) so the gate logic is pinned without the
/// end-to-end scanner harness.
#[test]
fn freshness_gate_classifies() {
    let now = chrono::Utc::now();
    let head = "aaaaaaa";
    let base = "bbbbbbb";
    let valid = |behind: u64| {
        let mut s = new_for_branch("o/r", "b", head, ReviewClass::Single);
        stamp_fresh_tuple(&mut s, head, base, behind);
        s
    };

    // Fresh: agreeing tuple, within TTL, behind_by == 0.
    assert_eq!(freshness_gate(&valid(0), now, 600), FreshnessGate::Fresh);
    // Behind: agreeing tuple, behind_by > 0.
    assert_eq!(
        freshness_gate(&valid(3), now, 600),
        FreshnessGate::Behind { behind_by: 3 }
    );
    // Unknown: no tuple at all (fresh state) ⇒ Suppress.
    assert_eq!(
        freshness_gate(
            &new_for_branch("o/r", "b", head, ReviewClass::Single),
            now,
            600
        ),
        FreshnessGate::Suppress
    );
    // Checked head != current head ⇒ Suppress.
    let mut s = valid(0);
    s.freshness_checked_head_sha = Some("ccccccc".into());
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    // Observed head != current head (torn) ⇒ Suppress.
    let mut s = valid(0);
    s.observed_head_sha = Some("ccccccc".into());
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    // Checked base != observed base (the #2749 main-advance case) ⇒ Suppress.
    let mut s = valid(0);
    s.observed_base_sha = Some("ddddddd".into());
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    // Compare error ⇒ Suppress.
    let mut s = valid(0);
    s.freshness_error = true;
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    // Observation error ⇒ Suppress.
    let mut s = valid(0);
    s.observed_error = true;
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    // Stale past TTL (evaluate well beyond the 600s bound) ⇒ Suppress.
    assert_eq!(
        freshness_gate(&valid(0), now + chrono::Duration::seconds(900), 600),
        FreshnessGate::Suppress
    );
    // behind_by unknown but tuple otherwise valid ⇒ Suppress (never guess 0).
    let mut s = valid(0);
    s.freshness_behind_by = None;
    assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
}

/// #2749 review-fix (codex): the TTL bound is TWO-SIDED — a FUTURE observed_at
/// or freshness_checked_at must FAIL CLOSED. The original within_ttl checked
/// only `age <= ttl_secs`; a future timestamp yields a NEGATIVE age that
/// silently passed, letting a clock-skewed / forged-future stamp read Fresh
/// indefinitely. Now `0 <= age <= ttl_secs`. A negative `ttl_secs` yields an
/// empty range and can never admit Fresh.
#[test]
fn freshness_gate_future_timestamp_and_negative_ttl_fail_closed() {
    let now = chrono::Utc::now();
    let head = "aaaaaaa";
    let base = "bbbbbbb";
    let valid = |behind: u64| {
        let mut s = new_for_branch("o/r", "b", head, ReviewClass::Single);
        stamp_fresh_tuple(&mut s, head, base, behind);
        s
    };
    let future = (now + chrono::Duration::seconds(300)).to_rfc3339();
    // R2: a SUB-second future stamp (+500ms) is the truncation trap —
    // `num_seconds()` would floor it to 0 and pass `0 <= age`. Full-Duration
    // comparison must still reject it.
    let future_ms = (now + chrono::Duration::milliseconds(500)).to_rfc3339();

    // Sanity: the un-tampered tuple (stamped in the past) is Fresh at `now`.
    assert_eq!(freshness_gate(&valid(0), now, 600), FreshnessGate::Fresh);

    // Future observed_at (300s ahead of `now`) ⇒ Suppress (fail closed).
    let mut s = valid(0);
    s.observed_at = Some(future.clone());
    assert_eq!(
        freshness_gate(&s, now, 600),
        FreshnessGate::Suppress,
        "future observed_at must fail closed (was fail-OPEN under `<= ttl`)"
    );

    // Future freshness_checked_at ⇒ Suppress.
    let mut s = valid(0);
    s.freshness_checked_at = Some(future);
    assert_eq!(
        freshness_gate(&s, now, 600),
        FreshnessGate::Suppress,
        "future freshness_checked_at must fail closed"
    );

    // R2: SUB-second future observed_at (+500ms) ⇒ Suppress. Under the
    // truncating `num_seconds()` this floored to 0 and passed — the fix's
    // full-Duration compare rejects it.
    let mut s = valid(0);
    s.observed_at = Some(future_ms.clone());
    assert_eq!(
        freshness_gate(&s, now, 600),
        FreshnessGate::Suppress,
        "sub-second future observed_at (+500ms) must fail closed (num_seconds truncation trap)"
    );

    // R2: SUB-second future freshness_checked_at (+500ms) ⇒ Suppress.
    let mut s = valid(0);
    s.freshness_checked_at = Some(future_ms);
    assert_eq!(
        freshness_gate(&s, now, 600),
        FreshnessGate::Suppress,
        "sub-second future freshness_checked_at (+500ms) must fail closed"
    );

    // Negative ttl ⇒ empty window ⇒ never Fresh, even for a perfectly current
    // tuple.
    assert_eq!(
        freshness_gate(&valid(0), now, -1),
        FreshnessGate::Suppress,
        "negative ttl must never admit Fresh"
    );
}

// ─── #2749 2b RED: behind → durable pr-needs-rebase + PTY wake ───────────
// These production-entry tests drive `scan_and_emit_with` and assert the
// durable [pr-needs-rebase] row AND its canonical [AGEND-MSG-PENDING] wake.
// They FAIL against this commit's parent (no Behind arm yet); the 2b-GREEN
// Behind arm + post-flock ledger drain + wake makes them pass.

// DeliveryKey::new requires a full 40/64-hex head — the durable ledger keys
// pr-needs-rebase on (repo, PR, head, recipient), so the behind tests use
// realistic full SHAs.
const BEHIND_HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";
const BEHIND_BASE: &str = "1234567890abcdef1234567890abcdef12345678";

/// Write a fleet.yaml so `resolve_merge_authority` returns `orch` (the team
/// orchestrator = merge authority) for a PR authored by team member `member`.
fn write_team_fleet(home: &std::path::Path, orch: &str, member: &str) {
    std::fs::create_dir_all(home.join("inbox")).ok();
    let y = format!(
        "instances:\n  {member}:\n    backend: claude\n  {orch}:\n    backend: claude\n\
             teams:\n  squad:\n    orchestrator: {orch}\n    members:\n      - {member}\n"
    );
    std::fs::write(crate::fleet::fleet_yaml_path(home), y).expect("write fleet.yaml");
}

fn needs_rebase_msgs(home: &std::path::Path, who: &str) -> Vec<crate::inbox::InboxMessage> {
    crate::inbox::drain(home, who)
        .into_iter()
        .filter(|m| m.kind.as_deref() == Some("pr-needs-rebase"))
        .collect()
}

fn tmp_home(ln: u32) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("agend-2749-2b-{}-{}", std::process::id(), ln));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn behind_state(home: &std::path::Path, pr: u64, behind_by: u64) {
    let mut s = merge_ready_state("owner/repo", "feat/x", BEHIND_HEAD, pr);
    s.pr_author = "dev".into();
    stamp_fresh_tuple(&mut s, BEHIND_HEAD, BEHIND_BASE, behind_by);
    save(home, &s).unwrap();
}

fn behind_poller(pr: u64) -> MockGhPoller {
    MockGhPoller::new(vec![Ok(vec![open_pr_meta(pr, "feat/x")])])
}

/// RED#1: behind ⇒ suppress pr-ready + exactly ONE durable [pr-needs-rebase]
/// to each deduped {merge authority (lead), PR owner (dev)}.
#[test]
fn behind_pr_suppresses_ready_and_notifies_authority_and_owner() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 2);

    scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

    let reloaded = load(&home, "owner/repo", "feat/x").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha, None,
        "#2749 behind ⇒ [pr-ready-for-merge] must be suppressed"
    );
    for who in ["lead", "dev"] {
        let nr = needs_rebase_msgs(&home, who);
        assert_eq!(nr.len(), 1, "#2749 behind ⇒ one [pr-needs-rebase] to {who}");
        assert!(
            nr[0].text.contains("owner/repo#77"),
            "PR ref: {}",
            nr[0].text
        );
        assert!(
            nr[0].text.to_lowercase().contains("behind"),
            "states behind: {}",
            nr[0].text
        );
    }
}

/// RED#2: the notice body carries the full payload + reviewed_head.
#[test]
fn behind_needs_rebase_body_carries_full_payload() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 3);

    scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

    let nr = needs_rebase_msgs(&home, "dev");
    assert_eq!(nr.len(), 1, "one notice to the owner");
    let body = &nr[0].text;
    assert!(body.contains("owner/repo#77"), "PR ref: {body}");
    assert!(body.contains(&BEHIND_HEAD[..8]), "head short sha: {body}");
    assert!(body.contains(&BEHIND_BASE[..8]), "main short sha: {body}");
    assert!(body.contains("by 3 commit"), "behind-by count: {body}");
    assert!(body.contains("Re-stamp checklist"), "checklist: {body}");
    assert_eq!(
        nr[0].reviewed_head.as_deref(),
        Some(BEHIND_HEAD),
        "reviewed_head pins the behind head"
    );
}

/// RED#3: the #2745 ledger dedups the ROW per (repo, PR, head, recipient) —
/// N ticks at the same head deliver exactly ONE notice per recipient.
#[test]
fn behind_needs_rebase_delivered_once_across_ticks() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 2);

    for _ in 0..3 {
        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
    }
    for who in ["lead", "dev"] {
        assert_eq!(
            needs_rebase_msgs(&home, who).len(),
            1,
            "#2749 ledger dedup: one notice to {who} across 3 ticks"
        );
    }
}

/// RED#4: recipients deduped BEFORE the ledger keys — owner == merge authority
/// (no team) ⇒ a single notice.
#[test]
fn behind_needs_rebase_dedups_recipient_when_owner_is_authority() {
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok(); // no fleet.yaml ⇒ no team
    let mut s = merge_ready_state("owner/repo", "feat/x", BEHIND_HEAD, 77);
    s.pr_author = "solo".into();
    stamp_fresh_tuple(&mut s, BEHIND_HEAD, BEHIND_BASE, 1);
    save(&home, &s).unwrap();

    scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

    assert_eq!(
        needs_rebase_msgs(&home, "solo").len(),
        1,
        "#2749 owner == authority ⇒ a single deduped notice"
    );
}

/// RED#5 (WAKE): a Delivered row emits exactly ONE canonical
/// [AGEND-MSG-PENDING] pointer wake per deduped recipient (kind=pr-needs-rebase).
#[test]
fn behind_delivered_emits_one_canonical_wake_per_recipient() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 2);

    let (_, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
    });
    let nr_wakes: Vec<_> = wakes
        .iter()
        .filter(|w| w.contains("kind=pr-needs-rebase"))
        .collect();
    assert_eq!(
        nr_wakes.len(),
        2,
        "#2749 wake: one canonical pointer per deduped recipient (lead+dev); got {wakes:?}"
    );
    for w in &nr_wakes {
        assert!(w.contains("[AGEND-MSG-PENDING]"), "canonical pointer: {w}");
        assert!(
            w.contains("inbox="),
            "carries authoritative unread count: {w}"
        );
    }
}

/// RED#6 (WAKE dedup): a second tick at the SAME head enqueues NO new row and
/// emits NO new wake (the ledger suppresses the already-delivered key).
#[test]
fn behind_same_head_next_tick_no_new_row_no_new_wake() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 2);

    // Tick 1: delivered + woken.
    let (_, w1) = crate::inbox::with_captured_pointer_wakes(|| {
        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
    });
    let rows1: usize = ["lead", "dev"]
        .iter()
        .map(|w| needs_rebase_msgs(&home, w).len())
        .sum();
    assert_eq!(rows1, 2, "tick 1 delivers one row per recipient");
    assert_eq!(
        w1.iter()
            .filter(|w| w.contains("kind=pr-needs-rebase"))
            .count(),
        2,
        "tick 1 wakes each recipient once"
    );

    // Tick 2 (same head): ledger Suppressed ⇒ no new row, no new wake.
    let (_, w2) = crate::inbox::with_captured_pointer_wakes(|| {
        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
    });
    let rows2: usize = ["lead", "dev"]
        .iter()
        .map(|w| needs_rebase_msgs(&home, w).len())
        .sum();
    assert_eq!(rows2, 0, "#2749 same head ⇒ no NEW row on tick 2");
    assert_eq!(
        w2.iter()
            .filter(|w| w.contains("kind=pr-needs-rebase"))
            .count(),
        0,
        "#2749 same head ⇒ no NEW wake on tick 2 (Suppressed)"
    );
}

/// RED#7 (WAKE failure): a dropped wake (delivery queue full) must NOT
/// invalidate the durable delivery — the row stays persisted and the ledger
/// stays recorded (a later tick still Suppresses).
#[test]
fn behind_wake_failure_leaves_row_and_ledger_durable() {
    let _guard = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(true);

    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    behind_state(&home, 77, 2);

    // Wake goes to the REAL inject path (no capture) → queue full → wake Errs.
    scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
    crate::daemon::delivery_worker::test_support::set_force_full(false);

    // The durable row is still there despite the dropped wake.
    for who in ["lead", "dev"] {
        assert_eq!(
            needs_rebase_msgs(&home, who).len(),
            1,
            "#2749 wake drop must NOT invalidate the durable row for {who}"
        );
    }
}

/// #2749 wake decision matrix (the ambiguous-record case + the no-wake cases):
/// wake ONLY on Delivered | RecordFailedAfterEnqueue (row durably persisted);
/// never on Suppressed (already delivered) or EnqueueFailed (no row).
#[test]
fn wake_after_ledger_decision_matrix() {
    use crate::daemon::ci_delivery_ledger::{DeliveryError, DeliveryOutcome};
    assert!(
        super::wake_after_ledger(&Ok(DeliveryOutcome::Delivered)),
        "Delivered ⇒ wake"
    );
    assert!(
        super::wake_after_ledger(&Err(DeliveryError::RecordFailedAfterEnqueue(
            anyhow::anyhow!("record write failed")
        ))),
        "RecordFailedAfterEnqueue ⇒ wake (row durably enqueued)"
    );
    assert!(
        !super::wake_after_ledger(&Ok(DeliveryOutcome::Suppressed)),
        "Suppressed ⇒ NO wake (a prior tick already delivered + woke)"
    );
    assert!(
        !super::wake_after_ledger(&Err(DeliveryError::EnqueueFailed(anyhow::anyhow!(
            "enqueue failed"
        )))),
        "EnqueueFailed ⇒ NO wake (no row persisted)"
    );
}

/// #2749: the narrow wake helper builds the CANONICAL [AGEND-MSG-PENDING]
/// pointer (id/kind/from/inbox count) for an already-persisted row.
#[test]
fn wake_persisted_pointer_builds_canonical_inbox_pointer() {
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok();
    // Pre-stamp the id (as the durable ledger path does), persist the row so
    // the authoritative unread count is non-zero, then wake THAT id.
    let mut msg = crate::inbox::InboxMessage::new_system("system:pr-state", "pr-needs-rebase", "b");
    let id = crate::inbox::stamp_message_id(&mut msg);
    assert!(!id.is_empty(), "stamp_message_id assigns an id");
    crate::inbox::enqueue(&home, "rcpt", msg).unwrap();

    let (res, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
        crate::inbox::wake_persisted_pointer(
            &home,
            "rcpt",
            &id,
            "pr-needs-rebase",
            "system:pr-state",
        )
    });
    res.expect("wake ok under capture");
    assert_eq!(wakes.len(), 1, "one pointer captured");
    let p = &wakes[0];
    assert!(p.contains("[AGEND-MSG-PENDING]"), "canonical prefix: {p}");
    assert!(p.contains(&format!("id={id}")), "pre-stamped id: {p}");
    assert!(p.contains("kind=pr-needs-rebase"), "kind: {p}");
    assert!(p.contains("inbox=1"), "authoritative unread count: {p}");
}

// ─── #2749 3a RED: gh_poll atomic head/base observation ──────────────────
// These real-entry tests drive the gh_poll → apply_gh_observations path and
// assert the ATOMIC observed pair is written / preserved. They FAIL against
// this commit's parent (apply_gh_observations does not write observed_* yet);
// the 3a-GREEN write block + failure arm make them pass.

/// An open-PR gh observation that ALSO carries the atomic head+base OIDs, so
/// the real gh_poll → apply_gh_observations path writes the observed pair.
fn open_pr_meta_oids(number: u64, branch: &str, head_oid: &str, base_oid: &str) -> GhPrMetadata {
    GhPrMetadata {
        head_ref_oid: Some(head_oid.into()),
        base_ref_oid: Some(base_oid.into()),
        ..open_pr_meta(number, branch)
    }
}

/// #2749 3a: a live gh-poll carrying head+base OIDs writes the ATOMIC observed
/// pair (observed_head_sha + observed_base_sha + observed_at TOGETHER, clearing
/// observed_error). Real gh_poll → apply_gh_observations path (not injection).
#[test]
fn gh_poll_writes_atomic_observed_head_and_base() {
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok();
    let mut s = new_for_branch("owner/repo", "feat/x", "curhead", ReviewClass::Single);
    s.pr_number = 55;
    s.pr_author = "dev".into();
    assert!(s.observed_head_sha.is_none(), "precondition: unobserved");
    save(&home, &s).unwrap();

    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(
            55, "feat/x", "HEADOID1", "BASEOID1",
        )])]),
    );

    let r = load(&home, "owner/repo", "feat/x").expect("state persists");
    assert_eq!(r.observed_head_sha.as_deref(), Some("HEADOID1"));
    assert_eq!(r.observed_base_sha.as_deref(), Some("BASEOID1"));
    assert!(
        r.observed_at.is_some(),
        "observed_at stamped from the same poll"
    );
    assert!(
        !r.observed_error,
        "a good observation clears observed_error"
    );
}

/// #2913 RED: a fresh gh observation of a force-pushed head must advance the
/// authoritative subject even when no CI run exists for the new head. The
/// scanner must reuse the reducer's Pending transition so stale review and
/// auto-arm state cannot survive the zero-CI head change.
#[test]
fn gh_poll_zero_ci_head_advance_syncs_authoritative_subject() {
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok();
    let mut s = new_for_branch("owner/repo", "feat/x", "sha-A", ReviewClass::Single);
    s.pr_number = 2913;
    s.pr_author = "dev".into();
    s.ci_state = CiState::Green {
        sha: "sha-A".into(),
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    add_typed_receipt(&mut s, crate::review_receipt::ReviewVerdict::Verified);
    s.auto_armed = true;
    s.auto_armed_for_sha = Some("sha-A".into());
    s.ready_emitted_for_sha = Some("sha-A".into());
    s.diagnostic_emitted_for_sha = Some("sha-A".into());
    save(&home, &s).unwrap();

    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(
            2913, "feat/x", "sha-B", "base-B",
        )])]),
    );

    let r = load(&home, "owner/repo", "feat/x").expect("state persists");
    assert_eq!(r.head_sha, "sha-B");
    assert!(matches!(r.ci_state, CiState::Pending));
    assert!(r.validated_review_receipts.is_empty());
    assert!(matches!(
        r.verdict_state,
        super::super::VerdictState::Pending
    ));
    assert!(!r.auto_armed);
    assert_eq!(r.auto_armed_for_sha, None);
    assert_eq!(r.ready_emitted_for_sha, None);
    assert_eq!(r.diagnostic_emitted_for_sha, None);

    let exact_b = crate::review_receipt::ReviewReceiptSummary {
        receipt_id: "review-receipt:exact-b".into(),
        source_id: "source-exact-b".into(),
        evidence_digest: "b".repeat(64),
        assignment_id: uuid::Uuid::new_v4(),
        reviewer_instance_id: crate::types::InstanceId::new(),
        reviewer_name: "reviewer".into(),
        repo: r.repo.clone(),
        pr_number: r.pr_number,
        branch: r.branch.clone(),
        task_id: "t-2913-review".into(),
        reviewed_head: "sha-B".into(),
        review_class: r.review_class,
        slot: crate::review_receipt::ReviewSlot::Primary,
        verdict: crate::review_receipt::ReviewVerdict::Verified,
    };
    assert!(exact_b.matches_state(&r));

    std::fs::remove_dir_all(&home).ok();
}

/// #2749 3a: a gh-poll TRANSPORT FAILURE flags observed_error and does NOT
/// advance observed_at nor clobber the last-good observed pair — the gate then
/// fails closed while the prior observation is preserved (CORRECTION 3 / GO-proof).
#[test]
fn gh_poll_failure_flags_observed_error_without_clobbering() {
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok();
    let mut s = new_for_branch("owner/repo", "feat/x", "curhead", ReviewClass::Single);
    s.pr_number = 55;
    s.pr_author = "dev".into();
    // A prior GOOD observation on disk.
    s.observed_head_sha = Some("GOODHEAD".into());
    s.observed_base_sha = Some("GOODBASE".into());
    s.observed_at = Some("2026-07-12T00:00:00+00:00".into());
    s.observed_error = false;
    save(&home, &s).unwrap();

    scan_and_emit_with(
        &home,
        &empty_registry(),
        &MockGhPoller::new(vec![Err(anyhow::anyhow!("gh transport failed"))]),
    );

    let r = load(&home, "owner/repo", "feat/x").expect("state persists");
    assert!(r.observed_error, "transport failure ⇒ observed_error");
    assert_eq!(
        r.observed_head_sha.as_deref(),
        Some("GOODHEAD"),
        "last-good head preserved (not clobbered)"
    );
    assert_eq!(
        r.observed_base_sha.as_deref(),
        Some("GOODBASE"),
        "last-good base preserved (not clobbered)"
    );
    assert_eq!(
        r.observed_at.as_deref(),
        Some("2026-07-12T00:00:00+00:00"),
        "observed_at NOT advanced on failure"
    );
}

// ─── #2749 3b RED: off-tick freshness populator → scanner (real-entry) ────
// Drive the ACTUAL off-tick worker (worker_poll_and_act) — which observes via
// gh-poll and, once 3b-GREEN wires it, runs the deterministic REMOTE ancestry
// compare (ScmProvider::compare) and stamps freshness_checked_* — then run the
// scanner and assert the end-to-end gate outcome. NO helper-stamped tuples.
// These FAIL vs this commit's parent (the worker does not populate freshness
// yet); 3b-GREEN wires the populator so they pass.

fn full_head(n: u8) -> String {
    format!("{:0>40}", format!("{n}beef"))
}

/// Run the REAL off-tick worker once (poll + observe-consumers + — in GREEN —
/// the ancestry compare + freshness stamp).
fn run_off_tick(home: &std::path::Path, pr: u64, head: &str, base: &str) {
    let cache = super::super::gh_poll::GhPollCache::new();
    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(pr, "feat/x", head, base)])]);
    super::super::gh_poll::worker_poll_and_act(home, &cache, "owner/repo", &poller);
}

/// Scan once with an OID-carrying open-PR poll (writes observed_head/base, 3a).
/// Clears `last_gh_poll_at` first so the production per-file poll cadence
/// (`should_poll`) does not skip the re-observation — the test needs each
/// observation to actually land (in production a main-advance is observed on
/// the next cadence-allowed poll; this just removes that latency for the test).
fn scan_observe(home: &std::path::Path, pr: u64, head: &str, base: &str) {
    let _ = super::super::with_pr_state(home, "owner/repo", "feat/x", |s| {
        s.last_gh_poll_at = None;
    });
    scan_and_emit_with(
        home,
        &empty_registry(),
        &MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(pr, "feat/x", head, base)])]),
    );
}

fn ready_state(home: &std::path::Path, pr: u64, head: &str) {
    let mut s = merge_ready_state("owner/repo", "feat/x", head, pr);
    s.pr_author = "dev".into();
    save(home, &s).unwrap();
}

/// RED 3b-1 (Fresh): observe → off-tick compare behind_by=0 → scan ⇒ pr-ready.
#[test]
fn off_tick_fresh_ancestry_opens_pr_ready() {
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(1);
    ready_state(&home, 88, &head);

    // Pre-populate: a plain scan observes but the gate has no freshness tuple.
    scan_observe(&home, 88, &head, BEHIND_BASE);
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha,
        None,
        "pre-populate: no freshness tuple ⇒ pr-ready suppressed"
    );

    run_off_tick(&home, 88, &head, BEHIND_BASE); // REAL worker: compare + stamp
    scan_observe(&home, 88, &head, BEHIND_BASE); // now Fresh ⇒ emit

    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha
            .as_deref(),
        Some(head.as_str()),
        "#2749 3b: fresh ancestry (behind_by=0) ⇒ pr-ready emits"
    );
}

/// RED 3b-2 (Behind): off-tick compare behind_by=2 ⇒ suppress pr-ready + emit
/// pr-needs-rebase + a canonical wake per recipient.
#[test]
fn off_tick_behind_ancestry_emits_needs_rebase_and_wake() {
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(2));
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = BEHIND_HEAD; // full hex for the ledger DeliveryKey
    ready_state(&home, 88, head);

    scan_observe(&home, 88, head, BEHIND_BASE);
    run_off_tick(&home, 88, head, BEHIND_BASE);
    let (_, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
        scan_observe(&home, 88, head, BEHIND_BASE);
    });

    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha,
        None,
        "#2749 3b: behind ⇒ pr-ready suppressed"
    );
    for who in ["lead", "dev"] {
        assert_eq!(
            needs_rebase_msgs(&home, who).len(),
            1,
            "#2749 3b: one [pr-needs-rebase] to {who}"
        );
    }
    assert_eq!(
        wakes
            .iter()
            .filter(|w| w.contains("kind=pr-needs-rebase"))
            .count(),
        2,
        "#2749 3b: behind ⇒ a canonical wake per recipient"
    );
}

/// RED 3b-3 (main-advance): after a Fresh compare, a NEW observation whose base
/// moved (checked_base != observed_base) ⇒ Suppress until the populator recomputes.
#[test]
fn off_tick_main_advance_suppresses_until_recompute() {
    let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(3);
    let base1 = full_head(10);
    let base2 = full_head(20);
    ready_state(&home, 88, &head);

    // Fresh against base1.
    scan_observe(&home, 88, &head, &base1);
    run_off_tick(&home, 88, &head, &base1);
    // Main advances: a new observation carries base2 ⇒ observed_base=base2, but
    // freshness_checked_base is still base1 ⇒ gate Suppress.
    scan_observe(&home, 88, &head, &base2);
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha,
        None,
        "#2749 3b: base advanced (checked_base != observed_base) ⇒ suppressed"
    );

    // Re-populate against base2 ⇒ converges to Fresh ⇒ emit.
    run_off_tick(&home, 88, &head, &base2);
    scan_observe(&home, 88, &head, &base2);
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha
            .as_deref(),
        Some(head.as_str()),
        "#2749 3b: re-compute against the new base ⇒ pr-ready re-converges"
    );
}

/// RED 3b-4 (compare error): a failed ancestry re-compare (base changed) stamps
/// freshness_error WITHOUT clobbering the last-good checked tuple ⇒ Suppress.
#[test]
fn off_tick_compare_error_suppresses_without_clobber() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(4);
    let base1 = full_head(11);
    let base2 = full_head(22);
    ready_state(&home, 88, &head);

    // A GOOD compare against base1 stamps a Fresh tuple.
    {
        let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
        scan_observe(&home, 88, &head, &base1);
        run_off_tick(&home, 88, &head, &base1);
    }
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .freshness_checked_base_sha
            .as_deref(),
        Some(base1.as_str()),
        "precondition: good compare stamped checked_base=base1"
    );

    // Base advances (tuple changed ⇒ re-compute needed) but the compare FAILS.
    {
        let _scm = crate::scm::set_test_scm_provider(
            crate::scm::MockScmProvider::with_compare_err("forge 500"),
        );
        scan_observe(&home, 88, &head, &base2); // observed_base=base2
        run_off_tick(&home, 88, &head, &base2); // compare(base2) → Err
    }
    let after_err = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(
        after_err.freshness_error,
        "#2749 3b: compare failure ⇒ freshness_error"
    );
    assert_eq!(
        after_err.freshness_checked_base_sha.as_deref(),
        Some(base1.as_str()),
        "#2749 3b: last-good checked tuple preserved (NOT clobbered) on failure"
    );

    scan_observe(&home, 88, &head, &base2);
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .ready_emitted_for_sha,
        None,
        "#2749 3b: freshness_error ⇒ pr-ready suppressed"
    );
}

// ─── #2749 correction (codex): retry-lease backoff + stale-error discard ──
// A persistent compare failure must back off to ONE compare per 60s lease
// (not one per 15s worker cycle), and a stale errored tuple must be discarded
// when the observation advances. These FAIL vs this commit's parent (the
// populator recomputes every cycle on error and never clears the stale error).

fn set_retry_after(home: &std::path::Path, deadline: Option<String>) {
    let _ = super::super::with_pr_state(home, "owner/repo", "feat/x", |s| {
        s.freshness_retry_after = deadline;
    });
}

/// RED (backoff): a persistently-FAILING compare stamps a 60s retry lease and
/// is NOT re-attempted within it (one compare, no 15s storm); it re-attempts
/// once the lease deadline passes.
#[test]
fn off_tick_persistent_failure_backs_off_then_retries_after_lease() {
    let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
    let _scm = crate::scm::set_test_scm_provider(mock.clone());
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(5);
    ready_state(&home, 88, &head);
    scan_observe(&home, 88, &head, BEHIND_BASE);

    run_off_tick(&home, 88, &head, BEHIND_BASE); // compare #1 → Err, lease set
    assert_eq!(mock.compare_calls(), 1, "first cycle compares once");
    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(
        s.freshness_error && s.freshness_retry_after.is_some(),
        "error + lease stamped"
    );

    run_off_tick(&home, 88, &head, BEHIND_BASE); // within lease → SKIP (no compare)
    assert_eq!(
            mock.compare_calls(),
            1,
            "#2749 correction: within the 60s lease the failing tuple must NOT re-compare (no 15s storm)"
        );

    // Age the lease past its deadline → re-attempt.
    set_retry_after(
        &home,
        Some((chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339()),
    );
    run_off_tick(&home, 88, &head, BEHIND_BASE);
    assert_eq!(
        mock.compare_calls(),
        2,
        "#2749 correction: after the lease deadline the tuple re-attempts"
    );
}

/// RED (stale-error discard): a failed compare (error + lease) whose observation
/// then ADVANCES must have the stale error + lease CLEARED, so the NEW tuple is
/// re-attempted immediately rather than staying errored.
#[test]
fn off_tick_observation_change_discards_stale_error_and_lease() {
    let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
    let _scm = crate::scm::set_test_scm_provider(mock.clone());
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(6);
    let base1 = full_head(12);
    let base2 = full_head(24);
    ready_state(&home, 88, &head);
    scan_observe(&home, 88, &head, &base1);
    run_off_tick(&home, 88, &head, &base1); // Err → error + lease for base1
    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(s.freshness_error && s.freshness_retry_after.is_some());

    // Observation advances to base2 ⇒ the base1 error/lease is stale.
    scan_observe(&home, 88, &head, &base2);
    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert!(
        !s.freshness_error,
        "#2749 correction: an observed tuple change must DISCARD the stale freshness_error"
    );
    assert!(
        s.freshness_retry_after.is_none(),
        "#2749 correction: the stale retry lease must be discarded on tuple change"
    );
}

/// R3 mutation guard (codex): the STRONGER form of the test above — it proves
/// the delayed old-tuple error is discarded by the populator's Err-path
/// FULL-TUPLE CAS, not merely by the upstream `apply_gh_observations` clear.
/// The observation advances base1→base2 *in-flight of the compare* (via a mock
/// hook), and only THEN does the compare return Err for base1. A head-only Err
/// CAS (head unchanged) would wrongly stamp the stale base1 error onto the
/// base2 tuple; the full-tuple CAS discards it, leaving base2 clean and
/// immediately eligible. Regressing the CAS in freshness_populator.rs to
/// head-only makes this test FAIL.
#[test]
fn off_tick_inflight_observation_change_discards_delayed_stale_error() {
    let home = tmp_home(line!());
    write_team_fleet(&home, "lead", "dev");
    let head = full_head(9);
    let base1 = full_head(31);
    let base2 = full_head(42);
    ready_state(&home, 88, &head);
    scan_observe(&home, 88, &head, &base1); // observed = (head, base1)

    // compare(base1) advances the persisted observation to base2 WHILE IN
    // FLIGHT, then fails — the delayed base1 Err now targets a superseded tuple.
    let home_hook = home.clone();
    let base2_hook = base2.clone();
    let mock = crate::scm::MockScmProvider::with_compare_err_hook("forge 500", move || {
        let _ = super::super::with_pr_state(&home_hook, "owner/repo", "feat/x", |s| {
            s.observed_base_sha = Some(base2_hook.clone());
        });
    });
    {
        let _scm = crate::scm::set_test_scm_provider(mock);
        run_off_tick(&home, 88, &head, &base1); // compare(base1) → [obs→base2] → Err
    }

    let s = load(&home, "owner/repo", "feat/x").unwrap();
    assert_eq!(
        s.observed_base_sha.as_deref(),
        Some(base2.as_str()),
        "precondition: the in-flight hook advanced the observation to base2"
    );
    assert!(
        !s.freshness_error,
        "#2749 R3: a delayed base1 Err must NOT be stamped onto the base2 tuple \
             (full-tuple CAS); a head-only CAS would wrongly error base2"
    );
    assert!(
        s.freshness_retry_after.is_none(),
        "#2749 R3: no stale retry lease on the superseding base2 tuple"
    );

    // base2 is immediately eligible: a subsequent GOOD compare stamps it fresh
    // (a stale error/lease would have suppressed/leased it instead).
    {
        let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
        run_off_tick(&home, 88, &head, &base2);
    }
    let s2 = load(&home, "owner/repo", "feat/x").unwrap();
    assert_eq!(
        s2.freshness_checked_base_sha.as_deref(),
        Some(base2.as_str()),
        "#2749 R3: base2 was immediately eligible and got a fresh tuple"
    );
    assert_eq!(s2.freshness_behind_by, Some(0));
    assert!(!s2.freshness_error);
}

/// The retry lease persists across restart (serde) and a MALFORMED / absurd
/// deadline fails-closed by re-attempting (self-heal to a valid lease) rather
/// than sticking the PR errored; the gate stays fail-closed on freshness_error.
#[test]
fn off_tick_retry_lease_persists_and_malformed_self_heals() {
    // Restart persistence: a stamped retry lease survives save→load.
    let home = tmp_home(line!());
    std::fs::create_dir_all(home.join("inbox")).ok();
    let mut s = merge_ready_state("owner/repo", "feat/x", &full_head(7), 88);
    s.freshness_retry_after = Some("2026-07-13T00:00:00+00:00".into());
    save(&home, &s).unwrap();
    assert_eq!(
        load(&home, "owner/repo", "feat/x")
            .unwrap()
            .freshness_retry_after
            .as_deref(),
        Some("2026-07-13T00:00:00+00:00"),
        "retry lease persists across restart"
    );

    // Malformed lease + freshness_error ⇒ the populator re-attempts (self-heal),
    // and the gate keeps suppressing (fail-closed) while errored.
    let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
    let _scm = crate::scm::set_test_scm_provider(mock.clone());
    let home2 = tmp_home(line!());
    write_team_fleet(&home2, "lead", "dev");
    let head = full_head(8);
    let mut s = merge_ready_state("owner/repo", "feat/x", &head, 88);
    s.pr_author = "dev".into();
    save(&home2, &s).unwrap();
    scan_observe(&home2, 88, &head, BEHIND_BASE);
    run_off_tick(&home2, 88, &head, BEHIND_BASE); // Err → error + valid lease
    let before = mock.compare_calls();
    set_retry_after(&home2, Some("not-a-timestamp".into())); // corrupt the lease
    run_off_tick(&home2, 88, &head, BEHIND_BASE); // malformed ⇒ re-attempt (self-heal)
    assert!(
        mock.compare_calls() > before,
        "#2749 correction: a malformed retry lease fails-closed by re-attempting (no stuck PR)"
    );
    assert!(
        load(&home2, "owner/repo", "feat/x")
            .unwrap()
            .freshness_error,
        "gate stays fail-closed while errored"
    );
}
/// T21 (B18) — the scanner's A7 terminal wire: a MERGED pr_state records the
/// generation's RETAINED marker and CAS-tombstones ONLY that generation's
/// reviewer-assignment record (real entry `scan_and_emit_with`, marker write is
/// POST-flock / assignment-lock-only — no lock inversion). Then RESTART: the
/// pr_state file is gone (terminal cleanup) and a replayed record for the same
/// terminal generation STAYS removed on reconcile (A10a, marker retained — I19).
#[test]
fn t21_scanner_terminal_marker_tombstone_survives_restart() {
    use crate::daemon::assignment_authority as store;
    let home =
        std::env::temp_dir().join(format!("agend-t21-scan-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let mk = || {
        store::ActiveAssignment::new_pending(
            "o/r",
            "b",
            "reviewer",
            55,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
            "review",
            None,
            None,
            "2026-07-13T00:00:00Z",
        )
    };
    store::persist(&home, &mk()).unwrap();

    // A pr_state ALREADY merged at generation 55. apply_gh_poll SKIPS terminal
    // states, so the main scan loop reads this snapshot and fires the A7 marker.
    let mut s = new_for_branch("o/r", "b", "abcdef0", ReviewClass::Single);
    s.pr_number = 55;
    s.merge_state = super::super::MergeState::Merged {
        merge_commit: "abcdef0".into(),
        merged_at: "2026-07-13T00:00:00Z".into(),
    };
    save(&home, &s).unwrap();

    scan_and_emit_with(&home, &empty_registry(), &MockGhPoller::new(vec![]));

    assert!(
        store::terminal_markers(&home, "o/r", "b").contains(55),
        "scanner recorded the terminal marker for the merged generation"
    );
    assert!(
        store::get(&home, "o/r", "b", "reviewer").is_none(),
        "scanner tombstoned the matching-generation reviewer-assignment record"
    );

    // RESTART: pr_state gone; a replayed record for the terminal generation must
    // STAY removed on the next reconcile (marker retained forever — I19).
    let _ = super::super::remove(&home, "o/r", "b");
    store::persist(&home, &mk()).unwrap();
    crate::daemon::per_tick::assignment_reconcile::reconcile_all(&home, "2026-07-13T01:00:00Z");
    assert!(
        store::get(&home, "o/r", "b", "reviewer").is_none(),
        "replayed terminal-generation record stays removed after restart (A10a)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t-…-17 B3 — the scanner must NOT `remove` the pr_state file when
/// `record_terminal` FAILED. The terminal marker is what A10a keys on; if the
/// marker was never durably written AND the pr_state (the retry source) is
/// deleted, the terminal is LOST — on the next scan CI recreates the file
/// NON-terminal, the terminal is never re-observed, and the merged PR's
/// assignment record RESURRECTS / re-reserves forever (re-opens B17). Fail closed:
/// a failed `record_terminal` must RETAIN the pr_state file as the retry source.
/// RED: the pr_state file is removed even though record_terminal returned Err.
#[test]
fn b3_terminal_record_failure_retains_pr_state_file() {
    use crate::daemon::assignment_authority as store;
    let home =
        std::env::temp_dir().join(format!("agend-b3-scan-{}-{}", std::process::id(), line!()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // An active reviewer-assignment record for the merged generation (55) — this
    // also creates the branch dir so we can poison the markers path.
    store::persist(
        &home,
        &store::ActiveAssignment::new_pending(
            "o/r",
            "b",
            "reviewer",
            55,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
            "review",
            None,
            None,
            "2026-07-13T00:00:00Z",
        ),
    )
    .unwrap();

    // POISON record_terminal deterministically: put a DIRECTORY where the markers
    // file is read/written, so `record_terminal` fails closed (unreadable markers)
    // — an unwritable markers seam, no production code change.
    let mpath = store::markers_path_for_test(&home, "o/r", "b");
    std::fs::create_dir_all(&mpath).unwrap();

    // A pr_state ALREADY merged at generation 55 whose merge was already emitted
    // (`ready_emitted_for_sha == head`), so the scan REPLAY-SUPPRESSES it and
    // returns ScanAction::Remove — the arm that would delete the retry source.
    let mut s = new_for_branch("o/r", "b", "abcdef0", ReviewClass::Single);
    s.pr_number = 55;
    s.merge_state = super::super::MergeState::Merged {
        merge_commit: "abcdef0".into(),
        merged_at: "2026-07-13T00:00:00Z".into(),
    };
    s.ready_emitted_for_sha = Some("abcdef0".into());
    save(&home, &s).unwrap();

    scan_and_emit_with(&home, &empty_registry(), &MockGhPoller::new(vec![]));

    assert!(
        super::super::load(&home, "o/r", "b").is_some(),
        "record_terminal failed ⇒ the pr_state file MUST be retained as the retry \
             source (fail closed); deleting it loses the terminal and resurrects the \
             merged PR's assignment record"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t-…-17 CRASH-SAFETY invariant: the durable terminal MARKER (`record_terminal`)
/// MUST be written BEFORE the pr_state file is `remove`d. Otherwise a crash
/// between the remove and the marker write loses BOTH — on restart CI recreates
/// the file NON-terminal (record_ci_result only applies CiObserved), so the
/// terminal is never re-observed, the marker is never written, and the merged
/// PR's assignment record RESURRECTS and re-reserves forever (B15/B17). The
/// reconciler's A10a cannot backstop a marker that was never written. Source-order
/// pin: prod-sliced (test section excluded) + concat-built needles so it can never
/// self-satisfy — same shape as the #1617 lock-order pins above.
#[test]
fn terminal_marker_recorded_before_pr_state_file_removed() {
    let src = include_str!("../scanner.rs");
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => src,
    };
    let marker_needle = ["record_", "terminal("].concat();
    let remove_needle = ["remove(home, ", "&repo, &branch)"].concat();
    let marker_at = prod
        .find(&marker_needle)
        .expect("assignment_authority::record_terminal call present in prod");
    let remove_at = prod
        .find(&remove_needle)
        .expect("pr_state remove(home, &repo, &branch) call present in prod");
    assert!(
        marker_at < remove_at,
        "the durable terminal marker (record_terminal) MUST be written BEFORE the \
             pr_state file is removed — a crash between them would lose the marker AND \
             the file, resurrecting the merged PR's assignment record (A10a cannot \
             backstop a marker that was never written)"
    );
}

// ─────── t-…-17 B4 (codex m-…-479): scanner defensive revalidation + self-heal ───────
// The scanner emits [pr-ready-for-merge] off the CACHED `merge_state == MergeReady`
// WITHOUT re-running `is_merge_ready`. A late active reservation or a freshly-unreadable
// authority (`authority_unknown`) leaves a stale cached MergeReady → the fail-closed
// merge gate is bypassed. The scanner now DEFENSIVELY re-runs is_merge_ready before the
// freshness delivery; a stale MergeReady is self-healed to NotReady + the event
// suppressed. Emission is asserted via the persisted `ready_emitted_for_sha` flag (the
// #2749 harness convention), self-heal via the persisted `merge_state`.

fn a_reservation() -> super::super::ReservedAssignment {
    super::super::ReservedAssignment {
        target: "reviewer".into(),
        review_author: crate::mcp::handlers::comms_gates::ReviewAuthor::External("octocat".into()),
        assignment_id: uuid::Uuid::new_v4(),
    }
}

/// RED (a): a cached FRESH MergeReady PR that ALSO carries a late ACTIVE reservation.
/// Pre-fix the gate-less scanner sees MergeReady + a Fresh tuple and emits pr-ready
/// (fail-open). The defensive re-check finds `is_merge_ready` false (reserved non-empty),
/// self-heals `merge_state` to NotReady, and emits NOTHING.
#[test]
fn b4_scanner_self_heals_cached_mergeready_with_late_reservation() {
    let home = std::env::temp_dir().join(format!(
        "agend-479-scan-reserved-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    let mut s = merge_ready_state("o/r", "b", head, 81);
    stamp_fresh_tuple(&mut s, head, "beef0001", 0);
    // A late ACTIVE reservation — `is_merge_ready` closes on this (I17), but the cached
    // MergeReady predates it.
    s.reserved_assignments = vec![a_reservation()];
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(81, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let reloaded = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha, None,
        "codex m-…-479: a cached MergeReady with a late reservation must NOT emit \
             [pr-ready-for-merge] (defensive re-check fails closed)"
    );
    assert_eq!(
        reloaded.merge_state,
        MergeState::NotReady,
        "codex m-…-479: the stale cached MergeReady must be SELF-HEALED to NotReady"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED (b): a cached FRESH MergeReady PR whose authority became UNREADABLE
/// (`authority_unknown == true`, the fail-closed flag set by the tri-state probe).
/// Pre-fix the scanner emits pr-ready off the stale cache; the defensive re-check finds
/// `is_merge_ready` false (authority_unknown), self-heals to NotReady, suppresses.
#[test]
fn b4_scanner_self_heals_cached_mergeready_with_authority_unknown() {
    let home = std::env::temp_dir().join(format!(
        "agend-479-scan-authunknown-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    let mut s = merge_ready_state("o/r", "b", head, 82);
    stamp_fresh_tuple(&mut s, head, "beef0001", 0);
    // The authority was UNREADABLE at the last drain ⇒ fail-closed flag set, but the
    // cached MergeReady predates it.
    s.authority_unknown = true;
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(82, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let reloaded = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        reloaded.ready_emitted_for_sha, None,
        "codex m-…-479: a cached MergeReady with authority_unknown must NOT emit \
             [pr-ready-for-merge] (defensive re-check fails closed)"
    );
    assert_eq!(
        reloaded.merge_state,
        MergeState::NotReady,
        "codex m-…-479: the stale cached MergeReady must be SELF-HEALED to NotReady"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED (d): a DIRECTLY-PERSISTED stale MergeReady carrying BOTH a reservation AND
/// authority_unknown ⇒ a real scan self-heals it to NotReady and emits nothing (the
/// self-heal is durably persisted). Repair-convergence via the OTHER modified path:
/// once the reservation/authority is repaired (no active records ⇒ probe Absent), a
/// `redrive_reserved` (the A10b reconciler's per-branch step) clears the flags AND
/// recomputes `merge_state` back to MergeReady — proving the shared
/// `apply_authority_transition` recompute is bidirectional in BOTH callers.
#[test]
fn b4_scanner_self_heal_persists_then_redrive_recomputes_bidirectional() {
    let home = std::env::temp_dir().join(format!(
        "agend-479-scan-heal-redrive-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let head = "abcdef0";
    let mut s = merge_ready_state("o/r", "b", head, 83);
    stamp_fresh_tuple(&mut s, head, "beef0001", 0);
    s.reserved_assignments = vec![a_reservation()];
    s.authority_unknown = true;
    save(&home, &s).unwrap();

    let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(83, "b")])]);
    scan_and_emit_with(&home, &empty_registry(), &poller);

    let healed = load(&home, "o/r", "b").expect("state persists");
    assert_eq!(
        healed.ready_emitted_for_sha, None,
        "codex m-…-479: a stale corrupt-carrying MergeReady must NOT emit pr-ready"
    );
    assert_eq!(
        healed.merge_state,
        MergeState::NotReady,
        "codex m-…-479: the scan must self-heal the stale MergeReady to NotReady (persisted)"
    );

    // Repair-convergence via redrive_reserved: there is NO assignment store for this
    // branch (reserved/authority_unknown were set directly), so the probe reports Absent
    // ⇒ derive lock-free ⇒ reserved emptied, authority_unknown CLEARED, and the recompute
    // restores MergeReady (the state is otherwise ready). This exercises the redrive
    // caller of the shared recompute (the reconciler path), complementing RED (c)'s
    // record_ci_result caller.
    crate::daemon::assignment_authority::redrive_reserved(&home, "o/r", "b");
    let repaired = load(&home, "o/r", "b").expect("state persists");
    assert!(
        repaired.reserved_assignments.is_empty(),
        "redrive with no active records ⇒ reserved cleared"
    );
    assert!(
        !repaired.authority_unknown,
        "redrive with Absent authority ⇒ authority_unknown CLEARED"
    );
    assert_eq!(
        repaired.merge_state,
        MergeState::MergeReady,
        "repair-convergence: redrive's recompute restores MergeReady when readiness returns \
             (bidirectional — not a one-way latch)"
    );
    std::fs::remove_dir_all(&home).ok();
}
