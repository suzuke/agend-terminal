//! #2755 real-entry RED: `repo action=checkout` must recursively initialize
//! submodules. The MCP entry `handle_checkout_repo` runs `git worktree add`
//! but (pre-fix) skips `submodule update --init --recursive`, so a
//! path-dependency submodule (e.g. `vendor/agentic-git`) is left EMPTY on the
//! provisioned worktree — the build then fails on missing nested content.
//!
//! Fixtures mirror `src/worktree/tests.rs::tmp_super_with_nested_submodules`
//! (that module's helpers are private); a two-level super→A→B nest pins that
//! the fix inits submodules RECURSIVELY, not just one level.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Run git with `AGEND_GIT_BYPASS` (skip the shim); panic on non-zero. When
/// `allow_file`, set `protocol.file.allow=always` so local-path submodule
/// fixtures clone (git's submodule helper ignores repo-stored config).
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
    assert!(!Phase::Prepared.worktree_exists());
    assert!(Phase::WorktreeAdded.worktree_exists());
    assert!(Phase::Committed.worktree_exists());
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

use super::checkout_txn::{recover_stale, rollback_failed};

fn save_at_phase(home: &std::path::Path, mangled: &str, phase: Phase) {
    let mut j = sample_journal();
    // advance from Prepared up to `phase`
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

/// A Committed tombstone left by a crashed cleanup is cleared, and no worktree
/// removal is attempted (the provision had completed).
#[test]
fn txn_recover_committed_clears_without_remove() {
    let home = tmp_home("rec-committed");
    save_at_phase(&home, "m", Phase::Committed);
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", fixed_now(), |_j| {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(!removed.get(), "Committed ⇒ no worktree removal");
    assert!(Journal::load(&home, "m").is_none(), "tombstone cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// A crashed in-flight attempt whose worktree is successfully removed clears.
#[test]
fn txn_recover_inflight_remove_ok_clears() {
    let home = tmp_home("rec-ok");
    save_at_phase(&home, "m", Phase::WorktreeAdded);
    let r = recover_stale(&home, "m", fixed_now(), |_j| true);
    assert!(r.is_ok());
    assert!(Journal::load(&home, "m").is_none(), "removed ⇒ cleared");
    std::fs::remove_dir_all(&home).ok();
}

/// A crashed in-flight attempt whose worktree CANNOT be removed retains intent
/// (armed + backoff) and returns Err so the caller aborts rather than colliding.
#[test]
fn txn_recover_inflight_remove_fail_retains_intent() {
    let home = tmp_home("rec-fail");
    save_at_phase(&home, "m", Phase::SubmodulesReady);
    let r = recover_stale(&home, "m", fixed_now(), |_j| false);
    assert!(r.is_err(), "remove failed ⇒ Err");
    let retained = Journal::load(&home, "m").expect("journal retained");
    assert!(retained.rollback_pending, "retained intent armed");
    assert!(
        retained.next_attempt_at.is_some(),
        "backoff deadline persisted"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A Prepared journal (no worktree materialized) is cleared without a remove.
#[test]
fn txn_recover_prepared_clears_without_remove() {
    let home = tmp_home("rec-prepared");
    sample_journal().save(&home, "m").unwrap(); // Prepared
    let removed = std::cell::Cell::new(false);
    let r = recover_stale(&home, "m", fixed_now(), |_j| {
        removed.set(true);
        true
    });
    assert!(r.is_ok());
    assert!(!removed.get(), "Prepared has no worktree ⇒ no remove");
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
    rollback_failed(
        &home,
        "m",
        &mut j,
        fixed_now(),
        || true,
        || unbound.set(true),
    );
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
    rollback_failed(&home, "m", &mut j, fixed_now(), || false, || {});
    let retained = Journal::load(&home, "m").expect("retained");
    assert!(
        retained.rollback_pending && retained.next_attempt_at.is_some(),
        "armed retained intent survives a failed remove"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ─── recover_pending_sweep (shared boot+periodic callable — injected closures) ─

use super::checkout_txn::recover_pending_sweep;

fn save_pending_journal(
    home: &std::path::Path,
    mangled: &str,
    attempts: u32,
    deadline: chrono::DateTime<chrono::Utc>,
) {
    let mut j = sample_journal();
    j.advance(Phase::SubmodulesReady);
    j.rollback_pending = true;
    j.attempts = attempts;
    j.next_attempt_at = Some(deadline.to_rfc3339());
    j.save(home, mangled).unwrap();
}

/// No transaction area ⇒ sweep resolves nothing.
#[test]
fn txn_sweep_empty_area_is_zero() {
    let home = tmp_home("sweep-empty");
    let n = recover_pending_sweep(&home, fixed_now(), |_| true, |_| {});
    assert_eq!(n, 0);
    std::fs::remove_dir_all(&home).ok();
}

/// A DUE pending rollback whose worktree removes is resolved + cleared.
#[test]
fn txn_sweep_due_remove_ok_clears() {
    let home = tmp_home("sweep-ok");
    save_pending_journal(&home, "m", 1, fixed_now() - chrono::Duration::seconds(1));
    let n = recover_pending_sweep(&home, fixed_now(), |_| true, |_| {});
    assert_eq!(n, 1);
    assert!(Journal::load(&home, "m").is_none());
    std::fs::remove_dir_all(&home).ok();
}

/// A NOT-yet-due pending rollback is left untouched (backoff respected).
#[test]
fn txn_sweep_not_due_is_skipped() {
    let home = tmp_home("sweep-notdue");
    save_pending_journal(&home, "m", 1, fixed_now() + chrono::Duration::seconds(60));
    let removed = std::cell::Cell::new(false);
    let n = recover_pending_sweep(
        &home,
        fixed_now(),
        |_| {
            removed.set(true);
            true
        },
        |_| {},
    );
    assert_eq!(n, 0);
    assert!(!removed.get(), "not-due journal is not touched");
    assert!(Journal::load(&home, "m").is_some(), "journal retained");
    std::fs::remove_dir_all(&home).ok();
}

/// A due rollback whose remove FAILS is re-armed (attempts bump) and not cleared.
#[test]
fn txn_sweep_remove_fail_rearms() {
    let home = tmp_home("sweep-fail");
    save_pending_journal(&home, "m", 0, fixed_now() - chrono::Duration::seconds(1));
    let n = recover_pending_sweep(&home, fixed_now(), |_| false, |_| {});
    assert_eq!(n, 0);
    let j = Journal::load(&home, "m").expect("retained");
    assert!(
        j.rollback_pending && j.attempts >= 1,
        "re-armed for a later retry"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// The INTERVENTION audit fires ONCE when a stuck journal crosses the ceiling and
/// is NOT re-emitted on subsequent sweeps of the same still-stuck journal.
#[test]
fn txn_sweep_intervention_audit_deduped() {
    let home = tmp_home("sweep-audit");
    // attempts=9 ⇒ next arm's backoff hits the ceiling ⇒ flips into intervention.
    save_pending_journal(&home, "m", 9, fixed_now() - chrono::Duration::seconds(1));
    let count = std::cell::Cell::new(0);
    recover_pending_sweep(
        &home,
        fixed_now(),
        |_| false,
        |_| count.set(count.get() + 1),
    );
    assert_eq!(count.get(), 1, "audit once on ENTERING intervention");

    // Make it due again; still stuck + already in intervention ⇒ no re-audit.
    let mut j = Journal::load(&home, "m").unwrap();
    assert!(j.intervention, "now in intervention");
    j.next_attempt_at = Some((fixed_now() - chrono::Duration::seconds(1)).to_rfc3339());
    j.save(&home, "m").unwrap();
    recover_pending_sweep(
        &home,
        fixed_now(),
        |_| false,
        |_| count.set(count.get() + 1),
    );
    assert_eq!(
        count.get(),
        1,
        "deduped — no re-audit while already in intervention"
    );
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
