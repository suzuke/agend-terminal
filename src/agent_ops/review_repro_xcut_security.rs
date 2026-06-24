//! xcut-security batch — verification/reproduction tests for confirmed
//! code-review findings, attached in-module to `src/agent_ops.rs` so the
//! crate-private `validate_branch` is reachable via `super::`.
//!
//! Each test encodes the CORRECT post-fix behavior and is GREEN on current
//! code; they run un-ignored as live regression guards.

/// Finding: `validate_branch` accepts refnames git itself rejects and, worse,
/// accepts leading-dot path components (`.`, `.config`, `.git`,
/// `feature/.hidden`) plus a `.lock` suffix and a trailing `/`. Because the
/// branch is also used as a filesystem path component in `worktree_path`
/// (`home/worktrees/<agent>/<branch>`), a `.`-prefixed component can collide
/// with control files in the worktree pool layout (`.git`, `.agend-managed`),
/// and a lone `.` resolves to the parent dir. The fix must reject any path
/// component that is `.` or begins with `.`, reject a trailing `/`, and reject
/// a `.lock` suffix — while keeping legitimate names (`v1.0.0`,
/// `feature/foo`, `release_2.0`) valid.
///
/// RED now: every bad case below currently returns `true` (verified
/// empirically). GREEN after the fix tightens the validator.
#[test]
fn validate_branch_rejects_leading_dot_and_git_invalid_refs_xcut_security() {
    // ── Must be REJECTED after the fix (currently all accepted = the bug). ──
    // Lone `.` — a no-op path component (resolves to the parent dir) and an
    // invalid git refname.
    assert!(
        !super::validate_branch("."),
        "a lone '.' must be rejected (no-op path component + invalid git ref)"
    );
    // Leading-dot component can collide with `.git` / `.agend-managed`.
    assert!(
        !super::validate_branch(".config"),
        "a leading-dot branch must be rejected (collision vector in worktree pool)"
    );
    assert!(
        !super::validate_branch(".git"),
        "'.git' must be rejected (collides with the worktree control dir)"
    );
    assert!(
        !super::validate_branch(".agend-managed"),
        "'.agend-managed' must be rejected (collides with a pool control file)"
    );
    // Leading-dot in a NON-leading path component must also be rejected.
    assert!(
        !super::validate_branch("feature/.hidden"),
        "a leading-dot in any path component must be rejected"
    );
    // git rejects refs ending in `.lock`.
    assert!(
        !super::validate_branch("foo.lock"),
        "a '.lock'-suffixed branch must be rejected (git refname rule)"
    );
    // Trailing slash yields an empty trailing path component.
    assert!(
        !super::validate_branch("foo/"),
        "a trailing '/' must be rejected (empty trailing path component)"
    );

    // ── Must STILL be ACCEPTED after the fix (guards against over-rejection). ──
    assert!(
        super::validate_branch("v1.0.0"),
        "a legitimate dotted version branch must stay valid"
    );
    assert!(
        super::validate_branch("feature/foo"),
        "a normal slashed branch must stay valid"
    );
    assert!(
        super::validate_branch("release_2.0"),
        "underscores + interior dots must stay valid"
    );
    assert!(
        super::validate_branch("main"),
        "plain 'main' must stay valid"
    );
}
