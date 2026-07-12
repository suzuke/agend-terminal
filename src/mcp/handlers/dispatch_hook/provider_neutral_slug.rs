//! Provider-neutral `owner/repo` derivation for the binding_state
//! `current_binding` projection (codex #2746 / S2 r2).
//!
//! A CI watch stores a HOST-INDEPENDENT `owner/repo` slug: the public `ci`
//! schema accepts a bare `repository=owner/repo` (provider-blind) plus
//! `ci_provider=bitbucket_cloud`, so a Bitbucket-Cloud watch on a non-protected
//! branch is reachable (only `bitbucket_server` is rejected; the
//! `provider_kind=="github"` gate is exact-head-PROTECTED-only). The GitHub-only
//! [`super::canonicalize_repo_slug`] returns `None` for a Bitbucket/GitLab
//! origin, which regressed `current_binding` to false for a non-GitHub-origin
//! binding whose OWN watch it should have matched.
//!
//! This strips the known forge hosts (GitHub + Bitbucket; GitLab included as
//! characterization — the public schema does not yet advertise it) and applies
//! the SAME owner/repo parse + lowercase + `.git`/trailing-slash trim as
//! `canonicalize_repo_slug`, so for a `github.com` URL the output is BYTE-
//! IDENTICAL. Watch-STORAGE canonicalization is unchanged — this only affects
//! how the current-binding identity is derived from the binding origin.
//!
//! LOCKSTEP INVARIANT: this MUST produce the same canonical form that watch
//! creation stores in `watch.repo` (see `ci::mod::resolve_repo_or_error`). If
//! the subscribe path's repo canonicalization ever changes (e.g. a future
//! non-GitHub provider gains its own normalization), THIS derivation must change
//! in step or `current_binding` silently drifts back to false.

use std::path::Path;

/// Forge hosts whose remote-URL prefixes are stripped to a bare `owner/repo`.
/// GitHub + Bitbucket are the providers the public `ci` schema can reach today;
/// GitLab is characterization (poller support exists but the schema does not
/// advertise it).
const KNOWN_FORGE_HOSTS: &[&str] = &["github.com", "bitbucket.org", "gitlab.com"];

/// Provider-neutral sibling of [`super::derive_repo_from_remote_pub`], used ONLY
/// by the binding_state `current_binding` projection. Runs
/// `git remote get-url origin` in `source_repo` and canonicalizes across the
/// known forge hosts. `None` on no origin / non-forge host / git failure.
pub(crate) fn derive_repo_slug_any_forge_pub(source_repo: &Path) -> Option<String> {
    let output =
        crate::git_helpers::git_bypass(source_repo, &["remote", "get-url", "origin"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    canonicalize_repo_slug_any_forge(&url)
}

/// Host-agnostic form of [`super::canonicalize_repo_slug`]: strips the known
/// forge hosts (in the https/http/ssh/scp remote-URL shapes) then applies the
/// identical owner/repo parse + `.git`/slash trim + lowercase. Byte-identical to
/// `canonicalize_repo_slug` for a `github.com` URL or a bare slug.
pub(crate) fn canonicalize_repo_slug_any_forge(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let stripped = strip_known_forge_host(s).unwrap_or(s);
    let slug = stripped.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

/// Strip the leading `scheme://host/` (or `git@host:` / `ssh://git@host/`) for a
/// KNOWN forge host, returning the `owner/repo(.git)` remainder. The trailing
/// `/` (or `:`) is part of every prefix, so a look-alike host such as
/// `github.com.evil.com` never matches `github.com` — boundary-safe.
fn strip_known_forge_host(s: &str) -> Option<&str> {
    for host in KNOWN_FORGE_HOSTS {
        for prefix in [
            format!("https://{host}/"),
            format!("http://{host}/"),
            format!("git@{host}:"),
            format!("ssh://git@{host}/"),
        ] {
            if let Some(rest) = s.strip_prefix(prefix.as_str()) {
                return Some(rest);
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::canonicalize_repo_slug_any_forge as canon;

    /// For GitHub URLs and bare slugs the any-forge canonicalizer must be
    /// byte-identical to the storage canonicalizer — GitHub behavior preserved.
    #[test]
    fn github_and_bare_are_byte_identical_to_storage_canonicalizer() {
        for url in [
            "https://github.com/Owner/Repo.git",
            "https://github.com/owner/repo",
            "http://github.com/owner/repo",
            "git@github.com:Owner/Repo.git",
            "ssh://git@github.com/owner/repo.git",
            "owner/repo",
            "Owner/Repo",
        ] {
            assert_eq!(
                canon(url),
                crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(url),
                "any-forge must match the GitHub storage canonicalizer for {url:?}"
            );
        }
    }

    #[test]
    fn bitbucket_https_and_ssh_resolve() {
        assert_eq!(
            canon("https://bitbucket.org/o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(canon("https://bitbucket.org/O/R"), Some("o/r".to_string()));
        assert_eq!(canon("git@bitbucket.org:o/r.git"), Some("o/r".to_string()));
        assert_eq!(
            canon("ssh://git@bitbucket.org/o/r.git"),
            Some("o/r".to_string())
        );
    }

    #[test]
    fn gitlab_characterization() {
        // Not advertised by the public ci schema; covered so a future enablement
        // has a defined baseline rather than a silent None.
        assert_eq!(canon("https://gitlab.com/o/r.git"), Some("o/r".to_string()));
        assert_eq!(canon("git@gitlab.com:o/r.git"), Some("o/r".to_string()));
    }

    #[test]
    fn unknown_host_and_lookalike_and_malformed_are_none() {
        // Unknown host is not stripped → the scheme/host inflates the path parts.
        assert_eq!(canon("https://example.com/o/r"), None);
        // Boundary: a look-alike host must NOT be treated as GitHub.
        assert_eq!(canon("https://github.com.evil.com/o/r"), None);
        assert_eq!(canon("single"), None);
        assert_eq!(canon("a/b/c"), None);
        assert_eq!(canon(""), None);
        assert_eq!(canon("   "), None);
    }

    /// r3 (codex #2746): unknown/look-alike SCP + SSH transports must be `None`.
    /// r2 did NOT strip these, and a `host:owner/repo` SCP form splits into exactly
    /// two slash-parts → was wrongly ACCEPTED as the bogus slug `host:owner/repo`
    /// (the false boundary-safety claim: earlier tests only exercised HTTPS shapes,
    /// which split into >2 parts and were already `None`).
    #[test]
    fn unknown_scp_and_ssh_transports_rejected() {
        // SCP form for an unknown host (the r2 gap): git@host:owner/repo.
        assert_eq!(canon("git@example.com:o/r"), None);
        // SCP look-alike host — github.com.evil.com is NOT github.com.
        assert_eq!(canon("git@github.com.evil.com:o/r"), None);
        // SCP without a user (host:path) is still an unknown-host transport.
        assert_eq!(canon("example.com:o/r"), None);
        // SSH URL + HTTPS look-alike (already >2 parts, kept for the boundary set).
        assert_eq!(canon("ssh://git@example.com/o/r"), None);
        assert_eq!(canon("https://github.com.evil.com/o/r"), None);
    }
}
