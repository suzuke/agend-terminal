//! Terminal application — multi-tab/pane TUI for agent management.
//!
//! Uses agent::spawn_agent() for all panes (agents and shells), sharing the
//! same PTY lifecycle as the daemon: auto-dismiss, state tracking, broadcast.

// #t-5: `pub(crate)` so `render::overlay` can read the completion specs
// (`CommandSpec` / `COMMAND_SPECS` / `matching_specs`). `execute` stays
// `pub(super)` = app-only, so command EXECUTION is not widened.
pub(crate) mod commands;
mod discord_hooks;
mod dispatch;
mod menu;
mod mouse;
mod overlay;
mod pane_factory;
mod restart_resume;
mod rpc;
#[cfg(all(unix, test))]
use restart_resume::restart_argv;
pub(crate) use restart_resume::run_restart_probe;
use restart_resume::{
    commit_app_restart, poll_commit_pending, poll_restart_probe, spawn_restart_probe,
    CommitPending, CommitPoll, ProbePoll, RestartProbe, RESTART_COMMIT_WATCHDOG,
};
mod ui_state;
use ui_state::{UiDeps, UiState};
// #2453: root AppState/RestartState owners, re-homed to a sibling module
// (src/app/mod.rs sits under the grandfathered anti-monolith ratchet).
mod app_state;
use app_state::{AppDeps, AppState, LoopFlow};
mod session;
mod telegram_hooks;
mod tui_spawn;

use menu::{build_menu_items, pane_from_menu_item};
pub use overlay::{BoardView, DecisionMode, MenuItem, MenuItemKind, TaskBoardMode};

use crate::agent::{self, AgentRegistry};
use crate::keybinds::KeyHandler;
use crate::layout::{Layout, Pane};
use crate::notification_queue;
use crate::render;
use overlay::{CloseTarget, Overlay};

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use parking_lot::Mutex;
use ratatui::DefaultTerminal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run the terminal application.
pub fn run(
    fleet_path_override: Option<&str>,
    restart_requester_id: Option<crate::types::InstanceId>,
) -> Result<()> {
    // Fail fast with an actionable message when stdout/stdin is not a real
    // terminal (piped, redirected, or run under a non-interactive harness).
    // Without this guard `ratatui::init()` panics deep in terminal setup
    // ("Device not configured") AND leaks raw mouse/alt-screen escape
    // sequences to the captured stream. The TUI workbench needs a TTY; the
    // headless control surface is `agend-terminal start`.
    {
        use std::io::IsTerminal as _;
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "`agend-terminal app` requires an interactive terminal (TTY). \
                 stdin/stdout is not a TTY here. Run it in a real terminal, or use \
                 `agend-terminal start` for headless/daemon mode."
            );
        }
    }

    // Redirect tracing to log file BEFORE ratatui takes over stderr.
    // Must happen before main.rs's tracing init — caller should skip init for App.
    let home = crate::home_dir();

    // #927 PR-A: was a raw `OpenOptions::truncate(true)` write on
    // `app.log` with hardcoded `debug` filter — long sessions hit
    // unbounded growth (operator-observed). Now uses the parameterized
    // rolling-appender shared with the daemon path:
    //   - DAILY rotation, retain N days (env: AGEND_LOG_RETAIN_DAYS).
    //   - Default filter `agend_terminal=info` (was `debug`); opt into
    //     verbose via `AGEND_LOG=agend_terminal=debug`.
    //   - First-boot pre-rotation `app.log` is dropped (synthesis policy:
    //     tiny file, no rescue value).
    //
    // Guard lifetime: the `WorkerGuard` returned by setup_rolling_tracing
    // must outlive the entire app session; drop = flush + close the
    // worker thread. Bound here in `app::run`'s scope so it lives until
    // the fn returns (the entire TUI loop lifetime).
    // #2453 R2: stash the app log guard in the global flush slot so the
    // owner-restart path (which re-execs, bypassing Drop) can flush it explicitly
    // via `flush_app_log()`. The guard lives for the process lifetime either way.
    if let Ok(guard) = crate::logging::setup_rolling_tracing(
        &home,
        "app",
        "agend_terminal=info",
        crate::logging::MigrationPolicy::Drop,
    ) {
        crate::logging::store_app_flush_guard(guard);
    }

    // Extract embedded fleet protocol to AGEND_HOME/protocol/.default/.
    // App boot remains available when the daemon-owned delivery cannot be
    // refreshed, but the failure is explicit for doctor/status follow-up.
    // The subscriber above is installed first so this diagnostic reaches
    // app.log even when extraction fails.
    if let Err(error) = crate::protocol::extract_default(&home) {
        tracing::error!(%error, "protocol default extraction failed during app boot");
    }

    let fleet_path = fleet_path_override.map(PathBuf::from);

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
    )
    .ok();

    let mut terminal = ratatui::init();

    // Push keyboard enhancement AFTER entering alternate screen — Kitty
    // protocol push/pop stack is per-screen, so pushing on the main screen
    // is lost when ratatui::init() switches to the alternate screen.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        ),
    )
    .ok();

    // Panic hook: restore terminal on panic so the user doesn't get stuck
    // in raw mode with mouse capture enabled. Chains the original hook so
    // panic messages still print.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::PopKeyboardEnhancementFlags,
        );
        ratatui::restore();
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
        );
        original_hook(info);
    }));

    let result = run_app(&mut terminal, fleet_path.as_deref(), restart_requester_id);

    // Restore default panic hook before normal cleanup (avoid double-restore).
    drop(std::panic::take_hook());

    // Pop before leaving alternate screen (symmetric with push).
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
    )
    .ok();

    ratatui::restore();

    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste,
    )
    .ok();

    // #2453 R2: commit an owner-restart AFTER the terminal is restored (raw mode
    // off, alternate screen left) and the api guard dropped — re-exec in place so
    // the fresh app re-acquires the flock + re-enters the TUI on the SAME terminal.
    match result {
        Ok(RunOutcome::Normal) => Ok(()),
        Ok(RunOutcome::RestartRequested(requester_id)) => {
            crate::logging::flush_app_log();
            commit_app_restart(fleet_path_override, requester_id)
        }
        Err(e) => Err(e),
    }
}

/// Main event loop for the TUI app.
///
/// M5 note: this function is 550+ lines with 15+ locals. Extraction to
/// `app/event_loop.rs` deferred — the function is a single coherent event
/// loop with no natural split point that wouldn't increase coupling.
/// Locals are all loop-scoped state (layout, registry, overlay, etc.)
/// that the event loop needs in every iteration. Splitting would require
/// passing all state as a context struct, adding complexity without
/// reducing cognitive load. Revisit if the function grows further.
/// #2057 instrument (gated on `AGEND_TUI_SIZE_DEBUG=1`): log the controlling
/// TTY's kernel winsize (crossterm reads fd 1) at a named STARTUP milestone.
/// The operator A/B showed fd-1 rows drop 56→53 only in the default home (12
/// agents / 7 tabs) — somewhere in startup a phase shrinks the TUI's OWN
/// terminal. Bracketing the phases (baseline → post-fleet-spawn → pre-loop)
/// pins which one; the per-frame loop probe (`#2057-size`) shows the loop only
/// ever observes the post-shrink value, so the culprit is pre-loop.
fn trace_tty_size(enabled: bool, phase: &str) {
    if !enabled {
        return;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    tracing::info!(
        tag = "#2057-startup",
        phase,
        cols,
        rows,
        "controlling-TTY kernel winsize at startup milestone"
    );
}

/// #t-84833-10 redraw-storm frame cap: the render loop draws at most once per
/// `FRAME_INTERVAL`. Under a boot-time PTY-output flood (11 agents spewing
/// startup output → one `wakeup_tx` per chunk), the loop used to draw once per
/// wakeup (observed 300–741 fps) and saturate the render thread, starving input.
/// 33 ms ≈ 30 fps is plenty for a TUI and halves the render CPU vs 60 fps.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// #freeze-4 (t-…2324) per-frame TIME budget for the BOOT catch-up drain
/// ([`render::drain_all_panes_until`]). Load-bearing safety: each boot frame yields
/// to `select!` (input) after this much draining, so a restart flood can never
/// hard-freeze input — worst case the loading phase lasts a few more frames. ~80 ms
/// keeps input serviced ~12×/s while clearing the flood far faster than the
/// steady-state 64 KiB/frame. Provisional (conservative); tune from `#freeze-*`
/// restart data if needed.
const BOOT_FRAME_TIME_CAP: std::time::Duration = std::time::Duration::from_millis(80);

/// #freeze-4 hard ceiling on the boot catch-up phase: after this the loop reverts
/// to the steady-state cap regardless of remaining backlog (which then drains under
/// the normal bounded path). Guarantees boot can't hang unbounded on a pathological
/// backlog. Provisional (conservative).
const MAX_BOOT_CATCHUP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Pure frame-cap decision (the test seam): may we draw now? `None` = never drawn
/// (always draw the first frame); otherwise only once `FRAME_INTERVAL` has elapsed
/// since the last draw. Independent of *what* changed — coalescing/dirtiness is
/// the caller's job; this only rate-limits.
fn should_draw(
    last_draw: Option<std::time::Instant>,
    now: std::time::Instant,
    frame_interval: std::time::Duration,
) -> bool {
    match last_draw {
        None => true,
        Some(t) => now.duration_since(t) >= frame_interval,
    }
}

/// #84833-15 R2 perf: `sync_notification_state` scans the notification-queue dir
/// (`read_dir` + `read_to_string` per pane) on EVERY render wakeup just to refresh
/// the tab/title `[N]` badge — a disk-I/O storm under the same wakeup flood the
/// frame cap addresses. The badge tolerates ≥1s staleness, so throttle the scan to
/// once per second (the sibling idle-flush is already ≥1s-gated). This is independent
/// of #2346's draw cap: the scan runs at the loop-body TOP, before `should_draw`.
const NOTIF_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// #2524 P2b / #2313: same disk-I/O-storm shape as `NOTIF_SYNC_INTERVAL` above,
/// for the decision-board pending-question badge — throttle to once per second
/// instead of scanning `decisions/` every wakeup.
const DECISION_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Pure throttle decision (the test seam), mirroring `should_draw`: may we re-scan
/// the notification queues now? `None` = never scanned (scan the first frame so the
/// badge is correct at startup); otherwise only once `NOTIF_SYNC_INTERVAL` has
/// elapsed since the last scan.
fn should_sync_notifications(
    last_sync: Option<std::time::Instant>,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    match last_sync {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// #2050 simplify PR-C (②): render the active overlay on top of the main frame.
/// Extracted verbatim from the two byte-identical blocks in `run_app` — the normal
/// draw path and the screenshot (TestBackend) path — so they can't drift. Takes
/// `&mut Overlay` because `ScratchShell` drains/resizes its pane during render.
fn render_active_overlay(
    frame: &mut ratatui::Frame,
    overlay: &mut Overlay,
    layout: &Layout,
    registry: &AgentRegistry,
    home: &Path,
) {
    match overlay {
        Overlay::NewTabMenu { items, selected }
        | Overlay::SplitMenu {
            items, selected, ..
        } => {
            render::render_menu(frame, items, *selected);
        }
        Overlay::RenameTab { input } | Overlay::RenamePane { input } => {
            render::render_rename(frame, input);
        }
        Overlay::ConfirmClose { target } => {
            let msg = match target {
                CloseTarget::Pane => "Close pane view? (does not delete fleet agent) (y/n)",
                CloseTarget::Tab => "Close tab view? (does not delete fleet agents) (y/n)",
            };
            render::render_confirm(frame, msg);
        }
        Overlay::ConfirmRestart { command } => {
            render::render_confirm(frame, &format!("Restart '{command}'? (y/n)"));
        }
        Overlay::ConfirmDeleteInstance {
            name,
            input,
            notice,
        } => {
            let mut msg = format!("Type '{name}' to delete: {input}");
            if let Some(notice) = notice {
                msg.push_str(" — ");
                msg.push_str(notice);
            }
            render::render_confirm(frame, &msg);
        }
        Overlay::TabList { selected } => {
            render::render_tab_list(frame, layout, *selected);
        }
        Overlay::MovePaneTarget {
            selected,
            source_tab_idx,
            split_dir,
            ..
        } => {
            render::render_move_pane_target(frame, layout, *selected, *source_tab_idx, *split_dir);
        }
        Overlay::Help => {
            render::render_help(frame);
        }
        Overlay::ReconnectNotice { message } => {
            render::render_notice(frame, message);
        }
        Overlay::Scroll => {
            let so = layout
                .active_tab()
                .and_then(|t| t.focused_pane())
                .map(|p| p.scroll_offset)
                .unwrap_or(0);
            render::render_scroll_indicator(frame, so);
        }
        Overlay::Command {
            ref input,
            selected,
        } => {
            // Compute the completion once (same `palette_completion` the key
            // handler uses) and hand it to the renderer, so the highlighted
            // candidate always matches what Tab completes. Registry is touched
            // only for agent-argument completion — off the per-pane render path.
            let completion = commands::palette_completion(input, registry);
            render::render_command_palette(frame, input, *selected, &completion);
        }
        Overlay::Decisions {
            ref items,
            selected,
            ref mode,
        } => {
            render::render_decisions(frame, items, *selected, mode);
        }
        Overlay::Tasks {
            ref items,
            col,
            row,
            ref mode,
            ref view,
            ref notice,
            ..
        } => {
            render::render_tasks(
                frame,
                items,
                *col,
                *row,
                mode,
                *view,
                home,
                notice.as_deref(),
            );
        }
        Overlay::ScratchShell { pane } => {
            render::render_scratch_shell(frame, pane, registry);
        }
        Overlay::Organizer {
            scope,
            plan,
            applied,
            notice,
            ..
        } => {
            render::render_organizer(frame, plan, &scope.label(), *applied, notice.as_deref());
        }
        Overlay::None => {}
    }
}

/// #2453 R2: outcome of the app TUI loop — either a normal quit or an
/// operator-requested owner-restart that `run()` commits via in-place re-exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOutcome {
    Normal,
    RestartRequested(Option<crate::types::InstanceId>),
}

fn disable_closed_daemon_event_receiver(
    daemon_event_rx: &mut crossbeam_channel::Receiver<rpc::EventStreamOutcome>,
    disabled_event_rx: &crossbeam_channel::Receiver<rpc::EventStreamOutcome>,
) {
    *daemon_event_rx = disabled_event_rx.clone();
}

fn run_app(
    terminal: &mut DefaultTerminal,
    fleet_override: Option<&Path>,
    _restart_requester_id: Option<crate::types::InstanceId>,
) -> Result<RunOutcome> {
    let home = crate::home_dir();
    app_boot_preflight(&home);
    crate::bootstrap::signals::install_term_only();
    let fleet_path =
        fleet_override.map_or_else(|| crate::fleet::fleet_yaml_path(&home), Path::to_path_buf);
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // #1027: shared TUI status-bar flag — supervisor flips it, render reads it.
    let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
        Default::default();
    let (app_restart_gate, _app_restart_inject, _app_restart_rx) = build_app_restart_wiring();
    let attached_run_dir = setup_app_bootstrap(&home, &fleet_path)?;
    if !wait_for_agent_spawn_completion(&attached_run_dir, std::time::Duration::from_secs(30)) {
        anyhow::bail!("daemon control plane is ready but agent startup did not complete");
    }
    let attached_run_dir = Some(attached_run_dir);
    let attached_mode = true;
    let app_restart_rx = crossbeam_channel::never();
    let mut state = AppState::new();
    let (wakeup_tx, wakeup_rx) = crossbeam_channel::unbounded::<usize>();
    // #2057: size-probe env gate, read once; shared by milestones + per-frame probe.
    let size_debug = std::env::var("AGEND_TUI_SIZE_DEBUG").as_deref() == Ok("1");
    trace_tty_size(size_debug, "startup-baseline");
    // restart-freeze RCA anchor: boot critical path start (pure tracing).
    let restore_start = std::time::Instant::now();
    let (task_rpc_tx, task_rpc_rx, task_rpc_worker) = rpc::spawn_task_worker(&home);
    let (remote_state_rpc_tx, remote_state_rpc_rx, remote_state_rpc_worker) =
        rpc::spawn_agent_state_worker(&home);
    let (remote_restart_request_tx, remote_restart_request_rx) =
        crossbeam_channel::bounded::<commands::RemoteRestartRequest>(16);
    let (remote_restart_worker_tx, remote_restart_outcome_rx, remote_restart_worker) =
        rpc::spawn_remote_restart_worker(&home);
    let (event_stop_tx, daemon_event_rx, event_worker) = rpc::spawn_event_worker(&home);
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: crate::channel::TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode,
        size_debug,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    // `_attach_tx` keepalive for the loop scope: see `restore_and_attach`.
    let (_attach_tx, attach_rx, attach_workers) = state.restore_and_attach(&deps, restore_start)?;
    let mut reap_workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let event_rx = spawn_crossterm_event_reader();
    let mut daemon_event_rx = daemon_event_rx;
    let disabled_event_rx = crossbeam_channel::never::<rpc::EventStreamOutcome>();
    log_pre_render_milestone(size_debug, restore_start, attached_mode);
    let loop_result: Result<()> = loop {
        if term_requested_logged() || state.poll_restart(&deps) == LoopFlow::Break {
            break Ok(());
        }
        state.pre_select(terminal, &deps, &mut reap_workers);
        if let Err(error) = state.render_frame(terminal, &deps) {
            break Err(error);
        }
        crossbeam_channel::select! {
            recv(app_restart_rx) -> req => state.handle_restart_request(req, &deps),
            recv(event_rx) -> ev => {
                if state.handle_crossterm_event(ev, terminal, &deps, &mut reap_workers)
                    == LoopFlow::Break
                {
                    break Ok(());
                }
            }
            recv(wakeup_rx) -> _ => state.handle_wakeup(&wakeup_rx),
            recv(attach_rx) -> outcome => state.handle_attach_outcome(outcome, &deps, &mut reap_workers),
            recv(task_rpc_rx) -> outcome => state.handle_task_rpc_outcome(outcome),
            recv(remote_state_rpc_rx) -> outcome => state.handle_agent_state_rpc_outcome(outcome),
            recv(remote_restart_request_rx) -> request => {
                if let Ok(request) = request {
                    state.handle_remote_restart_request(request, &deps);
                }
            },
            recv(remote_restart_outcome_rx) -> outcome => {
                state.handle_remote_restart_outcome(outcome, &deps);
            },
            recv(daemon_event_rx) -> outcome => {
                let receiver_closed = outcome.is_err();
                state.handle_event_stream_outcome(outcome, &deps);
                if receiver_closed {
                    disable_closed_daemon_event_receiver(&mut daemon_event_rx, &disabled_event_rx);
                }
            },
            default(state.select_timeout()) => state.handle_idle_tick(&deps),
        }
    };
    drop(remote_state_rpc_tx);
    drop(remote_state_rpc_rx);
    let _ = remote_state_rpc_worker.join();
    drop(remote_restart_worker_tx);
    drop(remote_restart_request_tx);
    // Stop consuming outcomes before joining: a full bounded outcome queue
    // must wake the worker's blocking send so teardown cannot deadlock.
    drop(remote_restart_outcome_rx);
    let _ = remote_restart_worker.join();
    drop(task_rpc_tx);
    let _ = task_rpc_worker.join();
    let _ = event_stop_tx.try_send(());
    drop(event_stop_tx);
    let _ = event_worker.join();
    // Teardown gating rationale is documented on `app_teardown`.
    app_teardown(&home, &state.ui.layout, reap_workers, attach_workers);
    loop_result?;
    Ok(state.restart.restart_outcome)
}

/// App is permanently a thin client: attach to a ready daemon, or start one
/// detached and wait for its control plane. A failed spawn may simply mean a
/// concurrent starter won the singleton race, so re-probe before surfacing it.
fn setup_app_bootstrap(home: &Path, fleet_path: &Path) -> Result<PathBuf> {
    if let Some(run_dir) = wait_for_ready_daemon(home, std::time::Duration::from_millis(300)) {
        return Ok(run_dir);
    }

    let spawn_error = crate::bootstrap::daemon_spawn::spawn_detached(home, Some(fleet_path)).err();
    if let Some(run_dir) = wait_for_ready_daemon(home, std::time::Duration::from_secs(5)) {
        return Ok(run_dir);
    }

    match spawn_error {
        Some(error) => Err(error.context("start daemon for app thin client")),
        None => anyhow::bail!("daemon started but its control plane did not become ready"),
    }
}

fn wait_for_ready_daemon(home: &Path, timeout: std::time::Duration) -> Option<PathBuf> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(run_dir) = crate::daemon::find_active_run_dir(home) {
            if crate::ipc::probe_api(&run_dir) {
                return Some(run_dir);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Session reconciliation needs the complete daemon registry. This is separate
/// from the API liveness gate above: `.ready` means the agent spawn loop ended.
fn wait_for_agent_spawn_completion(run_dir: &Path, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if run_dir.join(".ready").is_file() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Process-global flag used by app-owned pane attachment workers while the TUI
/// exits. Sticky-true because the process exits immediately afterwards.
static APP_SHUTDOWN: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

pub(crate) fn app_shutdown_flag() -> &'static Arc<std::sync::atomic::AtomicBool> {
    APP_SHUTDOWN.get_or_init(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// #2453 R2: app owner-restart gate + bounded(1) request channel. Owned mode
/// injects the Sender into the API server; the run_app loop owns the Receiver
/// and drives probe → re-exec commit.
fn build_app_restart_wiring() -> (
    crate::api::app_restart::AppRestartGate,
    crate::api::app_restart::AppRestart,
    crossbeam_channel::Receiver<crate::api::app_restart::AppRestartRequest>,
) {
    let gate = crate::api::app_restart::AppRestartGate::new();
    let (tx, rx) = crossbeam_channel::bounded::<crate::api::app_restart::AppRestartRequest>(1);
    let inject = crate::api::app_restart::AppRestart {
        tx,
        gate: gate.clone(),
    };
    (gate, inject, rx)
}

/// #2453 R2: attached mode never listens for app restart requests — the
/// daemon owns restart and the injected Sender was dropped in
/// `setup_app_bootstrap`, so the arm must be never-ready.
/// restart-freeze RCA (t-…55279): entering the render loop — the first draw
/// is imminent; elapsed from `restore_start` is the full boot critical path
/// the operator perceives as the restart freeze. Pure tracing.
fn log_pre_render_milestone(size_debug: bool, restore_start: std::time::Instant, attached: bool) {
    trace_tty_size(size_debug, "pre-render-loop");
    tracing::info!(
        phase = "pre-render-loop",
        elapsed_ms = restore_start.elapsed().as_millis() as u64,
        attached = attached,
        "pre-render-loop: entering render loop (first draw imminent)"
    );
}

fn term_requested_logged() -> bool {
    if crate::bootstrap::signals::term_requested() {
        tracing::info!("app: SIGTERM received, exiting main loop");
        return true;
    }
    false
}

/// #2453 Slice 2: one-shot app boot preflight, extracted verbatim from the
/// head of `run_app`.
fn app_boot_preflight(home: &Path) {
    // #2325: the app process (unlike the daemon's `run_core`, the only other
    // `runtime_config::reload` caller) never tick-reloads runtime config, so load
    // the persisted values once at startup. Otherwise a persisted `copy_on_select`
    // (the TUI mouse copy-on-select mode) would reset to the compile-time default
    // on every restart. In-session changes update the in-process global directly
    // via `runtime_config::set` (the `Ctrl+B e` toggle and `:set`/`:config set`).
    crate::runtime_controls::reload_runtime_controls(home);

    // PR-D6/F1: fail LOUD if the retired `AGEND_WORKTREE_PRUNE_LIVE` is still set.
    // The LIVE fleet daemon runs THIS app-mode path — never `run_core` (see
    // `main.rs`'s subcommand dispatch: `App` → `app::run` → here, vs `Start` →
    // `daemon::run`/`run_core`; the two are mutually exclusive per process). So
    // the run_core:764 boot-warn is DEAD in production without this call. Fired
    // ONCE per app boot here: `run_app` is invoked exactly once (from `run`, not
    // in any retry loop) and this sits before the sweep/TUI event loop. Mirrors
    // run_core's placement — a single startup warn so an operator carrying the
    // stale flag learns it is ignored (not silent).
    crate::worktree_cleanup::warn_if_prune_live_retired();
}

/// #2453 Slice 2: the crossterm event-reader thread, extracted verbatim.
fn spawn_crossterm_event_reader() -> crossbeam_channel::Receiver<Event> {
    // fire-and-forget: blocks in crossterm::event::read(); terminated by process exit.
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<Event>();
    std::thread::Builder::new()
        .name("crossterm_events".into())
        .spawn(move || loop {
            if let Ok(ev) = event::read() {
                if event_tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .ok();
    event_rx
}

/// How long teardown may spend joining the unmanaged-child reapers.
///
/// One grace window plus a small margin: `terminate_agents_parallel` gives each
/// child `daemon::SHUTDOWN_GRACE` (2s) and reaps in parallel, so a healthy reap
/// finishes inside that and this only bites on a child that ignores SIGTERM.
const REAP_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(2_600);

/// How long teardown may then spend joining the attach workers.
const ATTACH_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(1_500);

/// Deadlines for teardown's two join groups, from one start instant.
///
/// SEPARATE budgets, deliberately, rather than the single shared deadline this
/// replaced. The reap join runs first, and with one deadline a reap that used the
/// whole window handed the attach join an already-expired one — and
/// `bounded_join_attach_workers` detaches on an expired deadline, so every attach
/// worker would be abandoned. That is exactly the failure this file's reaper
/// ownership exists to prevent, just relocated to the other group.
///
/// The documented total is REAP_JOIN_BUDGET + ATTACH_JOIN_BUDGET = 4.1s of
/// worst-case join time at quit. Nothing bounds app quit below that: the only
/// wall-clock consumers nearby are `bounded_join_attach_workers`'s own test
/// (which injects its own deadline) and overlay's close-path assertion (which
/// bounds CLOSE, not quit, and stays asynchronous).
fn teardown_join_deadlines(start: std::time::Instant) -> (std::time::Instant, std::time::Instant) {
    let reap_deadline = start + REAP_JOIN_BUDGET;
    (reap_deadline, reap_deadline + ATTACH_JOIN_BUDGET)
}

/// #render-first phase-(b) F2: join app workers, but DETACH any that haven't
/// finished by the shared `deadline` — a worker wedged mid-spawn (fork/exec /
/// skills / subscribe) must not hang quit (that would move the restore freeze to
/// quit). A detached worker's child, if it registered one, is reaped by the
/// registry drain + the OS (same stance as #2311's grace→SIGKILL). Returns the
/// number detached (wedged past the deadline).
fn bounded_join_attach_workers(
    handles: Vec<std::thread::JoinHandle<()>>,
    deadline: std::time::Instant,
) -> usize {
    let mut detached = 0usize;
    for h in handles {
        while !h.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if h.is_finished() {
            let _ = h.join();
        } else {
            detached += 1; // past the shared deadline → detach (drop the handle)
        }
    }
    detached
}

/// Flush presentation state and bound worker shutdown. The daemon and agent
/// processes remain independently owned.
fn app_teardown(
    home: &Path,
    layout: &Layout,
    reap_workers: Vec<std::thread::JoinHandle<()>>,
    attach_workers: Vec<std::thread::JoinHandle<()>>,
) {
    // The event loop has stopped, so this final batch may wait for a metadata
    // lock. Periodic UI flushing uses only the nonblocking path above; placing
    // this before every other teardown side effect also covers render errors.
    notification_queue::flush_pending_activity_at_teardown(home);
    session::save_session(home, layout);
    let (reap_deadline, attach_deadline) = teardown_join_deadlines(std::time::Instant::now());
    let detached_reapers = bounded_join_attach_workers(reap_workers, reap_deadline);
    if detached_reapers > 0 {
        tracing::warn!(
            detached = detached_reapers,
            "detached unmanaged child reaper(s) still running at app quit"
        );
    }
    let detached = bounded_join_attach_workers(attach_workers, attach_deadline);
    if detached > 0 {
        tracing::warn!(
            detached,
            "detached attach worker(s) still running at app quit"
        );
    }
}

/// #1762: does this forwarded keystroke ENTER the agent's input buffer
/// (text-composing), as opposed to NAVIGATE / CONTROL (arrows, F-keys, Esc,
/// Ctrl-combos, Tab, Backspace)? Only composing input should mark a draft
/// (#1457/#1675 draft-gating); navigation/control must NOT, or an idle operator
/// who merely browses history (Up/Down) or fat-fingers a non-text key traps every
/// actionable inject (task dispatch / ci-ready) behind the ~5min draft escape
/// window — exactly when away (the #1762 report).
///
/// Composing = at least one byte that enters the buffer: a non-space printable
/// (`> 0x20`, excluding DEL `0x7f`) or any UTF-8 continuation/lead byte
/// (`>= 0x80`). Deliberately NON-composing: ESC-prefixed sequences (arrows /
/// F-keys / Esc / Alt-combos — `key_to_bytes` encodes every nav key with a `0x1b`
/// lead), bare control bytes (Ctrl-combos, `Tab`=`\t`, `Backspace`=`0x7f`, and
/// `Enter`=`\r`/`\n` — Enter is the separately-detected SUBMIT signal), and lone
/// whitespace (the fat-fingered-space case — a real draft always carries a
/// non-space char that marks it, so #1675 protection is preserved). EXCEPTION:
/// bracketed paste (`ESC [ 200 ~`) wraps PASTED TEXT and IS composing.
fn is_text_composing_input(bytes: &[u8]) -> bool {
    if bytes.first() == Some(&0x1b) {
        return bytes.starts_with(b"\x1b[200~");
    }
    bytes.iter().any(|&b| (b > 0x20 && b != 0x7f) || b >= 0x80)
}

/// Write bytes to the focused pane's PTY (Local) or remote bridge (Remote).
fn write_to_focused(home: &Path, layout: &mut Layout, registry: &AgentRegistry, bytes: &[u8]) {
    if let Some(pane) = layout.active_tab_mut().and_then(|t| t.focused_pane_mut()) {
        // #1762: only text-composing input marks a draft — navigation / control
        // keys (and lone whitespace) must not defer actionable injects.
        if is_text_composing_input(bytes) {
            notification_queue::record_input_activity(home, &pane.agent_name);
        }
        // Sprint 54 P2-3: backend-aware submit detection (claude-first
        // allowlist). When the keystroke buffer contains the agent's
        // submit key (`\r` for claude, also matches paste-with-newlines
        // since the underlying CLI submits on any \r), record a
        // separate timestamp so the daemon supervisor can detect
        // "typed but not submitted" against this paired signal. Other
        // backends gracefully no-op — the supervisor tick reads
        // `last_submit_at_ms == 0` for them and skips emission per
        // the explicit backend allowlist there.
        if pane_input_contains_submit(pane.backend.as_ref(), bytes) {
            notification_queue::record_submit_activity(home, &pane.agent_name);
        }
        pane.write_input(registry, bytes);
    }
}

/// #783: write bytes to a SPECIFIC pane by id, bypassing focus. Used by
/// the mouse-forward path so the SGR report reaches the pane under the
/// cursor (e.g. opencode in a non-focused split) instead of the focused
/// pane. Shares the same submit-detection bookkeeping as
/// `write_to_focused` since the byte stream eventually lands at the
/// same `Pane::write_input` sink.
fn write_to_pane(
    home: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    pane_id: usize,
    bytes: &[u8],
) {
    if let Some(pane) = layout
        .active_tab_mut()
        .and_then(|t| t.root_mut().find_pane_mut(pane_id))
    {
        // #1762: only text-composing input marks a draft (see `write_to_focused`).
        if is_text_composing_input(bytes) {
            notification_queue::record_input_activity(home, &pane.agent_name);
        }
        if pane_input_contains_submit(pane.backend.as_ref(), bytes) {
            notification_queue::record_submit_activity(home, &pane.agent_name);
        }
        pane.write_input(registry, bytes);
    }
}

/// Sprint 54 P2-3: backend-aware submit detection. Returns true iff
/// the backend is on the submit-detection allowlist AND the keystroke
/// buffer contains its submit key. Hard-coded claude-only first round
/// per dispatch — extending to other backends just requires adding
/// arms to the match.
fn pane_input_contains_submit(backend: Option<&crate::backend::Backend>, bytes: &[u8]) -> bool {
    let Some(b) = backend else {
        return false;
    };
    // #1457: detect the submit key for ALL backends (was claude-only). Without
    // this, non-claude panes never record a submit timestamp, so `draft_state`
    // would see `submit=0` and treat every keystroke as a permanent unsent
    // draft → notifications would NEVER deliver to them (worse than the bug
    // this fixes). `submit_key` is `\r` for every preset; the empty-key guard
    // below no-ops backends (Shell/Raw) that declare no submit key.
    let submit = b.preset().submit_key.as_bytes();
    if submit.is_empty() || bytes.len() < submit.len() {
        return false;
    }
    bytes.windows(submit.len()).any(|w| w == submit)
}

fn sync_notification_state(home: &Path, layout: &mut Layout) {
    // #2967/#2978: ONE read_dir of the queue directory per pass — was one
    // `pending_count` (its own read_dir) per PANE, every ~1s.
    let snapshot = notification_queue::QueueDirSnapshot::scan(home);
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            if let Some(pane) = tab.root_mut().find_pane_mut(pane_id) {
                let prev = pane.pending_notification_count;
                let now = snapshot.pending_count(&pane.agent_name);
                // #1944 instrument: the pane-title `[N]` badge renders off this
                // count (core_render.rs). Log every CHANGE so the "badge
                // disappeared" report can be located at runtime — the code is
                // intact, so this catches whether the count actually reaches the
                // render with N>0 (or is reset to 0 before the next frame).
                if now != prev {
                    tracing::info!(
                        tag = "#1944-badge-state",
                        agent = %pane.agent_name,
                        prev,
                        now,
                        "pending-notification badge count changed"
                    );
                }
                pane.pending_notification_count = now;
            }
        }
    }
}

/// #2524 P2b / #2313: refresh the per-pane "you asked something" badge and
/// return the fleet-wide pending-question total, from ONE
/// `decisions::count_pending` scan — not one scan per pane (same disk-I/O
/// shape `sync_notification_state` already documents above).
fn sync_decision_badge_state(home: &Path, layout: &mut Layout) -> usize {
    let counts = crate::decisions::count_pending(home);
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            if let Some(pane) = tab.root_mut().find_pane_mut(pane_id) {
                pane.pending_decision_count = counts
                    .by_author
                    .get(pane.agent_name.as_str())
                    .copied()
                    .unwrap_or(0);
            }
        }
    }
    counts.total
}

fn flush_idle_notifications(home: &Path, layout: &mut Layout) {
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            let Some(pane) = tab.root_mut().find_pane_mut(pane_id) else {
                continue;
            };
            let agent_name = pane.agent_name.clone();
            flush_notifications_for_pane(home, pane, |text, channel_origin| {
                // #982 RC: queue contents come from compose_aware_*
                // which would have submit-injected on the immediate-
                // idle path. The flush must preserve that contract or
                // queued hints (e.g. `[AGEND-MSG-PENDING]`) land in
                // the prompt buffer without the backend submit_key —
                // codex one-shots silently drop the wake.
                //
                // #3324: and it forwards the row's typed channel provenance,
                // so a deferred external message is not re-classified as
                // internal on the way out of the queue.
                crate::inbox::inject_notification_with_submit(
                    home,
                    &agent_name,
                    text,
                    channel_origin,
                )
            });
        }
    }
}

/// #1944: bottom rows of the rendered screen scanned for the input box (prompt +
/// a few wrapped input rows). Mirrors the #1912 readback `READBACK_TAIL_ROWS`.
const DRAFT_INPUT_TAIL_ROWS: usize = 8;

/// Per-pane wrapper around the shared flush core
/// (`inbox::notify::flush_agent_queue_with_state` — busy/typing holds and
/// MAX_DEFER caps live there so the daemon's per-tick `notification_flush`
/// handler applies the IDENTICAL release policy in headless mode). The
/// TUI-only part kept here: the #1944/#1948 input-box probe that refines a
/// raw `Drafting` against the ACTUAL rendered input box (`pane.vterm` is
/// TUI-owned; the headless flush has no pane and conservatively honors the
/// raw draft state), plus the badge refresh.
///
/// #1944: the input-box probe shared by the flush gate below and the #3663
/// publish path. `Some(true)` = box verifiably empty, `Some(false)` = real
/// draft, `None` = undeterminable (fail toward protection). Only runs when
/// `raw_state` is `Drafting` — otherwise there is no stale draft to refine.
fn probe_input_box_empty(
    pane: &mut Pane,
    raw_state: notification_queue::DraftState,
) -> Option<bool> {
    if raw_state != notification_queue::DraftState::Drafting {
        return None;
    }
    pane.backend.as_ref().and_then(|b| {
        // #1948(b): codex's empty box shows DIM ghost text after `›`, which a
        // plain marker probe mis-reads as typed content — route it through the
        // DIM-aware check (needs the per-char dim mask). Everyone else uses the
        // text-only probe: marker (claude/agy) → placeholder (kiro) → fallback.
        // #t-97931 (F-A): route through the path-aware `Pane::tail_lines*` — off-
        // thread the main-thread `pane.vterm` is idle/blank, so reading it directly
        // mis-reads a real unsent draft as an empty box and the gate clobbers it.
        if let Some(marker) = b.input_dim_ghost_marker() {
            let (text, dim) = pane.tail_lines_with_dim(DRAFT_INPUT_TAIL_ROWS);
            notification_queue::input_box_dim_aware_empty(&text, &dim, marker)
        } else {
            notification_queue::input_box_empty_probe(
                &pane.tail_lines(DRAFT_INPUT_TAIL_ROWS),
                b.input_prompt_marker(),
                b.input_empty_placeholder(),
            )
        }
    })
}

/// #3663: publish the TUI-observed empty box so DAEMON-side gates (restart
/// defer, `should_defer_inject`, ambient gate — all metadata-only readers in
/// another process) see the same refinement the TUI flush applies. Runs even
/// when the queue is empty (the restart gate reads metadata, not the queue);
/// gated on `raw_state == Drafting` so idle panes never write metadata every
/// tick. Buffered + non-blocking; the ~1s badge cadence flushes it.
fn publish_cleared_observation(home: &Path, pane: &mut Pane) {
    let raw_state = notification_queue::draft_state(home, &pane.agent_name);
    if raw_state != notification_queue::DraftState::Drafting {
        return;
    }
    if probe_input_box_empty(pane, raw_state) == Some(true) {
        notification_queue::record_cleared_activity(home, &pane.agent_name);
    }
}

fn flush_notifications_for_pane<F>(home: &Path, pane: &mut Pane, injector: F)
where
    F: FnMut(&str, Option<crate::channel::ChannelKind>) -> anyhow::Result<()>,
{
    // #3663: publish the empty-box observation even when nothing is queued —
    // the restart gate reads metadata, not the queue, so a type-then-clear
    // with zero pending notifications must still lift the daemon-side defer.
    publish_cleared_observation(home, pane);
    if pane.pending_notification_count == 0 {
        return;
    }
    // #1457: gate on draft state (input-vs-submit order), not the 3s idle window.
    // Drafting → defer everything; Abandoned → escape valve releases just the
    // oldest (trickle); None (clean buffer) → drain the whole backlog.
    //
    // #1944: refine `Drafting` with the input box's ACTUAL content. A
    // type-then-clear (typed then deleted to empty, or typed-but-not-submitted)
    // leaves `typed_ms > submit_ms` for up to 5 min while the box is visibly
    // EMPTY — the timestamp-only heuristic mis-read that as a live draft and held
    // messages until the next real submit. `pane.vterm` is the owned, live
    // rendered screen (no lock), so reading the input line here is cheap. When the
    // box is verifiably empty → deliver; a real draft (text in the box) OR an
    // undeterminable read (no marker / agent mid-output) both keep deferring
    // (fail toward draft-protection — never risk clobbering a real draft).
    let raw_state = notification_queue::draft_state(home, &pane.agent_name);
    let buffer_empty = probe_input_box_empty(pane, raw_state);
    let effective_state = if buffer_empty == Some(true) {
        notification_queue::DraftState::None
    } else {
        raw_state
    };
    // #1944 instrument: the RCA had ZERO logs on this path. Surface every DEFER
    // (or buffer-override) decision so the next stranded-message report is
    // diagnosable. Clean immediate deliveries (None, no draft) are not logged.
    if effective_state != notification_queue::DraftState::None || buffer_empty.is_some() {
        let (typed_ms, submit_ms) =
            notification_queue::read_input_submit_timestamps(home, &pane.agent_name);
        tracing::info!(
            tag = "#1944-draftgate-decision",
            agent = %pane.agent_name,
            raw_state = ?raw_state,
            effective_state = ?effective_state,
            buffer_empty = ?buffer_empty,
            typed_ms,
            submit_ms,
            pending = pane.pending_notification_count,
            "draft-gate delivery decision"
        );
    }
    crate::inbox::notify::flush_agent_queue_with_state(
        home,
        &pane.agent_name,
        effective_state,
        injector,
    );
    pane.pending_notification_count = notification_queue::pending_count(home, &pane.agent_name);
}

/// Adjust scroll offset of the focused pane by `delta` lines (positive = up, negative = down).
fn scroll_focused(layout: &mut Layout, delta: i32) {
    if let Some(tab) = layout.active_tab_mut() {
        let fid = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
            // #offthread-scroll: off-thread mode leaves `pane.vterm` idle, so use the
            // path-aware max (snapshot history when off-thread, else live vterm).
            let max = pane.scroll_max();
            if delta > 0 {
                pane.scroll_offset = (pane.scroll_offset + delta as usize).min(max);
            } else {
                // AUDIT2-017: clamp to `max` before stepping down so a stale
                // offset (history shrank under it) snaps back immediately rather
                // than needing hundreds of `saturating_sub` steps to recover.
                pane.scroll_offset = pane
                    .scroll_offset
                    .min(max)
                    .saturating_sub((-delta) as usize);
            }
        }
    }
}

/// Kill an agent and remove from registry. Delegates to
/// [`crate::daemon::lifecycle::delete_transaction`] so app-mode and
/// daemon-mode share one tear-down path.
///
/// Sprint 20 F3 fix: previously called only `child.kill()` (leader-only,
/// leaving subprocess trees alive on backends like kiro-cli) and skipped
/// event_log + Telegram binding rollback. The shared transaction now does
/// `kill_process_tree` + synchronous wait-for-exit + `take_binding` + event
/// log, matching the API delete path.
fn kill_agent(home: &Path, registry: &AgentRegistry, name: &str) {
    // The shared managed-delete boundary marks deleting and fences transport
    // before it touches the registry or child process.
    crate::daemon::lifecycle::delete_transaction(home, name, registry, None, false);
}

/// Reap a TUI-local shell by its authoritative registry key.
///
/// Unmanaged shells are intentionally absent from fleet.yaml, so the managed
/// name-based delete transaction cannot resolve them. Remove the exact UUID
/// entry first, then terminate and reap the owned child.
fn kill_unmanaged_agents(
    registry: &AgentRegistry,
    instance_ids: impl IntoIterator<Item = crate::types::InstanceId>,
    reap_workers: &mut Vec<std::thread::JoinHandle<()>>,
) {
    let drained: Vec<(String, crate::daemon::ChildHandle)> = instance_ids
        .into_iter()
        .filter_map(|instance_id| agent::remove_and_unregister(registry, &instance_id))
        .map(|handle| (handle.name.to_string(), handle.child))
        .collect();
    if drained.is_empty() {
        return;
    }
    // The JoinHandle is retained by the app owner and joined before attach
    // workers at teardown, so close-then-quit cannot abandon this reap.
    let reap_worker = std::thread::Builder::new()
        .name("unmanaged_child_reaper".to_string())
        .spawn(move || {
            crate::daemon::terminate_agents_parallel(drained);
        })
        .expect("spawn unmanaged child reaper");
    reap_workers.push(reap_worker);
}

fn kill_unmanaged_agent(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    reap_workers: &mut Vec<std::thread::JoinHandle<()>>,
) {
    kill_unmanaged_agents(registry, [instance_id], reap_workers);
}

/// Whether the agent's child process is still running.
///
/// Used by the scratch shell overlay to self-close when the user exits the
/// shell naturally (`exit`, Ctrl+D) or the process crashes. Returns `false`
/// if the name is no longer registered (already reaped) or `try_wait`
/// reports the child has exited. `AgentHandle.child` is a `parking_lot::Mutex`
/// (which never poisons), so a CONTENDED lock is read via `try_lock()` and
/// treated as alive: this runs on the TUI main loop, and a blocking `.lock()`
/// would wedge the whole UI if another thread panicked while holding the child
/// lock (parking_lot leaves it locked). Transient contention just keeps the
/// overlay open for that tick — Esc still works.
fn agent_is_alive(registry: &AgentRegistry, instance_id: crate::types::InstanceId) -> bool {
    let reg = agent::lock_registry(registry);
    let Some(handle) = reg.get(&instance_id) else {
        return false;
    };
    // Bind to a local so the child-lock's temporary MutexGuard drops
    // before `reg` does — returning the match expression directly trips
    // the borrow checker because temporaries outlive the registry lock.
    let alive = match handle.child.try_lock() {
        Some(mut child) => !matches!(child.try_wait(), Ok(Some(_))),
        // Contended → cannot prove the child exited without blocking the main
        // loop; treat as alive and re-check next tick.
        None => true,
    };
    alive
}

/// `app` unit tests live in the sibling `tests.rs` (exempt from the src
/// file-size invariant `tests/src_file_size_invariant.rs` by filename);
/// `mod tests;` resolves to `tests.rs`, and its `use super::*` reaches this
/// module's private items exactly as the former inline `mod tests {}` did.
#[cfg(test)]
mod tests;

/// #2453 R2 slice 2 — preflight failure proves ZERO teardown. `Prepared` is the
/// ONLY `ProbePoll` variant that can lead the TUI loop into the irreversible
/// teardown+exec (and only after the post-flush ack); every probe failure mode must
/// instead roll the gate back to `Serving` and yield `Abort`, leaving the app
/// serving. This unit-checks that on the extracted `poll_restart_probe` without the TUI.
#[cfg(all(test, unix))]
mod probe_poll_tests {
    use super::*;
    use crate::api::app_restart::{AppRestartGate, AppRestartVerdict};
    use std::time::{Duration, Instant};

    /// A `RestartProbe` whose direct child exits with `code` (via `sh -c 'exit N'`)
    /// and a far-future deadline, so polling exercises the exit-code branch (not
    /// the timeout branch). Returns the reply receiver so the channel stays open.
    fn exited_probe(code: i32) -> (RestartProbe, crossbeam_channel::Receiver<AppRestartVerdict>) {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .spawn()
            .expect("spawn probe child");
        let (reply, rx) = crossbeam_channel::unbounded::<AppRestartVerdict>();
        // The failure path never reads flush_ack; a disconnected receiver is fine.
        let (_ack_tx, flush_ack) = crossbeam_channel::bounded::<()>(1);
        let deadline = Instant::now() + Duration::from_secs(60);
        (
            RestartProbe {
                child,
                reply,
                flush_ack,
                requester_id: None,
                deadline,
            },
            rx,
        )
    }

    #[test]
    fn remote_restart_teardown_drops_outcome_receiver_before_join_3649() {
        let source = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let drop_at = source
            .find("drop(remote_restart_outcome_rx)")
            .expect("teardown must close the outcome receiver");
        let join_at = source
            .find("remote_restart_worker.join()")
            .expect("teardown must join the restart worker");
        assert!(
            drop_at < join_at,
            "closing the outcome receiver must precede joining the worker"
        );
    }

    #[test]
    fn preflight_failure_aborts_gate_to_serving_no_commit() {
        let (probe, _rx) = exited_probe(1);
        let gate = AppRestartGate::new();
        assert!(gate.try_begin_probe(), "handler claimed the gate (Probing)");
        let mut slot = Some(probe);
        // The child exits immediately; poll until it is observed terminal.
        let result = loop {
            match poll_restart_probe(&mut slot, &gate) {
                ProbePoll::Pending => std::thread::sleep(Duration::from_millis(10)),
                other => break other,
            }
        };
        let ProbePoll::Abort(_reply, reason) = result else {
            panic!(
                "a FAILED preflight must yield Abort (never Commit) — Commit is the ONLY \
                 path that breaks the loop into the irreversible teardown+exec"
            );
        };
        assert!(
            reason.contains("preflight failed"),
            "abort reason must name the preflight failure — got {reason:?}"
        );
        assert!(
            gate.is_serving(),
            "gate MUST roll back to Serving on a failed preflight (zero teardown)"
        );
        assert!(
            !gate.is_committing(),
            "a failed preflight must NEVER reach Committing (which would teardown+exec)"
        );
        assert!(
            slot.is_none(),
            "the failed probe must be consumed out of the slot"
        );
    }
}

#[cfg(test)]
mod review_repro_app_tui;

// #2453 R2 P0-2: commit-pending witnesses re-homed to a sibling `*tests*.rs` file
// (exempt from the src file-size invariant); `use super::*` reaches this module's
// private `CommitPending` / `CommitPoll` / `poll_commit_pending`.
#[cfg(test)]
mod commit_pending_tests;

#[cfg(test)]
mod restart_resume_tests;

// #2453: AppState ownership structural guards, re-homed to a sibling
// `*_tests.rs` file (exempt from the src file-size invariant) mirroring
// `commit_pending_tests` above — src/app/mod.rs sits under a grandfathered
// anti-monolith ratchet and may not grow.
#[cfg(test)]
mod appstate_2453_tests;

#[cfg(test)]
mod reap_workers_3420_tests;
