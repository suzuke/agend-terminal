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

/// Boot-time hygiene entry — discover distinct canonical repos from
/// `config.instances[*].source_repo` and apply [`apply_to_canonical`]
/// to each. Best-effort: any per-canonical failure is logged and
/// skipped (boot must never fail because hygiene couldn't shell out
/// to git).
pub(crate) fn run_at_boot(config: &crate::fleet::FleetConfig) {
    let mut seen = std::collections::HashSet::<std::path::PathBuf>::new();
    for (name, instance) in &config.instances {
        let Some(source_repo) = instance.source_repo.as_ref() else {
            continue;
        };
        let path = std::path::PathBuf::from(source_repo);
        if !seen.insert(path.clone()) {
            continue;
        }
        if !path.is_dir() {
            tracing::debug!(
                instance = %name,
                source_repo = %path.display(),
                "#852 canonical hygiene: source_repo not a directory, skipping"
            );
            continue;
        }
        apply_to_canonical(&path);
    }
}

/// Run the canonical-hygiene decision against a single canonical
/// repo path. Shells out to git twice (rev-parse and status), calls
/// [`decide_canonical_action`], and dispatches by variant: NoOp is
/// silent, SwitchToDefault runs `git switch main` plus info log,
/// WarnDirtyDetached emits a warn log only (no git mutation).
///
/// Best-effort: subprocess failures log a debug line and return; the
/// daemon boot continues regardless.
pub(crate) fn apply_to_canonical(canonical: &std::path::Path) {
    let head_state = match git_capture(canonical, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(s) => s,
        None => return,
    };
    let status = match git_capture(canonical, &["status", "--porcelain"]) {
        Some(s) => s,
        None => return,
    };
    let working_tree_clean = status.is_empty();
    match decide_canonical_action(&head_state, working_tree_clean) {
        CanonicalAction::NoOp => {}
        CanonicalAction::SwitchToDefault => {
            let switch_result = std::process::Command::new("git")
                .args([
                    "-C",
                    &canonical.display().to_string(),
                    "switch",
                    DEFAULT_BRANCH,
                ])
                .env("AGEND_GIT_BYPASS", "1")
                .output();
            match switch_result {
                Ok(out) if out.status.success() => {
                    tracing::info!(
                        canonical = %canonical.display(),
                        "#852 canonical hygiene: detached HEAD on clean tree, auto-switched to {DEFAULT_BRANCH}"
                    );
                }
                Ok(out) => {
                    tracing::warn!(
                        canonical = %canonical.display(),
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "#852 canonical hygiene: git switch {DEFAULT_BRANCH} failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        canonical = %canonical.display(),
                        error = %e,
                        "#852 canonical hygiene: git switch spawn failed"
                    );
                }
            }
        }
        CanonicalAction::WarnDirtyDetached => {
            tracing::warn!(
                canonical = %canonical.display(),
                "#852 canonical hygiene: detached HEAD with dirty working tree — \
                 NOT auto-switching (operator WIP protection). Run `git switch \
                 {DEFAULT_BRANCH}` manually after committing/stashing changes."
            );
        }
    }
}

/// Pure helper: shell out to git with `AGEND_GIT_BYPASS=1` (so we
/// bypass the shim's restrictions on boot), capture trimmed stdout
/// on success. Returns `None` on spawn failure or non-zero exit.
fn git_capture(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    cmd.env("AGEND_GIT_BYPASS", "1");
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                repo = %repo.display(),
                error = %e,
                "#852 canonical hygiene: git spawn failed"
            );
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            repo = %repo.display(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "#852 canonical hygiene: git command failed"
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Classification table:
///
/// | head_state | working_tree_clean | action               |
/// | ---------- | ------------------ | -------------------- |
/// | `"HEAD"`   | true               | `SwitchToDefault`    |
/// | `"HEAD"`   | false              | `WarnDirtyDetached`  |
/// | `""`       | true               | `SwitchToDefault`    |
/// | `""`       | false              | `WarnDirtyDetached`  |
/// | anything   | any                | `NoOp`               |
///   else                            |                      |
///
/// Empty `head_state` is treated as detached defensively — `git
/// rev-parse --abbrev-ref HEAD` should never produce empty stdout
/// but if it does, fail-closed by routing through the detached
/// branches (with the dirty-tree warn protecting against any
/// silent state change on weird repo states).
pub fn decide_canonical_action(head_state: &str, working_tree_clean: bool) -> CanonicalAction {
    let detached = head_state == "HEAD" || head_state.is_empty();
    if !detached {
        return CanonicalAction::NoOp;
    }
    if working_tree_clean {
        CanonicalAction::SwitchToDefault
    } else {
        CanonicalAction::WarnDirtyDetached
    }
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
