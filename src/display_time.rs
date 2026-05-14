//! Display-side timestamp rendering for human-facing surfaces.
//!
//! Storage stays UTC ISO 8601 unconditionally (see binding.json,
//! decisions/*.json, event_log.jsonl). This module exists to convert
//! those UTC strings into operator-local display form on the way out
//! to TUI overlays and notification bodies, without touching the
//! storage layer.
//!
//! The `tz: Option<&str>` parameter takes an IANA timezone name (e.g.
//! `Asia/Taipei`). `None` falls back to `chrono::Local` (system tz),
//! which preserves the pre-#790 behaviour from Sprint 54 P2-6.
//! Invalid IANA strings warn once and fall back to `chrono::Local`.

/// Convert an RFC 3339 UTC timestamp into a short display string of
/// the form `MM-DD HH:MM` in the configured display timezone.
///
/// - `rfc3339`: UTC ISO 8601 string (e.g. from storage)
/// - `tz`: IANA timezone name (e.g. `Asia/Taipei`); `None` → system tz
///
/// Falls back to the first-10-char slice on parse error so legacy /
/// mid-migration fixtures render the same as before.
pub fn format_local_short(rfc3339: &str, _tz: Option<&str>) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.chars().take(10).collect())
}

#[cfg(test)]
mod tests {
    use super::format_local_short;

    /// `Z` form parses → render-local conversion → `MM-DD HH:MM` shape
    /// (5 chars + space + 5 chars = 11 chars). Shape-only assertion so
    /// the test passes on every CI runner regardless of TZ.
    #[test]
    fn format_local_short_shapes_rfc3339_z_to_md_hm() {
        let out = format_local_short("2026-05-07T22:00:00Z", None);
        assert_eq!(out.len(), 11, "expected MM-DD HH:MM shape, got {out:?}");
        let bytes = out.as_bytes();
        assert!(bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit());
        assert_eq!(bytes[2], b'-');
        assert!(bytes[3].is_ascii_digit() && bytes[4].is_ascii_digit());
        assert_eq!(bytes[5], b' ');
        assert!(bytes[6].is_ascii_digit() && bytes[7].is_ascii_digit());
        assert_eq!(bytes[8], b':');
        assert!(bytes[9].is_ascii_digit() && bytes[10].is_ascii_digit());
    }

    /// Explicit `+00:00` offset form yields the same shaped output as
    /// the `Z` form — proves the parser accepts both RFC 3339 variants.
    #[test]
    fn format_local_short_explicit_offset_matches_z_form() {
        let z_form = format_local_short("2026-05-07T22:00:00Z", None);
        let off_form = format_local_short("2026-05-07T22:00:00+00:00", None);
        assert_eq!(
            z_form, off_form,
            "Z and +00:00 RFC 3339 forms must produce identical local renders"
        );
    }

    /// Garbage input falls back to the first-10-char slice — preserves
    /// the pre-fix behaviour exactly so legacy fixtures / mid-migration
    /// data render the same as before, just without TZ adjustment.
    #[test]
    fn format_local_short_falls_back_on_parse_error() {
        assert_eq!(format_local_short("garbage123456", None), "garbage123");
        assert_eq!(format_local_short("not-a-date", None), "not-a-date");
        assert_eq!(format_local_short("short", None), "short");
    }

    // ─── #790 anchor tests (§3.10 RED) ─────────────────────────────────

    /// Explicit `Asia/Taipei` IANA tz converts UTC → UTC+8 — May 7
    /// 22:00 UTC is May 8 06:00 Taipei. Pinned exact output so the
    /// helper's chrono-tz path is verifiably-correct, not just
    /// shape-conforming. RED until the GREEN commit wires `chrono_tz::Tz`.
    #[test]
    fn format_local_short_with_asia_taipei_renders_utc_plus_8() {
        let out = format_local_short("2026-05-07T22:00:00Z", Some("Asia/Taipei"));
        assert_eq!(out, "05-08 06:00", "Asia/Taipei should yield UTC+8 render");
    }

    /// Invalid IANA name falls back to `chrono::Local` so the helper
    /// never panics on hand-edited fleet.yaml typos. The output shape
    /// is `MM-DD HH:MM` (11 chars) — same shape as the `None` path so
    /// the operator gets a render, just with the wrong tz, plus a
    /// (one-shot) warn log to surface the misconfiguration.
    #[test]
    fn format_local_short_with_invalid_iana_falls_back_to_chrono_local() {
        let out = format_local_short("2026-05-07T22:00:00Z", Some("Not/A_Real_TZ"));
        let local_out = format_local_short("2026-05-07T22:00:00Z", None);
        assert_eq!(
            out, local_out,
            "invalid IANA must fall back to chrono::Local, identical to None"
        );
    }

    /// `tz = None` preserves Sprint 54 P2-6 behaviour exactly:
    /// chrono::Local render. Backward-compat anchor — if this regresses,
    /// every existing fleet.yaml without `display_timezone:` would
    /// silently change output format.
    #[test]
    fn format_local_short_with_none_tz_matches_chrono_local() {
        let out_none = format_local_short("2026-05-07T22:00:00Z", None);
        // Re-derive what chrono::Local should produce directly.
        let expected = chrono::DateTime::parse_from_rfc3339("2026-05-07T22:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert_eq!(out_none, expected, "None tz must match chrono::Local");
    }
}
