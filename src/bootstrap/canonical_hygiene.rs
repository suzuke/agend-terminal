//! #852 PR-C: boot-time canonical-repo hygiene.
//!
//! When the daemon comes up, the canonical source repo might have
//! been left in an unhealthy state by a previous fleet session
//! (e.g. reviewer agent's `git checkout <sha>` left HEAD detached;
//! see PR-A §3.19 protocol + PR-B shim enforcement). This module
//! decides whether to auto-switch the canonical's HEAD back to the
//! default branch ("main") so subsequent operator commands aren't
//! surprised.
//!
//! Decision matrix (pure helper [`decide_canonical_action`]):
//!
//! | head state              | working tree | action               |
//! | ----------------------- | ------------ | -------------------- |
//! | already on default      | any          | `NoOp`               |
//! | detached + clean        | clean        | `SwitchToDefault`    |
//! | detached + dirty        | dirty        | `WarnDirtyDetached`  |
//! | normal branch (non-def) | any          | `NoOp`               |
//!
//! `WarnDirtyDetached` is intentionally NOT auto-resolved — operator
//! might be mid-bisect or have legitimate WIP in a detached state,
//! and silently switching would clobber that work. The boot log
//! surfaces the warning so the operator can clean up manually.

/// The default branch the canonical-hygiene helper switches TO when
/// it auto-resolves a detached-HEAD state. Mirror of the protected-
/// refs convention used by `agend-git` shim's `is_protected_ref`.
pub const DEFAULT_BRANCH: &str = "main";

/// Possible actions for the canonical's HEAD state at boot.
/// Returned by [`decide_canonical_action`] as a pure value; the
/// integration fn dispatches based on the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalAction {
    /// HEAD is on a normal branch (default or otherwise) — nothing
    /// to do. Most-common path on boot.
    NoOp,
    /// HEAD is detached AND the working tree is clean. Safe to
    /// `git switch main` so subsequent operator commands land on the
    /// expected branch.
    SwitchToDefault,
    /// HEAD is detached BUT the working tree has uncommitted changes.
    /// Operator might be mid-bisect / mid-cherry-pick / have
    /// legitimate WIP. Log a warning and leave alone.
    WarnDirtyDetached,
}

/// Pure helper: classify the canonical's HEAD state from
/// `git rev-parse --abbrev-ref HEAD` output + a working-tree-clean
/// boolean. Caller is responsible for shelling out to git and
/// passing the trimmed outputs.
///
/// `head_state`:
/// - `"HEAD"` (literal) → detached
/// - `"main"` / `"master"` → on default branch
/// - any other non-empty string → on some other normal branch
/// - empty string → treat as detached (defensive — `rev-parse`
///   should never emit empty stdout but guard fail-closed)
///
/// `working_tree_clean`: true iff `git status --porcelain` produced
/// no output. The caller is expected to compute this once at boot.
///
/// C1 RED stub — returns `NoOp` unconditionally so the new tests
/// asserting `SwitchToDefault` / `WarnDirtyDetached` fail. C2 GREEN
/// fills in the actual classification table.
pub fn decide_canonical_action(head_state: &str, working_tree_clean: bool) -> CanonicalAction {
    let _ = (head_state, working_tree_clean);
    CanonicalAction::NoOp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most-common boot path: HEAD is on `main`. No-op regardless of
    /// working-tree state (operator might have WIP that's expected).
    #[test]
    fn boot_no_op_when_canonical_on_main() {
        assert_eq!(
            decide_canonical_action("main", true),
            CanonicalAction::NoOp,
            "HEAD on main + clean → no-op"
        );
        assert_eq!(
            decide_canonical_action("main", false),
            CanonicalAction::NoOp,
            "HEAD on main + dirty → no-op (operator WIP is not our concern)"
        );
    }

    /// HEAD detached AND working tree clean → safe to auto-switch.
    /// This is the symptom that reviewer agents historically caused
    /// (PR-A §3.19 / PR-B shim enforcement context).
    #[test]
    fn boot_auto_switch_when_detached_and_clean() {
        assert_eq!(
            decide_canonical_action("HEAD", true),
            CanonicalAction::SwitchToDefault,
            "detached HEAD + clean tree → SwitchToDefault is safe and \
             closes the operator-visible 'git 又處在一個未知的 branch 了' \
             surface"
        );
    }

    /// HEAD detached BUT working tree dirty → warn and leave alone.
    /// Operator might be mid-bisect / mid-cherry-pick / have
    /// legitimate WIP. Silent auto-switch would clobber that work.
    #[test]
    fn boot_skips_auto_switch_when_detached_and_dirty() {
        assert_eq!(
            decide_canonical_action("HEAD", false),
            CanonicalAction::WarnDirtyDetached,
            "detached HEAD + dirty tree → warn only, never auto-switch \
             (operator WIP protection)"
        );
    }

    /// HEAD on a non-default normal branch (e.g. operator was on a
    /// feature branch when the daemon started). No-op — daemon
    /// shouldn't reorganize the operator's working state.
    #[test]
    fn boot_no_op_on_feature_branch() {
        assert_eq!(
            decide_canonical_action("feat/some-work", true),
            CanonicalAction::NoOp,
            "HEAD on feature branch → no-op (operator's choice of branch \
             must not be overridden by daemon hygiene)"
        );
        assert_eq!(
            decide_canonical_action("feat/some-work", false),
            CanonicalAction::NoOp,
            "HEAD on feature branch + dirty → no-op (daemon never \
             touches non-detached HEADs)"
        );
    }

    /// Defensive: empty string from `rev-parse` is unexpected but
    /// guard fail-closed — treat as detached. If working tree is
    /// also dirty, warn rather than auto-switch.
    #[test]
    fn boot_treats_empty_rev_parse_as_detached() {
        assert_eq!(
            decide_canonical_action("", true),
            CanonicalAction::SwitchToDefault,
            "empty rev-parse + clean → treat as detached (defensive)"
        );
        assert_eq!(
            decide_canonical_action("", false),
            CanonicalAction::WarnDirtyDetached,
            "empty rev-parse + dirty → warn (defensive)"
        );
    }
}
