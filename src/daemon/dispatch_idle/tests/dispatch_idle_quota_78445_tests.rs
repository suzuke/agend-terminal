//! #78445-2 PR-B — quota-wedge dispatch_idle tests, homed in a sibling `*_tests.rs`
//! file loaded via `#[path]` from the inline `mod tests` so `dispatch_idle/mod.rs`
//! stays under the anti-monolith LOC ceiling (the `src_file_size_invariant`
//! established split pattern). As a submodule of `mod tests`, `use super::*`
//! inherits both the inline test helpers (`tmp_home` / `write_pending_at` /
//! `write_target_snapshot`) and the production items under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// #78445-2 (c): the quota-wedge message is HONEST — it uses its own subtype tag,
/// names the usage_limit / quota block, and does NOT reuse the long-running "still
/// showing activity" text (which falsely claimed the usage_limit target was
/// active). The long-running (activity) text itself is unchanged. (Supersedes the
/// #t-116 recovery-reescalate test: #78445-2 made the quota latch a durable
/// one-shot, so a recovery no longer clears it to re-escalate — see
/// `quota_wedge_one_shot_persists_across_flicker_78445_2`.)
#[test]
fn quota_wedge_message_is_honest_not_activity_78445_2() {
    let quota = dispatch_idle_text(
        "di-q",
        "lead",
        "dev",
        "task",
        Some("t-q"),
        900,
        600,
        false,
        true,
    );
    assert!(
        quota.contains("[dispatch_idle_quota_wedged]"),
        "quota message uses its own subtype tag: {quota}"
    );
    assert!(
        !quota.contains("still showing activity"),
        "quota message must NOT claim the usage_limit target is active: {quota}"
    );
    assert!(
        quota.contains("usage_limit") || quota.to_lowercase().contains("quota"),
        "quota message names the usage_limit / quota block: {quota}"
    );
    // The long-running (activity) text is untouched — its own distinct wording.
    let lr = dispatch_idle_text(
        "di-l",
        "lead",
        "dev",
        "task",
        Some("t-l"),
        900,
        600,
        true,
        false,
    );
    assert!(
        lr.contains("still showing activity") && lr.contains("[dispatch_idle_long_running]"),
        "long-running text unchanged: {lr}"
    );
}

/// #78445-2 PR-B (b)+(c) RED-first: the quota-wedge escalation is a ONE-SHOT per
/// dispatch that SURVIVES a flicker, and it emits an HONEST quota event (its own
/// tag), not the long-running "still showing activity" one. A brief non-usage_limit
/// snapshot blip must NOT clear the latch and let a re-wedge re-fire — the observed
/// "same heads-up twice, 4 min apart" noise on a quota-wedged reviewer.
#[test]
fn quota_wedge_one_shot_persists_across_flicker_78445_2() {
    let home = tmp_home("78445-quota-flicker");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-qf"), "task", 600, issued);

    // Episode 1 — wedged → escalate once + latch.
    write_target_snapshot(&home, "dev", "usage_limit");
    scan_and_emit(&home);
    assert!(
        list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .unwrap()
            .quota_escalated,
        "wedged → one-shot latch set"
    );

    // FLICKER: one non-usage_limit tick (a snapshot blip, NOT a sustained
    // recovery). "idle" leaves the clock untouched (a working state would refresh
    // issued_at and drop the re-wedge below threshold) and a single scan only bumps
    // the debounce streak (no stuck fire).
    write_target_snapshot(&home, "dev", "idle");
    scan_and_emit(&home);
    assert!(
        list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == id)
            .unwrap()
            .quota_escalated,
        "#78445-2 (b): a flicker must NOT clear the quota one-shot latch"
    );

    // Re-wedge → still latched → NO second escalation.
    write_target_snapshot(&home, "dev", "usage_limit");
    scan_and_emit(&home);
    let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog.matches("dispatch_idle_quota_wedged").count(),
        1,
        "#78445-2: exactly ONE quota escalation across wedge→flicker→re-wedge: {elog}"
    );
    assert_eq!(
        elog.matches("dispatch_idle_long_running").count(),
        0,
        "#78445-2 (c): quota-wedge must NOT borrow the long-running 'still showing \
         activity' event: {elog}"
    );
    std::fs::remove_dir_all(&home).ok();
}
