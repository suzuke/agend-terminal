use super::{bounded_mangled, bounded_mangled_for_key, legacy_journal_exists};
use std::path::Path;

/// Raw spelling is diagnostic-only; admitted identity still selects the target.
pub(in crate::mcp::handlers::ci) fn warn_legacy_raw_identity(
    home: &Path,
    instance_name: &str,
    raw_source: &str,
    admitted_source: &str,
) {
    let raw_bounded = bounded_mangled_for_key(instance_name, raw_source);
    let canonical_bounded = bounded_mangled(instance_name, admitted_source);
    let raw_present = home.join("worktrees").join(&raw_bounded).exists()
        || legacy_journal_exists(home, &raw_bounded);
    if raw_bounded != canonical_bounded && raw_present {
        tracing::warn!(
            "legacy raw-path checkout identity remains; preserving it for binding/GC recovery"
        );
    }
}
