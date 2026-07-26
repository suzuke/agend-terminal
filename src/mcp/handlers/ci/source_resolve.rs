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

/// Resolve `source` to `(source_path, source_canonical)` — the path the caller
/// uses as the git cwd (also echoed in the `source` response field) and the
/// canonicalized validated `PathBuf` — or return a ready-to-emit error `Value`.
/// Behaviour-preserving for the two legitimate arms (absolute/`~`, known agent
/// name) + the canonicalize / system-dir guards; the fail-closed reject of a
/// non-absolute agent-name miss (#2158) is unchanged.
///
/// `d-20260726055029475978-81`: when the resolved path names a working tree, both
/// halves are normalized to the OWNING canonical repository root, so a linked
/// worktree of R and a subdirectory of R name R exactly as R itself does. This is
/// the sole source admission point of `handle_checkout_repo`, so the normalized
/// root is what every downstream identity keys on (target mangling, git cwd,
/// lease key, marker/binding, journal, rollback, reuse).
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
        // daemon cwd. #2454 first slice: resolve this in-process from fleet.yaml
        // instead of loopbacking through the API LIST endpoint just to read the same
        // resolved instance working directory.
        match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|fleet| {
                fleet
                    .resolve_instance(source)
                    .and_then(|inst| inst.working_directory)
            }) {
            Some(working_dir) => working_dir.display().to_string(),
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
    // Git repository identity, not filesystem identity: `canonicalize` resolves
    // symlinks but keeps a linked worktree as itself, so the same (repo, branch)
    // would split across spellings of the path. A path that is not a working tree
    // is left exactly as before — this normalizes, it rejects nothing new.
    match canonical_repo_root(&source_canonical) {
        Some(root) => Ok((root.display().to_string(), root)),
        None => Ok((source_path, source_canonical)),
    }
}

/// The canonical root of the repository that OWNS `path` — R for R itself, for a
/// subdirectory of R, and for a linked worktree of R. `None` when `path` is not a
/// non-bare working tree, so the caller keeps the plain filesystem path.
fn canonical_repo_root(path: &Path) -> Option<PathBuf> {
    let common_dir = repo_common_dir(path)?;
    // `<repo>/.git` ⇒ the owning repo root. A `--separate-git-dir` common dir has
    // an unrelated parent, so only normalize when the derived root verifiably owns
    // the SAME repository (fail-safe: otherwise leave the path untouched).
    let root = common_dir.parent()?;
    (repo_common_dir(root)? == common_dir).then(|| root.to_path_buf())
}

/// Canonical `--git-common-dir` of the non-bare working tree at `path`. Git's own
/// resolution is the authority (never lexical gitlink arithmetic — cf. the
/// canonical-identity rule in `ci/release.rs`); `--path-format=absolute` makes the
/// answer directly canonicalizable however the gitlink is spelled.
fn repo_common_dir(path: &Path) -> Option<PathBuf> {
    let out = crate::git_helpers::git_cmd(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--is-bare-repository",
        ],
    )
    .ok()?;
    match out.lines().map(str::trim).collect::<Vec<_>>().as_slice() {
        // A bare repo has no working tree to admit, and its parent directory is
        // not a repo root — leave it to the existing downstream guards.
        [common_dir, "false"] => std::fs::canonicalize(common_dir).ok(),
        _ => None,
    }
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
    ///
    /// `#[cfg(unix)]`: the absolute arm is `/`-prefixed (Unix semantics); a Windows
    /// drive path (`C:`-rooted) is not `/`-absolute, so the helper routes it through
    /// the agent-name arm — the `/`-absolute path is a Unix-only contract. The input
    /// is canonicalized FIRST so symlink resolution on CI (e.g. macOS `/var` →
    /// `/private/var`) can't skew a literal-string comparison (cf. #2226/#2231
    /// cross-platform test fragility — compare canonicalized forms, not raw strings).
    #[cfg(unix)]
    #[test]
    fn absolute_existing_path_still_resolves_2158() {
        let abs = std::env::temp_dir()
            .canonicalize()
            .expect("temp dir canonicalizes");
        let abs_str = abs.display().to_string();
        let (src_path, canonical) = resolve_checkout_source_path(&abs, &abs_str)
            .expect("absolute existing path must still resolve (legit arm preserved)");
        assert!(canonical.is_absolute());
        // Both sides are already canonical → no /var-vs-/private/var skew.
        assert_eq!(canonical, abs, "canonical resolves to the same real dir");
        assert_eq!(
            src_path, abs_str,
            "pre-canonical source string preserved for the caller"
        );
    }

    /// #2454 first slice: agent-name source resolution no longer needs an MCP-to-API
    /// socket loopback. fleet.yaml already owns the instance working_directory.
    #[test]
    fn agent_name_source_resolves_from_fleet_yaml_without_api_loopback_2454() {
        let home =
            std::env::temp_dir().join(format!("agend-source-resolve-agent-{}", std::process::id()));
        let work = home.join("workspace").join("dev");
        std::fs::create_dir_all(&work).expect("mkdir work");
        std::fs::write(
            home.join("fleet.yaml"),
            format!(
                "instances:\n  dev:\n    backend: claude\n    working_directory: {}\n",
                work.display()
            ),
        )
        .expect("write fleet");

        let (source_path, canonical) =
            resolve_checkout_source_path(&home, "dev").expect("known agent name resolves");
        assert_eq!(source_path, work.display().to_string());
        assert_eq!(canonical, work.canonicalize().expect("canonical work"));

        std::fs::remove_dir_all(&home).ok();
    }

    /// Pin the architectural intent for #2454: this helper must not grow a new API socket
    /// dependency while reducing MCP-to-API loopbacks incrementally.
    #[test]
    fn source_resolve_has_no_api_call_loopback_2454() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/mcp/handlers/ci/source_resolve.rs");
        let body = std::fs::read_to_string(path).expect("read source_resolve.rs");
        let production_body = body
            .split("#[cfg(test)]")
            .next()
            .expect("source has production body before tests");
        assert!(
            !production_body.contains("api::call"),
            "source_resolve.rs must stay off MCP→API socket loopbacks"
        );
    }
}
