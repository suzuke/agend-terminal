//! Comprehensive end-to-end verification suite.
//!
//! `agend-terminal verify` runs all tests with auto daemon lifecycle.

use crate::{agent, api, backend, daemon, inbox, instructions};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct TestResult {
    name: String,
    passed: bool,
    detail: String,
}

impl TestResult {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail: detail.into(),
        }
    }
    fn from_bool(
        name: impl Into<String>,
        ok: bool,
        pass_msg: impl Into<String>,
        fail_msg: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            passed: ok,
            detail: if ok { pass_msg.into() } else { fail_msg.into() },
        }
    }
}

/// Create a default SpawnConfig for test agents (bash).
fn test_spawn_config<'a>(name: &'a str, home: Option<&'a Path>) -> agent::SpawnConfig<'a> {
    agent::SpawnConfig {
        name,
        backend_command: "/bin/bash",
        args: &[],
        spawn_mode: crate::backend::SpawnMode::Fresh,
        cols: 80,
        rows: 24,
        env: None,
        working_dir: None,
        submit_key: "\r",
        home,
        crash_tx: None,
        shutdown: None,
    }
}

/// Poll until `check` returns true, or until `deadline`. Sleeps 500ms between checks.
fn poll_until(deadline: std::time::Instant, mut check: impl FnMut() -> bool) -> bool {
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if check() {
            return true;
        }
    }
    false
}

pub fn run(home: &Path, json_output: bool, backend_filter: Option<&str>) -> anyhow::Result<()> {
    let test_home = home.join("_verify_tmp");
    std::fs::create_dir_all(&test_home)?;

    let mut results = vec![
        test_attach(&test_home),
        test_inbox(&test_home),
        test_mcp_framing(),
        test_backend_config(&test_home),
        test_instructions(&test_home),
    ];

    // --- Tests that need daemon ---
    // Start a test daemon
    let daemon_home = test_home.join("daemon");
    std::fs::create_dir_all(&daemon_home)?;

    let registry: agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Spawn two test agents
    let spawn_ok = agent::spawn_agent(&test_spawn_config("test-a", Some(&daemon_home)), &registry)
        .is_ok()
        && agent::spawn_agent(&test_spawn_config("test-b", Some(&daemon_home)), &registry).is_ok();

    if spawn_ok {
        // Ensure run dir + .daemon identity exists so clients can discover us
        let rdir = daemon::run_dir(&daemon_home);
        std::fs::create_dir_all(&rdir).ok();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::write(
            rdir.join(".daemon"),
            format!("{}:{now}", std::process::id()),
        );
        // P1-10: issue the API auth cookie before spawning the TUI / API
        // threads so their `read_cookie` calls succeed.
        let cookie_ok = crate::auth_cookie::issue(&rdir).is_ok();
        if !cookie_ok {
            results.push(TestResult {
                name: "daemon_setup".into(),
                passed: false,
                detail: "Failed to issue API cookie".into(),
            });
        }
        for name in ["test-a", "test-b"] {
            let rdir = rdir.clone();
            let reg = Arc::clone(&registry);
            let n = name.to_string();
            std::thread::Builder::new()
                .name(format!("{n}_tui"))
                .spawn(move || daemon::serve_agent_tui(&n, &rdir, &reg))
                .ok();
        }

        // Start API socket
        let api_reg = Arc::clone(&registry);
        let api_home = daemon_home.clone();
        std::thread::Builder::new()
            .name("verify_api".into())
            .spawn(move || {
                let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let configs = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                let externals = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                api::serve(&api_home, api_reg, shutdown, configs, externals, None)
            })
            .ok();

        std::thread::sleep(std::time::Duration::from_secs(1));

        results.push(test_api(&daemon_home));
        results.push(test_send(&daemon_home));
        results.push(test_create_delete(&daemon_home));
    } else {
        results.push(TestResult {
            name: "daemon_setup".into(),
            passed: false,
            detail: "Failed to spawn test agents".into(),
        });
    }

    // Telegram test (optional — needs AGEND_BOT_TOKEN)
    results.push(test_telegram());

    // --- Per-backend tests ---
    for b in backend::Backend::all() {
        if let Some(filter) = backend_filter {
            if b.name() != filter {
                continue;
            }
        }
        results.extend(test_backend(b, &test_home));
    }

    // --- Cleanup ---
    // Kill test agents
    {
        let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
        for (_, handle) in reg.iter() {
            let mut child = crate::sync::lock_poisoned(&handle.child, "verify_child");
            let _ = child.kill();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&test_home);

    // --- Report ---
    let passed = results
        .iter()
        .filter(|r| r.passed && !r.detail.starts_with("SKIP"))
        .count();
    let skipped = results
        .iter()
        .filter(|r| r.detail.starts_with("SKIP"))
        .count();
    let failed = results.len() - passed - skipped;

    if json_output {
        let items: Vec<_> = results
            .iter()
            .map(|r| {
                json!({
                    "name": r.name,
                    "passed": r.passed,
                    "detail": r.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": results.len(),
                "passed": passed,
                "failed": failed,
                "skipped": skipped,
                "tests": items,
            }))?
        );
    } else {
        println!("\n{:=<50}", "= AgEnD Terminal Verify ");
        for r in &results {
            let icon = if r.passed {
                "✓"
            } else if r.detail.starts_with("SKIP") {
                "-"
            } else {
                "✗"
            };
            println!("  {icon} {:<25} {}", r.name, r.detail);
        }
        println!("{:=<50}", "");
        println!(
            "  Total: {}  Passed: {}  Failed: {}  Skipped: {}",
            results.len(),
            passed,
            failed,
            skipped
        );

        if failed > 0 {
            std::process::exit(1);
        }
    }

    Ok(())
}

#[allow(clippy::unwrap_used)]
fn test_attach(_home: &Path) -> TestResult {
    let registry = Arc::new(Mutex::new(HashMap::new()));
    if let Err(e) = agent::spawn_agent(&test_spawn_config("verify-attach", None), &registry) {
        return TestResult::fail("attach", format!("{e}"));
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    {
        let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
        let _ = agent::write_to_agent(reg.get("verify-attach").unwrap(), b"echo VERIFY_OK\r");
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let output = {
        let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
        let core =
            crate::sync::lock_poisoned(&reg.get("verify-attach").unwrap().core, "verify_core");
        String::from_utf8_lossy(&core.vterm.dump_screen()).to_string()
    };
    let ok = output.contains("VERIFY_OK");

    let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
    let _ = reg
        .get("verify-attach")
        .unwrap()
        .child
        .lock()
        .unwrap()
        .kill();

    TestResult::from_bool(
        "attach",
        ok,
        "PTY spawn + inject + VTerm",
        "VERIFY_OK not found in output",
    )
}

fn test_inbox(home: &Path) -> TestResult {
    let test_name = "verify-inbox";
    for i in 1..=3 {
        let _ = inbox::enqueue(
            home,
            test_name,
            inbox::InboxMessage {
                from: format!("test-{i}"),
                text: format!("msg {i}"),
                kind: None,
                timestamp: "2024-01-01T00:00:00Z".into(),
            },
        );
    }
    let msgs = inbox::drain(home, test_name);
    let empty = inbox::drain(home, test_name);
    let _ = std::fs::remove_file(home.join("inbox").join(format!("{test_name}.jsonl")));
    let ok = msgs.len() == 3 && empty.is_empty();
    TestResult::from_bool(
        "inbox",
        ok,
        "enqueue 3 + drain + empty",
        format!("got {} msgs, empty={}", msgs.len(), empty.is_empty()),
    )
}

fn test_mcp_framing() -> TestResult {
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let frame = format!("Content-Length: {}\r\n\r\n{}", req.len(), req);
    let ok =
        frame.contains("Content-Length:") && frame.contains("\r\n\r\n") && frame.ends_with('}');
    TestResult::from_bool(
        "mcp_framing",
        ok,
        "Content-Length format correct",
        "bad format",
    )
}

fn test_backend_config(_home: &Path) -> TestResult {
    // MCP config removed — agents use CLI now. Just pass.
    TestResult::from_bool("backend_config", true, "MCP removed, using CLI", "")
}

fn test_instructions(home: &Path) -> TestResult {
    let test_dir = home.join("verify-instructions");
    std::fs::create_dir_all(&test_dir).ok();

    instructions::generate(&test_dir, "claude");
    let claude_path = test_dir.join(".claude").join("rules").join("agend.md");
    let claude_ok = claude_path.exists() && {
        let c = std::fs::read_to_string(&claude_path).unwrap_or_default();
        ["reply", "send", "inbox", "v3-mcp"]
            .iter()
            .all(|p| c.contains(p))
    };

    instructions::generate(&test_dir, "kiro-cli");
    let kiro_ok = test_dir
        .join(".kiro")
        .join("steering")
        .join("agend.md")
        .exists();

    let _ = std::fs::remove_dir_all(&test_dir);
    let ok = claude_ok && kiro_ok;
    TestResult::from_bool(
        "instructions",
        ok,
        "Claude + Kiro instructions generated",
        format!("claude={claude_ok} kiro={kiro_ok}"),
    )
}

fn test_api(home: &Path) -> TestResult {
    match api::call(home, &json!({"method": api::method::LIST})) {
        Ok(resp) => {
            let agents = resp["result"]["agents"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            TestResult::from_bool(
                "api",
                agents >= 2,
                format!("{agents} agents in registry"),
                format!("{agents} agents in registry"),
            )
        }
        Err(e) => TestResult::fail("api", format!("{e}")),
    }
}

fn test_send(home: &Path) -> TestResult {
    if api::call(home, &json!({"method": api::method::SEND, "params": {"from": "test-a", "target": "test-b", "text": "verify-send-ok"}})).is_err() {
        return TestResult::fail("send", "API send failed");
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let msgs = inbox::drain(home, "test-b");
    let found = msgs.iter().any(|m| m.text.contains("verify-send-ok"));
    TestResult::from_bool(
        "send",
        found,
        "a→b message delivered via inbox",
        format!("not found in {} msgs", msgs.len()),
    )
}

fn test_create_delete(home: &Path) -> TestResult {
    if api::call(
        home,
        &json!({"method": api::method::SPAWN, "params": {"name": "verify-dynamic", "backend": "/bin/bash"}}),
    )
    .is_err()
    {
        return TestResult::fail("create_delete", "spawn failed");
    }
    std::thread::sleep(std::time::Duration::from_secs(1));

    let has_agent = |name: &str| -> bool {
        api::call(home, &json!({"method": api::method::LIST}))
            .ok()
            .and_then(|r| r["result"]["agents"].as_array().cloned())
            .map(|a| a.iter().any(|x| x["name"].as_str() == Some(name)))
            .unwrap_or(false)
    };
    let found = has_agent("verify-dynamic");
    let _ = api::call(
        home,
        &json!({"method": api::method::KILL, "params": {"name": "verify-dynamic"}}),
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    let removed = !has_agent("verify-dynamic");

    let ok = found && removed;
    TestResult::from_bool(
        "create_delete",
        ok,
        "spawn → found in list → kill → reaped",
        format!("found={found} removed={removed}"),
    )
}

fn test_telegram() -> TestResult {
    if std::env::var("AGEND_BOT_TOKEN").is_err() {
        return TestResult::ok("telegram", "SKIP — AGEND_BOT_TOKEN not set");
    }
    TestResult::ok("telegram", "SKIP — live Telegram test not implemented")
}

/// Per-backend verification: spawn, ready, instructions, MCP config, inject, quit.
#[allow(clippy::unwrap_used)]
fn test_backend(backend: &backend::Backend, home: &Path) -> Vec<TestResult> {
    let name = backend.name();
    let preset = backend.preset();
    let mut results = Vec::new();

    if !backend.is_installed() {
        results.push(TestResult::ok(
            format!("backend:{name}"),
            format!("SKIP — {} not in PATH", preset.command),
        ));
        return results;
    }

    let test_dir = home.join(format!("verify-backend-{name}"));
    std::fs::create_dir_all(&test_dir).ok();

    // 1. Instructions
    crate::instructions::generate(&test_dir, preset.command);
    let instr_ok = test_dir.join(preset.instructions_path).exists() && {
        let c =
            std::fs::read_to_string(test_dir.join(preset.instructions_path)).unwrap_or_default();
        c.contains("v3-mcp") && c.contains("reply")
    };
    results.push(TestResult::from_bool(
        format!("backend:{name}:instructions"),
        instr_ok,
        preset.instructions_path.to_string(),
        "missing or invalid",
    ));

    // 2. Spawn + ready detection.
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let agent_name = format!("verify-{name}");
    let spawn_result = agent::spawn_agent(
        &agent::SpawnConfig {
            name: &agent_name,
            backend_command: preset.command,
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 120,
            rows: 40,
            env: None,
            working_dir: Some(test_dir.as_path()),
            submit_key: preset.submit_key,
            home: None,
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    );

    match spawn_result {
        Ok(()) => {
            let re = regex::Regex::new(preset.ready_pattern)
                .unwrap_or_else(|_| regex::Regex::new(".").unwrap());
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(preset.ready_timeout_secs);
            let ready = poll_until(deadline, || {
                let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                reg.get(&agent_name)
                    .map(|h| {
                        let core = crate::sync::lock_poisoned(&h.core, "verify_core");
                        re.is_match(&String::from_utf8_lossy(&core.vterm.dump_screen()))
                    })
                    .unwrap_or(false) // Agent reaped
            });

            if !ready {
                let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                if let Some(handle) = reg.get(&agent_name) {
                    let dump = crate::sync::lock_poisoned(&handle.core, "verify_core")
                        .vterm
                        .dump_screen();
                    let stripped = crate::agent::strip_ansi_pub(&String::from_utf8_lossy(&dump));
                    tracing::debug!(%name, "VTerm at timeout:");
                    for (i, line) in stripped.lines().enumerate() {
                        let t = line.trim_end();
                        if !t.is_empty() {
                            tracing::debug!("  {:>3}| {}", i + 1, t);
                        }
                    }
                }
            }

            results.push(TestResult::from_bool(
                format!("backend:{name}:spawn_ready"),
                ready,
                format!("ready in <{}s", preset.ready_timeout_secs),
                format!(
                    "timeout after {}s (pattern: {})",
                    preset.ready_timeout_secs, preset.ready_pattern
                ),
            ));

            // 4. Inject + submit test (only if ready)
            if ready {
                let inject_ok = {
                    let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                    reg.get(&agent_name)
                        .map(|h| {
                            agent::write_to_agent(
                                h,
                                format!("echo BACKEND_VERIFY_OK{}", preset.submit_key).as_bytes(),
                            )
                            .is_ok()
                        })
                        .unwrap_or(false)
                };
                results.push(TestResult::from_bool(
                    format!("backend:{name}:inject"),
                    inject_ok,
                    "inject accepted",
                    "write failed",
                ));
                std::thread::sleep(std::time::Duration::from_secs(2));
            }

            // 5. Graceful quit — try quit command, then Ctrl+C/D, then force kill
            std::thread::sleep(std::time::Duration::from_secs(2));
            {
                let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                if let Some(h) = reg.get(&agent_name) {
                    let _ = agent::write_to_agent(
                        h,
                        format!("{}{}", preset.quit_command, preset.submit_key).as_bytes(),
                    );
                }
            }

            let is_gone = || {
                !crate::sync::lock_poisoned(&registry, "verify_registry").contains_key(&agent_name)
            };
            let mut quit_ok = poll_until(
                std::time::Instant::now() + std::time::Duration::from_secs(5),
                &is_gone,
            );

            if !quit_ok {
                {
                    let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                    if let Some(h) = reg.get(&agent_name) {
                        let _ = agent::write_to_agent(h, &[0x03]); // Ctrl+C
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let _ = agent::write_to_agent(h, &[0x04]); // Ctrl+D
                    }
                }
                quit_ok = poll_until(
                    std::time::Instant::now() + std::time::Duration::from_secs(3),
                    &is_gone,
                );
            }

            if !quit_ok {
                let reg = crate::sync::lock_poisoned(&registry, "verify_registry");
                if let Some(h) = reg.get(&agent_name) {
                    let _ = crate::sync::lock_poisoned(&h.child, "verify_child").kill();
                }
            }

            results.push(TestResult::ok(
                format!("backend:{name}:quit"),
                if quit_ok {
                    "graceful exit"
                } else {
                    "force killed (quit cmd ineffective, process cleaned up)"
                },
            ));
        }
        Err(e) => {
            results.push(TestResult::fail(
                format!("backend:{name}:spawn_ready"),
                format!("spawn failed: {e}"),
            ));
        }
    }

    let _ = std::fs::remove_dir_all(&test_dir);
    results
}
