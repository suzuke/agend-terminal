//! Operator recovery CLI must refuse ordinary agent contexts before API access.

#![allow(clippy::unwrap_used)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_GRACE: Duration = Duration::from_secs(3);
const DAEMON_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Own a spawned daemon from the instant `spawn()` succeeds. Drop never waits
/// indefinitely for a graceful stop: it polls for a bounded grace period,
/// kills if still alive, then reaps the direct child.
struct ChildGuard {
    child: Option<Child>,
    reap_receipt: Option<Arc<Mutex<Option<ExitStatus>>>>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            reap_receipt: None,
        }
    }

    fn with_reap_receipt(child: Child, reap_receipt: Arc<Mutex<Option<ExitStatus>>>) -> Self {
        Self {
            child: Some(child),
            reap_receipt: Some(reap_receipt),
        }
    }

    fn record_reaped(&self, status: ExitStatus) {
        if let Some(receipt) = &self.reap_receipt {
            *receipt.lock().unwrap() = Some(status);
        }
    }

    fn wait_bounded(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.child.as_mut().map_or(Ok(None), Child::try_wait);
            match status {
                Ok(Some(status)) => {
                    self.child.take();
                    self.record_reaped(status);
                    return true;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    }

    fn kill_and_reap(&mut self) -> bool {
        let status = {
            let Some(child) = self.child.as_mut() else {
                return true;
            };
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    child.wait().ok()
                }
            }
        };
        if let Some(status) = status {
            self.child.take();
            self.record_reaped(status);
            true
        } else {
            false
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.wait_bounded(DAEMON_GRACE) {
            let _ = self.kill_and_reap();
        }
    }
}

struct OutputCapture {
    path: PathBuf,
    reader: File,
}

impl OutputCapture {
    fn new(label: &str) -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "agend-admin-job-recovery-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            reader: file.try_clone()?,
        })
    }

    fn stdio(&self) -> io::Result<Stdio> {
        Ok(Stdio::from(self.reader.try_clone()?))
    }

    fn read(&mut self) -> io::Result<Vec<u8>> {
        self.reader.seek(SeekFrom::Start(0))?;
        let mut output = Vec::new();
        self.reader.read_to_end(&mut output)?;
        Ok(output)
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run a short-lived CLI with a hard deadline. Output is collected only after
/// the child exits; timeout cleanup kills and reaps before returning, so a
/// blocked child cannot strand this test process.
fn run_cli_bounded(mut command: Command) -> std::io::Result<Output> {
    let mut stdout_capture = OutputCapture::new("stdout")?;
    let mut stderr_capture = OutputCapture::new("stderr")?;
    command
        .stdout(stdout_capture.stdio()?)
        .stderr(stderr_capture.stdio()?);
    let child = command.spawn()?;
    let mut child = ChildGuard::new(child);
    let deadline = Instant::now() + CLI_TIMEOUT;
    loop {
        let status = child.child.as_mut().map_or(Ok(None), Child::try_wait);
        match status {
            Ok(Some(status)) => {
                child.child.take();
                let stdout = stdout_capture.read()?;
                let stderr = stderr_capture.read()?;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill_and_reap();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "CLI command exceeded bounded test timeout",
                ));
            }
            Err(error) => {
                let _ = child.kill_and_reap();
                return Err(error);
            }
        }
    }
}

fn daemon_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agend-terminal"));
    command
        .args(["start", "--foreground", "--fleet"])
        .arg(home.join("fleet.yaml"))
        .env("AGEND_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn recovery_admin_cli_refuses_agent_environment_before_connecting() {
    let home =
        std::env::temp_dir().join(format!("agend-admin-job-recovery-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agend-terminal"));
    command
        .args([
            "admin",
            "resolve-job-recovery",
            "j-test",
            "--attempt",
            "1",
            "--cleanup-confirmed",
            "--result",
            "external cleanup confirmed",
        ])
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "job-worker-test")
        .current_dir(&home);
    let output = run_cli_bounded(command).unwrap();
    let mutated_store = home.join("schedule-jobs.json").exists();
    std::fs::remove_dir_all(&home).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("operator-only"),
        "unexpected refusal: {stderr}"
    );
    assert!(
        !stderr.contains("no active daemon"),
        "must refuse before API access: {stderr}"
    );
    assert!(!mutated_store);
}

#[test]
fn recovery_admin_cli_resolves_seeded_run_against_isolated_daemon() {
    let home = std::env::temp_dir().join(format!(
        "agend-admin-job-recovery-success-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("fleet.yaml"), "instances: {}\n").unwrap();
    std::fs::create_dir_all(home.join("artifacts")).unwrap();
    std::fs::write(
        home.join("schedule-jobs.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "watermarks": {},
            "overlap_skips": {},
            "runs": [{
                "id": "r-cli-success",
                "schedule_id": "s-cli-success",
                "scheduled_at": 1,
                "created_by": "operator",
                "message": "isolated recovery fixture",
                "config": {
                    "backends": ["codex"],
                    "artifact_directory": home.join("artifacts"),
                    "timeout_secs": 60,
                    "max_attempts": 1,
                    "retry_delay_secs": 1,
                    "output_context": "",
                    "notification": null
                },
                "phase": "stopping",
                "revision": 0,
                "attempt": {
                    "number": 1,
                    "name": "worker-cli-success",
                    "uuid": null,
                    "backend": "codex",
                    "started_at": 1
                },
                "previous_attempts": [],
                "task_id": null,
                "result": null,
                "error": "external worker stopped",
                "next_attempt_at": 1,
                "deadline": 2,
                "cleanup_pending": true,
                "recovery_required": true,
                "recovery_resolution": null,
                "task_settled": false,
                "notification": "not_requested",
                "notification_receipt": null,
                "notification_error": null
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let daemon = daemon_command(&home).spawn().unwrap();
    let mut daemon = ChildGuard::new(daemon);
    let run_dir = home.join("run");
    let mut api_ready = false;
    for _ in 0..100 {
        if std::fs::read_dir(&run_dir)
            .ok()
            .into_iter()
            .flatten()
            .any(|entry| {
                entry
                    .ok()
                    .is_some_and(|entry| entry.path().join("api.port").exists())
            })
        {
            api_ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(api_ready, "isolated daemon did not publish api.port");

    let mut command = Command::new(env!("CARGO_BIN_EXE_agend-terminal"));
    command
        .args([
            "admin",
            "resolve-job-recovery",
            "r-cli-success",
            "--attempt",
            "1",
            "--cleanup-confirmed",
            "--result",
            "operator verified worker stopped and delivery reconciled",
        ])
        .env("AGEND_HOME", &home)
        .current_dir(&home);
    let output = run_cli_bounded(command).unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("schedule-jobs.json")).unwrap()).unwrap();
    assert_eq!(state["runs"][0]["recovery_required"], false);
    assert_eq!(state["runs"][0]["phase"], "failed");

    let mut stop = Command::new(env!("CARGO_BIN_EXE_agend-terminal"));
    stop.arg("stop").env("AGEND_HOME", &home);
    let stop_output = run_cli_bounded(stop).unwrap();
    assert!(
        stop_output.status.success(),
        "stop stdout={} stderr={}",
        String::from_utf8_lossy(&stop_output.stdout),
        String::from_utf8_lossy(&stop_output.stderr)
    );
    assert!(
        daemon.wait_bounded(DAEMON_STOP_TIMEOUT),
        "isolated daemon did not stop within bounded timeout"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn recovery_fixture_reaps_daemon_when_observation_panics() {
    let home = std::env::temp_dir().join(format!(
        "agend-admin-job-recovery-failure-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("fleet.yaml"), "instances: {}\n").unwrap();

    let reap_receipt = Arc::new(Mutex::new(None));
    let receipt = Arc::clone(&reap_receipt);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let daemon = daemon_command(&home).spawn().unwrap();
        let _daemon = ChildGuard::with_reap_receipt(daemon, receipt);
        panic!("controlled fixture observation failure");
    }));

    assert!(result.is_err(), "control must exercise panic cleanup");
    assert!(
        reap_receipt.lock().unwrap().is_some(),
        "ChildGuard must observe an owned daemon exit status during unwinding"
    );
    std::fs::remove_dir_all(&home).unwrap();
}
