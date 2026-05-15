//! #830: shared runtime-registry helpers. Consolidates the
//! `api::call(LIST)` liveness-cross-ref pattern that was duplicated
//! across `src/teams.rs:200-212` (#785), `src/render/panels.rs::
//! fetch_live_agents` (#827), and `src/tasks.rs::fetch_live_agents`
//! (#829). The fourth consumer arriving in `task action=health`
//! (this PR) is the design-call threshold that justifies extraction.

use std::collections::HashSet;
use std::path::Path;

/// Fetch the daemon's live runtime agent registry via
/// `api::call(LIST)`. Returns `Some(set)` on success (the set may
/// legitimately be empty when no agents are running) and `None`
/// when the daemon is offline / unreachable.
///
/// The `None`-vs-`Some(empty)` distinction lets callers degrade
/// differently: render-time filters (#827) keep all assignees on
/// `None` to avoid misleading "all idle" reports; the #829 boot
/// sweeper skips the orphan-clear entirely on `None` to avoid
/// over-orphaning during the daemon's own socket-bind race; #830's
/// health response surfaces a degraded `live_agents_available: false`
/// hint to the operator. Each caller chooses its own fallback by
/// matching on the `Option`.
pub fn list_live_agents(home: &Path) -> Option<HashSet<String>> {
    crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::LIST}),
    )
    .ok()
    .and_then(|r| {
        r["result"]["agents"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|a| a["name"].as_str().map(String::from))
                .collect()
        })
    })
}
