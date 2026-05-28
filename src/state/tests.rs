use super::anchor::{find_subslice, ANCHOR_WINDOW_MS};
use super::patterns::is_generic_startup_prompt;
use super::*;
use crate::health::HealthTracker;

/// Create a tracker with a given current state and elapsed time since that state began.
fn tracker_at(backend: &Backend, state: AgentState, elapsed_secs: u64) -> StateTracker {
    let mut t = StateTracker::new(Some(backend));
    t.current = state;
    t.since = Instant::now() - Duration::from_secs(elapsed_secs);
    t
}

// ── False-positive regression pins ──────────────────────────────────
//
// Operator reported two misfires:
//   1. `shell` flagged with "⚠️ shell 靜默 38s，可能卡在互動 prompt" while
//      sitting at a normal `❯` prompt.
//   2. `dev-reviewer` (Codex) flagged with "卡在互動 prompt" after
//      dismissing a transient banner — the state never recovered
//      because hash-dedup suppressed re-detection.

// Sprint 31+ #4: rate-limit regex false-positive on benign tokens.
// Operator m-2 quick fix (Option A): word-boundary the `429` token to
// avoid matching substrings like "build #4290" / "request id: 4291abc"
// / "response time: 4290ms". `\b429\b` ensures `429` is a standalone
// token (preceded + followed by non-word boundary).
#[test]
fn rate_limit_regex_does_not_misfire_on_build_number_4290() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("CI: build #4290 succeeded in 12s");
    assert_ne!(
        t.get_state(),
        AgentState::RateLimit,
        "benign build number `4290` must not trigger RateLimit"
    );
}

#[test]
fn rate_limit_regex_does_not_misfire_on_request_id_4291abc() {
    let mut t = tracker_at(&Backend::Codex, AgentState::Idle, 0);
    t.feed("request id: 4291abcdef");
    assert_ne!(
        t.get_state(),
        AgentState::RateLimit,
        "benign request id starting with `4291` must not trigger RateLimit"
    );
}

#[test]
fn rate_limit_regex_still_matches_real_429_token() {
    // Regression guard: the narrowed #848 pattern must still classify
    // the canonical Claude 429-rejection wording as RateLimit. Pre-#848
    // this test fed the casual `"API error: 429 Too Many Requests"`
    // form, which only passed because the OLD broad pattern matched
    // `\b429\b` as a substring. The narrowed pattern keys on the
    // verbatim Anthropic docs phrasing — `API Error: Request rejected
    // (429)` — so the test now feeds the canonical form.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: Request rejected (429) · this may be a temporary capacity issue");
    assert_eq!(
        t.get_state(),
        AgentState::RateLimit,
        "canonical Anthropic 429-rejection wording must still trigger RateLimit"
    );
}

// Sprint 46: codex InteractivePrompt regex (`Update available!|Press
// enter to continue`) false-positived on normal idle prompts because
// the `›` idle pattern and the InteractivePrompt pattern both matched
// transient output. Removed the codex InteractivePrompt entry entirely.
#[test]
fn codex_idle_prompt_does_not_trigger_interactive_prompt() {
    let mut t = tracker_at(&Backend::Codex, AgentState::Idle, 0);
    t.feed("› ");
    assert_ne!(
        t.get_state(),
        AgentState::InteractivePrompt,
        "codex idle prompt `›` must not trigger InteractivePrompt"
    );
}

#[test]
fn shell_backend_starts_in_ready_not_starting() {
    // Regression: Shell/Raw have no state pattern catalog, so `detect()`
    // can never produce Ready; they used to sit in `Starting` forever
    // and `check_awaiting_operator` fired on every idle shell after
    // 30 s. Initial state Ready sidesteps the silence fallback — a
    // truly-stuck shell still gets caught by `check_hang` later.
    let t = StateTracker::new(Some(&Backend::Shell));
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn raw_backend_starts_in_ready_not_starting() {
    let t = StateTracker::new(Some(&Backend::Raw("/opt/whatever".to_string())));
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn managed_backends_still_start_in_starting() {
    // Keep the handshake for real backends so their
    // onboarding / auth prompts have a chance to pattern-match before
    // we declare Ready.
    for backend in [
        Backend::ClaudeCode,
        Backend::KiroCli,
        Backend::Gemini,
        Backend::Codex,
        Backend::OpenCode,
    ] {
        let t = StateTracker::new(Some(&backend));
        assert_eq!(
            t.get_state(),
            AgentState::Starting,
            "managed backend {backend:?} must still start in Starting"
        );
    }
}

#[test]
fn interactive_prompt_expires_to_ready_after_two_minutes() {
    // `dev-reviewer`-style lockup: operator dismissed the dialog
    // out-of-band, screen went stable, hash-dedup meant `detect()`
    // never re-fired. With no re-detect and only
    // Thinking/ToolUse in the expiry list, the state was stuck
    // indefinitely. Ticking past INTERACTIVE_EXPIRY now drops to
    // Ready on its own.
    let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 119);
    t.tick();
    assert_eq!(
        t.get_state(),
        AgentState::InteractivePrompt,
        "must still be latched before expiry"
    );

    let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 121);
    t.tick();
    assert_eq!(
        t.get_state(),
        AgentState::Ready,
        "expected Ready after INTERACTIVE_EXPIRY, still {:?}",
        t.get_state()
    );
}

#[test]
fn permission_prompt_also_expires_to_ready() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 130);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn tool_use_still_uses_short_expiry() {
    // Regression guard against accidentally widening the short
    // expiry — Thinking / ToolUse should still drop at 30 s.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 31);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Ready);
}

// ── P0: Core behavior ───────────────────────────────────────────────

#[test]
#[allow(clippy::unwrap_used)]
fn error_state_instant_transition() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    // #848: pre-#848 this fed `"429 rate limit exceeded"` which only
    // matched because the OLD broad pattern keyed on `\b429\b` /
    // `rate.?limit` substring. The narrowed pattern requires canonical
    // Anthropic wording. The test's intent (error transitions instant,
    // no hysteresis) is unchanged — only the feed string is canonicalized.
    t.feed("API Error: Request rejected (429) · this may be a temporary capacity issue");
    assert_eq!(t.get_state(), AgentState::RateLimit);
}

#[test]
fn active_to_passive_needs_hold() {
    let backend = Backend::ClaudeCode;

    // Thinking held for only 1s — transition to Idle should NOT happen
    let mut t = tracker_at(&backend, AgentState::Thinking, 1);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Thinking);

    // Thinking held for 3s (>= 2s active hold) — transition to Idle SHOULD happen
    let mut t = tracker_at(&backend, AgentState::Thinking, 3);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn passive_to_passive_needs_5s() {
    let backend = Backend::ClaudeCode;

    // Idle(3) → Ready(2): lower priority, passive hold = 5s
    // Idle held for 3s — should NOT transition
    let mut t = tracker_at(&backend, AgentState::Idle, 3);
    t.transition(AgentState::Ready);
    assert_eq!(t.get_state(), AgentState::Idle);

    // Idle held for 6s (>= 5s passive hold) — SHOULD transition
    let mut t = tracker_at(&backend, AgentState::Idle, 6);
    t.transition(AgentState::Ready);
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn higher_priority_instant() {
    let backend = Backend::ClaudeCode;

    // Idle → Thinking: higher priority, should transition immediately even at 0s
    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::Thinking);
    assert_eq!(t.get_state(), AgentState::Thinking);
}

// ── #1005 Phase A2: oscillation guard ─────────────────────────────────
//
// All tests in this section read `oscillation_guard_window()` which
// peeks `AGEND_OSCILLATION_GUARD_WINDOW_SECS`. The env-disable test
// (oscillation_guard_window_env_disable) flips the var temporarily,
// so every other test in this section is marked `#[serial]` to
// prevent cross-test env-var bleed when cargo test runs them in
// parallel.

/// Phase A2 core invariant: when a priority-up to active state X is
/// followed within `OSCILLATION_LOWER_HOLD_THRESHOLD` (5s) by another
/// priority-up to the SAME X, the second one is SUPPRESSED. Without
/// the guard the cycle `ToolUse(2s) → Idle(2s) → ToolUse(2s)` keeps
/// `since` recent and `LATCHED_STATE_EXPIRY` (30s) never fires —
/// tracker stays stuck on ToolUse indefinitely (the #1005 surface).
#[test]
#[serial_test::serial]
fn oscillation_guard_suppresses_quick_bounce_to_same_active_state() {
    let backend = Backend::ClaudeCode;

    // Step 1: legitimate Idle → ToolUse priority-up (first entry,
    // guard unarmed). Guard records `(ToolUse, t0)` on success.
    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::ToolUse);
    assert_eq!(t.get_state(), AgentState::ToolUse);
    assert!(t.last_priority_up_into.is_some());

    // Step 2: ToolUse held 3s (≥ 2s active min_hold). Pattern
    // detects Idle (lower priority) — natural priority-down fires.
    t.since = Instant::now() - Duration::from_secs(3);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);

    // Step 3: Idle held only 1s (< 5s OSCILLATION_LOWER_HOLD_THRESHOLD).
    // ToolUse pattern matches again (scrollback `✓ Bash` banner).
    // Guard MUST suppress the priority-up — operator sees tracker
    // settle in Idle instead of bouncing back into ToolUse.
    t.since = Instant::now() - Duration::from_secs(1);
    t.transition(AgentState::ToolUse);
    assert_eq!(
        t.get_state(),
        AgentState::Idle,
        "#1005 A2: priority-up to ToolUse within 5s of Idle entry MUST be suppressed"
    );
}

/// Phase A2 negative-pin: when the lower state was held for ≥ 5s,
/// the next priority-up is LEGITIMATE (operator was idle then
/// resumed real work) and MUST fire normally. Distinguishes the
/// bounce-cycle from natural activity gaps.
#[test]
#[serial_test::serial]
fn oscillation_guard_does_not_suppress_legitimate_re_entry() {
    let backend = Backend::ClaudeCode;

    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::ToolUse);
    // Held 3s, then natural drop to Idle
    t.since = Instant::now() - Duration::from_secs(3);
    t.transition(AgentState::Idle);
    // Held Idle for ≥ 5s — legitimate work pause
    t.since = Instant::now() - Duration::from_secs(6);
    t.transition(AgentState::ToolUse);
    assert_eq!(
        t.get_state(),
        AgentState::ToolUse,
        "#1005 A2: priority-up after ≥ 5s lower-state hold is legitimate, must fire"
    );
}

/// Phase A2 window expiry: outside `oscillation_guard_window()`
/// (default 30s), the prior priority-up record is stale and the
/// guard no longer applies — the original problem space already
/// elapsed naturally.
#[test]
#[serial_test::serial]
fn oscillation_guard_does_not_suppress_after_window() {
    let backend = Backend::ClaudeCode;

    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::ToolUse);
    // Manually age the priority-up record past the window
    let stale = Instant::now() - Duration::from_secs(35);
    t.last_priority_up_into = Some((AgentState::ToolUse, stale));
    // Drop to Idle, hold briefly, try priority-up again
    t.current = AgentState::Idle;
    t.since = Instant::now() - Duration::from_secs(1);
    t.transition(AgentState::ToolUse);
    assert_eq!(
        t.get_state(),
        AgentState::ToolUse,
        "#1005 A2: stale priority-up record (>30s old) must not suppress new re-entry"
    );
}

/// Phase A2 cross-state independence: a priority-up to a DIFFERENT
/// active state isn't a bounce — bouncing between Thinking and
/// ToolUse is legitimate task progression, not oscillation.
#[test]
#[serial_test::serial]
fn oscillation_guard_does_not_suppress_different_active_state() {
    let backend = Backend::ClaudeCode;

    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::ToolUse);
    t.since = Instant::now() - Duration::from_secs(3);
    t.transition(AgentState::Idle);
    t.since = Instant::now() - Duration::from_secs(1);
    // Different active state — Thinking, not ToolUse
    t.transition(AgentState::Thinking);
    assert_eq!(
        t.get_state(),
        AgentState::Thinking,
        "#1005 A2: priority-up to a DIFFERENT active state must not be suppressed"
    );
}

/// Phase A2 multi-tick simulation: the #1005 issue's actual cycle
/// pattern. Without the guard, this loop sticks at ToolUse forever
/// (LATCHED_STATE_EXPIRY never reachable because `since` resets on
/// every bounce). With the guard, the second priority-up to
/// ToolUse is suppressed and the tracker settles in Idle.
#[test]
#[serial_test::serial]
fn oscillation_guard_multi_cycle_settles_in_idle() {
    let backend = Backend::ClaudeCode;
    let mut t = tracker_at(&backend, AgentState::Idle, 0);

    // Cycle 1: Idle → ToolUse → Idle (each leg held briefly)
    t.transition(AgentState::ToolUse);
    t.since = Instant::now() - Duration::from_secs(2);
    t.transition(AgentState::Idle);
    t.since = Instant::now() - Duration::from_secs(2);

    // Cycle 2: try to re-enter ToolUse (bounce) — must be suppressed
    t.transition(AgentState::ToolUse);
    assert_eq!(t.get_state(), AgentState::Idle, "cycle 2 bounce");

    // Cycle 3: more attempts — still suppressed while within window
    t.since = Instant::now() - Duration::from_secs(2);
    t.transition(AgentState::ToolUse);
    assert_eq!(t.get_state(), AgentState::Idle, "cycle 3 bounce");

    // Cycle 4: still in window — still suppressed
    t.since = Instant::now() - Duration::from_secs(2);
    t.transition(AgentState::ToolUse);
    assert_eq!(t.get_state(), AgentState::Idle, "cycle 4 bounce");
}

/// Phase A2 env tunable: `AGEND_OSCILLATION_GUARD_WINDOW_SECS=0`
/// effectively disables the guard. Operators who experience
/// false-suppression can opt out.
#[test]
#[serial_test::serial]
fn oscillation_guard_window_env_disable() {
    // SAFETY: only set + remove the env var around this test.
    // serial_test ensures no concurrent test races.
    unsafe { std::env::set_var("AGEND_OSCILLATION_GUARD_WINDOW_SECS", "0") };
    assert_eq!(oscillation_guard_window(), Duration::from_secs(0));

    let backend = Backend::ClaudeCode;
    let mut t = tracker_at(&backend, AgentState::Idle, 0);
    t.transition(AgentState::ToolUse);
    t.since = Instant::now() - Duration::from_secs(3);
    t.transition(AgentState::Idle);
    t.since = Instant::now() - Duration::from_secs(1);
    // With window=0, no priority-up record falls within the window
    // → guard cannot trigger → bounce goes through.
    t.transition(AgentState::ToolUse);
    assert_eq!(
        t.get_state(),
        AgentState::ToolUse,
        "#1005 A2: window=0 must disable the guard"
    );

    unsafe { std::env::remove_var("AGEND_OSCILLATION_GUARD_WINDOW_SECS") };
}

/// Phase A2 companion (#1005 dev-2 fixture finding): opencode's
/// in-flight `~ Reading file...` banner must match ToolUse. Pre-A2
/// the alternation `[✱→]\s+(Read|...)` missed this form so a
/// session that sustained `~ Reading…` without firing `✱`/`→`
/// never entered ToolUse. Fixture: opencode-tooluse.raw byte ~30720.
#[test]
#[serial_test::serial]
fn opencode_tilde_dash_ing_matches_tooluse() {
    let backend = Backend::OpenCode;
    let mut t = tracker_at(&backend, AgentState::Ready, 6);
    t.feed("~ Reading file...");
    assert_eq!(
        t.get_state(),
        AgentState::ToolUse,
        "#1005 A2: opencode in-flight `~ Reading…` banner must match ToolUse"
    );
}

#[test]
fn error_recovery_needs_hold() {
    let backend = Backend::ClaudeCode;

    // RateLimit held for 1s — transition to Idle (lower priority) needs hold time
    // RateLimit priority > Idle priority, and RateLimit is active (priority > Idle),
    // so 2s active hold applies
    let mut t = tracker_at(&backend, AgentState::RateLimit, 1);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::RateLimit);

    // RateLimit held for 3s (>= 2s) — should transition
    let mut t = tracker_at(&backend, AgentState::RateLimit, 3);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);
}

// ── P1: Edge cases ──────────────────────────────────────────────────

#[test]
fn dismissed_prompt_clears_on_next_feed() {
    // Screen-based detection: once the prompt text leaves the current
    // screen, the next feed re-evaluates without lag — the replacement
    // for the old state_buf-clearing behavior.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
    // Simulate the update-menu screen
    t.feed("Update available! 0.120.0 -> 0.121.0\n1. Update now\n2. Skip");
    // Then the banner re-renders after dismissal
    t.feed("bypass permissions");
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn unchanged_screen_does_not_reset_last_output() {
    // Hash dedup: feeding the same screen twice must not bump
    // last_output (used by hang/awaiting-operator predicates).
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed("hello world");
    t.last_output = Instant::now() - Duration::from_secs(10);
    let pinned = t.last_output;
    t.feed("hello world");
    assert_eq!(
        t.last_output, pinned,
        "identical screen must not bump last_output"
    );
}

#[test]
fn same_state_no_timer_reset() {
    let backend = Backend::ClaudeCode;
    let mut t = tracker_at(&backend, AgentState::Thinking, 10);
    let since_before = t.since;
    // Re-transition to same state — should be no-op
    t.transition(AgentState::Thinking);
    assert_eq!(t.since, since_before);
}

// ── Phase 1c: latched-state fallback ────────────────────────────────

#[test]
fn feed_fallback_expires_thinking_after_threshold() {
    // Thinking held past LATCHED_STATE_EXPIRY (30s) with no pattern
    // matching on the current screen must drop to Ready so a vanished
    // spinner cannot latch the tracker.
    let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 31);
    // Fresh screen content that matches no pattern for gemini.
    t.feed("some unrelated output that matches nothing");
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn feed_fallback_does_not_expire_before_threshold() {
    // Under the threshold Thinking must stay — legitimate thinking can
    // run for tens of seconds with a quiet but still-active spinner.
    let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 10);
    t.feed("some unrelated output that matches nothing");
    assert_eq!(t.get_state(), AgentState::Thinking);
}

#[test]
fn feed_fallback_expires_tooluse_after_threshold() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
    t.feed("no tool banner, no ready footer visible here");
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn feed_fallback_does_not_expire_fresh_permission_prompt() {
    // Before INTERACTIVE_EXPIRY (120 s) a PermissionPrompt must stay
    // latched — an operator answering a dialog within a reasonable
    // window expects the banner to still be marked as active.
    // Post-INTERACTIVE_EXPIRY expiry is covered by
    // `permission_prompt_also_expires_to_ready`.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 60);
    t.feed("nothing matches here");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

// ── tick() regression pins (t-20260423040613) ───────────────────────

#[test]
fn tick_expires_stale_tool_use_without_feed() {
    // ToolUse held > 30s with no PTY output. tick() must expire it
    // to Ready without requiring feed() (which needs new screen text).
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn tick_does_not_expire_fresh_tool_use() {
    // ToolUse held < 30s should NOT be expired by tick().
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 10);
    t.tick();
    assert_eq!(t.get_state(), AgentState::ToolUse);
}

#[test]
fn tick_does_not_expire_fresh_permission_prompt() {
    // Within INTERACTIVE_EXPIRY the prompt stays latched so operators
    // have a real reaction window. Post-expiry recovery is covered
    // by `permission_prompt_also_expires_to_ready`.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 60);
    t.tick();
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

#[test]
fn tick_called_twice_still_expires() {
    // Verify tick() works on repeated calls (no hash-based early return).
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Ready);
    // Second tick on Ready is a no-op (Ready is not expiring).
    t.tick();
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn feed_fallback_does_not_expire_fresh_interactive_prompt() {
    // Same contract as PermissionPrompt — within INTERACTIVE_EXPIRY
    // the prompt stays latched; after the threshold it recovers
    // (see `interactive_prompt_expires_to_ready_after_two_minutes`).
    let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 60);
    t.feed("nothing matches here");
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
}

#[test]
fn feed_fallback_no_op_when_already_ready() {
    // Ready → Ready must not reset `since` (that would defeat the hold).
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 60);
    let since_before = t.since;
    t.feed("arbitrary text without any markers");
    assert_eq!(t.get_state(), AgentState::Ready);
    assert_eq!(t.since, since_before);
}

// ── Phase 1d: structural startup prompt detection ──────────────────

#[test]
fn generic_startup_prompt_matches_yes_no() {
    assert!(is_generic_startup_prompt("Trust this workspace? (y/n)"));
    assert!(is_generic_startup_prompt("Continue? (Y/n)"));
    assert!(is_generic_startup_prompt("Accept? (yes/no)"));
    assert!(is_generic_startup_prompt("Overwrite [y/N]?"));
    assert!(is_generic_startup_prompt("Use default [Y/n]"));
}

#[test]
fn generic_startup_prompt_matches_press_enter() {
    assert!(is_generic_startup_prompt("Press enter to continue"));
    assert!(is_generic_startup_prompt("PRESS ENTER to dismiss"));
    assert!(is_generic_startup_prompt("Press Return when ready"));
    assert!(is_generic_startup_prompt("press any key to exit"));
}

#[test]
fn generic_startup_prompt_rejects_ordinary_prose() {
    // Question marks and colons alone must not trigger — AI model
    // output is full of these.
    assert!(!is_generic_startup_prompt(
        "Should I continue with the refactor?"
    ));
    assert!(!is_generic_startup_prompt("Next steps:"));
    assert!(!is_generic_startup_prompt("Select: option A vs option B"));
    assert!(!is_generic_startup_prompt("Type your message"));
    assert!(!is_generic_startup_prompt(""));
}

#[test]
fn starting_transitions_to_interactive_prompt_on_generic_token() {
    // Starting + pattern-None + generic prompt → InteractivePrompt,
    // no silence waiting required.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
    t.feed("Trust this workspace? (y/n)");
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
}

#[test]
fn non_starting_ignores_generic_prompt_token() {
    // Ready + a model output containing `(y/n)` must not flip state —
    // false positives here were the reason we scope generic detection
    // to Starting.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 0);
    t.feed("Here's an example: `git clean -n (y/n)` — the -n flag previews");
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn starting_without_generic_prompt_stays_starting() {
    // Starting + no recognized pattern + no generic token → still
    // Starting; silence fallback in supervisor handles this.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
    t.feed("loading configuration...");
    assert_eq!(t.get_state(), AgentState::Starting);
}

#[test]
fn feed_fallback_gated_by_hash_dedup() {
    // If the screen hasn't changed, fallback must not fire even if the
    // tracker has been latched forever — the hash dedup short-circuits
    // feed() before detection runs. Feed the same no-match text twice;
    // only the first call can possibly trigger the fallback path.
    let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 31);
    t.feed("no marker");
    // Tracker already dropped to Ready on the first feed. Reset to
    // Thinking to exercise the dedup-gate specifically.
    t.current = AgentState::Thinking;
    t.since = Instant::now() - Duration::from_secs(31);
    t.feed("no marker"); // same text → hash dedup → early return
    assert_eq!(t.get_state(), AgentState::Thinking);
}

#[test]
fn starting_hang_120s() {
    let mut h = HealthTracker::new();
    assert!(!h.check_hang(
        AgentState::Starting,
        Duration::from_secs(119),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    assert!(h.check_hang(
        AgentState::Starting,
        Duration::from_secs(121),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

#[test]
fn idle_never_hangs() {
    let mut h = HealthTracker::new();
    // Even with 10000s of silence, Idle should never be considered hung.
    assert!(!h.check_hang(
        AgentState::Idle,
        Duration::from_secs(10_000),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

#[test]
fn thinking_hang_600s() {
    let mut h = HealthTracker::new();
    assert!(!h.check_hang(
        AgentState::Thinking,
        Duration::from_secs(599),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
    assert!(h.check_hang(
        AgentState::Thinking,
        Duration::from_secs(601),
        Duration::from_secs(0),
        1_000_000,
        0
    ));
}

// ── P2: Pattern matching ────────────────────────────────────────────

#[test]
fn claude_tooluse_spinner_match() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // Real claude format: spinner glyph + space + tool name
    let detected = patterns.detect("⠋ Read file.txt");
    assert_eq!(detected, Some(AgentState::ToolUse));
}

#[test]
fn claude_tooluse_record_glyph_completion_does_not_fire_per_1005() {
    // #1005 Phase A1: `⏺ Write(...)` is the COMPLETED-tool banner
    // (the test's own original comment said "after the user denies
    // a write" — the action terminated, the banner is a record).
    // Pre-fix this fired ToolUse, contributing to the priority
    // oscillation bug. Re-pinned: bare-verb glyph banners must NOT
    // fire ToolUse. In-flight uses the -ing form
    // (`⏺ Writing src/...`) covered by `claude_tooluse_ing_verb_match`.
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let detected = patterns.detect("⏺ Write(/tmp/claude-perm-test.txt)");
    assert_ne!(
        detected,
        Some(AgentState::ToolUse),
        "#1005: `⏺ Write` completion banner must NOT fire ToolUse"
    );
}

#[test]
fn claude_permission_prompt_dialog_match() {
    // Claude 2.1.98 permission dialog — distinctive footer + body.
    // Observed in claude-perm.raw ~byte 9216.
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    assert_eq!(
        patterns.detect("Esc to cancel · Tab to amend"),
        Some(AgentState::PermissionPrompt),
        "dialog footer must fire PermissionPrompt",
    );
    assert_eq!(
        patterns.detect("Do you want to create /tmp/out.txt?"),
        Some(AgentState::PermissionPrompt),
        "dialog question prefix must fire PermissionPrompt",
    );
    assert_eq!(
        patterns.detect("   2. Yes, allow all edits during this session (shift+tab)"),
        Some(AgentState::PermissionPrompt),
        "allow-all-edits option must fire PermissionPrompt",
    );
}

#[test]
fn permission_prompt_legacy_wording_still_matches() {
    let cases: &[(Backend, &[&str])] = &[
        (
            Backend::ClaudeCode,
            &["Allow once", "Allow always", "approve"],
        ),
        (Backend::Codex, &["Request approval", "approve", "deny"]),
    ];
    for (backend, samples) in cases {
        let patterns = StatePatterns::for_backend(backend);
        for sample in *samples {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::PermissionPrompt),
                "{backend:?} legacy wording {sample:?} must still fire PermissionPrompt",
            );
        }
    }
}

#[test]
fn claude_tooluse_ing_verb_match() {
    // Claude 2.1.98 in-flight banners use present-participle verbs —
    // `⏺ Listing 1 directory…`, `⏺ Reading file`, etc. — rather than
    // the bare tool name on the completion banner. Real-PTY recording
    // claude-tooluse.raw exhibits `⏺ Listing 1 directory…` mid-stream;
    // this synthetic test anchors each -ing verb we added to the
    // alternation.
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    for sample in [
        "⏺ Listing 1 directory…",
        "⏺ Reading src/main.rs",
        "⏺ Writing /tmp/out.txt",
        "⏺ Searching for TODO",
        "⏺ Editing Cargo.toml",
    ] {
        assert_eq!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "expected ToolUse for {sample:?}"
        );
    }
}

#[test]
fn pattern_does_not_cross_backends() {
    // Claude's "❯" idle pattern should not match on Gemini tracker
    let gemini_patterns = StatePatterns::for_backend(&Backend::Gemini);
    let detected = gemini_patterns.detect("❯");
    assert_ne!(detected, Some(AgentState::Idle));
}

#[test]
fn empty_input_no_change() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
    t.feed("");
    assert_eq!(t.get_state(), AgentState::Starting);
}

#[test]
fn ready_detection() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed("bypass permissions");
    assert_eq!(t.get_state(), AgentState::Ready);
}

#[test]
fn idle_detection() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    // First get to Ready so that Idle (lower prio than Starting) can be tested
    // Starting → Ready (higher prio) is instant
    t.feed("bypass permissions");
    assert_eq!(t.get_state(), AgentState::Ready);
    // Now wait enough time for passive hold (5s) then feed idle pattern
    t.since = Instant::now() - Duration::from_secs(6);
    t.feed("❯");
    assert_eq!(t.get_state(), AgentState::Idle);
}

// ── Additional edge cases ───────────────────────────────────────────

#[test]
fn context_full_instant_transition() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
    t.feed("compacting context");
    assert_eq!(t.get_state(), AgentState::ContextFull);
}

#[test]
fn auth_error_instant_transition() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API key invalid");
    assert_eq!(t.get_state(), AgentState::AuthError);
}

#[test]
fn permission_prompt_higher_than_thinking() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
    // PermissionPrompt (priority 8) > Thinking (priority 6) — instant
    t.feed("Allow once");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

#[test]
fn set_restarting_transitions_state() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 5);
    t.set_restarting();
    assert_eq!(t.get_state(), AgentState::Restarting);
}

#[test]
fn error_state_is_error() {
    assert!(AgentState::ContextFull.is_error());
    assert!(AgentState::RateLimit.is_error());
    assert!(AgentState::UsageLimit.is_error());
    assert!(AgentState::AuthError.is_error());
    assert!(AgentState::ApiError.is_error());
    assert!(AgentState::Crashed.is_error());
    assert!(AgentState::Restarting.is_error());
    assert!(!AgentState::Thinking.is_error());
    assert!(!AgentState::Idle.is_error());
    assert!(!AgentState::Starting.is_error());
}

#[test]
fn awaiting_operator_not_error() {
    // AwaitingOperator means "needs human keystrokes", not a failure mode.
    assert!(!AgentState::AwaitingOperator.is_error());
    assert!(!AgentState::AwaitingOperator.is_unavailable());
}

#[test]
fn awaiting_operator_priority_between_hang_and_ready() {
    // Hang < AwaitingOperator < Ready so it preempts Starting/Hang in
    // tab-bar highest-priority display but doesn't outrank real activity.
    assert!(AgentState::Hang.priority() < AgentState::AwaitingOperator.priority());
    assert!(AgentState::AwaitingOperator.priority() < AgentState::Ready.priority());
}

#[test]
fn awaiting_operator_display_name() {
    assert_eq!(
        AgentState::AwaitingOperator.display_name(),
        "awaiting_operator"
    );
}

#[test]
fn set_awaiting_operator_from_starting() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
    t.set_awaiting_operator();
    assert_eq!(t.current, AgentState::AwaitingOperator);
}

#[test]
fn set_awaiting_operator_noop_from_non_starting() {
    // Only Starting transitions. All other states (including Ready) are
    // no-ops so late-firing tick-loop detections can't corrupt a healthy
    // mid-task agent. Known interactive prompts from Ready are caught
    // by pattern-based detection, not this time-based fallback.
    for s in [
        AgentState::Ready,
        AgentState::Idle,
        AgentState::Thinking,
        AgentState::ToolUse,
        AgentState::PermissionPrompt,
        AgentState::AwaitingOperator,
        AgentState::Crashed,
    ] {
        let mut t = tracker_at(&Backend::ClaudeCode, s, 10);
        t.set_awaiting_operator();
        assert_eq!(t.current, s, "state {:?} should be unchanged", s);
    }
}

#[test]
fn ready_pattern_lifts_awaiting_operator() {
    // Once operator unblocks the stall and the ready banner fires,
    // transition() takes the usual higher-priority-wins path.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
    t.set_awaiting_operator();
    assert_eq!(t.current, AgentState::AwaitingOperator);
    t.feed("bypass permissions");
    assert_eq!(t.current, AgentState::Ready);
}

// ── Full pipeline: PTY bytes → VTerm → screen → StateTracker ────────
//
// Exercises the production path `agent::pty_read_loop` takes: push
// raw bytes (with ANSI escapes) through the vterm, pull tail_lines of
// the screen, feed to state. Without these, unit tests can drift from
// how detection actually behaves once vterm rendering is involved —
// wrapped lines, cleared screens, scroll-off, etc.

use crate::vterm::VTerm;

/// Drive one full PTY cycle: process bytes, snapshot screen, feed state.
fn drive(vterm: &mut VTerm, state: &mut StateTracker, bytes: &[u8]) {
    vterm.process(bytes);
    let rows = vterm.rows() as usize;
    let screen = vterm.tail_lines(rows);
    state.feed(&screen);
}

#[test]
fn pipeline_claude_ready_via_vterm() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    // Bytes include ANSI colors — vterm must resolve them so screen
    // text is plain and pattern can match.
    drive(
        &mut vt,
        &mut st,
        b"\x1b[1;32mClaude Code\x1b[0m ready (bypass permissions mode)\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Ready);
}

#[test]
fn pipeline_codex_ready_via_vterm() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive(&mut vt, &mut st, b"\x1b[1mOpenAI Codex\x1b[0m v0.120.0\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
}

#[test]
fn pipeline_dismiss_drops_stale_pattern() {
    // Regression for the bug this whole refactor exists to fix: an
    // interactive prompt shows up, pattern matches; operator dismisses;
    // screen re-renders without the prompt text; state must re-evaluate
    // and fall back — NOT stay wedged on the stale buffered text.
    //
    // Uses codex-style flow: usage-limit banner (high priority) appears,
    // then a clear-screen + ready banner re-renders.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));

    // Step 1: screen shows usage limit
    drive(
        &mut vt,
        &mut st,
        b"You've hit your usage limit. Try again at 10:00 AM.\r\n",
    );
    assert_eq!(st.get_state(), AgentState::UsageLimit);

    // Step 2: clear screen + fresh banner (simulates user re-auth / reset)
    // \x1b[2J clears screen, \x1b[H moves cursor home.
    // Advance `since` so the lower-priority Ready transition clears the
    // active-hold gate (UsageLimit is an error state; leaving requires
    // active hold 2s).
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
    assert_eq!(
        st.get_state(),
        AgentState::Ready,
        "after screen clear + ready banner, stale UsageLimit must release"
    );
}

#[test]
fn pipeline_screen_unchanged_preserves_silence_timer() {
    // Cursor-blink-like bytes (show/hide cursor) must not bump
    // last_output since they leave the rendered grid unchanged.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive(&mut vt, &mut st, b"OpenAI Codex v0.120.0\r\n");
    st.last_output = Instant::now() - Duration::from_secs(10);
    let before = st.last_output;
    // Cursor hide/show — visible grid unchanged.
    drive(&mut vt, &mut st, b"\x1b[?25l\x1b[?25h");
    assert_eq!(
        st.last_output, before,
        "cursor-visibility toggles must not reset silence timer"
    );
}

#[test]
fn pipeline_usage_limit_instant_from_idle() {
    // Claude flow: agent sitting at idle prompt, then rate-limit burst
    // arrives. Error state must win immediately (no hysteresis).
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    drive(
        &mut vt,
        &mut st,
        b"bypass permissions\r\n> ready\r\n\xe2\x9d\xaf",
    );
    assert!(matches!(
        st.get_state(),
        AgentState::Ready | AgentState::Idle
    ));
    // #848: canonical Anthropic 429-rejection wording instead of the
    // bare `429 rate limit exceeded` that the OLD broad pattern matched.
    drive(
        &mut vt,
        &mut st,
        b"\r\n\x1b[31mAPI Error: Request rejected (429) - rate_limit_error\x1b[0m\r\n",
    );
    assert_eq!(st.get_state(), AgentState::RateLimit);
}

#[test]
fn interactive_prompt_notice_armed_on_entry_and_dedupes() {
    // Use tracker_at to place the tracker directly into InteractivePrompt
    // (no backend pattern triggers it after Sprint 46 removal).
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    assert!(!t.take_interactive_prompt_notice());

    // Simulate entering InteractivePrompt via direct transition.
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    assert!(t.take_interactive_prompt_notice(), "first entry must arm");
    assert!(
        !t.take_interactive_prompt_notice(),
        "subsequent ticks within the same InteractivePrompt must not re-arm"
    );
}

#[test]
fn interactive_prompt_notice_rearms_on_reentry() {
    // Enter InteractivePrompt, leave to Ready, re-enter — notice must re-arm.
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    assert!(t.take_interactive_prompt_notice());

    // Simulate passive-hold window so InteractivePrompt can drop back.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Ready);
    assert_eq!(t.get_state(), AgentState::Ready);
    assert!(!t.take_interactive_prompt_notice(), "no notice while Ready");

    // Re-enter InteractivePrompt.
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    assert!(
        t.take_interactive_prompt_notice(),
        "re-entry after a leave must re-arm the notice"
    );
}

#[test]
fn recovery_notice_armed_when_leaving_interactive_prompt() {
    // Use tracker_at to place the tracker directly into InteractivePrompt.
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    assert!(!t.take_recovery_notice());

    // Enter InteractivePrompt.
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    // Still nothing to report — we only arm when we LEAVE the blocked
    // state, not when we enter it.
    assert!(!t.take_recovery_notice());

    // Dismiss → Ready.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Ready);
    assert_eq!(t.get_state(), AgentState::Ready);

    // First take fires; subsequent ticks within the same Ready don't
    // re-spam.
    assert!(t.take_recovery_notice(), "recovery must arm on exit");
    assert!(
        !t.take_recovery_notice(),
        "supervisor ticks after the first must not re-arm"
    );
}

#[test]
fn recovery_notice_armed_when_leaving_awaiting_operator() {
    // AwaitingOperator → Ready goes through `transition()` with the
    // forced AwaitingOperator as the previous state, so the symmetric
    // arm path still fires.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    st.set_awaiting_operator();
    assert_eq!(st.get_state(), AgentState::AwaitingOperator);
    assert!(!st.take_recovery_notice());

    // Fresh Ready banner appears. Ready (prio 3) > AwaitingOperator
    // (prio 2) so the transition is immediate.
    drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    assert!(
        st.take_recovery_notice(),
        "recovery must arm on AwaitingOperator → Ready"
    );
}

#[test]
fn recovery_notice_not_armed_for_unrelated_transitions() {
    // Ready → Thinking → Ready must not arm the recovery notice: the
    // operator never saw a blocked state, so "ready again" is noise.
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.current = AgentState::Ready;
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
    st.transition(AgentState::Thinking);
    assert_eq!(st.get_state(), AgentState::Thinking);
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
    st.transition(AgentState::Ready);
    assert_eq!(st.get_state(), AgentState::Ready);
    assert!(!st.take_recovery_notice());
}

#[test]
fn gemini_tooluse_banner_does_not_fire_per_1005() {
    // #1005 Phase A1: gemini's `✓ <ToolName> <target>` lines ARE
    // completion records — the `✓` IS the completion marker.
    // Matching them as ToolUse caused priority oscillation against
    // Idle (same class as the claude `✓ Bash` bug). Re-pinned:
    // completion lines must NOT fire ToolUse.
    //
    // Follow-up risk acknowledged: gemini-tooluse.raw fixture does
    // not cover an in-flight tool-call shape distinct from
    // Thinking. If gemini adds a dedicated in-flight banner we can
    // detect, capture a new fixture + re-introduce a narrow
    // ToolUse pattern.
    let patterns = StatePatterns::for_backend(&Backend::Gemini);
    for sample in [
        "   ✓  ReadFile  Cargo.toml",
        "   ✓  WriteFile  /tmp/out.txt",
        "   ✓  Edit  Cargo.toml",
        "   ✓  Shell  ls -la",
        "   ✓  WebFetch  https://example.com",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005: gemini `✓` completion line must NOT fire ToolUse: {sample:?}"
        );
    }
}

#[test]
fn codex_tooluse_past_tense_title_does_not_fire_per_1005() {
    // #1005 Phase A1: `• Explored|Edited|Ran` are past-tense title
    // lines — unambiguously completion-render. Matching them as
    // ToolUse caused priority oscillation against Idle/Ready same
    // as the claude `✓ Bash` bug. Re-pinned: past-tense titles must
    // NOT fire ToolUse. Codex's in-flight indicator is the
    // `• Working (...)` spinner which fires Thinking.
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for sample in ["• Explored", "• Edited", "• Ran"] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005: codex past-tense title `{sample}` is completion record — must NOT fire ToolUse"
        );
    }
}

#[test]
fn codex_tooluse_continuation_line_does_not_fire_per_1005() {
    // #1005 Phase A1: `└ <Verb>` continuation lines render UNDER a
    // tool-output block AFTER the tool finishes — completion record,
    // not active execution. Re-pinned: continuation lines must NOT
    // fire ToolUse.
    //
    // #1005 RC1 (reviewer #1009 verdict): `└ Ran apply_patch` is
    // INCLUDED in the negative list — the pre-RC1 carveout kept
    // the broad `apply_patch` substring pattern, which violated
    // the A1 semantic by re-firing ToolUse on completion banners.
    // Now the legacy alternation is gone and this test pins that
    // removal.
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for sample in [
        "  └ Read README.md",
        "  └ Write /tmp/out.txt",
        "  └ Edit Cargo.toml",
        "  └ List src/",
        "  └ Ran apply_patch",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005: codex `└` continuation `{sample}` is completion record — must NOT fire ToolUse"
        );
    }
}

#[test]
fn codex_tooluse_apply_patch_completion_does_not_fire_per_1005_rc1() {
    // #1005 RC1 (reviewer #1009 verdict): the broad legacy
    // `r"apply_patch"` substring pattern was removed. Pre-RC1 it
    // fired ToolUse on `• Ran apply_patch` / `└ Ran apply_patch`
    // completion banners, re-triggering the priority oscillation
    // class the rest of #1005 Phase A1 closed. Pin: completion
    // banners containing `apply_patch` must NOT fire ToolUse.
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for sample in [
        "• Ran apply_patch",
        "  └ Ran apply_patch",
        "Error: apply_patch failed at /tmp/foo.txt",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005 RC1: codex `apply_patch` completion / error surface must NOT fire ToolUse: {sample:?}"
        );
    }
}

#[test]
fn codex_tooluse_does_not_false_positive_on_spinner_or_narration() {
    // `• Working (...)` is the spinner; `• I'm reading ...` is
    // assistant narration. Neither should fire ToolUse — pattern
    // must only match the Explored/Edited/Ran titles and the `└`
    // continuation line. (Codex Thinking pattern is `Thinking`
    // which never matches the literal `Working` spinner, so these
    // lines simply fall through to lower-priority matches or None.)
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    assert_ne!(
        patterns.detect("• Working (1s • esc to interrupt)"),
        Some(AgentState::ToolUse),
        "`• Working` spinner must not fire ToolUse"
    );
    assert_ne!(
        patterns.detect("• I'm reading README.md from the repo root"),
        Some(AgentState::ToolUse),
        "narration `• I'm reading ...` must not fire ToolUse"
    );
}

#[test]
fn codex_permission_prompt_dialog_match() {
    // Codex 0.120.0 approval dialog — header, every option, and
    // footer must all fire PermissionPrompt. Observed in
    // codex-perm.raw byte ~68K-90K.
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for sample in [
        "  Would you like to run the following command?",
        "  1. Yes, proceed (y)",
        "› 3. No, and tell Codex what to do differently (esc)",
        "  Press enter to confirm or esc to cancel",
    ] {
        assert_eq!(
            patterns.detect(sample),
            Some(AgentState::PermissionPrompt),
            "expected PermissionPrompt for {sample:?}"
        );
    }
}

#[test]
fn codex_permission_prompt_does_not_false_positive_on_narration() {
    // Narration lines that precede the dialog (`• I'm writing...`,
    // `outside the writable sandbox`, `escalated command`) must not
    // fire PermissionPrompt — they're assistant narration, not the
    // interactive dialog itself. We only assert they don't fire
    // PermissionPrompt (they may legitimately match other states).
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for sample in [
        "• I'm writing the requested content to /tmp/foo.txt.",
        "  writable sandbox, so I need to run one escalated command",
        "• Running printf 'hello' > /tmp/foo.txt",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::PermissionPrompt),
            "narration {sample:?} must NOT fire PermissionPrompt",
        );
    }
}

#[test]
fn kiro_tooluse_banner_does_not_fire_per_1005() {
    // #1005 Phase A1: kiro's `● <Verb> <target>` banners are
    // COMPLETION render (dev-2 fixture inspection HIGH-confirmed).
    // Matching them as ToolUse caused priority oscillation against
    // Idle, same class as the claude `✓ Bash` bug. Re-pinned:
    // completion banners must NOT fire ToolUse. Kiro's in-flight
    // indicator is the `Kiro is working` spinner which fires
    // Thinking.
    let patterns = StatePatterns::for_backend(&Backend::KiroCli);
    for sample in [
        "● Read .",
        "● Write /tmp/out.txt",
        "● Edit Cargo.toml",
        "● Bash ls -la",
        "● Grep TODO src/",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005: kiro `●` completion banner must NOT fire ToolUse: {sample:?}"
        );
    }
}

#[test]
fn kiro_tooluse_legacy_internal_names_still_match() {
    // Preserve backwards-compat: old pattern alternation
    // (execute_bash|fs_read|fs_write) continues to match stack-trace
    // / error-surface output that may leak these internal tool IDs.
    let patterns = StatePatterns::for_backend(&Backend::KiroCli);
    for sample in ["execute_bash", "fs_read", "fs_write"] {
        assert_eq!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "expected ToolUse for {sample:?}"
        );
    }
}

#[test]
fn pipeline_kiro_thinking_via_vterm() {
    // Sprint 34 PR-1: kiro-cli now shows "Kiro is working" during
    // generation, not "Thinking". Updated from old pattern.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::KiroCli));
    drive(&mut vt, &mut st, b"ask a question or describe a task\r\n");
    assert_eq!(st.get_state(), AgentState::Idle);
    drive(&mut vt, &mut st, b"Kiro is working\r\n");
    assert_eq!(st.get_state(), AgentState::Thinking);
}

#[test]
fn pipeline_kiro_slash_menu_does_not_trigger_context_full() {
    // Regression: kiro's slash-command autocomplete renders `/compact`
    // as one entry among many when the user types `/`. The old
    // ContextFull pattern `|/compact` matched that menu text and
    // wrongly reported the agent as ContextFull on every `/` keypress.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::KiroCli));
    drive(&mut vt, &mut st, b"Trust All Tools active\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // User opens slash menu — `/compact` is listed alongside `/quit`.
    drive(
        &mut vt,
        &mut st,
        b"/quit    Quit the application\r\n/compact Compact context\r\n",
    );
    assert_ne!(
        st.get_state(),
        AgentState::ContextFull,
        "slash-menu listing /compact must not trigger ContextFull"
    );
}

#[test]
fn pipeline_opencode_thinking_via_vterm() {
    // Regression for BUG 4: opencode never prints "Working". While a
    // request is in flight it draws `■⬝⬝⬝⬝⬝⬝⬝  esc interrupt` on its
    // bottom bar; that line disappears once streaming completes.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::OpenCode));
    drive(
        &mut vt,
        &mut st,
        b"\xe2\x96\xa0\xe2\xac\x9d\xe2\xac\x9d\xe2\xac\x9d\xe2\xac\x9d  esc interrupt   tab agents\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Thinking);
}

#[test]
fn pipeline_gemini_thinking_via_vterm() {
    // Regression for BUG 1: matching on the bare word "Thinking"
    // latched the state permanently because chat history kept the
    // token visible. "esc to cancel" lives only on the active spinner
    // line and is overwritten when streaming completes.
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::Gemini));
    drive(&mut vt, &mut st, b"Type your message or @path/to/file\r\n");
    assert_eq!(st.get_state(), AgentState::Idle);
    drive(
        &mut vt,
        &mut st,
        b"\xe2\xa0\xa6 Thinking... (esc to cancel, 2s)\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Thinking);
}

#[test]
fn pipeline_gemini_chat_history_does_not_latch_thinking() {
    // Once the spinner line is gone, "Thinking" left behind in chat
    // history (e.g. the model's narrative) must NOT keep detect() in
    // Thinking. After hysteresis, screen should allow transition back
    // to Idle on "Type your message" prompt re-appearing.
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::Gemini));
    drive(
        &mut vt,
        &mut st,
        b"\xe2\xa0\xa6 Thinking... (esc to cancel, 2s)\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Thinking);
    // Simulate hysteresis window closing — in production this is wall
    // time between reads, here we backdate `since` so the passive-hold
    // gate doesn't block the downgrade.
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    // Clear screen, redraw with "I was thinking about..." in chat
    // history and a fresh prompt. "Thinking" alone would have latched;
    // now only the spinner line matches — and it's gone.
    drive(
        &mut vt,
        &mut st,
        b"\x1b[2J\x1b[HI was thinking about your question. 2+2 = 4.\r\nType your message or @path/to/file\r\n",
    );
    assert_eq!(
        st.get_state(),
        AgentState::Idle,
        "stale 'Thinking' word in chat history must not latch"
    );
}

#[test]
fn gemini_thinking_pattern_matches_spinner_with_timer() {
    // F39 contract (decision d-20260513231713506833-1): active Gemini
    // spinner line with timer-paren prefix `(esc to cancel, Ns)` must
    // trigger Thinking transition.
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::Gemini));
    drive(
        &mut vt,
        &mut st,
        b"\xe2\xa0\xa6 Thinking... (esc to cancel, 5s)\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Thinking);
}

#[test]
fn gemini_thinking_pattern_does_not_match_bare_scrollback_text() {
    // F39 contract (decision d-20260513231713506833-1): prose containing
    // literal "esc to cancel" without the timer-paren prefix (chat
    // history, docs, help text) must NOT trigger Thinking. Locks the
    // narrowing: r"esc to cancel" → r"\(esc to cancel,". Test asserts
    // semantic outcome, not regex literal — future further-narrowing
    // that preserves this contract is free to refactor.
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::Gemini));
    drive(
        &mut vt,
        &mut st,
        b"Press Ctrl-C or esc to cancel the operation.\r\n",
    );
    assert_ne!(st.get_state(), AgentState::Thinking);
}

#[test]
fn pipeline_opencode_ready_prompt_resolves_to_idle() {
    // OpenCode's input prompt "Ask anything" matches BOTH the Idle
    // pattern (listed first) and the Ready pattern, so first-match
    // returns Idle. This reflects the backend's pattern table as
    // currently configured — noted here so future tweaks don't
    // accidentally flip to Ready without intent. Semantically OK:
    // Idle is "waiting for input at an alive prompt", and that's
    // what the user sees when opencode is ready.
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::OpenCode));
    drive(
        &mut vt,
        &mut st,
        b"Ask anything   \xe2\x8c\x85 tab agents\r\n",
    );
    assert_eq!(st.get_state(), AgentState::Idle);
}

#[test]
fn opencode_tooluse_in_flight_banner_still_fires() {
    // #1005 Phase A1: opencode's `✱ <Verb>` (U+2731 HEAVY ASTERISK,
    // in-flight) banner KEPT in the ToolUse alternation — it
    // represents active execution.
    let patterns = StatePatterns::for_backend(&Backend::OpenCode);
    for sample in [
        "   ✱ Glob \"README.md\" (1 match)",
        "   ✱ Write src/lib.rs",
        "   ✱ Read README.md",
        "   ✱ Edit Cargo.toml",
    ] {
        assert_eq!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "in-flight banner `{sample}` must fire ToolUse"
        );
    }
}

#[test]
fn opencode_tooluse_completed_banner_does_not_fire_per_1005() {
    // #1005 Phase A1: opencode's `→ <Verb>` (U+2192, COMPLETED)
    // banner DROPPED from the ToolUse alternation. Pre-fix matched
    // both `✱` and `→`; the `→ Read README.md` line stays in
    // scrollback after the tool completes, causing the same
    // priority oscillation class as the claude `✓ Bash` bug.
    let patterns = StatePatterns::for_backend(&Backend::OpenCode);
    for sample in [
        "   → Read README.md",
        "   → Edit Cargo.toml",
        "   → Write src/lib.rs",
    ] {
        assert_ne!(
            patterns.detect(sample),
            Some(AgentState::ToolUse),
            "#1005: opencode `→` completion banner must NOT fire ToolUse: {sample:?}"
        );
    }
}

// ── Phase 1e: manifest-driven replay regression ─────────────────────
//
// Feeds each recorded PTY session through vterm + StateTracker and
// asserts the observed transition sequence matches the expected list
// in MANIFEST.yaml exactly. Catches regressions when backend patterns
// are changed without re-recording — any deviation from the baseline
// sequence should be reviewed manually (either a legitimate pattern
// improvement, or a regression).
//
// When a CLI version is updated, re-record the fixture, bump
// `cli_version` + `recorded_on`, and regenerate the expected
// transitions from a manual inspection of the ignored replay_session
// output.

#[derive(serde::Deserialize)]
struct ReplayManifest {
    fixtures: Vec<ReplayFixture>,
}

#[derive(serde::Deserialize)]
struct ReplayFixture {
    file: String,
    backend: String,
    cli_version: String,
    #[allow(dead_code)]
    recorded_on: String,
    #[allow(dead_code)]
    scenario: String,
    expected_transitions: Vec<String>,
    expected_final_state: String,
    expected_final_detect: Option<String>,
    // F685 sub-task 5 fixture corpus extension (decision
    // d-20260514015214320625-1 §1.A). All fields optional with serde
    // defaults — existing 13 fixtures (schema_version 1, implicit)
    // remain valid without manifest edits. Schema_version is a
    // future-compat metadata marker; no runtime enforcement Phase 1.
    // See docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.2.
    #[serde(default)]
    scenario_kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    expected_hung_classification: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    expected_oscillation_count: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    productive_marker_expectations: Vec<ProductiveMarkerExpectation>,
    #[serde(default)]
    #[allow(dead_code)]
    capture_kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    provenance: Option<String>,
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct ProductiveMarkerExpectation {
    time_ms: u64,
    source: String,
}

fn default_schema_version() -> u32 {
    1
}

fn parse_state(name: &str) -> AgentState {
    match name {
        "starting" => AgentState::Starting,
        "hang" => AgentState::Hang,
        "awaiting_operator" => AgentState::AwaitingOperator,
        "ready" => AgentState::Ready,
        "idle" => AgentState::Idle,
        "tool_use" => AgentState::ToolUse,
        "thinking" => AgentState::Thinking,
        "interactive_prompt" => AgentState::InteractivePrompt,
        "permission" => AgentState::PermissionPrompt,
        "git_conflict" => AgentState::GitConflict,
        "context_full" => AgentState::ContextFull,
        "rate_limit" => AgentState::RateLimit,
        "server_rate_limit" => AgentState::ServerRateLimit,
        "usage_limit" => AgentState::UsageLimit,
        "auth_error" => AgentState::AuthError,
        "api_error" => AgentState::ApiError,
        "crashed" => AgentState::Crashed,
        "restarting" => AgentState::Restarting,
        other => panic!("unknown state name in manifest: {other}"),
    }
}

fn parse_backend(name: &str) -> Backend {
    match name {
        "claude" | "claude-code" => Backend::ClaudeCode,
        "kiro" | "kiro-cli" => Backend::KiroCli,
        "codex" | "codex-cli" => Backend::Codex,
        "opencode" | "opencode-cli" => Backend::OpenCode,
        "gemini" | "gemini-cli" => Backend::Gemini,
        // #987: agy / antigravity / antigravity-cli — Google's gemini-cli successor.
        "agy" | "antigravity" | "antigravity-cli" => Backend::Agy,
        other => panic!("unknown backend name in manifest: {other}"),
    }
}

#[test]
#[allow(clippy::unwrap_used)]
fn replay_manifest_regression() {
    let fixtures_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-replay");
    let manifest_path = fixtures_dir.join("MANIFEST.yaml");
    let raw = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let manifest: ReplayManifest =
        serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse MANIFEST.yaml: {e}"));

    assert!(
        !manifest.fixtures.is_empty(),
        "MANIFEST.yaml must list at least one fixture"
    );

    for f in &manifest.fixtures {
        let ctx = format!("{} ({} v{})", f.file, f.backend, f.cli_version);
        let fixture_path = fixtures_dir.join(&f.file);
        let bytes =
            std::fs::read(&fixture_path).unwrap_or_else(|e| panic!("[{ctx}] read fixture: {e}"));

        let backend = parse_backend(&f.backend);
        let mut vt = VTerm::new(120, 40);
        let mut st = StateTracker::new(Some(&backend));
        let mut observed: Vec<AgentState> = vec![st.current];

        for chunk in bytes.chunks(512) {
            vt.process(chunk);
            let rows = vt.rows() as usize;
            let screen = vt.tail_lines(rows);
            st.feed(&screen);
            if observed.last().copied() != Some(st.current) {
                observed.push(st.current);
            }
        }

        let expected: Vec<AgentState> = f
            .expected_transitions
            .iter()
            .map(|s| parse_state(s))
            .collect();
        assert_eq!(
            observed, expected,
            "[{ctx}] transition mismatch — pattern change or upstream CLI UI drift? \
             observed {:?}, expected {:?}",
            observed, expected
        );

        let expected_final = parse_state(&f.expected_final_state);
        assert_eq!(
            st.current, expected_final,
            "[{ctx}] final state mismatch: got {:?}, expected {:?}",
            st.current, expected_final
        );

        let patterns = StatePatterns::for_backend(&backend);
        let final_screen = vt.tail_lines(vt.rows() as usize);
        let detect_result = patterns.detect(&final_screen);
        let expected_detect = f.expected_final_detect.as_deref().map(parse_state);
        assert_eq!(
            detect_result, expected_detect,
            "[{ctx}] final detect() mismatch: got {:?}, expected {:?}",
            detect_result, expected_detect
        );
    }
}

// -----------------------------------------------------------------
// F685 sub-task 5 corpus measurement (decision d-20260514015214320625-1)
// -----------------------------------------------------------------
//
// The integration-test side (`tests/fixture_corpus_measurement.rs`)
// validates manifest schema + byte-existence. THIS unit-test side
// exercises the actual `infer_productivity` measurement path against
// labelled fixtures, since the function lives in the binary crate
// and is not exposed via `src/lib.rs`. The harness here is a smoke
// test for the measurement plumbing, NOT a pass/fail gate on the
// FP < 1% / FN < 10% acceptance criteria — those gate on corpus
// growth (N ≥ 300 / N ≥ 30) over weeks per decision §1 reframe.

#[test]
fn corpus_measurement_smoke_f9_marker_signals() {
    // Run F9 productive-signal inference against the three schema-v2
    // fixtures and assert the signal classification matches the
    // labelled scenario_kind. This is the F9 measurement loop in
    // miniature: harness reports rates, gates on growth.
    let fixtures_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-replay");
    let manifest_path = fixtures_dir.join("MANIFEST.yaml");
    let raw = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let manifest: ReplayManifest =
        serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| panic!("parse: {e}"));

    let v2: Vec<&ReplayFixture> = manifest
        .fixtures
        .iter()
        .filter(|f| f.schema_version >= 2)
        .collect();
    assert!(
        v2.len() >= 3,
        "smoke test requires ≥3 schema-v2 fixtures, got {}",
        v2.len()
    );

    let mut productive_marker_fire_observations = 0usize;
    let mut productive_silence_observations = 0usize;

    for f in &v2 {
        let backend = parse_backend(&f.backend);
        let pconfig = crate::behavioral::config_for_productivity(&backend);
        let bytes = std::fs::read(fixtures_dir.join(&f.file))
            .unwrap_or_else(|e| panic!("read {}: {e}", f.file));

        // Render through vterm to get the screen the StateTracker
        // would feed against. Mirrors `replay_manifest_regression`
        // loop body.
        let mut vt = VTerm::new(120, 40);
        for chunk in bytes.chunks(512) {
            vt.process(chunk);
        }
        let rows = vt.rows() as usize;
        let screen = vt.tail_lines(rows);

        // Heartbeat_age = MAX simulates "no MCP integration / stale";
        // forces evaluation through the marker path only. This is the
        // common-case for synthetic byte-only fixtures.
        let signal = crate::behavioral::infer_productivity(
            &pconfig,
            &screen,
            std::time::Duration::from_secs(u32::MAX as u64),
        );

        match f.scenario_kind.as_deref() {
            Some("productive_marker_fire") => {
                assert!(
                    matches!(
                        signal,
                        crate::behavioral::ProductivitySignal::Productive { .. }
                    ),
                    "fixture {} labelled productive_marker_fire but signal = {:?}",
                    f.file,
                    signal
                );
                productive_marker_fire_observations += 1;
            }
            Some("productive_silence") => {
                assert_eq!(
                    signal,
                    crate::behavioral::ProductivitySignal::NoSignal,
                    "fixture {} labelled productive_silence but signal = {:?}",
                    f.file,
                    signal
                );
                productive_silence_observations += 1;
            }
            Some("silent_stuck") => {
                // silent_stuck = no productive evidence; signal must
                // be NoSignal (heartbeat MAX + no marker).
                assert_eq!(
                    signal,
                    crate::behavioral::ProductivitySignal::NoSignal,
                    "fixture {} labelled silent_stuck but signal = {:?}",
                    f.file,
                    signal
                );
            }
            Some("priority_oscillation") | Some(_) | None => {
                // priority_oscillation and other kinds need
                // time-injection harness extension — deferred per
                // §F685-CORPUS.6 open questions. Skip silently in
                // Phase 1 smoke test.
            }
        }
    }

    assert!(
        productive_marker_fire_observations >= 1,
        "initial corpus must include at least one productive_marker_fire fixture"
    );
    assert!(
        productive_silence_observations >= 1,
        "initial corpus must include at least one productive_silence fixture"
    );
}

// -----------------------------------------------------------------
// Track 1 PR-2: Heartbeat gate regression pins (design §7)
// -----------------------------------------------------------------

#[test]
fn heartbeat_fresh_overrides_permission_prompt() {
    // Pin 1 — A5 incident scenario: agent has fresh heartbeat, PTY
    // shows permission pattern → must NOT latch PermissionPrompt.
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    t.update_heartbeat(Duration::from_secs(10)); // 10s ago = fresh
    t.feed("Allow this action y/n/t");
    assert_ne!(
        t.get_state(),
        AgentState::PermissionPrompt,
        "fresh heartbeat must suppress PermissionPrompt"
    );
    assert_eq!(t.get_state(), AgentState::Thinking);
}

#[test]
fn stale_heartbeat_allows_permission_prompt() {
    // Pin 2 — stale heartbeat: agent silent for 200s, PTY shows
    // permission pattern → must latch PermissionPrompt normally.
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    t.update_heartbeat(Duration::from_secs(200)); // 200s ago > 120s
    t.feed("Allow this action y/n/t");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

#[test]
fn no_heartbeat_allows_permission_prompt() {
    // Pin 3 — no heartbeat ever: default behavior preserved.
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    // last_heartbeat is None by default
    t.feed("Allow this action y/n/t");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

#[test]
fn test_classify_fixtures() {
    use crate::health::BlockedReason;

    let fixtures: &[(&str, &Backend, Option<BlockedReason>)] = &[
        (
            "claude_429.txt",
            &Backend::ClaudeCode,
            Some(BlockedReason::RateLimit {
                retry_after_secs: None,
            }),
        ),
        (
            "claude_quota.txt",
            &Backend::ClaudeCode,
            Some(BlockedReason::QuotaExceeded),
        ),
        (
            "kiro_throttle.txt",
            &Backend::KiroCli,
            Some(BlockedReason::RateLimit {
                retry_after_secs: None,
            }),
        ),
        (
            "kiro_quota.txt",
            &Backend::KiroCli,
            Some(BlockedReason::QuotaExceeded),
        ),
        (
            "kiro_false_usage_limit.txt",
            &Backend::KiroCli,
            None, // false positive: agent discussing usage limits, not an error
        ),
        (
            "codex_quota.txt",
            &Backend::Codex,
            Some(BlockedReason::QuotaExceeded),
        ),
        (
            "gemini_resource_exhausted.txt",
            &Backend::Gemini,
            Some(BlockedReason::QuotaExceeded),
        ),
    ];

    let fixture_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backend_error_fixtures");

    for (file, backend, expected) in fixtures {
        let path = fixture_dir.join(file);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing fixture {file}: {e}"));
        let result = super::classify_pty_output(backend, &content);
        assert_eq!(
            result, *expected,
            "fixture {file}: expected {expected:?}, got {result:?}"
        );
    }
}

// --- Sprint 34 PR-1: thinking pattern fixture tests ---

#[test]
fn claude_spinner_verb_triggers_thinking() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    drive(&mut vt, &mut st, b"bypass permissions\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // Claude spinner uses random verbs, not "Thinking"
    drive(&mut vt, &mut st, b"Cogitating\xe2\x80\xa6\r\n");
    assert_eq!(
        st.get_state(),
        AgentState::Thinking,
        "claude spinner verb 'Cogitating' must trigger Thinking"
    );
}

#[test]
fn claude_thought_for_triggers_thinking() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    drive(&mut vt, &mut st, b"bypass permissions\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // Post-thinking summary line
    drive(&mut vt, &mut st, b"thought for 12s\r\n");
    assert_eq!(
        st.get_state(),
        AgentState::Thinking,
        "claude 'thought for Ns' must trigger Thinking"
    );
}

#[test]
fn kiro_working_triggers_thinking() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::KiroCli));
    drive(&mut vt, &mut st, b"Trust All Tools active\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // Kiro shows "Kiro is working" during generation
    drive(&mut vt, &mut st, b"Kiro is working\r\n  esc to cancel\r\n");
    assert_eq!(
        st.get_state(),
        AgentState::Thinking,
        "kiro 'Kiro is working' must trigger Thinking"
    );
}

#[test]
fn codex_working_triggers_thinking() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive(&mut vt, &mut st, b"OpenAI Codex gpt-4.1 left\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // Codex shows "Working (Ns • esc to interrupt)"
    drive(
        &mut vt,
        &mut st,
        b"\xc2\xb7 Working (3s \xe2\x80\xa2 esc to interrupt)\r\n",
    );
    assert_eq!(
        st.get_state(),
        AgentState::Thinking,
        "codex 'Working' or 'esc to interrupt' must trigger Thinking"
    );
}

#[test]
fn kiro_literal_thinking_in_chat_does_not_trigger() {
    // After pattern change, "Thinking" alone should NOT trigger on kiro
    let patterns = StatePatterns::for_backend(&Backend::KiroCli);
    let detected = patterns.detect("The agent was Thinking about the problem");
    assert_ne!(
        detected,
        Some(AgentState::Thinking),
        "literal 'Thinking' in chat must not trigger Thinking on kiro"
    );
}

// --- Sprint 34 PR-2: ToolUse anchor fixture tests ---

#[test]
fn claude_tool_banner_at_line_start_triggers_tooluse() {
    // #1005 Phase A1: bare-verb banners (`⏺ Bash`, `⏺ Read`) are
    // COMPLETION records, not active execution. They stay on screen
    // after the tool finishes — matching them as ToolUse caused
    // priority oscillation against Idle, preventing
    // `LATCHED_STATE_EXPIRY` from firing. Re-pinned: bare-verb
    // banners must NOT trigger ToolUse. `-ing` verb banners
    // (`● Listing`, `⏺ Reading`, etc.) are the in-flight progress
    // shape and DO still fire ToolUse via the post-#1005
    // alternation `[✓●⏺]\s+(Listing|Reading|Writing|Searching|Editing)`.
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    assert_ne!(
        patterns.detect("⏺ Bash(echo hi)"),
        Some(AgentState::ToolUse),
        "#1005: completion banner `⏺ Bash` is historical, not active — must NOT fire ToolUse"
    );
    assert_ne!(
        patterns.detect("⏺ Read(README.md)"),
        Some(AgentState::ToolUse),
        "#1005: completion banner `⏺ Read` is historical, not active — must NOT fire ToolUse"
    );
    // Glyph + -ing verb = in-flight progress (still active).
    assert_eq!(
        patterns.detect("● Listing files..."),
        Some(AgentState::ToolUse),
        "completion glyph + in-flight `-ing` verb = active progress, must fire ToolUse"
    );
}

#[test]
fn claude_chat_with_glyph_does_not_trigger_tooluse() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    drive(&mut vt, &mut st, b"bypass permissions\r\n");
    assert_eq!(st.get_state(), AgentState::Ready);
    // Chat content with glyph + tool name elsewhere on the line
    drive(
        &mut vt,
        &mut st,
        "⏺ 已拒絕 general 的請求並回報原因：該指令違反 Bash 工具規範\r\n".as_bytes(),
    );
    assert_ne!(
        st.get_state(),
        AgentState::ToolUse,
        "chat content with glyph + distant tool name must NOT trigger ToolUse"
    );
}

// --- Sprint 34 PR-3: RateLimit expiry tests ---

#[test]
fn rate_limit_expires_after_300s_window() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::RateLimit, 301);
    t.tick();
    assert_eq!(
        t.get_state(),
        AgentState::Ready,
        "RateLimit must expire to Ready after 300s"
    );
}

#[test]
fn rate_limit_remains_sticky_before_300s() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::RateLimit, 250);
    t.tick();
    assert_eq!(
        t.get_state(),
        AgentState::RateLimit,
        "RateLimit must stay sticky before 300s window"
    );
}

// --- Sprint 34 PR-4: OpenCode provider error tests ---

#[test]
fn opencode_provider_error_triggers_error_state() {
    let patterns = StatePatterns::for_backend(&Backend::OpenCode);
    assert_eq!(
        patterns.detect("Error from provider: Anthropic API error: 39 validation errors"),
        Some(AgentState::ApiError),
        "provider error must trigger ApiError state"
    );
    assert_eq!(
        patterns.detect("request validation errors for tools[0]"),
        Some(AgentState::ApiError),
        "request validation errors must trigger ApiError state"
    );
}

#[test]
fn opencode_normal_pane_does_not_trigger_error() {
    let patterns = StatePatterns::for_backend(&Backend::OpenCode);
    assert_ne!(
        patterns.detect("Ask anything  tab agents"),
        Some(AgentState::ApiError),
        "normal opencode pane must not trigger ApiError"
    );
}

// ── ServerRateLimit detection tests ──────────────────────────────

#[test]
fn server_rate_limit_pattern_detected() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let screen =
        "API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited";
    assert_eq!(
        patterns.detect(screen),
        Some(AgentState::ServerRateLimit),
        "must detect Anthropic server-side rate limit"
    );
}

#[test]
fn server_rate_limit_distinct_from_usage_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // ServerRateLimit
    let server_msg = "Server is temporarily limiting requests";
    assert_eq!(
        patterns.detect(server_msg),
        Some(AgentState::ServerRateLimit)
    );
    // UsageLimit (different pattern)
    let usage_msg = "Usage limit reached. Resets at 15:14 UTC";
    let detected = patterns.detect(usage_msg);
    assert_ne!(
        detected,
        Some(AgentState::ServerRateLimit),
        "usage limit must NOT be ServerRateLimit"
    );
}

#[test]
fn generic_rate_limit_still_detected() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // #848: the narrowed RateLimit pattern keys on Anthropic API
    // `error.type` field values (`rate_limit_error` for HTTP 429)
    // instead of the pre-#848 casual `overloaded` substring match —
    // bare `overloaded` is too ambiguous (discussion prose,
    // generic system-load messages, etc.) and previously caused
    // false RateLimit classification on agents debugging the bug.
    // The new pattern still distinguishes RateLimit (project-level
    // 429) from ServerRateLimit (transient throttle) at the
    // wire-format level.
    let screen = "Anthropic API responded with rate_limit_error: too many requests";
    assert_eq!(
        patterns.detect(screen),
        Some(AgentState::RateLimit),
        "rate_limit_error must trigger RateLimit (not ServerRateLimit)"
    );
}

// ── Issue #668: generic 5xx classifier extension ─────────────────
//
// Operator observed agents stuck on `API Error: 500/502/503/504` and
// `server-side issue, usually temporary` strings — semantically the
// same transient server fault as the existing "Server is temporarily
// limiting requests" message, but unclassified, so the supervisor
// never re-injected the last input and the agent sat idle. These
// tests pin the expectation that all four common Anthropic 5xx
// status strings and the generic "server-side issue ... temporary"
// phrase route into ServerRateLimit so the supervisor's auto-retry
// path (3-attempt cap at SERVER_RATE_LIMIT_MAX_RETRIES) covers them.

#[test]
fn api_error_5xx_classified_as_server_rate_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let cases = [
        ("500", "API Error: 500 Internal server error"),
        ("502", "API Error: 502 Bad Gateway"),
        ("503", "API Error: 503 Service Unavailable"),
        ("504", "API Error: 504 Gateway Timeout"),
    ];
    for (code, screen) in cases {
        assert_eq!(
            patterns.detect(screen),
            Some(AgentState::ServerRateLimit),
            "API Error: {code} must classify as ServerRateLimit (#668)"
        );
    }
}

#[test]
fn server_side_issue_temporary_classified_as_server_rate_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let screen = "Encountered a server-side issue, usually temporary";
    assert_eq!(
        patterns.detect(screen),
        Some(AgentState::ServerRateLimit),
        "server-side issue + temporary must classify as ServerRateLimit (#668)"
    );
}

#[test]
fn api_error_4xx_not_classified_as_server_rate_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // 4xx is a client error — must NOT trigger 5xx auto-retry path.
    // 401 is also caught by the AuthError pattern, so use 404 here to
    // isolate the 5xx-vs-non-5xx classifier behaviour.
    let screen = "API Error: 404 Not Found";
    assert_ne!(
        patterns.detect(screen),
        Some(AgentState::ServerRateLimit),
        "4xx must NOT trigger ServerRateLimit (#668)"
    );
}

#[test]
fn api_error_long_digit_run_not_classified_as_server_rate_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // \b word boundary after the 3-digit status code rejects strings
    // where the "5xx" is actually a prefix of a longer integer
    // (e.g. timestamp or request id) — guards against false positives
    // like "API Error: 5000123 something else happened".
    let screen = "API Error: 5000123 some other failure";
    assert_ne!(
        patterns.detect(screen),
        Some(AgentState::ServerRateLimit),
        "5-followed-by-many-digits must NOT trigger ServerRateLimit (#668)"
    );
}

// ── #848 PR-A — classifier root cause unit tests ─────────────────
//
// The data-driven "fixture → MANIFEST.expected_final_state" smoke
// test that the PR-A dispatch named `classifier_matches_expected_
// state_per_fixture` already exists in this module as
// `replay_manifest_regression` (line ~2573). That test parses
// MANIFEST.yaml, drives every fixture through the full
// VTerm + StateTracker pipeline, and asserts both
// `expected_transitions` AND `expected_final_state`. The 5 new
// #848 fixtures land directly in its coverage via MANIFEST.yaml.
// The 6 individual unit tests below provide focused regression
// signal for the specific narrowed-pattern alternations, on top
// of (not instead of) the broader replay_manifest_regression sweep.

/// Fixture-based RateLimit detection across backends — each fixture
/// must classify as RateLimit under the narrowed post-#848 pattern.
#[test]
fn rate_limit_fixture_triggers_rate_limit_per_backend() {
    let cases: &[(Backend, &str)] = &[
        (
            Backend::ClaudeCode,
            "tests/fixtures/state-replay/claude-rate-limit-429.raw",
        ),
        (
            Backend::OpenCode,
            "tests/fixtures/state-replay/opencode-rate-limit-typical.raw",
        ),
        (
            Backend::Gemini,
            "tests/fixtures/state-replay/gemini-rate-limit-typical.raw",
        ),
        (
            Backend::KiroCli,
            "tests/fixtures/state-replay/kiro-rate-limit-typical.raw",
        ),
    ];
    for (backend, path) in cases {
        let bytes = std::fs::read(path).expect("read fixture");
        let text = String::from_utf8_lossy(&bytes);
        let mut t = tracker_at(backend, AgentState::Idle, 0);
        t.feed(&text);
        assert_eq!(
            t.get_state(),
            AgentState::RateLimit,
            "{backend:?} fixture {path} must trigger RateLimit"
        );
    }
}

/// Individual: the existing server-throttle wording must keep
/// classifying as ServerRateLimit (regression-proof for the pattern
/// already in src/state.rs — #848 leaves the existing alternation
/// intact and only ADDS new alternations alongside it).
#[test]
fn claude_server_throttle_fixture_triggers_server_rate_limit() {
    let bytes = std::fs::read("tests/fixtures/state-replay/claude-server-throttle.raw")
        .expect("read fixture");
    let text = String::from_utf8_lossy(&bytes);
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(&text);
    assert_eq!(t.get_state(), AgentState::ServerRateLimit);
}

/// Individual: the new 529 overload wording must classify as
/// ServerRateLimit (new alternation in the narrowed pattern).
/// Pre-#848 this fell through (`overloaded` lowercase didn't match
/// the capitalized `Overloaded`; the 5xx pattern required no
/// intervening text between `API Error: ` and the digits).
#[test]
fn claude_overloaded_529_fixture_triggers_server_rate_limit() {
    let bytes = std::fs::read("tests/fixtures/state-replay/claude-overloaded-529.raw")
        .expect("read fixture");
    let text = String::from_utf8_lossy(&bytes);
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(&text);
    assert_eq!(t.get_state(), AgentState::ServerRateLimit);
}

/// Fixture-based UsageLimit detection across backends — each fixture
/// must classify as UsageLimit (patterns added by #848).
#[test]
fn usage_limit_fixture_triggers_usage_limit_per_backend() {
    let cases: &[(Backend, &str)] = &[
        (
            Backend::ClaudeCode,
            "tests/fixtures/state-replay/claude-session-limit.raw",
        ),
        (
            Backend::OpenCode,
            "tests/fixtures/state-replay/opencode-usage-limit-typical.raw",
        ),
        (
            Backend::KiroCli,
            "tests/fixtures/state-replay/kiro-usage-limit-typical.raw",
        ),
    ];
    for (backend, path) in cases {
        let bytes = std::fs::read(path).expect("read fixture");
        let text = String::from_utf8_lossy(&bytes);
        let mut t = tracker_at(backend, AgentState::Idle, 0);
        t.feed(&text);
        assert_eq!(
            t.get_state(),
            AgentState::UsageLimit,
            "{backend:?} fixture {path} must trigger UsageLimit"
        );
    }
}

/// **CRITICAL**: discussion prose containing rate_limit / rate-limit /
/// overloaded substrings must NOT trigger RateLimit across any backend.
/// Root cause of #841 nudge spam; post-#848 narrowed patterns fix this.
#[test]
fn discussion_text_does_not_trigger_rate_limit_any_backend() {
    let fixture_cases: &[(Backend, &str)] = &[
        (
            Backend::ClaudeCode,
            "tests/fixtures/state-replay/claude-discussion-text.raw",
        ),
        (
            Backend::OpenCode,
            "tests/fixtures/state-replay/opencode-discussion-text.raw",
        ),
        (
            Backend::Gemini,
            "tests/fixtures/state-replay/gemini-discussion-text.raw",
        ),
        (
            Backend::KiroCli,
            "tests/fixtures/state-replay/kiro-discussion-text.raw",
        ),
    ];
    for (backend, path) in fixture_cases {
        let bytes = std::fs::read(path).expect("read fixture");
        let text = String::from_utf8_lossy(&bytes);
        let mut t = tracker_at(backend, AgentState::Idle, 0);
        t.feed(&text);
        assert_ne!(
            t.get_state(),
            AgentState::RateLimit,
            "{backend:?} discussion text from {path} must NOT trigger RateLimit"
        );
    }
    // Codex: inline prose (no fixture file)
    let mut t = tracker_at(&Backend::Codex, AgentState::Idle, 0);
    t.feed(
        "We are debugging the rate_limit classification issue in src/state.rs. \
         The current Codex regex matches rate-limit and rate_limit as substring \
         matches anywhere in PTY scrollback.",
    );
    assert_ne!(
        t.get_state(),
        AgentState::RateLimit,
        "Codex discussion prose must NOT trigger RateLimit"
    );
}

// ── #919 Phase A: red-SGR anchor tests ──────────────────────────────
//
// The anchor gate suppresses HIGH_FP pattern matches that lack a
// red SGR escape in the raw-byte ring within ANCHOR_WINDOW_BYTES /
// ANCHOR_WINDOW_MS of the matched phrase. The 4 tests pin:
//   1. anchor_suppresses_prose_match — prose injected without
//      color → pattern matches but anchor fails → no transition
//   2. anchor_allows_backend_error — same phrase with red SGR
//      nearby → pattern matches AND anchor succeeds → transition
//   3. ring_buffer_rotation — ring caps at RAW_RING_CHUNKS; oldest
//      entries evicted (no panic; bounded memory)
//   4. window_ms_expiry — chunks older than ANCHOR_WINDOW_MS are
//      ignored by the anchor check
//
// All tests are §3.20 SOP 1 deterministic — no sleep-based timing
// assertions; window_ms_expiry mutates the ring's chunk timestamps
// directly via a test seam.

/// #919 RED 1: HIGH_FP phrase appears in screen text (post-vterm
/// strip) but the raw ring has no red SGR near the phrase →
/// transition suppressed. Pre-#919 behavior: bare pattern match
/// fires transition unconditionally. Post-#919: suppressed.
#[test]
fn anchor_suppresses_prose_match_without_red_sgr() {
    let backend = Backend::ClaudeCode;
    let mut tracker = StateTracker::new(Some(&backend));
    // Simulate the inject_to_agent path: ANSI-stripped prose lands
    // in the raw ring with no color codes.
    let prose = b"[AGEND-MSG] Server is temporarily limiting requests";
    tracker.feed_raw(prose);
    // The screen (post-vterm strip) would contain the same phrase.
    tracker.feed(std::str::from_utf8(prose).expect("ASCII test fixture"));
    assert_ne!(
        tracker.get_state(),
        AgentState::ServerRateLimit,
        "#919: HIGH_FP match without red-SGR anchor must NOT fire transition. Got state {:?}",
        tracker.get_state()
    );
}

/// #919 RED 2: HIGH_FP phrase with red SGR nearby in raw ring →
/// transition fires (the real backend-error path).
#[test]
fn anchor_allows_backend_error_with_red_sgr() {
    let backend = Backend::ClaudeCode;
    let mut tracker = StateTracker::new(Some(&backend));
    // Real backend error: red SGR wraps the phrase.
    let raw_error = b"\x1b[31mServer is temporarily limiting requests\x1b[0m";
    tracker.feed_raw(raw_error);
    // Post-vterm-strip screen view: SGR escapes stripped, phrase
    // intact. (Simulated here; in production the vterm does the
    // strip.)
    tracker.feed("Server is temporarily limiting requests");
    assert_eq!(
        tracker.get_state(),
        AgentState::ServerRateLimit,
        "#919: HIGH_FP match WITH red-SGR anchor must fire transition. Got state {:?}",
        tracker.get_state()
    );
}

/// #919 RED 3: ring buffer caps at RAW_RING_CHUNKS. Push 12 chunks,
/// only last 10 retained.
#[test]
fn anchor_ring_buffer_rotation_caps_at_chunks_limit() {
    let backend = Backend::ClaudeCode;
    let mut tracker = StateTracker::new(Some(&backend));
    for i in 0..12u32 {
        // Each chunk is a unique byte-tag so we can identify
        // retention order. Truncation cap (RAW_CHUNK_MAX) doesn't
        // fire — bytes are short.
        let tag = format!("chunk-{i}");
        tracker.feed_raw(tag.as_bytes());
    }
    let ring = tracker.raw_ring();
    assert_eq!(
        ring.len(),
        RAW_RING_CHUNKS,
        "ring must cap at {} entries; got {}",
        RAW_RING_CHUNKS,
        ring.len()
    );
    // First two entries should have been evicted (chunk-0, chunk-1).
    // Last entry should be chunk-11.
    let first_bytes = &ring.front().expect("non-empty").bytes;
    let last_bytes = &ring.back().expect("non-empty").bytes;
    assert_eq!(
        first_bytes, b"chunk-2",
        "oldest should be chunk-2 after eviction"
    );
    assert_eq!(last_bytes, b"chunk-11", "newest should be chunk-11");
}

#[test]
fn find_subslice_characterization() {
    assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
    assert_eq!(find_subslice(b"hello world", b"hello"), Some(0));
    assert_eq!(find_subslice(b"hello world", b"xyz"), None);
    assert_eq!(find_subslice(b"hello", b"hello world"), None);
    assert_eq!(find_subslice(b"", b"a"), None);
    assert_eq!(find_subslice(b"\x1b[31mERR\x1b[0m", b"\x1b[31m"), Some(0));
}

/// #919 RED 4: chunks older than ANCHOR_WINDOW_MS are ignored by
/// the anchor check.
///
/// Deterministic without sleep: we directly mutate the chunk's `at`
/// timestamp to simulate an aged chunk. has_red_ansi_anchor then
/// rejects it on the freshness check.
#[test]
fn anchor_window_ms_expiry_ignores_stale_chunks() {
    let backend = Backend::ClaudeCode;
    let mut tracker = StateTracker::new(Some(&backend));
    // Push a red-SGR-bearing chunk.
    let raw = b"\x1b[31mServer is temporarily limiting requests\x1b[0m";
    tracker.feed_raw(raw);
    // Anchor should succeed immediately (chunk is fresh).
    assert!(
        has_red_ansi_anchor(
            tracker.raw_ring(),
            "Server is temporarily limiting requests",
            Instant::now(),
        ),
        "fresh chunk with red SGR must anchor"
    );
    // Synthetic clock advance: pretend the test runs ANCHOR_WINDOW_MS + 1s
    // after the chunk was pushed by querying anchor with a future Instant.
    let future = Instant::now() + ANCHOR_WINDOW_MS + Duration::from_secs(1);
    assert!(
        !has_red_ansi_anchor(
            tracker.raw_ring(),
            "Server is temporarily limiting requests",
            future,
        ),
        "stale chunk (> ANCHOR_WINDOW_MS old at query time) must NOT anchor"
    );
}

/// #919 bonus (optional 5th): backend opt-out wiring. Shell backend
/// has should_anchor_on_red() == false → tracker's anchor_on_red
/// field is false → HIGH_FP match fires transition without anchor
/// check (fail-open for non-managed backends).
#[test]
fn anchor_backend_opt_out_for_shell_fires_transition_without_anchor() {
    let backend = Backend::Shell;
    let mut tracker = StateTracker::new(Some(&backend));
    // Shell backend has no StatePatterns (initial_state == Ready;
    // patterns is None). This test confirms the wiring path —
    // even if patterns existed, anchor_on_red would be false.
    // Direct introspection: the field is private but
    // should_anchor_on_red() is the source of truth.
    assert!(
        !backend.should_anchor_on_red(),
        "Shell backend must opt out of red-SGR anchor"
    );
    // Push a prose match with NO red SGR; if anchor_on_red were
    // true, the gate would suppress. For Shell it's a moot point
    // (no patterns), but the wiring is correct.
    let _ = &mut tracker; // silence unused-mut warning
}

// ── #685 PR-2: F9 productive-marker freshness/dedup ─────────────────

/// Build a viewport-shaped screen text with a productive marker at
/// the indicated row. Helper for the freshness-dedup test trio.
fn screen_with_marker_at_row(marker: &str, marker_row: usize, total_rows: usize) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(total_rows);
    for i in 0..total_rows {
        if i == marker_row {
            lines.push(marker.to_string());
        } else {
            lines.push(format!("placeholder content row {i}"));
        }
    }
    lines.join("\n")
}

/// #685 PR-2 T1: a productive marker landing in the recent tail
/// (last MARKER_SCAN_TAIL_LINES rows) MUST refresh
/// `last_productive_output`. Anti-regression for the fix not over-
/// rotating into rejecting legit fresh markers.
#[test]
fn t1_685_pr2_active_marker_in_recent_tail_refreshes_productive_output() {
    let mut tracker = StateTracker::new(Some(&Backend::Codex));
    tracker.last_productive_output = Instant::now() - Duration::from_secs(10);
    let before = tracker.last_productive_output;

    // Place marker in the recent tail (last 5 rows). Codex
    // productive markers include `apply_patch` (from sub-task 6).
    let screen = screen_with_marker_at_row(
        "apply_patch succeeded for /tmp/foo.txt",
        38, // total_rows=40, MARKER_SCAN_TAIL_LINES=5 ⇒ rows 35-39 in tail
        40,
    );
    tracker.feed(&screen);

    assert!(
        tracker.last_productive_output > before,
        "fresh marker in recent tail must refresh last_productive_output"
    );
}

/// #685 PR-2 T2: a stale productive marker at the TOP of the
/// viewport (outside the last MARKER_SCAN_TAIL_LINES) MUST NOT
/// refresh `last_productive_output`. The exact bug class the
/// reviewer flagged: scrollback completion markers masking grey-
/// failure detection.
#[test]
fn t2_685_pr2_stale_marker_outside_recent_tail_does_not_refresh() {
    let mut tracker = StateTracker::new(Some(&Backend::Codex));
    tracker.last_productive_output = Instant::now() - Duration::from_secs(60);
    let before = tracker.last_productive_output;

    // Marker at row 5 of a 40-row viewport — far outside the
    // last-5 tail (rows 35-39). Pre-PR-2 this would refresh.
    let screen = screen_with_marker_at_row("apply_patch succeeded for /tmp/foo.txt", 5, 40);
    tracker.feed(&screen);

    assert_eq!(
        tracker.last_productive_output, before,
        "#685 PR-2: stale marker in scrollback (row 5 of 40) must NOT refresh last_productive_output"
    );
}

/// #685 PR-2 T3: grey-failure simulation. Marker once in the
/// recent tail then scrolled out of the tail by a long burst of
/// silence-producing content (e.g. spinner ticks) — second feed
/// MUST NOT refresh the timer. Combined effect: real stuck agent
/// (output trickling but no fresh productive evidence) becomes
/// detectable via the productive path.
#[test]
fn t3_685_pr2_grey_failure_trickle_does_not_refresh_after_marker_scrolled_out() {
    let mut tracker = StateTracker::new(Some(&Backend::Codex));
    tracker.last_productive_output = Instant::now() - Duration::from_secs(60);

    // Feed 1: marker in recent tail (fresh) — DOES refresh.
    let screen1 = screen_with_marker_at_row("apply_patch succeeded for /tmp/foo.txt", 38, 40);
    tracker.feed(&screen1);
    let after_fresh = tracker.last_productive_output;

    // Feed 2 (grey-failure shape): marker scrolled to row 2 by a
    // burst of spinner output below. Tail-5 = rows 35-39, no
    // marker. MUST NOT refresh again.
    let screen2 = screen_with_marker_at_row("apply_patch succeeded for /tmp/foo.txt", 2, 40);
    tracker.feed(&screen2);

    assert_eq!(
        tracker.last_productive_output, after_fresh,
        "#685 PR-2: scrolled-out marker must NOT re-refresh — grey-failure detection requires fresh evidence"
    );
}

/// #685 PR-2 RC1 T5 (reviewer #1013 regression-pin): stale marker
/// stays at a fixed row in the tail across feeds while an ADJACENT
/// line (e.g. a spinner cycling through `⠋⠙⠹⠸`) changes around
/// it. Pre-RC1 the dedup hashed the entire tail → spinner tick
/// changed the hash → false refresh fired. Post-RC1 the dedup
/// hashes ONLY the matched marker substring → adjacent-line
/// changes don't break dedup → no refresh.
#[test]
fn t5_685_pr2_rc1_changing_adjacent_line_does_not_refresh() {
    let mut tracker = StateTracker::new(Some(&Backend::Codex));
    tracker.last_productive_output = Instant::now() - Duration::from_secs(60);

    // Feed 1: stale marker in tail (row 36 of 40), spinner-A at
    // row 38. Note marker is in last-5-rows window so it WILL
    // match — but it's stale evidence (operator caused this in
    // some past tool run that's still visible in viewport).
    let lines1: Vec<String> = (0..40)
        .map(|i| match i {
            36 => "apply_patch succeeded for /tmp/foo.txt".to_string(),
            38 => "⠋ Working...".to_string(),
            _ => format!("background row {i}"),
        })
        .collect();
    tracker.feed(&lines1.join("\n"));
    let after_first = tracker.last_productive_output;

    // Feed 2: SAME marker at SAME row 36, spinner-A → spinner-B
    // (next braille tick) at row 38. Tail changes (spinner) but
    // matched marker substring unchanged → dedup MUST suppress
    // refresh.
    let lines2: Vec<String> = (0..40)
        .map(|i| match i {
            36 => "apply_patch succeeded for /tmp/foo.txt".to_string(),
            38 => "⠙ Working...".to_string(), // ← tick changed
            _ => format!("background row {i}"),
        })
        .collect();
    tracker.feed(&lines2.join("\n"));

    assert_eq!(
        tracker.last_productive_output, after_first,
        "#685 PR-2 RC1: stale marker + adjacent spinner tick must NOT re-refresh — \
         dedup hashes matched substring, not surrounding context"
    );
}

/// #685 PR-2 T4: same-tail-content dedup. When the recent tail
/// stays identical across two feeds (some screen change OUTSIDE
/// the tail breaks the top-level hash-dedup gate), the productive
/// timer must not double-refresh — no fresh evidence.
#[test]
fn t4_685_pr2_identical_recent_tail_does_not_double_refresh() {
    let mut tracker = StateTracker::new(Some(&Backend::Codex));
    tracker.last_productive_output = Instant::now() - Duration::from_secs(60);

    // Feed 1: marker in tail, plus arbitrary content above.
    let mut screen1: Vec<String> = (0..35).map(|i| format!("scrollback row {i}")).collect();
    screen1.push("apply_patch succeeded for /tmp/foo.txt".to_string());
    screen1.push("(no other change)".to_string());
    screen1.push("(no other change)".to_string());
    screen1.push("(no other change)".to_string());
    screen1.push("(no other change)".to_string());
    let s1 = screen1.join("\n");
    tracker.feed(&s1);
    let after_first = tracker.last_productive_output;

    // Feed 2: ABOVE-tail content changes (forces top-level hash to
    // differ → feed() body runs) but the LAST 5 rows are
    // identical to feed 1. Per #685 PR-2 dedup, refresh must NOT
    // re-fire — same recent context, no new evidence.
    let mut screen2: Vec<String> = (0..35)
        .map(|i| format!("scrollback row {i} CHANGED"))
        .collect();
    screen2.push("apply_patch succeeded for /tmp/foo.txt".to_string());
    screen2.push("(no other change)".to_string());
    screen2.push("(no other change)".to_string());
    screen2.push("(no other change)".to_string());
    screen2.push("(no other change)".to_string());
    let s2 = screen2.join("\n");
    tracker.feed(&s2);

    assert_eq!(
        tracker.last_productive_output, after_first,
        "#685 PR-2: identical recent tail across feeds must NOT double-refresh"
    );
}

// ── #1073 API Error detection tests ──────────────────────────

#[test]
fn claude_api_error_529_triggers_server_rate_limit() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(
        "API Error: 529 Overloaded. This is a server-side issue, \
         usually temporary — try again in a moment.",
    );
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "529 Overloaded must trigger ServerRateLimit"
    );
}

#[test]
fn claude_api_error_500_triggers_server_rate_limit() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: 500 Internal Server Error");
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "500 must trigger ServerRateLimit"
    );
}

#[test]
fn claude_api_error_503_triggers_server_rate_limit() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: 503 Service Unavailable");
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "503 must trigger ServerRateLimit"
    );
}

#[test]
fn claude_api_error_403_triggers_auth_error() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: 403 Forbidden");
    assert_eq!(
        t.get_state(),
        AgentState::AuthError,
        "#1073: 403 must trigger AuthError"
    );
}

#[test]
fn claude_api_error_401_triggers_auth_error() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: 401 Unauthorized");
    assert_eq!(
        t.get_state(),
        AgentState::AuthError,
        "#1073: 401 must trigger AuthError"
    );
}

#[test]
fn claude_api_error_401_does_not_false_positive_on_prose() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("The server returned a 4013 error code in the response body.");
    assert_ne!(
        t.get_state(),
        AgentState::AuthError,
        "word-boundary must prevent false positive on 4013"
    );
}

// ── #1125 M3: for_backend OnceLock caching ──────────────────────

/// #1125 M3: `for_backend` must return the same `&'static` reference
/// on repeated calls — proving the OnceLock cache is active.
#[test]
fn for_backend_returns_cached_static_ref() {
    let p1 = StatePatterns::for_backend(&Backend::ClaudeCode);
    let p2 = StatePatterns::for_backend(&Backend::ClaudeCode);
    assert!(
        std::ptr::eq(p1, p2),
        "for_backend must return the same &'static ref (OnceLock cached)"
    );
}

/// #1125 M3 source-pin: `for_backend` must contain `OnceLock` so
/// regex compilation is cached per-backend per-process.
#[test]
fn for_backend_uses_oncelock() {
    let src = include_str!("patterns.rs");
    let fn_start = src
        .find("pub fn for_backend(")
        .expect("for_backend must exist");
    let rest = &src[fn_start..];
    let fn_end = rest.find("\n    fn compile_for(").unwrap_or(rest.len());
    let body = &rest[..fn_end];
    assert!(
        body.contains("OnceLock"),
        "for_backend must use OnceLock for caching (#1125 M3)"
    );
}

// ── #1125 M4: classify_pty_output delegates to for_backend ──────

/// #1125 M4: classify_pty_output must NOT contain its own LazyLock
/// regexes — it delegates to `for_backend` as the single source of truth.
#[test]
fn classify_pty_output_delegates_to_for_backend() {
    let src = include_str!("patterns.rs");
    let fn_start = src
        .find("pub fn classify_pty_output(")
        .expect("classify_pty_output must exist");
    let rest = &src[fn_start..];
    let fn_end = rest
        .find("\npub fn ")
        .or_else(|| rest.find("\nfn "))
        .unwrap_or(rest.len());
    let body = &rest[..fn_end];
    assert!(
        body.contains("StatePatterns::for_backend"),
        "classify_pty_output must delegate to for_backend (#1125 M4)"
    );
    assert!(
        !body.contains("LazyLock"),
        "classify_pty_output must NOT maintain its own LazyLock regexes — \
         single source of truth is for_backend (#1125 M4)"
    );
}

/// #1125 M4: Claude UsageLimit patterns must produce QuotaExceeded
/// via classify_pty_output (post-unification regression pin).
#[test]
fn classify_pty_output_claude_usage_limit() {
    use crate::health::BlockedReason;
    let result = classify_pty_output(&Backend::ClaudeCode, "You've hit your session limit");
    assert_eq!(
        result,
        Some(BlockedReason::QuotaExceeded),
        "Claude UsageLimit must map to QuotaExceeded"
    );
}

/// #1125 M4: Claude ServerRateLimit patterns must produce RateLimit
/// via classify_pty_output (post-unification regression pin).
#[test]
fn classify_pty_output_claude_server_rate_limit() {
    use crate::health::BlockedReason;
    let result = classify_pty_output(
        &Backend::ClaudeCode,
        "Server is temporarily limiting requests",
    );
    assert_eq!(
        result,
        Some(BlockedReason::RateLimit {
            retry_after_secs: None
        }),
        "Claude ServerRateLimit must map to RateLimit"
    );
}

// ── #1136: Network error → ServerRateLimit (auto-retry) ──────────

#[test]
fn network_error_econnreset_triggers_server_rate_limit() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    assert_eq!(
        patterns.detect("Error: ECONNRESET"),
        Some(AgentState::ServerRateLimit),
        "#1136: ECONNRESET must route to ServerRateLimit for auto-retry"
    );
}

#[test]
fn network_error_patterns_all_backends() {
    let cases = [
        "ECONNRESET",
        "ETIMEDOUT",
        "connection reset",
        "socket hang up",
        "fetch failed",
    ];
    let backends = [
        Backend::ClaudeCode,
        Backend::KiroCli,
        Backend::Codex,
        Backend::OpenCode,
        Backend::Gemini,
    ];
    for backend in &backends {
        let patterns = StatePatterns::for_backend(backend);
        for case in &cases {
            assert_eq!(
                patterns.detect(case),
                Some(AgentState::ServerRateLimit),
                "#1136: '{case}' must trigger ServerRateLimit on {backend:?}"
            );
        }
    }
}
