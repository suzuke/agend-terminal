//! t-20260705005551919287-14440-22: `request_kind` server-side validation.
//!
//! An unrecognized `request_kind` (typo, wrong case, garbage value) used to
//! fall through `handle_unified_send`'s dispatch `match`'s `_` arm exactly
//! like an OMITTED one — silently reinterpreted as a plain send instead of
//! rejected. Omitting the field is a valid, deliberate "plain message"
//! request; a present-but-unknown value is very likely a caller bug and
//! should be rejected loudly, not silently reinterpreted.

use serde_json::{json, Value};

/// The only `request_kind` values `handle_unified_send`/`handle_broadcast`
/// dispatch on. Single source of truth both entry points validate against.
const VALID_REQUEST_KINDS: &[&str] = &["task", "report", "query", "update"];

/// `None` when `request_kind` is absent (plain send) or one of the
/// recognized values. `Some(error)` otherwise.
pub(crate) fn validate_request_kind(args: &Value) -> Option<Value> {
    let rk = args["request_kind"].as_str()?;
    if VALID_REQUEST_KINDS.contains(&rk) {
        return None;
    }
    Some(json!({
        "error": format!(
            "unknown request_kind '{rk}' — must be one of {VALID_REQUEST_KINDS:?}, or omitted for a plain message"
        )
    }))
}
