use super::InjectTarget;

const READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(15);
const POSTSUBMIT_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
/// Bottom rows scanned for the input area — the prompt + a few wrapped input rows.
const READBACK_TAIL_ROWS: usize = 8;

/// #1912: tail-sentinel of a (possibly multi-line) injected payload — the last
/// run of up to `MAX` chars on the final non-empty line, the line the submit `\r`
/// commits. Short + drawn from the bottom line so it stays robust to input-box
/// wrapping. Empty when the payload has no non-blank line (nothing to confirm).
pub(super) fn inject_sentinel(stripped: &str) -> String {
    const MAX: usize = 24;
    let last_line = stripped
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let take_from = last_line
        .char_indices()
        .rev()
        .take(MAX)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    last_line[take_from..]
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect()
}

/// #1912/#3175: poll until the typed line's tail sentinel is a suffix of the
/// visible text ending exactly at the cursor. The VTerm matcher walks backwards
/// only over proven `WRAPLINE` boundaries, so an older row, scrollback, or text
/// after the cursor cannot authorize submit. Returns `false` on timeout; the
/// caller leaves the first write untouched and never retries or submits it.
/// Acquires the core lock only briefly per poll so `pty_read_loop` can render the
/// backend's echo between polls.
pub(super) fn readback_confirm_typed(target: &InjectTarget, stripped: &str) -> bool {
    readback_confirm_typed_with(target, stripped, READBACK_TIMEOUT, READBACK_POLL)
}

pub(super) fn readback_confirm_typed_with(
    target: &InjectTarget,
    stripped: &str,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let sentinel = inject_sentinel(stripped);
    if sentinel.is_empty() {
        return false;
    }
    let max_rows = unicode_width::UnicodeWidthStr::width(sentinel.as_str()).max(1);
    let start = std::time::Instant::now();
    let mut polls = 0u32;
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let confirmed = target
            .core
            .lock()
            .vterm
            .cursor_anchored_suffix_matches(&sentinel, max_rows);
        if confirmed {
            tracing::debug!(
                tag = "#1912-readback-confirmed",
                polls,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "typed inject line rendered in input area before submit"
            );
            return true;
        }
        if start.elapsed() >= timeout {
            tracing::warn!(
                tag = "#1912-readback-timeout",
                elapsed_ms = start.elapsed().as_millis() as u64,
                sentinel_len = sentinel.len(),
                "typed inject line not confirmed in input area within timeout"
            );
            return false;
        }
        std::thread::sleep(poll);
        polls += 1;
    }
}

/// #1912: post-submit observability (no retry). A successful submit clears the
/// input line / grows the transcript, so the rendered tail CHANGES; returns `true`
/// the moment it does. If it stays byte-identical for `POSTSUBMIT_WINDOW`, the
/// submit likely didn't take — warn (log/metric only) and return `false`. NEVER
/// retries the submit: a second `\r` would risk double-submit.
pub(super) fn observe_post_submit(target: &InjectTarget) -> bool {
    observe_post_submit_with(target, POSTSUBMIT_WINDOW, READBACK_POLL)
}

pub(super) fn observe_post_submit_with(
    target: &InjectTarget,
    window: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let before = target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS);
    let start = std::time::Instant::now();
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(poll);
        if target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS) != before {
            return true;
        }
        if start.elapsed() >= window {
            tracing::warn!(
                tag = "#1912-postsubmit-nochange",
                "input area unchanged after submit — a readback-confirmed line may not have submitted"
            );
            return false;
        }
    }
}
