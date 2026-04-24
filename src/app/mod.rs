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

pub use overlay::{MenuItem, MenuItemKind, TaskBoardMode};
pub(crate) use tui_events::{TuiEvent, TuiEventSender, TuiNotifier};

use crate::agent::{self, AgentRegistry};
use crate::backend::Backend;
use crate::keybinds::KeyHandler;
use crate::layout::{Layout, Pane};
use crate::notification_queue;
use crate::render;
use overlay::{CloseTarget, Overlay, OverlayCtx};

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
            .with_writer(Mutex::new(file))
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

    let result = run_app(&mut terminal, fleet_path.as_deref());

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

fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();
    let fleet_path = fleet_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join("fleet.yaml"));

    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (tui_event_tx, tui_event_rx) = crossbeam::channel::bounded::<TuiEvent>(256);

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
                    render::TelegramStatus::Connected
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
                    render::TelegramStatus::NotConfigured,
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "bootstrap failed, running TUI without in-process API");
                (
                    api_server::noop_guard(),
                    None,
                    render::TelegramStatus::NotConfigured,
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
        crate::daemon::supervisor::spawn(home.clone(), Arc::clone(&registry));
        // Attached mode stays unwired: that process never owns the registry,
        // and the Telegram bot (if any) runs under the other daemon which
        // already did its own attach.
        if let Some(tg) = telegram_state.as_ref() {
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

    let (wakeup_tx, wakeup_rx) = crossbeam::channel::unbounded::<usize>();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    // Remote agent roster (Attached mode). Mirrors `*.port` files the daemon
    // publishes for each live agent; periodic sync below diffs this against
    // the filesystem so hot-reload-added agents auto-materialize as tabs.
    let mut known_remote_agents: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    if let Some(ref run_dir) = attached_run_dir {
        // Attached: one tab per agent the daemon is serving. Tabs derive from
        // the daemon's `*.port` files (the same list `agend-terminal list`
        // reads), not from session.json — the daemon is the source of truth
        // for which agents exist while it's alive. Connect failures are logged
        // and the corresponding tab is dropped; the app still starts.
        let mut names = crate::ipc::list_agent_ports(run_dir);
        names.sort();
        for name in &names {
            match pane_factory::create_remote_pane(
                name,
                &home,
                &fleet_path,
                &mut layout,
                pane_cols,
                pane_rows,
                &wakeup_tx,
            ) {
                Ok(pane) => {
                    let tab_name = pane.agent_name.clone();
                    known_remote_agents.insert(tab_name.clone());
                    layout.add_tab(crate::layout::Tab::new(tab_name, pane));
                }
                Err(e) => tracing::warn!(agent = %name, error = %e, "remote pane attach failed"),
            }
        }
        if layout.tabs.is_empty() {
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

    // Crossterm event reader thread
    let (event_tx, event_rx) = crossbeam::channel::unbounded::<Event>();
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

    // Periodic maintenance tick (10s) — mirrors daemon tick cadence.
    // Only active in owned (non-attached) mode; when attached, the daemon
    // process handles schedules, CI watches, and health decay.
    let tick_rx = if !attached_mode {
        let (tx, rx) = crossbeam::channel::bounded(1);
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
    let never_rx = crossbeam::channel::never::<()>();
    let tick_rx_ref = tick_rx.as_ref().unwrap_or(&never_rx);

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
                kill_agent(&registry, &name);
            }
        }
        if needs_resize {
            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
            let pane_area = ratatui::layout::Rect::new(0, 1, c, r.saturating_sub(2));
            render::resize_panes(pane_area, &mut layout, &registry);
            needs_resize = false;
        }
        sync_notification_state(&home, &mut layout);
        flush_idle_notifications(&home, &mut layout);

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
                } => {
                    render::render_tasks(frame, items, *col, *row, mode);
                }
                Overlay::ScratchShell { pane } => {
                    render::render_scratch_shell(frame, pane, &registry);
                }
                Overlay::None => {}
            }
        })?;

        crossbeam::select! {
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
                        render::resize_panes(pane_area, &mut layout, &registry);
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
                    for (_name, handle) in reg.iter() {
                        if let Ok(mut core) = handle.core.lock() {
                            core.health.maybe_decay();
                            core.state.tick();
                            let agent_state = core.state.current;
                            let silent = core.state.last_output.elapsed();
                            core.health.check_hang(agent_state, silent);
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
                if let Some(ref run_dir) = attached_run_dir {
                    if last_remote_sync.elapsed() >= std::time::Duration::from_secs(2) {
                        let current: std::collections::HashSet<String> =
                            crate::ipc::list_agent_ports(run_dir).into_iter().collect();
                        let mut to_add: Vec<String> = current
                            .difference(&known_remote_agents)
                            .cloned()
                            .collect();
                        to_add.sort();
                        for name in &to_add {
                            match pane_factory::create_remote_pane(
                                name,
                                &home,
                                &fleet_path,
                                &mut layout,
                                pane_cols,
                                pane_rows,
                                &wakeup_tx,
                            ) {
                                Ok(pane) => {
                                    let tab_name = pane.agent_name.clone();
                                    known_remote_agents.insert(tab_name.clone());
                                    layout.push_tab_preserve_focus(
                                        crate::layout::Tab::new(tab_name, pane),
                                    );
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

    // Attached mode: the daemon owns session state, fleet.yaml reconciliation,
    // and every agent's PTY. Touching any of them on exit would clobber live
    // daemon state — `sync_fleet_yaml` in particular would silently delete
    // fleet entries whose remote connect happened to fail at startup.
    if !attached_mode {
        // Sync fleet.yaml to match current state, then save layout
        session::sync_fleet_yaml(&home, &layout);
        session::save_session(&home, &layout);

        // Cleanup: kill all agents
        for tab in &layout.tabs {
            for name in tab.root().agent_names() {
                kill_agent(&registry, &name);
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
    wakeup_tx: &crossbeam::channel::Sender<usize>,
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
                },
            ) {
                tracing::warn!(error = %e, "failed to write fleet.yaml");
            }
            // Resolve from fleet to get defaults merged
            let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
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
        pane.write_input(registry, bytes);
    }
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

/// Kill an agent and remove from both registry and fleet.yaml.
fn kill_agent(registry: &AgentRegistry, name: &str) {
    let mut reg = agent::lock_registry(registry);
    if let Some(handle) = reg.get(name) {
        let mut child = crate::sync::lock_poisoned(&handle.child, "app_child");
        let _ = child.kill();
    }
    reg.remove(name);
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
    let alive = match handle.child.lock() {
        Ok(mut child) => !matches!(child.try_wait(), Ok(Some(_))),
        Err(_) => true,
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
            rx: crossbeam::channel::bounded(1).1,
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
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
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
