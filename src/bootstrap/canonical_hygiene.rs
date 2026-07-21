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
//! falls back to `emit_dirty_detached_warning` — no half-switch,
//! no mutation, operator's WIP preserved.

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
    /// HEAD is detached AND the working tree is dirty: stash the WIP
    /// with a timestamped marker, switch to the default branch, and
    /// notify the operator about the stash ref so they can recover
    /// via `git stash pop`. Reversible by definition — safer than
    /// letting reviewer pollution land with no recovery path. On
    /// stash failure, `apply_stash_and_switch` calls
    /// `emit_dirty_detached_warning` directly (warn log, no mutation).
    StashAndSwitchToDefault,
}

/// A managed canonical repo found DIRTY on the default branch (non-ignored
/// working-tree changes while HEAD is on `main`). Produced by
/// [`apply_to_canonical`] as a pure value; the caller decides whether/how to
/// notify (boot notifies once; the runtime tracker throttles by [`fingerprint`]).
///
/// Strict policy (operator-chosen): under AgEnD management the canonical default
/// branch must stay clean — agents and operators work in worktrees. We report
/// "managed canonical is dirty", NOT "an agent did it"; we have no provenance, so
/// the wording avoids attribution.
///
/// [`fingerprint`]: CanonicalDirtyReport::fingerprint
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalDirtyReport {
    /// Canonical repo path (a `source_repo` from fleet.yaml).
    pub path: std::path::PathBuf,
    /// The repo's resolved default branch (e.g. "main", "dev", "master").
    pub default_branch: String,
    /// Raw `git status --porcelain` lines (non-ignored entries), original order,
    /// preserved verbatim for the audit trail.
    pub porcelain_lines: Vec<String>,
    /// Stable, order-independent hash of the dirty set — the re-alert throttle
    /// key. "Same WIP still dirty" yields the same fingerprint; a changed dirty
    /// set yields a new one, so the throttle re-notifies immediately on change.
    pub fingerprint: u64,
}

impl CanonicalDirtyReport {
    fn from_status(path: &std::path::Path, porcelain: &str, default_branch: &str) -> Self {
        let porcelain_lines: Vec<String> = porcelain
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        // Fingerprint over a SORTED copy so git's line ordering doesn't perturb
        // the throttle key — only the actual set of changes matters.
        let mut sorted = porcelain_lines.clone();
        sorted.sort();
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            sorted.hash(&mut h);
            h.finish()
        };
        Self {
            path: path.to_path_buf(),
            default_branch: default_branch.to_string(),
            porcelain_lines,
            fingerprint,
        }
    }
}

/// Hygiene entry — discover distinct canonical repos from
/// `config.instances[*].source_repo`, apply [`apply_to_canonical`] to each, and
/// return the set found dirty on the default branch (strict-policy L2 detection).
/// Best-effort: any per-canonical failure is logged and skipped (the caller —
/// boot or per-tick runtime — must never fail because hygiene couldn't shell out
/// to git).
///
/// Called from two sites: `bootstrap/mod.rs` at daemon startup (notifies each
/// report once) and `daemon/canonical_drift.rs` per-tick (throttles re-alerts by
/// fingerprint). The HEAD-hygiene side effect (auto-switch/stash of a detached
/// canonical) is unchanged — the dirty report is an orthogonal, additive output.
pub(crate) fn run_hygiene_with_dirty_report(
    config: &crate::fleet::FleetConfig,
) -> Vec<CanonicalDirtyReport> {
    let mut seen = std::collections::HashSet::<std::path::PathBuf>::new();
    let mut dirty = Vec::new();
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
        if let Some(report) = apply_to_canonical(&path) {
            dirty.push(report);
        }
    }
    dirty
}

/// Run the canonical-hygiene decision against a single canonical
/// repo path. Shells out to git twice (rev-parse and status), calls
/// [`decide_canonical_action`], and dispatches by variant: NoOp is
/// silent, SwitchToDefault runs `git switch main` plus info log,
/// StashAndSwitchToDefault attempts `git stash push -u` + `git switch
/// main` + operator notify (on stash failure, calls
/// `emit_dirty_detached_warning` directly — no half-switch, no
/// mutation, operator's WIP preserved).
///
/// Best-effort: subprocess failures log a debug line and return `None`; the
/// daemon boot continues regardless.
///
/// L2 (strict policy): in ADDITION to the HEAD-state side effects, returns
/// `Some(CanonicalDirtyReport)` when the canonical is on the default branch with a
/// non-ignored dirty working tree — a worktree-discipline violation the caller
/// surfaces to the operator. The HEAD action and the dirty report are orthogonal:
/// `main + dirty` still maps to `NoOp` for the HEAD action (we never stash/switch
/// the operator's branch), and detached states never produce a dirty report
/// (their WIP is handled by the stash path).
pub(crate) fn apply_to_canonical(canonical: &std::path::Path) -> Option<CanonicalDirtyReport> {
    let head_state = git_capture(canonical, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    // #2895: resolve the per-repo default branch once (e.g. "main", "dev",
    // "master") instead of assuming "main".
    let repo_default = crate::git_helpers::default_branch(canonical);
    // `--untracked-files=normal` is explicit so a global `status.showUntrackedFiles=no`
    // can't hide an untracked stray file (exactly the SESSION-HANDOFF-006.md class).
    let status = git_capture(
        canonical,
        &["status", "--porcelain", "--untracked-files=normal"],
    )?;
    let working_tree_clean = status.is_empty();
    match decide_canonical_action(&head_state, working_tree_clean) {
        CanonicalAction::NoOp => {}
        CanonicalAction::SwitchToDefault => {
            // #1899: bounded via git_bypass (current_dir == `-C`, LOCAL 60s).
            let switch_result =
                crate::git_helpers::git_bypass(canonical, &["switch", &repo_default]);
            match switch_result {
                Ok(out) if out.status.success() => {
                    tracing::info!(
                        canonical = %canonical.display(),
                        default_branch = %repo_default,
                        "#852 canonical hygiene: detached HEAD on clean tree, auto-switched to default branch"
                    );
                }
                Ok(out) => {
                    tracing::warn!(
                        canonical = %canonical.display(),
                        default_branch = %repo_default,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "#852 canonical hygiene: git switch to default branch failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        canonical = %canonical.display(),
                        default_branch = %repo_default,
                        error = %e,
                        "#852 canonical hygiene: git switch spawn failed"
                    );
                }
            }
        }
        CanonicalAction::StashAndSwitchToDefault => {
            apply_stash_and_switch(canonical, &repo_default);
        }
    }

    // L2 (strict): a managed canonical on the default branch must stay clean. A
    // non-ignored dirty tree here means someone wrote into the canonical working
    // tree instead of a worktree — return a report so the caller notifies (with
    // runtime re-alert throttling). Detached states are handled by the stash path
    // above; they have `head_state != repo_default`, so they never surface here.
    if head_state == repo_default && !working_tree_clean {
        Some(CanonicalDirtyReport::from_status(
            canonical,
            &status,
            &repo_default,
        ))
    } else {
        None
    }
}

/// #852 residual PR-C: handle the detached-HEAD + dirty-tree case
/// by stashing the WIP with a timestamped marker, switching back to
/// the default branch, and notifying the operator with recovery
/// instructions. On stash failure, fall back to
/// [`emit_dirty_detached_warning`] so the operator's WIP is preserved.
fn apply_stash_and_switch(canonical: &std::path::Path, default_branch: &str) {
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
            emit_dirty_detached_warning(canonical, default_branch);
        }
        Ok(()) => {
            // #1899: bounded via git_bypass (current_dir == `-C`, LOCAL 60s).
            let switch_result =
                crate::git_helpers::git_bypass(canonical, &["switch", default_branch]);
            match switch_result {
                Ok(out) if out.status.success() => {
                    tracing::info!(
                        canonical = %canonical.display(),
                        default_branch = %default_branch,
                        stash_message = %stash_message,
                        "#852 canonical hygiene: dirty detached HEAD auto-stashed and switched to default branch"
                    );
                    notify_operator_of_auto_stash(canonical, &stash_message, default_branch);
                }
                Ok(out) => {
                    // Stash succeeded but switch failed — operator's
                    // WIP is in the stash, canonical is still detached
                    // but clean. Warn so the operator restores manually.
                    tracing::warn!(
                        canonical = %canonical.display(),
                        default_branch = %default_branch,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        stash_message = %stash_message,
                        "#852 canonical hygiene: stash succeeded but git switch to default branch failed — recover via `git stash pop`"
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

/// Emit the dirty-detached warn log. Called by
/// `apply_stash_and_switch` on stash failure when the detached HEAD
/// + dirty tree state can't be auto-recovered.
fn emit_dirty_detached_warning(canonical: &std::path::Path, default_branch: &str) {
    tracing::warn!(
        canonical = %canonical.display(),
        default_branch = %default_branch,
        "#852 canonical hygiene: detached HEAD with dirty working tree — \
         NOT auto-switching (operator WIP protection). Run `git switch <default>` \
         manually after committing/stashing changes."
    );
}

/// Best-effort: shell out to `git stash push -u -m <message>` with
/// the AGEND bypass set. Returns `Ok(())` on success, `Err(stderr)`
/// on failure (spawn fail or non-zero exit). The `-u` flag includes
/// untracked files so any reviewer-left dirt is captured even when
/// it hasn't been `git add`-ed.
fn git_stash_push(canonical: &std::path::Path, message: &str) -> Result<(), String> {
    // #1899: bounded via git_bypass (current_dir == `-C`, LOCAL 60s). `stash
    // push` is a LOCAL stash op (NOT `git push` / network).
    let out =
        match crate::git_helpers::git_bypass(canonical, &["stash", "push", "-u", "-m", message]) {
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
/// The canonical-auto-stash notice BODY. The `[system:canonical_auto_stash]`
/// marker is added by the notify layer (`NotifySource::System`); the body must
/// NOT embed a second (double-prefix bug, #t-…61315-2).
fn canonical_auto_stash_notice(
    canonical: &std::path::Path,
    stash_message: &str,
    default_branch: &str,
) -> String {
    format!(
        "Canonical at `{path}` was detached + dirty; \
         auto-stashed WIP as `{stash_message}` and switched back to {default_branch}. \
         Recover via:\n  git -C {path} stash list\n  git -C {path} stash pop  # or: git stash apply <ref>\n#852.",
        path = canonical.display(),
    )
}

fn notify_operator_of_auto_stash(
    canonical: &std::path::Path,
    stash_message: &str,
    default_branch: &str,
) {
    let home = crate::home_dir();
    let text = canonical_auto_stash_notice(canonical, stash_message, default_branch);
    let source = crate::inbox::NotifySource::System("canonical_auto_stash");
    crate::inbox::notify_agent(&home, "general", &source, &text);
}

/// L2: notify the operator-mapped agent (`general`, per convention) that a managed
/// canonical repo is DIRTY on the default branch, and record a structured audit
/// event. Strict-policy wording surfaces the violation + the fix (use a worktree /
/// move scratch out) WITHOUT attributing it to any agent — we have no provenance.
/// The notification body is bounded (first `MAX_LINES` porcelain entries + a
/// count); the full porcelain list goes to the event log for audit.
/// The canonical-dirty notice BODY. The `[system:canonical_dirty]` marker is added
/// by the notify layer (`NotifySource::System`); the body must NOT embed a second
/// (double-prefix bug, #t-…61315-2). Bounded to the first `MAX_LINES` porcelain
/// entries + a count; the full list goes to the event log.
fn canonical_dirty_notice(report: &CanonicalDirtyReport) -> String {
    const MAX_LINES: usize = 10;
    let total = report.porcelain_lines.len();
    let mut body: String = report
        .porcelain_lines
        .iter()
        .take(MAX_LINES)
        .map(|l| format!("\n  {l}"))
        .collect();
    if total > MAX_LINES {
        body.push_str(&format!(
            "\n  … (+{} more; full list in event log)",
            total - MAX_LINES
        ));
    }
    format!(
        "Managed canonical repo `{path}` is DIRTY on \
         {branch} ({total} non-ignored change(s)). Canonical repos must stay clean \
         under AgEnD — work in a git worktree and move any scratch/handoff files \
         OUT of the canonical tree.{body}\nInspect: git -C {path} status",
        path = report.path.display(),
        branch = report.default_branch,
    )
}

pub(crate) fn notify_operator_of_canonical_dirty(report: &CanonicalDirtyReport) {
    let home = crate::home_dir();
    let text = canonical_dirty_notice(report);
    let source = crate::inbox::NotifySource::System("canonical_dirty");
    crate::inbox::notify_agent(&home, "general", &source, &text);

    // Structured audit trail — full (unbounded) porcelain for forensic detail.
    crate::event_log::log(
        &home,
        "canonical_dirty_detected",
        &report.path.display().to_string(),
        &report.porcelain_lines.join("; "),
    );
}

/// Pure helper: shell out to git with `AGEND_GIT_BYPASS=1` (so we
/// bypass the shim's restrictions on boot), capture trimmed stdout
/// on success. Returns `None` on spawn failure or non-zero exit.
fn git_capture(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    // #1899: bounded via git_bypass (current_dir == `-C`, LOCAL 60s).
    let out = match crate::git_helpers::git_bypass(repo, args) {
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

    /// #t-…61315-2 Bug2 (RED-first): the `[system:…]` marker is added exactly
    /// once by the notify layer (`NotifySource::System`). The text builder must
    /// NOT embed a second copy — else the delivered message double-prefixes.
    #[test]
    fn canonical_dirty_notice_no_embedded_marker() {
        let report = CanonicalDirtyReport::from_status(
            std::path::Path::new("/tmp/canon"),
            " M src/foo.rs\n?? scratch.txt",
            "main",
        );
        let notice = canonical_dirty_notice(&report);
        assert!(
            !notice.contains("[system:"),
            "notice body must not embed a [system:…] marker (notify layer adds it): {notice}"
        );
    }

    /// #t-…61315-2 Bug2 (RED-first): auto-stash notice builder — same invariant.
    #[test]
    fn canonical_auto_stash_notice_no_embedded_marker() {
        let notice =
            canonical_auto_stash_notice(std::path::Path::new("/tmp/canon"), "wip-abc123", "main");
        assert!(
            !notice.contains("[system:"),
            "notice body must not embed a [system:…] marker (notify layer adds it): {notice}"
        );
    }

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
    /// resolve to [`CanonicalAction::StashAndSwitchToDefault`] —
    /// reversible auto-recovery via stash, with
    /// `emit_dirty_detached_warning` as the warn-only fallback when
    /// the stash itself fails.
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

        // Drive apply. Must not panic; HEAD must remain detached. (Detached →
        // no default-branch dirty report; the stash path owns this WIP.)
        let _ = apply_to_canonical(&canonical);

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

    /// L2 strict-policy detection (the SESSION-HANDOFF-006.md incident): a managed
    /// canonical on `main` must report ANY non-ignored dirty state, while an
    /// exactly-gitignored variant stays silent. Walks one repo through several
    /// states asserting the dirty report at each.
    #[test]
    fn apply_to_canonical_reports_dirty_main_strict_l2() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let canonical = std::env::temp_dir().join(format!(
            "agend-test-canonical-l2-dirty-{}-{id}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&canonical);
        std::fs::create_dir_all(&canonical).unwrap();

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
        // .gitignore ignores EXACTLY `SESSION-HANDOFF.md` (mirrors the real repo's
        // exact-match ignore that the `-006` variant slipped past).
        std::fs::write(canonical.join(".gitignore"), "SESSION-HANDOFF.md\n").unwrap();
        std::fs::write(canonical.join("file.txt"), "initial\n").unwrap();
        assert!(run(&["add", "-A"]).status.success());
        assert!(run(&["commit", "-q", "-m", "initial"]).status.success());

        // 1. Clean main → no report.
        assert!(
            apply_to_canonical(&canonical).is_none(),
            "clean main must not report dirty"
        );

        // 2. Only an exactly-gitignored file present → still no report (porcelain
        //    excludes ignored entries).
        std::fs::write(canonical.join("SESSION-HANDOFF.md"), "ignored handoff\n").unwrap();
        assert!(
            apply_to_canonical(&canonical).is_none(),
            "an only-gitignored dirty file must NOT report (it can't pollute git)"
        );

        // 3. The incident: an untracked, NON-ignored stray → report includes it.
        std::fs::write(canonical.join("SESSION-HANDOFF-006.md"), "stray on main\n").unwrap();
        let report = apply_to_canonical(&canonical)
            .expect("untracked non-ignored stray on main MUST report (the -006 incident)");
        assert!(
            report
                .porcelain_lines
                .iter()
                .any(|l| l.contains("SESSION-HANDOFF-006.md")),
            "report must carry the stray file's porcelain line"
        );
        assert!(
            !report
                .porcelain_lines
                .iter()
                .any(|l| l.contains("SESSION-HANDOFF.md\"") || l.ends_with("SESSION-HANDOFF.md")),
            "the gitignored variant must NOT appear in the report"
        );
        let fp_untracked = report.fingerprint;

        // 4. A tracked modification also alerts, with a DIFFERENT fingerprint.
        std::fs::remove_file(canonical.join("SESSION-HANDOFF-006.md")).unwrap();
        std::fs::write(canonical.join("file.txt"), "modified-on-main\n").unwrap();
        let report2 =
            apply_to_canonical(&canonical).expect("a tracked modification on main must report");
        assert!(
            report2
                .porcelain_lines
                .iter()
                .any(|l| l.contains("file.txt")),
            "report must carry the modified tracked file"
        );
        assert_ne!(
            report2.fingerprint, fp_untracked,
            "a different dirty set must yield a different fingerprint"
        );

        let _ = std::fs::remove_dir_all(&canonical);
    }

    /// #2895 RED: `DEFAULT_BRANCH` is hardcoded to `"main"` — repos whose
    /// actual default is NOT `"main"` (e.g. `"dev"`) hit two bugs:
    ///   (a) `head_state == DEFAULT_BRANCH` is false → dirty-on-default report
    ///       never fires even when the canonical IS on its true default + dirty.
    ///   (b) `git switch DEFAULT_BRANCH` switches to `"main"` instead of the
    ///       repo's actual default → fails or targets the wrong branch.
    ///
    /// Fixture: real micro-repo with `refs/remotes/origin/HEAD → origin/dev`.
    #[test]
    fn apply_to_canonical_respects_repo_default_branch_2895() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);

        let base = std::env::temp_dir().join(format!(
            "agend-test-2895-dev-default-{}-{id}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&base);
        let bare = base.join("origin.git");
        let canonical = base.join("canonical");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&canonical).unwrap();

        let run_at = |dir: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("AGEND_GIT_BYPASS", "1")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git command spawn")
        };

        // Bare origin with default branch "dev".
        assert!(run_at(&bare, &["init", "--bare", "-b", "dev"])
            .status
            .success());
        // Canonical repo on "dev".
        assert!(run_at(&canonical, &["init", "-b", "dev"]).status.success());
        std::fs::write(canonical.join("file.txt"), "initial\n").unwrap();
        assert!(run_at(&canonical, &["add", "file.txt"]).status.success());
        assert!(run_at(&canonical, &["commit", "-q", "-m", "initial"])
            .status
            .success());
        // Wire remote + push + set origin/HEAD → origin/dev.
        assert!(run_at(
            &canonical,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        )
        .status
        .success());
        assert!(run_at(&canonical, &["push", "-u", "origin", "dev"])
            .status
            .success());
        assert!(run_at(&canonical, &["remote", "set-head", "origin", "dev"])
            .status
            .success());

        // Sanity: git_helpers::default_branch must resolve to "dev".
        let resolved = crate::git_helpers::default_branch(&canonical);
        assert_eq!(
            resolved, "dev",
            "setup: default_branch must resolve to 'dev'"
        );

        // --- (a) HEAD on "dev" + dirty → MUST produce CanonicalDirtyReport ---
        std::fs::write(canonical.join("scratch.txt"), "dirty\n").unwrap();
        let report = apply_to_canonical(&canonical);
        assert!(
            report.is_some(),
            "#2895 bug (a): canonical dirty on 'dev' (the true default) must report, \
             but hardcoded DEFAULT_BRANCH='main' makes the comparison false"
        );
        // Clean up for part (b).
        std::fs::remove_file(canonical.join("scratch.txt")).unwrap();

        // --- (b) Detach HEAD + clean → must auto-switch to "dev", not "main" ---
        let sha = String::from_utf8(run_at(&canonical, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(
            run_at(&canonical, &["checkout", "-q", &sha])
                .status
                .success(),
            "detach HEAD via checkout SHA"
        );
        let _ = apply_to_canonical(&canonical);
        let head_after =
            String::from_utf8(run_at(&canonical, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
                .unwrap()
                .trim()
                .to_string();
        assert_eq!(
            head_after, "dev",
            "#2895 bug (b): detached clean canonical must auto-switch to 'dev' (the true \
             default), not 'main'"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
