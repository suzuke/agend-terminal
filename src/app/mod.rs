//! Terminal application — multi-tab/pane TUI for agent management.
//!
//! Uses agent::spawn_agent() for all panes (agents and shells), sharing the
//! same PTY lifecycle as the daemon: auto-dismiss, state tracking, broadcast.

mod api_server;
mod commands;
mod dispatch;
mod mouse;
mod overlay;
mod pane_factory;
mod session;
mod telegram_hooks;
mod tui_events;
mod tui_spawn;

pub use overlay::{BoardView, MenuItem, MenuItemKind, TaskBoardMode};
pub(crate) use tui_events::{TuiEvent, TuiEventSender, TuiNotifier};

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::channel::TelegramStatus;
use crate::keybinds::KeyHandler;
use crate::layout::{Layout, Pane};
use crate::notification_queue;
use crate::render;
use overlay::{CloseTarget, Overlay, OverlayCtx};

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use parking_lot::Mutex;
use ratatui::DefaultTerminal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run the terminal application.
pub fn run(fleet_path_override: Option<&str>) -> Result<()> {
    // Redirect tracing to log file BEFORE ratatui takes over stderr.
    // Must happen before main.rs's tracing init — caller should skip init for App.
    let home = crate::home_dir();

    // Extract embedded fleet protocol to AGEND_HOME/protocol/.default/
    crate::protocol::extract_default(&home);

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
    let _app_log_guard = crate::logging::setup_rolling_tracing(
        &home,
        "app",
        "agend_terminal=info",
        crate::logging::MigrationPolicy::Drop,
    )
    .ok();

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

    let result = run_app(&mut terminal, fleet_path.as_deref());

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
    result
}

/// #1726: per-tick handlers that app-standalone INTENTIONALLY does not run,
/// excluded from the otherwise-shared `build_default_handlers` set. Each is a
/// deliberate, justified omission; the completeness invariant
/// (`app_tick_handlers_cover_every_non_allowlisted_daemon_handler`) fails CI if a
/// NEW handler is added to `build_default_handlers` but is neither run in app nor
/// listed here — so additions are a conscious decision, not silent drift.
///
/// - `recovery_dispatcher`: coupled to run_core's `crash_rx` + `handle_crash_respawn`,
///   which app-standalone has no equivalent of — it does pane-based respawn in the
///   TUI loop. (#685 dispatch-recovery is shadow-default-off anyway.)
/// - `snapshot_rotation`: app owns session persistence via `session::save_session_if_changed`.
/// - `thread_dump`: env-gated diagnostic, not needed in the interactive TUI.
const APP_TICK_ALLOWLIST: &[&str] = &["recovery_dispatcher", "snapshot_rotation", "thread_dump"];

/// Build the per-tick handler set app-standalone runs: the shared
/// `build_default_handlers` minus `APP_TICK_ALLOWLIST`. Extracted so the
/// completeness invariant can compare it against the full daemon set.
///
/// `RecoveryDispatcherHandler` is allowlisted out, so the `crash_tx` it would
/// consume is a throwaway sender (its receiver is dropped immediately).
fn app_tick_handlers() -> Vec<Box<dyn crate::daemon::per_tick::PerTickHandler>> {
    let (crash_tx, _crash_rx) = crossbeam_channel::bounded(1);
    let mut handlers = crate::daemon::build_default_handlers(crash_tx);
    handlers.retain(|h| !APP_TICK_ALLOWLIST.contains(&h.name()));
    handlers
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
fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();
    let fleet_path = fleet_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| crate::fleet::fleet_yaml_path(&home));

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    // #1027: shared TUI status-bar flag. supervisor's mcp_registry_watcher
    // flips it true on post-startup binary refresh; the render loop reads
    // it every frame to surface a warning. Sticky-true — clears only on
    // process restart (fresh tracker = fresh `started_at`).
    let daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tui_event_tx, tui_event_rx) = crossbeam_channel::bounded::<TuiEvent>(256);

    // Preflight via the shared bootstrap seam so `api.cookie` is issued before
    // `api::serve` starts — otherwise `inbox::notify_agent`'s `api::call(INJECT)`
    // from Telegram's router would silently fail.
    //
    // `Attached` means another daemon owns the fleet; the TUI connects as a
    // client (Stage 3.4). `attached_mode` gates every operation that would
    // conflict with the live daemon — session persistence, fleet.yaml sync,
    // supervisor spawn, and agent kill on exit.
    let opts = crate::bootstrap::PrepareOptions {
        resolve_agents: false, // app spawns via pane_factory from tabs
        ..Default::default()
    };
    let mut attached_run_dir: Option<PathBuf> = None;
    let (_api_guard, telegram_state, telegram_status) =
        match crate::bootstrap::prepare(&home, &fleet_path, opts) {
            Ok(crate::bootstrap::BootstrapOutcome::Owned(prepared)) => {
                let telegram = prepared.telegram.clone();
                let status = if telegram.is_some() {
                    TelegramStatus::Connected
                } else {
                    telegram_hooks::telegram_status_from_config(&prepared.config)
                };
                let guard = api_server::start_api_server(prepared, &registry, tui_event_tx);
                // SIGTERM-only handler: `agend-terminal stop` can cleanly exit
                // the owned app. SIGINT stays with crossterm so Ctrl+C still
                // reaches the focused pane's PTY as 0x03.
                crate::bootstrap::signals::install_term_only();
                (guard, telegram, status)
            }
            Ok(crate::bootstrap::BootstrapOutcome::Attached(attached)) => {
                tracing::info!(
                    pid = attached.daemon_pid,
                    path = %attached.run_dir.display(),
                    "attached to existing daemon, connecting as remote client"
                );
                attached_run_dir = Some(attached.run_dir.clone());
                (
                    api_server::noop_guard(),
                    None,
                    TelegramStatus::NotConfigured,
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "bootstrap failed, running TUI without in-process API");
                (
                    api_server::noop_guard(),
                    None,
                    TelegramStatus::NotConfigured,
                )
            }
        };
    let attached_mode = attached_run_dir.is_some();

    // SIGINT / SIGHUP are left to their defaults: Ctrl+C must reach the
    // focused pane's PTY as 0x03 (crossterm reads it as a KeyEvent in raw
    // mode), and SIGHUP's default "kill the process group" keeps shell-exit
    // semantics intact. SIGTERM is the only signal the app intercepts, and
    // only in the Owned branch — see `install_term_only` above.

    // Per-agent AwaitingOperator supervisor: watches for stdout silence during
    // Starting (or recently-entered Ready — some backends like codex match
    // ready_pattern against the startup banner that precedes the update menu)
    // and pushes a vterm tail to the agent's Telegram topic. In Attached mode
    // the daemon already runs its own supervisor against the real registry, so
    // the app must not also poll a disjoint (empty) registry.
    if !attached_mode {
        crate::daemon::supervisor::spawn(
            home.clone(),
            Arc::clone(&registry),
            Arc::clone(&daemon_binary_stale),
        );
        crate::instance_monitor::spawn_monitor_tick(home.clone(), Arc::clone(&registry));
        // Attached mode stays unwired: that process never owns the registry,
        // and the Telegram bot (if any) runs under the other daemon which
        // already did its own attach.
        //
        // #945 Phase 1: telegram_init is now backgrounded; `telegram_state`
        // is always None at this point post-backgrounding. Publish registry
        // to the pending slot so the background thread can attach when its
        // ~6s HTTP init completes. The if-let path below covers the eager
        // (fast/mocked init) case.
        crate::agent::set_pending_registry(Arc::clone(&registry));
        if let Some(tg) = telegram_state.as_ref() {
            tg.attach_registry(Arc::clone(&registry));
        } else if let Some(tg) = crate::channel::active_channel() {
            tg.attach_registry(Arc::clone(&registry));
        }
    }

    let mut layout = Layout::new();
    let mut key_handler = KeyHandler::new();
    let mut overlay = Overlay::None;
    let mut last_tab: usize = 0;
    let mut mouse_state = mouse::MouseState::default();
    // Counter for auto-dedup agent names
    let mut name_counter: HashMap<String, usize> = HashMap::new();

    let (wakeup_tx, wakeup_rx) = crossbeam_channel::unbounded::<usize>();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    // Remote agent roster (Attached mode). Mirrors `*.port` files the daemon
    // publishes for each live agent; periodic sync below diffs this against
    // the filesystem so hot-reload-added agents auto-materialize as tabs.
    let mut known_remote_agents: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    if let Some(ref run_dir) = attached_run_dir {
        // Attached (#895 fix): tabs derive from the union of
        //   (a) daemon's `*.port` files (live agent registry — source of truth
        //       for WHICH agents exist while daemon is alive), and
        //   (b) session.json (layout hint — source of truth for HOW the user
        //       arranged those agents in the TUI).
        //
        // Pre-#895: tabs built solely from (a) in alphabetical order; custom
        // splits/grouping were lost on every detach/reattach cycle.
        //
        // Post-#895: `restore_with_reconciliation_attached` walks session.json
        // tabs, drops leaves whose agent is not in (a) (silent — daemon drift
        // between attaches is normal), then appends agents in (a) that weren't
        // placed in session as new tabs (Rule 3, team-grouped). Falls back to
        // pre-#895 alphabetical-from-(a) when session.json is missing.
        let started = session::restore_with_reconciliation_attached(
            &home,
            &fleet_path,
            run_dir,
            &mut layout,
            &wakeup_tx,
            pane_cols,
            pane_rows,
        );
        // Populate the remote-agent roster from the placed tabs so the periodic
        // sync (lines 615-665) tracks them correctly.
        for tab in &layout.tabs {
            for name in tab.root().agent_names() {
                known_remote_agents.insert(name);
            }
        }
        if !started {
            tracing::warn!(
                "attached to daemon but no agents are reachable; check `agend-terminal list`"
            );
        }
    } else {
        // Owned: reconcile fleet.yaml (agent definitions) with session.json
        // (layout hint); fall back to a shell tab on cold start.
        let started = session::restore_with_reconciliation(
            &home,
            &fleet_path,
            &mut layout,
            &registry,
            &wakeup_tx,
            &mut name_counter,
            pane_cols,
            pane_rows,
        );
        if !started {
            pane_factory::spawn_pane_tab(
                &mut layout,
                &registry,
                &home,
                "shell",
                &std::env::var("SHELL").unwrap_or_else(|_| crate::default_shell().to_string()),
                &[],
                crate::backend::SpawnMode::Fresh,
                None,
                &HashMap::new(),
                "\r",
                pane_cols,
                pane_rows,
                &wakeup_tx,
                &mut name_counter,
            )?;
        }
    }

    // Flag to trigger resize pass after layout changes (split, close, zoom, tab switch).
    // Start true so restored split panes get correct sizes before first draw.
    let mut needs_resize = true;

    // Throttle for Attached-mode remote agent discovery. 2s is short enough
    // that a fleet.yaml reload (daemon tick is 10s) feels timely but long
    // enough that the readdir cost is trivial.
    let mut last_remote_sync = std::time::Instant::now();

    // #1479: throttled, change-gated session.json persistence. Graceful exit
    // already saves (so a hard crash kept the OLD layout); this periodically
    // persists the current layout (incl. move_pane / split / close) so a
    // kill -9 / power loss preserves what's on screen. `last_session_json`
    // caches the last write to skip no-op rewrites.
    let mut last_session_save = std::time::Instant::now();
    let mut last_session_json: Option<String> = None;

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

    // #event-bus: owned `app` mode never calls `daemon::run_core`, so it must
    // register the bus subscribers itself — otherwise the maintenance tick below
    // emits `CronFire` / `CiReady` / idle nudges into a bus with ZERO subscribers
    // and every delivery silently drops (the live #1720 cron silent-drop; same
    // regression class as #1002 / #982). Mirrors run_core; gated to owned mode
    // because the attached daemon process owns delivery when attached.
    if !attached_mode {
        crate::daemon::register_event_subscribers(&registry);
    }

    // Periodic maintenance tick (10s) — mirrors daemon tick cadence.
    // Only active in owned (non-attached) mode; when attached, the daemon
    // process handles schedules, CI watches, and health decay.
    let tick_rx = if !attached_mode {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::Builder::new()
            .name("app_tick".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                if tx.send(()).is_err() {
                    break;
                }
            })
            .ok();
        Some(rx)
    } else {
        None
    };
    // Never-ready channel used when attached_mode — select! needs a
    // concrete Receiver but this arm will never fire.
    let never_rx = crossbeam_channel::never::<()>();
    let tick_rx_ref = tick_rx.as_ref().unwrap_or(&never_rx);

    // #1726: app-standalone runs the FULL daemon per-tick pipeline (same
    // `build_default_handlers` as run_core) minus APP_TICK_ALLOWLIST, replacing
    // the old hand-picked subset that kept silently dropping handlers (#1002 /
    // #982 / #1719 class). Built once, reused each tick like run_core. App has no
    // external-agent / AgentConfig registry, so these are empty; the handlers that
    // read them (external_liveness, inbox_maintenance worktree cleanup) graceful-
    // no-op on empty. In attached mode the tick arm never fires, so these are
    // harmlessly unused.
    let app_externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let app_configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
    let app_handlers = app_tick_handlers();

    loop {
        if crate::bootstrap::signals::term_requested() {
            tracing::info!("app: SIGTERM received, exiting main loop");
            break;
        }
        // Auto-close the scratch shell overlay once its backing process
        // exits (user ran `exit`, hit Ctrl+D, or the shell crashed). The
        // 50ms `default` arm of the main `select!` below guarantees this
        // runs at least every 50ms even without new PTY output.
        if let Overlay::ScratchShell { pane } = &overlay {
            if !agent_is_alive(&registry, &pane.agent_name) {
                let name = pane.agent_name.clone();
                overlay = Overlay::None;
                kill_agent(&home, &registry, &name);
            }
        }
        if needs_resize {
            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
            let pane_area = ratatui::layout::Rect::new(0, 1, c, r.saturating_sub(2));
            crate::layout::resize_panes(pane_area, &mut layout, &registry);
            // #1140: force full redraw to clear wide-char ghost artifacts.
            // ratatui's Buffer::diff() can leave stale spacer cells when
            // wide chars are replaced by narrow chars across frames.
            let _ = terminal.clear();
            needs_resize = false;
        }
        sync_notification_state(&home, &mut layout);
        // H3: throttle flush to ≥1s intervals (was every 50ms tick → disk I/O storm).
        // std::sync::Mutex is fine here: only the main thread touches this,
        // the critical section is sub-microsecond, and the TUI render loop is
        // not async (crossbeam select!, not tokio).
        {
            static LAST_FLUSH: std::sync::Mutex<Option<std::time::Instant>> =
                std::sync::Mutex::new(None);
            let now = std::time::Instant::now();
            let should_flush = LAST_FLUSH
                .lock()
                .map(|guard| {
                    guard
                        .map(|t| now.duration_since(t).as_secs() >= 1)
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if should_flush {
                flush_idle_notifications(&home, &mut layout);
                if let Ok(mut guard) = LAST_FLUSH.lock() {
                    *guard = Some(now);
                }
            }
        }

        let repeat_mode = key_handler.in_repeat();

        terminal.draw(|frame| {
            // #1027: snapshot the shared daemon-binary-stale flag once
            // per frame so the render path sees a consistent value.
            // Relaxed is enough — single-bit flag, no fence vs other
            // state needed; the supervisor's SeqCst store will always
            // be visible to this load before the next paint tick.
            let binary_stale = daemon_binary_stale.load(std::sync::atomic::Ordering::Relaxed);
            render::render(
                frame,
                &mut layout,
                repeat_mode,
                &registry,
                telegram_status,
                binary_stale,
            );
            // &mut because ScratchShell needs to drain output and maybe
            // resize its pane's VTerm/PTY during render.
            match &mut overlay {
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
                        CloseTarget::Pane => "Close pane? (y/n)",
                        CloseTarget::Tab => "Close tab and kill all agents? (y/n)",
                    };
                    render::render_confirm(frame, msg);
                }
                Overlay::TabList { selected } => {
                    render::render_tab_list(frame, &layout, *selected);
                }
                Overlay::MovePaneTarget {
                    selected,
                    source_tab_idx,
                    split_dir,
                    ..
                } => {
                    render::render_move_pane_target(
                        frame,
                        &layout,
                        *selected,
                        *source_tab_idx,
                        *split_dir,
                    );
                }
                Overlay::Help => {
                    render::render_help(frame);
                }
                Overlay::Scroll => {
                    let so = layout
                        .active_tab()
                        .and_then(|t| t.focused_pane())
                        .map(|p| p.scroll_offset)
                        .unwrap_or(0);
                    render::render_scroll_indicator(frame, so);
                }
                Overlay::Command { ref input } => {
                    render::render_command_palette(frame, input);
                }
                Overlay::Decisions { ref items, scroll } => {
                    render::render_decisions(frame, items, *scroll);
                }
                Overlay::Tasks {
                    ref items,
                    col,
                    row,
                    ref mode,
                    ref view,
                } => {
                    render::render_tasks(frame, items, *col, *row, mode, *view, &home);
                }
                Overlay::ScratchShell { pane } => {
                    render::render_scratch_shell(frame, pane, &registry);
                }
                Overlay::None => {}
            }
        })?;

        crossbeam_channel::select! {
            recv(event_rx) -> ev => {
                let ev = match ev {
                    Ok(e) => e,
                    Err(_) => break,
                };
                match ev {
                    // Windows crossterm emits both Press and Release events for every key;
                    // Unix only emits Press. Ignoring non-Press avoids every keystroke
                    // firing the handler twice (breaks Ctrl+B prefix state, etc.).
                    Event::Key(key) if key.kind != KeyEventKind::Press => {}
                    Event::Key(key) => {
                        // Overlay input handling
                        if !matches!(overlay, Overlay::None) {
                            let mut octx = OverlayCtx {
                                layout: &mut layout,
                                registry: &registry,
                                home: &home,
                                fleet_path: &fleet_path,
                                wakeup_tx: &wakeup_tx,
                                name_counter: &mut name_counter,
                                telegram_state: &telegram_state,
                            };
                            let outcome = overlay::handle_key(&mut overlay, key, &mut octx);
                            if outcome.needs_resize {
                                needs_resize = true;
                            }
                            continue;
                        }

                        let action = key_handler.handle(key);
                        let mut dctx = dispatch::DispatchCtx {
                            layout: &mut layout,
                            registry: &registry,
                            home: &home,
                            fleet_path: &fleet_path,
                            last_tab: &mut last_tab,
                            wakeup_tx: &wakeup_tx,
                            name_counter: &mut name_counter,
                        };
                        let out = dispatch::dispatch(action, &mut dctx);
                        if out.needs_resize {
                            needs_resize = true;
                        }
                        if let Some(ov) = out.new_overlay {
                            overlay = ov;
                        }
                        if out.should_break {
                            break;
                        }
                    }
                    // Overlays (Help, Tasks, Decisions, Command palette, rename,
                    // etc.) are modal — mouse events must not reach hidden panes,
                    // otherwise drag/selection state accumulates on panes the user
                    // can't see. Swallow mouse events while any overlay is active.
                    Event::Mouse(_) if !matches!(overlay, Overlay::None) => {}
                    Event::Mouse(mouse_evt) => {
                        let out = mouse::handle(
                            mouse_evt,
                            &mut layout,
                            &mut mouse_state,
                            &fleet_path,
                            &registry,
                        );
                        if out.needs_resize {
                            needs_resize = true;
                        }
                        if let Some(prev) = out.new_last_tab {
                            last_tab = prev;
                        }
                        if let Some(ov) = out.new_overlay {
                            overlay = ov;
                        }
                    }
                    Event::Paste(text) => {
                        match &mut overlay {
                            Overlay::RenameTab { ref mut input }
                            | Overlay::RenamePane { ref mut input }
                            | Overlay::Command { ref mut input } => {
                                input.push_str(&text);
                            }
                            Overlay::ScratchShell { pane } => {
                                pane.write_input(&registry, text.as_bytes());
                            }
                            Overlay::None => {
                                write_to_focused(&home, &mut layout, &registry, text.as_bytes());
                            }
                            _ => {} // ignore paste in non-input overlays
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let pane_area = ratatui::layout::Rect::new(0, 1, cols, rows.saturating_sub(2));
                        crate::layout::resize_panes(pane_area, &mut layout, &registry);
                    }
                    _ => {}
                }
            }
            recv(wakeup_rx) -> _ => {
                // Wakeup from PTY output — triggers redraw
            }
            recv(tui_event_rx) -> ev => {
                if let Ok(event) = ev {
                    // #1257: handle screenshot request directly (needs terminal access).
                    if let TuiEvent::ScreenshotRequest(tx) = event {
                        let svg = {
                            let size = terminal.size().unwrap_or_default();
                            let backend = ratatui::backend::TestBackend::new(
                                if size.width > 0 { size.width } else { 120 },
                                if size.height > 0 { size.height } else { 40 },
                            );
                            let mut snap_term = ratatui::Terminal::new(backend).expect("TestBackend::new cannot fail");
                            let binary_stale = daemon_binary_stale.load(std::sync::atomic::Ordering::Relaxed);
                            let _ = snap_term.draw(|frame| {
                                crate::render::render(
                                    frame,
                                    &mut layout,
                                    key_handler.in_repeat(),
                                    &registry,
                                    telegram_status,
                                    binary_stale,
                                );
                                // Render overlay (same as normal draw path).
                                match &mut overlay {
                                    Overlay::NewTabMenu { items, selected }
                                    | Overlay::SplitMenu { items, selected, .. } => {
                                        crate::render::render_menu(frame, items, *selected);
                                    }
                                    Overlay::RenameTab { input } | Overlay::RenamePane { input } => {
                                        crate::render::render_rename(frame, input);
                                    }
                                    Overlay::ConfirmClose { target } => {
                                        let msg = match target {
                                            CloseTarget::Pane => "Close pane? (y/n)",
                                            CloseTarget::Tab => "Close tab and kill all agents? (y/n)",
                                        };
                                        crate::render::render_confirm(frame, msg);
                                    }
                                    Overlay::TabList { selected } => {
                                        crate::render::render_tab_list(frame, &layout, *selected);
                                    }
                                    Overlay::MovePaneTarget { selected, source_tab_idx, split_dir, .. } => {
                                        crate::render::render_move_pane_target(frame, &layout, *selected, *source_tab_idx, *split_dir);
                                    }
                                    Overlay::Help => {
                                        crate::render::render_help(frame);
                                    }
                                    Overlay::Scroll => {
                                        let so = layout.active_tab().and_then(|t| t.focused_pane()).map(|p| p.scroll_offset).unwrap_or(0);
                                        crate::render::render_scroll_indicator(frame, so);
                                    }
                                    Overlay::Command { ref input } => {
                                        crate::render::render_command_palette(frame, input);
                                    }
                                    Overlay::Decisions { ref items, scroll } => {
                                        crate::render::render_decisions(frame, items, *scroll);
                                    }
                                    Overlay::Tasks { ref items, col, row, ref mode, ref view } => {
                                        crate::render::render_tasks(frame, items, *col, *row, mode, *view, &home);
                                    }
                                    Overlay::ScratchShell { pane } => {
                                        crate::render::render_scratch_shell(frame, pane, &registry);
                                    }
                                    Overlay::None => {}
                                }
                            });
                            crate::screenshot::buffer_to_svg(snap_term.backend())
                        };
                        let _ = tx.send(svg);
                    } else {
                        tui_events::handle_tui_event(
                            event,
                            &mut layout,
                            &registry,
                            &wakeup_tx,
                        );
                    }
                    needs_resize = true;
                }
            }
            recv(tick_rx_ref) -> _ => {
                // #1726: owned-mode periodic maintenance now runs the FULL daemon
                // per-tick pipeline (build_default_handlers minus APP_TICK_ALLOWLIST)
                // instead of a hand-picked subset. Gated to owned mode via tick_rx
                // (None in attached mode). The replaced manual calls map to:
                //   cron_tick::check_schedules      → CheckSchedulesHandler
                //   ci_watch::check_ci_watches      → CiWatchPollHandler
                //   health.maybe_decay + check_hang → HangDetectionHandler
                //   core.state.tick()               → supervisor::spawn (runs in
                //     owned mode too — supervisor.rs:tick; the old manual call here
                //     was a benign idempotent double, now removed).
                let tick_ctx = crate::daemon::per_tick::TickContext {
                    home: &home,
                    registry: &registry,
                    externals: &app_externals,
                    configs: &app_configs,
                };
                crate::daemon::per_tick::run_handlers_with_panic_guard(&app_handlers, &tick_ctx);
            }
            default(std::time::Duration::from_millis(50)) => {
                // #1479: throttled, change-gated session persistence (every
                // 10s). Cheap when the layout is unchanged (no write); on a
                // change it preserves the on-screen layout against a hard
                // crash. Graceful exit still saves unconditionally below.
                if last_session_save.elapsed() >= std::time::Duration::from_secs(10) {
                    last_session_save = std::time::Instant::now();
                    session::save_session_if_changed(&home, &layout, &mut last_session_json);
                }
                // Periodic redraw for state updates. In Attached mode, also
                // poll the daemon's `*.port` directory every 2s and open a
                // tab for each newly-appeared remote agent (hot-reload
                // Phase C). Matches the daemon's add-only policy: removed
                // agents are logged but their panes stay put so the user's
                // scrollback isn't destroyed mid-session.
                if attached_run_dir.is_some()
                    && last_remote_sync.elapsed() >= std::time::Duration::from_secs(2)
                {
                    {
                        // #910 PR3 of 4: daemon-registry truth via runtime
                        // helper. The state-transition log gate inside the
                        // helper keeps this 2s-cadence call from spamming
                        // daemon.log — only Live↔Fallback transitions
                        // emit (validated by `runtime::tests::
                        // helper_steady_state_does_not_log_per_call`).
                        let current: std::collections::HashSet<String> =
                            crate::runtime::list_agents_with_fallback(&home)
                                .into_iter()
                                .collect();
                        let mut to_add: Vec<String> = current
                            .difference(&known_remote_agents)
                            .cloned()
                            .collect();
                        to_add.sort();
                        for name in &to_add {
                            let (dc, dr) = crossterm::terminal::size().unwrap_or((120, 40));
                            match pane_factory::create_remote_pane(
                                name,
                                &home,
                                &fleet_path,
                                &mut layout,
                                dc.saturating_sub(2),
                                dr.saturating_sub(4),
                                &wakeup_tx,
                            ) {
                                Ok(pane) => {
                                    let tab_name = pane.agent_name.clone();
                                    known_remote_agents.insert(tab_name.to_string());
                                    // #1591: this sync is add-only — a gone
                                    // agent's tab is RETAINED (stale output) so
                                    // scrollback survives. If a same-named agent
                                    // re-appears (recovery respawn / operator
                                    // create-after-delete churn), REUSE the
                                    // retained single-pane tab in place (the
                                    // fresh pane reconnects to the new instance;
                                    // the stale dead-connection pane is dropped)
                                    // instead of appending a DUPLICATE tab.
                                    // Preserves tab position + focus.
                                    if let Some(idx) =
                                        layout.single_pane_tab_index_for_agent(&tab_name)
                                    {
                                        layout.tabs[idx] = crate::layout::Tab::new(
                                            tab_name.to_string(),
                                            pane,
                                        );
                                        tracing::info!(
                                            agent = %name,
                                            "reused retained tab for re-appeared remote agent (no duplicate)"
                                        );
                                    } else {
                                        layout.push_tab_preserve_focus(
                                            crate::layout::Tab::new(tab_name.to_string(), pane),
                                        );
                                        tracing::info!(
                                            agent = %name,
                                            "opened tab for newly-appeared remote agent"
                                        );
                                    }
                                    needs_resize = true;
                                }
                                Err(e) => tracing::warn!(
                                    agent = %name,
                                    error = %e,
                                    "remote pane attach failed during sync",
                                ),
                            }
                        }
                        let gone: Vec<String> = known_remote_agents
                            .difference(&current)
                            .cloned()
                            .collect();
                        for name in &gone {
                            tracing::warn!(
                                agent = %name,
                                "daemon-side agent gone; pane retained with stale output",
                            );
                            known_remote_agents.remove(name);
                        }
                        last_remote_sync = std::time::Instant::now();
                    }
                }
            }
        }
    }

    // Attached mode: the daemon owns agent state + every agent's PTY.
    // `sync_fleet_yaml` STAYS gated — in Attached mode, fleet entries whose
    // remote connect happened to fail at startup would be silently deleted
    // if we touched fleet.yaml. Agent kill also STAYS gated — daemon owns
    // those PTYs.
    //
    // BUT `save_session` is now UNGATED (#895 fix). Layout state (tab
    // grouping, splits, ratios) is presentation-layer client state and
    // belongs to the app, NOT the daemon. Saving session.json in Attached
    // mode lets the next attached relaunch restore custom layout via the
    // same reconciliation path Owned mode uses (parameterized over agent
    // source: fleet.yaml for Owned, `runtime::list_agents_with_fallback`
    // for Attached — see #910 for the registry-truth migration).
    session::save_session(&home, &layout);
    if !attached_mode {
        // Sync fleet.yaml to match current state (Owned-only — daemon owns
        // fleet.yaml in Attached).
        session::sync_fleet_yaml(&home, &layout);

        // Cleanup: kill all agents (Owned-only — daemon owns PTYs in Attached).
        for tab in &layout.tabs {
            for name in tab.root().agent_names() {
                kill_agent(&home, &registry, &name);
            }
        }
    }

    Ok(())
}

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
fn build_menu_items(fleet_path: &Path, registry: &AgentRegistry) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.values().map(|h| h.name.to_string()).collect()
    };

    if let Ok(fleet) = crate::fleet::FleetConfig::load(fleet_path) {
        let mut names = fleet.instance_names();
        names.sort();
        for name in names {
            // Skip if exact name or deduped variant (name-1, name-2...) is running
            let already_open = running
                .iter()
                .any(|r| r == &name || r.starts_with(&format!("{name}-")));
            if already_open {
                continue;
            }
            let label = if let Some(resolved) = fleet.resolve_instance(&name) {
                format!("{name}  ({backend})", backend = resolved.backend_command)
            } else {
                name.clone()
            };
            items.push(MenuItem {
                label: format!("[fleet] {label}"),
                kind: MenuItemKind::FleetInstance(name),
            });
        }
    }

    for backend in Backend::all() {
        if backend.is_installed() {
            items.push(MenuItem {
                label: format!("[backend] {}", backend.name()),
                kind: MenuItemKind::Backend(backend.clone()),
            });
        }
    }

    items.push(MenuItem {
        label: "[shell] bash".to_string(),
        kind: MenuItemKind::Shell,
    });

    items
}

/// Create a pane from a menu item selection (shared by NewTab and Split handlers).
#[allow(clippy::too_many_arguments)]
fn pane_from_menu_item(
    item: MenuItem,
    fleet_path: &Path,
    layout: &mut Layout,
    registry: &AgentRegistry,
    home: &Path,
    cols: u16,
    rows: u16,
    wakeup_tx: &crossbeam_channel::Sender<usize>,
    name_counter: &mut HashMap<String, usize>,
) -> Result<Pane> {
    match item.kind {
        MenuItemKind::Shell => {
            let shell =
                std::env::var("SHELL").unwrap_or_else(|_| crate::default_shell().to_string());
            pane_factory::create_pane(
                layout,
                registry,
                home,
                "shell",
                &shell,
                &[],
                crate::backend::SpawnMode::Fresh,
                None,
                &HashMap::new(),
                "\r",
                cols,
                rows,
                wakeup_tx,
                name_counter,
            )
        }
        MenuItemKind::Backend(backend) => {
            let preset = backend.preset();
            let inst_name = pane_factory::unique_fleet_name(home, preset.command);
            // #966: TUI Backend menu (ctrl+b c) previously called
            // `add_instance_to_yaml` directly, bypassing the topic-creation
            // side effect that `handle_spawn` does. Now routes through
            // `tui_spawn::add_instance_with_topic` so the channel topic is
            // created + topic_id persisted to topics.json at TUI-spawn time.
            if let Err(e) = tui_spawn::add_instance_with_topic(
                home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend.name().to_string()),
                    working_directory: None,
                    role: None,
                    instructions: None,
                    // Sprint 54 P1-B Bug 2 fix: see instance.rs:593.
                    source_repo: None,
                    // Sprint 55 P0-B EC4: see instance.rs (gradient).
                    repo: None,
                    github_login: None,
                    args: None,
                    model: None,
                    env: None,
                    ready_pattern: None,
                    command: None,
                    worktree: None,
                    topic_binding_mode: None,
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml");
            }
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
            if let Some(resolved) = fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name)) {
                pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    layout,
                    registry,
                    home,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                    crate::backend::SpawnMode::Fresh,
                )
            } else {
                // Preset args are added by spawn_agent; no need to compose here.
                pane_factory::create_pane(
                    layout,
                    registry,
                    home,
                    &inst_name,
                    preset.command,
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    preset.submit_key,
                    cols,
                    rows,
                    wakeup_tx,
                    name_counter,
                )
            }
        }
        MenuItemKind::FleetInstance(inst_name) => {
            let fleet = crate::fleet::FleetConfig::load(fleet_path)?;
            let resolved = fleet
                .resolve_instance(&inst_name)
                .ok_or_else(|| anyhow::anyhow!("fleet instance '{inst_name}' not found"))?;
            pane_factory::create_pane_from_resolved(
                &inst_name,
                &resolved,
                layout,
                registry,
                home,
                cols,
                rows,
                wakeup_tx,
                name_counter,
                crate::backend::SpawnMode::Resume,
            )
        }
    }
}

/// Write bytes to the focused pane's PTY (Local) or remote bridge (Remote).
fn write_to_focused(home: &Path, layout: &mut Layout, registry: &AgentRegistry, bytes: &[u8]) {
    if let Some(pane) = layout.active_tab_mut().and_then(|t| t.focused_pane_mut()) {
        notification_queue::record_input_activity(home, &pane.agent_name);
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
        notification_queue::record_input_activity(home, &pane.agent_name);
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
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            if let Some(pane) = tab.root_mut().find_pane_mut(pane_id) {
                pane.pending_notification_count =
                    notification_queue::pending_count(home, &pane.agent_name);
            }
        }
    }
}

fn flush_idle_notifications(home: &Path, layout: &mut Layout) {
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            let Some(pane) = tab.root_mut().find_pane_mut(pane_id) else {
                continue;
            };
            let agent_name = pane.agent_name.clone();
            flush_notifications_for_pane(home, pane, |text| {
                // #982 RC: queue contents come from compose_aware_*
                // which would have submit-injected on the immediate-
                // idle path. The flush must preserve that contract or
                // queued hints (e.g. `[AGEND-MSG-PENDING]`) land in
                // the prompt buffer without the backend submit_key —
                // codex one-shots silently drop the wake.
                crate::inbox::inject_notification_with_submit(home, &agent_name, text)
            });
        }
    }
}

fn flush_notifications_for_pane<F>(home: &Path, pane: &mut Pane, mut injector: F)
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    if pane.pending_notification_count == 0 {
        return;
    }
    // #1457: gate on draft state (input-vs-submit order), not the 3s idle
    // window. Drafting → defer everything; Abandoned → escape valve releases
    // just the oldest (trickle, no clobbering batch); None (clean buffer) →
    // drain the whole backlog.
    match notification_queue::draft_state(home, &pane.agent_name) {
        notification_queue::DraftState::Drafting => {}
        notification_queue::DraftState::Abandoned => {
            if let Some(notification) = notification_queue::drain_one(home, &pane.agent_name) {
                if injector(&notification.text).is_err() {
                    notification_queue::requeue_all(home, &pane.agent_name, &[notification]);
                }
            }
            pane.pending_notification_count =
                notification_queue::pending_count(home, &pane.agent_name);
        }
        notification_queue::DraftState::None => {
            let mut queued = notification_queue::drain(home, &pane.agent_name);
            if queued.is_empty() {
                pane.pending_notification_count = 0;
                return;
            }
            // #1513: actionable wakes drain FIRST, then ambient (stable by ts).
            queued.sort_by(|a, b| {
                b.actionable
                    .cmp(&a.actionable)
                    .then_with(|| a.timestamp.cmp(&b.timestamp))
            });
            // #1513: if the agent is mid-generation (Thinking/ToolUse), injecting
            // now would corrupt the PTY stream — HOLD non-expired items and only
            // release those past their MAX_DEFER cap (anti-starvation). The state
            // read is lock-free from the snapshot (inject path must not take the
            // core lock — #1492). Once any inject fails, preserve the remaining
            // order by holding the rest.
            let agent_busy = crate::snapshot::agent_is_busy(home, &pane.agent_name);
            // #1513 case A: same live-keystroke signal the inject-time gate uses,
            // so a queued item is held off the operator's input line at the drain
            // too (bounded by the MAX_DEFER cap below).
            let typing_recent =
                crate::inbox::notify::operator_typing_recent(home, &pane.agent_name);
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mut keep: Vec<notification_queue::QueuedNotification> = Vec::new();
            let mut inject_failed = false;
            for notification in queued {
                if inject_failed || !flush_release(&notification, agent_busy, typing_recent, now_ms)
                {
                    keep.push(notification);
                } else if injector(&notification.text).is_err() {
                    inject_failed = true;
                    keep.push(notification);
                }
            }
            if !keep.is_empty() {
                notification_queue::requeue_all(home, &pane.agent_name, &keep);
            }
            pane.pending_notification_count =
                notification_queue::pending_count(home, &pane.agent_name);
        }
    }
}

/// #1513: MAX_DEFER anti-starvation caps — once an item has been deferred this
/// long it is released even while the agent is still busy. Actionable wakes get
/// a tight cap (work delivery must land fast); ambient can wait longer.
const ACTIONABLE_MAX_DEFER_MS: i64 = 1_000;
const AMBIENT_MAX_DEFER_MS: i64 = 7_000;

/// #1513: should this queued item be RELEASED (injected) now vs HELD? Released
/// when the pane is SETTLED — the agent is not mid-generation AND the operator
/// isn't mid-keystroke — OR the item is past its MAX_DEFER cap.
///
/// #1513 case A: the drain previously held only on `agent_busy`, so the
/// anti-starvation release could land an inject on the operator's input line
/// mid-typing. `typing_recent` (the SAME `operator_typing_recent` live-keystroke
/// signal the inject-time gate uses — one source of truth) now also holds. The
/// MAX_DEFER cap stays the backstop: a perpetually-busy OR perpetually-typing
/// operator never traps the queue (actionable work still lands within
/// `ACTIONABLE_MAX_DEFER_MS`). Pure so the hold/release matrix is unit-testable.
fn flush_release(
    item: &notification_queue::QueuedNotification,
    agent_busy: bool,
    typing_recent: bool,
    now_ms: i64,
) -> bool {
    let cap = if item.actionable {
        ACTIONABLE_MAX_DEFER_MS
    } else {
        AMBIENT_MAX_DEFER_MS
    };
    if now_ms.saturating_sub(item.deferred_since_ms) >= cap {
        return true; // MAX_DEFER backstop wins, even mid-generation / mid-keystroke.
    }
    !agent_busy && !typing_recent
}

#[cfg(test)]
mod flush_release_tests_1513 {
    use super::{flush_release, ACTIONABLE_MAX_DEFER_MS};
    use crate::notification_queue::QueuedNotification;

    fn item(actionable: bool, deferred_since_ms: i64) -> QueuedNotification {
        QueuedNotification {
            text: "x".into(),
            timestamp: String::new(),
            actionable,
            deferred_since_ms,
        }
    }

    #[test]
    fn not_busy_not_typing_always_releases() {
        let now = 10_000;
        assert!(
            flush_release(&item(true, now), false, false, now),
            "settled agent releases actionable"
        );
        assert!(
            flush_release(&item(false, now), false, false, now),
            "settled agent releases ambient"
        );
    }

    #[test]
    fn busy_holds_then_cap_releases() {
        let base = 100_000;
        // fresh defer while busy (not typing) → held
        assert!(
            !flush_release(&item(true, base), true, false, base),
            "busy holds fresh actionable"
        );
        assert!(
            !flush_release(&item(false, base), true, false, base),
            "busy holds fresh ambient"
        );
        // past the actionable cap (1s) but within ambient cap (7s) → actionable releases, ambient holds
        let mid = base + ACTIONABLE_MAX_DEFER_MS + 1;
        assert!(
            flush_release(&item(true, base), true, false, mid),
            "actionable releases past its 1s cap even while busy"
        );
        assert!(
            !flush_release(&item(false, base), true, false, mid),
            "ambient still held at ~1s while busy"
        );
        // well past ambient cap → ambient releases too
        let late = base + 8_000;
        assert!(
            flush_release(&item(false, base), true, true, late),
            "ambient releases past its cap even while typing (backstop)"
        );
    }

    /// #1513 case A: the operator-typing hold + its MAX_DEFER backstop, across
    /// the four scenarios in the fix spec.
    #[test]
    fn typing_holds_until_cap() {
        let base = 100_000;
        // (1) busy + actionable + typing + NOT past cap → defer (no collision).
        assert!(
            !flush_release(&item(true, base), true, true, base),
            "typing holds a fresh actionable wake off the input line"
        );
        // also holds when the agent is idle but the operator is mid-keystroke —
        // the case the old `!agent_busy` early-return missed.
        assert!(
            !flush_release(&item(true, base), false, true, base),
            "typing holds even when the agent is idle"
        );
        // (2) same but PAST MAX_DEFER → release (backstop; task dispatch 1s).
        let past = base + ACTIONABLE_MAX_DEFER_MS + 1;
        assert!(
            flush_release(&item(true, base), true, true, past),
            "actionable releases past its 1s cap despite typing"
        );
        // (3) NOT typing → unchanged from before (release when settled).
        assert!(
            flush_release(&item(true, base), false, false, base),
            "not typing + idle → release as before"
        );
        assert!(
            !flush_release(&item(true, base), true, false, base),
            "not typing + busy + fresh → held as before"
        );
        // (4) ambient honors typing too, bounded by its 7s cap.
        assert!(
            !flush_release(&item(false, base), false, true, base + 6_000),
            "ambient held while typing within its cap"
        );
        assert!(
            flush_release(&item(false, base), false, true, base + 7_001),
            "ambient releases past its 7s cap despite typing"
        );
    }
}

/// Adjust scroll offset of the focused pane by `delta` lines (positive = up, negative = down).
fn scroll_focused(layout: &mut Layout, delta: i32) {
    if let Some(tab) = layout.active_tab_mut() {
        let fid = tab.focus_id;
        if let Some(pane) = tab.root_mut().find_pane_mut(fid) {
            let max = pane.vterm.max_scroll();
            if delta > 0 {
                pane.scroll_offset = (pane.scroll_offset + delta as usize).min(max);
            } else {
                pane.scroll_offset = pane.scroll_offset.saturating_sub((-delta) as usize);
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
    crate::daemon::lifecycle::delete_transaction(home, name, registry, None, false);
}

/// Whether the agent's child process is still running.
///
/// Used by the scratch shell overlay to self-close when the user exits the
/// shell naturally (`exit`, Ctrl+D) or the process crashes. Returns `false`
/// if the name is no longer registered (already reaped) or `try_wait`
/// reports the child has exited. A poisoned child mutex is treated as alive
/// so a spurious poison doesn't auto-dismiss the overlay — Esc still works.
fn agent_is_alive(registry: &AgentRegistry, name: &str) -> bool {
    let reg = agent::lock_registry(registry);
    // #1441: registry is UUID-keyed; the overlay only knows the display name,
    // so locate the handle by name (no fleet.yaml on the scratch-shell path).
    let Some(handle) = reg.values().find(|h| h.name.as_str() == name) else {
        return false;
    };
    // Bind to a local so the child-lock's temporary MutexGuard drops
    // before `reg` does — returning the match expression directly trips
    // the borrow checker because temporaries outlive the registry lock.
    let alive = {
        let mut child = handle.child.lock();
        !matches!(child.try_wait(), Ok(Some(_)))
    };
    alive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneSource;
    use crate::vterm::VTerm;

    /// #1457 regression guard: submit detection must fire for ALL backends, not
    /// just claude. If this regresses to claude-only, non-claude panes never
    /// record a submit timestamp → `draft_state` sees `submit=0` → every
    /// keystroke looks like a permanent unsent draft → notifications NEVER
    /// deliver to them (strictly worse than the bug #1457 fixes).
    #[test]
    fn submit_detection_fires_for_all_backends() {
        use crate::backend::Backend;
        for b in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::Gemini,
            Backend::KiroCli,
            Backend::OpenCode,
            Backend::Agy,
        ] {
            assert!(
                pane_input_contains_submit(Some(&b), b"hello\r"),
                "submit key must be detected for {b:?}"
            );
            assert!(
                !pane_input_contains_submit(Some(&b), b"hello"),
                "no submit key in plain text for {b:?}"
            );
        }
        // No backend → never a submit (anonymous/unknown pane).
        assert!(!pane_input_contains_submit(None, b"hello\r"));
    }

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-app-phase2-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #1726 completeness invariant — the guard that closes the recurring
    /// #1002 / #982 / #1719 "app silently drops a handler" class. Compares two
    /// independently-built `name()` sets (the full daemon pipeline vs app's actual
    /// run set), so it has teeth and is not a tautology: a NEW handler added to
    /// `build_default_handlers` lands in `all`, and unless app runs it OR it is
    /// allowlisted, `missing` is non-empty → CI red.
    #[test]
    fn app_tick_handlers_cover_every_non_allowlisted_daemon_handler() {
        use std::collections::HashSet;
        let (crash_tx, _rx) = crossbeam_channel::bounded(1);
        let all: HashSet<&str> = crate::daemon::build_default_handlers(crash_tx)
            .iter()
            .map(|h| h.name())
            .collect();
        let app: HashSet<&str> = app_tick_handlers().iter().map(|h| h.name()).collect();

        // Positive: every non-allowlisted daemon handler must run in app.
        let missing: Vec<&str> = all
            .difference(&app)
            .filter(|n| !APP_TICK_ALLOWLIST.contains(n))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "app-standalone must run these per_tick handlers (or add to APP_TICK_ALLOWLIST \
             with a justification): {missing:?}"
        );
        // Negative probe: no stale allowlist entry — every allowlisted name must
        // still exist in the daemon set (catches a renamed/removed handler).
        for a in APP_TICK_ALLOWLIST {
            assert!(
                all.contains(a),
                "stale APP_TICK_ALLOWLIST entry '{a}' — handler renamed or removed?"
            );
            assert!(
                !app.contains(a),
                "allowlisted handler '{a}' must NOT run in app-standalone"
            );
        }
    }

    /// #1726 must-verify: app-standalone runs these handlers with EMPTY
    /// externals/configs (it has no external-agent / AgentConfig registry) and a
    /// possibly-empty registry. None may panic — `run_handlers` has catch_unwind,
    /// but we want a clean no-op degrade, so we call each `run()` directly (no
    /// catch) and a panic fails the test.
    #[test]
    fn app_tick_handlers_no_panic_on_empty_context() {
        let home = tmp_home("tick-empty-ctx");
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
        let ctx = crate::daemon::per_tick::TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        for h in app_tick_handlers() {
            h.run(&ctx); // panic here = test failure
        }
        std::fs::remove_dir_all(&home).ok();
    }

    fn pane(name: &str) -> Pane {
        Pane {
            agent_name: name.into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
        }
    }

    /// #982 RC wiring-pin: assert `flush_idle_notifications` invokes
    /// the submit-aware injector (`inject_notification_with_submit`)
    /// so queued hints get the backend `submit_key` applied on flush.
    ///
    /// Implemented as a file-level source pin — the raw
    /// `inject_notification` was deleted in this PR, so the negative
    /// half of the invariant is compile-time enforced. The positive
    /// half (this assertion) is platform-agnostic and survives
    /// rustfmt re-wrapping. Companion test:
    /// `inbox::tests::t15_composing_flush_uses_submit_aware_inject`
    /// pins the JSON payload contract end-to-end.
    #[test]
    fn flush_idle_notifications_wired_to_submit_aware_inject() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        assert!(
            source.contains("inject_notification_with_submit"),
            "flush_idle_notifications must wire the submit-aware injector \
             (#982 reviewer #999 verdict) — searched for \
             'inject_notification_with_submit' in src/app/mod.rs"
        );
    }

    /// app-mode subscriber-wiring source pin. Owned `agend-terminal app` mode
    /// never calls `daemon::run_core`, so `run_app` MUST itself register the
    /// event-bus subscribers — otherwise the maintenance tick emits `CronFire` /
    /// `CiReady` / idle nudges into an empty bus and every delivery silently
    /// drops (the live #1720 cron silent-drop; regression class #1002 / #982).
    ///
    /// File-level positive pin (cross-platform-safe; survives rustfmt re-wrap),
    /// same pattern as `flush_idle_notifications_wired_to_submit_aware_inject`.
    /// The functional counterpart —
    /// `cron_tick::tests::global_bus_cron_subscriber_delivers` — proves the
    /// registered set actually delivers a CronFire on the process-global bus.
    #[test]
    fn run_app_registers_event_bus_subscribers() {
        let source = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("source file must be readable from test cwd");
        assert!(
            source.contains("register_event_subscribers"),
            "run_app must call daemon::register_event_subscribers in owned mode \
             (app mode never reaches run_core's registration — #1720 app-mode \
             silent-drop root fix). Searched for 'register_event_subscribers' in \
             src/app/mod.rs"
        );
    }

    #[test]
    fn flush_drains_queue_on_idle() {
        let home = tmp_home("flush");
        let mut pane = pane("agent1");
        notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
        pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");
        let mut flushed = Vec::new();
        flush_notifications_for_pane(&home, &mut pane, |text| {
            flushed.push(text.to_string());
            Ok(())
        });
        assert_eq!(flushed, vec!["queued".to_string()]);
        assert_eq!(pane.pending_notification_count, 0);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn flush_respects_disk_compose_state_for_fresh_pane() {
        let home = tmp_home("flush-compose-disk");
        let mut pane = pane("agent1");
        notification_queue::record_input_activity(&home, "agent1");
        notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
        pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");

        let mut flushed = Vec::new();
        flush_notifications_for_pane(&home, &mut pane, |text| {
            flushed.push(text.to_string());
            Ok(())
        });

        assert!(
            flushed.is_empty(),
            "fresh pane must respect disk compose state"
        );
        assert_eq!(pane.pending_notification_count, 1);
        std::fs::remove_dir_all(home).ok();
    }

    // -----------------------------------------------------------------------
    // Regression pins: app mode tick consumers (t-20260423022134)
    // -----------------------------------------------------------------------

    #[test]
    fn app_mode_fires_one_shot_schedule() {
        // Write a one-shot schedule with past run_at directly to disk,
        // call check_schedules, verify it fires (auto-disabled).
        let home = tmp_home("sched-fire");
        let past = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
        let store_json = serde_json::json!({
            "schema_version": 2,
            "schedules": [{
                "id": "s-test-oneshot",
                "message": "ping",
                "target": "nonexistent-agent",
                "trigger": {"kind": "once", "at": past},
                "enabled": true,
                "timezone": "UTC",
                "label": "test-oneshot",
                "created_at": chrono::Utc::now().to_rfc3339(),
                "updated_at": chrono::Utc::now().to_rfc3339(),
                "run_history": []
            }]
        });
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::write(
            home.join("schedules.json"),
            serde_json::to_string_pretty(&store_json).expect("serialize"),
        )
        .expect("write schedule file");

        // Fire the tick — schedule is past due, should trigger.
        crate::daemon::cron_tick::check_schedules(&home);

        // Verify: schedule should now be disabled (one-shot auto-disable).
        let store = crate::schedules::load(&home);
        let sched = store.schedules.iter().find(|s| s.id == "s-test-oneshot");
        assert!(
            sched.is_some_and(|s| !s.enabled),
            "one-shot schedule must be auto-disabled after firing"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn app_mode_health_decay_runs() {
        // Verify health.maybe_decay() is callable on an agent handle —
        // binding test that the tick consumer code path compiles and
        // exercises the health decay method.
        use crate::health::HealthTracker;
        let mut health = HealthTracker::new();
        // maybe_decay on a fresh tracker should not panic or change state.
        health.maybe_decay();
        assert_eq!(
            health.state.display_name(),
            "healthy",
            "fresh tracker should remain healthy after decay tick"
        );
    }
}
