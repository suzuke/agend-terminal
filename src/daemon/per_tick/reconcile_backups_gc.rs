//! #2234 (B) prereq: retention GC for `<home>/reconcile-backups/`.
//!
//! When the (B) gray-rollout flips default ON, each agent's first
//! standalone→worktree reconcile takes a WHOLE-dir snapshot to
//! `<home>/reconcile-backups/<agent>[-<branch>]-<epoch>/` (`worktree_pool`'s
//! `backup_worktree_dir`) — deliberately never auto-deleted at creation time
//! (毁-work safety net). At batch flip-default scale that grows without bound,
//! the gap the (B) gray-rollout design (§4a) flagged as a hard prereq.
//!
//! `reconcile-backups` has ZERO production readers (restore is operator-manual),
//! so a pure age GC is safe. Belt + safety net:
//! - **mtime-age ≥ [`MIN_AGE`] (14 days)** → `remove_dir_all`-eligible. Generous,
//!   so a recently-reconciled agent's lifeline is never reaped; `mtime` (not
//!   creation) so a just-written backup is naturally fresh.
//! - **per-agent newest-1 floor**: every agent ALWAYS keeps its single most-recent
//!   backup (even if older than 14d) — the last-resort destroy-work lifeline.
//!   Keyed per-AGENT (not per-branch), so the floor stays bounded to the fleet
//!   size instead of accumulating one survivor per branch forever (which would
//!   re-introduce the unbounded growth this GC exists to stop).
//! - noise-discipline: ONE aggregate log line, only when something was removed.
//!
//! Scope: touches ONLY `<home>/reconcile-backups/`. Zero git ops; never touches
//! `worktrees/`, `workspace/`, or the canonical repo. (B) OFF → no backups are
//! ever created → this handler is a natural no-op.

use super::{PerTickHandler, TickContext};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Minimum `mtime`-age before a reconcile-backup is GC-eligible. Generous (14
/// days) because a backup is the LAST line of destroy-work defence — a 2-week
/// window comfortably outlives "did the agent's new worktree actually work?",
/// and the per-agent floor below guarantees a lifeline survives regardless.
const MIN_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

pub(crate) struct ReconcileBackupsGcHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl ReconcileBackupsGcHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for ReconcileBackupsGcHandler {
    fn name(&self) -> &'static str {
        "reconcile_backups_gc"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // The fleet roster disambiguates the `<agent>[-<branch>]-<epoch>` dir name
        // (hyphenated agent names make a plain split ambiguous). Falls back to a
        // glob of the runtime dir when the daemon registry is unreachable.
        let agents = crate::runtime::list_agents_with_fallback(ctx.home);
        let removed = gc_reconcile_backups(ctx.home, &agents, MIN_AGE, SystemTime::now());
        if removed > 0 {
            tracing::info!(target: "reconcile_backups_gc", removed,
                "#2234: GC'd aged reconcile-backups (per-agent newest-1 kept)");
        }
    }
}

/// One scanned backup directory.
struct Backup {
    path: PathBuf,
    /// The owning agent (or, for an unknown/retired agent, the name with its
    /// `-<epoch>` suffix stripped — so its backups still group + keep a floor).
    key: String,
    mtime: SystemTime,
}

/// Sweep `<home>/reconcile-backups/` and remove aged backup dirs, preserving each
/// agent's single most-recent backup (the destroy-work lifeline). Returns the
/// count removed. `agents` (the known fleet roster) disambiguates the
/// `<agent>[-<branch>]-<epoch>` dir name; `now` is injected so tests drive the
/// age belt deterministically. A missing `reconcile-backups/` → 0 (natural no-op
/// when (B) never created any backup).
fn gc_reconcile_backups(
    home: &Path,
    agents: &[String],
    min_age: Duration,
    now: SystemTime,
) -> usize {
    let root = home.join("reconcile-backups");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };

    let mut backups: Vec<Backup> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Directories only — a stray file under reconcile-backups/ is left alone.
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let key = agent_key(name, agents);
        backups.push(Backup { path, key, mtime });
    }

    // per-agent newest-1 floor: the single most-recent (max mtime) backup per key
    // is protected. Tie-break by path so the survivor is deterministic when two
    // dirs share an mtime.
    let mut newest: HashMap<&str, &Backup> = HashMap::new();
    for b in &backups {
        newest
            .entry(b.key.as_str())
            .and_modify(|cur| {
                if (b.mtime, b.path.as_path()) > (cur.mtime, cur.path.as_path()) {
                    *cur = b;
                }
            })
            .or_insert(b);
    }

    let mut removed = 0usize;
    for b in &backups {
        // Floor: never remove an agent's newest backup, even when aged.
        if newest.get(b.key.as_str()).is_some_and(|n| n.path == b.path) {
            continue;
        }
        let age = now.duration_since(b.mtime).unwrap_or_default();
        if age < min_age {
            continue;
        }
        if std::fs::remove_dir_all(&b.path).is_ok() {
            removed += 1;
            tracing::info!(target: "reconcile_backups_gc", path = %b.path.display(),
                age_days = age.as_secs() / 86_400,
                "#2234: removed aged reconcile-backup (not this agent's newest)");
        } else {
            tracing::warn!(target: "reconcile_backups_gc", path = %b.path.display(),
                "#2234: failed to remove aged reconcile-backup");
        }
    }
    removed
}

/// Resolve the agent that owns a `<agent>[-<branch>]-<epoch>` backup dir name.
/// Picks the LONGEST known agent the name equals or is prefixed by `"<agent>-"`
/// — longest-match disambiguates nested rosters (`fixup-dev` vs `fixup-dev-3`).
/// Falls back (unknown/retired agent) to the name with its trailing `-<epoch>`
/// stripped, so an orphan agent's backups still group together under one key and
/// keep their own newest-1 floor.
fn agent_key(name: &str, agents: &[String]) -> String {
    let matched = agents
        .iter()
        .filter(|a| name == a.as_str() || name.starts_with(&format!("{a}-")))
        .max_by_key(|a| a.len());
    match matched {
        Some(a) => a.clone(),
        None => strip_trailing_epoch(name).to_string(),
    }
}

/// Strip a trailing `-<digits>` epoch segment (the backup-name suffix). Returns
/// the input unchanged when there is no numeric trailing segment.
fn strip_trailing_epoch(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => name,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn backups_root(tag: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-2234-rbgc-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("reconcile-backups")).unwrap();
        home
    }

    /// Create `reconcile-backups/<name>/` (with a payload file so it is non-empty).
    fn mk_backup(home: &Path, name: &str) -> PathBuf {
        let dir = home.join("reconcile-backups").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("WIP.txt"), b"unsaved work").unwrap();
        dir
    }

    /// Force a backup dir's mtime. Unix-only: forcing a directory mtime needs
    /// `File::open` + `set_modified`, and `File::open` on a Windows directory
    /// requires `FILE_FLAG_BACKUP_SEMANTICS`, which `std::fs` doesn't expose.
    /// The GC logic is `SystemTime` arithmetic — platform-agnostic — so Unix
    /// coverage suffices (mirrors `daemon::retention::worktrees` tests).
    #[cfg(unix)]
    fn set_age(dir: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        std::fs::File::open(dir)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    fn days(n: u64) -> Duration {
        Duration::from_secs(n * 86_400)
    }

    #[test]
    fn agent_key_longest_prefix_disambiguates_nested_rosters() {
        let agents = vec!["fixup-dev".to_string(), "fixup-dev-3".to_string()];
        // Nested: must resolve to the LONGEST matching agent.
        assert_eq!(agent_key("fixup-dev-3-1718000000", &agents), "fixup-dev-3");
        assert_eq!(agent_key("fixup-dev-1718000000", &agents), "fixup-dev");
        // Branch-discriminated name still keys to the agent (per-AGENT floor).
        assert_eq!(
            agent_key("fixup-dev-3-feat-x-1718000000", &agents),
            "fixup-dev-3"
        );
        // Unknown agent → epoch-stripped fallback key.
        assert_eq!(agent_key("ghost-9-1718000000", &agents), "ghost-9");
        // No numeric suffix → unchanged.
        assert_eq!(agent_key("weird-name", &agents), "weird-name");
    }

    #[cfg(unix)]
    #[test]
    fn gc_removes_aged_nonnewest_keeps_newest_per_agent() {
        let home = backups_root("aged-floor");
        let agents = vec!["dev".to_string()];
        let old = mk_backup(&home, "dev-100");
        let mid = mk_backup(&home, "dev-200");
        let new = mk_backup(&home, "dev-300");
        // All three are well past the 14d window; newest by mtime = `new`.
        set_age(&old, days(40));
        set_age(&mid, days(30));
        set_age(&new, days(20));

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 2, "two aged non-newest backups removed");
        assert!(!old.exists() && !mid.exists(), "aged non-newest removed");
        assert!(
            new.exists(),
            "agent's newest backup kept by the floor even though aged"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn gc_keeps_recent_backups_even_when_nonnewest() {
        let home = backups_root("recent-keep");
        let agents = vec!["dev".to_string()];
        let a = mk_backup(&home, "dev-100");
        let b = mk_backup(&home, "dev-200");
        // Both younger than 14d → age belt spares them regardless of newest.
        set_age(&a, days(3));
        set_age(&b, days(1));

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0, "recent backups are not eligible");
        assert!(a.exists() && b.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn gc_floor_keeps_sole_aged_backup_for_agent() {
        let home = backups_root("sole-floor");
        let agents = vec!["dev".to_string()];
        let only = mk_backup(&home, "dev-100");
        set_age(&only, days(90)); // ancient but the agent's ONLY lifeline

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0, "an agent's sole backup is never reaped");
        assert!(only.exists(), "destroy-work lifeline preserved");
        std::fs::remove_dir_all(&home).ok();
    }

    /// per-AGENT (not per-branch): an agent that reconciled several BRANCHES keeps
    /// exactly ONE survivor total, not one per branch — the bound that stops the
    /// unbounded growth this GC exists for.
    #[cfg(unix)]
    #[test]
    fn gc_floor_is_per_agent_not_per_branch() {
        let home = backups_root("per-agent");
        let agents = vec!["dev".to_string()];
        let b1 = mk_backup(&home, "dev-feat-a-100");
        let b2 = mk_backup(&home, "dev-feat-b-200");
        let b3 = mk_backup(&home, "dev-300"); // teardown (no-branch) form
        set_age(&b1, days(40));
        set_age(&b2, days(30));
        set_age(&b3, days(20)); // newest by mtime

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(
            removed, 2,
            "only the single newest survives, across branches"
        );
        assert!(!b1.exists() && !b2.exists());
        assert!(b3.exists(), "the one newest backup for the agent is kept");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Independent floors: each agent keeps its own newest.
    #[cfg(unix)]
    #[test]
    fn gc_floor_is_independent_across_agents() {
        let home = backups_root("multi-agent");
        let agents = vec!["dev".to_string(), "rev".to_string()];
        let dev_old = mk_backup(&home, "dev-100");
        let dev_new = mk_backup(&home, "dev-200");
        let rev_old = mk_backup(&home, "rev-100");
        let rev_new = mk_backup(&home, "rev-200");
        for (d, a) in [
            (&dev_old, 40),
            (&dev_new, 25),
            (&rev_old, 50),
            (&rev_new, 30),
        ] {
            set_age(d, days(a));
        }

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 2, "one aged non-newest removed per agent");
        assert!(!dev_old.exists() && !rev_old.exists());
        assert!(
            dev_new.exists() && rev_new.exists(),
            "each agent's newest kept"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_missing_root_is_noop() {
        let home =
            std::env::temp_dir().join(format!("agend-2234-rbgc-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // No reconcile-backups/ at all → (B) OFF natural no-op.
        let removed = gc_reconcile_backups(&home, &["dev".to_string()], MIN_AGE, SystemTime::now());
        assert_eq!(removed, 0);
    }

    #[cfg(unix)]
    #[test]
    fn gc_ignores_stray_files() {
        let home = backups_root("stray-file");
        let agents = vec!["dev".to_string()];
        let aged = mk_backup(&home, "dev-100");
        let newer = mk_backup(&home, "dev-200");
        set_age(&aged, days(40));
        set_age(&newer, days(30));
        // A stray FILE under reconcile-backups/ must be left untouched.
        let stray = home.join("reconcile-backups").join("README.txt");
        std::fs::write(&stray, b"note").unwrap();

        let removed = gc_reconcile_backups(&home, &agents, MIN_AGE, SystemTime::now());
        assert_eq!(removed, 1, "only the aged non-newest dir removed");
        assert!(stray.exists(), "stray file untouched");
        assert!(newer.exists());
        std::fs::remove_dir_all(&home).ok();
    }
}
