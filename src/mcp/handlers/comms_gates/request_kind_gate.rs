//! t-20260705005551919287-14440-22: `request_kind` server-side validation.
//!
//! An unrecognized `request_kind` (typo, wrong case, garbage value) used to
//! fall through `handle_unified_send`'s dispatch `match`'s `_` arm exactly
//! like an OMITTED one — silently reinterpreted as a plain send instead of
//! rejected. Omitting the field is a valid, deliberate "plain message"
//! request; a present-but-unknown value is very likely a caller bug and
//! should be rejected loudly, not silently reinterpreted.
//!
//! reviewer4 (#2639 r0): `args["request_kind"].as_str()?` conflated ABSENT
//! (key missing — a deliberate plain send) with PRESENT-but-not-a-string
//! (`123`, `null` — a malformed caller bug) — both hit the `?` early-return
//! and passed validation silently. A malformed value is worse than a typo
//! and must be rejected, not downgraded.

use serde_json::{json, Value};

/// The only `request_kind` values `handle_unified_send`/`handle_broadcast`
/// dispatch on. Single source of truth both entry points validate against.
const VALID_REQUEST_KINDS: &[&str] = &["task", "report", "query", "update"];

/// Three-state: `None` when `request_kind` is ABSENT (deliberate plain send)
/// or a recognized string. `Some(error)` when it's PRESENT but not a string
/// (wrong type, `null`) or an unrecognized string — never silently
/// downgraded to a plain send.
pub(crate) fn validate_request_kind(args: &Value) -> Option<Value> {
    let rk_value = args.get("request_kind")?;
    let Some(rk) = rk_value.as_str() else {
        return Some(json!({
            "error": format!(
                "request_kind must be a string (got {rk_value}) — must be one of {VALID_REQUEST_KINDS:?}, or omitted for a plain message"
            )
        }));
    };
    if VALID_REQUEST_KINDS.contains(&rk) {
        return None;
    }
    Some(json!({
        "error": format!(
            "unknown request_kind '{rk}' — must be one of {VALID_REQUEST_KINDS:?}, or omitted for a plain message"
        )
    }))
}
