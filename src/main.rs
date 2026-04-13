mod agent;
mod agent_cli;
mod api;
mod backend;
mod bugreport;
mod channel;
mod cli;
mod daemon;
mod decisions;
mod deployments;
mod error;
mod event_log;
mod fleet;
mod framing;
mod health;
mod inbox;
mod instructions;
mod ops;
mod quickstart;
mod schedules;
mod snapshot;
mod state;
mod store;
mod tasks;
mod teams;
mod telegram;
mod tui;
mod verify;
mod vterm;
mod worktree;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AGEND_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let base = PathBuf::from(base);
    // Prefer ~/.agend, fallback to ~/.agend-terminal for backwards compat
    let new_path = base.join(".agend");
    let legacy_path = base.join(".agend-terminal");
    if new_path.exists() || !legacy_path.exists() {
        new_path
    } else {
        legacy_path
    }
}

/// Load .env file from AGEND_HOME.
///
/// Supports: `KEY=value`, `export KEY=value`, single/double quoted values.
/// Quoted values preserve `#` inside; unquoted values strip inline comments.
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
        if let Some((key, rest)) = line.split_once('=') {
            let key = key.trim();
            let rest = rest.trim();
            let value = if rest.starts_with('"') {
                // Double-quoted: find matching close quote (handles escaped \")
                parse_double_quoted(rest)
            } else if let Some(inner) = rest.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
                // Single-quoted: literal content, no escapes
                inner.to_string()
            } else {
                // Unquoted: strip inline comment
                rest.split(" #").next().unwrap_or(rest).trim().to_string()
            };
            if !key.is_empty() {
                std::env::set_var(key, &value);
            }
        }
    }
}

/// Parse a double-quoted value, handling escaped quotes (e.g. `"hello \"world\""`)
pub(crate) fn parse_double_quoted(s: &str) -> String {
    let inner = &s[1..]; // skip opening "
    let mut result = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    match next {
                        '"' | '\\' => result.push(next),
                        'n' => result.push('\n'),
                        't' => result.push('\t'),
                        _ => {
                            result.push('\\');
                            result.push(next);
                        }
                    }
                }
            }
            '"' => break, // closing quote
            _ => result.push(c),
        }
    }
    result
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
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show detailed agent status (state, health)
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
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
    /// Agent CLI — commands for agent-to-agent coordination (JSON output)
    #[command(alias = "a")]
    Agent {
        #[command(subcommand)]
        command: agent_cli::AgentCommand,
    },
    /// Capture backend output for debugging
    Capture {
        /// Backend name (claude, kiro-cli, codex, opencode, gemini)
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
    /// Interactive demo — experience multi-agent orchestration in 30 seconds
    Demo,
    /// Interactive setup — detect backends, configure Telegram, generate fleet.yaml
    Quickstart,
    /// Generate bug report with diagnostics, logs, and config
    Bugreport,
    /// Generate shell completions (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Shell type
        shell: clap_complete::Shell,
    },
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGEND_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agend_terminal=info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

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
            let agents: Vec<_> = if agents.is_empty() {
                vec![(
                    "shell".into(),
                    "/bin/bash".into(),
                    Vec::new(),
                    None,
                    None,
                    "\r".into(),
                )]
            } else {
                agents
                    .iter()
                    .map(|a| {
                        let (name, cmd) = a
                            .split_once(':')
                            .map(|(n, c)| (n.to_string(), c.to_string()))
                            .unwrap_or_else(|| (a.to_string(), a.to_string()));
                        let (preset_args, submit_key) = backend::Backend::from_command(&cmd)
                            .map(|b| {
                                let p = b.preset();
                                let mut a: Vec<String> =
                                    p.args.iter().map(|s| s.to_string()).collect();
                                a.extend(p.resume_mode.args_for(&home, &name));
                                (a, p.submit_key.to_string())
                            })
                            .unwrap_or_else(|| (Vec::new(), "\r".to_string()));
                        (name, cmd, preset_args, None, None, submit_key)
                    })
                    .collect()
            };
            daemon::run(&home, agents)?;
        }
        Some(Commands::Attach { name }) => {
            let sock = daemon::agent_socket_path(&home, &name);
            if let Err(e) = tui::attach(&sock) {
                let err = format!("{e}");
                if err.contains("No such file") || err.contains("Connection refused") {
                    if daemon::find_active_run_dir(&home).is_none() {
                        daemon_not_running_hint();
                    } else {
                        eprintln!("Agent '{name}' not found.");
                        list_running_agents(&home);
                    }
                } else {
                    return Err(e);
                }
            }
        }
        Some(Commands::Inject { name, text }) => {
            let text = text.join(" ");
            if text.is_empty() {
                anyhow::bail!("No text provided");
            }
            match api::call(
                &home,
                &serde_json::json!({"method": "inject", "params": {"name": name, "data": text}}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => println!("Injected: {text}"),
                Ok(resp) => eprintln!(
                    "Inject failed: {}",
                    resp["error"].as_str().unwrap_or("unknown")
                ),
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::Stop) => {
            match api::call(&home, &serde_json::json!({"method": "shutdown"})) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!("Daemon shutdown initiated.")
                }
                Ok(_) => eprintln!("Shutdown request failed."),
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::List { json }) => {
            if let Some(run) = daemon::find_active_run_dir(&home) {
                let agents: Vec<String> = std::fs::read_dir(&run)?
                    .flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.ends_with(".sock").then(|| n[..n.len() - 5].to_string())
                    })
                    .filter(|n| n != "api")
                    .collect();
                if json {
                    println!("{}", serde_json::json!(agents));
                } else {
                    for a in &agents {
                        println!("  {a}");
                    }
                }
            } else if json {
                println!("[]");
            } else {
                println!("No running daemon found.");
            }
        }
        Some(Commands::Status { json }) => {
            match api::call(&home, &serde_json::json!({"method": "list"})) {
                Ok(resp) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&resp["result"]).unwrap_or_default()
                        );
                    } else if let Some(agents) = resp["result"]["agents"].as_array() {
                        if agents.is_empty() {
                            println!("No agents running.");
                        } else {
                            for a in agents {
                                println!(
                                    "  {}: state={} health={} cmd={}",
                                    a["name"].as_str().unwrap_or("?"),
                                    a["agent_state"].as_str().unwrap_or("?"),
                                    a["health_state"].as_str().unwrap_or("?"),
                                    a["command"].as_str().unwrap_or("?")
                                );
                            }
                        }
                    }
                }
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::Kill { name }) => {
            match api::call(
                &home,
                &serde_json::json!({"method": "kill", "params": {"name": name}}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => println!("Killed {name}"),
                Ok(resp) => eprintln!(
                    "Kill failed: {}",
                    resp["error"].as_str().unwrap_or("unknown")
                ),
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::Fleet { command }) => match command {
            FleetCommands::Start { config } => {
                let fleet_path = config
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join("fleet.yaml"));
                cli::start_with_fleet(&home, &fleet_path)?;
            }
            FleetCommands::Stop => match api::call(&home, &serde_json::json!({"method": "list"})) {
                Ok(resp) => {
                    if let Some(agents) = resp["result"]["agents"].as_array() {
                        for a in agents {
                            let name = a["name"].as_str().unwrap_or("");
                            let _ = api::call(
                                &home,
                                &serde_json::json!({"method": "kill", "params": {"name": name}}),
                            );
                            println!("  Stopped {name}");
                        }
                        println!("Fleet stopped ({} agents)", agents.len());
                    }
                }
                Err(_) => daemon_not_running_hint(),
            },
        },
        Some(Commands::Agent { command }) => {
            agent_cli::run(&home, command);
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
        Some(Commands::Test { suite }) => cli::run_tests(&suite, &home)?,
        Some(Commands::Verify { json, backend }) => verify::run(&home, json, backend.as_deref())?,
        Some(Commands::Doctor) => cli::run_doctor(&home)?,
        Some(Commands::Demo) => cli::run_demo()?,
        Some(Commands::Quickstart) => quickstart::run(&home)?,
        Some(Commands::Bugreport) => bugreport::run(&home)?,
        Some(Commands::Completions { shell }) => {
            use clap::CommandFactory;
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "agend-terminal",
                &mut std::io::stdout(),
            );
        }
    }

    Ok(())
}

fn daemon_not_running_hint() {
    eprintln!("Daemon is not running.");
    eprintln!("  Start it with:  agend-terminal start");
    eprintln!("  Or first setup: agend-terminal quickstart");
}

fn list_running_agents(home: &std::path::Path) {
    if let Ok(resp) = api::call(home, &serde_json::json!({"method": "list"})) {
        if let Some(agents) = resp["result"]["agents"].as_array() {
            if !agents.is_empty() {
                eprintln!("Running agents:");
                for a in agents {
                    eprintln!("  - {}", a["name"].as_str().unwrap_or("?"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_default() {
        let home = home_dir();
        let s = home.display().to_string();
        assert!(
            s.contains(".agend") || s.contains("agend"),
            "home_dir should contain 'agend': {s}"
        );
    }

    #[test]
    fn parse_double_quoted_simple() {
        assert_eq!(parse_double_quoted(r#""hello""#), "hello");
    }

    #[test]
    fn parse_double_quoted_escaped_quote() {
        assert_eq!(
            parse_double_quoted(r#""hello \"world\"""#),
            r#"hello "world""#
        );
    }

    #[test]
    fn parse_double_quoted_escaped_backslash() {
        assert_eq!(
            parse_double_quoted(r#""path\\to\\file""#),
            r#"path\to\file"#
        );
    }

    #[test]
    fn parse_double_quoted_newline_escape() {
        assert_eq!(parse_double_quoted(r#""line1\nline2""#), "line1\nline2");
    }

    #[test]
    fn parse_double_quoted_tab_escape() {
        assert_eq!(parse_double_quoted(r#""col1\tcol2""#), "col1\tcol2");
    }

    #[test]
    fn parse_double_quoted_unknown_escape() {
        // Unknown escape sequences preserved as-is
        assert_eq!(parse_double_quoted(r#""foo\xbar""#), r#"foo\xbar"#);
    }

    #[test]
    fn parse_double_quoted_hash_preserved() {
        assert_eq!(
            parse_double_quoted(r#""https://example.com#fragment""#),
            "https://example.com#fragment"
        );
    }

    #[test]
    fn parse_double_quoted_empty() {
        assert_eq!(parse_double_quoted(r#""""#), "");
    }
}
