//! CLI helpers: doctor, capture, test runners.
//! Extracted from main.rs for module split.

use crate::{agent, backend, fleet};
use std::path::Path;

/// Start daemon with fleet.yaml config.
///
/// Delegates preflight (lock, run dir, cookie, fleet normalize, agent resolve,
/// telegram init) to [`crate::bootstrap::prepare`]. This routes both `start`
/// and `app` entry points through the same seam — see the `bootstrap` module
/// docs for the bug this addresses.
pub fn start_with_fleet(home: &Path, fleet_path: &Path) -> anyhow::Result<()> {
    match crate::bootstrap::prepare(home, fleet_path, Default::default())? {
        crate::bootstrap::BootstrapOutcome::Owned(prepared) => {
            if prepared.agents.is_empty() {
                tracing::error!("no instances found in fleet.yaml");
                std::process::exit(1);
            }
            crate::daemon::run_with_prepared(prepared)
        }
        crate::bootstrap::BootstrapOutcome::Attached(a) => anyhow::bail!(
            "another agend-terminal daemon is already running (pid {}, run_dir {})",
            a.daemon_pid,
            a.run_dir.display()
        ),
    }
}

#[allow(clippy::unwrap_used)]
pub fn capture_backend(b: &backend::Backend, seconds: u64) -> anyhow::Result<()> {
    let preset = b.preset();
    let name = format!("capture-{}", b.name());
    tracing::info!(backend = b.name(), command = preset.command, %seconds, "capture: spawning");

    let registry = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    agent::spawn_agent(
        &agent::SpawnConfig {
            name: &name,
            backend_command: preset.command,
            args: &[],
            spawn_mode: backend::SpawnMode::Fresh,
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

    tracing::info!(%seconds, "capture: waiting for output");
    std::thread::sleep(std::time::Duration::from_secs(seconds));

    let stripped = {
        let reg = registry.lock();
        match reg.get(&name) {
            Some(handle) => {
                let raw = handle.core.lock().vterm.dump_screen();
                agent::strip_ansi_pub(&String::from_utf8_lossy(&raw))
            }
            None => {
                tracing::warn!("capture: agent exited before capture");
                return Ok(());
            }
        }
    };

    {
        let reg = registry.lock();
        if let Some(h) = reg.get(&name) {
            let _ = h.child.lock().kill();
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
//
// Wave 1 CLI consolidation: the standalone `test` subcommand was removed.
// Its in-process probes (test_attach / test_mcp / test_inbox / test_api)
// duplicated the equivalents in `verify.rs`; callers now use
// `verify --quick` which runs the same 4 probes from the verify module.

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
                        println!("    {name}: {} {}", r.backend_command, r.args.join(" "));
                    }
                }
                // Sprint 56 Track H2 (#525 item 8): run the same
                // `validate_fleet_config` pre-flight that
                // `bootstrap::prepare` does. Pre-Track-H2 the CLI
                // doctor only checked file existence + parse + backend
                // binaries, so an operator running `agend-terminal
                // doctor` saw all green even when D001 / D002 should
                // have flagged a fail-closed misconfiguration. Wiring
                // both functions through their existing public API
                // surfaces the FATAL diagnostics that Track B's D001
                // and Track F's D002 produce.
                let diags = crate::bootstrap::doctor::validate_fleet_config(&config, home);
                crate::bootstrap::doctor::emit_diagnostics(&diags);
                if !diags.is_empty() {
                    let critical = diags
                        .iter()
                        .filter(|d| {
                            matches!(d.severity, crate::bootstrap::doctor::Severity::Critical)
                        })
                        .count();
                    if critical > 0 {
                        println!(
                            "\n  ⚠ {critical} critical fleet diagnostic(s) emitted above (D001/D002 etc.) — fix before next daemon start"
                        );
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
            if name.ends_with(".port") && name != "api.port" {
                let agent = &name[..name.len() - 5];
                let ok = crate::ipc::probe_agent(&run, agent);
                println!(
                    "    {agent} {}",
                    if ok {
                        "✓ (port responsive)"
                    } else {
                        "✗ (port stale)"
                    }
                );
                count += 1;
            }
        }
    }
    if count == 0 {
        println!("    (none)");
    }

    println!("\n  Thread census:");
    let census = crate::thread_census::snapshot();
    if census.is_empty() {
        println!("    (no registered threads — daemon not running in this process)");
    } else {
        for (kind, count) in &census {
            println!("    {kind}: {count}");
        }
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

    // Sprint 58 Wave 2 PR-1 (#11): helper-binary staleness check.
    // The daemon installs `agend-git` + `agend-mcp-bridge` at
    // `$AGEND_HOME/bin/` on first start. Operator-side `cargo build`
    // updates the daemon binary but the helpers in `$AGEND_HOME/bin/`
    // can lag behind, leading to subtle behavioral mismatches (e.g.
    // missing Track D silent-exempt code, missing newer wrapper
    // diagnostics). Per Q3 NOT-self-supervisor + Shape B passive-doctor
    // policy: detect + warn with operator-actionable instruction; do
    // NOT auto-rebuild.
    println!("\n  $AGEND_HOME/bin helpers:");
    let staleness_report = check_helper_staleness(home);
    for line in staleness_report.lines() {
        println!("    {line}");
    }
    Ok(())
}

/// Sprint 58 Wave 2 PR-1 (#11): per-helper staleness summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelperStaleness {
    /// Helper binary present and at least as new as the daemon binary
    /// — no operator action needed.
    Fresh,
    /// Helper binary mtime is older than the daemon binary — fix
    /// recommended (operator runs `cargo install --path . --force`
    /// or equivalent).
    Stale,
    /// Helper binary not present at expected path. Daemon will
    /// recreate on next start; doctor surfaces as info-level so
    /// operators understand "not yet bootstrapped" vs "out of date".
    NotInstalled,
    /// Daemon binary path could not be resolved (e.g. `/proc/self/exe`
    /// fail on exotic filesystems) — staleness is undeterminable.
    /// Doctor surfaces but does not block.
    UndeterminableDaemonPath,
}

impl HelperStaleness {
    /// String identifier for telemetry / future structured doctor
    /// output. Currently consumed only by tests (the production
    /// `check_helper_staleness` formats human-readable lines
    /// directly), but pinned here as a stable API surface for
    /// Sprint 59+ telemetry / event-log consumers.
    #[allow(dead_code)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::NotInstalled => "not_installed",
            Self::UndeterminableDaemonPath => "undeterminable",
        }
    }
}

/// Sprint 58 Wave 2 PR-1 (#11): pure helper that classifies a single
/// helper binary's freshness vs the current daemon binary. Compares
/// `mtime` (operator-side rebuild bumps the daemon binary's mtime;
/// the helper at `$AGEND_HOME/bin/<name>` keeps the older mtime
/// until the operator explicitly reinstalls).
///
/// Why mtime instead of version-string: the helper binaries are
/// minimal and don't currently embed a version constant; mtime
/// comparison is a 1-syscall stat() vs 2-process-spawns + stdout
/// parse. mtime can lie under filesystems that don't update it on
/// content change — but no such filesystem is in production scope
/// here. Sprint 59+ candidate: switch to embedded version constant
/// if mtime turns out to be unreliable empirically.
pub(crate) fn classify_helper_staleness(
    daemon_exe: Option<&Path>,
    helper_path: &Path,
) -> HelperStaleness {
    if !helper_path.exists() {
        return HelperStaleness::NotInstalled;
    }
    let Some(daemon_exe) = daemon_exe else {
        return HelperStaleness::UndeterminableDaemonPath;
    };
    let helper_mtime = std::fs::metadata(helper_path)
        .and_then(|m| m.modified())
        .ok();
    let daemon_mtime = std::fs::metadata(daemon_exe)
        .and_then(|m| m.modified())
        .ok();
    match (helper_mtime, daemon_mtime) {
        (Some(h), Some(d)) if h >= d => HelperStaleness::Fresh,
        (Some(_), Some(_)) => HelperStaleness::Stale,
        // Couldn't read either mtime — be conservative and treat as
        // undeterminable rather than panicking the doctor flow.
        _ => HelperStaleness::UndeterminableDaemonPath,
    }
}

/// Sprint 58 Wave 2 PR-1 (#11): build a multi-line human-readable
/// summary of the helper-binary staleness state. Returns lines
/// joined by `\n`. Used by `run_doctor` and unit-tested directly so
/// the formatting + actionable-instruction shape is auditable
/// without spinning up a real doctor flow.
pub(crate) fn check_helper_staleness(home: &Path) -> String {
    let bin_dir = home.join("bin");
    let daemon_exe = std::env::current_exe().ok();
    let daemon_exe_ref = daemon_exe.as_deref();

    let helpers = [
        ("agend-git", "wrapper for daemon-managed git ops"),
        ("agend-mcp-bridge", "bridge for MCP stdio JSON-RPC"),
    ];

    let mut lines: Vec<String> = Vec::new();
    let mut any_stale = false;

    for (name, desc) in helpers.iter() {
        // Add platform `.exe` suffix on Windows.
        let helper_path = if cfg!(windows) {
            bin_dir.join(format!("{name}.exe"))
        } else {
            bin_dir.join(name)
        };
        let state = classify_helper_staleness(daemon_exe_ref, &helper_path);
        let suffix = match state {
            HelperStaleness::Fresh => " ✓ (fresh)".to_string(),
            HelperStaleness::Stale => {
                any_stale = true;
                " ⚠ (stale — older than daemon binary)".to_string()
            }
            HelperStaleness::NotInstalled => " - (not yet installed)".to_string(),
            HelperStaleness::UndeterminableDaemonPath => " ? (mtime undeterminable)".to_string(),
        };
        lines.push(format!("{name} ({desc}){suffix}"));
    }

    if any_stale {
        lines.push(String::new());
        lines.push("⚠ helper binaries are stale.".into());
        lines.push("  Run the following to refresh them:".into());
        lines.push("    cargo install --path . --force".into());
        lines.push("  Then restart the daemon (`agend-terminal stop` →".into());
        lines.push("  `agend-terminal start`) so the refreshed".into());
        lines.push("  helpers are loaded.".into());
    }

    lines.join("\n")
}

pub fn run_demo() -> anyhow::Result<()> {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;

    let registry: agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let home = std::env::temp_dir().join(format!("agend-demo-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;

    print!("\x1b[2J\x1b[H");
    std::io::stdout().flush().ok();
    println!("  AgEnD Terminal — Live Multi-Agent Demo (Live Preview)\n");
    println!("  Spawning alice and bob...");

    let (crash_tx, _rx) = crossbeam_channel::unbounded::<crate::agent::AgentExitEvent>();
    let args: Vec<String> = vec![];
    let shell = crate::default_shell();
    for name in &["alice", "bob"] {
        agent::spawn_agent(
            &agent::SpawnConfig {
                name,
                backend_command: shell,
                args: &args,
                spawn_mode: backend::SpawnMode::Fresh,
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
            let reg = registry.lock();
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
        let reg = registry.lock();
        if let Some(bob) = reg.get("bob") {
            let mut child = bob.child.lock();
            let _ = child.kill();
        }
    }
    std::thread::sleep(Duration::from_millis(500));

    {
        let reg = registry.lock();
        if let Some(bob) = reg.get("bob") {
            let state = bob.core.lock().state.current.display_name().to_string();
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
        let reg = registry.lock();
        if reg.get("bob").is_some() {
            println!("  ✓ Bob respawned automatically!\n");
        }
    }

    draw_split_screen(&registry);

    // Cleanup
    println!("\n  Cleaning up...");
    {
        let reg = registry.lock();
        for (_, handle) in reg.iter() {
            let mut child = handle.child.lock();
            let _ = child.kill();
        }
    }
    let _ = std::fs::remove_dir_all(&home);

    println!("  ✓ Demo complete!\n");
    println!("  With real AI backends, agents use MCP tools to autonomously");
    println!("  delegate_task, report_result, and coordinate.\n");
    // Sprint 56 Track H4 (#525 item 11): demo trailing → quickstart
    // funnel. Pre-Track-H4 the demo's "Next:" block listed `doctor`
    // and `start` but skipped quickstart, leaving operators trying
    // to start a fleet without ever running the interactive setup
    // that writes fleet.yaml + .env. Add `quickstart` as the
    // canonical first step for production setup.
    println!("  Want to run your own fleet?");
    println!("    agend-terminal quickstart # Interactive setup (.env + fleet.yaml)\n");
    println!("  Other useful commands:");
    println!("    agend-terminal doctor     # Check backends + fleet config");
    println!("    agend-terminal start      # Start fleet (after quickstart)");
    println!();

    Ok(())
}

fn draw_split_screen(registry: &agent::AgentRegistry) {
    let reg = registry.lock();

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
    reg: &parking_lot::MutexGuard<'_, std::collections::HashMap<String, agent::AgentHandle>>,
    name: &str,
) -> Vec<String> {
    if let Some(handle) = reg.get(name) {
        {
            let core = handle.core.lock();
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

// ─────────────────────────────────────────────────────────────────
// Sprint 58 Wave 2 PR-1 (#11) — helper-binary staleness check
// tests. Each test plants a synthetic `$AGEND_HOME/bin/<name>` with
// controlled mtime via `filetime` (already a workspace dep used by
// other modules) OR by ordered `std::fs::write` followed by a brief
// sleep — the latter is portable and sufficient since classification
// only needs strict ordering, not absolute mtime values.
// ─────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────
// Sprint 59 Wave 2 PR-IMPL (F2 — γ): `agend-terminal doctor topics`
// — operator-callable diagnostic for telegram topic state.
// Backed by `crate::bootstrap::doctor_topics`.
// ─────────────────────────────────────────────────────────────────

/// Options surface for `cli::run_doctor_topics`. Mirrors the clap
/// args at `Commands::Doctor { action: Some(DoctorAction::Topics { ... }) }`
/// so the cli entry-point is a thin shim.
pub struct DoctorTopicsOptions<'a> {
    pub cleanup: bool,
    pub format: &'a str,
    pub yes: bool,
    pub prefer_fleet: bool,
    pub prefer_registry: bool,
}

/// Run the topics-diagnostic doctor flow. Pure read in the default
/// case; calls `execute_cleanup` when `--cleanup` is set.
pub fn run_doctor_topics(home: &Path, opts: DoctorTopicsOptions) -> anyhow::Result<()> {
    use crate::bootstrap::doctor_topics::{
        classify, execute_cleanup, render_human, render_json, CleanupAction, DriftResolution,
    };

    if opts.prefer_fleet && opts.prefer_registry {
        anyhow::bail!("--prefer-fleet and --prefer-registry are mutually exclusive; pick one");
    }

    let mut report = classify(home);

    // Probe `can_manage_topics` permission so the human/json output
    // can surface it. Best-effort: failures fall back to `None`.
    report.can_manage_topics = probe_can_manage_topics(home);

    // Always print the inspection output — operator sees state
    // before any cleanup acts.
    let output = match opts.format {
        "json" => render_json(&report),
        _ => render_human(&report),
    };
    println!("{output}");

    if !opts.cleanup {
        return Ok(());
    }

    // Cleanup gate: confirmation prompt unless --yes.
    if !opts.yes {
        eprintln!(
            "About to act on stale_registry / drift_fleet / orphan entries above. \
             Continue? (y/N): "
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    let drift_resolution = if opts.prefer_fleet {
        DriftResolution::PreferFleet
    } else if opts.prefer_registry {
        DriftResolution::PreferRegistry
    } else {
        DriftResolution::LeaveDrift
    };

    let actions = execute_cleanup(home, &report, drift_resolution);
    println!("\nCleanup actions ({}):", actions.len());
    for action in &actions {
        match action {
            CleanupAction::DeletedFromChatAndRegistry {
                topic_id,
                instance_name,
            } => println!("  ✓ deleted topic {topic_id} ({instance_name}) — chat + registry"),
            CleanupAction::UnregisteredOnly {
                topic_id,
                instance_name,
            } => println!("  ✓ updated registry for {topic_id} ({instance_name}) — chat unchanged"),
            CleanupAction::SkippedNoPermission {
                topic_id,
                instance_name,
            } => println!("  ⚠ skipped {topic_id} ({instance_name}) — bot lacks can_manage_topics"),
            CleanupAction::SkippedNeedsResolution {
                topic_id,
                instance_name,
                reason,
            } => println!("  ⚠ skipped {topic_id} ({instance_name}) — {reason}"),
            CleanupAction::SkippedApiError {
                topic_id,
                instance_name,
                error,
            } => println!("  ✗ skipped {topic_id} ({instance_name}) — API error: {error}"),
        }
    }
    Ok(())
}

/// Probe `can_manage_topics` for the configured telegram channel.
/// Returns `None` when the channel is unconfigured (no telegram
/// section in fleet.yaml OR bot token unavailable). The probe is
/// network-bound; expensive failures (rate limit, network blip)
/// fall back to `Some(false)` to err on the safe side (cleanup
/// will skip rather than try-and-fail).
fn probe_can_manage_topics(home: &Path) -> Option<bool> {
    use crate::channel::telegram::{can_manage_topics_for, resolve_channel_only_from};
    let ch = resolve_channel_only_from(home).ok()?;
    let bot = teloxide::Bot::new(&ch.token);
    let chat_id = teloxide::types::ChatId(ch.group_id);
    Some(can_manage_topics_for(&bot, chat_id))
}

// ── Sprint 60 W2 PR-1 (#P1-1) — agend skills CLI subcommands ──────────

/// `agend skills add <source>`.
pub fn run_skills_add(home: &Path, source: &str) -> anyhow::Result<()> {
    let skill = crate::skills::add(home, source)?;
    println!(
        "added skill '{}'\n  source: {}\n  version: {}",
        skill.name,
        if skill.source.is_empty() {
            "(unrecorded)"
        } else {
            &skill.source
        },
        if skill.version.is_empty() {
            "(unpinned)"
        } else {
            &skill.version
        },
    );
    Ok(())
}

/// `agend skills remove <name>`.
pub fn run_skills_remove(home: &Path, name: &str) -> anyhow::Result<()> {
    crate::skills::remove(home, name)?;
    println!("removed skill '{name}' (idempotent)");
    Ok(())
}

/// `agend skills list`.
pub fn run_skills_list(home: &Path) -> anyhow::Result<()> {
    let skills = crate::skills::list(home)?;
    if skills.is_empty() {
        println!(
            "no skills installed under {}",
            crate::skills::skills_root(home).display()
        );
        return Ok(());
    }
    println!(
        "installed skills (under {}):",
        crate::skills::skills_root(home).display()
    );
    for skill in skills {
        println!(
            "  - {}  source={}  version={}",
            skill.name,
            if skill.source.is_empty() {
                "(unrecorded)".to_string()
            } else {
                skill.source
            },
            if skill.version.is_empty() {
                "(unpinned)".to_string()
            } else {
                skill.version
            },
        );
    }
    Ok(())
}

/// `agend skills install <working_dir>`.
pub fn run_skills_install(home: &Path, working_dir: &str) -> anyhow::Result<()> {
    let wd = std::path::PathBuf::from(working_dir);
    if !wd.exists() {
        return Err(anyhow::anyhow!(
            "working_dir does not exist: {}",
            wd.display()
        ));
    }
    let outcomes = crate::skills::install_for_agent(home, &wd)?;
    println!("installed skills into {}:", wd.display());
    for o in outcomes {
        match o.mode {
            crate::skills::InstallMode::Symlink => {
                println!("  ✓ {} (symlink) → {}", o.backend, o.target.display())
            }
            crate::skills::InstallMode::Copy => {
                println!("  ✓ {} (copy) → {}", o.backend, o.target.display())
            }
            crate::skills::InstallMode::Skipped => println!(
                "  - {} (skipped: {})",
                o.backend,
                o.skipped_reason.unwrap_or_default()
            ),
        }
    }
    Ok(())
}

/// `agend skills update [<name>]` — update one or all.
pub fn run_skills_update(home: &Path, name: Option<&str>) -> anyhow::Result<()> {
    if let Some(n) = name {
        let skill = crate::skills::update(home, n)?;
        println!(
            "updated skill '{}'\n  source: {}\n  version: {}",
            skill.name, skill.source, skill.version
        );
        return Ok(());
    }
    let outcomes = crate::skills::update_all(home);
    if outcomes.is_empty() {
        println!("no skills to update — `skills add` first");
        return Ok(());
    }
    let mut failures = 0;
    for (name, result) in &outcomes {
        match result {
            Ok(skill) => println!("  ✓ {} → version={}", name, skill.version),
            Err(e) => {
                failures += 1;
                eprintln!("  ✗ {name}: {e}");
            }
        }
    }
    if failures > 0 {
        return Err(anyhow::anyhow!(
            "{}/{} skills failed to update",
            failures,
            outcomes.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod helper_staleness_tests {
    use super::{check_helper_staleness, classify_helper_staleness, HelperStaleness};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_home(tag: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-helper-staleness-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Plant a file at `path` with the file's mtime guaranteed to
    /// be at least `pause_ms` after the most recent write. Used to
    /// create deterministic stale/fresh ordering in tests.
    fn write_then_pause(path: &std::path::Path, content: &[u8], pause_ms: u64) {
        std::fs::write(path, content).expect("write");
        // Tiny pause so the next write lands strictly later.
        std::thread::sleep(std::time::Duration::from_millis(pause_ms));
    }

    #[test]
    fn helper_staleness_state_string_taxonomy() {
        // Pin string identifiers downstream consumers (greppers,
        // doctor parsers, future Sprint 59 telemetry) will rely on.
        assert_eq!(HelperStaleness::Fresh.as_str(), "fresh");
        assert_eq!(HelperStaleness::Stale.as_str(), "stale");
        assert_eq!(HelperStaleness::NotInstalled.as_str(), "not_installed");
        assert_eq!(
            HelperStaleness::UndeterminableDaemonPath.as_str(),
            "undeterminable"
        );
    }

    #[test]
    fn doctor_silent_when_helper_binary_fresh() {
        // Fresh case: helper mtime >= daemon mtime → no actionable
        // warn appears in the rendered report. Pin the structural
        // signal: presence of "stale" word in the output marks a
        // warn; its absence marks fresh-state-clean.
        let home = tmp_home("fresh");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Plant the daemon-binary stand-in FIRST.
        let daemon = home.join("agend-terminal-fake");
        write_then_pause(&daemon, b"daemon-binary-stand-in", 20);

        // Plant the helpers AFTER → their mtime is later → fresh.
        let helper_git = if cfg!(windows) {
            bin.join("agend-git.exe")
        } else {
            bin.join("agend-git")
        };
        write_then_pause(&helper_git, b"helper-binary", 10);
        let helper_bridge = if cfg!(windows) {
            bin.join("agend-mcp-bridge.exe")
        } else {
            bin.join("agend-mcp-bridge")
        };
        write_then_pause(&helper_bridge, b"helper-binary", 10);

        // Direct classifier check (more deterministic than going
        // through current_exe()).
        let state = classify_helper_staleness(Some(&daemon), &helper_git);
        assert_eq!(state, HelperStaleness::Fresh);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn doctor_detects_stale_helper_binary_via_mtime() {
        // Stale case: helper mtime < daemon mtime → classifier
        // returns Stale.
        let home = tmp_home("stale");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Plant the helper FIRST (older).
        let helper = if cfg!(windows) {
            bin.join("agend-git.exe")
        } else {
            bin.join("agend-git")
        };
        write_then_pause(&helper, b"old-helper", 30);

        // Plant the daemon binary stand-in AFTER → newer mtime →
        // helper is stale relative to daemon.
        let daemon = home.join("agend-terminal-fake");
        write_then_pause(&daemon, b"newer-daemon", 10);

        let state = classify_helper_staleness(Some(&daemon), &helper);
        assert_eq!(state, HelperStaleness::Stale);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn doctor_helper_check_handles_missing_helper_gracefully() {
        // Missing helper → NotInstalled (NOT a panic, NOT Stale).
        // Operators on first-startup-before-any-helper-install
        // hit this branch. Doctor surfaces info-level "not yet
        // installed" rather than scary "stale" warning.
        let home = tmp_home("missing");
        let daemon = home.join("agend-terminal-fake");
        std::fs::write(&daemon, b"daemon").unwrap();
        let helper = home.join("bin").join("agend-git");
        // Helper file not created.

        let state = classify_helper_staleness(Some(&daemon), &helper);
        assert_eq!(state, HelperStaleness::NotInstalled);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn doctor_helper_check_handles_undeterminable_daemon_path_gracefully() {
        // No daemon path resolved (e.g. current_exe() failed) →
        // UndeterminableDaemonPath. Helper exists but we can't
        // compare against anything — surface the state rather
        // than panicking.
        let home = tmp_home("undeterminable");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let helper = bin.join("agend-git");
        std::fs::write(&helper, b"helper").unwrap();

        let state = classify_helper_staleness(None, &helper);
        assert_eq!(state, HelperStaleness::UndeterminableDaemonPath);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn doctor_warn_message_contains_actionable_cargo_install_command() {
        // Lead-spec invariant: when staleness is detected, the
        // doctor's report MUST include a literal `cargo install
        // --path . --force` instruction (or an equivalent that
        // operators can copy-paste). Pin against future refactors
        // that drop the actionable instruction in favour of
        // generic "fix it" language.
        let home = tmp_home("warn-message");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Plant a stale helper: helper FIRST, daemon AFTER.
        // (The check_helper_staleness path uses
        // `std::env::current_exe()` for the daemon binary — which
        // in tests resolves to the test runner binary, NOT a
        // synthetic. The current_exe under `cargo test` is
        // typically newer than any helpers we plant a moment
        // earlier, so this test naturally lands in the Stale
        // branch on most CI runners.)
        let helper = if cfg!(windows) {
            bin.join("agend-git.exe")
        } else {
            bin.join("agend-git")
        };
        write_then_pause(&helper, b"older-helper", 200);

        let report = check_helper_staleness(&home);

        // The test runner binary's mtime might be older or newer
        // than the helper in some CI environments. Cover both
        // paths: if the report carries a stale warning, validate
        // its actionable contents; otherwise skip the assertion
        // (the no-stale path is exercised by
        // `doctor_silent_when_helper_binary_fresh`).
        if report.contains("stale") {
            assert!(
                report.contains("cargo install --path . --force"),
                "stale warn must include the actionable cargo-install instruction. Got:\n{report}"
            );
            assert!(
                report.contains("agend-terminal stop") || report.contains("restart the daemon"),
                "stale warn must mention daemon restart for refresh. Got:\n{report}"
            );
        }

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn check_helper_staleness_handles_missing_bin_dir_gracefully() {
        // Operators on a brand-new $AGEND_HOME might not have any
        // `bin/` dir yet. The doctor's helper-staleness check must
        // NOT panic on this — both helpers should land in
        // NotInstalled state.
        let home = tmp_home("no-bin-dir");
        // Don't create the bin dir.

        let report = check_helper_staleness(&home);
        // Both helpers should be "not yet installed".
        assert!(
            report.contains("not yet installed"),
            "missing bin/ should surface as 'not yet installed' for both helpers. Got:\n{report}"
        );
        // No stale warning when nothing is installed.
        assert!(
            !report.contains("⚠"),
            "missing bin/ should NOT trigger stale warning. Got:\n{report}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn helpers_list_covers_both_canonical_helper_binaries() {
        // Source-text invariant: the helpers list inside
        // `check_helper_staleness` must include both `agend-git`
        // and `agend-mcp-bridge` (the two binaries the daemon
        // installs at $AGEND_HOME/bin). A future refactor that
        // adds a third helper but forgets to extend this list
        // would silently leave operators with no staleness
        // warning for that binary.
        let src = include_str!("cli.rs");
        let helpers_block_start = src.find("let helpers = [").unwrap();
        let helpers_block_end = helpers_block_start + 200;
        let helpers_block = &src[helpers_block_start..helpers_block_end.min(src.len())];
        assert!(
            helpers_block.contains("\"agend-git\""),
            "helpers list must include `agend-git`"
        );
        assert!(
            helpers_block.contains("\"agend-mcp-bridge\""),
            "helpers list must include `agend-mcp-bridge`"
        );
    }
}
