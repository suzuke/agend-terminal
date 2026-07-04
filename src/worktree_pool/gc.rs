//! Worktree GC candidate enumeration and removal.

use super::{daemon_managed_worktree_root, is_daemon_managed, is_pinned, MANAGED_MARKER};
use std::path::{Path, PathBuf};

/// Grace period before a released worktree becomes a GC candidate.
const GC_GRACE_HOURS: i64 = 24;

/// t-worktree-leak PR-2: hard age cap for the force-reclaim backstop. A
/// never-released lease whose agent shows NO liveness AND whose `leased_at` is
/// older than this is force-reclaimed. Configurable (`AGEND_WORKTREE_FORCE_RECLAIM_DAYS`).
pub(crate) fn force_reclaim_age_days() -> i64 {
    crate::env_util::env_parse_min::<i64>("AGEND_WORKTREE_FORCE_RECLAIM_DAYS", 7, 1)
}

/// reviewer-2 #5: force-reclaim post-boot grace (seconds). After a daemon restart
/// the live-agent registry (the process-liveness signal) is empty until agents
/// re-spawn; suspend force-reclaim for this window so a mid-respawn agent is not
/// reclaimed during the liveness blind spot. Fixed const 600s / 10 min
/// (#env-cleanup: was env-overridable via
/// `AGEND_WORKTREE_FORCE_RECLAIM_BOOT_GRACE_SECS`; demoted to YAGNI).
const FORCE_RECLAIM_BOOT_GRACE_SECS: u64 = 600;

fn force_reclaim_boot_grace_secs() -> u64 {
    FORCE_RECLAIM_BOOT_GRACE_SECS
}

/// Pure boot-grace predicate: is `now_unix` within `grace_secs` of `boot_unix`?
/// Unknown boot time → conservative `true` (suspend reclaim — never reclaim when
/// we cannot tell how long the daemon has been up).
pub(crate) fn within_boot_grace(boot_unix: Option<u64>, now_unix: u64, grace_secs: u64) -> bool {
    match boot_unix {
        Some(b) => now_unix.saturating_sub(b) < grace_secs,
        None => true,
    }
}

/// reviewer-2 #5: is the running daemon still inside its post-boot grace window?
/// No active daemon run dir → NOT in grace (tests / non-daemon contexts — GC only
/// runs inside the daemon). Daemon present but boot time unreadable → conservative
/// in-grace (suspend).
fn daemon_within_boot_grace(home: &Path) -> bool {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    within_boot_grace(
        crate::daemon::read_daemon_boot_unix(&run_dir),
        now,
        force_reclaim_boot_grace_secs(),
    )
}

/// PR-2: liveness recency window (mirrors the binding-reconcile heartbeat window,
/// binding.rs:380). A heartbeat / PTY input within this counts as alive.
const LIVENESS_WINDOW_MS: u64 = 3_600_000; // 1h

/// PR-2: per-agent jitter ceiling (hours) added to the age cap, so a fleet whose
/// leases all crossed the cap together (e.g. after a long daemon outage) is
/// reclaimed spread across ticks rather than in a single thundering-herd archive.
const FORCE_RECLAIM_JITTER_HOURS: i64 = 6;

/// t-worktree-leak PR-2: how a candidate was selected — drives the retention
/// sweep's action (clean releases just archive; force-reclaims also emit a LOUD
/// confidence-classified ALERT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    /// Released past the grace TTL — the normal, expected path.
    CleanRelease,
    /// Never released, agent abandoned (no liveness) AND past the age cap — the
    /// force-reclaim backstop tail (no-event abandonment / dead agent).
    ForceReclaim,
}

/// A worktree identified as a GC candidate.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub agent: String,
    pub reason: String,
    /// t-worktree-leak PR-2: selection kind (clean-release vs force-reclaim).
    pub kind: GcKind,
}

/// Scan for GC candidates: daemon-tagged, past grace TTL, not pinned, no active binding.
/// Max directory depth the marker-walk descends under the worktree root. Covers
/// flat (`<agent>-<enc>/` = depth 1), nested (`<agent>/<branch>/` = depth 2), and
/// slash-branch (`<agent>/fix/xxx/` = depth 3) layouts with headroom; bounded so a
/// pathological tree can't make the walk unbounded.
pub(crate) const MARKER_WALK_MAX_DEPTH: usize = 5;

/// t-worktree-leak (reviewer-2 #4): recursively collect daemon-managed worktree
/// dirs (those holding a `.agend-managed` marker) under `root`, to any depth up to
/// `max_depth`. Once a dir carries the marker it IS a worktree → collected and NOT
/// descended into (so we never walk a worktree's own working tree). This replaces
/// the old fixed-depth scan that missed slash-branch worktrees.
///
/// Shared by `gc_candidates` and (#restart-freeze) `binding::reconcile_hooks` —
/// both need every real worktree leaf regardless of slash-branch nesting depth.
pub(crate) fn collect_managed_worktrees(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(MANAGED_MARKER).exists() {
            out.push(p); // a worktree — collect, don't descend into its working tree
        } else {
            collect_managed_worktrees(&p, max_depth - 1, out);
        }
    }
}

/// #2234 Phase 2: derive the owning agent from a worktree path, layout-aware —
/// the FIRST path component under whichever managed root contains it. Used as
/// the fallback when the `.agend-managed` marker lacks an authoritative `agent=`.
///
/// - `<home>/worktrees/<agent>/<branch...>` → `<agent>` (slash branches nest
///   deeper; the first component is the agent — #worktree-git-6).
/// - `<home>/workspace/<agent>` (cure-(B): the worktree IS the workspace dir) →
///   `<agent>` (the dir name). The OLD fallback used the immediate PARENT dir
///   name here → `"workspace"` (the root, not the agent) → liveness keyed on a
///   non-agent → a live agent's `/workspace` cwd could be GC-reclaimed (#2234
///   no-wrong-delete break). Strip-prefix per managed root fixes it.
///
/// `None` when the path is under neither managed root — the caller treats that
/// as unresolvable and SKIPS the worktree (fail-toward-alive), never guessing
/// from the parent dir.
pub(crate) fn agent_from_layout(home: &Path, wt_path: &Path) -> Option<String> {
    for root in [
        daemon_managed_worktree_root(home),
        crate::paths::workspace_dir(home),
    ] {
        if let Ok(rel) = wt_path.strip_prefix(&root) {
            if let Some(s) = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .filter(|s| !s.is_empty())
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// #2234 Phase 2: one daemon-managed worktree, layout-agnostic. The single
/// enumeration shape consumed by Phase 1c `release_stale_branch_holders` and
/// (future) the GC scan — replacing the dual fs-root scans that assume the
/// `worktrees/<agent>/<branch>` layout and miss `/workspace/<agent>` worktrees
/// once cure-(B) moves them there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    /// Canonicalized worktree directory.
    pub path: PathBuf,
    /// Owning agent (marker `agent=` authoritative, else layout-derived).
    pub agent: Option<String>,
    /// HEAD branch from the registry (`None` = detached or fs-only/orphan).
    pub branch: Option<String>,
    /// `true` if it appears in `git worktree list` (canonical knows it).
    pub registered: bool,
}

/// #2234 Phase 2: cure-(B) workspace worktrees — `workspace/<agent>` dirs whose
/// `.git` is a gitlink FILE (a real worktree, Phase 0's discriminator). Empty
/// when (B) is OFF (no workspace dir is a worktree), so every consumer stays
/// byte-identical until (B) ships. Marker-LESS interrupted-reconcile worktrees
/// are still caught here (gitlink-alone). Single impl shared by
/// `fs_managed_worktrees` (→ enumerate / gc) and `worktree::list_residual`.
pub(crate) fn workspace_gitlink_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(crate::paths::workspace_dir(home)) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.join(".git").is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// #2234 Phase 2: the fs-scan portion of [`enumerate_managed_worktrees`] —
/// every daemon-managed worktree dir on disk across BOTH layouts:
/// `worktrees/<agent>/<branch...>` (marker-walk, slash-branch aware) +
/// cure-(B) `workspace/<agent>` (gitlink). The SINGLE marker-walk impl shared by
/// `enumerate` (registry ∪ fs) and `gc_candidates` — no parallel rewrite, no
/// drift. Home-only (no `source_repo`): gc/list are home-wide and need no
/// `git worktree list`. byte-identical when (B) OFF (the workspace part is empty).
pub(crate) fn fs_managed_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_managed_worktrees(
        &daemon_managed_worktree_root(home),
        MARKER_WALK_MAX_DEPTH,
        &mut out,
    );
    out.extend(workspace_gitlink_worktrees(home));
    out
}

/// #2234 Phase 2: enumerate EVERY daemon-managed worktree across BOTH layouts
/// (`worktrees/<agent>/<branch>` and cure-(B) `workspace/<agent>`), unioning the
/// canonical registry (authoritative for any path) with an fs-scan of the known
/// roots (catches orphan dirs whose registration was pruned). Single source of
/// truth replacing the dual fs-root scans. De-duped by canonicalized path.
///
/// no-miss: any real worktree is registered (in `git worktree list`) OR a dir
/// under a known root (in the fs-scan) — both false ⟹ it doesn't exist. The
/// union therefore covers the full set: `git worktree list` alone misses orphan
/// dirs; the fs-scan alone misses pruned-registration / non-standard roots.
pub fn enumerate_managed_worktrees(home: &Path, source_repo: &Path) -> Vec<ManagedWorktree> {
    let worktrees_root = daemon_managed_worktree_root(home);
    let workspace = crate::paths::workspace_dir(home);
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (croot, cws) = (canon(&worktrees_root), canon(&workspace));
    let under_managed = |cp: &Path| cp.starts_with(&croot) || cp.starts_with(&cws);
    // Agent = first path component under whichever CANONICAL managed root
    // contains the (canonical) worktree path. Derived against the canonical roots
    // here — NOT `agent_from_layout` (which strips the un-canonicalized roots for
    // the GC path) — because `git worktree list` returns CANONICAL paths while
    // `home` may be un-canonicalized (macOS `/var`→`/private/var`), so the two
    // must be compared canonical-to-canonical.
    let agent_of = |cp: &Path| -> Option<String> {
        for root in [&croot, &cws] {
            if let Ok(rel) = cp.strip_prefix(root) {
                if let Some(s) = rel
                    .components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some(s.to_string());
                }
            }
        }
        None
    };

    // Keyed by canonicalized path → natural dedup; registry entries win (they
    // carry the branch). BTreeMap for deterministic ordering.
    let mut by_path: std::collections::BTreeMap<PathBuf, ManagedWorktree> =
        std::collections::BTreeMap::new();

    // Registry pass — authoritative, any path. Filter to the managed roots
    // (excludes the canonical main worktree + foreign worktrees).
    if let Ok(entries) = crate::git_worktree::list_porcelain(source_repo) {
        for (path, branch) in entries {
            let cp = canon(&path);
            if under_managed(&cp) {
                let agent = agent_of(&cp);
                by_path.insert(
                    cp.clone(),
                    ManagedWorktree {
                        path: cp,
                        agent,
                        branch,
                        registered: true,
                    },
                );
            }
        }
    }

    // fs pass — catch orphan dirs the registry doesn't know. Shared
    // `fs_managed_worktrees` is the SINGLE marker-walk + workspace-gitlink impl
    // (also used by gc_candidates / list_residual — no parallel rewrite).
    for p in fs_managed_worktrees(home) {
        let cp = canon(&p);
        by_path
            .entry(cp.clone())
            .or_insert_with(|| ManagedWorktree {
                agent: agent_of(&cp),
                branch: None,
                registered: false,
                path: cp,
            });
    }

    by_path.into_values().collect()
}

pub fn gc_candidates(home: &Path) -> Vec<GcCandidate> {
    let mut candidates = Vec::new();
    // t-worktree-leak PR-2: snapshot the live-agent set ONCE per pass (the
    // force-reclaim liveness check consults it per candidate; this is the
    // process-alive signal that protects idle-but-running agents).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();

    // New layout `worktrees/<agent>/<branch>/` (marker-walk, slash-branch aware)
    // + cure-(B) `workspace/<agent>` gitlink worktrees, via the shared
    // `fs_managed_worktrees` (#2234 Phase 2 — single marker-walk impl). The
    // workspace part is empty when (B) is OFF → byte-identical candidate set; the
    // `evaluate_candidate` marker-gate filters anything non-managed regardless.
    for wt_path in fs_managed_worktrees(home) {
        if let Some(candidate) = evaluate_candidate(home, &wt_path, &live_agents) {
            candidates.push(candidate);
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let workspace = crate::paths::workspace_dir(home);
    if workspace.exists() {
        if let Ok(entries) = std::fs::read_dir(&workspace) {
            for entry in entries.flatten() {
                let wt_base = entry.path().join(".worktrees");
                if !wt_base.is_dir() {
                    continue;
                }
                if let Ok(wts) = std::fs::read_dir(&wt_base) {
                    for wt in wts.flatten() {
                        let wt_path = wt.path();
                        if !wt_path.is_dir() {
                            continue;
                        }
                        if let Some(candidate) = evaluate_candidate(home, &wt_path, &live_agents) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
    }

    candidates
}

/// t-worktree-leak PR-2 safety #1: does the agent show ANY sign of life? This is
/// MULTI-signal — never just heartbeat — so an idle-but-running agent (no recent
/// heartbeat) is still protected. A positive on ANY signal → the worktree is
/// NEVER force-reclaimed, regardless of age (liveness-AND-age). Reads that fail
/// lean toward "alive" (conservative — never mis-reclaim).
fn agent_has_liveness(
    home: &Path,
    agent: &str,
    live_agents: &std::collections::HashSet<String>,
) -> bool {
    // (process) In the live-agent registry — covers idle-but-running agents that
    // are not currently heartbeating.
    if live_agents.contains(agent) {
        return true;
    }
    let hb = crate::daemon::heartbeat_pair::snapshot_for(agent);
    let now = crate::daemon::heartbeat_pair::now_ms();
    // (heartbeat) any MCP tool call within the recency window.
    if hb.heartbeat_at_ms != 0 && now.saturating_sub(hb.heartbeat_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (PTY) recent terminal input.
    if hb.last_input_at_ms != 0 && now.saturating_sub(hb.last_input_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (waiting_on) actively declared a blocker → alive.
    if hb.waiting_on_since_ms.is_some() {
        return true;
    }
    // (ci-watch) subscribed to a live ci-watch → active CI-tracked work.
    if agent_is_ci_watch_subscriber(home, agent) {
        return true;
    }
    false
}

/// t-worktree-leak PR-2: fresh multi-signal liveness check for `agent` (snapshots
/// the live-agent set itself). Used by the retention sweep's pre-archive fencing
/// re-validation so an agent that came back to life between enumeration and
/// archive is spared.
pub(crate) fn is_agent_alive(home: &Path, agent: &str) -> bool {
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    agent_has_liveness(home, agent, &live_agents)
}

/// PR-2: is `agent` a subscriber on any live ci-watch? codex gap ②: this is a
/// liveness source, so every read failure FAILS TOWARD ALIVE (returns `true`,
/// blocking reclaim) rather than silently treating the agent as not-subscribed —
/// a mis-read must never let us reclaim a live agent. The ONE exception is the
/// watch dir being genuinely absent (NotFound), which is a real "no watches"
/// state, not a read failure.
fn agent_is_ci_watch_subscriber(home: &Path, agent: &str) -> bool {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true, // can't enumerate watches → fail-toward-alive
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // A watch file we cannot read/parse COULD carry this agent's subscription
        // → fail-toward-alive rather than skip it.
        let Ok(content) = std::fs::read_to_string(&path) else {
            return true;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            return true;
        };
        if let Some(subs) = v.get("subscribers").and_then(|s| s.as_array()) {
            if subs
                .iter()
                .any(|s| s.get("instance").and_then(|i| i.as_str()) == Some(agent))
            {
                return true;
            }
        }
    }
    false
}

/// PR-2: is a never-released lease past the (per-agent jittered) force-reclaim age
/// cap? The deterministic jitter spreads a fleet whose leases all crossed the cap
/// together across ticks (anti-thundering-herd, safety #3). No `leased_at` → not
/// reclaimable (conservative).
fn leased_at_force_reclaimable(leased_at: Option<&str>, agent: &str) -> bool {
    let Some(ts) = leased_at else {
        return false;
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
    let jitter_h = (fnv1a(agent) % (FORCE_RECLAIM_JITTER_HOURS.max(1) as u64)) as i64;
    let cap = chrono::Duration::days(force_reclaim_age_days()) + chrono::Duration::hours(jitter_h);
    age > cap
}

/// Stable per-agent FNV-1a hash → deterministic jitter (no randomness, so reclaim
/// timing is reproducible).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub(crate) fn evaluate_candidate(
    home: &Path,
    wt_path: &Path,
    live_agents: &std::collections::HashSet<String>,
) -> Option<GcCandidate> {
    // Must be daemon-managed (R14).
    if !is_daemon_managed(wt_path) {
        return None;
    }
    // Must not be pinned.
    if is_pinned(wt_path) {
        return None;
    }
    // Resolve agent name: read from .agend-managed marker (authoritative),
    // else derive layout-aware from the path (#2234 Phase 2).
    let marker = wt_path.join(MANAGED_MARKER);
    let marker_content = std::fs::read_to_string(&marker).unwrap_or_default();
    let agent_name = marker_content
        .lines()
        .find(|l| l.starts_with("agent="))
        .and_then(|l| l.strip_prefix("agent="))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| agent_from_layout(home, wt_path))
        .unwrap_or_default();
    // #2234: unresolvable agent → NOT a GC candidate (fail-toward-alive). Never
    // reclaim a worktree whose owner we can't name.
    if agent_name.is_empty() {
        return None;
    }
    let binding_present = crate::binding::read(home, &agent_name).is_some();
    let released_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("released_at="));

    match released_at {
        // ── Clean-release path: explicitly released, past the grace TTL. ──
        Some(ts) => {
            // A released lease should already be unbound; if it is still bound,
            // that is a contradiction — leave it alone (conservative).
            if binding_present {
                return None;
            }
            match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => {
                    let age =
                        chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
                    if age < chrono::Duration::hours(GC_GRACE_HOURS) {
                        return None; // still within grace
                    }
                }
                // #1870 (H1): a malformed `released_at=` (e.g. a partial-write /
                // crash-truncated marker) MUST NOT be treated as "past grace" — the
                // grace window protects a just-released worktree's WIP. But it is
                // also `Some(garbage)`, so it never reaches the never-released
                // force-reclaim arm below → pre-#1882 it leaked FOREVER (both GC
                // paths skipped it). #1882 (WT-LEAK-1): treat "corrupt released_at ≈
                // never-released" — hand off to the SAME force-reclaim backstop. Its
                // liveness + leased_at age-cap guards (NOT the unparseable grace
                // window) protect a still-used / recently-leased worktree; only an
                // abandoned (no liveness, leased past the cap) corrupt-marker
                // worktree is reclaimed. This does NOT reintroduce the H1
                // WIP-destruction (that was the grace-window bypass).
                Err(_) => {
                    return force_reclaim_candidate(
                        home,
                        wt_path,
                        agent_name,
                        &marker_content,
                        live_agents,
                        "malformed released_at marker",
                    );
                }
            }
            Some(GcCandidate {
                path: wt_path.to_path_buf(),
                agent: agent_name,
                reason: format!("daemon-tagged, released >{}h, not pinned", GC_GRACE_HOURS),
                kind: GcKind::CleanRelease,
            })
        }
        // ── t-worktree-leak PR-2 force-reclaim backstop: NEVER released. ──
        // This is ONLY the no-event-abandonment / dead-agent tail (the
        // invariant + sweeper in PR-1 handle every worktree that DID see a
        // merge/close/task-done event; the 7-day expired-intent path hands off
        // here). liveness-AND-age: ANY live signal → never reclaim (even past the
        // cap); otherwise require the per-agent-jittered age cap.
        None => force_reclaim_candidate(
            home,
            wt_path,
            agent_name,
            &marker_content,
            live_agents,
            "never-released lease",
        ),
    }
}

/// t-worktree-leak PR-2 force-reclaim backstop: reclaim a worktree ONLY when it
/// is genuinely abandoned — not in the daemon's post-boot grace window (#5), NO
/// liveness signal for its agent, AND its `leased_at` is past the per-agent
/// force-reclaim age cap. ANY live signal → never reclaim (even past the cap).
/// Shared by the never-released (`released_at` absent) arm AND the #1882 WT-LEAK-1
/// corrupt-`released_at` fall-through. `marker_state` names why we're here, for
/// the candidate's reason. The liveness + age-cap guards (NOT the grace window)
/// are what protect a just-leased / just-released worktree from premature reclaim.
fn force_reclaim_candidate(
    home: &Path,
    wt_path: &Path,
    agent_name: String,
    marker_content: &str,
    live_agents: &std::collections::HashSet<String>,
    marker_state: &str,
) -> Option<GcCandidate> {
    // reviewer-2 #5: suspend force-reclaim during the daemon's post-boot grace
    // window (the process-liveness signal is still re-establishing).
    if daemon_within_boot_grace(home) {
        return None;
    }
    if agent_has_liveness(home, &agent_name, live_agents) {
        return None;
    }
    let leased_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("leased_at="));
    if !leased_at_force_reclaimable(leased_at, &agent_name) {
        return None;
    }
    Some(GcCandidate {
        path: wt_path.to_path_buf(),
        agent: agent_name,
        reason: format!(
            "force-reclaim: {marker_state}, no liveness signal, leased >{}d (abandoned)",
            force_reclaim_age_days()
        ),
        kind: GcKind::ForceReclaim,
    })
}

/// Dry-run: log candidates without deleting. Returns candidate list.
pub fn gc_dry_run(home: &Path) -> Vec<GcCandidate> {
    let candidates = gc_candidates(home);
    for c in &candidates {
        tracing::info!(
            agent = %c.agent,
            path = %c.path.display(),
            reason = %c.reason,
            "gc_dry_run candidate"
        );
    }
    if !candidates.is_empty() {
        crate::event_log::log(
            home,
            "gc_dry_run",
            "",
            &format!("{} candidates identified", candidates.len()),
        );
    }
    candidates
}

// ─────────────────────────────────────────────────────────────────────────
// t-…50793-9: managed-worktree `target/` retention sweep.
//
// Build `target/` dirs are the dominant fleet disk consumer (incident
// 2026-06-21: r4 ~90GB + dev-2 ~64GB stale worktree targets → /Users ENOSPC →
// daemon inbox went readonly). The whole-worktree GC above frees `target/` only
// as a SIDE-EFFECT of deleting the entire worktree, gated on explicit-release +
// 24h grace OR 7-day abandonment — so an alive-agent-never-released worktree
// (e.g. a reviewer that finished a branch but never released) leaks `target/`
// indefinitely. This sweep reclaims a managed worktree's `target/` once it goes
// STALE (no build activity within the age threshold) WITHOUT deleting the
// worktree/checkout itself. `target/` is regenerable (already excluded from
// worktree backups), so a swept worktree pays only a one-time rebuild on reuse.
//
// SAFETY (footgun — must NEVER delete canonical/operator data, never clobber an
// active build). Layered:
//   1. marker-STRICT enumeration — only worktrees under `home/worktrees` that
//      carry `.agend-managed`, via `target_sweep_worktrees` (NOT the looser
//      `fs_managed_worktrees`, which unions markerless workspace gitlinks incl.
//      operator-owned ones). The operator's canonical repo has no marker + lives
//      OUTSIDE the managed root → unreachable by the enumerator.
//   2. symlinked-root refusal + canonical-home confinement (`safe_managed_root`)
//      — never enumerate or delete through a symlinked / home-escaping root.
//   3. symlink refusal — a worktree's `target` must be a REAL directory; a
//      symlinked `target` (could point at the canonical 49GB target) is refused.
//   4. active-build exclusion (`predicate_protects`, round-4) — a build can only
//      happen in a worktree whose owner is in the daemon ROSTER and CURRENTLY
//      bound HERE (stable signals; liveness is FLAPPY and was dropped). Such
//      worktrees are excluded. The delete pass HOLDS the owner's
//      `.binding.json.lock` (the SAME lock `bind_full` takes) through
//      predicate→recheck→delete, so the binding can't change under us — closing
//      the bound-but-not-yet-live and rebind-during-window races (the lock
//      guards BIND, not cargo). Only instance-gone / bound-elsewhere / unbound
//      stale targets are swept.
//   5. fail-CLOSED mtime gate — swept only when nothing under `target/` changed
//      within `max_age`, RE-checked immediately before deletion (load-bearing
//      last line: any stat/read error ⇒ treated as active ⇒ skip). ONLY
//      `target/` is removed — never the worktree dir or source.

/// Result of a single GC removal attempt.
#[derive(Debug, Clone)]
pub struct GcResult {
    pub path: PathBuf,
    pub agent: String,
    pub removed: bool,
    pub error: Option<String>,
}

/// Execute GC: remove all candidates identified by [`gc_candidates`].
/// Each candidate is removed via `git worktree remove --force` with
/// `remove_dir_all` fallback (mirrors [`release_full`] deletion pattern).
pub fn gc_run(home: &Path) -> Vec<GcResult> {
    let candidates = gc_candidates(home);
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for c in &candidates {
        let result = gc_remove_one(home, c);
        results.push(result);
    }
    let removed_count = results.iter().filter(|r| r.removed).count();
    let removed_paths: Vec<String> = results
        .iter()
        .filter(|r| r.removed)
        .map(|r| r.path.display().to_string())
        .collect();
    if removed_count > 0 {
        crate::event_log::log(
            home,
            "gc_run",
            "",
            &format!(
                "{removed_count} worktrees removed: [{}]",
                removed_paths.join(", ")
            ),
        );
    }
    results
}

pub(crate) fn gc_remove_one(home: &Path, candidate: &GcCandidate) -> GcResult {
    let wt_path = &candidate.path;

    // t-worktree-leak PR-2 (codex gap ① CRITICAL): a force-reclaim candidate MUST
    // NEVER be hard-deleted. Route it through the SINGLE safe deletion path
    // (retention's `maybe_remove_candidate`: pre-archive liveness re-check +
    // atomic archive-to-trash + unbind + LOUD confidence ALERT), so this path
    // and the clean-release archive-fallthrough below cannot diverge into an
    // irrecoverable delete. Clean-release candidates keep the historical
    // hard-delete below (ungated, unconditional — decision Q3).
    if candidate.kind == GcKind::ForceReclaim {
        use crate::daemon::retention::worktrees::{maybe_remove_candidate, RemovalOutcome};
        let outcome = maybe_remove_candidate(home, candidate);
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: matches!(outcome, RemovalOutcome::Removed),
            error: match outcome {
                RemovalOutcome::Skipped { reason } => Some(reason),
                RemovalOutcome::Removed => None,
            },
        };
    }

    // Acquire the same binding lock that bind_full() uses, making
    // GC deletion and bind mutually exclusive (eliminates TOCTOU).
    let lock_path = crate::paths::runtime_dir(home)
        .join(&candidate.agent)
        .join(".binding.json.lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            // #2550 W5 (decision d-20260703062722787157-1, Q1): the ORIGINAL
            // two-tick fallback (this skip, caught later by an independent
            // retention sweep) is decided intentional defense-in-depth — kept,
            // but as an IMMEDIATE same-pass fallthrough instead of waiting for
            // a separately-cadenced re-scan. This candidate is still eligible
            // (we haven't re-validated yet, but the ONLY reason we're here is
            // lock contention, not invalidity) — hand it to the archive path.
            return archive_fallthrough_result(
                home,
                candidate,
                format!("skipped: binding lock acquisition failed: {e}"),
            );
        }
    };

    // Re-validate under lock: binding/pinned/grace state may have
    // changed since gc_candidates() enumerated this worktree. t-worktree-leak
    // PR-2: re-snapshot liveness here too, so a force-reclaim candidate whose
    // agent came back to life between enumeration and removal is spared (fencing).
    //
    // NOTE: unlike the lock-failure and later branches, a re-validation
    // failure means the candidate is NO LONGER ELIGIBLE (rebound / re-pinned /
    // grace state changed) — a fresh gc_candidates() scan wouldn't find it
    // either, so this does NOT fall through to the archive path.
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    if evaluate_candidate(home, wt_path, &live_agents).is_none() {
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: false,
            error: Some("skipped: pre-deletion re-validation failed".to_string()),
        };
    }

    // #worktree-git-4: the owning repo's cwd is MANDATORY for `git worktree
    // remove`. Empirically, running it with the daemon's inherited cwd (an
    // unrelated repo) fails with "is not a working tree", leaving the dir on
    // disk; the remove_dir_all fallback then physically deletes the dir but
    // CANNOT prune the owning repo's registry (the prune is keyed on
    // source_repo) → a prunable-registry leak that blocks re-lease. If the
    // owning repo can't be resolved, fall through to the archive path (still
    // eligible, just couldn't act) rather than run git cwd-less.
    let Some(source_repo) = resolve_source_repo(wt_path) else {
        return archive_fallthrough_result(
            home,
            candidate,
            "skipped: owning source repo unresolved — refusing to run \
             `git worktree remove` without the owning-repo cwd"
                .to_string(),
        );
    };

    // git-raw-allowed: kept raw (not git_cmd) per the decided #2128 migration
    // scope; cwd is now always the resolved owning repo.
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "worktree",
        "remove",
        "--force",
        &wt_path.display().to_string(),
    ])
    .env("AGEND_GIT_BYPASS", "1")
    .current_dir(&source_repo);
    match cmd.output() {
        Ok(o) if o.status.success() => GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: true,
            error: None,
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %stderr,
                "gc: git worktree remove failed — falling back to remove_dir_all"
            );
            let _ = std::fs::remove_dir_all(wt_path);
            if !wt_path.exists() {
                // W1.2: best-effort prune (result already ignored).
                let _ = crate::git_helpers::git_ok(&source_repo, &["worktree", "prune"]);
                GcResult {
                    path: wt_path.clone(),
                    agent: candidate.agent.clone(),
                    removed: true,
                    error: None,
                }
            } else {
                // Both `git worktree remove` and `remove_dir_all` failed — the
                // worktree is still on disk and still eligible. Fall through.
                archive_fallthrough_result(
                    home,
                    candidate,
                    format!("git worktree remove failed: {stderr}"),
                )
            }
        }
        Err(e) => {
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %e,
                "gc: git command failed"
            );
            archive_fallthrough_result(home, candidate, format!("git command failed: {e}"))
        }
    }
}

/// #2550 W5: hand a still-eligible CleanRelease candidate the hard-delete
/// attempt could not act on to the (gated) archive path, translating the
/// `RemovalOutcome` into this function's `GcResult` shape.
fn archive_fallthrough_result(
    home: &Path,
    candidate: &GcCandidate,
    hard_delete_skip_reason: String,
) -> GcResult {
    use crate::daemon::retention::worktrees::{archive_fallthrough, RemovalOutcome};
    let outcome = archive_fallthrough(home, candidate, hard_delete_skip_reason);
    GcResult {
        path: candidate.path.clone(),
        agent: candidate.agent.clone(),
        removed: matches!(outcome, RemovalOutcome::Removed),
        error: match outcome {
            RemovalOutcome::Skipped { reason } => Some(reason),
            RemovalOutcome::Removed => None,
        },
    }
}

/// Resolve the source (owning) repo from a worktree's `.git` file.
/// A git worktree's `.git` is a file containing `gitdir: <path>` pointing
/// to `<source>/.git/worktrees/<name>`. We walk up from that to find the
/// source repo root.
pub(crate) fn resolve_source_repo(wt_path: &Path) -> Option<PathBuf> {
    let git_file = wt_path.join(".git");
    let content = std::fs::read_to_string(&git_file).ok()?;
    let gitdir_line = content.lines().find(|l| l.starts_with("gitdir:"))?;
    let gitdir = gitdir_line.strip_prefix("gitdir:")?.trim();
    let gitdir_path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        wt_path.join(gitdir).canonicalize().ok()?
    };
    // gitdir_path is <source>/.git/worktrees/<name>
    // Walk up: worktrees → .git → source_repo
    gitdir_path.parent()?.parent()?.parent().map(PathBuf::from)
}

/// Cleanup stale ci-watch lock files whose PRs merged >7 days ago.
pub fn gc_stale_ci_watch_locks(home: &Path) -> usize {
    let ci_dir = home.join("ci-watches");
    if !ci_dir.is_dir() {
        return 0;
    }
    let mut removed = 0;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            // Check file modification time as a proxy for PR merge time.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let modified_dt: chrono::DateTime<chrono::Utc> = modified.into();
            if modified_dt < cutoff && std::fs::remove_file(&path).is_ok() {
                tracing::info!(path = %path.display(), "gc: removed stale ci-watch lock");
                removed += 1;
            }
        }
    }
    if removed > 0 {
        crate::event_log::log(
            home,
            "gc_stale_ci_watch_locks",
            "",
            &format!("{removed} stale lock files removed"),
        );
    }
    removed
}
