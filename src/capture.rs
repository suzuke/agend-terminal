//! Passive PTY capture for growing the fixture corpus (issue #704).
//!
//! Activated by `AGEND_CAPTURE_FIXTURES=1`; zero overhead when unset.
//! Captures land in `$AGEND_HOME/captures/<agent>/<epoch_ms>.cap` with a
//! companion `<epoch_ms>.cap.meta.json` sidecar written on drop.
//!
//! `promote_capture` copies a chosen .cap into `tests/fixtures/<backend>/`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Cumulative .cap budget per agent; oldest files deleted when exceeded.
const CAPTURE_ROTATION_BUDGET_BYTES: u64 = 50 * 1024 * 1024;

// ── Trait ──────────────────────────────────────────────────────────────────

/// Abstraction over the PTY byte sink so `pty_read_loop` can call `.write()`
/// unconditionally without an `Option` branch.
pub trait CaptureWriter: Send {
    fn write(&mut self, data: &[u8]);
}

/// Zero-sized no-op: produced when `AGEND_CAPTURE_FIXTURES` is unset.
/// `Box<NoOpCapture>` does not allocate (ZST optimisation).
pub struct NoOpCapture;

impl CaptureWriter for NoOpCapture {
    #[inline(always)]
    fn write(&mut self, _data: &[u8]) {}
}

// ── Real sink ──────────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CaptureMeta {
    pub backend: String,
    pub agent_name: String,
    pub started_at: String,
    pub ended_at: String,
    pub byte_count: u64,
}

/// Open-file capture sink. Writes raw PTY bytes; flushes meta sidecar on drop.
pub struct CaptureSink {
    file: std::fs::File,
    meta_path: PathBuf,
    agent_name: String,
    backend: String,
    started_at: String,
    pub byte_count: u64,
}

impl CaptureSink {
    fn new_if_enabled(home: &Path, agent_name: &str, backend: &str) -> Option<Self> {
        if std::env::var("AGEND_CAPTURE_FIXTURES").as_deref() != Ok("1") {
            return None;
        }
        let dir = home.join("captures").join(agent_name);
        std::fs::create_dir_all(&dir).ok()?;
        rotate_captures(&dir);
        let epoch_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let cap_path = dir.join(format!("{epoch_ms}.cap"));
        let meta_path = {
            let mut s = cap_path.clone().into_os_string();
            s.push(".meta.json");
            PathBuf::from(s)
        };
        let file = std::fs::File::create(&cap_path).ok()?;
        Some(Self {
            file,
            meta_path,
            agent_name: agent_name.to_string(),
            backend: backend.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            byte_count: 0,
        })
    }
}

impl CaptureWriter for CaptureSink {
    fn write(&mut self, data: &[u8]) {
        if self.file.write_all(data).is_ok() {
            self.byte_count += data.len() as u64;
        }
    }
}

impl Drop for CaptureSink {
    fn drop(&mut self) {
        let meta = CaptureMeta {
            backend: self.backend.clone(),
            agent_name: self.agent_name.clone(),
            started_at: self.started_at.clone(),
            ended_at: chrono::Utc::now().to_rfc3339(),
            byte_count: self.byte_count,
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            // IO errors on drop are silently discarded — never propagate.
            let _ = std::fs::write(&self.meta_path, json);
        }
    }
}

// ── Factory ────────────────────────────────────────────────────────────────

/// Return a `FsCapture` when `AGEND_CAPTURE_FIXTURES=1`, else a `NoOpCapture`.
/// The caller unconditionally calls `.write()` — no branch on the hot path.
pub fn make_capture_writer(
    home: Option<&Path>,
    agent_name: &str,
    backend: &str,
) -> Box<dyn CaptureWriter + Send> {
    home.and_then(|h| CaptureSink::new_if_enabled(h, agent_name, backend))
        .map(|s| Box::new(s) as Box<dyn CaptureWriter + Send>)
        .unwrap_or_else(|| Box::new(NoOpCapture))
}

// ── Rotation ───────────────────────────────────────────────────────────────

fn rotate_captures(dir: &Path) {
    rotate_captures_with_budget(dir, CAPTURE_ROTATION_BUDGET_BYTES);
}

/// Delete oldest (by mtime) `.cap` files until total is below `budget`.
/// Always keeps at least one file so an in-progress capture is never deleted.
fn rotate_captures_with_budget(dir: &Path, budget: u64) {
    let mut files: Vec<(PathBuf, u64, SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("cap") {
                return None;
            }
            let meta = p.metadata().ok()?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((p, meta.len(), mtime))
        })
        .collect();

    if files.len() <= 1 {
        return;
    }

    let total: u64 = files.iter().map(|(_, s, _)| s).sum();
    if total < budget {
        return;
    }

    files.sort_by_key(|(_, _, mtime)| *mtime); // oldest-first

    let mut remaining = total;
    let mut files_left = files.len();
    for (path, size, _) in &files {
        if remaining < budget || files_left <= 1 {
            break;
        }
        let sidecar = {
            let mut s = path.clone().into_os_string();
            s.push(".meta.json");
            PathBuf::from(s)
        };
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(&sidecar);
        remaining = remaining.saturating_sub(*size);
        files_left -= 1;
    }
}

// ── Promote ────────────────────────────────────────────────────────────────

/// Copy `capture_path` to `tests/fixtures/<backend>/<scenario_name>.cap`.
///
/// Backend is read from the sidecar `.meta.json`. The destination path is
/// relative to the current working directory (run from the project root).
pub fn promote_capture(capture_path: &Path, scenario_name: &str) -> anyhow::Result<()> {
    let meta_path = {
        let mut s = capture_path.to_path_buf().into_os_string();
        s.push(".meta.json");
        PathBuf::from(s)
    };
    let meta_json = std::fs::read_to_string(&meta_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", meta_path.display()))?;
    let meta: CaptureMeta =
        serde_json::from_str(&meta_json).map_err(|e| anyhow::anyhow!("invalid meta JSON: {e}"))?;

    let dest_dir = PathBuf::from("tests/fixtures").join(&meta.backend);
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(format!("{scenario_name}.cap"));
    std::fs::copy(capture_path, &dest)?;
    println!(
        "promoted: {} → {}  ({} bytes, backend={})",
        capture_path.display(),
        dest.display(),
        meta.byte_count,
        meta.backend,
    );
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-capture-unit-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[serial(capture_env)]
    fn sink_none_when_env_unset() {
        std::env::remove_var("AGEND_CAPTURE_FIXTURES");
        let dir = tmp_dir("none");
        assert!(CaptureSink::new_if_enabled(&dir, "a", "shell").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial(capture_env)]
    fn sink_creates_cap_and_meta_on_drop() {
        let dir = tmp_dir("create");
        std::env::set_var("AGEND_CAPTURE_FIXTURES", "1");
        let mut sink = CaptureSink::new_if_enabled(&dir, "myagent", "claude").unwrap();
        let data = b"test bytes";
        sink.write(data);
        assert_eq!(sink.byte_count, data.len() as u64);
        drop(sink);
        std::env::remove_var("AGEND_CAPTURE_FIXTURES");

        let cap_dir = dir.join("captures").join("myagent");
        let caps: Vec<_> = std::fs::read_dir(&cap_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("cap"))
            .collect();
        assert_eq!(caps.len(), 1);
        let cap_path = caps[0].path();
        assert_eq!(std::fs::read(&cap_path).unwrap(), data);

        let meta_path = PathBuf::from({
            let mut s = cap_path.into_os_string();
            s.push(".meta.json");
            s
        });
        assert!(meta_path.exists());
        let meta: CaptureMeta =
            serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
        assert_eq!(meta.backend, "claude");
        assert_eq!(meta.agent_name, "myagent");
        assert_eq!(meta.byte_count, data.len() as u64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotate_keeps_single_file_even_over_budget() {
        let dir = tmp_dir("rotate-single");
        std::fs::write(dir.join("1000.cap"), b"big").unwrap();
        rotate_captures_with_budget(&dir, 1); // budget=1 byte, file is 3 bytes
        assert!(
            dir.join("1000.cap").exists(),
            "must not delete last remaining file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotate_deletes_oldest_by_mtime_not_name() {
        let dir = tmp_dir("rotate-mtime");
        // "9000.cap" created first (older mtime), "1000.cap" created second (newer).
        // Alphabetically "1000" < "9000", so old sort-by-name would delete the
        // wrong file. Mtime sort must delete "9000.cap".
        std::fs::write(dir.join("9000.cap"), b"aaa").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("1000.cap"), b"bbb").unwrap();
        // budget = 5 bytes, total = 6 bytes → need to delete one
        rotate_captures_with_budget(&dir, 5);
        assert!(
            !dir.join("9000.cap").exists(),
            "oldest by mtime must be deleted"
        );
        assert!(
            dir.join("1000.cap").exists(),
            "newest by mtime must survive"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn noop_capture_writer_is_zero_cost() {
        let mut noop = NoOpCapture;
        noop.write(b"ignored");
    }
}
