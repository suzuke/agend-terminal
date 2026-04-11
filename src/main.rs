mod agent;
mod api;
mod backend;
#[allow(dead_code)]
mod channel;
mod cli;
mod daemon;
#[allow(dead_code)]
mod error;
mod fleet;
mod framing;
#[allow(dead_code)]
mod health;
mod inbox;
mod instructions;
mod mcp;
mod mcp_config;
mod state;
mod telegram;
mod tui;
mod verify;
mod vterm;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AGEND_TERMINAL_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(".agend-terminal")
}

/// Load .env file from AGEND_TERMINAL_HOME.
fn load_dotenv() {
    let env_path = home_dir().join(".env");
    if !env_path.exists() {
        return;
    }
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            let value = if let Some(quoted) = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            {
                quoted
            } else {
                value.split('#').next().unwrap_or(value).trim()
            };
            if !key.is_empty() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// AgEnD Terminal — Agent Process Manager
#[derive(Parser)]
#[command(name = "agend-terminal", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start daemon with fleet.yaml
    Start,
    /// Start daemon with explicit agent specs (name:cmd ...)
    Daemon {
        /// Agent specs in name:command format
        agents: Vec<String>,
    },
    /// Attach to an agent's terminal (Ctrl+B d to detach)
    Attach {
        /// Agent name
        #[arg(default_value = "shell")]
        name: String,
    },
    /// Send input to an agent's PTY
    Inject {
        /// Agent name
        name: String,
        /// Text to inject
        text: Vec<String>,
    },
    /// List running agents
    #[command(alias = "ls")]
    List,
    /// Show detailed agent status (state, health)
    Status,
    /// Stop the daemon
    Stop,
    /// Kill a specific agent
    Kill {
        /// Agent name
        name: String,
    },
    /// Fleet management
    Fleet {
        #[command(subcommand)]
        command: FleetCommands,
    },
    /// Start MCP stdio server
    Mcp,
    /// Capture backend output for debugging
    Capture {
        /// Backend name (claude-code, kiro-cli, codex, open-code, gemini)
        #[arg(long)]
        backend: String,
        /// Capture duration in seconds
        #[arg(long, default_value = "15")]
        seconds: u64,
    },
    /// Run tests
    Test {
        /// Test suite (mcp, attach, inbox, api, all)
        #[arg(default_value = "all")]
        suite: String,
    },
    /// Full E2E verification
    Verify {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by backend
        #[arg(long)]
        backend: Option<String>,
    },
    /// Health check
    Doctor,
}

#[derive(Subcommand)]
enum FleetCommands {
    /// Start fleet from config
    Start {
        /// Path to fleet config YAML
        config: Option<String>,
    },
    /// Stop all fleet agents
    Stop,
}

fn main() -> anyhow::Result<()> {
    load_dotenv();

    let cli = Cli::parse();
    let home = home_dir();
    std::fs::create_dir_all(&home)?;

    match cli.command {
        None => {
            // Print help
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
        }
        Some(Commands::Start) => {
            let fleet_path = home.join("fleet.yaml");
            if fleet_path.exists() {
                cli::start_with_fleet(&home, &fleet_path)?;
            } else {
                daemon::run(
                    &home,
                    vec![(
                        "shell".to_string(),
                        "/bin/bash".to_string(),
                        Vec::new(),
                        None,
                        None,
                        "\r".to_string(),
                    )],
                )?;
            }
        }
        Some(Commands::Daemon { agents }) => {
            let agents: Vec<_> = agents
                .iter()
                .map(|a| {
                    let (name, cmd) = if let Some((n, c)) = a.split_once(':') {
                        (n.to_string(), c.to_string())
                    } else {
                        (a.to_string(), a.to_string())
                    };
                    let detected = backend::Backend::from_command(&cmd);
                    let (preset_args, submit_key) = match detected {
                        Some(ref b) => {
                            let p = b.preset();
                            let mut a: Vec<String> =
                                p.args.iter().map(|s| s.to_string()).collect();
                            a.extend(p.resume_mode.args_for(&home, &name));
                            (a, p.submit_key.to_string())
                        }
                        None => (Vec::new(), "\r".to_string()),
                    };
                    (name, cmd, preset_args, None, None, submit_key)
                })
                .collect();
            let agents = if agents.is_empty() {
                vec![(
                    "shell".to_string(),
                    "/bin/bash".to_string(),
                    Vec::new(),
                    None,
                    None,
                    "\r".to_string(),
                )]
            } else {
                agents
            };
            daemon::run(&home, agents)?;
        }
        Some(Commands::Attach { name }) => {
            let sock = daemon::agent_socket_path(&home, &name);
            tui::attach(&sock)?;
        }
        Some(Commands::Inject { name, text }) => {
            let text = text.join(" ");
            if text.is_empty() {
                anyhow::bail!("No text provided");
            }
            match api::call(
                &home,
                &serde_json::json!({
                    "method": "inject",
                    "params": {"name": name, "data": text}
                }),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!("Injected: {text}");
                }
                Ok(resp) => {
                    eprintln!(
                        "Inject failed: {}",
                        resp["error"].as_str().unwrap_or("unknown")
                    );
                }
                Err(e) => {
                    eprintln!("Failed to connect to daemon: {e}");
                }
            }
        }
        Some(Commands::Stop) => {
            match api::call(&home, &serde_json::json!({"method": "shutdown"})) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!("Daemon shutdown initiated.");
                }
                Ok(_) => eprintln!("Shutdown request failed."),
                Err(e) => eprintln!("Failed to connect to daemon: {e}"),
            }
        }
        Some(Commands::List) => {
            if let Some(run) = daemon::find_active_run_dir(&home) {
                for entry in std::fs::read_dir(&run)?.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".sock") && name != "api.sock" {
                        let agent = &name[..name.len() - 5];
                        println!("  {agent}");
                    }
                }
            } else {
                println!("No running daemon found.");
            }
        }
        Some(Commands::Status) => {
            match api::call(&home, &serde_json::json!({"method": "list"})) {
                Ok(resp) => {
                    if let Some(agents) = resp["result"]["agents"].as_array() {
                        if agents.is_empty() {
                            println!("No agents running.");
                        } else {
                            for agent in agents {
                                let name = agent["name"].as_str().unwrap_or("?");
                                let cmd = agent["command"].as_str().unwrap_or("?");
                                let state = agent["agent_state"].as_str().unwrap_or("?");
                                let health = agent["health_state"].as_str().unwrap_or("?");
                                println!("  {name}: state={state} health={health} cmd={cmd}");
                            }
                        }
                    }
                }
                Err(e) => eprintln!("Failed to connect to daemon: {e}"),
            }
        }
        Some(Commands::Kill { name }) => {
            match api::call(
                &home,
                &serde_json::json!({"method": "kill", "params": {"name": name}}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!("Killed {name}");
                }
                Ok(resp) => {
                    eprintln!(
                        "Kill failed: {}",
                        resp["error"].as_str().unwrap_or("unknown")
                    );
                }
                Err(e) => eprintln!("Failed to connect to daemon: {e}"),
            }
        }
        Some(Commands::Fleet { command }) => match command {
            FleetCommands::Start { config } => {
                let fleet_path = config
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join("fleet.yaml"));
                cli::start_with_fleet(&home, &fleet_path)?;
            }
            FleetCommands::Stop => {
                match api::call(&home, &serde_json::json!({"method": "list"})) {
                    Ok(resp) => {
                        if let Some(agents) = resp["result"]["agents"].as_array() {
                            for agent in agents {
                                let name = agent["name"].as_str().unwrap_or("");
                                let _ = api::call(
                                    &home,
                                    &serde_json::json!({"method": "kill", "params": {"name": name}}),
                                );
                                println!("  Stopped {name}");
                            }
                            println!("Fleet stopped ({} agents)", agents.len());
                        }
                    }
                    Err(e) => eprintln!("Failed to connect to daemon: {e}"),
                }
            }
        },
        Some(Commands::Mcp) => {
            let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_else(|_| {
                eprintln!(
                    "Error: AGEND_INSTANCE_NAME not set. MCP server must run inside a session."
                );
                std::process::exit(1);
            });
            let sock = daemon::agent_socket_path(&home, &instance_name);
            mcp::run(&sock)?;
        }
        Some(Commands::Capture { backend, seconds }) => {
            let b: backend::Backend = serde_json::from_str(&format!("\"{backend}\""))
                .unwrap_or_else(|_| {
                    eprintln!("Unknown backend: {backend}");
                    std::process::exit(1);
                });
            if !b.is_installed() {
                eprintln!("{} ({}) not found in PATH", backend, b.preset().command);
                std::process::exit(1);
            }
            cli::capture_backend(&b, seconds)?;
        }
        Some(Commands::Test { suite }) => {
            cli::run_tests(&suite, &home)?;
        }
        Some(Commands::Verify { json, backend }) => {
            verify::run(&home, json, backend.as_deref())?;
        }
        Some(Commands::Doctor) => {
            cli::run_doctor(&home)?;
        }
    }

    Ok(())
}
