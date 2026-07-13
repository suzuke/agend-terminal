use std::path::Path;

use super::gh_poll;
use super::{
    apply, format_ready_body, freshness_gate, is_merge_ready, pr_state_dir, remove, resolve_author,
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
                // codex m-…-479: DEFENSIVE revalidation + self-heal. A cached
                // `merge_state == MergeReady` reaches this arm WITHOUT re-running the
                // reducer's readiness gate — so a late ACTIVE reservation or a
                // freshly-unreadable authority (`authority_unknown`) that invalidated
                // readiness AFTER the cache was written would still emit pr-ready →
                // the fail-closed merge gate bypassed. Re-run `is_merge_ready` HERE.
                // It SUBSUMES the earlier #2749 A5 Unresolved-only re-check (it also
                // gates authority_unknown + reserved + draft + CI + verdicts). If it
                // now returns FALSE the cache is STALE: SELF-HEAL the persisted
                // `merge_state` to `NotReady` (dirty ⇒ saved under this flock) and
                // SUPPRESS — emit nothing (fail closed). The freshness gate below runs
                // only on a still-ready state, so a self-healed NotReady never reaches
                // it (t-9's freshness gate is unchanged). This complements the
                // reducer/redrive recompute (the primary fix): the scanner is the last
                // fail-closed backstop for any stale MergeReady reaching disk by any
                // path (e.g. a legacy file persisted before the recompute existed).
                if !is_merge_ready(state) {
                    state.merge_state = MergeState::NotReady;
                    dirty = true;
                    tracing::debug!(
                        repo = %state.repo,
                        branch = %state.branch,
                        head = %state.head_sha,
                        "codex m-…-479 pr_state: cached MergeReady failed defensive is_merge_ready re-check (stale reservation / authority_unknown / unresolved) — self-healed to NotReady, pr-ready suppressed (fail closed)"
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

        // t-…-17 A7 (I18/I19) — CRASH-SAFETY ORDER: durably record the terminal
        // generation's marker + tombstone BEFORE the pr_state file is removed below.
        // The marker is what the reconciler's A10a keys on; if the file were removed
        // FIRST, a crash before this write would lose BOTH — on restart CI recreates
        // the file NON-terminal (record_ci_result only applies CiObserved), the
        // terminal is never re-observed, the marker is never written, and the merged
        // PR's assignment record RESURRECTS forever. With this ordering a crash
        // between the marker write and the remove leaves the file present, so the
        // terminal is re-observed next scan and `record_terminal` (idempotent) retries.
        //
        // Done POST-FLOCK (the pr_state flock is released here): `record_terminal`
        // takes ONLY the assignment lock, so it is assignment-lock-with-no-pr_state-
        // flock — it can NEVER nest under the pr_state flock, preserving the global
        // assignment-OUTER / pr_state-INNER order (record_ci_result's A6 drain is the
        // only assignment→pr_state path; nothing goes pr_state→assignment). It records
        // ONLY records whose stored pr_number matches — NEVER a different generation
        // (no force-bind by branch, no unbound window).
        // t-…-17 B3: track whether the terminal marker was DURABLY recorded (or there
        // was no terminal to record). Only then may the pr_state file be removed — a
        // FAILED record_terminal must RETAIN the file as the retry source, else the
        // terminal marker (never written) AND the pr_state (deleted) are both lost and
        // the merged PR's assignment record resurrects (A10a cannot backstop a marker
        // that was never written).
        let terminal_recorded = match crate::daemon::assignment_authority::terminal_kind_of(
            &snapshot,
        ) {
            None => true,                               // no terminal to record ⇒ nothing to lose
            Some(_) if snapshot.pr_number == 0 => true, // unbound generation: no marker keyed
            Some(kind) => match crate::daemon::assignment_authority::record_terminal(
                home,
                &repo,
                &branch,
                snapshot.pr_number,
                kind,
            ) {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(
                            repo = %repo, branch = %branch, pr = snapshot.pr_number,
                            tombstoned = n,
                            "t-…-17 A7: terminal marker recorded + reviewer-assignment records tombstoned"
                        );
                    }
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        repo = %repo, branch = %branch, pr = snapshot.pr_number, error = %e,
                        "t-…-17 A7/B3: record_terminal FAILED — retaining pr_state as the retry source (fail closed); next scan re-observes the terminal and retries"
                    );
                    false
                }
            },
        };
        match result {
            Ok(Some(ScanAction::Remove)) if terminal_recorded => {
                // The durable terminal marker is already recorded above (or there was
                // none to record), so removing the pr_state file here can never lose
                // it (crash-safe ordering).
                let _ = remove(home, &repo, &branch);
            }
            Ok(Some(ScanAction::Remove)) => {
                // record_terminal FAILED — SKIP the remove (B3 fail closed). The
                // pr_state file is retained as the retry source; the next scan
                // re-observes the terminal and record_terminal (idempotent) retries.
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
mod tests;

#[cfg(test)]
mod review_repro_daemon_ci_pr;
