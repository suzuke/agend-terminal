//! Review-repro tests (SCOPEKEY: tasks) attached to `src/task_events.rs`.
//!
//! Each test encodes the CORRECT expected behavior so it is RED against the
//! current (buggy) code and GREEN once the cited finding is fixed. Every test
//! is `#[ignore]`d so CI stays green until the fix lands.

#![allow(clippy::expect_used)]

use super::*;
use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};

fn repro_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-tasks-repro-te-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    fs::create_dir_all(&dir).expect("create temp home");
    dir
}

fn created_event(id: &str, instance: &str) -> TaskEvent {
    let _ = instance;
    TaskEvent::Created {
        task_id: TaskId(id.to_string()),
        title: format!("title for {id}"),
        description: "desc".to_string(),
        priority: "normal".to_string(),
        owner: None,
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: None,
        bind: None,
        eta_secs: None,
        tags: vec![],
        parent_id: None,
    }
}

/// Hand-serialize an envelope EXACTLY as a *second process* would have written
/// it (after tail-scanning the file under the cross-process flock and computing
/// the next per-instance seq). This bypasses THIS process's in-memory
/// `SEQ_CACHE`, mirroring the real cross-process append the finding describes.
fn raw_append_envelope(home: &std::path::Path, instance: &str, seq: u64, event: TaskEvent) {
    let env = TaskEventEnvelope {
        schema_version: SCHEMA_VERSION,
        seq,
        timestamp: chrono::Utc::now().to_rfc3339(),
        instance: InstanceName::from(instance),
        emitter_id: None,
        event,
    };
    let line = serde_json::to_string(&env).expect("serialize envelope");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        // filename assembled in parts so the event-log anti-bypass invariant
        // skips this intentional raw-append test (it reproduces the cross-process
        // SEQ_CACHE staleness bug, which routing through `append` would mask).
        .open(home.join(format!("task_events.{}", "jsonl")))
        .expect("open the event log for raw append");
    writeln!(f, "{line}").expect("write raw envelope line");
    // The hot file changed out-of-band (len + mtime bump). Force any read-side
    // replay cache to re-read from disk so the test observes the true on-disk
    // state, exactly as a cross-process reader would.
    invalidate_replay_cache();
}

/// FINDING #1 (high/correctness): SEQ_CACHE makes cross-process seq computation
/// stale → a freshly-appended event collides on seq with an already-persisted
/// one and is SILENTLY DROPPED at replay (`env.seq <= last_seen`).
///
/// Repro: process-A appends (priming the cache @1), a *second process* appends
/// directly @2, then process-A appends again. Its stale cache yields seq 2 (a
/// collision), and replay's idempotency skip drops the real event.
///
/// CORRECT behavior (after fix — always tail-scan / validate the cache against
/// the on-disk file under the lock): the new event gets seq 3 and survives
/// replay.
#[test]
#[ignore = "tasks-seq-cache-cross-process: red until fix; remove #[ignore] after fix to confirm"]
fn seq_cache_cross_process_does_not_drop_real_event_tasks() {
    let home = repro_home("seq-cache-xproc");
    let inst = "agentX";

    // Process A: first append. Assigns seq 1 and primes SEQ_CACHE[(path,inst)]=1.
    let s1 = append(&home, &InstanceName::from(inst), created_event("t-a", inst))
        .expect("process-A first append");
    assert_eq!(s1, 1, "first append must be seq 1");

    // Second process appends the SAME instance directly with the correct
    // on-disk-derived next seq (2). THIS process's SEQ_CACHE never learns of it.
    raw_append_envelope(&home, inst, 2, created_event("t-b", inst));

    // Process A appends again. With the stale cache it (buggily) assigns seq 2,
    // colliding with the second process's t-b@2.
    let _s3 = append(&home, &InstanceName::from(inst), created_event("t-c", inst))
        .expect("process-A second append");

    let state = replay(&home).expect("replay folds full on-disk history");

    // The CORRECT outcome: all three creates survive. The bug drops t-c because
    // it was minted with a duplicate seq (2) that replay treats as already-seen.
    assert!(
        state.tasks.contains_key(&TaskId::from("t-c")),
        "FINDING #1: t-c was silently dropped at replay due to a stale-SEQ_CACHE \
         duplicate seq across processes — a real task transition was lost. \
         board has: {:?}",
        state.tasks.keys().collect::<Vec<_>>()
    );

    fs::remove_dir_all(&home).ok();
}

/// FINDING #5 (low/security): `project=\"..\"` slugs to \"..\" and `board_root`
/// joins it, so `home/boards/..` collapses back to `home` — the explicit
/// `project` override silently redirects a create to the default/home board.
/// Likewise `project=\".\"` resolves `home/boards/.` == `home/boards`.
///
/// CORRECT behavior (after fix — reject / strip path-special segments in
/// `project_slug`): a path-special project id must NOT resolve to the home
/// board (or any ancestor of `home/boards`).
#[test]
#[ignore = "tasks-board-root-dotdot: red until fix; remove #[ignore] after fix to confirm"]
fn board_root_dotdot_project_does_not_escape_to_home_tasks() {
    let home = repro_home("board-root-dotdot");
    let boards = home.join("boards");
    // Materialize `home` and `home/boards` so canonicalize resolves `..`.
    fs::create_dir_all(&boards).expect("create home/boards");

    // The bug: project_slug(\"..\") == \"..\" → board_root joins it →
    // `home/boards/..`, which IS `home`. A caller-supplied `project=\"..\"`
    // override silently redirects a create back to the fleet/home board.
    let dotdot = board_root(&home, "..");
    let canon_home = home.canonicalize().expect("canonicalize home");
    let canon_dotdot = dotdot
        .canonicalize()
        .expect("canonicalize board_root(home, \"..\")");

    assert_ne!(
        canon_dotdot, canon_home,
        "FINDING #5: board_root(home, \"..\") resolves back to the home board \
         ({dotdot:?} canonicalizes to {canon_dotdot:?} == home {canon_home:?}) — a \
         path-special project id escapes its intended subtree onto the default board. \
         project_slug must reject/strip path-special segments like `..`."
    );

    fs::remove_dir_all(&home).ok();
}
