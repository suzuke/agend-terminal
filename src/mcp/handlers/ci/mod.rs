use serde_json::{json, Value};
use std::path::Path;

// #2140: the deterministic merge-freshness gate lives in a sibling module so this
// file (at its LOC ceiling) gains only a single `merge_freshness::gate(...)` call.
mod merge_freshness;

// CR-2026-06-14: same LOC-relief pattern — `compute_next_poll_eta` lives in a
// sibling module; re-exported so callers (incl. the in-module repro) are unchanged.
mod poll_eta;
pub(crate) use poll_eta::compute_next_poll_eta;

// #2158 PR1: the security-sensitive checkout source-path resolution (absolute /
// agent-name only, fail-closed on miss + canonicalize + system-dir reject) lives
// in a sibling module — same LOC-relief pattern, and isolates the boundary fix.
mod source_resolve;

// #t-61: ci/mod.rs was split into per-action submodules (checkout / watch / merge
// / cleanup / release) to drop below the file-size ceiling. Pure mechanical move —
// the re-exports below preserve EVERY `ci::handle_*` path used by dispatch.rs and
// every `super::*` path used by the child `tests` module (zero caller/test edits).
mod checkout;
mod cleanup;
mod merge;
mod release;
mod watch;

pub(super) use checkout::handle_checkout_repo;
pub(super) use cleanup::handle_cleanup_init_commits;
pub(crate) use cleanup::handle_cleanup_merged_branches;
pub(super) use merge::handle_merge_repo;
pub(super) use release::handle_release_repo;
pub(crate) use watch::{handle_status_ci, handle_unwatch_ci, handle_watch_ci};
// Test-facing helpers — re-exported under cfg(test) so a non-test build carries no
// unused re-export (each is used WITHIN its own submodule in non-test builds).
#[cfg(test)]
pub(crate) use checkout::checkout_source;
#[cfg(test)]
pub(crate) use merge::{base_drift_refusal, classify_merge_summary, MergeVerdict};
#[cfg(test)]
pub(crate) use release::validate_release_path;

/// #1619: resolve the target `owner/repo` for a PR/CI handler.
///
/// Resolution order: explicit `repository` arg (canonicalized) → the
/// caller's `binding.json` `source_repo` origin remote → error. It
/// NEVER falls back to a hardcoded repo slug: a detection miss on
/// someone else's deployment must fail loud, not silently operate
/// (merge/checks/state) on the maintainer's repo.
///
/// Originally inline in `handle_watch_ci` (Sprint 55 P0-B); extracted so
/// `handle_merge_repo` shares the exact same resolution instead of the
/// old `.unwrap_or("suzuke/agend-terminal")` footgun. EC1: explicit
/// error when neither arg nor binding present (no silent cwd-derivation).
/// EC15: validate the binding's source_repo path still exists.
fn resolve_repo_or_error(home: &Path, instance_name: &str, args: &Value) -> Result<String, Value> {
    match args["repository"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => {
            // #942: canonicalize on entry so the hash key + stored
            // `repo` field both reflect the single canonical form.
            // Rejects obviously-malformed input (non-GitHub URL, malformed
            // slug) with operator-actionable error.
            match crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(r) {
                Some(c) => Ok(c),
                None => Err(json!({
                    "error": format!(
                        "invalid 'repository' format: {r:?} — expected `owner/repo` or full GitHub URL"
                    ),
                    "code": "invalid_repo_format",
                })),
            }
        }
        None => {
            let binding = home
                .join("runtime")
                .join(instance_name)
                .join("binding.json");
            let Ok(content) = std::fs::read_to_string(&binding) else {
                return Err(json!({
                    "error": "could not determine repo slug; pass explicit 'repository' arg or call bind_self first (no active binding)",
                    "code": "no_binding_no_repo"
                }));
            };
            let Ok(v) = serde_json::from_str::<Value>(&content) else {
                return Err(json!({
                    "error": "binding.json corrupt — re-bind or pass explicit 'repository'",
                    "code": "binding_corrupt"
                }));
            };
            let Some(src) = v["source_repo"].as_str().filter(|s| !s.is_empty()) else {
                return Err(json!({
                    "error": "binding has no source_repo — pass explicit 'repository' arg",
                    "code": "no_binding_no_repo"
                }));
            };
            let src_path = std::path::Path::new(src);
            if !src_path.exists() {
                return Err(json!({
                    "error": format!("binding source_repo '{src}' no longer exists — re-bind or pass explicit 'repository'"),
                    "code": "source_repo_path_deleted"
                }));
            }
            match crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(src_path) {
                Some(r) => Ok(r),
                None => Err(json!({
                    "error": format!("could not derive owner/repo from '{src}' origin remote — pass explicit 'repository' arg or set fleet.yaml `repo:` override"),
                    "code": "non_github_remote_no_override"
                })),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
