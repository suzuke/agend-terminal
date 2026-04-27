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

/// Append an event to the log file. Rotates when size exceeds MAX_LOG_SIZE.
pub fn log(home: &Path, kind: &'static str, instance: &str, detail: &str) {
    let log_path = home.join("event-log.jsonl");
    let event = Event {
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind,
        instance: instance.to_string(),
        detail: detail.to_string(),
    };

    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > MAX_LOG_SIZE {
            rotate(&log_path);
        }
    }

    if let Ok(json) = serde_json::to_string(&event) {
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{json}") {
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to write event log entry");
                    return;
                }
                // Flush kernel buffers so audit records survive a host
                // crash. Best-effort: we cannot fail a caller on fsync
                // error, but we surface it in logs.
                if let Err(e) = f.sync_all() {
                    tracing::warn!(path = %log_path.display(), error = %e, "event log fsync failed");
                }
            }
            Err(e) => {
                tracing::warn!(path = %log_path.display(), error = %e, "failed to open event log");
            }
        }
    }
}

/// Append a typed event to a sister audit log (e.g. `task_events.jsonl`).
///
/// Locked + rotated + fsynced. Returns Err so sister modules with strict
/// audit semantics can fail-loud rather than swallow IO errors the way
/// [`log`] does. Future audit logs (decision_events, capability_events)
/// reuse this primitive.
#[allow(dead_code)]
pub fn append<T: Serialize>(home: &Path, log_name: &str, event: &T) -> anyhow::Result<()> {
    append_lines_under_lock(home, log_name, |_| Ok(vec![serde_json::to_string(event)?]))
}

/// Append multiple typed events under a single fsync (F7 atomic-batch
/// pattern). Either all events land or none do — readers never observe a
/// partial-write window.
#[allow(dead_code)]
pub fn append_batch<T: Serialize>(home: &Path, log_name: &str, events: &[T]) -> anyhow::Result<()> {
    append_lines_under_lock(home, log_name, |_| {
        events
            .iter()
            .map(|e| serde_json::to_string(e).map_err(anyhow::Error::from))
            .collect()
    })
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
