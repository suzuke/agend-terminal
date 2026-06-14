//! Verification / reproduction tests for confirmed code-review findings in the
//! `ci` MCP handler module (batch: mcp-ci-worktree). Each test encodes the
//! CORRECT expected behavior: it is RED against the current (buggy) code and
//! flips GREEN once the cited bug is fixed. Every repro test is `#[ignore]`d so
//! CI stays green until the fix lands — remove the `#[ignore]` after fixing to
//! confirm.
//!
//! Placement: in-module submodule of `src/mcp/handlers/ci/mod.rs` so the
//! `pub(crate)` handlers (`handle_watch_ci`) and the private
//! `compute_next_poll_eta` are reachable via `super::`, and the crate-internal
//! dispatch surface (`crate::mcp::handlers::dispatch::try_dispatch`) is reachable
//! to drive finding-1 through the same wiring the fix changes.

use crate::mcp::handlers::ci::{compute_next_poll_eta, handle_watch_ci};
use serde_json::json;
use std::path::Path;

// ---------------------------------------------------------------------------
// Shared fixture helpers — mirror the existing `#[cfg(test)] mod tests` in this
// same source file (`watch_path_for` / `read_watch`).
// ---------------------------------------------------------------------------

fn watch_path_for(home: &Path, repo: &str, branch: &str) -> std::path::PathBuf {
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    crate::daemon::ci_watch::ci_watches_dir(home).join(filename)
}

fn read_watch(path: &Path) -> serde_json::Value {
    let s = std::fs::read_to_string(path).expect("watch file must exist");
    serde_json::from_str(&s).expect("watch must be valid JSON")
}

// ===========================================================================
// Finding 1 (HIGH, correctness):
//   "ci unwatch ignores validated caller identity and unsubscribes ALL agents
//    via daemon-env fallback"
//
// `unwatch` is dispatched with the `ha` adapter (dispatch.rs: `"unwatch" =>
// ci::handle_unwatch_ci, ha;`), which passes only (home, args) and DROPS the
// already-validated `ctx.instance_name`. To identify the caller it falls back
// to `std::env::var("AGEND_INSTANCE_NAME")` — wrong process's environment. When
// an agent calls `ci unwatch` with no explicit `instance` arg, `caller` resolves
// EMPTY and the empty-caller branch CLEARS ALL SUBSCRIBERS, unsubscribing every
// OTHER agent — contradicting "Sprint 54 P0-1: only the caller is removed".
//
// We drive through the STABLE dispatch surface `try_dispatch("ci", ctx)` so the
// test survives the fix (which changes the handler signature + rewires `ha` →
// `hai`). `ctx.instance_name = "lead"` is the validated caller. After fix, only
// `lead` is removed and `dev` stays subscribed; the bug removes BOTH.
//
// Method: behavioral_fs (touches the AGEND_INSTANCE_NAME process-global env, so
// `#[serial]`). RED now: the empty-caller fallback wipes `dev` too.
// ===========================================================================
#[test]
#[serial_test::serial]
fn unwatch_uses_validated_caller_not_clear_all_mcp_ci_worktree() {
    // The bug surfaces only when no `instance` arg is given AND the daemon's
    // env lacks AGEND_INSTANCE_NAME. Force that precondition deterministically.
    std::env::remove_var("AGEND_INSTANCE_NAME");

    let home = std::env::temp_dir().join(format!(
        "agend-unwatch-validated-caller-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();

    // Two distinct subscribers on the same branch.
    let watch_args = json!({"repository": "owner/repo", "branch": "feat-unwatch-caller"});
    handle_watch_ci(&home, &watch_args, "lead");
    handle_watch_ci(&home, &watch_args, "dev");

    let path = watch_path_for(&home, "owner/repo", "feat-unwatch-caller");
    let before = crate::daemon::ci_watch::parse_subscribers(&read_watch(&path));
    assert!(
        before.iter().any(|s| s == "lead") && before.iter().any(|s| s == "dev"),
        "precondition: both lead and dev subscribed, got {before:?}"
    );

    // `lead` calls `ci unwatch` WITHOUT an explicit `instance` arg. The
    // validated caller identity rides on ctx.instance_name = "lead".
    let unwatch_args = json!({
        "action": "unwatch",
        "repository": "owner/repo",
        "branch": "feat-unwatch-caller",
    });
    let no_sender: Option<crate::identity::Sender> = None;
    let ctx = crate::mcp::handlers::dispatch::HandlerCtx {
        home: &home,
        args: &unwatch_args,
        instance_name: "lead",
        sender: &no_sender,
    };
    let resp = crate::mcp::handlers::dispatch::try_dispatch("ci", &ctx)
        .expect("ci tool must be registered in the dispatch table");
    assert!(
        resp.get("error").is_none(),
        "unwatch must not error: {resp:?}"
    );

    // CORRECT behavior: only the caller (`lead`) is unsubscribed; `dev` — who
    // never asked to unwatch — must REMAIN subscribed. The bug's empty-caller
    // `subscribers.clear()` path removes `dev` too (and tombstones the watch).
    let after = crate::daemon::ci_watch::parse_subscribers(&read_watch(&path));
    assert!(
        after.iter().any(|s| s == "dev"),
        "dev must remain subscribed after lead unwatches; the daemon-env empty-caller \
         fallback wrongly cleared ALL subscribers. subscribers after = {after:?}"
    );
    assert!(
        !after.iter().any(|s| s == "lead"),
        "lead (the caller) should have been removed; subscribers after = {after:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ===========================================================================
// Finding 4 (LOW, error-handling):
//   "compute_next_poll_eta can integer-overflow on attacker/buggy interval_secs"
//
// `Some(last_polled_at + (interval_secs as i64) * 1000)` overflows when
// `interval_secs` is large but still positive as i64 (e.g. 99_999_999_999_999_999
// → *1000 exceeds i64::MAX). In debug builds (default for `cargo test`) this
// PANICS ("attempt to multiply with overflow"); in release it wraps to a
// nonsensical (possibly negative) eta.
//
// Method: behavioral_unit (panic vector). Wrap the call in catch_unwind and
// assert it returns Ok. RED now: the multiply panics → catch_unwind == Err.
// GREEN after the fix uses clamped + saturating math.
// ===========================================================================
#[test]
#[ignore = "mcp-ci-worktree-4: compute-next-poll-eta-overflow; red until fix; remove #[ignore] after fix to confirm"]
fn compute_next_poll_eta_does_not_overflow_on_huge_interval_mcp_ci_worktree() {
    // last_polled_at present (so the function gets past its early `?`), and an
    // interval_secs that is positive-as-i64 yet overflows when multiplied by
    // 1000 — the exact value class called out in the finding.
    let watch = json!({
        "last_polled_at": 1_700_000_000_000_i64,
        "interval_secs": 99_999_999_999_999_999_u64,
    });

    let result = std::panic::catch_unwind(|| compute_next_poll_eta(&watch));
    assert!(
        result.is_ok(),
        "compute_next_poll_eta panicked (arithmetic overflow) on a huge interval_secs; \
         it must clamp + use saturating math instead"
    );
    // After the fix the eta should be a sane, non-overflowing value. We only
    // assert it is Some(_) (a value was produced) — the exact clamp bound is a
    // fix-detail, not pinned here.
    let eta = result.expect("no panic asserted above");
    assert!(
        eta.is_some(),
        "with last_polled_at present, an eta should be computed (got None)"
    );
}

// ===========================================================================
// Finding 5 (LOW, correctness):
//   "Stale next_after_ci handoff target persists across re-watch unless
//    explicitly overwritten"
//
// `handle_watch_ci` only SETS `next_after_ci` when the caller passes a NON-empty
// `next_after_ci` arg; it never CLEARS a previously-stored value. So a watch
// armed earlier with `next_after_ci=reviewer-A` retains that handoff target even
// when a later `ci action=watch` passes an explicitly-EMPTY `next_after_ci`
// intending to re-arm with no chaining. The actionable `[ci-ready-for-action]`
// is then still routed to the stale target.
//
// Method: behavioral_fs. Watch with next_after_ci=reviewer-A, then re-watch with
// an EXPLICIT empty `next_after_ci:""`, then read the persisted watch file. The
// fix treats explicit-empty as a request to clear the field. RED now: the empty
// arg is filtered out (`filter(|s| !s.is_empty())`) so the stale value persists.
// ===========================================================================
#[test]
#[ignore = "mcp-ci-worktree-5: stale-next-after-ci-persists-across-rewatch; red until fix; remove #[ignore] after fix to confirm"]
fn explicit_empty_next_after_ci_clears_stale_handoff_target_mcp_ci_worktree() {
    let home = std::env::temp_dir().join(format!(
        "agend-rewatch-clear-handoff-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).ok();

    // Arm the watch with a handoff target.
    handle_watch_ci(
        &home,
        &json!({
            "repository": "owner/repo",
            "branch": "feat-handoff",
            "next_after_ci": "reviewer-A",
        }),
        "lead",
    );

    let path = watch_path_for(&home, "owner/repo", "feat-handoff");
    assert_eq!(
        read_watch(&path)["next_after_ci"].as_str(),
        Some("reviewer-A"),
        "precondition: initial watch stored the handoff target"
    );

    // Re-watch with an EXPLICIT empty next_after_ci — the operator/agent intent
    // is "re-arm with NO chaining". The field must be cleared, not carried over.
    handle_watch_ci(
        &home,
        &json!({
            "repository": "owner/repo",
            "branch": "feat-handoff",
            "next_after_ci": "",
        }),
        "lead",
    );

    let after = read_watch(&path);
    let stale = after["next_after_ci"].as_str().unwrap_or("");
    assert!(
        stale.is_empty(),
        "explicit empty next_after_ci must clear the stale handoff target; \
         it still routes [ci-ready-for-action] to {stale:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}
