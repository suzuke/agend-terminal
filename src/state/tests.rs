use super::patterns::is_generic_startup_prompt;
use super::*;
use crate::health::HealthTracker;
use crate::vterm::CellFg;

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
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn raw_backend_starts_in_ready_not_starting() {
    let t = StateTracker::new(Some(&Backend::Raw("/opt/whatever".to_string())));
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn managed_backends_still_start_in_starting() {
    // Keep the handshake for real backends so their
    // onboarding / auth prompts have a chance to pattern-match before
    // we declare Ready.
    for backend in [
        Backend::ClaudeCode,
        Backend::KiroCli,
        Backend::Agy,
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
        AgentState::Idle,
        "expected Ready after INTERACTIVE_EXPIRY, still {:?}",
        t.get_state()
    );
}

#[test]
fn permission_prompt_also_expires_to_ready() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 130);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn tool_use_still_uses_short_expiry() {
    // Regression guard against accidentally widening the short
    // expiry — Thinking / ToolUse should still drop at 30 s.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 31);
    t.tick();
    assert_eq!(t.get_state(), AgentState::Idle);
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
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);

    // Idle held for 6s (>= 5s passive hold) — SHOULD transition
    let mut t = tracker_at(&backend, AgentState::Idle, 6);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);
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
// All tests in this section read `oscillation_guard_window()`, now a fixed
// 30s const (#env-cleanup: the `AGEND_OSCILLATION_GUARD_WINDOW_SECS` override
// was demoted). The `#[serial]` markers below are retained as-is — they no
// longer guard against env-var bleed (none remains) but are harmless.

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

/// Phase A2 companion (#1005 dev-2 fixture finding): opencode's
/// in-flight `~ Reading file...` banner must match ToolUse. Pre-A2
/// the alternation `[✱→]\s+(Read|...)` missed this form so a
/// session that sustained `~ Reading…` without firing `✱`/`→`
/// never entered ToolUse. Fixture: opencode-tooluse.raw byte ~30720.
#[test]
#[serial_test::serial]
fn opencode_tilde_dash_ing_matches_tooluse() {
    let backend = Backend::OpenCode;
    let mut t = tracker_at(&backend, AgentState::Idle, 6);
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
    assert_eq!(t.get_state(), AgentState::Idle);
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
    let mut t = tracker_at(&Backend::Agy, AgentState::Thinking, 31);
    // Fresh screen content that matches no pattern for agy.
    t.feed("some unrelated output that matches nothing");
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn feed_fallback_does_not_expire_before_threshold() {
    // Under the threshold Thinking must stay — legitimate thinking can
    // run for tens of seconds with a quiet but still-active spinner.
    let mut t = tracker_at(&Backend::Agy, AgentState::Thinking, 10);
    t.feed("some unrelated output that matches nothing");
    assert_eq!(t.get_state(), AgentState::Thinking);
}

#[test]
fn feed_fallback_expires_tooluse_after_threshold() {
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
    t.feed("no tool banner, no ready footer visible here");
    assert_eq!(t.get_state(), AgentState::Idle);
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
    assert_eq!(t.get_state(), AgentState::Idle);
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
    assert_eq!(t.get_state(), AgentState::Idle);
    // Second tick on Ready is a no-op (Ready is not expiring).
    t.tick();
    assert_eq!(t.get_state(), AgentState::Idle);
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
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 60);
    let since_before = t.since;
    t.feed("arbitrary text without any markers");
    assert_eq!(t.get_state(), AgentState::Idle);
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
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("Here's an example: `git clean -n (y/n)` — the -n flag previews");
    assert_eq!(t.get_state(), AgentState::Idle);
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
    let mut t = tracker_at(&Backend::Agy, AgentState::Thinking, 31);
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
    // #1546: the bare "Do you want to …" question prefix is NO LONGER an anchor
    // (it false-positived on prose / pasted docs / test content). It must NOT
    // fire PermissionPrompt on its own.
    assert_ne!(
        patterns.detect("Do you want to create /tmp/out.txt?"),
        Some(AgentState::PermissionPrompt),
        "#1546: bare 'Do you want to …' prose must NOT fire PermissionPrompt",
    );
    assert_eq!(
        patterns.detect("   2. Yes, allow all edits during this session (shift+tab)"),
        Some(AgentState::PermissionPrompt),
        "allow-all-edits option must fire PermissionPrompt",
    );
}

#[test]
fn codex_permission_chrome_anchor_1559() {
    // #1559 (cross-backend of #1546): codex PermissionPrompt keys on the
    // live-dialog CHROME (header + footer) + the one distinctive, non-prose
    // option (`No, and tell Codex what to do differently`) — each fires alone.
    // The prose-echoable bare words (`Request approval`, `approve`, `deny`, bare
    // `Yes, proceed`) were CUT: they content-FP'd on a reviewer's prose /
    // quoted approval discussion (and `approve|deny|Request approval` never
    // matched real codex anyway — fixture commit e0716ec).
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    // The three real-dialog anchors — each must fire on its own (FN-safety:
    // an approval frame is caught even if one line is off-screen).
    for anchor in [
        "Would you like to run the following command?",
        "Press enter to confirm or esc to cancel",
        "› 3. No, and tell Codex what to do differently (esc)",
    ] {
        assert_eq!(
            patterns.detect(anchor),
            Some(AgentState::PermissionPrompt),
            "codex dialog anchor {anchor:?} must fire PermissionPrompt",
        );
    }
    // FP-block: bare approval words in prose (NOT a live dialog) must NOT fire.
    for prose in [
        "I'll approve this PR once CI is green",
        "Yes, proceed with the merge",
        "the reviewer will deny the request",
        "Request approval from the lead before merging",
    ] {
        assert_ne!(
            patterns.detect(prose),
            Some(AgentState::PermissionPrompt),
            "#1559: bare approval prose {prose:?} must NOT fire PermissionPrompt",
        );
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
    // Claude's "❯" idle pattern should not match on Agy's tracker
    let agy_patterns = StatePatterns::for_backend(&Backend::Agy);
    let detected = agy_patterns.detect("❯");
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
    assert_eq!(t.get_state(), AgentState::Idle);
}

#[test]
fn idle_detection() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    // First get to Ready so that Idle (lower prio than Starting) can be tested
    // Starting → Ready (higher prio) is instant
    t.feed("bypass permissions");
    assert_eq!(t.get_state(), AgentState::Idle);
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
    // PermissionPrompt (priority 8) > Thinking (priority 6) — instant.
    // #1546: trigger via the chrome footer (the new anchor), not bare "Allow once".
    t.feed("Esc to cancel · Tab to amend");
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
    assert!(AgentState::AwaitingOperator.priority() < AgentState::Idle.priority());
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
fn set_awaiting_operator_fires_from_runtime_prompt_states() {
    // #1552: a mid-task stall on a permission/interactive prompt must be able
    // to escalate to AwaitingOperator (was Starting-only before).
    for s in [AgentState::PermissionPrompt, AgentState::InteractivePrompt] {
        let mut t = tracker_at(&Backend::ClaudeCode, s, 10);
        t.set_awaiting_operator();
        assert_eq!(
            t.current,
            AgentState::AwaitingOperator,
            "runtime prompt state {s:?} must escalate"
        );
    }
}

#[test]
fn set_awaiting_operator_noop_from_non_starting() {
    // #1552: only Starting + the runtime prompt states (PermissionPrompt /
    // InteractivePrompt) transition. Every OTHER state is a no-op so a late
    // tick-loop detection can't corrupt a healthy mid-task agent.
    for s in [
        AgentState::Idle,
        AgentState::Idle,
        AgentState::Thinking,
        AgentState::ToolUse,
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
    assert_eq!(t.current, AgentState::Idle);
}

// ── Full pipeline: PTY bytes → VTerm → screen → StateTracker ────────
//
// Exercises the production path `agent::pty_read_loop` takes: push
// raw bytes (with ANSI escapes) through the vterm, pull tail_lines of
// the screen, feed to state. Without these, unit tests can drift from
// how detection actually behaves once vterm rendering is involved —
// wrapped lines, cleared screens, scroll-off, etc.

use crate::vterm::VTerm;

/// Drive one full PTY cycle: process bytes, snapshot screen + color mask,
/// feed state. Mirrors the production `agent::pty_read` seam exactly
/// (`tail_lines_with_fg` + `feed_with_fg`), so the #1450 color anchor is
/// exercised end-to-end through the real vterm — no hand-written escapes.
fn drive(vterm: &mut VTerm, state: &mut StateTracker, bytes: &[u8]) {
    vterm.process(bytes);
    let rows = vterm.rows() as usize;
    let (screen, fg) = vterm.tail_lines_with_fg(rows);
    state.feed_with_fg(&screen, &fg);
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
    assert_eq!(st.get_state(), AgentState::Idle);
}

#[test]
fn pipeline_codex_ready_via_vterm() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive(&mut vt, &mut st, b"\x1b[1mOpenAI Codex\x1b[0m v0.120.0\r\n");
    assert_eq!(st.get_state(), AgentState::Idle);
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
        AgentState::Idle,
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
    assert!(matches!(st.get_state(), AgentState::Idle));
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
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);
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
    assert!(t.take_recovery_notice().is_none());

    // Enter InteractivePrompt.
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    // Still nothing to report — we only arm when we LEAVE the blocked
    // state, not when we enter it.
    assert!(t.take_recovery_notice().is_none());

    // Dismiss → Ready.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Idle);
    assert_eq!(t.get_state(), AgentState::Idle);

    // First take fires; subsequent ticks within the same Ready don't
    // re-spam.
    assert!(
        t.take_recovery_notice().is_some(),
        "recovery must arm on exit"
    );
    assert!(
        t.take_recovery_notice().is_none(),
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
    assert!(st.take_recovery_notice().is_none());

    // Fresh Ready banner appears. Ready (prio 3) > AwaitingOperator
    // (prio 2) so the transition is immediate.
    drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
    assert_eq!(st.get_state(), AgentState::Idle);
    assert!(
        st.take_recovery_notice().is_some(),
        "recovery must arm on AwaitingOperator → Ready"
    );
}

#[test]
fn recovery_notice_not_armed_for_unrelated_transitions() {
    // Ready → Thinking → Ready must not arm the recovery notice: the
    // operator never saw a blocked state, so "ready again" is noise.
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.current = AgentState::Idle;
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
    st.transition(AgentState::Thinking);
    assert_eq!(st.get_state(), AgentState::Thinking);
    st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
    st.transition(AgentState::Idle);
    assert_eq!(st.get_state(), AgentState::Idle);
    assert!(st.take_recovery_notice().is_none());
}

// ── #2033: recovery-notice episode gate inputs ──

/// A notified, long-enough block produces an episode the supervisor gate treats
/// as actionable (notice_sent + full duration captured).
#[test]
fn recovery_episode_captures_notified_long_block_2033() {
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    t.transition(AgentState::InteractivePrompt);
    assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    // Operator was told about the block (supervisor forwarded the Stall).
    t.mark_blocked_notice_sent();
    // Simulate a block that lasted well past the recovery threshold.
    t.blocked_since = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
    // Leave the blocked state.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Idle);
    let ep = t
        .take_recovery_notice()
        .expect("recovery must arm on leaving a blocked state");
    assert!(
        ep.notice_sent,
        "#2033: episode records the block WAS notified"
    );
    assert!(
        ep.block_duration >= std::time::Duration::from_secs(60),
        "#2033: episode spans the full block duration, got {:?}",
        ep.block_duration
    );
}

/// A self-resolving block the operator was NEVER told about produces an episode
/// with `notice_sent=false` — the supervisor gate makes the recovery log-only.
#[test]
fn recovery_episode_unnotified_block_2033() {
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    t.transition(AgentState::InteractivePrompt);
    // NO mark_blocked_notice_sent — e.g. role-gated forward, or a transient block.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Idle);
    let ep = t
        .take_recovery_notice()
        .expect("recovery flag still arms on leaving");
    assert!(
        !ep.notice_sent,
        "#2033: an un-notified block must record notice_sent=false (recovery → silent)"
    );
}

/// A second blocked episode must NOT inherit the first's `notice_sent` — the
/// per-episode latch resets on each fresh blocked-state entry.
#[test]
fn blocked_episode_resets_notice_sent_on_reentry_2033() {
    let mut t = tracker_at(&Backend::Codex, AgentState::Starting, 0);
    // Episode 1: notified.
    t.transition(AgentState::InteractivePrompt);
    t.mark_blocked_notice_sent();
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Idle);
    let _ = t.take_recovery_notice();
    // Episode 2: NOT notified — must not carry over episode 1's flag.
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::InteractivePrompt);
    t.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
    t.transition(AgentState::Idle);
    let ep = t.take_recovery_notice().expect("episode 2 arms");
    assert!(
        !ep.notice_sent,
        "#2033: re-entry resets notice_sent — no stale notified flag from a prior episode"
    );
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
    // Codex 0.120.0 approval dialog. #1559: anchor on the live-dialog CHROME
    // (header + footer) + the one distinctive option (`No, and tell Codex what
    // to do differently`) — all three present in the real box (codex-perm.raw),
    // each fires alone. The prose-echoable rows are NOT anchors.
    let patterns = StatePatterns::for_backend(&Backend::Codex);
    for anchor in [
        "  Would you like to run the following command?",
        "  Press enter to confirm or esc to cancel",
        "› 3. No, and tell Codex what to do differently (esc)",
    ] {
        assert_eq!(
            patterns.detect(anchor),
            Some(AgentState::PermissionPrompt),
            "expected PermissionPrompt for anchor {anchor:?}"
        );
    }
    // The prose-echoable option row alone (no chrome / no distinctive option)
    // must NOT fire — `Yes, proceed` is dropped.
    assert_ne!(
        patterns.detect("  1. Yes, proceed (y)"),
        Some(AgentState::PermissionPrompt),
        "#1559: bare `Yes, proceed` option row must NOT fire PermissionPrompt on its own"
    );
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
    assert_eq!(st.get_state(), AgentState::Idle);
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
        "ready" => AgentState::Idle,
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
        "model_unsupported" => AgentState::ModelUnsupported,
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
    t.feed("shell requires approval");
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
    t.feed("shell requires approval");
    assert_eq!(t.get_state(), AgentState::PermissionPrompt);
}

#[test]
fn no_heartbeat_allows_permission_prompt() {
    // Pin 3 — no heartbeat ever: default behavior preserved.
    let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
    // last_heartbeat is None by default
    t.feed("shell requires approval");
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
    assert_eq!(st.get_state(), AgentState::Idle);
    // #1541: Claude spinner uses random verbs; the verb-agnostic structural
    // anchor (sparkle glyph + `<verb>…`) must fire regardless of the verb.
    // `\xe2\x9c\xbb` = ✻ (U+273B sparkle), `\xe2\x80\xa6` = … (U+2026).
    drive(&mut vt, &mut st, b"\xe2\x9c\xbb Cogitating\xe2\x80\xa6\r\n");
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
    assert_eq!(st.get_state(), AgentState::Idle);
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
    assert_eq!(st.get_state(), AgentState::Idle);
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
    assert_eq!(st.get_state(), AgentState::Idle);
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
    assert_eq!(st.get_state(), AgentState::Idle);
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
        AgentState::Idle,
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

/// #2090: ContextFull has NO narrow-pane rescue caller (unlike UsageLimit — see
/// `usagelimit_hard_wrapped_banner_rescued_2090`). A hard-wrapped `compacting
/// context` that the single-line regex misses must therefore stay benign (Idle),
/// NOT relatch to ContextFull — guarding against accidentally re-amplifying the
/// context-prose false positives (#2090) via a content-blind rescue.
#[test]
fn contextfull_narrow_wrap_not_rescued_2090() {
    // "compacting context" hard-wrapped across narrow rows → the single-line
    // ContextFull regex misses it; the ❯ prompt lands Idle. There is no
    // ContextFull rescue caller, so the state stays Idle.
    let screen = "the agent is\n\
                  compacting\n\
                  context now\n\
                  \n\
                  ❯\n";
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(screen);
    assert_ne!(
        t.get_state(),
        AgentState::ContextFull,
        "#2090: ContextFull has no rescue caller — a hard-wrapped phrase the raw \
         regex misses must stay benign (Idle), not relatch to ContextFull"
    );
}

/// strip CSI/SGR escapes to recover the plain screen_text from an ansi_colored_tail.
fn strip_ansi_2089(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// #2089 — the REAL captured fixup-reviewer-2 screen REPRODUCES the suppression:
/// a narrow-pane hard-wrapped SRL → single-line `detect_with_match` misses it and
/// matches the idle `❯` prompt → classified Idle. (This capture is truncated to
/// the bottom-15 rows the #1562 instrument logs, so it lacks the wrapped
/// `⏺ API Error:` prefix that lived higher on the real full-height pane — hence
/// it can't be *rescued* here; the full-pane latch is pinned in the test below.)
#[test]
fn srl_narrow_wrap_real_capture_reproduces_idle_suppression_2089() {
    let raw =
        std::fs::read_to_string("tests/fixtures/state-replay/claude-srl-narrow-wrap-2089.raw")
            .expect("real reviewer-2 fixture");
    let screen = strip_ansi_2089(&raw);
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    assert_ne!(
        patterns.detect_with_match(&screen).map(|(s, _)| s),
        Some(AgentState::ServerRateLimit),
        "#2089: the single-line regex misses the hard-wrapped SRL (this is the miss)"
    );
}

/// #2089 dual-control ① — a narrow-pane hard-wrapped SRL with its `⏺ API Error:`
/// indicator wrapped a few rows above the phrase (the REAL full-height layout)
/// must LATCH. Pre-fix: `detect_with_match` matched the `❯` idle prompt (the
/// wrapped SRL is invisible to the single-line regex) → Idle, and the `None`-arm
/// hard-wrap fallback was unreachable. The fix runs that fallback on the benign
/// Idle arm too (A) and widens the indicator search window (B).
#[test]
fn srl_hard_wrapped_narrow_pane_latches_2089() {
    // Faithful reconstruction of the reviewer-2 narrow pane (matches the real
    // capture's wrap pattern): the SRL error hard-wrapped (Ink `\n` per row, NOT
    // a soft-wrap); `⏺ API Error:` sits ABOVE the bottom-15 window (the real
    // capture started mid-phrase at "Server is"); a Baked spinner + the live `❯`
    // input box below.
    let screen = "⏺ API Error:\n\
                  Server is\n\
                  temporarily\n\
                  limiting\n\
                  requests (not\n\
                  your usage\n\
                  limit) · Rate\n\
                  limited\n\
                  \n\
                  ✻ Baked for 23s\n\
                  \n\
                  ────────────────────────\n\
                  ❯\n\
                  ────────────────────────\n\
                  Model: Opus 4.8\n\
                  bypass permissions\n";
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // Suppression precondition: single-line detect does NOT see the SRL.
    assert_ne!(
        patterns.detect_with_match(screen).map(|(s, _)| s),
        Some(AgentState::ServerRateLimit),
        "precondition: the hard-wrapped SRL is invisible to the single-line regex"
    );
    // B precondition: the `⏺ API Error:` indicator is OUTSIDE the bottom-15
    // (ERROR_TAIL_SCAN_LINES) window but INSIDE the widened HARD_WRAP_TAIL_LINES
    // window — so the rescue only succeeds because B widened the indicator search.
    assert!(
        !recent_screen_tail(screen, ERROR_TAIL_SCAN_LINES).contains("API Error"),
        "B precondition: indicator must be ABOVE the old bottom-15 window"
    );
    assert!(
        recent_screen_tail(screen, HARD_WRAP_TAIL_LINES).contains("API Error"),
        "B precondition: the widened window must reach the wrapped indicator"
    );
    // The fix: full feed rescues it via the flattened hard-wrap fallback.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(screen);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2089: a hard-wrapped SRL with an adjacent (wrapped) API Error: indicator must latch"
    );
}

/// #2089 dual-control ② — FP guard: a conversation line that merely MENTIONS a
/// throttle word ("limiting"/"throttle") with NO error indicator nearby must NOT
/// be misclassified as a throttle — even though (A) now runs the flattened
/// fallback on benign Idle/Thinking screens. The (B) indicator-adjacency guard
/// is what holds the line.
#[test]
fn prose_throttle_mention_without_indicator_not_misclassified_2089() {
    let screen = "⏺ I updated the rate limiter so the server stops\n\
                  limiting requests during the throttle test window.\n\
                  \n\
                  ────────────────────────\n\
                  ❯\n\
                  ────────────────────────\n";
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(screen);
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2089 FP guard: prose mentioning 'limiting/throttle' with no error indicator must NOT latch"
    );
    assert_ne!(
        t.get_state(),
        AgentState::RateLimit,
        "#2089 FP guard: prose mention must not latch RateLimit either"
    );
}

// ── #2090 P2: UsageLimit hard-wrap rescue + ContextFull broad-arm tighten ──────

/// #2090 P2: the UsageLimit banner structural guard requires BOTH the `⎿`
/// box-draw prefix AND a `resets` stamp within proximity of the phrase — so a real
/// (flattened) hard-wrapped banner passes while a prose quote (missing either)
/// does not. Pins the both-required logic directly (the `\n`-less anchor analogue
/// of SRL's `throttle_indicator_adjacent`).
#[test]
fn usagelimit_banner_guard_2090() {
    let phrase = "You've hit your weekly limit";
    // real flattened banner: box-draw before + reset stamp after → accept.
    assert!(usagelimit_banner_adjacent(
        "⎿ You've hit your weekly limit · resets 4am",
        phrase
    ));
    // prose quote: neither marker → reject.
    assert!(!usagelimit_banner_adjacent(
        "the agent reported You've hit your weekly limit in its summary",
        phrase
    ));
    // box-draw but no reset stamp → reject (both required).
    assert!(!usagelimit_banner_adjacent(
        "⎿ You've hit your weekly limit and then stopped working",
        phrase
    ));
    // reset stamp but no box-draw → reject.
    assert!(!usagelimit_banner_adjacent(
        "note: You've hit your weekly limit, it resets soon enough",
        phrase
    ));
}

/// #2090 P2 (RED→GREEN, real §3.9 capture): the narrow-pane usage-limit banner
/// from the live hardwrap_miss shadow — "You've hit your weekly limit · resets 4am"
/// hard-wrapped across rows with the idle `❯` below, so single-line detect lands
/// Idle. The flatten+guard rescue must recover UsageLimit. Pre-fix (no rescue) the
/// agent read Idle while actually quota-blocked.
#[test]
fn usagelimit_hard_wrapped_banner_rescued_2090() {
    let raw =
        std::fs::read_to_string("tests/fixtures/state-replay/claude-usagelimit-narrow-wrap.raw")
            .expect("real usage-limit narrow-wrap fixture");
    let screen = strip_ansi_2089(&raw);
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    // precondition: the hard-wrapped banner is invisible to single-line detect.
    assert_ne!(
        patterns.detect(&screen),
        Some(AgentState::UsageLimit),
        "#2090 precondition: hard-wrapped banner must miss the single-line regex"
    );
    // the rescue (feed's benign arm) recovers UsageLimit.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(&screen);
    assert_eq!(
        t.get_state(),
        AgentState::UsageLimit,
        "#2090 P2: hard-wrapped usage-limit banner must be rescued to UsageLimit"
    );
}

/// #2090 P2 FP guard (e2e): a WRAPPED prose quote of the banner phrase — no `⎿`
/// box-draw, no `resets` stamp — must NOT be rescued. The flatten finds the phrase
/// but the structural guard rejects it, so the agent stays Idle (not UsageLimit).
#[test]
fn usagelimit_wrapped_prose_quote_not_rescued_2090() {
    // The quote is itself hard-wrapped (so the raw single-line path misses it and
    // the flatten path is the one under test), but carries neither banner marker.
    let screen = "⏺ I think the\n\
                  \"You've hit your\n\
                  weekly limit\"\n\
                  message wraps in\n\
                  a narrow pane.\n\
                  \n\
                  ❯\n";
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(screen);
    assert_ne!(
        t.get_state(),
        AgentState::UsageLimit,
        "#2090 P2 FP guard: a wrapped prose quote (no box-draw/reset stamp) must not latch UsageLimit"
    );
}

/// #2090 P2: the ContextFull broad arm is char-bounded (`context.{0,16}(full|limit)`,
/// was `context.*(full|limit)`). Real same-context wording still matches; the
/// cross-sentence "context … limit" prose (the dominant ~97% shadow FP) no longer
/// does. RED on the old unbounded `.*`.
#[test]
fn contextfull_broad_arm_bounded_2090() {
    let p = StatePatterns::for_backend(&Backend::ClaudeCode);
    // real wording within the bound still classifies ContextFull.
    assert_eq!(
        p.detect("context window full"),
        Some(AgentState::ContextFull),
        "#2090: real 'context window full' must still match the bounded arm"
    );
    assert_eq!(
        p.detect("compacting context"),
        Some(AgentState::ContextFull),
        "#2090: the concrete 'compacting context' arm is kept verbatim"
    );
    // cross-sentence prose (a real shadow FP shape) no longer matches ContextFull.
    let prose =
        "context 95%, harness will auto-summarize; fixup-dev is transient rate_limit not usage_limit";
    assert_ne!(
        p.detect(prose),
        Some(AgentState::ContextFull),
        "#2090: bounded arm must not match cross-sentence 'context … limit' prose"
    );
}

/// #2086 SPIKE: reproduce the live incident at the state.feed level — an IDLE
/// agent, then a static ServerRateLimit screen appears and persists for many
/// ticks. Does feed() latch (and stay latched)? Instrument every feed.
#[test]
fn spike_2086_idle_then_static_srl_latches_at_feed_level() {
    let srl = String::from_utf8_lossy(
        &std::fs::read("tests/fixtures/state-replay/claude-server-throttle.raw").expect("fixture"),
    )
    .to_string();
    let idle = "│ > Try \"edit <filepath>\" to get started                                    │\n";

    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    // Phase 1: idle frames (establish a non-throttle last_screen_hash + Idle).
    for i in 0..3 {
        t.feed(idle);
        eprintln!("[2086] idle feed {i}: state={:?}", t.get_state());
    }
    // Phase 2: SRL appears (hash changes → first classification).
    t.feed(&srl);
    eprintln!("[2086] SRL first feed: state={:?}", t.get_state());
    let after_first = t.get_state();
    // Phase 3: static SRL persists (same hash) for many ticks (the 26-min stall).
    for i in 0..20 {
        t.feed(&srl);
        if i < 3 || i == 19 {
            eprintln!("[2086] static SRL feed {i}: state={:?}", t.get_state());
        }
    }
    eprintln!(
        "[2086] FINAL after_first={after_first:?} final={:?}",
        t.get_state()
    );
    assert_eq!(
        after_first,
        AgentState::ServerRateLimit,
        "#2086: a static SRL screen appearing while Idle must latch SRL at the feed level"
    );
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2086: SRL must STAY latched across static repeats (hash-dedup throttle-hint override)"
    );
}

/// #2086 SPIKE: reproduce through the REAL production path — SRL fixture BYTES
/// → VTerm render → tail_lines_with_fg → feed_with_fg (exactly agent/mod.rs:1490),
/// NOT the raw fixture string. The fixture starts with `\x1b[2J\x1b[H` so the
/// SRL line renders at row 1 of a tall terminal with blank rows below.
#[test]
fn spike_2086_srl_via_vterm_render_path() {
    let bytes =
        std::fs::read("tests/fixtures/state-replay/claude-server-throttle.raw").expect("fixture");

    let mut vt = crate::vterm::VTerm::new(80, 24);
    vt.process(&bytes);
    let rows = vt.rows() as usize;
    let (screen, fg) = vt.tail_lines_with_fg(rows);
    eprintln!(
        "[2086] vterm-rendered screen ({} chars), SRL line index from top:",
        screen.len()
    );
    for (i, l) in screen.lines().enumerate() {
        let t = l.trim_end();
        if !t.is_empty() {
            eprintln!("[2086]   row {i}: {t:?}");
        }
    }
    let total = screen.lines().count();
    let srl_row = screen.lines().position(|l| l.contains("limiting requests"));
    eprintln!("[2086] total rows={total}, SRL at row={srl_row:?}, ERROR_TAIL_SCAN_LINES=15");

    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed_with_fg(&screen, &fg);
    eprintln!("[2086] state after VTERM-path feed = {:?}", t.get_state());
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2086: SRL via the real vterm render path must latch (if this FAILS, that IS the bug)"
    );
}

/// #2086 SPIKE — FAITHFUL repro of the live incident layout: the real claude
/// screen renders the SRL error HIGH up, then the live input box (`────`/`❯`) at
/// the BOTTOM (the state-transitions.jsonl pty_snippet was `────…` = the input
/// box border, NOT the error). So the SRL match sits ABOVE the bottom-15-row
/// tail the #1518 position gate scans → suppressed → never latched. (The
/// synthetic fixture `\x1b[2J`-clears to a 5-row screen with SRL at the bottom,
/// which is why it latches and never reproduced the bug.)
#[test]
fn spike_2086_srl_above_input_box_is_suppressed_by_position_gate() {
    // 24-row terminal: SRL error at row 1, blanks, input box at rows 22-23.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice(b"\x1b[2;1H\x1b[31mAPI Error: Server is temporarily limiting requests (not your usage limit) \xc2\xb7 check status.claude.com\x1b[0m");
    bytes.extend_from_slice(b"\x1b[4;1HRetrying automatically...");
    // Live input box at the very bottom (what claude re-renders under the error).
    bytes.extend_from_slice(b"\x1b[22;1H");
    bytes.extend_from_slice("─".repeat(80).as_bytes());
    bytes.extend_from_slice(b"\x1b[23;1H\xe2\x9d\xaf ");

    let mut vt = crate::vterm::VTerm::new(80, 24);
    vt.process(&bytes);
    let (screen, fg) = vt.tail_lines_with_fg(vt.rows() as usize);
    let total = screen.lines().count();
    let srl_row = screen.lines().position(|l| l.contains("limiting requests"));
    eprintln!("[2086] tall screen: total_rows={total} SRL_at_row={srl_row:?} tail=bottom-15");
    let dist = srl_row.map(|r| total.saturating_sub(r));
    eprintln!("[2086] SRL distance-from-bottom={dist:?} (>15 ⇒ position gate suppresses)");

    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed_with_fg(&screen, &fg);
    // Regression pin (a DISTINCT mechanism from #2086): when the SRL error is
    // genuinely far above the live tail with NO working marker below it, the
    // #1518 position gate legitimately suppresses it as a scrolled-off stale
    // error. This is kept to prove the #2086 working-marker fix did NOT weaken
    // the position gate's stale-error job (#1518/#919).
    assert_eq!(
        t.get_state(),
        AgentState::Idle,
        "position gate must still suppress a genuinely scrolled-off SRL (no working marker below)"
    );
}

/// #2086 dual-control ① — the REAL incident screen (captured by the #1562
/// instrument, `unclassified_errors.jsonl` ts=2026-06-13T10:00:55Z, which the
/// daemon mis-classified as `thinking` for ~26 min). Layout: SRL error, then a
/// "· Stewing…" spinner (a Thinking marker) BELOW it, then the input box — and
/// NO "Retrying automatically…" chrome. With NO recent productive output
/// (`recovered_within=false`) the spinner is a STUCK rate-limited retry, not
/// recovery → the SRL must STAY latched (pre-#2086 the #1768 working_state_below
/// override swallowed it → Thinking → never latched → no recovery).
#[test]
fn srl_stuck_spinner_below_error_stays_latched_2086() {
    let bytes = std::fs::read("tests/fixtures/state-replay/claude-srl-stewing-spinner-2086.raw")
        .expect("real incident fixture");
    let mut vt = crate::vterm::VTerm::new(120, 40);
    vt.process(&bytes);
    let (screen, fg) = vt.tail_lines_with_fg(vt.rows() as usize);

    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    // recovered_within=false (default: last_productive_output=None).
    t.feed_with_fg(&screen, &fg);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2086: a working spinner below the SRL with NO recent productive output is a \
         stuck retry — the SRL must stay latched (pre-fix it was swallowed to Thinking)"
    );
}

/// #2086 dual-control ② — GENUINE recovery must still win (don't break
/// #1768/#1769). The SAME screen, but with RECENT productive output
/// (`recovered_within=true`) → the working marker below the error IS real
/// recovery → land the working state, NOT SRL.
#[test]
fn srl_yields_to_working_marker_when_recently_productive_2086() {
    let bytes = std::fs::read("tests/fixtures/state-replay/claude-srl-stewing-spinner-2086.raw")
        .expect("real incident fixture");
    let mut vt = crate::vterm::VTerm::new(120, 40);
    vt.process(&bytes);
    let (screen, fg) = vt.tail_lines_with_fg(vt.rows() as usize);

    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    // Recent productive output ⇒ recovered_within(SERVER_RATE_LIMIT_RECOVERY_SILENCE)=true.
    t.last_productive_output = Some(Instant::now());
    t.feed_with_fg(&screen, &fg);
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2086: with recent productive output the working marker below IS genuine \
         recovery (#1768/#1769) — must land the working state, not re-latch the stale SRL"
    );
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

/// #1523 #1470-1: the real net-error phrasing "Connection reset by peer"
/// (capital `C`) must classify as ServerRateLimit. The shared
/// SERVER_RATE_LIMIT_NET_ERRORS const carries the lowercase `connection reset`
/// token; pre-#1470-1 it was compiled case-SENSITIVELY, so the capitalised
/// real wording silently MISSED (agent read Idle while the socket dropped).
/// The `(?i:…)` fold rescues it. The fixture deliberately carries NO
/// `ECONNRESET` (which would have matched case-sensitively anyway), so this
/// test isolates the case-fold: mutating the const back to a bare alternation
/// (drop the `(?i:…)`) flips this RED.
#[test]
fn claude_connection_reset_capital_c_triggers_server_rate_limit_1470() {
    let bytes = std::fs::read("tests/fixtures/state-replay/claude-net-connection-reset.raw")
        .expect("read fixture");
    let text = String::from_utf8_lossy(&bytes);
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed(&text);
    assert_eq!(t.get_state(), AgentState::ServerRateLimit);
}

/// #1523 #1470-1 iron-rule (unit form): the case-fold must NOT widen what
/// counts as the token. A bare prose line that merely MENTIONS
/// "Connection reset by peer" — with no error-line shape — must NOT classify
/// as ServerRateLimit. (The broad raw pattern matches the substring; the
/// `in_error_line_excluding_input` content anchor is what suppresses it —
/// `detect()` alone would match, `feed()`'s gate is the guard.)
#[test]
fn connection_reset_in_prose_line_is_not_server_rate_limit_1470() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let prose = "We hit Connection reset by peer while testing the retry path yesterday.";
    // Raw detect matches the broad token …
    assert_eq!(patterns.detect(prose), Some(AgentState::ServerRateLimit));
    // … but the content anchor rejects it (no error-line shape on the line).
    assert!(
        !crate::state::patterns::in_error_line_excluding_input(prose, "Connection reset", &["❯"]),
        "#1470-1: a bare prose mention must not satisfy the error-line content anchor"
    );
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

// ── #1450 Phase A: VTerm cell-color anchor tests ────────────────────
//
// #1450 replaced the #919 raw-byte red-SGR ring with a check on the
// RESOLVED vterm grid-cell foreground color. These tests drive REAL SGR
// byte streams through the vterm (via `drive`), exercising the exact
// production seam (`tail_lines_with_fg` + `feed_with_fg`) rather than
// hand-asserted color flags.
//
// The #919 RCA: its sole fixture hand-wrote an idealized
// `\x1b[31m`+contiguous-phrase shape that the production
// COLORTERM=truecolor PTY never produces — so truecolor reds and Ink
// redraw fragmentation went untested and the anchor silently suppressed
// real rate-limit errors (operator incident 2026-05-29). This suite
// exercises all three SGR encodings × single/fragmented framing (FN
// matrix) and the plain-prose false-positive classes (FP matrix), and
// asserts the INTENDED contract, never the buggy pre-fix output.

/// Full Claude-Code rate-limit error line. The ServerRateLimit regex
/// matches the `Server is temporarily limiting requests` substring.
const SRL_LINE: &str = "API Error: Server is temporarily limiting requests (not your usage limit)";

// Red in each of the three SGR encodings the daemon's COLORTERM=truecolor
// PTY can elicit from a chalk/Ink backend.
const RED_16: &str = "\x1b[31m"; // standard 16-color red
const RED_256: &str = "\x1b[38;5;196m"; // 256-color cube red (r=5,g=0,b=0)
const RED_TRUE: &str = "\x1b[38;2;215;40;40m"; // 24-bit truecolor red (chalk-style)
const SGR_RESET: &str = "\x1b[0m";

/// Render a colored error line in ONE PTY write (single chunk).
fn drive_colored_line(vt: &mut VTerm, st: &mut StateTracker, sgr: &str, text: &str) {
    let bytes = format!("\x1b[2J\x1b[H{sgr}{text}{SGR_RESET}\r\n");
    drive(vt, st, bytes.as_bytes());
}

/// Render a colored error line FRAGMENTED across many PTY writes (one char
/// per `process` call). Emulates Ink's char-by-char redraw that shattered
/// the #919 raw-byte contiguous-substring search across chunk boundaries.
/// The vterm reassembles the grid, so the cell-color anchor must still fire.
fn drive_fragmented_colored_line(vt: &mut VTerm, st: &mut StateTracker, sgr: &str, text: &str) {
    drive(vt, st, format!("\x1b[2J\x1b[H{sgr}").as_bytes());
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        drive(vt, st, ch.encode_utf8(&mut buf).as_bytes());
    }
    drive(vt, st, format!("{SGR_RESET}\r\n").as_bytes());
}

/// Render the phrase in DEFAULT foreground (no SGR) — the shape produced by
/// injected prose / generated discussion / user typing (all uncolored).
fn drive_plain_line(vt: &mut VTerm, st: &mut StateTracker, text: &str) {
    let bytes = format!("\x1b[2J\x1b[H{text}\r\n");
    drive(vt, st, bytes.as_bytes());
}

fn claude_tracker() -> (VTerm, StateTracker) {
    (
        VTerm::new(120, 24),
        StateTracker::new(Some(&Backend::ClaudeCode)),
    )
}

// ── FN matrix: a REAL red error MUST fire ServerRateLimit ───────────
// 3 encodings × {single chunk, fragmented} = 6 cases. Each proves the
// anchor reads color off the resolved grid regardless of SGR encoding or
// raw-byte framing.

#[test]
fn fn_red_16color_single_chunk_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_colored_line(&mut vt, &mut st, RED_16, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

#[test]
fn fn_red_16color_fragmented_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_fragmented_colored_line(&mut vt, &mut st, RED_16, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

#[test]
fn fn_red_256color_single_chunk_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_colored_line(&mut vt, &mut st, RED_256, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

#[test]
fn fn_red_256color_fragmented_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_fragmented_colored_line(&mut vt, &mut st, RED_256, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

#[test]
fn fn_red_truecolor_single_chunk_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_colored_line(&mut vt, &mut st, RED_TRUE, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

#[test]
fn fn_red_truecolor_fragmented_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_fragmented_colored_line(&mut vt, &mut st, RED_TRUE, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
}

// ── #1808/#1809 NARROW-pane soft-wrap regression ────────────────────

/// #1808/#1809 NAMED REGRESSION (live repro 2026-06-08, fixup-dev + dev-2,
/// BOTH in narrow split panes) — would FAIL before the de-wrap fix.
///
/// In a NARROW pane alacritty soft-wraps the long SRL line across several
/// physical grid rows. The detection feed (`tail_lines_with_fg` → the
/// `feed_with_fg` ingress) used to join EVERY physical row with `\n`,
/// inserting `\n` MID-PHRASE ("Server is\ntemporarily\nlimiting\nrequests").
/// The single-line SRL regex (backend_profile.rs) then failed to match across
/// the `\n` → `detect_with_match` returned None → ServerRateLimit never
/// latched → no auto-retry → the agent hung. WIDE panes (lead) kept the phrase
/// on one physical row → matched → recovered: the lead-vs-worker asymmetry
/// that mystified the whole session. The fix de-wraps soft-wrapped rows
/// (WRAPLINE-aware) before the regex sees them. Drives the REAL vterm →
/// `tail_lines_with_fg` → `feed_with_fg` entry (§3.9 — not a hand-fed `\n`
/// string), at a width that forces alacritty to soft-wrap.
#[test]
fn regression_1808_narrow_pane_wrapped_srl_latches() {
    // 25 cols: "Server is temporarily limiting requests" (39 chars) cannot fit
    // on one physical row → alacritty soft-wraps the phrase across rows.
    let mut vt = VTerm::new(25, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    // Render RED like the real Ink error (vterm resolves the SGR off the grid).
    drive_colored_line(&mut vt, &mut st, RED_16, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1808: a soft-wrapped SRL line in a narrow pane must latch ServerRateLimit \
         (pre-fix: physical-row `\\n` joins split the phrase → single-line regex \
         missed → no detection → autopilot hang)"
    );
}

/// #1808 companion — the SAME narrow-pane wrapped SRL line rendered PLAIN
/// (no red) must ALSO latch via the content-anchor (`in_error_line` sees the
/// de-wrapped "API Error:" line). Proves the de-wrap — not the color — is what
/// restores detection, and that the content path survives de-wrap.
#[test]
fn regression_1808_narrow_pane_wrapped_srl_plain_content_anchor() {
    let mut vt = VTerm::new(25, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1808: de-wrapped SRL line still qualifies via in_error_line content anchor"
    );
}

/// #1450 NAMED REGRESSION — would FAIL before the fix.
///
/// Pre-#1450 the raw-byte anchor allow-list knew only 4 sixteen-color
/// escapes; a truecolor red error (the daemon ships COLORTERM=truecolor,
/// so chalk/Ink emits `\x1b[38;2;..m`) matched the ServerRateLimit pattern
/// but failed the anchor → suppressed → no auto-retry → autopilot hang
/// (operator incident 2026-05-29, #1450 RCA). Combined with Ink-style
/// fragmentation, the old contiguous-byte search failed twice over. This
/// asserts the INTENDED contract (fires), not the buggy pre-fix output —
/// directly closing the "test-encodes-bug-as-spec" gap that let the bug
/// ship green.
#[test]
fn regression_1450_truecolor_fragmented_rate_limit_fires() {
    let (mut vt, mut st) = claude_tracker();
    drive_fragmented_colored_line(&mut vt, &mut st, RED_TRUE, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1450: truecolor + fragmented red rate-limit error must fire ServerRateLimit \
         (pre-fix: suppressed by 16-color-only raw-byte anchor → autopilot hang)"
    );
}

// ── FP matrix: the SAME phrase rendered PLAIN MUST suppress ─────────
// dev-2's classes — none carry red, none may fire ServerRateLimit. The
// discriminator is purely the rendered cell color.

/// (a) injected prose: daemon-relayed `[AGEND-MSG]` quoting the phrase.
#[test]
fn fp_injected_prose_plain_suppressed() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "[AGEND-MSG] fixup-lead: I hit 'Server is temporarily limiting requests' — diagnosing",
    );
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "injected prose (plain fg) must NOT fire"
    );
}

/// (b) agent-generated discussion: the agent itself writing about the bug.
#[test]
fn fp_generated_discussion_plain_suppressed() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "The pattern matches 'Server is temporarily limiting requests' but I'm just explaining it.",
    );
    assert_ne!(st.get_state(), AgentState::ServerRateLimit);
}

/// (c) user typing the phrase into the input box (echoed in default fg).
#[test]
fn fp_user_typed_plain_suppressed() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "> why did 'Server is temporarily limiting requests' not trigger a retry?",
    );
    assert_ne!(st.get_state(), AgentState::ServerRateLimit);
}

/// (d-plain) historical scrollback rendered as plain text → color
/// suppresses. NOTE the OTHER (d) sub-case — a PAST *red* error lingering
/// in scrollback — is NOT the color anchor's job: it stays red, so color
/// cannot (and must not try to) discriminate it. That staleness is handled
/// by `feed()` hash-dedup + `LATCHED_STATE_EXPIRY` (see the latched-expiry
/// tests above), deliberately out of anchor scope.
#[test]
fn fp_plain_historical_scrollback_suppressed() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "transcript 12:01> Server is temporarily limiting requests (resolved earlier)",
    );
    assert_ne!(st.get_state(), AgentState::ServerRateLimit);
}

// ── Boundary: injected (plain) + real red error on the SAME screen ──
// The regex finds the EARLIER plain occurrence first; the all-occurrence
// scan must still find the red one below → FIRE. Guards the false-negative
// a naive first-match-only color check would introduce.
#[test]
fn boundary_plain_quote_above_red_error_still_fires() {
    let (mut vt, mut st) = claude_tracker();
    let bytes = format!(
        "\x1b[2J\x1b[H[AGEND-MSG] quoting: {SRL_LINE}\r\n{RED_16}{SRL_LINE}{SGR_RESET}\r\n"
    );
    drive(&mut vt, &mut st, bytes.as_bytes());
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "a real red error below a plain quote must still fire (all-occurrence scan)"
    );
}

// ── t-coloranchor-remove-ratelimit: RateLimit/ServerRateLimit CONTENT anchor ──
// Operator-approved after the corpus gate: these two now gate on `in_error_line`
// (error-line shape) instead of the #1450 RED anchor, so a real fault rendered
// PLAIN/grey (codex/gemini net errors, or any error-line-shaped line) FIRES, and
// the prose FP classes still suppress because they lack the `…error:` label.
// ContextFull / ModelUnsupported KEEP the red anchor (see the two tests below).

/// ServerRateLimit: a real error line rendered PLAIN (no red, fg mask present)
/// now FIRES via the content anchor — pre-change the red anchor suppressed it
/// (the #1450/#1757 net-error stuck-agent class, now the general rule).
#[test]
fn srl_plain_error_line_fires_via_content_anchor() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "plain (grey) ServerRateLimit on an error-line-shaped line must fire via \
         the in_error_line content anchor (no red required)"
    );
}

/// #badge-recovery (operator-reported flicker): a ServerRateLimit error still
/// matching in the tail, but with PRODUCTIVE output within the recovery window →
/// the agent recovered → the badge must NOT re-latch ServerRateLimit. The
/// `last_productive_output` signal is position-independent (it works even when the
/// re-injected error is the bottom-most line, defeating `working_state_below`).
#[test]
fn server_rate_limit_recent_productive_does_not_relatch_badge() {
    let (mut vt, mut st) = claude_tracker();
    // Recovered: produced productive output just now (overrides the None default).
    st.last_productive_output = Some(std::time::Instant::now());
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#badge-recovery: a recently-productive agent must NOT re-latch ServerRateLimit"
    );
}

/// #badge-recovery 2-sided: a fresh / just-spawned agent that has NEVER produced
/// (`last_productive_output == None`) is NOT recovery — it must latch
/// ServerRateLimit normally (so the retry + nudge engage). `None` must never be
/// misread as recovery — exactly the creation-stamp ambiguity the Option fix
/// resolves (and the #1795 fresh-agent edge it closes).
#[test]
fn server_rate_limit_fresh_agent_no_productive_history_latches_badge() {
    let (mut vt, mut st) = claude_tracker();
    assert!(
        st.last_productive_output.is_none(),
        "a fresh tracker must start with no productive history (None)"
    );
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#badge-recovery: a never-produced (fresh) agent must latch ServerRateLimit — None is not recovery"
    );
}

/// #1809: a CROSS-CYCLE stale SRL re-match — the agent recovered to a non-SRL
/// state, then the SAME error line is re-grabbed AFTER the recovery window (a CLI
/// clock-tick flips the screen hash → feed re-scans the still-present stale error)
/// — must NOT re-latch ServerRateLimit. Re-latching would schedule another
/// auto-retry → blind `continue` inject: the ~45s phantom storm cheerc traced.
#[test]
fn stale_srl_cross_cycle_rematch_does_not_relatch_1809() {
    let (mut vt, mut st) = claude_tracker();
    // 1. SRL appears while the agent is RECOVERED (recent productive output) →
    //    #badge-recovery lands Idle (not latched) but RECORDS the error signature
    //    (`last_srl_match_sig`) — the same state the storm leaves behind after the
    //    genuine episode resolved.
    st.last_productive_output = Some(std::time::Instant::now());
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "recovered SRL lands Idle (badge) and records the error signature"
    );
    // 2. Agent passes through a clean non-SRL screen → sets non_srl_since_last_srl.
    drive_plain_line(&mut vt, &mut st, "> ready");
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "a non-SRL state"
    );
    // 3. Recovery window expired (no productive output for >45s → recovered=false).
    st.last_productive_output = None;
    // 4. The SAME stale error line is re-grabbed (a CLI clock-tick flips the screen
    //    hash → feed re-scans the still-present old error) → cross-cycle phantom.
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1809: a cross-cycle stale SRL re-match must NOT re-latch (the phantom storm)"
    );
}

/// #1809 no-false-kill: a genuinely-NEW SRL error (DIFFERENT line signature) after
/// a recovery must STILL latch — the stale-ignore is keyed on the error-line
/// signature, so a new error is never suppressed.
#[test]
fn new_srl_error_after_recovery_still_latches_1809() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(st.get_state(), AgentState::ServerRateLimit);
    drive_plain_line(&mut vt, &mut st, "> ready");
    st.last_productive_output = None;
    // A DIFFERENT SRL line (still matches the throttle phrase, different content
    // → different signature).
    drive_plain_line(
        &mut vt,
        &mut st,
        "API Error: Server is temporarily limiting requests (not your usage limit) — attempt 3",
    );
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1809: a genuinely-new SRL error (different signature) must still latch"
    );
}

/// #1809 conservatism: an IN-PLACE re-match (same error, agent STAYED in SRL, no
/// intervening non-SRL state) is `consecutive_rematch`, NOT a cross-cycle phantom —
/// a genuine long throttle. It must NOT be suppressed (the retry must keep firing).
#[test]
fn in_place_srl_rematch_still_latches_1809() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "first SRL latches"
    );
    st.last_productive_output = None;
    // Re-feed the SAME error WITHOUT an intervening non-SRL state → consecutive
    // (not cross-cycle) → a genuine still-throttled agent → must STILL latch.
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#1809: an in-place same-error re-match (still SRL, no recovery) is a genuine throttle — keep latching"
    );
}

/// #badge-recovery boundary: an agent that DID produce, but whose last productive
/// output is PAST the recovery window, is a genuinely-throttled agent → must still
/// latch ServerRateLimit. Confirms the suppression is BOUNDED (it only suppresses
/// recently-productive flicker, never hides a real stuck throttle indefinitely).
#[test]
fn server_rate_limit_stale_productive_past_window_still_latches_badge() {
    let (mut vt, mut st) = claude_tracker();
    // Produced, but 90s ago — past the 45s recovery window → NOT recovered.
    st.last_productive_output = Some(Instant::now() - Duration::from_secs(90));
    drive_plain_line(&mut vt, &mut st, SRL_LINE);
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#badge-recovery: productive output past the recovery window must still latch (bounded suppression)"
    );
}

/// Capture ALL tracing events (any target, TRACE and above) emitted while `f`
/// runs, returning the formatted log text. `tracing_test::traced_test`'s default
/// filter is crate-path-scoped and DROPS the `#1809-srl-swallow-probe`'s custom
/// `target: "state_detection"` (verified empirically), so the probe tests need
/// an unfiltered subscriber installed for the closure's duration.
fn capture_all_logs<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture buf mutex")
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(Buf(buf.clone()))
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(sub, f);
    let bytes = buf.lock().expect("capture buf mutex").clone();
    String::from_utf8(bytes).expect("capture buf is utf8")
}

/// #1809-srl-swallow-probe (Path B — the previously-SILENT gate, prime suspect
/// for the live claude incident): a claude ServerRateLimit that is swallowed by
/// the `recovered_within`→Idle fallback (NO working marker below the error) must
/// now emit the probe naming `path = "recovered_within_idle"` together with the
/// RAW `recovered_within` bool + `productive_silent_secs` — so an investigator
/// can tell WHICH gate ate the SRL (and whether `recovered_within` fired
/// legitimately) instead of guessing. Drives the FULL `feed_with_fg` ingress.
#[test]
fn srl_swallow_probe_names_recovered_within_idle_gate() {
    let (mut vt, mut st) = claude_tracker();
    // Recovered: productive output just now → `recovered_within` is true → the
    // fallback lands Idle, swallowing the SRL (no working marker below it).
    st.last_productive_output = Some(std::time::Instant::now());
    let logs = capture_all_logs(|| {
        drive_plain_line(&mut vt, &mut st, SRL_LINE);
    });
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "precondition: recovered_within must swallow the SRL (Path B)"
    );
    assert!(
        logs.contains("#1809-srl-swallow-probe")
            && logs.contains("path=\"recovered_within_idle\"")
            && logs.contains("recovered_within=true"),
        "Path B (recovered_within→Idle, previously SILENT) must emit the probe \
         naming the gate + the raw recovered_within bool. logs:\n{logs}"
    );
}

/// #2086 fix (was the #1809-srl-swallow-probe Path A WARN): a claude
/// ServerRateLimit with a Thinking marker rendered BELOW it but NO recent
/// productive output (`recovered_within=false`) is a STUCK rate-limited retry
/// spinner, NOT recovery → the SRL must be KEPT latched (pre-#2086 the
/// `working_state_below` override swallowed it → Thinking → 26-min stall). The
/// #2086 log names `path = "working_state_below"` + the masked working state.
#[test]
fn srl_kept_latched_when_working_marker_below_but_not_recovered_2086() {
    let (mut vt, mut st) = claude_tracker();
    // SRL error line with a claude working marker ("thought for Ns" → Thinking)
    // rendered BELOW it. No productive history → `recovered_within` false → the
    // #2086 fix keeps the SRL latched instead of swallowing it.
    let screen = format!("\x1b[2J\x1b[H{SRL_LINE}\r\nthought for 12s\r\n");
    let logs = capture_all_logs(|| {
        drive(&mut vt, &mut st, screen.as_bytes());
    });
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#2086: a working marker below the SRL with NO recent productive output is a \
         stuck retry — the SRL must stay latched, not be swallowed to the working state"
    );
    assert!(
        logs.contains("#2086-srl-keep-latched") && logs.contains("path=\"working_state_below\""),
        "the keep-latched decision must log #2086-srl-keep-latched naming the gate. logs:\n{logs}"
    );
}

/// RateLimit: a claude 429-rejection error rendered PLAIN fires via content.
#[test]
fn rate_limit_plain_error_line_fires_via_content_anchor() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "API Error: Request rejected (429) · this may be a temporary capacity issue",
    );
    assert_eq!(st.get_state(), AgentState::RateLimit);
}

/// The content anchor still rejects a plain line with NO error-label — the prose
/// FP class — pinned against the new gate (the inner phrase quoted without the
/// `API Error:` wrapper is not error-line-shaped).
#[test]
fn srl_plain_prose_without_error_label_still_suppressed() {
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(
        &mut vt,
        &mut st,
        "note: Server is temporarily limiting requests is the phrase we detect",
    );
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "prose without an error-label must stay suppressed under the content anchor"
    );
}

/// ContextFull KEEPS the red anchor (no fixture corpus to prove a content path):
/// a plain ContextFull line does NOT fire; the same line in red does.
#[test]
fn context_full_still_requires_red_anchor() {
    let line = "context window is full, compacting";
    let (mut vt, mut st) = claude_tracker();
    drive_plain_line(&mut vt, &mut st, line);
    assert_ne!(
        st.get_state(),
        AgentState::ContextFull,
        "plain ContextFull (no red) must stay suppressed — it keeps the red anchor"
    );
    let (mut vt2, mut st2) = claude_tracker();
    drive_colored_line(&mut vt2, &mut st2, RED_16, line);
    assert_eq!(
        st2.get_state(),
        AgentState::ContextFull,
        "red ContextFull must fire (anchor unchanged)"
    );
}

/// ModelUnsupported KEEPS the red anchor (never auto-clears + suppresses
/// hang-check, so a verbatim-quote FP would silently disable a healthy agent):
/// a plain codex line does NOT fire; red does.
#[test]
fn model_unsupported_still_requires_red_anchor() {
    let line = "stream error: invalid_request_error: model is not supported";
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive_plain_line(&mut vt, &mut st, line);
    assert_ne!(
        st.get_state(),
        AgentState::ModelUnsupported,
        "plain ModelUnsupported (no red) must stay suppressed — it keeps the red anchor"
    );
    let mut vt2 = VTerm::new(120, 24);
    let mut st2 = StateTracker::new(Some(&Backend::Codex));
    drive_colored_line(&mut vt2, &mut st2, RED_16, line);
    assert_eq!(
        st2.get_state(),
        AgentState::ModelUnsupported,
        "red ModelUnsupported must fire (anchor unchanged)"
    );
}

// ── Backend opt-out: Shell fails the anchor open (pre-#919 behavior) ─
#[test]
fn anchor_backend_opt_out_for_shell() {
    // Shell has should_anchor_on_red() == false → the gate fails open.
    assert!(
        !Backend::Shell.should_anchor_on_red(),
        "Shell backend must opt out of the color anchor"
    );
    assert!(
        Backend::ClaudeCode.should_anchor_on_red(),
        "ClaudeCode must opt into the color anchor"
    );
}

// ── Fail-open: text-only feed() (empty color mask) still fires ──────
// Guarantees the text-only entry point (tests, non-managed callers) keeps
// pre-#919 unconditional behavior — a HIGH_FP pattern match fires without a
// color check when no mask is supplied.
#[test]
fn text_only_feed_fails_open_and_fires() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed("API Error: Server is temporarily limiting requests (not your usage limit)");
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "text-only feed (empty fg mask) must fail open and fire"
    );
}

// ── matched_span_has_red unit coverage ─────────────────────────────
#[test]
fn matched_span_has_red_detects_any_red_occurrence() {
    // fg aligned 1:1 with chars: "ab" red, separator, "ab" plain.
    let screen = "ab\nab";
    let fg = vec![
        CellFg::Default,
        CellFg::Default, // line 1 "ab" (plain)
        CellFg::Default, // '\n'
        CellFg::Red,
        CellFg::Red, // line 2 "ab" (red)
    ];
    assert!(
        super::matched_span_has_red(screen, "ab", &fg),
        "must find the red occurrence even though the first is plain"
    );
    // All-plain → false.
    let fg_plain = vec![CellFg::Default; 5];
    assert!(!super::matched_span_has_red(screen, "ab", &fg_plain));
    // Empty phrase → false (guard).
    assert!(!super::matched_span_has_red(screen, "", &fg));
}

// ── #1634: model-unsupported detection + red-anchor FP boundary ─────

/// #1634: a red-rendered codex model-unsupported error latches
/// `ModelUnsupported`; the SAME wording WITHOUT red must NOT latch (the FP
/// boundary — this reason never auto-clears and suppresses hang-check, so a
/// codex agent merely discussing the error wording must not silently disable
/// itself). Text-only feed fails open (pre-#919 contract).
#[test]
fn codex_model_unsupported_red_anchor_fp_boundary_1634() {
    let screen = "invalid_request_error: model is not supported";
    let n = screen.chars().count();
    let phrase = "invalid_request_error"; // regex's leftmost match
    let plen = phrase.chars().count();

    // RED over the matched phrase → latches ModelUnsupported.
    let mut fg_red = vec![CellFg::Default; n];
    for c in fg_red.iter_mut().take(plen) {
        *c = CellFg::Red;
    }
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &fg_red);
    assert_eq!(
        t.get_state(),
        AgentState::ModelUnsupported,
        "red-rendered codex model error must latch ModelUnsupported"
    );

    // NO RED (plain) → FP boundary: must NOT latch the never-clearing reason.
    let mut t2 = StateTracker::new(Some(&Backend::Codex));
    let fg_plain = vec![CellFg::Default; n];
    t2.feed_with_fg(screen, &fg_plain);
    assert_ne!(
        t2.get_state(),
        AgentState::ModelUnsupported,
        "#1634: same wording without red must NOT latch (prose / dogfood FP boundary)"
    );

    // Text-only feed (empty fg mask) fails open per the #919/#1450 contract.
    let mut t3 = StateTracker::new(Some(&Backend::Codex));
    t3.feed(screen);
    assert_eq!(
        t3.get_state(),
        AgentState::ModelUnsupported,
        "text-only feed (no color mask) fails open and fires"
    );
}

/// #1634: the captured-incident fixture (codex error rendered RED) driven
/// through the production vterm → `tail_lines_with_fg` → `feed_with_fg` path
/// must latch `ModelUnsupported`. End-to-end regression pin for the real
/// silent-error incident (the red SGR is resolved by the vterm, not hand-fed).
#[test]
fn codex_model_unsupported_fixture_replay_1634() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/state-replay/codex-model-unsupported.raw"
    ))
    .expect("read codex-model-unsupported.raw fixture");
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::Codex));
    drive(&mut vt, &mut st, &bytes);
    assert_eq!(
        st.get_state(),
        AgentState::ModelUnsupported,
        "#1634: the red-rendered codex model-unsupported incident must latch ModelUnsupported"
    );
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
    tracker.last_productive_output = Some(Instant::now() - Duration::from_secs(10));
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
    tracker.last_productive_output = Some(Instant::now() - Duration::from_secs(60));
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
    tracker.last_productive_output = Some(Instant::now() - Duration::from_secs(60));

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
    tracker.last_productive_output = Some(Instant::now() - Duration::from_secs(60));

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
    tracker.last_productive_output = Some(Instant::now() - Duration::from_secs(60));

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
    // #1580: `compile_for` (the old body boundary) is deleted; bound at the next
    // method, `detect`.
    let fn_end = rest.find("\n    pub fn detect(").unwrap_or(rest.len());
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

/// #1587 → #1757: a real network error rendered RED latches ServerRateLimit (the
/// auto-retry path these tokens exist for). #1757 SUPERSEDED the original #1587
/// "prose-in-default does NOT latch" half: codex/gemini render hard net errors in
/// DEFAULT (not red), so the red anchor was suppressing REAL faults → no
/// auto-retry → stuck agent. Net-error tokens are now exempt from the red anchor,
/// so the same wording in DEFAULT ALSO latches; the residual prose/source FP is
/// bounded by #1518/#1586/#1760. The dropped `network error` token still must not
/// trigger. (The FP-prone api_error/overloaded/context HIGH_FP tokens remain
/// red-anchored — see `codex_model_unsupported_red_anchor_fp_boundary_1634 (a non-net-error HIGH_FP still red-anchored)`.)
#[test]
fn server_rate_limit_red_anchor_fp_boundary_1587() {
    // Realistic Node rendering carries an `Error:` label prefix.
    let screen = "Error: socket hang up";
    let n = screen.chars().count();

    // RED-rendered error → latches ServerRateLimit (auto-retry preserved).
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Red; n]);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1587: a real network error rendered red must still latch ServerRateLimit"
    );

    // #1757: net-error tokens are now EXEMPT from the red anchor. codex/gemini
    // render `InvalidHTTPResponse`/`ECONNRESET`/`socket hang up` in DEFAULT/grey
    // (not red), so the anchor wrongly suppressed REAL faults → no auto-retry →
    // stuck agent. The SAME wording in DEFAULT therefore now LATCHES — the #1587
    // net-error red-anchor boundary is deliberately superseded; the residual
    // prose/source FP (these tokens appear in code/logs) is bounded by the #1518
    // live-tail gate, #1586, and the #1760 nudge cap. The FP-prone
    // api_error/overloaded/context HIGH_FP tokens STAY red-anchored — see
    // `codex_model_unsupported_red_anchor_fp_boundary_1634 (a non-net-error HIGH_FP still red-anchored)`.
    let mut t2 = StateTracker::new(Some(&Backend::Codex));
    t2.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t2.get_state(),
        AgentState::ServerRateLimit,
        "#1757: a net-error rendered DEFAULT must STILL latch ServerRateLimit \
         (exempt from the red anchor — real faults are not always rendered red)"
    );

    // Dropped token regression: `network error` (the unverified #1136 guess) is
    // gone — even text-only (fail-open) must NOT classify it as ServerRateLimit.
    let mut t3 = StateTracker::new(Some(&Backend::Codex));
    t3.feed("network error");
    assert_ne!(
        t3.get_state(),
        AgentState::ServerRateLimit,
        "#1587: bare `network error` was dropped — must no longer trigger ServerRateLimit"
    );
}

/// #1757 regression: the EXACT failure from the issue. codex hit a real
/// `API Error: InvalidHTTPResponse fetching "http://…"` that VTerm recorded as
/// `span_fg=[Default,…]` (the backend renders it default/grey, not red). Before
/// #1757 the #1450 red anchor suppressed the transition → no ServerRateLimit →
/// no auto-retry → the agent sat stuck (worst case: operator away). The net-error
/// exemption must let a DEFAULT-rendered net error latch ServerRateLimit so
/// auto-retry fires.
#[test]
fn net_error_invalidhttpresponse_default_latches_1757() {
    let screen = "API Error: InvalidHTTPResponse fetching \"http://127.0.0.1:3456/v1/messages\".";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    // All-DEFAULT fg — exactly what VTerm recorded for the real fault in the
    // issue's logs (`span_fg=[Default × 19]`).
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1757: a real InvalidHTTPResponse rendered DEFAULT must latch \
         ServerRateLimit (→ auto-retry), not be suppressed by the red anchor"
    );
}

/// #1768 regression (retry storm): a HIGH_FP ServerRateLimit error wins the
/// priority race over Thinking even after it scrolled UP and the agent RESUMED
/// WORK below it — so it kept re-latching, `clears_server_rate_limit_retry`
/// (Idle-only) never fired, and the supervisor re-injected `continue` into a
/// working agent. When a working-marker (codex Thinking `esc to interrupt`) is
/// rendered BELOW the error, the agent recovered → land on the working state.
#[test]
fn retry_storm_recovered_below_error_lands_on_working_state_1768() {
    // Error scrolled UP (line 1); the agent's most-recent line is the work spinner.
    let screen = "▌ API Error: InvalidHTTPResponse fetching \"http://127.0.0.1:3456\".\n\
                  ▌ retrying the request\n\
                  ▌ esc to interrupt";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    // #2086: GENUINE recovery is now PROVEN by recent productive output, not just
    // a working-marker spinner below the error (a stuck rate-limited retry shows a
    // spinner too — the #2086 incident). With productive output recorded,
    // `recovered_within` is true → the working_state_below override lands the
    // working state, preserving #1768's no-retry-storm behavior for real recovery.
    t.last_productive_output = Some(Instant::now());
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1768: a recovered agent (working-marker below + recent productive output) must \
         NOT re-latch ServerRateLimit (→ no retry storm)"
    );
    assert_eq!(
        t.get_state(),
        AgentState::Thinking,
        "#1768: it lands on the working state the marker indicates"
    );
}

/// #2086 companion to the #1768 test above: the SAME scrolled-up-error +
/// working-spinner-below layout, but with NO recent productive output, is a
/// STUCK retry (not recovery) → the SRL must stay latched. This is the
/// deliberate #2086 semantic change: a working spinner below an SRL error only
/// defeats it when PROVEN recovered (productive output), never on the spinner
/// alone (which a stuck rate-limited retry also renders).
#[test]
fn srl_with_working_spinner_below_but_no_productive_stays_latched_2086() {
    let screen = "▌ API Error: InvalidHTTPResponse fetching \"http://127.0.0.1:3456\".\n\
                  ▌ retrying the request\n\
                  ▌ esc to interrupt";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    // recovered_within=false (no productive output) → stuck retry, keep latched.
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#2086: working spinner below SRL with NO productive output is a stuck retry — \
         keep SRL latched so the supervisor's backoff retry fires"
    );
}

/// #1768 boundary: a fresh error rendered BELOW the working-marker is genuine →
/// must STILL latch ServerRateLimit (the fix must not mask a real fault).
#[test]
fn retry_storm_fresh_error_below_working_marker_still_latches_1768() {
    let screen = "▌ working on the task\n\
                  ▌ esc to interrupt\n\
                  ▌ API Error: InvalidHTTPResponse fetching \"http://127.0.0.1:3456\".";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1768: a fresh error BELOW the working-marker is genuine → must still latch \
         ServerRateLimit (auto-retry must fire)"
    );
}

/// #1768 boundary: the classic stuck case — an error with NO working-marker at
/// all must latch ServerRateLimit so auto-retry fires.
#[test]
fn retry_storm_error_with_no_working_marker_still_latches_1768() {
    let screen = "▌ API Error: InvalidHTTPResponse fetching \"http://127.0.0.1:3456\".";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1768: an error with no working-marker must latch ServerRateLimit"
    );
}

/// #1777 (cheerc, "Sticky UsageLimit"): UsageLimit (prio 11 > Thinking/ToolUse,
/// and never auto-expires) is the same sticky-error class as the #1768 retry
/// storm — after the agent resumed work below a scrolled-up UsageLimit line, the
/// status stayed stuck on `[UsageLimit]` until the line scrolled off. The #1768
/// working-marker override now covers UsageLimit too.
#[test]
fn usage_limit_recovered_below_error_lands_on_working_state_1777() {
    // UsageLimit line scrolled UP; the agent's most-recent line is the work spinner.
    let screen = "▌ You've hit your usage limit. try again at 3pm.\n\
                  ▌ resuming\n\
                  ▌ esc to interrupt";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_ne!(
        t.get_state(),
        AgentState::UsageLimit,
        "#1777: a recovered agent (working-marker below the scrolled-up UsageLimit) \
         must NOT re-latch UsageLimit"
    );
    assert_eq!(
        t.get_state(),
        AgentState::Thinking,
        "#1777: it lands on the working state the marker indicates"
    );
}

/// #1777: the override is recovery-only — a genuinely-stuck UsageLimit (no
/// in-flight working marker below it) must STILL latch so the operator is notified.
#[test]
fn usage_limit_stuck_no_working_marker_still_latches_1777() {
    let screen = "▌ You've hit your usage limit. try again at 3pm.\n\
                  ▌ waiting for the limit to reset";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_eq!(
        t.get_state(),
        AgentState::UsageLimit,
        "#1777: a UsageLimit with no working-marker below must still latch"
    );
}

/// #1768 (the idle-between-turns FP that REJECTED the first cut): a net-error
/// TOKEN merely MENTIONED in prose — an orchestrator discussing the bug, idle
/// between turns — rendered DEFAULT, with NO error-label on its line and NO
/// working-marker below, must NOT latch ServerRateLimit. The #1757 net-error
/// red-anchor exemption is narrowed to real error-LINES (#1768), so a bare prose
/// mention stays red-anchored → default-rendered → suppressed. (Contrast
/// `net_error_invalidhttpresponse_default_latches_1757`: a real `API Error:`
/// line IS error-line-shaped → still latches.)
#[test]
fn net_error_prose_mention_does_not_latch_1768() {
    let screen = "we keep hitting the InvalidHTTPResponse problem when the proxy is flaky";
    let n = screen.chars().count();
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed_with_fg(screen, &vec![CellFg::Default; n]);
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1768: a net-error token in prose (no error-label, default-rendered, no \
         working-marker below) must NOT latch ServerRateLimit (idle-between-turns FP)"
    );
}

/// `t-impl-errorline-regex-extend`: the extended error-line-shape regex (now the
/// generic `in_error_line`) must MATCH the corpus's real backend error lines —
/// including the ones the old `\b(api ?|fetch)?error[:?]` MISSED: ModelUnsupported
/// `invalid_request_error:` (the `_error` underscore killed `\berror`), gemini
/// bare-status (`429` / `got status:` / `RESOURCE_EXHAUSTED`), and structured-JSON
/// error fields — while still REJECTING bare prose / source mentions.
#[test]
fn in_error_line_matches_corpus_errors_rejects_prose_step1() {
    use crate::state::patterns::in_error_line_excluding_input;

    // (line, a token on that line) — must read as a real backend error line.
    let real: &[(&str, &str)] = &[
        (
            "API Error: InvalidHTTPResponse fetching \"http://x\"",
            "InvalidHTTPResponse",
        ),
        (
            "FetchError: request failed, reason: ECONNRESET",
            "ECONNRESET",
        ),
        // #corpus bug: the `_error` underscore killed the old `\berror` boundary.
        (
            "invalid_request_error: model is not supported",
            "invalid_request_error",
        ),
        (
            "✕ API Error: got status: 429 RESOURCE_EXHAUSTED",
            "RESOURCE_EXHAUSTED",
        ),
        ("429 Too Many Requests", "429"),
        ("got status: 429", "429"),
        ("{\"reason\": \"rateLimitExceeded\"}", "rateLimitExceeded"),
        ("{\"type\": \"error\", \"message\": \"x\"}", "error"),
        ("Error: token limit exceeded", "token limit"),
        // t-coloranchor-corpus-gate: kiro RateLimit — the one color-gated content
        // FN the rg-on-fixture check found. Needs the new `\w*exception` label
        // (`Exception` has no `error` substring; the line carries no 429/JSON).
        (
            "ThrottlingException: Rate exceeded for this AWS account",
            "ThrottlingException",
        ),
        // codex RateLimit — already covered by the existing `\w*error:` via the
        // verified `stream error:` wrapper (cf. codex-model-unsupported fixture)
        // and by the `RateLimitError:` token; pinned here to lock that coverage.
        (
            "stream error: rate_limit_exceeded: retry after 60s",
            "rate_limit_exceeded",
        ),
        ("RateLimitError: 429 Too Many Requests", "RateLimitError"),
    ];
    for (line, tok) in real {
        assert!(
            in_error_line_excluding_input(line, tok, &[]),
            "must read as an error line: {line:?} (token {tok:?})"
        );
    }

    // Bare prose / source mentions of the same tokens — must NOT read as an error line.
    let prose: &[(&str, &str)] = &[
        (
            "we keep hitting the InvalidHTTPResponse problem",
            "InvalidHTTPResponse",
        ),
        ("the ECONNRESET error happened twice today", "ECONNRESET"),
        (
            "pub(crate) const SERVER_RATE_LIMIT_NET_ERRORS: &str = r\"ECONNRESET|...\"",
            "ECONNRESET",
        ),
        ("see issue 4290 for details", "4290"),
        // t-coloranchor-corpus-gate: the new `\w*exception` label requires a
        // trailing `:`/`?` — a bare prose mention with no label stays prose, and
        // the bare `Rate exceeded` / `rate_limit_exceeded` phrases are NOT in the
        // regex (prose-ambiguous, #848/#854 class).
        (
            "we keep hitting a ThrottlingException in prod lately",
            "ThrottlingException",
        ),
        (
            "discussing the rate_limit_exceeded code path in state.rs",
            "rate_limit_exceeded",
        ),
    ];
    for (line, tok) in prose {
        assert!(
            !in_error_line_excluding_input(line, tok, &[]),
            "prose/source mention must NOT read as an error line: {line:?} (token {tok:?})"
        );
    }
}

// ── #1527: transition recording at the mutation source ──

#[test]
fn record_set_buffers_transitions_fifo_then_drains() {
    let mut st = StateTracker::new(None); // starts Ready
    st.record_set(AgentState::Thinking);
    st.record_set(AgentState::Idle);
    let (recs, dropped) = st.drain_pending_transitions();
    assert_eq!(dropped, 0);
    assert_eq!(recs.len(), 2);
    assert_eq!(
        (recs[0].from, recs[0].to),
        (AgentState::Idle, AgentState::Thinking)
    );
    assert_eq!(
        (recs[1].from, recs[1].to),
        (AgentState::Thinking, AgentState::Idle)
    );
    assert!(!recs[0].ts.is_empty(), "ts must be captured at record time");
    assert!(
        st.drain_pending_transitions().0.is_empty(),
        "second drain is empty"
    );
}

/// The #1527 regression: a mutation that bypasses BOTH `transition()` and
/// `tick()` (here `set_restarting`) is still recorded — pre-fix the
/// supervisor's prev/new-at-tick comparison could miss it entirely because
/// the change completed async in the read-loop / reaper thread.
#[test]
fn set_restarting_records_transition_without_tick() {
    let mut st = StateTracker::new(None); // Ready
    st.set_restarting();
    let (recs, _) = st.drain_pending_transitions();
    assert_eq!(
        recs.len(),
        1,
        "set_restarting must record (it bypasses transition())"
    );
    assert_eq!(recs[0].to, AgentState::Restarting);
}

#[test]
fn set_awaiting_operator_records_transition() {
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode)); // starts Starting
    st.set_awaiting_operator();
    let (recs, _) = st.drain_pending_transitions();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].from, AgentState::Starting);
    assert_eq!(recs[0].to, AgentState::AwaitingOperator);
}

#[test]
fn same_state_record_set_is_noop() {
    let mut st = StateTracker::new(None); // Ready
    st.record_set(AgentState::Idle); // same → no record
    assert!(
        st.drain_pending_transitions().0.is_empty(),
        "same-state record_set must not buffer a spurious transition"
    );
}

#[test]
fn pending_transitions_bounded_drops_oldest() {
    let mut st = StateTracker::new(None); // Ready
    let overflow = 5usize;
    for i in 0..(StateTracker::PENDING_TRANSITIONS_CAP + overflow) {
        let s = if i % 2 == 0 {
            AgentState::Thinking
        } else {
            AgentState::Idle
        };
        st.record_set(s);
    }
    let (recs, dropped) = st.drain_pending_transitions();
    assert_eq!(
        recs.len(),
        StateTracker::PENDING_TRANSITIONS_CAP,
        "buffer must be capped"
    );
    assert_eq!(
        dropped as usize, overflow,
        "overflow count surfaced for the warn"
    );
}

/// #1523 phase-1 — end-to-end regression net for the #1527 wiring, ERROR case.
/// A real error-triggering screen fed to the read-loop classifier must flow the
/// whole pipeline: `feed` → `detect` → `transition()` (the instant error branch,
/// no hysteresis) → `record_set` (source capture) → `drain_pending_transitions`
/// → `log_state_transition_at` → `state-transitions.jsonl`. Pre-#1527 the
/// supervisor's prev/new-at-tick comparison silently dropped feed-driven error
/// transitions (they complete async between two ticks, so prev==new at tick).
/// The two existing tests cover the ends in isolation —
/// `record_set_buffers_transitions_fifo_then_drains` is generic (Thinking/Idle)
/// and `log_state_transition_creates_file` calls `log_state_transition_at`
/// directly without routing through `record_set` — but neither pins an error
/// state traversing the full seam, which is exactly what #1527 fixed.
#[test]
#[allow(clippy::unwrap_used)]
fn error_transition_flows_end_to_end_record_set_drain_to_jsonl() {
    use crate::daemon::usage_limit::log_state_transition_at;

    // Read-loop entry: a canonical Anthropic 429 rejection drives RateLimit, an
    // error state (instant transition, no hysteresis). `tracker_at` sets
    // `current` directly (bypassing record_set), so the buffer starts empty and
    // the feed produces exactly one source-captured transition.
    let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    t.feed("API Error: Request rejected (429) · this may be a temporary capacity issue");
    assert_eq!(t.get_state(), AgentState::RateLimit);
    assert!(
        t.get_state().is_error(),
        "guard: the driven state must be an error state for this e2e to be meaningful"
    );

    // record_set captured the transition at its source (the #1527 funnel).
    let (recs, dropped) = t.drain_pending_transitions();
    assert_eq!(dropped, 0);
    let err = recs
        .iter()
        .find(|r| r.to == AgentState::RateLimit)
        .expect("the feed-driven error transition must be captured at source by record_set");
    assert_eq!(
        err.from,
        AgentState::Idle,
        "from must be the pre-error state"
    );
    assert!(
        !err.ts.is_empty(),
        "ts captured at record time, not drain time"
    );

    // Replay the supervisor's lock-free drain → file-append step into a temp
    // home (unique dir → no cross-test pollution of the shared jsonl path).
    let dir = std::env::temp_dir().join("agend-test-error-transition-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::remove_file(dir.join("state-transitions.jsonl")).ok();
    log_state_transition_at(
        &dir,
        "dev",
        err.from,
        err.to,
        &err.ts,
        "API Error: Request rejected (429)",
    );

    let content = std::fs::read_to_string(dir.join("state-transitions.jsonl")).unwrap();
    assert!(
        content.contains("\"to\":\"rate_limit\""),
        "error state must land in state-transitions.jsonl: {content}"
    );
    assert!(
        content.contains("\"from\":\"idle\""),
        "from recorded: {content}"
    );
    assert!(
        content.contains("\"agent\":\"dev\""),
        "agent recorded: {content}"
    );
    assert!(
        content.contains(&format!("\"ts\":\"{}\"", err.ts)),
        "must persist the record-time ts (not drain time): {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── #1808-probe0-phantom: SRL re-match signature freshness primitive ──

/// `srl_match_signature` must be STABLE for the same error line at the same
/// distance-from-bottom (the clock-tick re-render that flips the screen hash but
/// does NOT move the error), and CHANGE when the error scrolls up because fresh
/// output rendered below it. This is the freshness primitive the phantom probe
/// compares across ticks to tell an in-place re-scan from real progress.
#[test]
fn srl_match_signature_stable_until_error_moves_1808() {
    use super::srl_match_signature;
    let err = "Server is temporarily limiting requests";
    // Same error at the bottom; only a benign top line differs (screen hash flips,
    // error unmoved) → identical signature. The two top lines are equal length so
    // the error's line_start (and thus dist_from_bottom) is unchanged.
    let a = format!("clock 12:00:01\n{err}\n");
    let b = format!("clock 12:00:02\n{err}\n");
    assert_eq!(
        srl_match_signature(&a, err),
        srl_match_signature(&b, err),
        "same error line at the same distance-from-bottom must sign identically \
         (the in-place clock-tick re-render that triggers the phantom re-scan)"
    );
    // Error pushed UP by fresh output below it → different dist_from_bottom.
    let c = format!("clock 12:00:03\n{err}\nthe agent resumed work\n");
    assert_ne!(
        srl_match_signature(&a, err).1,
        srl_match_signature(&c, err).1,
        "an error pushed up by new output below must change dist_from_bottom"
    );
}

// ── #1518: HIGH_FP error detection bounded to the live bottom-N tail ──

#[test]
fn matched_span_in_recent_tail_unit_1518() {
    use super::{matched_span_in_recent_tail, ERROR_TAIL_SCAN_LINES};
    let mut screen = String::from("ERR: boom\n");
    for i in 0..(ERROR_TAIL_SCAN_LINES + 3) {
        screen.push_str(&format!("line {i}\n"));
    }
    // The error is now above the last ERROR_TAIL_SCAN_LINES rows.
    assert!(
        !matched_span_in_recent_tail(&screen, "ERR: boom", ERROR_TAIL_SCAN_LINES),
        "error scrolled above the tail → not in recent tail"
    );
    // A marker within the last N rows IS in the tail.
    assert!(
        matched_span_in_recent_tail(&screen, "line 7", ERROR_TAIL_SCAN_LINES),
        "a marker in the last N rows is in the recent tail"
    );
    assert!(
        !matched_span_in_recent_tail(&screen, "", ERROR_TAIL_SCAN_LINES),
        "empty match is never in tail"
    );
}

#[test]
fn high_fp_error_only_fires_within_live_tail_1518() {
    // #1518: a HIGH_FP ServerRateLimit line visible in the live bottom rows fires
    // the error state; the SAME line scrolled above the live tail (pushed up by
    // the agent's post-recovery output) must NOT — that level-triggered re-match
    // was the retry-storm root. Text-path (`feed`) so the #1450 red anchor
    // fail-opens (no fg mask) and we test the POSITION gate in isolation.
    // RateLimit is HIGH_FP; the string is the canonical ClaudeCode trigger.
    let err = "API Error: Request rejected (429) · this may be a temporary capacity issue";

    // A: error in the live tail → RateLimit fires.
    let mut a = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    a.feed(err);
    assert_eq!(
        a.get_state(),
        AgentState::RateLimit,
        "error visible in the live tail must fire the HIGH_FP error state"
    );

    // B: SAME error scrolled above the bottom-N (ERROR_TAIL_SCAN_LINES rows of
    // subsequent output below it) → suppressed (must NOT keep firing the error
    // state). 20 lines clears the N=15 bound with margin.
    let mut b = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    let mut screen = format!("{err}\n");
    for i in 0..20 {
        screen.push_str(&format!("subsequent output line {i}\n"));
    }
    b.feed(&screen);
    assert_ne!(
        b.get_state(),
        AgentState::RateLimit,
        "#1518: error scrolled above the live tail must NOT keep firing the error state"
    );
}

#[test]
fn modal_prompt_above_live_tail_still_detected_1518() {
    // reviewer-2 refinement: modal / interactive prompts are NOT HIGH_FP, so they
    // keep FULL-screen scanning — a permission prompt that sits above the live
    // streaming tail must still be detected (the bottom-N bound is error-only).
    // (gate_on_heartbeat may relabel a fresh PermissionPrompt as Thinking; either
    // way it is DETECTED, not suppressed/missed — the point of this test.)
    // 20 streaming lines push the prompt above the bottom-N=15 error bound; a
    // HIGH_FP marker this deep WOULD be suppressed, so detection here proves the
    // bound is error-only.
    // #1546: trigger via the chrome footer (the new zero-FP anchor), not the old
    // bare "Do you want to …" string (cut as a false-positive source).
    let mut st = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
    let mut screen = String::from("Esc to cancel · Tab to amend\n");
    for i in 0..20 {
        screen.push_str(&format!("streaming output {i}\n"));
    }
    st.feed(&screen);
    assert!(
        matches!(
            st.get_state(),
            AgentState::PermissionPrompt | AgentState::Thinking
        ),
        "#1518: a modal above the live tail must still be detected (full-screen, not bottom-N bounded), got {:?}",
        st.get_state()
    );
}

// ── #1541: verb-agnostic Claude thinking spinner anchor ─────────────────

#[test]
fn claude_thinking_anchor_is_verb_agnostic_1541() {
    // The crux of #1541: spinner verbs roll randomly and were NOT in the old
    // whitelist (Whisking / Julienning / Burrowing / Lollygagging are all new;
    // `Churned` proves a past-tense verb in an ACTIVE spinner still counts).
    // Each frame must fire Thinking so a heads-down agent never reads `idle`.
    for frame in [
        "✻ Whisking… (5s)",                         // sparkle glyph + elapsed tail
        "✶ Churned… (12s · ↑ 2.1k tokens)",         // active past-tense verb + token counter
        "· Julienning… (16m · thinking)",           // `·` glyph + minutes elapsed
        "✳ Burrowing… (thinking with high effort)", // sparkle + non-elapsed tail
        "Lollygagging…(running stop hook)",         // no glyph — Branch B `(running` tail
    ] {
        let mut st = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        st.feed(frame);
        assert_eq!(
            st.get_state(),
            AgentState::Thinking,
            "#1541: unlisted-verb spinner {frame:?} must fire Thinking"
        );
    }
}

#[test]
fn claude_thinking_anchor_rejects_prose_and_completion_1541() {
    // False-positive guards the verb whitelist used to provide for free.
    for frame in [
        "Thinking...(7s)",          // prose: ASCII `...`, NOT U+2026
        "Churned for 7m39s",        // completion: past-tense `for Xm Ys`, no `…`
        "Let me think about this…", // prose: U+2026 but no glyph and no `(tail`
    ] {
        let mut st = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        st.feed(frame);
        assert_ne!(
            st.get_state(),
            AgentState::Thinking,
            "#1541: {frame:?} must NOT be misread as Thinking"
        );
    }
}

#[test]
fn claude_thinking_anchor_does_not_cross_backends_1541() {
    // The sparkle anchor is Claude-scoped; a Claude spinner frame fed to other
    // backends' pattern sets must not yield Thinking (each owns its own arm).
    let claude_frame = "✻ Whisking… (5s)";
    for backend in [Backend::Codex, Backend::Agy, Backend::KiroCli] {
        let detected = StatePatterns::for_backend(&backend).detect(claude_frame);
        assert_ne!(
            detected,
            Some(AgentState::Thinking),
            "#1541: claude sparkle anchor must not fire Thinking on {backend:?}"
        );
    }
}

// ── #1546: PermissionPrompt chrome-footer anchor (cut speculative bare strings) ──

#[test]
fn claude_permission_footer_anchor_1546() {
    let p = StatePatterns::for_backend(&Backend::ClaudeCode);
    // FN-preserved: the self-identifying chrome footer + the fully-specific
    // allow-all-edits phrase still fire PermissionPrompt.
    assert_eq!(
        p.detect("Esc to cancel · Tab to amend"),
        Some(AgentState::PermissionPrompt),
        "footer chrome must fire PermissionPrompt"
    );
    assert_eq!(
        p.detect("  2. Yes, allow all edits during this session (shift+tab)"),
        Some(AgentState::PermissionPrompt),
        "allow-all-edits phrase must fire PermissionPrompt"
    );
    // #1546 trust follow-up: the trust-folder dialog uses a DIFFERENT footer
    // (`Enter to confirm · Esc to cancel`) — its own chrome anchor.
    assert_eq!(
        p.detect("Enter to confirm · Esc to cancel"),
        Some(AgentState::PermissionPrompt),
        "trust-folder footer must fire PermissionPrompt"
    );
    // FP-safe: a partial fragment that is NOT the full footer chrome must not fire.
    assert_ne!(
        p.detect("Press Enter to confirm your email address"),
        Some(AgentState::PermissionPrompt),
        "#1546: bare 'Enter to confirm' prose must NOT false-fire PermissionPrompt"
    );
    // FP-fixed: the cut bare strings must NOT fire on prose / pasted content
    // (this is the member-state + dispatch-idle bleed #1546 stops).
    for prose in [
        "Do you want to proceed with this edit?", // the #1518 modal fixture content
        "I'll approve the PR once CI is green",   // 'approve' in prose
        "Allow once you've reviewed it, then merge", // 'Allow once' in prose
        "Should I Allow always, or just this time?", // 'Allow always' in prose
    ] {
        assert_ne!(
            p.detect(prose),
            Some(AgentState::PermissionPrompt),
            "#1546: prose {prose:?} must NOT false-fire PermissionPrompt"
        );
    }
    // The footer anchor is Claude-only — other backends don't borrow it.
    for b in [Backend::Codex, Backend::Agy, Backend::KiroCli] {
        assert_ne!(
            StatePatterns::for_backend(&b).detect("Esc to cancel · Tab to amend"),
            Some(AgentState::PermissionPrompt),
            "claude footer must not fire PermissionPrompt on {b:?}"
        );
    }
}

/// #1559: cross-backend permission patterns are chrome-anchored — the real
/// dialog chrome fires PermissionPrompt, but the dropped FP-prone bare option
/// words do NOT fire on prose. Pairs with the replay fixtures (which prove the
/// chrome fires on the real captures); this pins the FP direction.
#[test]
fn cross_backend_permission_chrome_anchor_1559() {
    // kiro: chrome header + footer fire; the old [docs] guess (a false negative)
    // and any bare prose do not.
    let kiro = StatePatterns::for_backend(&Backend::KiroCli);
    assert_eq!(
        kiro.detect("rm -rf / requires approval"),
        Some(AgentState::PermissionPrompt),
        "kiro header 'requires approval' must fire"
    );
    assert_eq!(
        kiro.detect("ESC to close | Tab to edit"),
        Some(AgentState::PermissionPrompt),
        "kiro footer chrome must fire"
    );
    // the spinner footer (lowercase 'esc to cancel') must NOT be read as the
    // permission footer.
    assert_ne!(
        kiro.detect("Thinking… (esc to cancel)"),
        Some(AgentState::PermissionPrompt),
        "kiro spinner footer must not fire PermissionPrompt"
    );

    // opencode: the option-row co-occurrence fires; single bare option words in
    // prose (changelog / release-notes echo) do not.
    let opencode = StatePatterns::for_backend(&Backend::OpenCode);
    assert_eq!(
        opencode.detect("┃ Allow once   Allow always   Reject"),
        Some(AgentState::PermissionPrompt),
        "opencode option-row chrome must fire"
    );
    for prose in [
        "Allow once you've reviewed it, then merge",
        "Should I Allow always, or just this time?",
    ] {
        assert_ne!(
            opencode.detect(prose),
            Some(AgentState::PermissionPrompt),
            "#1559: bare opencode option word in prose {prose:?} must NOT fire"
        );
    }
}

/// #1450 regression: a HIGH_FP pattern that matches but never renders red —
/// e.g. an opencode pane statically displaying the source identifier
/// `ContextOverflow` — must log the red-anchor suppression ONCE, not on every
/// render tick. The gate is level-triggered (`feed_with_fg` re-runs detection
/// each tick), so before the dedup this flooded the daemon log with 14k+
/// identical WARN lines per incident — the symptom that buried the real signal
/// during a freeze investigation. A changing line elsewhere on screen (which
/// defeats `feed()`'s screen-hash dedup) must NOT re-open the floodgate, since
/// the suppressed (state, matched, line) tuple is unchanged.
#[test]
#[tracing_test::traced_test]
fn anchor_suppress_warn_deduped_across_ticks() {
    let mut vt = VTerm::new(80, 24);
    let mut st = StateTracker::new(Some(&Backend::OpenCode));
    // Five renders: each scrolls a fresh counter line (new screen hash → passes
    // feed()'s screen-hash dedup) while the non-red `ContextOverflow` line stays
    // in the live tail, so the HIGH_FP match + red-anchor-fail fires every tick.
    for i in 0..5 {
        drive(
            &mut vt,
            &mut st,
            format!("working tick {i}\r\nContextOverflow\r\n").as_bytes(),
        );
    }
    logs_assert(|lines: &[&str]| {
        let hits = lines.iter().filter(|l| l.contains("#1450")).count();
        if hits == 1 {
            Ok(())
        } else {
            Err(format!(
                "#1450 red-anchor suppress WARN must be deduped to exactly one \
                 across 5 identical-suppression ticks, got {hits}"
            ))
        }
    });
}

// ── #1562 self-capture instrument ───────────────────────────────────
//
// Pure-additive diagnostic: when a known server-throttle phrase is on
// screen but the classifier did NOT land on a retryable state, side-log
// the (ANSI-colored) tail. These pin the three behaviors the lead spec
// names — phrase + non-retryable → logged; classified ServerRateLimit →
// skipped (no noise); no phrase → skipped — plus the scrollback gate and
// the color reconstruction (the color-anchor diagnostic point).

const THROTTLE_SCREEN: &str = "\
some preamble line\n\
Server is temporarily limiting requests (not your usage limit)\n\
the agent kept going after the throttle\n";

#[test]
fn unclassified_throttle_logged_on_phrase_plus_nonretryable_state() {
    // Throttle phrase present, classifier landed on Ready (the in-the-wild
    // miss — e.g. anchor suppressed it / wording drifted) → capture.
    let captured = unclassified_throttle_tail(AgentState::Idle, THROTTLE_SCREEN, &[]);
    let (tail, wrap_split) =
        captured.expect("throttle phrase + non-retryable state must be captured");
    assert!(
        tail.contains("temporarily limiting requests"),
        "captured tail must carry the throttle line, got: {tail:?}"
    );
    assert!(
        !wrap_split,
        "a contiguous (un-wrapped) phrase must NOT be flagged wrap_split"
    );
}

/// #1808: the instrument was BLIND to a LINE-WRAPPED throttle phrase because it
/// only did a contiguous `str::contains` — the exact reason the live narrow-pane
/// SRL miss captured nothing. A phrase split by hard `\n` (Ink-style layout) must
/// now be captured AND flagged `wrap_split` (the Phase 2 soft-vs-hard signal).
#[test]
fn unclassified_throttle_captures_hard_wrapped_phrase_with_wrap_split_flag() {
    // Phrase split across rows by `\n` the way an app word-wrap emits it — NOT
    // contiguous, so the old `contains` check missed it entirely.
    let screen = "⏺ API Error:\ntemporarily limiting\nrequests (not your\nusage limit)\n";
    let captured = unclassified_throttle_tail(AgentState::Idle, screen, &[]);
    let (_tail, wrap_split) =
        captured.expect("a hard-`\\n`-wrapped throttle phrase must still be captured (#1808)");
    assert!(
        wrap_split,
        "a phrase found only after whitespace-flatten must be flagged wrap_split (hard-wrap signal)"
    );
}

#[test]
fn unclassified_throttle_skipped_when_classified_serverratelimit() {
    // Classifier correctly recognized the throttle (auto-retry handles it)
    // → nothing to diagnose → no side-log (keeps the instrument low-noise).
    for state in [
        AgentState::ServerRateLimit,
        AgentState::RateLimit,
        AgentState::ApiError,
        AgentState::ContextFull,
    ] {
        assert!(
            unclassified_throttle_tail(state, THROTTLE_SCREEN, &[]).is_none(),
            "classified retryable state {state:?} must NOT be side-logged"
        );
    }
}

#[test]
fn unclassified_throttle_skipped_without_phrase() {
    // No known throttle phrase on screen → never logged, regardless of state.
    let screen = "claude ready\n❯ awaiting input\n";
    assert!(unclassified_throttle_tail(AgentState::Idle, screen, &[]).is_none());
    assert!(unclassified_throttle_tail(AgentState::Thinking, screen, &[]).is_none());
}

#[test]
fn unclassified_throttle_skipped_when_phrase_only_in_scrollback() {
    // Throttle phrase scrolled up past the live bottom-N tail (the agent has
    // moved on) → stale, not a current miss → not logged.
    let mut screen =
        String::from("Server is temporarily limiting requests (not your usage limit)\n");
    for i in 0..(UNCLASSIFIED_TAIL_LINES + 5) {
        screen.push_str(&format!("post-recovery output line {i}\n"));
    }
    assert!(
        unclassified_throttle_tail(AgentState::Idle, &screen, &[]).is_none(),
        "a throttle phrase only surviving in scrollback must NOT be logged"
    );
}

#[test]
fn ansi_colored_tail_marks_red_cells_and_aligns_past_newline() {
    // fg is 1:1 with screen_text.chars() INCLUDING the '\n' separator (vterm
    // emits a Default entry for it). Verify red cells reconstruct \x1b[31m and
    // that alignment survives a newline.
    // chars:  o k \n b a d   (indices 0..=5)
    let screen = "ok\nbad";
    let fg = vec![
        CellFg::Default, // o
        CellFg::Default, // k
        CellFg::Default, // \n
        CellFg::Red,     // b
        CellFg::Red,     // a
        CellFg::Default, // d
    ];
    let out = ansi_colored_tail(screen, &fg, 5);
    assert!(
        out.contains("\x1b[31mba"),
        "red cells must emit SGR 31: {out:?}"
    );
    assert!(
        out.contains("ok"),
        "earlier (non-red) line must still render: {out:?}"
    );
    // Empty fg (text-only callers): tail captured without color, no panic.
    let plain = ansi_colored_tail(screen, &[], 5);
    assert!(plain.contains("bad") && !plain.contains("\x1b[31m"));
}

#[test]
fn append_jsonl_appends_one_record_per_line() {
    let dir = std::env::temp_dir().join("agend_1562_append_jsonl_test");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nested").join("unclassified_errors.jsonl");

    append_jsonl(&path, &serde_json::json!({"backend": "codex", "n": 1})).expect("first append");
    append_jsonl(&path, &serde_json::json!({"backend": "claude", "n": 2})).expect("second append");

    let body = std::fs::read_to_string(&path).expect("read back jsonl");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "one record per line");
    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("parse line 0");
    let second: serde_json::Value = serde_json::from_str(lines[1]).expect("parse line 1");
    assert_eq!(first["backend"], "codex");
    assert_eq!(second["n"], 2);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── #SRL-phase2: Ink hard-wrap + hash-dedup blind-spot ───────────────────────

/// #SRL-phase2 (a): a narrow-pane Ink HARD-wrapped ServerRateLimit line (the
/// phrase split across rows by real `\n`, not an alacritty soft-wrap) must latch
/// SRL via the flattened-tail fallback. The raw single-line regex misses it
/// (Phase 1 fails); the flattened bottom-N tail matches + `in_error_line` holds
/// ("API Error:" on the rejoined line).
#[test]
fn hardwrap_srl_latches_via_flattened_tail_phase2() {
    let (mut vt, mut st) = claude_tracker();
    // Each word-wrapped row separated by a REAL CRLF (hard wrap).
    let hw =
        "API Error: Server is\r\ntemporarily limiting\r\nrequests (not your\r\nusage limit)\r\n";
    // Sanity: the raw (un-flattened) screen does NOT match the single-line SRL
    // regex — this is exactly the Phase-1 miss.
    assert!(
        patterns::StatePatterns::for_backend(&Backend::ClaudeCode)
            .detect_with_match(
                "API Error: Server is\ntemporarily limiting\nrequests (not your\nusage limit)"
            )
            .is_none(),
        "precondition: hard-wrapped SRL must NOT match the raw single-line regex"
    );
    drive(&mut vt, &mut st, hw.as_bytes());
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#SRL-phase2: hard-wrapped SRL must latch via the flattened-tail fallback"
    );
}

/// #SRL-phase2 (b): a SETTLED (static, unchanged-hash) SRL pane must be
/// re-detected despite the `feed_with_fg` hash-dedup early-return — otherwise a
/// stuck SRL pane that was cleared (spuriously) never re-latches and never
/// recovers. Simulate the clear-while-static blind spot: latch, clear `current`
/// without changing the screen, feed the IDENTICAL frame → must re-latch.
#[test]
fn static_srl_pane_redetected_after_dedup_blindspot_phase2() {
    let (mut vt, mut st) = claude_tracker();
    let hw =
        "API Error: Server is\r\ntemporarily limiting\r\nrequests (not your\r\nusage limit)\r\n";
    drive(&mut vt, &mut st, hw.as_bytes());
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "first feed latches"
    );
    // Spurious clear while the pane stays STATIC (same frame ⇒ same hash). The
    // blind spot: the next identical feed would dedup-skip and never re-latch.
    st.current = AgentState::Idle;
    drive(&mut vt, &mut st, hw.as_bytes());
    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#SRL-phase2: a static SRL pane must re-detect across the hash-dedup blind spot"
    );
}

/// #SRL-phase2 (c) prose-FP: multi-line prose that mentions the SRL phrase but is
/// NOT an error render (no error indicator on the flattened line) must NOT latch.
/// The `in_error_line` guard on the flattened text rejects it.
#[test]
fn multiline_prose_mentioning_srl_does_not_latch_phase2() {
    let (mut vt, mut st) = claude_tracker();
    // Contains the exact SRL phrase (so detect_with_match matches the flattened
    // tail) but no error indicator (no "API Error:"/"429"/…) → in_error_line
    // rejects → no latch.
    let prose = "Docs note: Server is\r\ntemporarily limiting\r\nrequests is the SRL\r\nbanner text, FYI.\r\n";
    drive(&mut vt, &mut st, prose.as_bytes());
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#SRL-phase2: multi-line prose mentioning the SRL phrase (no error indicator) must NOT latch"
    );
}

/// #SRL-phase2 (c') reviewer-2 #1857 regression: prose that mentions the SRL
/// phrase AND has an UNRELATED error indicator on a DIFFERENT (distant) row must
/// NOT latch. The old `in_error_line(flat, …)` degenerated to "indicator anywhere
/// in the tail" (flat has no `\n`) → false latch; the proximity-scoped check
/// rejects the distant indicator.
#[test]
fn prose_with_distant_unrelated_error_indicator_does_not_latch_phase2() {
    let (mut vt, mut st) = claude_tracker();
    // Row A: an unrelated `TypeError:` (an in_error_line indicator, but NOT an
    // SRL/RateLimit pattern). Rows of filler push it well past the proximity
    // window. Last row: the SRL throttle phrase as prose (no adjacent indicator).
    let frame = "TypeError: cannot read property foo of undefined here\r\n\
                 filler alpha beta gamma delta epsilon\r\n\
                 filler zeta eta theta iota kappa\r\n\
                 more filler lambda mu nu xi omicron\r\n\
                 Server is temporarily limiting requests is just the banner text FYI\r\n";
    drive(&mut vt, &mut st, frame.as_bytes());
    assert_ne!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#SRL-phase2 #1857: SRL prose + a DISTANT unrelated error indicator must NOT latch"
    );
}

// ── Context% telemetry (operator-directed context-usage detection) ──────────

/// Live-capture fixture: the fleet Claude statusline as rendered 2026-06-10
/// (`pane_snapshot` of a real pane). The percent is fractional.
const CLAUDE_STATUSLINE_FRAME: &str = "⏺ working on the thing\n\
     some conversation text\n\
     ❯\n\
     ────────────\n\
       Model: Fable 5 | Ctx Used: 61.0% | ⎇ fix/879-v4-daemon-reorder | (+0,-0)\n\
       ⏵⏵ bypass permissions on (shift+tab to cycle)";

#[test]
fn claude_context_pct_extracted_from_statusline() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_STATUSLINE_FRAME);
    let (pct, source) = t.resolved_context().expect("statusline percent extracted");
    assert!((pct - 61.0).abs() < f32::EPSILON);
    assert_eq!(source, "pattern");
}

/// A stale frame can leave an OLD statusline copy above the live one (seen in
/// the live capture) — the LAST matching row must win.
#[test]
fn claude_context_pct_duplicate_statusline_last_wins() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "  Model: Fable 5 | Ctx Used: 55.0% | ⎇ b | (+0,-0)\n\
         ──⏵⏵─bypass─permissions─on──────\n\
         ❯\n\
         ────────────\n\
           Model: Fable 5 | Ctx Used: 61.0% | ⎇ b | (+0,-0)\n\
           ⏵⏵ bypass permissions on (shift+tab to cycle)",
    );
    let (pct, _) = t.resolved_context().expect("reading present");
    assert!(
        (pct - 61.0).abs() < f32::EPSILON,
        "last (live) statusline wins, got {pct}"
    );
}

/// Live-capture fixture: the kiro footer (`Kiro · auto · ◔ 10%`) sits a few
/// rows above the bottom (input hint + key help below it).
#[test]
fn kiro_context_pct_extracted_from_footer() {
    let mut t = StateTracker::new(Some(&Backend::KiroCli));
    t.feed(
        "conversation text above\n\
         ────────────\n\
         Kiro · auto · ◔ 10%                    ~/.agend-terminal/workspace/kiro\n\
         \n\
          ask a question or describe a task ↵\n\
         \n\
                                       /copy to clipboard",
    );
    let (pct, source) = t.resolved_context().expect("footer percent extracted");
    assert!((pct - 10.0).abs() < f32::EPSILON);
    assert_eq!(source, "pattern");
}

/// Prose-FP guard: agents routinely DISCUSS context% in conversation text.
/// A mention ABOVE the bottom status rows must not produce a reading, and a
/// bare "NN% context" phrase (no statusline form) must not match at all.
#[test]
fn context_pct_prose_mention_does_not_match() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "⏺ 報告:lead 的 pane 顯示 Ctx Used: 95.0% 需要重啟\n\
         filler line one\n\
         filler line two\n\
         filler line three\n\
         filler line four\n\
         filler line five\n\
         filler line six\n\
         ❯ discussing that we are at 80% context now\n\
         ────────────\n\
         (no statusline rendered — narrow pane)",
    );
    assert!(
        t.resolved_context().is_none(),
        "prose mentions (above the status rows / non-statusline form) must not read"
    );
}

/// A pane that stops rendering the statusline (narrow-pane truncation) keeps
/// the previous reading — "can't read" must not erase what we knew.
#[test]
fn context_pct_truncated_statusline_keeps_previous_reading() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_STATUSLINE_FRAME);
    t.feed("totally different screen\nwith no statusline at all\n❯");
    let (pct, _) = t.resolved_context().expect("previous reading retained");
    assert!((pct - 61.0).abs() < f32::EPSILON);
}

/// #1945-disable: context resolution is PATTERN ONLY — the transcript
/// estimate is disabled (its first live minute fired a triple false 100%
/// alert), so an unreadable statusline is honestly unknown (no alert) and a
/// readable one reports source "pattern". The estimate plumbing
/// (`set_context_estimate` / the "transcript" source) is REMOVED from the
/// tracker — this test pins that the disabled path stays disabled.
#[test]
fn context_resolution_is_pattern_only_estimate_disabled_1945() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    assert!(
        t.resolved_context().is_none(),
        "#1945: no pattern reading → honestly unknown (never an estimate)"
    );
    t.feed(CLAUDE_STATUSLINE_FRAME);
    assert_eq!(
        t.resolved_context().map(|(p, s)| (p as u32, s)),
        Some((61, "pattern")),
        "pattern reading reports with source \"pattern\""
    );
}

/// Backends without a context display stay honestly unknown even when the
/// screen happens to carry a Claude-style statusline string.
#[test]
fn context_pct_unknown_for_backends_without_pattern() {
    for backend in [
        Backend::Codex,
        Backend::OpenCode,
        Backend::Agy,
        Backend::Shell,
    ] {
        let mut t = StateTracker::new(Some(&backend));
        t.feed("  Model: X | Ctx Used: 61.0% | done\n❯");
        assert!(
            t.resolved_context().is_none(),
            "{backend:?} has no context_pattern → unknown"
        );
    }
}

// ── #1947: input-line exclusion — operator-typed / quoted error strings ─────

/// #1947 (operator-reproduced live, 2026-06-10): typing the SRL error string
/// into the claude input line must NOT latch — the line self-satisfies
/// `line_has_error_indicator` ("…Error:"), so pre-#1947 the content anchor
/// passed and a false ServerRateLimit latched (→ spurious AUTO-retry).
#[test]
fn typed_srl_quote_on_claude_input_line_does_not_latch_1947() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "⏺ some earlier assistant output\n\
         ❯ API Error: Server is temporarily limiting requests (not your usage limit)",
    );
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1947: operator-typed SRL quote on the ❯ input line must not latch"
    );
}

/// #1947: a SUBMITTED user message quoting the error (e.g. a dispatch message
/// discussing an SRL incident) renders with the same `❯` prefix — excluded.
#[test]
fn submitted_dispatch_quote_does_not_latch_1947() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "❯ 處理這個 issue:API Error: Server is temporarily limiting requests 的 retry storm\n\
         ⏺ 收到,開始分析",
    );
    assert_ne!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1947: a submitted message quoting the SRL string must not latch"
    );
}

/// #1947 FN guard: the REAL claude SRL error block renders BARE at line start
/// (claude-server-throttle.raw fixture — note: no `⏺` prefix, which is why the
/// v2 line-start anchor needs the corpus gate). It must still latch with the
/// input prompt right below it.
#[test]
fn real_srl_error_with_prompt_below_still_latches_1947() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "API Error: Server is temporarily limiting requests (not your usage limit) · check status.claude.com\n\
         Retrying automatically...\n\
         ❯ ",
    );
    assert_eq!(
        t.get_state(),
        AgentState::ServerRateLimit,
        "#1947: the real (bare, line-start) SRL error block must still latch"
    );
}

/// #1947 multi-backend: codex echoes submitted input as `› <text>` — a quoted
/// rate-limit string there must not latch; the real bare error still does
/// (codex-rate-limit-typical.raw shapes).
#[test]
fn codex_input_echo_quote_excluded_real_error_latches_1947() {
    let mut quoted = StateTracker::new(Some(&Backend::Codex));
    quoted.feed("OpenAI Codex v0.135.0\n› look into rate_limit_exceeded: Rate limit reached\n› ");
    assert_ne!(
        quoted.get_state(),
        AgentState::RateLimit,
        "#1947: a quoted rate-limit string on the › input echo must not latch"
    );

    let mut real = StateTracker::new(Some(&Backend::Codex));
    real.feed(
        "› run the migration\n\
         stream error: rate_limit_exceeded: Rate limit reached for requests. Please try again in 60s.\n\
         › ",
    );
    assert_eq!(
        real.get_state(),
        AgentState::RateLimit,
        "#1947: the real bare codex rate-limit error must still latch"
    );
}

/// #1947 multi-backend: kiro's input prompt is `> ` — a typed quote there must
/// not latch; the real bare `ThrottlingException:` still does
/// (kiro-rate-limit-typical.raw shapes).
#[test]
fn kiro_typed_quote_excluded_real_error_latches_1947() {
    let mut quoted = StateTracker::new(Some(&Backend::KiroCli));
    quoted.feed("conversation above\n> debugging ThrottlingException: Rate exceeded handling");
    assert_ne!(
        quoted.get_state(),
        AgentState::RateLimit,
        "#1947: a typed throttle quote on the kiro > input line must not latch"
    );

    let mut real = StateTracker::new(Some(&Backend::KiroCli));
    real.feed(
        "ThrottlingException: Rate exceeded for this AWS account\n\
         Waiting before retry...\n\
         > ",
    );
    assert_eq!(
        real.get_state(),
        AgentState::RateLimit,
        "#1947: the real bare kiro throttle error must still latch"
    );
}

/// #1947: the hard-wrap flatten fallback drops input/user-message lines BEFORE
/// flattening — a typed SRL quote can't flatten into a legit-looking
/// `API Error: <throttle>` adjacency; a genuine bare hard-wrap still detects.
#[test]
fn flatten_fallback_excludes_input_lines_1947() {
    let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
    let typed = "❯ API Error: Server is temporarily\n❯ limiting requests (not your usage limit)";
    assert_eq!(
        flattened_throttle_detect(patterns, typed, &["❯", ">"]),
        None,
        "#1947: input-marked lines are dropped before flattening"
    );
    // The same content WITHOUT the input markers (a real hard-wrapped error)
    // still detects through the flatten fallback.
    let real = "API Error: Server is temporarily\nlimiting requests (not your usage limit)";
    assert_eq!(
        flattened_throttle_detect(patterns, real, &["❯", ">"]),
        Some(AgentState::ServerRateLimit),
        "#1947: a genuine bare hard-wrapped SRL still detects"
    );
}

// ── #1955: UsageLimit episode lifecycle + self-poisoning gates ──────────────

/// Live banner shape (the `general` incident pane): banner parked in the tail
/// above a clean idle prompt.
const CLAUDE_USAGE_LIMIT_FRAME: &str = "⏺ some earlier work\n\
     ⎿  You've hit your weekly limit · resets 4am (Asia/Taipei)\n\
     \n\
     ❯ \n\
     ────────────";

/// #1955 fail-toward-detection: the real banner above an idle prompt still
/// latches UsageLimit, and the latch anchors a release deadline parsed from
/// the banner's own unlock hint.
#[test]
fn usage_limit_real_banner_latches_with_release_anchor_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(t.get_state(), AgentState::UsageLimit, "real banner latches");
    let release = t.usage_limit_release_at.expect("release deadline anchored");
    assert!(
        release <= Instant::now() + Duration::from_secs(24 * 3600),
        "anchor bounded at 24h"
    );
}

/// #1955 the `general` repro: once the release deadline passes, the SAME
/// stale banner (level-triggered, never scrolls on a silent pane) must
/// release to Idle instead of re-latching — and STAY released on further
/// re-scans of the identical screen (expired-sig suppression). The re-scan
/// rides the throttle-hint static-pane bypass (same-hash feeds).
#[test]
fn usage_limit_stale_banner_releases_after_deadline_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(t.get_state(), AgentState::UsageLimit);

    // Deadline passes (account reset). The pane is SILENT: the identical
    // frame re-feeds (claude clock-tick / static-pane hint bypass).
    t.usage_limit_release_at = Some(Instant::now());
    // transition() only allows a priority DROP after a 2s min-hold; the real
    // incident latch is days old. Backdate safely (checked_sub — windows-safe).
    t.since = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(
        t.get_state(),
        AgentState::Idle,
        "#1955: past the release deadline the stale banner must release to Idle"
    );

    // Further re-scans of the SAME banner stay Idle (no enter-only latch).
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(
        t.get_state(),
        AgentState::Idle,
        "#1955: the released banner signature must not re-latch"
    );
}

/// #1955: a genuinely-NEW limit hit AFTER a release renders fresh (different
/// position → different signature) and must latch again — the suppression is
/// scoped to the released episode, not to usage limits in general.
#[test]
fn usage_limit_new_banner_after_release_relatches_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    t.usage_limit_release_at = Some(Instant::now());
    // transition() only allows a priority DROP after a 2s min-hold; the real
    // incident latch is days old. Backdate safely (checked_sub — windows-safe).
    t.since = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(t.get_state(), AgentState::Idle, "released");

    // A new attempt re-renders the banner at a DIFFERENT screen position.
    let fresh = "⏺ retrying the dispatch\n\
         output line\n\
         more output\n\
         ⎿  You've hit your weekly limit · resets 4am (Asia/Taipei)\n\
         ❯ ";
    t.feed(fresh);
    assert_eq!(
        t.get_state(),
        AgentState::UsageLimit,
        "#1955: a fresh banner (new signature) must latch a new episode"
    );
    assert!(
        t.usage_limit_release_at.is_some(),
        "new episode re-anchors its release"
    );
}

/// #1955: banner scrolled away (detection None) while latched → the
/// maybe_expire arm releases on the deadline.
#[test]
fn usage_limit_expires_via_maybe_expire_when_banner_gone_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(CLAUDE_USAGE_LIMIT_FRAME);
    assert_eq!(t.get_state(), AgentState::UsageLimit);
    t.usage_limit_release_at = Some(Instant::now());
    // transition() only allows a priority DROP after a 2s min-hold; the real
    // incident latch is days old. Backdate safely (checked_sub — windows-safe).
    t.since = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    // A screen with NO matching pattern at all (banner scrolled, no prompt).
    t.feed("plain output text with nothing recognisable\nsecond line");
    assert_eq!(
        t.get_state(),
        AgentState::Idle,
        "#1955: deadline-passed UsageLimit must expire even with the banner gone"
    );
}

/// #1955 self-poisoning: the banner string typed/quoted on the input line
/// must not latch (input-line exclusion — same #1950 technique).
#[test]
fn usage_limit_quote_on_input_line_does_not_latch_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.feed(
        "⏺ analysing the incident\n\
         ❯ the pane showed You've hit your weekly limit · resets 4am yesterday",
    );
    assert_ne!(
        t.get_state(),
        AgentState::UsageLimit,
        "#1955: a quoted banner on the ❯ input line must not latch"
    );
}

/// #1955 self-poisoning: a banner quote buried in DEEP scrollback (outside
/// the bottom-N tail) is discussion, not a live limit (position gate).
#[test]
fn usage_limit_deep_scrollback_quote_does_not_latch_1955() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    let mut screen = String::from("⏺ RCA: You've hit your weekly limit was the banner\n");
    for i in 0..16 {
        screen.push_str(&format!("filler line {i}\n"));
    }
    screen.push_str("❯ ");
    t.feed(&screen);
    assert_ne!(
        t.get_state(),
        AgentState::UsageLimit,
        "#1955: a banner quote outside the live tail must not latch"
    );
}

/// #1955 multi-backend: the codex usage-limit banner walks the same lifecycle
/// (latch + unlock-hint anchor from "Try again at HH:MM").
#[test]
fn usage_limit_codex_banner_same_path_1955() {
    let mut t = StateTracker::new(Some(&Backend::Codex));
    t.feed(
        "You've hit your usage limit. Try again at 15:14.\n\
         › ",
    );
    assert_eq!(
        t.get_state(),
        AgentState::UsageLimit,
        "codex banner latches"
    );
    assert!(
        t.usage_limit_release_at.is_some(),
        "codex unlock hint anchors the release"
    );
}

/// #1955: unlock-hint parser — live forms anchor within 24h; ambiguous /
/// absent hints fall back (None).
#[test]
fn parse_usage_limit_release_forms_1955() {
    let day = Duration::from_secs(24 * 3600);
    for line in [
        "⎿  You've hit your weekly limit · resets 4am (Asia/Taipei)",
        "You've hit your session limit · resets at 11:59pm",
        "You've hit your usage limit. Try again at 15:14.",
    ] {
        let dur = parse_usage_limit_release(line).unwrap_or_else(|| panic!("must parse: {line}"));
        assert!(dur <= day, "bounded at 24h: {line}");
    }
    assert!(
        parse_usage_limit_release("You've hit your weekly limit · resets 4").is_none(),
        "bare hour without am/pm or :MM is ambiguous"
    );
    assert!(
        parse_usage_limit_release("You've hit your weekly limit").is_none(),
        "no hint → conservative fallback path"
    );
}
