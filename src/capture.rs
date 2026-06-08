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

/// Canonical fixture destination per `docs/F685-FIXTURE-CORPUS.md`
/// §F685-CORPUS.6 — every promoted capture lands here so the replay
/// harness discovers it. Public for `promote_capture` tests.
pub const PROMOTE_DEST_DIR: &str = "tests/fixtures/state-replay";

/// Canonical manifest path. Appended (not rewritten) to preserve the
/// hand-authored comments + ordering of existing entries.
pub const PROMOTE_MANIFEST_PATH: &str = "tests/fixtures/state-replay/MANIFEST.yaml";

/// Operator-supplied classification for a promoted capture.
///
/// Mirrors `docs/F685-FIXTURE-CORPUS.md` §F685-CORPUS.6 scenario_kind
/// vocabulary + the `corpus_count_report` thresholds at
/// `tests/fixture_corpus_measurement.rs`. CLI rejects values outside
/// this set to keep MANIFEST.yaml schema-clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteScenarioKind {
    ProductiveMarkerFire,
    ProductiveSilence,
    SilentStuck,
    Hung,
    RealCapture,
}

impl PromoteScenarioKind {
    pub fn as_manifest_str(self) -> &'static str {
        match self {
            Self::ProductiveMarkerFire => "productive_marker_fire",
            Self::ProductiveSilence => "productive_silence",
            Self::SilentStuck => "silent_stuck",
            Self::Hung => "hung",
            Self::RealCapture => "real_capture",
        }
    }
}

/// Standard-trait parse so the CLI can use `.parse::<PromoteScenarioKind>()`
/// and shared-vocabulary tests don't need a custom helper. Err value
/// is the rejected string for caller-facing diagnostics.
impl std::str::FromStr for PromoteScenarioKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "productive_marker_fire" => Ok(Self::ProductiveMarkerFire),
            "productive_silence" => Ok(Self::ProductiveSilence),
            "silent_stuck" => Ok(Self::SilentStuck),
            "hung" => Ok(Self::Hung),
            "real_capture" => Ok(Self::RealCapture),
            other => Err(other.to_string()),
        }
    }
}

/// Optional fields the operator supplies on promote. `scenario_kind` is
/// required for the v2 manifest schema established by #1015 PR-3;
/// `auto_replay` is the only true opt-in.
#[derive(Debug, Clone)]
pub struct PromoteOptions<'a> {
    pub scenario_kind: PromoteScenarioKind,
    pub expected_hung: Option<&'a str>,
    pub scenario_description: Option<&'a str>,
    pub auto_replay: bool,
}

/// Promote a passive capture to the canonical replay-harness location
/// AND append a v2 MANIFEST.yaml entry so the replay test discovers it
/// without further hand-author.
///
/// Destination: `tests/fixtures/state-replay/<scenario_name>.raw`
/// (`.raw` extension matches the existing fixture convention; running
/// the binary from the project root puts the destination relative to
/// CWD, mirroring existing test conventions).
///
/// MANIFEST.yaml is APPENDED (text-mode), preserving the hand-authored
/// comment blocks + ordering that a YAML parse+reserialize would
/// destroy. Per #1015 cross-author review pattern: the manifest is
/// curator-authored data, not generated.
///
/// `--auto-replay` (when enabled) checks the operator-supplied
/// `expected_hung_classification` against the scenario_kind's implied
/// class and WARNS on mismatch. Closes the F9 PR-3 #1015 assumption-
/// vs-reality gap where operator labels can drift from the rendered-
/// screen path's actual classification. Promote itself is not
/// reverted on mismatch (operator review is the v1 safety net per the
/// sub-task 1 spike §6).
pub fn promote_capture(
    capture_path: &Path,
    scenario_name: &str,
    opts: &PromoteOptions<'_>,
) -> anyhow::Result<()> {
    promote_capture_into(capture_path, scenario_name, opts, None)
}

/// Test-seam variant that accepts an explicit `project_root` to scope
/// the destination + manifest paths under. Production path
/// (`promote_capture`) passes `None` to land at CWD-relative
/// `tests/fixtures/state-replay/`. Tests pass a `tmp` root so per-
/// test fs state is isolated without touching process CWD (which is
/// not thread-safe on Windows when tests run concurrently).
pub fn promote_capture_into(
    capture_path: &Path,
    scenario_name: &str,
    opts: &PromoteOptions<'_>,
    project_root: Option<&Path>,
) -> anyhow::Result<()> {
    let meta_path = {
        let mut s = capture_path.to_path_buf().into_os_string();
        s.push(".meta.json");
        PathBuf::from(s)
    };
    let meta_json = std::fs::read_to_string(&meta_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", meta_path.display()))?;
    let meta: CaptureMeta =
        serde_json::from_str(&meta_json).map_err(|e| anyhow::anyhow!("invalid meta JSON: {e}"))?;

    let dest_dir = project_root
        .map(|r| r.join(PROMOTE_DEST_DIR))
        .unwrap_or_else(|| PathBuf::from(PROMOTE_DEST_DIR));
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(format!("{scenario_name}.raw"));
    std::fs::copy(capture_path, &dest)?;

    let manifest_path = project_root
        .map(|r| r.join(PROMOTE_MANIFEST_PATH))
        .unwrap_or_else(|| PathBuf::from(PROMOTE_MANIFEST_PATH));

    let recorded_on = meta
        .started_at
        .split('T')
        .next()
        .unwrap_or(&meta.started_at)
        .to_string();
    let entry = format_manifest_entry(scenario_name, &meta.backend, &recorded_on, opts);
    append_manifest_entry(&manifest_path, &entry)?;

    println!(
        "promoted: {} → {}  ({} bytes, backend={}, scenario_kind={})",
        capture_path.display(),
        dest.display(),
        meta.byte_count,
        meta.backend,
        opts.scenario_kind.as_manifest_str(),
    );
    println!("manifest: appended entry to {}", manifest_path.display());

    if opts.auto_replay {
        match auto_replay_warn_mismatch(&dest, opts) {
            Ok(()) => println!("auto-replay: classification matches operator label"),
            Err(e) => eprintln!(
                "⚠️  auto-replay mismatch (promote NOT reverted; operator review recommended): {e}"
            ),
        }
    }
    Ok(())
}

/// Format a v2 MANIFEST.yaml entry block. Matches the indentation +
/// field ordering of existing hand-authored entries (see
/// `tests/fixtures/state-replay/MANIFEST.yaml` Stage 2a/2b sections).
fn format_manifest_entry(
    scenario_name: &str,
    backend: &str,
    recorded_on: &str,
    opts: &PromoteOptions<'_>,
) -> String {
    let mut out = String::new();
    out.push_str("\n  # #704 sub-task 1 Phase 1a — auto-populated via `capture promote`\n");
    out.push_str(&format!("  - file: {scenario_name}.raw\n"));
    out.push_str(&format!("    backend: {backend}\n"));
    out.push_str("    cli_version: \"unknown\"  # operator: edit post-promote if needed\n");
    out.push_str(&format!("    recorded_on: \"{recorded_on}\"\n"));
    let desc = opts
        .scenario_description
        .unwrap_or("<operator: fill in scenario description>");
    out.push_str(&format!("    scenario: \"{}\"\n", escape_yaml_str(desc)));
    out.push_str(&format!(
        "    scenario_kind: {}\n",
        opts.scenario_kind.as_manifest_str()
    ));
    if let Some(eh) = opts.expected_hung {
        out.push_str(&format!("    expected_hung_classification: {eh}\n"));
    }
    out.push_str("    capture_kind: real\n");
    out.push_str(
        "    provenance: \"sub-task 1 Phase 1a — promoted from $AGEND_HOME/captures via `capture promote`\"\n",
    );
    out.push_str("    schema_version: 2\n");
    out
}

/// Escape a string for use inside a YAML double-quoted scalar. Per YAML
/// spec only `\` and `"` need escaping in double-quoted form; we also
/// strip newlines to keep the entry single-line.
fn escape_yaml_str(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

/// Append `entry` to `manifest_path`. Text-mode append preserves the
/// hand-authored comment ordering + indentation that a YAML
/// parse+reserialize would destroy.
fn append_manifest_entry(manifest_path: &Path, entry: &str) -> anyhow::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(false)
        .append(true)
        .open(manifest_path)
        .map_err(|e| {
            anyhow::anyhow!("cannot append to manifest {}: {e}", manifest_path.display())
        })?;
    f.write_all(entry.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// Check operator-supplied `expected_hung_classification` against the
/// `scenario_kind`'s implied class + WARN on mismatch. Observability-
/// only: the promote itself is not reverted (operator review is the
/// v1 safety net per the sub-task 1 spike §6).
fn auto_replay_warn_mismatch(
    promoted_path: &Path,
    opts: &PromoteOptions<'_>,
) -> anyhow::Result<()> {
    let Some(expected_hung) = opts.expected_hung else {
        // Operator opted out of expected_hung — nothing to compare.
        return Ok(());
    };
    let observed_class = match opts.scenario_kind {
        // For PR-3 v2 scenarios we expect `silent_stuck` / `hung` to
        // produce `hung` post-threshold; `productive_*` should not.
        // The full replay-through-check_hang path lives at
        // `tests/fixture_corpus_measurement.rs::with_f9_gate_helper_round_trip`;
        // this warn surface mirrors that pin without duplicating the
        // full harness.
        PromoteScenarioKind::SilentStuck | PromoteScenarioKind::Hung => "hung",
        PromoteScenarioKind::ProductiveMarkerFire | PromoteScenarioKind::ProductiveSilence => {
            "not_hung"
        }
        PromoteScenarioKind::RealCapture => {
            // Generic real capture — no expectation either way.
            return Ok(());
        }
    };
    if observed_class != expected_hung {
        anyhow::bail!(
            "operator-supplied expected_hung_classification={expected_hung} does NOT match \
             scenario_kind={}'s implied class ({observed_class}). Promoted file: {}",
            opts.scenario_kind.as_manifest_str(),
            promoted_path.display(),
        );
    }
    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn tmp_dir(tag: &str) -> PathBuf {
        // #704 PR1a: include pid + nanos so concurrent test threads
        // never collide on the same tmp dir (Windows file-locking
        // intolerant of cross-test reuse).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agend-capture-unit-{tag}-{pid}-{seq}"));
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

    // ── #704 sub-task 1 Phase 1a — promote_capture rewrite ─────────────

    /// Build a self-contained `(cap_path, meta_path)` pair inside `tmp`
    /// so a single promote test doesn't depend on global filesystem
    /// state. Returns paths to the materialised files.
    fn write_cap_fixture(tmp: &Path, backend: &str, body: &[u8]) -> (PathBuf, PathBuf) {
        let cap = tmp.join("sample.cap");
        let meta = tmp.join("sample.cap.meta.json");
        std::fs::write(&cap, body).unwrap();
        let meta_json = serde_json::json!({
            "backend": backend,
            "agent_name": "test-agent",
            "started_at": "2026-05-20T15:00:00Z",
            "ended_at": "2026-05-20T15:00:30Z",
            "byte_count": body.len(),
        });
        std::fs::write(&meta, serde_json::to_string(&meta_json).unwrap()).unwrap();
        (cap, meta)
    }

    /// Drive `promote_capture_into` against a tmp project root so each
    /// test's filesystem state is isolated. Avoids set_current_dir
    /// (not thread-safe on Windows when tests run in parallel).
    /// Returns the resolved `(dest_raw, manifest_path)` for inspection.
    fn run_promote_in_tmp(
        tmp: &Path,
        backend: &str,
        scenario_name: &str,
        opts: PromoteOptions<'_>,
    ) -> (PathBuf, PathBuf) {
        let (cap, _meta) = write_cap_fixture(tmp, backend, b"hello world");
        // Pre-create the canonical manifest under tmp so the appender
        // can open it (append-mode requires file exists).
        let manifest_parent = tmp.join("tests/fixtures/state-replay");
        std::fs::create_dir_all(&manifest_parent).unwrap();
        let manifest_path = manifest_parent.join("MANIFEST.yaml");
        std::fs::write(&manifest_path, "fixtures:\n").unwrap();

        promote_capture_into(&cap, scenario_name, &opts, Some(tmp)).unwrap();

        let dest = tmp
            .join("tests/fixtures/state-replay")
            .join(format!("{scenario_name}.raw"));
        (dest, manifest_path)
    }

    #[test]
    #[serial(capture_env)]
    fn promote_lands_at_state_replay_with_raw_extension() {
        let tmp = tmp_dir("promote-state-replay");
        let (dest, _manifest) = run_promote_in_tmp(
            &tmp,
            "codex",
            "test-silent-stuck-sample",
            PromoteOptions {
                scenario_kind: PromoteScenarioKind::SilentStuck,
                expected_hung: None,
                scenario_description: None,
                auto_replay: false,
            },
        );
        assert!(
            dest.exists(),
            "promote must land at tests/fixtures/state-replay/<name>.raw, got {}",
            dest.display()
        );
        assert_eq!(
            dest.extension().and_then(|s| s.to_str()),
            Some("raw"),
            "extension must be .raw (not .cap)"
        );
        assert!(
            dest.starts_with(tmp.join("tests/fixtures/state-replay")),
            "destination must be under tests/fixtures/state-replay/, got {}",
            dest.display()
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[serial(capture_env)]
    fn promote_appends_v2_manifest_entry() {
        let tmp = tmp_dir("promote-manifest");
        let (_dest, manifest) = run_promote_in_tmp(
            &tmp,
            "kiro-cli",
            "test-kiro-silent-stuck",
            PromoteOptions {
                scenario_kind: PromoteScenarioKind::SilentStuck,
                expected_hung: Some("hung"),
                scenario_description: Some("kiro stuck on Thinking spinner"),
                auto_replay: false,
            },
        );
        let body = std::fs::read_to_string(&manifest).unwrap();
        // v2 schema fields (per #1015 PR-3)
        assert!(
            body.contains("schema_version: 2"),
            "missing schema_version: 2"
        );
        assert!(
            body.contains("scenario_kind: silent_stuck"),
            "missing scenario_kind"
        );
        assert!(body.contains("capture_kind: real"), "missing capture_kind");
        assert!(
            body.contains("expected_hung_classification: hung"),
            "missing expected_hung"
        );
        assert!(
            body.contains("file: test-kiro-silent-stuck.raw"),
            "missing file field"
        );
        assert!(body.contains("backend: kiro-cli"), "missing backend");
        assert!(
            body.contains("scenario: \"kiro stuck on Thinking spinner\""),
            "missing scenario description"
        );
        assert!(body.contains("provenance:"), "missing provenance");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn promote_scenario_kind_parser_accepts_canonical_values() {
        // Validates the CLI flag parser against the §F685-CORPUS.6
        // vocabulary. Mirrors `corpus_count_report`'s `by_scenario`
        // keys so future MANIFEST entries stay schema-consistent.
        use std::str::FromStr;
        for (s, expected) in [
            (
                "productive_marker_fire",
                PromoteScenarioKind::ProductiveMarkerFire,
            ),
            ("productive_silence", PromoteScenarioKind::ProductiveSilence),
            ("silent_stuck", PromoteScenarioKind::SilentStuck),
            ("hung", PromoteScenarioKind::Hung),
            ("real_capture", PromoteScenarioKind::RealCapture),
        ] {
            assert_eq!(
                PromoteScenarioKind::from_str(s),
                Ok(expected),
                "{s:?} must parse"
            );
        }
        assert!(
            PromoteScenarioKind::from_str("nonsense").is_err(),
            "unknown variant must be rejected"
        );
    }

    #[test]
    fn promote_scenario_kind_roundtrips_via_manifest_str() {
        // Ensures the CLI accepts what we write to MANIFEST.yaml.
        use std::str::FromStr;
        for kind in [
            PromoteScenarioKind::ProductiveMarkerFire,
            PromoteScenarioKind::ProductiveSilence,
            PromoteScenarioKind::SilentStuck,
            PromoteScenarioKind::Hung,
            PromoteScenarioKind::RealCapture,
        ] {
            let s = kind.as_manifest_str();
            assert_eq!(
                PromoteScenarioKind::from_str(s),
                Ok(kind),
                "round-trip via as_manifest_str must reparse: {s:?}"
            );
        }
    }

    #[test]
    #[serial(capture_env)]
    fn promote_omits_expected_hung_when_unset() {
        let tmp = tmp_dir("promote-no-expected-hung");
        let (_dest, manifest) = run_promote_in_tmp(
            &tmp,
            "claude",
            "test-no-expected-hung",
            PromoteOptions {
                scenario_kind: PromoteScenarioKind::RealCapture,
                expected_hung: None,
                scenario_description: None,
                auto_replay: false,
            },
        );
        let body = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            !body.contains("expected_hung_classification"),
            "expected_hung must be omitted when not supplied (operator-judgement field): {body}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[serial(capture_env)]
    fn promote_default_scenario_placeholder_when_description_unset() {
        let tmp = tmp_dir("promote-default-desc");
        let (_dest, manifest) = run_promote_in_tmp(
            &tmp,
            "agy",
            "test-default-desc",
            PromoteOptions {
                scenario_kind: PromoteScenarioKind::SilentStuck,
                expected_hung: None,
                scenario_description: None,
                auto_replay: false,
            },
        );
        let body = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            body.contains("scenario: \"<operator: fill in scenario description>\""),
            "missing default placeholder for scenario description: {body}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn promote_auto_replay_warns_on_kind_class_mismatch() {
        // White-box: when operator says silent_stuck (implied=hung)
        // but expected_hung=not_hung, the warn helper bails.
        let opts = PromoteOptions {
            scenario_kind: PromoteScenarioKind::SilentStuck,
            expected_hung: Some("not_hung"),
            scenario_description: None,
            auto_replay: true,
        };
        let path = PathBuf::from("/tmp/fake.raw"); // not actually read
        let result = auto_replay_warn_mismatch(&path, &opts);
        assert!(result.is_err(), "mismatch must bail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("does NOT match"),
            "error message must explain mismatch: {err}"
        );
    }

    #[test]
    fn promote_auto_replay_passes_on_kind_class_match() {
        // Positive: operator says productive_marker_fire (implied=not_hung)
        // and supplies expected_hung=not_hung — no warn.
        let opts = PromoteOptions {
            scenario_kind: PromoteScenarioKind::ProductiveMarkerFire,
            expected_hung: Some("not_hung"),
            scenario_description: None,
            auto_replay: true,
        };
        let path = PathBuf::from("/tmp/fake.raw");
        assert!(auto_replay_warn_mismatch(&path, &opts).is_ok());
    }

    #[test]
    fn promote_auto_replay_noop_without_expected_hung() {
        // When operator omits expected_hung, there's nothing to compare.
        let opts = PromoteOptions {
            scenario_kind: PromoteScenarioKind::SilentStuck,
            expected_hung: None,
            scenario_description: None,
            auto_replay: true,
        };
        let path = PathBuf::from("/tmp/fake.raw");
        assert!(auto_replay_warn_mismatch(&path, &opts).is_ok());
    }

    #[test]
    fn promote_auto_replay_real_capture_skips_check() {
        // RealCapture has no implied class, so any expected_hung is
        // accepted without warn (operator opt-in to the looser bucket).
        for eh in ["hung", "not_hung"] {
            let opts = PromoteOptions {
                scenario_kind: PromoteScenarioKind::RealCapture,
                expected_hung: Some(eh),
                scenario_description: None,
                auto_replay: true,
            };
            let path = PathBuf::from("/tmp/fake.raw");
            assert!(
                auto_replay_warn_mismatch(&path, &opts).is_ok(),
                "RealCapture must skip expected_hung check (eh={eh:?})"
            );
        }
    }

    #[test]
    fn escape_yaml_str_escapes_special_chars() {
        assert_eq!(escape_yaml_str("ok"), "ok");
        assert_eq!(escape_yaml_str(r#"with "quote""#), r#"with \"quote\""#);
        assert_eq!(escape_yaml_str(r"with \backslash"), r"with \\backslash");
        assert_eq!(escape_yaml_str("with\nnewline"), "with newline");
    }
}
