//! #2755 real-entry RED: `repo action=checkout` must recursively initialize
//! submodules. The MCP entry `handle_checkout_repo` runs `git worktree add`
//! but (pre-fix) skips `submodule update --init --recursive`, so a
//! path-dependency submodule (e.g. `vendor/agentic-git`) is left EMPTY on the
//! provisioned worktree — the build then fails on missing nested content.
//!
//! Fixtures mirror `src/worktree/tests.rs::tmp_super_with_nested_submodules`
//! (that module's helpers are private); a two-level super→A→B nest pins that
//! the fix inits submodules RECURSIVELY, not just one level.

// `#[cfg(unix)]`: `json!` is used only by the real-checkout tests below, which are
// Unix-only (absolute-source contract — see the first such test). Windows would
// otherwise warn `unused_imports` under strict clippy.
#[cfg(unix)]
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Run git with `AGEND_GIT_BYPASS` (skip the shim); panic on non-zero. When
/// `allow_file`, set `protocol.file.allow=always` so local-path submodule
/// fixtures clone (git's submodule helper ignores repo-stored config).
///
/// `#[cfg(unix)]`: consumed only by the Unix-only real-checkout fixtures/tests
/// below; gating avoids a `dead_code` warning on Windows under strict clippy.
#[cfg(unix)]
fn git_run_ok(dir: &Path, args: &[&str], allow_file: bool) {
    let mut cmd = std::process::Command::new("git");
    cmd.env("AGEND_GIT_BYPASS", "1").current_dir(dir);
    if allow_file {
        cmd.args(["-c", "protocol.file.allow=always"]);
    }
    cmd.args(args);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed local repo with one file at `rel` (the innermost submodule).
#[cfg(unix)] // Unix-only real-checkout fixture (see `git_run_ok`).
fn tmp_repo_with_file(name: &str, rel: &str, body: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-co-subfix-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    git_run_ok(&dir, &["init", "-b", "main"], false);
    git_run_ok(&dir, &["config", "user.email", "test@test"], false);
    git_run_ok(&dir, &["config", "user.name", "test"], false);
    if let Some(parent) = Path::new(rel).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(dir.join(parent)).unwrap();
        }
    }
    std::fs::write(dir.join(rel), body).unwrap();
    git_run_ok(&dir, &["add", rel], false);
    git_run_ok(&dir, &["commit", "-m", "init"], false);
    dir
}

/// Hermetic superproject with two submodule levels:
///   super → `vendor/mid` (A) → `nested` (B, holds `nested_b.txt`).
#[cfg(unix)] // Unix-only real-checkout fixture (see `git_run_ok`).
fn tmp_super_with_nested_submodules(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agend-co-nest-root-{}-{}-{}",
        std::process::id(),
        name,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();

    // Level B (innermost)
    let b = tmp_repo_with_file(&format!("{name}-b"), "nested_b.txt", "level-b-payload\n");

    // Level A: depends on B at nested/
    let a = {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("a-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        git_run_ok(
            &dir,
            &["submodule", "add", &b.display().to_string(), "nested"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "A with nested B"], false);
        dir
    };

    // Super: depends on A at vendor/mid/
    let super_repo = {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("super-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        git_run_ok(
            &dir,
            &["submodule", "add", &a.display().to_string(), "vendor/mid"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "super with A->B nest"], false);
        dir
    };

    let _ = (b, a);
    super_repo
}

fn tmp_home(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-co-home-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// #2755 RED: a real `repo action=checkout` of a repo with (nested) submodules
/// must leave the submodule CONTENT materialized in the provisioned worktree.
/// Pre-fix the handler runs `git worktree add` and skips `--init --recursive`,
/// so the level-B file is missing and the assert fails.
///
/// `#[cfg(unix)]`: drives the real entry with an ABSOLUTE temp `repository_path`.
/// The #2158 source guard's absolute arm is `/`-prefixed (Unix-only by design —
/// a `C:\`-rooted Windows path routes to the agent-name arm and fails closed with
/// `ambiguous_source_path`). Same Unix-only contract as
/// `source_resolve.rs::absolute_existing_path_still_resolves_2158`.
#[cfg(unix)]
#[test]
fn checkout_initializes_nested_submodules_2755() {
    let home = tmp_home("co-submod");
    let super_repo = tmp_super_with_nested_submodules("co-submod");
    assert!(
        super_repo.join(".gitmodules").is_file(),
        "fixture: super must have .gitmodules"
    );

    // Real MCP entry. bind:false is the minimal materialization path (no
    // lease/bind_full/signing confounds); the `git worktree add` it runs is the
    // exact site that skips submodule init.
    let args = json!({
        "repository_path": super_repo.display().to_string(),
        "branch": "main",
        "bind": false,
    });
    let resp = super::checkout::handle_checkout_repo(&home, &args, "agent-co");
    assert!(resp.get("error").is_none(), "checkout errored: {resp}");
    let wt = PathBuf::from(
        resp["path"]
            .as_str()
            .unwrap_or_else(|| panic!("checkout must return a path: {resp}")),
    );

    // Decisive pin: the level-B file exists inside the provisioned worktree.
    // `git worktree add` alone leaves vendor/mid (and its nested/) empty.
    let nested_b = wt.join("vendor/mid/nested/nested_b.txt");
    assert!(
        nested_b.is_file(),
        "#2755: repo checkout must recursively init submodules so {} exists; \
         `git worktree add` alone leaves submodule dirs empty",
        nested_b.display()
    );
    // Windows git may rewrite LF→CRLF on checkout; pin payload only.
    let body = std::fs::read_to_string(&nested_b).unwrap();
    assert_eq!(
        body.trim_end_matches(['\r', '\n']),
        "level-b-payload",
        "nested submodule payload must match regardless of CRLF vs LF"
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

// ─── #2755 transaction journal (pure state machine — no live git) ────────────
// These pin the durable-transaction invariants from d-20260713024125724636-10
// deterministically (fixed clock, temp home), independent of `git worktree add`
// — the vendored shim's agent-ancestry stray-worktree guard flakes live
// concurrent worktree adds under an agent process tree (CI-safe, but not a
// reliable seam for unit tests).

use super::checkout_txn::{backoff_secs, Journal, Phase, INTERVENTION_CEILING_SECS};

fn fixed_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-07-13T00:00:00+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn sample_journal() -> Journal {
    Journal::prepared(
        "nonce-abc",
        "/wt/agent-src",
        "/src",
        "feat/x",
        true,
        fixed_now().to_rfc3339(),
    )
}

/// Phases advance monotonically; only Prepared has no on-disk worktree.
#[test]
fn txn_phase_order_and_worktree_existence() {
    let order = [
        Phase::Prepared,
        Phase::WorktreeAdded,
        Phase::MarkerDurable,
        Phase::SubmodulesReady,
        Phase::Committed,
    ];
    for w in order.windows(2) {
        assert!(w[0].rank() < w[1].rank(), "{:?} < {:?}", w[0], w[1]);
    }
}

/// save → load round-trips durably (via store::atomic_write).
#[test]
fn txn_journal_persists_and_loads() {
    let home = tmp_home("txn-persist");
    let mangled = "agent-co-_src";
    let mut j = sample_journal();
    j.advance(Phase::WorktreeAdded);
    j.save(&home, mangled).expect("save");
    let loaded = Journal::load(&home, mangled).expect("load");
    assert_eq!(loaded.nonce, "nonce-abc");
    assert_eq!(loaded.phase, Phase::WorktreeAdded);
    assert_eq!(loaded.schema_version, 1);
    Journal::clear(&home, mangled);
    assert!(
        Journal::load(&home, mangled).is_none(),
        "clear removes journal"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A minimal on-disk record (pre-rollback fields absent) loads with defaults —
/// forward/back compat via serde(default).
#[test]
fn txn_journal_serde_back_compat_defaults() {
    let home = tmp_home("txn-compat");
    let mangled = "agent-co-_src";
    let path = super::checkout_txn::journal_path(&home, mangled);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // No rollback_pending / attempts / next_attempt_at / intervention keys.
    let minimal = r#"{"schema_version":1,"nonce":"n","phase":"submodules_ready",
        "worktree_path":"/wt","source_repo":"/src","branch":"b","bind":false,
        "created_at":"2026-07-13T00:00:00+00:00"}"#;
    std::fs::write(&path, minimal).unwrap();
    let j = Journal::load(&home, mangled).expect("loads minimal record");
    assert_eq!(j.phase, Phase::SubmodulesReady);
    assert!(!j.rollback_pending);
    assert_eq!(j.attempts, 0);
    assert!(j.next_attempt_at.is_none());
    assert!(!j.intervention);
    std::fs::remove_dir_all(&home).ok();
}

/// backoff doubles per attempt and is capped at the intervention ceiling.
#[test]
fn txn_backoff_doubles_then_caps_at_ceiling() {
    assert_eq!(backoff_secs(0), 1);
    assert_eq!(backoff_secs(1), 2);
    assert_eq!(backoff_secs(4), 16);
    assert_eq!(backoff_secs(8), 256);
    assert_eq!(
        backoff_secs(9),
        INTERVENTION_CEILING_SECS,
        "2^9=512 capped to 300"
    );
    assert_eq!(
        backoff_secs(40),
        INTERVENTION_CEILING_SECS,
        "huge attempts stay capped, no overflow"
    );
}

/// arm_rollback sets pending + a future deadline, increments attempts, and flips
/// intervention once the backoff reaches the ceiling; retained intent persists.
#[test]
fn txn_arm_rollback_backoff_and_intervention() {
    let now = fixed_now();
    let mut j = sample_journal();
    j.advance(Phase::SubmodulesReady);

    j.arm_rollback(now);
    assert!(j.rollback_pending, "rollback owed");
    assert_eq!(j.attempts, 1);
    assert!(!j.intervention, "first failure is under the ceiling");
    // Deadline is now + backoff(0) = 1s.
    let deadline = chrono::DateTime::parse_from_rfc3339(j.next_attempt_at.as_deref().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(
        deadline,
        now + chrono::Duration::seconds(1),
        "backoff(0)=1s deadline"
    );

    // Drive attempts up to the ceiling → intervention, still retrying.
    for _ in 0..9 {
        j.arm_rollback(now);
    }
    assert!(
        j.intervention,
        "backoff reached the 300s ceiling ⇒ operator intervention"
    );
    assert!(
        j.rollback_pending,
        "intervention keeps retrying, never abandons"
    );
}

/// A stale journal (different nonce) is distinguishable from the in-flight attempt.
#[test]
fn txn_nonce_distinguishes_attempts() {
    let a = Journal::prepared(
        "nonce-1",
        "/wt",
        "/src",
        "b",
        false,
        fixed_now().to_rfc3339(),
    );
    let b = Journal::prepared(
        "nonce-2",
        "/wt",
        "/src",
        "b",
        false,
        fixed_now().to_rfc3339(),
    );
    assert_ne!(a.nonce, b.nonce);
}

// ─── recover_stale / rollback_failed (injected remove/unbind — no live git) ──

use super::checkout_txn::{recover_stale, rollback_failed, RollbackOutcome};

/// Save a journal at `phase` whose `worktree_path` is `wt`, optionally creating a
/// real `wt` dir on disk (recovery now decides remove-vs-clear by on-disk
/// existence, not the phase alone).
fn save_journal_at(home: &std::path::Path, mangled: &str, wt: &Path, phase: Phase, real: bool) {
    if real {
        std::fs::create_dir_all(wt).unwrap();
    }
    let mut j = Journal::prepared(
        "nonce-x",
        wt.display().to_string(),
        "/src",
        "b",
        false,
        fixed_now().to_rfc3339(),
    );
    for p in [
        Phase::WorktreeAdded,
        Phase::MarkerDurable,
        Phase::SubmodulesReady,
        Phase::Committed,
    ] {
        if p.rank() <= phase.rank() {
            j.advance(p);
        }
    }
    j.save(home, mangled).unwrap();
}

fn write_corrupt_journal(home: &Path, mangled: &str) {
    let jp = super::checkout_txn::journal_path(home, mangled);
    std::fs::create_dir_all(jp.parent().unwrap()).unwrap();
    std::fs::write(&jp, b"{ not valid json").unwrap();
}

/// A Committed tombstone → cleared, NO worktree removal (the provision completed;
/// its worktree is valid for reuse).
#[test]
fn txn_recover_committed_clears_without_remove() {
    let home = tmp_home("rec-committed");
    let wt = home.join("wt");
    save_journal_at(&home, "m", &wt, Phase::Committed, true);
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(!removed.get(), "Committed ⇒ no worktree removal");
    assert!(Journal::load(&home, "m").is_none(), "tombstone cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// A crashed non-Committed attempt whose REAL worktree removes → cleared.
#[test]
fn txn_recover_inflight_remove_ok_clears() {
    let home = tmp_home("rec-ok");
    let wt = home.join("wt");
    save_journal_at(&home, "m", &wt, Phase::WorktreeAdded, true);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || true);
    assert!(r.is_ok());
    assert!(Journal::load(&home, "m").is_none(), "removed ⇒ cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// A crashed attempt whose REAL worktree CANNOT be removed retains intent and Errs.
#[test]
fn txn_recover_inflight_remove_fail_retains_intent() {
    let home = tmp_home("rec-fail");
    let wt = home.join("wt");
    save_journal_at(&home, "m", &wt, Phase::SubmodulesReady, true);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || false);
    assert!(r.is_err(), "remove failed ⇒ Err");
    let retained = Journal::load(&home, "m").expect("journal retained");
    assert!(retained.rollback_pending && retained.next_attempt_at.is_some());
    std::fs::remove_dir_all(&home).ok();
}

/// A non-Committed journal with NO worktree on disk (crashed before/without add) →
/// cleared, no remove.
#[test]
fn txn_recover_no_worktree_clears_without_remove() {
    let home = tmp_home("rec-nowt");
    let wt = home.join("wt"); // NOT created
    save_journal_at(&home, "m", &wt, Phase::WorktreeAdded, false);
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(!removed.get(), "no worktree on disk ⇒ no remove");
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// Prepared-with-REAL-worktree (crash after `git worktree add`, before the
/// WorktreeAdded save): the on-disk worktree is removed even though phase=Prepared.
#[test]
fn txn_recover_prepared_with_real_worktree_removes() {
    let home = tmp_home("rec-prep-wt");
    let wt = home.join("wt");
    save_journal_at(&home, "m", &wt, Phase::Prepared, true);
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(
        removed.get(),
        "Prepared-with-real-worktree ambiguity ⇒ removed"
    );
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// A CORRUPT (torn) journal + a real worktree → the worktree is removed and the
/// torn record cleared (treated as a crashed attempt).
#[test]
fn txn_recover_corrupt_removes_worktree() {
    let home = tmp_home("rec-corrupt");
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    write_corrupt_journal(&home, "m");
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", &wt, fixed_now(), || {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(removed.get(), "corrupt + real worktree ⇒ removed");
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// rollback_failed: worktree removed ⇒ unbind runs and the journal is cleared.
#[test]
fn txn_rollback_failed_remove_ok_unbinds_and_clears() {
    let home = tmp_home("rb-ok");
    let mut j = sample_journal();
    j.advance(Phase::SubmodulesReady);
    j.save(&home, "m").unwrap();
    let unbound = std::cell::Cell::new(false);
    let outcome = rollback_failed(
        &home,
        "m",
        &mut j,
        fixed_now(),
        || true,
        || unbound.set(true),
    );
    assert_eq!(outcome, RollbackOutcome::Removed, "remove ok ⇒ Removed");
    assert!(unbound.get(), "unbind runs");
    assert!(Journal::load(&home, "m").is_none(), "removed ⇒ cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// rollback_failed: worktree NOT removed ⇒ journal retained (armed) for recovery.
#[test]
fn txn_rollback_failed_remove_fail_retains() {
    let home = tmp_home("rb-fail");
    let mut j = sample_journal();
    j.advance(Phase::SubmodulesReady);
    j.save(&home, "m").unwrap();
    // #2755 R3: a failed remove (Windows open-handle / transient FS) ⇒ RollbackPending,
    // NEVER Removed — the caller must not claim "rolled back". intent_durable=true here
    // (the armed journal saved cleanly). Cross-platform (injected remove) — this is the
    // genuine Windows/open-handle rollback-pending row.
    let outcome = rollback_failed(&home, "m", &mut j, fixed_now(), || false, || {});
    assert_eq!(
        outcome,
        RollbackOutcome::RollbackPending {
            intent_durable: true
        },
        "remove fail ⇒ RollbackPending with durably-saved intent"
    );
    let retained = Journal::load(&home, "m").expect("retained");
    assert!(
        retained.rollback_pending && retained.next_attempt_at.is_some(),
        "armed retained intent survives a failed remove"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2755 R3: when the worktree remove AND the retained-intent journal SAVE both fail,
/// RollbackPending surfaces `intent_durable: false` — a worse durability state the
/// response flags for intervention (indep P1.3: the save result must be observed).
/// Seam: journal.json pre-created as a DIR so `store::atomic_write` can't rename over
/// it. Cross-platform.
#[test]
fn txn_rollback_failed_pending_flags_nondurable_intent() {
    let home = tmp_home("rb-nondurable");
    let mut j = sample_journal();
    j.advance(Phase::SubmodulesReady);
    // Make the armed-intent save fail: its journal.json path is a directory.
    std::fs::create_dir_all(super::checkout_txn::journal_path(&home, "m")).unwrap();
    let outcome = rollback_failed(&home, "m", &mut j, fixed_now(), || false, || {});
    assert_eq!(
        outcome,
        RollbackOutcome::RollbackPending {
            intent_durable: false
        },
        "remove fail + save fail ⇒ pending with non-durable intent"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2755 R3 (public response): the checkout error response reports the ACTUAL cleanup
/// state. `Removed` keeps the historical "worktree rolled back" text + original code.
/// `RollbackPending` NEVER claims rolled back — a structured pending state (code
/// `rollback_pending`, `rollback_pending:true`, `intent_durable`, original
/// `failed_code` preserved). Pure mapping, cross-platform.
#[test]
fn rollback_response_pending_never_claims_rolled_back() {
    let removed = super::checkout::rollback_response(
        RollbackOutcome::Removed,
        "submodule init failed",
        "submodule_init_failed",
        "submodules_ready",
        "feat/x",
    );
    assert_eq!(removed["code"], "submodule_init_failed");
    assert!(removed["error"].as_str().unwrap().contains("rolled back"));
    assert!(removed.get("rollback_pending").is_none());

    let pending = super::checkout::rollback_response(
        RollbackOutcome::RollbackPending {
            intent_durable: false,
        },
        "submodule init failed",
        "submodule_init_failed",
        "submodules_ready",
        "feat/x",
    );
    assert_eq!(
        pending["code"], "rollback_pending",
        "pending ⇒ distinct code"
    );
    assert_eq!(pending["rollback_pending"], true);
    assert_eq!(
        pending["failed_code"], "submodule_init_failed",
        "root-cause code preserved"
    );
    assert_eq!(pending["intent_durable"], false);
    let err = pending["error"].as_str().unwrap();
    assert!(
        !err.contains("worktree rolled back"),
        "must NOT claim rolled back: {err}"
    );
    assert!(err.contains("rollback pending"), "surfaces pending: {err}");
    assert!(
        err.contains("intervention"),
        "non-durable intent flagged: {err}"
    );
}

/// #2755 R3 (marker durability): `sync_marker_contents` opens + fsyncs the marker
/// CONTENTS and OBSERVES failure — a `std::fs::write` + parent-dir fsync makes only the
/// dirent durable, not the bytes. The seam forces the `sync_all` error (crash/power-loss
/// surrogate); disarmed it durably syncs a real file. Cross-platform (no real checkout).
#[test]
fn marker_sync_contents_observes_failure_via_seam() {
    let home = tmp_home("marker-sync");
    let f = home.join(".agend-managed");
    std::fs::write(&f, "agent=x\n").unwrap();
    super::checkout::set_fail_marker_sync(true);
    let armed = super::checkout::sync_marker_contents(&f);
    super::checkout::set_fail_marker_sync(false);
    assert!(
        armed.is_err(),
        "armed seam ⇒ marker fsync observed as failure"
    );
    assert!(
        super::checkout::sync_marker_contents(&f).is_ok(),
        "disarmed ⇒ real marker contents fsync succeeds"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─── recover_pending_sweep (shared boot+periodic — injected try_lock/remove) ───
// try_lock: Some(_) ⇒ path-lock free (crashed, safe to recover); None ⇒ held (a
// live checkout owns the path — skip).

use super::checkout_txn::recover_pending_sweep;

fn save_pending(
    home: &Path,
    mangled: &str,
    wt: &Path,
    attempts: u32,
    deadline: chrono::DateTime<chrono::Utc>,
) {
    std::fs::create_dir_all(wt).unwrap();
    let mut j = Journal::prepared(
        "nonce-x",
        wt.display().to_string(),
        "/src",
        "b",
        false,
        fixed_now().to_rfc3339(),
    );
    j.advance(Phase::SubmodulesReady);
    j.rollback_pending = true;
    j.attempts = attempts;
    j.next_attempt_at = Some(deadline.to_rfc3339());
    j.save(home, mangled).unwrap();
}

/// No transaction area ⇒ nothing.
#[test]
fn txn_sweep_empty_area_is_zero() {
    let home = tmp_home("sweep-empty");
    let n = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| true, |_| {});
    assert_eq!(n, 0);
    std::fs::remove_dir_all(&home).ok();
}

/// Due + real worktree + lock free ⇒ removed + cleared.
#[test]
fn txn_sweep_due_remove_ok_clears() {
    let home = tmp_home("sweep-ok");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        1,
        fixed_now() - chrono::Duration::seconds(1),
    );
    let n = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| true, |_| {});
    assert_eq!(n, 1);
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// Not-yet-due ⇒ skipped (backoff respected).
#[test]
fn txn_sweep_not_due_is_skipped() {
    let home = tmp_home("sweep-notdue");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        1,
        fixed_now() + chrono::Duration::seconds(60),
    );
    let removed = std::cell::Cell::new(false);
    let n = recover_pending_sweep(
        &home,
        fixed_now(),
        |_| Some(()),
        |_| {
            removed.set(true);
            true
        },
        |_| {},
    );
    assert_eq!(n, 0);
    assert!(!removed.get());
    assert!(Journal::load(&home, "m").is_some());
    std::fs::remove_dir_all(&home).ok();
}

/// Due, remove FAILS ⇒ re-armed, not cleared.
#[test]
fn txn_sweep_remove_fail_rearms() {
    let home = tmp_home("sweep-fail");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        0,
        fixed_now() - chrono::Duration::seconds(1),
    );
    let n = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| false, |_| {});
    assert_eq!(n, 0);
    let j = Journal::load(&home, "m").expect("retained");
    assert!(j.rollback_pending && j.attempts >= 1);
    std::fs::remove_dir_all(&home).ok();
}

/// INTERVENTION audit fires ONCE on entering the ceiling; deduped after.
#[test]
fn txn_sweep_intervention_audit_deduped() {
    let home = tmp_home("sweep-audit");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        9,
        fixed_now() - chrono::Duration::seconds(1),
    );
    let count = std::cell::Cell::new(0);
    recover_pending_sweep(
        &home,
        fixed_now(),
        |_| Some(()),
        |_| false,
        |_| count.set(count.get() + 1),
    );
    assert_eq!(count.get(), 1, "audit once on entering intervention");
    let mut j = Journal::load(&home, "m").unwrap();
    assert!(j.intervention);
    j.next_attempt_at = Some((fixed_now() - chrono::Duration::seconds(1)).to_rfc3339());
    j.save(&home, "m").unwrap();
    recover_pending_sweep(
        &home,
        fixed_now(),
        |_| Some(()),
        |_| false,
        |_| count.set(count.get() + 1),
    );
    assert_eq!(count.get(), 1, "deduped");
    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKER 2 — sweep-vs-new-generation: the journal's nonce CHANGES under the lock
/// (a newer checkout re-provisioned this path) ⇒ the sweep skips it, so a NEWER
/// generation's worktree is never deleted.
#[test]
fn txn_sweep_nonce_cas_skips_newer_generation() {
    let home = tmp_home("sweep-nonce");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        1,
        fixed_now() - chrono::Duration::seconds(1),
    );
    let removed = std::cell::Cell::new(false);
    let home2 = home.clone();
    let n = recover_pending_sweep(
        &home,
        fixed_now(),
        |seen| {
            // Newer generation takes over between the unlocked read and the
            // under-lock re-read: overwrite with a DIFFERENT nonce.
            let mut newer = seen.clone();
            newer.nonce = "NEWER-GEN".into();
            newer.save(&home2, "m").unwrap();
            Some(())
        },
        |_| {
            removed.set(true);
            true
        },
        |_| {},
    );
    assert_eq!(n, 0, "nonce changed ⇒ skip");
    assert!(!removed.get(), "NEWER generation's worktree never removed");
    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKER 2 — a HELD path-lock (an active checkout) ⇒ the sweep skips: a live
/// provision is never disturbed.
#[test]
fn txn_sweep_skips_live_locked_checkout() {
    let home = tmp_home("sweep-locked");
    let wt = home.join("wt");
    save_pending(
        &home,
        "m",
        &wt,
        1,
        fixed_now() - chrono::Duration::seconds(1),
    );
    let removed = std::cell::Cell::new(false);
    let n = recover_pending_sweep(
        &home,
        fixed_now(),
        |_| None::<()>,
        |_| {
            removed.set(true);
            true
        },
        |_| {},
    );
    assert_eq!(n, 0);
    assert!(!removed.get(), "locked (live) checkout not touched");
    assert!(Journal::load(&home, "m").is_some(), "journal retained");
    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKER 4 — a crashed NON-rollback_pending journal (crashed before arming) is
/// still recovered: every non-Committed phase with a real worktree is handled.
#[test]
fn txn_sweep_recovers_unarmed_crash() {
    let home = tmp_home("sweep-unarmed");
    let wt = home.join("wt");
    save_journal_at(&home, "m", &wt, Phase::WorktreeAdded, true); // rollback_pending=false
    let n = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| true, |_| {});
    assert_eq!(n, 1, "unarmed crash recovered");
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// A corrupt journal is cleared by the sweep (any orphan worktree ⇒ worktree GC).
#[test]
fn txn_sweep_corrupt_cleared() {
    let home = tmp_home("sweep-corrupt");
    write_corrupt_journal(&home, "m");
    let n = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| true, |_| {});
    assert_eq!(n, 0);
    assert!(Journal::load(&home, "m").is_none(), "corrupt cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// The typed path-lock guard carries the normalized TARGET-PATH identity and
/// revalidates against a path (the forward API Slice A will require on bind_full).
#[test]
fn txn_path_lock_guard_carries_identity() {
    let home = tmp_home("pathlock");
    let wt = home.join("worktrees").join("agent-src");
    std::fs::create_dir_all(&wt).unwrap();
    let g = super::checkout_txn::acquire_path_lock(&home, &wt, "agent-src").expect("acquire");
    assert_eq!(g.mangled(), "agent-src", "mangled metadata preserved");
    assert_eq!(g.target(), super::checkout_txn::normalize_target(&wt));
    assert!(g.guards(&wt), "revalidates its own target path");
    assert!(
        !g.guards(&home.join("worktrees").join("other")),
        "rejects a different target path"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Alias spellings (`.`-containing, symlink) of the SAME real worktree normalize
/// to the SAME lock identity, so checkout/bind/release/GC mutually exclude on that
/// path however it is spelled; distinct paths key distinctly. (Under the old
/// (instance,source) mangled key these aliases would key DIFFERENTLY — the RED.)
#[test]
fn txn_normalize_target_aliases_share_identity() {
    use super::checkout_txn::normalize_target;
    let home = tmp_home("alias");
    let real = home.join("worktrees").join("realwt");
    std::fs::create_dir_all(&real).unwrap();

    let via_dot = home.join("worktrees").join(".").join("realwt");
    assert_eq!(
        normalize_target(&real),
        normalize_target(&via_dot),
        "`.`-alias normalizes to the same identity"
    );
    #[cfg(unix)]
    {
        let link = home.join("worktrees").join("linkwt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            normalize_target(&real),
            normalize_target(&link),
            "symlink normalizes to its canonical target ⇒ same lock identity"
        );
    }
    let other = home.join("worktrees").join("otherwt");
    std::fs::create_dir_all(&other).unwrap();
    assert_ne!(
        normalize_target(&real),
        normalize_target(&other),
        "distinct paths keep distinct identities"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─── frozen-scope remaining: redaction + real-entry recovery (deterministic) ──

use super::checkout::redact_paths;

/// Structured redaction strips absolute paths / git stderr from returned errors
/// while keeping the actionable non-path text; "and/or" and plain words are not
/// mangled (the ≥2-segment + boundary rule).
#[test]
fn redact_paths_strips_absolute_paths_only() {
    let s = "fatal: could not clone /Users/alice/.agend/worktrees/x into /private/tmp/y";
    let r = redact_paths(s);
    assert!(!r.contains("/Users/alice"), "unix home redacted: {r}");
    assert!(!r.contains("/private/tmp"), "temp path redacted: {r}");
    assert!(
        r.contains("<path>") && r.contains("could not clone"),
        "placeholder + non-path text kept: {r}"
    );
    assert!(
        !redact_paths(r"at C:\Users\bob\wt\x").contains(r"C:\Users"),
        "windows drive path redacted"
    );
    assert_eq!(
        redact_paths("retry and/or wait"),
        "retry and/or wait",
        "relative token untouched"
    );
    assert_eq!(
        redact_paths("git worktree add failed"),
        "git worktree add failed",
        "no false positive on plain words"
    );
}

/// The `(instance, source)` → worktree-dir mangling `handle_checkout_repo` uses.
#[cfg(unix)] // Used only by the Unix-only real-checkout tests below.
fn mangled_for(instance: &str, source: &Path) -> String {
    format!(
        "{}-{}",
        instance,
        source
            .display()
            .to_string()
            .replace(['/', '\\', ':'], "_")
            .replace('~', "")
    )
}

/// BLOCKER 1 — a checked PREPARED-save failure is fatal-but-CLEAN: no worktree is
/// created (no side effect yet), so nothing to roll back. (journal.json
/// pre-created as a DIR ⇒ the first `store::atomic_write` rename fails.)
///
/// `#[cfg(unix)]`: absolute temp `repository_path` — Unix-only source contract
/// (see `checkout_initializes_nested_submodules_2755`).
#[cfg(unix)]
#[test]
fn checkout_prepared_write_failure_fails_clean_2755() {
    let home = tmp_home("prepfail");
    let repo = tmp_repo_with_file("prepfail", "readme.txt", "x\n");
    let instance = "agent-pf";
    let mangled = mangled_for(instance, &repo);
    let jpath = home
        .join("checkout_txn")
        .join(&mangled)
        .join("journal.json");
    std::fs::create_dir_all(&jpath).unwrap(); // seam: journal.json is a DIR

    let args =
        json!({"repository_path": repo.display().to_string(), "branch": "main", "bind": false});
    let resp = super::checkout::handle_checkout_repo(&home, &args, instance);
    assert_eq!(
        resp["code"].as_str(),
        Some("journal_write"),
        "Prepared save failure ⇒ journal_write: {resp}"
    );
    assert_eq!(resp["stage"].as_str(), Some("prepared"));
    assert!(
        !home.join("worktrees").join(&mangled).exists(),
        "no worktree created (fatal-but-clean)"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// BLOCKER 1 — a COMMITTED-write failure (all earlier phase saves succeed) ABORTS
/// into rollback: the worktree is removed and the structured `commit_failed`
/// returned, never success. Uses the checkout_txn `set_fail_committed_save` seam.
///
/// `#[cfg(unix)]`: absolute temp `repository_path` — Unix-only source contract
/// (see `checkout_initializes_nested_submodules_2755`).
#[cfg(unix)]
#[test]
fn checkout_commit_write_failure_rolls_back_2755() {
    let home = tmp_home("commitfail");
    let repo = tmp_repo_with_file("commitfail", "readme.txt", "x\n");
    let instance = "agent-cf";
    let mangled = mangled_for(instance, &repo);
    super::checkout_txn::set_fail_committed_save(true);
    let args =
        json!({"repository_path": repo.display().to_string(), "branch": "main", "bind": false});
    let resp = super::checkout::handle_checkout_repo(&home, &args, instance);
    super::checkout_txn::set_fail_committed_save(false);
    assert_eq!(
        resp["code"].as_str(),
        Some("commit_failed"),
        "Committed-write failure ⇒ commit_failed: {resp}"
    );
    assert!(
        !home.join("worktrees").join(&mangled).exists(),
        "worktree rolled back on Committed-write failure"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2755 R3 (marker durability, real entry): a marker CONTENTS fsync failure during
/// provisioning ABORTS the transaction — the worktree is rolled back and a structured
/// `marker_fsync_failed` error returned, never a success with a non-durable marker.
/// Seam forces `sync_all` err at the MarkerDurable stage.
///
/// `#[cfg(unix)]`: absolute temp `repository_path` — Unix-only source contract
/// (see `checkout_initializes_nested_submodules_2755`). The cross-platform durability
/// observation is covered by `marker_sync_contents_observes_failure_via_seam`.
#[cfg(unix)]
#[test]
fn checkout_marker_fsync_failure_rolls_back_2755() {
    let home = tmp_home("markerfail");
    let repo = tmp_repo_with_file("markerfail", "readme.txt", "x\n");
    let instance = "agent-mf";
    let mangled = mangled_for(instance, &repo);
    super::checkout::set_fail_marker_sync(true);
    let args =
        json!({"repository_path": repo.display().to_string(), "branch": "main", "bind": false});
    let resp = super::checkout::handle_checkout_repo(&home, &args, instance);
    super::checkout::set_fail_marker_sync(false);
    assert_eq!(
        resp["stage"].as_str(),
        Some("marker_durable"),
        "aborts at marker durability: {resp}"
    );
    assert_eq!(
        resp["code"].as_str(),
        Some("marker_fsync_failed"),
        "marker fsync failure ⇒ structured marker_fsync_failed (worktree removable ⇒ rolled back): {resp}"
    );
    assert!(
        !home.join("worktrees").join(&mangled).exists(),
        "worktree rolled back on marker fsync failure"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// Restart-replay: a crashed prior attempt left a REAL stale worktree + a
/// non-Committed journal at this path; a fresh real checkout REPLAYS it (removes
/// the stale worktree under the path-lock) then provisions cleanly.
///
/// `#[cfg(unix)]`: absolute temp `repository_path` — Unix-only source contract
/// (see `checkout_initializes_nested_submodules_2755`).
#[cfg(unix)]
#[test]
fn checkout_restart_replays_stale_worktree_2755() {
    let home = tmp_home("replay");
    let repo = tmp_repo_with_file("replay", "readme.txt", "x\n");
    let instance = "agent-rp";
    let mangled = mangled_for(instance, &repo);
    let wt = home.join("worktrees").join(&mangled);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    // Crashed attempt: a REAL stale worktree at the target path + its
    // non-Committed journal (WorktreeAdded ⇒ recover_stale must remove it).
    git_run_ok(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            &wt.display().to_string(),
            "main",
        ],
        false,
    );
    let mut j = Journal::prepared(
        "stale-nonce",
        wt.display().to_string(),
        repo.display().to_string(),
        "main",
        false,
        fixed_now().to_rfc3339(),
    );
    j.advance(Phase::WorktreeAdded);
    j.save(&home, &mangled).unwrap();

    let args =
        json!({"repository_path": repo.display().to_string(), "branch": "main", "bind": false});
    let resp = super::checkout::handle_checkout_repo(&home, &args, instance);
    assert!(
        resp.get("error").is_none(),
        "checkout succeeds after replaying the stale attempt: {resp}"
    );
    assert!(
        wt.join("readme.txt").is_file(),
        "freshly provisioned worktree materialized"
    );
    assert!(
        Journal::load(&home, &mangled).is_none(),
        "journal cleared on Committed"
    );
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2755 item 1 (codex R2): the PRODUCTION recovery entry
/// `checkout_txn::recover_pending_sweep_prod` — the ONE callable run from
/// boot-repair AND the per-tick handler — invoked DIRECTLY (not via another
/// checkout) removes a crashed attempt's REAL stale worktree under the real
/// path-lock via a real `git worktree remove --force`, then clears its journal.
/// The restart-replay test above drives recovery THROUGH a second checkout; this
/// pins the boot / per-tick standalone path codex flagged as untested.
#[cfg(unix)]
#[test]
fn recover_pending_sweep_prod_removes_stale_worktree_directly_2755() {
    let home = tmp_home("prod-sweep");
    let repo = tmp_repo_with_file("prod-sweep", "readme.txt", "x\n");
    let instance = "agent-ps";
    let mangled = mangled_for(instance, &repo);
    let wt = home.join("worktrees").join(&mangled);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    // A crashed attempt: a REAL registered worktree + a non-Committed journal.
    git_run_ok(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            &wt.display().to_string(),
            "main",
        ],
        false,
    );
    assert!(
        wt.join("readme.txt").is_file(),
        "fixture: stale worktree materialized"
    );
    let mut j = Journal::prepared(
        "crashed-nonce",
        wt.display().to_string(),
        repo.display().to_string(),
        "main",
        false,
        fixed_now().to_rfc3339(),
    );
    j.advance(Phase::WorktreeAdded);
    j.save(&home, &mangled).unwrap();

    // DIRECT production entry — no second checkout drives this.
    let resolved = super::checkout_txn::recover_pending_sweep_prod(&home);
    assert_eq!(
        resolved, 1,
        "prod sweep resolves exactly the one crashed worktree"
    );
    assert!(
        !wt.exists(),
        "stale worktree removed by the real `git worktree remove --force`"
    );
    assert!(
        Journal::load(&home, &mangled).is_none(),
        "journal cleared after successful recovery"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&repo).ok();
}

/// #2755 item 2 (codex R2): a `bind:true` idempotent reuse (THIS agent already
/// bound to this branch, worktree still on disk) must recursively INIT submodules
/// on the reused worktree before returning it. A worktree provisioned before this
/// fix — or partially inited — can have EMPTY submodule dirs; reuse must self-heal
/// rather than hand back a broken tree. RED before the reuse-path
/// `init_submodules_strict` (the short-circuit returned the stale worktree as-is).
///
/// Feasible without daemon signing: `binding::read` is an unsigned read (no HMAC
/// verify) and the reuse short-circuit returns BEFORE `bind_full`, so a hand-seeded
/// `binding.json` drives the path. The branch is non-protected (E4.5 rejects `main`
/// under `bind:true`).
#[cfg(unix)]
#[test]
fn checkout_idempotent_bound_reuse_inits_empty_submodules_2755() {
    let home = tmp_home("reuse-submod");
    let super_repo = tmp_super_with_nested_submodules("reuse-submod");
    let instance = "agent-reuse";
    let branch = "feat/reuse"; // non-protected (main is E4.5-protected for bind)
    git_run_ok(&super_repo, &["branch", branch, "main"], false);

    // A pre-existing bound worktree whose submodules are EMPTY: `git worktree add`
    // does NOT recurse submodules, so vendor/mid/ starts uninitialized.
    let mangled = mangled_for(instance, &super_repo);
    let wt = home.join("worktrees").join(&mangled);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git_run_ok(
        &super_repo,
        &["worktree", "add", &wt.display().to_string(), branch],
        false,
    );
    let nested_b = wt.join("vendor/mid/nested/nested_b.txt");
    assert!(
        !nested_b.exists(),
        "fixture: reused worktree must start with EMPTY submodules"
    );

    // Seed THIS agent's binding pointing at the existing worktree (unsigned; the
    // reuse short-circuit reads branch+worktree and returns before bind_full).
    let bdir = home.join("runtime").join(instance);
    std::fs::create_dir_all(&bdir).unwrap();
    std::fs::write(
        bdir.join("binding.json"),
        json!({
            "version": 1,
            "agent": instance,
            "task_id": "T-test",
            "branch": branch,
            "worktree": wt.display().to_string(),
            "source_repo": super_repo.display().to_string(),
            "issued_at": "2026-01-01T00:00:00+00:00",
        })
        .to_string(),
    )
    .unwrap();

    let args = json!({
        "repository_path": super_repo.display().to_string(),
        "branch": branch,
        "bind": true,
    });
    let resp = super::checkout::handle_checkout_repo(&home, &args, instance);
    assert_eq!(
        resp.get("idempotent").and_then(|v| v.as_bool()),
        Some(true),
        "must take the idempotent bound-reuse path: {resp}"
    );
    assert!(
        nested_b.is_file(),
        "#2755: idempotent reuse must recursively init submodules so {} exists",
        nested_b.display()
    );

    std::fs::remove_dir_all(&home).ok();
    if let Some(root) = super_repo.parent() {
        std::fs::remove_dir_all(root).ok();
    }
}

/// Windows/open-handle: a rollback whose `git worktree remove --force` FAILS
/// (an open handle pins the dir) RETAINS intent (armed + backoff); a later sweep,
/// once the handle is released (remove succeeds), resolves and clears it.
/// Deterministic via injected remove — a real held OS handle is platform-specific.
#[test]
fn txn_open_handle_remove_failure_retained_then_recovered() {
    let home = tmp_home("openhandle");
    let mangled = "agent-oh";
    let wt = home.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    let mut j = Journal::prepared(
        "nonce-x",
        wt.display().to_string(),
        "/src",
        "b",
        false,
        fixed_now().to_rfc3339(),
    );
    j.advance(Phase::SubmodulesReady);
    j.save(&home, mangled).unwrap();
    // Handle held open ⇒ remove fails ⇒ intent retained (not cleared).
    assert!(
        matches!(
            rollback_failed(&home, mangled, &mut j, fixed_now(), || false, || {}),
            RollbackOutcome::RollbackPending { .. }
        ),
        "open handle ⇒ remove fails ⇒ RollbackPending"
    );
    let stuck = Journal::load(&home, mangled).expect("retained while handle open");
    assert!(stuck.rollback_pending && stuck.next_attempt_at.is_some());
    // Make it due; handle released ⇒ the sweep removes + clears.
    let mut s = Journal::load(&home, mangled).unwrap();
    s.next_attempt_at = Some((fixed_now() - chrono::Duration::seconds(1)).to_rfc3339());
    s.save(&home, mangled).unwrap();
    let resolved = recover_pending_sweep(&home, fixed_now(), |_| Some(()), |_| true, |_| {});
    assert_eq!(resolved, 1, "handle released ⇒ recovered");
    assert!(
        Journal::load(&home, mangled).is_none(),
        "cleared after successful remove"
    );
    std::fs::remove_dir_all(&home).ok();
}
