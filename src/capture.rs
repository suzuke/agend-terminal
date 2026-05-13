//! Passive PTY capture for growing the fixture corpus (issue #704).
//!
//! Activated by `AGEND_CAPTURE_FIXTURES=1`; zero overhead when unset.
//! Captures land in `$AGEND_HOME/captures/<agent>/<epoch_ms>.cap` with a
//! companion `<epoch_ms>.cap.meta.json` sidecar written on drop.
//!
//! `promote_capture` copies a chosen .cap into `tests/fixtures/<backend>/`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Cumulative .cap budget per agent; oldest files deleted when exceeded.
const CAPTURE_ROTATION_BUDGET_BYTES: u64 = 50 * 1024 * 1024;

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
    /// Create capture dir, rotate if needed, open a new .cap file.
    /// Returns `None` if the env flag is off or directory/file creation fails.
    pub fn new_if_enabled(home: &Path, agent_name: &str, backend: &str) -> Option<Self> {
        if std::env::var("AGEND_CAPTURE_FIXTURES").as_deref() != Ok("1") {
            return None;
        }
        let dir = home.join("captures").join(agent_name);
        std::fs::create_dir_all(&dir).ok()?;
        rotate_captures(&dir);
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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

    pub fn write(&mut self, data: &[u8]) {
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
            let _ = std::fs::write(&self.meta_path, json);
        }
    }
}

/// Delete oldest .cap files (and their sidecars) until total is below limit.
fn rotate_captures(dir: &Path) {
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("cap")).then(|| {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                (p, size)
            })
        })
        .collect();
    let total: u64 = files.iter().map(|(_, s)| s).sum();
    if total < CAPTURE_ROTATION_BUDGET_BYTES {
        return;
    }
    files.sort_by(|(a, _), (b, _)| a.cmp(b)); // oldest-first via epoch prefix
    let mut remaining = total;
    for (path, size) in files {
        if remaining < CAPTURE_ROTATION_BUDGET_BYTES {
            break;
        }
        let sidecar = {
            let mut s = path.clone().into_os_string();
            s.push(".meta.json");
            PathBuf::from(s)
        };
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&sidecar);
        remaining = remaining.saturating_sub(size);
    }
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agend-capture-unit-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sink_none_when_env_unset() {
        std::env::remove_var("AGEND_CAPTURE_FIXTURES");
        let dir = tmp_dir("none");
        assert!(CaptureSink::new_if_enabled(&dir, "a", "shell").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
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
    fn rotate_removes_oldest_when_over_limit() {
        let dir = tmp_dir("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        // Write 3 fake .cap files totalling > CAPTURE_ROTATION_BUDGET_BYTES
        let big = vec![0u8; (CAPTURE_ROTATION_BUDGET_BYTES / 2 + 1) as usize];
        for epoch in [1000u64, 2000, 3000] {
            std::fs::write(dir.join(format!("{epoch}.cap")), &big).unwrap();
        }
        rotate_captures(&dir);
        let remaining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("cap"))
            .collect();
        // After rotation, total must be < CAPTURE_ROTATION_BUDGET_BYTES
        let total: u64 = remaining
            .iter()
            .map(|e| e.path().metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        assert!(
            total < CAPTURE_ROTATION_BUDGET_BYTES,
            "total={total} must be < limit"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
