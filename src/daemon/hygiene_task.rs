//! V1 of d-20260712065632138568-7: durable system-hygiene alerts live ON the
//! task board — one active task per episode key, created/updated atomically
//! under the board JSONL lock via `append_batch_checked` (fresh-replay
//! precondition). Task metadata is the ONLY durable episode authority: there
//! is no separate receipt store to desync (create-then-crash or
//! receipt-then-crash两store窗 impossible by construction).
//!
//! Key semantics:
//! - active task (status != Done/Cancelled) with `system_alert_key` == key
//!   already on the board → UPDATE it (evidence/last_seen/occurrences), no new
//!   task.
//! - no active task with the key (never existed, or prior episode Done) →
//!   CREATE a fresh task; a Done episode re-opens as a NEW task.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::task_events::{self, InstanceName, TaskBoardState, TaskEvent, TaskId, TaskStatus};

/// Metadata key carrying the stable episode identity.
pub const ALERT_KEY_META: &str = "system_alert_key";
/// Metadata key carrying the latest evidence payload (JSON object).
pub const EVIDENCE_META: &str = "evidence";
/// Metadata key carrying the RFC3339 time the episode was last observed.
pub const LAST_SEEN_META: &str = "last_seen";
/// Metadata key counting observations folded into this episode task.
pub const OCCURRENCES_META: &str = "occurrences";

const EMITTER: &str = "system:hygiene";

#[derive(Debug, PartialEq, Eq)]
pub enum HygieneUpsert {
    Created(TaskId),
    Updated(TaskId),
}

impl HygieneUpsert {
    pub fn task_id(&self) -> &TaskId {
        match self {
            HygieneUpsert::Created(t) | HygieneUpsert::Updated(t) => t,
        }
    }
}

/// Find the ACTIVE (not Done/Cancelled) task carrying `key` in
/// `system_alert_key` metadata, plus its current occurrence count.
fn find_active(state: &TaskBoardState, key: &str) -> Option<(TaskId, u64)> {
    state.tasks.values().find_map(|t| {
        if matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled) {
            return None;
        }
        (t.metadata.get(ALERT_KEY_META)? == key).then(|| {
            let n = t
                .metadata
                .get(OCCURRENCES_META)
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            (t.id.clone(), n)
        })
    })
}

/// r2 (§3.20 deterministic RED): test-only rendezvous at the stale-probe
/// window — between capturing `(tid, n)` from a rejected create and issuing
/// the update append. Lets a test hold N writers at the SAME probed `n`
/// deterministically. Each participating thread waits at most once
/// (thread-local latch), so CAS-retry loops can never re-block; unarmed =
/// no-op; test-only compile (nextest = process-per-test, no cross-test leak).
#[cfg(test)]
pub(crate) mod test_sync {
    use std::cell::Cell;
    use std::sync::{Arc, Barrier, Mutex};

    /// (generation, barrier): the generation makes a reused test-runner
    /// thread distinguish "already rendezvoused THIS arming" from a fresh
    /// arming by a later test in the same process (`cargo test` runs tests
    /// as threads in one process; nextest is process-per-test).
    static GATE: Mutex<Option<(u64, Arc<Barrier>)>> = Mutex::new(None);
    static NEXT_GEN: Mutex<u64> = Mutex::new(1);
    thread_local! {
        static WAITED_GEN: Cell<u64> = const { Cell::new(0) };
    }

    /// Scoped arming: dropping the guard clears the gate, so an armed
    /// barrier can never leak into later tests/threads (r3 blocker on
    /// 8bf5b73e: static gate was set and never reset).
    #[must_use = "the gate stays armed only while this guard lives"]
    pub(crate) struct GateGuard;
    impl Drop for GateGuard {
        fn drop(&mut self) {
            GATE.lock().expect("gate lock").take();
        }
    }

    pub fn arm(parties: usize) -> GateGuard {
        let mut next = NEXT_GEN.lock().expect("gen lock");
        let gen = *next;
        *next += 1;
        *GATE.lock().expect("gate lock") = Some((gen, Arc::new(Barrier::new(parties))));
        GateGuard
    }

    pub fn wait_if_armed() {
        let gate = GATE.lock().expect("gate lock").clone();
        if let Some((gen, b)) = gate {
            if WAITED_GEN.with(Cell::get) == gen {
                return; // this thread already rendezvoused this generation
            }
            WAITED_GEN.with(|w| w.set(gen));
            b.wait();
        }
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn new_task_id() -> TaskId {
    // Same shape as the MCP create path (messaging.rs): timestamp + pid +
    // process-unique seq.
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    TaskId(format!("t-{ts}-{}-h{seq}", std::process::id()))
}

/// Atomically create-or-update the hygiene task for `key`. `title` is used
/// only on create; `evidence` (a JSON object with exact repo/branch/reason
/// fields) replaces the previous evidence on update. CAS loop: each attempt
/// is individually atomic and validates the exact state its write assumes;
/// rejections hand back the fresh state, so W concurrent writers converge in
/// ≤W serialized rounds (bounded by `ATTEMPTS`, overrun = loud error).
pub fn upsert_system_hygiene_task(
    home: &Path,
    key: &str,
    title: &str,
    evidence: serde_json::Value,
) -> anyhow::Result<HygieneUpsert> {
    let emitter = InstanceName::from(EMITTER);
    let meta = |task_id: &TaskId, k: &str, v: serde_json::Value| TaskEvent::MetadataSet {
        task_id: task_id.clone(),
        by: emitter.clone(),
        key: k.to_string(),
        value: v,
    };
    // CAS loop. Every attempt validates its full assumption under the board
    // lock against a fresh replay — "no active task" for create, the exact
    // `(task id, occurrences)` pair for update — and every rejection hands the
    // freshly observed state to the next attempt. An ID-only update check
    // would let two stale probes both commit `n+1` (lost update, REJECTED
    // verdict on 943cb54b). Each serialized round commits at least one
    // writer, so W concurrent callers converge in ≤W rounds; the budget is a
    // generous multiple and overrunning it is a loud error, never a silent
    // drop.
    const ATTEMPTS: usize = 16;
    let mut probe: Option<(TaskId, u64)> = None;
    for _ in 0..ATTEMPTS {
        let now = chrono::Utc::now().to_rfc3339();
        match probe.take() {
            None => {
                let task_id = new_task_id();
                let mut seen: Option<(TaskId, u64)> = None;
                let create = vec![
                    TaskEvent::Created {
                        task_id: task_id.clone(),
                        title: title.to_string(),
                        description: format!(
                            "system hygiene alert (episode key `{key}`) — evidence in \
                             task metadata; close when resolved (a recurrence reopens \
                             a fresh task)."
                        ),
                        priority: "normal".to_string(),
                        owner: None,
                        due_at: None,
                        depends_on: Vec::new(),
                        routed_to: None,
                        branch: None,
                        bind: Some(false),
                        eta_secs: None,
                        tags: vec!["system-hygiene".to_string()],
                        parent_id: None,
                    },
                    meta(&task_id, ALERT_KEY_META, key.into()),
                    meta(&task_id, EVIDENCE_META, evidence.clone()),
                    meta(&task_id, LAST_SEEN_META, now.clone().into()),
                    meta(&task_id, OCCURRENCES_META, 1u64.into()),
                ];
                match task_events::append_batch_checked(home, &emitter, create, |state| {
                    match find_active(state, key) {
                        Some(hit) => {
                            seen = Some(hit);
                            Err("active episode task already exists".to_string())
                        }
                        None => Ok(()),
                    }
                })? {
                    Ok(_) => return Ok(HygieneUpsert::Created(task_id)),
                    Err(_) => {
                        probe = seen;
                        continue;
                    }
                }
            }
            Some((tid, n)) => {
                // r2: deterministic stale-probe rendezvous — no-op in
                // production, lets tests pin writers at the same probed `n`.
                #[cfg(test)]
                test_sync::wait_if_armed();
                let update = vec![
                    meta(&tid, EVIDENCE_META, evidence.clone()),
                    meta(&tid, LAST_SEEN_META, now.into()),
                    meta(&tid, OCCURRENCES_META, (n + 1).into()),
                ];
                let tid_check = tid.clone();
                let mut seen: Option<(TaskId, u64)> = None;
                match task_events::append_batch_checked(home, &emitter, update, |state| {
                    match find_active(state, key) {
                        // r2a RED state: identity-only check — the rendezvous
                        // test above FAILS at this commit by construction
                        // (two stale probes both commit n+1). Next commit
                        // tightens this to a full CAS.
                        Some((t, _)) if t == tid_check => Ok(()),
                        other => {
                            seen = other;
                            Err("episode state moved since probe".to_string())
                        }
                    }
                })? {
                    Ok(_) => return Ok(HygieneUpsert::Updated(tid)),
                    // Fresh (tid, n) → CAS-retry; None (closed) → create path.
                    Err(_) => {
                        probe = seen;
                        continue;
                    }
                }
            }
        }
    }
    anyhow::bail!("hygiene upsert for `{key}`: contention exceeded {ATTEMPTS} attempts, giving up")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ev(reason: &str) -> serde_json::Value {
        serde_json::json!({
            "repo": "/tmp/fixture-repo",
            "branch": "feat/x",
            "reason": reason,
        })
    }

    /// Bin crate has no `tempfile` dep — same pattern as
    /// `worktree_cleanup::tests::tmp_home`.
    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::AtomicU32;
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-hygiene-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn board_state(home: &Path) -> TaskBoardState {
        task_events::replay(home).expect("replay")
    }

    /// C1 (RED): concurrent double-fire for the SAME key must yield exactly
    /// one active task — one thread Creates, the other Updates.
    #[test]
    fn concurrent_double_fire_yields_one_active_task() {
        let home = tmp_home("c1");
        let h1 = {
            let home = home.clone();
            std::thread::spawn(move || {
                upsert_system_hygiene_task(&home, "k:r:b", "residue", ev("remove failed"))
            })
        };
        let h2 = {
            let home = home.clone();
            std::thread::spawn(move || {
                upsert_system_hygiene_task(&home, "k:r:b", "residue", ev("remove failed"))
            })
        };
        let r1 = h1.join().unwrap().unwrap();
        let r2 = h2.join().unwrap().unwrap();
        let state = board_state(&home);
        let active: Vec<_> = state
            .tasks
            .values()
            .filter(|t| t.metadata.contains_key(ALERT_KEY_META))
            .collect();
        assert_eq!(active.len(), 1, "exactly one hygiene task after race");
        // One side must have created, the other updated (order free).
        let created = matches!(r1, HygieneUpsert::Created(_)) as u8
            + matches!(r2, HygieneUpsert::Created(_)) as u8;
        assert_eq!(
            created, 1,
            "exactly one Created among racers: {r1:?} {r2:?}"
        );
        assert_eq!(r1.task_id(), r2.task_id(), "both refer to the same task");
    }

    /// R2' (RED): re-observing the same episode is an UPDATE (occurrences
    /// increments, last_seen moves, evidence replaced) — never a second task.
    /// Restart-equivalence: there is no process state, so a fresh call after
    /// daemon restart is byte-identical to this second call.
    #[test]
    fn same_episode_updates_not_duplicates() {
        let home_buf = tmp_home("r2");
        let home = home_buf.as_path();
        let a = upsert_system_hygiene_task(home, "k:r:b", "residue", ev("first")).unwrap();
        let b = upsert_system_hygiene_task(home, "k:r:b", "residue", ev("second")).unwrap();
        assert!(matches!(a, HygieneUpsert::Created(_)));
        assert!(matches!(b, HygieneUpsert::Updated(_)));
        let state = board_state(home);
        let t = state.tasks.get(b.task_id()).unwrap();
        assert_eq!(
            t.metadata.get(OCCURRENCES_META).and_then(|v| v.as_u64()),
            Some(2)
        );
        assert_eq!(
            t.metadata.get(EVIDENCE_META).unwrap()["reason"],
            "second",
            "latest evidence wins"
        );
        let hygiene_count = state
            .tasks
            .values()
            .filter(|t| t.metadata.contains_key(ALERT_KEY_META))
            .count();
        assert_eq!(hygiene_count, 1);
    }

    /// r2 (§3.20 deterministic RED): force the exact stale-probe interleaving
    /// via the production-path rendezvous — TWO writers both create-reject
    /// against the seeded task, both capture `n=1`, the armed barrier holds
    /// them at the update window, then both race the append. CAS: exactly one
    /// commit per observed `n`; the loser retries with fresh `n=2` → final
    /// occurrences=3, always. Old ID-only predicate: both commit `n+1=2` →
    /// final 2, always (deterministic loss — mutation-checked in the r2
    /// report).
    #[test]
    fn stale_probe_rendezvous_cas_exactly_once() {
        let home = tmp_home("cas-det");
        let seed = upsert_system_hygiene_task(&home, "k:r:b", "residue", ev("seed")).unwrap();
        assert!(matches!(seed, HygieneUpsert::Created(_)));
        let gate = test_sync::arm(2);
        let mk = |tag: &'static str| {
            let home = home.clone();
            std::thread::spawn(move || {
                upsert_system_hygiene_task(&home, "k:r:b", "residue", ev(tag))
            })
        };
        let (a, b) = (mk("obs-a"), mk("obs-b"));
        let ra = a.join().unwrap().unwrap();
        let rb = b.join().unwrap().unwrap();
        assert!(matches!(ra, HygieneUpsert::Updated(_)), "{ra:?}");
        assert!(matches!(rb, HygieneUpsert::Updated(_)), "{rb:?}");
        let state = board_state(&home);
        let t = state.tasks.get(seed.task_id()).unwrap();
        assert_eq!(
            t.metadata.get(OCCURRENCES_META).and_then(|v| v.as_u64()),
            Some(3),
            "both rendezvoused observations must land: 1 (seed) + 2"
        );
        // r3: same-process post-rendezvous regression — once the guard is
        // dropped the gate is cleared, so an ordinary update must neither
        // block on a stale barrier nor miscount.
        drop(gate);
        let after = upsert_system_hygiene_task(&home, "k:r:b", "residue", ev("post")).unwrap();
        assert!(matches!(after, HygieneUpsert::Updated(_)), "{after:?}");
        let state = board_state(&home);
        let t = state.tasks.get(seed.task_id()).unwrap();
        assert_eq!(
            t.metadata.get(OCCURRENCES_META).and_then(|v| v.as_u64()),
            Some(4),
            "post-rendezvous ordinary update must count normally"
        );
    }

    /// r1 contention/load coverage: N concurrent updaters against an EXISTING
    /// episode must each be accounted exactly once — occurrences goes 1 →
    /// 1+N, one active task, all callers report Updated. (The FINAL count is
    /// deterministic post-fix regardless of scheduling; the deterministic
    /// stale-probe interleaving itself is pinned by
    /// `stale_probe_rendezvous_cas_exactly_once` above.)
    #[test]
    fn concurrent_updates_on_existing_episode_lose_nothing() {
        const N: usize = 8;
        let home = tmp_home("cas");
        let seed = upsert_system_hygiene_task(&home, "k:r:b", "residue", ev("seed")).unwrap();
        assert!(matches!(seed, HygieneUpsert::Created(_)));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let home = home.clone();
                let b = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    upsert_system_hygiene_task(&home, "k:r:b", "residue", ev(&format!("obs-{i}")))
                })
            })
            .collect();
        for h in handles {
            let r = h.join().unwrap().unwrap();
            assert!(
                matches!(r, HygieneUpsert::Updated(_)),
                "existing episode: {r:?}"
            );
        }
        let state = board_state(&home);
        let t = state.tasks.get(seed.task_id()).unwrap();
        assert_eq!(
            t.metadata.get(OCCURRENCES_META).and_then(|v| v.as_u64()),
            Some(1 + N as u64),
            "every concurrent observation must be accounted exactly once"
        );
        assert_eq!(
            state
                .tasks
                .values()
                .filter(|t| t.metadata.contains_key(ALERT_KEY_META))
                .count(),
            1
        );
    }

    /// C2 (RED): a Done episode re-opens as a NEW task (fresh episode), the
    /// done task stays done.
    #[test]
    fn done_episode_reopens_as_new_task() {
        let home_buf = tmp_home("c2");
        let home = home_buf.as_path();
        let a = upsert_system_hygiene_task(home, "k:r:b", "residue", ev("first")).unwrap();
        let emitter = InstanceName::from(EMITTER);
        task_events::append(
            home,
            &emitter,
            TaskEvent::Done {
                task_id: a.task_id().clone(),
                by: emitter.clone(),
                source: task_events::DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: Some("fixed".into()),
                },
            },
        )
        .unwrap();
        let b = upsert_system_hygiene_task(home, "k:r:b", "residue", ev("again")).unwrap();
        assert!(matches!(b, HygieneUpsert::Created(_)), "reopen = new task");
        assert_ne!(a.task_id(), b.task_id());
        let state = board_state(home);
        assert_eq!(
            state.tasks.get(a.task_id()).unwrap().status,
            TaskStatus::Done
        );
        assert_ne!(
            state.tasks.get(b.task_id()).unwrap().status,
            TaskStatus::Done
        );
    }

    /// Evidence exactness (RED): created task carries the exact
    /// repo/branch/reason payload + key + last_seen metadata.
    #[test]
    fn created_task_carries_exact_evidence() {
        let home_buf = tmp_home("ev");
        let home = home_buf.as_path();
        let r = upsert_system_hygiene_task(
            home,
            "residue-remove-failed:/r/repo:feat/x",
            "worktree remove failed",
            ev("sweep worktree remove failed"),
        )
        .unwrap();
        let state = board_state(home);
        let t = state.tasks.get(r.task_id()).unwrap();
        assert_eq!(
            t.metadata.get(ALERT_KEY_META).unwrap(),
            "residue-remove-failed:/r/repo:feat/x"
        );
        let e = t.metadata.get(EVIDENCE_META).unwrap();
        assert_eq!(e["repo"], "/tmp/fixture-repo");
        assert_eq!(e["branch"], "feat/x");
        assert_eq!(e["reason"], "sweep worktree remove failed");
        assert!(t.metadata.contains_key(LAST_SEEN_META));
        assert_eq!(t.status, TaskStatus::Open);
    }
}
