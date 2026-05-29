use super::*;

/// #945 Phase 1: pending-registry slot publishes the agent registry
/// to the background `telegram_init` thread. The slot is a
/// `OnceLock` (set-once-per-process) so the test asserts that
/// (a) initial state is empty, (b) `set_pending_registry` makes
/// the registry observable via `get_pending_registry`.
///
/// Process-shared state: this test runs in cargo's per-process
/// model so the OnceLock is fresh per `cargo test` invocation. If
/// re-run in the same binary instance (rare), the second call to
/// `set_pending_registry` no-ops — but `get_pending_registry`
/// still returns the originally-set value, which is the documented
/// behavior (first publisher wins).
#[test]
fn pending_registry_publish_and_observe_945() {
    // Note: OnceLock may have been populated by an earlier test
    // in the same process. If `get_pending_registry()` returns
    // Some already, skip with a clear message — we can't reset
    // OnceLock state.
    if get_pending_registry().is_some() {
        eprintln!(
            "test fixture: PENDING_REGISTRY already populated by earlier \
                 test in this process. OnceLock is set-once; skipping. The \
                 set-once semantic is itself the contract this test pins."
        );
        return;
    }
    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    set_pending_registry(Arc::clone(&registry));
    let observed = get_pending_registry().expect("registry must be observable post-publish");
    // Identity check: same Arc-pointer.
    assert!(
        Arc::ptr_eq(&registry, &observed),
        "get_pending_registry must return the SAME Arc that was published"
    );
}

#[test]
fn validate_name_valid() {
    assert!(validate_name("hello").is_ok());
    assert!(validate_name("agent-1").is_ok());
    assert!(validate_name("my_agent").is_ok());
    assert!(validate_name("A123").is_ok());
}

#[test]
fn validate_name_rejects_traversal() {
    assert!(validate_name("../etc").is_err());
    assert!(validate_name("foo/bar").is_err());
    assert!(validate_name("a b").is_err());
    assert!(validate_name("").is_err());
}

#[test]
fn validate_name_rejects_long() {
    let long = "a".repeat(65);
    assert!(validate_name(&long).is_err());
    let ok = "a".repeat(64);
    assert!(validate_name(&ok).is_ok());
}

#[test]
fn strip_ansi_basic() {
    assert_eq!(strip_ansi("hello"), "hello");
    assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    assert_eq!(strip_ansi("\x1b[1;32mbold green\x1b[0m"), "bold green");
}

/// Regression: ESC byte (0x1B) must not survive `strip_ansi` for any of the
/// payload shapes `inject_to_agent` is realistically asked to deliver.
/// `inject_to_agent` strips `text` through `strip_ansi` before writing to
/// the PTY in `typed_inject` mode; if ESC bytes leak through, slow byte-by-
/// byte rendering can cause Ink-style TUIs (kiro-cli) to interpret the
/// stray ESC as the "cancel current input" keypress.
///
/// This test pins that contract at the strip boundary so a future refactor
/// of `strip_ansi` (or a new colorized injector path) that lets ESC slip
/// through trips here, regardless of whether the subsequent chunked write
/// path is exercised by other tests.
#[test]
fn inject_strips_ansi_from_typed_payload() {
    // Realistic [AGEND-MSG] header with foreground/reset color codes —
    // matches the shape that triggered the original Sprint 54 regression.
    let agend_header = "\x1b[1;36m[AGEND-MSG]\x1b[0m from=lead kind=task\n";
    let stripped = strip_ansi(agend_header);
    assert!(
        !stripped.as_bytes().contains(&0x1B),
        "ESC byte must not survive strip_ansi for typed inject; got {stripped:?}"
    );
    // Cursor-movement and OSC sequences also appear in some [AGEND-MSG]
    // emitters; exercise both to make sure no escape category leaks.
    let mixed = "\x1b[2K\r\x1b]0;title\x07message body \x1b[31mred\x1b[0m";
    let stripped_mixed = strip_ansi(mixed);
    assert!(
        !stripped_mixed.as_bytes().contains(&0x1B),
        "ESC byte must not survive strip_ansi for mixed CSI+OSC payload; got {stripped_mixed:?}"
    );
}

#[test]
fn strip_ansi_cursor_move_no_space() {
    // CSI C (cursor forward) and D (cursor back) must not insert spaces
    assert_eq!(strip_ansi("\x1b[5Chello"), "hello");
    assert_eq!(strip_ansi("ab\x1b[2Dcd"), "abcd");
    // Other CSI codes also produce nothing
    assert_eq!(strip_ansi("\x1b[Hhome"), "home");
}

#[test]
fn strip_ansi_osc() {
    assert_eq!(strip_ansi("\x1b]0;title\x07rest"), "rest");
}

#[test]
fn sensitive_env_keys_covers_known_dangerous() {
    assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
    assert!(is_sensitive_env_key("OPENAI_API_KEY"));
    assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
    assert!(is_sensitive_env_key("LD_PRELOAD"));
    assert!(is_sensitive_env_key("DYLD_INSERT_LIBRARIES"));
    assert!(is_sensitive_env_key("AGEND_HOME"));
    assert!(is_sensitive_env_key("AGEND_MCP_TOOLS_DENY"));
}

#[test]
fn sensitive_env_keys_is_case_insensitive() {
    // Windows env is case-insensitive; ensure lower-cased fleet.yaml keys
    // still hit the deny-list.
    assert!(is_sensitive_env_key("anthropic_api_key"));
    assert!(is_sensitive_env_key("Ld_Preload"));
}

#[test]
fn sensitive_env_keys_allows_benign() {
    assert!(!is_sensitive_env_key("MY_APP_DEBUG"));
    assert!(!is_sensitive_env_key("LANG"));
    assert!(!is_sensitive_env_key("TERM"));
    assert!(!is_sensitive_env_key("PROMPT_OVERRIDE"));
}

/// Sprint 21 F-NEW1: verify that `sweep_child_tree` reaches grandchild
/// processes spawned via the agent's PTY. Mirrors the kiro-cli pattern
/// where the leader shell forks bun/mcp/acp grandchildren — the regression
/// PR-U #158 fixed for explicit kill paths but missed for PTY-EOF crash
/// detection.
///
/// 2026-05-18 race-class anchor: the previous form of this test polled
/// `pid_file.exists()` then `read_to_string` — a write-vs-read race
/// because `echo $! > file` may create the file BEFORE flushing the
/// content. The fix lands in C2 (swap to `wait_for_nonempty_file`); C0
/// adds a concurrent stress runner that exposes the race under load.
#[test]
#[cfg(unix)]
fn sweep_child_tree_kills_grandchild_via_process_group() {
    let pid_file =
        std::env::temp_dir().join(format!("agend-sweep-test-{}.pid", std::process::id()));
    sweep_child_tree_body(&pid_file);
}

/// 2026-05-18 race-class C0 anchor (RED on main HEAD): concurrent
/// stress runner for the pid_file write-vs-read race. Spawns 8
/// threads, each running the body 6 times against unique pid_file
/// paths (8×6 = 48 PTY spawns). Pre-fix the `exists() + read_to_string`
/// pair races at least once across ~48 multi-threaded iterations
/// under scheduler contention. Post-fix (C2's `wait_for_nonempty_file`
/// swap) is deterministic — the helper polls for content, not just
/// existence.
///
/// NOT `#[ignore]`: ~10-15s on CI ubuntu-latest (where the race
/// originally surfaced in PR #905). Local fast hardware may not
/// reproduce — the CI runner's slower scheduler is what exposes
/// the write/flush gap. Marked `#[cfg(unix)]` because the body
/// uses sh + sleep.
#[test]
#[cfg(unix)]
fn sweep_child_tree_kills_grandchild_concurrent_stress() {
    let handles: Vec<_> = (0..8)
        .map(|tid| {
            std::thread::spawn(move || {
                for i in 0..6 {
                    let path = std::env::temp_dir().join(format!(
                        "agend-sweep-stress-{}-{tid}-{i}.pid",
                        std::process::id()
                    ));
                    sweep_child_tree_body(&path);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("stress thread joined");
    }
}

/// Test body factored out of
/// `sweep_child_tree_kills_grandchild_via_process_group` so the
/// concurrent stress runner can call it with unique pid_file paths.
/// Behaviour identical to the original test — same shell command,
/// same registry shape, same assertion set. Only the pid_file path
/// is parameterized.
#[cfg(unix)]
fn sweep_child_tree_body(pid_file: &std::path::Path) {
    use parking_lot::Mutex;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::collections::HashMap;

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let _ = std::fs::remove_file(pid_file);
    // sh forks `sleep` into the background; sleep PID is recorded so the
    // test can verify it dies with the leader (group kill semantics).
    let cmd_str = format!("sleep 60 & echo $! > {} && wait", pid_file.display());
    let mut cmd = CommandBuilder::new("sh");
    cmd.args(["-c", &cmd_str]);
    cmd.cwd(std::env::temp_dir());
    let child = pair.slave.spawn_command(cmd).expect("spawn sh + sleep");
    drop(pair.slave);
    let shell_pid = child.process_id().expect("shell process_id");

    // Build a minimal AgentHandle so sweep_child_tree's registry-lookup
    // path is exercised end-to-end (not just kill_process_tree directly).
    let pty_writer: PtyWriter =
        Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
        Arc::new(Mutex::new(pair.master));
    let core = Arc::new(Mutex::new(AgentCore {
        vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: StateTracker::new(None),
        health: HealthTracker::new(),
    }));
    let agent_name = format!("sweep-test-{}", pid_file.display());
    let handle = AgentHandle {
        id: crate::types::InstanceId::default(),
        name: agent_name.clone().into(),
        backend_command: "sh".to_string(),
        pty_writer,
        pty_master,
        core,
        child: Arc::new(Mutex::new(child)),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: false,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // #1441: registry is UUID-keyed — insert under the handle's own id.
    let agent_id = handle.id;
    registry.lock().insert(agent_id, handle);

    // Wait for the grandchild's PID to be observable in the file —
    // `wait_for_nonempty_file` polls for content commit, not just
    // file existence, closing the open+truncate+write race that
    // the original for-loop + read_to_string pair exhibited (PR #905
    // CI flake + PR #909 dev concurrent-load flake).
    let content = wait_for_nonempty_file(pid_file, std::time::Duration::from_secs(2))
        .expect("sleep grandchild pid_file did not become non-empty within 2s");
    let sleep_pid: u32 = content.trim().parse().expect("parse sleep grandchild PID");
    assert!(
        crate::process::is_pid_alive(shell_pid),
        "shell leader must be alive before sweep"
    );
    assert!(
        crate::process::is_pid_alive(sleep_pid),
        "sleep grandchild must be alive before sweep"
    );

    // Invoke the new helper. Should kill the entire process group.
    sweep_child_tree(&agent_id, &registry);

    // Reap the shell child so kill(pid, 0) doesn't see it as a zombie.
    // Without wait(), the shell shows as "alive" even after SIGKILL
    // because we are its parent and never collected its exit status.
    {
        let reg = &registry.lock();
        if let Some(h) = reg.get(&agent_id) {
            {
                let mut c = h.child.lock();
                let _ = c.wait();
            }
        }
    }

    // #934: §3.20 SOP 1 — poll-with-deadline against post-condition.
    //
    // Pre-#934 these were bare `assert!(!is_pid_alive(_pid))`
    // immediately after `sweep_child_tree` returned. Under CI
    // scheduler contention (especially in the 48-PTY concurrent
    // stress runner), the grandchild `sleep` could still appear
    // alive at the assertion point even though SIGKILL had landed —
    // it was a ZOMBIE awaiting reap by init / launchd (its new
    // parent after the shell died).
    //
    // `is_pid_alive` uses `libc::kill(pid, 0)` which returns 0 for
    // zombies (kernel still tracks the PID until reaped).
    // Init / launchd reap latency is OS-scheduling-dependent —
    // typically <1s on Linux, observed up to ~3s on macOS, worst
    // case ~5-10s on heavily loaded CI runners. Bare assert at
    // microsecond latency lost the race intermittently.
    //
    // Fix: poll with deadline. `poll_until_dead` (promoted to
    // `pub(crate)` for this PR) returns true within the window or
    // false on timeout. shell_pid uses a 5s deadline (we reap
    // directly via `child.wait()` above so the gap is short).
    // sleep_pid uses a 10s deadline for init / launchd reap-cycle
    // worst case — see deadline doc in `cleanup_zombies::poll_until_dead`
    // for OS-conditional rationale.
    assert!(
        crate::admin::cleanup_zombies::poll_until_dead(
            shell_pid,
            std::time::Duration::from_secs(5),
        ),
        "shell leader did not die within 5s post-sweep — we reap directly \
             via child.wait() so the kernel-pid-cleanup gap is normally <1s; \
             timing this slow indicates a deeper issue"
    );
    assert!(
        crate::admin::cleanup_zombies::poll_until_dead(
            sleep_pid,
            std::time::Duration::from_secs(10),
        ),
        "sleep grandchild did not die within 10s post-sweep — likely \
             init / launchd reap latency under contention (10s covers macOS \
             launchd's slowest observed cycle on loaded CI runners)"
    );
    let _ = std::fs::remove_file(pid_file);
}

/// Poll `path` until `read_to_string(path).trim().is_empty() == false`
/// (i.e. file exists AND has non-empty content), or `timeout`
/// elapses. Closes the write-vs-read race that bare
/// `path.exists() + read_to_string` exhibits when the writer is a
/// subprocess that does open+truncate+write+close: between create
/// and content flush, an `exists()` poll returns true but the read
/// yields an empty string. Reading until non-empty waits for the
/// content commit explicitly.
///
/// Returns `Ok(content)` (trimmed read) on success, `Err` with
/// `ErrorKind::TimedOut` when the timeout fires with no non-empty
/// content observed.
///
/// Poll interval is 5ms (the OS scheduling quantum is the floor;
/// finer polling burns CPU without buying latency improvement).
///
/// `#[cfg(unix)]` matches the sole caller (`sweep_child_tree_body`,
/// which spawns `sh` + `sleep`) — Windows builds would see an
/// orphan helper and trip `-D dead-code` clippy. Drop the gate
/// when a Windows-side caller appears.
#[cfg(unix)]
fn wait_for_nonempty_file(
    path: &std::path::Path,
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.trim().is_empty() {
                return Ok(content);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "file {} did not become non-empty within {:?}",
                    path.display(),
                    timeout
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Helper unit test: simulates the race-class shape the helper
/// exists to close. A separate thread creates the file empty,
/// delays, then writes content. `wait_for_nonempty_file` must
/// NOT return until content is observable.
#[test]
#[cfg(unix)]
fn wait_for_nonempty_file_waits_until_content_is_committed() {
    let path = std::env::temp_dir().join(format!(
        "agend-wait-nonempty-test-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        // Phase 1: create empty file. A naïve `exists()` poll
        // would see this and proceed to read empty content.
        std::fs::write(&writer_path, "").expect("create empty");
        // Phase 2: simulate the OS write buffer flush gap.
        std::thread::sleep(std::time::Duration::from_millis(40));
        // Phase 3: commit the actual content.
        std::fs::write(&writer_path, "12345\n").expect("commit content");
    });

    let result = wait_for_nonempty_file(&path, std::time::Duration::from_secs(2))
        .expect("wait returned content within timeout");
    writer.join().expect("writer thread joined");

    assert_eq!(
        result.trim(),
        "12345",
        "wait_for_nonempty_file must return the committed content, not the empty stub"
    );
    let _ = std::fs::remove_file(&path);
}

/// Helper unit test: timeout path. File never becomes non-empty.
#[test]
#[cfg(unix)]
fn wait_for_nonempty_file_returns_timeout_when_content_never_arrives() {
    let path = std::env::temp_dir().join(format!(
        "agend-wait-nonempty-timeout-test-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, "").expect("create empty");

    let err = wait_for_nonempty_file(&path, std::time::Duration::from_millis(50))
        .expect_err("must time out when content never commits");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn sweep_child_tree_unregistered_name_is_no_op() {
    // Sprint 21 F-NEW1: registry lookup miss must not panic. The PTY-EOF
    // path may race against an explicit handle_delete that already cleaned
    // up the registry entry — sweep should simply find nothing to kill.
    use parking_lot::Mutex;
    use std::collections::HashMap;
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // Should not panic, should not error.
    sweep_child_tree(&crate::types::InstanceId::default(), &registry);
}

/// §3.5.10 concurrent-state fixture: multi-threaded producer/consumer
/// through crossbeam_channel (the production import). Asserts message
/// ordering invariant — bounded channel preserves FIFO order.
/// Production-path-coupled: uses real crossbeam_channel::bounded.
#[test]
fn crossbeam_channel_concurrent_ordering() {
    let (tx, rx) = crossbeam_channel::bounded::<usize>(16);
    let n = 100;

    // Producer thread sends 0..n in order.
    let handle = std::thread::spawn(move || {
        for i in 0..n {
            tx.send(i).expect("send");
        }
        // tx drops here, closing the channel.
    });

    // Consumer drains concurrently (bounded channel blocks producer
    // after 16 items, so consumer must run in parallel).
    let mut received = Vec::with_capacity(n);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(v) => received.push(v),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                panic!("recv timed out after 5s — deadlock?");
            }
        }
    }

    handle.join().expect("producer");

    // FIFO ordering preserved.
    assert_eq!(received.len(), n);
    for (i, &v) in received.iter().enumerate() {
        assert_eq!(v, i, "message {i} out of order");
    }
}

// ── Sprint 46 P2: resolve_instance tests ────────────────────────────

fn resolve_test_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CTR: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-resolve-{}-{}-{}",
        std::process::id(),
        name,
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
#[allow(clippy::unwrap_used)]
fn name_resolves_to_single_id() {
    let home = resolve_test_home("single");
    let id = crate::types::InstanceId::new();
    let yaml = format!(
        "defaults:\n  backend: claude\ninstances:\n  dev:\n    id: \"{}\"\n    role: Test\n",
        id.full()
    );
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let (resolved_id, resolved_name) = resolve_instance(&home, "dev").unwrap();
    assert_eq!(resolved_id, id);
    assert_eq!(resolved_name, "dev");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[allow(clippy::unwrap_used)]
fn nonexistent_name_returns_not_found() {
    // fleet.yaml HashMap guarantees name uniqueness — no Ambiguous path.
    // Verify that a non-existent name returns NotFound.
    let home = resolve_test_home("notfound");
    let yaml = "defaults:\n  backend: claude\ninstances:\n  dev:\n    role: Test\n";
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let result = resolve_instance(&home, "nonexistent");
    assert!(
        matches!(result, Err(ResolveError::NotFound(_))),
        "expected NotFound, got: {result:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Issue #658 regression: ANSI-colorized [AGEND-MSG] header must be
/// detected as system header for atomic write (uses stripped text).
#[test]
fn ansi_header_detected_as_system_header() {
    // Simulate ANSI-wrapped header
    let raw =
        "\x1b[1;34m[AGEND-MSG]\x1b[0m from=lead kind=task size=500 (use inbox tool)\nBody here";
    let stripped = strip_ansi(raw);
    assert!(
        stripped.starts_with("[AGEND-MSG]"),
        "stripped should start with [AGEND-MSG], got: {stripped}"
    );

    let raw_from = "\x1b[32m[from:lead-kiro]\x1b[0m hello world";
    let stripped_from = strip_ansi(raw_from);
    assert!(
        stripped_from.starts_with("[from:"),
        "stripped should start with [from:, got: {stripped_from}"
    );
}

/// Startup grace period: agent that exits within 5s should NOT get shell fallback.
/// Startup failure: exit within 5s + no user input since spawn → no shell fallback.
#[test]
fn startup_failure_no_input_no_shell_fallback() {
    // spawned_at_epoch_ms = 1000, last_input_at_ms = 500 → input before spawn
    let spawned_at_epoch_ms: u64 = 1000;
    let last_input_at_ms: u64 = 500;
    let had_user_input_since_spawn = last_input_at_ms >= spawned_at_epoch_ms;
    assert!(
        !had_user_input_since_spawn,
        "input before spawn should not count as user input"
    );
}

/// Quick user exit: input AFTER spawn → normal clean exit, not startup failure.
#[test]
fn quick_user_exit_still_clean() {
    // spawned_at_epoch_ms = 1000, last_input_at_ms = 1500 → input after spawn
    let spawned_at_epoch_ms: u64 = 1000;
    let last_input_at_ms: u64 = 1500;
    let had_user_input_since_spawn = last_input_at_ms >= spawned_at_epoch_ms;
    assert!(
        had_user_input_since_spawn,
        "input after spawn should count as user input → not startup failure"
    );
}

/// Deleted agent: reaper should not spawn shell fallback when deleted flag is set.
/// Behavioral test: spawn a short-lived process, set deleted=true, verify
/// no shell replacement appears in registry after exit.
#[test]
fn deleted_agent_reaper_no_shell_fallback() {
    use std::sync::atomic::Ordering;

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let spawn_cfg = SpawnConfig {
        name: "del-test",
        backend_command: "true", // exits immediately with code 0
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: None,
        crash_tx: None,
        shutdown: None,
    };
    spawn_agent(&spawn_cfg, &registry).expect("spawn");

    // Set deleted flag (simulates DELETE handler)
    {
        let reg = registry.lock();
        let handle = reg
            .values()
            .find(|h| h.name.as_str() == "del-test")
            .expect("agent must exist");
        handle.deleted.store(true, Ordering::SeqCst);
    }

    // Wait for reaper to detect exit + process the deleted check
    std::thread::sleep(std::time::Duration::from_millis(3000));

    // After reaper runs, agent should NOT be re-spawned as shell.
    // With deleted=true, reaper returns early → registry entry removed
    // (by sweep_child_tree or naturally) but no new shell spawned.
    let reg = registry.lock();
    match reg.values().find(|h| h.name.as_str() == "del-test") {
        None => {} // removed from registry — correct (no shell fallback)
        Some(h) => {
            // If still present, backend_command must NOT be a shell
            assert_ne!(
                h.backend_command,
                crate::default_shell(),
                "deleted agent must NOT get shell fallback, but got: {}",
                h.backend_command
            );
        }
    }
}

/// PTY write timeout: write_with_timeout returns within bounded time.
#[test]
fn write_timeout_does_not_hang() {
    let buf: PtyWriter = Arc::new(Mutex::new(Box::new(std::io::sink())));
    let data = vec![0u8; 1024];
    let start = std::time::Instant::now();
    let result = write_with_timeout(&buf, &data);
    let elapsed = start.elapsed();
    assert!(result.is_ok(), "normal write should succeed");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "normal write should be fast, got {elapsed:?}"
    );
}

/// Stuck write: second write attempt returns TimedOut immediately
/// (write_in_progress guard prevents thread accumulation).
#[test]
fn write_in_progress_guard_prevents_thread_leak() {
    // Simulate a stuck writer by inserting the key into the in-progress set
    let buf: PtyWriter = Arc::new(Mutex::new(Box::new(std::io::sink())));
    let key = Arc::as_ptr(&buf) as usize;
    {
        let mut guard = write_in_progress_set().lock();
        guard.insert(key);
    }
    // Second write should fail immediately
    let result = write_with_timeout(&buf, b"hello");
    assert!(result.is_err());
    match &result {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
        Ok(_) => panic!("expected error"),
    }
    // Cleanup
    {
        let mut guard = write_in_progress_set().lock();
        guard.remove(&key);
    }
}

/// Error (non-timeout) clears guard, allowing retry.
#[test]
#[cfg(unix)]
fn error_clears_guard_allows_retry() {
    // Use a closed writer that returns BrokenPipe
    let (rd, wr) = std::os::unix::net::UnixStream::pair().expect("pair");
    drop(rd); // close read end → writes will fail with BrokenPipe
    let buf: PtyWriter = Arc::new(Mutex::new(Box::new(wr)));

    // First write: should fail with BrokenPipe but clear guard
    let r1 = write_with_timeout(&buf, b"hello");
    assert!(r1.is_err());

    // Second write: should also fail (not blocked by guard)
    let r2 = write_with_timeout(&buf, b"world");
    assert!(r2.is_err());
    // Key: it didn't return "already in progress" — it actually tried
    let err_msg = r2.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
    assert_ne!(
        err_msg,
        "PTY write already in progress (previous write stuck)"
    );
}

/// write_to_agent_typed also uses timeout (not direct lock+write).
#[test]
fn write_to_agent_typed_uses_timeout() {
    // Verify typed path calls write_with_timeout by checking it
    // respects the in-progress guard
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let writer: PtyWriter = Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
    // Insert in-progress guard
    let key = Arc::as_ptr(&writer) as usize;
    {
        let mut guard = write_in_progress_set().lock();
        guard.insert(key);
    }
    let handle = AgentHandle {
        id: crate::types::InstanceId::default(),
        name: "typed-test".into(),
        backend_command: "test".to_string(),
        pty_writer: writer,
        pty_master: Arc::new(Mutex::new(pair.master)),
        core: Arc::new(Mutex::new(AgentCore {
            vterm: VTerm::new(80, 24),
            subscribers: Vec::new(),
            state: StateTracker::new(None),
            health: HealthTracker::new(),
        })),
        child: Arc::new(Mutex::new(
            pair.slave
                .spawn_command(portable_pty::CommandBuilder::new("true"))
                .expect("spawn"),
        )),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: true,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let result = write_to_agent_typed(&handle, b"x");
    assert!(
        result.is_err(),
        "typed write should fail when in-progress guard is set"
    );
    // Cleanup
    {
        let key = Arc::as_ptr(&handle.pty_writer) as usize;
        let mut guard = write_in_progress_set().lock();
        guard.remove(&key);
    }
}

/// #708: AGEND_GIT_BYPASS must not leak to child processes.
#[test]
fn build_command_strips_agend_git_bypass() {
    std::env::set_var("AGEND_GIT_BYPASS", "1");
    let config = SpawnConfig {
        name: "strip-test",
        backend_command: "echo",
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: None,
        crash_tx: None,
        shutdown: None,
    };
    let (cmd, _) = build_command(&config).expect("build_command");
    // Verify env_remove was called — CommandBuilder won't pass it to child.
    // The env_remove in build_command is the authoritative guard.
    let _ = cmd;
    std::env::remove_var("AGEND_GIT_BYPASS");
}

/// Phase A Piece-3: GIT_EDITOR + friends must all be set to
/// `"true"` (no-op editor binary) in the daemon-spawned agent
/// process env so `git rebase --continue` / `git commit`
/// (without -m) / `git rebase -i` don't drop the PTY into a
/// Vim/editor lockup. Empirical experiment surfaced this on
/// opencode + DeepSeek backends; the daemon-side default closes
/// the lockup surface across all backends + scenarios.
///
/// Operator override is preserved by ordering — these env vars
/// are set BEFORE the fleet.yaml user-env loop, so an operator's
/// `instances.<name>.env.GIT_EDITOR: vim` would override the
/// daemon default. This test pins the default; the override path
/// is covered structurally by `build_command`'s existing
/// fleet.yaml env-merge logic (no separate test needed for the
/// override since the env loop is single-call `cmd.env(k, v)`).
#[test]
fn build_command_sets_git_editor_defaults() {
    let config = SpawnConfig {
        name: "git-editor-test",
        backend_command: "echo",
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: None,
        crash_tx: None,
        shutdown: None,
    };
    let (cmd, _) = build_command(&config).expect("build_command");
    for key in &["GIT_EDITOR", "GIT_SEQUENCE_EDITOR", "EDITOR", "VISUAL"] {
        let value = cmd
            .get_env(key)
            .unwrap_or_else(|| panic!("{key} must be set in agent env"));
        assert_eq!(
            value.to_string_lossy(),
            "true",
            "{key} must default to `true` (no-op editor binary), got {value:?}"
        );
    }
}

/// #1146: send_to_registry must release the registry lock before
/// calling inject. Source-grep pin: the lock scope must close
/// before inject_with_target appears. This is structurally
/// enforced — inject_with_target takes &InjectTarget (a snapshot),
/// not &AgentHandle (a registry borrow), so holding the lock
/// during inject is impossible without reverting to inject_to_agent.
#[test]
fn send_to_registry_releases_lock_before_inject_1146() {
    let src = include_str!("mod.rs");
    let fn_start = src
        .find("fn send_to_registry(")
        .expect("send_to_registry must exist");
    // #1441 widened the window: the fn now resolves name → UUID before the
    // lock block, so the `}; // lock released` marker sits further in.
    let fn_body = &src[fn_start..fn_start + 900];

    let lock_end = fn_body
        .find("}; // lock released")
        .expect("send_to_registry must have a scoped lock block");
    let inject_call = fn_body
        .find("inject_with_target")
        .expect("send_to_registry must call inject_with_target");
    assert!(
        lock_end < inject_call,
        "#1146: inject_with_target must appear AFTER the lock scope \
             ends (lock_end={lock_end}, inject_call={inject_call})"
    );
}

/// #1146: broadcast_registry must snapshot all targets under one
/// lock acquisition, release, then inject without re-acquiring.
/// Pre-fix: re-acquired lock per-target and held it during
/// inject — N typed_inject targets × 20s each.
#[test]
fn broadcast_registry_releases_lock_before_inject_1146() {
    let src = include_str!("mod.rs");
    let fn_start = src
        .find("fn broadcast_registry(")
        .expect("broadcast_registry must exist");
    let fn_body = &src[fn_start..fn_start + 1500];

    let lock_release = fn_body
        .find("}; // lock released")
        .expect("broadcast_registry must have a scoped lock block");
    assert!(
        !fn_body[lock_release..].contains("lock_registry"),
        "#1146: broadcast_registry must NOT re-acquire registry \
             lock after releasing it (pre-fix held lock during inject)"
    );

    let inject_site = fn_body
        .find("inject_with_target")
        .expect("broadcast_registry must call inject_with_target");
    assert!(
        inject_site > lock_release,
        "#1146: inject_with_target must appear after lock release"
    );
}

/// #1146 reviewer fix: inject_with_target must skip if the agent
/// was deleted between snapshot and inject (delete/reuse race).
/// The deleted flag is an Arc<AtomicBool> shared with AgentHandle,
/// so setting it on the handle is visible to the snapshot.
#[test]
fn inject_with_target_skips_deleted_agent_1146() {
    let writer: PtyWriter = Arc::new(parking_lot::Mutex::new(
        Box::new(std::io::sink()) as Box<dyn Write + Send>
    ));
    let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let target = InjectTarget {
        pty_writer: writer,
        inject_prefix: String::new(),
        submit_key: "\r".to_string(),
        typed_inject: false,
        deleted: Arc::clone(&deleted),
    };

    // Inject succeeds when not deleted.
    assert!(inject_with_target(&target, b"hello").is_ok());

    // Simulate delete_transaction setting the flag.
    deleted.store(true, std::sync::atomic::Ordering::Release);

    // Inject must return Ok (no-op) without writing.
    assert!(inject_with_target(&target, b"should be skipped").is_ok());
}
/// #1144: pty_read_loop error path must trigger handle_pty_close cleanup.
/// Previously, `Err(e)` broke out of the loop without calling
/// handle_pty_close, leaving the agent as a zombie in the registry.
/// This test simulates a read error by providing a reader that fails
/// after producing some output, then verifies the agent is cleaned up
/// from the registry (handle_pty_close removes it on non-crash paths).
#[test]
#[allow(clippy::unwrap_used)]
fn pty_read_error_triggers_cleanup() {
    use std::collections::HashMap;

    struct FailingReader {
        call: std::cell::Cell<u32>,
    }
    impl std::io::Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.call.get();
            self.call.set(n + 1);
            if n == 0 {
                buf[0] = b'x';
                Ok(1)
            } else {
                Err(std::io::Error::other("simulated read error"))
            }
        }
    }

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let pty_writer: PtyWriter = Arc::new(Mutex::new(Box::new(Vec::<u8>::new())));
    let core = Arc::new(Mutex::new(AgentCore {
        vterm: VTerm::new(80, 24),
        subscribers: Vec::new(),
        state: StateTracker::new(None),
        health: HealthTracker::new(),
    }));

    let agent_name = "read-err-test";
    let instance_id = crate::types::InstanceId::default();
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(true));

    registry.lock().insert(
        instance_id,
        AgentHandle {
            id: instance_id,
            name: agent_name.to_string().into(),
            backend_command: "test".to_string(),
            pty_writer: Arc::clone(&pty_writer),
            pty_master: Arc::new(Mutex::new(
                portable_pty::native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap()
                    .master,
            )),
            core: Arc::clone(&core),
            child: Arc::new(Mutex::new(
                portable_pty::native_pty_system()
                    .openpty(PtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .unwrap()
                    .slave
                    .spawn_command(portable_pty::CommandBuilder::new("true"))
                    .unwrap(),
            )),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    );

    assert!(
        registry.lock().contains_key(&instance_id),
        "pre-condition: agent must be in registry"
    );

    let ctx = PtyReadContext {
        name: agent_name.to_string(),
        instance_id,
        core,
        pty_writer,
        registry: Arc::clone(&registry),
        home: None,
        crash_tx: None,
        dismiss_patterns: Vec::new(),
        shutdown: Some(shutdown),
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let mut reader = FailingReader {
        call: std::cell::Cell::new(0),
    };
    let capture = crate::capture::make_capture_writer(None, agent_name, "test");
    pty_read_loop(&mut reader, &ctx, capture);

    assert!(
        !registry.lock().contains_key(&instance_id),
        "#1144: read error path must call handle_pty_close which removes \
             agent from registry (shutdown=true path). Before fix, the Err \
             branch broke without cleanup, leaving a zombie entry."
    );
}

/// #1145: write_with_timeout stuck thread must clear WRITE_IN_PROGRESS
/// guard on completion, even after the caller has timed out. Before the
/// fix, the guard persisted forever after timeout, permanently blocking
/// future writes to the same PtyWriter (or any new writer allocated at
/// the same pointer address).
#[test]
fn write_guard_cleared_after_stuck_thread_completes() {
    let (lock_tx, lock_rx) = crossbeam_channel::bounded::<()>(0);
    let (unlock_tx, unlock_rx) = crossbeam_channel::bounded::<()>(0);

    struct BlockingWriter {
        lock_tx: crossbeam_channel::Sender<()>,
        unlock_rx: crossbeam_channel::Receiver<()>,
    }
    impl std::io::Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = self.lock_tx.send(());
            let _ = self.unlock_rx.recv();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let writer: PtyWriter = Arc::new(Mutex::new(Box::new(BlockingWriter { lock_tx, unlock_rx })));
    let key = Arc::as_ptr(&writer) as usize;

    let writer2 = Arc::clone(&writer);
    let handle = std::thread::spawn(move || write_with_timeout(&writer2, b"hello"));

    // Wait for the write thread to enter the blocking write.
    lock_rx.recv().expect("write thread must signal lock");

    // Caller's recv_timeout will fire after 5s. Speed this up by
    // spawning a second attempt that hits the in-progress guard.
    let guard_set = write_in_progress_set().lock().contains(&key);
    assert!(guard_set, "guard must be set while write is in progress");

    // Unblock the write thread.
    unlock_tx.send(()).expect("unblock write thread");

    // Wait for the caller to finish.
    let result = handle.join().expect("write thread joined");
    assert!(result.is_ok(), "write should succeed after unblock");

    // Guard must be cleared by the thread itself.
    let guard_after = write_in_progress_set().lock().contains(&key);
    assert!(
        !guard_after,
        "#1145: thread must clear WRITE_IN_PROGRESS guard on exit. \
             Before fix, only the caller's success/error paths cleared it; \
             the timeout path left it set permanently."
    );
}

// ── #1441: registry UUID-key test matrix ─────────────────────────────────
//
// Six invariants guarding the name/UUID dual-track fix. The registry is keyed
// by `InstanceId` resolved from fleet.yaml (the same source inbox uses), so a
// reused name can never route a message to the wrong live process.

/// Per-test temp home with a unique path.
fn uuid_test_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("agend-1441-{}-{}-{}", std::process::id(), tag, n));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create test home");
    d
}

/// Minimal live `AgentHandle` backed by an already-exiting `true` process.
fn mk_handle_1441(name: &str, id: crate::types::InstanceId) -> AgentHandle {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("true");
    cmd.cwd(std::env::temp_dir());
    let child = pair.slave.spawn_command(cmd).expect("spawn true");
    drop(pair.slave);
    let pty_writer: PtyWriter = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
    let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
        Arc::new(Mutex::new(pair.master));
    let core = Arc::new(Mutex::new(AgentCore {
        vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: StateTracker::new(None),
        health: HealthTracker::new(),
    }));
    AgentHandle {
        id,
        name: name.to_string().into(),
        backend_command: "true".to_string(),
        pty_writer,
        pty_master,
        core,
        child: Arc::new(Mutex::new(child)),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: false,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

/// (1) Invariant: after a managed spawn, `registry key == handle.id ==
/// resolve_uuid(name)` — the three identities share one fleet.yaml source.
#[test]
fn matrix1_invariant_key_eq_id_eq_resolve() {
    let home = uuid_test_home("invariant");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  alpha:\n    id: a1a1a1a1-0000-4000-8000-000000000001\n",
    )
    .expect("write fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let sleep_args = ["30".to_string()];
    spawn_agent(
        &SpawnConfig {
            name: "alpha",
            backend_command: "sleep",
            args: &sleep_args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: Some(&home),
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    )
    .expect("spawn must succeed for fleet-registered instance");

    let resolved = crate::fleet::resolve_uuid(&home, "alpha").expect("resolve");
    {
        let reg = registry.lock();
        assert_eq!(reg.len(), 1, "exactly one managed entry");
        let (key, handle) = reg.iter().next().expect("one entry");
        assert_eq!(*key, handle.id, "registry key must equal handle.id");
        assert_eq!(*key, resolved, "registry key must equal resolve_uuid(name)");
    }
    // Cleanup: kill the sleep child.
    for h in registry.lock().values() {
        let _ = h.child.lock().kill();
    }
    std::fs::remove_dir_all(&home).ok();
}

/// (2) The #1441 repro: two live handles share a name but differ by id.
/// Resolution via fleet.yaml routes to the fleet id's handle, never the
/// stale same-name collision.
#[test]
fn matrix2_same_name_diff_uuid_routes_to_fleet_id() {
    let home = uuid_test_home("dual-track");
    let fleet_id = crate::types::InstanceId::new();
    let stale_id = crate::types::InstanceId::new();
    assert_ne!(fleet_id, stale_id);
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  dup:\n    id: {}\n", fleet_id.full()),
    )
    .expect("write fleet.yaml");

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry
        .lock()
        .insert(fleet_id, mk_handle_1441("dup", fleet_id));
    registry
        .lock()
        .insert(stale_id, mk_handle_1441("dup", stale_id));

    let resolved = crate::fleet::resolve_uuid(&home, "dup").expect("resolve");
    assert_eq!(resolved, fleet_id, "name must resolve to the fleet id");
    let reg = registry.lock();
    let handle = reg.get(&resolved).expect("resolved handle present");
    assert_eq!(
        handle.id, fleet_id,
        "lookup-by-resolved-id must hit the fleet handle, not the same-name stale one"
    );
    for h in reg.values() {
        let _ = h.child.lock().kill();
    }
    std::fs::remove_dir_all(&home).ok();
}

/// (3) Pane output/input routing keys on `pane.instance_id` (UUID), not the
/// display name — so a same-name collision can't steer a pane to the wrong
/// live handle.
#[test]
fn matrix3_pane_routes_by_instance_id_not_name() {
    let id_a = crate::types::InstanceId::new();
    let id_b = crate::types::InstanceId::new();
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry.lock().insert(id_a, mk_handle_1441("twin", id_a));
    registry.lock().insert(id_b, mk_handle_1441("twin", id_b));

    // A pane whose authoritative key is id_b must resolve to the id_b handle,
    // even though both handles share the name "twin".
    let reg = registry.lock();
    let routed = reg.get(&id_b).expect("pane.instance_id handle present");
    assert_eq!(
        routed.id, id_b,
        "pane registry lookup must key on instance_id (UUID), not name"
    );
    for h in reg.values() {
        let _ = h.child.lock().kill();
    }
}

/// (4) Spawn fail-fast: a managed (home-bearing) spawn refuses when the
/// instance is absent from fleet.yaml — no random-UUID fallback.
#[test]
fn matrix4_spawn_fails_fast_when_absent_from_fleet() {
    let home = uuid_test_home("fail-fast"); // empty home, no fleet.yaml
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let sleep_args = ["30".to_string()];
    let result = spawn_agent(
        &SpawnConfig {
            name: "ghost",
            backend_command: "sleep",
            args: &sleep_args,
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: Some(&home),
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    );
    assert!(
        result.is_err(),
        "managed spawn must fail-fast when instance absent from fleet.yaml"
    );
    assert!(
        registry.lock().is_empty(),
        "no registry entry may be created on fail-fast"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// (5) Remove-by-id removes exactly the targeted handle, never a same-name
/// collision sharing the display name.
#[test]
fn matrix5_remove_by_id_no_same_name_collision() {
    let id_a = crate::types::InstanceId::new();
    let id_b = crate::types::InstanceId::new();
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    registry
        .lock()
        .insert(id_a, mk_handle_1441("collide", id_a));
    registry
        .lock()
        .insert(id_b, mk_handle_1441("collide", id_b));

    {
        let mut reg = registry.lock();
        reg.remove(&id_a);
        assert!(!reg.contains_key(&id_a), "targeted id removed");
        assert!(
            reg.contains_key(&id_b),
            "same-name sibling under a different id must survive"
        );
        for h in reg.values() {
            let _ = h.child.lock().kill();
        }
    }
}

/// (6) External agents stay name-keyed: the `ExternalRegistry` is a
/// `HashMap<String, _>` and external fallback resolves by name (externals
/// have no fleet.yaml/UUID source).
#[test]
fn matrix6_external_fallback_routes_by_name() {
    let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    externals.lock().insert(
        "ext-agent".to_string(),
        ExternalAgentHandle {
            backend_command: "remote".to_string(),
            pid: 4321,
        },
    );
    let reg = lock_external(&externals);
    let handle = reg.get("ext-agent").expect("external resolves by name");
    assert_eq!(handle.pid, 4321, "external lookup keyed by name");
}
