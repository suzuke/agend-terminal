//! Binding manager — writes per-agent binding.json for git shim + hook.
//!
//! Daemon-only writer. Shim and hooks are read-only consumers.
//! Uses atomic_write (temp + fsync + rename) + flock for safety.

use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
/// #1990: on-disk schema version for `binding.json`. The file has always carried
/// a bare `version` field (written by [`bind_full`]) but did NOT go through the
/// `SchemaVersioned` trait, so it had no future-version guard — this const is
/// that guard (see [`parse_binding_guarded`]). Additive field adds don't need a
/// bump; only a non-additive change to an existing field does.
const BINDING_SCHEMA_VERSION: u64 = 1;
pub(crate) mod release_guard;
pub(crate) use release_guard::{
    acquire_agent_mutation_lock, acquire_binding_file_lock, guarded_binding_disk_fresh,
    preflight_guarded_binding, snapshot_guarded_binding, BindingFingerprint, GuardedBinding,
};
mod rebind_guard;
use rebind_guard::same_agent_metadata_catchup_allowed;
mod signature;
pub(crate) use signature::signature_valid;
mod unbind;
pub(crate) use unbind::{unbind_with_permit, BindingRemoval};
mod unbind_compat;
#[allow(unused_imports)]
pub use unbind_compat::unbind;
#[cfg(test)]
mod catalog_tests;
mod reaper_notify;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod reaper_notify_tests;
static INDEX: OnceLock<RwLock<HashMap<String, serde_json::Value>>> = OnceLock::new();
/// #1990: parse a `binding.json` body, rejecting one a NEWER daemon wrote
/// (`version` > [`BINDING_SCHEMA_VERSION`]) → `None`. This guards only the
/// DAEMON-SIDE readers that route through [`read`]: they treat a future-version
/// binding as absent and fail-closed (e.g. auto-release / lease helpers won't act
/// on a binding shape they can't fully understand). It does NOT cover the git
/// shim, which has its OWN reader (`agend-git.rs::read_binding`) that
/// HMAC-verifies and treats a parseable future-version binding as BOUND — so the
/// agent stays restricted to its own worktree (safe), but is not "denied".
/// DESTRUCTIVE daemon-side sites (worktree retention) must use
/// [`present_including_future`] instead, so they never mistake a future binding
/// for absent and reclaim a newer daemon's live worktree. The missing-signature
/// fail-closed path is unchanged.
pub(crate) fn parse_binding_guarded(content: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let found = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    if found > BINDING_SCHEMA_VERSION {
        tracing::warn!(
            found,
            supported = BINDING_SCHEMA_VERSION,
            "binding.json written by a newer schema version — daemon-side readers treat it as absent"
        );
        return None;
    }
    Some(v)
}
/// #1990: true if a binding file is present at a version this daemon understands
/// OR a NEWER one. Distinct from [`read`], which returns `None` for a
/// future-version binding (the correct fail-closed for daemon-side actors that
/// would ACT on the binding). A DESTRUCTIVE retention site must use THIS so it
/// never reclaims a worktree a newer daemon legitimately (re)bound just because
/// this older daemon can't parse the binding's version — "future ≠ absent".
/// A non-JSON / missing file is genuinely absent → `false` (pre-existing behavior).
pub fn present_including_future(home: &Path, agent: &str) -> bool {
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .map(|v| v.is_object())
        .unwrap_or(false)
}

fn binding_index() -> &'static RwLock<HashMap<String, serde_json::Value>> {
    INDEX.get_or_init(|| RwLock::new(HashMap::new()))
}

fn index_key(home: &Path, agent: &str) -> String {
    format!("{}:{agent}", home.display())
}

/// #1882 / #2117 P3b: lock-file path for the per-lease flock. Keyed by a hash of
/// the **(source_repo, branch)** pair so it is path-safe (branches contain `/`)
/// and collision-free; a different lease never shares a lock file.
///
/// #2117 P3b: keying now includes `source_repo` so the SAME branch name in two
/// DIFFERENT repos is two independent leases that never contend. An EMPTY
/// `source_repo` (the test-only `bind()` wrapper; every production lease path
/// carries a resolved repo) falls back to the legacy branch-only key — so the
/// single-project world (one repo) keys on `(repo, branch)` consistently and a
/// rare empty bind keys branch-only, both behaviorally equivalent to pre-P3b.
fn branch_lease_lock_path(home: &Path, source_repo: &str, branch: &str) -> PathBuf {
    let key = if source_repo.is_empty() {
        crate::daemon::utils::sha256_hex(branch.as_bytes())
    } else {
        // NUL separator: unambiguous since neither path nor branch contains it.
        crate::daemon::utils::sha256_hex(format!("{source_repo}\0{branch}").as_bytes())
    };
    crate::paths::runtime_dir(home)
        .join(".lease-locks")
        .join(format!("{key}.lock"))
}

/// #1882 / #2117 P3b: acquire the exclusive per-lease flock so the cross-agent
/// "a (source_repo, branch) is held by at most one agent" check-then-bind
/// (`scan_existing_branch_binding` → `bind_full`) is ATOMIC. Two DIFFERENT agents
/// racing to lease the SAME (repo, branch) serialize here: the first binds; the
/// second blocks until the first's guard drops, then its rescan sees the first's
/// binding and rejects — so neither double-binds. This is a SHORT-LIVED mutex
/// around the bind operation, NOT a lease-lifetime lock: the persistent lease
/// state is `binding.json` (which the scan reads), so `release_full` needs no lock
/// cleanup. Per-(repo,branch) keying means different leases never contend → normal
/// single-agent binds are unaffected. Blocking `acquire_file_lock`; released when
/// the guard drops. Pass `""` for `source_repo` only on the test-only empty-bind
/// path (branch-only key).
pub fn acquire_branch_lease_lock(
    home: &Path,
    source_repo: &str,
    branch: &str,
) -> anyhow::Result<crate::store::FileFlockGuard> {
    crate::store::acquire_file_lock(&branch_lease_lock_path(home, source_repo, branch))
}

/// Write a binding for an agent (task assigned).
///
/// Fail-closed: if lock acquisition or I/O fails, binding.json is NOT
/// written and the error is logged. Pre-#1163 this silently proceeded
/// via `.ok()`, breaking the serialization guarantee.
#[allow(dead_code)] // Used by tests + auto-watch dispatch path
pub fn bind(home: &Path, agent: &str, task_id: &str, branch: &str) {
    if let Err(e) = bind_full(
        home,
        agent,
        task_id,
        branch,
        std::path::Path::new(""),
        std::path::Path::new(""),
        false, // #2158 GR1: internal convenience bind — not a self-claim, no notify
    ) {
        tracing::warn!(%agent, task_id, branch, error = %e, "bind failed (fail-closed)");
    }
}

/// Parse a daemon-managed worktree's `.agend-managed` marker for its recorded
/// `agent=` line. `None` when the marker is missing or the line isn't present
/// — callers that require ownership certainty must already have checked
/// `worktree_pool::is_daemon_managed` before this matters.
///
/// `pub(crate)` — also used by `mcp::handlers::force_release`'s #2496 safe
/// rebind-repair path (same ownership check, different flow control).
pub(crate) fn managed_marker_agent(worktree: &Path) -> Option<String> {
    let content =
        std::fs::read_to_string(worktree.join(crate::worktree_pool::MANAGED_MARKER)).ok()?;
    let agent = content
        .lines()
        .find_map(|l| l.strip_prefix("agent="))
        .map(str::trim)
        .filter(|s| !s.is_empty() && crate::agent::validate_name(s).is_ok())?;
    Some(agent.to_string())
}

/// #2496: does `agent` hold an active CI watch on `branch`? A stale branch
/// still being watched must not be silently abandoned by a metadata-only
/// binding repair. `pub(crate)` — shared with `force_release`'s repair path.
pub(crate) fn agent_has_active_ci_watch_on_branch(home: &Path, agent: &str, branch: &str) -> bool {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if watch["branch"].as_str() != Some(branch) {
            continue;
        }
        if crate::daemon::ci_watch::parse_subscribers(&watch)
            .iter()
            .any(|s| s == agent)
        {
            return true;
        }
    }
    false
}

/// #2496: is any task's STRUCTURED `branch` link still pointing at `branch`
/// in an active status? Same active-status set
/// `status_summary::auto_close_merged_tasks` uses for its structured-link arm
/// (Open/Claimed/InProgress/InReview/Blocked/Verified) — a stale branch still
/// tracked by a live task must not be silently abandoned by a metadata-only
/// binding repair. `pub(crate)` — shared with `force_release`'s repair path.
pub(crate) fn branch_has_active_task(home: &Path, branch: &str) -> bool {
    use crate::task_events::TaskStatus;
    let Ok(tasks) = crate::tasks::list_all_strict(home) else {
        tracing::warn!(branch, "task catalog unreadable; preserving branch binding");
        return true;
    };
    tasks.iter().any(|t| {
        t.branch.as_deref() == Some(branch)
            && matches!(
                t.status,
                TaskStatus::Open
                    | TaskStatus::Claimed
                    | TaskStatus::InProgress
                    | TaskStatus::InReview
                    | TaskStatus::Blocked
                    | TaskStatus::Verified
            )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingProvenance<'a> {
    DaemonProvisionedReview { provisioned_head: &'a str },
}

/// Write a full binding including worktree + source-repo paths.
///
/// `source_repo` is the parent repo that owns the worktree, persisted as a
/// schema field so `worktree_pool::release_full` (Sprint 53 P0-X r1) can run
/// `git worktree remove --force` from the owning repo's cwd. Without this,
/// the git registry leaves a stale prunable entry after a manual `remove_dir_all`
/// fallback. Pass an empty path when unknown — `release_full` falls back to
/// deriving the source from the worktree path's `.worktrees/<agent>` ancestor.
/// #779 P2 (Option B): hard-break API now returns `Result<(), String>` so
/// callers can surface partial-failure diagnostics. I/O failures
/// (`create_dir_all`, lock, `atomic_write`) are explicit `Err` cases.
///
/// Production callers match the Result (they do **not** swallow with
/// `.ok()`): `dispatch_auto_bind_lease` rolls back a fresh lease on `Err`
/// (`dispatch_hook`); `ci::handle_checkout_repo` hard-fails + rolls back;
/// `binding::bind` logs and stays fail-closed. `worktree_pool::lease` no
/// longer writes bindings — the authoritative caller binds after lease.
pub fn bind_full(
    home: &Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    worktree: &std::path::Path,
    source_repo: &std::path::Path,
    // #2158 GR1: true ⟺ an AGENT SELF-CLAIM (`bind_self` / `repo checkout bind:true`),
    // which surfaces to the operator UNLESS `task_id` is non-empty (#2533: a
    // task_id-carrying self-claim is attributable to a task, so it's in-dispatch).
    // Dispatch / internal binds pass `is_self_claim=false` and are NEVER gated on
    // task_id here — a single-target auto-create `send kind=task` legitimately
    // binds with task_id="" and must NOT false-notify.
    is_self_claim: bool,
) -> Result<(), String> {
    bind_full_with_provenance(
        home,
        agent,
        task_id,
        branch,
        worktree,
        source_repo,
        is_self_claim,
        None,
        false,
    )
}

/// Write a full binding with optional typed provenance included in the initial
/// signed document. Provenance is deliberately not a post-bind augmentation:
/// callers that need destructive lifecycle authority must publish all identity
/// fields before the binding is signed and visible to release/GC readers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bind_full_with_provenance(
    home: &Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    worktree: &std::path::Path,
    source_repo: &std::path::Path,
    is_self_claim: bool,
    provenance: Option<BindingProvenance<'_>>,
    // #3546: the provisioning call could not refresh the remote-tracking ref it
    // based this branch on, so the worktree may sit tens of commits behind the
    // default branch while every health check passes. Recorded INSIDE the signed
    // document: a post-bind edit would invalidate the HMAC sidecar, and this fact
    // is not recomputable later (nothing else records whether a fetch succeeded
    // at provision time).
    base_from_stale_view: bool,
) -> Result<(), String> {
    // #1888 phase-2: the agent claiming a branch is acting on any pending
    // ci-handoff for it — resolve the track (re-nudge stops). Scoped to this
    // agent's own tracks; other targets' handoffs for the branch are untouched.
    let _ = crate::daemon::ci_handoff_track::resolve_claimed(home, agent, branch);
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all {}: {e}", dir.display()))?;
    let path = dir.join("binding.json");
    // S1: stable A lives outside runtime; legacy B stays for compatibility. A→B.
    let _agent_lock = acquire_agent_mutation_lock(home, agent)?;
    let _binding_lock = acquire_binding_file_lock(home, agent)?;
    // #2158 PR2: read the CURRENT on-disk binding UNDER the lock (the in-memory
    // index can be stale) — drives guard-b + the binding-CHANGE audit.
    let existing: Option<serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| parse_binding_guarded(&c));
    // #2158 PR2 (i) guard-b — identity-INDEPENDENT prevention. Reject a rebind that
    // moves a LIVE binding to a DIFFERENT branch with NO intervening release (a
    // release clears binding.json, so a present binding + an existing worktree on a
    // DIFFERENT branch == no release happened). ALLOWS first-bind (no existing) and
    // same-branch idempotent reuse (#2226). Gates on STATE, so — unlike a
    // caller-identity check — it is not defeated by the sub-agent/primary
    // indistinguishability: it stops a transient helper silently moving the primary's
    // live binding to a branch it never chose (#2158). Verified free: every legit
    // rebind flow is fresh-lease / same-branch-early-return / cross-branch-rejected
    // upstream (dispatch_hook LeaseConflict), so none does a live cross-branch bind-over.
    if let Some(ref ex) = existing {
        let ex_branch = ex
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let ex_worktree_live = ex
            .get("worktree")
            .and_then(|v| v.as_str())
            .map(|w| std::path::Path::new(w).exists())
            .unwrap_or(false);
        if ex_worktree_live && !ex_branch.is_empty() && ex_branch != branch {
            // #2496 (adversarial consensus d-20260701140903334693-1): guard-b's
            // blanket rejection can't distinguish a genuine live cross-branch
            // bind-over (the #2158 danger this guard exists for) from "the
            // worktree was ALREADY cleanly switched to the requested branch
            // out-of-band (a plain `git checkout`, bypassing bind/release) and
            // binding.json just hasn't caught up". `same_agent_metadata_catchup_allowed`
            // is the strict, same-agent-only exception for the latter: pure
            // metadata catch-up, zero mutation to the worktree. Any condition
            // failing keeps the original reject — this only WIDENS what's
            // allowed, never narrows the existing guard.
            if same_agent_metadata_catchup_allowed(
                home,
                agent,
                worktree,
                source_repo,
                branch,
                ex_branch,
            ) {
                tracing::info!(
                    %agent, %ex_branch, %branch,
                    "#2496: same-agent stale binding metadata caught up to the worktree's \
                     already-correct branch — no worktree/branch mutation"
                );
            } else {
                crate::event_log::log(
                    home,
                    "binding_rebind_rejected",
                    agent,
                    &format!(
                        "live binding on '{ex_branch}' — refused cross-branch rebind to '{branch}' (#2158); {}",
                        crate::event_log::caller_process_context()
                    ),
                );
                return Err(format!(
                    "#2158: agent '{agent}' is bound to a LIVE worktree on branch '{ex_branch}' — \
                     release_worktree first before binding to '{branch}' (no silent cross-branch rebind)"
                ));
            }
        }
    }
    let wt_str = worktree.display().to_string();
    let src_str = source_repo.display().to_string();
    let mut binding = json!({
        "version": BINDING_SCHEMA_VERSION,
        "agent": agent,
        "task_id": task_id,
        "branch": branch,
        "issued_at": chrono::Utc::now().to_rfc3339(),
    });
    if !wt_str.is_empty() {
        binding["worktree"] = json!(wt_str);
    }
    if !src_str.is_empty() {
        binding["source_repo"] = json!(src_str);
        register_managed_repo(home, &src_str);
    }
    if base_from_stale_view {
        binding["base_from_stale_view"] = json!(true);
    }
    if let Some(BindingProvenance::DaemonProvisionedReview { provisioned_head }) = provenance {
        binding["checkout_purpose"] = json!("disposable_review");
        binding["provenance"] = json!("DaemonProvisionedReview");
        binding["provisioned_head"] = json!(provisioned_head);
    }
    let body = serde_json::to_string_pretty(&binding).unwrap_or_default();
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("atomic_write {}: {e}", path.display()))?;
    // #1651: HMAC-sign the binding (sidecar `binding.json.sig`, mirroring #1576's
    // operator-mode.json scheme) so the agend-git shim can reject a blind
    // self-authorization rewrite — an injected agent editing its own `branch`
    // without re-signing makes the shim's verify fail → unbound → push denied.
    // Defense-in-depth against injection blind-write, NOT a security boundary
    // (a same-uid agent could read the key + re-sign; true sealing needs
    // OS-isolation, parked #1653). Best-effort: a missing/failed sidecar leaves
    // the binding unsigned → the shim fails CLOSED (denies), never open.
    // embedder P1b: BINDING signer → core `sign_binding` (SAME bare-hex bytes; rollback = revert to `config_integrity::sign`; rationale in Cargo.toml + golden `daemon_signer_core_swap_is_byte_identical_p1b`).
    match agentic_git_core::integrity_core::sign_binding(home, body.as_bytes()) {
        Ok(tag) => {
            if let Err(e) = crate::store::atomic_write(&binding_sig_path(&dir), tag.as_bytes()) {
                tracing::warn!(%agent, error = %e,
                    "#1651 binding sidecar write failed — shim fails closed (deny) until re-bind");
            }
        }
        Err(e) => tracing::warn!(%agent, error = %e,
            "#1651 binding HMAC sign failed — shim fails closed (deny) until re-bind"),
    }
    if let Ok(mut map) = binding_index().write() {
        map.insert(index_key(home, agent), binding);
    }
    // #2158 PR2 (ii) binding-CHANGE audit — DETECTION, not attribution (a sub-agent
    // shares the primary's identity/process tree, so we log the CHANGE + caller
    // process context, not "who"). Fires on a CREATE (first bind) or a CHANGE
    // (branch / worktree differs), making the #2158 first-bind hijack — and the
    // #2234 reset-to-origin/main churn — visible even though identity can't prevent
    // it. The realistic defense for the variant guard-b can't catch (first-bind).
    let prev_branch = existing
        .as_ref()
        .and_then(|e| e.get("branch").and_then(|v| v.as_str()))
        .map(String::from);
    let prev_worktree = existing
        .as_ref()
        .and_then(|e| e.get("worktree").and_then(|v| v.as_str()))
        .map(String::from);
    let changed = existing.is_none()
        || prev_branch.as_deref() != Some(branch)
        || prev_worktree.as_deref() != Some(wt_str.as_str());
    if changed {
        crate::event_log::log(
            home,
            "binding_changed",
            agent,
            &format!(
                "branch={branch} worktree={wt_str} prev_branch={} task_id={task_id}; {}",
                prev_branch.as_deref().unwrap_or("<none>"),
                crate::event_log::caller_process_context()
            ),
        );
    }
    // #2158 GR1: notify the operator on an AGENT SELF-CLAIM (`bind_self` /
    // `repo checkout bind:true`) — the first-bind-hijack / accidental-sub-agent vector
    // guard-b cannot prevent (identity-indistinguishable). NOT gated on `changed`: the
    // dispatch flow double-binds (lease then re-bind), so a self-claim's intent-bind is
    // frequently a no-op (changed=false); the per-(agent,branch) sidecar in the helper
    // gives fire-once. #2533: ALSO gated on an empty `task_id` — a self-claim that
    // carries a task_id (e.g. `bind_self(task_id=...)` / `repo checkout bind:true
    // task_id=...`) is attributable to a task and is treated as in-dispatch, so it
    // does not warn. A single-target auto-create DISPATCH (is_self_claim=false)
    // never reaches this branch regardless of task_id (see the param doc above).
    if is_self_claim && task_id.is_empty() {
        notify_operator_out_of_dispatch_bind(home, agent, branch, &wt_str, prev_branch.as_deref());
    }
    Ok(())
}

mod review_lease;
pub(crate) use review_lease::{
    augment_binding_with_lease, retarget_disposable_review_binding_for_receipt,
    try_augment_review_lease,
};

/// #2158 GR1: surface an OUT-OF-DISPATCH binding CREATE/CHANGE (no task_id) to the
/// operator — the realistic accidental-sub-agent / first-bind-hijack vector that
/// guard-b can't prevent. Dedup is per `(agent, branch)` via a runtime-dir sidecar
/// so a stable or re-applied binding never re-notifies (fire-once). Honest about
/// attribution: the daemon CANNOT name the caller — a transient Claude Code Task
/// sub-agent shares the primary's `instance_name` AND its process, so neither
/// `instance_name` nor a pid separates them (the binding audit's
/// `caller_process_context` is daemon-side regardless). Best-effort + file-based
/// (the same `inbox::notify_agent` primitive as `canonical_auto_stash`); never
/// blocks and runs under the caller's binding lock, so no network/no spawn.
/// #2347 GR1: per-agent sidecar listing the branches that have already surfaced
/// an out-of-dispatch notify — fire-once-per-branch dedup WITHIN a bind cycle.
/// Cleared by [`unbind`] so a release resets the latch (bug-audit Rank6): without
/// the clear, a self-claim → notify → release left the branch latched forever, so
/// a LATER real re-claim hijack of the same branch was silently swallowed as
/// "already notified". The clear is scoped to release, so intra-cycle dedup
/// (repeated binds to the same branch within one cycle) is preserved.
const OUT_OF_DISPATCH_SIDECAR: &str = ".out_of_dispatch_notified";

/// #2347: the live delivery is routed to the bound agent's TEAM ORCHESTRATOR
/// (`out_of_dispatch_notify_recipient`, fallback `general`) rather than the
/// global operator inbox, and SKIPPED when the orchestrator resolves to the
/// agent itself (a top-level lead) — a self-notify is pure noise. The event-log
/// marker below stays unconditional, so this routing never weakens GR1 detection.
fn notify_operator_out_of_dispatch_bind(
    home: &Path,
    agent: &str,
    branch: &str,
    wt_str: &str,
    prev_branch: Option<&str>,
) {
    let sidecar = crate::paths::runtime_dir(home)
        .join(agent)
        .join(OUT_OF_DISPATCH_SIDECAR);
    // Dedup: skip if this (agent, branch) was already surfaced.
    if let Ok(seen) = std::fs::read_to_string(&sidecar) {
        if seen.lines().any(|l| l == branch) {
            return;
        }
    }
    // Mark BEFORE notifying so a delivery hiccup can't drive a re-notify loop.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)
    {
        use std::io::Write;
        let _ = writeln!(f, "{branch}");
    }
    // Distinct, greppable marker (also the regression-test hook) — separate from the
    // always-on `binding_changed` audit above.
    crate::event_log::log(
        home,
        "binding_out_of_dispatch",
        agent,
        &format!(
            "branch={branch} worktree={wt_str} prev_branch={}",
            prev_branch.unwrap_or("<none>")
        ),
    );
    // #2347: route the operator-visible delivery to the bound agent's team
    // orchestrator (fallback `general`), skipping a self-notify. `None` ⟹ the
    // recipient would be the agent itself (a top-level lead) ⟹ pure noise ⟹
    // skip; the event-log marker above already recorded it for GR1 forensics.
    let recipient = match out_of_dispatch_notify_recipient(home, agent) {
        Some(r) => r,
        None => return,
    };
    // Best-effort, file-based inbox to the resolved recipient — mirrors `canonical_auto_stash`.
    let text = out_of_dispatch_notify_body(agent, branch, prev_branch);
    crate::inbox::notify_agent(
        home,
        &recipient,
        &crate::inbox::NotifySource::System("binding_out_of_dispatch"),
        &text,
    );
}

/// #2533: the notification BODY text for an out-of-dispatch self-claim bind.
/// Extracted as a pure fn so the double-tag regression is unit-testable:
/// `NotifySource::System("binding_out_of_dispatch")` already renders the
/// `[system:binding_out_of_dispatch]` tag at the delivery-layer wrapper
/// (`inbox::format_notification_for_inject`) — this body must NOT hardcode
/// the same tag, or the rendered notification doubles it.
fn out_of_dispatch_notify_body(agent: &str, branch: &str, prev_branch: Option<&str>) -> String {
    let was = prev_branch
        .map(|p| format!(", was `{p}`"))
        .unwrap_or_default();
    format!(
        "instance `{agent}` bound to branch `{branch}`{was} \
         OUTSIDE a task dispatch (no task_id). If unexpected, a transient sub-agent or a \
         self-claim may have moved this instance's worktree. The daemon CANNOT name the exact \
         caller — a Task sub-agent shares the primary's identity and process. Inspect with \
         `binding_state instance={agent}`; undo with `release_worktree`. (#2158 GR1)"
    )
}

/// #2347: resolve WHO receives the out-of-dispatch live notice for `agent`.
/// Routes to the agent's team orchestrator so an operator-directed team-lead
/// bind doesn't spam the global `general` operator inbox; a teamless/orphan
/// agent (no team lists it, or its team has no orchestrator) falls back to
/// `general`. Returns `None` when the recipient would be the bound agent itself
/// (a top-level lead's orchestrator resolves to itself) — a self-notify is pure
/// noise and is skipped. Purely a routing decision: the #2158 GR1 event-log
/// marker is emitted unconditionally by the caller regardless, so this never
/// attempts the structurally-impossible legit-vs-stolen discrimination and
/// never weakens GR1 detection.
fn out_of_dispatch_notify_recipient(home: &Path, agent: &str) -> Option<String> {
    let recipient =
        crate::fleet::team_orchestrator_for(home, agent).unwrap_or_else(|| "general".to_string());
    if recipient == agent {
        return None;
    }
    Some(recipient)
}

/// #1651: the HMAC sidecar path for a binding dir. The agend-git shim hard-codes
/// the same `binding.json.sig` name (it cannot import this — separate binary).
fn binding_sig_path(dir: &Path) -> PathBuf {
    dir.join("binding.json.sig")
}

// #1688 (codex): there is intentionally NO startup "re-sign unsigned bindings"
// pass. It was a wash-white hole — keying the decision on "has no sidecar" cannot
// distinguish a legit pre-#1651 binding from an attacker that tampered
// binding.json AND deleted the sidecar, and the daemon has NO trusted source at
// startup to tell them apart (`reconcile_orphan_leases` is log-only; binding.json
// is the sole on-disk record; bindings are only (re)established via
// `dispatch_auto_bind_lease`/`bind_full` at dispatch time). So a sidecar-less
// binding is left UNSIGNED → the shim fails closed (unbound → deny), exactly like
// a fresh, never-dispatched agent. A legit binding re-signs on its next dispatch
// or `bind_self`. The rollout cost — agents whose binding survives the activating
// restart are denied pushes until re-dispatched — is a VISIBLE, self-healing
// trade-off (the agent reports `blocked`), deliberately chosen over a SILENT
// wash-white. (Activating restart: the operator re-dispatches / has running
// agents `bind_self` once; one-time.)

/// Returns Some(agent_name) if any other agent holds the lease for this
/// **(source_repo, branch)**. Used by dispatch_auto_bind_lease / repo-checkout to
/// enforce cross-agent lease uniqueness.
///
/// #2117 P3b: the lease key is now `(source_repo, branch)` — the SAME branch name
/// in a DIFFERENT repo is a DIFFERENT lease and does not conflict. Pass `""` for
/// `source_repo` to keep the legacy branch-only semantics (callers that only want
/// "who holds this branch anywhere", e.g. pr-state / auto-arm / auto-release,
/// which does its own slug-based repo guard).
///
/// **Backward-compat wildcard gate (reviewer-2, the P3b core risk)**: `bind_full`
/// only writes the `source_repo` field when non-empty, so a pre-P3b "legacy" live
/// binding can lack it. On rescan after P3b ships, requiring `source_repo`
/// equality would make `None != Some(repo)` MISS that legacy binding → a second
/// agent binds the same branch → **double-bind**. To prevent that, a missing/empty
/// `source_repo` on EITHER side (the scanned binding OR the query) falls back to
/// **branch-only** exclusion (match-any); only when BOTH carry a non-empty
/// `source_repo` is the match tightened to `(source_repo, branch)`. Fail-closed:
/// when in doubt (a field is absent), we still treat the branch as taken.
pub fn scan_existing_branch_binding(
    home: &Path,
    source_repo: &str,
    branch: &str,
    exclude_agent: &str,
) -> Option<String> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let agent = entry.file_name().to_string_lossy().to_string();
        if agent == exclude_agent {
            continue;
        }
        let binding_path = entry.path().join("binding.json");
        let Ok(content) = std::fs::read_to_string(&binding_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if v["branch"].as_str() != Some(branch) {
            continue;
        }
        // Wildcard backward-compat gate: branch-only exclusion unless BOTH the
        // query and the scanned binding carry a non-empty source_repo.
        let binding_repo = v["source_repo"].as_str().unwrap_or("");
        let both_present = !source_repo.is_empty() && !binding_repo.is_empty();
        if !both_present || binding_repo == source_repo {
            return Some(agent);
        }
    }
    None
}

/// #2550 W3 Wave1: fresh (uncached) enumeration of every agent under `home`'s
/// runtime dir whose `binding.json` is present and parses as JSON. An agent
/// with a missing/unreadable/corrupt binding.json is silently skipped —
/// every existing scan-all-agents call site already tolerated this, so the
/// shared primitive preserves it uniformly. Field-level extraction and any
/// further validation (branch match, empty-task_id, dedup, ...) is the
/// caller's job.
pub(crate) fn binding_scan_all(home: &Path) -> Vec<(String, serde_json::Value)> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let Ok(entries) = std::fs::read_dir(&runtime_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let agent_name = entry.file_name().to_string_lossy().into_owned();
            let binding_path = crate::paths::binding_path(home, &agent_name);
            let content = std::fs::read_to_string(&binding_path).ok()?;
            let v: serde_json::Value = serde_json::from_str(&content).ok()?;
            Some((agent_name, v))
        })
        .collect()
}

/// PR-3 (t-ci-ready-pr3-arm-not-armed): the distinct `source_repo` paths of every
/// LIVE bound branch (each `runtime/<agent>/binding.json`'s `source_repo`).
///
/// The pr_state scanner seeds its poll-repo list from these (after resolving each
/// to a gh `owner/repo` slug) so a repo that has a bound branch but NO pr-state
/// file yet — a bypass / non-dispatch PR — is still polled. Without this seed the
/// scanner only ever polls repos that already have a pr-state, so a brand-new
/// unwatched PR in an otherwise-unseeded repo would never be discovered (the
/// #1782 gap). Returns raw paths (slug resolution is the caller's job) to keep
/// this module free of the git/scm dependency.
mod managed_repos;
#[cfg(test)]
pub(crate) use managed_repos::read_managed_repo_registry;
pub(crate) use managed_repos::register_managed_repo;
pub use managed_repos::{all_managed_repos, bound_source_repos};

/// Read the current binding for an agent.
/// Hot path: returns from in-memory index (read lock). Cold path
/// (first access per agent): acquires write lock, double-checks,
/// then reads disk and populates. Disk read under write lock
/// prevents stale resurrection when a concurrent unbind() deletes
/// the file between our miss and our insert.
pub fn read(home: &Path, agent: &str) -> Option<serde_json::Value> {
    let key = index_key(home, agent);
    if let Ok(map) = binding_index().read() {
        if let Some(v) = map.get(&key) {
            return Some(v.clone());
        }
    }
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    if let Ok(mut map) = binding_index().write() {
        if let Some(v) = map.get(&key) {
            return Some(v.clone());
        }
        let v: serde_json::Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|c| parse_binding_guarded(&c))?;
        map.insert(key, v.clone());
        return Some(v);
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| parse_binding_guarded(&c))
}

/// Check if an agent is bound in a daemon-managed worktree.
/// Returns true if the agent has a binding with a worktree path that
/// contains the `.agend-managed` marker file.
pub fn is_agent_in_managed_worktree(home: &Path, agent: &str) -> bool {
    read(home, agent)
        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
        .map(|wt| wt.join(".agend-managed").exists())
        .unwrap_or(false)
}

/// #2755 R4: binding-adoption query for checkout recovery, extracted to the
/// `worktree_state` submodule; re-exported for the stable `crate::binding` path.
mod worktree_state;
pub(crate) use worktree_state::{refresh_cached, worktree_binding_state, WorktreeBindingState};

/// Install the prepare-commit-msg hook into a worktree via core.hooksPath.
/// Points to `$AGEND_HOME/hooks/` unified directory.
/// Installs bash hook on Unix, PowerShell hook on Windows.
pub fn install_hooks(home: &Path, worktree: &Path) {
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir).ok();

    // Extract embedded hook scripts (both platforms for portability).
    let bash_hook = include_str!("../assets/hooks/prepare-commit-msg");
    let bash_path = hooks_dir.join("prepare-commit-msg");
    if let Err(error) = std::fs::write(&bash_path, bash_hook) {
        tracing::error!(path = %bash_path.display(), %error, "failed to install daemon-owned prepare-commit-msg hook");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bash_path, std::fs::Permissions::from_mode(0o755));
    }

    // Windows: also install PowerShell version.
    let ps_hook = include_str!("../assets/hooks/prepare-commit-msg.ps1");
    let ps_path = hooks_dir.join("prepare-commit-msg.ps1");
    if let Err(error) = std::fs::write(&ps_path, ps_hook) {
        tracing::error!(path = %ps_path.display(), %error, "failed to install daemon-owned prepare-commit-msg PowerShell hook");
    }

    // #2234: canonical-HEAD-detach instrument. A reference-transaction hook dropped
    // in `<repo>/.git/hooks/` is SHADOWED by this same `core.hooksPath` and never
    // fires (the empty-culprit-log mystery, Phase 1) — it MUST live here. It is the
    // one git-CLI layer no caller can bypass (real git always honors core.hooksPath,
    // independent of AGEND_GIT_BYPASS). The script is fail-open (observes only in the
    // `committed` phase + always exits 0, so it can NEVER abort a ref transaction)
    // and scoped to the bug signature (HEAD detached to origin/main), so routine ref
    // churn is not logged. Instrument-only — no behavior change.
    let reftx_hook = include_str!("../assets/hooks/reference-transaction");
    let reftx_path = hooks_dir.join("reference-transaction");
    let _ = std::fs::write(&reftx_path, reftx_hook);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&reftx_path, std::fs::Permissions::from_mode(0o755));
    }

    // Set core.hooksPath on the worktree.
    // W1.2 class-2: BEHAVIOR DELTA — adds AGEND_GIT_BYPASS (this site previously
    // ran raw `git` with NO bypass env). `git config` against a fleet-managed
    // worktree is exactly the forgot-bypass latent class (#821/#1463): a daemon
    // git mutation should always bypass the agend-git shim. Intended fix.
    let hooks_set = crate::git_helpers::git_ok(
        worktree,
        &["config", "core.hooksPath", &hooks_dir.display().to_string()],
    );
    // CR-2026-06-14: surface the silent failure. If the worktree isn't a git repo
    // yet (or the config write fails) the prepare-commit-msg hook is never wired
    // up; warn so an operator can notice rather than the binding/commit-msg
    // enforcement vanishing silently for that worktree.
    if !hooks_set {
        tracing::warn!(
            worktree = %worktree.display(),
            "install_hooks: `git config core.hooksPath` failed — prepare-commit-msg hook NOT wired up for this worktree"
        );
    }
}

/// Install hooks on all existing worktrees (daemon startup reconcile).
pub fn reconcile_hooks(home: &Path) {
    // New layout: <home>/worktrees/<agent>/<branch>/. #restart-freeze: marker-walk
    // to the real worktree LEAVES (mirrors gc_candidates / collect_managed_worktrees).
    // The old fixed 2-level descent hit the INTERMEDIATE dir for a slash branch
    // (`<agent>/fix`, NOT a git worktree) → `git config core.hooksPath` failed
    // there → the prepare-commit-msg hook was silently never installed for
    // slash-branch worktrees (the common case), plus one wasted failing `git
    // config` subprocess per intermediate dir on the boot critical path.
    let new_root = crate::worktree_pool::daemon_managed_worktree_root(home);
    let mut worktrees = Vec::new();
    crate::worktree_pool::collect_managed_worktrees(
        &new_root,
        crate::worktree_pool::MARKER_WALK_MAX_DEPTH,
        &mut worktrees,
    );
    for wt in &worktrees {
        install_hooks(home, wt);
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let worktrees_base = crate::paths::workspace_dir(home);
    if !worktrees_base.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&worktrees_base) {
        for entry in entries.flatten() {
            let wt_dir = entry.path().join(".worktrees");
            if wt_dir.is_dir() {
                if let Ok(wts) = std::fs::read_dir(&wt_dir) {
                    for wt in wts.flatten() {
                        if wt.path().is_dir() {
                            install_hooks(home, &wt.path());
                        }
                    }
                }
            }
        }
    }
}

/// Git/kill shim installation — see [`shim_install::symlink_shim`]. Homed in its
/// own module so the #2524-P2 flag-gated backend-swap logic AND its unit tests
/// don't push this core file past the anti-monolith LOC ceiling. Re-exported so
/// the public path stays `crate::binding::symlink_shim` (call site + wiring
/// invariant unchanged).
mod shim_install;
pub use shim_install::symlink_shim;

/// Clear orphan bindings (agents no longer in registry).
/// Called at daemon startup, after the singleton `.daemon.lock` is acquired by
/// normal bootstrap/app/handoff paths. This is a source-pinned pre-agent
/// exemption from the per-agent lifecycle permit: no competing daemon writer
/// can enter until bootstrap releases that singleton lock.
pub fn reconcile_orphans(home: &Path) {
    let runtime_dir = crate::paths::runtime_dir(home);
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if binding_path.exists() {
                let entry_path = entry.path();
                let agent_name = entry_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                // Check if binding is stale (issued_at > 24h ago).
                if let Ok(content) = std::fs::read_to_string(&binding_path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(issued) = v["issued_at"].as_str() {
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(issued) {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(dt.with_timezone(&chrono::Utc));
                                if age > chrono::Duration::hours(24) {
                                    // #693: check heartbeat — if agent is still active, don't delete
                                    let hb =
                                        crate::daemon::heartbeat_pair::snapshot_for(agent_name);
                                    let hb_age_ms = crate::daemon::heartbeat_pair::now_ms()
                                        .saturating_sub(hb.heartbeat_at_ms);
                                    if hb_age_ms < 3_600_000 {
                                        // Heartbeat within 1h — agent still active, skip
                                        continue;
                                    }
                                    reaper_notify::remove_and_notify(
                                        home,
                                        agent_name,
                                        &binding_path,
                                        &v,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod review_repro_agent_binding;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
