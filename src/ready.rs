//! #40783-35 β1: typed runtime-provenance identity for the `.ready` marker.
//!
//! The marker keeps its historical path and its existence semantics — every
//! consumer that only asks `run_dir/.ready`.is_file()` is unaffected. What
//! changes is the CONTENT: a `schema_version = 1` JSON identity of the process
//! that wrote it, so an out-of-process reader can later distinguish a running
//! image from a replaced on-disk one without guessing.
//!
//! Two legacy shapes predate this and are still found on disk: an RFC3339
//! timestamp (both writers, pre-β1) and the literal `ready` (test fixtures).
//! Neither carries identity, so both resolve to [`ReadyProvenance::Unknown`] —
//! honest, never a guessed match. Malformed bytes and a future
//! `schema_version` also resolve to `Unknown` rather than erroring, so a reader
//! fails closed instead of crashing.
//!
//! Scope note: this slice only writes and reads the marker. Validating the
//! identity (liveness, PID reuse, on-disk `--version`) and acting on a mismatch
//! are deliberately NOT here.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The historical marker filename. Unchanged — existence consumers depend on it.
pub const FILENAME: &str = ".ready";

/// Current marker schema. A reader that sees anything higher reports
/// [`UnknownReason::FutureSchema`] rather than guessing.
pub const SCHEMA_VERSION: u8 = 1;

/// Identity of the process that published the marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyMarker {
    pub schema_version: u8,
    pub pid: u32,
    /// OS process-start identity token for the recorded `pid`, so a later
    /// reader can tell the same process from a PID reuse. `0` means the token
    /// was not determinable when the marker was written — never a match.
    pub started_at: u64,
    /// Absolute path of the executable that was running. Empty means
    /// `current_exe()` failed; the marker is still published, because
    /// existence semantics matter more than identity completeness.
    pub exe: PathBuf,
    pub build_sha: String,
    pub build_dirty: bool,
    pub version: String,
}

/// Why a marker on disk carries no usable identity.
///
/// The reader half below is consumed by tests in this slice; its production
/// consumer is the doctor gate, which is deliberately a separate slice. Pinned
/// here as the stable surface that gate will read, mirroring the same
/// `#[allow(dead_code)]` rationale on `cli::HelperStaleness::as_str`.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownReason {
    /// A pre-schema marker: the RFC3339 timestamp both writers used before
    /// this slice, or the literal `ready` fixture payload.
    LegacyText,
    /// Present but unparseable as the schema it claims.
    Malformed,
    /// Written by a newer build than this one.
    FutureSchema(u8),
    /// Absent, or unreadable.
    Unreadable,
}

/// What the marker on disk proves about the process that wrote it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyProvenance {
    V1(ReadyMarker),
    Unknown(UnknownReason),
}

/// Probe just enough to route on the declared schema, so a future version is
/// reported as future rather than as a malformed v1.
#[allow(dead_code)]
#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u8,
}

fn current_marker() -> ReadyMarker {
    let pid = std::process::id();
    ReadyMarker {
        schema_version: SCHEMA_VERSION,
        pid,
        started_at: crate::process::process_start_token(pid).unwrap_or(0),
        exe: std::env::current_exe().unwrap_or_default(),
        build_sha: option_env!("AGEND_BUILD_SHA")
            .unwrap_or("unknown")
            .to_owned(),
        build_dirty: option_env!("AGEND_BUILD_DIRTY") == Some("1"),
        version: env!("AGEND_CLI_VERSION").to_owned(),
    }
}

/// Publish this process's identity to `run_dir/.ready`.
///
/// Atomic: a crashed or racing write cannot leave a half-written marker that a
/// reader would classify as malformed. The previous hand-rolled
/// `std::fs::write` at both call sites had no such guarantee.
pub fn write(run_dir: &Path) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(&current_marker())?;
    crate::store::atomic_write(&run_dir.join(FILENAME), &bytes)
}

/// Classify whatever is at `run_dir/.ready`. Never errors and never guesses:
/// anything that is not a complete current-schema marker is `Unknown`.
#[allow(dead_code)]
pub fn read(run_dir: &Path) -> ReadyProvenance {
    let Ok(bytes) = std::fs::read(run_dir.join(FILENAME)) else {
        return ReadyProvenance::Unknown(UnknownReason::Unreadable);
    };
    match serde_json::from_slice::<SchemaProbe>(&bytes) {
        Ok(probe) if probe.schema_version == SCHEMA_VERSION => {
            match serde_json::from_slice::<ReadyMarker>(&bytes) {
                Ok(marker) => ReadyProvenance::V1(marker),
                Err(_) => ReadyProvenance::Unknown(UnknownReason::Malformed),
            }
        }
        Ok(probe) => ReadyProvenance::Unknown(UnknownReason::FutureSchema(probe.schema_version)),
        // Not JSON at all — either a legacy text marker or garbage. Both are
        // Unknown; the distinction is diagnostic only.
        Err(_) => {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            if text == "ready" || chrono::DateTime::parse_from_rfc3339(text).is_ok() {
                ReadyProvenance::Unknown(UnknownReason::LegacyText)
            } else {
                ReadyProvenance::Unknown(UnknownReason::Malformed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_run_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-ready-{}-{}-{}",
            std::process::id(),
            tag,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create run dir");
        dir
    }

    /// The writer publishes a COMPLETE v1 identity — every field the slice
    /// promises, not a partial record — and the reader round-trips it.
    #[test]
    fn write_publishes_complete_v1_identity() {
        let run_dir = tmp_run_dir("complete");
        write(&run_dir).expect("write ready marker");

        let marker = match read(&run_dir) {
            ReadyProvenance::V1(marker) => marker,
            other => panic!("expected a v1 identity, got {other:?}"),
        };
        assert_eq!(marker.schema_version, SCHEMA_VERSION);
        assert_eq!(marker.pid, std::process::id(), "pid must be this process");
        assert_eq!(
            marker.exe,
            std::env::current_exe().expect("current exe"),
            "exe must be the absolute running executable"
        );
        assert!(marker.exe.is_absolute(), "exe must be absolute");
        assert!(!marker.version.is_empty(), "version must be recorded");
        assert!(!marker.build_sha.is_empty(), "build_sha must be recorded");
        assert_eq!(
            marker.started_at,
            crate::process::process_start_token(std::process::id()).unwrap_or(0),
            "started_at must be this process's start-identity token"
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// Existence semantics are the load-bearing back-compat contract: the file
    /// stays at the same path and stays a regular file, so `is_file()`
    /// consumers keep working.
    #[test]
    fn write_preserves_path_and_existence_semantics() {
        let run_dir = tmp_run_dir("existence");
        write(&run_dir).expect("write ready marker");

        let path = run_dir.join(FILENAME);
        assert_eq!(FILENAME, ".ready", "the historical filename must not move");
        assert!(path.is_file(), "existence consumers must still see a file");

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// The pre-β1 writers wrote an RFC3339 timestamp. It carries no identity,
    /// so it must read as honest Unknown — not an error, not a guess.
    #[test]
    fn legacy_rfc3339_marker_is_honest_unknown() {
        let run_dir = tmp_run_dir("legacy-rfc3339");
        std::fs::write(
            run_dir.join(FILENAME),
            chrono::Utc::now().to_rfc3339().as_bytes(),
        )
        .expect("write legacy marker");

        assert_eq!(
            read(&run_dir),
            ReadyProvenance::Unknown(UnknownReason::LegacyText)
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// The second legacy shape in-tree: a literal `ready` payload.
    #[test]
    fn legacy_literal_ready_marker_is_honest_unknown() {
        let run_dir = tmp_run_dir("legacy-literal");
        std::fs::write(run_dir.join(FILENAME), b"ready").expect("write legacy marker");

        assert_eq!(
            read(&run_dir),
            ReadyProvenance::Unknown(UnknownReason::LegacyText)
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// Truncated/garbage JSON must fail closed, not panic and not half-parse.
    #[test]
    fn malformed_marker_fails_closed_without_crashing() {
        let run_dir = tmp_run_dir("malformed");
        std::fs::write(run_dir.join(FILENAME), b"{\"schema_version\": 1, \"pid\":")
            .expect("write malformed marker");

        assert_eq!(
            read(&run_dir),
            ReadyProvenance::Unknown(UnknownReason::Malformed)
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// A newer writer's schema must be reported as such rather than
    /// mis-parsed as v1 — the difference between "I don't know" and a wrong
    /// answer.
    #[test]
    fn future_schema_is_unknown_not_guessed() {
        let run_dir = tmp_run_dir("future");
        std::fs::write(
            run_dir.join(FILENAME),
            br#"{"schema_version":2,"something_new":true}"#,
        )
        .expect("write future marker");

        assert_eq!(
            read(&run_dir),
            ReadyProvenance::Unknown(UnknownReason::FutureSchema(2))
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// An absent marker is Unknown, not an error — callers must not have to
    /// special-case "not bootstrapped yet".
    #[test]
    fn absent_marker_is_unknown() {
        let run_dir = tmp_run_dir("absent");

        assert_eq!(
            read(&run_dir),
            ReadyProvenance::Unknown(UnknownReason::Unreadable)
        );

        std::fs::remove_dir_all(&run_dir).ok();
    }

    /// Source pin for "BOTH real writers". Booting the daemon and the TUI to
    /// observe their writes is disproportionate for a marker change, so this
    /// asserts at the source level that neither call site still hand-writes an
    /// RFC3339 payload and that both go through the shared atomic writer.
    /// The sibling idiom is `daemon::supervisor::tests`' brace-matched anchor.
    #[test]
    fn both_real_writers_route_through_the_shared_atomic_writer() {
        for (label, src) in [
            ("daemon", include_str!("daemon/mod.rs")),
            ("app/TUI", include_str!("api/mod.rs")),
        ] {
            assert!(
                src.contains("crate::ready::write("),
                "{label} writer must publish through crate::ready::write"
            );
            // Anchored on the ready path specifically: `daemon/mod.rs` still
            // writes an RFC3339 payload for the SEPARATE `.daemon` control-plane
            // marker, which this slice must not touch.
            assert!(
                !src.contains("std::fs::write(&ready_path,"),
                "{label} writer must no longer hand-write the .ready marker"
            );
        }
    }
}
