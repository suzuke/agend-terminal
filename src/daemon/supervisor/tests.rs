use super::*;

/// #2033: the recovery-notice gate — actionable iff the operator was told
/// about the block AND it lasted past the threshold (actionable-or-silent).
#[test]
fn recovery_notice_gate_actionable_or_silent_2033() {
    use crate::state::RecoveryEpisode;
    let ep = |secs, notice_sent| RecoveryEpisode {
        block_duration: Duration::from_secs(secs),
        notice_sent,
    };
    // notified + long enough → fire
    assert!(recovery_notice_is_actionable(ep(60, true)));
    // notified but self-resolved fast → silent (the InteractivePrompt noise)
    assert!(!recovery_notice_is_actionable(ep(5, true)));
    // long but NEVER notified → silent (the #2020 false-AwaitingOperator class)
    assert!(!recovery_notice_is_actionable(ep(300, false)));
    // neither → silent
    assert!(!recovery_notice_is_actionable(ep(2, false)));
    // boundary: exactly the threshold is actionable (>=)
    assert!(recovery_notice_is_actionable(RecoveryEpisode {
        block_duration: RECOVERY_NOTICE_MIN_BLOCK,
        notice_sent: true,
    }));
}

// NOTE: `recovery_clears_retry_track` (+ its `fresh_retry` helper) was removed
// here — it only asserted `HashMap` insert/remove semantics on a local map and
// never exercised the production recovery path. The REAL recovery gate that
// clears the retry track, `clears_server_rate_limit_retry`, is already covered
// with real inputs by `clears_server_rate_limit_retry_covers_only_terminal_
// recovery_1713` (Idle clears; every other state does not). The other clear
// path (`ServerRateLimit && recovered` via productive-output) has no pure seam
// without restructuring the registry-locked `process_error_recovery` hot loop,
// which would not be a behavior-preserving extraction.

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-supervisor-test-{}-{}-{}",
        std::process::id(),
        tag,
        id,
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
fn waiting_on_cleared_when_heartbeat_stale() {
    let home = tmp_home("stale_decay");
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta = serde_json::json!({
        "waiting_on": "review from at-dev-4",
        "waiting_on_since": "2026-04-22T10:00:00Z",
        "last_heartbeat": "2026-04-22T09:00:00Z",
    });
    std::fs::write(
        meta_dir.join("agent1.json"),
        serde_json::to_string_pretty(&meta).expect("serialize"),
    )
    .ok();

    // Stale → must clear
    clear_waiting_on_if_stale(&home, "agent1", true);

    let content = std::fs::read_to_string(meta_dir.join("agent1.json")).expect("read after clear");
    let result: serde_json::Value = serde_json::from_str(&content).expect("parse");
    assert!(
        result["waiting_on"].is_null(),
        "waiting_on must be null after stale decay"
    );
    assert!(
        result["waiting_on_since"].is_null(),
        "waiting_on_since must be null after stale decay"
    );

    // Fresh → must NOT clear
    let meta2 = serde_json::json!({
        "waiting_on": "still waiting",
        "waiting_on_since": "2026-04-22T10:00:00Z",
    });
    std::fs::write(
        meta_dir.join("agent2.json"),
        serde_json::to_string_pretty(&meta2).expect("serialize"),
    )
    .ok();
    clear_waiting_on_if_stale(&home, "agent2", false);
    let content2 = std::fs::read_to_string(meta_dir.join("agent2.json")).expect("read agent2");
    let result2: serde_json::Value = serde_json::from_str(&content2).expect("parse");
    assert_eq!(
        result2["waiting_on"], "still waiting",
        "fresh heartbeat must NOT clear waiting_on"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Sprint 22 P2a F7 regression — both `waiting_on` and `waiting_on_since`
/// must land in a single atomic disk write so a crash mid-clear cannot
/// leave divergent state (waiting_on=null + waiting_on_since=set, which
/// `set_waiting_on` freshness logic interprets on restart as "agent is
/// currently waiting" without a `waiting_on` value).
///
/// The pre-fix code had two sequential `save_metadata` calls; this test
/// pins the contract that the call site delegates to
/// `agent_ops::save_metadata_batch` (single read-modify-write cycle).
/// Source-grep verifies the two-call regression cannot reappear:
/// `clear_waiting_on_if_stale` must contain `save_metadata_batch` and
/// must NOT contain two adjacent `save_metadata(` calls.
#[test]
fn waiting_on_clear_uses_atomic_batch_write() {
    // Source-grep guard: pin that the impl uses save_metadata_batch
    // (closes F7 race window). Future regression to two-call form
    // would fail-loud here.
    let src = include_str!("../supervisor.rs");
    let body_start = src
        .find("fn clear_waiting_on_if_stale(")
        .expect("clear_waiting_on_if_stale must exist");
    // Bound the search to the function body (next top-level fn).
    let rest = &src[body_start..];
    let body_end = rest
        .find("\nfn ")
        .or_else(|| rest.find("\npub fn "))
        .or_else(|| rest.find("\n#[cfg(test)]"))
        .unwrap_or(rest.len());
    let body = &rest[..body_end];

    assert!(
        body.contains("save_metadata_batch("),
        "clear_waiting_on_if_stale must use `save_metadata_batch` for atomic \
             multi-field write (Sprint 22 P2a F7 fix). Found body:\n{body}"
    );
    // Sanity check: the legacy two-call pattern must NOT reappear.
    // We check that the body contains at most ONE `save_metadata(`
    // substring — `save_metadata_batch(` matches separately because
    // we look for the open paren after `metadata` not `metadata_batch`.
    let single_calls = body.matches("save_metadata(").count();
    assert!(
        single_calls == 0,
        "clear_waiting_on_if_stale must NOT call individual `save_metadata` \
             — F7 race fix requires `save_metadata_batch` (single atomic write). \
             Found {single_calls} `save_metadata(` call(s) in body:\n{body}"
    );
}

/// Sprint 22 P2a F7 behavioural regression — verify the atomic batch
/// write produces the expected on-disk state when both fields land
/// together. Pairs with the source-grep guard above; this test catches
/// a regression where the helper signature changes but the call site
/// still compiles.
#[test]
fn waiting_on_clear_writes_both_nulls_atomically() {
    let home = tmp_home("f7_atomic");
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    // Pre-populate with active wait state + an unrelated field that
    // must survive the batch write (read-modify-write contract).
    let meta = serde_json::json!({
        "waiting_on": "review from at-dev-4",
        "waiting_on_since": "2026-04-27T05:00:00Z",
        "last_heartbeat": "2026-04-27T04:55:00Z",
        "role": "dev-impl-2",
    });
    std::fs::write(
        meta_dir.join("agent_atomic.json"),
        serde_json::to_string_pretty(&meta).expect("serialize"),
    )
    .ok();

    clear_waiting_on_if_stale(&home, "agent_atomic", true);

    let raw =
        std::fs::read_to_string(meta_dir.join("agent_atomic.json")).expect("metadata file present");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert!(
        v["waiting_on"].is_null(),
        "waiting_on must be null after F7 atomic clear"
    );
    assert!(
        v["waiting_on_since"].is_null(),
        "waiting_on_since must be null after F7 atomic clear (paired with waiting_on)"
    );
    assert_eq!(
        v["last_heartbeat"], "2026-04-27T04:55:00Z",
        "unrelated `last_heartbeat` must survive the batch write"
    );
    assert_eq!(
        v["role"], "dev-impl-2",
        "unrelated `role` must survive the batch write"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 43: member-state-change notify tests ──────────────────

/// is_notify_error_class matches exactly the GO-NARROW 6 states.
#[test]
fn is_notify_error_class_matches_go_narrow_6() {
    use crate::state::AgentState;
    assert!(AgentState::UsageLimit.is_notify_error_class());
    assert!(AgentState::RateLimit.is_notify_error_class());
    assert!(AgentState::Hang.is_notify_error_class());
    assert!(AgentState::Crashed.is_notify_error_class());
    assert!(AgentState::AuthError.is_notify_error_class());
    assert!(AgentState::PermissionPrompt.is_notify_error_class());
    assert!(!AgentState::ContextFull.is_notify_error_class());
    assert!(!AgentState::AwaitingOperator.is_notify_error_class());
    assert!(!AgentState::ApiError.is_notify_error_class());
    assert!(!AgentState::Restarting.is_notify_error_class());
    assert!(!AgentState::InteractivePrompt.is_notify_error_class());
    assert!(!AgentState::Idle.is_notify_error_class());
    assert!(!AgentState::Idle.is_notify_error_class());
    assert!(!AgentState::Active.is_notify_error_class());
    assert!(!AgentState::Starting.is_notify_error_class());
}

// ── #1530: feed-driven UsageLimit / member-state reaction de-gate ──

fn tr(
    from: crate::state::AgentState,
    to: crate::state::AgentState,
) -> crate::state::TransitionRecord {
    crate::state::TransitionRecord {
        from,
        to,
        ts: "2026-05-31T00:00:00+00:00".to_string(),
    }
}

/// RED ①: a feed-driven `Idle → UsageLimit` (the read-loop records it, so
/// the drain carries it even though `prev == new` at the supervisor tick)
/// MUST still produce a reaction decision. Pre-#1530 the `prev != new` gate
/// skipped it → the UsageLimit reaction was dead since #1176.
#[test]
fn reactions_from_transitions_fires_on_feed_driven_usagelimit() {
    use crate::state::AgentState;
    let decisions = reactions_from_transitions(&[tr(AgentState::Idle, AgentState::UsageLimit)]);
    assert_eq!(
        decisions,
        vec![ReactionDecision {
            from: AgentState::Idle,
            to: AgentState::UsageLimit
        }],
        "feed-driven →UsageLimit must yield a reaction decision (de-gated off prev!=new)"
    );
}

/// RED ②: an intra-tick flap (`Idle → UsageLimit → Idle`) has no NET state
/// change → no reaction. Avoids double/noise notifications. (Logging still
/// records every transition via #1527 — that path is independent.)
#[test]
fn reactions_from_transitions_converges_on_net_state_no_flap_double_fire() {
    use crate::state::AgentState;
    let decisions = reactions_from_transitions(&[
        tr(AgentState::Idle, AgentState::UsageLimit),
        tr(AgentState::UsageLimit, AgentState::Idle),
    ]);
    assert!(
        decisions.is_empty(),
        "flap in-and-out (net Idle→Idle) must not fire a reaction, got {decisions:?}"
    );
}

/// Net change to a non-error state, and the empty drain, both yield nothing.
#[test]
fn reactions_from_transitions_ignores_non_error_and_empty() {
    use crate::state::AgentState;
    assert!(
        reactions_from_transitions(&[]).is_empty(),
        "empty drain → no reaction"
    );
    assert!(
        reactions_from_transitions(&[tr(AgentState::Idle, AgentState::Active)]).is_empty(),
        "net change to a non-error state → no reaction"
    );
}

/// Net change THROUGH a flap into a different error state reacts on the
/// final state: `UsageLimit → Idle → Hang` ⇒ react on Hang, not UsageLimit.
#[test]
fn reactions_from_transitions_reacts_on_final_error_state() {
    use crate::state::AgentState;
    let decisions = reactions_from_transitions(&[
        tr(AgentState::UsageLimit, AgentState::Idle),
        tr(AgentState::Idle, AgentState::Hang),
    ]);
    assert_eq!(
        decisions,
        vec![ReactionDecision {
            from: AgentState::UsageLimit,
            to: AgentState::Hang
        }],
        "net from = first.from, net to = last.to (final state)"
    );
}

/// RED ③: a UsageLimit final state drives BOTH the operator/propagate path
/// AND member-notify — the latter was silently eaten by the pre-#1530
/// propagate `continue`. A non-UsageLimit error state drives member-notify
/// only; a non-error state drives nothing.
#[test]
fn reaction_kinds_usagelimit_does_not_drop_member_notify() {
    use crate::state::AgentState;
    assert_eq!(
        reaction_kinds(AgentState::UsageLimit, true),
        vec![
            ReactionKind::NotifyOperator,
            ReactionKind::Propagate,
            ReactionKind::MemberNotify
        ],
        "UsageLimit + propagation: all three reactions fire (member-notify NOT eaten)"
    );
    assert_eq!(
        reaction_kinds(AgentState::UsageLimit, false),
        vec![ReactionKind::NotifyOperator, ReactionKind::MemberNotify],
        "UsageLimit without propagation: operator notice + member-notify"
    );
    assert_eq!(
        reaction_kinds(AgentState::Hang, true),
        vec![ReactionKind::MemberNotify],
        "non-UsageLimit error state: member-notify only"
    );
    assert!(
        reaction_kinds(AgentState::Idle, true).is_empty(),
        "non-error state: no reaction"
    );
}

// ── #1552: runtime AwaitingOperator escalation FP-gates ──

/// ClaudeCode permission chrome footer — the self-identifying anchor #1546
/// installed; `StatePatterns` detects it as `PermissionPrompt`.
const PERM_CHROME: &str = "Do you want to proceed?\nEsc to cancel · Tab to amend";

#[test]
fn awaiting_gate_starting_is_ungated() {
    // Legacy startup-stall path: fires regardless of chrome/position for an
    // `Active` worker (the default).
    assert!(awaiting_escalation_allowed(
        crate::state::AgentState::Starting,
        Duration::from_secs(0),
        None,
        "no chrome here",
        0,
        0,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_starting_ondemand_suppressed() {
    // #1563: a stuck-`Starting` `OnDemand` coordinator (e.g. `general`) must
    // NOT forward its startup-stall pane to the operator.
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::Starting,
        Duration::from_secs(0),
        None,
        "no chrome here",
        0,
        0,
        crate::fleet::IdleExpectation::OnDemand,
        false,
    ));
}

#[test]
fn awaiting_gate_runtime_permission_all_gates_pass() {
    assert!(awaiting_escalation_allowed(
        crate::state::AgentState::PermissionPrompt,
        AWAITING_STABILITY, // held long enough
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME, // chrome IS in the live tail
        0,           // operator never typed
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_ondemand_real_permission_still_escalates() {
    // #1563 preserves #1552: the role gate covers ONLY the `Starting`
    // startup-stall arm. A genuine runtime permission prompt that satisfies
    // all three FP-gates STILL escalates for an `OnDemand` agent — otherwise
    // a coordinator stuck on a real permission dialog would never be surfaced.
    assert!(awaiting_escalation_allowed(
        crate::state::AgentState::PermissionPrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME,
        0,
        10_000,
        crate::fleet::IdleExpectation::OnDemand,
        false,
    ));
}

// ── #1563 part-B: InteractivePrompt role gate ──
// NB: `StatePatterns::detect` has NO `InteractivePrompt` regex (that state
// only comes from the weak `is_generic_startup_prompt` at the StateTracker
// level), so the position gate (a) for an `InteractivePrompt`-STATE agent is
// satisfiable only by a tail that detects as `PermissionPrompt`. `PERM_CHROME`
// models exactly the real FP combo: an agent latched to `InteractivePrompt`
// whose live tail also shows prompt chrome.

#[test]
fn awaiting_gate_ondemand_interactive_prompt_suppressed() {
    // #1563 part-B: `general` (OnDemand) latched to `InteractivePrompt` by a
    // `(y/n)` in its PR-review prose must NOT escalate, even with all three
    // #1564 gates satisfied — the InteractivePrompt source is prose-FP-prone.
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::InteractivePrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME,
        0,
        10_000,
        crate::fleet::IdleExpectation::OnDemand,
        false,
    ));
}

#[test]
fn awaiting_gate_active_interactive_prompt_still_escalates() {
    // An `Active` worker's InteractivePrompt still escalates (gates pass) —
    // the role gate only suppresses OnDemand.
    assert!(awaiting_escalation_allowed(
        crate::state::AgentState::InteractivePrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME,
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_interactive_prompt_1564_gates_still_apply_when_active() {
    // The new role gate is ADDITIVE: an `Active` InteractivePrompt with the
    // chrome NOT in the live tail still fails the position gate (a).
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::InteractivePrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        "no chrome in the live tail",
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn idle_expectation_for_resolves_role_and_defaults() {
    let home = tmp_home("idle_exp_resolve");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        r#"
defaults:
  backend: claude
instances:
  worker:
    role: worker
  general:
    role: General assistant
    idle_expectation: on-demand
"#,
    )
    .expect("write fleet.yaml");
    // The shared resolver both branch-1 (startup-stall) and branch-2
    // (startup-prose forward) gate on. `on-demand` → OnDemand suppresses
    // BOTH forwards; omitted → Active leaves the worker unchanged; an
    // unknown agent fails open to Active (never silently suppress).
    assert_eq!(
        idle_expectation_for(&home, "general"),
        crate::fleet::IdleExpectation::OnDemand
    );
    assert_eq!(
        idle_expectation_for(&home, "worker"),
        crate::fleet::IdleExpectation::Active
    );
    assert_eq!(
        idle_expectation_for(&home, "nonexistent"),
        crate::fleet::IdleExpectation::Active
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn awaiting_gate_blocks_scrollback_footer_fp() {
    // The meta-FP: state is PermissionPrompt (full-screen detection saw the
    // chrome), held + no typing — but the chrome is NOT in the live bottom
    // tail (it scrolled up / is a working agent's echo). Position gate (a)
    // must block escalation. This is the dev-2 live case.
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::PermissionPrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        "just normal working output, no dialog chrome at the bottom",
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

/// #2020 live shape 2 (fixup-lead, 2026-06-11 20:09): a respawned agent
/// that was injected work immediately never renders the clean
/// ready-prompt — heuristic stays `Starting` — but it HAS rendered
/// productive markers. The startup-stall arm must veto: demonstrably
/// working ≠ stalled at a login prompt.
#[test]
fn starting_stall_vetoed_by_productive_output_2020() {
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::Starting,
        Duration::from_secs(120),
        Some(crate::backend::Backend::ClaudeCode),
        "tail irrelevant for the Starting arm",
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        true, // productive markers seen since this spawn
    ));
}

/// #2020 guard on the guard: with NO productive output since spawn the
/// startup-stall fallback must still fire — a real login-prompt stall
/// (the fallback's actual job) renders no tool chrome, and echoed
/// injected text doesn't count (markers, not raw output).
#[test]
fn starting_stall_still_fires_without_productive_output_2020() {
    assert!(awaiting_escalation_allowed(
        crate::state::AgentState::Starting,
        Duration::from_secs(120),
        Some(crate::backend::Backend::ClaudeCode),
        "Please log in to continue",
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_blocks_when_not_stable() {
    // (b) stability: prompt state held < AWAITING_STABILITY → no escalate.
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::PermissionPrompt,
        Duration::from_secs(1),
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME,
        0,
        10_000,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_blocks_when_operator_typing() {
    // (c) engagement: operator typed 2s ago (< 15s window) → suppress.
    let now = 100_000i64;
    assert!(!awaiting_escalation_allowed(
        crate::state::AgentState::PermissionPrompt,
        AWAITING_STABILITY,
        Some(crate::backend::Backend::ClaudeCode),
        PERM_CHROME,
        now - 2_000,
        now,
        crate::fleet::IdleExpectation::Active,
        false,
    ));
}

#[test]
fn awaiting_gate_non_prompt_state_never_escalates() {
    for s in [
        crate::state::AgentState::Idle,
        crate::state::AgentState::Active,
    ] {
        assert!(
            !awaiting_escalation_allowed(
                s,
                AWAITING_STABILITY,
                Some(crate::backend::Backend::ClaudeCode),
                PERM_CHROME,
                0,
                10_000,
                crate::fleet::IdleExpectation::Active,
                false,
            ),
            "{s:?} must never escalate via this path"
        );
    }
}

/// NOTIFY_COOLDOWN constant is 60 seconds.
#[test]
fn notify_cooldown_is_60_seconds() {
    assert_eq!(super::NOTIFY_COOLDOWN, std::time::Duration::from_secs(60));
}

/// #1530/F2 (lockaudit): the per-agent tick must NOT re-acquire the registry
/// while holding an agent core (the core→registry inversion that risked an
/// AB-BA deadlock with the registry→core render/monitor loops). The backend
/// is pre-captured in the handles snapshot (registry→core order) and resolved
/// lock-free; the old nested per-agent registry lookups under the core lock
/// are gone. Source-grep pin (mirrors #1146); scoped to the `tick` fn body so
/// it never matches its own assertion text.
#[test]
fn tick_does_not_reacquire_registry_under_core_f2() {
    let src = include_str!("../supervisor.rs");
    let start = src
        .find("\nfn tick(")
        .expect("supervisor tick fn must exist");
    // The per-agent loop lives well within the first 18 KB of the fn; the
    // test module is far past that, so this window excludes this test.
    let body = &src[start..(start + 18_000).min(src.len())];
    // The removed nested lookup keyed the registry by the per-agent id.
    let needle = ["reg.get(&", "instance_id)"].concat();
    assert!(
        !body.contains(&needle),
        "#1530/F2: the tick per-agent loop must not re-look-up the registry by \
             agent id while holding the core — the backend is pre-captured in the \
             handles snapshot (registry→core)"
    );
    assert!(
        body.contains("backend_command"),
        "#1530/F2: tick must pre-capture each agent's backend_command in the \
             handles snapshot and resolve Backend lock-free"
    );
}

/// #1644: CI-time pin of the collect→drop→emit boundary in `tick`. The
/// self-IPC / blocking emitters (member-notify `api::call(INJECT)`, the
/// usage-limit propagate, the Telegram `gated_notify`) must run AFTER the
/// per-agent `let action = { … core.lock() … }` block drops the core lock —
/// never inside it (a core-held self-IPC is the #1492/#1535 deadlock class).
/// The runtime guard (`CORE_LOCK_DEPTH` + `assert_no_registry_lock_for_self_ipc`,
/// #1535) already fail-fasts a violation; this source-grep catches it earlier,
/// at CI. It is the cheap structural slice of the deferred
/// `supervise_one()->TickOutcome` extraction (#1644). Brace-matches the
/// lock block and scopes to the `tick` fn body so it never matches itself.
#[test]
fn tick_emitters_run_after_core_lock_drops_1644() {
    let src = include_str!("../supervisor.rs");
    let tick_start = src.find("\nfn tick(").expect("tick fn must exist");
    let after = &src[tick_start..];
    // End the slice at the next top-level `fn ` so the test module (and its
    // needle literals) are excluded.
    let tick_end = after[1..]
        .find("\nfn ")
        .map(|i| i + 1)
        .unwrap_or(after.len());
    let tick = &after[..tick_end];

    // Brace-match the per-agent core-lock block `let action … = { … };`.
    let anchor = ["let action", ": Option<NoticeAction> = {"].concat();
    let astart = tick.find(&anchor).expect("tick core-lock block present");
    let open = astart + tick[astart..].find('{').expect("block opens");
    let mut depth = 0usize;
    let mut close = open;
    for (i, c) in tick[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(close > open, "core-lock block must close");
    let in_block = &tick[open..=close];
    let after_block = &tick[close..];

    for emitter in [
        ["maybe_notify", "_member_state_change("].concat(),
        ["gated", "_notify("].concat(),
        ["propagate", "_usage_limit("].concat(),
    ] {
        assert!(
            !in_block.contains(&emitter),
            "#1644: `{emitter}` is a self-IPC/blocking emitter and must NOT run inside the \
                 core-lock block (collect→drop→emit; #1492/#1535 deadlock class)"
        );
        assert!(
            after_block.contains(&emitter),
            "#1644: `{emitter}` must run AFTER the core lock drops"
        );
    }
}

// ── #1523: AuthError content-FP stability gate ──────────────────────

/// The stability window must exceed the observed self-heal time (~31s) by a
/// safe margin so a transient AuthError can never reach the alert.
#[test]
fn auth_error_notify_stability_exceeds_observed_self_heal() {
    assert!(
        super::AUTH_ERROR_NOTIFY_STABILITY >= std::time::Duration::from_secs(60),
        "stability window must be well above the observed 31s self-heal"
    );
}

/// Transient (self-healed): on a later tick the state is no longer AuthError
/// → `None` → Cancel → NO alert. This is the FP that #1523 fixes.
#[test]
fn auth_error_gate_cancels_when_state_left() {
    assert_eq!(super::auth_error_gate(None), super::AuthErrorGate::Cancel);
}

/// Still in AuthError but inside the window (e.g. the 31s blip before it
/// heals) → Wait → no alert yet.
#[test]
fn auth_error_gate_waits_within_window() {
    let held = super::AUTH_ERROR_NOTIFY_STABILITY - std::time::Duration::from_secs(1);
    assert_eq!(
        super::auth_error_gate(Some(held)),
        super::AuthErrorGate::Wait
    );
    // The observed self-heal point (31s) is firmly in the Wait band.
    assert_eq!(
        super::auth_error_gate(Some(std::time::Duration::from_secs(31))),
        super::AuthErrorGate::Wait
    );
}

/// Sustained (real auth failure): held ≥ window → Fire → alert sent.
#[test]
fn auth_error_gate_fires_when_held_past_window() {
    assert_eq!(
        super::auth_error_gate(Some(super::AUTH_ERROR_NOTIFY_STABILITY)),
        super::AuthErrorGate::Fire
    );
    let well_past = super::AUTH_ERROR_NOTIFY_STABILITY + std::time::Duration::from_secs(120);
    assert_eq!(
        super::auth_error_gate(Some(well_past)),
        super::AuthErrorGate::Fire
    );
}

/// D4: 2×2 invariant fixture — production-path-coupled.
/// 2 teams (team-a: orch-a + worker-a, team-b: orch-b + worker-b).
/// worker-a transitions Idle → UsageLimit.
/// Assert: orch-a inbox has 1 event; orch-b/worker-a/worker-b have 0.
#[test]
fn notify_single_receiver_2x2_invariant() {
    let home = std::env::temp_dir().join(format!("agend-notify-2x2-{}", std::process::id()));
    std::fs::create_dir_all(home.join("inbox")).ok();

    // Setup teams via teams API (correct store format).
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "team-a", "members": ["orch-a", "worker-a"], "orchestrator": "orch-a"}),
    );
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "team-b", "members": ["orch-b", "worker-b"], "orchestrator": "orch-b"}),
    );

    // Call production function (§3.5.10 production-path-coupled).
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "worker-a",
        crate::state::AgentState::Idle,
        crate::state::AgentState::UsageLimit,
        "Usage limit reached. Resets at 15:14 UTC",
        &mut tracks,
    );
    assert!(sent, "notify must be sent");

    // Assert: orch-a has 1 event (JSONL file).
    let orch_a_inbox = home.join("inbox").join("orch-a.jsonl");
    let orch_a_count = std::fs::read_to_string(&orch_a_inbox)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(orch_a_count, 1, "orch-a must have exactly 1 event");

    // Assert: others have 0.
    for other in &["orch-b", "worker-a", "worker-b", "general"] {
        let inbox = home.join("inbox").join(format!("{other}.jsonl"));
        let count = std::fs::read_to_string(&inbox)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .count();
        assert_eq!(count, 0, "{other} must have 0 events");
    }

    std::fs::remove_dir_all(&home).ok();
}

/// D3: skip self-notify — orchestrator hits UsageLimit → 0 events.
#[test]
fn notify_skip_self_when_member_is_orchestrator() {
    let home = std::env::temp_dir().join(format!("agend-notify-self-{}", std::process::id()));
    std::fs::create_dir_all(home.join("inbox")).ok();
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "team-a", "members": ["orch-a"], "orchestrator": "orch-a"}),
    );

    // Call production function — should return false (self-notify skip).
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "orch-a",
        crate::state::AgentState::Idle,
        crate::state::AgentState::UsageLimit,
        "",
        &mut tracks,
    );
    assert!(!sent, "self-notify must be skipped");
    let content =
        std::fs::read_to_string(home.join("inbox").join("orch-a.jsonl")).unwrap_or_default();
    assert_eq!(
        content.lines().filter(|l| !l.is_empty()).count(),
        0,
        "orch-a=0"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #1595 Step 2 (pure): only AuthError escalates a self-orchestrator.
#[test]
fn self_orchestrator_escalates_only_on_autherror_1595() {
    use crate::state::AgentState;
    assert!(super::self_orchestrator_escalates(AgentState::AuthError));
    for s in [
        AgentState::UsageLimit,
        AgentState::RateLimit,
        AgentState::Hang,
        AgentState::Crashed,
        AgentState::PermissionPrompt,
        AgentState::Idle,
        AgentState::Idle,
    ] {
        assert!(
            !super::self_orchestrator_escalates(s),
            "{s:?} must NOT escalate (only AuthError is terminal + operator-only)"
        );
    }
}

/// #1595 Step 2: a self-orchestrator (orch==name) hitting AuthError escalates
/// (Telegram path + cooldown-track stamp) but still skips the inbox self-notify;
/// a non-terminal state stays a plain drop (no stamp). Telegram is a no-op here
/// (no active channel in tests) — the cooldown-track stamp is the observable
/// signal that the escalation branch ran. Cooldown prevents re-escalation.
#[test]
fn self_orchestrator_autherror_escalates_others_drop_1595() {
    let home = std::env::temp_dir().join(format!("agend-1595-selforch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("inbox")).ok();
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "t", "members": ["solo"], "orchestrator": "solo"}),
    );

    // Non-terminal (RateLimit) self-orchestrator → plain drop, no escalation.
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "solo",
        crate::state::AgentState::Idle,
        crate::state::AgentState::RateLimit,
        "",
        &mut tracks,
    );
    assert!(!sent, "self-notify skipped");
    assert!(
        !tracks.contains_key("solo"),
        "#1595: a non-AuthError self-orchestrator must NOT escalate (no track stamp)"
    );

    // AuthError self-orchestrator → escalation branch runs (stamps cooldown
    // track), still returns false (the escalation is Telegram, not inbox).
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "solo",
        crate::state::AgentState::Idle,
        crate::state::AgentState::AuthError,
        "",
        &mut tracks,
    );
    assert!(!sent, "inbox self-notify still skipped");
    let t = tracks
        .get("solo")
        .expect("#1595: AuthError self-orchestrator must escalate → cooldown track stamped");
    assert_eq!(t.consecutive, 1, "escalation counted once");
    let inbox = std::fs::read_to_string(home.join("inbox").join("solo.jsonl")).unwrap_or_default();
    assert_eq!(
        inbox.lines().filter(|l| !l.is_empty()).count(),
        0,
        "escalation is Telegram, not an inbox self-notify"
    );

    // Cooldown: a second immediate AuthError must NOT re-escalate.
    let sent2 = super::maybe_notify_member_state_change(
        &home,
        "solo",
        crate::state::AgentState::Idle,
        crate::state::AgentState::AuthError,
        "",
        &mut tracks,
    );
    assert!(!sent2);
    assert_eq!(
        tracks["solo"].consecutive, 1,
        "#1595: NOTIFY_COOLDOWN must prevent re-escalation within the window"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #1744-M7: when the teams config is UNREADABLE (exists but corrupt → the
/// orchestrator can't be identified), a self-orch AuthError must STILL escalate
/// to the operator (fail-closed) — we can't relay to a peer we can't find and
/// AuthError is operator-only. A non-escalation state under the same unreadable
/// config stays dropped (we genuinely can't route it).
#[test]
fn self_orch_autherror_fail_closed_on_unreadable_teams_1744_m7() {
    let home = std::env::temp_dir().join(format!("agend-1744m7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("inbox")).ok();
    // Corrupt (existing-but-invalid) fleet.yaml → try_load_fleet Err → Unknown.
    let _ = std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams: : : not valid [[[\n",
    );

    // AuthError → fail-closed escalation runs (stamps the cooldown track).
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "solo",
        crate::state::AgentState::Idle,
        crate::state::AgentState::AuthError,
        "",
        &mut tracks,
    );
    assert!(!sent, "still not an inbox self-notify");
    assert_eq!(
        tracks.get("solo").map(|t| t.consecutive),
        Some(1),
        "#1744-M7: AuthError must escalate even when teams config is unreadable (fail-closed)"
    );

    // A non-escalation state under the same unreadable config → no escalation.
    let mut tracks2 = std::collections::HashMap::new();
    let sent2 = super::maybe_notify_member_state_change(
        &home,
        "solo",
        crate::state::AgentState::Idle,
        crate::state::AgentState::RateLimit,
        "",
        &mut tracks2,
    );
    assert!(!sent2);
    assert!(
        !tracks2.contains_key("solo"),
        "#1744-M7: a non-AuthError state under an unreadable config must NOT escalate"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #1861 §3.9: a usage_limit notify must NOT re-fire on daemon restart (fresh
/// in-mem tracks) while the SAME unlock window is still open; a NEW unlock
/// window (different reset time) must re-notify. Drives the real production
/// entry `maybe_notify_member_state_change`.
#[test]
fn usage_limit_notify_not_refired_across_restart_1861() {
    let home = tmp_home("1861-restart");
    std::fs::create_dir_all(home.join("inbox")).ok();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  dev:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [dev, lead]\n    orchestrator: lead\n",
    )
    .expect("seed fleet");
    // A parseable reset time well in the future → deadline-not-passed is
    // wall-clock-robust (avoids a flake if "now" happens to be past a fixed HH:MM).
    let future = (chrono::Utc::now() + chrono::Duration::hours(3))
        .format("%H:%M")
        .to_string();
    let pane = format!("Usage limit reached. Resets at {future} UTC");

    // First detection → notifies + persists the (member, unlock_at) record.
    let mut tracks = std::collections::HashMap::new();
    let sent1 = super::maybe_notify_member_state_change(
        &home,
        "dev",
        crate::state::AgentState::Idle,
        crate::state::AgentState::UsageLimit,
        &pane,
        &mut tracks,
    );
    assert!(
        sent1,
        "first usage_limit detection notifies the orchestrator"
    );

    // Simulate daemon RESTART: fresh in-mem tracks; the persisted record stays.
    let mut tracks_after_restart = std::collections::HashMap::new();
    let sent2 = super::maybe_notify_member_state_change(
        &home,
        "dev",
        crate::state::AgentState::Idle,
        crate::state::AgentState::UsageLimit,
        &pane,
        &mut tracks_after_restart,
    );
    assert!(
        !sent2,
        "#1861: the same unlock window after a restart must NOT re-notify"
    );

    // A NEW limit (different reset time) DOES notify, even after a restart.
    let later = (chrono::Utc::now() + chrono::Duration::hours(5))
        .format("%H:%M")
        .to_string();
    let pane2 = format!("Usage limit reached. Resets at {later} UTC");
    let mut tracks3 = std::collections::HashMap::new();
    let sent3 = super::maybe_notify_member_state_change(
        &home,
        "dev",
        crate::state::AgentState::Idle,
        crate::state::AgentState::UsageLimit,
        &pane2,
        &mut tracks3,
    );
    assert!(
        sent3,
        "#1861: a new unlock window (different reset time) must re-notify"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1861 §3.9 (helper, deterministic `now`): same unlock_at before its
/// deadline → suppress; after the deadline (limit reset) → re-notify;
/// different unlock_at (new limit) → re-notify; no record → re-notify.
#[test]
fn usage_limit_notify_suppressed_logic_1861() {
    let home = tmp_home("1861-helper");
    std::fs::create_dir_all(&home).ok();
    std::fs::write(
        super::usage_limit_notify_path(&home),
        r#"{"dev":{"unlock_at":"15:14","notified_at":"2026-06-09T14:00:00+00:00"}}"#,
    )
    .expect("seed record");
    let at = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc)
    };
    assert!(
        super::usage_limit_notify_suppressed(
            &home,
            "dev",
            Some("15:14"),
            at("2026-06-09T14:30:00+00:00")
        ),
        "same unlock_at, before the 15:14 deadline → suppress"
    );
    assert!(
        !super::usage_limit_notify_suppressed(
            &home,
            "dev",
            Some("15:14"),
            at("2026-06-09T16:00:00+00:00")
        ),
        "same unlock_at, past the deadline (limit reset) → re-notify"
    );
    assert!(
        !super::usage_limit_notify_suppressed(
            &home,
            "dev",
            Some("18:00"),
            at("2026-06-09T14:30:00+00:00")
        ),
        "different unlock_at (new limit) → re-notify"
    );
    assert!(
        !super::usage_limit_notify_suppressed(
            &home,
            "other",
            Some("15:14"),
            at("2026-06-09T14:30:00+00:00")
        ),
        "no record for this member → re-notify"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1894 §3.9 (helper): an UNPARSEABLE unlock time falls back to the long
/// `NULL_UNLOCK_NOTIFY_WINDOW` (24h), NOT the 60s cooldown — so restarts hours
/// apart WITHIN the same ongoing usage-limit episode stay silent (the operator
/// pain). A genuinely-new episode past the window re-notifies. Regression-
/// proof: revert to `NOTIFY_COOLDOWN` and the 5h-restart assertion flips to
/// re-notify (the #1861/#1864 residual).
#[test]
fn usage_limit_null_unlock_long_window_1894() {
    let home = tmp_home("1894-null");
    std::fs::create_dir_all(&home).ok();
    std::fs::write(
        super::usage_limit_notify_path(&home),
        r#"{"dev":{"unlock_at":null,"notified_at":"2026-06-09T14:00:00+00:00"}}"#,
    )
    .expect("seed record");
    let at = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc)
    };
    assert!(
        super::usage_limit_notify_suppressed(&home, "dev", None, at("2026-06-09T14:00:30+00:00")),
        "null unlock_at, +30s → suppress"
    );
    // The fix: a restart HOURS later (same ongoing limit) is still suppressed.
    assert!(
        super::usage_limit_notify_suppressed(&home, "dev", None, at("2026-06-09T19:00:00+00:00")),
        "#1894: null unlock_at, +5h restart (same episode) → still suppress (was re-notify at 60s)"
    );
    // Past the 24h window (a genuinely-new episode) → re-notify.
    assert!(
        !super::usage_limit_notify_suppressed(&home, "dev", None, at("2026-06-10T15:00:00+00:00")),
        "#1894: null unlock_at, +25h (past window) → re-notify"
    );
    // Missing record still FAILS OPEN (notify) — never silently swallowed.
    assert!(
        !super::usage_limit_notify_suppressed(
            &home,
            "ghost",
            None,
            at("2026-06-09T14:00:30+00:00")
        ),
        "no record → FAIL-OPEN re-notify (#1864 contract preserved)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #event-bus pattern #9: gate-ON emit→subscriber re-delivers the inbox half
/// (A) BYTE-IDENTICALLY to the legacy `deliver_member_state_change`. The
/// frozen `detected_at` is passed identically to both paths, so the structured
/// payloads match exactly. The notify_agent half (B) is a PTY-inject covered by
/// the shared-deliver-fn invariant (same fn invoked by both paths), so it is
/// not separately drain-asserted (PTY-readback would be platform-gated + fragile).
#[test]
fn member_state_change_gate_on_emit_subscriber_matches_legacy() {
    let detected_at = "2026-06-03T09:00:00+00:00";
    let mk = |tag: &str| {
        let h = std::env::temp_dir().join(format!("agend-msc-parity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        std::fs::create_dir_all(h.join("inbox")).ok();
        h
    };
    let payloads =
        |home: &std::path::Path| -> Vec<(String, Option<String>, String, Option<String>)> {
            crate::inbox::drain(home, "orch-a")
                .into_iter()
                .map(|m| (m.from, m.kind, m.text, m.correlation_id))
                .collect()
        };

    let home_legacy = mk("legacy");
    super::deliver_member_state_change(
        &home_legacy,
        "orch-a",
        "worker-a",
        "team-a",
        crate::state::AgentState::Idle.display_name(),
        crate::state::AgentState::UsageLimit.display_name(),
        crate::state::AgentState::UsageLimit,
        "Usage limit reached. Resets at 15:14 UTC",
        Some("15:14"),
        1,
        detected_at,
    );

    let home_bus = mk("bus");
    let bus = crate::daemon::event_bus::EventBus::new();
    bus.subscribe(super::handle_event);
    bus.emit(
        &home_bus,
        crate::daemon::event_bus::EventKind::MemberStateChanged {
            agent: "worker-a".into(),
            team: "team-a".into(),
            from_state: crate::state::AgentState::Idle.display_name().to_string(),
            to_state: crate::state::AgentState::UsageLimit
                .display_name()
                .to_string(),
            orch: "orch-a".into(),
            new_state: crate::state::AgentState::UsageLimit,
            pane_tail: "Usage limit reached. Resets at 15:14 UTC".into(),
            unlock_at: Some("15:14".into()),
            consecutive_count: 1,
            detected_at: detected_at.into(),
        },
    );

    let legacy = payloads(&home_legacy);
    let via_bus = payloads(&home_bus);
    assert!(!legacy.is_empty(), "legacy enqueue must land");
    assert_eq!(
        legacy, via_bus,
        "bus inbox-half (A) must match legacy byte-for-byte"
    );

    std::fs::remove_dir_all(&home_legacy).ok();
    std::fs::remove_dir_all(&home_bus).ok();
}

/// E: no orchestrator → notify returns false (warn logged).
#[test]
fn notify_warns_when_no_orchestrator() {
    let home = std::env::temp_dir().join(format!("agend-notify-noorch-{}", std::process::id()));
    std::fs::create_dir_all(home.join("inbox")).ok();
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "team-a", "members": ["worker-a"]}),
    );
    let mut tracks = std::collections::HashMap::new();
    let sent = super::maybe_notify_member_state_change(
        &home,
        "worker-a",
        crate::state::AgentState::Idle,
        crate::state::AgentState::Hang,
        "",
        &mut tracks,
    );
    assert!(!sent, "no orchestrator → no notify");
    std::fs::remove_dir_all(&home).ok();
}

/// parse_unlock_at extracts time from pane output.
#[test]
fn parse_unlock_at_extracts_time() {
    assert_eq!(
        super::parse_unlock_at("Usage limit reached. Resets at 15:14 UTC"),
        Some("15:14".to_string())
    );
    assert_eq!(super::parse_unlock_at("no time here"), None);
}

// ── ServerRateLimit auto-retry tests ─────────────────────────────

/// #1696: tiered schedule — Phase A burst (5/15/30s), Phase B backoff
/// (1m/2m/5m), Phase C sustained (10m × 6). 12 retries, ~75min budget.
#[test]
fn backoff_tiered_phase_a_b_c_schedule_1696() {
    assert_eq!(
        super::SERVER_RATE_LIMIT_BACKOFF,
        [5, 15, 30, 60, 120, 300, 600, 600, 600, 600, 600, 600]
    );
    assert_eq!(super::SERVER_RATE_LIMIT_MAX_RETRIES, 12);
    // Phase boundaries (for the escalation INFO logs) must index into the array.
    assert_eq!(
        super::SERVER_RATE_LIMIT_BACKOFF[super::RETRY_PHASE_B_START as usize],
        60
    );
    assert_eq!(
        super::SERVER_RATE_LIMIT_BACKOFF[super::RETRY_PHASE_C_START as usize],
        600
    );
}

#[test]
fn retries_stop_at_tiered_max_1696() {
    // #1696: the budget is now MAX_RETRIES (12, tiered A/B/C), not 3.
    let mut retry = RateLimitRetry {
        retry_count: super::SERVER_RATE_LIMIT_MAX_RETRIES,
        next_retry_at: std::time::Instant::now(),
        exhausted: false,
        inject_failures: 0,
        abort_pending: false,
    };
    retry.retry_count += 1;
    assert!(
        retry.retry_count > super::SERVER_RATE_LIMIT_MAX_RETRIES,
        "the (count+1 > max) guard exhausts only after the full tiered budget"
    );
}

/// #1325: validate the retry payload constant value and that it ends
/// with a newline (required for CLI agent prompt submission).
#[test]
fn continue_retry_payload_is_valid() {
    assert_eq!(
        super::CONTINUE_RETRY_PAYLOAD,
        b"continue\n",
        "payload must be the fixed resume signal"
    );
    assert!(
        super::CONTINUE_RETRY_PAYLOAD.ends_with(b"\n"),
        "payload must end with newline for prompt submission"
    );
}

/// #1325: validate "continue" works as input for all backends that can
/// enter ServerRateLimit (backends with API-backed models). Shell/Raw
/// backends never enter ServerRateLimit so they're excluded.
#[test]
fn continue_payload_compatible_with_all_api_backends() {
    use crate::backend::Backend;
    let api_backends = [
        Backend::ClaudeCode,
        Backend::KiroCli,
        Backend::Codex,
        Backend::OpenCode,
        Backend::Agy,
    ];
    for backend in &api_backends {
        let preset = backend.preset();
        assert_eq!(
            preset.submit_key, "\r",
            "{:?} must use \\r submit_key for continue inject to work",
            backend
        );
    }
}

/// Helper: create a minimal AgentHandle with a real PTY for behavioral
/// tests. Spawns a stdin-echoing process (Unix: `cat`, Windows: `findstr .*`).
fn mock_agent_handle(
    name: &str,
    state: crate::state::AgentState,
) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
    mock_agent_handle_with_size(name, state, 10, 80)
}

fn mock_agent_handle_with_size(
    name: &str,
    state: crate::state::AgentState,
    rows: u16,
    cols: u16,
) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    #[cfg(not(target_os = "windows"))]
    let mut cmd = portable_pty::CommandBuilder::new("cat");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = portable_pty::CommandBuilder::new("cmd");
        c.args(["/c", "findstr", ".*"]);
        c
    };
    cmd.cwd(std::env::temp_dir());
    let child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn stdin-echo process");
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let writer = pair.master.take_writer().expect("take writer");
    let pty_writer: crate::agent::PtyWriter = Arc::new(parking_lot::Mutex::new(writer));
    let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
        vterm: crate::vterm::VTerm::with_pty_writer(cols, rows, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: crate::state::StateTracker::new(None),
        health: crate::health::HealthTracker::new(),
        api_activity: crate::agent::ApiActivity::default(),
        observed_status: None,
    }));
    core.lock().state.current = state;
    // Direct `.current` write bypasses record_set, so sync the lock-free mirror.
    let published_state = core.lock().state.published_handle();
    let published_observed = core.lock().state.published_observed_handle();
    published_state.store(state as u8, std::sync::atomic::Ordering::Relaxed);
    let handle = crate::agent::AgentHandle {
        id: crate::types::InstanceId::default(),
        name: name.to_string().into(),
        declared_backend: None,
        backend_command: "claude".to_string(),
        pty_writer,
        pty_master: Arc::new(parking_lot::Mutex::new(pair.master)),
        core,
        published_state,
        published_observed,
        child: Arc::new(parking_lot::Mutex::new(child)),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: false,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        spawn_mode: crate::backend::SpawnMode::Fresh,
        generation: crate::agent::crash_disposition::SpawnGeneration::default(),
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    (handle, reader)
}

/// #1325: phase 1 — ServerRateLimit detection populates retry_tracks.
#[test]
fn phase1_detects_rate_limit_and_schedules_retry() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("phase1-detect");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    // #1441: registry is UUID-keyed — insert under the handle's own id.
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        tracks.contains_key("test-agent"),
        "phase 1 must detect ServerRateLimit and insert retry track"
    );
    assert_eq!(tracks["test-agent"].retry_count, 0);
    assert!(!tracks["test-agent"].exhausted);
    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────── #2466 work-turn 529 looser-signal arm ───────────────────

/// PURE truth table for the work-turn throttle arm predicate. The arm fires ONLY with the
/// full conjunction; dropping any guard (the primary `blocked_rl`, the `throttle_hint`
/// corroboration, or the `!recovered` liveness) must turn it off. NEUTER for
/// the integration arm below.
#[test]
fn work_turn_throttle_arm_truth_table_2466() {
    // Full conjunction → arm.
    assert!(
        super::work_turn_throttle_arm(true, true, false),
        "blocked_rl + throttle_hint + !recovered → arm"
    );
    // Each guard is load-bearing.
    assert!(
        !super::work_turn_throttle_arm(false, true, false),
        "no blocked_reason=RateLimit (a bare throttle-token mention) → must NOT arm"
    );
    assert!(
        !super::work_turn_throttle_arm(true, false, false),
        "no throttle hint on screen → must NOT arm"
    );
    assert!(
        !super::work_turn_throttle_arm(true, true, true),
        "recovered (productive output) → must NOT arm"
    );
}

/// LOAD-BEARING (the dev-3 incident, real path through `process_error_recovery`): a work-turn
/// hit a 529/ApiError — the StateTracker `AgentState` reads Idle (the #1769 positional defeat)
/// but the watchdog's gate-free `classify_pty_output` latched `blocked_reason=RateLimit` and a
/// throttle banner is still on screen. The retry arm MUST latch a track (so a `continue` retry
/// fires) instead of leaving the agent hung. NEUTER: drop `|| loose_arm` from the arm branch →
/// state=Idle falls straight to the Idle-clear branch → no track → RED.
#[test]
fn work_turn_529_arms_retry_via_looser_signal_2466() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("wt529-arm");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

    // state=Idle (NOT ServerRateLimit — the strict arm can't fire), but the looser signals
    // that the dev-3 incident proved fired: blocked_reason=RateLimit (watchdog/classify) and a
    // throttle banner on screen (screen_has_throttle_hint).
    let (handle, _reader) = mock_agent_handle("wt529", crate::state::AgentState::Idle);
    {
        let mut core = handle.core.lock();
        core.health
            .set_blocked_reason(crate::health::BlockedReason::RateLimit {
                retry_after_secs: None,
            });
        core.vterm
            .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
    }
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
            tracks.contains_key("wt529"),
            "#2466: a work-turn 529 (Idle + blocked_reason=RateLimit + throttle_hint) must arm the retry track via the looser signal"
        );
    std::fs::remove_dir_all(&home).ok();
}

/// FP guard (real path): the loose arm must require BOTH the primary `blocked_reason=RateLimit`
/// AND the throttle-hint corroboration. An Idle agent with only one of the two must NOT arm —
/// a bare ApiError or a mere prose mention of a throttle token cannot trigger a `continue` spam.
#[test]
fn work_turn_529_partial_signal_does_not_arm_2466() {
    let home = tmp_home("wt529-fp");

    // (a) throttle hint on screen but NO blocked_reason=RateLimit (classify did not match a
    //     real rate-limit error — just a token in prose) → must NOT arm.
    {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let (handle, _r) = mock_agent_handle("fp-no-blocked", crate::state::AgentState::Idle);
        handle
            .core
            .lock()
            .vterm
            .process(b"\r\ndiscussing the 429 limiting error in an RCA\r\n");
        registry.lock().insert(handle.id, handle);
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            !tracks.contains_key("fp-no-blocked"),
            "#2466 FP: throttle token without blocked_reason=RateLimit must NOT arm"
        );
    }

    // (b) blocked_reason=RateLimit but NO throttle hint on screen (banner scrolled off) →
    //     must NOT arm (corroboration absent).
    {
        let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
        let (handle, _r) = mock_agent_handle("fp-no-hint", crate::state::AgentState::Idle);
        handle
            .core
            .lock()
            .health
            .set_blocked_reason(crate::health::BlockedReason::RateLimit {
                retry_after_secs: None,
            });
        registry.lock().insert(handle.id, handle);
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            past_boot_grace(),
        );
        assert!(
            !tracks.contains_key("fp-no-hint"),
            "#2466 FP: blocked_reason=RateLimit without a screen throttle hint must NOT arm"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

/// ARM/CLEAR SYNC (real path): once the throttle signal is gone (blocked_reason cleared) the
/// loose arm goes false, so the Idle-clear branch reclaims the track — the work-turn track does
/// NOT linger forever. Proves the #4 synchronization: `clears` only fires when `loose_arm` is
/// false, so a still-throttled Idle stays armed but a genuinely-cleared Idle clears.
#[test]
fn work_turn_529_clears_when_throttle_signal_gone_2466() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("wt529-clear");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

    let (handle, _reader) = mock_agent_handle("wt529c", crate::state::AgentState::Idle);
    {
        let mut core = handle.core.lock();
        core.health
            .set_blocked_reason(crate::health::BlockedReason::RateLimit {
                retry_after_secs: None,
            });
        core.vterm
            .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
    }
    let id = handle.id;
    registry.lock().insert(id, handle);

    // First pass arms the track.
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(tracks.contains_key("wt529c"), "work-turn 529 armed");

    // The throttle lifts: clear the watchdog latch → blocked_rl false → loose_arm false.
    registry
        .lock()
        .get(&id)
        .expect("handle present")
        .core
        .lock()
        .health
        .clear_blocked_reason();

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
            !tracks.contains_key("wt529c"),
            "#2466: once the throttle latch clears, the Idle-clear branch reclaims the work-turn track (arm/clear synchronized)"
        );
    std::fs::remove_dir_all(&home).ok();
}

// ─────────────────── #t-26795 SRL hook-override ───────────────────

/// PURE truth-table + FORWARD-PROGRESS (test ②) for the hook→recovery decision:
/// a hook seq STRICTLY greater than the floor recovers; a seq equal to (consumed)
/// or below (pre-onset) the floor does not — so once the floor advances onto a
/// hook, that SAME hook no longer overrides, only a NEWER one does.
#[test]
fn hook_recovered_for_srl_truth_table() {
    let floor = 1000u64;
    assert!(
        super::hook_recovered_for_srl(true, Some(1500), Some(floor)),
        "claude + a fresh active hook NEWER than the floor → recovered (forward progress)"
    );
    assert!(
        !super::hook_recovered_for_srl(true, Some(500), Some(floor)),
        "a hook seq BELOW the floor (pre-onset / prior turn) → genuine new SRL not masked"
    );
    assert!(
            !super::hook_recovered_for_srl(true, Some(1000), Some(floor)),
            "a hook seq EQUAL to the floor (already consumed — no newer hook) → not recovered → re-arms"
        );
    assert!(
        !super::hook_recovered_for_srl(true, None, Some(floor)),
        "no fresh ACTIVE hook (idle/stale/absent) → not recovered"
    );
    assert!(
        !super::hook_recovered_for_srl(true, Some(1500), None),
        "no floor (agent not in SRL) → not recovered"
    );
    assert!(
        !super::hook_recovered_for_srl(false, Some(1500), Some(floor)),
        "non-claude backend → never (unaffected)"
    );
}

/// FLAP REGRESSION (operator's exact symptom, #t-26795). An agent latched on a
/// STICKY screen `ServerRateLimit` with `recovered`=false (no productive output
/// this instant) but ALIVE — firing a NEW tool-call hook every tick. Each fresh
/// hook seq exceeds the floor (forward progress) → the retry track stays CLEARED
/// across re-detect ticks (no re-arm = the `continue`-spam flap killed) and the
/// floor advances as each hook is consumed. NEUTER: drop `|| hook_recovered` from
/// the clear gate → a recovered=false tick re-arms → this RED.
#[test]
#[serial_test::serial]
fn srl_hook_override_kills_flap() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-hook-flap");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    let name = "srl-flap-agent";
    let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    let id = handle.id;
    registry.lock().insert(handle.id, handle);
    // Onset baseline: a pre-SRL hook pins the floor BELOW the recovery hooks.
    crate::daemon::hook_shadow::record_event(name, "Stop", None); // idle baseline
    let floor = crate::daemon::hook_shadow::latest_hook_seq(name);
    srl_floor.insert(id, floor);
    // The agent is actually ALIVE (false sticky SRL): it fires a NEW tool-call hook
    // each tick → each seq > floor → forward progress → the retry stays cleared.
    for _ in 0..3 {
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            &mut srl_floor,
            past_boot_grace(),
        );
        assert!(
                !tracks.contains_key(name),
                "a fresh post-floor claude hook each tick keeps the SRL retry cleared — the continue-spam flap is killed"
            );
    }
    assert!(
        srl_floor
            .get(&id)
            .copied()
            .expect("floor present after override")
            > floor,
        "the floor ADVANCES to the latest consumed hook seq (forward progress)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// FORWARD-PROGRESS (test ①, #t-26795 r6 finding-1): the multi-episode case the
/// stable first-onset design missed, driven end-to-end through the code. The
/// screen STAYS sticky-SRL the whole time (never emits a non-SRL tick → the floor
/// is never reset). (a) onset: an idle baseline seeds the floor, no active hook →
/// the retry ARMS. (b) episode A: a fresh tool-call hook (seq > floor) overrides
/// AND ADVANCES the floor onto it → the retry clears. (c) episode B: the agent is
/// now genuinely stuck — NO newer hook — so that SAME hook's seq == the advanced
/// floor → no override → the retry must RE-ARM. NEUTER: drop the floor-advance
/// (revert to the stable first-onset design) → the still-fresh episode-A hook
/// stays seq > the un-advanced floor → it permanently re-masks B → no re-arm → RED.
#[test]
#[serial_test::serial]
fn srl_forward_progress_rearms_genuine_episode_b() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-fwd-progress");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    let name = "srl-fwd-agent";
    let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    registry.lock().insert(handle.id, handle);
    let pe = |tracks: &mut HashMap<String, RateLimitRetry>,
              srl_floor: &mut HashMap<crate::types::InstanceId, u64>| {
        super::process_error_recovery(
            &home,
            &registry,
            tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            srl_floor,
            past_boot_grace(),
        );
    };
    // (a) onset: an idle baseline seeds the floor; no active hook → the retry arms.
    crate::daemon::hook_shadow::record_event(name, "Stop", None);
    pe(&mut tracks, &mut srl_floor);
    assert!(
        tracks.contains_key(name),
        "onset with no active hook arms the retry"
    );
    // (b) episode A: a fresh tool-call hook NEWER than the floor overrides AND the
    // production code advances the floor onto it → the retry clears.
    crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
    pe(&mut tracks, &mut srl_floor);
    assert!(
        !tracks.contains_key(name),
        "episode A: a fresh post-floor hook overrides — the retry clears"
    );
    // (c) episode B: agent genuinely stuck — NO newer hook. The same episode-A hook
    // is still Fresh(ToolUse) but its seq == the advanced floor → no override.
    pe(&mut tracks, &mut srl_floor);
    assert!(
            tracks.contains_key(name),
            "a genuine episode B (no hook newer than the CONSUMED floor) must re-arm the retry — forward progress, not permanent mask"
        );
    std::fs::remove_dir_all(&home).ok();
}

/// CHURN PRUNE (#t-26795 r6 finding-2): an instance's SRL floor is dropped once it
/// leaves the registry, so the UUID-keyed map stays bounded across agent churn.
/// NEUTER: drop the `srl_floor.retain(...)` churn-prune → RED.
#[test]
#[serial_test::serial]
fn srl_floor_pruned_on_agent_churn() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-floor-churn");
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    // A prior instance left a floor seq behind; its uuid is NO LONGER in the
    // registry (deleted / restarted).
    let stale_id = crate::types::InstanceId::new();
    srl_floor.insert(stale_id, 1);
    super::process_error_recovery(
        &home,
        &registry,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut srl_floor,
        past_boot_grace(),
    );
    assert!(
        !srl_floor.contains_key(&stale_id),
        "a churned-out instance's SRL floor must be pruned to bound the map across churn"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// UUID-KEY (test ③, #t-26795 r6 finding-2): the floor must key on the STABLE
/// `InstanceId`, NOT the agent name — else a same-name handle SWAPPED between two
/// consecutive ticks (delete/recreate/restart, with NO intermediate absent-name
/// pass so the name-keyed retain never fires) lets the new instance INHERIT the old
/// one's advanced floor and its genuine first SRL is wrongly overridden. Drives the
/// old instance to advance its floor, swaps in a new same-name handle (new uuid)
/// that emits a pre-onset hook (global seq > the old floor), and asserts the new
/// instance's genuine SRL RE-ARMS. NEUTER: key the floor by name → the new instance
/// inherits the old floor → its hook seq > inherited floor → override → no arm → RED.
#[test]
#[serial_test::serial]
fn srl_floor_keyed_by_instance_id_survives_same_name_swap() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-floor-swap");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    let name = "swapped-agent";
    let pe = |tracks: &mut HashMap<String, RateLimitRetry>,
              srl_floor: &mut HashMap<crate::types::InstanceId, u64>| {
        super::process_error_recovery(
            &home,
            &registry,
            tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut Default::default(),
            srl_floor,
            past_boot_grace(),
        );
    };
    // OLD instance: onset baseline → then a fresh hook overrides + advances its
    // floor (so the old floor is LOW relative to later global seqs).
    let (old, _r1) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    let old_id = old.id;
    registry.lock().insert(old_id, old);
    crate::daemon::hook_shadow::record_event(name, "Stop", None);
    pe(&mut tracks, &mut srl_floor);
    crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
    pe(&mut tracks, &mut srl_floor);
    assert!(!tracks.contains_key(name), "old instance recovered");
    // SWAP: same NAME, NEW uuid — delete old (forget its hooks) + insert new, with
    // NO intermediate process_error_recovery call where the name is absent.
    registry.lock().clear();
    crate::daemon::hook_shadow::forget(name);
    let (new, _r2) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    let new_id = new.id;
    registry.lock().insert(new_id, new);
    // The new instance emits a pre-onset hook (global seq > the OLD floor) BEFORE
    // its first genuine SRL.
    crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
    pe(&mut tracks, &mut srl_floor);
    assert!(
            tracks.contains_key(name),
            "a same-name replacement's genuine first SRL must NOT be masked by the prior instance's inherited floor (UUID-keyed)"
        );
    assert!(
        !srl_floor.contains_key(&old_id),
        "the prior instance's floor is pruned (its uuid left the registry)"
    );
    assert!(
        srl_floor.contains_key(&new_id),
        "the new instance seeded its OWN floor under its own uuid"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// EDGE (#t-26795): a genuine NEW SRL must NOT be masked. The agent's latest hook
/// PRESENT at onset seeds the floor to its OWN seq, so with NO newer hook the
/// active seq EQUALS the floor → not strictly greater → no override → the retry
/// arms. (A hook present at onset is "pre-onset" w.r.t. the floor it seeds.) This
/// exercises the `or_insert_with(latest_hook_seq)` onset-init path — r4's blessed
/// edge-a, preserved under the seq model. NEUTER: relax `h > f` to `h >= f` → the
/// onset hook wrongly overrides → no arm → RED.
#[test]
#[serial_test::serial]
fn srl_genuine_not_masked_by_pre_onset_hook() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-genuine");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();
    let name = "srl-genuine-agent";
    let (handle, _r) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    registry.lock().insert(handle.id, handle);
    // A hook present at onset: the floor `or_insert`s to its seq → active seq ==
    // floor → no override. No newer hook arrives = a genuine SRL.
    crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut srl_floor,
        past_boot_grace(),
    );
    assert!(
        tracks.contains_key(name),
        "a hook no newer than the onset floor must NOT mask a genuine SRL — the retry must arm"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #ratelimit-recovery (the live storm that wedged fixup-lead): an agent still
/// LATCHED ServerRateLimit (the stale "Server is temporarily limiting" line
/// re-matches in the tail, and `working_state_below` can't see a marker BELOW
/// the most-recent error line) but that has produced PRODUCTIVE output within
/// RECOVERY_SILENCE has recovered — its retry track must be CLEARED and NO
/// `continue` injected. `last_productive_output` is position-independent, so it
/// breaks the Thinking↔ServerRateLimit flicker the Idle-only #1713 clear missed.
#[test]
fn server_rate_limit_recent_productive_output_clears_and_skips_inject() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-recovered");
    // Pre-arm an in-flight retry track (a ServerRateLimit episode already running).
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    // Recovered: produced productive output just now (< RECOVERY_SILENCE),
    // overriding the `None` (never-produced) default.
    handle.core.lock().state.last_productive_output = Some(Instant::now());
    registry.lock().insert(handle.id, handle);

    // Several ticks (the live flicker) — must never re-arm + inject.
    for _ in 0..3 {
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
    }

    assert!(
        !tracks.contains_key("test-agent"),
        "#ratelimit-recovery: a recently-productive ServerRateLimit agent's retry \
             track must be cleared (recovered), not maintained/re-armed"
    );
    assert!(
        !last_inject.contains_key("test-agent"),
        "#ratelimit-recovery: no `continue` may be injected into a recovered \
             (recently-productive) agent — that was the live storm"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #26795-3 (was #2232 (a)+(c)): the `self_cleared` agent-driven recovery signal
/// was removed (2746/2746 telemetry: agents never set it). Its unique role —
/// clearing the SRL retry track on recovery even WITHOUT productive output (the
/// pure-text fast-reply gap where `recovered_within` misses) — is now carried by
/// the surviving `hook_recovered` signal. This pins the supervisor SRL clear
/// composition `recovered || hook_recovered`: a fresh post-onset claude hook
/// clears the track and injects NO `continue` across repeated ticks, with no
/// productive-output marker. (Complements `srl_hook_override_kills_flap`, which
/// pins the floor-advance; this pins the no-over-inject guarantee.)
#[test]
#[serial_test::serial]
fn server_rate_limit_hook_recovered_clears_track_and_skips_inject_26795_3() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-hook-recovered");
    let name = "srl-hook-recovered-agent";
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        name.to_string(),
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now(), // DUE — would inject absent recovery
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();
    let mut srl_floor: HashMap<crate::types::InstanceId, u64> = HashMap::new();

    let (handle, _reader) = mock_agent_handle(name, crate::state::AgentState::ServerRateLimit);
    let id = handle.id;
    // NOT recovered: last_productive_output stays None (the pure-text gap).
    registry.lock().insert(handle.id, handle);
    // Onset baseline pins the floor BELOW the recovery hooks.
    crate::daemon::hook_shadow::record_event(name, "Stop", None);
    let floor = crate::daemon::hook_shadow::latest_hook_seq(name);
    srl_floor.insert(id, floor);

    // A fresh post-onset tool-call hook each tick (seq > floor) = hook_recovered:
    // ground-truth the agent is alive even with no productive-output marker.
    for _ in 0..3 {
        crate::daemon::hook_shadow::record_event(name, "PreToolUse", None);
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut srl_floor,
            past_boot_grace(),
        );
    }

    assert!(
        !tracks.contains_key(name),
        "#26795-3: a hook-recovered SRL agent's retry track must be dropped even \
             with no recent productive output (the pure-text gap self_cleared covered)"
    );
    assert!(
        !last_inject.contains_key(name),
        "#26795-3: no `continue` may be injected into a hook-recovered agent"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #26795-3 (a): a NON-CLAUDE backend has no state hooks, so `hook_recovered` is
/// always false — its only SRL retry-exit is the productive-output heuristic
/// (`recovered`). Removing the dead `self_cleared` signal must leave that path
/// unchanged: a recovered (recent productive output) non-claude agent still has
/// its retry track cleared and no `continue` injected.
#[test]
fn non_claude_srl_retry_exit_unchanged_after_self_clear_removal_26795_3() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-nonclaude-exit");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now(), // DUE — would inject absent recovery
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    let (mut handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    // NON-CLAUDE: codex has no state hooks (`has_state_hooks()` false) → the hook
    // recovery path can never fire; only the productive-output heuristic can exit.
    handle.backend_command = "codex".to_string();
    // Recovered via the heuristic: recent productive output.
    handle.core.lock().state.last_productive_output = Some(Instant::now());
    registry.lock().insert(handle.id, handle);

    for _ in 0..3 {
        super::process_error_recovery(
            &home,
            &registry,
            &mut tracks,
            &mut Default::default(),
            &mut Default::default(),
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
    }

    assert!(
        !tracks.contains_key("test-agent"),
        "#26795-3 (a): a recovered non-claude agent's SRL retry track must clear \
             via the productive-output heuristic (unchanged by the signal removal)"
    );
    assert!(
        !last_inject.contains_key("test-agent"),
        "#26795-3 (a): no `continue` injected into a recovered non-claude agent"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #26795-3 (was #2232 D1(b)): when the supervisor tracks/injects a rate-limit
/// retry it ALREADY knows the agent is rate-limited, so it marks the agent
/// `RateLimit`-blocked. The `self_cleared` consumer this originally fed was
/// removed (dead signal, 2746/2746 never set), but the `set_blocked_reason` is
/// RETAINED: it surfaces the block in the operator-visible health status and
/// feeds the loose-arm `blocked_rl` read on the next tick. This pins that
/// retained behaviour (unchanged by the signal removal).
#[test]
fn server_rate_limit_inject_schedule_marks_ratelimit_block_26795_3() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("srl-mark-block");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();

    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    // No prior blocked_reason, not recovered.
    assert!(handle.core.lock().health.current_reason.is_none());
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );

    let reg = registry.lock();
    let h = reg.values().next().expect("agent present");
    assert!(
        matches!(
            h.core.lock().health.current_reason,
            Some(crate::health::BlockedReason::RateLimit { .. })
        ),
        "#26795-3: inject-schedule must still mark the agent RateLimit-blocked \
             (retained health-status marking after the self_cleared signal removal)"
    );
    drop(reg);
    std::fs::remove_dir_all(&home).ok();
}

/// #1325/#1946: phase 1 — GENUINE recovery (Idle + recent productive output)
/// clears the retry track. (#1946 narrowed the clear: an Idle WITHOUT recent
/// productive output and retries in flight is the abort shape and retains —
/// see the 1946 tests below — so this genuine-recovery contract now requires
/// the productive-output signal it always meant.)
#[test]
fn phase1_recovery_clears_retry_track() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("phase1-recovery");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );

    let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
    // Genuine recovery: the agent produced real output before idling.
    handle.core.lock().state.last_productive_output = Some(Instant::now());
    // #1441: registry is UUID-keyed — insert under the handle's own id.
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        !tracks.contains_key("test-agent"),
        "phase 1 must clear retry track on genuine Idle recovery"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1946 (closes #1808 Flaw 1, production-evidenced 2026-06-10 08:59 probe
/// fire + 08:55-08:59 dev-2 freeze): an abort-to-Idle with an in-flight
/// ServerRateLimit retry and NO recent productive output must RETAIN the
/// track (ownership of recovery stays with the supervisor) and schedule a
/// delayed after-abort retry on the SAME tiered backoff — not clear it
/// (the pre-#1946 behavior, which froze the agent until manual rescue
/// because post-#1936 detection never re-creates a track either).
#[test]
fn abort_to_idle_retains_track_and_resumes_retry_1946() {
    // one_agent_registry writes fleet.yaml so the Phase-2 inject can
    // resolve the agent (a bare registry insert reads as AgentGone).
    // The agent sits at an Idle prompt with NO recent productive output
    // (`last_productive_output` defaults to None) — the freeze shape.
    let (home, registry, _reader) = one_agent_registry(
        "test-agent",
        crate::state::AgentState::Idle,
        "abort-retain-1946",
    );
    {
        let reg = registry.lock();
        let handle = reg.values().next().expect("agent handle exists");
        handle
            .core
            .lock()
            .vterm
            .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
    }
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    // Tick 1: the abort is detected → track retained, abort_pending set,
    // next retry scheduled on the tiered backoff (BACKOFF[2] = 30s out) —
    // NOT due yet, so no inject this tick.
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    {
        let track = tracks
            .get("test-agent")
            .expect("#1946: abort-to-Idle must RETAIN the in-flight track, not clear it");
        assert!(track.abort_pending, "#1946: abort_pending marked");
        assert!(
            track.next_retry_at > Instant::now() + Duration::from_secs(20),
            "#1946: delayed retry continues the tiered schedule (BACKOFF[2]=30s), not immediate"
        );
        assert!(
            !last_inject.contains_key("test-agent"),
            "#1946: no inject before the delayed retry is due"
        );
    }

    // Make the delayed retry due → tick 2 must inject the after-abort
    // `continue` (the ONLY Idle-state inject, gated on abort_pending +
    // !recovered) and keep the track.
    tracks
        .get_mut("test-agent")
        .expect("track retained")
        .next_retry_at = Instant::now();
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        last_inject.contains_key("test-agent"),
        "#1946: due after-abort retry must inject `continue` into the Idle agent"
    );
    let track = tracks.get("test-agent").expect("track survives the inject");
    assert_eq!(
        track.retry_count, 3,
        "#1946: after-abort attempts consume the SAME 12-retry budget"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1946: genuine recovery AFTER an abort (productive output appears — e.g.
/// the operator dispatched work, or the after-abort `continue` revived the
/// agent) clears the retained track; no further inject.
#[test]
fn abort_pending_recovered_clears_track_1946() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-recovered-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 3,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
    // Productive output landed after the abort — genuine recovery.
    handle.core.lock().state.last_productive_output = Some(Instant::now());
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        !tracks.contains_key("test-agent"),
        "#1946: genuine recovery after an abort clears the retained track"
    );
    assert!(
        !last_inject.contains_key("test-agent"),
        "#1946: no `continue` into a recovered agent"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1946 / #1808: when the rate limit error has scrolled off the screen
/// (the vterm no longer contains the throttle hint), the abort-pending retry track must be cleared
/// even if the agent hasn't produced new output within the silence window.
#[test]
fn abort_pending_scrolled_off_clears_track_1946() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-scrolled-off-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 3,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );

    let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
    // core.vterm is empty by default, so screen_has_throttle_hint returns false (scrolled off).
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );

    assert!(
        !tracks.contains_key("test-agent"),
        "scrolled off error must clear the abort-pending retry track"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1946: a fresh ServerRateLimit observation while abort-recovery is
/// pending hands ownership back to the normal fresh-SRL retry path (same
/// track, same budget — structurally a single owner, no double-continue).
#[test]
fn abort_pending_stands_down_on_srl_relatch_1946() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-relatch-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 3,
            // Not due — proves the stand-down happens on observation, not inject.
            next_retry_at: Instant::now() + Duration::from_secs(600),
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );

    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    let track = tracks.get("test-agent").expect("track persists");
    assert!(
        !track.abort_pending,
        "#1946: SRL re-latch resumes normal retry ownership (abort-recovery stands down)"
    );
    assert_eq!(track.retry_count, 3, "budget carries over, no reset");
    std::fs::remove_dir_all(&home).ok();
}

/// #1946: the after-abort path consumes the SAME tiered budget — at the
/// 12-retry cap the existing exhaustion path (orchestrator inbox notify +
/// Error-severity channel alert) finally becomes REACHABLE in a sustained
/// outage (pre-#1946 the track died at the first abort, so exhaustion—and
/// its escalation—never fired).
#[test]
fn abort_pending_budget_exhaustion_reachable_1946() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-exhaust-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: SERVER_RATE_LIMIT_MAX_RETRIES, // budget already burned
            next_retry_at: Instant::now(),              // due
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    let (handle, _reader) = mock_agent_handle("test-agent", crate::state::AgentState::Idle);
    handle
        .core
        .lock()
        .vterm
        .process(b"\r\nAPI Error: Server is temporarily limiting requests\r\n");
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    let track = tracks
        .get("test-agent")
        .expect("exhausted track retained this tick");
    assert!(
        track.exhausted,
        "#1946: after-abort attempts walk into the existing exhaustion path"
    );
    assert!(
        !last_inject.contains_key("test-agent"),
        "no inject past the budget cap"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Verify that if the throttle error is sitting in rows 16–40 (e.g. at row 20 on a 50-row screen),
/// the track is retained (proving the TAIL_LINES window correctly scans up to 40 rows).
#[test]
fn abort_pending_retains_track_when_error_in_rows_16_to_40() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-rows-16-40-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 3,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );

    let (handle, _reader) =
        mock_agent_handle_with_size("test-agent", crate::state::AgentState::Idle, 50, 80);

    // Write the error message, then write 20 empty lines so the error is pushed to row 20 from bottom.
    {
        let mut core_lock = handle.core.lock();
        core_lock
            .vterm
            .process(b"API Error: Server is temporarily limiting requests\r\n");
        for _ in 0..20 {
            core_lock.vterm.process(b"\r\n");
        }
    }

    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );

    assert!(
            tracks.contains_key("test-agent"),
            "error at row 20 (within 40-row TAIL_LINES window) must retain the abort-pending retry track"
        );
    std::fs::remove_dir_all(&home).ok();
}

/// #1985 / Item 4: Document the soft-wrap split edge case. If the throttle hint
/// is split across a soft-wrap boundary, it won't match, and the track is cleared.
#[test]
fn abort_pending_split_wrap_clears_track_1946() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("abort-split-wrap-1946");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 3,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: true,
        },
    );

    let (handle, _reader) =
        mock_agent_handle_with_size("test-agent", crate::state::AgentState::Idle, 10, 38);

    // Write the error message. Because cols is 38, "limiting" is soft-wrapped across lines.
    {
        let mut core_lock = handle.core.lock();
        core_lock
            .vterm
            .process(b"API Error: Server is temporarily limiting requests\r\n");
    }

    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );

    assert!(
        !tracks.contains_key("test-agent"),
        "soft-wrapped split error token does not match in tail_lines, so track is cleared"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1713: the recovery-clear predicate is now {Idle} (genuine terminal
/// recovery → cross-episode reset). #1586's Thinking/ToolUse broadening is
/// removed: with the #1713 state-gate at the decision point the blind-fire
/// storm is structurally impossible, so the broadening that compensated for it
/// is unnecessary. Thinking/ToolUse no longer clear (a working agent reaches
/// Ready/Idle between turns and clears then; it never injects mid-work anyway).
#[test]
fn clears_server_rate_limit_retry_covers_only_terminal_recovery_1713() {
    use crate::state::AgentState::*;
    // Genuine terminal recovery → clear (cross-episode reset). (`Idle` is the
    // sole terminal-recovery state since the Ready/Idle merge.)
    assert!(
        super::clears_server_rate_limit_retry(Idle),
        "#1713: terminal-recovery state Idle must clear the retry track"
    );
    // Everything else — incl mid-work Active and every waiting/error
    // state — must NOT clear.
    for s in [
        Active,
        ServerRateLimit,
        RateLimit,
        ApiError,
        AuthError,
        UsageLimit,
        ContextFull,
        Hang,
        PermissionPrompt,
        Starting,
    ] {
        assert!(
            !super::clears_server_rate_limit_retry(s),
            "#1713: state {s:?} must NOT clear the retry track"
        );
    }
}

/// #1713 reachability regression (credit @cheerc + angle-A trace): a track
/// scheduled while the agent was ServerRateLimit must NOT inject `continue`
/// once the agent has moved into a legit WAITING state (here PermissionPrompt
/// — e.g. the resumed agent ran a tool needing approval). PermissionPrompt is
/// not a clearing state, so the track PERSISTS (a genuine throttle could still
/// be present and resume), but the #1713 decision-point gate only fires an
/// inject on a FRESH ServerRateLimit observation — so no `continue` is injected
/// into the prompt. Pre-#1713 the state-blind Phase-2 loop injected every backoff.
#[test]
fn permission_prompt_keeps_track_but_does_not_inject_1713() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("1713-permission-no-inject");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    // A live, DUE track (as if scheduled while the agent was ServerRateLimit).
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: Instant::now(), // due now
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::PermissionPrompt);
    registry.lock().insert(handle.id, handle);

    let mut last_inject: HashMap<String, Instant> = HashMap::new();
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );

    // Track persists (PermissionPrompt is not a clearing state)…
    assert!(
        tracks.contains_key("test-agent"),
        "#1713: PermissionPrompt must NOT clear the track (a real throttle could resume)"
    );
    // …but NO inject fired and the retry budget was NOT consumed (the bug was
    // injecting `continue` into the waiting prompt every backoff).
    assert!(
        last_inject.is_empty(),
        "#1713: no `continue` inject into a non-ServerRateLimit (waiting) state"
    );
    assert_eq!(
        tracks["test-agent"].retry_count, 1,
        "#1713: a non-ServerRateLimit tick must not advance the retry count"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1586 FP-direction (the OTHER half): a genuine throttle leaves the agent
/// STUCK in ServerRateLimit — the retry track must PERSIST (so the
/// `continue` nudge still fires for real throttles).
#[test]
fn phase1_stuck_throttle_keeps_retry_track_1586() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("phase1-real-stuck");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    // Far-future retry so phase 2 doesn't fire / inject during this test.
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: Instant::now() + Duration::from_secs(3600),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let (handle, _reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    registry.lock().insert(handle.id, handle);

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        tracks.contains_key("test-agent"),
        "#1586: a still-throttled (stuck) agent must KEEP its retry track"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1325: phase 2 — due retry injects "continue\n" to PTY. Captures
/// actual PTY output via the reader end to verify the injected payload.
/// Windows PTY injects ANSI escapes (`\x1b[6n`) that contaminate the
/// read — skip on Windows where `findstr` cannot echo stdin faithfully.
#[test]
#[cfg(not(target_os = "windows"))]
fn phase2_injects_continue_to_pty() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("phase2-inject");

    let (handle, mut reader) =
        mock_agent_handle("test-agent", crate::state::AgentState::ServerRateLimit);
    // #1441: phase 2 inject resolves the name-keyed track via fleet.yaml;
    // seed the entry with the handle's own id so resolution hits this
    // registry entry (registry key == handle.id == resolve_uuid(name)).
    let agent_id = handle.id;
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  test-agent:\n    id: {}\n", agent_id.full()),
    )
    .ok();
    registry.lock().insert(agent_id, handle);

    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "test-agent".to_string(),
        RateLimitRetry {
            retry_count: 0,
            next_retry_at: Instant::now() - Duration::from_secs(1),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert_eq!(
        tracks["test-agent"].retry_count, 1,
        "retry_count must increment after inject"
    );

    let mut buf = vec![0u8; 256];
    use std::io::Read;
    let n = reader.read(&mut buf).expect("read from PTY");
    let captured = String::from_utf8_lossy(&buf[..n]);
    assert!(
        captured.contains("continue"),
        "PTY must receive \"continue\" payload, got: {:?}",
        captured.trim_end_matches('\0')
    );
    // #1769: the ServerRateLimit auto-retry inject is tagged so an
    // orchestrator can tell it apart from a real operator "continue".
    assert!(
        captured.contains("[AGEND-AUTO kind=ratelimit-retry]"),
        "#1769: daemon auto-inject must carry the [AGEND-AUTO kind=...] marker, got: {:?}",
        captured.trim_end_matches('\0')
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn retry_loop_does_not_restart_after_max_exceeded() {
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "agent-loop".into(),
        RateLimitRetry {
            retry_count: 4,
            next_retry_at: std::time::Instant::now(),
            exhausted: true,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    assert!(tracks.contains_key("agent-loop"));
    assert!(tracks["agent-loop"].exhausted);
}

/// #1470 (slice): a retry track for an agent no longer in the registry
/// (killed / restarted / deleted) is dropped — the map can't grow unbounded
/// across agent churn.
#[test]
fn retry_track_cleared_when_agent_removed_from_registry() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("slice-clear-removed-agent");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "ghost-agent".to_string(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: Instant::now() + Duration::from_secs(60),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );

    // Empty registry → the agent is gone → its track must be reaped.
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        !tracks.contains_key("ghost-agent"),
        "retry track must be cleared when the agent is no longer in the registry"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1470 (slice): when auto-retry is exhausted, the agent's team
/// orchestrator is notified via its INBOX (not operator Telegram).
#[test]
fn retry_exhaustion_notifies_orchestrator_inbox() {
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let home = tmp_home("slice-exhaustion-notify");
    std::fs::create_dir_all(home.join("inbox")).ok();

    // Agent stays in ServerRateLimit so Phase 1 keeps its seeded track
    // (a productive state would clear it via clears_server_rate_limit_retry).
    let (handle, _reader) =
        mock_agent_handle("worker-x", crate::state::AgentState::ServerRateLimit);
    let agent_id = handle.id;
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "teams:\n  team-x:\n    members: [orch-x, worker-x]\n    orchestrator: orch-x\n\
                 instances:\n  worker-x:\n    id: {}\n",
            agent_id.full()
        ),
    )
    .ok();
    registry.lock().insert(agent_id, handle);

    // Seed at MAX so the next increment exceeds it → exhaustion branch.
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "worker-x".to_string(),
        RateLimitRetry {
            retry_count: super::SERVER_RATE_LIMIT_MAX_RETRIES,
            next_retry_at: Instant::now() - Duration::from_secs(1),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );

    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );

    assert!(
        tracks["worker-x"].exhausted,
        "track must be marked exhausted after exceeding max retries"
    );
    let orch_inbox = home.join("inbox").join("orch-x.jsonl");
    let content = std::fs::read_to_string(&orch_inbox).unwrap_or_default();
    assert!(
        content.contains("member_retry_exhausted") && content.contains("worker-x"),
        "orchestrator inbox must carry the retry-exhaustion notice, got: {content}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #1742: ServerRateLimit inject-failure handling (no silent exhaust) ──

/// #1742 (pure): a transient inject failure self-heals, so fewer than
/// `MAX_INJECT_FAILURES` consecutive failures keep retrying; only the Nth
/// back-to-back failure exhausts (and the caller routes THAT through the full
/// notification path). This is the unit gate for the InjectFailed branch,
/// which is otherwise hard to drive (a PTY write failing while the agent is
/// still present can't be cheaply mocked) — see PR notes.
#[test]
fn classify_inject_failure_exhausts_only_after_max_1742() {
    use super::{classify_inject_failure, InjectFailAction, MAX_INJECT_FAILURES};
    assert_eq!(
        MAX_INJECT_FAILURES, 3,
        "design-pinned: give up after 3 fails"
    );
    for n in 0..MAX_INJECT_FAILURES {
        assert_eq!(
                classify_inject_failure(n),
                InjectFailAction::RetrySoon,
                "#1742: {n} consecutive failures (< {MAX_INJECT_FAILURES}) must keep retrying, not exhaust"
            );
    }
    for n in MAX_INJECT_FAILURES..(MAX_INJECT_FAILURES + 3) {
        assert_eq!(
            classify_inject_failure(n),
            InjectFailAction::Exhaust,
            "#1742: {n} consecutive failures (>= {MAX_INJECT_FAILURES}) must exhaust"
        );
    }
}

/// #1742 regression (the silent-drop bug): a due ServerRateLimit track whose
/// inject hits `AgentGone` (the agent vanished between the Phase-1 decision and
/// the Phase-2 PTY write — here modelled by an unresolvable name: in the
/// registry but absent from fleet.yaml, so `resolve_uuid` returns None) must
/// NOT be marked exhausted and must NOT consume a retry. The track is left for
/// the next-tick `retain` to reap. Pre-#1742 this set `exhausted=true` with a
/// bare warn — permanently disabling auto-recovery with no notification.
#[test]
fn srl_inject_agent_gone_does_not_exhaust_1742() {
    let home = tmp_home("1742-agent-gone");
    // Registry has the agent in ServerRateLimit, but NO fleet.yaml mapping →
    // Phase 1 schedules it (it iterates the registry directly), Phase 2's
    // resolve_uuid misses → InjectOutcome::AgentGone.
    let (handle, _reader) = mock_agent_handle("gone-x", crate::state::AgentState::ServerRateLimit);
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    registry.lock().insert(handle.id, handle);

    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "gone-x".to_string(),
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now() - Duration::from_secs(1), // due
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );

    let t = tracks
            .get("gone-x")
            .expect("#1742: track must survive an AgentGone tick (reaped only once the agent leaves the registry)");
    assert!(
        !t.exhausted,
        "#1742: AgentGone must NOT silently exhaust the retry track"
    );
    assert_eq!(
            t.retry_count, 2,
            "#1742: a no-op AgentGone tick must roll back the pre-counted attempt (retry_count == successful injects)"
        );
    assert!(
        t.inject_failures == 0,
        "#1742: AgentGone is not a present-agent inject failure → no failure-streak bump"
    );
    assert!(
        last_inject.is_empty(),
        "#1742: nothing was injected (no PTY write happened)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t-...14440-6 caller-level integration pin: `InjectOutcome::InjectFailed`
/// was previously "hard to drive" (see `classify_inject_failure_exhausts_
/// only_after_max_1742`'s doc comment above — "a PTY write failing while the
/// agent is still present can't be cheaply mocked"). Post-#2620, it can: spawn
/// a REAL agent via `spawn_agent` (so `write_actor::register` runs exactly as
/// in production) onto a permanently-wedged backend (`stty raw -echo; sleep
/// 30` — never drains stdin), saturate its actor queue, then drive
/// `inject_continue_gated` for real. Confirms the 3-state enum's third state
/// is reachable through a genuine failure, not just constructible in a match
/// arm. Unix-only (real PTY wedge semantics; mirrors the other real-PTY
/// inject tests in this file).
#[test]
#[cfg(not(target_os = "windows"))]
fn srl_inject_failed_reachable_via_real_wedged_actor_writer_2620() {
    let home = tmp_home("inject-failed-2620");
    let name = "wedged-continue-target";
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  {name}:\n    id: {}\n",
            crate::types::InstanceId::new().full()
        ),
    )
    .expect("seed fleet.yaml");

    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let wedge_args = vec!["-c".to_string(), "stty raw -echo; sleep 30".to_string()];
    let spawn_cfg = crate::agent::SpawnConfig {
        name,
        backend: None,
        backend_command: "sh",
        args: &wedge_args,
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    crate::agent::spawn_agent(&spawn_cfg, &registry).expect("spawn wedged test agent");
    // Let `stty raw -echo` take effect — mirrors write_actor.rs's own
    // `wedged_pty()` fixture's post-spawn wait.
    std::thread::sleep(Duration::from_millis(300));

    let pty_writer = {
        let reg = crate::agent::lock_registry(&registry);
        let id = crate::fleet::resolve_uuid(&home, name).expect("fleet.yaml seeded id");
        let handle = reg.get(&id).expect("spawned handle must be present");
        Arc::clone(&handle.pty_writer)
    };
    // Saturate the queue (write_actor.rs's MAX_QUEUE_BYTES_PER_WRITER, 1 MiB
    // at time of writing — duplicated here by value, private to that module)
    // so `inject_continue_gated`'s own write below genuinely fails.
    let priming_result = crate::agent::write_to_pty(&pty_writer, &vec![b'x'; 1 << 20]);
    assert!(
        priming_result.is_err(),
        "test invariant: the priming write itself must also see a saturated/wedged queue"
    );

    let outcome = inject_continue_gated(&home, &registry, name, "test-2620");
    assert!(
        matches!(outcome, InjectOutcome::InjectFailed),
        "a genuinely-present agent whose PTY write fails must report InjectFailed \
         (distinct from AgentGone) — got {outcome:?}"
    );

    if let Some(handle) = crate::agent::lock_registry(&registry)
        .get(&crate::fleet::resolve_uuid(&home, name).expect("id"))
    {
        let _ = handle.child.lock().kill();
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #1742: a SUCCESSFUL inject clears any accumulated failure streak and
/// advances the tiered budget — so a recovered PTY blip doesn't leave the
/// track one failure away from giving up. Unix-only (mirrors the other PTY
/// inject tests: the Windows mock PTY doesn't accept the write the same way).
#[test]
#[cfg(not(target_os = "windows"))]
fn srl_successful_inject_resets_failure_streak_1742() {
    let (home, registry, _reader) = one_agent_registry(
        "ok-x",
        crate::state::AgentState::ServerRateLimit,
        "1742-reset-streak",
    );
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "ok-x".to_string(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: Instant::now() - Duration::from_secs(1), // due
            exhausted: false,
            inject_failures: 2, // one short of MAX_INJECT_FAILURES
            abort_pending: false,
        },
    );
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        &mut Default::default(),
        past_boot_grace(),
    );
    let t = tracks
        .get("ok-x")
        .expect("track persists after a successful inject");
    assert!(!t.exhausted, "#1742: a successful inject must not exhaust");
    assert_eq!(
        t.inject_failures, 0,
        "#1742: a successful inject must reset the consecutive-failure streak"
    );
    assert_eq!(
        t.retry_count, 2,
        "#1742: a real inject advances the tiered budget (1 → 2)"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn retry_resumes_after_recovery_then_new_failure() {
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "agent-recover".into(),
        RateLimitRetry {
            retry_count: 4,
            next_retry_at: std::time::Instant::now(),
            exhausted: true,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    tracks.remove("agent-recover");
    assert!(!tracks.contains_key("agent-recover"));
    tracks.insert(
        "agent-recover".into(),
        RateLimitRetry {
            retry_count: 0,
            next_retry_at: std::time::Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    assert_eq!(tracks["agent-recover"].retry_count, 0);
    assert!(!tracks["agent-recover"].exhausted);
}

#[test]
fn retry_does_not_count_state_persistence_as_new_failure() {
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    tracks.insert(
        "agent-persist".into(),
        RateLimitRetry {
            retry_count: 1,
            next_retry_at: std::time::Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    for _ in 0..30 {
        assert!(tracks.contains_key("agent-persist"));
    }
    assert_eq!(tracks.len(), 1);
}

// ─── Sprint 54 P2-3: pane-input-not-submitted detection tests ───

/// Helper: minimal `UxEventSink` that records every emitted event
/// in-memory so the supervisor's emission can be asserted without
/// standing up a real channel adapter.
struct TestSink {
    events: parking_lot::Mutex<Vec<crate::channel::ux_event::UxEvent>>,
}
impl crate::channel::ux_event::UxEventSink for TestSink {
    fn emit(&self, event: &crate::channel::ux_event::UxEvent) {
        self.events.lock().push(event.clone());
    }
}

/// Helper: stand up `home/fleet.yaml` declaring `agent_name` with the
/// chosen backend command, then return `home`. Used by the
/// pane-input-not-submitted suite so `pane_input_backend_supported`
/// resolves the agent against a real fleet config.
fn fleet_with_backend(tag: &str, agent_name: &str, backend_cmd: &str) -> std::path::PathBuf {
    let home = tmp_home(tag);
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  {agent_name}:\n    backend: {backend_cmd}\n    \
                 working_directory: \"/tmp\"\n"
        ),
    )
    .expect("write fleet.yaml");
    home
}

/// Helper: pre-populate the agent's metadata with a typed timestamp
/// older than `now - threshold_secs` and a (possibly absent) submit
/// timestamp. Bypasses `record_input_activity` / `record_submit_activity`
/// so tests can set arbitrary epoch-ms values.
fn seed_input_submit(home: &std::path::Path, agent: &str, typed_ms: i64, submit_ms: i64) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let mut meta = serde_json::Map::new();
    if typed_ms > 0 {
        meta.insert("last_input_epoch_ms".into(), serde_json::json!(typed_ms));
    }
    if submit_ms > 0 {
        meta.insert("last_submit_epoch_ms".into(), serde_json::json!(submit_ms));
    }
    std::fs::write(
        meta_dir.join(format!("{agent}.json")),
        serde_json::to_string_pretty(&serde_json::Value::Object(meta)).expect("serialize"),
    )
    .expect("write metadata");
}

/// A `loop_started_at` far enough in the past that `in_boot_grace` is
/// false, so the #1741 boot-grace gate added to
/// `check_pane_input_not_submitted_for_agents` lets the detection run.
/// Mirrors the `past` helper in per_tick/{poll_reminder,inbox_stuck,
/// handoff_timeout}.rs boot-grace tests.
fn past_boot_grace() -> Instant {
    Instant::now() - crate::daemon::per_tick::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)
}

#[test]
fn pane_input_not_submitted_emits_event_when_threshold_exceeded() {
    // Per-test unique agent name avoids cross-test sink_registry
    // contamination (cargo test runs in parallel; the global sink
    // registry sees emissions from every test concurrently).
    let agent = "claude-agent-pin-emit";
    let home = fleet_with_backend("pin_emit", agent, "claude");
    // Typed 5 minutes ago, never submitted → must emit.
    let now_ms = chrono::Utc::now().timestamp_millis();
    seed_input_submit(&home, agent, now_ms - 300_000, 0);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let events = sink.events.lock();
    let matched = events.iter().filter_map(|e| match e {
        crate::channel::ux_event::UxEvent::Fleet(
            crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
        ) if emitted == agent => Some(()),
        _ => None,
    });
    assert!(
        matched.count() >= 1,
        "expected ≥1 PaneInputNotSubmitted event for {agent}, got: {events:?}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pane_input_not_submitted_skips_when_within_threshold() {
    let agent = "claude-agent-pin-within";
    let home = fleet_with_backend("pin_within", agent, "claude");
    // Typed 5s ago — well within default 60s threshold → no emit.
    let now_ms = chrono::Utc::now().timestamp_millis();
    seed_input_submit(&home, agent, now_ms - 5_000, 0);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let events = sink.events.lock();
    for e in events.iter() {
        if let crate::channel::ux_event::UxEvent::Fleet(
            crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
        ) = e
        {
            assert_ne!(emitted, agent, "must not emit within threshold");
        }
    }
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pane_input_not_submitted_skips_when_submit_caught_up() {
    let agent = "claude-agent-pin-submit";
    let home = fleet_with_backend("pin_submit", agent, "claude");
    // Typed 5min ago AND submitted 4min ago (submit > 0 and >= typed).
    let now_ms = chrono::Utc::now().timestamp_millis();
    seed_input_submit(&home, agent, now_ms - 300_000, now_ms - 240_000);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let events = sink.events.lock();
    for e in events.iter() {
        if let crate::channel::ux_event::UxEvent::Fleet(
            crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
        ) = e
        {
            assert_ne!(
                emitted, agent,
                "must not emit when submit timestamp >= typed"
            );
        }
    }
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pane_input_not_submitted_now_fires_for_non_claude_backend() {
    // #1457: submit detection widened from claude-only to ALL backends with
    // a submit key. kiro-cli (submit_key=`\r`) is now supported, so a
    // typed-but-not-submitted kiro pane MUST emit the diagnostic.
    let agent = "kiro-agent-pin-nonclaude";
    let home = fleet_with_backend("pin_nonclaude", agent, "kiro-cli");
    let now_ms = chrono::Utc::now().timestamp_millis();
    seed_input_submit(&home, agent, now_ms - 300_000, 0);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let events = sink.events.lock();
    let fired = events.iter().any(|e| {
        matches!(
            e,
            crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
            ) if emitted == agent
        )
    });
    assert!(
        fired,
        "non-claude backend with a submit key must now emit PaneInputNotSubmitted (#1457)"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pane_input_not_submitted_dedups_per_typed_timestamp() {
    let agent = "claude-agent-pin-dedup";
    let home = fleet_with_backend("pin_dedup", agent, "claude");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let typed_ms = now_ms - 300_000;
    seed_input_submit(&home, agent, typed_ms, 0);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    // Tick once → one emit. Tick again with same metadata → still one.
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let events = sink.events.lock();
    let count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::channel::ux_event::UxEvent::Fleet(
                        crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
                    ) if emitted == agent
                )
            })
            .count();
    assert_eq!(
        count, 1,
        "must dedup repeated ticks for same typed_ms; got {count}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn pane_input_not_submitted_suppressed_during_boot_grace() {
    // #1741: a daemon restart zeroes `pane_input_tracks`, so without the
    // boot-grace gate the diagnostic re-fires on the first ticks for an
    // input typed BEFORE the restart (a pre-existing operator draft the
    // detector cannot tell apart from a fresh strand). A `loop_started_at`
    // still within NOTIFICATION_BOOT_GRACE must suppress the emit AND leave
    // the dedup map untouched; once the grace elapses the same
    // still-stranded input emits exactly once.
    let agent = "claude-agent-pin-bootgrace";
    let home = fleet_with_backend("pin_bootgrace", agent, "claude");
    let now_ms = chrono::Utc::now().timestamp_millis();
    seed_input_submit(&home, agent, now_ms - 300_000, 0);
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();

    // Within boot-grace (loop just started) → suppressed, dedup untouched.
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        Instant::now(),
    );
    let fired_in_grace = sink.events.lock().iter().any(|e| {
        matches!(
            e,
            crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
            ) if emitted == agent
        )
    });
    assert!(!fired_in_grace, "must NOT emit during boot-grace");
    assert!(
        !tracks.contains_key(agent),
        "boot-grace must skip the scan entirely (no dedup-map mutation)"
    );

    // After boot-grace elapsed → the still-stranded input emits once.
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
    );
    let count_after = sink
            .events
            .lock()
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    crate::channel::ux_event::UxEvent::Fleet(
                        crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: emitted, .. },
                    ) if emitted == agent
                )
            })
            .count();
    assert_eq!(count_after, 1, "must emit exactly once after grace ends");
    std::fs::remove_dir_all(home).ok();
}

/// #1125 M1 source-pin: supervisor's per-tick loop body MUST be
/// wrapped in `catch_unwind` so a panic in any tracker doesn't kill
/// the supervisor thread (silent loss of all health monitoring).
#[test]
fn supervisor_tick_loop_has_catch_unwind() {
    let src = include_str!("../supervisor.rs");
    let loop_start = src.find("fn run_loop(").expect("run_loop must exist");
    let rest = &src[loop_start..];
    assert!(
        rest.contains("catch_unwind"),
        "supervisor run_loop must wrap tick body in catch_unwind (#1125 M1)"
    );
}

/// #986 source-pin (INVERTED from #1002 Phase 2): the supervisor's per-tick
/// loop must NOT scan pr_state. The `PrStateScanHandler` per-tick handler is the
/// SINGLE scanner+worker in EVERY mode — it runs in `run_core`'s handler vec
/// (daemon) AND in `app::app_tick_handlers` (app standalone, attached AND owned,
/// since `pr_state_scan` is not in `APP_TICK_ALLOWLIST`). The #1002-era direct
/// supervisor scan was a vestigial belt from when the handler was run_core-only;
/// with the handler now live in every mode it was a redundant second scanner +
/// (post-#986) a second gh-poll worker. This pin guards against re-adding it.
#[test]
fn pr_state_scan_wired_into_supervisor_loop() {
    let source = std::fs::read_to_string("src/daemon/supervisor.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/supervisor.rs"))
        .expect("source file must be readable from test cwd");
    // #986: the supervisor loop must NOT scan pr_state. `PrStateScanHandler`
    // is the SINGLE scanner+worker in ALL modes (run_core handler vec + app
    // `app_tick_handlers`, both attached and owned). A supervisor scan would be
    // a redundant SECOND scanner + a SECOND gh-poll worker. Guard against
    // re-adding it. The needle is assembled from fragments so this assertion's
    // own source does not match (the file never contains the verbatim call).
    let needle = format!("{}{}", "scan_and", "_emit");
    assert!(
        !source.contains(&needle),
        "supervisor loop must NOT invoke a pr_state scan (#986: the handler is \
             the sole scanner+worker in every mode; a supervisor scan would double \
             both the scanner and the gh-poll worker)."
    );
}

// ── #1696 / #1697: tiered retry + ApiError quick-nudge ──

/// Build a registry with one agent at `state`, fleet.yaml seeded so the
/// name-keyed tracking resolves to the handle. Returns (home, registry, reader).
fn one_agent_registry(
    name: &str,
    state: crate::state::AgentState,
    tag: &str,
) -> (
    std::path::PathBuf,
    AgentRegistry,
    Box<dyn std::io::Read + Send>,
) {
    let home = tmp_home(tag);
    let (handle, reader) = mock_agent_handle(name, state);
    let id = handle.id;
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  {name}:\n    id: {}\n", id.full()),
    )
    .ok();
    let registry: AgentRegistry = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    registry.lock().insert(id, handle);
    (home, registry, reader)
}

/// #1713 (replaces the #1696 `suppress_thinking_clear` band-aid): a
/// continue-inject's transient Thinking must NOT clear the retry track (else
/// tiered Phase B/C would restart at Phase A) AND must not itself inject
/// (Thinking != ServerRateLimit). With #1713 this holds STRUCTURALLY — Thinking
/// is neither a clearing state ({Ready,Idle}) nor the decision state
/// (ServerRateLimit) — so no inject-cooldown suppression window is needed.
/// Tiered progress (retry_count) is preserved; the next ServerRateLimit
/// observation continues it.
#[test]
fn thinking_transient_keeps_track_and_progress_1713() {
    let (home, registry, _r) =
        one_agent_registry("ag", crate::state::AgentState::Active, "1713-thinking-keep");
    let mut tracks: HashMap<String, RateLimitRetry> = HashMap::new();
    // next_retry_at = now (DUE) — proves a due track still does NOT inject when
    // the fresh state is Thinking (the gate is state, not merely the timer).
    tracks.insert(
        "ag".into(),
        RateLimitRetry {
            retry_count: 4,
            next_retry_at: Instant::now(),
            exhausted: false,
            inject_failures: 0,
            abort_pending: false,
        },
    );
    let mut last_inject: HashMap<String, Instant> = HashMap::new();
    super::process_error_recovery(
        &home,
        &registry,
        &mut tracks,
        &mut Default::default(),
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        tracks.contains_key("ag"),
        "#1713: a Thinking transient must NOT clear the retry track"
    );
    assert_eq!(
        tracks["ag"].retry_count, 4,
        "#1713: tiered retry progress preserved (no Phase-A restart)"
    );
    assert!(
        last_inject.is_empty(),
        "#1713: Thinking is not ServerRateLimit → no `continue` inject even when due"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1697: an ApiError-at-prompt agent gets an immediate `continue` nudge, ONCE
/// per episode (no re-nudge while still in the same ApiError episode).
// Reads the injected payload back off the PTY — Windows' mock PTY (`cmd
// findstr`) doesn't echo like unix `cat`, so this is unix-only, mirroring the
// existing `phase2_injects_continue_to_pty` gate.
#[test]
#[cfg(not(target_os = "windows"))]
fn apierror_at_prompt_quick_nudge_once_per_episode_1697() {
    let (home, registry, mut reader) =
        one_agent_registry("ag", crate::state::AgentState::ApiError, "apierror-nudge");
    let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    super::process_error_recovery(
        &home,
        &registry,
        &mut Default::default(),
        &mut episodes,
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        episodes.contains("ag"),
        "#1697: ApiError episode must be marked nudged"
    );
    let mut buf = vec![0u8; 256];
    use std::io::Read;
    let n = reader.read(&mut buf).expect("read from PTY");
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("continue"),
        "#1697: ApiError nudge must inject \"continue\""
    );

    // Second tick, STILL ApiError + in episode → no re-nudge.
    let before = last_inject.get("ag").copied();
    super::process_error_recovery(
        &home,
        &registry,
        &mut Default::default(),
        &mut episodes,
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert_eq!(
        last_inject.get("ag").copied(),
        before,
        "#1697: must not re-nudge within the same ApiError episode"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn apierror_nudge_caps_per_flicker_window_1742_f4() {
    // #1742-F4: a content-FP `ApiError↔Thinking` flicker re-arms the
    // per-episode dedup every cycle, so without a total cap the quick-nudge
    // injects indefinitely (bounded only by MIN_INTERVAL). Simulate the
    // flicker by clearing `episodes` (re-armed) + `last_inject` (>MIN_INTERVAL
    // elapsed) each cycle while the agent stays ApiError, and assert the nudge
    // count caps at APIERROR_NUDGE_MAX instead of growing unbounded.
    let (home, registry, _reader) =
        one_agent_registry("ag", crate::state::AgentState::ApiError, "apierror-cap");
    let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    for _ in 0..(super::APIERROR_NUDGE_MAX + 3) {
        episodes.clear(); // flicker → next ApiError counts as a "new episode"
        last_inject.clear(); // >CONTINUE_INJECT_MIN_INTERVAL elapsed
        super::process_error_recovery(
            &home,
            &registry,
            &mut Default::default(),
            &mut episodes,
            &mut counts,
            &mut last_inject,
            &mut Default::default(),
            past_boot_grace(),
        );
    }

    // The first APIERROR_NUDGE_MAX cycles nudge (proving a single ApiError
    // still nudges); the rest are capped — so the count is exactly the cap,
    // not APIERROR_NUDGE_MAX + 3. (Negative-probe: drop the `!capped` gate and
    // this reaches APIERROR_NUDGE_MAX + 3.)
    assert_eq!(
        counts.get("ag").copied(),
        Some(super::APIERROR_NUDGE_MAX),
        "#1742-F4: ApiError nudge count must cap at APIERROR_NUDGE_MAX despite \
             continued flicker"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn resolve_pending_auth_holds_fire_during_boot_grace_1741() {
    use super::{resolve_pending_auth, AuthErrorGate, PendingAuthError};
    let entry = || PendingAuthError {
        from: crate::state::AgentState::Idle,
        pane_tail: String::new(),
    };

    // Fire WITHIN boot-grace → held: pending KEPT, nothing fired (the
    // confirm-window is preserved; it fires once the grace ends).
    let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
    pending.insert("ag".into(), entry());
    assert!(
        resolve_pending_auth(AuthErrorGate::Fire, true, "ag", &mut pending).is_none(),
        "#1741: Fire during boot-grace must NOT fire"
    );
    assert!(
        pending.contains_key("ag"),
        "#1741: Fire during boot-grace must KEEP pending (no lost notify)"
    );

    // Fire AFTER boot-grace → fires: entry returned + removed from pending.
    assert!(
        resolve_pending_auth(AuthErrorGate::Fire, false, "ag", &mut pending).is_some(),
        "#1741: Fire after grace must fire"
    );
    assert!(
        !pending.contains_key("ag"),
        "#1741: Fire after grace must remove pending"
    );

    // Cancel → drop pending, never fires (boot-grace irrelevant).
    let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
    pending.insert("ag".into(), entry());
    assert!(resolve_pending_auth(AuthErrorGate::Cancel, true, "ag", &mut pending).is_none());
    assert!(
        !pending.contains_key("ag"),
        "Cancel must drop the self-healed pending entry"
    );

    // Wait → keep pending, never fires.
    let mut pending: HashMap<String, PendingAuthError> = HashMap::new();
    pending.insert("ag".into(), entry());
    assert!(resolve_pending_auth(AuthErrorGate::Wait, false, "ag", &mut pending).is_none());
    assert!(pending.contains_key("ag"), "Wait must keep pending");
}

#[test]
#[cfg(not(target_os = "windows"))]
// Mirrors #1697's gate: the one_agent_registry PTY/inject path (the post-grace
// `reader.read(...).contains("continue")` assertion) doesn't work under
// Windows conpty. The boot-grace logic itself is platform-agnostic and the
// pure `resolve_pending_auth` test covers the confirm-window path on all OSes.
fn apierror_nudge_suppressed_during_boot_grace_1741() {
    let (home, registry, mut reader) = one_agent_registry(
        "ag",
        crate::state::AgentState::ApiError,
        "apierror-bootgrace-1741",
    );
    let mut episodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_inject: HashMap<String, Instant> = HashMap::new();

    // WITHIN boot-grace (loop just started) → no nudge queued, episode UNMARKED
    // (so a still-ApiError agent gets a fresh nudge after grace, not a phantom
    // "already nudged" mark).
    super::process_error_recovery(
        &home,
        &registry,
        &mut Default::default(),
        &mut episodes,
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        Instant::now(),
    );
    assert!(
        !episodes.contains("ag"),
        "#1741: boot-grace must NOT mark the ApiError episode"
    );
    assert!(
        !last_inject.contains_key("ag"),
        "#1741: boot-grace must suppress the ApiError nudge"
    );

    // AFTER boot-grace → still ApiError → fresh nudge fires + episode marked.
    super::process_error_recovery(
        &home,
        &registry,
        &mut Default::default(),
        &mut episodes,
        &mut Default::default(),
        &mut last_inject,
        &mut Default::default(),
        past_boot_grace(),
    );
    assert!(
        episodes.contains("ag"),
        "#1741: after grace, a still-ApiError agent must be nudged fresh"
    );
    let mut buf = vec![0u8; 256];
    use std::io::Read;
    let n = reader.read(&mut buf).expect("read from PTY");
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("continue"),
        "#1741: post-grace ApiError nudge must inject \"continue\""
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1680 regression (source guard): the shared continue-inject MUST pass
/// `force=false` so it routes through `should_defer_direct_inject` and defers
/// while the operator is typing — never clobbering a half-typed draft. Pins the
/// fix of the pre-existing force-true retry inject.
#[test]
fn continue_inject_is_draft_gated_force_false_1680() {
    let src = include_str!("../supervisor.rs");
    // #1769: the call is now multi-line (gained the `auto_kind` arg), so
    // normalize whitespace before substring-matching the arg order.
    let norm = src.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        norm.contains("CONTINUE_RETRY_PAYLOAD, false, Some(auto_kind),"),
        "#1680: the continue-inject must pass force=false (draft-gated); \
             #1769: and the daemon-auto marker (auto_kind)"
    );
    // Split needle so this assertion's own text can't false-match the source.
    let force_true = format!("CONTINUE_RETRY_PAYLOAD,{}true", " ");
    assert!(
        !norm.contains(&force_true),
        "#1680: no force=true continue-inject may remain"
    );
    // #2232: the ratelimit-retry inject (sibling payload; #26795-3 dropped its
    // self-clear guidance) must ALSO be draft-gated (force=false) — same #1680
    // safety on the new path.
    assert!(
        norm.contains("RATELIMIT_RETRY_PAYLOAD, false, Some(auto_kind),"),
        "#2232: the ratelimit-retry inject must pass force=false (draft-gated)"
    );
    let rl_force_true = format!("RATELIMIT_RETRY_PAYLOAD,{}true", " ");
    assert!(
        !norm.contains(&rl_force_true),
        "#2232: no force=true ratelimit-retry inject may remain"
    );
}

/// #26795-3 ② (payload copy): the ServerRateLimit auto-retry payload is now a
/// PLAIN `continue` nudge — the ineffective self-clear instruction (agents never
/// called it: recovery_shadow 2746/2746 `self_cleared=false`; recovery is now
/// carried by hook_recovered + the recovered_within heuristic) is gone. The
/// `[AGEND-AUTO kind=ratelimit-retry]` marker is driven by `auto_kind`, NOT this
/// body, so the marker contract is unchanged.
#[test]
fn ratelimit_retry_payload_is_plain_continue_no_self_clear_26795_3() {
    let payload = std::str::from_utf8(super::RATELIMIT_RETRY_PAYLOAD).expect("ASCII payload");
    assert!(
        !payload.contains("clear_blocked_reason"),
        "the ineffective self-clear instruction must be gone (agents never called it): {payload:?}"
    );
    assert_eq!(
        super::RATELIMIT_RETRY_PAYLOAD,
        b"continue\n",
        "ratelimit-retry payload is now the plain continue nudge"
    );
    // Single-line submit contract: exactly one trailing newline, no embedded one
    // that would submit the nudge early.
    assert_eq!(
        payload.matches('\n').count(),
        1,
        "exactly one trailing newline"
    );
    assert!(payload.ends_with('\n'));
}

/// #1595 Step 1 (source guard): the ServerRateLimit retry-exhausted Telegram
/// notify MUST be `Error` (not Warn) so it breaks through the #1594 Sleep-mode
/// gate. The gate's Error-passes-Sleep / Warn-suppressed policy is pinned by
/// `channel::tests::should_notify_in_mode_policy_grid`; this pins the producer
/// side — that exhaustion (the full #1696 tiered budget burned) escalates to a
/// P0 that wakes a sleeping operator instead of being silently dropped.
#[test]
fn server_rate_limit_exhausted_notify_is_error_severity_1595() {
    let src = include_str!("../supervisor.rs");
    // Window the exhaust branch (production), well away from this test body.
    let idx = src
        .find("auto-retry exhausted")
        .expect("exhaust notice present in source");
    let window = &src[idx..(idx + 1200).min(src.len())];
    assert!(
        window.contains("NotifySeverity::Error"),
        "#1595: the ServerRateLimit-exhausted gated_notify must use Error severity"
    );
    assert!(
        !window.contains("NotifySeverity::Warn"),
        "#1595: the exhaust notify must not remain Warn (suppressed in Sleep)"
    );
}

/// #2937: `check_pane_input_not_submitted_for_agents` must use the
/// pre-computed `input_timestamps` map rather than re-reading from disk.
/// RED: the function does not yet accept the parameter — compile error.
#[test]
fn pane_input_uses_precomputed_timestamps_not_disk_reread() {
    let agent = "pin-precomputed";
    let home = fleet_with_backend("precomputed", agent, "claude");
    let now_ms = chrono::Utc::now().timestamp_millis();
    // Disk: typed 5min ago AND already submitted → no detection.
    seed_input_submit(&home, agent, now_ms - 300_000, now_ms - 10_000);
    // Pre-computed map: typed 5min ago, NOT submitted → should fire.
    let mut input_timestamps = HashMap::new();
    input_timestamps.insert(agent.to_string(), (now_ms - 300_000, 0i64));
    let sink = std::sync::Arc::new(TestSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });
    crate::channel::sink_registry::registry().register(sink.clone());
    let mut tracks: HashMap<String, PaneInputTrack> = HashMap::new();
    check_pane_input_not_submitted_for_agents(
        &home,
        &[agent.to_string()],
        &mut tracks,
        past_boot_grace(),
        &input_timestamps,
    );
    let emitted = sink.events.lock().iter().any(|e| {
        matches!(
            e,
            crate::channel::ux_event::UxEvent::Fleet(
                crate::channel::ux_event::FleetEvent::PaneInputNotSubmitted { agent: a, .. },
            ) if a == agent
        )
    });
    assert!(
        emitted,
        "must use pre-computed timestamps (submit=0 → fire), not re-read from disk (submit>0 → no fire)"
    );
    std::fs::remove_dir_all(home).ok();
}
