use crate::agent::AgentRegistry;
use std::path::Path;

use super::poller::check_ci_watches_with_provider;
use super::provider::{
    detect_provider_from_remote, BitbucketCiProvider, CiProvider, GitHubCiProvider,
    GitLabCiProvider,
};
use super::sweep::gc_stale_watches;

/// Check CI watch configs and inject failure logs to agents when CI fails.
pub fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
    // Sprint 57 Wave 2 Track B (#546 Item 1) — eager per-tick GC pass
    // BEFORE the poll loop. The lazy expiry inside the poll body still
    // runs (Sprint 53/54 era), but it can only see watches actively
    // being polled. This pass closes the "stale on disk after upstream
    // branch deletion" gap.
    let _ = gc_stale_watches(home, "eager_gc");
    check_ci_watches_with_provider(home, registry, |watch| {
        let ci_url = watch
            .get("ci_provider_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let repo = watch.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        // Explicit ci_provider wins; absent → auto-detect from repo URL.
        let (ci_type, default_url) = match watch.get("ci_provider").and_then(|v| v.as_str()) {
            Some(explicit) => (explicit, String::new()),
            None => {
                let (kind, is_custom) = detect_provider_from_remote(repo);
                if is_custom {
                    tracing::warn!(
                        repo,
                        kind,
                        "ci_watch: custom CI host pattern detected — suggest setting fleet.yaml ci_provider: explicitly"
                    );
                }
                let default = match kind {
                    "gitlab" => "https://gitlab.com",
                    "bitbucket_cloud" => "https://api.bitbucket.org",
                    _ => "https://api.github.com",
                };
                (kind, default.to_string())
            }
        };
        let url = ci_url.unwrap_or(default_url);
        match ci_type {
            "gitlab" => {
                let url = if url.is_empty() {
                    "https://gitlab.com".to_string()
                } else {
                    url
                };
                Some(Box::new(GitLabCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
            "bitbucket_cloud" => {
                let url = if url.is_empty() {
                    "https://api.bitbucket.org".to_string()
                } else {
                    url
                };
                Some(Box::new(BitbucketCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
            "bitbucket_server" => {
                tracing::error!(
                    "Bitbucket Server not yet supported — track Sprint 41+ candidate. \
                     Use bitbucket_cloud for Bitbucket Cloud repos."
                );
                None
            }
            _ => {
                let url = if url.is_empty() {
                    "https://api.github.com".to_string()
                } else {
                    url
                };
                Some(Box::new(GitHubCiProvider::with_base_url(url).ok()?) as Box<dyn CiProvider>)
            }
        }
    });
}
