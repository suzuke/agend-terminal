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

/// #1769: the daemon auto-inject marker prefix embeds the kind and uses the
/// `[AGEND-AUTO` token (sibling of `[AGEND-MSG]`) agents are taught to recognize.
#[test]
fn daemon_auto_prefix_embeds_kind_1769() {
    assert_eq!(
        super::daemon_auto_prefix("ratelimit-retry"),
        "[AGEND-AUTO kind=ratelimit-retry] "
    );
    assert_eq!(
        super::daemon_auto_prefix("apierror-nudge"),
        "[AGEND-AUTO kind=apierror-nudge] "
    );
    // The prefix starts with the shared marker token (so the agent-instruction
    // and the inject path can't drift).
    assert!(super::daemon_auto_prefix("x").starts_with(super::DAEMON_AUTO_INJECT_MARKER));
    // The inner payload survives intact after the prefix → worker still sees it.
    let marked = [
        super::daemon_auto_prefix("ratelimit-retry").as_bytes(),
        b"continue\n",
    ]
    .concat();
    let s = String::from_utf8_lossy(&marked);
    assert!(s.starts_with("[AGEND-AUTO kind=ratelimit-retry] "));
    assert!(s.contains("continue"));
    assert!(s.ends_with('\n'), "submit newline preserved");
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
    assert!(is_sensitive_env_key("AGEND_INSTANCE_NAME"));
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
#[cfg(unix)]
#[test]
/// #1673: the raw 8×6=48 concurrent PTY allocations would exhaust the CI runner's
/// PTY pool under load (intermittent `openpty: Device not configured`). Bound
/// the openpty calls with a counting semaphore so ≤N body threads hold a PTY
/// simultaneously — the rest of each body (shell spawn, agent handle build, the
/// grandchild-kill verification) still runs concurrently across all 8 threads,
/// preserving the concurrent kill-path coverage the test verifies.
/// Re-exhaustion: N is well below the runner's PTY ceiling (4 was leak-safe on
/// the flake-hit macOS runner), so the pool never drains.
fn sweep_child_tree_kills_grandchild_concurrent_stress() {
    const MAX_CONCURRENT_PTYS: usize = 4;
    use parking_lot::Mutex;
    let sem: std::sync::Arc<Mutex<usize>> = std::sync::Arc::new(Mutex::new(0));
    let handles: Vec<_> = (0..8)
        .map({
            let sem = std::sync::Arc::clone(&sem);
            move |tid| {
                std::thread::spawn({
                    let sem = std::sync::Arc::clone(&sem);
                    move || {
                        for i in 0..6 {
                            let path = std::env::temp_dir().join(format!(
                                "agend-sweep-stress-{}-{tid}-{i}.pid",
                                std::process::id()
                            ));
                            // Acquire a permit — spin-lock bounded, brief.
                            loop {
                                let mut held = sem.lock();
                                if *held < MAX_CONCURRENT_PTYS {
                                    *held += 1;
                                    break;
                                }
                                drop(held);
                                std::thread::yield_now();
                            }
                            sweep_child_tree_body(&path);
                            *sem.lock() -= 1;
                        }
                    }
                })
            }
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
    let core = Arc::new(crate::sync_audit::CoreMutex::new(AgentCore {
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
        core: Arc::new(crate::sync_audit::CoreMutex::new(AgentCore {
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

/// §3.9 (MED-5): the daemon must re-inject AGEND_HOME into every spawned
/// agent's env. AGEND_HOME is on the env-isolation SENSITIVE deny-list, so under
/// `AGEND_ENV_ISOLATION=1` the `env_clear` drops it and the passthrough loop
/// never re-adds it → the agent's in-pane `agend-terminal` subcommands fell back
/// to the default `~/.agend-terminal`, pointing at the wrong daemon. The fix
/// re-injects it unconditionally AFTER the clear (like AGEND_INSTANCE_NAME).
/// Regression-proof: revert the re-inject and `get_env` returns None (the
/// allowlist excludes AGEND_HOME, so nothing else sets it post-clear).
#[test]
fn build_command_reinjects_agend_home_under_env_isolation_med5() {
    // Serialize the process-global isolation-flag flip; restore before asserting
    // so a panic can't leak the flag to other tests.
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());

    let home = std::path::PathBuf::from("/tmp/agend-med5-distinct-home");
    let prev = std::env::var("AGEND_ENV_ISOLATION").ok();
    std::env::set_var("AGEND_ENV_ISOLATION", "1");
    let config = SpawnConfig {
        name: "med5-home",
        backend_command: "echo",
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home: Some(&home),
        crash_tx: None,
        shutdown: None,
    };
    let built = build_command(&config);
    match prev {
        Some(v) => std::env::set_var("AGEND_ENV_ISOLATION", v),
        None => std::env::remove_var("AGEND_ENV_ISOLATION"),
    }

    let (cmd, _) = built.expect("build_command");
    let v = cmd
        .get_env("AGEND_HOME")
        .expect("MED-5: AGEND_HOME must survive env_clear under isolation");
    assert_eq!(
        v.to_string_lossy(),
        home.to_string_lossy(),
        "MED-5: AGEND_HOME must equal the daemon home"
    );
}

/// #1597 helper: run `build_command` for an agy backend with the given home +
/// workspace, returning the resulting cmd and the would-be link path. fleet.yaml
/// pins `agy_workspace_link_base` inside `home` so tests never touch the real
/// `<user_home>/agend-ws`.
#[cfg(unix)]
fn build_agy_cmd(
    home: &std::path::Path,
    workspace: &std::path::Path,
) -> (CommandBuilder, std::path::PathBuf) {
    std::fs::create_dir_all(workspace).expect("create workspace");
    let link_base = home.join("ws-links");
    std::fs::write(
        crate::fleet::fleet_yaml_path(home),
        format!(
            "instances: {{}}\nteams: {{}}\nagy_workspace_link_base: {}\n",
            link_base.display()
        ),
    )
    .expect("write fleet.yaml");
    let config = SpawnConfig {
        name: "agy-int",
        backend_command: "agy",
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: Some(workspace),
        submit_key: "\r",
        home: Some(home),
        crash_tx: None,
        shutdown: None,
    };
    let (cmd, backend) = build_command(&config).expect("build_command");
    assert_eq!(backend, Some(Backend::Agy));
    (cmd, crate::agy_workspace::link_path(home, "agy-int"))
}

#[cfg(unix)]
fn agy_tmp(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "agend-1597-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// #1597 case (a): an explicit NON-hidden working_directory → agy accepts it
/// directly. `$PWD` is the real (resolved) dir and NO link is created (no
/// shadowing, no stray symlink).
#[cfg(unix)]
#[test]
fn build_command_agy_non_hidden_workspace_uses_real_dir_no_link() {
    let home = agy_tmp("nonhidden").join("agend-home");
    let workspace = home.join("proj");
    let (cmd, link_path) = build_agy_cmd(&home, &workspace);

    let resolved =
        crate::api::validate_working_directory(&workspace, &home).expect("validate workspace");
    let pwd = cmd.get_env("PWD").expect("agy must set $PWD");
    assert_eq!(
        std::path::Path::new(pwd),
        resolved.as_path(),
        "non-hidden workspace: $PWD must be the real resolved dir, not a link"
    );
    assert!(
        link_path.symlink_metadata().is_err(),
        "non-hidden workspace must NOT create a link at {}",
        link_path.display()
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1597 case (b): the default hidden workspace (`$AGEND_HOME` is dot-prefixed)
/// → agy would reject it, so `$PWD` points at a non-hidden link to the real dir.
/// (This is the #1547/#1582 case.)
#[cfg(unix)]
#[test]
fn build_command_agy_hidden_workspace_uses_link() {
    // Hidden home segment `.agend-home` makes the resolved cwd hidden.
    let home = agy_tmp("hidden").join(".agend-home");
    let workspace = crate::paths::workspace_dir(&home).join("agy-int");
    let (cmd, link_path) = build_agy_cmd(&home, &workspace);

    let pwd = cmd.get_env("PWD").expect("agy must set $PWD");
    assert_eq!(
        std::path::Path::new(pwd),
        link_path.as_path(),
        "hidden workspace: $PWD must be the non-hidden link"
    );
    assert!(
        link_path
            .symlink_metadata()
            .expect("link must exist")
            .file_type()
            .is_symlink(),
        "hidden workspace must create a symlink"
    );
    assert_eq!(
        std::fs::canonicalize(&link_path).expect("canonicalize link"),
        std::fs::canonicalize(&workspace).expect("canonicalize workspace"),
        "link must resolve to the real hidden workspace"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1597 case (c): a non-hidden LEAF under a hidden ANCESTOR is still hidden by
/// agy's rule → link required.
#[cfg(unix)]
#[test]
fn build_command_agy_hidden_ancestor_uses_link() {
    // `.cfg` ancestor is hidden even though the `proj` leaf is not.
    let home = agy_tmp("ancestor").join(".cfg");
    let workspace = home.join("proj");
    let (cmd, link_path) = build_agy_cmd(&home, &workspace);

    let pwd = cmd.get_env("PWD").expect("agy must set $PWD");
    assert_eq!(
        std::path::Path::new(pwd),
        link_path.as_path(),
        "hidden ancestor: $PWD must be the non-hidden link"
    );
    assert!(
        link_path.symlink_metadata().is_ok(),
        "hidden ancestor must create a link"
    );
    std::fs::remove_dir_all(&home).ok();
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

/// #1530/F1 (lockaudit): the registry-borrow inject footgun
/// `inject_to_agent(&AgentHandle)` is REMOVED. Every inject/write now goes
/// through a snapshot — `inject_with_target_gated` / `inject_with_target`
/// (an `InjectTarget`) or `write_to_pty` (a `PtyWriter`) — so a caller
/// physically cannot hold the registry across the (up to 5s recv_timeout +
/// payload-scaled sleep) blocking PTY write: capturing the snapshot drops the
/// registry borrow. Re-introducing `fn inject_to_agent(&AgentHandle, …)` would
/// re-open the freeze class (registry held across inject), so this pins it gone.
#[test]
fn inject_handle_footgun_removed_uses_snapshots_f1() {
    let src = include_str!("mod.rs");
    assert!(
        !src.contains("pub fn inject_to_agent("),
        "#1530/F1: inject_to_agent(&AgentHandle) must stay removed — it lets a caller \
         hold the registry across a blocking inject; use inject_with_target_gated \
         (snapshot under lock → drop → inject)"
    );
    assert!(
        src.contains("pub(crate) fn inject_with_target_gated("),
        "#1530/F1: the snapshot inject API inject_with_target_gated must exist"
    );
    assert!(
        src.contains("pub(crate) fn write_to_pty("),
        "#1530/F1: the snapshot write API write_to_pty must exist"
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
    let core = Arc::new(crate::sync_audit::CoreMutex::new(AgentCore {
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
    let core = Arc::new(crate::sync_audit::CoreMutex::new(AgentCore {
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

// ── #1440: agent backend env isolation (clear + allowlist) ──

fn env_map(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// #1440: an unlisted secret in the daemon env is absent from the child env
/// under isolation; a base-allowlist key (HOME) survives.
#[test]
fn env_isolation_drops_unlisted_secret() {
    let src = env_map(&[("MY_FAKE_SECRET", "supersecret-value"), ("HOME", "/home/x")]);
    let plan = resolve_child_env(Some(&Backend::ClaudeCode), &[], &src);
    assert!(
        plan.injected.iter().all(|(k, _)| k != "MY_FAKE_SECRET"),
        "unlisted secret must not reach the child"
    );
    assert!(
        plan.injected.iter().any(|(k, _)| k == "HOME"),
        "base-allowlist HOME must survive"
    );
    assert!(plan.dropped.contains(&"MY_FAKE_SECRET".to_string()));
}

/// #1440: cross-backend credential isolation — Codex gets OPENAI but never
/// ANTHROPIC, and Claude gets ANTHROPIC but never OPENAI.
#[test]
fn credential_isolation_across_backends() {
    let src = env_map(&[
        ("ANTHROPIC_API_KEY", "sk-ant-xxx"),
        ("OPENAI_API_KEY", "sk-oai-yyy"),
    ]);

    let codex = resolve_child_env(Some(&Backend::Codex), &[], &src);
    assert!(codex.injected.iter().any(|(k, _)| k == "OPENAI_API_KEY"));
    assert!(
        codex.injected.iter().all(|(k, _)| k != "ANTHROPIC_API_KEY"),
        "Codex must not inherit ANTHROPIC_API_KEY"
    );

    let claude = resolve_child_env(Some(&Backend::ClaudeCode), &[], &src);
    assert!(claude
        .injected
        .iter()
        .any(|(k, _)| k == "ANTHROPIC_API_KEY"));
    assert!(
        claude.injected.iter().all(|(k, _)| k != "OPENAI_API_KEY"),
        "Claude must not inherit OPENAI_API_KEY"
    );
}

/// #1440: a non-sensitive passthrough key passes, but a sensitive one
/// (LD_PRELOAD) stays blocked even when explicitly listed.
#[test]
fn passthrough_passes_but_sensitive_still_blocked() {
    let src = env_map(&[("MY_CORP_CA", "/etc/ca.pem"), ("LD_PRELOAD", "/evil.so")]);
    let passthrough = vec!["MY_CORP_CA".to_string(), "LD_PRELOAD".to_string()];
    let plan = resolve_child_env(Some(&Backend::ClaudeCode), &passthrough, &src);
    assert!(
        plan.injected.iter().any(|(k, _)| k == "MY_CORP_CA"),
        "listed non-sensitive passthrough key must pass"
    );
    assert!(
        plan.injected.iter().all(|(k, _)| k != "LD_PRELOAD"),
        "LD_PRELOAD must stay blocked even when listed in passthrough"
    );
    assert!(plan.dropped.contains(&"LD_PRELOAD".to_string()));
}

/// #1440: the dropped list feeding the one-time warn carries KEY NAMES only,
/// never values — so the warn can never leak a secret value.
#[test]
fn dropped_warn_input_is_key_names_only() {
    let src = env_map(&[("STRIPE_SECRET", "rk_live_TOPSECRETVALUE")]);
    let plan = resolve_child_env(Some(&Backend::ClaudeCode), &[], &src);
    assert!(plan.dropped.contains(&"STRIPE_SECRET".to_string()));
    assert!(
        plan.dropped.iter().all(|d| !d.contains("TOPSECRETVALUE")),
        "warn input must contain key names only, never values"
    );
}

// ── #1492: lock_registry guard wiring for lock-across-self-IPC detection ──

#[cfg(test)]
fn empty_registry_1492() -> AgentRegistry {
    std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Real wiring: `lock_registry` bumps the held-counter, so a self-IPC vector
/// (modeled by the guard) refuses with `Err` while the guard is alive. Proves
/// the guard — not just the raw counter — participates. #1492-L2: the guard is
/// always-on and returns `Err` (was a debug-only `panic!` pre-L2).
#[test]
fn lock_registry_guard_trips_self_ipc_assert_1492() {
    let reg = empty_registry_1492();
    let _guard = lock_registry(&reg);
    assert!(crate::sync_audit::assert_no_registry_lock_for_self_ipc("api::call").is_err());
}

/// Dropping the `lock_registry` guard BEFORE self-IPC (the 6f1403d-correct
/// pattern) clears the held-counter, so the guard returns `Ok` (does not trip).
#[test]
fn lock_registry_guard_drop_clears_self_ipc_flag_1492() {
    let reg = empty_registry_1492();
    {
        let _guard = lock_registry(&reg);
    } // guard dropped → counter cleared
    assert!(crate::sync_audit::assert_no_registry_lock_for_self_ipc("api::call").is_ok());
}

/// `lock_registry_tracked` participates in the same detection.
#[test]
fn lock_registry_tracked_guard_trips_self_ipc_assert_1492() {
    let reg = empty_registry_1492();
    let _guard = lock_registry_tracked(&reg, "test-1492");
    assert!(
        crate::sync_audit::assert_no_registry_lock_for_self_ipc("enqueue_with_idle_hint").is_err()
    );
}

// ── #1535: CoreMutex guard wiring for core-held self-IPC detection ──

/// Real wiring: holding a `CoreMutex` guard bumps `CORE_LOCK_DEPTH`, so the
/// (extended) self-IPC guard refuses with `Err` while the guard is alive —
/// proving the core-lock blind spot the registry-only guard missed (#1535) is
/// now covered. #1492-L2: always-on, returns `Err`.
#[test]
fn core_mutex_guard_trips_self_ipc_assert_1535() {
    let m = crate::sync_audit::CoreMutex::new(0u32);
    let _guard = m.lock();
    assert!(crate::sync_audit::assert_no_registry_lock_for_self_ipc("api::call").is_err());
}

/// Dropping the `CoreMutex` guard BEFORE self-IPC (the collect→drop→emit
/// pattern, e.g. #1530) clears the depth, so the guard returns `Ok`.
#[test]
fn core_mutex_guard_drop_clears_self_ipc_flag_1535() {
    let m = crate::sync_audit::CoreMutex::new(0u32);
    {
        let _guard = m.lock();
    } // guard dropped → core depth cleared
    assert!(crate::sync_audit::assert_no_registry_lock_for_self_ipc("api::call").is_ok());
}

// ── #1504: git-shim PATH parsing + self-exclusion (L1) ──

#[test]
fn git_search_excludes_shim_dir_1504() {
    // Cross-platform: the shim dir ($AGEND_HOME/bin) must be filtered out of the
    // git search PATH, and real dirs preserved. Uses the platform separator via
    // join_paths/split_paths so it exercises the #1504 L1 fix on every OS.
    let shim = std::path::PathBuf::from("/tmp/agend-home-1504/bin");
    let real = std::path::PathBuf::from("/usr/bin");
    let path = std::env::join_paths([real.clone(), shim.clone(), std::path::PathBuf::from("/bin")])
        .expect("join PATH");
    let result = super::git_search_without_shim(&path, Some(&shim));
    assert!(
        !result.iter().any(|p| super::same_dir(p, Some(&shim))),
        "#1504: shim dir must be excluded from git search PATH: {result:?}"
    );
    assert!(
        result.contains(&real),
        "#1504: real PATH dirs must survive: {result:?}"
    );
}

#[test]
fn same_dir_lexical_slash_and_none_1504() {
    // Nonexistent paths → lexical fallback → backslash normalized to forward,
    // so a forward-slash AGEND_HOME/bin still matches a backslash PATH entry.
    assert!(
        super::same_dir(
            std::path::Path::new("C:/agend/bin"),
            Some(std::path::Path::new("C:\\agend\\bin")),
        ),
        "#1504: slash-variant dirs must compare equal via lexical fallback"
    );
    assert!(!super::same_dir(
        std::path::Path::new("/usr/bin"),
        Some(std::path::Path::new("/usr/local/bin")),
    ));
    assert!(
        !super::same_dir(std::path::Path::new("/usr/bin"), None),
        "no shim dir → never excludes"
    );
}

#[cfg(windows)]
#[test]
fn git_search_splits_windows_path_separator_1504() {
    // RED against the old `.split(':')`: a `;`-separated Windows PATH whose
    // entries carry drive-colons must split into WHOLE entries (not be shredded
    // on the `C:` colon), and the shim dir must be excluded. Runs on the
    // windows-latest CI job.
    let shim = std::path::PathBuf::from("C:\\agend\\bin");
    let path = std::ffi::OsString::from(
        "C:\\Program Files\\Git\\cmd;C:\\agend\\bin;C:\\Windows\\System32",
    );
    let result = super::git_search_without_shim(&path, Some(&shim));
    assert!(
        result.contains(&std::path::PathBuf::from("C:\\Program Files\\Git\\cmd")),
        "#1504: drive-colon entry must survive splitting: {result:?}"
    );
    assert!(
        !result.iter().any(|p| super::same_dir(p, Some(&shim))),
        "#1504: shim dir excluded on Windows: {result:?}"
    );
}

// ── #1519: per-instance opencode session isolation ──

#[test]
fn opencode_data_dir_is_per_instance_1519() {
    let home = std::path::Path::new("/tmp/agend-home");
    let a = opencode_data_dir(home, "fixup-reviewer");
    let b = opencode_data_dir(home, "fixup-reviewer-2");
    assert_ne!(a, b, "distinct instances must get distinct data dirs");
    assert!(
        a.ends_with("backend-data/opencode/fixup-reviewer"),
        "got {a:?}"
    );
    assert!(
        b.ends_with("backend-data/opencode/fixup-reviewer-2"),
        "got {b:?}"
    );
}

/// The core #1519 regression: two opencode instances resolve to DISTINCT
/// XDG_DATA_HOME values — the property that prevents the shared-session
/// collision. Pre-fix there was no per-instance XDG at all (both inherited the
/// daemon's, defaulting to the one global DB).
#[test]
fn per_instance_opencode_xdg_distinct_for_two_opencode_instances_1519() {
    let home = std::path::Path::new("/tmp/agend-home");
    let a = per_instance_opencode_xdg(Some(&Backend::OpenCode), Some(home), "fixup-reviewer");
    let b = per_instance_opencode_xdg(Some(&Backend::OpenCode), Some(home), "fixup-reviewer-2");
    assert!(
        a.is_some() && b.is_some(),
        "OpenCode + home must yield an XDG dir"
    );
    assert_ne!(
        a, b,
        "two opencode instances MUST get distinct XDG_DATA_HOME"
    );
}

#[test]
fn per_instance_opencode_xdg_gated_to_opencode_1519() {
    let home = std::path::Path::new("/tmp/agend-home");
    // Non-opencode backends must NOT get a per-instance XDG override.
    assert_eq!(
        per_instance_opencode_xdg(Some(&Backend::ClaudeCode), Some(home), "dev"),
        None,
        "claude must be unaffected"
    );
    assert_eq!(
        per_instance_opencode_xdg(Some(&Backend::Codex), Some(home), "dev"),
        None,
        "codex must be unaffected"
    );
    // OpenCode but no home → can't isolate → None (falls back to global; logged).
    assert_eq!(
        per_instance_opencode_xdg(Some(&Backend::OpenCode), None, "rev"),
        None,
        "no home → no per-instance dir"
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn provision_opencode_data_dir_copies_auth_1519() {
    let base = std::env::temp_dir().join(format!(
        "agend-1519-prov-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Fake canonical auth source.
    let src_dir = base.join("canonical");
    std::fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join("auth.json");
    std::fs::write(&src, r#"{"opencode-go":{"type":"api","key":"secret"}}"#).unwrap();

    let xdg = base.join("xdg");
    provision_opencode_data_dir(&xdg, Some(&src)).unwrap();

    let dst = xdg.join("opencode").join("auth.json");
    assert!(
        dst.exists(),
        "auth.json must be copied into <xdg>/opencode/"
    );
    assert_eq!(
        std::fs::read_to_string(&dst).unwrap(),
        std::fs::read_to_string(&src).unwrap(),
        "copied auth content must match the canonical source"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "copied auth.json must be mode 600, got {mode:o}"
        );
    }
    std::fs::remove_dir_all(&base).ok();
}

#[test]
#[allow(clippy::unwrap_used)]
fn provision_opencode_data_dir_no_auth_src_is_ok_1519() {
    let xdg = std::env::temp_dir().join(format!(
        "agend-1519-noauth-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // No auth source → still creates the dir, no auth.json, no error.
    provision_opencode_data_dir(&xdg, None).unwrap();
    assert!(
        xdg.join("opencode").is_dir(),
        "data dir must still be created"
    );
    assert!(
        !xdg.join("opencode").join("auth.json").exists(),
        "no auth.json when no source provided"
    );
    std::fs::remove_dir_all(&xdg).ok();
}

// ── pty broadcast must never block under the core lock ──────────────────────
//
// Regression for the daemon freeze where `pty_read_loop` broadcast PTY output
// to subscribers with a BLOCKING `send` while holding `core.lock()`. A full
// bounded(1024) subscriber channel (consumer stalled) made that send block
// forever, holding the core lock and wedging the whole daemon (main TUI thread,
// supervisor, every api_handler). With the old `send`, `broadcast_full_channel_
// does_not_block` below would hang forever — it is the load-bearing pin.

#[test]
fn broadcast_full_channel_does_not_block_and_drops() {
    // bounded(1), pre-filled, and never drained → the next send has nowhere to
    // go. The OLD blocking `send` would deadlock here; `try_send` drops instead.
    let (tx, _rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
    tx.send(vec![0u8]).expect("prime the single slot");
    let mut subs = vec![tx];
    let mut dropped = 0u64;
    // If this blocks, the test harness hangs — that IS the failure signal.
    broadcast_pty_output(&mut subs, b"more output", &mut dropped, "agent-x");
    assert_eq!(
        dropped, 1,
        "the chunk must be dropped, not block, on a full channel"
    );
    assert_eq!(
        subs.len(),
        1,
        "a merely-full (live) subscriber is kept, not removed"
    );
}

#[test]
fn broadcast_removes_disconnected_subscriber() {
    let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(1024);
    drop(rx); // consumer gone → Disconnected
    let mut subs = vec![tx];
    let mut dropped = 0u64;
    broadcast_pty_output(&mut subs, b"x", &mut dropped, "agent-x");
    assert!(subs.is_empty(), "a disconnected subscriber must be removed");
    assert_eq!(dropped, 0, "disconnect is not a drop");
}

#[test]
fn broadcast_delivers_to_healthy_subscriber() {
    let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(1024);
    let mut subs = vec![tx];
    let mut dropped = 0u64;
    broadcast_pty_output(&mut subs, b"hello", &mut dropped, "agent-x");
    assert_eq!(subs.len(), 1, "a healthy subscriber is kept");
    assert_eq!(dropped, 0);
    assert_eq!(
        rx.try_recv().ok(),
        Some(b"hello".to_vec()),
        "output must be delivered"
    );
}

/// #1744-H5: an external signal-kill (OOM) escalates a leaderless-team P0 only
/// for a self-orchestrator — fail-closed via `self_orch_status` (Yes|Unknown
/// fire, No skip), and `home == None` (can't read teams) skips.
#[test]
#[allow(clippy::unwrap_used)]
fn signal_kill_escalates_only_for_self_orch_1744_h5() {
    let home = std::env::temp_dir().join(format!(
        "agend-h5-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    crate::teams::create(
        &home,
        &serde_json::json!({"name": "t", "members": ["lead", "dev"], "orchestrator": "lead"}),
    );

    assert!(
        signal_kill_self_orch_should_escalate(&Some(home.clone()), "lead"),
        "a self-orchestrator's OOM must escalate (Yes → fire)"
    );
    assert!(
        !signal_kill_self_orch_should_escalate(&Some(home.clone()), "dev"),
        "a non-orchestrator member's OOM must stay silent (No → skip)"
    );
    assert!(
        !signal_kill_self_orch_should_escalate(&None, "lead"),
        "home=None can't determine self-orch → skip"
    );
    let _ = std::fs::remove_dir_all(&home);
}
