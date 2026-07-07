//! #t-83936-5 — `ensure_branch_exists` create path (local `refs/heads/<branch>`
//! absent → create it), extracted from `dispatch_hook/mod.rs` to keep that handler
//! under its LOC ceiling (Sprint-26 file_size_invariant split pattern). The
//! data-loss guard (prefer an existing `origin/<branch>` over `from_ref`) and its
//! fail-closed state table live inline below, next to the code they govern.

use super::{DispatchError, ErrorCode, Stage, DISPATCH_FETCH_TIMEOUT};
use std::path::Path;

/// Create the local `refs/heads/<branch>` when it does not yet exist. Prefers an
/// existing `origin/<branch>` (the #t-83936-5 data-loss guard), fails CLOSED only
/// when totally blind (no remote-tracking view AND origin unreachable), and
/// otherwise creates from `from_ref` (the #1755 refresh-then-create path). Returns
/// `(n_branch, fetch_attempted)`. `remote` / `from_ref_branch` are the base ref's
/// resolved remote (`resolve_from_ref_remote`), computed once by the caller.
pub(super) fn create_new_branch(
    home: &Path,
    source: &Path,
    branch: &str,
    from_ref: &str,
    actor: &str,
    remote: &str,
    from_ref_branch: Option<&str>,
) -> Result<(bool, bool), DispatchError> {
    // Step 1.5 (#t-83936-5 data-loss, incident-followup — lead-vetted hybrid):
    // the LOCAL ref is absent, but the branch may ALREADY EXIST on the remote — the
    // fresh-canonical-clone / pruned-after-release re-bind scenario (exactly
    // canonical-incident recovery, where dev2 lost commits before catching it with a
    // diff-stat). The "Step 2: create from `from_ref`" arm below bases the new local
    // branch on origin/main and would SILENTLY ORPHAN every commit already on
    // origin/<branch> the moment the agent commits. Mirror the branch-EXISTS path's
    // precedence (origin/<branch> wins over from_ref): if origin/<branch> exists,
    // create the local branch FROM that remote tip so history is preserved. There is
    // NO local ref here, so nothing to clobber — no ff-check (unlike the EXISTS
    // path). `origin` is the working-branch push remote (#2047), correct regardless
    // of a fork `from_ref`.
    //
    // The PRIMARY signal is the remote-tracking VIEW `refs/remotes/origin/<branch>`,
    // not a live fetch: `git clone` populates refs/remotes/origin/* for ALL remote
    // branches, and pruning a local `refs/heads/<branch>` never touches the
    // remote-tracking ref — so in EVERY recovery scenario the view already knows the
    // branch with NO network I/O. We still `git fetch origin` first (best-effort) to
    // (a) advance the view to the current tip when online and (b) discover a branch
    // pushed since our last fetch.
    //
    // FAIL-CLOSED, state-enumerated (#2662 lesson — lead-confirmed 4-state table):
    //   1. view HAS origin/<branch>              → create from it (definitely exists).
    //   2. view none, fetch OK, still none       → origin reachable & confirmed to
    //                                               lack it ⇒ genuinely new ⇒ from_ref.
    //   3. view none, fetch FAILED, view NON-EMPTY (we HAVE synced with origin before
    //      — refs/remotes/origin/* incl. origin/HEAD is populated) → create from
    //      from_ref. This is the ONE fail-OPEN state, DELIBERATELY accepted:
    //        · its residual data-loss window needs THREE conditions stacked — offline
    //          NOW + a stale (non-fresh) clone + the branch pushed to origin only
    //          AFTER this repo's last fetch. The incident is a FRESH clone (view
    //          complete → caught by state 1), so this edge is NOT the incident.
    //        · it is NOT silent: a later push of the wrong-based branch to the
    //          existing origin/<branch> is a non-fast-forward and is REJECTED — a
    //          natural second line of defense + a loud signal.
    //   4. view none, fetch FAILED, view EMPTY (never synced with origin at all)
    //                                            → truly blind ⇒ REFUSE (Err).
    // Asymmetry vs the EXISTS arm (state 1): that arm update-refs the local ref to the
    // tip; here we base on the view SHA, which when OFFLINE may LAG origin's tip. Lag
    // ≠ loss — the base is a real ancestor on the correct branch and the non-ff
    // backstop covers the rest; when online the pre-fetch above advances the view to
    // the tip anyway.
    let work_tracking_ref = format!("refs/remotes/origin/{branch}");
    let work_fetch_ok = crate::git_helpers::git_bypass_timeout(
        source,
        &["fetch", "origin", "--quiet"],
        DISPATCH_FETCH_TIMEOUT,
    )
    .map(|o| o.status.success())
    .unwrap_or(false);
    let work_remote_exists =
        crate::git_helpers::git_bypass(source, &["rev-parse", "--verify", &work_tracking_ref])
            .map(|o| o.status.success())
            .unwrap_or(false);
    if work_remote_exists {
        match crate::git_helpers::git_bypass(source, &["branch", branch, &work_tracking_ref]) {
            Ok(o) if o.status.success() => {
                crate::event_log::log(
                    home,
                    "ensure_branch_from_origin",
                    actor,
                    &format!("branch={branch} based on refs/remotes/origin/{branch} fetched={work_fetch_ok} (#t-83936-5 data-loss guard)"),
                );
                // n_branch=false: the branch pre-existed on the remote; we only
                // materialized the local ref (consistent with the EXISTS path).
                return Ok((false, work_fetch_ok));
            }
            Ok(o) if String::from_utf8_lossy(&o.stderr).contains("already exists") => {
                // Race: a concurrent caller authored the local ref between our
                // rev-parse gate and here. Idempotent — observed, not created.
                return Ok((false, work_fetch_ok));
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                return Err(DispatchError {
                    message: format!(
                        "failed to create '{branch}' from existing origin/{branch}: {}",
                        stderr.trim()
                    ),
                    code: ErrorCode::BranchCreateFailed,
                    stage: Stage::CreateBranch,
                    fetch_attempted: work_fetch_ok,
                    raw: Some(stderr),
                });
            }
            Err(e) => {
                return Err(DispatchError {
                    message: format!("git branch from origin/{branch} spawn failed: {e}"),
                    code: ErrorCode::BranchCreateFailed,
                    stage: Stage::CreateBranch,
                    fetch_attempted: work_fetch_ok,
                    raw: Some(e.to_string()),
                });
            }
        }
    }
    if !work_fetch_ok {
        // origin/<branch> is absent from our view AND we could not refresh it.
        // Discriminate state 3 (fail-open) from state 4 (fail-closed) by whether we
        // have ANY remote-tracking view of origin at all. `refs/remotes/origin/HEAD`
        // counts — `git clone` always populates it, so "have we ever synced with
        // origin?" == "is refs/remotes/origin/* non-empty?".
        let have_origin_view = crate::git_helpers::git_bypass(
            source,
            &["for-each-ref", "--count=1", "refs/remotes/origin/"],
        )
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
        if !have_origin_view {
            // State 4 — FAIL-CLOSED: we have never synced with origin (no
            // remote-tracking view) and cannot reach it now, so we cannot rule out an
            // existing origin/<branch> at all. Creating from `from_ref` could silently
            // orphan its remote commits — the recovery-scenario data-loss this guard
            // prevents. Refuse loudly.
            crate::event_log::log(
                home,
                "ensure_branch_fail_closed",
                actor,
                &format!("branch={branch} from_ref={from_ref} refused: no origin remote-tracking view + fetch failed, cannot rule out existing origin/{branch} (#t-83936-5 data-loss guard)"),
            );
            return Err(DispatchError {
                message: format!(
                    "refusing to provision '{branch}': cannot reach origin and have no \
                     remote-tracking view to confirm whether origin/{branch} already exists. \
                     Creating from '{from_ref}' now could silently orphan existing remote \
                     commits on that branch. Restore connectivity and retry."
                ),
                code: ErrorCode::FetchFailed,
                stage: Stage::Fetch,
                fetch_attempted: false,
                raw: None,
            });
        }
        // State 3 — FAIL-OPEN (the one accepted gap; see the state table above):
        // origin is unreachable NOW but we DO have a remote-tracking view in which
        // origin/<branch> is absent. Create from `from_ref`. The residual data-loss
        // needs offline + stale-clone + a branch pushed since our last fetch to stack
        // (non-incident — the incident is a fresh, complete clone), and a wrong-based
        // push to an existing origin/<branch> is a rejected non-ff (loud 2nd defense).
        crate::event_log::log(
            home,
            "ensure_branch_fail_open",
            actor,
            &format!("branch={branch} from_ref={from_ref}: origin unreachable but remote-tracking view present and lacks origin/{branch}; creating from from_ref (residual-edge, non-ff backstop) (#t-83936-5)"),
        );
    }
    // States 2 & 3: origin reachable & confirmed to lack the branch, or offline with a
    // view that lacks it → genuinely new (for our purposes).
    // Step 2: create from `from_ref`. #1755: a remote-tracking `from_ref` like
    // `origin/main` ALWAYS resolves as a local ref, so a bare `git branch` here
    // silently bases the new branch on a STALE local ref (whatever was last
    // fetched) — the reverse-revert hazard where a fresh checkout starts behind
    // main and would clobber merges that landed since. Refresh the remote ref
    // FIRST (mirrors the #869 branch-EXISTS path above) so the create lands on
    // current `origin/<x>`. Best-effort: a fetch failure (offline / no-remote
    // fixture) leaves the local ref as-is and the create still succeeds against
    // whatever's present (degraded but functional, same contract as #869).
    // `fetch_attempted` reports SUCCESS (matches #869's `fetched_ok`), so the
    // no-remote test fixtures keep reporting `false`.
    let mut create_fetched = false;
    if let Some(remote_branch) = from_ref_branch {
        let fetch_start = std::time::Instant::now();
        let fetch_out = crate::git_helpers::git_bypass_timeout(
            source,
            &[
                "fetch",
                remote,
                &format!("+{remote_branch}:refs/remotes/{remote}/{remote_branch}"),
                "--quiet",
            ],
            DISPATCH_FETCH_TIMEOUT,
        );
        create_fetched = matches!(&fetch_out, Ok(o) if o.status.success());
        crate::event_log::log(
            home,
            "ensure_branch_fetch",
            actor,
            &format!(
                "branch={branch} from_ref={from_ref} duration_ms={} ok={create_fetched} (#1755 pre-create refresh)",
                fetch_start.elapsed().as_millis()
            ),
        );
    }
    match crate::git_helpers::git_bypass(source, &["branch", branch, from_ref]) {
        Ok(o) if o.status.success() => Ok((true, create_fetched)),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            if stderr.contains("already exists") {
                // Race: concurrent caller authored the branch between
                // rev-parse and branch. Idempotent fall-through —
                // auto_created stays false so callers can distinguish
                // "I created it" vs "I observed it pre-existing".
                Ok((false, false))
            } else if stderr.contains("not a valid object name")
                || stderr.contains("not a valid ref")
            {
                tracing::warn!(
                    target: "dispatch_hook",
                    %branch,
                    %from_ref,
                    "ensure_branch_exists fallback: from_ref unresolved locally — fetching origin"
                );
                let fetch_start = std::time::Instant::now();
                let fetch_out = crate::git_helpers::git_bypass_timeout(
                    source,
                    &["fetch", remote, "--quiet"],
                    DISPATCH_FETCH_TIMEOUT,
                );
                let fetch_ms = fetch_start.elapsed().as_millis();
                crate::event_log::log(
                    home,
                    "ensure_branch_fetch",
                    actor,
                    &format!("branch={branch} from_ref={from_ref} duration_ms={fetch_ms}"),
                );
                match fetch_out {
                    Ok(fo) if fo.status.success() => {
                        match crate::git_helpers::git_bypass(source, &["branch", branch, from_ref])
                        {
                            Ok(ro) if ro.status.success() => Ok((true, true)),
                            Ok(ro) => {
                                let rstderr = String::from_utf8_lossy(&ro.stderr).to_string();
                                if rstderr.contains("already exists") {
                                    Ok((false, true))
                                } else {
                                    tracing::warn!(
                                        target: "dispatch_hook",
                                        %branch, %from_ref, stderr = %rstderr,
                                        "ensure_branch_exists retry failed after fetch"
                                    );
                                    Err(DispatchError {
                                        message: format!(
                                            "from_ref '{from_ref}' invalid (branch creation failed after fetch)"
                                        ),
                                        code: ErrorCode::InvalidFromRef,
                                        stage: Stage::RetryCreate,
                                        fetch_attempted: true,
                                        raw: Some(rstderr),
                                    })
                                }
                            }
                            Err(e) => Err(DispatchError {
                                message: format!("git branch retry spawn failed: {e}"),
                                code: ErrorCode::BranchCreateFailed,
                                stage: Stage::RetryCreate,
                                fetch_attempted: true,
                                raw: Some(e.to_string()),
                            }),
                        }
                    }
                    Ok(fo) => {
                        let fstderr = String::from_utf8_lossy(&fo.stderr).to_string();
                        tracing::warn!(
                            target: "dispatch_hook",
                            %branch, %from_ref, stderr = %fstderr,
                            "ensure_branch_exists fetch failed"
                        );
                        Err(DispatchError {
                            message: format!(
                                "git fetch origin failed (from_ref '{from_ref}' cannot be resolved)"
                            ),
                            code: ErrorCode::FetchFailed,
                            stage: Stage::Fetch,
                            fetch_attempted: true,
                            raw: Some(fstderr),
                        })
                    }
                    Err(e) => Err(DispatchError {
                        message: format!("git fetch spawn failed: {e}"),
                        code: ErrorCode::FetchFailed,
                        stage: Stage::Fetch,
                        fetch_attempted: true,
                        raw: Some(e.to_string()),
                    }),
                }
            } else {
                Err(DispatchError {
                    message: format!("git branch failed: {}", stderr.trim()),
                    code: ErrorCode::BranchCreateFailed,
                    stage: Stage::CreateBranch,
                    fetch_attempted: false,
                    raw: Some(stderr),
                })
            }
        }
        Err(e) => Err(DispatchError {
            message: format!("git branch spawn failed: {e}"),
            code: ErrorCode::BranchCreateFailed,
            stage: Stage::CreateBranch,
            fetch_attempted: false,
            raw: Some(e.to_string()),
        }),
    }
}
