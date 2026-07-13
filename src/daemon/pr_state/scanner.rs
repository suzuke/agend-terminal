use std::path::Path;

use super::gh_poll;
use super::{
    apply, format_ready_body, freshness_gate, pr_state_dir, remove, resolve_author,
    resolve_merge_authority, with_pr_state, CiState, DraftState, Event, FreshnessGate, MergeState,
    PrState, ReviewClass, VerdictState, FRESHNESS_TTL_SECS,
};

enum ScanAction {
    None,
    Saved,
    Remove,
}

/// [C1 / #1842] Persistent dedup ensuring a terminal `[pr-merged]` /
/// `[pr-closed-unmerged]` is announced ONCE per merge identity — even when the
/// per-PR state file is `remove`d by the scan terminal-cleanup and then
/// RE-CREATED by a lingering CI observation (`record_ci_result`'s `_or_create`,
/// mod.rs). That delete→recreate loop reset the per-file `ready_emitted_for_sha`
/// flag every poll, re-emitting `[pr-merged]` ~once per poll (#1842: 8× for one
/// merge). Keyed on the terminal-event identity, this ledger survives the file
/// delete; the per-file flag cannot. Pruned by TTL on each record.
const TERMINAL_EMIT_LEDGER_TTL_SECS: i64 = 7 * 24 * 60 * 60;

fn terminal_emit_ledger_path(home: &Path) -> std::path::PathBuf {
    pr_state_dir(home).join(".emitted-terminal.json")
}

/// True if a `[pr-merged]`/`[pr-closed-unmerged]` for `key` was already emitted
/// (lock-free read; a missing/corrupt ledger reads as "not emitted").
fn terminal_already_emitted(home: &Path, key: &str) -> bool {
    std::fs::read_to_string(terminal_emit_ledger_path(home))
        .ok()
        .and_then(|c| serde_json::from_str::<std::collections::HashMap<String, String>>(&c).ok())
        .is_some_and(|m| m.contains_key(key))
}

/// Record `key` as emitted (locked RMW), pruning entries older than the TTL.
fn record_terminal_emitted(home: &Path, key: &str) {
    let now = chrono::Utc::now();
    let _ = crate::store::with_json_state_or_create::<
        std::collections::HashMap<String, String>,
        _,
        _,
        _,
    >(
        &terminal_emit_ledger_path(home),
        std::collections::HashMap::new,
        |m| {
            m.retain(|_, ts| {
                chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|t| {
                        now.signed_duration_since(t.with_timezone(&chrono::Utc))
                            .num_seconds()
                            < TERMINAL_EMIT_LEDGER_TTL_SECS
                    })
                    .unwrap_or(false)
            });
            m.insert(key.to_string(), now.to_rfc3339());
        },
    );
}

/// Per-tick scanner: walks `<home>/pr-state/*.json`, emits any newly-
/// eligible `[pr-ready-for-merge]` events (debounced via
/// `ready_emitted_for_sha`), and sweeps terminal-state files.
///
/// gh-poll for pr_number/pr_author/draft/merge state is fired here
/// (rate-limited — at most one gh call per scanner tick per file).
pub fn scan_and_emit_with(
    home: &Path,
    registry: &crate::agent::AgentRegistry,
    poller: &dyn gh_poll::GhPoller,
) {
    let dir = pr_state_dir(home);
    // #986: Phase 1 — batched gh-poll per repo for files due.
    apply_gh_poll(home, &dir, poller);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "#1002 pr_state: scan_and_emit_with read_dir failed — skipping tick"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !crate::daemon::pr_state::is_pr_state_file(&path) {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 pr_state: scan_and_emit_with read_to_string failed — skipping file"
                );
                continue;
            }
        };
        let snapshot: PrState = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 pr_state: scan_and_emit_with json parse failed — skipping file"
                );
                continue;
            }
        };
        let repo = snapshot.repo.clone();
        let branch = snapshot.branch.clone();

        // #1342: all emit + flag-set under flock to prevent lost-update race.
        // #bughunt3 (#1617 lock-while-blocking class): the worktree auto-release
        // does a `git` subprocess + acquires a SECOND (binding) flock — it must
        // NOT run under the PR-state flock. The closure only records the branch
        // to release; the actual release runs after `with_pr_state` returns and
        // the flock is dropped (see below).
        // t-worktree-leak (PR-1): (branch, event_kind) of a terminal PR transition
        // observed this scan — the release-invariant recompute runs post-flock.
        let mut release_after_unlock: Option<(String, &'static str)> = None;
        // #1629: collect deferred inbox emits here under the flock; drain them
        // AFTER `with_pr_state` returns so enqueue_with_idle_hint (self-IPC via
        // loopback api::call) runs lock-free (#1617 lock-while-blocking class).
        let mut pending_emits: Vec<(String, crate::inbox::InboxMessage)> = Vec::new();
        // The [pr-ready-for-merge] emit is kept SEPARATE from `pending_emits`
        // because — unlike the terminal (Merged/ClosedUnmerged) emits — it has NO
        // persistent ledger backstop. Its optimistic dedup flag
        // (`ready_emitted_for_sha`) must be RESET if the post-flock enqueue fails,
        // or the signal is lost until head_sha next changes. Carries
        // (recipient, msg, head_sha) so the reset is guarded on the same head — a
        // concurrent head advance (which already clears the flag) must not be
        // clobbered. pr-ready and the terminal arms are mutually exclusive per
        // scan (distinct merge_state), so this never coexists with pending_emits.
        let mut pending_ready: Option<(String, crate::inbox::InboxMessage, String)> = None;
        // #2749: [pr-needs-rebase] notices for a proven-BEHIND merge-ready PR.
        // Collected under the flock, delivered AFTER it drops through the durable
        // #2745 ledger. Each carries (recipient, pr_number, head_sha, msg_id, from,
        // msg): the msg id is PRE-STAMPED (crate::inbox::stamp_message_id) so the
        // post-flock wake can point at the exact persisted row. The DURABLE ledger
        // enqueue is a plain storage append (NOT self-IPC); the separate best-effort
        // pointer WAKE is the self-IPC vector — so the wake runs AFTER the flock is
        // dropped (#1617 lock-while-blocking class), and a dropped wake never
        // invalidates the durable row.
        #[allow(clippy::type_complexity)]
        let mut pending_needs_rebase: Vec<(
            String,
            u64,
            String,
            String,
            String,
            crate::inbox::InboxMessage,
        )> = Vec::new();
        // [C1 / #1842] Terminal-event identity for the persistent emit-dedup
        // ledger. Checked lock-free here (before the flock) and recorded
        // post-flock if we emit — so a recreated state file (lingering-CI
        // `_or_create` after the scan `remove`) cannot re-announce the merge.
        let terminal_ledger_key: Option<String> = match &snapshot.merge_state {
            MergeState::Merged { merge_commit, .. } => {
                Some(format!("merged:{repo}@{branch}:{merge_commit}"))
            }
            MergeState::ClosedUnmerged { closed_at } => {
                Some(format!("closed:{repo}@{branch}:{closed_at}"))
            }
            _ => None,
        };
        let ledger_says_emitted = terminal_ledger_key
            .as_deref()
            .is_some_and(|k| terminal_already_emitted(home, k));
        let mut emitted_terminal = false;
        let result = with_pr_state(home, &repo, &branch, |state| {
            let mut dirty = false;

            // Emit [pr-ready-for-merge] if eligible and not already fired.
            if matches!(state.merge_state, MergeState::MergeReady)
                && state.ready_emitted_for_sha.as_deref() != Some(state.head_sha.as_str())
            {
                // #2749 A5 (Fable): a legacy/torn state can carry a stale
                // `merge_state == MergeReady` while `review_class` is Unresolved
                // (files persisted before #2745, or a future off-tick populator
                // stamping a legacy Unresolved watch fresh). `is_merge_ready`
                // already refuses Unresolved, but a PERSISTED MergeReady reaches
                // this arm WITHOUT re-running that reducer — so refuse HERE, before
                // ANY freshness delivery (pr-ready now, or the pr-needs-rebase
                // Behind arm in the next increment). Fail closed: emit nothing. The
                // #2745 [review-class-unresolved] diagnostic below is NotReady-gated,
                // so it does not fire for a MergeReady state either.
                if matches!(state.review_class, ReviewClass::Unresolved) {
                    tracing::debug!(
                        repo = %state.repo,
                        branch = %state.branch,
                        head = %state.head_sha,
                        "#2749 A5 pr_state: freshness delivery suppressed — review_class Unresolved on a MergeReady state"
                    );
                } else {
                    // #2749 (decision d-20260712092257798199-17): read-only freshness
                    // gate. A MergeReady PR may announce [pr-ready-for-merge] ONLY when
                    // deterministic latest-main ancestry is PROVEN fresh at the current
                    // head. The gate is PURE — it reads the freshness cache tuple
                    // stamped by the OFF-TICK populator, doing ZERO provider.compare on
                    // this tick. Unknown / torn observation / stale-past-TTL / error ⇒
                    // fail CLOSED (suppress; #2747's exact-head merge remains the hard
                    // backstop). Behind ⇒ suppress here too; the durable
                    // [pr-needs-rebase] notice is wired in the next increment.
                    match freshness_gate(state, chrono::Utc::now(), FRESHNESS_TTL_SECS) {
                        FreshnessGate::Fresh => {
                            // #2059-#3: ready-for-MERGE routes to the MERGE AUTHORITY
                            // (the team orchestrator via durable fleet.yaml teams), NOT
                            // the binding-resolved author — the implementer releases the
                            // worktree post-push, so the binding-first `resolve_notify_
                            // recipient` falls through to the author by merge-ready time
                            // (the PR #2058 mis-route). `[review-verdict]` keeps the
                            // author-facing resolver; only this terminal signal changes
                            // audience.
                            let recipient = resolve_merge_authority(home, state);
                            let body = format_ready_body(state);
                            let msg =
                                build_event_message("pr-ready-for-merge", &recipient, state, body);
                            // #1629: defer the enqueue (see top of fn). Set the dedup
                            // flag optimistically under the flock. The pr-ready arm has
                            // NO persistent ledger backstop (unlike the Merged/
                            // ClosedUnmerged arms), so the deferred `pending_ready` drain
                            // below RESETS this flag if the post-flock enqueue fails —
                            // otherwise a failed enqueue would leave the flag set and the
                            // signal would be lost until head_sha next changes. The
                            // flock-free emit (the reset is a separate post-flock
                            // `with_pr_state`) preserves the #1617 lock-while-blocking
                            // guarantee.
                            state.ready_emitted_for_sha = Some(state.head_sha.clone());
                            dirty = true;
                            tracing::info!(
                                repo = %state.repo,
                                branch = %state.branch,
                                head = %state.head_sha,
                                recipient = %recipient,
                                "#972 pr_state: [pr-ready-for-merge] queued (emit after flock drop)"
                            );
                            pending_ready = Some((recipient, msg, state.head_sha.clone()));
                        }
                        FreshnessGate::Behind { behind_by } => {
                            // #2749: PROVEN behind current main — suppress pr-ready
                            // (ready flag untouched; #2747 stays the hard backstop)
                            // and durably emit ONE [pr-needs-rebase] per (repo, PR,
                            // head, recipient) to the DEDUPED {merge authority, PR
                            // owner}. For a valid Behind tuple observed_base ==
                            // checked_base, so observed_base_sha is the current-main
                            // tip. Deferred post-flock like pending_ready.
                            let main_sha = state
                                .observed_base_sha
                                .clone()
                                .or_else(|| state.freshness_checked_base_sha.clone())
                                .unwrap_or_default();
                            let body = format_needs_rebase_body(state, behind_by, &main_sha);
                            // Dedupe recipients BEFORE the ledger keys (owner ==
                            // authority ⇒ one notice, not two).
                            let mut recipients =
                                vec![resolve_merge_authority(home, state), resolve_author(state)];
                            recipients.sort();
                            recipients.dedup();
                            for recipient in recipients {
                                let mut msg = build_event_message(
                                    "pr-needs-rebase",
                                    &recipient,
                                    state,
                                    body.clone(),
                                );
                                // Pre-stamp the id so the post-flock wake can point
                                // at the exact row the ledger closure enqueues.
                                let id = crate::inbox::stamp_message_id(&mut msg);
                                let from = msg.from.clone();
                                pending_needs_rebase.push((
                                    recipient,
                                    state.pr_number,
                                    state.head_sha.clone(),
                                    id,
                                    from,
                                    msg,
                                ));
                            }
                            tracing::info!(
                                repo = %state.repo,
                                branch = %state.branch,
                                head = %state.head_sha,
                                behind_by,
                                "#2749 pr_state: [pr-needs-rebase] queued (behind main; deliver after flock drop)"
                            );
                        }
                        FreshnessGate::Suppress => {
                            tracing::debug!(
                                repo = %state.repo,
                                branch = %state.branch,
                                head = %state.head_sha,
                                "#2749 pr_state: [pr-ready-for-merge] suppressed — latest-main ancestry not proven fresh"
                            );
                        }
                    }
                }
            }

            // #2745 fail-closed: a would-be-ready state whose review_class is
            // `Unresolved` (a legacy `None` watch, or one armed before this fix)
            // can NEVER open the merge gate. Surface an actionable re-arm
            // diagnostic to the merge authority INSTEAD of an (absent) pr-ready,
            // once per head_sha (debounced via `diagnostic_emitted_for_sha`, a
            // field kept separate from `ready_emitted_for_sha` so it never touches
            // terminal-replay suppression). Gated on CI-green ∧ VERIFIED at head —
            // the exact point pr-ready would have fired under an explicit class — so
            // it fires only when the unresolved class is actually blocking a merge,
            // not on every freshly-armed watch. This is the "legacy None inventory"
            // (decision d-…-11): each blocked watch announces itself for re-arm.
            if matches!(state.review_class, ReviewClass::Unresolved)
                && matches!(state.merge_state, MergeState::NotReady)
                && matches!(&state.ci_state, CiState::Green { sha, .. } if sha == &state.head_sha)
                && matches!(&state.verdict_state, VerdictState::Verified { .. })
                && state.diagnostic_emitted_for_sha.as_deref() != Some(state.head_sha.as_str())
            {
                let recipient = resolve_merge_authority(home, state);
                let sha_short = &state.head_sha[..8.min(state.head_sha.len())];
                let body = format!(
                    "[review-class-unresolved] {}@{} (head {sha_short}): CI green ∧ VERIFIED, but \
                     the ci-watch review_class is UNRESOLVED (absent/unknown/typo) — the merge gate \
                     is fail-closed and will NOT open (#2745). Re-arm with an explicit threshold: \
                     `ci action=watch repository={} branch={} review_class=single|dual`.",
                    state.repo, state.branch, state.repo, state.branch,
                );
                let msg = build_event_message("review-class-unresolved", &recipient, state, body);
                pending_emits.push((recipient, msg));
                state.diagnostic_emitted_for_sha = Some(state.head_sha.clone());
                dirty = true;
                tracing::warn!(
                    repo = %state.repo,
                    branch = %state.branch,
                    head = %state.head_sha,
                    "#2745 pr_state: [review-class-unresolved] queued — legacy/absent review_class; \
                     watch needs explicit re-arm (no merge-ready possible until then)"
                );
            }

            // Terminal-state sweep.
            let already_emitted =
                state.ready_emitted_for_sha.as_deref() == Some(state.head_sha.as_str());
            match &state.merge_state {
                MergeState::Merged {
                    merge_commit,
                    merged_at,
                } => {
                    // [C1] also suppress if the persistent ledger already recorded
                    // this merge — survives the delete→recreate that resets the
                    // per-file `ready_emitted_for_sha`.
                    if !already_emitted && !ledger_says_emitted {
                        // #1344/#bughunt3: defer the worktree auto-release until
                        // AFTER this flock is dropped (the git subprocess +
                        // nested binding flock are the #1617 lock-while-blocking
                        // class). Record the branch; release runs post-unlock.
                        release_after_unlock = Some((state.branch.clone(), "merge"));
                        let author = resolve_author(state);
                        let body = format!(
                            "[pr-merged] {}@{} (merge_commit {}, merged_at {})\n\n\
                             ⚠ Action checklist:\n\
                             1. `gh issue close` (if linked issue)\n\
                             2. `task action=done` (if correlation_id present)\n\
                             3. Report completion to lead",
                            state.repo,
                            state.branch,
                            &merge_commit[..8.min(merge_commit.len())],
                            merged_at,
                        );
                        // #1629: defer the enqueue (see top of fn) — run lock-free
                        // after the flock drops.
                        let msg = build_event_message("pr-merged", &author, state, body);
                        pending_emits.push((author, msg));
                        state.ready_emitted_for_sha = Some(state.head_sha.clone());
                        emitted_terminal = true; // [C1] record in ledger post-flock
                        dirty = true;
                    } else {
                        tracing::debug!(
                            repo = %state.repo,
                            branch = %state.branch,
                            head = %state.head_sha,
                            ledger_says_emitted,
                            "#1017/#1842 pr_state: Merged replay suppressed at scan"
                        );
                        return ScanAction::Remove;
                    }
                }
                MergeState::ClosedUnmerged { closed_at } => {
                    if !already_emitted && !ledger_says_emitted {
                        let author = resolve_author(state);
                        let body = format!(
                            "[pr-closed-unmerged] {}@{} (closed_at {})\n\n\
                             ⚠ Action checklist:\n\
                             1. `release_worktree` for branch `{}`\n\
                             2. Investigate closure reason (operator decision? superseded?)\n\
                             3. Report to lead with context",
                            state.repo, state.branch, closed_at, state.branch,
                        );
                        // #1629: defer the enqueue (see top of fn) — run lock-free
                        // after the flock drops.
                        let msg = build_event_message("pr-closed-unmerged", &author, state, body);
                        pending_emits.push((author, msg));
                        state.ready_emitted_for_sha = Some(state.head_sha.clone());
                        emitted_terminal = true; // [C1] record in ledger post-flock
                                                 // t-worktree-leak (PR-1): close-unmerged is a terminal PR
                                                 // transition → recompute the release invariant post-flock
                                                 // (the sweeper applies the conservative close-grace).
                        release_after_unlock = Some((state.branch.clone(), "close_unmerged"));
                        dirty = true;
                    } else {
                        tracing::debug!(
                            repo = %state.repo,
                            branch = %state.branch,
                            head = %state.head_sha,
                            ledger_says_emitted,
                            "#1017/#1842 pr_state: ClosedUnmerged replay suppressed at scan"
                        );
                        return ScanAction::Remove;
                    }
                }
                _ => {}
            }

            if dirty {
                ScanAction::Saved
            } else {
                ScanAction::None
            }
        });

        // #1629 (#1617 class): PR-state flock is now released — drain the deferred
        // inbox emits lock-free (enqueue_with_idle_hint self-IPCs via loopback
        // api::call). Emit BEFORE auto_release to preserve the prior
        // under-flock-emit → post-unlock-release ordering.
        for (author, msg) in pending_emits {
            if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &author, msg) {
                tracing::warn!(
                    author = %author,
                    error = %e,
                    "#1629 pr_state: deferred emit enqueue failed"
                );
            }
        }
        // pr-ready has no ledger backstop: if its deferred enqueue fails, RESET
        // the optimistic `ready_emitted_for_sha` dedup flag (guarded on the same
        // head_sha so a concurrent head advance — which already cleared the flag —
        // is not clobbered) so the next scan tick re-emits. This is the pr-ready
        // analogue of the terminal arms' persistent-ledger recovery.
        if let Some((author, msg, ready_sha)) = pending_ready {
            if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &author, msg) {
                tracing::warn!(
                    author = %author,
                    error = %e,
                    "#1629 pr_state: deferred [pr-ready-for-merge] enqueue failed — resetting dedup flag for retry"
                );
                let _ = with_pr_state(home, &repo, &branch, |s| {
                    if s.ready_emitted_for_sha.as_deref() == Some(ready_sha.as_str()) {
                        s.ready_emitted_for_sha = None;
                    }
                });
            }
        }

        // #2749 (#1617 class): deliver the deferred [pr-needs-rebase] notices
        // lock-free. deliver_once is at-least-once (enqueue-before-record): a
        // duplicate key SUPPRESSES, a missing key (fresh head, or a restart without
        // the record) DELIVERS — so N ticks emit exactly once per (repo, PR, head,
        // recipient), a head move rekeys, and a restart with the record stays quiet.
        // The DURABLE enqueue is a plain storage append; the recipient is then WOKEN
        // by a SEPARATE best-effort canonical pointer (the self-IPC vector) ONLY when
        // the row is guaranteed persisted (Delivered | RecordFailedAfterEnqueue —
        // see `wake_after_ledger`). A dropped wake is logged and NEVER invalidates
        // the durable delivery (the recipient still sees the row on its next drain).
        for (recipient, pr_number, head_sha, id, from, msg) in pending_needs_rebase {
            let key = match crate::daemon::ci_delivery_ledger::DeliveryKey::new(
                &repo,
                pr_number,
                &head_sha,
                &recipient,
                "pr-needs-rebase",
            ) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        head = %head_sha,
                        recipient = %recipient,
                        "#2749 pr_state: [pr-needs-rebase] invalid delivery key — skipping notice"
                    );
                    continue;
                }
            };
            let deliver_to = recipient.clone();
            let result = crate::daemon::ci_delivery_ledger::deliver_once(
                home,
                &key,
                chrono::Utc::now(),
                // Durable storage append only — NOT self-IPC (the wake below is).
                || crate::inbox::enqueue(home, &deliver_to, msg),
            );
            // Wake the recipient ONLY when the row is durably persisted.
            if wake_after_ledger(&result) {
                if let Err(e) = crate::inbox::wake_persisted_pointer(
                    home,
                    &recipient,
                    &id,
                    "pr-needs-rebase",
                    &from,
                ) {
                    tracing::warn!(
                        error = %e,
                        recipient = %recipient,
                        head = %head_sha,
                        "#2749 pr_state: [pr-needs-rebase] pointer wake dropped — delivery remains durable"
                    );
                }
            }
            match &result {
                Ok(outcome) => tracing::info!(
                    recipient = %recipient,
                    head = %head_sha,
                    ?outcome,
                    "#2749 pr_state: [pr-needs-rebase] ledger delivery"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    recipient = %recipient,
                    "#2749 pr_state: [pr-needs-rebase] deliver_once failed"
                ),
            }
        }

        // #bughunt3 (#1617 class): PR-state flock is now released — run the
        // release-invariant recompute lock-free. t-worktree-leak (PR-1): merge
        // keeps the named entry (the #1617 invariant test pins it); close-unmerged
        // enqueues through the same HYBRID path with its own event kind.
        if let Some((branch, event_kind)) = release_after_unlock {
            match event_kind {
                "merge" => crate::daemon::auto_release::auto_release_for_merged_branch(
                    home, &repo, &branch,
                ),
                _ => crate::daemon::auto_release::enqueue_release_recompute(
                    home, &repo, &branch, event_kind,
                ),
            }
        }

        match result {
            Ok(Some(ScanAction::Remove)) => {
                let _ = remove(home, &repo, &branch);
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo,
                    branch = %branch,
                    error = %e,
                    "#972 pr_state: post-emit save failed"
                );
            }
            _ => {}
        }
        // [C1 / #1842] Record the terminal emit in the persistent ledger AFTER the
        // flock drops (mirrors the deferred enqueue — keeps file I/O off the
        // PR-state lock). Done regardless of Saved/Remove: the announce happened.
        if emitted_terminal {
            if let Some(k) = &terminal_ledger_key {
                record_terminal_emitted(home, k);
            }
        }
        // #1888 phase-2: a PR reaching a terminal state (merged / closed)
        // resolves any pending ci-handoff track for it — the review obligation
        // is gone, the re-nudge stops. Post-flock (file delete, no locks) and
        // keyed on the snapshot's terminal state (NOT `emitted_terminal`) so a
        // replay-suppressed terminal still cleans a lingering track. Idempotent
        // — usually the reviewer's verdict report already resolved it.
        if terminal_ledger_key.is_some() {
            let _ = crate::daemon::ci_handoff_track::resolve_by_correlation(
                home,
                &format!("{repo}@{branch}"),
                "pr_terminal",
            );
        }
        // #t-92758 P1(a): a merge-BLOCKED-but-still-open PR (REJECTED verdict or
        // Draft) also resolves any pending ci-handoff track — the chain target
        // can't merge/act on it, so the ~2-min re-nudge is pure noise (#2297:
        // REJECTED is not a terminal PR state and the head doesn't move, so none
        // of the other resolvers fire). Symmetric with the terminal resolve above;
        // idempotent (no-op if already gone or no track). VERIFIED/green/None are
        // untouched — the normal "your turn / should-merge" ci-ready + re-nudge is
        // preserved (is_ci_ready_merge_blocked iron rule).
        if super::is_ci_ready_merge_blocked(&snapshot) {
            let _ = crate::daemon::ci_handoff_track::resolve_by_correlation(
                home,
                &format!("{repo}@{branch}"),
                "pr_merge_blocked",
            );
        }
        let _ = registry; // reserved for future gh-poll author lookup hook
    }
}

/// #986: batched gh-poll feeder. Groups PrState files by repo, issues
/// ONE `gh pr list` per repo with at least one file due for refresh,
/// then applies each PR's metadata back to its matching file by
/// `head_ref → branch`. Tiered cadence (15s armed / 60s default) +
/// exponential backoff on failures (`2^failures × tick` capped 300s).
///
/// Failures bump per-PrState `gh_poll_failures` (per-repo failures
/// would over-suppress); success clears the counter. Idempotent:
/// re-applying the same metadata is a no-op for the reducer if state
/// already matches.
/// #986 Bug A: is a poll taken at `polled_at` fresh enough to apply against a
/// state last advanced at `state_advanced_at`? True iff the poll happened at/after
/// the state's last advance, so the poll would have observed the current head.
///
/// The anchor is `updated_at` (bumped on every reducer event, INCLUDING a head
/// advance), NOT the immutable `created_at`. `created_at` only covers the
/// cold-start race (a snapshot predating branch tracking); it never moves when
/// the branch HEAD advances (force-push / head-reuse), so a snapshot polled after
/// `created_at` but before a head advance would wrongly read as fresh and could
/// drive a sticky terminal transition (e.g. an old `Closed` for a since-reopened
/// PR → false release). Anchoring on `updated_at` rejects those head-stale polls.
/// For a freshly-tracked branch `updated_at == created_at`, so the cold-start
/// guarantee is preserved unchanged.
///
/// Parse failure → conservative `false` (treat as stale → ambiguous, never a
/// false no-PR / false terminal).
fn poll_is_fresh_for(polled_at: &str, state_advanced_at: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(polled_at),
        chrono::DateTime::parse_from_rfc3339(state_advanced_at),
    ) {
        (Ok(p), Ok(c)) => p >= c,
        _ => false,
    }
}

fn apply_gh_poll(home: &Path, dir: &Path, poller: &dyn gh_poll::GhPoller) {
    use std::collections::{HashMap, HashSet};

    // PR-3 (t-ci-ready-pr3-arm-not-armed): the pr_state dir may be ABSENT (cold
    // start / no PR tracked yet). That must NOT skip the gh-poll — the
    // bound-branch seed below still drives discovery of unwatched open PRs. So
    // tolerate a missing/unreadable dir (empty iterator) instead of early
    // returning, which previously bypassed the whole auto-arm path.
    let entries = std::fs::read_dir(dir).ok();
    let now = chrono::Utc::now().to_rfc3339();
    // Group files by repo + collect those due for refresh.
    let mut by_repo: HashMap<String, Vec<PrState>> = HashMap::new();
    // PR-3 (t-ci-ready-pr3-arm-not-armed): every repo with a non-terminal
    // pr-state is already cadence-managed (it polls on its own `should_poll`
    // schedule). Track them so the bound-branch seed below does NOT force an
    // extra poll on an already-known repo (which would defeat `should_poll`).
    let mut seen_repos: HashSet<String> = HashSet::new();
    let mut skipped_terminal = 0u32;
    let mut skipped_should_poll = 0u32;
    for entry in entries.into_iter().flatten().flatten() {
        let path = entry.path();
        if !crate::daemon::pr_state::is_pr_state_file(&path) {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 apply_gh_poll read_to_string failed — skipping file"
                );
                continue;
            }
        };
        let state: PrState = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#1002 apply_gh_poll json parse failed — skipping file"
                );
                continue;
            }
        };
        // Skip already-terminal states — they'll be swept by the
        // main scanner loop on this pass.
        if matches!(
            state.merge_state,
            MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
        ) {
            skipped_terminal = skipped_terminal.saturating_add(1);
            continue;
        }
        // Non-terminal pr-state → this repo is cadence-managed already.
        seen_repos.insert(state.repo.clone());
        if gh_poll::should_poll(&state, &now) {
            by_repo.entry(state.repo.clone()).or_default().push(state);
        } else {
            skipped_should_poll = skipped_should_poll.saturating_add(1);
        }
    }

    // PR-3 (t-ci-ready-pr3-arm-not-armed): seed the poll-repo list from LIVE
    // BOUND BRANCHES whose repo has NO non-terminal pr-state yet. This is the
    // discovery path #1782 needs — a bypass / non-dispatch PR has neither a
    // ci-watch nor a pr-state, so its repo would never be polled from pr-state
    // alone. Each bound agent's binding.json carries the `source_repo`; resolve
    // it to a gh slug and poll it (empty `due_states` → the poll just feeds
    // `auto_arm_unwatched_open_prs`). `seen_repos` keeps this from re-polling a
    // repo the cadence already manages.
    for src_path in crate::binding::bound_source_repos(home) {
        if let Some(slug) =
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(&src_path)
        {
            if !seen_repos.contains(&slug) {
                by_repo.entry(slug).or_default();
            }
        }
    }
    if !by_repo.is_empty() || skipped_terminal > 0 || skipped_should_poll > 0 {
        tracing::debug!(
            repos_to_poll = by_repo.len(),
            skipped_terminal,
            skipped_should_poll_cadence = skipped_should_poll,
            "#1002 apply_gh_poll grouping done"
        );
    }
    for (repo, due_states) in by_repo {
        match poller.poll(&repo) {
            Ok((prs, polled_at)) => {
                for state in due_states {
                    let branch = state.branch.clone();
                    // #986 Bug A (codex round-3): freshness gates ALL state-changing
                    // observations UNIFORMLY — not just "no PR found". A stale
                    // snapshot (polled BEFORE this branch was first tracked) is
                    // applied to NOTHING: not a no-PR confirmation, and NOT a
                    // found-PR transition. The earlier `found ||` bypass let a stale
                    // found-PR — e.g. an old `Closed` for a since-reopened PR — drive
                    // a STICKY terminal transition (ClosedUnmergedObserved) →
                    // false-release. Async snapshots introduce this staleness (the
                    // pre-#986 synchronous poll was always fresh). Stale → leave the
                    // branch due + ambiguous; a fresh poll arrives within ~1 worker
                    // cadence (~15s) and only then applies observations + stamps.
                    if !poll_is_fresh_for(&polled_at, &state.updated_at) {
                        tracing::debug!(
                            repo = %repo, branch = %branch,
                            polled_at = %polled_at, updated_at = %state.updated_at,
                            "#986 gh-poll: stale snapshot predates last state advance (head-reuse / cold-start) — applying nothing, awaiting fresh poll"
                        );
                        continue;
                    }
                    if let Err(e) = with_pr_state(home, &repo, &branch, |s| {
                        apply_gh_observations(home, s, &prs, &now);
                        s.gh_poll_failures = 0;
                        s.last_gh_poll_at = Some(now.clone());
                    }) {
                        tracing::warn!(
                            repo = %repo,
                            branch = %branch,
                            error = %e,
                            "#986 pr_state: post-gh-poll save failed"
                        );
                    }
                }
                // #986 round-4: `gc_remote_orphans` (DESTRUCTIVE — #1750-B4) and
                // `auto_arm` (#1782 / PR-3) MOVED OUT of this stale-snapshot scanner
                // path into `gh_poll::worker_poll_and_act`, where they run on the
                // worker's FRESH poll. A stale snapshot here could have driven
                // `delete_remote_ref` against a since-reused live branch (Merged PR
                // branch-reuse). The scanner now only does the per-branch,
                // freshness-gated state apply above.
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "#986 gh-poll failed");
                for state in due_states {
                    let branch = state.branch.clone();
                    let _ = with_pr_state(home, &repo, &branch, |s| {
                        s.gh_poll_failures = s.gh_poll_failures.saturating_add(1);
                        s.last_gh_poll_at = Some(now.clone());
                        // #2749: a transport failure means we could NOT re-observe
                        // the head/base tips — the gate must fail closed. Flag it
                        // but do NOT advance `observed_at` and do NOT clobber the
                        // last-good observed pair (CORRECTION 3 / GO-proof).
                        s.observed_error = true;
                    });
                }
            }
        }
    }
}

/// Apply gh-poll observations to a single PrState. Detects state
/// transitions and dispatches the appropriate reducer events:
/// - `state=MERGED + mergedAt!=None` → `MergedObserved`
/// - `state=CLOSED + mergedAt=None` → `ClosedUnmergedObserved`
/// - `isDraft` toggle → `DraftTransition`
/// - First observation: populate `pr_number` + `pr_author` from the
///   matching metadata (no Event needed; direct field assignment).
fn apply_gh_observations(
    home: &Path,
    state: &mut PrState,
    prs: &[gh_poll::GhPrMetadata],
    now: &str,
) {
    let Some(meta) = prs.iter().find(|m| m.head_ref == state.branch) else {
        return;
    };

    // #2749: ATOMIC observation — write the head + base tips from the SAME gh
    // response TOGETHER (never a torn two-read compose; CORRECTION 3 / codex R2).
    // Only write when BOTH OIDs are present: GitHub always surfaces them, so
    // production always refreshes; a provider (or older gh) that omits them leaves
    // the last observation untouched so it simply ages out past the gate TTL
    // (fail-closed) rather than being clobbered into a half-pair. A fresh good
    // observation also CLEARS any prior `observed_error` (e.g. from an earlier
    // transport failure).
    if let (Some(h), Some(b)) = (meta.head_ref_oid.as_deref(), meta.base_ref_oid.as_deref()) {
        // #2749 correction (codex): an observed (head/base) tuple CHANGE makes any
        // prior freshness_error + retry lease STALE (they were for the old tuple) —
        // discard them so the off-tick populator re-attempts the NEW tuple
        // immediately instead of waiting out a lease that no longer applies.
        let tuple_changed = state.observed_head_sha.as_deref() != Some(h)
            || state.observed_base_sha.as_deref() != Some(b);
        state.observed_head_sha = Some(h.to_string());
        state.observed_base_sha = Some(b.to_string());
        state.observed_at = Some(now.to_string());
        state.observed_error = false;
        if tuple_changed {
            state.freshness_error = false;
            state.freshness_retry_after = None;
        }
    }

    // First observation — populate identity fields.
    if state.pr_author.is_empty() {
        state.pr_number = meta.number;
        state.pr_author = gh_poll::resolve_author_with_gh(home, Some(&meta.author_login), state);
        tracing::info!(
            repo = %state.repo,
            branch = %state.branch,
            pr_number = state.pr_number,
            pr_author = %state.pr_author,
            "#986 pr_state: first-observation populated PR identity"
        );
    }

    // Draft transition.
    let new_draft = meta.is_draft;
    let old_draft = matches!(state.draft_state, DraftState::Draft);
    if new_draft != old_draft {
        apply(
            state,
            Event::DraftTransition {
                is_draft: new_draft,
            },
        );
    }

    // Terminal state transitions.
    //
    // #2131: a `state=CLOSED + mergedAt=None` observation is AMBIGUOUS under
    // squash-merge eventual consistency — gh transiently reports it before the
    // merge-commit association lands and `mergedAt` flips. Classifying it
    // ClosedUnmerged on FIRST sight emitted a false (action-bearing)
    // `[pr-closed-unmerged]` for a merged PR (the emit is latched once-per-identity,
    // so a later `merged=true` can't retract the inbox signal). So a first
    // closed-unmerged observation only DEFERS (sets `closed_unmerged_pending`); the
    // grace block below confirms it only if a SUBSEQUENT poll STILL reports
    // closed-unmerged. A `MERGED` observation resolves the lag. `was_pending` is
    // captured BEFORE this poll's processing, so the poll that first sets pending
    // never confirms in the same pass.
    let was_pending = state.closed_unmerged_pending;
    if let Some(prev) = state.last_gh_state.as_ref() {
        if prev.state != meta.state {
            match (meta.state, meta.merged_at.as_deref()) {
                (gh_poll::GhPrState::Merged, Some(merged_at)) => {
                    apply(
                        state,
                        Event::MergedObserved {
                            // gh CLI doesn't return the merge commit hash
                            // in `pr list`; use head_sha as best-effort
                            // identifier. Operator can `gh pr view` for
                            // the real commit hash.
                            merge_commit: &state.head_sha.clone(),
                            merged_at: merged_at.to_string(),
                        },
                    );
                }
                (gh_poll::GhPrState::Closed, _) if meta.merged_at.is_none() => {
                    // #2131: DEFER, don't emit — confirmed by the grace block below.
                    state.closed_unmerged_pending = true;
                }
                _ => {}
            }
        }
    } else {
        // First observation — also catches case where PR was already
        // merged before we started watching.
        match (meta.state, meta.merged_at.as_deref()) {
            (gh_poll::GhPrState::Merged, Some(merged_at)) => {
                apply(
                    state,
                    Event::MergedObserved {
                        merge_commit: &state.head_sha.clone(),
                        merged_at: merged_at.to_string(),
                    },
                );
            }
            (gh_poll::GhPrState::Closed, _) if meta.merged_at.is_none() => {
                state.closed_unmerged_pending = true;
            }
            _ => {}
        }
    }

    // #2131: confirm-or-clear the deferred closed-unmerged.
    let closed_unmerged_now =
        matches!(meta.state, gh_poll::GhPrState::Closed) && meta.merged_at.is_none();
    if !closed_unmerged_now {
        // MERGED / reopened / draft — the lag resolved or the PR isn't closing.
        state.closed_unmerged_pending = false;
    } else if was_pending
        && !matches!(
            state.merge_state,
            MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
        )
    {
        // Two consecutive closed-unmerged observations → the close is real → emit.
        apply(
            state,
            Event::ClosedUnmergedObserved {
                closed_at: now.to_string(),
            },
        );
        state.closed_unmerged_pending = false;
    }

    state.last_gh_state = Some(meta.clone());
}

/// #2749: the [pr-needs-rebase] notice body for a proven-BEHIND merge-ready PR.
/// Carries the PR ref, the head SHA + the current-main SHA it trails, the
/// behind-by count, and a reviewer re-stamp checklist — a rebase INVALIDATES the
/// prior verdict, so ancestry + fresh full CI + fresh exact-head review must all
/// be re-established at the NEW head before the PR can merge (a stale verdict does
/// NOT carry across a rebase). `behind_by` and `main_sha` come from the proven
/// freshness tuple (the gate returned `Behind`), so they are non-zero / non-empty.
fn format_needs_rebase_body(state: &PrState, behind_by: u64, main_sha: &str) -> String {
    let pr_id = if state.pr_number > 0 {
        format!("{}#{}", state.repo, state.pr_number)
    } else {
        format!("{}@{}", state.repo, state.branch)
    };
    let head_short = &state.head_sha[..8.min(state.head_sha.len())];
    let main_short = &main_sha[..8.min(main_sha.len())];
    format!(
        "[pr-needs-rebase] {pr_id} is BEHIND main by {behind_by} commit(s) — head \
         {head_short} trails main {main_short}. Merge-ready gating is SUPPRESSED \
         until the head is rebased onto current main.\n\n\
         ⚠ Re-stamp checklist (a rebase INVALIDATES the prior verdict):\n\
         1. Rebase `{branch}` onto latest main and force-push.\n\
         2. Wait for FRESH full CI green on the rebased head.\n\
         3. Obtain FRESH exact-head review re-verification (dual for high-risk).\n\
         4. Do NOT merge on the stale verdict — ancestry ∧ CI ∧ review must all be \
         at the NEW head.",
        branch = state.branch,
    )
}

fn build_event_message(
    kind: &str,
    _author: &str,
    state: &PrState,
    body: String,
) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage::new_system("system:pr-state", kind, body)
        // #946 grep target: `{repo}@{branch}` canonical form
        .with_correlation_id(format!("{}@{}", state.repo, state.branch))
        .with_reviewed_head(state.head_sha.clone())
}

/// #2749 wake policy for the durable `[pr-needs-rebase]` delivery. A best-effort
/// PTY pointer wake fires ONLY when the row is guaranteed to be durably persisted
/// already: `Delivered` (enqueued + recorded) OR `RecordFailedAfterEnqueue`
/// (enqueued, only the dedup-record write failed — the row EXISTS so the recipient
/// must still be woken; the missing record just means a later tick may re-deliver,
/// which the at-least-once ledger tolerates). NO wake on `Suppressed` (a prior tick
/// already delivered + woke this exact key) or `EnqueueFailed` (no row persisted —
/// nothing to point at). A wake failure never invalidates the durable delivery.
fn wake_after_ledger(
    res: &Result<
        crate::daemon::ci_delivery_ledger::DeliveryOutcome,
        crate::daemon::ci_delivery_ledger::DeliveryError,
    >,
) -> bool {
    use crate::daemon::ci_delivery_ledger::{DeliveryError, DeliveryOutcome};
    matches!(
        res,
        Ok(DeliveryOutcome::Delivered) | Err(DeliveryError::RecordFailedAfterEnqueue(_))
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::gh_poll::tests::MockGhPoller;
    use super::super::gh_poll::{GhPrMetadata, GhPrState};
    use super::super::{
        freshness_gate, load, new_for_branch, save, CiState, FreshnessGate, MergeState,
        ReviewClass, VerdictState,
    };
    use super::scan_and_emit_with;

    fn empty_registry() -> crate::agent::AgentRegistry {
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    /// #2749 test helper: an otherwise fully MergeReady PR (CI green + VERIFIED
    /// at `head`, not draft) with an EMPTY freshness cache. Callers stamp /
    /// mutate the freshness+observed fields to exercise each gate branch.
    fn merge_ready_state(repo: &str, branch: &str, head: &str, pr: u64) -> super::super::PrState {
        let mut s = new_for_branch(repo, branch, head, ReviewClass::Single);
        s.pr_number = pr;
        s.ci_state = CiState::Green {
            sha: head.into(),
            observed_at: chrono::Utc::now().to_rfc3339(),
        };
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("r".into(), head.into())],
        };
        s.merge_state = MergeState::MergeReady;
        s
    }

    /// Stamp a VALID freshness tuple onto `s`: three heads agree at `head`,
    /// checked base == observed base == `base`, no error, both timestamps == now,
    /// the given `behind_by` — so `freshness_gate` returns `Fresh` (behind_by==0)
    /// or `Behind` (behind_by>0).
    fn stamp_fresh_tuple(s: &mut super::super::PrState, head: &str, base: &str, behind_by: u64) {
        // Stamp 1s in the PAST. In production the off-tick populator / gh_poll
        // writes these timestamps BEFORE the scanner tick reads them, so the gate
        // sees a positive age. It also keeps the pure-classifier unit tests robust
        // under the strict `0 <= age <= ttl` gate (they capture the gate's `now`
        // up front, before this stamp — a same-instant stamp would otherwise read
        // marginally in the future and fail closed).
        let now = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        s.observed_head_sha = Some(head.into());
        s.observed_base_sha = Some(base.into());
        s.observed_at = Some(now.clone());
        s.observed_error = false;
        s.freshness_checked_head_sha = Some(head.into());
        s.freshness_checked_base_sha = Some(base.into());
        s.freshness_checked_at = Some(now);
        s.freshness_behind_by = Some(behind_by);
        s.freshness_error = false;
    }

    fn open_pr_meta(number: u64, branch: &str) -> GhPrMetadata {
        // A live, open, non-draft PR matching the tracked branch — keeps the
        // snapshot OPEN through `apply_gh_poll` (an EMPTY poll would drive it
        // terminal and resolve the track via the wrong path). gh metadata never
        // carries the review verdict, so verdict_state is untouched.
        GhPrMetadata {
            number,
            author_login: "dev".into(),
            head_ref: branch.into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
            head_ref_oid: None,
            base_ref_oid: None,
        }
    }

    /// #t-92758 P1(a): a REJECTED-but-open PR resolves its pending ci-handoff
    /// track on the next scan — the #2297 noise root cause (REJECTED is not a
    /// terminal PR state, so none of the prior resolvers fired and the watchdog
    /// re-nudged every ~2 min).
    #[test]
    fn scan_evicts_ci_handoff_track_for_rejected_pr() {
        let home = std::env::temp_dir().join(format!(
            "agend-92758-scan-evict-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let mut s = new_for_branch("o/r", "b", "abcdef0", ReviewClass::Single);
        s.pr_number = 42;
        s.verdict_state = VerdictState::Rejected {
            reviewer: "r".into(),
            reviewed_head: "abcdef0".into(),
            reason: None,
        };
        save(&home, &s).unwrap();
        crate::daemon::ci_handoff_track::record(
            &home,
            "lead",
            "o/r@b",
            &chrono::Utc::now().to_rfc3339(),
            Some("abcdef0"),
            None,
        );

        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(42, "b")])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        assert!(
            crate::daemon::ci_handoff_track::list(&home).is_empty(),
            "REJECTED PR must evict the ci-handoff track (#2297 noise fix)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-92758 IRON RULE (end-to-end): a VERIFIED PR must KEEP its ci-handoff
    /// track — the normal "your turn / should-merge" handoff + re-nudge survives
    /// the new eviction path.
    #[test]
    fn scan_keeps_ci_handoff_track_for_verified_pr() {
        let home = std::env::temp_dir().join(format!(
            "agend-92758-scan-keep-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let mut s = new_for_branch("o/r", "b", "abcdef1", ReviewClass::Single);
        s.pr_number = 43;
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("r".into(), "abcdef1".into())],
        };
        save(&home, &s).unwrap();
        crate::daemon::ci_handoff_track::record(
            &home,
            "lead",
            "o/r@b",
            &chrono::Utc::now().to_rfc3339(),
            Some("abcdef1"),
            None,
        );

        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(43, "b")])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        assert!(
            crate::daemon::ci_handoff_track::list(&home)
                .iter()
                .any(|(_, t)| t.correlation == "o/r@b"),
            "IRON RULE: VERIFIED PR must KEEP its ci-handoff track"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #bughunt3 invariant (#1617 lock-while-blocking class): the worktree
    /// auto-release does a `git` subprocess + acquires a second (binding) flock,
    /// so it must NEVER run inside the `with_pr_state` closure — that closure
    /// runs under the PR-state flock. Structural source-scan (mirrors #1593 F2):
    /// brace-match the `|state| { ... }` closure body and assert
    /// `auto_release_for_merged_branch` is NOT called inside it, and IS called
    /// after the closure (lock-free, post-unlock). Needle is `concat`-built and
    /// the scan is prod-sliced so this test can't self-satisfy.
    #[test]
    fn auto_release_not_called_under_pr_state_flock() {
        let src = include_str!("scanner.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        let closure_needle = [", |state|", " {"].concat();
        let cstart = prod
            .find(&closure_needle)
            .expect("with_pr_state closure present");

        // Brace-match from the closure's opening `{` to find its body span.
        let open_rel = prod[cstart..].find('{').expect("closure block opens");
        let block_start = cstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "closure block must close");

        let release_needle = ["auto_release_for", "_merged_branch"].concat();
        let closure_body = &prod[block_start..=block_end];
        assert!(
            !closure_body.contains(&release_needle),
            "auto_release_for_merged_branch must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
        assert!(
            prod[block_end..].contains(&release_needle),
            "auto_release_for_merged_branch must run AFTER the PR-state flock is dropped"
        );
    }

    /// #1629 invariant (#1617 lock-while-blocking class): the inbox emit
    /// (`enqueue_with_idle_hint` → loopback `api::call`) must NEVER run inside
    /// the `with_pr_state` closure, which holds the PR-state flock. The emits are
    /// collected under the flock and drained after it drops. Same structural
    /// source-scan as the auto_release invariant above: brace-match the closure
    /// body and assert `enqueue_with_idle_hint` is NOT inside it and IS called
    /// after. Needle is `concat`-built and the scan is prod-sliced so this test
    /// can't self-satisfy.
    #[test]
    fn deferred_emit_not_called_under_pr_state_flock() {
        let src = include_str!("scanner.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };

        let closure_needle = [", |state|", " {"].concat();
        let cstart = prod
            .find(&closure_needle)
            .expect("with_pr_state closure present");
        let open_rel = prod[cstart..].find('{').expect("closure block opens");
        let block_start = cstart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "closure block must close");

        let emit_needle = ["enqueue_with", "_idle_hint"].concat();
        let closure_body = &prod[block_start..=block_end];
        assert!(
            !closure_body.contains(&emit_needle),
            "enqueue_with_idle_hint must NOT run inside the with_pr_state closure (under the PR-state flock — #1617 class)"
        );
        assert!(
            prod[block_end..].contains(&emit_needle),
            "enqueue_with_idle_hint must run AFTER the PR-state flock is dropped (deferred drain)"
        );
    }

    /// #2749 RED (fail-closed anchor, first small increment): an otherwise
    /// fully MergeReady PR (CI green at head, VERIFIED at head, not draft) whose
    /// deterministic-ancestry freshness tuple is UNKNOWN — `freshness_checked_*`
    /// all `None`, the first-observation / pre-populator state — must NOT emit
    /// `[pr-ready-for-merge]`. Ancestry is unproven, so the read-only gate fails
    /// CLOSED: it suppresses ready and emits NOTHING (never mislabels as
    /// pr-needs-rebase), leaving #2747's exact-head merge gate as the hard
    /// backstop while the off-tick populator stamps the tuple on a later cycle.
    ///
    /// Against the CURRENT gate-less scanner — which emits pr-ready whenever
    /// `merge_state == MergeReady` — this FAILS (ready_emitted_for_sha becomes
    /// `Some(head)`), which is the intended RED. The GREEN three-way gate makes
    /// it pass. Emission is asserted via the persisted `ready_emitted_for_sha`
    /// dedup flag (set under the flock exactly when pr-ready is queued, and only
    /// reset on a post-flock enqueue FAILURE — which does not happen against a
    /// real temp home).
    #[test]
    fn merge_ready_without_freshness_tuple_suppresses_pr_ready() {
        let home = std::env::temp_dir().join(format!(
            "agend-2749-fail-closed-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let head = "abcdef0";
        let mut s = new_for_branch("o/r", "b", head, ReviewClass::Single);
        s.pr_number = 77;
        // Drive is_merge_ready → MergeReady: CI green at head + VERIFIED at head.
        s.ci_state = CiState::Green {
            sha: head.into(),
            observed_at: chrono::Utc::now().to_rfc3339(),
        };
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("r".into(), head.into())],
        };
        s.merge_state = MergeState::MergeReady;
        // Freshness tuple left UNKNOWN (all None) — the fail-closed case.
        assert!(
            s.freshness_checked_head_sha.is_none()
                && s.freshness_checked_base_sha.is_none()
                && s.freshness_behind_by.is_none(),
            "precondition: freshness tuple is unknown"
        );
        save(&home, &s).unwrap();

        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(77, "b")])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        let reloaded = load(&home, "o/r", "b").expect("state persists");
        assert_eq!(
            reloaded.ready_emitted_for_sha, None,
            "#2749 fail-closed: a MergeReady PR with NO freshness tuple must NOT \
             emit [pr-ready-for-merge] (ancestry unproven ⇒ suppress; #2747 is \
             the backstop)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2749 no-regression guard (RED #6): a MergeReady PR whose freshness tuple
    /// is VALID and FRESH (three heads agree, checked base == observed base, no
    /// error, within TTL, behind_by == 0) must STILL emit [pr-ready-for-merge] —
    /// deterministic ancestry proven fresh WINS. This pins the gate so a GREEN
    /// implementation cannot degenerate into "never emit" (which would satisfy
    /// the fail-closed RED alone). Ancestry-fresh ⇒ ready fires unchanged.
    #[test]
    fn merge_ready_with_fresh_tuple_still_emits_pr_ready() {
        let home = std::env::temp_dir().join(format!(
            "agend-2749-fresh-emits-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let head = "abcdef0";
        let mut s = merge_ready_state("o/r", "b", head, 78);
        stamp_fresh_tuple(&mut s, head, "beef0001", 0);
        save(&home, &s).unwrap();

        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(78, "b")])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        let reloaded = load(&home, "o/r", "b").expect("state persists");
        assert_eq!(
            reloaded.ready_emitted_for_sha,
            Some(head.to_string()),
            "#2749 no-regression: a MergeReady PR with a valid FRESH tuple \
             (behind_by=0) must STILL emit [pr-ready-for-merge]"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2749 A5 (Fable pinning test): a PERSISTED MergeReady state whose
    /// `review_class` is Unresolved must emit NOTHING from the freshness arm —
    /// even with a VALID FRESH tuple that would otherwise open pr-ready. Guards
    /// against a future off-tick populator reviving a legacy stale MergeReady
    /// whose class was never resolved (the reducer's `is_merge_ready` refuses
    /// Unresolved, but a persisted MergeReady bypasses that path). Fail closed.
    #[test]
    fn merge_ready_unresolved_class_suppresses_freshness_delivery() {
        let home = std::env::temp_dir().join(format!(
            "agend-2749-a5-unresolved-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        let head = "abcdef0";
        let mut s = merge_ready_state("o/r", "b", head, 79);
        // Legacy/torn: persisted MergeReady, but the class was never resolved.
        s.review_class = ReviewClass::Unresolved;
        stamp_fresh_tuple(&mut s, head, "beef0001", 0);
        save(&home, &s).unwrap();

        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta(79, "b")])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        let reloaded = load(&home, "o/r", "b").expect("state persists");
        assert_eq!(
            reloaded.ready_emitted_for_sha, None,
            "#2749 A5: a MergeReady state with review_class Unresolved must NOT \
             emit pr-ready even with a fresh tuple (fail closed before delivery)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2749 the pure read-only three-way classifier. Fresh only when the whole
    /// tuple agrees and is within TTL at behind_by==0; behind_by>0 ⇒ Behind;
    /// every unknown/torn/stale/error input ⇒ Suppress (fail closed). Exercised
    /// directly (populator-independent) so the gate logic is pinned without the
    /// end-to-end scanner harness.
    #[test]
    fn freshness_gate_classifies() {
        let now = chrono::Utc::now();
        let head = "aaaaaaa";
        let base = "bbbbbbb";
        let valid = |behind: u64| {
            let mut s = new_for_branch("o/r", "b", head, ReviewClass::Single);
            stamp_fresh_tuple(&mut s, head, base, behind);
            s
        };

        // Fresh: agreeing tuple, within TTL, behind_by == 0.
        assert_eq!(freshness_gate(&valid(0), now, 600), FreshnessGate::Fresh);
        // Behind: agreeing tuple, behind_by > 0.
        assert_eq!(
            freshness_gate(&valid(3), now, 600),
            FreshnessGate::Behind { behind_by: 3 }
        );
        // Unknown: no tuple at all (fresh state) ⇒ Suppress.
        assert_eq!(
            freshness_gate(
                &new_for_branch("o/r", "b", head, ReviewClass::Single),
                now,
                600
            ),
            FreshnessGate::Suppress
        );
        // Checked head != current head ⇒ Suppress.
        let mut s = valid(0);
        s.freshness_checked_head_sha = Some("ccccccc".into());
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
        // Observed head != current head (torn) ⇒ Suppress.
        let mut s = valid(0);
        s.observed_head_sha = Some("ccccccc".into());
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
        // Checked base != observed base (the #2749 main-advance case) ⇒ Suppress.
        let mut s = valid(0);
        s.observed_base_sha = Some("ddddddd".into());
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
        // Compare error ⇒ Suppress.
        let mut s = valid(0);
        s.freshness_error = true;
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
        // Observation error ⇒ Suppress.
        let mut s = valid(0);
        s.observed_error = true;
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
        // Stale past TTL (evaluate well beyond the 600s bound) ⇒ Suppress.
        assert_eq!(
            freshness_gate(&valid(0), now + chrono::Duration::seconds(900), 600),
            FreshnessGate::Suppress
        );
        // behind_by unknown but tuple otherwise valid ⇒ Suppress (never guess 0).
        let mut s = valid(0);
        s.freshness_behind_by = None;
        assert_eq!(freshness_gate(&s, now, 600), FreshnessGate::Suppress);
    }

    /// #2749 review-fix (codex): the TTL bound is TWO-SIDED — a FUTURE observed_at
    /// or freshness_checked_at must FAIL CLOSED. The original within_ttl checked
    /// only `age <= ttl_secs`; a future timestamp yields a NEGATIVE age that
    /// silently passed, letting a clock-skewed / forged-future stamp read Fresh
    /// indefinitely. Now `0 <= age <= ttl_secs`. A negative `ttl_secs` yields an
    /// empty range and can never admit Fresh.
    #[test]
    fn freshness_gate_future_timestamp_and_negative_ttl_fail_closed() {
        let now = chrono::Utc::now();
        let head = "aaaaaaa";
        let base = "bbbbbbb";
        let valid = |behind: u64| {
            let mut s = new_for_branch("o/r", "b", head, ReviewClass::Single);
            stamp_fresh_tuple(&mut s, head, base, behind);
            s
        };
        let future = (now + chrono::Duration::seconds(300)).to_rfc3339();
        // R2: a SUB-second future stamp (+500ms) is the truncation trap —
        // `num_seconds()` would floor it to 0 and pass `0 <= age`. Full-Duration
        // comparison must still reject it.
        let future_ms = (now + chrono::Duration::milliseconds(500)).to_rfc3339();

        // Sanity: the un-tampered tuple (stamped in the past) is Fresh at `now`.
        assert_eq!(freshness_gate(&valid(0), now, 600), FreshnessGate::Fresh);

        // Future observed_at (300s ahead of `now`) ⇒ Suppress (fail closed).
        let mut s = valid(0);
        s.observed_at = Some(future.clone());
        assert_eq!(
            freshness_gate(&s, now, 600),
            FreshnessGate::Suppress,
            "future observed_at must fail closed (was fail-OPEN under `<= ttl`)"
        );

        // Future freshness_checked_at ⇒ Suppress.
        let mut s = valid(0);
        s.freshness_checked_at = Some(future);
        assert_eq!(
            freshness_gate(&s, now, 600),
            FreshnessGate::Suppress,
            "future freshness_checked_at must fail closed"
        );

        // R2: SUB-second future observed_at (+500ms) ⇒ Suppress. Under the
        // truncating `num_seconds()` this floored to 0 and passed — the fix's
        // full-Duration compare rejects it.
        let mut s = valid(0);
        s.observed_at = Some(future_ms.clone());
        assert_eq!(
            freshness_gate(&s, now, 600),
            FreshnessGate::Suppress,
            "sub-second future observed_at (+500ms) must fail closed (num_seconds truncation trap)"
        );

        // R2: SUB-second future freshness_checked_at (+500ms) ⇒ Suppress.
        let mut s = valid(0);
        s.freshness_checked_at = Some(future_ms);
        assert_eq!(
            freshness_gate(&s, now, 600),
            FreshnessGate::Suppress,
            "sub-second future freshness_checked_at (+500ms) must fail closed"
        );

        // Negative ttl ⇒ empty window ⇒ never Fresh, even for a perfectly current
        // tuple.
        assert_eq!(
            freshness_gate(&valid(0), now, -1),
            FreshnessGate::Suppress,
            "negative ttl must never admit Fresh"
        );
    }

    // ─── #2749 2b RED: behind → durable pr-needs-rebase + PTY wake ───────────
    // These production-entry tests drive `scan_and_emit_with` and assert the
    // durable [pr-needs-rebase] row AND its canonical [AGEND-MSG-PENDING] wake.
    // They FAIL against this commit's parent (no Behind arm yet); the 2b-GREEN
    // Behind arm + post-flock ledger drain + wake makes them pass.

    // DeliveryKey::new requires a full 40/64-hex head — the durable ledger keys
    // pr-needs-rebase on (repo, PR, head, recipient), so the behind tests use
    // realistic full SHAs.
    const BEHIND_HEAD: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const BEHIND_BASE: &str = "1234567890abcdef1234567890abcdef12345678";

    /// Write a fleet.yaml so `resolve_merge_authority` returns `orch` (the team
    /// orchestrator = merge authority) for a PR authored by team member `member`.
    fn write_team_fleet(home: &std::path::Path, orch: &str, member: &str) {
        std::fs::create_dir_all(home.join("inbox")).ok();
        let y = format!(
            "instances:\n  {member}:\n    backend: claude\n  {orch}:\n    backend: claude\n\
             teams:\n  squad:\n    orchestrator: {orch}\n    members:\n      - {member}\n"
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), y).expect("write fleet.yaml");
    }

    fn needs_rebase_msgs(home: &std::path::Path, who: &str) -> Vec<crate::inbox::InboxMessage> {
        crate::inbox::drain(home, who)
            .into_iter()
            .filter(|m| m.kind.as_deref() == Some("pr-needs-rebase"))
            .collect()
    }

    fn tmp_home(ln: u32) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-2749-2b-{}-{}", std::process::id(), ln));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn behind_state(home: &std::path::Path, pr: u64, behind_by: u64) {
        let mut s = merge_ready_state("owner/repo", "feat/x", BEHIND_HEAD, pr);
        s.pr_author = "dev".into();
        stamp_fresh_tuple(&mut s, BEHIND_HEAD, BEHIND_BASE, behind_by);
        save(home, &s).unwrap();
    }

    fn behind_poller(pr: u64) -> MockGhPoller {
        MockGhPoller::new(vec![Ok(vec![open_pr_meta(pr, "feat/x")])])
    }

    /// RED#1: behind ⇒ suppress pr-ready + exactly ONE durable [pr-needs-rebase]
    /// to each deduped {merge authority (lead), PR owner (dev)}.
    #[test]
    fn behind_pr_suppresses_ready_and_notifies_authority_and_owner() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 2);

        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

        let reloaded = load(&home, "owner/repo", "feat/x").expect("state persists");
        assert_eq!(
            reloaded.ready_emitted_for_sha, None,
            "#2749 behind ⇒ [pr-ready-for-merge] must be suppressed"
        );
        for who in ["lead", "dev"] {
            let nr = needs_rebase_msgs(&home, who);
            assert_eq!(nr.len(), 1, "#2749 behind ⇒ one [pr-needs-rebase] to {who}");
            assert!(
                nr[0].text.contains("owner/repo#77"),
                "PR ref: {}",
                nr[0].text
            );
            assert!(
                nr[0].text.to_lowercase().contains("behind"),
                "states behind: {}",
                nr[0].text
            );
        }
    }

    /// RED#2: the notice body carries the full payload + reviewed_head.
    #[test]
    fn behind_needs_rebase_body_carries_full_payload() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 3);

        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

        let nr = needs_rebase_msgs(&home, "dev");
        assert_eq!(nr.len(), 1, "one notice to the owner");
        let body = &nr[0].text;
        assert!(body.contains("owner/repo#77"), "PR ref: {body}");
        assert!(body.contains(&BEHIND_HEAD[..8]), "head short sha: {body}");
        assert!(body.contains(&BEHIND_BASE[..8]), "main short sha: {body}");
        assert!(body.contains("by 3 commit"), "behind-by count: {body}");
        assert!(body.contains("Re-stamp checklist"), "checklist: {body}");
        assert_eq!(
            nr[0].reviewed_head.as_deref(),
            Some(BEHIND_HEAD),
            "reviewed_head pins the behind head"
        );
    }

    /// RED#3: the #2745 ledger dedups the ROW per (repo, PR, head, recipient) —
    /// N ticks at the same head deliver exactly ONE notice per recipient.
    #[test]
    fn behind_needs_rebase_delivered_once_across_ticks() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 2);

        for _ in 0..3 {
            scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
        }
        for who in ["lead", "dev"] {
            assert_eq!(
                needs_rebase_msgs(&home, who).len(),
                1,
                "#2749 ledger dedup: one notice to {who} across 3 ticks"
            );
        }
    }

    /// RED#4: recipients deduped BEFORE the ledger keys — owner == merge authority
    /// (no team) ⇒ a single notice.
    #[test]
    fn behind_needs_rebase_dedups_recipient_when_owner_is_authority() {
        let home = tmp_home(line!());
        std::fs::create_dir_all(home.join("inbox")).ok(); // no fleet.yaml ⇒ no team
        let mut s = merge_ready_state("owner/repo", "feat/x", BEHIND_HEAD, 77);
        s.pr_author = "solo".into();
        stamp_fresh_tuple(&mut s, BEHIND_HEAD, BEHIND_BASE, 1);
        save(&home, &s).unwrap();

        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));

        assert_eq!(
            needs_rebase_msgs(&home, "solo").len(),
            1,
            "#2749 owner == authority ⇒ a single deduped notice"
        );
    }

    /// RED#5 (WAKE): a Delivered row emits exactly ONE canonical
    /// [AGEND-MSG-PENDING] pointer wake per deduped recipient (kind=pr-needs-rebase).
    #[test]
    fn behind_delivered_emits_one_canonical_wake_per_recipient() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 2);

        let (_, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
            scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
        });
        let nr_wakes: Vec<_> = wakes
            .iter()
            .filter(|w| w.contains("kind=pr-needs-rebase"))
            .collect();
        assert_eq!(
            nr_wakes.len(),
            2,
            "#2749 wake: one canonical pointer per deduped recipient (lead+dev); got {wakes:?}"
        );
        for w in &nr_wakes {
            assert!(w.contains("[AGEND-MSG-PENDING]"), "canonical pointer: {w}");
            assert!(
                w.contains("inbox="),
                "carries authoritative unread count: {w}"
            );
        }
    }

    /// RED#6 (WAKE dedup): a second tick at the SAME head enqueues NO new row and
    /// emits NO new wake (the ledger suppresses the already-delivered key).
    #[test]
    fn behind_same_head_next_tick_no_new_row_no_new_wake() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 2);

        // Tick 1: delivered + woken.
        let (_, w1) = crate::inbox::with_captured_pointer_wakes(|| {
            scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
        });
        let rows1: usize = ["lead", "dev"]
            .iter()
            .map(|w| needs_rebase_msgs(&home, w).len())
            .sum();
        assert_eq!(rows1, 2, "tick 1 delivers one row per recipient");
        assert_eq!(
            w1.iter()
                .filter(|w| w.contains("kind=pr-needs-rebase"))
                .count(),
            2,
            "tick 1 wakes each recipient once"
        );

        // Tick 2 (same head): ledger Suppressed ⇒ no new row, no new wake.
        let (_, w2) = crate::inbox::with_captured_pointer_wakes(|| {
            scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
        });
        let rows2: usize = ["lead", "dev"]
            .iter()
            .map(|w| needs_rebase_msgs(&home, w).len())
            .sum();
        assert_eq!(rows2, 0, "#2749 same head ⇒ no NEW row on tick 2");
        assert_eq!(
            w2.iter()
                .filter(|w| w.contains("kind=pr-needs-rebase"))
                .count(),
            0,
            "#2749 same head ⇒ no NEW wake on tick 2 (Suppressed)"
        );
    }

    /// RED#7 (WAKE failure): a dropped wake (delivery queue full) must NOT
    /// invalidate the durable delivery — the row stays persisted and the ledger
    /// stays recorded (a later tick still Suppresses).
    #[test]
    fn behind_wake_failure_leaves_row_and_ledger_durable() {
        let _guard = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(true);

        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        behind_state(&home, 77, 2);

        // Wake goes to the REAL inject path (no capture) → queue full → wake Errs.
        scan_and_emit_with(&home, &empty_registry(), &behind_poller(77));
        crate::daemon::delivery_worker::test_support::set_force_full(false);

        // The durable row is still there despite the dropped wake.
        for who in ["lead", "dev"] {
            assert_eq!(
                needs_rebase_msgs(&home, who).len(),
                1,
                "#2749 wake drop must NOT invalidate the durable row for {who}"
            );
        }
    }

    /// #2749 wake decision matrix (the ambiguous-record case + the no-wake cases):
    /// wake ONLY on Delivered | RecordFailedAfterEnqueue (row durably persisted);
    /// never on Suppressed (already delivered) or EnqueueFailed (no row).
    #[test]
    fn wake_after_ledger_decision_matrix() {
        use crate::daemon::ci_delivery_ledger::{DeliveryError, DeliveryOutcome};
        assert!(
            super::wake_after_ledger(&Ok(DeliveryOutcome::Delivered)),
            "Delivered ⇒ wake"
        );
        assert!(
            super::wake_after_ledger(&Err(DeliveryError::RecordFailedAfterEnqueue(
                anyhow::anyhow!("record write failed")
            ))),
            "RecordFailedAfterEnqueue ⇒ wake (row durably enqueued)"
        );
        assert!(
            !super::wake_after_ledger(&Ok(DeliveryOutcome::Suppressed)),
            "Suppressed ⇒ NO wake (a prior tick already delivered + woke)"
        );
        assert!(
            !super::wake_after_ledger(&Err(DeliveryError::EnqueueFailed(anyhow::anyhow!(
                "enqueue failed"
            )))),
            "EnqueueFailed ⇒ NO wake (no row persisted)"
        );
    }

    /// #2749: the narrow wake helper builds the CANONICAL [AGEND-MSG-PENDING]
    /// pointer (id/kind/from/inbox count) for an already-persisted row.
    #[test]
    fn wake_persisted_pointer_builds_canonical_inbox_pointer() {
        let home = tmp_home(line!());
        std::fs::create_dir_all(home.join("inbox")).ok();
        // Pre-stamp the id (as the durable ledger path does), persist the row so
        // the authoritative unread count is non-zero, then wake THAT id.
        let mut msg =
            crate::inbox::InboxMessage::new_system("system:pr-state", "pr-needs-rebase", "b");
        let id = crate::inbox::stamp_message_id(&mut msg);
        assert!(!id.is_empty(), "stamp_message_id assigns an id");
        crate::inbox::enqueue(&home, "rcpt", msg).unwrap();

        let (res, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
            crate::inbox::wake_persisted_pointer(
                &home,
                "rcpt",
                &id,
                "pr-needs-rebase",
                "system:pr-state",
            )
        });
        res.expect("wake ok under capture");
        assert_eq!(wakes.len(), 1, "one pointer captured");
        let p = &wakes[0];
        assert!(p.contains("[AGEND-MSG-PENDING]"), "canonical prefix: {p}");
        assert!(p.contains(&format!("id={id}")), "pre-stamped id: {p}");
        assert!(p.contains("kind=pr-needs-rebase"), "kind: {p}");
        assert!(p.contains("inbox=1"), "authoritative unread count: {p}");
    }

    // ─── #2749 3a RED: gh_poll atomic head/base observation ──────────────────
    // These real-entry tests drive the gh_poll → apply_gh_observations path and
    // assert the ATOMIC observed pair is written / preserved. They FAIL against
    // this commit's parent (apply_gh_observations does not write observed_* yet);
    // the 3a-GREEN write block + failure arm make them pass.

    /// An open-PR gh observation that ALSO carries the atomic head+base OIDs, so
    /// the real gh_poll → apply_gh_observations path writes the observed pair.
    fn open_pr_meta_oids(
        number: u64,
        branch: &str,
        head_oid: &str,
        base_oid: &str,
    ) -> GhPrMetadata {
        GhPrMetadata {
            head_ref_oid: Some(head_oid.into()),
            base_ref_oid: Some(base_oid.into()),
            ..open_pr_meta(number, branch)
        }
    }

    /// #2749 3a: a live gh-poll carrying head+base OIDs writes the ATOMIC observed
    /// pair (observed_head_sha + observed_base_sha + observed_at TOGETHER, clearing
    /// observed_error). Real gh_poll → apply_gh_observations path (not injection).
    #[test]
    fn gh_poll_writes_atomic_observed_head_and_base() {
        let home = tmp_home(line!());
        std::fs::create_dir_all(home.join("inbox")).ok();
        let mut s = new_for_branch("owner/repo", "feat/x", "curhead", ReviewClass::Single);
        s.pr_number = 55;
        s.pr_author = "dev".into();
        assert!(s.observed_head_sha.is_none(), "precondition: unobserved");
        save(&home, &s).unwrap();

        scan_and_emit_with(
            &home,
            &empty_registry(),
            &MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(
                55, "feat/x", "HEADOID1", "BASEOID1",
            )])]),
        );

        let r = load(&home, "owner/repo", "feat/x").expect("state persists");
        assert_eq!(r.observed_head_sha.as_deref(), Some("HEADOID1"));
        assert_eq!(r.observed_base_sha.as_deref(), Some("BASEOID1"));
        assert!(
            r.observed_at.is_some(),
            "observed_at stamped from the same poll"
        );
        assert!(
            !r.observed_error,
            "a good observation clears observed_error"
        );
    }

    /// #2749 3a: a gh-poll TRANSPORT FAILURE flags observed_error and does NOT
    /// advance observed_at nor clobber the last-good observed pair — the gate then
    /// fails closed while the prior observation is preserved (CORRECTION 3 / GO-proof).
    #[test]
    fn gh_poll_failure_flags_observed_error_without_clobbering() {
        let home = tmp_home(line!());
        std::fs::create_dir_all(home.join("inbox")).ok();
        let mut s = new_for_branch("owner/repo", "feat/x", "curhead", ReviewClass::Single);
        s.pr_number = 55;
        s.pr_author = "dev".into();
        // A prior GOOD observation on disk.
        s.observed_head_sha = Some("GOODHEAD".into());
        s.observed_base_sha = Some("GOODBASE".into());
        s.observed_at = Some("2026-07-12T00:00:00+00:00".into());
        s.observed_error = false;
        save(&home, &s).unwrap();

        scan_and_emit_with(
            &home,
            &empty_registry(),
            &MockGhPoller::new(vec![Err(anyhow::anyhow!("gh transport failed"))]),
        );

        let r = load(&home, "owner/repo", "feat/x").expect("state persists");
        assert!(r.observed_error, "transport failure ⇒ observed_error");
        assert_eq!(
            r.observed_head_sha.as_deref(),
            Some("GOODHEAD"),
            "last-good head preserved (not clobbered)"
        );
        assert_eq!(
            r.observed_base_sha.as_deref(),
            Some("GOODBASE"),
            "last-good base preserved (not clobbered)"
        );
        assert_eq!(
            r.observed_at.as_deref(),
            Some("2026-07-12T00:00:00+00:00"),
            "observed_at NOT advanced on failure"
        );
    }

    // ─── #2749 3b RED: off-tick freshness populator → scanner (real-entry) ────
    // Drive the ACTUAL off-tick worker (worker_poll_and_act) — which observes via
    // gh-poll and, once 3b-GREEN wires it, runs the deterministic REMOTE ancestry
    // compare (ScmProvider::compare) and stamps freshness_checked_* — then run the
    // scanner and assert the end-to-end gate outcome. NO helper-stamped tuples.
    // These FAIL vs this commit's parent (the worker does not populate freshness
    // yet); 3b-GREEN wires the populator so they pass.

    fn full_head(n: u8) -> String {
        format!("{:0>40}", format!("{n}beef"))
    }

    /// Run the REAL off-tick worker once (poll + observe-consumers + — in GREEN —
    /// the ancestry compare + freshness stamp).
    fn run_off_tick(home: &std::path::Path, pr: u64, head: &str, base: &str) {
        let cache = super::super::gh_poll::GhPollCache::new();
        let poller = MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(pr, "feat/x", head, base)])]);
        super::super::gh_poll::worker_poll_and_act(home, &cache, "owner/repo", &poller);
    }

    /// Scan once with an OID-carrying open-PR poll (writes observed_head/base, 3a).
    /// Clears `last_gh_poll_at` first so the production per-file poll cadence
    /// (`should_poll`) does not skip the re-observation — the test needs each
    /// observation to actually land (in production a main-advance is observed on
    /// the next cadence-allowed poll; this just removes that latency for the test).
    fn scan_observe(home: &std::path::Path, pr: u64, head: &str, base: &str) {
        let _ = super::super::with_pr_state(home, "owner/repo", "feat/x", |s| {
            s.last_gh_poll_at = None;
        });
        scan_and_emit_with(
            home,
            &empty_registry(),
            &MockGhPoller::new(vec![Ok(vec![open_pr_meta_oids(pr, "feat/x", head, base)])]),
        );
    }

    fn ready_state(home: &std::path::Path, pr: u64, head: &str) {
        let mut s = merge_ready_state("owner/repo", "feat/x", head, pr);
        s.pr_author = "dev".into();
        save(home, &s).unwrap();
    }

    /// RED 3b-1 (Fresh): observe → off-tick compare behind_by=0 → scan ⇒ pr-ready.
    #[test]
    fn off_tick_fresh_ancestry_opens_pr_ready() {
        let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = full_head(1);
        ready_state(&home, 88, &head);

        // Pre-populate: a plain scan observes but the gate has no freshness tuple.
        scan_observe(&home, 88, &head, BEHIND_BASE);
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha,
            None,
            "pre-populate: no freshness tuple ⇒ pr-ready suppressed"
        );

        run_off_tick(&home, 88, &head, BEHIND_BASE); // REAL worker: compare + stamp
        scan_observe(&home, 88, &head, BEHIND_BASE); // now Fresh ⇒ emit

        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha
                .as_deref(),
            Some(head.as_str()),
            "#2749 3b: fresh ancestry (behind_by=0) ⇒ pr-ready emits"
        );
    }

    /// RED 3b-2 (Behind): off-tick compare behind_by=2 ⇒ suppress pr-ready + emit
    /// pr-needs-rebase + a canonical wake per recipient.
    #[test]
    fn off_tick_behind_ancestry_emits_needs_rebase_and_wake() {
        let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(2));
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = BEHIND_HEAD; // full hex for the ledger DeliveryKey
        ready_state(&home, 88, head);

        scan_observe(&home, 88, head, BEHIND_BASE);
        run_off_tick(&home, 88, head, BEHIND_BASE);
        let (_, wakes) = crate::inbox::with_captured_pointer_wakes(|| {
            scan_observe(&home, 88, head, BEHIND_BASE);
        });

        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha,
            None,
            "#2749 3b: behind ⇒ pr-ready suppressed"
        );
        for who in ["lead", "dev"] {
            assert_eq!(
                needs_rebase_msgs(&home, who).len(),
                1,
                "#2749 3b: one [pr-needs-rebase] to {who}"
            );
        }
        assert_eq!(
            wakes
                .iter()
                .filter(|w| w.contains("kind=pr-needs-rebase"))
                .count(),
            2,
            "#2749 3b: behind ⇒ a canonical wake per recipient"
        );
    }

    /// RED 3b-3 (main-advance): after a Fresh compare, a NEW observation whose base
    /// moved (checked_base != observed_base) ⇒ Suppress until the populator recomputes.
    #[test]
    fn off_tick_main_advance_suppresses_until_recompute() {
        let _scm = crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = full_head(3);
        let base1 = full_head(10);
        let base2 = full_head(20);
        ready_state(&home, 88, &head);

        // Fresh against base1.
        scan_observe(&home, 88, &head, &base1);
        run_off_tick(&home, 88, &head, &base1);
        // Main advances: a new observation carries base2 ⇒ observed_base=base2, but
        // freshness_checked_base is still base1 ⇒ gate Suppress.
        scan_observe(&home, 88, &head, &base2);
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha,
            None,
            "#2749 3b: base advanced (checked_base != observed_base) ⇒ suppressed"
        );

        // Re-populate against base2 ⇒ converges to Fresh ⇒ emit.
        run_off_tick(&home, 88, &head, &base2);
        scan_observe(&home, 88, &head, &base2);
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha
                .as_deref(),
            Some(head.as_str()),
            "#2749 3b: re-compute against the new base ⇒ pr-ready re-converges"
        );
    }

    /// RED 3b-4 (compare error): a failed ancestry re-compare (base changed) stamps
    /// freshness_error WITHOUT clobbering the last-good checked tuple ⇒ Suppress.
    #[test]
    fn off_tick_compare_error_suppresses_without_clobber() {
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = full_head(4);
        let base1 = full_head(11);
        let base2 = full_head(22);
        ready_state(&home, 88, &head);

        // A GOOD compare against base1 stamps a Fresh tuple.
        {
            let _scm =
                crate::scm::set_test_scm_provider(crate::scm::MockScmProvider::with_compare(0));
            scan_observe(&home, 88, &head, &base1);
            run_off_tick(&home, 88, &head, &base1);
        }
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .freshness_checked_base_sha
                .as_deref(),
            Some(base1.as_str()),
            "precondition: good compare stamped checked_base=base1"
        );

        // Base advances (tuple changed ⇒ re-compute needed) but the compare FAILS.
        {
            let _scm = crate::scm::set_test_scm_provider(
                crate::scm::MockScmProvider::with_compare_err("forge 500"),
            );
            scan_observe(&home, 88, &head, &base2); // observed_base=base2
            run_off_tick(&home, 88, &head, &base2); // compare(base2) → Err
        }
        let after_err = load(&home, "owner/repo", "feat/x").unwrap();
        assert!(
            after_err.freshness_error,
            "#2749 3b: compare failure ⇒ freshness_error"
        );
        assert_eq!(
            after_err.freshness_checked_base_sha.as_deref(),
            Some(base1.as_str()),
            "#2749 3b: last-good checked tuple preserved (NOT clobbered) on failure"
        );

        scan_observe(&home, 88, &head, &base2);
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .ready_emitted_for_sha,
            None,
            "#2749 3b: freshness_error ⇒ pr-ready suppressed"
        );
    }

    // ─── #2749 correction (codex): retry-lease backoff + stale-error discard ──
    // A persistent compare failure must back off to ONE compare per 60s lease
    // (not one per 15s worker cycle), and a stale errored tuple must be discarded
    // when the observation advances. These FAIL vs this commit's parent (the
    // populator recomputes every cycle on error and never clears the stale error).

    fn set_retry_after(home: &std::path::Path, deadline: Option<String>) {
        let _ = super::super::with_pr_state(home, "owner/repo", "feat/x", |s| {
            s.freshness_retry_after = deadline;
        });
    }

    /// RED (backoff): a persistently-FAILING compare stamps a 60s retry lease and
    /// is NOT re-attempted within it (one compare, no 15s storm); it re-attempts
    /// once the lease deadline passes.
    #[test]
    fn off_tick_persistent_failure_backs_off_then_retries_after_lease() {
        let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
        let _scm = crate::scm::set_test_scm_provider(mock.clone());
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = full_head(5);
        ready_state(&home, 88, &head);
        scan_observe(&home, 88, &head, BEHIND_BASE);

        run_off_tick(&home, 88, &head, BEHIND_BASE); // compare #1 → Err, lease set
        assert_eq!(mock.compare_calls(), 1, "first cycle compares once");
        let s = load(&home, "owner/repo", "feat/x").unwrap();
        assert!(
            s.freshness_error && s.freshness_retry_after.is_some(),
            "error + lease stamped"
        );

        run_off_tick(&home, 88, &head, BEHIND_BASE); // within lease → SKIP (no compare)
        assert_eq!(
            mock.compare_calls(),
            1,
            "#2749 correction: within the 60s lease the failing tuple must NOT re-compare (no 15s storm)"
        );

        // Age the lease past its deadline → re-attempt.
        set_retry_after(
            &home,
            Some((chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339()),
        );
        run_off_tick(&home, 88, &head, BEHIND_BASE);
        assert_eq!(
            mock.compare_calls(),
            2,
            "#2749 correction: after the lease deadline the tuple re-attempts"
        );
    }

    /// RED (stale-error discard): a failed compare (error + lease) whose observation
    /// then ADVANCES must have the stale error + lease CLEARED, so the NEW tuple is
    /// re-attempted immediately rather than staying errored.
    #[test]
    fn off_tick_observation_change_discards_stale_error_and_lease() {
        let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
        let _scm = crate::scm::set_test_scm_provider(mock.clone());
        let home = tmp_home(line!());
        write_team_fleet(&home, "lead", "dev");
        let head = full_head(6);
        let base1 = full_head(12);
        let base2 = full_head(24);
        ready_state(&home, 88, &head);
        scan_observe(&home, 88, &head, &base1);
        run_off_tick(&home, 88, &head, &base1); // Err → error + lease for base1
        let s = load(&home, "owner/repo", "feat/x").unwrap();
        assert!(s.freshness_error && s.freshness_retry_after.is_some());

        // Observation advances to base2 ⇒ the base1 error/lease is stale.
        scan_observe(&home, 88, &head, &base2);
        let s = load(&home, "owner/repo", "feat/x").unwrap();
        assert!(
            !s.freshness_error,
            "#2749 correction: an observed tuple change must DISCARD the stale freshness_error"
        );
        assert!(
            s.freshness_retry_after.is_none(),
            "#2749 correction: the stale retry lease must be discarded on tuple change"
        );
    }

    /// The retry lease persists across restart (serde) and a MALFORMED / absurd
    /// deadline fails-closed by re-attempting (self-heal to a valid lease) rather
    /// than sticking the PR errored; the gate stays fail-closed on freshness_error.
    #[test]
    fn off_tick_retry_lease_persists_and_malformed_self_heals() {
        // Restart persistence: a stamped retry lease survives save→load.
        let home = tmp_home(line!());
        std::fs::create_dir_all(home.join("inbox")).ok();
        let mut s = merge_ready_state("owner/repo", "feat/x", &full_head(7), 88);
        s.freshness_retry_after = Some("2026-07-13T00:00:00+00:00".into());
        save(&home, &s).unwrap();
        assert_eq!(
            load(&home, "owner/repo", "feat/x")
                .unwrap()
                .freshness_retry_after
                .as_deref(),
            Some("2026-07-13T00:00:00+00:00"),
            "retry lease persists across restart"
        );

        // Malformed lease + freshness_error ⇒ the populator re-attempts (self-heal),
        // and the gate keeps suppressing (fail-closed) while errored.
        let mock = crate::scm::MockScmProvider::with_compare_err("forge 500");
        let _scm = crate::scm::set_test_scm_provider(mock.clone());
        let home2 = tmp_home(line!());
        write_team_fleet(&home2, "lead", "dev");
        let head = full_head(8);
        let mut s = merge_ready_state("owner/repo", "feat/x", &head, 88);
        s.pr_author = "dev".into();
        save(&home2, &s).unwrap();
        scan_observe(&home2, 88, &head, BEHIND_BASE);
        run_off_tick(&home2, 88, &head, BEHIND_BASE); // Err → error + valid lease
        let before = mock.compare_calls();
        set_retry_after(&home2, Some("not-a-timestamp".into())); // corrupt the lease
        run_off_tick(&home2, 88, &head, BEHIND_BASE); // malformed ⇒ re-attempt (self-heal)
        assert!(
            mock.compare_calls() > before,
            "#2749 correction: a malformed retry lease fails-closed by re-attempting (no stuck PR)"
        );
        assert!(
            load(&home2, "owner/repo", "feat/x")
                .unwrap()
                .freshness_error,
            "gate stays fail-closed while errored"
        );
    }
}

#[cfg(test)]
mod review_repro_daemon_ci_pr;
