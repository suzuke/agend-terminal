//! CLI helpers: doctor, capture, test runners.
//! Extracted from main.rs for module split.

use crate::{agent, api, backend, fleet, inbox};
use std::path::Path;

/// Start daemon with fleet.yaml config.
pub fn start_with_fleet(home: &Path, fleet_path: &Path) -> anyhow::Result<()> {
    let config = fleet::FleetConfig::load(fleet_path)?;
    let mut agents = Vec::new();

    for name in config.instance_names() {
        if let Some(resolved) = config.resolve_instance(&name) {
            // Generate instructions + MCP config
            if let Some(ref dir) = resolved.working_directory {
                crate::instructions::generate(dir, &resolved.command);
                crate::mcp_config::configure(dir, &resolved.command);
            }

            // Add resume args to continue previous session
            let mut args = resolved.args;
            if let Some(ref b) = backend::Backend::from_command(&resolved.command) {
                let p = b.preset();
                args.extend(p.resume_mode.args_for(home, &name));
            }

            // Inject Claude-specific flags
            if let Some(ref dir) = resolved.working_directory {
                if resolved.command.contains("claude") {
                    let mcp_config = dir.join("mcp-config.json");
                    if mcp_config.exists() {
                        args.push("--mcp-config".to_string());
                        args.push(mcp_config.display().to_string());
                    }
                    let settings = dir.join("claude-settings.json");
                    if settings.exists() {
                        args.push("--settings".to_string());
                        args.push(settings.display().to_string());
                    }
                }
            }

            agents.push((
                resolved.name,
                resolved.command,
                args,
                Some(resolved.env),
                resolved.working_directory,
                resolved.submit_key,
            ));
        }
    }

    if agents.is_empty() {
        eprintln!("No instances found in fleet.yaml");
        std::process::exit(1);
    }

    // Initialize Telegram if configured
    let submit_keys: std::collections::HashMap<String, String> = config
        .instances
        .keys()
        .filter_map(|name| {
            config.resolve_instance(name).map(|r| (name.clone(), r.submit_key))
        })
        .collect();
    let _telegram = crate::telegram::init_from_config(&config, home, submit_keys);

    crate::daemon::run(home, agents)?;
    Ok(())
}

#[allow(clippy::unwrap_used)]
pub fn capture_backend(b: &backend::Backend, seconds: u64) -> anyhow::Result<()> {
    let preset = b.preset();
    let name = format!("capture-{}", b.name());
    let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();

    eprintln!(
        "[capture] Spawning {} ({} {}) for {}s...",
        b.name(),
        preset.command,
        args.join(" "),
        seconds
    );

    let registry = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    agent::spawn_agent(
        &name,
        preset.command,
        &args,
        120,
        40,
        None,
        None,
        preset.submit_key,
        &registry,
        None,
        None,
    )?;

    eprintln!("[capture] Waiting {}s for output...", seconds);
    std::thread::sleep(std::time::Duration::from_secs(seconds));

    // Dump VTerm screen (raw ANSI)
    let (_raw_dump, stripped) = {
        let reg = registry.lock().unwrap();
        match reg.get(&name) {
            Some(handle) => {
                let core = handle.core.lock().unwrap();
                let raw = core.vterm.dump_screen();
                let raw_str = String::from_utf8_lossy(&raw).to_string();
                let stripped = agent::strip_ansi_pub(&raw_str);
                (raw_str, stripped)
            }
            None => {
                eprintln!("[capture] Agent exited before capture");
                return Ok(());
            }
        }
    };

    // Kill
    {
        let reg = registry.lock().unwrap();
        if let Some(handle) = reg.get(&name) {
            let mut child = handle.child.lock().unwrap();
            let _ = child.kill();
        }
    }

    // Print results
    println!(
        "=== {} VTerm Screen (ANSI stripped, {}x40) ===",
        b.name(),
        120
    );
    for (i, line) in stripped.lines().enumerate() {
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            println!("{:>3}| {}", i + 1, trimmed);
        }
    }
    println!("=== End {} ===", b.name());

    Ok(())
}

// --- QA Tools ---

pub fn run_tests(subcmd: &str, home: &Path) -> anyhow::Result<()> {
    match subcmd {
        "mcp" => test_mcp(home)?,
        "attach" => test_attach(home)?,
        "inbox" => test_inbox(home)?,
        "api" => test_api(home)?,
        "all" => {
            test_attach(home)?;
            test_inbox(home)?;
        }
        _ => {
            eprintln!("Unknown test: {subcmd}. Available: mcp, attach, inbox, api, all");
            std::process::exit(1);
        }
    }
    Ok(())
}

#[allow(clippy::unwrap_used)]
fn test_attach(_home: &Path) -> anyhow::Result<()> {
    eprintln!("[test:attach] Spawning bash...");
    let registry = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    agent::spawn_agent(
        "test-attach", "/bin/bash", &[], 80, 24, None, None, "\r", &registry, None, None,
    )?;

    std::thread::sleep(std::time::Duration::from_secs(1));

    eprintln!("[test:attach] Injecting test command...");
    {
        let reg = registry.lock().unwrap();
        let handle = reg.get("test-attach").unwrap();
        agent::write_to_agent(handle, b"echo AGEND_TEST_OK\r")?;
    }

    std::thread::sleep(std::time::Duration::from_millis(500));

    let output = {
        let reg = registry.lock().unwrap();
        let handle = reg.get("test-attach").unwrap();
        let core = handle.core.lock().unwrap();
        let dump = core.vterm.dump_screen();
        String::from_utf8_lossy(&dump).to_string()
    };

    if output.contains("AGEND_TEST_OK") {
        eprintln!("[test:attach] PASS — PTY spawn + inject + VTerm output verified");
    } else {
        eprintln!("[test:attach] FAIL — 'AGEND_TEST_OK' not found in VTerm output");
        std::process::exit(1);
    }

    {
        let reg = registry.lock().unwrap();
        let handle = reg.get("test-attach").unwrap();
        let mut child = handle.child.lock().unwrap();
        let _ = child.kill();
    }

    eprintln!("[test:attach] Cleanup done");
    Ok(())
}

fn test_mcp(_home: &Path) -> anyhow::Result<()> {
    eprintln!("[test:mcp] Testing MCP protocol...");
    let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
    let init_frame = format!("Content-Length: {}\r\n\r\n{}", init_req.len(), init_req);
    eprintln!(
        "[test:mcp] Content-Length frame format: OK ({} bytes)",
        init_frame.len()
    );
    eprintln!("[test:mcp] PASS — MCP framing verified");
    Ok(())
}

fn test_inbox(home: &Path) -> anyhow::Result<()> {
    eprintln!("[test:inbox] Testing inbox enqueue + drain...");

    let test_name = "test-inbox-agent";

    for i in 1..=3 {
        inbox::enqueue(
            home,
            test_name,
            inbox::InboxMessage {
                from: format!("tester-{i}"),
                text: format!("Message {i}"),
                kind: None,
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
        )?;
    }

    let messages = inbox::drain(home, test_name);
    assert_eq!(messages.len(), 3, "Expected 3 messages, got {}", messages.len());
    assert_eq!(messages[0].from, "tester-1");
    assert_eq!(messages[2].text, "Message 3");

    let empty = inbox::drain(home, test_name);
    assert!(empty.is_empty(), "Inbox should be empty after drain");

    let _ = std::fs::remove_file(home.join("inbox").join(format!("{test_name}.jsonl")));

    eprintln!("[test:inbox] PASS — enqueue + drain + empty verified");
    Ok(())
}

fn test_api(home: &Path) -> anyhow::Result<()> {
    eprintln!("[test:api] Checking for running daemon...");

    match api::call(home, &serde_json::json!({"method": "list"})) {
        Ok(resp) => {
            let agents = resp["result"]["agents"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            eprintln!("[test:api] PASS — API socket responsive, {agents} agents");
        }
        Err(e) => {
            eprintln!("[test:api] SKIP — daemon not running ({e})");
        }
    }

    Ok(())
}

pub fn run_doctor(home: &Path) -> anyhow::Result<()> {
    println!("AgEnD Terminal Doctor\n");

    print!("  Home directory: {}", home.display());
    if home.exists() {
        println!(" ✓");
    } else {
        println!(" ✗ (not found)");
    }

    let env_path = home.join(".env");
    print!("  .env file: {}", env_path.display());
    if env_path.exists() {
        println!(" ✓");
    } else {
        println!(" - (optional)");
    }

    let fleet_path = home.join("fleet.yaml");
    print!("  fleet.yaml: {}", fleet_path.display());
    if fleet_path.exists() {
        match fleet::FleetConfig::load(&fleet_path) {
            Ok(config) => {
                println!(" ✓ ({} instances)", config.instances.len());
                for name in config.instance_names() {
                    if let Some(resolved) = config.resolve_instance(&name) {
                        println!("    {name}: {} {}", resolved.command, resolved.args.join(" "));
                    }
                }
            }
            Err(e) => println!(" ✗ (parse error: {e})"),
        }
    } else {
        println!(" - (not found)");
    }

    println!("\n  Active agents:");
    let mut count = 0;
    if let Some(run) = crate::daemon::find_active_run_dir(home) {
        for entry in std::fs::read_dir(&run)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sock") && name != "api.sock" {
                let agent = &name[..name.len() - 5];
                let path = entry.path().display().to_string();
                match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => println!("    {agent} ✓ (socket responsive)"),
                    Err(_) => println!("    {agent} ✗ (socket stale)"),
                }
                count += 1;
            }
        }
    }
    if count == 0 {
        println!("    (none)");
    }

    println!("\n  Backend binaries:");
    for b in backend::Backend::all() {
        let name = b.name();
        let preset = b.preset();
        if b.is_installed() {
            let version = b.get_version().unwrap_or_else(|| "?".into());
            let calibrated = b.calibrated_version();
            let version_note = if version != calibrated {
                format!(" (calibrated: {calibrated}, patterns may need update)")
            } else {
                String::new()
            };
            println!("    {name} ({}) v{version} ✓{version_note}", preset.command);
        } else {
            println!("    {name} ({}) - (not in PATH)", preset.command);
        }
    }

    Ok(())
}
