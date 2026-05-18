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

use anyhow::{Context, Result};
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

    let log_path = home.join("app.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .ok();
    if let Some(file) = log_file {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("agend_terminal=debug")
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_target(false)
            .try_init();
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
    let (tui_event_tx, tui_event_rx) = crossbeam_channel::bounded::<TuiEvent>(256);
    // #879v3 C2: previously fed by `api_server::start_api_server`'s
    // TuiNotifier; now the daemon owns the API + telegram outbox, so the
    // TUI has no in-process event producer. The sender is retained for the
    // future case of daemon→TUI push events landing here.
    let _ = tui_event_tx.clone();

    // #879v3 C2: always-Attached architecture. The TUI no longer holds the
    // daemon lock in-process; instead it requires a detached daemon to be
    // reachable and connects as a client (so MCP, supervisor, telegram,
    // session persistence all run in the daemon — single source of truth).
    //
    // If no daemon is running on `home`, auto-spawn one via the same
    // `bootstrap::daemon_spawn::canonical_spawn_daemon` topology the manual
    // `agend-terminal start` uses; then poll `find_active_run_dir` AND
    // `probe_api` until both pass (the dual gate that the #882 reattempt
    // missed). On readiness-probe failure, `cleanup_on_bail` SIGTERMs the
    // orphan + wipes the run_dir before propagating Err to the operator —
    // closes the new orphan-daemon class that always-Attached introduces.
    let attached_run_dir = ensure_daemon_running(&home, &fleet_path)?;
    tracing::info!(
        path = %attached_run_dir.display(),
        "attached to daemon (auto-spawned if cold)"
    );
    let _api_guard = api_server::noop_guard();
    let telegram_state: Option<std::sync::Arc<dyn crate::channel::Channel>> = None;
    let telegram_status = TelegramStatus::NotConfigured;

    // SIGINT / SIGHUP are left to their defaults: Ctrl+C must reach the
    // focused pane's PTY as 0x03 (crossterm reads it as a KeyEvent in raw
    // mode), and SIGHUP's default "kill the process group" keeps shell-exit
    // semantics intact. The daemon owns its own signal handling now; the
    // TUI client just exits on Ctrl+C from outside any focused pane.

    // #879v3 C3: supervisor / instance_monitor / telegram-attach all run in
    // the daemon process (daemon::run_with_prepared), not in this TUI
    // client. The pre-PR2 `if !attached_mode { ... }` block that double-
    // spawned them against an empty in-process registry is removed.

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

    // #879v3 C3: always-Attached — tabs derive from the union of
    //   (a) daemon's `*.port` files (live agent registry — source of truth
    //       for WHICH agents exist while daemon is alive), and
    //   (b) session.json (layout hint — source of truth for HOW the user
    //       arranged those agents in the TUI).
    // The pre-PR2 Owned branch (fleet.yaml + in-process spawn) is dropped;
    // when no daemon was reachable we now auto-spawned one above and are
    // guaranteed to be in the attached path. `name_counter` becomes dead
    // here too — left as a `let _` to keep the binding-shape stable for
    // future hot-reload Phase D fold-in.
    let _ = name_counter;
    let started = session::restore_with_reconciliation_attached(
        &home,
        &fleet_path,
        &attached_run_dir,
        &mut layout,
        &wakeup_tx,
        pane_cols,
        pane_rows,
    );
    // Populate the remote-agent roster from the placed tabs so the periodic
    // sync (lines below) tracks them correctly.
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

    // Flag to trigger resize pass after layout changes (split, close, zoom, tab switch).
    // Start true so restored split panes get correct sizes before first draw.
    let mut needs_resize = true;

    // Throttle for Attached-mode remote agent discovery. 2s is short enough
    // that a fleet.yaml reload (daemon tick is 10s) feels timely but long
    // enough that the readdir cost is trivial.
    let mut last_remote_sync = std::time::Instant::now();

    // Crossterm event reader thread
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

    // #879v3 C3: in always-Attached the daemon process owns schedules, CI
    // watches, and health-decay ticks. The TUI client used to run an
    // `app_tick` thread when Owned; that's redundant work against an empty
    // in-process registry now. `select!` keeps a never-ready receiver in
    // the slot so the loop's match arms stay structurally stable.
    let never_rx = crossbeam_channel::never::<()>();
    let tick_rx_ref = &never_rx;

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
            needs_resize = false;
        }
        sync_notification_state(&home, &mut layout);
        // H3: throttle flush to ≥1s intervals (was every 50ms tick → disk I/O storm)
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
            render::render(frame, &mut layout, repeat_mode, &registry, telegram_status);
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
                    tui_events::handle_tui_event(
                        event,
                        &mut layout,
                        &registry,
                        &wakeup_tx,
                    );
                    needs_resize = true;
                }
            }
            recv(tick_rx_ref) -> _ => {
                // Periodic maintenance — mirrors daemon tick consumers.
                // Gated to owned mode via tick_rx (None in attached mode).
                crate::daemon::cron_tick::check_schedules(&home, &registry);
                crate::daemon::ci_watch::check_ci_watches(&home, &registry);
                {
                    let reg = crate::agent::lock_registry(&registry);
                    for (name, handle) in reg.iter() {
                        {
                            let mut core = handle.core.lock();
                            core.health.maybe_decay();
                            core.state.tick();
                            let agent_state = core.state.current;
                            let silent = core.state.last_output.elapsed();
                            let silent_productive =
                                core.state.last_productive_output.elapsed();
                            // Sprint 24 P1: pair snapshot for input-aware
                            // hang discrimination (matches daemon/mod.rs
                            // pattern).
                            let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
                            core.health.check_hang(
                                agent_state,
                                silent,
                                silent_productive,
                                pair.last_input_at_ms,
                                pair.heartbeat_at_ms,
                            );
                        }
                    }
                }
            }
            default(std::time::Duration::from_millis(50)) => {
                // Periodic redraw for state updates. In Attached mode, also
                // poll the daemon's `*.port` directory every 2s and open a
                // tab for each newly-appeared remote agent (hot-reload
                // Phase C). Matches the daemon's add-only policy: removed
                // agents are logged but their panes stay put so the user's
                // scrollback isn't destroyed mid-session.
                if last_remote_sync.elapsed() >= std::time::Duration::from_secs(2) {
                    let run_dir = &attached_run_dir;
                    let current: std::collections::HashSet<String> =
                        crate::ipc::list_agent_ports(run_dir).into_iter().collect();
                    let mut to_add: Vec<String> =
                        current.difference(&known_remote_agents).cloned().collect();
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
                                known_remote_agents.insert(tab_name.clone());
                                layout.push_tab_preserve_focus(crate::layout::Tab::new(
                                    tab_name, pane,
                                ));
                                needs_resize = true;
                                tracing::info!(
                                    agent = %name,
                                    "opened tab for newly-appeared remote agent"
                                );
                            }
                            Err(e) => tracing::warn!(
                                agent = %name,
                                error = %e,
                                "remote pane attach failed during sync",
                            ),
                        }
                    }
                    let gone: Vec<String> =
                        known_remote_agents.difference(&current).cloned().collect();
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
    // source: fleet.yaml for Owned, `list_agent_ports` for Attached).
    session::save_session(&home, &layout);
    // #879v3 C3: fleet.yaml sync + agent kill on exit run in the daemon,
    // not in this TUI client. The pre-PR2 Owned-only branch here would
    // double-act against the daemon's source of truth.

    Ok(())
}

/// Maximum time `ensure_daemon_running` polls for the auto-spawned daemon to
/// become probe-API ready. Generous compared to `STARTUP_TIMEOUT` inside
/// `spawn_detached` (5s) — the cold-start window covers flock acquire, cookie
/// issue, fleet load, AND the api.port bind that `spawn_detached`'s 5s budget
/// alone doesn't guarantee. The dual gate (`find_active_run_dir` AND
/// `probe_api`) is the lesson #882 missed.
const ENSURE_DAEMON_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Always-Attached entry point (#879v3 C2). Returns the run_dir of a live,
/// probe-API-ready daemon under `home`, auto-spawning one via the canonical
/// detached-daemon path if none is reachable.
///
/// Error paths invoke `cleanup_on_bail(Some(pid), Some(run_dir))` so an
/// orphan daemon left behind by a spawn-then-fail-to-probe sequence is
/// terminated + its rundir wiped — closes the new orphan-daemon class that
/// always-Attached introduces (the spawned-but-unattached daemon would
/// otherwise stick around and confuse the next launch).
fn ensure_daemon_running(home: &Path, fleet_path: &Path) -> Result<PathBuf> {
    // Fast path: already running and ready.
    if let Some(rd) = crate::daemon::find_active_run_dir(home) {
        if crate::ipc::probe_api(&rd) {
            tracing::info!(path = %rd.display(), "daemon already running");
            return Ok(rd);
        }
    }
    // Cold path: auto-spawn detached + wait for ready.
    tracing::info!("no live daemon on this AGEND_HOME; auto-spawning detached daemon");
    let handle = crate::bootstrap::daemon_spawn::spawn_detached(
        home,
        fleet_path.exists().then_some(fleet_path),
    )
    .context(
        "auto-spawn detached daemon failed — \
         check daemon.log under AGEND_HOME for the underlying cause",
    )?;
    tracing::info!(
        pid = handle.pid,
        run_dir = %handle.run_dir.display(),
        log = %handle.log_path.display(),
        "daemon auto-spawned; waiting for API readiness"
    );
    // Dual gate: find_active_run_dir + probe_api. spawn_detached's internal
    // wait only confirms the run dir is published; api.port bind happens
    // moments later. #881 race was here.
    if let Some(rd) =
        crate::bootstrap::daemon_spawn::wait_until_ready(home, ENSURE_DAEMON_READY_TIMEOUT)
    {
        return Ok(rd);
    }
    tracing::error!(
        pid = handle.pid,
        run_dir = %handle.run_dir.display(),
        timeout_secs = ENSURE_DAEMON_READY_TIMEOUT.as_secs(),
        "auto-spawned daemon did not become probe-API ready within timeout; cleaning up"
    );
    crate::bootstrap::daemon_spawn::cleanup_on_bail(Some(handle.pid), Some(&handle.run_dir));
    anyhow::bail!(
        "auto-spawned daemon (pid={}) did not become API-ready within {}s — \
         see {} for details. cleanup_on_bail SIGTERMed the daemon and removed the run dir.",
        handle.pid,
        ENSURE_DAEMON_READY_TIMEOUT.as_secs(),
        handle.log_path.display(),
    );
}

/// Build menu items for new-tab selection.
/// Fleet instances already running in the registry are excluded.
fn build_menu_items(fleet_path: &Path, registry: &AgentRegistry) -> Vec<MenuItem> {
    let mut items = Vec::new();

    // Collect already-running agent names
    let running: Vec<String> = {
        let reg = agent::lock_registry(registry);
        reg.keys().cloned().collect()
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
            if let Err(e) = crate::fleet::add_instance_to_yaml(
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
    if !matches!(b, crate::backend::Backend::ClaudeCode) {
        return false;
    }
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
                crate::inbox::inject_notification(home, &agent_name, text)
            });
        }
    }
}

fn flush_notifications_for_pane<F>(home: &Path, pane: &mut Pane, mut injector: F)
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    if pane.pending_notification_count == 0
        || notification_queue::is_composing(home, &pane.agent_name)
    {
        return;
    }
    let queued = notification_queue::drain(home, &pane.agent_name);
    if queued.is_empty() {
        pane.pending_notification_count = 0;
        return;
    }

    let mut failed_at = None;
    for (idx, notification) in queued.iter().enumerate() {
        if injector(&notification.text).is_err() {
            failed_at = Some(idx);
            break;
        }
    }
    if let Some(idx) = failed_at {
        notification_queue::requeue_all(home, &pane.agent_name, &queued[idx..]);
    }
    pane.pending_notification_count = notification_queue::pending_count(home, &pane.agent_name);
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
    crate::daemon::lifecycle::delete_transaction(home, name, registry, None);
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
    let Some(handle) = reg.get(name) else {
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

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-app-phase2-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn pane(name: &str) -> Pane {
        Pane {
            agent_name: name.to_string(),
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
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
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
        crate::daemon::cron_tick::check_schedules(&home, &registry);

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
