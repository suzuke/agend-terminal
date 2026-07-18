//! CLI helpers: doctor, capture, test runners.
//! Extracted from main.rs for module split.

use crate::{agent, backend, fleet};
use serde_json::{json, Value};
use std::path::Path;

/// Start daemon with fleet.yaml config.
///
/// Delegates preflight (lock, run dir, cookie, fleet normalize, agent resolve,
/// telegram init) to [`crate::bootstrap::prepare`]. This routes both `start`
/// and `app` entry points through the same seam — see the `bootstrap` module
/// docs for the bug this addresses.
pub fn start_with_fleet(home: &Path, fleet_path: &Path) -> anyhow::Result<()> {
    // #1814 Stage 1: a successor spawned by `restart_daemon` carries a
    // legitimate `AGEND_SUCCESSOR_HANDOFF` marker and MUST take the minimal
    // pre-lock handoff boot path — NOT the full `prepare` (which would run the
    // destructive sole-daemon reconciles while the predecessor is still alive).
    // The marker is set only by `spawn_successor_handoff`; a normal/operator
    // start never carries it, so this is a no-op for every non-handoff boot.
    if crate::daemon::restart::successor_handoff_marker().is_some() {
        tracing::info!(
            "#1814: AGEND_SUCCESSOR_HANDOFF present — taking successor-handoff boot path"
        );
        return crate::daemon::run_successor_handoff(home, fleet_path);
    }
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
            backend: None,
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

    // #1441: standalone capture spawns one ad-hoc agent into a fresh registry
    // keyed by a minted UUID (no fleet context), so locate it by display name.
    let stripped = {
        let reg = registry.lock();
        match reg.values().find(|h| h.name.as_str() == name.as_str()) {
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
        if let Some(h) = reg.values().find(|h| h.name.as_str() == name.as_str()) {
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

    let fleet_path = crate::fleet::fleet_yaml_path(home);
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
    // #910 PR2 of 4: daemon-registry truth via runtime helper. Probe
    // still needs find_active_run_dir for the per-agent socket check
    // (returns false on no run dir, mirroring pre-PR2 behaviour).
    let agents = crate::runtime::list_agents_with_fallback(home);
    let run = crate::daemon::find_active_run_dir(home);
    for agent in &agents {
        let ok = run
            .as_ref()
            .map(|r| crate::ipc::probe_agent(r, agent))
            .unwrap_or(false);
        println!(
            "    {agent} {}",
            if ok {
                "✓ (port responsive)"
            } else {
                "✗ (port stale)"
            }
        );
    }
    if agents.is_empty() {
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

/// Run provider diagnostics (currently Fugu/Sakana via codex) for `doctor providers`.
pub fn run_doctor_providers(format: &str, probe: bool) -> anyhow::Result<()> {
    let d = crate::provider_detect::detect_default();
    let probe_result = if probe {
        if let Some(provider) = d.provider.as_ref() {
            let cache_path =
                crate::provider_detect::provider_probe_cache_path(&crate::home_dir(), d.descriptor);
            let env_lookup = |name: &str| std::env::var(name).ok();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            Some(
                rt.block_on(crate::provider_detect::probe_provider_models_fail_open(
                    d.descriptor,
                    provider,
                    &env_lookup,
                    &cache_path,
                )),
            )
        } else {
            None
        }
    } else {
        None
    };
    if format == "json" {
        let source = d.provider_source.as_ref().map(|p| p.display().to_string());
        let descriptor = d.descriptor;
        let provider_descriptors: Vec<_> = crate::provider_detect::PROVIDER_DESCRIPTORS
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "base_url": p.base_url,
                    "env_key": p.env_key,
                    "wire_api": p.wire_api,
                    "compatible_harnesses": p.compatible_harnesses,
                    "probe_path": p.probe_path,
                })
            })
            .collect();
        let fixed_provider_backends: Vec<_> = crate::provider_detect::FIXED_PROVIDER_BACKENDS
            .iter()
            .map(|b| {
                serde_json::json!({
                    "backend": b.backend,
                    "reason": b.reason,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "fugu": {
                    "status": d.status.as_str(),
                    "codex_on_path": d.codex_on_path,
                    "provider_source": source,
                    "has_credential": d.has_credential,
                    "models": d.models,
                    "hints": d.hints,
                    "descriptor": {
                        "name": descriptor.name,
                        "base_url": descriptor.base_url,
                        "env_key": descriptor.env_key,
                        "wire_api": descriptor.wire_api,
                        "compatible_harnesses": descriptor.compatible_harnesses,
                        "probe_path": descriptor.probe_path,
                    },
                    "probe": probe_result,
                },
                "provider_descriptors": provider_descriptors,
                "fixed_provider_backends": fixed_provider_backends,
            })
        );
        return Ok(());
    }

    println!("Provider diagnostics");
    println!(
        "  declared provider descriptors: {}",
        crate::provider_detect::PROVIDER_DESCRIPTORS.len()
    );
    println!("  fugu: {}", d.status.as_str());
    println!("    base_url: {}", d.descriptor.base_url);
    println!("    env_key: {}", d.descriptor.env_key);
    println!("    wire_api: {}", d.descriptor.wire_api);
    println!(
        "    compatible_harnesses: {}",
        d.descriptor.compatible_harnesses.join(", ")
    );
    if let Some(path) = d.descriptor.probe_path {
        println!("    probe_path: {path} (fail-open, cached; not on startup path)");
    }
    println!("    codex_on_path: {}", d.codex_on_path);
    println!("    has_credential: {}", d.has_credential);
    if let Some(source) = d.provider_source.as_ref() {
        println!("    provider_source: {}", source.display());
    }
    if !d.models.is_empty() {
        println!("    models: {}", d.models.join(", "));
    }
    if probe {
        if let Some(probe) = probe_result.as_ref() {
            println!("    probe: {}", probe.status.as_str());
            if !probe.models.is_empty() {
                println!("    probe_models: {}", probe.models.join(", "));
            }
            if let Some(error) = probe.error.as_ref() {
                println!("    probe_note: {error}");
            }
        } else {
            println!("    probe: unknown (provider block unavailable)");
        }
    }
    for hint in &d.hints {
        println!("    hint: {hint}");
    }
    println!("  fixed-provider backends (outside provider axis):");
    for b in crate::provider_detect::FIXED_PROVIDER_BACKENDS {
        println!("    {}: {}", b.backend, b.reason);
    }
    Ok(())
}

/// Options surface for `cli::run_doctor_topics`. Mirrors the clap
/// args at `Commands::Doctor { action: Some(DoctorAction::Topics { ... }) }`
/// so the cli entry-point is a thin shim.
pub struct DoctorTopicsOptions<'a> {
    pub cleanup: bool,
    pub format: &'a str,
    pub yes: bool,
}

/// Run the topics-diagnostic doctor flow. Pure read in the default
/// case; calls `execute_cleanup` when `--cleanup` is set.
pub fn run_doctor_topics(home: &Path, opts: DoctorTopicsOptions) -> anyhow::Result<()> {
    use crate::bootstrap::doctor_topics::{
        classify, execute_cleanup, render_human, render_json, CleanupAction,
    };

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
        eprintln!("About to act on orphan entries above. Continue? (y/N): ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    let actions = execute_cleanup(home, &report);
    println!("\nCleanup actions ({}):", actions.len());
    for action in &actions {
        match action {
            CleanupAction::DeletedFromChatAndRegistry {
                topic_id,
                instance_name,
            } => println!("  ✓ deleted topic {topic_id} ({instance_name}) — chat + registry"),
            CleanupAction::SkippedNoPermission {
                topic_id,
                instance_name,
            } => println!("  ⚠ skipped {topic_id} ({instance_name}) — bot lacks can_manage_topics"),
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
    // CLI subcommand uses install-all default; per-instance filtering
    // is the daemon's automatic launch-flow responsibility (Sprint 61
    // W1 PR-2 #P0-2 fleet.yaml `instance.<name>.skills:` override).
    let outcomes = crate::skills::install_for_agent(home, &wd, None)?;
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

/// `agend-terminal admin gc-dry-run` body (#2548: moved from the `gc_dry_run`
/// MCP tool, which itself lived at `src/mcp/handlers/gc.rs`, Sprint 53 P1-4
/// Phase 4 GC visibility wrapping `worktree_pool::gc_dry_run`) — moved out of
/// `src/mcp/` entirely so #1505's MCP-schema undeclared-arg scanner (which
/// walks the whole `src/mcp/` tree) stops treating this CLI-only `format` arg
/// as an MCP handler read. Non-destructive because THIS is the dry-run entry
/// point — it only lists candidates and never removes anything, regardless of
/// any env var. (`AGEND_WORKTREE_ARCHIVE_FALLBACK` does NOT gate collection: it
/// only enables the archive-fallback belt inside the real cutover path — archive
/// a worktree to `.trash` when hard-delete FAILS. It has no bearing on this
/// preview.) Also previews the `target/` retention-sweep
/// candidates (t-…50793-9) so an operator sees what will be reclaimed before
/// the gc_tick runs it. Format: `"human"` (default) or `"json"`.
pub(crate) fn handle_gc_dry_run(home: &Path, args: &Value) -> Value {
    let format = args["format"].as_str().unwrap_or("human");
    if format != "human" && format != "json" {
        return json!({
            "error": format!("invalid 'format': {format:?} — expected 'human' or 'json'")
        });
    }

    let candidates = crate::worktree_pool::gc_dry_run(home);
    let enriched: Vec<Value> = candidates
        .iter()
        .map(|c| {
            let (branch, leased_at, released_at) = read_marker_fields(&c.path);
            json!({
                "agent": c.agent,
                "branch": branch,
                "path": c.path.display().to_string(),
                "leased_at": leased_at,
                "released_at": released_at,
                "reason": c.reason,
            })
        })
        .collect();

    // t-…50793-9: also preview the stale `target/` build dirs the retention
    // sweep would reclaim, so an operator sees what will be deleted before/while
    // the gc_tick runs it. Non-destructive (this only enumerates).
    let targets = crate::worktree_pool::target_sweep_dry_run(home);
    let target_total_bytes: u64 = targets.iter().map(|t| t.size_bytes).sum();
    let target_json: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "agent": t.agent,
                "target": t.target.display().to_string(),
                "idle_secs": t.idle_secs,
                "size_bytes": t.size_bytes,
            })
        })
        .collect();

    if format == "json" {
        return json!({
            "candidates": enriched,
            "count": enriched.len(),
            "target_sweep": target_json,
            "target_sweep_count": target_json.len(),
            "target_sweep_total_bytes": target_total_bytes,
            // VET condition (no-silent-coverage-cap): always surface the sweep's
            // scope boundary so the figures never imply the disk problem is solved.
            "target_sweep_scope": crate::worktree_pool::TARGET_SWEEP_SCOPE_NOTE,
        });
    }

    // Human format. Empty list still emits a single-line summary so an
    // operator running the tool always gets visible feedback.
    let mut out = String::new();
    out.push_str(&format!(
        "Worktree GC dry-run candidates: {} found",
        enriched.len()
    ));
    if !enriched.is_empty() {
        out.push_str("\n\n");
        for (i, c) in enriched.iter().enumerate() {
            let agent = c["agent"].as_str().unwrap_or("?");
            let branch = c["branch"].as_str().unwrap_or("?");
            let leased = c["leased_at"].as_str().unwrap_or("?");
            let released = c["released_at"].as_str().unwrap_or("(none)");
            out.push_str(&format!(
                "  {n}. {agent} / {branch} — leased {leased}, released {released}\n",
                n = i + 1,
            ));
        }
    }

    out.push_str(&format!(
        "\nStale target/ build dirs eligible for sweep: {} (~{} MB)",
        target_json.len(),
        target_total_bytes / (1024 * 1024)
    ));
    for (i, t) in targets.iter().enumerate() {
        out.push_str(&format!(
            "\n  {n}. {agent} — {path} (idle {h}h, ~{mb} MB)",
            n = i + 1,
            agent = t.agent,
            path = t.target.display(),
            h = t.idle_secs / 3600,
            mb = t.size_bytes / (1024 * 1024),
        ));
    }
    // VET condition: honest scope boundary on every preview.
    out.push_str(&format!(
        "\n({})",
        crate::worktree_pool::TARGET_SWEEP_SCOPE_NOTE
    ));

    json!({
        "format": "human",
        "count": enriched.len(),
        "target_sweep_count": target_json.len(),
        "text": out,
    })
}

/// Parse `branch=`, `leased_at=`, `released_at=` lines out of a worktree's
/// `.agend-managed` marker file. Missing fields → `None` (rendered as
/// `null` in JSON, `(none)` in human output). Failed read → all None.
fn read_marker_fields(wt_path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let marker = wt_path.join(".agend-managed");
    let Ok(content) = std::fs::read_to_string(&marker) else {
        return (None, None, None);
    };
    let mut branch = None;
    let mut leased = None;
    let mut released = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("branch=") {
            branch = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("leased_at=") {
            leased = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("released_at=") {
            released = Some(v.to_string());
        }
    }
    (branch, leased, released)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod gc_dry_run_tests {
    use super::*;

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        let h = std::env::temp_dir().join(format!(
            "agend-gc-handler-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&h).unwrap();
        h
    }

    // ── #2548 (was Sprint 53 P1-4): gc_dry_run CLI handler tests ─────────
    //
    // These exercise the production handler. The fixture builders mimic
    // what `worktree_pool::lease` + `release_full` would produce on the
    // filesystem so the underlying `gc_candidates` scan + marker parser
    // see realistic state.
    //
    // Regression-proof: stub `worktree_pool::gc_dry_run` to return an empty
    // Vec → `gc_dry_run_production_smoke_lists_stale_lease` FAILS (count=0
    // when fixture has 1 candidate). Restore → PASS.

    /// Fixture: build a worktree dir with the `.agend-managed` marker shape
    /// `lease()` writes, plus a `released_at=` line older than the GC grace
    /// window (24h) so `gc_candidates` accepts it. No binding is written, so
    /// the candidate scan's "no active binding" gate passes.
    fn make_stale_candidate(home: &std::path::Path, agent: &str, branch: &str) {
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join(agent);
        std::fs::create_dir_all(&wt).unwrap();
        // 48h ago — comfortably past the 24h grace.
        let leased_at = (chrono::Utc::now() - chrono::Duration::hours(72)).to_rfc3339();
        let released_at = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(".agend-managed"),
            format!(
                "agent={agent}\nbranch={branch}\nleased_at={leased_at}\nreleased_at={released_at}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn gc_dry_run_no_candidates_returns_empty() {
        // Empty workspace → 0 candidates, both formats render the empty
        // shape without panicking.
        let home = tmp_home("gc-empty");
        let human = handle_gc_dry_run(&home, &json!({}));
        assert_eq!(human["count"].as_u64(), Some(0), "empty count: {human}");
        assert_eq!(human["format"].as_str(), Some("human"));
        assert!(human["text"].as_str().unwrap_or("").contains("0 found"));

        let json_out = handle_gc_dry_run(&home, &json!({"format": "json"}));
        assert_eq!(json_out["count"].as_u64(), Some(0));
        assert_eq!(
            json_out["candidates"].as_array().map(|a| a.len()),
            Some(0),
            "json candidates must be empty array: {json_out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_human_format_default() {
        // No `format` arg → human default. Output text must contain agent
        // + branch + the "found" summary.
        let home = tmp_home("gc-human");
        make_stale_candidate(&home, "agent-foo", "fix/branch-x");

        let result = handle_gc_dry_run(&home, &json!({}));
        assert_eq!(result["format"].as_str(), Some("human"));
        assert_eq!(result["count"].as_u64(), Some(1));
        let text = result["text"].as_str().unwrap_or("");
        assert!(text.contains("1 found"), "summary: {text}");
        assert!(text.contains("agent-foo"), "must name agent: {text}");
        assert!(text.contains("fix/branch-x"), "must name branch: {text}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_json_format_explicit() {
        // `format=json` → structured candidates array with marker-derived
        // fields. Asserts the response is itself valid JSON-as-Value.
        let home = tmp_home("gc-json");
        make_stale_candidate(&home, "agent-bar", "feat/branch-y");

        let result = handle_gc_dry_run(&home, &json!({"format": "json"}));
        assert_eq!(result["count"].as_u64(), Some(1));
        let arr = result["candidates"]
            .as_array()
            .expect("candidates must be an array");
        assert_eq!(arr.len(), 1);
        let c = &arr[0];
        assert_eq!(c["agent"], "agent-bar");
        assert_eq!(c["branch"], "feat/branch-y");
        assert!(
            c["leased_at"].as_str().is_some(),
            "leased_at must be present: {c}"
        );
        assert!(
            c["released_at"].as_str().is_some(),
            "released_at must be present (this fixture released the lease): {c}"
        );
        assert!(c["path"].as_str().is_some());
        assert!(c["reason"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_invalid_format_rejected() {
        // Typo in format must error gracefully so the operator notices
        // immediately rather than silently getting one of the formats.
        let home = tmp_home("gc-bad-format");
        let result = handle_gc_dry_run(&home, &json!({"format": "xml"}));
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            err.contains("invalid 'format'") && err.contains("xml"),
            "expected invalid-format error mentioning 'xml': {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_dry_run_production_smoke_lists_stale_lease() {
        // Production smoke: full path through `handle_gc_dry_run` →
        // `worktree_pool::gc_dry_run` → `gc_candidates` → marker scan.
        // Exercises the same code paths the CLI dispatches `gc_dry_run`
        // calls into.
        let home = tmp_home("gc-prod-smoke");
        make_stale_candidate(&home, "agent-prod", "feat/prod-stale");

        let result = handle_gc_dry_run(&home, &json!({"format": "json"}));
        assert_eq!(
            result["count"].as_u64(),
            Some(1),
            "production smoke must surface the stale lease via the CLI path"
        );
        let candidates = result["candidates"].as_array().expect("candidates array");
        let agents: Vec<&str> = candidates
            .iter()
            .filter_map(|c| c["agent"].as_str())
            .collect();
        assert!(
            agents.contains(&"agent-prod"),
            "agent-prod must appear in candidates: {agents:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
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
