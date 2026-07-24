//! Event log — append-only audit trail for daemon events.
//!
//! Rotates at 10 MB. Keeps up to MAX_GENERATIONS historical files
//! (event-log.jsonl.1 .. event-log.jsonl.N). Entries are fsynced so
//! audit records survive a kernel-level crash of the daemon host.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Maximum log file size before rotation (10 MB).
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

/// Number of rotated generations retained. Oldest is pruned on rotation.
const MAX_GENERATIONS: u32 = 5;

#[derive(Debug, Serialize)]
pub struct Event {
    pub timestamp: String,
    pub kind: &'static str,
    pub instance: String,
    pub detail: String,
}

fn rotated_path(base: &Path, gen: u32) -> PathBuf {
    let mut name = base.file_name().map(|s| s.to_owned()).unwrap_or_default();
    name.push(format!(".{gen}"));
    base.with_file_name(name)
}

// Shift generations up one slot: .N-1 -> .N (drop oldest), ..., .1 -> .2,
// then the live file takes slot .1. Preserves history across repeated
// rotations, unlike the previous single-slot scheme which overwrote .1
// on every rotation and silently lost audit records.
fn rotate(base: &Path) {
    let oldest = rotated_path(base, MAX_GENERATIONS);
    let _ = std::fs::remove_file(&oldest);
    for gen in (1..MAX_GENERATIONS).rev() {
        let src = rotated_path(base, gen);
        let dst = rotated_path(base, gen + 1);
        if src.exists() {
            let _ = std::fs::rename(&src, &dst);
        }
    }
    let first = rotated_path(base, 1);
    let _ = std::fs::rename(base, &first);
}

/// #2158 PR2: best-effort PROCESS context for binding / bypass audit lines —
/// `pid`, parent `ppid`, and `cwd`. ppid is unix-only (`libc::getppid`); `-1` else.
///
/// #2158 GR1 CAVEAT (verified): this captures the context of WHOEVER CALLS it. All
/// current call sites (`bind_full`, `git_helpers`) run DAEMON-side (MCP handler →
/// `execute_tool`), so the captured pid/ppid/cwd is the DAEMON's, NOT the calling
/// agent's — it does NOT attribute a binding change to a caller. Even agent-side
/// context wouldn't separate a transient Task sub-agent from the primary (they
/// share one process). Treat these audit fields as daemon-side, not caller identity.
pub(crate) fn caller_process_context() -> String {
    let pid = std::process::id();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string());
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let ppid: i32 = -1;
    format!("pid={pid} ppid={ppid} cwd={cwd}")
}

/// Build a timestamped event without writing it — the batching counterpart to
/// [`log`], for callers that hand a run of events to [`log_many`].
///
/// #2991: the stamp is taken HERE, when the event is produced, not when the
/// batch is flushed. That keeps the distinct per-event timestamps a per-item
/// `log` loop wrote instead of collapsing a whole run onto one flush instant.
pub fn event(kind: &'static str, instance: &str, detail: String) -> Event {
    Event {
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind,
        instance: instance.to_string(),
        detail,
    }
}

/// Append an event to the log file. Rotates when size exceeds MAX_LOG_SIZE.
pub fn log(home: &Path, kind: &'static str, instance: &str, detail: &str) {
    log_many(home, &[event(kind, instance, detail.to_string())]);
}

/// Append a run of pre-built events in ONE lock + append + fsync cycle.
///
/// #2991: writes the same lines N successive [`log`] calls would, but pays the
/// flock + open + fsync once for the run instead of N times (~4.2ms each on the
/// measured daemon filesystem). The durability unit becomes the batch: a crash
/// mid-run loses the whole run rather than a prefix. That trade is only sound
/// for high-volume ADVISORY events — destructive-audit call sites that carry
/// recovery data (e.g. `branch_sweep`'s `restore_hint`) must keep using [`log`]
/// so each record is durable before the next destructive step runs.
pub fn log_many(home: &Path, events: &[Event]) {
    if events.is_empty() {
        return;
    }

    // H4: size-check + rotation under lock to prevent TOCTOU race
    if let Err(e) = append_lines_under_lock(home, "event-log", |path| {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > MAX_LOG_SIZE {
                rotate(path);
            }
        }
        events
            .iter()
            .map(|e| Ok(serde_json::to_string(e)?))
            .collect()
    }) {
        tracing::warn!(error = %e, count = events.len(), "failed to write event log entries");
    }
}

/// Lower-level primitive used by sister modules that need read access to
/// existing log content under the same lock as the write — for example
/// `task_events::append` computes a monotonic per-instance sequence number
/// by scanning the log, then writes the new envelope, all in one critical
/// section to avoid TOCTOU races between two concurrent appenders.
///
/// `build_lines` receives the log path (lock already held) and returns the
/// lines to append. Returning `Vec::new()` is a no-op.
pub fn append_lines_under_lock<F>(home: &Path, log_name: &str, build_lines: F) -> anyhow::Result<()>
where
    F: FnOnce(&Path) -> anyhow::Result<Vec<String>>,
{
    let log_path = home.join(format!("{log_name}.jsonl"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = log_path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;

    // No size-based rotation here — sister modules own their retention
    // policy (e.g. task_events::compact archives events past
    // COMPACTION_KEEP into a sibling directory replay() also reads).
    // Rotation would silently move events to `.jsonl.N` files outside
    // the replay path, breaking the audit invariant.

    let lines = build_lines(&log_path)?;
    if lines.is_empty() {
        return Ok(());
    }

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    for line in &lines {
        writeln!(f, "{line}")?;
    }
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-event-log-{}-{}-{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn appends_entries() {
        let home = tmp_home("append");
        log(&home, "test", "inst-1", "hello");
        log(&home, "test", "inst-1", "world");
        let content = fs::read_to_string(home.join("event-log.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 2);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rotates_preserving_multiple_generations() {
        let home = tmp_home("rotate");
        let base = home.join("event-log.jsonl");
        // Prime rotated slots 1 and 2 with distinguishable markers.
        fs::write(rotated_path(&base, 1), "GEN1\n").unwrap();
        fs::write(rotated_path(&base, 2), "GEN2\n").unwrap();
        // Live file must exceed MAX_LOG_SIZE to trigger rotation.
        let mut big = String::new();
        while (big.len() as u64) < MAX_LOG_SIZE + 16 {
            big.push('x');
        }
        fs::write(&base, &big).unwrap();

        log(&home, "test", "x", "trigger");

        // Live file reset and contains only the new entry.
        let live = fs::read_to_string(&base).unwrap();
        assert_eq!(live.lines().count(), 1);

        // Previous live -> .1, previous .1 -> .2, previous .2 -> .3.
        let g1 = fs::read_to_string(rotated_path(&base, 1)).unwrap();
        assert!(g1.starts_with("xxxx"), "gen1 must hold rotated live body");
        let g2 = fs::read_to_string(rotated_path(&base, 2)).unwrap();
        assert_eq!(g2, "GEN1\n");
        let g3 = fs::read_to_string(rotated_path(&base, 3)).unwrap();
        assert_eq!(g3, "GEN2\n");

        fs::remove_dir_all(&home).ok();
    }

    /// #2991: the high-volume advisory maintenance loops must persist their N
    /// events through ONE append/sync critical section instead of N.
    ///
    /// The size check that owns rotation lives inside that critical section, so
    /// it fires once per section — which makes the boundary count observable
    /// without adding instrumentation. Priming the live file to exactly
    /// MAX_LOG_SIZE (not over it, so nothing rotates up front) means a per-item
    /// loop appends event 1, pushing the file past the limit, and then event 2's
    /// own size check rotates *mid-run* and splits the events across two files.
    /// A single critical section checks size once and keeps all N together.
    #[test]
    fn advisory_maintenance_batch_writes_through_one_boundary() {
        const N: usize = 6;
        let home = tmp_home("advisory-batch");
        let base = home.join("event-log.jsonl");

        // Exactly MAX_LOG_SIZE bytes, newline-terminated so the primed body is
        // one countable line and appended events start on a fresh line.
        let mut primed = "x".repeat(usize::try_from(MAX_LOG_SIZE).unwrap() - 1);
        primed.push('\n');
        fs::write(&base, &primed).unwrap();
        assert_eq!(fs::metadata(&base).unwrap().len(), MAX_LOG_SIZE);

        // Age past DISPATCH_ASK_MINUTES so sweep_stuck classifies each as an
        // `ask` (the measured high-volume advisory loop), not a `warn`.
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(45)).to_rfc3339();
        for i in 0..N {
            crate::dispatch_tracking::track_dispatch(
                &home,
                crate::dispatch_tracking::DispatchEntry {
                    task_id: Some(format!("t-{i}")),
                    from: "dispatcher".to_string(),
                    to: format!("agent-{i}"),
                    delegated_at: stale.clone(),
                    status: "pending".to_string(),
                    ..Default::default()
                },
            );
        }

        crate::daemon::run_task_maintenance(&home);

        assert!(
            !rotated_path(&base, 1).exists(),
            "a rotated .1 means the advisory run was split by per-item size \
             checks; the batch must cross one append/sync boundary"
        );
        let live = fs::read_to_string(&base).unwrap();
        let asks = live
            .lines()
            .filter(|l| l.contains("dispatch_stuck_ask"))
            .count();
        assert_eq!(
            asks, N,
            "all {N} advisory events must land in the live file"
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rotation_prunes_oldest_beyond_max_generations() {
        let home = tmp_home("prune");
        let base = home.join("event-log.jsonl");
        for gen in 1..=MAX_GENERATIONS {
            fs::write(rotated_path(&base, gen), format!("GEN{gen}\n")).unwrap();
        }
        let mut big = String::new();
        while (big.len() as u64) < MAX_LOG_SIZE + 16 {
            big.push('x');
        }
        fs::write(&base, &big).unwrap();

        log(&home, "test", "x", "trigger");

        // Oldest slot now holds what used to be in the second-oldest slot.
        let gmax = fs::read_to_string(rotated_path(&base, MAX_GENERATIONS)).unwrap();
        assert_eq!(gmax, format!("GEN{}\n", MAX_GENERATIONS - 1));

        fs::remove_dir_all(&home).ok();
    }
}
