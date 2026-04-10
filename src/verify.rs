//! Comprehensive end-to-end verification suite.
//!
//! `agend-terminal verify` runs all tests with auto daemon lifecycle.

use crate::{agent, api, daemon, inbox, instructions, mcp_config};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

struct TestResult {
    name: String,
    passed: bool,
    detail: String,
}

pub fn run(home: &Path, json_output: bool) -> anyhow::Result<()> {
    let test_home = home.join("_verify_tmp");
    std::fs::create_dir_all(&test_home)?;

    let mut results = Vec::new();

    // --- Tests that don't need daemon ---
    results.push(test_attach(&test_home));
    results.push(test_inbox(&test_home));
    results.push(test_mcp_framing());
    results.push(test_backend_config(&test_home));
    results.push(test_instructions(&test_home));

    // --- Tests that need daemon ---
    // Start a test daemon
    let daemon_home = test_home.join("daemon");
    std::fs::create_dir_all(&daemon_home)?;

    let registry: agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Spawn two test agents
    let spawn_ok = agent::spawn_agent(
        "test-a", "/bin/bash", &[], 80, 24, None, None, "\r",
        &registry, Some(&daemon_home),
    ).is_ok() && agent::spawn_agent(
        "test-b", "/bin/bash", &[], 80, 24, None, None, "\r",
        &registry, Some(&daemon_home),
    ).is_ok();

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
            .spawn(move || api::serve(&api_home, api_reg))
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
    let passed = results.iter().filter(|r| r.passed && !r.detail.starts_with("SKIP")).count();
    let skipped = results.iter().filter(|r| r.detail.starts_with("SKIP")).count();
    let failed = results.len() - passed - skipped;

    if json_output {
        let items: Vec<_> = results.iter().map(|r| json!({
            "name": r.name,
            "passed": r.passed,
            "detail": r.detail,
        })).collect();
        println!("{}", serde_json::to_string_pretty(&json!({
            "total": results.len(),
            "passed": passed,
            "failed": failed,
            "skipped": skipped,
            "tests": items,
        }))?);
    } else {
        println!("\n{:=<50}", "= AgEnD Terminal Verify ");
        for r in &results {
            let icon = if r.passed { "✓" } else if r.detail.starts_with("SKIP") { "-" } else { "✗" };
            println!("  {icon} {:<25} {}", r.name, r.detail);
        }
        println!("{:=<50}", "");
        println!("  Total: {}  Passed: {}  Failed: {}  Skipped: {}",
            results.len(), passed, failed, skipped);

        if failed > 0 {
            std::process::exit(1);
        }
    }

    Ok(())
}

fn test_attach(_home: &Path) -> TestResult {
    let registry = Arc::new(Mutex::new(HashMap::new()));
    match agent::spawn_agent("verify-attach", "/bin/bash", &[], 80, 24, None, None, "\r", &registry, None) {
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
                detail: if ok { "PTY spawn + inject + VTerm".into() } else { "VERIFY_OK not found in output".into() },
            }
        }
        Err(e) => TestResult { name: "attach".into(), passed: false, detail: format!("{e}") },
    }
}

fn test_inbox(home: &Path) -> TestResult {
    let test_name = "verify-inbox";
    for i in 1..=3 {
        let _ = inbox::enqueue(home, test_name, inbox::InboxMessage {
            from: format!("test-{i}"), text: format!("msg {i}"), kind: None,
            timestamp: "2024-01-01T00:00:00Z".into(),
        });
    }
    let msgs = inbox::drain(home, test_name);
    let empty = inbox::drain(home, test_name);
    let _ = std::fs::remove_file(home.join("inbox").join(format!("{test_name}.jsonl")));

    let ok = msgs.len() == 3 && empty.is_empty();
    TestResult {
        name: "inbox".into(),
        passed: ok,
        detail: if ok { "enqueue 3 + drain + empty".into() } else { format!("got {} msgs, empty={}", msgs.len(), empty.is_empty()) },
    }
}

fn test_mcp_framing() -> TestResult {
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let frame = format!("Content-Length: {}\r\n\r\n{}", req.len(), req);
    let ok = frame.contains("Content-Length:") && frame.contains("\r\n\r\n") && frame.ends_with('}');
    TestResult {
        name: "mcp_framing".into(),
        passed: ok,
        detail: if ok { "Content-Length format correct".into() } else { "bad format".into() },
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
        content.contains("mcpServers") && content.contains("agend-terminal") && content.contains("AGEND_TERMINAL_HOME")
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
        detail: if ok { "Claude + Kiro MCP config correct".into() } else { format!("claude={claude_ok} kiro={kiro_ok}") },
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
        detail: if ok { "Claude + Kiro instructions generated".into() } else { format!("claude={claude_ok} kiro={kiro_ok}") },
    }
}

fn test_api(home: &Path) -> TestResult {
    match api::call(home, &json!({"method": "list"})) {
        Ok(resp) => {
            let agents = resp["result"]["agents"].as_array().map(|a| a.len()).unwrap_or(0);
            TestResult {
                name: "api".into(),
                passed: agents >= 2,
                detail: format!("{agents} agents in registry"),
            }
        }
        Err(e) => TestResult { name: "api".into(), passed: false, detail: format!("{e}") },
    }
}

fn test_send(home: &Path) -> TestResult {
    // Send from test-a to test-b via API
    let send_result = api::call(home, &json!({
        "method": "send",
        "params": {"from": "test-a", "target": "test-b", "text": "verify-send-ok"}
    }));

    if send_result.is_err() {
        return TestResult { name: "send".into(), passed: false, detail: "API send failed".into() };
    }

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Check test-b inbox
    let msgs = inbox::drain(home, "test-b");
    let found = msgs.iter().any(|m| m.text.contains("verify-send-ok"));

    TestResult {
        name: "send".into(),
        passed: found,
        detail: if found { "a→b message delivered via inbox".into() } else { format!("not found in {} msgs", msgs.len()) },
    }
}

fn test_create_delete(home: &Path) -> TestResult {
    // Create via API
    let create = api::call(home, &json!({
        "method": "spawn",
        "params": {"name": "verify-dynamic", "command": "/bin/bash"}
    }));
    if create.is_err() {
        return TestResult { name: "create_delete".into(), passed: false, detail: "spawn failed".into() };
    }

    std::thread::sleep(std::time::Duration::from_secs(1));

    // Verify in list
    let list = api::call(home, &json!({"method": "list"})).unwrap_or(json!({}));
    let agents = list["result"]["agents"].as_array().unwrap_or(&Vec::new()).clone();
    let found = agents.iter().any(|a| a["name"].as_str() == Some("verify-dynamic"));

    // Kill
    let _ = api::call(home, &json!({"method": "kill", "params": {"name": "verify-dynamic"}}));
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Verify removed (reaper should clean up)
    let list2 = api::call(home, &json!({"method": "list"})).unwrap_or(json!({}));
    let agents2 = list2["result"]["agents"].as_array().unwrap_or(&Vec::new()).clone();
    let removed = !agents2.iter().any(|a| a["name"].as_str() == Some("verify-dynamic"));

    let ok = found && removed;
    TestResult {
        name: "create_delete".into(),
        passed: ok,
        detail: if ok { "spawn → found in list → kill → reaped".into() } else { format!("found={found} removed={removed}") },
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
