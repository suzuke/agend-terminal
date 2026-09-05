//! t-…-67: what a `ci status` view IS, said out loud.
//!
//! A named caller sees only the watches it subscribes to (`handle_status_ci`),
//! so `watches: []` never means "no watch exists" — four such readings in two
//! days (#3524, #3521, #3526, #3527) were made by a non-subscriber and each
//! ended in a needless manual re-arm of a live watch. The response therefore
//! names its scope and counts what it hid on the requested repo/branch filter.
//! The hint states the one thing a re-arm actually does: an explicit
//! `review_class` reconciles the PR gate and recomputes readiness (see
//! `handle_watch_ci`); a bare `ci watch` only adds a subscriber.
//!
//! Split out of `watch.rs` so that file stays under the handler LOC invariant.

/// The `scope` field: whose view this is.
pub(super) fn scope_label(instance_name: &str) -> String {
    if instance_name.is_empty() {
        "all".to_string()
    } else {
        format!("subscriber:{instance_name}")
    }
}

/// The `hint` field, present only when `hidden_watches > 0`.
pub(super) fn hidden_hint(hidden_watches: usize) -> String {
    format!(
        "{hidden_watches} watch(es) exist on the requested repo/branch that you are not \
         subscribed to; `ci status` is subscriber-scoped. `ci watch repository=<owner/repo> \
         branch=<branch> review_class=single|dual` adds you as a subscriber and forces a \
         readiness recompute; a bare `ci watch` only subscribes."
    )
}
