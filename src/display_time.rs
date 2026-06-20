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

use std::collections::HashSet;
use std::sync::Mutex;

/// Convert an RFC 3339 UTC timestamp into a short display string of
/// the form `MM-DD HH:MM` in the configured display timezone.
///
/// - `rfc3339`: UTC ISO 8601 string (e.g. from storage)
/// - `tz`: IANA timezone name (e.g. `Asia/Taipei`); `None` → system tz
///
/// Falls back to the first-10-char slice on parse-error of the input
/// timestamp so legacy / mid-migration fixtures render the same as
/// before. Invalid IANA tz strings warn once per process per name and
/// fall back to `chrono::Local` so a typoed `display_timezone` in
/// fleet.yaml degrades to the system tz rather than panicking.
pub fn format_local_short(rfc3339: &str, tz: Option<&str>) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.chars().take(10).collect();
    };
    render_in_tz(parsed, tz, "%m-%d %H:%M")
}

/// #1487: format a UTC instant in the operator display timezone as a
/// SPACE-FREE RFC 3339 string with year + offset, e.g.
/// `2026-05-30T16:12:34+08:00`.
///
/// Unlike [`format_local_short`] (`MM-DD HH:MM` — spaced, no year, no
/// offset), this shape is safe to embed as a `now=` field inside the
/// space-delimited `[AGEND-MSG]` header (agents split fields on spaces),
/// and it carries the absolute instant plus zone so an agent can reason
/// about the operator's wall-clock time unambiguously.
///
/// `tz` semantics match `format_local_short`: `None` → `chrono::Local`
/// (system tz); invalid IANA → warn-once + `chrono::Local` fallback.
pub fn format_iso_offset(now: chrono::DateTime<chrono::Utc>, tz: Option<&str>) -> String {
    const FMT: &str = "%Y-%m-%dT%H:%M:%S%:z";
    render_in_tz(now, tz, FMT)
}

/// #2050 simplify PR-E (⑨): the shared timezone-render branch behind
/// [`format_local_short`] and [`format_iso_offset`] — resolve the IANA `tz`
/// (warn-once + `chrono::Local` fallback on an invalid name; `None` → `Local`)
/// and format `dt` with `fmt`. Byte-identical to the former inline matches; the
/// warn-once + fallback ordering is preserved verbatim.
fn render_in_tz<Tz: chrono::TimeZone>(
    dt: chrono::DateTime<Tz>,
    tz: Option<&str>,
    fmt: &str,
) -> String {
    match tz {
        Some(iana) => match iana.parse::<chrono_tz::Tz>() {
            Ok(tz) => dt.with_timezone(&tz).format(fmt).to_string(),
            Err(_) => {
                warn_invalid_iana_once(iana);
                dt.with_timezone(&chrono::Local).format(fmt).to_string()
            }
        },
        None => dt.with_timezone(&chrono::Local).format(fmt).to_string(),
    }
}

/// One-shot warn per invalid IANA name to surface fleet.yaml typos
/// without spamming logs on every render frame.
fn warn_invalid_iana_once(iana: &str) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut guard = match SEEN.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let set = guard.get_or_insert_with(HashSet::new);
    if set.insert(iana.to_string()) {
        tracing::warn!(
            iana = %iana,
            "display_timezone: invalid IANA name, falling back to system tz"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{format_iso_offset, format_local_short};

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
            .expect("known-good RFC 3339 fixture")
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string();
        assert_eq!(out_none, expected, "None tz must match chrono::Local");
    }

    // ─── #1487 format_iso_offset (now= header field) ───────────────────

    /// `Asia/Taipei` converts UTC → UTC+8 and renders the exact
    /// space-free RFC 3339 + offset string the `now=` header field needs.
    #[test]
    fn format_iso_offset_asia_taipei_is_utc_plus_8_space_free() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T08:12:34Z")
            .expect("known-good RFC 3339 fixture")
            .with_timezone(&chrono::Utc);
        let out = format_iso_offset(now, Some("Asia/Taipei"));
        assert_eq!(out, "2026-05-30T16:12:34+08:00");
        assert!(
            !out.contains(' '),
            "now= value must be space-free for header field-splitting, got {out:?}"
        );
    }

    /// Invalid IANA name falls back to `chrono::Local` (never panics) and
    /// stays space-free regardless of the system tz.
    #[test]
    fn format_iso_offset_invalid_iana_falls_back_and_stays_space_free() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-30T08:12:34Z")
            .expect("known-good RFC 3339 fixture")
            .with_timezone(&chrono::Utc);
        let out = format_iso_offset(now, Some("Not/A_Real_TZ"));
        let local = format_iso_offset(now, None);
        assert_eq!(out, local, "invalid IANA must match chrono::Local (None)");
        assert!(
            !out.contains(' '),
            "fallback must stay space-free, got {out:?}"
        );
    }
}
