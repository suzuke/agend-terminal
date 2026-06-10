//! Shared daemon utilities — extracted from legacy_backfill and task_sweep
//! to eliminate duplication (G3 M2).

use sha2::{Digest, Sha256};

/// #1870-H3: wall-clock elapsed `now − ts`, clamped to a non-negative `Duration`.
///
/// `DateTime::signed_duration_since` goes NEGATIVE when `ts` is in the future
/// relative to `now` — a backward clock skew (NTP correction, VM resume /
/// snapshot restore, DST). Every "elapsed > positive-threshold" watchdog then
/// reads not-yet-due forever and **silently wedges** until real time catches up
/// (the #N2 class — cron #1852, plus `anti_stall` + `handoff_timeout`). Clamping
/// to zero treats a future timestamp as "just started", so the watchdog simply
/// re-evaluates as the clock advances instead of stalling.
///
/// A NO-OP for the normal `ts ≤ now` case: the elapsed is byte-identical to the
/// raw `signed_duration_since`. Route every daemon "elapsed vs positive
/// threshold" read through this so a single clamp covers the whole class.
pub(crate) fn elapsed_since(
    now: chrono::DateTime<chrono::Utc>,
    ts: chrono::DateTime<chrono::Utc>,
) -> chrono::Duration {
    now.signed_duration_since(ts).max(chrono::Duration::zero())
}

/// Strip HTML comments (`<!-- ... -->`) from a string.
pub fn strip_html_comments(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                // Unterminated comment — drop the tail (security: attacker
                // can't sneak directives past us via partial comments)
                return result;
            }
        }
    }
    result.push_str(rest);
    result
}

/// SHA-256 hex digest of arbitrary bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    // #1934 (digest 0.11): the output array no longer implements LowerHex —
    // hex::encode produces the identical lowercase hex (known-answer pinned).
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_comments_removes_comments() {
        assert_eq!(
            strip_html_comments("before<!-- hidden -->after"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_html_comments_no_comments() {
        assert_eq!(strip_html_comments("plain text"), "plain text");
    }

    /// #1934 cross-version pin: known-answer test (FIPS 180-4 vector via
    /// python hashlib) — the digest must be byte-identical across the
    /// sha2 0.10→0.11 upgrade.
    #[test]
    fn sha256_hex_known_answer_1934() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_hex_deterministic() {
        let h1 = sha256_hex(b"hello");
        let h2 = sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    /// §3.9 #1870-H3: the clamp is a NO-OP for `ts ≤ now` (byte-identical to the
    /// raw `signed_duration_since`) and clamps a FUTURE `ts` (backward clock skew)
    /// to zero instead of a negative `Duration`. This is the one place the clamp
    /// is observable — the downstream `> threshold` comparisons treat negative and
    /// zero the same, so the helper test is the regression-proof anchor.
    #[test]
    fn elapsed_since_clamps_future_to_zero_noop_for_past_1870_h3() {
        let now = chrono::Utc::now();

        // Past `ts` → byte-identical to the raw signed_duration_since (no-op).
        let past = now - chrono::Duration::seconds(123);
        assert_eq!(
            elapsed_since(now, past),
            now.signed_duration_since(past),
            "#1870-H3: a past ts must be byte-identical (clamp is a no-op)"
        );
        assert_eq!(elapsed_since(now, past).num_seconds(), 123);

        // `ts == now` → zero.
        assert_eq!(elapsed_since(now, now), chrono::Duration::zero());

        // Future `ts` (backward clock skew) → clamped to zero, NOT negative.
        let future = now + chrono::Duration::seconds(456);
        assert_eq!(
            elapsed_since(now, future),
            chrono::Duration::zero(),
            "#1870-H3: a future ts must clamp to 0 (not a negative wedge)"
        );
        assert!(
            now.signed_duration_since(future).num_seconds() < 0,
            "sanity: the raw value the clamp guards against is indeed negative"
        );
    }
}
