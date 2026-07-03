//! `target/` retention sweep for daemon-managed worktrees.

use super::{
    agent_from_layout, collect_managed_worktrees, daemon_managed_worktree_root, is_daemon_managed,
    MARKER_WALK_MAX_DEPTH,
};
use std::path::{Path, PathBuf};

/// Default staleness age for the `target/` sweep — no build activity within
/// this window ⇒ eligible. Conservative: a 2-day-idle build cache is cheap to
/// regenerate relative to the GBs reclaimed.
const TARGET_GC_AGE_HOURS_DEFAULT: u64 = 48;

/// Resolve the `target/` sweep config from env. Returns `None` when the sweep
/// is disabled via `AGEND_TARGET_GC_DISABLE` (operator kill-switch).
/// `(max_age, min_size_bytes)`:
///   - `AGEND_TARGET_GC_AGE_HOURS` (default 48) — staleness window.
///   - `AGEND_TARGET_GC_MIN_SIZE_BYTES` (default 0 = no floor) — skip targets
///     smaller than this (avoid churn on trivially-small build dirs).
pub fn target_gc_config() -> Option<(std::time::Duration, u64)> {
    if std::env::var_os("AGEND_TARGET_GC_DISABLE").is_some() {
        return None;
    }
    let hours = std::env::var("AGEND_TARGET_GC_AGE_HOURS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(TARGET_GC_AGE_HOURS_DEFAULT);
    let min_size = std::env::var("AGEND_TARGET_GC_MIN_SIZE_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    Some((
        std::time::Duration::from_secs(hours.saturating_mul(3600)),
        min_size,
    ))
}

/// A managed-worktree `target/` dir eligible for the retention sweep.
#[derive(Debug, Clone)]
pub struct TargetSweepCandidate {
    pub worktree: PathBuf,
    pub target: PathBuf,
    pub agent: String,
    /// Seconds since the most-recent modification anywhere under `target/`.
    pub idle_secs: u64,
    pub size_bytes: u64,
}

/// Outcome of one `target/` removal.
#[derive(Debug, Clone)]
pub struct TargetSweepResult {
    pub target: PathBuf,
    pub agent: String,
    pub removed: bool,
    pub freed_bytes: u64,
    pub error: Option<String>,
}

/// Fail-CLOSED activity probe for a DESTRUCTIVE sweep. Returns `true` if ANY
/// entry under `path` (inclusive) was modified at/after `cutoff` OR if any
/// `symlink_metadata`/`read_dir`/mtime call fails — i.e. uncertainty ⇒ `true` ⇒
/// the caller MUST NOT delete. Returns `false` (eligible) ONLY when the ENTIRE
/// tree was readable AND every mtime is older than `cutoff`. Uses
/// `symlink_metadata` and recurses only into REAL directories, so it never
/// follows symlinks (can't escape the tree or loop). Early-exits on the first
/// fresh/unreadable entry (fast path for active builds).
fn tree_active_or_unreadable(path: &Path, cutoff: std::time::SystemTime) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return true; // can't stat ⇒ uncertain ⇒ fail-closed (treat as active)
    };
    match meta.modified() {
        Ok(m) if m >= cutoff => return true, // fresh ⇒ active
        Ok(_) => {}
        Err(_) => return true, // no mtime ⇒ uncertain ⇒ fail-closed
    }
    if meta.file_type().is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return true; // can't list a dir we're about to delete ⇒ fail-closed
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return true; // unreadable entry ⇒ fail-closed
            };
            if tree_active_or_unreadable(&entry.path(), cutoff) {
                return true;
            }
        }
    }
    false
}

/// Total bytes under `path` (real files; symlinks counted by their own link
/// size, never followed). Best-effort — unreadable entries are skipped.
fn tree_size_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total = total.saturating_add(tree_size_bytes(&entry.path()));
            }
        }
        total
    } else {
        meta.len()
    }
}

/// Newest mtime anywhere under `path` (inclusive); `None` if unreadable. Full
/// walk (no early-exit) — used only to compute `idle_secs` for the preview.
fn tree_newest_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok();
    if meta.file_type().is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Some(m) = tree_newest_mtime(&entry.path()) {
                    newest = Some(match newest {
                        Some(n) if n >= m => n,
                        _ => m,
                    });
                }
            }
        }
    }
    newest
}

/// Operator-facing scope boundary (no-silent-coverage-cap, lead VET condition).
/// This sweep reclaims `target/` ONLY for `.agend-managed` `home/worktrees`
/// worktrees whose owner is GONE from the daemon roster, or in the roster but
/// bound ELSEWHERE / unbound (instance-gone / rebound-away / orphan). It
/// deliberately does NOT reclaim any worktree whose owner is in the roster AND
/// currently bound there — REGARDLESS of liveness — because that owner can start
/// a build at any instant (mtime cannot prevent a build starting between check
/// and delete). It also does NOT touch legacy markerless `workspace/<agent>/target`
/// or agent-self-built `.claude/worktrees/*/target` (the larger fleet consumers,
/// but markerless = the operator-data danger zone, left to a separate
/// authoritative binding-registry sweep). Surfaced in dry-run/log so
/// reclaimed-space figures never imply the fleet disk problem is fully solved.
pub const TARGET_SWEEP_SCOPE_NOTE: &str = "scope: sweeps stale target/ ONLY for .agend-managed home/worktrees worktrees whose owner is gone from the roster, or bound elsewhere/unbound (instance-gone / rebound-away / orphan). NOT reclaimed: any currently-bound worktree (regardless of liveness — a build can start anytime), legacy markerless workspace/<agent>/target, or .claude/worktrees/*/target.";

/// Marker-STRICT enumerator for the `target/` sweep (r6/r4 #1 fix): ONLY
/// daemon-leased worktrees under `home/worktrees` that carry the
/// `.agend-managed` marker, via `collect_managed_worktrees`. Deliberately does
/// NOT union `workspace_gitlink_worktrees` — that scan collects `.git`-gitlink
/// dirs WITHOUT a marker (incl. operator-owned, interrupted-reconcile
/// worktrees), which is by-design for read-only LISTING but a footgun for a
/// DESTRUCTIVE sweep. A sweep must never inherit the looser enumeration.
fn target_sweep_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_managed_worktrees(
        &daemon_managed_worktree_root(home),
        MARKER_WALK_MAX_DEPTH,
        &mut out,
    );
    out
}

/// Resolve the canonical `home/worktrees` sweep root, or `None` if it is unsafe
/// to sweep — root missing, root is a symlink, or root escapes the canonical
/// home (r6 #2 fix). Anchoring confinement to the canonical HOME and refusing a
/// symlinked root closes "canonicalize-a-symlinked-root-then-trust-it" escapes.
/// (FIX1 already drops the workspace enumeration that was the real escape
/// vector; this is defense-in-depth.)
pub(crate) fn safe_managed_root(home: &Path) -> Option<PathBuf> {
    let root = daemon_managed_worktree_root(home);
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.file_type().is_symlink() => return None, // never sweep through a symlinked root
        Ok(_) => {}
        Err(_) => return None, // missing/unreadable ⇒ nothing to sweep
    }
    let canon_home = dunce::canonicalize(home).ok()?;
    let canon_root = dunce::canonicalize(&root).ok()?;
    canon_root.starts_with(&canon_home).then_some(canon_root)
}

/// Validate `target` is safe to hard-delete: the managed root must be safe
/// (`safe_managed_root` — non-symlink, under canonical home), `target` must be a
/// REAL directory (not a symlink — a symlinked `target` could point at the
/// canonical repo's target), and `canonicalize(target)` must resolve under the
/// canonical managed root. Returns the validated canonical target path.
pub(crate) fn validate_target_for_delete(home: &Path, target: &Path) -> Result<PathBuf, String> {
    let canon_root = safe_managed_root(home).ok_or_else(|| {
        "refusing: managed root is missing, a symlink, or escapes home".to_string()
    })?;
    let meta = std::fs::symlink_metadata(target).map_err(|e| format!("stat failed: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("refusing: `target` is a symlink (could escape to canonical)".to_string());
    }
    if !meta.file_type().is_dir() {
        return Err("refusing: `target` is not a directory".to_string());
    }
    let canon = dunce::canonicalize(target).map_err(|e| format!("canonicalize failed: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!(
            "refusing: {} does not resolve under the managed root {}",
            canon.display(),
            canon_root.display()
        ));
    }
    Ok(canon)
}

/// Stable-signal protect predicate (round-4, r6 re-DUAL — DROPS the flappy
/// `liveness` signal that caused the bound-but-not-yet-live TOCTOU). A worktree
/// is PROTECTED when its owner instance is in the `roster` AND its binding
/// currently points HERE — meaning a process can still build in it. The binding
/// is read from DISK (not the in-process cache) so the caller's held
/// `.binding.json.lock` makes it authoritative (no bind can mutate it).
///
/// - owner unresolvable: PROTECT (fail-closed).
/// - owner NOT in roster (deleted): sweepable — no process can ever bind here again.
/// - in roster, binding points HERE: PROTECT — could build (closes the
///   bound-but-not-yet-live race).
/// - in roster, binding elsewhere/absent: sweepable (can't rebind here while the
///   caller holds the bind lock).
/// - in roster, binding UNREADABLE/malformed: PROTECT (fail-closed).
fn predicate_protects(home: &Path, wt: &Path, roster: &std::collections::HashSet<String>) -> bool {
    let Some(owner) = agent_from_layout(home, wt) else {
        return true; // unresolvable owner ⇒ fail-closed protect
    };
    if !roster.contains(&owner) {
        return false; // instance gone (deleted) ⇒ no process can build ⇒ sweepable
    }
    let binding_path = crate::paths::binding_path(home, &owner);
    if !binding_path.exists() {
        return false; // in roster but unbound ⇒ sweepable (can't rebind under our lock)
    }
    // Read the FILE directly (not the cache) so the caller's held lock is the
    // source of truth for the binding.
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match std::fs::read_to_string(&binding_path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => match v["worktree"].as_str() {
                Some(bw) => canon(std::path::Path::new(bw)) == canon(wt), // bound HERE ⇒ PROTECT
                None => true, // malformed (no worktree field) ⇒ fail-closed PROTECT
            },
            Err(_) => true, // parse error ⇒ fail-closed PROTECT
        },
        Err(_) => true, // exists but unreadable ⇒ fail-closed PROTECT
    }
}

/// Snapshot the daemon's known-agent roster (stable membership, NOT flappy
/// process-liveness — round-4) for a sweep pass.
fn sweep_roster(home: &Path) -> std::collections::HashSet<String> {
    crate::runtime::list_agents_with_fallback(home)
        .into_iter()
        .collect()
}

/// Enumerate daemon-leased worktrees (marker-strict, `home/worktrees` only —
/// see [`target_sweep_worktrees`] / [`TARGET_SWEEP_SCOPE_NOTE`]) whose `target/`
/// build dir is STALE (no activity within `max_age`, fail-closed), NOT protected
/// by [`predicate_protects`] (owner-in-roster + bound-here), and at least
/// `min_size` bytes. Resolves the roster itself; tests use the `_with_roster`
/// variant to inject a deterministic roster.
pub fn target_sweep_candidates(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
) -> Vec<TargetSweepCandidate> {
    target_sweep_candidates_with_roster(home, max_age, min_size, &sweep_roster(home))
}

/// Roster-injected core of [`target_sweep_candidates`]. NOTE: the protect
/// predicate here is BEST-EFFORT (no `.binding.json.lock` held — this is the
/// enumeration/dry-run pass). The AUTHORITATIVE, lock-frozen protect check runs
/// in [`target_sweep_run_with_roster`] before each delete.
pub(crate) fn target_sweep_candidates_with_roster(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
    roster: &std::collections::HashSet<String>,
) -> Vec<TargetSweepCandidate> {
    // SAFETY 2: never enumerate through a symlinked / escaping managed root.
    if safe_managed_root(home).is_none() {
        return Vec::new();
    }
    let now = std::time::SystemTime::now();
    let cutoff = now.checked_sub(max_age).unwrap_or(now);
    let mut out = Vec::new();
    for wt in target_sweep_worktrees(home) {
        // SAFETY 4 (active-build): owner in roster + bound here ⇒ a process can
        // build at any instant ⇒ exclude. (Best-effort here; re-checked under
        // the bind lock in the run pass.)
        if predicate_protects(home, &wt, roster) {
            continue;
        }
        let target = wt.join("target");
        // SAFETY 3: real directory only (symlink_metadata → is_dir is false for
        // a symlink-to-dir, so a symlinked `target` is skipped here too).
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        // SAFETY 5 (fail-closed): active build OR any unreadable entry ⇒ skip.
        if tree_active_or_unreadable(&target, cutoff) {
            continue;
        }
        let size_bytes = tree_size_bytes(&target);
        if size_bytes < min_size {
            continue;
        }
        let idle_secs = tree_newest_mtime(&target)
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let agent = agent_from_layout(home, &wt).unwrap_or_default();
        out.push(TargetSweepCandidate {
            worktree: wt,
            target,
            agent,
            idle_secs,
            size_bytes,
        });
    }
    out
}

/// Execute the `target/` sweep. For each candidate, try-acquire the OWNER's
/// `.binding.json.lock` (the SAME lock `bind_full` holds) and HOLD it through
/// {predicate → marker re-assert → fail-closed mtime recheck → delete}. While
/// held, no bind/rebind can occur (bind_full would block on it), so the protect
/// predicate is authoritative and a rebind-to-here cannot race the delete.
/// Contended lock ⇒ an active bind/release ⇒ SKIP this worktree this tick
/// (fail-safe). Resolves the roster itself; tests use the `_with_roster` variant.
pub fn target_sweep_run(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
) -> Vec<TargetSweepResult> {
    target_sweep_run_with_roster(home, max_age, min_size, &sweep_roster(home))
}

/// Roster-injected core of [`target_sweep_run`].
pub(crate) fn target_sweep_run_with_roster(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
    roster: &std::collections::HashSet<String>,
) -> Vec<TargetSweepResult> {
    let candidates = target_sweep_candidates_with_roster(home, max_age, min_size, roster);
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for c in &candidates {
        let skip = |reason: String| TargetSweepResult {
            target: c.target.clone(),
            agent: c.agent.clone(),
            removed: false,
            freed_bytes: 0,
            error: Some(reason),
        };
        let Some(owner) = agent_from_layout(home, &c.worktree) else {
            results.push(skip(
                "skipped: unresolvable owner (fail-closed)".to_string(),
            ));
            continue;
        };
        // Hold the owner's binding lock through {predicate → recheck → delete}.
        // bind_full holds this same lock while writing binding.json, so while we
        // hold it NO bind/rebind can occur — freezing the binding the predicate
        // reads and making a rebind-to-here-vs-delete race impossible. Non-blocking:
        // a held lock = an active bind/release in flight ⇒ skip this tick (fail-safe).
        let lock_path = crate::paths::runtime_dir(home)
            .join(&owner)
            .join(".binding.json.lock");
        let _lock = match crate::store::try_acquire_file_lock(&lock_path) {
            Ok(Some(l)) => l,
            Ok(None) => {
                results.push(skip(
                    "skipped: binding lock held (bind/release in flight)".to_string(),
                ));
                continue;
            }
            Err(e) => {
                results.push(skip(format!(
                    "skipped: binding lock error (fail-closed): {e}"
                )));
                continue;
            }
        };
        // UNDER LOCK — binding is frozen ⇒ this protect check is authoritative.
        if predicate_protects(home, &c.worktree, roster) {
            results.push(skip(
                "skipped: owner in roster + bound here (active-build protection)".to_string(),
            ));
            continue;
        }
        // Re-assert the daemon-managed marker at delete time.
        if !is_daemon_managed(&c.worktree) {
            results.push(skip(
                "skipped: worktree no longer .agend-managed".to_string(),
            ));
            continue;
        }
        // LOAD-BEARING fail-closed last line: any fresh mtime OR unreadable
        // entry ⇒ skip (don't delete).
        let now = std::time::SystemTime::now();
        let cutoff = now.checked_sub(max_age).unwrap_or(now);
        if tree_active_or_unreadable(&c.target, cutoff) {
            results.push(skip(
                "skipped: target became active/unreadable before delete".to_string(),
            ));
            continue;
        }
        // SAFETY 2 & 3: symlinked-root/target refusal + canonical-root confinement.
        // Removes ONLY `target/` (canon resolves under the managed root) — never
        // the worktree dir or source.
        let canon = match validate_target_for_delete(home, &c.target) {
            Ok(p) => p,
            Err(e) => {
                results.push(skip(e));
                continue;
            }
        };
        match std::fs::remove_dir_all(&canon) {
            Ok(()) => results.push(TargetSweepResult {
                target: c.target.clone(),
                agent: c.agent.clone(),
                removed: true,
                freed_bytes: c.size_bytes,
                error: None,
            }),
            Err(e) => results.push(skip(format!("remove failed: {e}"))),
        }
    }
    let removed_count = results.iter().filter(|r| r.removed).count();
    if removed_count > 0 {
        let freed: u64 = results
            .iter()
            .filter(|r| r.removed)
            .map(|r| r.freed_bytes)
            .sum();
        crate::event_log::log(
            home,
            "target_gc",
            "",
            &format!(
                "{removed_count} stale target/ dirs reclaimed (~{} MB)",
                freed / (1024 * 1024)
            ),
        );
    }
    results
}

/// Non-destructive preview of `target/` sweep candidates (mirrors `gc_dry_run`).
/// Resolves config from env; returns empty when the sweep is disabled.
pub fn target_sweep_dry_run(home: &Path) -> Vec<TargetSweepCandidate> {
    let Some((max_age, min_size)) = target_gc_config() else {
        return Vec::new();
    };
    let candidates = target_sweep_candidates(home, max_age, min_size);
    for c in &candidates {
        tracing::info!(
            agent = %c.agent,
            target = %c.target.display(),
            idle_secs = c.idle_secs,
            size_bytes = c.size_bytes,
            "target_sweep_dry_run candidate"
        );
    }
    if !candidates.is_empty() {
        let total: u64 = candidates.iter().map(|c| c.size_bytes).sum();
        crate::event_log::log(
            home,
            "target_sweep_dry_run",
            "",
            &format!(
                "{} stale target/ dirs (~{} MB) eligible — {}",
                candidates.len(),
                total / (1024 * 1024),
                TARGET_SWEEP_SCOPE_NOTE
            ),
        );
    }
    candidates
}
