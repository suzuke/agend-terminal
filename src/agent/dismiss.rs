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

/// The PTY read loop's post-dismiss cooldown: once a dismiss has fired, the
/// loop refuses to scan the next frames until this has elapsed.
///
/// RATE LIMITING ONLY — it is NOT a bound on how many times a startup modal
/// can be answered, and no comment here or in `crate::backend` may claim it
/// is. `StateTracker::feed_with_lazy_fg` reports `state_changed` on ANY
/// screen-hash change, so a pre-Idle repaint that still shows the modal
/// re-arms the scan the instant this window lapses, and `DISMISS_IN_FLIGHT`
/// only bounds CONCURRENT dismisses. The real bound is the per-spawn one-shot
/// threaded through [`try_prepared_dismiss_dialog_once_per_spawn`]
/// (t-…-82348-29 r2, review F1).
const DISMISS_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
thread_local! {
    /// Test seam: compress the read loop's cooldown so a loop-level test can
    /// deliver repaints AFTER it has lapsed without sleeping 10s per repaint —
    /// a wall-clock wait is exactly the dependence that lets a one-shot
    /// regression pass against unfixed code (the #3314 lesson). THREAD-local
    /// for the same reason as `INLINE_DISMISS_WRITE`: cases run in parallel and
    /// a process-global lets one case retune another. The loop-level test
    /// drives `pty_read_loop` on its own thread, so the loop reads this value.
    static DISMISS_COOLDOWN_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_dismiss_cooldown_for_test(cooldown: Option<std::time::Duration>) {
    DISMISS_COOLDOWN_OVERRIDE.with(|c| c.set(cooldown));
}

/// The cooldown the PTY read loop applies after a dismiss fires. Production
/// always returns [`DISMISS_COOLDOWN`]; the single accessor exists so the test
/// seam above has one place to intercept.
pub(crate) fn dismiss_cooldown() -> std::time::Duration {
    #[cfg(test)]
    if let Some(cooldown) = DISMISS_COOLDOWN_OVERRIDE.with(std::cell::Cell::get) {
        return cooldown;
    }
    DISMISS_COOLDOWN
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
    /// running? True for daemon-CAUSED startup modals
    /// ([`REARM_PRE_IDLE_STARTUP_HINTS`]) and for backend-caused startup modals
    /// with a fixed safe answer ([`REARM_PRE_IDLE_BACKEND_STARTUP_HINTS`],
    /// t-…-82348-29); see [`DismissScanScope`].
    rearm_pre_idle: bool,
    /// #3314: does this pattern route through the dev-modal generation gate
    /// ([`DevModalGate`])? That gate binds recognition to facts the daemon OWNS
    /// (the dev-channel argv it passed, frame-epoch, a per-generation one-shot)
    /// and its fingerprint is the dev-channel modal's static lines — so it is
    /// TRUE only for [`REARM_PRE_IDLE_STARTUP_HINTS`]. A backend-caused startup
    /// modal (codex update prompt) carries no daemon-owned fact to bind and its
    /// frame never satisfies the dev fingerprint, so routing it through the gate
    /// would refuse it unconditionally (t-…-82348-29 preflight).
    dev_gated: bool,
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

/// t-…-82348-29 r2 (review F2): dismiss labels that are STRUCTURAL rather than
/// `DISMISS_REGEX_*_PREFIX` + literal, mapped to the plain literal that every
/// match necessarily contains.
///
/// Two consumers need that literal and neither can recover it by stripping a
/// prefix: the cheap `screen.contains` prefilter in
/// [`try_prepared_dismiss_dialog_once_per_spawn`] (handing it a whole regex
/// would make the pattern permanently unfireable — the trap
/// [`dismiss_literal_hint`]'s doc warns about), and the hint lists below, whose
/// membership stays keyed on the human-readable literal so the audit point
/// still reads as one.
const STRUCTURAL_DISMISS_HINTS: &[(&str, &str)] = &[(
    crate::backend_profile::CODEX_UPDATE_MENU_LIVE,
    crate::backend_profile::CODEX_UPDATE_MENU_LITERAL,
)];

fn dismiss_literal_hint(pattern: &str) -> &str {
    for (label, hint) in STRUCTURAL_DISMISS_HINTS {
        if *label == pattern {
            return hint;
        }
    }
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
    // t-…-65: the SAME trust modal with the cursor on `No, exit` — listed so a
    // post-latch re-arm keeps it eligible instead of falling through to the
    // `Yes, I trust` entry, whose bare Enter confirms the exit on that shape.
    // Marker-qualified: the bare literal `No, exit` is kiro's hint too.
    "❯ No, exit",
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

/// t-…-82348-29: literal hints of BACKEND-caused startup modals — prompts the
/// backend itself raises during its startup sequence, whose answer is fixed and
/// is never the operator's to make. Like [`REARM_PRE_IDLE_STARTUP_HINTS`] they
/// may fire on a post-latch re-arm ONLY while the agent has never reached Idle,
/// but they do NOT route through the dev-modal generation gate: there is no
/// daemon-owned fact (argv flag) to bind, and the gate's fingerprint is the
/// dev-channel modal's static lines, so the gate would refuse them outright.
///
/// The codex "Update available!" entry is here because codex 0.150.x shows the
/// update menu even when the child argv carries
/// `-c check_for_update_on_startup=false` (#1626 — verified on the live
/// stranded pid), and the modal frame previously classified Idle/Active, so the
/// #1069 fallback was structurally dead: the agent stranded for ~11h until a
/// human pressed a key. The pre-Idle bound keeps the #2474 exposure to the
/// startup window: post-Idle the phrase is transcript and the settled re-arm
/// refuses it.
///
/// r2 (review F1 + F2) — the round-1 residual note that stood here ("one
/// possible stray 2\r, 10s-cooldown-limited") was wrong on BOTH halves. It is
/// replaced by two real bounds, and no comment in this repo may restate the old
/// claim:
///
/// * RECOGNITION (F2): the label is no longer a phrase. It is
///   [`crate::backend_profile::CODEX_UPDATE_MENU_LIVE`] — the complete menu
///   block anchored to the TAIL of the rendered screen — so a transcript that
///   quotes the modal cannot match it in ANY scope, Startup included, not
///   merely on the post-latch re-arm. The same const is the classifier's
///   PermissionPrompt pattern, so quoted text never even reaches
///   `prompt_blocked`.
/// * COUNT (F1): membership in THIS list makes a pattern a PER-SPAWN ONE-SHOT.
///   The flag is owned by the PTY read loop and threaded through
///   [`try_prepared_dismiss_dialog_once_per_spawn`], which spends it at the
///   first DISPATCH. The loop's 10s cooldown is rate limiting, NOT the bound:
///   `StateTracker::feed_with_lazy_fg` reports `state_changed` on any
///   screen-hash change, so without the flag every pre-Idle repaint still
///   showing the menu enqueued another "2\r" once the window lapsed, and
///   `DISMISS_IN_FLIGHT` bounds only CONCURRENT dismisses.
///
/// Consequence, stated plainly: exactly one "2\r" per spawn. If that single
/// write fails, the auto-skip is FORFEIT for the spawn — the pane stays
/// prompt-blocked and visible to the stuck watchdog, which is the honest
/// degradation; `restart_instance` gets a fresh one-shot because the read loop,
/// and with it the flag, restarts.
const REARM_PRE_IDLE_BACKEND_STARTUP_HINTS: &[&str] =
    &[crate::backend_profile::CODEX_UPDATE_MENU_LITERAL];

fn is_rearm_pre_idle_backend_startup_hint(literal_hint: &str) -> bool {
    REARM_PRE_IDLE_BACKEND_STARTUP_HINTS.contains(&literal_hint)
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

    /// t-…-82348-29 r2: is this a BACKEND-caused startup-modal hint
    /// ([`REARM_PRE_IDLE_BACKEND_STARTUP_HINTS`]) — the class the per-spawn
    /// one-shot governs? DERIVED from the two flags that define the class, not
    /// stored, so it cannot drift from them: pre-Idle re-arm eligible AND not
    /// routed through the dev-modal generation gate (the daemon-caused class
    /// carries its own per-generation one-shot inside that gate, so applying
    /// this flag to it would double-count and is deliberately excluded).
    fn is_backend_startup_hint(&self) -> bool {
        self.rearm_pre_idle && !self.dev_gated
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
                rearm_pre_idle: is_rearm_pre_idle_hint(&literal_hint)
                    || is_rearm_pre_idle_backend_startup_hint(&literal_hint),
                dev_gated: is_rearm_pre_idle_hint(&literal_hint),
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
    // A caller with no spawn-scoped flag gets a FRESH one-shot for this single
    // frame — the historical meaning of this seam, and the right default for a
    // one-frame verdict. The PTY read loop must NOT use it: its flag has to
    // live for the whole spawn (see `pty_read_loop`).
    let mut backend_startup_hint_spent = false;
    try_prepared_dismiss_dialog_once_per_spawn(
        name,
        screen,
        pty_writer,
        dismiss_patterns,
        scope,
        dev_gate,
        now,
        &mut backend_startup_hint_spent,
    )
}

/// As [`try_prepared_dismiss_dialog`], plus the t-…-82348-29 r2 (review F1)
/// per-spawn ONE-SHOT for backend-caused startup-modal hints.
///
/// `backend_startup_hint_spent` is owned by the PTY read loop, created once per
/// `pty_read_loop` invocation (i.e. per spawn) and NEVER reset within it. A
/// pattern in [`REARM_PRE_IDLE_BACKEND_STARTUP_HINTS`] is eligible in EVERY
/// scope — Startup as well as RearmPreIdle — only while the flag is false, and
/// the flag is set at DISPATCH: after the in-flight slot is claimed and before
/// the inline write or the detached worker runs. So exactly one keystroke per
/// spawn reaches the PTY, and a failed write forfeits the auto-skip for that
/// spawn rather than opening a retry loop (see the hint list's doc comment).
///
/// Everything else is untouched: `DISMISS_IN_FLIGHT` concurrency, the #3314
/// dev-modal gate (`enqueue_receipt` / Spent / write barrier / `still_valid`),
/// and the #3314 RearmSettled / #2474 settled-scope behaviour.
#[allow(clippy::too_many_arguments)]
pub fn try_prepared_dismiss_dialog_once_per_spawn(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[PreparedDismissPattern],
    scope: DismissScanScope,
    dev_gate: &mut DevModalGate,
    now: LogicalMs,
    backend_startup_hint_spent: &mut bool,
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
        // t-…-82348-29 r2 (review F1): a backend-caused startup modal is
        // answered AT MOST ONCE per spawn, in EVERY scope. Scope-independent
        // deliberately: the codex update menu holds the pane in
        // PermissionPrompt, so the startup latch never closes and the scope
        // stays `Startup` for as long as the modal is up — a RearmPreIdle-only
        // guard would bound nothing in exactly the situation it exists for.
        if pattern.is_backend_startup_hint() && *backend_startup_hint_spent {
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
            if pattern.dev_gated {
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
            // t-…-82348-29 r2 (review F1): spend the per-spawn one-shot HERE —
            // at dispatch, after the in-flight slot is claimed (a collision
            // returned above without dispatching, so it must not spend) and
            // before either write path runs. Never reset within this spawn.
            //
            // Deliberately NOT the dev-modal gate's spend-only-on-success rule:
            // that gate may safely retry because its recognition is bound to
            // facts the daemon OWNS (this generation's argv, the frame epoch).
            // This one is bound only to what is on the screen, so a retry after
            // a failed write is a retry on evidence that may already be stale —
            // forfeiting the auto-skip leaves the pane blocked and VISIBLE to
            // the watchdog, which is the safer failure.
            if pattern.is_backend_startup_hint() {
                *backend_startup_hint_spent = true;
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
            let barrier = pattern.dev_gated.then(|| dev_gate.write_barrier());
            let enqueue_receipt = pattern.dev_gated.then(|| dev_gate.enqueue_receipt());
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
            // fire-and-forget: dialog-dismiss keystroke writer is bounded by the
            // startup eligibility window. It normally waits 300ms; complete
            // repaints restart that stability wait inside this SAME worker. H2:
            // in-flight slot freed by InFlightGuard on any exit (incl. panic),
            // armed at thread entry below.
            if std::thread::Builder::new()
                .name("dismiss-dialog".into())
                .spawn(move || {
                    // #1886 follow-up: arm the in-flight removal as a Drop guard at
                    // thread entry so a panic / early-return still frees the slot.
                    let _guard = InFlightGuard(worker_flight_key);
                    let stable_for = std::time::Duration::from_millis(
                        crate::agent::dev_modal::MIN_STABLE_MS,
                    );
                    if let Some(barrier) = barrier.as_ref() {
                        if !barrier.wait_until_stable(
                            stable_for,
                            std::time::Duration::from_millis(
                                crate::agent::dev_modal::ELIGIBILITY_EXPIRY_MS
                                    .saturating_sub(now.0),
                            ),
                        ) {
                            tracing::debug!(
                                agent = %agent,
                                "#3314: startup-modal stability wait cancelled"
                            );
                            return;
                        }
                    } else {
                        std::thread::sleep(stable_for);
                    }
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
mod tests;
