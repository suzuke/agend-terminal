//! Managed workers use the same spawn and teardown services as interactive instances.
use super::{Attempt, Run};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observation {
    Starting,
    Running,
    UsageLimited,
    Exited,
    Missing,
}

pub(crate) trait JobRuntime {
    fn start(&self, run: &Run, attempt: &Attempt) -> Result<String>;
    fn observe(&self, attempt: &Attempt) -> Result<Observation>;
    fn stop(&self, attempt: &Attempt) -> Result<bool>;
    fn dispatch(&self, run: &Run, attempt: &Attempt) -> Result<()>;
}

const OWNER: &str = "system:schedule_job";

#[derive(serde::Serialize, serde::Deserialize)]
struct ProcessJournal {
    uuid: String,
    phase: ProcessPhase,
    pid: Option<u32>,
    start_token: Option<u64>,
}
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
enum ProcessPhase {
    Intent,
    Running,
    Stopped,
}
fn journal_path(home: &Path, name: &str) -> PathBuf {
    home.join("schedule_job_processes")
        .join(format!("{name}.json"))
}
fn read_journal(home: &Path, name: &str) -> Result<Option<ProcessJournal>> {
    match std::fs::read(journal_path(home, name)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn write_journal(home: &Path, name: &str, journal: &ProcessJournal) -> Result<()> {
    std::fs::create_dir_all(home.join("schedule_job_processes"))?;
    crate::store::save_atomic(&journal_path(home, name), journal)
}

pub(crate) struct ManagedRuntime {
    home: PathBuf,
    registry: crate::agent::AgentRegistry,
    configs: crate::api::ConfigRegistry,
    externals: crate::agent::ExternalRegistry,
}

impl ManagedRuntime {
    pub(crate) fn new(
        home: &Path,
        registry: &crate::agent::AgentRegistry,
        configs: &crate::api::ConfigRegistry,
        externals: &crate::agent::ExternalRegistry,
    ) -> Self {
        Self {
            home: home.into(),
            registry: Arc::clone(registry),
            configs: Arc::clone(configs),
            externals: Arc::clone(externals),
        }
    }

    fn identity(&self, attempt: &Attempt) -> Result<Option<crate::types::InstanceId>> {
        crate::agent::validate_name(&attempt.name).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            super::owns_worker(&self.home, &attempt.name),
            "worker is not reserved by a job"
        );
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&self.home))?;
        let Some(entry) = fleet.instances.get(&attempt.name) else {
            return Ok(None);
        };
        anyhow::ensure!(
            entry.created_by.as_deref() == Some(OWNER),
            "worker name belongs to another creator"
        );
        let id = entry
            .id
            .as_deref()
            .and_then(crate::types::InstanceId::parse)
            .context("job worker has no valid UUID")?;
        if let Some(expected) = &attempt.uuid {
            anyhow::ensure!(expected == &id.full(), "job worker UUID changed");
        }
        Ok(Some(id))
    }
}

/// Reserve a fleet identity atomically: the ordinary add helper merges existing names.
fn register_worker(home: &Path, attempt: &Attempt, workspace: &Path) -> Result<String> {
    let mut worker_id = None;
    crate::fleet::persist::mutate_fleet_yaml(home, "instances: {}\n", |doc| {
        let rows = doc["instances"]
            .as_mapping_mut()
            .context("instances is not a mapping")?;
        let key = serde_yaml_ng::Value::String(attempt.name.clone());
        if let Some(entry) = rows.get(&key) {
            anyhow::ensure!(
                entry["created_by"].as_str() == Some(OWNER),
                "worker name already exists"
            );
            let id = entry["id"]
                .as_str()
                .context("reserved worker has no UUID")?;
            anyhow::ensure!(
                crate::types::InstanceId::parse(id).is_some(),
                "invalid reserved worker UUID"
            );
            if let Some(expected) = &attempt.uuid {
                anyhow::ensure!(id == expected, "reserved worker UUID changed");
            }
            anyhow::ensure!(
                entry["working_directory"].as_str() == workspace.to_str(),
                "reserved workspace changed"
            );
            anyhow::ensure!(
                entry["backend"].as_str() == Some(attempt.backend.as_str()),
                "reserved backend changed"
            );
            worker_id = Some(id.to_owned());
            return Ok(false);
        }
        anyhow::ensure!(
            attempt.uuid.is_none(),
            "previously registered worker disappeared"
        );
        anyhow::ensure!(!workspace.exists(), "worker workspace already exists");
        let id = crate::types::InstanceId::new().full();
        rows.insert(
            key,
            serde_yaml_ng::to_value(serde_json::json!({
                "id": id, "backend": attempt.backend, "working_directory": workspace,
                "created_by": OWNER, "topic_binding_mode": "skip", "worktree": false,
                "args": [], "role": "scheduled job worker"
            }))?,
        );
        worker_id = Some(id);
        Ok(true)
    })?;
    worker_id.context("fleet reservation did not return a UUID")
}

impl JobRuntime for ManagedRuntime {
    fn start(&self, run: &Run, attempt: &Attempt) -> Result<String> {
        crate::agent::validate_name(&attempt.name).map_err(anyhow::Error::msg)?;
        let _permit = crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
            &self.home,
            &attempt.name,
            crate::mcp::handlers::dispatch_hook::LifecycleOperation::Bind,
        )
        .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            super::owns_worker(&self.home, &attempt.name),
            "job ownership must precede spawn"
        );
        anyhow::ensure!(
            !crate::agent::lock_external(&self.externals).contains_key(&attempt.name),
            "worker name belongs to external instance"
        );
        let workspace = crate::paths::workspace_dir(&self.home).join(&attempt.name);
        std::fs::create_dir_all(&run.config.artifact_directory)?;
        let artifacts = run.config.artifact_directory.canonicalize()?;
        let workspace_root = crate::paths::workspace_dir(&self.home);
        std::fs::create_dir_all(&workspace_root)?;
        anyhow::ensure!(
            !artifacts.starts_with(workspace_root.canonicalize()?.join(&attempt.name)),
            "artifacts must be outside disposable worker workspace"
        );
        let id = register_worker(&self.home, attempt, &workspace)?;
        let parsed = crate::types::InstanceId::parse(&id).context("invalid registered UUID")?;
        let journal = read_journal(&self.home, &attempt.name)?;
        if let Some(journal) = &journal {
            anyhow::ensure!(journal.uuid == id, "process journal UUID changed");
            anyhow::ensure!(
                journal.phase != ProcessPhase::Stopped,
                "stopped attempts cannot restart"
            );
        }
        let live_child = crate::agent::lock_registry(&self.registry)
            .get(&parsed)
            .map(|handle| Arc::clone(&handle.child));
        if let Some(child) = live_child {
            let journal = journal
                .as_ref()
                .context("recovery_required: registered child has no spawn intent")?;
            let pid = child.lock().process_id();
            if journal.phase == ProcessPhase::Running {
                anyhow::ensure!(
                    journal.pid == pid,
                    "recovery_required: registered process identity changed"
                );
                if let Some(token) = journal.start_token {
                    anyhow::ensure!(
                        child.lock().try_wait()?.is_some()
                            || pid.and_then(crate::process::process_start_token) == Some(token),
                        "recovery_required: registered process birth token changed"
                    );
                }
            }
            return Ok(id);
        }
        // A dead or absent leader PID cannot prove all descendant tools exited.
        // Without the live child handle, only a completed teardown is proof.
        anyhow::ensure!(journal.is_none(),
            "recovery_required: spawn was attempted but process-tree exit is unproven; refusing duplicate worker");
        let params = crate::agent_ops::spawn::SpawnParams {
            name: &attempt.name,
            backend: Some(&attempt.backend),
            args: Some(""),
            model: None,
            model_tier: None,
            working_directory: Some(&workspace),
            env: None,
            mode: crate::backend::SpawnMode::Fresh,
            explicit_role: Some("scheduled job worker"),
            self_kick_on_ready: false,
            topic_binding: "skip",
            layout: "tab",
            spawner: None,
            target_pane: None,
        };
        let request = crate::agent_ops::spawn::resolve_spawn_request(&self.home, &params)?;
        // Reject common pre-launch failures before creating an uncertain intent.
        which::which(&request.backend).context("job backend executable unavailable")?;
        crate::agent_ops::spawn::prepare_instructions(
            &self.home,
            &attempt.name,
            &request.declared_backend.command_string(),
            &workspace,
            request.explicit_role.as_deref(),
        )
        .map_err(anyhow::Error::msg)?;
        write_journal(
            &self.home,
            &attempt.name,
            &ProcessJournal {
                uuid: id.clone(),
                phase: ProcessPhase::Intent,
                pid: None,
                start_token: None,
            },
        )?;
        crate::agent_ops::spawn::spawn_instance(
            &crate::agent_ops::spawn::SpawnContext {
                home: &self.home,
                registry: &self.registry,
                configs: &self.configs,
                externals: &self.externals,
                notifier: None,
            },
            &request,
        )
        .map_err(anyhow::Error::msg)?;
        let child = crate::agent::lock_registry(&self.registry)
            .get(&parsed)
            .map(|handle| Arc::clone(&handle.child))
            .context("recovery_required: spawned child is no longer registered")?;
        let pid = child.lock().process_id();
        write_journal(
            &self.home,
            &attempt.name,
            &ProcessJournal {
                uuid: id.clone(),
                phase: ProcessPhase::Running,
                pid,
                start_token: pid.and_then(crate::process::process_start_token),
            },
        )?;
        Ok(id)
    }

    fn observe(&self, attempt: &Attempt) -> Result<Observation> {
        let Some(id) = self.identity(attempt)? else {
            return Ok(Observation::Missing);
        };
        let snapshot = {
            let registry = crate::agent::lock_registry(&self.registry);
            registry
                .get(&id)
                .map(|handle| (Arc::clone(&handle.child), Arc::clone(&handle.core)))
        };
        let Some((child, core)) = snapshot else {
            return Ok(Observation::Missing);
        };
        if child.lock().try_wait()?.is_some() {
            return Ok(Observation::Exited);
        }
        let state = core.lock().state.current;
        Ok(match state {
            crate::state::AgentState::Starting | crate::state::AgentState::Restarting => {
                Observation::Starting
            }
            crate::state::AgentState::UsageLimit => Observation::UsageLimited,
            crate::state::AgentState::Crashed => Observation::Exited,
            _ => Observation::Running,
        })
    }

    fn stop(&self, attempt: &Attempt) -> Result<bool> {
        let identity = self.identity(attempt)?;
        let (tail, child) = {
            let registry = crate::agent::lock_registry(&self.registry);
            if registry
                .values()
                .any(|h| h.name.as_str() == attempt.name && Some(h.id) != identity)
            {
                bail!("refusing to stop a different worker identity");
            }
            let handle = identity.and_then(|id| registry.get(&id));
            (
                handle.map(|h| h.core.lock().vterm.tail_lines(10000)),
                handle.map(|h| Arc::clone(&h.child)),
            )
        };
        let journal = read_journal(&self.home, &attempt.name)?;
        if let Some(journal) = &journal {
            if let Some(expected) = &attempt.uuid {
                anyhow::ensure!(&journal.uuid == expected, "process journal UUID changed");
            }
            if let Some(id) = identity {
                anyhow::ensure!(journal.uuid == id.full(), "process journal UUID changed");
            }
        }
        if let (Some(journal), Some(child)) = (&journal, &child) {
            if journal.phase == ProcessPhase::Running {
                let pid = child.lock().process_id();
                anyhow::ensure!(
                    pid == journal.pid,
                    "recovery_required: registered process identity changed"
                );
                if let Some(token) = journal.start_token {
                    // An already-exited child can no longer expose a birth token.
                    let exited = child.lock().try_wait()?.is_some();
                    anyhow::ensure!(
                        exited || pid.and_then(crate::process::process_start_token) == Some(token),
                        "recovery_required: registered process birth token changed"
                    );
                }
            }
        }
        if tail.is_none() {
            anyhow::ensure!(journal.as_ref().is_some_and(|j| j.phase == ProcessPhase::Stopped)
                || (journal.is_none() && attempt.uuid.is_none()),
                "recovery_required: worker is not registered and process-tree exit is unproven; cleanup refused");
        }
        if let Some(tail) = tail {
            let logs = self.home.join("schedule_job_logs");
            std::fs::create_dir_all(&logs)?;
            std::fs::write(logs.join(format!("{}.txt", attempt.name)), tail)?;
        }
        let record_exit = || -> std::result::Result<(), String> {
            if let Some(id) = identity
                .map(|id| id.full())
                .or_else(|| journal.as_ref().map(|j| j.uuid.clone()))
            {
                write_journal(
                    &self.home,
                    &attempt.name,
                    &ProcessJournal {
                        uuid: id,
                        phase: ProcessPhase::Stopped,
                        pid: journal.as_ref().and_then(|j| j.pid),
                        start_token: journal.as_ref().and_then(|j| j.start_token),
                    },
                )
                .map_err(|error| {
                    format!("recovery_required: exit receipt could not persist: {error}")
                })?;
            }
            Ok(())
        };
        crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_exit_receipt(
            &self.home,
            &attempt.name,
            Some(&crate::agent_ops::DeleteContext {
                registry: &self.registry,
                configs: &self.configs,
                externals: &self.externals,
                notifier: None,
            }),
            Some((OWNER, identity.as_ref().map(|id| id.full()).as_deref())),
            Some(&record_exit),
        )
        .map_err(anyhow::Error::msg)?;
        Ok(true)
    }

    fn dispatch(&self, run: &Run, attempt: &Attempt) -> Result<()> {
        anyhow::ensure!(
            self.identity(attempt)?.is_some(),
            "cannot dispatch without reserved worker identity"
        );
        let prompt = format!(
            "Execute scheduled job {} (run {}, attempt {}).\n{}\n\nPersistent artifacts and checkpoints: {}\nOutput context: {}\nTask: {}\nResume existing checkpoints and record delivery per destination before retrying. Save results outside the disposable instance workspace. When ALL requested work is complete, call schedule with action=complete, run_id={}, attempt_id={}, result=<nonempty result summary>. Do not report completion merely because work was queued.",
            run.schedule_id, run.id, attempt.number, run.message, run.config.artifact_directory.display(),
            run.config.output_context, run.task_id.as_deref().unwrap_or("pending"), run.id, attempt.number,
        );
        let mut message = crate::inbox::InboxMessage::new_system(OWNER, "task", prompt);
        if let Some(task) = &run.task_id {
            message = message.with_correlation_id(task);
        }
        crate::inbox::notify::enqueue_once_with_idle_hint(
            &self.home,
            &attempt.name,
            message,
            &format!("schedule-job:{}:{}", run.id, attempt.number),
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("job-runtime-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn attempt() -> Attempt {
        Attempt {
            number: 1,
            name: "job-reservation-test".into(),
            uuid: None,
            backend: "codex".into(),
            started_at: 1,
        }
    }

    #[test]
    fn fleet_reservation_reloads_exact_identity_and_rejects_replacement() {
        let home = TempHome::new();
        let mut attempt = attempt();
        let workspace = crate::paths::workspace_dir(home.path()).join(&attempt.name);
        let id = register_worker(home.path(), &attempt, &workspace).unwrap();
        assert_eq!(
            register_worker(home.path(), &attempt, &workspace).unwrap(),
            id
        );
        attempt.uuid = Some(crate::types::InstanceId::new().full());
        assert!(register_worker(home.path(), &attempt, &workspace).is_err());
        let fleet =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home.path())).unwrap();
        assert_eq!(
            fleet.instances[&attempt.name].id.as_deref(),
            Some(id.as_str())
        );
    }

    #[test]
    fn fleet_reservation_never_merges_a_foreign_name() {
        let home = TempHome::new();
        let attempt = attempt();
        crate::fleet::add_instance_to_yaml(
            home.path(),
            &attempt.name,
            &crate::fleet::InstanceYamlEntry {
                backend: Some("claude".into()),
                created_by: Some("operator".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let path = crate::fleet::fleet_yaml_path(home.path());
        let before = std::fs::read(&path).unwrap();
        assert!(register_worker(
            home.path(),
            &attempt,
            &crate::paths::workspace_dir(home.path()).join(&attempt.name)
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn concurrent_reservations_share_one_durable_uuid() {
        let home = TempHome::new();
        let barrier = std::sync::Barrier::new(2);
        let ids = std::thread::scope(|scope| {
            let launch = || {
                barrier.wait();
                let attempt = attempt();
                register_worker(
                    home.path(),
                    &attempt,
                    &crate::paths::workspace_dir(home.path()).join(&attempt.name),
                )
                .unwrap()
            };
            let a = scope.spawn(launch);
            let b = scope.spawn(launch);
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!(ids.0, ids.1);
    }

    #[test]
    fn guarded_delete_rejects_changed_uuid_before_any_cleanup() {
        let home = TempHome::new();
        let attempt = attempt();
        let workspace = crate::paths::workspace_dir(home.path()).join(&attempt.name);
        register_worker(home.path(), &attempt, &workspace).unwrap();
        let path = crate::fleet::fleet_yaml_path(home.path());
        let before = std::fs::read(&path).unwrap();
        let wrong_id = crate::types::InstanceId::new().full();
        let result = crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_expected_identity(
            home.path(), &attempt.name, None, Some((OWNER, Some(&wrong_id))),
        );
        assert!(result.unwrap_err().contains("identity changed"));
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    fn reserved_fixture(home: &Path) -> (ManagedRuntime, Run, Attempt) {
        let attempt = attempt();
        let run = Run {
            id: "r-journal-fixture".into(),
            schedule_id: "s-journal".into(),
            scheduled_at: 1,
            created_by: "operator".into(),
            message: "fixture".into(),
            config: super::super::config::JobConfig {
                backends: vec!["codex".into()],
                artifact_directory: home.join("artifacts"),
                timeout_secs: 60,
                max_attempts: 2,
                retry_delay_secs: 1,
                output_context: String::new(),
                notification: None,
            },
            phase: super::super::Phase::Starting,
            revision: 0,
            attempt: Some(attempt.clone()),
            previous_attempts: vec![],
            task_id: None,
            result: None,
            error: None,
            next_attempt_at: 1,
            deadline: 100,
            cleanup_pending: false,
            task_settled: false,
            notification: super::super::NotificationState::NotRequested,
            notification_receipt: None,
            notification_error: None,
        };
        super::super::mutate(home, |state| {
            state.runs.push(run.clone());
            Ok(())
        })
        .unwrap();
        let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        (
            ManagedRuntime::new(home, &registry, &configs, &externals),
            run,
            attempt,
        )
    }

    #[test]
    fn durable_spawn_intent_without_registry_refuses_restart_and_cleanup() {
        let home = TempHome::new();
        let (runtime, run, attempt) = reserved_fixture(home.path());
        let workspace = crate::paths::workspace_dir(home.path()).join(&attempt.name);
        let id = register_worker(home.path(), &attempt, &workspace).unwrap();
        write_journal(
            home.path(),
            &attempt.name,
            &ProcessJournal {
                uuid: id,
                phase: ProcessPhase::Intent,
                pid: None,
                start_token: None,
            },
        )
        .unwrap();
        assert!(runtime
            .start(&run, &attempt)
            .unwrap_err()
            .to_string()
            .contains("recovery_required"));
        assert!(runtime
            .stop(&attempt)
            .unwrap_err()
            .to_string()
            .contains("recovery_required"));
        assert!(read_journal(home.path(), &attempt.name).unwrap().is_some());
        assert!(crate::fleet::resolve_uuid(home.path(), &attempt.name).is_some());
    }

    #[test]
    fn durable_live_orphan_cannot_be_replaced_or_deleted() {
        let home = TempHome::new();
        let (runtime, run, attempt) = reserved_fixture(home.path());
        let workspace = crate::paths::workspace_dir(home.path()).join(&attempt.name);
        let id = register_worker(home.path(), &attempt, &workspace).unwrap();
        let pid = std::process::id();
        write_journal(
            home.path(),
            &attempt.name,
            &ProcessJournal {
                uuid: id,
                phase: ProcessPhase::Running,
                pid: Some(pid),
                start_token: crate::process::process_start_token(pid),
            },
        )
        .unwrap();
        assert!(runtime
            .start(&run, &attempt)
            .unwrap_err()
            .to_string()
            .contains("recovery_required"));
        assert!(runtime
            .stop(&attempt)
            .unwrap_err()
            .to_string()
            .contains("recovery_required"));
        assert!(crate::process::is_pid_alive(pid));
    }

    #[test]
    fn exit_receipt_runs_before_ancillary_cleanup_and_failure_retains_fleet() {
        let home = TempHome::new();
        let (runtime, _, attempt) = reserved_fixture(home.path());
        let workspace = crate::paths::workspace_dir(home.path()).join(&attempt.name);
        let id = register_worker(home.path(), &attempt, &workspace).unwrap();
        let record_exit = || {
            write_journal(
                home.path(),
                &attempt.name,
                &ProcessJournal {
                    uuid: id.clone(),
                    phase: ProcessPhase::Stopped,
                    pid: None,
                    start_token: None,
                },
            )
            .unwrap();
            Err("injected post-exit receipt failure".to_string())
        };
        let result =
            crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_exit_receipt(
                home.path(),
                &attempt.name,
                Some(&crate::agent_ops::DeleteContext {
                    registry: &runtime.registry,
                    configs: &runtime.configs,
                    externals: &runtime.externals,
                    notifier: None,
                }),
                Some((OWNER, Some(&id))),
                Some(&record_exit),
            );
        assert!(result.unwrap_err().contains("injected post-exit"));
        assert!(
            read_journal(home.path(), &attempt.name)
                .unwrap()
                .unwrap()
                .phase
                == ProcessPhase::Stopped
        );
        assert!(crate::fleet::resolve_uuid(home.path(), &attempt.name).is_some());
        // Reconstructed runtime can repeat ancillary cleanup using the receipt.
        runtime.stop(&attempt).unwrap();
        assert!(crate::fleet::resolve_uuid(home.path(), &attempt.name).is_none());
    }
}
