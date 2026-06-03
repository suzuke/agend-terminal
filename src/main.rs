// #1630: `#[macro_use]` must precede every module that calls `persist_or_log!`,
// so it leads the module list. Defines the macro for the whole bin crate.
#[macro_use]
mod macros;

mod admin;
mod agent;
mod agent_ops;
mod agy_workspace;
mod api;
mod app;
mod auth_cookie;
mod backend;
mod backend_harness;
mod backend_profile;
mod behavioral;
mod binding;
mod bootstrap;
mod branch_sweep;
mod bridge_client;
mod bugreport;
mod capture;
mod channel;
mod claim_verifier;
mod cli;
mod config_integrity;
mod connect;
mod daemon;
mod daemon_config;
mod decisions;
mod deployments;
mod dispatch_tracking;
mod display_time;
mod error;
mod event_log;
mod fleet;
mod framing;
mod git_helpers;
mod github_token;
mod health;
mod identity;
mod inbox;
mod instance_monitor;
mod instructions;
mod integrity_core;
mod ipc;
mod keybinds;
mod layout;
mod logging;
mod mcp;
mod mcp_config;
mod mouse_forward;
mod notification_queue;
pub mod operator_mode;
mod paths;
mod process;
mod protocol;
mod quickstart;
mod render;
mod reply_ledger;
mod runtime;
pub mod runtime_config;
mod schedules;
mod scm;
mod screenshot;
mod service;
mod skills;
mod snapshot;
mod state;
mod status_summary;
mod store;
mod sync;
mod sync_audit;
mod task_events;
mod tasks;
mod teams;
mod thread_census;
mod token_cost;
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

/// Orchestrate AI coding agents
#[derive(Parser)]
#[command(
    name = "agend-terminal",
    version,
    about = "Orchestrate AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon
    Start {
        /// Run the daemon in the foreground (blocks the calling shell, stdio
        /// stays attached to the terminal).
        ///
        /// `start` defaults to detached service mode. Pass `--foreground` to
        /// keep the legacy blocking behaviour — useful for debugging the
        /// daemon's own logs, or running under a process supervisor
        /// (systemd / launchd / Task Scheduler) that owns the lifecycle.
        #[arg(long)]
        foreground: bool,
        /// Path to fleet.yaml (default: $AGEND_HOME/fleet.yaml).
        #[arg(long)]
        fleet: Option<String>,
        /// Start with explicit agent specs (`name:command` pairs) instead of
        /// fleet.yaml. Skips fleet loading; the daemon spawns exactly the
        /// listed agents. Subsumes the former `daemon` subcommand. Implies
        /// `--foreground` (no fleet.yaml path → can't register with the
        /// supervisor in detached mode).
        ///
        /// Example: `start --agents dev:claude reviewer:claude shell:/bin/bash`
        #[arg(long, num_args = 1.., value_name = "NAME:CMD")]
        agents: Vec<String>,
    },
    /// Attach to an agent's terminal
    Attach {
        /// Agent name
        #[arg(default_value = "shell")]
        name: String,
    },
    /// Send input to an agent
    Inject {
        /// Agent name
        name: String,
        /// Text to inject
        text: Vec<String>,
    },
    /// List running agents
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
        /// #938: emit the pre-#938 JSON shape (`result.agents` top-level
        /// passthrough) instead of the new `{"mode", "agents"}` envelope.
        /// One-release-cycle deprecation window for operator JSON parsers
        /// that hard-code the old shape. Removed after operator migration
        /// completes. Has no effect without `--json`.
        #[arg(long)]
        legacy_json: bool,
    },
    /// Connect an external agent to the daemon
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
    /// Launch the TUI app
    App {
        /// Path to fleet.yaml (default: $AGEND_HOME/fleet.yaml)
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop the daemon
    Stop,
    /// Kill an agent
    Kill {
        /// Agent name
        name: String,
    },
    /// #1339: Set the operator availability mode (operator-only authority control).
    Mode {
        /// active | away | sleep
        mode: String,
        /// Delegate instance that may proxy in-scope ops in sleep mode
        #[arg(long)]
        delegate: Option<String>,
        /// Comma-separated operation/tool names the delegate may proxy in sleep
        #[arg(long)]
        scope: Option<String>,
    },
    /// Admin utilities
    Admin {
        #[command(subcommand)]
        command: AdminCommands,
    },
    /// Capture agent output
    Capture {
        #[command(subcommand)]
        action: CaptureAction,
    },
    /// Run end-to-end verification
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
    /// Install or manage the OS service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Run health checks
    Doctor {
        #[command(subcommand)]
        action: Option<DoctorAction>,
    },
    /// Manage shared agent skills
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Launch the system tray app
    #[cfg(feature = "tray")]
    Tray,
    /// Interactive first-time setup
    Quickstart,
    /// Generate a bug report
    Bugreport,
    /// Generate shell completions
    Completions {
        /// Shell type
        shell: clap_complete::Shell,
    },
    /// Verify push claims against the actual diff
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
    /// #927: kill long-running zombie daemons holding `<home>/run/<pid>/`.
    ///
    /// Lists daemon processes whose `<run_dir>/.daemon` mtime is older
    /// than `--age` (default `14d`). Prompts `[y/N]` before sending
    /// signals unless `--yes` is supplied for scripted scenarios.
    ///
    /// Termination semantics (platform asymmetry):
    ///   - **Unix**: SIGTERM → 5s grace → SIGKILL on timeout. Grace
    ///     allows the daemon's own SHUTDOWN_GRACE=2s agent teardown +
    ///     3s buffer for cleanup hooks and log-worker flush.
    ///   - **Windows**: TerminateProcess single-stage (no SIGTERM
    ///     equivalent in the Win32 API surface this CLI uses today).
    ///     Future improvement: CTRL_BREAK_EVENT path for two-stage
    ///     parity with Unix.
    ///
    /// Exit codes:
    ///   - 0: all candidates reaped (or no candidates found).
    ///   - non-zero: at least one candidate refused to die after the
    ///     full grace window. Operator must investigate (kernel-stuck
    ///     process / uninterruptible sleep / kernel module hold).
    CleanupZombies {
        /// Age threshold (e.g. `14d`, `3h`, `30m`). Daemons whose
        /// `.daemon` file mtime is older than this are candidates.
        #[arg(long, default_value = "14d")]
        age: String,
        /// Skip the interactive `[y/N]` prompt. Logs a
        /// "non-interactive destructive mode" line for the audit trail.
        #[arg(long)]
        yes: bool,
    },
}

/// Issue #704: passive capture subcommands.
#[derive(Subcommand)]
enum CaptureAction {
    /// Capture backend output for debugging (existing behaviour)
    Backend {
        /// Backend name (claude, kiro-cli, codex, opencode, gemini)
        #[arg(long)]
        backend: String,
        /// Capture duration in seconds
        #[arg(long, default_value = "15")]
        seconds: u64,
    },
    /// Promote a passive capture to tests/fixtures/state-replay/<scenario>.raw
    /// AND append a v2 MANIFEST.yaml entry. Per #704 sub-task 1 Phase 1a.
    Promote {
        /// Path to a .cap file (produced by AGEND_CAPTURE_FIXTURES=1)
        capture_path: String,
        /// Scenario name (becomes the file stem in tests/fixtures/state-replay/)
        scenario_name: String,
        /// Required: F9 measurement classification per
        /// docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.6. Valid values:
        /// `productive_marker_fire`, `productive_silence`,
        /// `silent_stuck`, `hung`, `real_capture`.
        #[arg(long)]
        scenario_kind: String,
        /// Optional: expected hung classification (e.g. `hung`,
        /// `not_hung`). Used by --auto-replay for cross-validation.
        #[arg(long)]
        expected_hung: Option<String>,
        /// Optional: one-line scenario description for the MANIFEST
        /// `scenario` field. Defaults to a placeholder operator can
        /// edit post-promote.
        #[arg(long)]
        scenario_description: Option<String>,
        /// Optional: cross-check that the scenario_kind's implied
        /// hung-class matches the operator-supplied --expected-hung.
        /// WARN-only (promote not reverted).
        #[arg(long, default_value = "false")]
        auto_replay: bool,
    },
}

/// Subcommands for `agend-terminal skills`.
#[derive(Subcommand)]
enum SkillsAction {
    /// Add a skill from a local directory or git URL. The skill name
    /// is derived from the source basename. Idempotent — re-adding
    /// overwrites the existing copy.
    Add {
        /// Local path or git URL (https://, git@, ssh://, *.git).
        source: String,
    },
    /// Remove a skill from the unified source. Idempotent — calling
    /// on a missing skill is a no-op.
    Remove {
        /// Skill name (directory name under `<home>/skills/`).
        name: String,
    },
    /// List installed skills with source/version metadata from
    /// `<home>/skills-lock.json`.
    List,
    /// Re-fetch a skill from its lock-recorded source (or all skills
    /// if no name is given). Path sources re-copy from disk; git
    /// sources re-clone.
    Update {
        /// Skill name. Omit to update every skill in the lock.
        name: Option<String>,
    },
    /// Install all unified-source skills into the given working
    /// directory's backend skill paths (.claude/skills/, .codex/skills/,
    /// .gemini/skills/, .opencode/skills/, .kiro/skills/). Symlinks
    /// on Unix; copies with `.agend-skills-managed` marker on Windows.
    /// Pre-existing non-managed directories are preserved (operator
    /// hand-crafted skills are never clobbered).
    Install {
        /// Agent working directory. Skills will be installed at the
        /// 5 backend-conventional paths under this directory.
        working_dir: String,
    },
}

/// Subcommands for `agend-terminal doctor`. Default (no subcommand)
/// runs the existing fleet-wide health check; `topics` adds telegram
/// topic state diagnostic + optional cleanup.
#[derive(Subcommand)]
enum DoctorAction {
    /// Diagnose telegram topic state (live / orphan).
    /// Pair with `--cleanup` to act on orphan entries (chat-mutating
    /// operations gated by bot's `can_manage_topics` permission).
    Topics {
        /// Act on orphan entries (registry update + chat-side delete).
        /// Requires the bot to have `can_manage_topics` permission for chat-mutating
        /// operations on `orphan` entries; without permission, those skip with warn.
        #[arg(long)]
        cleanup: bool,
        /// Output format: `human` (default, multi-line table) or `json` (structured
        /// for piping into other tools).
        #[arg(long, default_value = "human")]
        format: String,
        /// Skip interactive confirmation prompt for `--cleanup`.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Register the daemon with the OS service manager so it auto-
    /// starts at user login and restarts on crash. User-level on all
    /// supported platforms (no admin / root required). Idempotent:
    /// re-running regenerates the template + re-registers.
    Install,
    /// Remove the OS-level service registration. Idempotent on missing
    /// install (returns success with `was_installed: false`).
    Uninstall,
    /// Query the OS service manager for current state. Reports
    /// running / stopped / not_installed.
    Status,
}

fn main() -> anyhow::Result<()> {
    load_dotenv();

    let cli = Cli::parse();

    // App mode redirects tracing to a log file (stderr is owned by ratatui).
    // The daemon child path (`start --foreground`, including the spawn_detached
    // child after it re-execs with --foreground) routes through #914's
    // rolling-file appender so daemon.log no longer grows unbounded. All
    // other commands keep stderr (single-shot CLI invocations, no growth
    // concern).
    let is_app = matches!(cli.command, Some(Commands::App { .. }));
    let is_daemon_child = matches!(
        cli.command,
        Some(Commands::Start {
            foreground: true,
            ..
        })
    );
    let home = home_dir();
    std::fs::create_dir_all(&home)?;

    // `_log_guard` must outlive any tracing call below. Drop = flush + close
    // the rolling appender's worker thread, so we deliberately bind it in
    // `main`'s scope rather than returning it from `setup_rolling_tracing`'s
    // call site. CLI mode produces `None`; the binding is still required so
    // the daemon-path `Some(guard)` lives until process exit.
    //
    // #927 PR-A: was `setup_daemon_tracing`; parametrized so the app path
    // can share the same rolling-appender + panic-hook machinery. App's
    // guard is owned by `app::run` itself (see `src/app/mod.rs`).
    let _log_guard = if is_app {
        None
    } else if is_daemon_child {
        Some(crate::logging::setup_rolling_tracing(
            &home,
            "daemon",
            "agend_terminal=info",
            crate::logging::MigrationPolicy::Migrate,
        )?)
    } else {
        crate::logging::setup_cli_tracing();
        None
    };

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
            foreground,
            fleet,
            agents,
        }) => {
            // Sprint 57 Wave 3 PR-2 (#548 Q1): default-flip to detached
            // service mode. `--foreground` is the new opt-out flag.
            // `--agents` always implies foreground (no fleet.yaml path
            // to register with the supervisor in detached mode), so the
            // detach branch only fires when a fleet path is in play.
            let force_foreground = foreground || !agents.is_empty();

            // Wave 1 CLI consolidation: `--agents` subsumes the former
            // `daemon` subcommand. When provided, skip fleet loading and
            // spawn the listed agents directly. Mutually exclusive with
            // a fleet.yaml — bail early if both supplied to avoid
            // ambiguous start semantics.
            if !agents.is_empty() {
                if fleet.is_some() {
                    anyhow::bail!("`--agents` and `--fleet` are mutually exclusive");
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
                // #1441: managed spawns fail-fast unless the instance is in
                // fleet.yaml (registry key == inbox identity, no random
                // fallback). The `--agents` path skips fleet loading, so
                // register each listed agent in fleet.yaml first — the same
                // non-destructive merge the MCP create path uses; `FleetConfig::
                // load` backfills the authoritative id, which the daemon spawn
                // loop then resolves and registers under.
                std::fs::create_dir_all(&home)?;
                for (name, ..) in &agents {
                    let entry = crate::fleet::InstanceYamlEntry::default();
                    if let Err(e) = crate::fleet::add_instance_to_yaml(&home, name, &entry) {
                        anyhow::bail!("failed to register '{name}' in fleet.yaml: {e}");
                    }
                }
                daemon::run(&home, agents)?;
                return Ok(());
            }

            let fleet_path = fleet
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::fleet::fleet_yaml_path(&home));
            if !force_foreground {
                // Sprint 57 Wave 3 PR-2 (#548 Q1) default branch: detach.
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
                // #1441: register the fallback shell agent in fleet.yaml so the
                // managed spawn resolves (registry key == inbox identity, no
                // random fallback). See the `--agents` branch for rationale.
                std::fs::create_dir_all(&home)?;
                if let Err(e) = crate::fleet::add_instance_to_yaml(
                    &home,
                    "shell",
                    &crate::fleet::InstanceYamlEntry::default(),
                ) {
                    anyhow::bail!("failed to register 'shell' in fleet.yaml: {e}");
                }
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
        Some(Commands::List {
            json,
            detailed,
            legacy_json,
        }) => {
            // Wave 1 CLI consolidation: `--detailed/-d` (or `--json`) shows
            // state/health/cmd via the daemon API. Plain `list` falls
            // back to scanning `*.port` files in the run dir — works
            // even when the daemon API is briefly unresponsive (the
            // historical reason `list` and `status` were two commands).
            //
            // #938: plain output now surfaces a fallback-mode hint to
            // stderr; JSON output gains a `mode` field. Operator JSON
            // parsers pinning the pre-#938 shape can opt into
            // `--legacy-json` for one release cycle.
            let want_detailed = detailed || json;
            if want_detailed {
                let api_resp = api::call(&home, &serde_json::json!({"method": api::method::LIST}));
                if json {
                    // #938: unified JSON output. Prefer the rich API
                    // response (state/health/backend fields) when daemon
                    // reachable; fall through to the fallback helper for
                    // offline / stuck-daemon coverage.
                    let (fallback_agents, mode) =
                        crate::runtime::list_agents_with_fallback_with_mode(&home);
                    let agents_value: serde_json::Value = match (
                        &api_resp,
                        matches!(mode, crate::runtime::AgentListMode::Live),
                    ) {
                        (Ok(resp), true) => resp["result"]["agents"].clone(),
                        _ => serde_json::Value::Array(
                            fallback_agents
                                .iter()
                                .map(|n| serde_json::json!({"name": n}))
                                .collect(),
                        ),
                    };
                    if legacy_json {
                        // Pre-#938 shape passthrough: print
                        // `{"agents": [...], ...}` exactly as the API
                        // returned. One-release-cycle deprecation window.
                        let payload = match &api_resp {
                            Ok(resp) => resp["result"].clone(),
                            Err(_) => serde_json::json!({"agents": agents_value}),
                        };
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&payload).unwrap_or_default()
                        );
                    } else {
                        let envelope = serde_json::json!({
                            "mode": mode.as_str(),
                            "agents": agents_value,
                        });
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&envelope).unwrap_or_default()
                        );
                    }
                } else {
                    // Detailed plain output: prefer rich API; fallback
                    // to flat names + mode hint if daemon offline.
                    match &api_resp {
                        Ok(resp) => {
                            if let Some(agents) = resp["result"]["agents"].as_array() {
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
                        Err(_) => {
                            let (agents, mode) =
                                crate::runtime::list_agents_with_fallback_with_mode(&home);
                            if agents.is_empty() {
                                daemon_not_running_hint();
                            } else {
                                for a in &agents {
                                    println!("  {a}");
                                }
                                if let Some(h) = mode.hint() {
                                    eprintln!("  {h}");
                                }
                            }
                        }
                    }
                }
            } else if daemon::find_active_run_dir(&home).is_some() {
                // #910 PR2 of 4 / #938 (C) bundle: daemon-registry truth
                // via runtime helper. The mode discriminator surfaces
                // a fallback hint to stderr so operator can tell live
                // from stale-port-glob without reading daemon.log.
                let (agents, mode) = crate::runtime::list_agents_with_fallback_with_mode(&home);
                for a in &agents {
                    println!("  {a}");
                }
                if let Some(h) = mode.hint() {
                    eprintln!("  {h}");
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
        Some(Commands::Mode {
            mode,
            delegate,
            scope,
        }) => {
            // #1339: operator-only mode control over the DIRECT `MODE` method —
            // the operator transport (the gate lets direct methods through;
            // agents can only send `mcp_tool`, which the gate blocks for set).
            let scope_arr: Vec<String> = scope
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let mut p = serde_json::json!({"mode": mode});
            if let Some(d) = &delegate {
                p["delegate"] = serde_json::json!(d);
            }
            if !scope_arr.is_empty() {
                p["scope"] = serde_json::json!(scope_arr);
            }
            match api::call(
                &home,
                &serde_json::json!({"method": api::method::MODE, "params": p}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    println!(
                        "Operator mode → {}{}",
                        resp["mode"].as_str().unwrap_or(&mode),
                        resp["delegate_to"]
                            .as_str()
                            .map(|d| format!(" (delegate: {d})"))
                            .unwrap_or_default()
                    );
                }
                Ok(resp) => eprintln!(
                    "mode failed: {}",
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
            AdminCommands::CleanupZombies { age, yes } => {
                use admin::cleanup_zombies::{
                    cleanup_zombie_daemon, find_zombie_candidates, log_zombie_state, parse_age,
                    KillOutcome,
                };
                use std::io::Write;
                let min_age = parse_age(&age).unwrap_or_else(|| {
                    eprintln!(
                        "agend-terminal: invalid --age {age:?} \
                         (expected suffix d/h/m/s, e.g. 14d); using default 14d"
                    );
                    std::time::Duration::from_secs(14 * 86400)
                });
                let now = std::time::SystemTime::now();
                let zombies = find_zombie_candidates(&home, min_age, now);
                if zombies.is_empty() {
                    println!(
                        "No zombie daemons older than {age} found in {}",
                        home.join("run").display()
                    );
                    return Ok(());
                }
                println!(
                    "Found {} zombie daemon process(es) older than {age}:",
                    zombies.len()
                );
                for z in &zombies {
                    let age_h = z.age.as_secs() / 3600;
                    let age_d = age_h / 24;
                    let age_str = if age_d > 0 {
                        format!("{age_d}d")
                    } else {
                        format!("{age_h}h")
                    };
                    println!(
                        "  PID {pid}  age {age}  run_dir {dir}",
                        pid = z.pid,
                        age = age_str,
                        dir = z.run_dir.display()
                    );
                }
                println!();
                let proceed = if yes {
                    tracing::info!(
                        "#927 cleanup-zombies: non-interactive destructive mode (--yes)"
                    );
                    println!("--yes supplied; proceeding without prompt.");
                    true
                } else {
                    print!("Send SIGTERM (then SIGKILL after 5s)? [y/N] ");
                    std::io::stdout().flush().ok();
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input).ok();
                    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                };
                if !proceed {
                    println!("Aborted. No signals sent.");
                    return Ok(());
                }
                let term_grace = std::time::Duration::from_secs(5);
                let kill_grace = std::time::Duration::from_secs(2);
                let mut refused = 0;
                for z in &zombies {
                    log_zombie_state(z.pid);
                    let outcome = cleanup_zombie_daemon(z.pid, term_grace, kill_grace);
                    match outcome {
                        KillOutcome::Graceful(d) => {
                            println!("  PID {} reaped gracefully ({}ms)", z.pid, d.as_millis());
                        }
                        KillOutcome::ForceKilled => {
                            println!("  PID {} required SIGKILL (SIGTERM ignored)", z.pid);
                        }
                        KillOutcome::AlreadyExited => {
                            println!("  PID {} already exited (race with sweep)", z.pid);
                        }
                        KillOutcome::WindowsTerminated => {
                            println!("  PID {} terminated (Windows TerminateProcess)", z.pid);
                        }
                        KillOutcome::RefusedToDie => {
                            eprintln!(
                                "  PID {} REFUSED TO DIE after SIGTERM+SIGKILL — investigate",
                                z.pid
                            );
                            refused += 1;
                        }
                    }
                }
                if refused > 0 {
                    eprintln!(
                        "\n{refused} zombie(s) refused to die; operator must investigate \
                         (kernel-stuck / uninterruptible sleep)."
                    );
                    std::process::exit(2);
                }
            }
        },
        Some(Commands::Capture { action }) => match action {
            CaptureAction::Backend { backend, seconds } => {
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
            CaptureAction::Promote {
                capture_path,
                scenario_name,
                scenario_kind,
                expected_hung,
                scenario_description,
                auto_replay,
            } => {
                let kind: capture::PromoteScenarioKind = scenario_kind.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "invalid --scenario-kind {scenario_kind:?}; \
                         valid values: productive_marker_fire, \
                         productive_silence, silent_stuck, hung, real_capture"
                    )
                })?;
                let opts = capture::PromoteOptions {
                    scenario_kind: kind,
                    expected_hung: expected_hung.as_deref(),
                    scenario_description: scenario_description.as_deref(),
                    auto_replay,
                };
                capture::promote_capture(
                    std::path::Path::new(&capture_path),
                    &scenario_name,
                    &opts,
                )?;
            }
        },
        Some(Commands::Verify {
            json,
            backend,
            quick,
        }) => verify::run(&home, json, backend.as_deref(), quick)?,
        Some(Commands::Service { action }) => match action {
            ServiceAction::Install => match service::install(&home) {
                Ok(path) => {
                    println!("service installed: {}", path.display());
                }
                Err(e) => {
                    eprintln!("agend-terminal service install: {e}");
                    std::process::exit(1);
                }
            },
            ServiceAction::Uninstall => match service::uninstall(&home) {
                Ok(outcome) => {
                    if outcome.was_installed {
                        println!("service uninstalled");
                    } else {
                        println!("service was not installed (idempotent no-op)");
                    }
                }
                Err(e) => {
                    eprintln!("agend-terminal service uninstall: {e}");
                    std::process::exit(1);
                }
            },
            ServiceAction::Status => match service::status(&home) {
                Ok(state) => {
                    println!("service status: {}", state.as_str());
                    if matches!(state, service::ServiceState::NotInstalled) {
                        std::process::exit(2);
                    }
                }
                Err(e) => {
                    eprintln!("agend-terminal service status: {e}");
                    std::process::exit(1);
                }
            },
        },
        Some(Commands::Doctor { action: None }) => cli::run_doctor(&home)?,
        Some(Commands::Doctor {
            action:
                Some(DoctorAction::Topics {
                    cleanup,
                    format,
                    yes,
                }),
        }) => {
            cli::run_doctor_topics(
                &home,
                cli::DoctorTopicsOptions {
                    cleanup,
                    format: &format,
                    yes,
                },
            )?;
        }
        Some(Commands::Skills { action }) => match action {
            SkillsAction::Add { source } => cli::run_skills_add(&home, &source)?,
            SkillsAction::Remove { name } => cli::run_skills_remove(&home, &name)?,
            SkillsAction::List => cli::run_skills_list(&home)?,
            SkillsAction::Update { name } => cli::run_skills_update(&home, name.as_deref())?,
            SkillsAction::Install { working_dir } => cli::run_skills_install(&home, &working_dir)?,
        },
        #[cfg(feature = "tray")]
        Some(Commands::Tray) => tray::run(&home)?,
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
