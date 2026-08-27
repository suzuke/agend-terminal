use super::*;
use crate::agent::dev_modal::{DevModalGate, GateOutcome, LogicalMs};

// #3314 test seam: perform the dismiss write INLINE instead of on a detached
// thread with the production 300ms pacing. Every new race/one-shot assertion
// needs to observe bytes without sleeping — a wall-clock wait is exactly the
// dependence that let the first draft of the one-shot regression pass against
// unfixed code. Production is untouched; other backends keep their #468 pacing.
#[cfg(test)]
thread_local! {
    /// THREAD-local, deliberately: the test binary runs cases in parallel, and a
    /// process-global flag lets one case's teardown disarm another case
    /// mid-run — which is exactly how the first draft of this seam produced
    /// three spurious failures.
    static INLINE_DISMISS_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_inline_dismiss_write_for_test(on: bool) {
    INLINE_DISMISS_WRITE.with(|f| f.set(on));
}

#[cfg(test)]
thread_local! {
    /// #3314 rendezvous: runs at the LAST instant before the startup-modal
    /// keystroke syscall, so a test can deterministically deliver a
    /// cancellation or a competing write exactly there and assert zero bytes.
    /// It also makes W3 honest rather than rhetorical — the same hook placed
    /// after the write shows a byte already gone cannot be recalled.
    static PRE_WRITE_RENDEZVOUS: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_pre_write_rendezvous_for_test(hook: Option<Box<dyn Fn()>>) {
    PRE_WRITE_RENDEZVOUS.with(|h| *h.borrow_mut() = hook);
}

#[cfg(test)]
fn run_pre_write_rendezvous() {
    let hook = PRE_WRITE_RENDEZVOUS.with(|h| h.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

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
/// H2: per-agent/class flights dedupe without stranding the next startup modal (#1886).
static DISMISS_IN_FLIGHT: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashSet<(String, bool)>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static DISMISS_SCAN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_dismiss_scan_count_for_test() {
    DISMISS_SCAN_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn dismiss_scan_count_for_test() -> usize {
    DISMISS_SCAN_COUNT.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn dismiss_in_flight_for_test(name: &str) -> bool {
    DISMISS_IN_FLIGHT.lock().iter().any(|key| key.0 == name)
}

#[derive(Clone)]
pub struct PreparedDismissPattern {
    pattern: String,
    literal_hint: String,
    regex: std::sync::Arc<regex::Regex>,
    key_seq: Vec<u8>,
    /// #2473: may this pattern fire on a POST-latch re-arm (the agent is in a
    /// dismissible-prompt state but the startup latch is already off)? True only
    /// for workspace-trust prompts; a runtime-approval pattern (claude
    /// `Yes, proceed`) is false so it never auto-acts a real mid-session
    /// permission modal. Derived from [`REARM_PAST_LATCH_TRUST_HINTS`].
    rearm_past_latch: bool,
    /// #3314: may this pattern fire on a post-latch re-arm while the agent has
    /// NEVER reached Idle — i.e. the startup sequence is demonstrably still
    /// running? True for daemon-CAUSED startup modals only. Derived from
    /// [`REARM_PRE_IDLE_STARTUP_HINTS`]; see [`DismissScanScope`].
    rearm_pre_idle: bool,
}

/// #1886 follow-up: RAII guard that removes an agent from `DISMISS_IN_FLIGHT` on
/// drop — including on a panic or early-return of the dismiss thread. Previously
/// the removal was a trailing statement, so a panic before it left a stale entry
/// that silently no-op'd every future dismiss for that agent until daemon
/// restart. Arm it at thread entry; the in-flight slot is freed on any exit.
struct InFlightGuard((String, bool));

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
const DISMISS_REGEX_WIDE_PREFIX: &str = r"(?m)^[^A-Za-z\n]*";

fn dismiss_literal_hint(pattern: &str) -> &str {
    pattern
        .strip_prefix(DISMISS_REGEX_PREFIX)
        .or_else(|| pattern.strip_prefix(DISMISS_REGEX_WIDE_PREFIX))
        .unwrap_or(pattern)
}

/// #2473: literal hints of the WORKSPACE-TRUST dismiss patterns — the ONLY class
/// permitted to RE-ARM past the startup latch (see `try_prepared_dismiss_dialog`'s
/// `trust_class_only`). The agy + claude folder-trust prompts both render the
/// literal `Yes, I trust`; everything else (claude `Yes, proceed` runtime
/// approval, update menus, kiro/codex prompts) stays startup-window-only.
///
/// This is the SINGLE audit point for "what may a daemon auto-keystroke on a
/// post-latch prompt modal". r6's PR #2474 finding: without this scoping, the
/// re-arm fired claude `Yes, proceed` (↑↑Enter) on a real mid-session tool-
/// approval modal — stealing the operator's decision. A new trust prompt that
/// must survive the latch is added here by its literal hint, deliberately, in
/// one place.
const REARM_PAST_LATCH_TRUST_HINTS: &[&str] = &[
    "Yes, I trust",
    "Run Grok Build in a project directory",
    "Do you trust the contents of this directory",
];

fn is_rearm_past_latch_hint(literal_hint: &str) -> bool {
    REARM_PAST_LATCH_TRUST_HINTS.contains(&literal_hint)
}

/// #3314: literal hints of DAEMON-CAUSED STARTUP modals — prompts the daemon
/// itself provokes by how it launches the backend, whose answer is fixed and is
/// never the operator's to make. They may fire on a post-latch re-arm, but ONLY
/// while the agent has never reached Idle ([`DismissScanScope::RearmPreIdle`]).
///
/// The dev-channel modal is here because the daemon passes
/// `--dangerously-load-development-channels`; on a FRESH spawn the workspace-
/// trust dialog renders first and settles the (monotonic) startup latch before
/// this modal ever appears, so the startup window alone never sees it and the
/// agent hangs at `awaiting operator`.
///
/// Why NOT [`REARM_PAST_LATCH_TRUST_HINTS`]: that list carries no time bound, and
/// this literal circulates as ordinary transcript text (issue bodies, PR text,
/// this file). Listed there, a quoted marker line above a LIVE approval modal
/// would auto-answer it — the #2474 (r6) footgun. The pre-Idle bound closes that:
/// a live modal needs a running session, which needs the composer, which is Idle.
const REARM_PRE_IDLE_STARTUP_HINTS: &[&str] = &["WARNING: Loading development channels"];

fn is_rearm_pre_idle_hint(literal_hint: &str) -> bool {
    REARM_PRE_IDLE_STARTUP_HINTS.contains(&literal_hint)
}

/// #3314: which dismiss patterns may a single rendered frame's scan consider?
/// Selected by [`dismiss_scan_scope`] from the startup latch plus whether the
/// agent has ever been Idle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DismissScanScope {
    /// Startup latch still open: every configured pattern (unchanged behavior).
    Startup,
    /// Post-latch re-arm, agent has never reached Idle — the startup sequence is
    /// still running: workspace-trust patterns plus daemon-caused startup modals.
    RearmPreIdle,
    /// Post-latch re-arm after the agent has settled at least once: workspace-
    /// trust only, so a runtime-approval modal is never auto-acted (#2473/#2474).
    RearmSettled,
}

/// #3314: the scan scope for this frame. `scan_enabled` is the startup latch;
/// `ever_idle` records that the agent has reached Idle at least once, which is
/// the point after which a startup modal can no longer legitimately appear.
pub(crate) fn dismiss_scan_scope(scan_enabled: bool, ever_idle: bool) -> DismissScanScope {
    match (scan_enabled, ever_idle) {
        (true, _) => DismissScanScope::Startup,
        (false, false) => DismissScanScope::RearmPreIdle,
        (false, true) => DismissScanScope::RearmSettled,
    }
}

impl PreparedDismissPattern {
    /// #3314: may this pattern be considered under `scope`?
    fn eligible(&self, scope: DismissScanScope) -> bool {
        match scope {
            DismissScanScope::Startup => true,
            DismissScanScope::RearmPreIdle => self.rearm_past_latch || self.rearm_pre_idle,
            DismissScanScope::RearmSettled => self.rearm_past_latch,
        }
    }
}

pub fn prepare_dismiss_patterns(
    dismiss_patterns: &[(String, Vec<u8>)],
) -> Vec<PreparedDismissPattern> {
    dismiss_patterns
        .iter()
        .filter_map(|(pattern, key_seq)| {
            let regex = compile_dismiss_regex(pattern)?;
            let literal_hint = dismiss_literal_hint(pattern).to_string();
            Some(PreparedDismissPattern {
                pattern: pattern.clone(),
                rearm_past_latch: is_rearm_past_latch_hint(&literal_hint),
                rearm_pre_idle: is_rearm_pre_idle_hint(&literal_hint),
                literal_hint,
                regex,
                key_seq: key_seq.clone(),
            })
        })
        .collect()
}

#[allow(dead_code)]
pub fn try_dismiss_dialog(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[(String, Vec<u8>)],
) -> bool {
    let prepared = prepare_dismiss_patterns(dismiss_patterns);
    // Default callers (tests + the `try_dismiss_dialog` seam) get the full
    // startup-window scan.
    // Default callers (tests + the `try_dismiss_dialog` seam) get the full
    // startup-window scan with a permanently-armed, never-spent gate, and the
    // stability window PRE-SATISFIED by observing this same frame once at t=0,
    // so a single call keeps its historical meaning for the backends that use
    // this seam. It is therefore NOT a model of production timing: the read
    // loop supplies a real monotonic clock and a real per-generation gate, and
    // there a candidate must genuinely persist unmodified across the window.
    let mut gate = DevModalGate::new(true);
    let _ = gate.observe(screen, LogicalMs(0));
    try_prepared_dismiss_dialog(
        name,
        screen,
        pty_writer,
        &prepared,
        DismissScanScope::Startup,
        &mut gate,
        LogicalMs(crate::agent::dev_modal::MIN_STABLE_MS),
    )
}

/// `scope` (#2473, narrowed by #3314): which patterns this frame's scan may
/// consider. A POST-latch re-arm never reaches runtime-approval patterns (claude
/// `Yes, proceed`), so it can never auto-act a real mid-session permission modal;
/// a re-arm before the agent has ever been Idle additionally reaches daemon-caused
/// startup modals. The startup-window scan passes [`DismissScanScope::Startup`]
/// (full list, unchanged behavior). See [`REARM_PAST_LATCH_TRUST_HINTS`],
/// [`REARM_PRE_IDLE_STARTUP_HINTS`] and [`dismiss_scan_scope`].
pub fn try_prepared_dismiss_dialog(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[PreparedDismissPattern],
    scope: DismissScanScope,
    dev_gate: &mut DevModalGate,
    now: LogicalMs,
) -> bool {
    #[cfg(test)]
    DISMISS_SCAN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    if dismiss_patterns.is_empty() {
        return false;
    }

    for pattern in dismiss_patterns {
        // #2473/#3314: on a post-latch re-arm, skip everything the scope does
        // not admit — never fire runtime-approval (`Yes, proceed`) off the latch,
        // and admit a daemon-caused startup modal only before the agent settles.
        if !pattern.eligible(scope) {
            continue;
        }
        if !pattern.literal_hint.is_empty() && !screen.contains(&pattern.literal_hint) {
            continue;
        }
        // Issue #468: regex match anchored to line start + optional TUI prefix.
        // Substring match (the prior behavior) auto-injected `2\n` / `3\n`
        // whenever the phrase appeared anywhere on screen — including in agent
        // output and scrollback — sending input the user never authorized.
        if pattern.regex.is_match(screen) {
            // Delayed write: TUI escape-sequence parsers need time to distinguish
            // \x1b (ESC key) from \x1b[ (CSI start).  Writing immediately causes
            // Ink-based TUIs (kiro-cli) to interpret \x1b as "ESC to cancel".
            // H2: bounded dismiss — skip if one already in-flight for this agent.
            // Prevents thread accumulation from rapid dialog re-detection.
            // #3314: a daemon-caused STARTUP modal is gated on facts the daemon
            // OWNS — this generation's argv, the untouched-frame epoch, and a
            // per-generation one-shot — never on the frame alone. Recognition is
            // precision, not safety: a replayed or quoted modal satisfies the
            // fingerprint just as well as a live one (measured, see the fixtures).
            // Every other pattern keeps its existing path and pacing untouched.
            #[cfg(test)]
            let mut scheduled_from_first_sighting = false;
            if pattern.rearm_pre_idle {
                match dev_gate.observe(screen, now) {
                    GateOutcome::Enqueue => {}
                    GateOutcome::Schedule => {
                        #[cfg(test)]
                        {
                            scheduled_from_first_sighting = true;
                        }
                    }
                    GateOutcome::Hold => return false,
                    GateOutcome::Refuse(reason) => {
                        tracing::debug!(
                            agent = name,
                            ?reason,
                            "startup-modal dismiss refused by the generation gate"
                        );
                        return false;
                    }
                }
            }
            let flight_key = (name.to_string(), pattern.rearm_pre_idle);
            {
                let mut inflight = DISMISS_IN_FLIGHT.lock();
                if inflight.contains(&flight_key) {
                    return true; // dismiss already pending
                }
                inflight.insert(flight_key.clone());
            }
            let writer = Arc::clone(pty_writer);
            let keys = pattern.key_seq.clone();
            let agent = name.to_string();
            let pattern_text = pattern.pattern.clone();
            // #3314: the barrier is scoped to the startup-modal class ONLY.
            // Other backends send multi-chunk sequences (agy's up-up-Enter), and
            // each chunk's own write bumps the epoch — applying the barrier
            // there would cancel a pattern midway through its own keystrokes.
            //
            // P2-D: snapshot the barrier here, but spend the one-shot only AFTER
            // the write is successfully submitted. A failed thread spawn or a
            // rejected enqueue must leave the generation able to try again;
            // spending early strands it as Refused(Spent) with the modal
            // unanswered, which is the exact failure the rule exists to prevent.
            let barrier = pattern.rearm_pre_idle.then(|| dev_gate.write_barrier());
            let enqueue_receipt = pattern.rearm_pre_idle.then(|| dev_gate.enqueue_receipt());
            #[cfg(test)]
            if INLINE_DISMISS_WRITE.with(std::cell::Cell::get) {
                if scheduled_from_first_sighting {
                    DISMISS_IN_FLIGHT.lock().remove(&flight_key);
                    return false;
                }
                let _guard = InFlightGuard(flight_key.clone());
                run_pre_write_rendezvous();
                let result = if barrier
                    .as_ref()
                    .is_none_or(crate::agent::dev_modal::WriteBarrier::still_valid)
                {
                    write_with_timeout(&writer, &keys)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "PTY write cancelled by stale-frame barrier",
                    ))
                };
                if result.is_ok() {
                    if let Some(receipt) = enqueue_receipt.as_ref() {
                        receipt.mark_enqueued();
                    }
                }
                if result.is_ok() {
                    tracing::info!(agent = name, pattern = %pattern.pattern, "dialog dismiss submitted");
                }
                return true;
            }
            let worker_flight_key = flight_key.clone();
            // fire-and-forget: dialog-dismiss keystroke writer is short-lived
            // (sleep 300ms then write). H2: in-flight slot freed by InFlightGuard
            // on any exit (incl. panic), armed at thread entry below.
            if std::thread::Builder::new()
                .name("dismiss-dialog".into())
                .spawn(move || {
                    // #1886 follow-up: arm the in-flight removal as a Drop guard at
                    // thread entry so a panic / early-return still frees the slot.
                    let _guard = InFlightGuard(worker_flight_key);
                    std::thread::sleep(std::time::Duration::from_millis(
                        crate::agent::dev_modal::MIN_STABLE_MS,
                    ));
                    // #3314 W3: re-check as late as we can. This closes the wide
                    // decide-then-sleep-then-write window, but a check and a
                    // syscall cannot be atomic with respect to another process's
                    // output, so the last instant before `write` remains
                    // irreducible. Bounded by exactly one CR and no retry.
                    if barrier.as_ref().is_some_and(|b| !b.still_valid()) {
                        tracing::debug!(
                            agent = %agent,
                            "#3314: startup-modal keystroke cancelled before write"
                        );
                        return;
                    }
                    // Send keys in chunks split on \r/\n boundaries with delay between,
                    // so TUI frameworks process navigation before confirmation.
                    // H13: route each chunk through `write_with_timeout` (bounded
                    // worker + 5s deadline) rather than holding the raw shared
                    // `writer.lock()` across an unbounded `write_all`. A hung agent
                    // that has stopped draining its PTY input buffer — exactly the
                    // state that triggers a dismiss — would otherwise pin the writer
                    // lock forever, wedging every future inject to that agent until
                    // daemon restart. `write_with_timeout` flushes internally.
                    let result = (|| -> std::io::Result<()> {
                        let mut start = 0;
                        for (i, &b) in keys.iter().enumerate() {
                            if b == b'\r' || b == b'\n' {
                                // Send everything up to (not including) this Enter
                                if start < i {
                                    write_with_timeout_guarded(
                                        &writer,
                                        &keys[start..i],
                                        barrier.clone(),
                                    )?;
                                    std::thread::sleep(std::time::Duration::from_millis(200));
                                }
                                // Send the Enter
                                write_with_timeout_guarded(
                                    &writer,
                                    &keys[i..=i],
                                    barrier.clone(),
                                )?;
                                start = i + 1;
                            }
                        }
                        if start < keys.len() {
                            write_with_timeout_guarded(
                                &writer,
                                &keys[start..],
                                barrier.clone(),
                            )?;
                        }
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            if let Some(receipt) = enqueue_receipt {
                                receipt.mark_enqueued();
                            }
                            tracing::info!(agent = %agent, pattern = %pattern_text, "dialog dismiss submitted");
                            tracing::debug!(agent = %agent, "dismiss keystrokes sent");
                        }
                        Err(error) => {
                            tracing::debug!(agent = %agent, %error, "dialog dismiss write failed");
                        }
                    }
                    // H2: in-flight slot freed by `_guard` on scope exit.
                })
                .is_err()
            {
                tracing::warn!(agent = name, "failed to spawn dismiss-dialog thread");
                DISMISS_IN_FLIGHT.lock().remove(&flight_key);
            }
            return true;
        }
        // Step 4 (Issue #468): operator-visibility log when the literal hint
        // would have triggered the old substring path but the new regex
        // anchor declined — surfaces realistic false positives (mid-paragraph
        // matches, scrollback echoes) without auto-injecting bytes.
        if pattern.literal_hint != pattern.pattern && !pattern.literal_hint.is_empty() {
            tracing::debug!(
                agent = name,
                pattern = %pattern.pattern,
                literal = %pattern.literal_hint,
                "dismiss substring seen but regex didn't match — likely false positive"
            );
        }
    }

    false
}

/// #2473: states in which the agent is BLOCKED on an interactive modal that a
/// configured `dismiss_pattern` keystroke can clear — workspace-trust prompt,
/// update menu, tool-approval. The PTY read loop re-arms its dismiss scan for a
/// frame in one of these states even after the startup latch has fired (see
/// [`dismiss_scan_armed`]).
pub(crate) fn is_dismissible_prompt_state(state: crate::state::AgentState) -> bool {
    use crate::state::AgentState;
    matches!(
        state,
        AgentState::PermissionPrompt | AgentState::InteractivePrompt | AgentState::AwaitingOperator
    )
}

/// #2473: is the dismiss matcher ARMED for this rendered frame? (Cooldown — the
/// rate-limit guard after a fired dismiss — is checked separately at the call
/// site, so this stays a pure two-input arming decision plus the new-frame gate.)
///
/// `scan_enabled` is the STARTUP latch: it begins true (the backend ships
/// dismiss patterns) and the read loop clears it once the agent settles
/// (`Idle || has_productive_output()`), so a backend trust/update modal is only
/// scanned during the launch window — the secondary defense (alongside the
/// anchored regex) against auto-pressing a key on conversation text that merely
/// quotes the modal phrase (#996, the 37-event fixup-lead false-positive class).
///
/// The bug (#2473): agy paints an "Antigravity CLI" startup banner that trips
/// the latch BEFORE its "Do you trust this folder?" modal renders — and
/// `has_productive_output()` is monotonic, so once tripped the latch never
/// re-opens. The matcher therefore never scanned the modal and every fresh agy
/// spawn hung at `prev_state=permission` awaiting an operator.
///
/// Fix: a `prompt_blocked` frame (the agent is in a state a `dismiss_pattern`
/// targets — see [`is_dismissible_prompt_state`]) re-arms the scan even past the
/// latch. This is false-positive-safe: ordinary conversation that quotes the
/// phrase does not put the agent into PermissionPrompt/InteractivePrompt, and the
/// keystroke only fires when the anchored backend regex ALSO matches the frame.
pub(crate) fn dismiss_scan_armed(
    scan_enabled: bool,
    prompt_blocked: bool,
    state_changed: bool,
    pre_idle_dev_modal_visible: bool,
) -> bool {
    pre_idle_dev_modal_visible || (state_changed && (scan_enabled || prompt_blocked))
}

#[cfg(test)]
mod tests {
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
        include_str!("../../tests/fixtures/devchannel-3314/live_modal.txt");
    const FRAME_REPLAY_3314: &str = include_str!("../../tests/fixtures/devchannel-3314/replay.txt");
    const FRAME_COMPETING_3314: &str =
        include_str!("../../tests/fixtures/devchannel-3314/competing.txt");
    const FRAME_QUOTED_3314: &str = include_str!("../../tests/fixtures/devchannel-3314/quoted.txt");
    const FRAME_LIVE_MODAL_2_1_240_WRAPPED_3314: &str =
        include_str!("../../tests/fixtures/devchannel-3314/live_modal_2_1_240_wrapped.txt");
    const FRAME_LIVE_MODAL_2_1_241_3314: &str =
        include_str!("../../tests/fixtures/devchannel-3314/live_modal_2_1_241.txt");

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

    /// #3314: resize/teardown-shaped repaints after the first CR add nothing.
    /// The one-shot is never reset, so there is no path back.
    #[test]
    fn repaints_after_the_first_cr_add_no_bytes_3314() {
        let mut gen = Generation3314::new("3314-repaint", true);
        assert!(gen.stable_frame(FRAME_LIVE_MODAL_3314));
        for _ in 0..5 {
            gen.note_activity(); // attach / resize / any writer
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
        assert!(
            !gen.stable_frame("  WARNING: Loading development channels\n\n  ❯ 1. Yes\n    2. No\n")
        );
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
        let agent_src = include_str!("mod.rs");
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
        let src = include_str!("dev_modal.rs");
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
        let src = include_str!("write_actor.rs");
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
        let vterm = include_str!("../vterm.rs");
        assert!(
            vterm.contains("note_pty_write"),
            "#3314 P1-C: VTerm's terminal-query responses write directly to the \
             PTY and must invalidate an in-flight startup-modal candidate"
        );
    }

    /// #3314 r2 (P2-D): the one-shot must be spent only AFTER the write is
    /// successfully delivered. A spawn, queue, barrier, or write failure must
    /// leave the generation eligible for another observed attempt.
    #[test]
    fn one_shot_is_spent_only_after_successful_submission_3314() {
        let src = include_str!("dismiss.rs");
        let detached = src
            .find("let result = (|| -> std::io::Result<()>")
            .expect("the detached dismiss writer must retain its result");
        let production = &src[detached..];
        let write = production
            .find("write_with_timeout_guarded(")
            .expect("the detached dismiss write must exist");
        let spend = production
            .find("receipt.mark_enqueued()")
            .expect("the detached one-shot spend must exist");
        assert!(
            spend > write,
            "#3314 P2-D: the one-shot is spent BEFORE the write is submitted; a \
             failed spawn or a full queue then strands the generation as Spent \
             with the modal unanswered"
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
        let src = include_str!("mod.rs");
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
            trust_count, 1,
            "claude must have exactly ONE re-arm-eligible (workspace-trust) dismiss pattern"
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
}
