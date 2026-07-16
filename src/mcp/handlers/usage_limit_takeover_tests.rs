use super::dispatch::RuntimeContext;
use crate::daemon::supervisor::usage_limit_control::{Episode, EpisodeKey, EpisodeState};
use crate::mcp::execute_tool_with_runtime;
use crate::types::InstanceId;
use serde_json::{json, Value};
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

struct Fixture {
    home: PathBuf,
    runtime: RuntimeContext,
    episode_id: String,
    worktree: PathBuf,
    _env_guard: parking_lot::MutexGuard<'static, ()>,
    _source: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var("AGEND_HOME");
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git utf8")
        .trim()
        .to_string()
}

fn fixture(tag: &str) -> Fixture {
    let env_guard = super::fleet_test_guard();
    let home = std::env::temp_dir().join(format!(
        "agend-usage-limit-takeover-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).expect("home");
    std::env::set_var("AGEND_HOME", &home);
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  worker-a: { backend: codex, role: dev }\n  worker-b: { backend: claude, role: dev }\nteams:\n  archfix:\n    members: [worker-a, worker-b]\n    orchestrator: worker-a\n",
    )
    .expect("fleet");

    let worktree = home.join("source-worktree");
    std::fs::create_dir_all(&worktree).expect("worktree");
    git(&worktree, &["init", "-q"]);
    git(&worktree, &["config", "user.email", "test@example.invalid"]);
    git(&worktree, &["config", "user.name", "test"]);
    std::fs::write(worktree.join("tracked.txt"), "clean\n").expect("tracked");
    git(&worktree, &["add", "tracked.txt"]);
    git(&worktree, &["commit", "-qm", "fixture"]);
    git(&worktree, &["branch", "-M", "feat/slice-1"]);
    let source_head = git(&worktree, &["rev-parse", "HEAD"]);

    let runtime_dir = crate::paths::runtime_dir(&home).join("worker-a");
    std::fs::create_dir_all(&runtime_dir).expect("runtime");
    let binding = json!({
        "version": 1,
        "agent": "worker-a",
        "task_id": "t-takeover",
        "branch": "feat/slice-1",
        "issued_at": "2026-07-16T05:00:00Z",
        "worktree": worktree,
        "source_repo": worktree,
    });
    std::fs::write(
        runtime_dir.join("binding.json"),
        serde_json::to_vec_pretty(&binding).expect("binding json"),
    )
    .expect("binding");

    let task_id = crate::task_events::TaskId("t-takeover".into());
    let owner = crate::task_events::InstanceName("worker-a".into());
    crate::task_events::append_batch(
        &home,
        &owner,
        vec![
            crate::task_events::TaskEvent::Created {
                task_id: task_id.clone(),
                title: "takeover".into(),
                description: String::new(),
                priority: "high".into(),
                owner: Some(owner.clone()),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: Some("feat/slice-1".into()),
                bind: Some(true),
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
            },
            crate::task_events::TaskEvent::Claimed {
                task_id: task_id.clone(),
                by: owner.clone(),
            },
            crate::task_events::TaskEvent::InProgress {
                task_id: task_id.clone(),
                by: owner.clone(),
            },
            crate::task_events::TaskEvent::Blocked {
                task_id,
                reason: json!({
                    "type": "usage_limit_episode",
                    "episode_id": "usage-limit:t-takeover:worker-a:2026-07-16T05:00:00Z:feat/slice-1",
                    "source": "worker-a",
                    "binding_issued_at": "2026-07-16T05:00:00Z",
                    "branch": "feat/slice-1",
                    "state": "CandidateReady",
                    "proposal": {
                        "candidate": "worker-b",
                        "executable": false,
                        "requires": "operator_takeover_slice_2"
                    }
                })
                .to_string(),
            },
        ],
    )
    .expect("task events");

    let key = EpisodeKey {
        task_id: "t-takeover".into(),
        source: "worker-a".into(),
        binding_issued_at: "2026-07-16T05:00:00Z".into(),
        branch: "feat/slice-1".into(),
    };
    let episode_id = key.notification_id();
    std::fs::write(
        runtime_dir.join("usage_limit_episode.json"),
        serde_json::to_vec_pretty(&Episode {
            key,
            state: EpisodeState::CandidateReady,
            consecutive_ticks: 2,
            candidate: Some("worker-b".into()),
            unlock_at: Some("2026-07-16T06:00:00Z".into()),
            recipient: "worker-a".into(),
        })
        .expect("episode json"),
    )
    .expect("episode");

    let candidate = crate::agent::mk_test_handle("worker-b", InstanceId::new());
    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([
        (candidate.id, candidate),
    ])));
    let runtime = RuntimeContext {
        registry,
        externals: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: None,
    };
    let _ = source_head;
    Fixture {
        home,
        runtime,
        episode_id,
        worktree,
        _env_guard: env_guard,
        _source: "worker-a".into(),
    }
}

fn call(f: &Fixture, args: Value) -> Value {
    execute_tool_with_runtime("usage_limit_takeover", &args, "", f.runtime.clone())
}

fn code(value: &Value) -> &str {
    value["error_code"].as_str().unwrap_or("")
}

#[test]
#[serial]
fn usage_limit_takeover_prepares_through_real_mcp_entry() {
    let f = fixture("success");
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(result["phase"], "Prepared", "unexpected result: {result}");
    assert_eq!(result["candidate"], "worker-b");
}

#[test]
#[serial]
fn usage_limit_takeover_repeat_is_idempotent() {
    let f = fixture("repeat");
    let args = json!({"instance": "worker-a", "episode_id": f.episode_id});
    let first = call(&f, args.clone());
    let second = call(&f, args);
    assert_eq!(first["phase"], "Prepared", "first: {first}");
    assert_eq!(second["phase"], "Prepared", "second: {second}");
    assert_eq!(second["idempotent"], true, "repeat: {second}");
}

#[test]
#[serial]
fn usage_limit_takeover_concurrent_barrier_requests_converge_one_prepared_identity() {
    let f = fixture("concurrency-barrier");
    let args = json!({"instance": "worker-a", "episode_id": f.episode_id});
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let runtime = f.runtime.clone();
        let args = args.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            execute_tool_with_runtime("usage_limit_takeover", &args, "", runtime)
        }));
    }
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("takeover thread"))
        .collect();
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|result| result["phase"] == "Prepared"),
        "concurrent results: {results:?}"
    );
    let journal_path =
        crate::paths::runtime_dir(&f.home).join("worker-a/usage_limit_takeover.json");
    let journal_bytes = std::fs::read(&journal_path).expect("one prepared journal");
    let journal: Value = serde_json::from_slice(&journal_bytes).expect("journal json");
    assert_eq!(journal["phase"], "Prepared");
    assert_eq!(journal["episode_id"], f.episode_id);
    assert_eq!(journal["source"], "worker-a");
    assert_eq!(journal["candidate"], "worker-b");
    assert_eq!(journal["binding_issued_at"], "2026-07-16T05:00:00Z");
    assert_eq!(journal["source_head"].as_str().map(str::len), Some(40));
    assert!(
        journal_path.is_file(),
        "exactly one journal identity must remain"
    );
}

#[test]
#[serial]
fn usage_limit_takeover_conflicting_existing_journal_fails_closed() {
    let f = fixture("conflict");
    let args = json!({"instance": "worker-a", "episode_id": f.episode_id});
    let first = call(&f, args.clone());
    assert_eq!(first["phase"], "Prepared", "first: {first}");
    let journal = crate::paths::runtime_dir(&f.home).join("worker-a/usage_limit_takeover.json");
    let mut persisted: Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("journal")).expect("journal json");
    persisted["candidate"] = json!("worker-c");
    std::fs::write(
        &journal,
        serde_json::to_vec_pretty(&persisted).expect("journal write"),
    )
    .expect("journal");
    let second = call(&f, args);
    assert_eq!(
        code(&second),
        "conflicting_preparation",
        "unexpected result: {second}"
    );
}

#[test]
#[serial]
fn usage_limit_takeover_refuses_dirty_source_without_journal() {
    let f = fixture("dirty");
    std::fs::write(f.worktree.join("tracked.txt"), "dirty\n").expect("dirty");
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(code(&result), "source_dirty", "unexpected result: {result}");
    assert!(!crate::paths::runtime_dir(&f.home)
        .join("worker-a/usage_limit_takeover.json")
        .exists());
}

#[test]
#[serial]
fn usage_limit_takeover_refuses_generation_mismatch() {
    let f = fixture("generation");
    let binding_path = crate::paths::runtime_dir(&f.home).join("worker-a/binding.json");
    let mut binding: Value =
        serde_json::from_slice(&std::fs::read(&binding_path).expect("binding"))
            .expect("binding json");
    binding["issued_at"] = json!("2026-07-16T06:00:00Z");
    std::fs::write(
        &binding_path,
        serde_json::to_vec_pretty(&binding).expect("binding write"),
    )
    .expect("binding");
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(
        code(&result),
        "binding_generation_mismatch",
        "unexpected result: {result}"
    );
}

#[test]
#[serial]
fn usage_limit_takeover_refuses_candidate_mismatch() {
    let f = fixture("candidate");
    let episode_path = crate::paths::runtime_dir(&f.home).join("worker-a/usage_limit_episode.json");
    let mut episode: Value =
        serde_json::from_slice(&std::fs::read(&episode_path).expect("episode"))
            .expect("episode json");
    episode["candidate"] = json!("worker-c");
    std::fs::write(
        &episode_path,
        serde_json::to_vec_pretty(&episode).expect("episode write"),
    )
    .expect("episode");
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(
        code(&result),
        "candidate_mismatch",
        "unexpected result: {result}"
    );
}

#[test]
#[serial]
fn usage_limit_takeover_refuses_currently_ineligible_candidate() {
    let f = fixture("ineligible");
    let registry = f.runtime.registry.lock();
    let handle = registry.values().next().expect("candidate");
    handle.published_state.store(
        crate::state::AgentState::UsageLimit as u8,
        std::sync::atomic::Ordering::Relaxed,
    );
    drop(registry);
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(
        code(&result),
        "candidate_ineligible",
        "unexpected result: {result}"
    );
}

#[test]
#[serial]
fn usage_limit_takeover_refuses_journal_write_failure_without_partial() {
    let f = fixture("write-failure");
    let journal = crate::paths::runtime_dir(&f.home).join("worker-a/usage_limit_takeover.json");
    std::fs::create_dir_all(&journal).expect("journal collision");
    let result = call(
        &f,
        json!({"instance": "worker-a", "episode_id": f.episode_id}),
    );
    assert_eq!(
        code(&result),
        "journal_write_failed",
        "unexpected result: {result}"
    );
    assert!(journal.is_dir(), "failed write must not replace collision");
}

#[test]
#[serial]
fn usage_limit_takeover_is_operator_only() {
    let f = fixture("operator-only");
    let result = execute_tool_with_runtime(
        "usage_limit_takeover",
        &json!({"instance": "worker-a", "episode_id": f.episode_id}),
        "worker-a",
        f.runtime.clone(),
    );
    assert_eq!(
        code(&result),
        "operator_only",
        "unexpected result: {result}"
    );
}
