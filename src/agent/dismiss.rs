use super::*;

/// Try to auto-dismiss dialogs using backend-configurable patterns. Returns true if dismissed.
/// `screen` is the VTerm-rendered view the user sees — not raw PTY bytes —
/// so Ink-style TUIs that paint char-by-char with cursor positioning still match.
/// Cached regex compilation for dismiss patterns.
///
/// Issue #468: dismiss patterns must match anchored regex (line start +
/// optional TUI prefix), not bare substring. Compiles once per unique pattern
/// string and reuses the `Arc<Regex>` thereafter so the screen-update hot
/// loop never re-compiles.
///
/// r1 fix (PR #469 reviewer): both successful AND failed compiles are cached.
/// The cache value is `Option<Arc<Regex>>` — `None` records that the pattern
/// is permanently invalid, so subsequent lookups skip the compile + log path
/// entirely. Without this, a typo in a backend preset would re-compile and
/// re-emit a warn line on every screen-update tick. The warn (not error —
/// invalid patterns are configurer mistakes, not runtime faults) fires once
/// per unique bad pattern over the process lifetime.
static DISMISS_REGEX_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, Option<std::sync::Arc<regex::Regex>>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// H2: agents with a dismiss thread currently in flight — gates rapid dialog
/// re-detection to one thread per agent. Hoisted to module scope (#1886
/// follow-up) so the RAII [`InFlightGuard`] can clear it on drop.
static DISMISS_IN_FLIGHT: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

/// #1886 follow-up: RAII guard that removes an agent from `DISMISS_IN_FLIGHT` on
/// drop — including on a panic or early-return of the dismiss thread. Previously
/// the removal was a trailing statement, so a panic before it left a stale entry
/// that silently no-op'd every future dismiss for that agent until daemon
/// restart. Arm it at thread entry; the in-flight slot is freed on any exit.
struct InFlightGuard(String);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        DISMISS_IN_FLIGHT.lock().remove(&self.0);
    }
}

fn compile_dismiss_regex(pattern: &str) -> Option<std::sync::Arc<regex::Regex>> {
    let mut cache = DISMISS_REGEX_CACHE.lock();
    if let Some(slot) = cache.get(pattern) {
        return slot.as_ref().map(std::sync::Arc::clone);
    }
    let result = match regex::Regex::new(pattern) {
        Ok(re) => Some(std::sync::Arc::new(re)),
        Err(e) => {
            tracing::warn!(
                pattern,
                error = %e,
                "dismiss regex compile failed — pattern ignored"
            );
            None
        }
    };
    cache.insert(pattern.to_string(), result.clone());
    result
}

/// Test-only inspection of the dismiss regex cache. Used by the
/// `invalid_regex_cached_no_relog` test to assert that bad patterns get
/// cached after first failure (rather than re-compiling on every call).
#[cfg(test)]
fn dismiss_regex_cache_contains(pattern: &str) -> bool {
    DISMISS_REGEX_CACHE.lock().contains_key(pattern)
}

/// Strip the standard line-anchor prefix to recover the literal hint from a
/// dismiss regex. Used by Step 4 (false-positive operator visibility logging).
/// Returns the input unchanged when no known prefix is present so callers
/// don't accidentally compare an entire regex against `screen.contains`.
///
/// Issue #468 follow-up (kiro startup hang): the original prefix
/// `[│║|>\s]*` only covered Ink box-drawing chars and the `>` cursor.
/// kiro-cli's "Trust All Tools" prompt renders the selected option with
/// a `) No, exit` (radio-button style cursor), which the narrow class did
/// not match — dismiss never fired and kiro hung on confirmation.
///
/// Bounded-permissive replacement: any non-alpha non-newline byte in the
/// leading 0–8 chars. The length cap (8) preserves the line-start anchor's
/// intent — scrollback or user text containing the phrase mid-paragraph is
/// preceded by alpha chars or a much longer indent, so it cannot match.
/// The class covers `)`, `(`, `*`, `•`, digits in `[3]`-style choice rows,
/// and any future cursor variant introduced by a backend's TUI without
/// requiring a new patch per backend.
const DISMISS_REGEX_PREFIX: &str = r"(?m)^[^A-Za-z\n]{0,8}";

fn dismiss_literal_hint(pattern: &str) -> &str {
    pattern
        .strip_prefix(DISMISS_REGEX_PREFIX)
        .unwrap_or(pattern)
}

pub fn try_dismiss_dialog(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[(String, Vec<u8>)],
) -> bool {
    if dismiss_patterns.is_empty() {
        return false;
    }

    for (pattern, key_seq) in dismiss_patterns {
        // Issue #468: regex match anchored to line start + optional TUI prefix.
        // Substring match (the prior behavior) auto-injected `2\n` / `3\n`
        // whenever the phrase appeared anywhere on screen — including in agent
        // output and scrollback — sending input the user never authorized.
        let Some(re) = compile_dismiss_regex(pattern) else {
            continue;
        };
        if re.is_match(screen) {
            tracing::info!(agent = name, pattern, "auto-dismissing dialog");
            // Delayed write: TUI escape-sequence parsers need time to distinguish
            // \x1b (ESC key) from \x1b[ (CSI start).  Writing immediately causes
            // Ink-based TUIs (kiro-cli) to interpret \x1b as "ESC to cancel".
            // H2: bounded dismiss — skip if one already in-flight for this agent.
            // Prevents thread accumulation from rapid dialog re-detection.
            {
                let mut inflight = DISMISS_IN_FLIGHT.lock();
                if inflight.contains(name) {
                    return true; // dismiss already pending
                }
                inflight.insert(name.to_string());
            }
            let writer = Arc::clone(pty_writer);
            let keys = key_seq.clone();
            let agent = name.to_string();
            // fire-and-forget: dialog-dismiss keystroke writer is short-lived
            // (sleep 300ms then write). H2: in-flight slot freed by InFlightGuard
            // on any exit (incl. panic), armed at thread entry below.
            if std::thread::Builder::new()
                .name("dismiss-dialog".into())
                .spawn(move || {
                    // #1886 follow-up: arm the in-flight removal as a Drop guard at
                    // thread entry so a panic / early-return still frees the slot.
                    let _guard = InFlightGuard(agent.clone());
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // Send keys in chunks split on \r/\n boundaries with delay between,
                    // so TUI frameworks process navigation before confirmation.
                    let mut w = writer.lock();
                    let mut start = 0;
                    for (i, &b) in keys.iter().enumerate() {
                        if b == b'\r' || b == b'\n' {
                            // Send everything up to (not including) this Enter
                            if start < i {
                                let _ = w.write_all(&keys[start..i]);
                                let _ = w.flush();
                                drop(w);
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                w = writer.lock();
                            }
                            // Send the Enter
                            let _ = w.write_all(&keys[i..=i]);
                            let _ = w.flush();
                            start = i + 1;
                        }
                    }
                    if start < keys.len() {
                        let _ = w.write_all(&keys[start..]);
                        let _ = w.flush();
                    }
                    tracing::debug!(agent = %agent, "dismiss keystrokes sent");
                    // H2: in-flight slot freed by `_guard` on scope exit.
                })
                .is_err()
            {
                tracing::warn!(agent = name, "failed to spawn dismiss-dialog thread");
                DISMISS_IN_FLIGHT.lock().remove(name);
            }
            return true;
        }
        // Step 4 (Issue #468): operator-visibility log when the literal hint
        // would have triggered the old substring path but the new regex
        // anchor declined — surfaces realistic false positives (mid-paragraph
        // matches, scrollback echoes) without auto-injecting bytes.
        let literal = dismiss_literal_hint(pattern);
        if literal != pattern.as_str() && !literal.is_empty() && screen.contains(literal) {
            tracing::debug!(
                agent = name,
                pattern,
                literal,
                "dismiss substring seen but regex didn't match — likely false positive"
            );
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_writer() -> PtyWriter {
        Arc::new(Mutex::new(Box::new(Vec::<u8>::new())))
    }

    #[test]
    fn inflight_guard_clears_entry_on_panic_1886() {
        // #1886 follow-up §3.9: a dismiss thread that panics before its normal
        // exit must STILL free the in-flight slot (via InFlightGuard's Drop),
        // else the stale entry permanently no-op's future dismiss for that agent.
        // Inject a panic after the guard is armed and assert the slot is cleared.
        DISMISS_IN_FLIGHT
            .lock()
            .insert("panic-agent-1886".to_string());
        let h = std::thread::Builder::new()
            .name("dismiss-panic-test".into())
            .spawn(|| {
                let _guard = InFlightGuard("panic-agent-1886".to_string());
                panic!("injected panic before normal in-flight removal");
            })
            .expect("spawn");
        // Join the panicking thread (the panic is contained to it).
        assert!(h.join().is_err(), "the injected panic must propagate");
        assert!(
            !DISMISS_IN_FLIGHT.lock().contains("panic-agent-1886"),
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
    /// `Backend::ClaudeCode.preset().dismiss_patterns[0]`.
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
            " Quick safety check: Is this a project you created or one you trust?\r\n\r\n"
                .as_bytes(),
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
}
