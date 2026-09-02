// t-20260902112535524748-82348-37: re-homed verbatim (one dedent) from the
// inline `mod tests` of src/agent/dismiss.rs — the lane's new tests pushed the
// parent over the 2500-LOC anti-monolith ceiling, and test files are exempt by
// path. Same module path (agent::dismiss::tests); no content change.
use super::*;

/// Pre-#3314-r1 tests exercise the MATCHER, not the generation gate. Give
/// them a permanently-armed gate already past the stability window so their
/// meaning is unchanged by the new parameters.
fn ungated_3314() -> (DevModalGate, LogicalMs) {
    (
        DevModalGate::new(true),
        LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
    )
}

/// As [`ungated_3314`], but with the stability window already satisfied for
/// `screen`, for tests that assert the MATCHER's verdict in a single call.
/// Production never gets this shortcut — see `try_prepared_dismiss_dialog`.
fn ungated_stable_3314(screen: &str) -> (DevModalGate, LogicalMs) {
    let mut gate = DevModalGate::new(true);
    let _ = gate.observe(screen, LogicalMs(0));
    (gate, LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS))
}

fn test_writer() -> PtyWriter {
    Arc::new(Mutex::new(Box::new(Vec::<u8>::new())))
}

#[derive(Clone)]
struct RecordingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for RecordingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FailingWriter {
    attempted: std::sync::mpsc::Sender<()>,
}

impl std::io::Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.attempted.send(());
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected PTY write failure",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// #3294: the dev-channel startup modal IS classified as `PermissionPrompt`,
/// honestly, like any other matching frame (`state::prompt_latch` changes only
/// when that latch releases, never the classification). The startup latch is
/// state-INDEPENDENT, so a frame inside the launch window arms the scan with
/// no dismissible state at all — that half is unchanged.
///
/// #3314 CORRECTED the other half. This test used to assert
/// `!is_rearm_past_latch_hint(hint)` and read that as "so `prompt_blocked` is
/// not its delivery path". The first clause still holds and is still pinned —
/// the dev-channel modal is deliberately NOT trust-class, because that list
/// has no time bound and this literal circulates as ordinary transcript text.
/// The reading was wrong: startup-window-only is precisely what made a fresh
/// spawn hang, since the trust dialog settles the latch before this modal
/// renders. `prompt_blocked` IS its delivery path now — through the pre-Idle
/// re-arm scope, which is bounded by "the agent has never been Idle" instead.
#[test]
fn dev_channel_modal_dismissal_does_not_depend_on_prompt_state_3294() {
    assert!(
        dismiss_scan_armed(true, false, true, false),
        "#3294: a new frame inside the startup latch must arm the scan with no dismissible state"
    );
    let hint = crate::backend::Backend::ClaudeCode
        .preset()
        .dismiss_patterns
        .iter()
        .map(|pattern| dismiss_literal_hint(pattern.label))
        .find(|hint| hint.starts_with("WARNING: Loading development channels"))
        .expect("claude ships the dev-channel dismiss pattern");
    assert!(
        !is_rearm_past_latch_hint(hint),
        "#3294/#3314: the dev-channel pattern must NOT be trust-class — that list is unbounded in time, and this literal circulates as ordinary transcript text"
    );
    assert!(
        is_rearm_pre_idle_hint(hint),
        "#3314: it is a daemon-caused startup modal, so it rides the PRE-IDLE re-arm scope"
    );
}

#[test]
fn claude_development_channel_modal_matches_and_sends_one_enter() {
    let patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::ClaudeCode
        .preset()
        .dismiss_patterns
        .iter()
        .map(|pattern| (pattern.label.to_string(), pattern.sequence.to_vec()))
        .collect();
    // Exact text captured from Claude Code v2.1.224 after starting with
    // --dangerously-load-development-channels server:agend-claude-channel.
    let screen = "\
────────────────────────────────────────────────────────────────────────────────
  WARNING: Loading development channels

  --dangerously-load-development-channels is for local channel development
  only. Do not use this option to run channels you have downloaded off the
  internet.

  Please use --channels to run a list of approved channels.

  Channels:   server:agend-claude-channel

  ❯ 1. I am using this for local development
2. Exit

  Enter to confirm · Esc to cancel
";
    let written = Arc::new(Mutex::new(Vec::new()));
    let writer: PtyWriter = Arc::new(Mutex::new(Box::new(RecordingWriter {
        bytes: Arc::clone(&written),
    })));

    assert!(try_dismiss_dialog(
        "claude-development-channel-modal",
        screen,
        &writer,
        &patterns
    ));

    for _ in 0..20 {
        if written.lock().as_slice() == b"\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(written.lock().as_slice(), b"\r");
}

/// A PTY writer whose `write` BLOCKS — simulates a backend that stopped
/// draining its input (the exact #2160/H13 wedge). Bounded at 60s (>> the 5s
/// `PTY_WRITE_TIMEOUT`) so the timeout fires first while the helper thread +
/// in-progress guard still self-clean instead of leaking for the whole run.
struct ParkWriter;
impl std::io::Write for ParkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(std::time::Duration::from_secs(60));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// CR-2026-06-14 t-25 (RUNTIME behavioral hardening of #2160/H13): the
/// static-grep repro (`writer.lock()` absent in dismiss.rs) is bypassable by
/// aliasing / an API rename. This test proves the actual property the fix
/// exists for — the dismiss write path is BOUNDED: a parked PTY writer must
/// make `write_with_timeout` (the primitive every dismiss keystroke routes
/// through) RETURN `Err(TimedOut)` within its bound, never hang the caller and
/// pin the writer lock forever.
#[test]
fn write_with_timeout_returns_on_parked_writer_h13_2160() {
    let writer: PtyWriter = Arc::new(Mutex::new(Box::new(ParkWriter)));
    let start = std::time::Instant::now();
    let res = write_with_timeout(&writer, b"dismiss-keystrokes");
    let elapsed = start.elapsed();
    let err = res.expect_err("a parked PTY write must time out, not report success");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::TimedOut,
        "a parked write must surface TimedOut"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "write_with_timeout must return within its ~5s bound even when the writer \
         parks (H13/#2160 — no unbounded lock-pinning hang); took {elapsed:?}"
    );
}

#[test]
fn inflight_guard_clears_entry_on_panic_1886() {
    // #1886 follow-up §3.9: a dismiss thread that panics before its normal
    // exit must STILL free the in-flight slot (via InFlightGuard's Drop),
    // else the stale entry permanently no-op's future dismiss for that agent.
    // Inject a panic after the guard is armed and assert the slot is cleared.
    DISMISS_IN_FLIGHT
        .lock()
        .insert(("panic-agent-1886".to_string(), false));
    let h = std::thread::Builder::new()
        .name("dismiss-panic-test".into())
        .spawn(|| {
            let _guard = InFlightGuard(("panic-agent-1886".to_string(), false));
            panic!("injected panic before normal in-flight removal");
        })
        .expect("spawn");
    // Join the panicking thread (the panic is contained to it).
    assert!(h.join().is_err(), "the injected panic must propagate");
    assert!(
        !dismiss_in_flight_for_test("panic-agent-1886"),
        "InFlightGuard must clear the in-flight slot even when the thread panics"
    );
}

#[test]
fn dismiss_fires_when_pattern_in_screen() {
    let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
    let hit = try_dismiss_dialog(
        "t",
        "Do you trust the contents of this directory?",
        &test_writer(),
        &patterns,
    );
    assert!(hit);
}

#[test]
fn dismiss_skips_when_pattern_absent() {
    let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
    let hit = try_dismiss_dialog("t", "unrelated screen content", &test_writer(), &patterns);
    assert!(!hit);
}

#[test]
fn dismiss_skips_when_no_patterns() {
    assert!(!try_dismiss_dialog("t", "anything", &test_writer(), &[]));
}

#[test]
fn dismiss_matches_ink_style_cursor_painted_prompt() {
    // Regression for macOS: Ink-based TUIs (codex) paint text by
    // positioning the cursor before each segment. VTerm resolves this
    // into a clean screen; the old raw-byte strip_ansi path was fragile
    // on such streams. Drive VTerm with BSU + cursor positioning and
    // confirm the rendered screen still contains the pattern literally.
    let mut vt = crate::vterm::VTerm::new(80, 24);
    vt.process(b"\x1b[?2026h"); // begin synchronized update
    vt.process(b"\x1b[5;2HDo you trust"); // row 5 col 2
    vt.process(b"\x1b[5;15H the contents of this directory?");
    vt.process(b"\x1b[?2026l"); // end synchronized update
    let screen = vt.tail_lines(24);
    let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
    assert!(try_dismiss_dialog("t", &screen, &test_writer(), &patterns));
}

#[test]
fn grok_02114_directory_trust_matches_real_modal() {
    let patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::Grok
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    let screen = "\
              Do you trust the contents of this directory?
/private/tmp/agend-s1/workspace/s1-grok

                     Yes, proceed                 y
                     No, quit                     n

                                                  Grok Build  0.2.114 Beta";
    assert!(try_dismiss_dialog(
        "grok-02114-trust",
        screen,
        &test_writer(),
        &patterns
    ));
}

#[test]
fn grok_02114_directory_trust_does_not_match_runtime_approval() {
    let patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::Grok
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    let screen = "\
Requesting permission for: rm -rf build
Yes, proceed                 y
No, cancel                   n";
    assert!(!try_dismiss_dialog(
        "grok-02114-runtime",
        screen,
        &test_writer(),
        &patterns
    ));
}

// ── Issue #468: dismiss precision regression tests ─────────────────
//
// Hotfix #468 replaces `screen.contains(pattern)` substring match with
// an anchored regex (`(?m)^[│║|>\s]*<text>`) so user input and
// scrollback content containing the dialog phrase mid-paragraph cannot
// trigger an unauthorized auto-dismiss.
//
// Production-realistic patterns: these tests use the EXACT regex strings
// from `BackendPreset::dismiss_patterns` so a future refactor that diverges
// the test pattern from prod would still trigger these assertions on the
// prod string.
//
// Regression-proof: revert `try_dismiss_dialog` to use
// `screen.contains(pattern.as_str())` (bare substring match) and the
// false-positive tests below FAIL. Restore the regex match → PASS.

/// Production dismiss regex for kiro-cli's "Trust All Tools" prompt
/// (Issue #468 follow-up — radio-button cursor `)` was unmatched).
const KIRO_TRUST_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}No, exit";
/// Production dismiss regex for Claude's workspace-trust prompt (#996
/// Phase 1). Modern Claude (v2.1.145+) defaults cursor to "Yes, I trust",
/// so the keystroke shipped is single Enter `\r` — see
/// `Backend::ClaudeCode.preset().dismiss_patterns`.
const CLAUDE_TRUST_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust";

/// `(regex, keystrokes)` pair for `try_dismiss_dialog` — `Down` then
/// `Enter` to dismiss kiro-cli's "Trust All Tools" prompt.
fn kiro_trust_patterns() -> Vec<(String, Vec<u8>)> {
    vec![(KIRO_TRUST_REGEX.to_string(), b"\x1b[B\r".to_vec())]
}

/// #996 Phase 1: true Claude workspace-trust prompt — vterm-rendered —
/// MUST still match the anchored regex so the dismiss fires. The fix
/// changes the keystroke (config-pinned in backend.rs tests) but the
/// regex is unchanged. Anti-regression for the dismiss path itself.
#[test]
fn claude_trust_dismiss_matches_real_modal() {
    let mut vt = crate::vterm::VTerm::new(120, 30);
    vt.process(b"\x1b[2J\x1b[H");
    vt.process(" Accessing workspace:\r\n\r\n /private/tmp/claude-test\r\n\r\n".as_bytes());
    vt.process(
        " Quick safety check: Is this a project you created or one you trust?\r\n\r\n".as_bytes(),
    );
    vt.process(" ❯ 1. Yes, I trust this folder\r\n".as_bytes()); // marker on row 1 (default)
    vt.process("   2. No, exit\r\n".as_bytes());
    vt.process(" Enter to confirm · Esc to cancel\r\n".as_bytes());
    let screen = vt.tail_lines(30);
    // Production keystroke after #996 Phase 1: single Enter.
    let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
    assert!(
        try_dismiss_dialog("t", &screen, &test_writer(), &patterns),
        "real Claude trust modal (default-Yes cursor) must still match anchored regex. Screen:\n{screen}"
    );
}

/// #996 Phase 1: operator-quoted content matching the anchored regex —
/// reproduces the exact false-positive class observed today on the
/// fixup-lead pane (37 events between 19:46:55-19:53:04 +08). The match
/// STILL fires (we don't change the regex), but the production keystroke
/// is now `\r` (non-destructive single Enter, pinned in backend.rs
/// tests) instead of the historical up+up+Enter (history-resubmit blast).
#[test]
fn claude_trust_false_positive_quoted_content_still_matches_regex() {
    // Operator pastes (or daemon-routed message includes) the Agy
    // trust-prompt example verbatim from issue #995. The leading `>` + ` `
    // satisfies the `[^A-Za-z\n]{0,8}` anchor → regex matches even
    // though this is normal conversation content, not a real modal.
    let screen = "\
[user] Filing #995 — agy bug. The trust prompt shows:
> Yes, I trust this folder
  No, exit
Should we add a dismiss_pattern?
[claude] checking the existing patterns now
";
    let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
    assert!(
        try_dismiss_dialog("t", screen, &test_writer(), &patterns),
        "regex anchor (?m)^[^A-Za-z\\n]{{0,8}} matches `> Yes, I trust` mid-conversation — \
         this is the surface that produced today's 37 false-positives on fixup-lead. \
         The fix is the keystroke (`\\r`, non-destructive), pinned in backend tests."
    );
}

// ── #2473: dismiss-scan re-arm on prompt-blocked states ──────────────
//
// Root cause: the read loop's startup latch (`dismiss_scan_enabled`) is
// cleared once the agent settles (`Idle || has_productive_output()`), and
// `has_productive_output()` is MONOTONIC. agy paints an "Antigravity CLI"
// banner that trips the latch BEFORE its "Do you trust this folder?" modal
// renders, so the matcher never scanned the modal — every fresh agy spawn
// hung at `prev_state=permission`. Fix: a `prompt_blocked` frame re-arms the
// scan past the latch. Two orthogonal defenses must BOTH hold (the DUAL
// reviewers' #996 boundary): the anchored regex gates "content looks like a
// modal", the state-gate (`dismiss_scan_armed`) gates "the timing is a real
// modal". The tests below cover BOTH directions of BOTH layers.

/// #2473 POSITIVE: the real agy trust modal. The startup latch is already
/// OFF (the banner settled the agent), but the agent is in PermissionPrompt,
/// so the scan RE-ARMS and the anchored regex matches the live grid → Enter
/// fires. This is the exact production path that was dead.
#[test]
fn agy_trust_modal_rearms_and_fires_when_prompt_blocked_2473() {
    use crate::state::AgentState;
    // Real agy modal rendered through VTerm (cursor `>` marker, col 0).
    let mut vt = crate::vterm::VTerm::new(120, 30);
    vt.process(b"\x1b[2J\x1b[H");
    vt.process(" Do you trust the contents of this folder?\r\n\r\n".as_bytes());
    vt.process(" > Yes, I trust this folder\r\n".as_bytes());
    vt.process("   No, exit\r\n".as_bytes());
    let screen = vt.tail_lines(30);
    // agy preset dismiss regex == claude's (backend.rs:642).
    let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
    // Latch OFF (startup banner already tripped it) but state is PermissionPrompt.
    let armed = dismiss_scan_armed(
        /* scan_enabled */ false,
        is_dismissible_prompt_state(AgentState::PermissionPrompt),
        /* state_changed */ true,
        /* pre_idle_dev_modal_visible */ false,
    );
    assert!(
        armed,
        "#2473: a PermissionPrompt frame must re-arm dismiss even past the startup latch"
    );
    // Fire through the ACTUAL post-latch re-arm path (`trust_class_only=true`):
    // agy `Yes, I trust` IS workspace-trust-class, so it still fires.
    let prepared = prepare_dismiss_patterns(&patterns);
    assert!(
        armed
            && try_prepared_dismiss_dialog(
                "agy",
                &screen,
                &test_writer(),
                &prepared,
                DismissScanScope::RearmSettled,
                &mut ungated_3314().0,
                LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
            ),
        "#2473: the re-armed (trust-class-only) scan must match the real agy trust modal \
         and fire Enter. Screen:\n{screen}"
    );
}

/// #2473 NEGATIVE (r6 blocking finding on PR #2474): a POST-latch re-arm must
/// NOT fire claude's RUNTIME-APPROVAL pattern `Yes, proceed` (↑↑Enter) — that
/// would auto-act a real mid-session permission modal (also classified
/// PermissionPrompt) and steal the operator's decision. Only workspace-trust
/// (`Yes, I trust`) may re-arm. Uses the REAL claude preset so a future preset
/// edit that re-introduces the footgun trips this test.
#[test]
fn claude_yes_proceed_not_rearmed_past_latch_2473_r6() {
    let claude_patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::ClaudeCode
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    let prepared = prepare_dismiss_patterns(&claude_patterns);

    // A real claude runtime tool-approval modal (the operator's call).
    let mut vt = crate::vterm::VTerm::new(120, 30);
    vt.process(b"\x1b[2J\x1b[H");
    vt.process(" Bash command\r\n   rm -rf build/\r\n\r\n".as_bytes());
    vt.process(" > Yes, proceed\r\n".as_bytes());
    vt.process("   No, and tell Claude what to do differently\r\n".as_bytes());
    vt.process(" Esc to cancel · Enter to confirm\r\n".as_bytes());
    let screen = vt.tail_lines(30);

    // Post-latch re-arm: `Yes, proceed` is NOT trust-class → skipped →
    // returns false. A false return is authoritative "did not fire": the
    // keystroke write happens ONLY inside the matched-fire block, which a
    // false return never enters — so no `\x1b[A\x1b[A\r`.
    // #3314 widened the re-arm into two scopes; the runtime-approval pattern
    // must stay excluded from BOTH, so the new pre-Idle scope cannot become a
    // back door into the exact footgun r6 rejected.
    for scope in [
        DismissScanScope::RearmSettled,
        DismissScanScope::RearmPreIdle,
    ] {
        assert!(
            !{
                let (mut g, t) = ungated_3314();
                try_prepared_dismiss_dialog(
                    "claude",
                    &screen,
                    &test_writer(),
                    &prepared,
                    scope,
                    &mut g,
                    t,
                )
            },
            "#2473 r6: `Yes, proceed` (runtime approval) must NOT re-arm past the latch \
             under {scope:?}. Screen:\n{screen}"
        );
    }

    // Sanity: in the STARTUP window the SAME pattern DOES match — proving it
    // is the re-arm GATE that blocks it, not the regex.
    assert!(
        try_prepared_dismiss_dialog(
            "claude",
            &screen,
            &test_writer(),
            &prepared,
            DismissScanScope::Startup,
            &mut ungated_3314().0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "sanity: in the startup window `Yes, proceed` still matches (gate, not regex, blocks re-arm)"
    );
}

/// #3314 fixture: the dev-channel startup modal as Claude Code v2.1.224
/// renders it under `--dangerously-load-development-channels`. Same capture
/// as `claude_development_channel_modal_matches_and_sends_one_enter` and
/// `state::tests::DEV_CHANNEL_STARTUP_MODAL_3294`.
const DEV_CHANNEL_STARTUP_MODAL_3314: &str = "\
────────────────────────────────────────────────────────────────────────────────
  WARNING: Loading development channels

  --dangerously-load-development-channels is for local channel development
  only. Do not use this option to run channels you have downloaded off the
  internet.

  Please use --channels to run a list of approved channels.

  Channels:   server:agend-claude-channel

  ❯ 1. I am using this for local development
2. Exit

  Enter to confirm · Esc to cancel
";

/// #3314 fixture: the SAME marker line as ORDINARY TRANSCRIPT TEXT sitting
/// above a LIVE, unrelated approval modal that the operator owns. Shape and
/// rationale taken verbatim from
/// `state::tests::bare_marker_transcript_does_not_blind_a_later_generic_dialog_3294`
/// — this string circulates in issue bodies, PR text and this fix's own
/// source, and the frame still classifies `PermissionPrompt`. Pressing Enter
/// here answers "Yes" to a question the operator was asked.
const DEV_CHANNEL_MARKER_OVER_LIVE_MODAL_3314: &str = "\
WARNING: Loading development channels

  Delete every file in this directory?

  ❯ 1. Yes
2. No

  Enter to confirm · Esc to cancel
";

fn claude_prepared_patterns_3314() -> Vec<PreparedDismissPattern> {
    let patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::ClaudeCode
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    prepare_dismiss_patterns(&patterns)
}

fn recording_writer_3314() -> (PtyWriter, Arc<Mutex<Vec<u8>>>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer: PtyWriter = Arc::new(Mutex::new(Box::new(RecordingWriter {
        bytes: Arc::clone(&bytes),
    })));
    (writer, bytes)
}

/// #3314 RED: on a FRESH spawn the trust dialog renders first, is dismissed
/// while the startup latch is still open, Claude then paints output that
/// trips the (monotonic) latch, and only THEN does the dev-channel modal
/// render. The frame re-arms the scan (#2473) because it is PermissionPrompt,
/// but the dev-channel pattern is excluded from the re-arm class — so the
/// modal is never dismissed and the agent escalates `awaiting operator` at
/// ~35-39s. This drives the production re-arm path end to end: real preset
/// patterns, real classifier, real matcher.
#[test]
fn dev_channel_modal_is_dismissed_on_a_post_latch_rearm_3314() {
    use crate::state::AgentState;
    let prepared = claude_prepared_patterns_3314();

    let mut st = crate::state::StateTracker::new(Some(&crate::backend::Backend::ClaudeCode));
    st.feed(DEV_CHANNEL_STARTUP_MODAL_3314);
    assert_eq!(
        st.get_state(),
        AgentState::PermissionPrompt,
        "precondition (#3294): the dev-channel modal classifies PermissionPrompt"
    );
    assert!(
        dismiss_scan_armed(
            /* scan_enabled (latch already off) */ false,
            is_dismissible_prompt_state(st.get_state()),
            /* state_changed */ true,
            /* pre_idle_dev_modal_visible */ false,
        ),
        "precondition (#2473): a prompt-blocked frame re-arms the scan past the startup latch"
    );

    let (writer, written) = recording_writer_3314();
    assert!(
        try_prepared_dismiss_dialog(
            "claude-3314-postlatch",
            DEV_CHANNEL_STARTUP_MODAL_3314,
            &writer,
            &prepared,
            DismissScanScope::RearmPreIdle,
            &mut ungated_stable_3314(DEV_CHANNEL_STARTUP_MODAL_3314).0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "#3314: a dev-channel modal rendered AFTER the startup latch closed must still be \
         dismissed — it is a daemon-CAUSED modal (the daemon passes \
         --dangerously-load-development-channels) with a fixed safe answer, never an \
         operator decision"
    );
    for _ in 0..20 {
        if written.lock().as_slice() == b"\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        written.lock().as_slice(),
        b"\r",
        "#3314: the dismissal sends exactly one Enter (confirms the default option 1)"
    );
}

/// #3314 NEGATIVE, and the reason a bare allowlist entry cannot be the fix:
/// once the agent is SETTLED (it has reached Idle at least once, so a session
/// is running), the same marker line on screen is transcript, not a modal —
/// and the modal that IS live belongs to the operator. Enter here answers it.
/// This is the #2474 (r6) failure mode the re-arm class exists to prevent, so
/// it must hold for the dev-channel pattern too.
#[test]
fn dev_channel_marker_over_a_live_modal_is_not_dismissed_when_settled_3314() {
    use crate::state::AgentState;
    let prepared = claude_prepared_patterns_3314();

    let mut st = crate::state::StateTracker::new(Some(&crate::backend::Backend::ClaudeCode));
    st.feed(DEV_CHANNEL_MARKER_OVER_LIVE_MODAL_3314);
    assert_eq!(
        st.get_state(),
        AgentState::PermissionPrompt,
        "precondition (#3294 r3): a quoted marker must not cost the live dialog its classification"
    );

    let (writer, written) = recording_writer_3314();
    assert!(
        !try_prepared_dismiss_dialog(
            "claude-3314-settled",
            DEV_CHANNEL_MARKER_OVER_LIVE_MODAL_3314,
            &writer,
            &prepared,
            DismissScanScope::RearmSettled,
            &mut ungated_3314().0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "#3314: after the agent has settled, a quoted dev-channel marker must NOT fire — \
         the live modal underneath it is the operator's decision (#2474 r6)"
    );
    assert!(
        written.lock().is_empty(),
        "#3314: nothing may be written to the PTY for a settled-agent transcript match"
    );
}

/// #3314: the BOUND itself. The very same real modal that must be dismissed
/// pre-Idle must NOT be dismissed once the agent has settled — otherwise the
/// pre-Idle scope would be indistinguishable from listing the hint as
/// trust-class, which is the unsafe fix this design rejects.
#[test]
fn dev_channel_modal_is_not_dismissed_after_the_agent_has_settled_3314() {
    let prepared = claude_prepared_patterns_3314();
    let (writer, written) = recording_writer_3314();
    assert!(
        !try_prepared_dismiss_dialog(
            "claude-3314-bound",
            DEV_CHANNEL_STARTUP_MODAL_3314,
            &writer,
            &prepared,
            DismissScanScope::RearmSettled,
            &mut ungated_3314().0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "#3314: a settled agent's re-arm must not fire the dev-channel pattern — after Idle \
         this text is transcript, and the modal that may be live is the operator's"
    );
    assert!(written.lock().is_empty());
    // ... and the pattern is genuinely reachable, so the assertion above is
    // the SCOPE refusing it rather than a regex that never matched.
    let (writer, written) = recording_writer_3314();
    assert!(
        try_prepared_dismiss_dialog(
            "claude-3314-bound-control",
            DEV_CHANNEL_STARTUP_MODAL_3314,
            &writer,
            &prepared,
            DismissScanScope::Startup,
            &mut ungated_stable_3314(DEV_CHANNEL_STARTUP_MODAL_3314).0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "control: the same screen fires inside the startup window"
    );
    for _ in 0..20 {
        if written.lock().as_slice() == b"\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(written.lock().as_slice(), b"\r");
}

// ── codex 0.150.x "Update available!" startup modal (t-…-82348-29) ──
//
// The exact frame archfix-codex-dev was stranded on for ~11h on 2026-09-02
// (pane_snapshot capture, codex 0.150.1). The live child argv DID carry
// `-c check_for_update_on_startup=false` (#1626) and the modal appeared
// anyway, so the #1069 dismiss fallback is the defense that must be true —
// and it was structurally dead: codex's banner matches ready_pattern
// ("OpenAI Codex|›") seconds before the update check returns, the startup
// latch closes, and `Update available!` was in neither re-arm class, so the
// post-latch scan never considered it.
const CODEX_UPDATE_MODAL_0150: &str = "\
  ✨ Update available! 0.150.1 -> 0.152.0

  Release notes: https://github.com/openai/codex/releases

  › 1. Update now (runs `sh -c 'curl -fsSL https://codex.openai.com/install.sh | sh'`)
2. Skip
3. Skip until next version

  Press enter to continue
";

fn codex_prepared_patterns() -> Vec<PreparedDismissPattern> {
    let patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::Codex
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    prepare_dismiss_patterns(&patterns)
}

/// RED (t-…-82348-29): the codex update modal rendered AFTER the startup
/// latch closed (banner matched the ready pattern first) must still be
/// dismissed while the agent has never reached Idle — it is a backend-caused
/// startup modal with a fixed safe answer ("2" = Skip), never an operator
/// decision. Drives the production path: real codex preset patterns, real
/// matcher, the exact stranded frame.
#[test]
fn codex_update_modal_is_dismissed_on_a_post_latch_rearm() {
    let prepared = codex_prepared_patterns();

    let mut st = crate::state::StateTracker::new(Some(&crate::backend::Backend::Codex));
    st.feed(CODEX_UPDATE_MODAL_0150);
    assert!(
        is_dismissible_prompt_state(st.get_state()),
        "precondition (#2473): the update modal frame must re-arm the scan — got {:?}",
        st.get_state()
    );

    let (writer, written) = recording_writer_3314();
    // The gate is UNARMED — codex spawns never pass the dev-channel flag, so
    // this is the production gate state; the update pattern must not route
    // through the dev-modal generation gate at all (its fingerprint is the
    // dev modal's static lines and would refuse this frame outright).
    assert!(
        try_prepared_dismiss_dialog(
            "codex-update-postlatch",
            CODEX_UPDATE_MODAL_0150,
            &writer,
            &prepared,
            DismissScanScope::RearmPreIdle,
            &mut DevModalGate::new(false),
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "a codex update modal rendered after the startup latch closed must still be \
         skipped pre-Idle — the #1069 comment promises this fallback degrades the \
         failure from blocking hang to auto-skip, and that promise must be true"
    );
    for _ in 0..20 {
        if written.lock().as_slice() == b"2\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        written.lock().as_slice(),
        b"2\r",
        "the dismissal selects option 2 (Skip) — least invasive, never auto-updates"
    );
}

/// The #2474 bound for the update-modal hint: once the agent has settled, a
/// quoted "Update available!" line is transcript (this literal circulates in
/// issue bodies and this very file), and any live modal under it is the
/// operator's — the settled re-arm must refuse it. Passes before AND after
/// the fix; with the Startup control below it proves the RED failure is the
/// SCOPE, not the regex or the ✨-prefixed frame text.
#[test]
fn codex_update_marker_is_not_dismissed_when_settled() {
    let prepared = codex_prepared_patterns();
    let (writer, written) = recording_writer_3314();
    assert!(
        !try_prepared_dismiss_dialog(
            "codex-update-settled",
            CODEX_UPDATE_MODAL_0150,
            &writer,
            &prepared,
            DismissScanScope::RearmSettled,
            &mut ungated_3314().0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "a settled agent's re-arm must not fire the update pattern — after Idle this \
         text is transcript (#2474 r6)"
    );
    assert!(written.lock().is_empty());

    // Startup-window control: the same exact frame (leading `  ✨ `) fires
    // inside the startup window, so the regex and the rendered text were
    // never the problem — only the post-latch scope was.
    let (writer, written) = recording_writer_3314();
    assert!(
        try_prepared_dismiss_dialog(
            "codex-update-startup-control",
            CODEX_UPDATE_MODAL_0150,
            &writer,
            &prepared,
            DismissScanScope::Startup,
            &mut ungated_stable_3314(CODEX_UPDATE_MODAL_0150).0,
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
        ),
        "control: the exact ✨-prefixed frame fires inside the startup window"
    );
    for _ in 0..20 {
        if written.lock().as_slice() == b"2\r" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(written.lock().as_slice(), b"2\r");
}
// ── r2 review remedies: F2 (quoted vs live) and F1 (per-spawn one-shot) ──
//
// Round 1 was REJECTED by both reviewers. Two P1 findings, one test block
// each. Everything here drives the PRODUCTION entry points — the real codex
// preset, the real `StateTracker`, the real scan, and (for the loop
// assertion) the real `pty_read_loop`.

/// #3314-style inline-write arming as an RAII guard, so a failing assertion
/// cannot leave the seam armed for the next case on this thread.
struct InlineWrite;

impl InlineWrite {
    fn arm() -> Self {
        set_inline_dismiss_write_for_test(true);
        Self
    }
}

impl Drop for InlineWrite {
    fn drop(&mut self) {
        set_inline_dismiss_write_for_test(false);
    }
}

/// The per-spawn one-shot seam for the backend-startup-hint tests.
///
/// At the RED commit `try_prepared_dismiss_dialog` had no one-shot parameter,
/// so this wrapper dropped the flag and the scan was bounded by nothing; GREEN
/// points it at the production `try_prepared_dismiss_dialog_once_per_spawn` and
/// threads the flag through. The wrapper exists so the test BODIES below are
/// byte-identical across both commits — the only thing that changed is the
/// production machinery they reach.
#[allow(clippy::too_many_arguments)]
fn scan_with_one_shot(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[PreparedDismissPattern],
    scope: DismissScanScope,
    dev_gate: &mut DevModalGate,
    now: LogicalMs,
    backend_startup_hint_spent: &mut bool,
) -> bool {
    try_prepared_dismiss_dialog_once_per_spawn(
        name,
        screen,
        pty_writer,
        dismiss_patterns,
        scope,
        dev_gate,
        now,
        backend_startup_hint_spent,
    )
}

// ── F2: a transcript QUOTING the update menu is not the live modal ──
//
// Reviewer finding F2 (P1): `Skip until next version` as a bare
// PermissionPrompt token is an UNANCHORED whole-visible-text prose match.
// `StatePatterns::detect_with_match` is `re.find(text)` over the entire
// rendered screen (src/state/patterns.rs), and PermissionPrompt is not a
// HIGH_FP state, so neither the #1450 colour anchor nor the #1518 position
// gate runs on it; `gate_on_heartbeat` can only downgrade a PermissionPrompt
// when the heartbeat is FRESH, so a stale or absent heartbeat cannot override
// it either. A codex pane whose transcript merely QUOTES the phrase therefore
// reads prompt_blocked, which re-arms the dismiss scan, which types `2\r`
// into the composer.
//
// The existing prose control (tests/fixtures/state-replay/
// codex-discussion-text.raw) proves nothing about this token: it contains
// NEITHER the phrase nor the menu. These frames are the real control. Each
// carries BOTH the bare phrase in prose AND the complete menu quoted verbatim,
// followed by codex's normal bottom chrome — which is the whole point: the
// live modal REPLACES codex's UI, so nothing is ever painted below it, while a
// quoted one always has the composer (and usually the status line) underneath.

/// Ordinary codex transcript prose that quotes the phrase AND the whole menu.
/// No bottom chrome yet — the two variants below append it.
const CODEX_QUOTED_UPDATE_MENU: &str = "\
  I read through the codex startup menu handling. Option 3 on that menu is
  labelled `Skip until next version`; the daemon auto-presses option 2.
  The frame it renders looks exactly like this:

  ✨ Update available! 0.150.1 -> 0.152.0

  Release notes: https://github.com/openai/codex/releases

  › 1. Update now (runs `sh -c 'curl -fsSL https://codex.openai.com/install.sh | sh'`)
2. Skip
3. Skip until next version

  Press enter to continue
";

/// The quoted menu with codex's idle composer painted under it — the shape a
/// resume repaint leaves on screen.
fn codex_quoted_menu_idle() -> String {
    format!("{CODEX_QUOTED_UPDATE_MENU}\n▌ › \n")
}

/// The quoted menu with the composer AND the working status line under it —
/// the shape while the agent that WROTE that prose is still streaming.
fn codex_quoted_menu_working() -> String {
    format!("{CODEX_QUOTED_UPDATE_MENU}\n▌ › \n  esc to interrupt\n")
}

/// F2 (1): through the REAL `StateTracker`, a codex pane quoting the menu must
/// not classify as a dismissible prompt — it is Idle (composer) or Active
/// (status line), exactly as it reads to a human.
#[test]
fn quoted_codex_update_menu_does_not_classify_as_a_prompt() {
    let patterns = crate::state::StatePatterns::for_backend(&crate::backend::Backend::Codex);
    for (screen, expected) in [
        (codex_quoted_menu_idle(), crate::state::AgentState::Idle),
        (
            codex_quoted_menu_working(),
            crate::state::AgentState::Active,
        ),
    ] {
        assert_eq!(
            patterns.detect(&screen),
            Some(expected),
            "F2: a transcript quoting the update menu under codex's own bottom \
             chrome must read {expected:?}, not a prompt"
        );
        let mut st = crate::state::StateTracker::new(Some(&crate::backend::Backend::Codex));
        st.feed(&screen);
        assert!(
            !is_dismissible_prompt_state(st.get_state()),
            "F2: quoting the menu must not put the pane in a dismissible-prompt \
             state (that is what re-arms the dismiss scan) — got {:?}",
            st.get_state()
        );
    }
}

/// F2 (2): and the dismiss scan itself must not fire on it — under the
/// post-latch pre-Idle re-arm OR inside the startup window, where every
/// pattern is eligible. Byte assertion, not a boolean: the scan's return value
/// is not what reaches the agent's PTY.
#[test]
fn quoted_codex_update_menu_writes_no_bytes_in_any_scope() {
    let _inline = InlineWrite::arm();
    let prepared = codex_prepared_patterns();
    for scope in [DismissScanScope::Startup, DismissScanScope::RearmPreIdle] {
        for screen in [codex_quoted_menu_idle(), codex_quoted_menu_working()] {
            let (writer, written) = recording_writer_3314();
            let mut spent = false;
            let tag = format!("codex-quoted-{scope:?}");
            scan_with_one_shot(
                &tag,
                &screen,
                &writer,
                &prepared,
                scope,
                &mut ungated_stable_3314(&screen).0,
                LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
                &mut spent,
            );
            DISMISS_IN_FLIGHT.lock().retain(|key| key.0 != tag);
            assert!(
                written.lock().is_empty(),
                "F2: a quoted update menu must write ZERO bytes under {scope:?} — \
                 the dismiss label must recognize the LIVE modal structurally, not \
                 a phrase that circulates as transcript"
            );
        }
    }
}

/// #468/#1087 anchoring, RE-HOMED from `backend::tests::
/// codex_update_dismiss_anchored_rejects_mid_line` (t-…-82348-29 r2) because
/// the label's subject changed: it is no longer a line-anchored TITLE
/// (`^[^A-Za-z\n]*Update available!`) but the structural, tail-anchored
/// [`crate::backend_profile::CODEX_UPDATE_MENU_LIVE`].
///
/// Both original intents are carried over — #468 (a mid-line mention must never
/// fire a keystroke) and #1087 (a centered TUI modal with a 40+ char prefix must
/// still match) — and two negatives the old label could not express are added:
/// the title line ALONE is not a menu, and the same block with codex's composer
/// painted under it is quoted transcript, not a live modal.
#[test]
fn codex_update_dismiss_matches_only_the_live_centered_menu() {
    let pattern = crate::backend::Backend::Codex
        .preset()
        .dismiss_patterns
        .iter()
        .find(|dp| dp.label.contains("Update available!"))
        .expect("#1069: the codex update dismiss pattern must exist")
        .label;
    let re = regex::Regex::new(pattern).expect("pattern must compile");
    // #1087: TUI modals are centered, so every line carries a wide indent.
    let pad = " ".repeat(45);
    let title = format!("{pad}✨ Update available! 1.0 -> 2.0");
    let menu = format!(
        "{title}\n\n{pad}Release notes: https://example.invalid\n\n\
         {pad}› 1. Update now (runs `sh -c 'curl -fsSL https://x | sh'`)\n\
         {pad}2. Skip\n{pad}3. Skip until next version\n\n{pad}Press enter to continue"
    );
    assert!(
        re.is_match(&menu),
        "#1087: a centered, complete, LIVE menu must match"
    );
    for (text, why) in [
        (
            "User asked: is there an Update available! for the tool?".to_string(),
            "#468: a mid-line mention must NOT match",
        ),
        (
            title,
            "r2/F2: the title line alone is not the menu — the old label matched it",
        ),
        (
            format!("{menu}\n\n▐ › "),
            "r2/F2: the same block with codex's composer under it is quoted, not live",
        ),
    ] {
        assert!(!re.is_match(&text), "{why}");
    }
}

// ── F1: the backend-startup hint is a per-spawn one-shot ──
//
// Reviewer finding F1 (P1, both reviewers): the ungated hint had NO real
// one-shot. The read loop's 10s `Instant` cooldown is rate limiting, not a
// bound — `feed_with_lazy_fg` reports `state_changed` on ANY screen-hash
// change, so every pre-Idle repaint that still shows the menu re-arms the scan
// once the window lapses, and `DISMISS_IN_FLIGHT` only bounds CONCURRENT
// dismisses. Round 1's "one possible stray 2\r, 10s-cooldown-limited" residual
// claim was false: the count is unbounded in the number of repaints.

/// The live 0.150.1 menu as codex repaints it while the modal owns the screen.
/// The menu block itself is byte-stable; the row ABOVE it changes, so every
/// repaint is a NEW screen hash — which is all `feed_with_lazy_fg` needs to
/// report `state_changed` and re-arm the scan the moment the cooldown lapses.
/// The menu still sits at the tail, so it is genuinely LIVE on every frame.
fn codex_live_menu_repaint(step: usize) -> String {
    format!("  ✱ contacting api.openai.com … {step}\n\n{CODEX_UPDATE_MODAL_0150}")
}

/// F1 at the dismiss level: three CHANGED pre-Idle repaints, each scanned
/// through the production scan entry point exactly as the read loop would once
/// the cooldown had lapsed. Total bytes on the PTY must be ONE `2\r`.
#[test]
fn codex_update_menu_startup_hint_is_one_shot_per_spawn() {
    let _inline = InlineWrite::arm();
    let prepared = codex_prepared_patterns();
    let (writer, written) = recording_writer_3314();
    // ONE flag for the whole spawn — the read loop owns exactly one of these
    // per `pty_read_loop` invocation and never resets it.
    let mut spent = false;
    for step in 0..3 {
        let screen = codex_live_menu_repaint(step);
        scan_with_one_shot(
            "codex-one-shot",
            &screen,
            &writer,
            &prepared,
            DismissScanScope::RearmPreIdle,
            &mut DevModalGate::new(false),
            LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
            &mut spent,
        );
        DISMISS_IN_FLIGHT
            .lock()
            .retain(|key| key.0 != "codex-one-shot");
    }
    assert_eq!(
        written.lock().as_slice(),
        b"2\r",
        "F1: the backend-startup hint must be spent for the whole spawn at its \
         FIRST dispatch — three post-cooldown scans of a still-live menu must \
         send one Skip, not one per repaint"
    );
}

/// F1 through the REAL read loop — the reviewer's exact wording: "changed
/// repaints before Idle across >10s through the real read loop".
///
/// This drives production `pty_read_loop` (src/agent/mod.rs) with a reader that
/// hands it three full repaints of the live menu and then EOF: real VTerm, real
/// `feed_with_lazy_fg` dedup, real `dismiss_scan_armed`, real cooldown
/// bookkeeping, real scope selection, real `try_prepared_dismiss_dialog*`.
///
/// The cooldown is compressed to 1ms through `dismiss_cooldown()` and the
/// reader pauses 5ms between repaints, so every repaint arrives AFTER the
/// window lapsed. That makes the assertion STRICTLY STRONGER than a 10s test
/// would be: the cooldown is provably not what bounds the writes, so the
/// per-spawn flag is the only thing that can.
#[test]
#[allow(clippy::unwrap_used)]
fn read_loop_answers_the_codex_update_menu_once_per_spawn() {
    let _inline = InlineWrite::arm();
    set_dismiss_cooldown_for_test(Some(std::time::Duration::from_millis(1)));
    struct Repaints {
        step: usize,
    }
    impl std::io::Read for Repaints {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.step >= 3 {
                return Ok(0);
            }
            if self.step > 0 {
                // Cross the (compressed) cooldown, so the next repaint is
                // scanned rather than rate-limited away.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // Full repaint: clear + home, CRLF line ends like a real PTY.
            let frame = format!(
                "\x1b[2J\x1b[H{}",
                codex_live_menu_repaint(self.step).replace('\n', "\r\n")
            );
            self.step += 1;
            let bytes = frame.as_bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            Ok(n)
        }
    }

    let (writer, written) = recording_writer_3314();
    let core = Arc::new(crate::sync_audit::CoreMutex::new(AgentCore {
        vterm: crate::vterm::VTerm::new(120, 40),
        subscribers: Vec::new(),
        state: crate::state::StateTracker::new(Some(&crate::backend::Backend::Codex)),
        health: crate::health::HealthTracker::new(),
        api_activity: crate::agent::ApiActivity::default(),
        observed_status: None,
    }));
    let dismiss_patterns: Vec<(String, Vec<u8>)> = crate::backend::Backend::Codex
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    let ctx = PtyReadContext {
        name: "codex-loop-one-shot".to_string(),
        instance_id: crate::types::InstanceId::default(),
        core,
        pty_writer: Arc::clone(&writer),
        registry: Arc::new(Mutex::new(std::collections::HashMap::new())),
        home: None,
        crash_tx: None,
        dismiss_patterns: prepare_dismiss_patterns(&dismiss_patterns),
        dev_modal_armed: false,
        // shutdown=true keeps `handle_pty_close` on its cleanup-only path at
        // EOF (no process wait, no crash classification) — the loop body is
        // what this test is about.
        shutdown: Some(Arc::new(std::sync::atomic::AtomicBool::new(true))),
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        generation: crate::agent::crash_disposition::SpawnGeneration::default(),
    };
    let mut reader = Repaints { step: 0 };
    let capture = crate::capture::make_capture_writer(None, "codex-loop-one-shot", "test");
    crate::agent::pty_read_loop(&mut reader, &ctx, capture);
    set_dismiss_cooldown_for_test(None);
    DISMISS_IN_FLIGHT
        .lock()
        .retain(|key| key.0 != "codex-loop-one-shot");

    assert_eq!(
        written.lock().as_slice(),
        b"2\r",
        "F1: three CHANGED pre-Idle repaints of the live menu, each arriving \
         after the cooldown lapsed, must produce exactly ONE Skip through the \
         real read loop — the per-spawn one-shot is the bound, not the cooldown"
    );
}

// ── #3314 r1: real-capture frames, generation gate, byte-count contract ──
//
// Provenance and the version rules are in
// tests/fixtures/devchannel-3314/MANIFEST.yaml. These are REAL captures
// (the #1450 rule) rendered from live daemon-managed Claude instances on CLI
// 2.1.237; every static line the recognizer depends on was separately
// confirmed byte-present in the 2.1.238 binary.
//
// Every assertion below is on BYTES, and none of them sleeps: the harness
// drives an injected logical clock and writes inline. Wall-clock waiting is
// what let the first draft of the one-shot regression pass against unfixed
// code, so it is banned here.

const FRAME_LIVE_MODAL_3314: &str =
    include_str!("../../../tests/fixtures/devchannel-3314/live_modal.txt");
const FRAME_REPLAY_3314: &str = include_str!("../../../tests/fixtures/devchannel-3314/replay.txt");
const FRAME_COMPETING_3314: &str =
    include_str!("../../../tests/fixtures/devchannel-3314/competing.txt");
const FRAME_QUOTED_3314: &str = include_str!("../../../tests/fixtures/devchannel-3314/quoted.txt");
const FRAME_LIVE_MODAL_2_1_240_WRAPPED_3314: &str =
    include_str!("../../../tests/fixtures/devchannel-3314/live_modal_2_1_240_wrapped.txt");
const FRAME_LIVE_MODAL_2_1_241_3314: &str =
    include_str!("../../../tests/fixtures/devchannel-3314/live_modal_2_1_241.txt");

/// One process generation, driven deterministically.
struct Generation3314 {
    gate: DevModalGate,
    prepared: Vec<PreparedDismissPattern>,
    writer: PtyWriter,
    written: Arc<Mutex<Vec<u8>>>,
    now: u64,
    tag: String,
}

impl Generation3314 {
    fn new(tag: &str, armed: bool) -> Self {
        set_inline_dismiss_write_for_test(true);
        let (writer, written) = recording_writer_3314();
        Self {
            gate: DevModalGate::new(armed),
            prepared: claude_prepared_patterns_3314(),
            writer,
            written,
            now: 0,
            tag: tag.to_string(),
        }
    }

    /// One rendered frame at the current logical time.
    fn frame(&mut self, screen: &str) -> bool {
        let fired = try_prepared_dismiss_dialog(
            &self.tag,
            screen,
            &self.writer,
            &self.prepared,
            DismissScanScope::Startup,
            &mut self.gate,
            LogicalMs(self.now),
        );
        DISMISS_IN_FLIGHT.lock().retain(|key| key.0 != self.tag);
        fired
    }

    /// The production shape: a candidate is seen, stays untouched, and is
    /// still identical once the stability window has elapsed.
    fn stable_frame(&mut self, screen: &str) -> bool {
        self.frame(screen);
        self.now += crate::agent::dev_modal::MIN_STABLE_MS;
        self.frame(screen)
    }

    fn note_activity(&mut self) {
        self.gate.note_pty_activity();
    }

    fn bytes(&self) -> Vec<u8> {
        self.written.lock().clone()
    }

    fn epoch_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.gate.epoch_handle()
    }
}

impl Drop for Generation3314 {
    fn drop(&mut self) {
        set_inline_dismiss_write_for_test(false);
    }
}

/// #3314: the live modal is answered exactly once. Without this the negative
/// tests are trivially satisfiable by never dismissing anything.
#[test]
fn live_modal_frame_writes_exactly_one_cr_3314() {
    let mut gen = Generation3314::new("3314-live", true);
    assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

/// #3314: a candidate must be STABLE before it is answered. One sighting is
/// not enough — deterministic, because the clock is injected.
#[test]
fn single_sighting_is_not_stable_enough_to_answer_3314() {
    let mut gen = Generation3314::new("3314-unstable", true);
    assert!(!gen.frame(FRAME_LIVE_MODAL_3314));
    assert!(gen.bytes().is_empty(), "one sighting must write nothing");
}

/// #3314: the observed production failure. The generation answers the LIVE
/// modal, and the replayed marker that follows must add nothing. On 2ef791ca
/// this generation sent the correct CR at +0.80s and stale ones at +11.25s
/// and +74.02s.
#[test]
fn replay_after_the_first_cr_adds_no_bytes_3314() {
    let mut gen = Generation3314::new("3314-replay", true);
    assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert_eq!(gen.bytes().as_slice(), b"\r");
    assert!(!gen.stable_frame(FRAME_REPLAY_3314));
    assert!(!gen.stable_frame(FRAME_REPLAY_3314));
    assert_eq!(
        gen.bytes().as_slice(),
        b"\r",
        "#3314: a replayed modal after the first CR must add nothing"
    );
}

/// #3314: same contract for a quoted transcript, which is routine in this
/// repo — the modal text lives in issue bodies, PR text and this file.
#[test]
fn quoted_transcript_after_the_first_cr_adds_no_bytes_3314() {
    let mut gen = Generation3314::new("3314-quoted", true);
    assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert!(!gen.stable_frame(FRAME_QUOTED_3314));
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

/// #3314: output/teardown-shaped activity after the first CR adds nothing.
/// The one-shot is never reset, so there is no path back.
#[test]
fn repaints_after_the_first_cr_add_no_bytes_3314() {
    let mut gen = Generation3314::new("3314-repaint", true);
    assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
    for _ in 0..5 {
        gen.note_activity(); // child output / any writer
        assert!(!gen.stable_frame(FRAME_LIVE_MODAL_3314));
    }
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

/// #3314 R1: a generation whose argv never carried the flag has no such
/// modal, so a marker on its screen is someone else's text. Zero bytes, and
/// this holds in the STARTUP window too.
#[test]
fn unarmed_generation_writes_no_bytes_3314() {
    let mut gen = Generation3314::new("3314-unarmed", false);
    assert!(!gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert!(gen.bytes().is_empty());
}

/// #3314 epoch: any writer touching the PTY invalidates the candidate, so
/// the stability window restarts rather than completing across the write.
#[test]
fn pty_activity_invalidates_the_candidate_3314() {
    let mut gen = Generation3314::new("3314-epoch", true);
    assert!(!gen.frame(FRAME_LIVE_MODAL_3314));
    gen.note_activity();
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    assert!(
        !gen.frame(FRAME_LIVE_MODAL_3314),
        "#3314: a candidate touched by a writer must restart, not complete"
    );
    assert!(gen.bytes().is_empty());
    // Untouched from here, it becomes answerable again.
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    assert!(gen.frame(FRAME_LIVE_MODAL_3314));
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

/// #3314: the harm frame — marker on screen while a LIVE `/model` picker
/// owns the prompt, where Enter would set the operator's default model.
/// Rejected because it does not carry the complete modal.
#[test]
fn competing_live_dialog_frame_writes_no_bytes_3314() {
    let mut gen = Generation3314::new("3314-competing", true);
    assert!(!gen.stable_frame(FRAME_COMPETING_3314));
    assert!(gen.bytes().is_empty());
}

/// #3314: a bare marker line is not a modal.
#[test]
fn bare_marker_line_is_not_a_complete_modal_3314() {
    let mut gen = Generation3314::new("3314-bare", true);
    assert!(!gen.stable_frame("  WARNING: Loading development channels\n\n  ❯ 1. Yes\n    2. No\n"));
    assert!(gen.bytes().is_empty());
}

/// #3314: production capture from Claude 2.1.240 at the operator's real
/// pane width. The explanatory sentence wraps between `this` and `option`;
/// terminal geometry must not make an otherwise complete modal invisible.
#[test]
fn production_wrapped_2_1_240_modal_is_complete_3314() {
    use crate::agent::dev_modal::complete_modal_digest;
    assert!(
        complete_modal_digest(FRAME_LIVE_MODAL_2_1_240_WRAPPED_3314).is_some(),
        "#3314: a width-induced line wrap must preserve the modal fingerprint"
    );
}

/// #3329: production capture from the operator's Claude 2.1.241 startup.
#[test]
fn production_2_1_241_modal_is_complete_3329() {
    use crate::agent::dev_modal::complete_modal_digest;
    assert!(complete_modal_digest(FRAME_LIVE_MODAL_2_1_241_3314).is_some());
}

/// #3314: recognizing the headline is not a successful dismiss. A refused
/// generation must not emit the success signal that operators rely on.
#[test]
#[tracing_test::traced_test]
fn refused_startup_modal_is_not_logged_as_dismissed_3314() {
    let mut gen = Generation3314::new("3314-refused-log", false);
    assert!(!gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert!(gen.bytes().is_empty());
    assert!(
        !logs_contain("auto-dismissing dialog") && !logs_contain("dialog dismiss submitted"),
        "a refused generation must not emit a dismiss-success log"
    );
}

/// #3314: the replacement success signal is emitted only after the write
/// has crossed the generation gate and been submitted.
#[test]
#[tracing_test::traced_test]
fn submitted_startup_modal_has_truthful_success_log_3314() {
    let mut gen = Generation3314::new("3314-submitted-log", true);
    assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert_eq!(gen.bytes().as_slice(), b"\r");
    assert!(logs_contain("dialog dismiss submitted"));
}

/// #3314: the real detached writer sleeps before its final barrier check.
/// Cancelling in that window is not a submission: it must neither spend the
/// generation nor emit the success signal.
#[test]
#[tracing_test::traced_test]
fn cancelled_detached_write_is_not_a_submission_3314() {
    let mut gen = Generation3314::new("3314-detached-cancel", true);
    assert!(!gen.frame(FRAME_LIVE_MODAL_3314));
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    set_inline_dismiss_write_for_test(false);
    assert!(try_prepared_dismiss_dialog(
        &gen.tag,
        FRAME_LIVE_MODAL_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::Startup,
        &mut gen.gate,
        LogicalMs(gen.now),
    ));

    gen.note_activity();
    std::thread::sleep(std::time::Duration::from_millis(350));
    assert!(gen.bytes().is_empty());
    assert!(!logs_contain("dialog dismiss submitted"));
    assert_ne!(
        gen.gate.observe(FRAME_LIVE_MODAL_3314, LogicalMs(gen.now)),
        GateOutcome::Refuse(crate::agent::dev_modal::Refused::Spent),
        "a barrier-cancelled detached write must leave the one-shot unspent"
    );
}

/// #3314: a valid barrier does not make a failed PTY write a submission.
/// This reaches the detached writer's result arm rather than its earlier
/// stale-frame check, pinning the production path that review found absent.
#[test]
fn failed_detached_write_leaves_the_one_shot_unspent_3314() {
    let mut gen = Generation3314::new("3314-detached-failure", true);
    assert!(!gen.frame(FRAME_LIVE_MODAL_3314));
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    set_inline_dismiss_write_for_test(false);
    let (attempted, observed) = std::sync::mpsc::channel();
    gen.writer = Arc::new(Mutex::new(Box::new(FailingWriter { attempted })));
    assert!(try_prepared_dismiss_dialog(
        &gen.tag,
        FRAME_LIVE_MODAL_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::Startup,
        &mut gen.gate,
        LogicalMs(gen.now),
    ));
    observed
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("the detached writer must attempt the PTY write");
    for _ in 0..100 {
        if !dismiss_in_flight_for_test(&gen.tag) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        !dismiss_in_flight_for_test(&gen.tag),
        "the detached writer must finish before the gate is inspected"
    );
    assert_ne!(
        gen.gate.observe(FRAME_LIVE_MODAL_3314, LogicalMs(gen.now)),
        GateOutcome::Refuse(crate::agent::dev_modal::Refused::Spent),
        "a failed detached write must leave the one-shot unspent"
    );
}

/// #3367: Claude patch releases must not strand an otherwise eligible,
/// structurally complete startup modal.
#[test]
fn claude_patch_version_does_not_control_arming_3367() {
    use crate::agent::dev_modal::{armed_for_spawn, SpawnProvenance};
    let updated = SpawnProvenance {
        argv_has_dev_channel_flag: true,
    };
    assert!(armed_for_spawn(&updated));
}

/// #3314 startup-window expiry: past the bound, text carrying the modal is
/// transcript and must never be answered — even in an armed, unspent
/// generation showing a byte-perfect stable modal.
#[test]
fn eligibility_expires_with_the_startup_window_3314() {
    use crate::agent::dev_modal::ELIGIBILITY_EXPIRY_MS;
    let mut gen = Generation3314::new("3314-expiry", true);
    gen.now = ELIGIBILITY_EXPIRY_MS + 1;
    assert!(!gen.stable_frame(FRAME_LIVE_MODAL_3314));
    assert!(
        gen.bytes().is_empty(),
        "#3314: an expired startup window must write nothing"
    );
}

/// #3314: same-class collisions dedupe without spending the one-shot, while
/// an ordinary trust-prompt worker must not block the distinct startup modal.
#[test]
fn collision_does_not_spend_the_one_shot_3314() {
    let mut gen = Generation3314::new("3314-collision", true);
    gen.frame(FRAME_LIVE_MODAL_3314);
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    DISMISS_IN_FLIGHT
        .lock()
        .insert(("3314-collision".to_string(), true));
    let _ = try_prepared_dismiss_dialog(
        "3314-collision",
        FRAME_LIVE_MODAL_3314,
        &gen.writer,
        &gen.prepared,
        DismissScanScope::Startup,
        &mut gen.gate,
        LogicalMs(gen.now),
    );
    DISMISS_IN_FLIGHT
        .lock()
        .remove(&("3314-collision".to_string(), true));
    assert!(gen.bytes().is_empty(), "#3314: a collision writes nothing");
    DISMISS_IN_FLIGHT
        .lock()
        .insert(("3314-collision".to_string(), false));
    assert!(
        gen.frame(FRAME_LIVE_MODAL_3314),
        "#3314: an ordinary worker must not strand the startup modal"
    );
    DISMISS_IN_FLIGHT
        .lock()
        .remove(&("3314-collision".to_string(), false));
    assert_eq!(gen.bytes().as_slice(), b"\r");
}

/// #3314 W3 rendezvous: a competing write arriving at the LAST instant
/// before the syscall cancels the keystroke. Deterministic — the hook runs
/// exactly at that point, with no sleeping and no racing.
///
/// This closes the decide-then-write window. It does NOT close W3 itself: a
/// check and a syscall cannot be atomic with respect to another process's
/// output, so bytes can still change after the final check. That residual is
/// documented, not tested away.
#[test]
fn competing_write_at_the_rendezvous_cancels_the_keystroke_3314() {
    let mut gen = Generation3314::new("3314-rendezvous", true);
    gen.frame(FRAME_LIVE_MODAL_3314);
    gen.now += crate::agent::dev_modal::MIN_STABLE_MS;
    let epoch = gen.gate.write_barrier();
    // Something else writes into this PTY at the rendezvous point.
    let bump = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bump_seen = std::sync::Arc::clone(&bump);
    let gate_epoch = gen.epoch_handle();
    set_pre_write_rendezvous_for_test(Some(Box::new(move || {
        gate_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        bump_seen.store(true, std::sync::atomic::Ordering::SeqCst);
    })));
    gen.frame(FRAME_LIVE_MODAL_3314);
    set_pre_write_rendezvous_for_test(None);
    assert!(
        bump.load(std::sync::atomic::Ordering::SeqCst),
        "the rendezvous must actually have run"
    );
    assert!(
        !epoch.still_valid(),
        "the competing write must invalidate the barrier"
    );
    assert!(
        gen.bytes().is_empty(),
        "#3314: a keystroke cancelled at the rendezvous writes zero bytes"
    );
}

/// #3314 PRODUCTION SEAM: the epoch is bumped by the real
/// `write_with_timeout`, not by a test helper. This is the claim that
/// "every PTY writer invalidates a candidate" rests on — `write_to_pty`
/// delegates here, and so do the inject, dismiss and TUI socket paths — so
/// it is asserted against the production function itself.
#[test]
fn production_pty_write_bumps_the_epoch_3314() {
    let (writer, _written) = recording_writer_3314();
    let epoch = crate::agent::dev_modal::arm_epoch(&writer);
    let before = epoch.load(std::sync::atomic::Ordering::SeqCst);
    let _ = write_with_timeout(&writer, b"anything");
    let after = epoch.load(std::sync::atomic::Ordering::SeqCst);
    crate::agent::dev_modal::disarm_epoch(&writer);
    assert!(
        after > before,
        "#3314: a real PTY write must invalidate an in-flight candidate"
    );
}

#[test]
fn failed_pty_write_does_not_bump_the_epoch_3314() {
    let (attempted, observed) = std::sync::mpsc::channel();
    let writer: PtyWriter = Arc::new(Mutex::new(Box::new(FailingWriter { attempted })));
    let epoch = crate::agent::dev_modal::arm_epoch(&writer);
    let before = epoch.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        write_with_timeout(&writer, b"anything")
            .expect_err("the injected write must fail")
            .kind(),
        std::io::ErrorKind::BrokenPipe
    );
    observed.recv().expect("the writer must be attempted");
    let after = epoch.load(std::sync::atomic::Ordering::SeqCst);
    crate::agent::dev_modal::disarm_epoch(&writer);
    assert_eq!(
        after, before,
        "#3314: a failed write delivered no input and must not invalidate the candidate"
    );
}

#[test]
fn fallback_write_rechecks_the_barrier_3314() {
    let (writer, written) = recording_writer_3314();
    let mut gate = DevModalGate::new(true);
    let barrier = gate.write_barrier();
    gate.note_pty_activity();
    assert_eq!(
        write_with_timeout_guarded(&writer, b"\r", Some(barrier))
            .expect_err("an invalid barrier must cancel the fallback write")
            .kind(),
        std::io::ErrorKind::Interrupted
    );
    assert!(written.lock().is_empty());
}

/// #3314: an UNARMED writer's writes are a no-op, so the registry cannot
/// grow unbounded and a disarmed generation costs nothing.
#[test]
fn writes_to_a_disarmed_writer_are_a_no_op_3314() {
    let (writer, _written) = recording_writer_3314();
    let epoch = crate::agent::dev_modal::arm_epoch(&writer);
    crate::agent::dev_modal::disarm_epoch(&writer);
    let before = epoch.load(std::sync::atomic::Ordering::SeqCst);
    let _ = write_with_timeout(&writer, b"anything");
    assert_eq!(
        epoch.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "#3314: a disarmed generation must not keep counting"
    );
}

/// #3314 TEARDOWN: a keystroke already queued behind the write delay must
/// not land after its generation ended. The queued barrier holds its own
/// Arc, so removing the registry entry alone would NOT have stopped it —
/// which is exactly the gap review caught. The generation-over flag is what
/// stops it.
#[test]
fn generation_teardown_cancels_a_queued_keystroke_3314() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let generation_over = std::sync::Arc::new(AtomicBool::new(false));
    let deleted = std::sync::Arc::new(AtomicBool::new(false));
    let gate = crate::agent::dev_modal::DevModalGate::with_epoch(
        true,
        std::sync::Arc::clone(&epoch),
        std::sync::Arc::clone(&generation_over),
        std::sync::Arc::clone(&deleted),
    );
    let barrier = gate.write_barrier();
    assert!(barrier.still_valid(), "valid while the generation is live");
    generation_over.store(true, Ordering::SeqCst);
    assert!(
        !barrier.still_valid(),
        "#3314: the generation ending must cancel a queued keystroke"
    );
}

/// #3314 DELETE: instance deletion cancels the same way, and it is a
/// SEPARATE flag on purpose — a child that merely exited is not a deleted
/// instance, and conflating them would mislabel a crashed agent.
#[test]
fn instance_deletion_cancels_a_queued_keystroke_3314() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let generation_over = std::sync::Arc::new(AtomicBool::new(false));
    let deleted = std::sync::Arc::new(AtomicBool::new(false));
    let gate = crate::agent::dev_modal::DevModalGate::with_epoch(
        true,
        std::sync::Arc::clone(&epoch),
        std::sync::Arc::clone(&generation_over),
        std::sync::Arc::clone(&deleted),
    );
    let barrier = gate.write_barrier();
    deleted.store(true, Ordering::SeqCst);
    assert!(
        !barrier.still_valid(),
        "#3314: deletion must cancel a queued keystroke"
    );
}

/// #3314 wiring pin (the #1644/#1530-F2 source-grep convention). The
/// semantics above are proven by real tests; this proves the production
/// write chokepoint exists, which no amount of gate-object testing can.
#[test]
fn production_wires_teardown_cancel_and_write_bump_3314() {
    let agent_src = include_str!("../mod.rs");
    assert!(
        agent_src.contains("dev_modal::arm_generation(pty_writer"),
        "#3314/#3315 B2: the read loop must take its gate from `arm_generation`, which \
         hands out the RAII guard that ends the generation. The two trailing statements \
         this replaced were skipped by an unwind — the EFFECT is proven behaviourally by \
         `pty_read_loop_ends_the_generation_on_both_exits_3315`; this pins the call site"
    );
    assert!(
        agent_src.contains("actor_write::record_successful_write"),
        "#3314: write_with_timeout must bump the epoch after successful PTY writes"
    );
}

// ── #3314 r2 RED: the four blockers from the exact-head dual review ──────
//
// All four are wiring/ordering defects that no gate-object test can see, so
// they are pinned where they live. Each assertion FAILS on the reviewed head
// f17fe148 and is environment-independent — no installed backend, no network,
// no timing.

/// #3314 r2 RED-1 (B1 / P1-A): arming must be a fact about the command we
/// ACTUALLY built and spawned, not a re-read afterwards.
///
/// `armed_for_spawn` is called ~104 lines AFTER `.spawn_command(cmd)`, and
/// inside it both facts are re-derived from mutable sources: `spawn_flags`
/// re-does `exists()` + `read_to_string` + parse on the workspace
/// mcp-config (backend.rs), and `which::which` is a SECOND, independent path
/// resolution that an auto-update symlink flip can move between the two
/// reads. Two fail-open paths follow: a generation whose argv never carried
/// the flag can be armed, and a generation running an UNVALIDATED binary can
/// be armed because the re-resolution saw a newer one.
#[test]
fn arming_must_not_re_derive_from_mutable_sources_3314() {
    let src = include_str!("../dev_modal.rs");
    let start = src
        .find("pub(crate) fn armed_for_spawn(")
        .expect("armed_for_spawn must exist");
    let body = &src[start..];
    let end = body[1..]
        .find("\npub(crate) fn ")
        .map(|i| i + 1)
        .unwrap_or(body.len());
    let body = &body[..end];
    assert!(
        !body.contains("spawn_flags"),
        "#3314 B1: arming must not re-read the workspace mcp-config after spawn — \
         take the argv fact captured from the command that was actually built"
    );
    assert!(
        !body.contains("which::which"),
        "#3314 B1: arming must not re-resolve the backend path after spawn — \
         take the binary identity resolved for the command that was actually spawned"
    );
}

/// #3314 r2 RED-2 (P1-B): the write barrier must be re-checked at the REAL
/// syscall. Today it is checked before the actor QUEUE, and
/// `write_actor::service_once` performs the raw `libc::write` later without
/// it — so a queued CR can outlive the last check. The inline recording-writer
/// seam the rendezvous test uses is a TEST path and proves nothing about the
/// registered production writer.
///
/// The first draft of this pin only asserted that `write_actor.rs` mentions
/// `WriteBarrier`, which the FIELD DECLARATION alone satisfies — deleting
/// the actual check left it green. It now pins the check INSIDE
/// `service_once` and, crucially, its ORDER relative to the syscall.
#[test]
fn write_actor_must_carry_the_barrier_to_the_syscall_3314() {
    let src = include_str!("../write_actor.rs");
    let start = src
        .find("fn service_once(")
        .expect("service_once must exist");
    let body = &src[start..];
    let check = body
        .find("still_valid()")
        .expect("#3314 P1-B: service_once must re-check the barrier before it writes");
    let syscall = body
        .find("libc::write(")
        .expect("service_once must perform the write syscall");
    assert!(
        check < syscall,
        "#3314 P1-B: the barrier must be re-checked BEFORE the write syscall, \
         not merely carried on the job"
    );
}

/// #3314 r2 RED-3 (P1-C): VTerm answers terminal queries by writing straight
/// through `writer.try_lock()`, bypassing the normal write chokepoint.
#[test]
fn direct_pty_write_paths_must_invalidate_the_epoch_3314() {
    let vterm = include_str!("../../vterm.rs");
    assert!(
        vterm.contains("note_pty_write"),
        "#3314 P1-C: VTerm's terminal-query responses write directly to the \
         PTY and must invalidate an in-flight startup-modal candidate"
    );
}

/// #3314 B1 GREEN: arming is now a PURE function of the provenance captured
/// from the command that was actually built. No filesystem, no path
/// resolution — so it cannot disagree with the process that is running,
/// which is exactly how r1 failed open.
#[test]
fn arming_is_a_pure_function_of_captured_provenance_3314() {
    use crate::agent::dev_modal::{armed_for_spawn, SpawnProvenance};
    let armed = SpawnProvenance {
        argv_has_dev_channel_flag: true,
    };
    assert!(armed_for_spawn(&armed));

    // And a generation whose argv never carried the flag can never be armed,
    // whatever is on disk — it cannot render this modal at all.
    let unflagged = SpawnProvenance {
        argv_has_dev_channel_flag: false,
    };
    assert!(!armed_for_spawn(&unflagged));
}

/// #3314 REVIEWER RED: the full static fingerprint is NOT a safety
/// mechanism, and this pins why so nobody later mistakes it for one.
/// Measured on the real captures: the replayed frame and the quoted frame
/// BOTH satisfy every static line, in order. Only the competing frame fails,
/// and only because its headline had scrolled off. Safety therefore has to
/// come from the argv / epoch / one-shot gates, which is what the tests
/// above exercise.
#[test]
fn full_static_fingerprint_alone_does_not_separate_live_from_replay_3314() {
    use crate::agent::dev_modal::complete_modal_digest;
    assert!(complete_modal_digest(FRAME_LIVE_MODAL_3314).is_some());
    assert!(
        complete_modal_digest(FRAME_REPLAY_3314).is_some(),
        "#3314: a REPLAYED modal satisfies the fingerprint — it cannot reject it"
    );
    assert!(
        complete_modal_digest(FRAME_QUOTED_3314).is_some(),
        "#3314: a QUOTED modal satisfies the fingerprint too"
    );
    assert!(
        complete_modal_digest(FRAME_COMPETING_3314).is_none(),
        "#3314: the competing frame fails only because its headline scrolled off"
    );
}

/// #3314: the scope selector. The startup latch wins outright; once it is off,
/// "has the agent ever been Idle" decides whether daemon-caused startup modals
/// are still admissible.
#[test]
fn dismiss_scan_scope_matrix_3314() {
    assert_eq!(
        dismiss_scan_scope(true, false),
        DismissScanScope::Startup,
        "latch open, never Idle → full startup scan"
    );
    assert_eq!(
        dismiss_scan_scope(true, true),
        DismissScanScope::Startup,
        "latch open wins even if the agent has been Idle (agy-shaped re-open never happens, but the latch is authoritative while set)"
    );
    assert_eq!(
        dismiss_scan_scope(false, false),
        DismissScanScope::RearmPreIdle,
        "#3314: latch closed by productive output but the agent has never settled → startup modals still admissible"
    );
    assert_eq!(
        dismiss_scan_scope(false, true),
        DismissScanScope::RearmSettled,
        "#2473/#2474: a settled agent's re-arm is trust-class only"
    );
}

/// #3314 wiring pin (the #1644/#1530-F2 source-grep convention): the tests
/// above prove the scope SEMANTICS, but `pty_read_loop` is a 150-line inline
/// block with no unit seam, so deleting the `ever_idle` bookkeeping or passing
/// a constant scope would leave every one of them green while fresh spawns
/// hang again — mutation-blind wiring. Pins the three wiring points.
#[test]
fn pty_read_loop_wires_the_pre_idle_scan_scope_3314() {
    let src = include_str!("../mod.rs");
    let start = src
        .find("\nfn pty_read_loop(")
        .expect("pty_read_loop must exist");
    let after = &src[start..];
    let end = after[1..]
        .find("\nfn ")
        .map(|i| i + 1)
        .unwrap_or(after.len());
    let body = &after[..end];

    assert!(
        body.contains("let mut dismiss_agent_ever_idle = false;"),
        "#3314: the read loop must track whether the agent has ever been Idle"
    );
    assert!(
        body.contains("let agent_is_idle = cur == crate::state::AgentState::Idle;"),
        "#3314: `ever_idle` must be derived from the classified state under the core lock"
    );
    assert!(
        body.contains("if agent_is_idle {"),
        "#3314: recording the flag must be guarded by this frame's classified state"
    );
    // Order is asserted by BYTE OFFSET, not by matching across a newline:
    // Windows runners check out `.rs` with CRLF (`.gitattributes` pins only
    // `docs/*.md` to LF), so a pattern containing `\n` cannot match there —
    // which is exactly how the first revision of this pin failed CI. The
    // offset form is line-ending independent and a stronger claim besides.
    let scope_call = body
        .find("dismiss_scan_scope(dismiss_scan_enabled, dismiss_agent_ever_idle)")
        .expect("#3314: the scan must be scoped by the latch AND the ever-Idle flag, not by the latch alone");
    let flag_set = body
        .find("dismiss_agent_ever_idle = true;")
        .expect("#3314: the read loop must record that the agent has reached Idle");
    assert!(
        flag_set > scope_call,
        "#3314: the flag must be set AFTER this frame's scan, so the settling frame is still scanned pre-Idle"
    );
}

/// #2473 (r6): pin the trust-class classification on the REAL presets — agy +
/// claude `Yes, I trust` re-arm; claude `Yes, proceed` does not; claude has
/// exactly one re-arm-eligible pattern.
#[test]
fn rearm_past_latch_classification_2473() {
    let agy = prepare_dismiss_patterns(&[(
        r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust".to_string(),
        b"\r".to_vec(),
    )]);
    assert!(agy[0].rearm_past_latch, "agy `Yes, I trust` is trust-class");

    let proceed = prepare_dismiss_patterns(&[(
        r"(?m)^[^A-Za-z\n]{0,8}Yes, proceed".to_string(),
        b"\x1b[A\x1b[A\r".to_vec(),
    )]);
    assert!(
        !proceed[0].rearm_past_latch,
        "`Yes, proceed` (runtime approval) is NOT trust-class"
    );

    let claude: Vec<(String, Vec<u8>)> = crate::backend::Backend::ClaudeCode
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    let trust_count = prepare_dismiss_patterns(&claude)
        .iter()
        .filter(|p| p.rearm_past_latch)
        .count();
    assert_eq!(
        trust_count, 2,
        "claude's re-arm-eligible set is exactly the two cursor positions of the ONE workspace-trust modal (`Yes, I trust` and `❯ No, exit`) — `Yes, proceed` and the dev-channel modal must stay out"
    );

    let grok: Vec<(String, Vec<u8>)> = crate::backend::Backend::Grok
        .preset()
        .dismiss_patterns
        .iter()
        .map(|p| (p.label.to_string(), p.sequence.to_vec()))
        .collect();
    assert_eq!(
        prepare_dismiss_patterns(&grok)
            .iter()
            .filter(|p| p.rearm_past_latch)
            .count(),
        2,
        "both Grok workspace-trust variants must re-arm past startup"
    );
}

/// #2473 NEGATIVE (#996 regression): the SAME `> Yes, I trust` phrase, but as
/// conversation content — the agent is Idle/Active (NOT a prompt modal) and
/// the latch is off. The anchored regex (content layer) WOULD match, but the
/// state-gate (timing layer) refuses to arm → the matcher is never called,
/// so no Enter. This is precisely the false-positive case the DUAL reviewers
/// construct; both layers must hold.
#[test]
fn quoted_trust_phrase_in_non_prompt_state_does_not_rearm_996() {
    use crate::state::AgentState;
    let screen = "\
[user] Filing #995 — the agy trust prompt shows:
> Yes, I trust this folder
  No, exit
[claude] checking the dismiss_pattern now
";
    let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
    // Content layer: the anchored regex DOES match the quoted phrase ...
    assert!(
        try_dismiss_dialog("t", screen, &test_writer(), &patterns),
        "precondition: the anchored regex matches the quoted phrase (content-layer FP surface)"
    );
    // ... but the timing layer refuses to arm: latch off + non-prompt state.
    for s in [AgentState::Idle, AgentState::Active] {
        assert!(
            !dismiss_scan_armed(false, is_dismissible_prompt_state(s), true, false),
            "#996: {s:?} + latched-off must NOT re-arm — no scan, no Enter, \
             despite the matching phrase on screen"
        );
    }
}

/// #2473: the arming-decision matrix + the state classification, both
/// directions. Pins which states re-arm (exactly those a `dismiss_pattern`
/// targets) and the frame/latch gating.
#[test]
fn dismiss_scan_arming_matrix_2473() {
    use crate::state::AgentState::*;
    // Startup window: latch ON + a new frame → armed.
    assert!(dismiss_scan_armed(true, false, true, false));
    // No new frame (dedup hit) → never armed, even if prompt-blocked — a
    // cursor-blink redraw that doesn't change the dewrapped tail must not rescan.
    assert!(!dismiss_scan_armed(true, true, false, false));
    assert!(!dismiss_scan_armed(false, true, false, false));
    // Steady-state after startup: latch off + non-prompt → off.
    assert!(!dismiss_scan_armed(false, false, true, false));
    // Latch off + prompt-blocked → RE-ARMED (#2473 core).
    assert!(dismiss_scan_armed(false, true, true, false));
    // A complete, argv-owned development modal may repaint without changing
    // the classified state. Re-arm only during the caller-proven pre-Idle
    // window so a resize cannot permanently cancel the first stable write.
    assert!(dismiss_scan_armed(false, false, false, true));
    // Exactly the states a dismiss_pattern targets re-arm; nothing else does.
    for s in [PermissionPrompt, InteractivePrompt, AwaitingOperator] {
        assert!(is_dismissible_prompt_state(s), "{s:?} must re-arm dismiss");
    }
    for s in [
        Idle,
        Active,
        Starting,
        Hang,
        RateLimit,
        ServerRateLimit,
        UsageLimit,
        Crashed,
    ] {
        assert!(
            !is_dismissible_prompt_state(s),
            "{s:?} must NOT re-arm dismiss (would reopen the #996 false-positive surface)"
        );
    }
}

#[test]
#[tracing_test::traced_test]
fn invalid_regex_cached_no_relog() {
    // r1 fix (PR #469 reviewer): a typo in a backend dismiss pattern must
    // not re-compile + re-log on every screen-update tick. Negative-cache
    // failed compiles so the warn fires once per unique bad pattern.

    // Use a pattern that the `regex` crate rejects. Unclosed group is
    // syntactically invalid in every regex flavor.
    let bad = "(?P<unclosed";
    // Pre-condition: not yet cached.
    assert!(
        !super::dismiss_regex_cache_contains(bad),
        "test invariant: cache must not pre-contain '{bad}'"
    );

    let r1 = super::compile_dismiss_regex(bad);
    assert!(
        r1.is_none(),
        "first call on invalid pattern must return None"
    );
    assert!(
        super::dismiss_regex_cache_contains(bad),
        "first call must populate the negative cache"
    );

    let r2 = super::compile_dismiss_regex(bad);
    assert!(
        r2.is_none(),
        "second call must also return None (from cache)"
    );

    // tracing-test capture: the warn must have fired (at least once).
    // Asserting "exactly once" is brittle across test-runner concurrency,
    // but the cache assertion above proves the second call did not
    // re-attempt compile — so the warn cannot have fired again from the
    // second invocation.
    assert!(
        logs_contain("dismiss regex compile failed"),
        "compile failure must be logged at warn level"
    );
}

#[test]
fn issue_468_logs_substring_near_miss_for_operator_visibility() {
    // Step 4 (Issue #468): when the literal hint would have triggered
    // the old substring path but the new regex declined, emit a debug
    // log so the operator can see realistic false positives.
    // Test asserts behavior: try_dismiss_dialog returns false (no
    // injection) but the regex compile + literal extraction path is
    // exercised. The log itself is observed indirectly via the no-op
    // outcome (the actual log line is captured by tracing-test in
    // dedicated integration suites elsewhere; keeping this test free
    // of subscriber setup avoids per-test global-state collisions).
    let screen = "user said: Yes, I trust this repo, right?";
    let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
    let fired = try_dismiss_dialog("t", screen, &test_writer(), &patterns);
    assert!(
        !fired,
        "Step 4: literal-hint near-miss must NOT inject keystrokes"
    );
    // dismiss_literal_hint should recover the bare phrase from the prod regex.
    assert_eq!(
        super::dismiss_literal_hint(CLAUDE_TRUST_REGEX),
        "Yes, I trust",
        "literal hint must strip the standard line-anchor prefix"
    );
}

// ── Issue #468 follow-up: bounded-permissive prefix variants ─────

/// Kiro startup hang (the bug that prompted this PR): the radio-button
/// `)` cursor was outside the original `[│║|>\s]` class, so dismiss
/// silently no-op'd and kiro hung on the trust-all-tools confirmation.
#[test]
fn kiro_trust_dismiss_matches_paren_cursor() {
    // Reproduces the operator's screenshot of kiro startup: the selected
    // option is rendered as `) No, exit`, alternatives as ` Yes, ...`.
    let screen = "\
Allow Trust All Tools mode?

) No, exit
  Yes, I accept
  Yes, and don't ask again
";
    let patterns = kiro_trust_patterns();
    assert!(
        try_dismiss_dialog("t", screen, &test_writer(), &patterns),
        "kiro `) No, exit` (radio-button cursor) must match the bounded class"
    );
}

/// Sanity: the bounded class still accepts the prefixes the original
/// `[│║|>\s]` class supported. Box-drawing + `>` cursor + plain space.
#[test]
fn dismiss_matches_classical_prefixes() {
    let cases = [
        "│ No, exit",   // Ink box-drawing
        "║ No, exit",   // double box-drawing
        "| No, exit",   // ASCII pipe
        "> No, exit",   // chevron cursor
        "  No, exit",   // bare indent
        ") No, exit",   // radio cursor (the new case)
        "[3] No, exit", // digit-bracket choice rows
    ];
    for screen in cases {
        let patterns = kiro_trust_patterns();
        assert!(
            try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "prefix variant must match: {screen:?}"
        );
    }
}

/// Length cap proof: a long indent (more than 8 non-alpha chars)
/// before the phrase must NOT match. Defends against pathological
/// scrollback that happens to start with many non-alpha chars.
#[test]
fn dismiss_rejects_when_prefix_exceeds_length_cap() {
    // 9 non-alpha chars ahead of the phrase — exceeds {0,8}.
    let screen = "         No, exit"; // 9 spaces
    let patterns = kiro_trust_patterns();
    assert!(
        !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
        "9-char non-alpha prefix must exceed length cap and not match"
    );
}

/// False-positive regression: alpha char anywhere in the prefix area
/// (typical of scrollback/user text) must still be rejected.
#[test]
fn dismiss_rejects_alpha_char_in_prefix_zone() {
    // Even though `Pre` is short, an alpha char in the [^A-Za-z\n]{0,8}
    // window breaks the match — proving mid-paragraph text is safe.
    let screen = "Pre: No, exit";
    let patterns = kiro_trust_patterns();
    assert!(
        !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
        "alpha char in prefix zone must invalidate match (regression-safe)"
    );
}

/// Production smoke: spawn a real kiro-cli process and observe its
/// startup screen via VTerm. Asserts that the rendered screen contains
/// the kiro trust prompt and that try_dismiss_dialog matches against
/// the production regex. Skipped when kiro-cli isn't on PATH so the
/// test is safe on CI without forcing a kiro-cli install matrix.
///
/// Run locally with:  cargo test -- --ignored kiro_real_spawn
///
/// Reader runs on a dedicated thread piping into an mpsc channel —
/// portable_pty's `try_clone_reader()` returns a blocking reader, so
/// polling for `WouldBlock` would hang forever waiting on a kiro that
/// has nothing more to write. The channel + `recv_timeout` pattern is
/// the only robust way to bound the wait without a runtime dependency.
#[test]
#[ignore = "spawns real kiro-cli process; run locally only"]
#[cfg(unix)]
fn issue_468_kiro_real_spawn_dismiss_smoke() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::sync::mpsc;

    if which::which("kiro-cli").is_err() {
        eprintln!("SKIP: kiro-cli not on PATH");
        return;
    }

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new("kiro-cli");
    cmd.args(["chat", "--trust-all-tools"]);
    cmd.env("AGEND_GIT_BYPASS", "1");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn kiro-cli");
    drop(pair.slave);

    // Reader thread → mpsc channel; main thread polls with timeout.
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    // fire-and-forget: thread exits when reader hits EOF after child kill.
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut vt = crate::vterm::VTerm::new(80, 24);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(chunk) => vt.process(&chunk),
            Err(_) => break, // timeout or sender disconnected
        }
        if vt.tail_lines(24).contains("No, exit") {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let screen = vt.tail_lines(24);
    let patterns = kiro_trust_patterns();

    // Two valid outcomes prove kiro startup is no longer hung on the
    // confirmation screen — the actual user-visible bug being fixed.
    //
    // (a) "No, exit" rendered → must match regex (real-spawn dismiss).
    // (b) Already past confirmation (kiro saved trust from a prior run,
    //     or `--trust-all-tools` bypassed it) → reaching the ready
    //     prompt within deadline proves no hang.
    //
    // Failure mode: neither marker present within the deadline → kiro
    // really did hang somewhere unexpected.
    let saw_prompt = screen.contains("No, exit");
    let saw_ready = screen.contains("Trust All Tools active")
        || screen.contains("ask a question or describe a task");

    if saw_prompt {
        assert!(
            try_dismiss_dialog("t", &screen, &test_writer(), &patterns),
            "production regex must match real kiro-cli trust prompt. Screen:\n{screen}"
        );
    } else {
        assert!(
            saw_ready,
            "kiro neither rendered the trust prompt nor reached ready state within 5s. \
             Screen:\n{screen}"
        );
        eprintln!(
            "SMOKE NOTE: kiro skipped the trust prompt (saved acceptance or --trust-all-tools \
             bypass). Synthetic-screen unit tests cover the regex correctness for the \
             reported operator screenshot."
        );
    }
}
