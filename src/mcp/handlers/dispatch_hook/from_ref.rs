//! `from_ref` → (remote, branch) resolution for the dispatch auto-create path.
//!
//! Extracted from `dispatch_hook/mod.rs` (CR-2026-06-14) to keep that file under
//! its grandfathered LOC ceiling while the F1/F3 security fixes land. Pure helper
//! (reads `git remote` only); re-exported from `mod.rs` so `super::` references
//! in the sibling test module keep resolving.

use std::path::Path;

/// #2010 (cheerc RCA): resolve which git remote a `from_ref` names, by
/// LONGEST-PREFIX match against the repo's actual remote list. Branch names can
/// contain `/`, so a naive `split('/')` mis-parses `upstream/feat/x`; matching
/// against the real remotes (longest name first, so `forkpa` wins over `fork`
/// on `forkpa/x`) is the only correct split. Returns the remote name and the
/// branch portion with that remote's prefix stripped — `None` branch when
/// `from_ref` carries no remote prefix (a bare local ref). Falls back to
/// `("origin", None)` when nothing matches (origin-only setups unaffected).
///
/// Documented ambiguity (§3.9 case 4): when a branch's first segment equals a
/// remote name — remote `fork` + a literal `from_ref` of `fork/feature` —
/// longest-prefix treats it as remote-qualified (remote=`fork`, branch=
/// `feature`). Fully-qualify (`origin/fork/feature`) to force the other reading.
pub(crate) fn resolve_from_ref_remote(source: &Path, from_ref: &str) -> (String, Option<String>) {
    let mut remotes: Vec<String> = crate::git_helpers::git_bypass(source, &["remote"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // Longest remote name first so a longer name wins the prefix race.
    remotes.sort_by_key(|r| std::cmp::Reverse(r.len()));
    for r in &remotes {
        if let Some(rest) = from_ref.strip_prefix(&format!("{r}/")) {
            return (r.clone(), Some(rest.to_string()));
        }
    }
    ("origin".to_string(), None)
}
