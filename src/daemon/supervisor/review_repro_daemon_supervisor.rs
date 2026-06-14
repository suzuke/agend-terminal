//! Review-repro tests (scope: daemon-supervisor).
//!
//! In-module submodule attached to `src/daemon/supervisor.rs` so the PRIVATE
//! `parse_unlock_at` is reachable via `super::parse_unlock_at` (the existing
//! `mod tests` already exercises it the same way).
//!
//! FINDING (high / error-handling): `parse_unlock_at` panics on non-ASCII pane
//! content. `idx` is the byte offset of the first ASCII digit found in `lower`
//! (the lowercased COPY), but it is then used to slice the ORIGINAL `line`
//! (`&line[idx..]`). `str::to_lowercase()` does NOT preserve byte length — the
//! Kelvin sign U+212A 'K' (3 bytes) lowercases to ASCII 'k' (1 byte) — so `idx`
//! derived from `lower` can land mid-multibyte-char in `line` and panic
//! ("byte index N is not a char boundary"). Separately, `rest[..5]` slices at a
//! fixed byte index 5 that can straddle a multibyte char.
//!
//! `pane_tail` is raw, fully content-controlled PTY output (the supervisor passes
//! `core.vterm.tail_lines(10)` into `parse_unlock_at`). The per-tick
//! `catch_unwind` swallows the panic but aborts the ENTIRE tick every cycle the
//! offending content is displayed.
//!
//! METHOD: behavioral_unit / panic-vector. We call the real private fn through
//! `super::` with the triggering input wrapped in `std::panic::catch_unwind` and
//! assert it returns `Ok` (it returns `Err` NOW → red). Baseline correct cases
//! are asserted directly (any reasonable fix preserves them). We deliberately do
//! NOT pin the returned value for the adversarial inputs, since the exact result
//! of a char-aware fix is implementation-dependent (the fix-agnostic contract is
//! simply: never panic on content-controlled bytes).

#[test]
fn parse_unlock_at_does_not_panic_on_nonascii_pane_content_daemon_supervisor() {
    // Silence the default panic hook so the (expected, caught) panics don't
    // spam the test output; restore it after.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    // Vector 1: cross-string byte-offset slice. `line` = "limit Ké 5:14"
    // (U+212A Kelvin sign + é). `lower` = "limit ké 5:14" is 2 bytes SHORTER,
    // so the digit offset (idx=10) found in `lower` is NOT a char boundary in
    // the original `line` (16 bytes) → `&line[idx..]` panics today.
    let v1 = "limit \u{212A}\u{e9} 5:14";
    let r1 = std::panic::catch_unwind(|| super::parse_unlock_at(v1));

    // Vector 2: fixed `rest[..5]` straddling a multibyte char. `line` =
    // "limit 15:1界 reset"; idx lands on the '1' of "15", rest = "15:1界 reset",
    // rest.as_bytes()[2]==b':' is satisfied, and `rest[..5]` cuts into the
    // 3-byte '界' → panic today.
    let v2 = "limit 15:1\u{754c} reset";
    let r2 = std::panic::catch_unwind(|| super::parse_unlock_at(v2));

    std::panic::set_hook(prev);

    assert!(
        r1.is_ok(),
        "parse_unlock_at panicked on cross-string byte-offset slice \
         (U+212A lowercases 3→1 byte; idx from `lower` is not a char boundary in \
         `line`). Operate on a single string (slice `lower`, not `line`)."
    );
    assert!(
        r2.is_ok(),
        "parse_unlock_at panicked on `rest[..5]` straddling a multibyte char. \
         Replace fixed byte indexing with char-aware extraction."
    );
}

#[test]
fn parse_unlock_at_still_extracts_ascii_time_after_fix_daemon_supervisor() {
    // Correct-behavior baselines that ANY valid fix must preserve. These pass
    // today AND after the fix; paired with the panic-vector test above so a fix
    // that merely stops panicking but breaks extraction is also caught.
    assert_eq!(
        super::parse_unlock_at("Usage limit reached. Resets at 15:14 UTC"),
        Some("15:14".to_string()),
        "must still extract HH:MM from a plain-ASCII usage-limit line"
    );
    assert_eq!(
        super::parse_unlock_at("no time here"),
        None,
        "must still return None when no HH:MM is present"
    );
}
