//! Canonical "is this a protected git ref" predicate — E4.5.
//!
//! #2550 W4: extracted out of `agent_ops.rs` into its own standalone file
//! (no `crate::*` dependencies) so `bin/agentic-git.rs` — a separate binary
//! that cannot link the full library — can `#[path]`-include this EXACT
//! source, closing the shim/lib manual-sync gap the same way
//! `integrity_core.rs` already does for the HMAC verifier (see
//! `bin/agentic-git.rs`'s `mod integrity_core` include).

/// The canonical protected-ref set: `main` and `master`. Extending this
/// propagates to every E4.5 enforcement site (`worktree_pool::lease`,
/// `mcp::handlers::ci::handle_watch_ci`) AND the shim's checkout-arm guard
/// + push-deny hardcode floor — all read this same list.
pub const PROTECTED_REFS: &[&str] = &["main", "master"];

/// E4.5 protected-branch invariant. Returns `true` for branches that
/// agents MUST NOT lease, watch, or otherwise hold a per-agent
/// concept of interest in. The canonical set is `main` and `master`;
/// extending the set here propagates to every E4.5 enforcement site
/// (currently `worktree_pool::lease` for worktree leases and
/// `mcp::handlers::ci::handle_watch_ci` for CI watch subscriptions).
///
/// CR-2026-06-14: matched **case-insensitively**. On a case-insensitive
/// filesystem (darwin/APFS, Windows NTFS) `refs/heads/Main` and
/// `refs/heads/main` collide — a `branch="Main"` lease passes a case-sensitive
/// guard, then `git worktree add -b Main` fails ("already exists") and the
/// fallback `git worktree add <path> Main` checks out the EXISTING `main`, so
/// the agent commits land on `main` (empirically reproduced on darwin/APFS:
/// committing on "Main" advanced `main`). `eq_ignore_ascii_case` is a full-
/// string compare, so substrings like `mainline` / `maintenance` /
/// `upstream-main` stay unprotected.
pub fn is_protected_ref(branch: &str) -> bool {
    PROTECTED_REFS
        .iter()
        .any(|r| branch.eq_ignore_ascii_case(r))
}
