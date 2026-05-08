mod admin;
mod agent;
mod agent_ops;
mod api;
mod app;
mod auth_cookie;
mod backend;
mod backend_harness;
mod behavioral;
mod binding;
mod bootstrap;
mod bridge_client;
mod bugreport;
mod channel;
mod claim_verifier;
mod cli;
mod connect;
mod daemon;
mod daemon_config;
mod decisions;
mod deployments;
mod dispatch_tracking;
mod error;
mod event_log;
mod fleet;
mod framing;
mod github_token;
mod health;
#[allow(dead_code)]
mod hotspot;
mod identity;
mod inbox;
mod instance_monitor;
mod instructions;
mod ipc;
mod keybinds;
mod layout;
mod mcp;
mod mcp_config;
mod notification_queue;
mod process;
mod protocol;
mod quickstart;
mod render;
mod schedules;
mod snapshot;
mod state;
mod status_summary;
mod store;
mod sync;
#[allow(dead_code)]
mod sync_audit;
mod task_events;
mod tasks;
mod teams;
mod thread_census;
#[cfg(feature = "tray")]
mod tray;
mod tui;
pub mod types;
mod verify;
mod vterm;
mod worktree;
mod worktree_cleanup;
#[allow(dead_code)]
mod worktree_pool;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Cross-platform user home directory with temp_dir fallback.
pub fn user_home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// Default shell command used whenever no explicit agent command is given.
/// Both `/bin/bash` (Unix) and `cmd.exe` (Windows) sit in the default PATH.
pub fn default_shell() -> &'static str {
    #[cfg(windows)]
    {
        "cmd.exe"
    }
    #[cfg(not(windows))]
    {
        "/bin/bash"
    }
}

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AGEND_HOME") {
        return PathBuf::from(home);
    }
    let base = user_home_dir();
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
    /// Start daemon with fleet.yaml or explicit `--agents`
    Start {
        /// Run the daemon in the background (detaches from this shell,
        /// stdio → $AGEND_HOME/daemon.log). Parent process exits immediately
        /// once the daemon has published its run dir.
        #[arg(long)]
        detached: bool,
        /// Path to fleet.yaml (default: $AGEND_HOME/fleet.yaml).
        #[arg(long)]
        fleet: Option<String>,
        /// Start with explicit agent specs (`name:command` pairs) instead of
        /// fleet.yaml. Skips fleet loading; the daemon spawns exactly the
        /// listed agents. Subsumes the former `daemon` subcommand.
        ///
        /// Example: `start --agents dev:claude reviewer:claude shell:/bin/bash`
        #[arg(long, num_args = 1.., value_name = "NAME:CMD")]
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
    /// List running agents (use `--detailed` for state/health/cmd, `--json` for JSON)
    #[command(aliases = ["ls", "status"])]
    List {
        /// Output as JSON. Forces detailed output (full agent record from API).
        #[arg(long)]
        json: bool,
        /// Show state / health / backend command for each agent. Without this
        /// flag, `list` reads run-dir port files directly and prints names
        /// only — useful when the daemon API is briefly unresponsive.
        /// `--json` implies `--detailed`. Subsumes the former `status` command
        /// (kept as an alias for backward compatibility).
        #[arg(long, short = 'd')]
        detailed: bool,
    },
    /// Connect a local agent to the running daemon
    Connect {
        /// Agent name (unique identifier)
        name: String,
        /// Backend command (claude, kiro-cli, codex, opencode, gemini)
        #[arg(long)]
        backend: String,
        /// Working directory (default: current dir)
        #[arg(long)]
        working_dir: Option<String>,
        /// Extra args passed to backend
        #[arg(last = true)]
        extra_args: Vec<String>,
    },
    /// Launch terminal app — multi-tab/pane TUI with agent management
    App {
        /// Path to fleet.yaml (default: $AGEND_HOME/fleet.yaml)
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop the daemon
    Stop,
    /// Kill a specific agent
    Kill {
        /// Agent name
        name: String,
    },
    /// Admin maintenance utilities
    Admin {
        #[command(subcommand)]
        command: AdminCommands,
    },
    /// Start MCP stdio server (auto-launched by AI backends; not for direct human use)
    Mcp,
    /// Capture backend output for debugging
    Capture {
        /// Backend name (claude, kiro-cli, codex, opencode, gemini)
        #[arg(long)]
        backend: String,
        /// Capture duration in seconds
        #[arg(long, default_value = "15")]
        seconds: u64,
    },
    /// E2E verification (use `--quick` for the lightweight subset)
    Verify {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by backend
        #[arg(long)]
        backend: Option<String>,
        /// Skip per-backend tests + daemon-spawning tests. Runs only the
        /// 4 in-process probes (attach, inbox, mcp framing, api). Subsumes
        /// the former `test` subcommand. Completes in <30s.
        #[arg(long)]
        quick: bool,
    },
    /// Health check
    Doctor,
    /// Menu-bar / system-tray resident app (requires `--features tray`).
    #[cfg(feature = "tray")]
    Tray,
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
    /// Verify push claims against actual diff (push-time semantic gate)
    VerifyPush {
        /// Base commit (e.g. origin/main)
        #[arg(long)]
        base: String,
        /// Head commit (default: HEAD)
        #[arg(long, default_value = "HEAD")]
        head: String,
        /// Read claim text from stdin
        #[arg(long)]
        claim_from_stdin: bool,
        /// Claim text (alternative to stdin)
        #[arg(long)]
        claim: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AdminCommands {
    /// Delete local branches whose PRs have been merged (squash-merge safe).
    /// Default: --dry-run (preview only). Pass --yes to actually delete.
    CleanupBranches {
        /// Actually delete branches (default is dry-run preview).
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> anyhow::Result<()> {
    load_dotenv();

    let cli = Cli::parse();

    // App mode redirects tracing to a log file (stderr is owned by ratatui).
    // All other commands use stderr.
    let is_app = matches!(cli.command, Some(Commands::App { .. }));
    if !is_app {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_env("AGEND_LOG")
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agend_terminal=info")),
            )
            .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
            .with_writer(std::io::stderr)
            .with_target(false)
            .init();
    }
    let home = home_dir();
    std::fs::create_dir_all(&home)?;

    match cli.command {
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
        }
        Some(Commands::App { fleet }) => {
            app::run(fleet.as_deref())?;
        }
        Some(Commands::Start {
            detached,
            fleet,
            agents,
        }) => {
            // Wave 1 CLI consolidation: `--agents` subsumes the former
            // `daemon` subcommand. When provided, skip fleet loading and
            // spawn the listed agents directly. Mutually exclusive with
            // a fleet.yaml — bail early if both supplied to avoid
            // ambiguous start semantics.
            if !agents.is_empty() {
                if fleet.is_some() {
                    anyhow::bail!("`--agents` and `--fleet` are mutually exclusive");
                }
                if detached {
                    anyhow::bail!(
                        "`--detached` is not supported with `--agents` (no fleet.yaml \
                         path to register with the supervisor); foreground only"
                    );
                }
                let agents: Vec<_> = agents
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
                                a.extend(p.resume_mode.args_for());
                                (a, p.submit_key.to_string())
                            })
                            .unwrap_or_else(|| (Vec::new(), "\r".to_string()));
                        (name, cmd, preset_args, None, None, submit_key)
                    })
                    .collect();
                daemon::run(&home, agents)?;
                return Ok(());
            }

            let fleet_path = fleet
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("fleet.yaml"));
            if detached {
                // Spawn self as a background process and exit. Child inherits
                // a clean environment from the parent shell but runs in its
                // own process group so this shell's Ctrl+C doesn't reach it.
                let handle = bootstrap::daemon_spawn::spawn_detached(
                    &home,
                    fleet_path.exists().then_some(fleet_path.as_path()),
                )?;
                println!(
                    "daemon started: pid={} run_dir={} log={}",
                    handle.pid,
                    handle.run_dir.display(),
                    handle.log_path.display()
                );
            } else if fleet_path.exists() {
                cli::start_with_fleet(&home, &fleet_path)?;
            } else {
                daemon::run(
                    &home,
                    vec![(
                        "shell".to_string(),
                        default_shell().to_string(),
                        Vec::new(),
                        None,
                        None,
                        "\r".to_string(),
                    )],
                )?;
            }
        }
        Some(Commands::Connect {
            name,
            backend,
            working_dir,
            extra_args,
        }) => {
            connect::run(&home, &name, &backend, working_dir.as_deref(), &extra_args)?;
        }
        Some(Commands::Attach { name }) => {
            if let Err(e) = tui::attach(&home, &name) {
                let err = format!("{e:#}").to_ascii_lowercase();
                if err.contains("no active daemon") {
                    daemon_not_running_hint();
                } else if err.contains("port file missing")
                    || err.contains("refused")
                    || err.contains("connectionreset")
                    || err.contains("connection reset")
                {
                    // Distinguish not-found from not-attachable by checking registry.
                    let agent_exists =
                        api::call(&home, &serde_json::json!({"method": api::method::LIST}))
                            .ok()
                            .and_then(|r| r["result"]["agents"].as_array().cloned())
                            .is_some_and(|agents| {
                                agents.iter().any(|a| a["name"].as_str() == Some(&name))
                            });

                    if agent_exists {
                        eprintln!("Agent '{name}' exists but is not attachable.");
                        eprintln!("Possible reasons:");
                        eprintln!("  - Already attached (only one attach session per agent)");
                        eprintln!(
                            "  - tui_bridge port not listening (daemon partially restarted?)"
                        );
                        eprintln!("  - Stale port file");
                        eprintln!("Run 'agend-terminal list' to confirm agent state.");
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
                &serde_json::json!({"method": api::method::INJECT, "params": {"name": name, "data": text}}),
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
            match api::call(&home, &serde_json::json!({"method": api::method::SHUTDOWN})) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!("Daemon shutdown initiated.")
                }
                Ok(_) => eprintln!("Shutdown request failed."),
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::List { json, detailed }) => {
            // Wave 1 CLI consolidation: `--detailed/-d` (or `--json`) shows
            // state/health/cmd via the daemon API. Plain `list` falls
            // back to scanning `*.port` files in the run dir — works
            // even when the daemon API is briefly unresponsive (the
            // historical reason `list` and `status` were two commands).
            let want_detailed = detailed || json;
            if want_detailed {
                match api::call(&home, &serde_json::json!({"method": api::method::LIST})) {
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
                                        a["backend"].as_str().unwrap_or("?")
                                    );
                                }
                            }
                        }
                    }
                    Err(_) => daemon_not_running_hint(),
                }
            } else if let Some(run) = daemon::find_active_run_dir(&home) {
                let agents: Vec<String> = std::fs::read_dir(&run)?
                    .flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.ends_with(".port").then(|| n[..n.len() - 5].to_string())
                    })
                    .filter(|n| n != "api")
                    .collect();
                for a in &agents {
                    println!("  {a}");
                }
            } else {
                println!("No running daemon found.");
            }
        }
        Some(Commands::Kill { name }) => {
            match api::call(
                &home,
                &serde_json::json!({"method": api::method::KILL, "params": {"name": name}}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => println!("Killed {name}"),
                Ok(resp) => eprintln!(
                    "Kill failed: {}",
                    resp["error"].as_str().unwrap_or("unknown")
                ),
                Err(_) => daemon_not_running_hint(),
            }
        }
        Some(Commands::Admin { command }) => match command {
            AdminCommands::CleanupBranches { yes } => {
                let repo = std::env::current_dir()?;
                let checks = admin::analyze_branches(&repo);
                let to_delete = checks
                    .iter()
                    .filter(|c| matches!(c.action, admin::BranchAction::Delete { .. }))
                    .count();
                if to_delete == 0 {
                    println!("No branches eligible for cleanup.");
                } else {
                    let dry_run = !yes;
                    if dry_run {
                        println!("Dry-run mode (pass --yes to delete):\n");
                    }
                    let (deleted, skipped) = admin::execute_cleanup(&repo, &checks, dry_run);
                    println!(
                        "\nSummary: {} deleted, {} skipped",
                        if dry_run { 0 } else { deleted },
                        skipped
                    );
                }
            }
        },
        Some(Commands::Mcp) => {
            // Sprint 56 Track I-Phase2b (#531): `agend-terminal mcp` is
            // deprecated and will be removed in Sprint 57+. Phase 1 RCA
            // identified this command as the silent-drop class root for
            // Windows operators (proxy-fail + daemon-state-required-tool
            // gate at `mcp/mod.rs:374-388`). Phase 2a shipped the
            // canonical replacement (`agend-mcp-bridge`) in release
            // artifacts; Phase 2b emits a deprecation warning here so
            // operators with hand-edited mcp.json see one clear signal
            // before Sprint 57's hard removal. The body still runs to
            // preserve one-Sprint backwards compat — daemon's atomic
            // mcp.json upsert clobbers their hand-edits with the bridge
            // path on next start, so this code path becomes unreachable
            // in normal operation soon after the upgrade.
            eprintln!(
                "DEPRECATED: `agend-terminal mcp` is deprecated and will be removed in Sprint 57. \
                 Update mcp.json to invoke `agend-mcp-bridge` directly. The agend-terminal \
                 daemon's atomic mcp.json upsert rewrites hand-edited configs to use the bridge \
                 automatically on next start (Sprint 56 Track I-Phase2a)."
            );
            tracing::warn!(
                "DEPRECATED `agend-terminal mcp` invocation — Sprint 57 will remove. See #531."
            );
            let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
            if instance_name.is_empty() {
                tracing::warn!("AGEND_INSTANCE_NAME not set, running in standalone mode");
            }
            mcp::run()?;
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
        Some(Commands::Verify {
            json,
            backend,
            quick,
        }) => verify::run(&home, json, backend.as_deref(), quick)?,
        Some(Commands::Doctor) => cli::run_doctor(&home)?,
        #[cfg(feature = "tray")]
        Some(Commands::Tray) => tray::run(&home)?,
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
        Some(Commands::VerifyPush {
            base,
            head,
            claim_from_stdin,
            claim,
            json,
        }) => {
            let claim_text = if claim_from_stdin {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                buf
            } else if let Some(c) = claim {
                c
            } else {
                anyhow::bail!("provide --claim or --claim-from-stdin");
            };
            let repo_dir = std::env::current_dir()?;
            let claims = claim_verifier::parse_claims(&claim_text);
            let result = claim_verifier::verify(&repo_dir, &base, &head, &claims, None);
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                for r in &result.results {
                    let icon = if r.passed { "✓" } else { "✗" };
                    println!("{icon} {}: {}", r.claim, r.detail);
                }
                if !result.ok {
                    std::process::exit(1);
                }
            }
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
    if let Ok(resp) = api::call(home, &serde_json::json!({"method": api::method::LIST})) {
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
