use crate::agent::AgentRegistry;
use std::path::Path;

use super::poller::check_ci_watches_with_provider;
use super::provider::{
    detect_provider_from_remote, BitbucketCiProvider, CiProvider, GitHubCiProvider,
    GitLabCiProvider,
};
use super::sweep::gc_stale_watches;
use super::watch_state::WatchState;

/// Check CI watch configs and inject failure logs to agents when CI fails.
pub fn check_ci_watches(home: &Path, registry: &AgentRegistry) {
    let _ = gc_stale_watches(home, "eager_gc");
    check_ci_watches_with_provider(home, registry, |watch: &WatchState| {
        let ci_url = watch.ci_provider_url.clone();
        let repo = &watch.repo;
        let (ci_type, default_url) = match watch.ci_provider.as_deref() {
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
