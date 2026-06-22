//! Terminal application — multi-tab/pane TUI for agent management.
//!
//! Uses agent::spawn_agent() for all panes (agents and shells), sharing the
//! same PTY lifecycle as the daemon: auto-dismiss, state tracking, broadcast.

mod api_server;
// #t-5: `pub(crate)` so `render::overlay` can read the completion specs
// (`CommandSpec` / `COMMAND_SPECS` / `matching_specs`). `execute` stays
// `pub(super)` = app-only, so command EXECUTION is not widened.
pub(crate) mod commands;
mod dispatch;
mod mouse;
mod overlay;
mod pane_factory;
mod session;
mod telegram_hooks;
mod tui_events;
mod tui_spawn;

pub use overlay::{BoardView, DecisionMode, MenuItem, MenuItemKind, TaskBoardMode};
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
/// - `snapshot_rotation`: app owns session persistence via `session::save_session_if_changed`.
/// - `thread_dump`: env-gated diagnostic, not needed in the interactive TUI.
///
/// #1694(a): `recovery_dispatcher` was REMOVED from this allowlist — the live
/// daemon runs in app mode (`app::run_app`, never `run_core`), so allowlisting it
/// out meant the #685 recovery ladder was silently dead in the live daemon (the
/// #1720 class). It now runs in app mode: Stage1 (ESC-nudge to the PTY) needs no
/// `crash_rx`; Stage2 (restart) has no app-mode consumer, so it escalates to
/// Stage3 instead of silent-dropping (see `build_default_handlers`'
/// `stage2_dispatch_available = false` below). All stages stay shadow-gated-off by
/// default — zero behavior change unless an operator opts in.
const APP_TICK_ALLOWLIST: &[&str] = &["snapshot_rotation", "thread_dump"];

/// Build the per-tick handler set app-standalone runs: the shared
/// `build_default_handlers` minus `APP_TICK_ALLOWLIST`. Extracted so the
/// completeness invariant can compare it against the full daemon set.
///
/// #1694(a): `crash_tx` here is a throwaway sender (its receiver is dropped), so
/// `stage2_dispatch_available = false` — `RecoveryDispatcherHandler` now RUNS
/// (no longer allowlisted out) and its Stage2 path escalates to Stage3 rather
/// than emit onto the consumerless channel.
fn app_tick_handlers(
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) -> Vec<Box<dyn crate::daemon::per_tick::PerTickHandler>> {
    let (crash_tx, _crash_rx) = crossbeam_channel::bounded(1);
    let mut handlers = crate::daemon::build_default_handlers(crash_tx, false, daemon_binary_stale);
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
                CloseTarget::Pane => "Close pane? (y/n)",
                CloseTarget::Tab => "Close tab and kill all agents? (y/n)",
            };
            render::render_confirm(frame, msg);
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
        } => {
            render::render_tasks(frame, items, *col, *row, mode, *view, home);
        }
        Overlay::ScratchShell { pane } => {
            render::render_scratch_shell(frame, pane, registry);
        }
        Overlay::None => {}
    }
}

fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    let home = crate::home_dir();
    // #2325: the app process (unlike the daemon's `run_core`, the only other
    // `runtime_config::reload` caller) never tick-reloads runtime config, so load
    // the persisted values once at startup. Otherwise a persisted `copy_on_select`
    // (the TUI mouse copy-on-select mode) would reset to the compile-time default
    // on every restart. In-session changes update the in-process global directly
    // via `runtime_config::set` (the `Ctrl+B e` toggle and `:set`/`:config set`).
    crate::runtime_config::reload(&home);
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
    let (_api_guard, telegram_state, telegram_status, attached_run_dir) =
        setup_app_bootstrap(&home, &fleet_path, &registry, tui_event_tx);
    let attached_mode = attached_run_dir.is_some();

    // SIGINT / SIGHUP are left to their defaults: Ctrl+C must reach the
    // focused pane's PTY as 0x03 (crossterm reads it as a KeyEvent in raw
    // mode), and SIGHUP's default "kill the process group" keeps shell-exit
    // semantics intact. SIGTERM is the only signal the app intercepts, and
    // only in the Owned branch — see `install_term_only` above.

    // Per-agent AwaitingOperator supervisor: watches for stdout silence during
    // Starting (or recently-entered Idle — some backends like codex match
    // ready_pattern against the startup banner that precedes the update menu)
    // and pushes a vterm tail to the agent's Telegram topic. In Attached mode
    // the daemon already runs its own supervisor against the real registry, so
    // the app must not also poll a disjoint (empty) registry.
    if !attached_mode {
        crate::daemon::supervisor::spawn(home.clone(), Arc::clone(&registry));
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

    // #2057: size-probe env gate, read ONCE here (hoisted from its old spot
    // below the spawn block) so the startup-milestone traces + the per-frame
    // loop probe share it — zero per-frame env-lookup cost (codex #2060).
    let size_debug = std::env::var("AGEND_TUI_SIZE_DEBUG").as_deref() == Ok("1");
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    // #2057 milestone 1: BEFORE the default-home-extra work (supervisor +
    // telegram already ran above and are reflected here; this baseline is the
    // 56 in the fresh-vs-default A/B).
    trace_tty_size(size_debug, "startup-baseline");
    let pane_rows = rows.saturating_sub(4);
    let pane_cols = cols.saturating_sub(2);

    // Remote agent roster (Attached mode). Mirrors `*.port` files the daemon
    // publishes for each live agent; periodic sync below diffs this against
    // the filesystem so hot-reload-added agents auto-materialize as tabs.
    let mut known_remote_agents: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // restart-freeze RCA (t-…55279): anchor for the boot critical path the
    // operator sees freeze on daemon restart — session restore + the
    // synchronous per-agent PTY spawns, then post-spawn wiring, up to the
    // first render. Logged at restore-complete and pre-render-loop below.
    // Pure tracing, zero behavior.
    let restore_start = std::time::Instant::now();

    // #render-first phase-(b): OWNED restore collects deferred attach jobs here;
    // the synchronous per-agent fork/exec + skills-install is replaced by
    // background workers spawned AFTER the render loop is live (below). Attached
    // mode leaves this empty — its remote panes attach via the bridge, a separate
    // cheap path not subject to the restore freeze.
    let mut attach_jobs: Vec<pane_factory::AttachJob> = Vec::new();

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
            &mut name_counter,
            &mut attach_jobs,
            pane_cols,
            pane_rows,
        );
        if !started {
            pane_factory::spawn_pane_tab(
                &mut layout,
                &registry,
                &home,
                "shell",
                &crate::shell_command(),
                &[],
                crate::backend::SpawnMode::Fresh,
                None,
                &HashMap::new(),
                "\r",
                pane_cols,
                pane_rows,
                &wakeup_tx,
                &mut name_counter,
                pane_factory::SpawnIdentity::UnmanagedLocalShell,
            )?;
        }
    }

    // #2057 milestone 2: AFTER session restore + the per-tab agent PTY spawns
    // (the default-home-extra work that scales with #agents/#tabs — the prime
    // suspect). If rows here < the baseline, a spawn/restore step shrank the
    // controlling TTY (e.g. a winsize written to fd 0/1 instead of a pane's
    // pty master).
    trace_tty_size(size_debug, "post-fleet-spawn");

    // restart-freeze RCA (t-…55279): session restore + all synchronous
    // per-agent PTY spawns are done. Sum of the per-agent `restore-spawn`
    // lines ≈ this; the gap to `pre-render-loop` below is post-spawn wiring.
    tracing::info!(
        phase = "restore-complete",
        elapsed_ms = restore_start.elapsed().as_millis() as u64,
        attached = attached_mode,
        "restore-complete: session restore + fleet PTY spawns done"
    );

    // #render-first phase-(b): all placeholders are now in the Layout. Spawn a
    // bounded (W=3) pool of background workers to run the deferred attaches
    // (skills + fork/exec + subscribe) and hand results back over `attach_rx`;
    // the render loop's select! arm applies each to its pane. The render loop is
    // entered immediately below — the operator sees the TUI shell while attaches
    // run in the background, instead of freezing on N synchronous spawns.
    //
    // attach_tx is held by the main thread for the ENTIRE render-loop scope
    // (mirrors `wakeup_tx`): a live sender keeps `attach_rx` connected, so
    // `recv(attach_rx)` BLOCKS instead of busy-spinning once every worker exits
    // and drops its sender clone (the #BLOCKER must-resolve).
    let (attach_tx, attach_rx) = crossbeam_channel::unbounded::<pane_factory::AttachOutcome>();
    // Placeholder forwarder senders, keyed by pane id, retained until the matching
    // AttachOutcome is applied (or the pane is closed before it arrives).
    let mut pending_fwd: HashMap<usize, crossbeam_channel::Sender<Vec<u8>>> = HashMap::new();
    // Stored JoinHandles — joined at teardown BEFORE the registry drain so every
    // worker-spawned child is reaped (not fire-and-forget).
    let mut attach_workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    if !attach_jobs.is_empty() {
        let (job_tx, job_rx) = crossbeam_channel::unbounded::<(usize, pane_factory::AttachSpec)>();
        const ATTACH_WORKERS: usize = 3;
        for w in 0..ATTACH_WORKERS {
            let job_rx = job_rx.clone();
            let attach_tx = attach_tx.clone();
            let registry = Arc::clone(&registry);
            let home = home.clone();
            let handle = std::thread::Builder::new()
                .name(format!("attach_worker_{w}"))
                .spawn(move || {
                    while let Ok((pane_id, spec)) = job_rx.recv() {
                        let outcome = pane_factory::run_attach(spec, pane_id, &registry, &home);
                        if attach_tx.send(outcome).is_err() {
                            break; // render loop gone
                        }
                    }
                })
                .expect("spawn attach worker");
            attach_workers.push(handle);
        }
        for job in attach_jobs.drain(..) {
            let pane_factory::AttachJob {
                pane_id,
                fwd_tx,
                spec,
            } = job;
            pending_fwd.insert(pane_id, fwd_tx);
            let _ = job_tx.send((pane_id, spec));
        }
        // Drop job_tx → workers exit once the queue drains (after each spawn or an
        // early-abort on the shutdown flag inside run_attach).
        drop(job_tx);
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
    // W1.1 (#2050): the `mcp_registry` tracker (now a handler) flips this same
    // `daemon_binary_stale` flag the render loop reads (line ~186); hand it the
    // shared `Arc` so the status-bar warning still surfaces after the tracker
    // moved off the supervisor thread.
    let app_handlers = app_tick_handlers(Arc::clone(&daemon_binary_stale));

    // #2057 milestone 3: just BEFORE the render loop. If this is < the baseline,
    // a startup phase shrank the controlling TTY; milestone 2 brackets whether
    // it was the fleet spawn/restore vs the post-spawn wiring (subscribers /
    // tick threads). The per-frame `#2057-size` probe below only ever sees this
    // post-shrink value (confirmed by the operator A/B), so the cause is here.
    trace_tty_size(size_debug, "pre-render-loop");

    // restart-freeze RCA (t-…55279): entering the render loop — the first
    // `terminal.draw` is imminent. Elapsed from `restore_start` is the full
    // boot critical path the operator perceives as the restart freeze
    // (restore + per-agent spawns + post-spawn wiring). Pure tracing.
    tracing::info!(
        phase = "pre-render-loop",
        elapsed_ms = restore_start.elapsed().as_millis() as u64,
        attached = attached_mode,
        "pre-render-loop: entering render loop (first draw imminent)"
    );

    // #t-84833-10 redraw-storm frame cap: `last_draw` rate-limits `terminal.draw`
    // to ≤1/FRAME_INTERVAL; `dirty` tracks whether anything changed since the last
    // draw (set by every select! arm) so an idle loop keeps the cheap ~50ms
    // refresh cadence instead of busy-drawing at 30 fps.
    let mut last_draw: Option<std::time::Instant> = None;
    let mut dirty = true;
    // #84833-15 R2 perf: stamps the last notification-queue disk scan so it runs at
    // most once per `NOTIF_SYNC_INTERVAL` instead of once per wakeup (see
    // `should_sync_notifications`). Mirrors `last_draw`'s frame-cap state.
    let mut last_notif_sync: Option<std::time::Instant> = None;

    // #freeze-4 (t-…2324) restart-flood boot phase: at restart every pane carries a
    // pre-restart backlog (its dump enqueues via #freeze-4 A1, plus the post-subscribe
    // burst). Until that flood is drained, the loop runs a bounded "loading" phase —
    // a TIME-capped drain ([`render::drain_all_panes_until`]) that clears the flood
    // fast while still yielding to input every frame, shown as a loading indicator —
    // instead of letting the steady-state 64 KiB/frame cap trickle it out over ~1s of
    // interactive freeze. Exits to the steady path once all deferred attaches are
    // applied AND every pane's rx is drained, or after `MAX_BOOT_CATCHUP`.
    let boot_start = std::time::Instant::now();
    let attaches_expected = pending_fwd.len();
    let mut booting = true;

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
        // #84833-15 R2 perf: throttle the per-wakeup notification-queue disk scan to
        // ≥1s (the badge it feeds tolerates staleness); see `should_sync_notifications`.
        let notif_now = std::time::Instant::now();
        if should_sync_notifications(last_notif_sync, notif_now, NOTIF_SYNC_INTERVAL) {
            last_notif_sync = Some(notif_now);
            sync_notification_state(&home, &mut layout);
        }
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

        // #2057 instrumentation (env-gated, AGEND_TUI_SIZE_DEBUG=1): the
        // operator sees ~3 blank rows below the status bar (frame shorter than
        // the window) and it follows the home dir, but the static trace found
        // NO stored size anywhere — every size source is live crossterm. Log
        // the actual numbers per draw so a repro says which one is short.
        if size_debug {
            let cross = crossterm::terminal::size().unwrap_or((0, 0));
            let term_sz = terminal
                .size()
                .map(|s| (s.width, s.height))
                .unwrap_or((0, 0));
            tracing::info!(
                tag = "#2057-size",
                crossterm_cols = cross.0,
                crossterm_rows = cross.1,
                terminal_size = ?term_sz,
                tabs = layout.tabs.len(),
                "TUI draw size probe"
            );
        }

        // #t-84833-10 redraw-storm frame cap: draw at most once per FRAME_INTERVAL
        // and only when something changed (`dirty`). Input is NOT throttled — the
        // `event_rx` arm below processes keystrokes immediately; only the (expensive)
        // full-TUI redraw is rate-limited, which is what the boot flood saturated.
        let frame_now = std::time::Instant::now();
        if dirty && should_draw(last_draw, frame_now, FRAME_INTERVAL) {
            last_draw = Some(frame_now);
            dirty = false;
            // #freeze-4: during the bounded boot catch-up phase, drain the restart
            // flood with a per-frame TIME-capped drain — it clears the backlog fast
            // as a "loading" phase while still yielding to input every frame. Exit
            // to the steady path once all deferred attaches are applied AND every
            // pane's rx is drained, or after MAX_BOOT_CATCHUP (so boot can't hang).
            if booting {
                let backlog_remains =
                    render::drain_all_panes_until(&mut layout, BOOT_FRAME_TIME_CAP);
                let timed_out = boot_start.elapsed() >= MAX_BOOT_CATCHUP;
                if (pending_fwd.is_empty() && !backlog_remains) || timed_out {
                    booting = false;
                    tracing::info!(
                        phase = "boot-catchup-complete",
                        elapsed_ms = boot_start.elapsed().as_millis() as u64,
                        attaches_expected = attaches_expected,
                        attaches_pending = pending_fwd.len(),
                        timed_out = timed_out,
                        "#freeze-4: restart-flood boot catch-up drained"
                    );
                }
            } else {
                // #freeze-3: drain queued PTY output for EVERY pane (active +
                // background) into its VTerm BEFORE drawing, within one shared
                // per-frame byte budget. `render_pane` no longer drains, so a
                // backgrounded busy tab's `rx` stays bounded instead of replaying a
                // multi-second catch-up when switched to. Background panes are drained
                // but not redrawn; only the active tab is painted below.
                render::drain_all_panes(&mut layout);
            }
            terminal.draw(|frame| {
                // #1027: snapshot the shared daemon-binary-stale flag once
                // per frame so the render path sees a consistent value.
                // Relaxed is enough — single-bit flag, no fence vs other
                // state needed; the supervisor's SeqCst store will always
                // be visible to this load before the next paint tick.
                let binary_stale = daemon_binary_stale.load(std::sync::atomic::Ordering::Relaxed);
                // #2057: the area render actually fills — compare to crossterm above.
                if size_debug {
                    let a = frame.area();
                    tracing::info!(
                        tag = "#2057-area",
                        x = a.x,
                        y = a.y,
                        w = a.width,
                        h = a.height,
                        "frame.area() in draw"
                    );
                }
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
                render_active_overlay(frame, &mut overlay, &layout, &registry, &home);
                // #freeze-4: loading indicator while the boot catch-up phase absorbs
                // the restart flood (so it reads as loading-with-progress, not a freeze).
                if booting {
                    render::render_boot_indicator(
                        frame,
                        attaches_expected.saturating_sub(pending_fwd.len()),
                        attaches_expected,
                    );
                }
            })?;
            // #freeze-4: while booting, keep cycling at frame cadence so the
            // time-capped catch-up runs every frame until the restart flood clears.
            // #freeze-2/#freeze-3: otherwise the budget-capped `drain_all_panes`
            // above may have left a backlog in a VISIBLE pane's channel — re-arm
            // `dirty` so the next frame continues the active tab's catch-up (the
            // select-timeout below shrinks to the frame boundary when dirty), clearing
            // over a few frames instead of one input-stalling mega-draw. Background
            // backlog needs no redraw: the loop's ≤50ms idle cadence + per-output
            // wakeups guarantee `drain_all_panes` runs again to bound every pane's `rx`.
            if booting || render::active_tab_has_pending_output(&layout) {
                dirty = true;
            }
        }

        // #t-84833-10: wake the loop at the next frame boundary when a change is
        // pending but throttled (so it never stays stale beyond one frame), else
        // keep the cheap ~50ms idle refresh cadence (catches non-wakeup state like
        // status-bar / notification updates).
        let select_timeout = if dirty {
            match last_draw {
                Some(t) => FRAME_INTERVAL
                    .saturating_sub(t.elapsed())
                    .max(std::time::Duration::from_millis(1)),
                None => std::time::Duration::from_millis(1),
            }
        } else {
            std::time::Duration::from_millis(50)
        };

        crossbeam_channel::select! {
            recv(event_rx) -> ev => {
                dirty = true; // input may change the display → redraw next due frame
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
                            | Overlay::RenamePane { ref mut input } => {
                                input.push_str(&text);
                            }
                            // #t-5: paste grows `input` → the candidate set changes,
                            // so reset the completion highlight (same as Char /
                            // Backspace) — otherwise a stale `selected` past the new
                            // (shorter) list left Tab silently no-op.
                            Overlay::Command {
                                ref mut input,
                                ref mut selected,
                            } => {
                                input.push_str(&text);
                                *selected = 0;
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
                        // #1140: an interactive terminal resize reflows pane widths
                        // (the wide→narrow transition that leaves stale wide-char
                        // spacer cells in ratatui's Buffer::diff), so force the same
                        // full clear the needs_resize path performs — otherwise the
                        // ghost artifacts reappear after the resize.
                        let _ = terminal.clear();
                    }
                    _ => {}
                }
            }
            recv(wakeup_rx) -> _ => {
                // Wakeup from PTY output — drain the whole burst (11 booting agents
                // flood this channel, one wakeup per output chunk) so N chunks
                // coalesce into ONE redraw, not N iterations. The frame cap above
                // then bounds the actual redraw rate.
                while wakeup_rx.try_recv().is_ok() {}
                dirty = true;
            }
            recv(attach_rx) -> outcome => {
                dirty = true;
                // #render-first phase-(b): a background worker finished a deferred
                // attach. Apply it to its placeholder pane (instance id + dump +
                // forwarder on Ready, or a visible "failed to start" banner on
                // Failed). If the pane was closed before the attach completed, drop
                // the outcome. The keepalive `attach_tx` keeps this arm from
                // busy-spinning after all workers exit.
                if let Ok(outcome) = outcome {
                    let pane_id = outcome.pane_id();
                    if let Some(fwd_tx) = pending_fwd.remove(&pane_id) {
                        if let Some(pane) = layout.find_pane_mut(pane_id) {
                            pane_factory::apply_attach_outcome(
                                pane, &registry, outcome, fwd_tx, &wakeup_tx,
                            );
                        } else if let pane_factory::AttachOutcome::Ready { name, .. } = &outcome {
                            // F1 (r4): the pane was closed while the attach was in
                            // flight → the agent is already spawned + registered with
                            // no host pane. Kill it so it doesn't run orphaned until
                            // quit (fwd_tx already removed above → forwarder/rx drop).
                            kill_agent(&home, &registry, name);
                        }
                    }
                }
            }
            recv(tui_event_rx) -> ev => {
                dirty = true;
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
                                render_active_overlay(
                                    frame,
                                    &mut overlay,
                                    &layout,
                                    &registry,
                                    &home,
                                );
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
                dirty = true;
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
                app_maintenance_tick(
                    &home,
                    &registry,
                    &app_externals,
                    &app_configs,
                    &app_handlers,
                );
            }
            default(select_timeout) => {
                // #t-84833-10: periodic idle refresh — mark dirty so the cap above
                // redraws (catches non-wakeup state changes; ~50ms cadence when idle).
                dirty = true;
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
    app_teardown(&home, &layout, &registry, attached_mode, attach_workers);

    Ok(())
}

/// App startup bootstrap: prepare the fleet (issuing `api.cookie` BEFORE any API
/// server thread starts — otherwise Telegram's router `api::call(INJECT)` would
/// silently fail), then either start the in-process API server + the SIGTERM
/// handler (Owned) or note the run dir to connect to (Attached). Extracted
/// verbatim from the head of `run_app` (#14 god-fn split) — byte-identical.
///
/// Returns `(api_guard, telegram_channel, telegram_status, attached_run_dir)`.
/// The RAII `ApiGuard` must outlive the TUI loop, so the caller binds it;
/// `attached_run_dir.is_some()` ⇒ Attached mode.
fn setup_app_bootstrap(
    home: &Path,
    fleet_path: &Path,
    registry: &AgentRegistry,
    tui_event_tx: TuiEventSender,
) -> (
    api_server::ApiGuard,
    Option<Arc<dyn crate::channel::Channel>>,
    TelegramStatus,
    Option<PathBuf>,
) {
    let opts = crate::bootstrap::PrepareOptions {
        resolve_agents: false, // app spawns via pane_factory from tabs
        ..Default::default()
    };
    let mut attached_run_dir: Option<PathBuf> = None;
    let (api_guard, telegram_state, telegram_status) =
        match crate::bootstrap::prepare(home, fleet_path, opts) {
            Ok(crate::bootstrap::BootstrapOutcome::Owned(prepared)) => {
                let telegram = prepared.telegram.clone();
                let status = if telegram.is_some() {
                    TelegramStatus::Connected
                } else {
                    telegram_hooks::telegram_status_from_config(&prepared.config)
                };
                let guard = api_server::start_api_server(prepared, registry, tui_event_tx);
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
    (api_guard, telegram_state, telegram_status, attached_run_dir)
}

/// Periodic owned-mode maintenance: run the full daemon per-tick handler
/// pipeline once. Extracted verbatim from `run_app`'s tick arm (#14 god-fn
/// split) — byte-identical, no behaviour change.
fn app_maintenance_tick(
    home: &Path,
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
    configs: &crate::api::ConfigRegistry,
    handlers: &[Box<dyn crate::daemon::per_tick::PerTickHandler>],
) {
    let tick_ctx = crate::daemon::per_tick::TickContext {
        home,
        registry,
        externals,
        configs,
    };
    crate::daemon::per_tick::run_handlers_with_panic_guard(handlers, &tick_ctx);
}

/// App exit teardown: persist the on-screen layout, then (Owned mode only) sync
/// fleet.yaml + kill every agent PTY. Extracted verbatim from the tail of
/// `run_app` (#14) — byte-identical.
///
/// `save_session` is UNGATED (#895): tab grouping / splits / ratios are
/// presentation-layer state the app owns even when Attached, so the next attach
/// can restore the custom layout. `sync_fleet_yaml` + agent-kill STAY gated to
/// Owned mode — in Attached mode the daemon owns fleet.yaml and the agent PTYs.
/// Process-global app-mode shutdown flag, cloned into every Owned-mode agent's
/// `SpawnConfig.shutdown` (see `pane_factory::attach_agent_to_pane`). app mode
/// is a singleton process, so one flag covers the whole fleet — this avoids
/// threading an `Arc<AtomicBool>` through the entire restore/pane-factory call
/// chain. `app_teardown` flips it true before killing agents so each agent's
/// PTY-close handler (`agent::handle_pty_close`) takes the fast `is_shutdown`
/// early-return (no per-thread 2 s exit-poll, no crash / shell-fallback events
/// during teardown). It is the app-mode equivalent of run_core's
/// "drain registry first" race guard. Sticky-true — process exits after.
static APP_SHUTDOWN: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> =
    std::sync::OnceLock::new();

pub(crate) fn app_shutdown_flag() -> &'static Arc<std::sync::atomic::AtomicBool> {
    APP_SHUTDOWN.get_or_init(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
}

/// #render-first phase-(b) F2: join attach workers, but DETACH any that haven't
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

fn app_teardown(
    home: &Path,
    layout: &Layout,
    registry: &AgentRegistry,
    attached_mode: bool,
    attach_workers: Vec<std::thread::JoinHandle<()>>,
) {
    session::save_session(home, layout);
    if !attached_mode {
        // Sync fleet.yaml to match current state (Owned-only — daemon owns
        // fleet.yaml in Attached).
        session::sync_fleet_yaml(home, layout);

        // Cleanup: kill all agents (Owned-only — daemon owns PTYs in Attached).
        //
        // restart-freeze 真嫌#1 (t-…55279): this was a SEQUENTIAL per-agent
        // `kill_agent` loop, each blocking on `wait_for_child_exit` (≤5 s),
        // ~0.5 s × N ≈ ~6 s of the operator-visible restart freeze. Now:
        //  1. flip the shutdown flag so PTY-close handlers fast-return (no
        //     crash/shell-fallback events, no redundant per-thread exit poll) —
        //     the app-mode equivalent of run_core's drain-first race guard;
        //  2. drain the registry and kill ALL agents in parallel via the shared
        //     run_core core (`terminate_agents_parallel`: parallel SIGTERM →
        //     single grace → SIGKILL/reap holdouts), wall time ≈ one grace
        //     window regardless of N;
        //  3. run the per-agent cleanup tail (drop active-channel binding +
        //     remove IPC port + event log) — mirrors `delete_transaction`'s
        //     steps 5/7/8 (the registry remove is already done by the drain;
        //     app mode tracks no AgentConfig map, matching its `configs: None`).
        app_shutdown_flag().store(true, std::sync::atomic::Ordering::SeqCst);
        // #render-first phase-(b): join the background attach workers BEFORE
        // draining the registry. The shutdown flag (just set) makes each worker
        // early-abort un-started attaches (run_attach checks it on entry), so a
        // holdout is at most ONE in-flight spawn per worker. Joining first means
        // every child a worker DID register is in the registry → the parallel
        // terminate below reaps it.
        //
        // F2 (r4/r6): the join is BOUNDED — a worker wedged mid-spawn (fork/exec /
        // skills / subscribe) must not move the restore freeze to quit. Poll up to
        // a shared grace deadline (slightly longer than #2311's 2s SHUTDOWN_GRACE);
        // past it, DETACH the holdout (drop its handle). `is_finished` (Rust 1.61+,
        // MSRV 1.88) avoids a blocking `join()` on a wedged thread.
        //
        // Detaching is SAFE w.r.t. the one-shot drain below because we set the
        // shutdown flag FIRST (above): a detached worker that finishes its spawn
        // AFTER the drain sees the flag set and reaps its own child in
        // `pane_factory::finish_attach` — so a late registration never outlives
        // teardown (the r6 child-leak race). Children registered before the drain
        // are reaped by the drain itself.
        let attach_join_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let detached = bounded_join_attach_workers(attach_workers, attach_join_deadline);
        if detached > 0 {
            tracing::warn!(
                detached,
                "render-first: detached attach worker(s) still wedged past the join grace at quit"
            );
        }
        // #t-41673 gap-instrument: clock the parallel teardown so the next
        // operator restart EMPIRICALLY confirms the ~6s sequential freeze is
        // gone (expect `teardown_elapsed_ms` ≈ one grace window). Mirrors
        // shutdown_sequence's `shutdown_elapsed_ms` for the app-mode path, plus
        // per-agent `reap_ms` inside `terminate_agents_parallel`.
        let teardown_started = std::time::Instant::now();
        let agents: Vec<(String, crate::daemon::ChildHandle)> = {
            let mut reg = registry.lock();
            reg.drain()
                .map(|(_id, handle)| (handle.name.to_string(), handle.child))
                .collect()
        };
        let agents_total = agents.len();
        let names: Vec<String> = agents.iter().map(|(n, _)| n.clone()).collect();
        crate::daemon::terminate_agents_parallel(agents);
        let run_dir = crate::daemon::run_dir(home);
        for name in &names {
            if let Some(ch) = crate::channel::active_channel() {
                let _ = ch.take_binding(name);
            }
            crate::ipc::remove_port(&run_dir, name);
            crate::event_log::log(home, "delete", name, "delete: app teardown (parallel)");
        }
        tracing::info!(
            agents_total,
            teardown_elapsed_ms = teardown_started.elapsed().as_millis() as u64,
            "app-mode parallel teardown complete"
        );
    }
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
            let shell = crate::shell_command();
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
                pane_factory::SpawnIdentity::UnmanagedLocalShell,
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
                    ..Default::default()
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
                    pane_factory::SpawnIdentity::Managed,
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
    for tab in &mut layout.tabs {
        let pane_ids = tab.root().pane_ids();
        for pane_id in pane_ids {
            if let Some(pane) = tab.root_mut().find_pane_mut(pane_id) {
                let prev = pane.pending_notification_count;
                let now = notification_queue::pending_count(home, &pane.agent_name);
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
fn flush_notifications_for_pane<F>(home: &Path, pane: &mut Pane, injector: F)
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
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
    let buffer_empty = if raw_state == notification_queue::DraftState::Drafting {
        pane.backend.as_ref().and_then(|b| {
            // #1948(b): codex's empty box shows DIM ghost text after `›`, which a
            // plain marker probe mis-reads as typed content — route it through the
            // DIM-aware check (needs the per-char dim mask). Everyone else uses the
            // text-only probe: marker (claude/agy) → placeholder (kiro) → fallback.
            // pane.vterm is owned (no lock).
            if let Some(marker) = b.input_dim_ghost_marker() {
                let (text, dim) = pane.vterm.tail_lines_with_dim(DRAFT_INPUT_TAIL_ROWS);
                notification_queue::input_box_dim_aware_empty(&text, &dim, marker)
            } else {
                notification_queue::input_box_empty_probe(
                    &pane.vterm.tail_lines(DRAFT_INPUT_TAIL_ROWS),
                    b.input_prompt_marker(),
                    b.input_empty_placeholder(),
                )
            }
        })
    } else {
        None
    };
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
    // #1915: app-mode teardown entry — mark "deleting" so a concurrent spawn
    // (e.g. a crash-respawn triggered by the kill below) cannot resurrect the
    // instance. Guard held to fn return; Drop un-marks on every path.
    let _delete_guard = crate::agent::deleting::mark_deleting(home, name);
    crate::daemon::lifecycle::delete_transaction(home, name, registry, None, false);
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
    let alive = match handle.child.try_lock() {
        Some(mut child) => !matches!(child.try_wait(), Ok(Some(_))),
        // Contended → cannot prove the child exited without blocking the main
        // loop; treat as alive and re-check next tick.
        None => true,
    };
    alive
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PaneSource;
    use crate::vterm::VTerm;

    /// #t-84833-10 redraw-storm frame cap — the `should_draw` rate-limit decision.
    #[test]
    fn should_draw_caps_at_frame_rate() {
        use std::time::{Duration, Instant};
        let fi = Duration::from_millis(33);
        let t0 = Instant::now();
        assert!(should_draw(None, t0, fi), "first frame always draws");
        assert!(
            !should_draw(Some(t0), t0, fi),
            "no draw immediately after a draw"
        );
        assert!(
            !should_draw(Some(t0), t0 + Duration::from_millis(10), fi),
            "10ms < interval → throttled"
        );
        assert!(
            should_draw(Some(t0), t0 + Duration::from_millis(33), fi),
            "interval elapsed → draw"
        );
        assert!(
            should_draw(Some(t0), t0 + Duration::from_millis(500), fi),
            "well past interval → draw"
        );
    }

    /// #t-84833-10: the redraw count under a wakeup flood is bounded by the FRAME
    /// RATE, not by the number of wakeups (the storm fix). 1000 wakeups packed into
    /// ~100ms must yield only ~3-4 draws (100ms / 33ms), not 1000.
    #[test]
    fn frame_cap_bounds_draws_under_wakeup_flood() {
        use std::time::{Duration, Instant};
        let fi = Duration::from_millis(33);
        let t0 = Instant::now();
        let mut last_draw: Option<Instant> = None;
        let mut draws = 0u32;
        for i in 0..1000u64 {
            let now = t0 + Duration::from_micros(i * 100); // 1000 wakeups over ~100ms
            if should_draw(last_draw, now, fi) {
                draws += 1;
                last_draw = Some(now);
            }
        }
        assert!(
            draws <= 5,
            "draws must be bounded by frame-rate, got {draws} for 1000 wakeups in 100ms"
        );
        assert!(
            draws >= 3,
            "but should still draw a few times across 100ms, got {draws}"
        );
    }

    /// #84833-15 R2 perf: the notification-queue disk-scan count under a wakeup flood
    /// is bounded by `NOTIF_SYNC_INTERVAL` (≥1s), not by the number of wakeups. A burst
    /// of M wakeups inside one <1s window must yield exactly ONE scan (pre-fix = M);
    /// crossing the ≥1s boundary admits exactly one more. Deterministic via constructed
    /// `Instant`s (no wall-clock timing), threading `last_sync` like the render loop.
    #[test]
    fn notif_sync_throttle_bounds_disk_scans_per_window() {
        use std::time::{Duration, Instant};
        let interval = Duration::from_secs(1);
        let t0 = Instant::now();

        // First-frame semantics: never-scanned ⇒ scan now (badge correct at startup).
        assert!(
            should_sync_notifications(None, t0, interval),
            "first frame always scans so the startup badge is correct"
        );

        // 200 wakeups packed into ~900ms (one <1s window) → exactly ONE scan.
        let mut last_sync: Option<Instant> = None;
        let mut scans = 0u32;
        for i in 0..200u64 {
            let now = t0 + Duration::from_micros(i * 4500); // ~900ms total, all < 1s
            if should_sync_notifications(last_sync, now, interval) {
                scans += 1;
                last_sync = Some(now);
            }
        }
        assert_eq!(
            scans, 1,
            "a wakeup burst within one <1s window must scan exactly once, got {scans}"
        );

        // Crossing the ≥1s boundary admits exactly one more scan...
        let after = t0 + Duration::from_millis(1000);
        assert!(
            should_sync_notifications(last_sync, after, interval),
            "≥1s since last scan → scan again"
        );
        last_sync = Some(after);
        // ...and the next sub-1s wakeup is throttled again.
        assert!(
            !should_sync_notifications(last_sync, after + Duration::from_millis(500), interval),
            "0.5s after the last scan → throttled"
        );
    }

    /// #render-first phase-(b) F2 (r4): `bounded_join_attach_workers` must DETACH a
    /// worker wedged past the deadline (so quit can't hang) while still joining the
    /// finished ones. Deterministic via a parked thread held by a channel.
    #[test]
    fn bounded_join_detaches_wedged_worker_without_hanging() {
        let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
        // Wedged: blocks until keep_tx drops (held past the join below).
        let wedged = std::thread::spawn(move || {
            let _ = keep_rx.recv();
        });
        let quick = std::thread::spawn(|| {}); // finishes immediately
        let start = std::time::Instant::now();
        let detached = bounded_join_attach_workers(
            vec![quick, wedged],
            start + std::time::Duration::from_millis(150),
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "bounded join must not hang on a wedged worker (took {:?})",
            start.elapsed()
        );
        assert_eq!(
            detached, 1,
            "the wedged worker is detached; the quick one is joined"
        );
        drop(keep_tx); // release the parked thread (cleanup)
    }

    /// restart-freeze 真嫌#1 (t-…55279) source-scan invariant: `app_teardown`'s
    /// Owned-mode cleanup must (1) flip the shutdown flag so PTY-close handlers
    /// fast-return, then (2) tear agents down through the shared parallel core
    /// `terminate_agents_parallel` — NOT the old SEQUENTIAL per-tab `kill_agent`
    /// loop (each blocking ≤5 s on `wait_for_child_exit`, ~6 s of the restart
    /// freeze). Regression-proof: revert app_teardown to a `kill_agent` loop and
    /// this fails.
    #[test]
    fn app_teardown_uses_parallel_core_not_sequential_kill_loop() {
        let src = include_str!("mod.rs");
        let start = src.find("fn app_teardown(").expect("app_teardown present");
        let after = &src[start..];
        let end = after.find("fn build_menu_items(").unwrap_or(after.len());
        let body = &after[..end];

        assert!(
            body.contains("app_shutdown_flag().store(true"),
            "app_teardown must flip the shutdown flag before killing agents \
             (fast PTY-close early-return, no crash events during teardown)"
        );
        assert!(
            body.contains("terminate_agents_parallel("),
            "app_teardown must route the kill through the shared parallel core"
        );
        assert!(
            !body.contains("kill_agent("),
            "#真嫌1: app_teardown must NOT use the sequential per-agent kill_agent \
             loop (that is the ~6s restart-freeze regression)"
        );
    }

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
        let stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            Arc::new(std::sync::atomic::AtomicBool::new(false));
        let all: HashSet<&str> = crate::daemon::build_default_handlers(crash_tx, true, stale)
            .iter()
            .map(|h| h.name())
            .collect();
        let app: HashSet<&str> =
            app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .iter()
                .map(|h| h.name())
                .collect();

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

    /// #1694(a): the #685 recovery ladder must RUN in app mode — the live daemon
    /// is app-standalone (`run_app`), never `run_core`, so allowlisting
    /// `recovery_dispatcher` out left the whole ladder silently dead in production
    /// (the #1720 / #1002 class). This pins it back IN the app run set.
    #[test]
    fn recovery_dispatcher_runs_in_app_mode_1694a() {
        let names: Vec<&str> =
            app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
                .iter()
                .map(|h| h.name())
                .collect();
        assert!(
            names.contains(&"recovery_dispatcher"),
            "recovery_dispatcher must RUN in app mode (#1694a) — got {names:?}"
        );
        assert!(
            !APP_TICK_ALLOWLIST.contains(&"recovery_dispatcher"),
            "recovery_dispatcher must NOT be allowlisted out of app mode (#1694a)"
        );
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
        for h in app_tick_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false))) {
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
            offthread: None,
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
        // Search only the production region. This assertion's own literal
        // lives in the #[cfg(test)] module below, so a whole-file substring
        // check self-matches and would stay green even if the real call were
        // deleted. Require the call form (with `(`) before the test cutoff.
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            prod.contains("inject_notification_with_submit("),
            "flush_idle_notifications must wire the submit-aware injector \
             (#982 reviewer #999 verdict) — no call to \
             'inject_notification_with_submit(' found in the production region \
             of src/app/mod.rs"
        );
    }

    // ── #1944: buffer-aware draft gate (the operator-facing fix) ──

    /// Build a pane whose live `vterm` renders `screen` with `backend`. The term
    /// is `VTerm::new(cols, rows)` — wide enough that input lines don't wrap, and
    /// few enough rows that the content stays within `DRAFT_INPUT_TAIL_ROWS`.
    fn pane_with_screen(name: &str, backend: Option<Backend>, screen: &str) -> Pane {
        let mut p = pane(name);
        p.backend = backend;
        p.vterm = crate::vterm::VTerm::new(80, 6);
        p.vterm.process(screen.as_bytes());
        p
    }

    /// Set up a recent unsent draft (typed_ms > submit_ms → `Drafting`) and one
    /// queued notification for `agent` under `home`.
    fn seed_drafting_with_queued(home: &Path, agent: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        crate::agent_ops::save_metadata(
            home,
            agent,
            "last_input_epoch_ms",
            serde_json::json!(now - 30_000),
        );
        crate::agent_ops::save_metadata(
            home,
            agent,
            "last_submit_epoch_ms",
            serde_json::json!(now - 60_000),
        );
        notification_queue::enqueue(home, agent, "[AGEND-MSG-PENDING] peer report")
            .expect("enqueue test notification");
    }

    /// #1944 §3.9: a stale type-then-clear draft (typed_ms > submit_ms but the
    /// input box is EMPTY) must DELIVER — the old timestamp-only gate held it.
    #[test]
    fn draft_gate_delivers_when_input_box_empty() {
        let home = tmp_home("draftgate-empty");
        seed_drafting_with_queued(&home, "lead");
        // claude pane, input box empty (`❯ ` with nothing typed).
        let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ ");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "empty input box → the stale-draft message must be delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1944 §3.9: a REAL live draft (text in the input box) must still DEFER —
    /// the draft-protection invariant is unchanged.
    #[test]
    fn draft_gate_defers_when_input_box_has_text() {
        let home = tmp_home("draftgate-typed");
        seed_drafting_with_queued(&home, "lead");
        let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ half-typed reply");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "a real draft (text in the box) must keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1944 §3.9: a backend with no prompt marker (Shell) → buffer-emptiness is
    /// undeterminable → fall back to the timestamp behavior (defer), fail toward
    /// draft-protection. Same outcome for a claude pane mid-output (no prompt in
    /// the tail) — covered by `input_box_none_when_marker_absent`.
    #[test]
    fn draft_gate_falls_back_to_timestamp_for_markerless_backend() {
        let home = tmp_home("draftgate-shell");
        seed_drafting_with_queued(&home, "lead");
        // Shell has no input_prompt_marker → None → keep the raw Drafting defer.
        let mut p = pane_with_screen("lead", Some(Backend::Shell), "$ ");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "markerless backend → fail toward protection (timestamp-only defer)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948 v2 §3.9: kiro has no prompt marker but its empty box shows a
    /// placeholder — a cleared kiro pane (placeholder visible) must DELIVER.
    #[test]
    fn draft_gate_delivers_for_kiro_when_placeholder_visible() {
        let home = tmp_home("draftgate-kiro-empty");
        seed_drafting_with_queued(&home, "lead");
        // cleared kiro box: the real placeholder is visible (no typed content).
        let mut p = pane_with_screen(
            "lead",
            Some(Backend::KiroCli),
            "Kiro auto\n\n ask a question or describe a task ↵\n /copy",
        );
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "kiro cleared (placeholder visible) → stale draft delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948 v2 §3.9: a kiro pane with a real draft (placeholder replaced by typed
    /// text) must still DEFER — protection unchanged.
    #[test]
    fn draft_gate_defers_for_kiro_when_typed() {
        let home = tmp_home("draftgate-kiro-typed");
        seed_drafting_with_queued(&home, "lead");
        let mut p = pane_with_screen("lead", Some(Backend::KiroCli), "Kiro auto\n\n half typed\n");
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "kiro with text (placeholder gone) → keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948(b) §3.9: codex's empty box shows DIM ghost text after `›` (SGR 2) —
    /// the dim-aware path must DELIVER (the v1 plain-marker path mis-read the ghost
    /// as typed content and held). The vterm processes the real SGR so the dim
    /// flag is set exactly as codex emits it.
    #[test]
    fn draft_gate_delivers_for_codex_when_ghost_is_dim() {
        let home = tmp_home("draftgate-codex-ghost");
        seed_drafting_with_queued(&home, "lead");
        // `ESC[1m›` (bold prompt) + `ESC[2m…` (dim ghost) — codex's real encoding.
        let screen = "\u{1b}[1m›\u{1b}[22m\u{1b}[2m Use /skills to list available skills\u{1b}[0m";
        let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert_eq!(
            injected.len(),
            1,
            "codex empty box (dim ghost after ›) → stale draft delivered, not held"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1948(b) §3.9: a real codex draft (normal-intensity text after `›`) must
    /// still DEFER — the dim signal must not false-deliver on a real draft.
    #[test]
    fn draft_gate_defers_for_codex_when_input_normal_intensity() {
        let home = tmp_home("draftgate-codex-typed");
        seed_drafting_with_queued(&home, "lead");
        // `ESC[1m›` then NORMAL intensity input (no SGR 2).
        let screen = "\u{1b}[1m›\u{1b}[22m my actual draft reply\u{1b}[0m";
        let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
        p.pending_notification_count = notification_queue::pending_count(&home, "lead");

        let mut injected: Vec<String> = Vec::new();
        flush_notifications_for_pane(&home, &mut p, |t| {
            injected.push(t.to_string());
            Ok(())
        });
        assert!(
            injected.is_empty(),
            "codex with normal-intensity input → keep deferring (protection unchanged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn input_prompt_marker_only_for_verified_backends() {
        assert_eq!(Backend::ClaudeCode.input_prompt_marker(), Some("❯"));
        assert_eq!(Backend::Agy.input_prompt_marker(), Some(">"));
        // #1948 codex follow-up: codex is NOT marker-covered — its empty box shows
        // a rotating ghost phrase after `›`, which the PLAIN marker probe mis-reads
        // as typed content. #1948(b): codex is instead covered via the DIM-aware
        // path (`input_dim_ghost_marker`) — the ghost is dim, real input is normal.
        assert_eq!(Backend::Codex.input_prompt_marker(), None);
        assert_eq!(Backend::Codex.input_empty_placeholder(), None);
        assert_eq!(Backend::Codex.input_dim_ghost_marker(), Some("›"));
        assert_eq!(Backend::Shell.input_prompt_marker(), None);
        assert_eq!(Backend::OpenCode.input_prompt_marker(), None);
        // #1948 v2: kiro covered via placeholder, NOT a marker; opencode stays
        // fully fallback (no marker, no placeholder).
        assert_eq!(Backend::KiroCli.input_prompt_marker(), None);
        assert_eq!(
            Backend::KiroCli.input_empty_placeholder(),
            Some("ask a question or describe a task")
        );
        assert_eq!(Backend::OpenCode.input_empty_placeholder(), None);
        assert_eq!(Backend::ClaudeCode.input_empty_placeholder(), None);
        // dim-ghost is codex-only: the marker-backends and kiro are NOT dim-aware.
        assert_eq!(Backend::ClaudeCode.input_dim_ghost_marker(), None);
        assert_eq!(Backend::Agy.input_dim_ghost_marker(), None);
        assert_eq!(Backend::KiroCli.input_dim_ghost_marker(), None);
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
        // Search only the production region. This assertion's own literal
        // lives in the #[cfg(test)] module below, so a whole-file substring
        // check self-matches and would stay green even if the real call were
        // deleted. Require the call form (with `(`) before the test cutoff.
        let prod = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
        assert!(
            prod.contains("register_event_subscribers("),
            "run_app must call daemon::register_event_subscribers in owned mode \
             (app mode never reaches run_core's registration — #1720 app-mode \
             silent-drop root fix). No call to 'register_event_subscribers(' \
             found in the production region of src/app/mod.rs"
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
    // #1762: draft detection — only text-composing input marks a draft
    // -----------------------------------------------------------------------

    /// #1762: navigation / control keys + lone whitespace are NOT text-composing
    /// (they must not defer actionable injects), while real character input,
    /// UTF-8, and bracketed paste ARE (so #1675 still protects a live draft).
    /// Byte forms mirror `tui::key_to_bytes`.
    #[test]
    fn is_text_composing_input_excludes_nav_control_whitespace_1762() {
        // Navigation / control (ESC-prefixed) → NOT composing.
        for seq in [
            &b"\x1b[A"[..], // Up
            b"\x1b[B",      // Down
            b"\x1b[C",      // Right
            b"\x1b[D",      // Left
            b"\x1b[H",      // Home
            b"\x1b[F",      // End
            b"\x1b[5~",     // PageUp
            b"\x1b[6~",     // PageDown
            b"\x1b[3~",     // Delete
            b"\x1b[Z",      // Shift+Tab (BackTab, if ever forwarded)
            b"\x1bOP",      // F1
            b"\x1b",        // Esc
            b"\x1ba",       // Alt+a
        ] {
            assert!(
                !is_text_composing_input(seq),
                "ESC-seq {seq:?} must NOT be text-composing"
            );
        }
        // Bare control bytes → NOT composing.
        assert!(!is_text_composing_input(&[0x01])); // Ctrl+A
        assert!(!is_text_composing_input(b"\t")); // Tab
        assert!(!is_text_composing_input(&[0x7f])); // Backspace (DEL)
        assert!(!is_text_composing_input(b"\r")); // Enter (submit — counted separately)
        assert!(!is_text_composing_input(b"\n")); // Shift+Enter
                                                  // Lone whitespace → NOT composing (#1762 fat-fingered space).
        assert!(!is_text_composing_input(b" "));
        assert!(!is_text_composing_input(b"   "));
        assert!(!is_text_composing_input(&[])); // empty

        // Real character input → IS composing.
        assert!(is_text_composing_input(b"a"));
        assert!(is_text_composing_input(b"hello"));
        assert!(is_text_composing_input(b"hi there")); // space among text still composing
        assert!(is_text_composing_input("café".as_bytes())); // UTF-8
        assert!(is_text_composing_input("日本語".as_bytes())); // multibyte
                                                               // Bracketed paste wraps PASTED TEXT → composing.
        assert!(is_text_composing_input(b"\x1b[200~pasted\x1b[201~"));
    }

    /// #1762 behavioral contract: exercising the exact gate `write_to_focused`
    /// applies (`if is_text_composing_input(bytes) { record_input_activity }`),
    /// a navigation key leaves the pane Clean (actionable injects NOT deferred),
    /// while real typing marks it Drafting (#1675 still protects a live draft).
    /// (`write_to_focused` itself needs a PTY-backed Layout; the wiring is the
    /// 3-line gate, exercised here against the real predicate + draft_state.)
    #[test]
    fn nav_key_does_not_defer_but_typing_does_1762() {
        let home = tmp_home("1762-behavior");
        let agent = "agent1";

        // (a) operator browses history with Up while idle → gate skips → no draft.
        let up = b"\x1b[A";
        if is_text_composing_input(up) {
            notification_queue::record_input_activity(&home, agent);
        }
        assert_eq!(
            notification_queue::draft_state(&home, agent),
            notification_queue::DraftState::None,
            "#1762: a nav key must NOT mark a draft → actionable notif not deferred"
        );

        // (b) operator types real text → gate records → draft present (deferred).
        if is_text_composing_input(b"hello") {
            notification_queue::record_input_activity(&home, agent);
        }
        assert_eq!(
            notification_queue::draft_state(&home, agent),
            notification_queue::DraftState::Drafting,
            "#1762: real typing still marks a draft (#1675 preserved)"
        );
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

#[cfg(test)]
mod review_repro_app_tui;
