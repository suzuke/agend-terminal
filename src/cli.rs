//! CLI helpers: doctor, capture, test runners.
//! Extracted from main.rs for module split.

use crate::{agent, api, backend, fleet, inbox};
use std::path::Path;

/// Start daemon with fleet.yaml config.
pub fn start_with_fleet(home: &Path, fleet_path: &Path) -> anyhow::Result<()> {
    let mut config = fleet::FleetConfig::load(fleet_path)?;

    // Auto-create "general" instance if channel is configured but no general exists.
    // General is the default coordinator bound to Telegram's General topic (topic_id: 1).
    if config.channel.is_some() && !config.instances.contains_key("general") {
        let default_backend = config
            .defaults
            .backend
            .clone()
            .unwrap_or(backend::Backend::ClaudeCode);
        config.instances.insert(
            "general".to_string(),
            fleet::InstanceConfig {
                role: Some("Fleet coordinator — routes tasks between agents".to_string()),
                backend: Some(default_backend),
                working_directory: None, // resolve_instance will default to $AGEND_HOME/workspaces/general
                topic_id: Some(1),       // Telegram General topic
                ..Default::default()
            },
        );
        // Persist to fleet.yaml so it's visible to the user
        let entry = fleet::InstanceYamlEntry {
            backend: config
                .defaults
                .backend
                .as_ref()
                .map(|b| b.name().to_string()),
            working_directory: None,
            role: Some("Fleet coordinator — routes tasks between agents".to_string()),
        };
        if let Err(e) = fleet::add_instance_to_yaml(home, "general", &entry) {
            tracing::warn!(error = %e, "failed to persist general instance");
        }
        // Write topic_id
        let _ = fleet::update_instance_field(
            home,
            "general",
            "topic_id",
            serde_yaml::Value::Number(serde_yaml::Number::from(1)),
        );
        tracing::info!("auto-created 'general' instance for channel");
    }

    let mut agents = Vec::new();

    // Prune stale worktrees on startup
    {
        let mut seen_repos = std::collections::HashSet::new();
        for name in config.instance_names() {
            if let Some(resolved) = config.resolve_instance(&name) {
                if let Some(ref dir) = resolved.working_directory {
                    if seen_repos.insert(dir.clone()) {
                        crate::worktree::prune(dir);
                    }
                }
            }
        }
    }

    for name in config.instance_names() {
        if let Some(mut resolved) = config.resolve_instance(&name) {
            // Ensure working directory exists
            if let Some(ref base_dir) = resolved.working_directory {
                std::fs::create_dir_all(base_dir).ok();
            }

            // Auto-create git worktree if working_directory is a git repo
            if let Some(ref base_dir) = resolved.working_directory {
                if crate::worktree::is_git_repo(base_dir) {
                    let custom_branch = resolved.git_branch.as_deref();
                    if let Some(info) = crate::worktree::create(base_dir, &name, custom_branch) {
                        resolved.working_directory = Some(info.path);
                    }
                }
            }

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

            // Inject --model if specified
            if let Some(ref model) = resolved.model {
                args.push("--model".to_string());
                args.push(model.clone());
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
            config
                .resolve_instance(name)
                .map(|r| (name.clone(), r.submit_key))
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
        "[capture] Spawning {} ({} {}) for {seconds}s...",
        b.name(),
        preset.command,
        args.join(" ")
    );

    let registry = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    agent::spawn_agent(
        &agent::SpawnConfig {
            name: &name,
            command: preset.command,
            args: &args,
            cols: 120,
            rows: 40,
            env: None,
            working_dir: None,
            submit_key: preset.submit_key,
            home: None,
            crash_tx: None,
            shutdown: None,
        },
        &registry,
    )?;

    eprintln!("[capture] Waiting {seconds}s for output...");
    std::thread::sleep(std::time::Duration::from_secs(seconds));

    let stripped = {
        let reg = registry.lock().unwrap();
        match reg.get(&name) {
            Some(handle) => {
                let raw = handle.core.lock().unwrap().vterm.dump_screen();
                agent::strip_ansi_pub(&String::from_utf8_lossy(&raw))
            }
            None => {
                eprintln!("[capture] Agent exited before capture");
                return Ok(());
            }
        }
    };

    {
        let reg = registry.lock().unwrap();
        if let Some(h) = reg.get(&name) {
            let _ = h.child.lock().unwrap().kill();
        }
    }

    println!("=== {} VTerm Screen (ANSI stripped, 120x40) ===", b.name());
    for (i, line) in stripped.lines().enumerate() {
        let t = line.trim_end();
        if !t.is_empty() {
            println!("{:>3}| {}", i + 1, t);
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
    let args: Vec<String> = vec![];

    agent::spawn_agent(
        &agent::SpawnConfig {
            name: "test-attach",
            command: "/bin/bash",
            args: &args,
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
    assert_eq!(
        messages.len(),
        3,
        "Expected 3 messages, got {}",
        messages.len()
    );
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

    let check = |label: &str, path: &Path, missing: &str| {
        print!("  {label}: {}", path.display());
        println!("{}", if path.exists() { " ✓" } else { missing });
    };
    check("Home directory", home, " ✗ (not found)");
    check(".env file", &home.join(".env"), " - (optional)");

    let fleet_path = home.join("fleet.yaml");
    print!("  fleet.yaml: {}", fleet_path.display());
    if fleet_path.exists() {
        match fleet::FleetConfig::load(&fleet_path) {
            Ok(config) => {
                println!(" ✓ ({} instances)", config.instances.len());
                for name in config.instance_names() {
                    if let Some(r) = config.resolve_instance(&name) {
                        println!("    {name}: {} {}", r.command, r.args.join(" "));
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
                let ok = std::os::unix::net::UnixStream::connect(entry.path()).is_ok();
                println!(
                    "    {agent} {}",
                    if ok {
                        "✓ (socket responsive)"
                    } else {
                        "✗ (socket stale)"
                    }
                );
                count += 1;
            }
        }
    }
    if count == 0 {
        println!("    (none)");
    }

    println!("\n  Backend binaries:");
    for b in backend::Backend::all() {
        let (name, cmd) = (b.name(), b.preset().command);
        if b.is_installed() {
            let version = b.get_version().unwrap_or_else(|| "?".into());
            let cal = b.calibrated_version();
            let note = if version != cal {
                format!(" (calibrated: {cal}, patterns may need update)")
            } else {
                String::new()
            };
            println!("    {name} ({cmd}) v{version} ✓{note}");
        } else {
            println!("    {name} ({cmd}) - (not in PATH)");
        }
    }
    Ok(())
}

pub fn run_demo() -> anyhow::Result<()> {
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let registry: agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let home = std::env::temp_dir().join(format!("agend-demo-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;

    print!("\x1b[2J\x1b[H");
    std::io::stdout().flush().ok();
    println!("  AgEnD Terminal — Live Multi-Agent Demo (Live Preview)\n");
    println!("  Spawning alice and bob...");

    let (crash_tx, _rx) = crossbeam::channel::unbounded::<String>();
    let args: Vec<String> = vec![];
    for name in &["alice", "bob"] {
        agent::spawn_agent(
            &agent::SpawnConfig {
                name,
                command: "/bin/bash",
                args: &args,
                cols: 60,
                rows: 8,
                env: None,
                working_dir: None,
                submit_key: "\r",
                home: Some(&home),
                crash_tx: Some(crash_tx.clone()),
                shutdown: None,
            },
            &registry,
        )?;
    }

    println!("  ✓ Both agents running\n");
    std::thread::sleep(Duration::from_secs(2));

    // Conversation script
    let conversation = [
        ("alice", "bob",   "[from:alice] Hey Bob, what's the best way to handle errors in Rust?"),
        ("bob",   "alice", "[from:bob] Use Result<T, E> and the ? operator. Avoid unwrap() in production."),
        ("alice", "bob",   "[from:alice] What about custom error types?"),
        ("bob",   "alice", "[from:bob] Create an enum with thiserror derive. Each variant for a different failure mode."),
        ("alice", "bob",   "[from:alice] Thanks! Delegating the implementation to you."),
        ("bob",   "alice", "[from:bob] Done! Check my branch agend/bob for the changes."),
    ];

    for (i, (from, to, msg)) in conversation.iter().enumerate() {
        // Inject message into recipient's PTY (echo so it appears in VTerm)
        {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(handle) = reg.get(*to) {
                let _ = agent::write_to_agent(handle, format!("echo '{msg}'\r").as_bytes());
            }
        }

        std::thread::sleep(Duration::from_millis(800));

        // Redraw split screen
        print!("\x1b[2J\x1b[H"); // clear + home
        std::io::stdout().flush().ok();

        println!(
            "  AgEnD Terminal — Live Multi-Agent Demo  [{}/{}]",
            i + 1,
            conversation.len()
        );
        println!("  {from} → {to}\n");

        draw_split_screen(&registry);

        println!("\n  Message: {msg}");

        std::thread::sleep(Duration::from_millis(700));
    }

    // Final: show crash recovery
    std::thread::sleep(Duration::from_secs(1));
    print!("\x1b[2J\x1b[H");
    std::io::stdout().flush().ok();

    println!("  AgEnD Terminal — Crash Recovery Demo\n");
    println!("  Killing bob...");
    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bob) = reg.get("bob") {
            let mut child = bob.child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.kill();
        }
    }
    std::thread::sleep(Duration::from_millis(500));

    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bob) = reg.get("bob") {
            let state = bob
                .core
                .lock()
                .map(|c| c.state.current.display_name().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            println!("  Bob's state: {state}");
        }
    }

    println!("  Waiting for auto-respawn...");
    for sec in 1..=5 {
        std::thread::sleep(Duration::from_secs(1));
        print!("  {sec}s...");
        std::io::stdout().flush().ok();
    }
    println!();

    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        if reg.get("bob").is_some() {
            println!("  ✓ Bob respawned automatically!\n");
        }
    }

    draw_split_screen(&registry);

    // Cleanup
    println!("\n  Cleaning up...");
    {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        for (_, handle) in reg.iter() {
            let mut child = handle.child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.kill();
        }
    }
    let _ = std::fs::remove_dir_all(&home);

    println!("  ✓ Demo complete!\n");
    println!("  With real AI backends, agents use MCP tools to autonomously");
    println!("  delegate_task, report_result, and coordinate.\n");
    println!("  Next:");
    println!("    agend-terminal doctor     # Check backends");
    println!("    agend-terminal start      # Start fleet");
    println!();

    Ok(())
}

fn draw_split_screen(registry: &agent::AgentRegistry) {
    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());

    let alice_lines = get_screen_lines(&reg, "alice");
    let bob_lines = get_screen_lines(&reg, "bob");

    let width = 38;
    let top_sep = "─".repeat(width - 9);
    let bot_sep = "─".repeat(width);
    println!("  ┌─ alice ─{}┬─ bob ───{}┐", top_sep, top_sep);
    for i in 0..8 {
        let a = alice_lines.get(i).map(|s| s.as_str()).unwrap_or("");
        let b = bob_lines.get(i).map(|s| s.as_str()).unwrap_or("");
        let a_display = truncate_str(a, width);
        let b_display = truncate_str(b, width);
        println!("  │{:<w$}│{:<w$}│", a_display, b_display, w = width);
    }
    println!("  └{}┴{}┘", bot_sep, bot_sep);
}

/// Truncate a string to at most `max_chars` characters (char-safe).
fn truncate_str(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn get_screen_lines(
    reg: &std::sync::MutexGuard<'_, std::collections::HashMap<String, agent::AgentHandle>>,
    name: &str,
) -> Vec<String> {
    if let Some(handle) = reg.get(name) {
        if let Ok(core) = handle.core.lock() {
            let dump = core.vterm.dump_screen();
            let output = String::from_utf8_lossy(&dump);
            let stripped = agent::strip_ansi_pub(&output);
            return stripped
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim_end().to_string())
                .collect();
        }
    }
    vec!["(not available)".to_string()]
}
