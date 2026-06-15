//! Repro test (panic_io_extra scope) for: `mask_token` byte-slices behind a
//! byte-length guard, so a multibyte token panics.
//!
//! `mask_token` guards on `tok.len() > 8` (BYTE length), then forms
//! `&tok[..4]` and `&tok[tok.len() - 4..]` (BYTE slices). A token whose first or
//! last 4 bytes split a multibyte UTF-8 char panics. Reached during interactive
//! quickstart when masking the `.env` bot token for display.

#![allow(clippy::unwrap_used)]

use super::mask_token;

/// `\u{4e2d}\u{6587}\u{4e2d}` is three 3-byte CJK chars = 9 bytes, so the
/// `tok.len() > 8` guard passes, but `&tok[..4]` splits the second char ->
/// panic "byte index 4 is not a char boundary".
///
/// RED now: `catch_unwind` returns `Err`. GREEN after fix: char-boundary-aware
/// truncation never panics and preserves the masked "..." shape.
#[test]
fn mask_token_multibyte_does_not_panic_panic_io_extra() {
    let tok = "\u{4e2d}\u{6587}\u{4e2d}";
    assert!(
        tok.len() > 8,
        "precondition: byte length passes the >8 guard"
    );

    let result = std::panic::catch_unwind(|| mask_token(tok));

    let masked = result.expect(
        "mask_token must not panic on a multibyte token: it byte-slices behind a byte-length \
         guard, so &tok[..4] / &tok[len-4..] can split a multibyte char",
    );

    // Once it no longer panics, a token long enough to mask should still produce
    // the elided "..." form rather than the "****" short-token fallback.
    assert!(
        masked.contains("..."),
        "masked long token should keep the elided form, got: {masked:?}"
    );
}
