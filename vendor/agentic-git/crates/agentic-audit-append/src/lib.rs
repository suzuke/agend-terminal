//! #3416: the one serialized appender for `$AGEND_HOME/fleet_events.jsonl`.
//!
//! ## The defect this exists to prevent
//! Every sink previously did `writeln!(file, "{event}")` on an UNBUFFERED
//! `std::fs::File`. `File::write_fmt` issues one `write()` syscall per formatting
//! fragment — `serde_json`'s `Display` emits the value token by token, roughly 64
//! syscalls for a real record — and `O_APPEND` makes only a SINGLE syscall atomic
//! with respect to other writers. Concurrent appenders therefore interleaved
//! *inside* a record and destroyed both. On the live fleet log that produced 14.05%
//! unparseable records overall and 44.61% across the most recent 20k; a real
//! 8-process run through the real shim entry corrupted 3135 of 3135 lines.
//!
//! Two of the four sinks are fail-closed audit gates (force-merge and creator
//! force-delete refuse when the write fails), so an interleaved-but-`Ok` write let
//! those gates report a destructive bypass as audited while the record on disk was
//! garbage. That is why this is a correctness contract and not log hygiene.
//!
//! ## The contract
//! 1. **Serialize first.** The complete line, newline included, is built in memory
//!    *before* any lock is taken, so the locked region contains no formatting.
//! 2. **One `write_all` under the lock.** The target file is opened before locking;
//!    the lock covers exactly one `write_all` call and nothing else.
//! 3. **No unlocked fallback.** A caller that cannot take the lock gets
//!    [`AppendError::Contended`] and writes nothing. Falling back to an unlocked
//!    append would reintroduce the interleaving this crate removes.
//!
//! What an `Err` does and does not promise: [`Contended`](AppendError::Contended)
//! and [`Open`](AppendError::Open) mean nothing reached the file, because both
//! occur before any write. [`Write`](AppendError::Write) does NOT — `write_all` can
//! fail with bytes already written, and this crate does not roll that back, so a
//! partial line may be on disk. Rolling back under the lock (record the length,
//! `set_len` on failure) would narrow but not close that window, since a crash
//! mid-write rolls back nothing; it is deliberately left out of this change.
//!
//! Note that (1) alone measurably removes the corruption at observed record sizes,
//! but single-`write()` atomicity for regular files is a platform property rather
//! than a POSIX guarantee (`PIPE_BUF` covers pipes only) and would fail silently on
//! filesystems that do not provide it. The lock is what makes the property a
//! contract; the pre-serialization is what makes the locked window one syscall wide.
//!
//! ## Contention policy is the caller's, not this crate's
//! The two policies differ because the callers' obligations differ, so each is a
//! separate entry point rather than a flag:
//! - [`append_audit_line_best_effort`] — one non-blocking attempt, skip on
//!   contention. For the high-frequency shim sinks, whose documented contract is
//!   that they never block (the caller `exec`s real git immediately after).
//! - [`append_audit_line_bounded`] — bounded retry, then an explicit error. For the
//!   destructive daemon gates, which must stay fail-closed without hanging an MCP
//!   worker.
//!
//! Advisory locks are released by the OS when the file descriptor closes, so a
//! crashed holder cannot poison the lock and no in-process mutex ordering is
//! introduced.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The fleet audit log, relative to `$AGEND_HOME`.
pub const AUDIT_FILE: &str = "fleet_events.jsonl";
/// Companion advisory lock for [`AUDIT_FILE`], relative to `$AGEND_HOME`.
///
/// A separate file rather than locking the log itself: the log is opened
/// `O_APPEND` by every writer and is also read by tooling, and a lock taken on the
/// data file would be entangled with those opens.
pub const LOCK_FILE: &str = "fleet_events.jsonl.lock";

/// Poll interval for [`append_audit_line_bounded`]. Short relative to any sane
/// budget so the retry is responsive, long enough not to spin.
const RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// Default budget for the destructive-gate callers.
///
/// The locked region is a single `write_all`, so a holder is expected to release
/// within microseconds; a budget three orders of magnitude larger absorbs
/// scheduling noise while still bounding an MCP worker's stall.
pub const DEFAULT_BOUNDED_BUDGET: Duration = Duration::from_millis(250);

/// Path of the audit log under `home`.
pub fn audit_path(home: &Path) -> PathBuf {
    home.join(AUDIT_FILE)
}

/// Path of the companion lock under `home`.
pub fn lock_path(home: &Path) -> PathBuf {
    home.join(LOCK_FILE)
}

/// Why an append did not complete.
///
/// The variants differ in what they promise about the file, and the distinction is
/// load-bearing for the fail-closed gates: only [`Write`](AppendError::Write) can
/// leave anything behind.
#[derive(Debug)]
pub enum AppendError {
    /// The companion lock could not be taken within the caller's policy. **Nothing
    /// was written** — no write was even attempted — and it must not be written
    /// unlocked.
    Contended,
    /// The lock file or the log could not be opened. **Nothing was written**; this
    /// happens before the lock is taken and before any write.
    Open(std::io::Error),
    /// `write_all` failed while the lock was held. **A partial record may be on
    /// disk**: `write_all` can fail after some bytes have already reached the file,
    /// and this crate does not roll that back.
    ///
    /// This is the one case a caller must not describe as "nothing was recorded".
    /// The record is not trustworthy either way — a truncated line is exactly the
    /// unparseable-row shape #3416 removes — so a fail-closed caller must still
    /// refuse, but it should say what it actually knows.
    Write(std::io::Error),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppendError::Contended => write!(
                f,
                "audit log is locked by another writer; nothing was appended"
            ),
            AppendError::Open(e) => {
                write!(f, "audit log could not be opened; nothing was appended: {e}")
            }
            AppendError::Write(e) => write!(
                f,
                "audit log write failed after the lock was taken; a partial record may be on disk: {e}"
            ),
        }
    }
}

impl std::error::Error for AppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppendError::Contended => None,
            AppendError::Open(e) | AppendError::Write(e) => Some(e),
        }
    }
}

/// How hard the caller is willing to try for the lock.
#[derive(Clone, Copy)]
enum Acquire {
    /// One non-blocking attempt; give up immediately on contention.
    TryOnce,
    /// Retry until the budget elapses, then give up.
    Bounded(Duration),
}

/// Append one JSON value as a line, best-effort: a single non-blocking lock
/// attempt, and on contention the record is **skipped**, never written unlocked.
///
/// For sinks documented as never blocking. Returns [`AppendError::Contended`] when
/// the record was skipped so the caller can account for it if it wants to.
pub fn append_audit_line_best_effort(
    home: &Path,
    value: &serde_json::Value,
) -> Result<(), AppendError> {
    append_audit_line(home, value, Acquire::TryOnce)
}

/// Append one JSON value as a line, retrying for the lock up to `budget` and then
/// failing explicitly.
///
/// For the destructive fail-closed gates: on `Err` the caller must refuse the
/// operation it was auditing, because no trustworthy record exists. Note the
/// variants differ — [`Contended`](AppendError::Contended) and
/// [`Open`](AppendError::Open) wrote nothing, while [`Write`](AppendError::Write)
/// may have left a partial line. Refuse in every case, but do not report "nothing
/// was recorded" for a `Write` failure.
pub fn append_audit_line_bounded(
    home: &Path,
    value: &serde_json::Value,
    budget: Duration,
) -> Result<(), AppendError> {
    append_audit_line(home, value, Acquire::Bounded(budget))
}

fn append_audit_line(
    home: &Path,
    value: &serde_json::Value,
    mode: Acquire,
) -> Result<(), AppendError> {
    // (1) Serialize EVERYTHING before touching the lock: one owned buffer, newline
    // included. No formatting happens inside the locked region.
    let mut line = value.to_string();
    line.push('\n');

    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path(home))
        .map_err(AppendError::Open)?;

    // Open the target BEFORE locking so the locked region is the write and nothing
    // else. An `open` here would otherwise sit inside every writer's critical
    // section for no benefit.
    let mut target = OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path(home))
        .map_err(AppendError::Open)?;

    acquire(&lock, mode)?;

    // (2) Exactly one `write_all` while the lock is held.
    let result = target.write_all(line.as_bytes()).map_err(AppendError::Write);

    // Released by the OS on close; explicit so the critical section's end is
    // visible rather than implied by scope.
    drop(lock);
    result
}

fn acquire(lock: &File, mode: Acquire) -> Result<(), AppendError> {
    // Explicit trait syntax throughout: Rust 1.89 stabilized an inherent
    // `File::lock`/`try_lock` with these names, and at the workspace MSRV the
    // inherent method would be selected and trip the clippy MSRV gate. Same
    // reasoning as `src/store.rs` in the embedding daemon.
    match mode {
        Acquire::TryOnce => match fs4::FileExt::try_lock(lock) {
            Ok(()) => Ok(()),
            Err(fs4::TryLockError::WouldBlock) => Err(AppendError::Contended),
            Err(fs4::TryLockError::Error(e)) => Err(AppendError::Open(e)),
        },
        Acquire::Bounded(budget) => {
            let deadline = Instant::now() + budget;
            loop {
                match fs4::FileExt::try_lock(lock) {
                    Ok(()) => return Ok(()),
                    Err(fs4::TryLockError::WouldBlock) => {
                        if Instant::now() >= deadline {
                            return Err(AppendError::Contended);
                        }
                        std::thread::sleep(RETRY_INTERVAL);
                    }
                    Err(fs4::TryLockError::Error(e)) => return Err(AppendError::Open(e)),
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agentic-audit-append-{tag}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn row(i: usize) -> serde_json::Value {
        // Sized like a real record (~2 KB p50): size is what made the old
        // multi-syscall write interleave, so the fixture should not be tiny.
        serde_json::json!({ "seq": i, "pad": "x".repeat(1800) })
    }

    fn hold(home: &Path) -> File {
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path(home))
            .unwrap();
        fs4::FileExt::lock(&f).unwrap();
        f
    }

    fn rows(home: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(audit_path(home))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every row must be parseable"))
            .collect()
    }

    /// #3416 r1 RED: `acquire` maps a flock SYSCALL failure onto
    /// `AppendError::Open`, whose text says the audit log "could not be opened".
    /// By the time `acquire` runs, BOTH the lock file and the target have already
    /// been opened successfully — the failure is the lock call, not an open.
    ///
    /// The safety half of that variant's claim is still true here (nothing has been
    /// written yet), which is exactly why the bug survived the previous correction:
    /// I checked that "nothing was written" held and did not check that the stated
    /// CAUSE held. A fail-closed gate surfaces this text to an operator, so it sends
    /// them to look at permissions and disk space for a failure that was neither.
    ///
    /// Pinned on a PURE classifier rather than by forcing a real `flock` error,
    /// because there is no portable way to make the syscall fail on demand. The
    /// classifier is the mapping under test; `acquire` calls it.
    #[test]
    fn a_lock_syscall_failure_is_not_reported_as_an_open_failure() {
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err = classify_lock_error(fs4::TryLockError::Error(io));
        let msg = err.to_string();
        assert!(
            !msg.contains("could not be opened"),
            "a lock-syscall failure must not be described as an open failure: {msg}"
        );
        assert!(
            msg.contains("lock"),
            "the message must name the lock as the thing that failed: {msg}"
        );
        assert!(
            matches!(err, AppendError::Lock(_)),
            "a lock-syscall failure needs its own variant so callers can tell it \
             apart from an open failure"
        );
    }

    /// The other arm must not drift while the first is fixed: contention is still
    /// contention, and it is the arm that carries the "skip, never write unlocked"
    /// contract.
    #[test]
    fn contention_still_classifies_as_contended() {
        assert!(matches!(
            classify_lock_error(fs4::TryLockError::WouldBlock),
            AppendError::Contended
        ));
    }

    /// The delivery guarantee: the bounded path retries, so when the lock is
    /// obtainable every record lands exactly once and intact. This is where exact
    /// delivery is pinned — the best-effort path deliberately does not promise it.
    #[test]
    fn bounded_append_delivers_every_record_exactly_once() {
        let home = tmp_home("bounded-delivery");
        const N: usize = 200;
        for i in 0..N {
            append_audit_line_bounded(&home, &row(i), DEFAULT_BOUNDED_BUDGET).unwrap();
        }
        let mut seqs: Vec<u64> = rows(&home).iter().filter_map(|r| r["seq"].as_u64()).collect();
        seqs.sort_unstable();
        assert_eq!(
            seqs,
            (0..N as u64).collect::<Vec<_>>(),
            "bounded append must deliver every record exactly once"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Fail-closed: past the budget the caller gets an explicit error and NOTHING
    /// is written. An unlocked fallback here would be the whole defect returning.
    #[test]
    fn bounded_append_refuses_and_writes_nothing_past_budget() {
        let home = tmp_home("bounded-refuse");
        let holder = hold(&home);

        let started = Instant::now();
        let err = append_audit_line_bounded(&home, &row(0), Duration::from_millis(50))
            .expect_err("must refuse while the lock is held");
        let waited = started.elapsed();

        assert!(matches!(err, AppendError::Contended), "got {err:?}");
        assert!(
            rows(&home).is_empty(),
            "a refused append must leave no record behind"
        );
        // Bounded means bounded: the budget is an upper bound on the stall, with
        // generous slack so this cannot flake on a loaded machine.
        assert!(
            waited < Duration::from_secs(5),
            "bounded retry must not hang; waited {waited:?}"
        );

        drop(holder);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Best-effort: one attempt, then skip. No retry, no wait, nothing written.
    #[test]
    fn best_effort_skips_immediately_and_writes_nothing() {
        let home = tmp_home("best-effort-skip");
        let holder = hold(&home);

        let err = append_audit_line_best_effort(&home, &row(0))
            .expect_err("must skip while the lock is held");
        assert!(matches!(err, AppendError::Contended), "got {err:?}");
        assert!(
            rows(&home).is_empty(),
            "a skipped append must leave no record behind"
        );

        drop(holder);
        // Once free, the next call lands.
        append_audit_line_best_effort(&home, &row(1)).unwrap();
        assert_eq!(rows(&home).len(), 1);

        std::fs::remove_dir_all(&home).ok();
    }

    /// The serialized line is exactly one JSON object plus one newline — the
    /// property that makes the locked region a single `write_all`.
    #[test]
    fn one_append_writes_exactly_one_terminated_line() {
        let home = tmp_home("one-line");
        append_audit_line_bounded(&home, &row(7), DEFAULT_BOUNDED_BUDGET).unwrap();
        let raw = std::fs::read_to_string(audit_path(&home)).unwrap();
        assert_eq!(raw.matches('\n').count(), 1, "exactly one newline");
        assert!(raw.ends_with('\n'), "the line must be terminated");
        assert_eq!(rows(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }
}
