//! #852 canonical-repo hygiene (boot-time + #852-residual runtime).
//!
//! When the canonical source repo is left in an unhealthy state
//! — e.g. a reviewer agent's `git checkout <sha>` left HEAD detached
//! (see §3.19 + the L2 shim deny matrix) — this module decides
//! whether to auto-switch the canonical's HEAD back to the default
//! branch ("main") so subsequent operator commands aren't surprised.
//!
//! Two call sites share the same helper:
//!
//! - **Boot path** (`bootstrap/mod.rs`): one-shot scan at daemon
//!   startup — catches state inherited from the prior session.
//! - **Runtime path** (`daemon/canonical_drift.rs`): per-tick
//!   throttled scan (5-min cadence) — catches drift accrued AFTER
//!   boot for long-lived daemons. #852 residual PR-B.
//!
//! Decision matrix (pure helper [`decide_canonical_action`]):
//!
//! | head state              | working tree | action                       |
//! | ----------------------- | ------------ | ---------------------------- |
//! | already on default      | any          | `NoOp`                       |
//! | detached + clean        | clean        | `SwitchToDefault`            |
//! | detached + dirty        | dirty        | `StashAndSwitchToDefault`    |
//! | normal branch (non-def) | any          | `NoOp`                       |
//!
//! `StashAndSwitchToDefault` (#852 residual PR-C) auto-stashes the
//! dirty WIP with a timestamped marker, switches the canonical back
//! to the default branch, and notifies the operator with the stash
//! reference so they can recover via `git stash pop`. The stash is
//! reversible by definition — safer than letting reviewer pollution
//! land with no recovery path. On stash failure (e.g. `.git/index.lock`
//! held by another process, in-progress rebase/merge) the dispatch
//! falls back to the `WarnDirtyDetached` warn log — no half-switch,
//! no mutation, operator's WIP preserved.

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
    ///
    /// After #852 residual PR-C the decision table no longer routes
    /// here — [`Self::StashAndSwitchToDefault`] handles dirty-detached
    /// reversibly and its stash-failure fall-back calls into the
    /// `WarnDirtyDetached` warn helper directly (via
    /// `emit_dirty_detached_warning`) rather than reconstructing this
    /// variant. The variant is retained for diagnostic / future
    /// reactivation and tagged `#[allow(dead_code)]` accordingly.
    #[allow(dead_code)]
    WarnDirtyDetached,
    /// HEAD is detached AND the working tree is dirty: stash the WIP
    /// with a timestamped marker, switch to the default branch, and
    /// notify the operator about the stash ref so they can recover
    /// via `git stash pop`. Reversible by definition — safer than
    /// letting reviewer pollution land with no recovery path. On
    /// stash failure, falls back to [`Self::WarnDirtyDetached`]
    /// behaviour (warn log, no mutation).
    StashAndSwitchToDefault,
}

/// Hygiene entry — discover distinct canonical repos from
/// `config.instances[*].source_repo` and apply [`apply_to_canonical`]
/// to each. Best-effort: any per-canonical failure is logged and
/// skipped (the caller — boot or per-tick runtime — must never fail
/// because hygiene couldn't shell out to git).
///
/// Called from two sites: `bootstrap/mod.rs` at daemon startup and
/// `daemon/canonical_drift.rs` per-tick (5-min throttled cadence).
pub(crate) fn run_hygiene(config: &crate::fleet::FleetConfig) {
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
/// StashAndSwitchToDefault attempts `git stash push -u` + `git switch
/// main` + operator notify (on stash failure, falls back to
/// WarnDirtyDetached's warn log), and WarnDirtyDetached is reached
/// only as the stash-failure fall-back today (the decision table
/// no longer routes there from clean inputs).
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
            emit_dirty_detached_warning(canonical);
        }
        CanonicalAction::StashAndSwitchToDefault => {
            apply_stash_and_switch(canonical);
        }
    }
}

/// #852 residual PR-C: handle the detached-HEAD + dirty-tree case
/// by stashing the WIP with a timestamped marker, switching back to
/// the default branch, and notifying the operator with recovery
/// instructions. On stash failure, fall back to
/// [`emit_dirty_detached_warning`] so the operator's WIP is preserved.
fn apply_stash_and_switch(canonical: &std::path::Path) {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let stash_message = format!("agend canonical hygiene auto-stash {timestamp}");
    match git_stash_push(canonical, &stash_message) {
        Err(stderr) => {
            tracing::warn!(
                canonical = %canonical.display(),
                stash_stderr = %stderr,
                "#852 canonical hygiene: stash push failed; falling back to \
                 warn-only (operator WIP preserved)"
            );
            emit_dirty_detached_warning(canonical);
        }
        Ok(()) => {
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
                        stash_message = %stash_message,
                        "#852 canonical hygiene: dirty detached HEAD auto-stashed and switched to {DEFAULT_BRANCH}"
                    );
                    notify_operator_of_auto_stash(canonical, &stash_message);
                }
                Ok(out) => {
                    // Stash succeeded but switch failed — operator's
                    // WIP is in the stash, canonical is still detached
                    // but clean. Warn so the operator restores manually.
                    tracing::warn!(
                        canonical = %canonical.display(),
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        stash_message = %stash_message,
                        "#852 canonical hygiene: stash succeeded but git switch {DEFAULT_BRANCH} failed — recover via `git stash pop`"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        canonical = %canonical.display(),
                        error = %e,
                        stash_message = %stash_message,
                        "#852 canonical hygiene: stash succeeded but git switch spawn failed — recover via `git stash pop`"
                    );
                }
            }
        }
    }
}

/// Emit the dirty-detached warn log. Extracted so the
/// `StashAndSwitchToDefault` fall-back can reuse the exact text the
/// `WarnDirtyDetached` arm would emit, keeping operator-visible
/// messaging consistent across the two reachable paths.
fn emit_dirty_detached_warning(canonical: &std::path::Path) {
    tracing::warn!(
        canonical = %canonical.display(),
        "#852 canonical hygiene: detached HEAD with dirty working tree — \
         NOT auto-switching (operator WIP protection). Run `git switch \
         {DEFAULT_BRANCH}` manually after committing/stashing changes."
    );
}

/// Best-effort: shell out to `git stash push -u -m <message>` with
/// the AGEND bypass set. Returns `Ok(())` on success, `Err(stderr)`
/// on failure (spawn fail or non-zero exit). The `-u` flag includes
/// untracked files so any reviewer-left dirt is captured even when
/// it hasn't been `git add`-ed.
fn git_stash_push(canonical: &std::path::Path, message: &str) -> Result<(), String> {
    let out = match std::process::Command::new("git")
        .args([
            "-C",
            &canonical.display().to_string(),
            "stash",
            "push",
            "-u",
            "-m",
            message,
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) => o,
        Err(e) => return Err(format!("spawn failed: {e}")),
    };
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Best-effort: notify the operator-mapped agent (`general` by
/// convention; see `bootstrap/fleet_normalize.rs`) about the auto-
/// stash with recovery instructions. Delivery failures are silently
/// dropped — the tracing::info above already records the canonical
/// hygiene event for log audits.
fn notify_operator_of_auto_stash(canonical: &std::path::Path, stash_message: &str) {
    let home = crate::home_dir();
    let text = format!(
        "[system:canonical_auto_stash] Canonical at `{path}` was detached + dirty; \
         auto-stashed WIP as `{stash_message}` and switched back to {DEFAULT_BRANCH}. \
         Recover via:\n  git -C {path} stash list\n  git -C {path} stash pop  # or: git stash apply <ref>\n#852.",
        path = canonical.display(),
    );
    let source = crate::inbox::NotifySource::System("canonical_auto_stash");
    crate::inbox::notify_agent(&home, "general", &source, &text);
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
/// | head_state | working_tree_clean | action                       |
/// | ---------- | ------------------ | ---------------------------- |
/// | `"HEAD"`   | true               | `SwitchToDefault`            |
/// | `"HEAD"`   | false              | `StashAndSwitchToDefault`    |
/// | `""`       | true               | `SwitchToDefault`            |
/// | `""`       | false              | `StashAndSwitchToDefault`    |
/// | anything   | any                | `NoOp`                       |
///   else                            |                              |
///
/// Empty `head_state` is treated as detached defensively — `git
/// rev-parse --abbrev-ref HEAD` should never produce empty stdout
/// but if it does, fail-closed by routing through the detached
/// branches. The dirty-tree case auto-stashes + switches (#852
/// residual PR-C) rather than warn-only, because the stash is fully
/// reversible (`git stash pop`) and the prior warn-only behaviour
/// left the canonical permanently polluted from the operator's POV.
pub fn decide_canonical_action(head_state: &str, working_tree_clean: bool) -> CanonicalAction {
    let detached = head_state == "HEAD" || head_state.is_empty();
    if !detached {
        return CanonicalAction::NoOp;
    }
    if working_tree_clean {
        CanonicalAction::SwitchToDefault
    } else {
        CanonicalAction::StashAndSwitchToDefault
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    /// also dirty, route through the stash-and-switch path so the
    /// canonical doesn't stay polluted (PR-C behaviour).
    #[test]
    fn boot_treats_empty_rev_parse_as_detached() {
        assert_eq!(
            decide_canonical_action("", true),
            CanonicalAction::SwitchToDefault,
            "empty rev-parse + clean → treat as detached (defensive)"
        );
        assert_eq!(
            decide_canonical_action("", false),
            CanonicalAction::StashAndSwitchToDefault,
            "empty rev-parse + dirty → stash-and-switch (defensive); \
             mirrors the detached-HEAD + dirty cell of the decision \
             table"
        );
    }

    // ----------------------------------------------------------------
    // #852 residual PR-C: dirty-detached stash-recovery tests.
    // ----------------------------------------------------------------

    /// PR-C contract: detached HEAD with dirty working tree must
    /// resolve to [`CanonicalAction::StashAndSwitchToDefault`] (reversible
    /// auto-recovery), not the obsolete `WarnDirtyDetached` (which
    /// left the canonical permanently polluted for the operator).
    ///
    /// In C1 RED, [`decide_canonical_action`] still returns the old
    /// `WarnDirtyDetached`, so this assertion fails. C2 GREEN updates
    /// the decision table and this passes.
    #[test]
    fn stash_and_switch_on_dirty_detached() {
        assert_eq!(
            decide_canonical_action("HEAD", false),
            CanonicalAction::StashAndSwitchToDefault,
            "detached HEAD + dirty tree → StashAndSwitchToDefault \
             (reversible auto-recovery; #852 residual PR-C)"
        );
    }

    /// PR-C contract: when `git stash push` fails at the syscall /
    /// repo-state level (here simulated by planting `.git/index.lock`
    /// before invoking [`apply_to_canonical`]), the integration fn
    /// must fall back to the warn-only branch — no panic, no half-
    /// switch, and HEAD must remain in its detached state so the
    /// operator can still recover manually.
    ///
    /// The fixture builds a real micro-repo so `rev-parse` and
    /// `status --porcelain` both succeed (otherwise apply exits
    /// before reaching the stash branch). C1 RED reaches this branch
    /// via the stub arm; C2 GREEN will actually attempt the stash,
    /// observe the index-lock failure, and route through the same
    /// fall-back. The invariants asserted hold in both phases —
    /// this test is the smoke that proves the new variant's
    /// dispatch never panics + never loses the operator's WIP.
    #[test]
    fn apply_to_canonical_falls_back_to_warn_on_stash_failure() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let canonical = std::env::temp_dir().join(format!(
            "agend-test-canonical-stash-fallback-{}-{id}",
            std::process::id(),
        ));
        // Best-effort cleanup from any prior run.
        let _ = std::fs::remove_dir_all(&canonical);
        std::fs::create_dir_all(&canonical).unwrap();

        // Build a minimal repo: init + initial commit on main, then
        // detach + dirty the tree so decide_canonical_action observes
        // the StashAndSwitchToDefault path in C2.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&canonical)
                .args(args)
                .env("AGEND_GIT_BYPASS", "1")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git command spawn")
        };
        assert!(run(&["init", "-q", "-b", "main"]).status.success());
        std::fs::write(canonical.join("file.txt"), "initial\n").unwrap();
        assert!(run(&["add", "file.txt"]).status.success());
        assert!(run(&["commit", "-q", "-m", "initial"]).status.success());
        let initial_sha = String::from_utf8(run(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(
            run(&["checkout", "-q", &initial_sha]).status.success(),
            "detach HEAD via checkout SHA"
        );
        std::fs::write(canonical.join("file.txt"), "modified-wip\n").unwrap();
        // Plant the index lock so `git stash push` cannot complete.
        std::fs::write(canonical.join(".git").join("index.lock"), "").unwrap();

        // Drive apply. Must not panic; HEAD must remain detached.
        apply_to_canonical(&canonical);

        let head_state = String::from_utf8(run(&["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(
            head_state, "HEAD",
            "post-fall-back HEAD must remain detached — operator WIP \
             must not be silently moved when stash recovery fails"
        );

        // No stash refs should exist on the failure path.
        let stash_ref = canonical.join(".git").join("refs").join("stash");
        assert!(
            !stash_ref.exists(),
            "stash ref must not exist when git stash push failed"
        );

        let _ = std::fs::remove_dir_all(&canonical);
    }
}
