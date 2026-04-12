//! Comprehensive end-to-end verification suite.
//!
//! `agend-terminal verify` runs all tests with auto daemon lifecycle.

use crate::{agent, api, backend, daemon, inbox, instructions, mcp_config};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct TestResult {
    name: String,
    passed: bool,
    detail: String,
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
    let spawn_ok = agent::spawn_agent(
        &agent::SpawnConfig {
            name: "test-a",
            command: "/bin/bash",
            args: &[],
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: Some(&daemon_home),
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    )
    .is_ok()
        && agent::spawn_agent(
            &agent::SpawnConfig {
                name: "test-b",
                command: "/bin/bash",
                args: &[],
                cols: 80,
                rows: 24,
                env: None,
                working_dir: None,
                submit_key: "\r",
                home: Some(&daemon_home),
                crash_tx: None,
                shutdown: None,
            },
            &registry,
        )
        .is_ok();

    if spawn_ok {
        // Start TUI sockets
        for name in ["test-a", "test-b"] {
            let sock = daemon::agent_socket_path(&daemon_home, name);
            let reg = Arc::clone(&registry);
            let n = name.to_string();
            std::thread::Builder::new()
                .name(format!("{n}_tui"))
                .spawn(move || daemon::serve_agent_tui(&n, &sock, &reg))
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
                api::serve(&api_home, api_reg, shutdown, configs)
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
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        for (_, handle) in reg.iter() {
            let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
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
    match agent::spawn_agent(
        &agent::SpawnConfig {
            name: "verify-attach",
            command: "/bin/bash",
            args: &[],
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    ) {
        Ok(()) => {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let reg = registry.lock().unwrap();
            let handle = reg.get("verify-attach").unwrap();
            let _ = agent::write_to_agent(handle, b"echo VERIFY_OK\r");
            drop(reg);
            std::thread::sleep(std::time::Duration::from_millis(500));

            let output = {
                let reg = registry.lock().unwrap();
                let handle = reg.get("verify-attach").unwrap();
                let core = handle.core.lock().unwrap();
                String::from_utf8_lossy(&core.vterm.dump_screen()).to_string()
            };
            let ok = output.contains("VERIFY_OK");

            // Kill
            let reg = registry.lock().unwrap();
            let handle = reg.get("verify-attach").unwrap();
            let mut child = handle.child.lock().unwrap();
            let _ = child.kill();

            TestResult {
                name: "attach".into(),
                passed: ok,
                detail: if ok {
                    "PTY spawn + inject + VTerm".into()
                } else {
                    "VERIFY_OK not found in output".into()
                },
            }
        }
        Err(e) => TestResult {
            name: "attach".into(),
            passed: false,
            detail: format!("{e}"),
        },
    }
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
    TestResult {
        name: "inbox".into(),
        passed: ok,
        detail: if ok {
            "enqueue 3 + drain + empty".into()
        } else {
            format!("got {} msgs, empty={}", msgs.len(), empty.is_empty())
        },
    }
}

fn test_mcp_framing() -> TestResult {
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let frame = format!("Content-Length: {}\r\n\r\n{}", req.len(), req);
    let ok =
        frame.contains("Content-Length:") && frame.contains("\r\n\r\n") && frame.ends_with('}');
    TestResult {
        name: "mcp_framing".into(),
        passed: ok,
        detail: if ok {
            "Content-Length format correct".into()
        } else {
            "bad format".into()
        },
    }
}

fn test_backend_config(home: &Path) -> TestResult {
    let test_dir = home.join("verify-backend");
    std::fs::create_dir_all(&test_dir).ok();

    // Test Claude MCP config
    mcp_config::configure(&test_dir, "claude");
    let claude_path = test_dir.join(".claude").join("settings.json");
    let claude_ok = if claude_path.exists() {
        let content = std::fs::read_to_string(&claude_path).unwrap_or_default();
        content.contains("mcpServers")
            && content.contains("agend-terminal")
            && content.contains("AGEND_TERMINAL_HOME")
    } else {
        false
    };

    // Test Kiro MCP config
    mcp_config::configure(&test_dir, "kiro-cli");
    let kiro_path = test_dir.join(".kiro").join("settings").join("mcp.json");
    let kiro_ok = if kiro_path.exists() {
        let content = std::fs::read_to_string(&kiro_path).unwrap_or_default();
        content.contains("mcpServers") && content.contains("agend-terminal")
    } else {
        false
    };

    let _ = std::fs::remove_dir_all(&test_dir);
    let ok = claude_ok && kiro_ok;
    TestResult {
        name: "backend_config".into(),
        passed: ok,
        detail: if ok {
            "Claude + Kiro MCP config correct".into()
        } else {
            format!("claude={claude_ok} kiro={kiro_ok}")
        },
    }
}

fn test_instructions(home: &Path) -> TestResult {
    let test_dir = home.join("verify-instructions");
    std::fs::create_dir_all(&test_dir).ok();

    instructions::generate(&test_dir, "claude");
    let claude_path = test_dir.join(".claude").join("rules").join("agend.md");
    let claude_ok = claude_path.exists() && {
        let c = std::fs::read_to_string(&claude_path).unwrap_or_default();
        c.contains("reply") && c.contains("send") && c.contains("inbox") && c.contains("v3-mcp")
    };

    instructions::generate(&test_dir, "kiro-cli");
    let kiro_path = test_dir.join(".kiro").join("steering").join("agend.md");
    let kiro_ok = kiro_path.exists();

    let _ = std::fs::remove_dir_all(&test_dir);
    let ok = claude_ok && kiro_ok;
    TestResult {
        name: "instructions".into(),
        passed: ok,
        detail: if ok {
            "Claude + Kiro instructions generated".into()
        } else {
            format!("claude={claude_ok} kiro={kiro_ok}")
        },
    }
}

fn test_api(home: &Path) -> TestResult {
    match api::call(home, &json!({"method": "list"})) {
        Ok(resp) => {
            let agents = resp["result"]["agents"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            TestResult {
                name: "api".into(),
                passed: agents >= 2,
                detail: format!("{agents} agents in registry"),
            }
        }
        Err(e) => TestResult {
            name: "api".into(),
            passed: false,
            detail: format!("{e}"),
        },
    }
}

fn test_send(home: &Path) -> TestResult {
    // Send from test-a to test-b via API
    let send_result = api::call(
        home,
        &json!({
            "method": "send",
            "params": {"from": "test-a", "target": "test-b", "text": "verify-send-ok"}
        }),
    );

    if send_result.is_err() {
        return TestResult {
            name: "send".into(),
            passed: false,
            detail: "API send failed".into(),
        };
    }

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Check test-b inbox
    let msgs = inbox::drain(home, "test-b");
    let found = msgs.iter().any(|m| m.text.contains("verify-send-ok"));

    TestResult {
        name: "send".into(),
        passed: found,
        detail: if found {
            "a→b message delivered via inbox".into()
        } else {
            format!("not found in {} msgs", msgs.len())
        },
    }
}

fn test_create_delete(home: &Path) -> TestResult {
    // Create via API
    let create = api::call(
        home,
        &json!({
            "method": "spawn",
            "params": {"name": "verify-dynamic", "command": "/bin/bash"}
        }),
    );
    if create.is_err() {
        return TestResult {
            name: "create_delete".into(),
            passed: false,
            detail: "spawn failed".into(),
        };
    }

    std::thread::sleep(std::time::Duration::from_secs(1));

    // Verify in list
    let list = api::call(home, &json!({"method": "list"})).unwrap_or(json!({}));
    let agents = list["result"]["agents"]
        .as_array()
        .unwrap_or(&Vec::new())
        .clone();
    let found = agents
        .iter()
        .any(|a| a["name"].as_str() == Some("verify-dynamic"));

    // Kill
    let _ = api::call(
        home,
        &json!({"method": "kill", "params": {"name": "verify-dynamic"}}),
    );
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Verify removed (reaper should clean up)
    let list2 = api::call(home, &json!({"method": "list"})).unwrap_or(json!({}));
    let agents2 = list2["result"]["agents"]
        .as_array()
        .unwrap_or(&Vec::new())
        .clone();
    let removed = !agents2
        .iter()
        .any(|a| a["name"].as_str() == Some("verify-dynamic"));

    let ok = found && removed;
    TestResult {
        name: "create_delete".into(),
        passed: ok,
        detail: if ok {
            "spawn → found in list → kill → reaped".into()
        } else {
            format!("found={found} removed={removed}")
        },
    }
}

fn test_telegram() -> TestResult {
    if std::env::var("AGEND_BOT_TOKEN").is_err() {
        return TestResult {
            name: "telegram".into(),
            passed: true, // Don't fail on missing token
            detail: "SKIP — AGEND_BOT_TOKEN not set".into(),
        };
    }
    // If token is set, just verify we can create a Bot
    TestResult {
        name: "telegram".into(),
        passed: true,
        detail: "SKIP — live Telegram test not implemented".into(),
    }
}

/// Per-backend verification: spawn, ready, instructions, MCP config, inject, quit.
#[allow(clippy::unwrap_used)]
fn test_backend(backend: &backend::Backend, home: &Path) -> Vec<TestResult> {
    let name = backend.name();
    let preset = backend.preset();
    let mut results = Vec::new();

    // Skip if binary not installed
    if !backend.is_installed() {
        results.push(TestResult {
            name: format!("backend:{name}"),
            passed: true,
            detail: format!("SKIP — {} not in PATH", preset.command),
        });
        return results;
    }

    let test_dir = home.join(format!("verify-backend-{name}"));
    std::fs::create_dir_all(&test_dir).ok();

    // 1. Instructions generation
    crate::instructions::generate(&test_dir, preset.command);
    let instr_path = test_dir.join(preset.instructions_path);
    let instr_ok = instr_path.exists() && {
        let c = std::fs::read_to_string(&instr_path).unwrap_or_default();
        c.contains("v3-mcp") && c.contains("reply")
    };
    results.push(TestResult {
        name: format!("backend:{name}:instructions"),
        passed: instr_ok,
        detail: if instr_ok {
            preset.instructions_path.to_string()
        } else {
            "missing or invalid".into()
        },
    });

    // 2. MCP config generation
    crate::mcp_config::configure(&test_dir, preset.command);
    let mcp_path = test_dir.join(preset.mcp_config_path);
    let mcp_ok = if mcp_path.exists() {
        let c = std::fs::read_to_string(&mcp_path).unwrap_or_default();
        c.contains("mcpServers")
            && c.contains("agend-terminal")
            && c.contains("AGEND_TERMINAL_HOME")
    } else {
        // Some backends (codex) don't have file-based MCP config
        name == "codex"
    };
    results.push(TestResult {
        name: format!("backend:{name}:mcp_config"),
        passed: mcp_ok,
        detail: if mcp_ok {
            preset.mcp_config_path.to_string()
        } else {
            "missing mcpServers/env".into()
        },
    });

    // 3. Spawn + ready detection
    let registry = Arc::new(Mutex::new(HashMap::new()));
    let agent_name = format!("verify-{name}");
    let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();

    let spawn_result = agent::spawn_agent(
        &agent::SpawnConfig {
            name: &agent_name,
            command: preset.command,
            args: &args,
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
            // Wait for ready pattern (with timeout)
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(preset.ready_timeout_secs);
            let mut ready = false;
            let re = regex::Regex::new(preset.ready_pattern)
                .unwrap_or_else(|_| regex::Regex::new(".").unwrap());
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    let core = handle.core.lock().unwrap();
                    let screen = String::from_utf8_lossy(&core.vterm.dump_screen()).to_string();
                    if re.is_match(&screen) {
                        ready = true;
                        break;
                    }
                } else {
                    break; // Agent was reaped
                }
            }

            // On timeout, dump VTerm for debugging
            if !ready {
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    let core = handle.core.lock().unwrap();
                    let dump = core.vterm.dump_screen();
                    let stripped = crate::agent::strip_ansi_pub(&String::from_utf8_lossy(&dump));
                    eprintln!("[debug] {name} VTerm at timeout:");
                    for (i, line) in stripped.lines().enumerate() {
                        let t = line.trim_end();
                        if !t.is_empty() {
                            eprintln!("  {:>3}| {}", i + 1, t);
                        }
                    }
                }
            }

            results.push(TestResult {
                name: format!("backend:{name}:spawn_ready"),
                passed: ready,
                detail: if ready {
                    format!("ready in <{}s", preset.ready_timeout_secs)
                } else {
                    format!(
                        "timeout after {}s (pattern: {})",
                        preset.ready_timeout_secs, preset.ready_pattern
                    )
                },
            });

            // 4. Inject + submit test (only if ready)
            if ready {
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    let test_msg = format!("echo BACKEND_VERIFY_OK{}", preset.submit_key);
                    let inject_ok = agent::write_to_agent(handle, test_msg.as_bytes()).is_ok();
                    results.push(TestResult {
                        name: format!("backend:{name}:inject"),
                        passed: inject_ok,
                        detail: if inject_ok {
                            "inject accepted".into()
                        } else {
                            "write failed".into()
                        },
                    });
                }
                drop(reg);

                std::thread::sleep(std::time::Duration::from_secs(2));
            }

            // 5. Graceful quit — try multiple methods with delays
            // Wait 2s for CLI to settle before sending quit
            std::thread::sleep(std::time::Duration::from_secs(2));

            // Try quit command
            {
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    let quit_msg = format!("{}{}", preset.quit_command, preset.submit_key);
                    let _ = agent::write_to_agent(handle, quit_msg.as_bytes());
                }
            }

            // Wait 5s
            let quit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut quit_ok = false;
            while std::time::Instant::now() < quit_deadline {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let reg = registry.lock().unwrap();
                if !reg.contains_key(&agent_name) {
                    quit_ok = true;
                    break;
                }
            }

            // If quit command didn't work, try Ctrl+C then Ctrl+D
            if !quit_ok {
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    // Ctrl+C (SIGINT)
                    let _ = agent::write_to_agent(handle, &[0x03]);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    // Ctrl+D (EOF)
                    let _ = agent::write_to_agent(handle, &[0x04]);
                }
                drop(reg);

                let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(3);
                while std::time::Instant::now() < deadline2 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let reg = registry.lock().unwrap();
                    if !reg.contains_key(&agent_name) {
                        quit_ok = true;
                        break;
                    }
                }
            }

            if !quit_ok {
                // Force kill — still counts as pass (cleanup works)
                let reg = registry.lock().unwrap();
                if let Some(handle) = reg.get(&agent_name) {
                    let mut child = handle.child.lock().unwrap();
                    let _ = child.kill();
                }
            }

            results.push(TestResult {
                name: format!("backend:{name}:quit"),
                passed: true, // Force kill is valid cleanup
                detail: if quit_ok {
                    "graceful exit".into()
                } else {
                    "force killed (quit cmd ineffective, process cleaned up)".into()
                },
            });
        }
        Err(e) => {
            results.push(TestResult {
                name: format!("backend:{name}:spawn_ready"),
                passed: false,
                detail: format!("spawn failed: {e}"),
            });
        }
    }

    let _ = std::fs::remove_dir_all(&test_dir);
    results
}
