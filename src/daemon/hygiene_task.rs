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
/// fields) replaces the previous evidence on update. Bounded retry: each
/// attempt is individually atomic; a rejection returns the authoritative
/// state that invalidated it, so the loop converges in ≤2 flips.
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
    // Each attempt is individually atomic (precondition + append under the
    // board lock against a fresh replay). A rejection means the other shape
    // won the race; ≤2 flips converge, 3rd is a hard error.
    for _ in 0..3 {
        let now = chrono::Utc::now().to_rfc3339();
        let mut existing: Option<(TaskId, u64)> = None;
        let task_id = new_task_id();
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
        match task_events::append_batch_checked(home, &emitter, create, |state| match find_active(
            state, key,
        ) {
            Some(hit) => {
                existing = Some(hit);
                Err("active episode task already exists".to_string())
            }
            None => Ok(()),
        })? {
            Ok(_) => return Ok(HygieneUpsert::Created(task_id)),
            Err(_) => {
                let Some((tid, n)) = existing else {
                    anyhow::bail!("hygiene upsert: rejected without an existing task");
                };
                let update = vec![
                    meta(&tid, EVIDENCE_META, evidence.clone()),
                    meta(&tid, LAST_SEEN_META, now.into()),
                    meta(&tid, OCCURRENCES_META, (n + 1).into()),
                ];
                let tid_check = tid.clone();
                match task_events::append_batch_checked(home, &emitter, update, |state| {
                    match find_active(state, key) {
                        Some((t, _)) if t == tid_check => Ok(()),
                        _ => Err("episode task closed since probe".to_string()),
                    }
                })? {
                    Ok(_) => return Ok(HygieneUpsert::Updated(tid)),
                    // Closed between the two attempts — loop back to create.
                    Err(_) => continue,
                }
            }
        }
    }
    anyhow::bail!("hygiene upsert for `{key}`: state flipped 3 times, giving up")
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
            .filter(|t| t.metadata.get(ALERT_KEY_META).is_some())
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
