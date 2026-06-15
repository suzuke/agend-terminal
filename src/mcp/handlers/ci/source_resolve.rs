//! #2158 PR1: resolve + validate the `repo checkout` source repository path,
//! fail-closed. Extracted from `ci/mod.rs::handle_checkout_repo_inner` (which sits
//! at its file_size ceiling) so the security boundary is isolated + reviewable.
//!
//! A `source` is valid ONLY as:
//!   1. an ABSOLUTE path (`/…`) or a `~`-relative home path, OR
//!   2. a known AGENT NAME, resolved to that agent's `working_directory`
//!      (the #1447 peer-workdir-by-name form).
//!
//! Pre-#2158, a non-absolute `source` that was NOT a known agent name fell back to
//! the literal string (`source.to_string()`), which the `canonicalize()` step then
//! resolved against the daemon's IMPLICIT cwd. So a bad/ambiguous value — the
//! literal `"undefined"` from the #2158 incident, or any relative string that
//! happens to exist under the daemon cwd — silently bound/wrote SOMEWHERE instead
//! of failing. This module rejects that miss fail-closed.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Resolve `source` to `(source_path, source_canonical)` — the resolved
/// pre-canonical string (used by the caller as the git cwd + echoed in the
/// `source` response field) and the canonicalized validated `PathBuf` — or return
/// a ready-to-emit error `Value`. Behaviour-preserving for the two legitimate arms
/// (absolute/`~`, known agent name) + the canonicalize / system-dir guards; the
/// ONLY new behaviour is the fail-closed reject of a non-absolute agent-name miss.
pub(super) fn resolve_checkout_source_path(
    home: &Path,
    source: &str,
) -> Result<(String, PathBuf), Value> {
    let source_path = if source.starts_with('/') || source.starts_with('~') {
        source
            .strip_prefix("~/")
            .map(|rest| format!("{}/{rest}", crate::user_home_dir().display()))
            .unwrap_or_else(|| source.to_string())
    } else {
        // Non-absolute → MUST be a known agent name. A miss is fail-closed (#2158):
        // never fall back to resolving `source` as a relative path against the
        // daemon cwd.
        match crate::api::call(home, &json!({"method": crate::api::method::LIST}))
            .ok()
            .and_then(|r| {
                r["result"]["agents"]
                    .as_array()?
                    .iter()
                    .find(|a| a["name"].as_str() == Some(source))
                    .and_then(|a| a["working_directory"].as_str().map(String::from))
            }) {
            Some(working_dir) => working_dir,
            None => {
                return Err(json!({
                    "error": format!(
                        "source '{source}' is neither an absolute path nor a known agent name — refusing to resolve a relative path against the daemon working directory (#2158)"
                    ),
                    "code": "ambiguous_source_path",
                }))
            }
        }
    };
    // H2: validate source_path — reject path traversal and system paths. The
    // contracted error strings are preserved byte-for-byte (any matcher/test).
    let source_canonical = match Path::new(&source_path).canonicalize() {
        Ok(p) => p,
        Err(e) => return Err(json!({"error": format!("invalid source path: {e}")})),
    };
    if source_canonical.starts_with("/etc")
        || source_canonical.starts_with("/usr")
        || source_canonical.starts_with("/sys")
        || source_canonical.starts_with("/proc")
    {
        return Err(json!({"error": "source path rejected: system directory"}));
    }
    Ok((source_path, source_canonical))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// #2158 PR1: a non-absolute `source` that is NOT a known agent name must FAIL
    /// CLOSED with the `ambiguous_source_path` code — never the pre-fix relative
    /// fallback (which canonicalized against the daemon cwd). RED pre-fix: the
    /// fallback returned Ok (relative-existing) or an `invalid source path`
    /// canonicalize error — neither carries this code.
    #[test]
    fn non_absolute_non_agent_source_is_rejected_fail_closed_2158() {
        let home = std::env::temp_dir();
        let err = resolve_checkout_source_path(&home, "undefined")
            .expect_err("non-absolute, non-agent source must be rejected");
        assert_eq!(
            err["code"].as_str(),
            Some("ambiguous_source_path"),
            "must fail-closed at resolution, not Ok / not a canonicalize error: {err}"
        );
    }

    /// The silent-resolve gap itself: a RELATIVE path string (the #2158 incident
    /// shape — a bad value treated as a relative dir). It must be rejected at the
    /// resolution stage, never resolved against the daemon cwd.
    #[test]
    fn relative_source_is_rejected_not_resolved_against_cwd_2158() {
        let home = std::env::temp_dir();
        let err = resolve_checkout_source_path(&home, "src")
            .expect_err("a relative source must NOT silently resolve against the cwd");
        assert_eq!(err["code"].as_str(), Some("ambiguous_source_path"), "{err}");
    }

    /// Legit arm preserved: an ABSOLUTE, existing, non-system path still resolves
    /// to (pre-canonical string, canonical PathBuf) — the fix only rejects the
    /// non-absolute agent-name MISS, not absolute callers.
    #[test]
    fn absolute_existing_path_still_resolves_2158() {
        let home = std::env::temp_dir();
        let abs = home.display().to_string();
        let (src_path, canonical) = resolve_checkout_source_path(&home, &abs)
            .expect("absolute existing path must still resolve (legit arm preserved)");
        assert!(canonical.is_absolute());
        assert_eq!(
            src_path, abs,
            "pre-canonical source string preserved for the caller"
        );
    }
}
