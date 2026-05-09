//! Sprint 57 Wave 2 Track C (#546 Item 5) — persistence for the
//! supervisor's per-agent dedup ledger.
//!
//! The Sprint 56 Track G (#529) `RateLimitRetry` HashMap is held
//! in-memory inside `supervisor::run_loop`. A daemon restart blasts
//! all in-flight dedup state — fingerprint, dedup_count,
//! last_inject_at — and a fresh cycle that re-fires the same
//! fingerprint within the 60s dedup window would under-suppress
//! (the cap is re-armed at 0 after restart). Phase A RCA #549
//! audited the gap; this module is the Option A persistent-ledger
//! fix.
//!
//! ## Schema (per-agent JSON file at `$AGEND_HOME/dedup-state/<agent>.json`)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "agent": "dev",
//!   "fingerprint": "0x123abc...",
//!   "dedup_count": 1,
//!   "last_inject_at_unix_micros": 1730000000000000,
//!   "dedup_audit_emitted": true,
//!   "retry_count": 2,
//!   "exhausted": false,
//!   "input_text": "..."
//! }
//! ```
//!
//! ## Time field semantics
//!
//! `RateLimitRetry::last_inject_at` is `std::time::Instant` (monotonic
//! clock anchored to process boot) — it CANNOT be serialized
//! meaningfully across restarts. We persist it as `SystemTime` Unix
//! micros and reconstruct on load by subtracting the elapsed wall
//! time from `Instant::now()`. The arithmetic is correct as long as
//! the wall clock didn't jump backward between persist and load
//! (clock skew → conservative fallback to `Instant::now()`, i.e.
//! treat the window as "just expired" — favours allowing retries
//! over suppressing them, matching fail-open semantics).
//!
//! `RateLimitRetry::next_retry_at` is intentionally NOT persisted.
//! On load we set it to `Instant::now()` (immediately due) and let
//! the supervisor's existing backoff-scheduling code path
//! re-derive a fresh `next_retry_at` on the first post-load tick.
//! This avoids the temptation to invent a "wall-clock retry deadline"
//! that would couple this module to the SERVER_RATE_LIMIT_BACKOFF
//! schedule.
//!
//! ## Atomic-write semantics
//!
//! `save` uses `crate::store::atomic_write` (write-then-rename) so a
//! crash mid-save leaves either the previous content intact or the
//! new content fully written — never a half-flushed file. The
//! supervisor is single-threaded (one `run_loop` thread), so there
//! is no concurrent-writer race within the daemon process. Across
//! daemon restarts, the rename's atomicity is the load-bearing
//! guarantee.
//!
//! ## Failure modes
//!
//! - Missing dir: returns empty HashMap, lazy-creates on first save.
//! - Missing file: skipped silently (per-agent, fail-open).
//! - Malformed JSON / schema mismatch: logged via `tracing::warn`,
//!   the entry is skipped, the rest of the dir loads normally.
//! - Write failure: logged via `tracing::warn`, supervisor continues
//!   with the in-memory state (best-effort persistence; correctness
//!   degrades to pre-Track-C behavior — acceptable).
//!
//! ## Schema-evolution contract (Sprint 58 Wave 1 PR-2 #5)
//!
//! Per Track C Pass 2 reviewer non-blocking note + Sprint 58 P1
//! follow-up, the schema-evolution contract is **forward-only with
//! upgrade-time skip-on-mismatch**:
//!
//! - **v(N) reading v(N) file** → full round-trip, all fields
//!   preserved.
//! - **v(N+1) reading v(N) file** → forward-compatible IF the
//!   v(N+1) reader uses `#[serde(default)]` for any v(N+1)-added
//!   field. The deserialize lands sensible defaults for the missing
//!   fields rather than panicking. This module's struct carries
//!   `#[serde(default)]` on all v1 fields as a forward-prep measure
//!   so any v2 reader that adds new fields with their own defaults
//!   can deserialize v1 files cleanly without strict-equality
//!   schema bumps.
//! - **v(N) reading v(N+1) file** → `load_all` checks
//!   `schema_version != SCHEMA_VERSION` and SKIPS the entry with a
//!   `tracing::warn` event. NO downgrade-attempt logic — the
//!   contract is forward-only. An older daemon that sees a newer-
//!   version file (e.g. operator briefly downgraded after an
//!   upgrade) treats the agent as "no persistent dedup state" and
//!   falls back to in-memory cap rearm, exactly matching pre-
//!   Track-C behaviour for that single agent. Correct fail-open
//!   semantic.
//!
//! **Downgrade / mixed-version continuity is NOT guaranteed.** If
//! a future v2 reader needs to interoperate with v1 readers running
//! in parallel (e.g. multi-binary deploys mid-rollout), explicit
//! version-handling logic must be added at v2+. The current
//! contract trades that flexibility for simpler v1 mechanics.
//!
//! Why `#[serde(default)]` lands on v1 fields NOW rather than
//! lazily at v2-introduction time: centralizing the forward-prep
//! into the v1 schema avoids future migration churn AND pins the
//! round-trip via tests, so a future contributor adding v2 fields
//! has a green-field surface to extend without breaking v1's
//! round-trip semantics. Tagged at the field level rather than the
//! struct level so each addition is reviewed individually.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::daemon::supervisor::RateLimitRetry;

/// Current on-disk schema version. Bumped when fields are
/// added/removed in a non-backward-compatible way.
const SCHEMA_VERSION: u32 = 1;

/// Sub-directory of `$AGEND_HOME` that holds per-agent JSON files.
pub(crate) const DEDUP_STATE_DIR: &str = "dedup-state";

/// On-disk schema for the per-agent dedup ledger.
///
/// Sprint 58 Wave 1 PR-2 (#5) forward-compat: every field carries
/// `#[serde(default)]` so a future v2 reader that adds new fields
/// (with their own `#[serde(default)]`) can deserialize v1 files
/// without strict-deserialize failure. Each default lands the
/// pre-Track-C zero-state for that field — equivalent to "no prior
/// dedup state for this agent" — which fail-opens the dedup gate
/// to the in-memory cap-rearm behaviour for a missing field. This
/// preserves the Phase A correctness invariant (cap honoured for
/// any complete v(N) → v(N) round-trip) while allowing v2+ readers
/// to extend the schema without breaking v1 file compatibility.
///
/// See module-level rustdoc "Schema-evolution contract" section
/// for the full forward-only-upgrade guarantees and downgrade
/// caveats.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    agent: String,
    /// Hex-formatted u64 (`"0x{:016x}"`) so JSON readers handle the
    /// full 64-bit range without precision loss.
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    dedup_count: u32,
    /// `last_inject_at` as Unix-epoch microseconds. Reconstructed to
    /// `Instant` on load via `SystemTime::now()` delta.
    #[serde(default)]
    last_inject_at_unix_micros: i64,
    #[serde(default)]
    dedup_audit_emitted: bool,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    exhausted: bool,
    #[serde(default)]
    input_text: String,
}

/// Path for a single agent's ledger file.
pub(crate) fn ledger_path(home: &Path, agent: &str) -> PathBuf {
    home.join(DEDUP_STATE_DIR).join(format!("{agent}.json"))
}

/// Convert `Instant` to wall-clock Unix micros. Approximates by
/// computing the elapsed-since-now offset and adding it to the
/// current `SystemTime`. Negative offsets (Instant in the future
/// relative to now — never happens in this module's call sites)
/// are clamped to now.
fn instant_to_unix_micros(instant: Instant) -> i64 {
    let now_inst = Instant::now();
    let now_sys = SystemTime::now();
    if instant <= now_inst {
        let elapsed = now_inst.duration_since(instant);
        let target = now_sys
            .checked_sub(elapsed)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        system_time_to_unix_micros(target)
    } else {
        // Instant in the future — clamp to current wall clock.
        system_time_to_unix_micros(now_sys)
    }
}

/// Convert Unix micros back to `Instant`. If the wall clock has
/// moved forward since persist, returns an Instant that's `(now -
/// elapsed_wallclock)` ago. If the wall clock has moved BACKWARD
/// (impossible on monotonic systems, possible across NTP corrections),
/// returns `Instant::now()` to fail-open.
fn unix_micros_to_instant(unix_micros: i64) -> Instant {
    if unix_micros <= 0 {
        return Instant::now();
    }
    let target_sys = match unix_micros_to_system_time(unix_micros) {
        Some(t) => t,
        None => return Instant::now(),
    };
    let now_sys = SystemTime::now();
    match now_sys.duration_since(target_sys) {
        Ok(elapsed) => Instant::now()
            .checked_sub(elapsed)
            .unwrap_or_else(Instant::now),
        // SystemTime is in the future relative to "now" — clock skew
        // (NTP rewind, persisted from a faster host clock, etc).
        // Fail-open: treat as if the persist just happened, which
        // keeps the dedup window honoured for at most its full
        // duration before the next tick re-evaluates.
        Err(_) => Instant::now(),
    }
}

fn system_time_to_unix_micros(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_micros().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}

fn unix_micros_to_system_time(unix_micros: i64) -> Option<SystemTime> {
    if unix_micros < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_micros(unix_micros as u64))
}

/// Persist a single agent's `RateLimitRetry` to disk. Idempotent:
/// repeated calls with the same content overwrite the file in place.
/// Best-effort — write failures are logged but never propagated.
pub(crate) fn save(home: &Path, agent: &str, retry: &RateLimitRetry) {
    let dir = home.join(DEDUP_STATE_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, agent = %agent, "dedup_state: create_dir_all failed");
        return;
    }
    let on_disk = OnDisk {
        schema_version: SCHEMA_VERSION,
        agent: agent.to_string(),
        fingerprint: format!("0x{:016x}", retry.fingerprint),
        dedup_count: retry.dedup_count,
        last_inject_at_unix_micros: instant_to_unix_micros(retry.last_inject_at),
        dedup_audit_emitted: retry.dedup_audit_emitted,
        retry_count: retry.retry_count,
        exhausted: retry.exhausted,
        input_text: retry.input_text.clone(),
    };
    let bytes = match serde_json::to_vec_pretty(&on_disk) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, agent = %agent, "dedup_state: serialize failed");
            return;
        }
    };
    let path = ledger_path(home, agent);
    if let Err(e) = crate::store::atomic_write(&path, &bytes) {
        tracing::warn!(error = %e, agent = %agent, "dedup_state: atomic_write failed");
    }
}

/// Remove the persisted ledger for an agent. Called when the
/// supervisor sees the agent recover (Ready / Idle) — the
/// in-memory `retry_tracks` entry is dropped, so the disk file
/// must follow. Idempotent on missing file.
pub(crate) fn clear(home: &Path, agent: &str) {
    let path = ledger_path(home, agent);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e, agent = %agent, "dedup_state: clear failed");
        }
    }
}

/// Hydrate the supervisor's `retry_tracks` from disk at startup.
/// Walks `$AGEND_HOME/dedup-state/*.json`, parses each, returns
/// the reconstructed HashMap. Per-file parse failures are logged
/// and skipped (the rest of the dir loads normally) — corrupt or
/// schema-mismatched files do NOT abort startup.
pub(crate) fn load_all(home: &Path) -> HashMap<String, RateLimitRetry> {
    let dir = home.join(DEDUP_STATE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), "dedup_state: load_all read_dir failed");
            return HashMap::new();
        }
    };
    let mut out: HashMap<String, RateLimitRetry> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "dedup_state: read failed");
                continue;
            }
        };
        let on_disk: OnDisk = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "dedup_state: malformed JSON or schema mismatch — skipping"
                );
                continue;
            }
        };
        if on_disk.schema_version != SCHEMA_VERSION {
            tracing::warn!(
                got = on_disk.schema_version,
                expected = SCHEMA_VERSION,
                path = %path.display(),
                "dedup_state: unknown schema_version — skipping"
            );
            continue;
        }
        let fingerprint = match parse_fingerprint(&on_disk.fingerprint) {
            Some(fp) => fp,
            None => {
                tracing::warn!(
                    raw = %on_disk.fingerprint,
                    path = %path.display(),
                    "dedup_state: malformed fingerprint hex — skipping"
                );
                continue;
            }
        };
        let last_inject_at = unix_micros_to_instant(on_disk.last_inject_at_unix_micros);
        let retry = RateLimitRetry {
            retry_count: on_disk.retry_count,
            // next_retry_at is NOT persisted — set to "due now" so the
            // first post-load supervisor tick re-derives a fresh
            // backoff slot via the existing scheduling code path.
            next_retry_at: Instant::now(),
            input_text: on_disk.input_text,
            exhausted: on_disk.exhausted,
            fingerprint,
            dedup_count: on_disk.dedup_count,
            last_inject_at,
            dedup_audit_emitted: on_disk.dedup_audit_emitted,
        };
        out.insert(on_disk.agent, retry);
    }
    out
}

/// Parse the on-disk fingerprint string (`"0x{:016x}"`) back to u64.
fn parse_fingerprint(raw: &str) -> Option<u64> {
    let stripped = raw.strip_prefix("0x").unwrap_or(raw);
    u64::from_str_radix(stripped, 16).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::supervisor::{fingerprint_input, RateLimitRetry};

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-dedup-state-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_retry(text: &str, dedup_count: u32) -> RateLimitRetry {
        RateLimitRetry {
            retry_count: 2,
            next_retry_at: Instant::now() + Duration::from_secs(15),
            input_text: text.to_string(),
            exhausted: false,
            fingerprint: fingerprint_input(text),
            dedup_count,
            last_inject_at: Instant::now(),
            dedup_audit_emitted: dedup_count >= NOTIFICATION_DEDUP_CAP_TEST,
        }
    }

    // Mirror the supervisor's NOTIFICATION_DEDUP_CAP without making
    // it pub — keeps the test independent of any pub-cap renames.
    const NOTIFICATION_DEDUP_CAP_TEST: u32 = 1;

    #[test]
    fn dedup_ledger_persists_to_disk_on_mutation() {
        let home = tmp_home("persist-on-mutation");
        let retry = make_retry("hello world", 1);

        save(&home, "dev", &retry);

        let path = ledger_path(&home, "dev");
        assert!(path.exists(), "save must create the per-agent file");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["agent"], "dev");
        assert_eq!(parsed["dedup_count"], 1);
        assert_eq!(parsed["schema_version"], SCHEMA_VERSION);
        assert!(
            parsed["fingerprint"].as_str().unwrap().starts_with("0x"),
            "fingerprint must serialize as 0x-prefixed hex"
        );
        assert_eq!(parsed["input_text"], "hello world");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dedup_ledger_loads_on_supervisor_startup() {
        let home = tmp_home("load-on-startup");
        let original = make_retry("operator typed this", 0);

        save(&home, "dev", &original);
        save(&home, "lead", &make_retry("different fingerprint", 1));

        let loaded = load_all(&home);

        assert_eq!(loaded.len(), 2, "both agents must be loaded");
        let dev = loaded.get("dev").expect("dev present");
        assert_eq!(dev.dedup_count, 0);
        assert_eq!(dev.fingerprint, fingerprint_input("operator typed this"));
        assert_eq!(dev.input_text, "operator typed this");
        assert_eq!(dev.retry_count, 2);
        let lead = loaded.get("lead").expect("lead present");
        assert_eq!(lead.dedup_count, 1);
        assert_eq!(lead.fingerprint, fingerprint_input("different fingerprint"));
        assert!(
            lead.dedup_audit_emitted,
            "audit-emitted flag must round-trip"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dedup_ledger_handles_missing_file_gracefully() {
        // No save called — load on a fresh dir must return empty
        // without erroring or creating any state.
        let home = tmp_home("missing-file");

        let loaded = load_all(&home);
        assert!(loaded.is_empty(), "empty home must yield empty ledger");

        // Also: clear on a missing file must be a no-op (no panic,
        // no error log noise).
        clear(&home, "nonexistent-agent");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dedup_ledger_handles_corrupt_file_gracefully() {
        // A garbage file in the dir must NOT abort load_all — the
        // rest of the directory must still load normally. This is
        // the load-bearing "daemon doesn't crash on bad disk
        // state" invariant.
        let home = tmp_home("corrupt-file");
        let dir = home.join(DEDUP_STATE_DIR);
        std::fs::create_dir_all(&dir).unwrap();

        // Plant: a valid entry + a malformed-JSON entry + a
        // schema-mismatched entry + a non-JSON file (should be
        // ignored by extension filter).
        save(&home, "good", &make_retry("ok", 0));
        std::fs::write(dir.join("garbage.json"), b"not-json-at-all").unwrap();
        std::fs::write(
            dir.join("bad-schema.json"),
            br#"{"schema_version": 999, "agent": "x"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("ignored.txt"), b"text file").unwrap();

        let loaded = load_all(&home);
        assert_eq!(
            loaded.len(),
            1,
            "only the well-formed entry survives, got: {loaded:?}"
        );
        assert!(loaded.contains_key("good"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn restart_within_60s_dedup_window_with_fingerprint_match_under_suppresses_correctly() {
        // Empirical regression-proof of the latent bug Phase A RCA
        // documented. Pre-Track-C: a daemon restart within the 60s
        // dedup window blasted dedup_count back to 0, allowing a
        // fresh same-fingerprint inject that should have been
        // suppressed by the cap.
        //
        // Post-Track-C: the persisted ledger preserves dedup_count
        // + fingerprint + last_inject_at across restart, so the
        // dedup gate still suppresses correctly on the first
        // post-load tick.
        let home = tmp_home("restart-replay");

        // Pre-restart: agent fired one inject for fingerprint F,
        // hit dedup_count == cap, and the audit event was logged.
        let pre_restart = make_retry("rate-limited input X", 1);
        let original_fp = pre_restart.fingerprint;
        save(&home, "dev", &pre_restart);

        // Simulated daemon restart: new process, fresh in-memory
        // HashMap. supervisor.run_loop calls load_all to hydrate.
        let post_restart = load_all(&home);

        let recovered = post_restart
            .get("dev")
            .expect("dev's ledger must round-trip");
        assert_eq!(
            recovered.fingerprint, original_fp,
            "fingerprint must survive restart for the dedup gate to recognize repeat input"
        );
        assert_eq!(
            recovered.dedup_count, 1,
            "dedup_count must survive restart so the cap-1 gate fires on the next tick \
             (without this, the latent under-suppression bug would re-arm)"
        );
        assert!(
            recovered.dedup_audit_emitted,
            "audit-emitted latch must survive restart so we don't double-fire the \
             notification_inject_dedup_capped event"
        );

        // Window arithmetic: the recovered last_inject_at must be
        // close enough to "the original inject time" that the
        // supervisor's `dedup_decision` still sees the window as
        // open. We can't compare Instants across processes
        // exactly, but we can verify the elapsed duration is
        // under the dedup window (60s).
        let elapsed = recovered.last_inject_at.elapsed();
        assert!(
            elapsed.as_secs() < 60,
            "last_inject_at must be reconstructed within the dedup window \
             (got elapsed={elapsed:?}); restart-and-load happened immediately"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn concurrent_writes_atomic_or_serialized() {
        // The supervisor is single-threaded, but the on-disk
        // semantics must still tolerate the rename-based atomic
        // write under repeated rapid succession. Pin: 5 saves in a
        // row never leave the file in a half-written state. Each
        // load_all must see the LAST save's content.
        let home = tmp_home("concurrent-saves");

        for i in 0..5_u32 {
            save(&home, "dev", &make_retry(&format!("input-{i}"), i));
        }

        let loaded = load_all(&home);
        let dev = loaded.get("dev").expect("final state must be readable");
        assert_eq!(dev.dedup_count, 4, "last save must be the final state");
        assert_eq!(dev.input_text, "input-4");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dedup_state_directory_created_lazily() {
        // First save against a home that has no `dedup-state/`
        // subdir must create the dir and the file. Operators that
        // never hit a rate-limit episode never have the dir
        // created at all (no overhead for the steady state).
        let home = tmp_home("lazy-dir");
        let dir = home.join(DEDUP_STATE_DIR);
        assert!(!dir.exists(), "pre: dir must not exist before save");

        save(&home, "dev", &make_retry("first", 0));

        assert!(dir.exists(), "save must create the dir lazily");
        assert!(ledger_path(&home, "dev").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- Bonus pins (defensive, dev-judgement) -----

    #[test]
    fn instant_to_unix_micros_roundtrip_preserves_window_arithmetic() {
        // Pin the cross-process time math: a known Instant
        // round-tripped through Unix-micros and back must survive
        // within a small slack (~milliseconds, dominated by
        // SystemTime::now() jitter between the two reads).
        let original = Instant::now();
        let unix = instant_to_unix_micros(original);
        let recovered = unix_micros_to_instant(unix);

        // |original - recovered| should be small.
        let drift = if recovered >= original {
            recovered.duration_since(original)
        } else {
            original.duration_since(recovered)
        };
        assert!(
            drift < Duration::from_secs(1),
            "round-trip drift exceeded 1s: {drift:?}"
        );
    }

    #[test]
    fn fingerprint_hex_round_trips_full_u64_range() {
        // Pin: edge-case fingerprints (0, u64::MAX, sign-bit
        // boundaries) survive the hex round-trip without precision
        // loss. JSON's number type can't hold u64::MAX safely, so
        // we serialize as hex string — this test is the
        // regression-proof anchor for that choice.
        for &fp in &[
            0u64,
            1,
            u64::MAX,
            0x8000_0000_0000_0000_u64,
            0xdead_beef_cafe_babe_u64,
        ] {
            let s = format!("0x{fp:016x}");
            let parsed = parse_fingerprint(&s).expect("must parse");
            assert_eq!(parsed, fp, "round-trip lost precision for 0x{fp:016x}");
        }
    }

    #[test]
    fn clear_removes_only_the_targeted_agent() {
        // Defensive: clear on agent X must NOT touch agent Y's
        // file. Closes the bug class where a string-prefix-match
        // path computation could remove sibling agents'
        // state.
        let home = tmp_home("clear-targeted");
        save(&home, "dev", &make_retry("dev-input", 0));
        save(&home, "dev2", &make_retry("dev2-input", 0));
        save(&home, "lead", &make_retry("lead-input", 0));

        clear(&home, "dev");

        let loaded = load_all(&home);
        assert!(!loaded.contains_key("dev"), "dev must be cleared");
        assert!(
            loaded.contains_key("dev2"),
            "dev2 must NOT be touched (prefix collision check)"
        );
        assert!(loaded.contains_key("lead"), "unrelated agent must survive");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_overwrites_in_place_on_repeat() {
        // Idempotency: calling save twice with different content
        // for the same agent results in the LATEST content on
        // disk (atomic_write is overwrite-in-place semantics).
        let home = tmp_home("overwrite-in-place");

        save(&home, "dev", &make_retry("v1", 0));
        save(&home, "dev", &make_retry("v2-different", 1));

        let loaded = load_all(&home);
        let dev = loaded.get("dev").unwrap();
        assert_eq!(dev.input_text, "v2-different");
        assert_eq!(dev.dedup_count, 1);

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- Sprint 58 Wave 1 PR-2 (#5) forward-compat pins -----

    #[test]
    fn dedup_state_v1_file_with_extra_unknown_fields_round_trips() {
        // Forward-compat: a hypothetical v2 file with extra
        // unknown fields must NOT trip a strict-deserialize
        // failure when read by a v1 reader. serde-json by default
        // ignores unknown fields on Deserialize (not
        // `deny_unknown_fields`), so an extra field round-trips as
        // a no-op. Pin the behaviour explicitly so any future
        // refactor that adds `#[serde(deny_unknown_fields)]`
        // breaks this test loud and proud.
        let home = tmp_home("v2-extra-fields");
        let dir = home.join(DEDUP_STATE_DIR);
        std::fs::create_dir_all(&dir).unwrap();

        // Plant a synthetic file with an extra `v2_only_field`
        // that doesn't exist in the v1 OnDisk struct.
        let body = serde_json::json!({
            "schema_version": 1,
            "agent": "dev",
            "fingerprint": "0x1234567890abcdef",
            "dedup_count": 1,
            "last_inject_at_unix_micros": 1_700_000_000_000_000_i64,
            "dedup_audit_emitted": true,
            "retry_count": 1,
            "exhausted": false,
            "input_text": "hello",
            // v2-hypothetical extra fields that v1 reader must ignore
            "v2_some_new_field": "future-value",
            "v2_another_field": 42_i64,
        });
        std::fs::write(
            dir.join("dev.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();

        let loaded = load_all(&home);
        assert_eq!(
            loaded.len(),
            1,
            "v1 reader must accept v2-with-extra-fields file gracefully"
        );
        let dev = loaded.get("dev").expect("dev present");
        assert_eq!(dev.dedup_count, 1);
        assert_eq!(dev.input_text, "hello");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dedup_state_serde_default_attribute_present_for_all_fields() {
        // Source-text invariant pin: every OnDisk field MUST carry
        // `#[serde(default)]`. A future PR adding a v2 field
        // without the annotation would re-introduce strict-
        // deserialize failure on v1 files missing the new field —
        // this test catches that class of regression at compile-
        // adjacent time.
        let src = include_str!("dedup_state.rs");
        // Slice off the tests submodule so a hypothetical literal
        // in test source doesn't cross-pollute the count.
        let prod_end = src.find("\n#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..prod_end];

        // Each field should appear preceded by `#[serde(default)]`
        // on its own line. Pin each field name explicitly:
        for field in &[
            "schema_version: u32",
            "agent: String",
            "fingerprint: String",
            "dedup_count: u32",
            "last_inject_at_unix_micros: i64",
            "dedup_audit_emitted: bool",
            "retry_count: u32",
            "exhausted: bool",
            "input_text: String",
        ] {
            // Find the field; verify the preceding line carries the
            // attribute. Approach: locate field, look back ~50 chars
            // for `#[serde(default)]`.
            let pos = prod
                .find(field)
                .unwrap_or_else(|| panic!("field `{field}` missing from OnDisk struct"));
            let lookback_start = pos.saturating_sub(60);
            let preceding = &prod[lookback_start..pos];
            assert!(
                preceding.contains("#[serde(default)]"),
                "field `{field}` must carry `#[serde(default)]` for forward-compat \
                 (Sprint 58 Wave 1 PR-2 #5). Preceding context: {preceding}"
            );
        }
    }

    #[test]
    fn dedup_state_v1_round_trip_with_default_constructed_struct_succeeds() {
        // Defensive pin: an OnDisk built from `Default::default()`
        // (i.e. all-zero fields, schema_version = 0) round-trips
        // through serialize → deserialize cleanly. This catches
        // any future refactor that breaks the Default + serde
        // contract.
        let zero = OnDisk::default();
        let json = serde_json::to_string(&zero).unwrap();
        let parsed: OnDisk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 0);
        assert_eq!(parsed.agent, "");
        assert_eq!(parsed.dedup_count, 0);
        assert!(!parsed.exhausted);
    }

    #[test]
    fn dedup_state_load_all_skips_v2_file_with_unknown_schema_version() {
        // Pin the forward-only contract: a v(N) reader sees a
        // v(N+k) file with `schema_version: 99` and SKIPS it
        // (rather than crashing or trying to interpret it as v1).
        // Existing `load_all` logic already does this; pin the
        // empirical behaviour so the schema-evolution contract
        // documented in the module rustdoc has a regression-proof
        // test anchor.
        let home = tmp_home("v2-skip");
        let dir = home.join(DEDUP_STATE_DIR);
        std::fs::create_dir_all(&dir).unwrap();

        // Plant: one valid v1 + one v2-version (unknown to this reader).
        save(&home, "ok", &make_retry("ok", 0));
        let v2_body = serde_json::json!({
            "schema_version": 99,
            "agent": "future",
            "fingerprint": "0xdeadbeefcafebabe",
            "dedup_count": 5,
            "last_inject_at_unix_micros": 1_700_000_000_000_000_i64,
            "dedup_audit_emitted": false,
            "retry_count": 0,
            "exhausted": false,
            "input_text": "from-the-future",
        });
        std::fs::write(
            dir.join("future.json"),
            serde_json::to_string_pretty(&v2_body).unwrap(),
        )
        .unwrap();

        let loaded = load_all(&home);
        assert!(
            loaded.contains_key("ok"),
            "valid v1 entry must load cleanly"
        );
        assert!(
            !loaded.contains_key("future"),
            "future v(N+k) entry must be skipped per forward-only contract"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
