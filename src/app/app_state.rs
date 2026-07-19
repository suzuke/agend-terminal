//! #2453: root `AppState` / bounded `RestartState` owners for `run_app`'s
//! durable render-loop state — re-homed from `mod.rs` (grandfathered
//! anti-monolith ratchet: the parent file may not grow). `pub(super)` is the
//! minimum visibility `run_app` needs; nothing outside `app` sees these.

#[allow(clippy::wildcard_imports)]
use super::*;

/// #2453 R2: bounded typed owner for the app owner-restart in-flight state.
/// At most one probe at a time (the gate CAS-serializes at the handler).
pub(super) struct RestartState {
    /// Read after the loop to drive the in-place re-exec in `run()`.
    pub(super) restart_outcome: RunOutcome,
    pub(super) restart_probe: Option<RestartProbe>,
    /// #2453 R2 P0-2: the commit-pending state after a passing probe. The loop
    /// NEVER blocks on the transport ack (that would freeze the UI on a wedged
    /// API writer — codex R3): it parks the `flush_ack` receiver here and polls
    /// it each tick.
    pub(super) restart_commit_pending: Option<CommitPending>,
}

/// #2453: root owner of `run_app`'s durable render-loop state. The only
/// mutable lifecycle locals permitted OUTSIDE this struct are `attach_jobs`
/// and `attach_workers` (startup/teardown-scoped). Channels, registries, and
/// RAII guards deliberately stay loose — they are wiring, not loop state.
pub(super) struct AppState {
    /// The five cohesive key/UI-interaction fields, owned by one type.
    /// `name_counter` counts auto-dedup agent names.
    pub(super) ui: UiState,
    /// Remote agent roster (Attached mode). Mirrors `*.port` files the daemon
    /// publishes for each live agent; periodic sync diffs this against the
    /// filesystem so hot-reload-added agents auto-materialize as tabs.
    pub(super) known_remote_agents: std::collections::HashSet<String>,
    /// Placeholder forwarder senders, keyed by pane id, retained until the
    /// matching AttachOutcome is applied (or the pane is closed first).
    pub(super) pending_fwd: HashMap<usize, crossbeam_channel::Sender<Vec<u8>>>,
    /// Flag to trigger resize pass after layout changes (split, close, zoom,
    /// tab switch). Starts true so restored split panes get correct sizes
    /// before first draw.
    pub(super) needs_resize: bool,
    /// Throttle for Attached-mode remote agent discovery. 2s is short enough
    /// that a fleet.yaml reload (daemon tick is 10s) feels timely but long
    /// enough that the readdir cost is trivial.
    pub(super) last_remote_sync: std::time::Instant,
    /// #1479: throttled, change-gated session.json persistence. Graceful exit
    /// already saves; this periodically persists the current layout so a
    /// kill -9 / power loss preserves what's on screen.
    pub(super) last_session_save: std::time::Instant,
    /// Caches the last session.json write to skip no-op rewrites (#1479).
    pub(super) last_session_json: Option<String>,
    /// #t-84833-10 redraw-storm frame cap: rate-limits `terminal.draw` to
    /// ≤1/FRAME_INTERVAL.
    pub(super) last_draw: Option<std::time::Instant>,
    /// Tracks whether anything changed since the last draw (set by every
    /// select! arm) so an idle loop keeps the cheap ~50ms refresh cadence
    /// instead of busy-drawing at 30 fps (#t-84833-10).
    pub(super) dirty: bool,
    /// #84833-15 R2 perf: stamps the last notification-queue disk scan so it
    /// runs at most once per `NOTIF_SYNC_INTERVAL` instead of once per wakeup
    /// (see `should_sync_notifications`). Mirrors `last_draw`'s frame cap.
    pub(super) last_notif_sync: Option<std::time::Instant>,
    /// #2524 P2b / #2313: mirrors `last_notif_sync` for the decision-badge
    /// throttle.
    pub(super) last_decision_sync: Option<std::time::Instant>,
    /// #2524 P2b / #2313: fleet-wide pending-decision total, refreshed
    /// alongside `last_decision_sync`; read at render time by both `render()`
    /// call sites (mirrors how `binary_stale` is snapshotted once per draw).
    pub(super) pending_decisions_total: usize,
    /// #freeze-4 (t-…2324) restart-flood boot phase: until the pre-restart
    /// backlog flood is drained, the loop runs a bounded "loading" phase — a
    /// TIME-capped drain ([`render::drain_all_panes_until`]) that clears the
    /// flood fast while still yielding to input every frame — instead of
    /// letting the steady-state 64 KiB/frame cap trickle it out over ~1s of
    /// interactive freeze. Exits once all deferred attaches are applied AND
    /// every pane's rx is drained, or after `MAX_BOOT_CATCHUP`.
    pub(super) booting: bool,
    /// #freeze-4 boot anchors: catch-up phase start + the number of deferred
    /// attaches expected at boot (progress denominator). Set once by
    /// `schedule_deferred_attaches`; read by `render_frame`.
    pub(super) boot_start: std::time::Instant,
    pub(super) attaches_expected: usize,
    /// #2453 R2: app owner-restart in-flight state (bounded typed sub-owner).
    pub(super) restart: RestartState,
}

/// #render-first attach pipeline handles: (keepalive sender, outcome
/// receiver, worker join handles).
pub(super) type AttachPipeline = (
    crossbeam_channel::Sender<pane_factory::AttachOutcome>,
    crossbeam_channel::Receiver<pane_factory::AttachOutcome>,
    Vec<std::thread::JoinHandle<()>>,
);

/// #2453 Slice 2: control-flow signal from AppState loop methods back to the
/// `run_app` orchestrator — a method cannot `break` the caller's loop.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum LoopFlow {
    Continue,
    Break,
}

/// #2453 Slice 2: loop-stable wiring shared by the AppState methods —
/// mirrors the `UiDeps` precedent. Everything here is either a reference to
/// run_app-owned wiring or a small Copy value; durable loop STATE lives in
/// `AppState` itself.
#[derive(Clone, Copy)]
pub(super) struct AppDeps<'a> {
    pub home: &'a std::path::PathBuf,
    pub fleet_path: &'a std::path::PathBuf,
    pub registry: &'a AgentRegistry,
    pub wakeup_tx: &'a crossbeam_channel::Sender<usize>,
    pub app_restart_gate: &'a crate::api::app_restart::AppRestartGate,
    pub daemon_binary_stale: &'a crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
    pub telegram_status: TelegramStatus,
    pub attached_run_dir: &'a Option<std::path::PathBuf>,
    pub attached_mode: bool,
    pub size_debug: bool,
}

/// #2453 Slice 2: the extracted run_app loop/setup logic, method-by-method.
/// Bodies are moved verbatim from `run_app` (state.* -> self.*); behavior is
/// pinned by the structural guards in `appstate_2453_tests.rs` plus the
/// real-entry characterization suite.
impl AppState {
    /// #2453: root AppState construction — every initializer is context-free,
    /// so the caller constructs before restore and the setup code works on
    /// owned fields. Field semantics are documented on the struct.
    pub(super) fn new() -> Self {
        Self {
            ui: UiState {
                layout: Layout::new(),
                last_tab: 0,
                name_counter: HashMap::new(),
                overlay: Overlay::None,
                key_handler: KeyHandler::new(),
                mouse_state: mouse::MouseState::default(),
            },
            known_remote_agents: std::collections::HashSet::new(),
            pending_fwd: HashMap::new(),
            needs_resize: true,
            last_remote_sync: std::time::Instant::now(),
            last_session_save: std::time::Instant::now(),
            last_session_json: None,
            last_draw: None,
            dirty: true,
            last_notif_sync: None,
            last_decision_sync: None,
            pending_decisions_total: 0,
            booting: true,
            boot_start: std::time::Instant::now(),
            attaches_expected: 0,
            restart: RestartState {
                restart_outcome: RunOutcome::Normal,
                restart_probe: None,
                restart_commit_pending: None,
            },
        }
    }

    pub(super) fn restore_panes(
        &mut self,
        deps: &AppDeps<'_>,
        restore_start: std::time::Instant,
    ) -> Result<Vec<pane_factory::AttachJob>> {
        let AppDeps {
            home,
            fleet_path,
            registry,
            wakeup_tx,
            attached_run_dir,
            attached_mode,
            size_debug,
            ..
        } = *deps;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        let pane_cols = cols.saturating_sub(2);
        let pane_rows = rows.saturating_sub(4);
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
                home,
                fleet_path,
                run_dir,
                &mut self.ui.layout,
                wakeup_tx,
                pane_cols,
                pane_rows,
            );
            // Populate the remote-agent roster from the placed tabs so the periodic
            // sync (lines 615-665) tracks them correctly.
            for tab in &self.ui.layout.tabs {
                for name in tab.root().agent_names() {
                    self.known_remote_agents.insert(name);
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
                home,
                fleet_path,
                &mut self.ui.layout,
                &mut self.ui.name_counter,
                &mut attach_jobs,
                pane_cols,
                pane_rows,
            );
            if !started {
                pane_factory::spawn_pane_tab(
                    &mut self.ui.layout,
                    registry,
                    home,
                    "shell",
                    &crate::shell_command(),
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    "\r",
                    pane_cols,
                    pane_rows,
                    wakeup_tx,
                    &mut self.ui.name_counter,
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
        Ok(attach_jobs)
    }

    /// #render-first pipeline: session restore (placeholder panes + deferred
    /// attach jobs) then background attach scheduling. The returned Sender is
    /// the keepalive the caller must hold for the render-loop scope.
    pub(super) fn restore_and_attach(
        &mut self,
        deps: &AppDeps<'_>,
        restore_start: std::time::Instant,
    ) -> Result<AttachPipeline> {
        let attach_jobs = self.restore_panes(deps, restore_start)?;
        let (attach_tx, attach_rx) = crossbeam_channel::unbounded::<pane_factory::AttachOutcome>();
        let attach_workers = self.schedule_deferred_attaches(attach_jobs, &attach_tx, deps);
        Ok((attach_tx, attach_rx, attach_workers))
    }

    pub(super) fn schedule_deferred_attaches(
        &mut self,
        mut attach_jobs: Vec<pane_factory::AttachJob>,
        attach_tx: &crossbeam_channel::Sender<pane_factory::AttachOutcome>,
        deps: &AppDeps<'_>,
    ) -> Vec<std::thread::JoinHandle<()>> {
        let AppDeps { home, registry, .. } = *deps;
        // Stored JoinHandles — joined at teardown BEFORE the registry drain so every
        // worker-spawned child is reaped (not fire-and-forget).
        let mut attach_workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        if !attach_jobs.is_empty() {
            let (job_tx, job_rx) =
                crossbeam_channel::unbounded::<(usize, pane_factory::AttachSpec)>();
            const ATTACH_WORKERS: usize = 3;
            for w in 0..ATTACH_WORKERS {
                let job_rx = job_rx.clone();
                let attach_tx = attach_tx.clone();
                let registry = Arc::clone(registry);
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
                self.pending_fwd.insert(pane_id, fwd_tx);
                let _ = job_tx.send((pane_id, spec));
            }
            // Drop job_tx → workers exit once the queue drains (after each spawn or an
            // early-abort on the shutdown flag inside run_attach).
            drop(job_tx);
        }

        // #2453 ownership-move fix: re-stamp the sync/save throttle baselines at
        // the former declaration seam. Restore + deferred-attach scheduling above
        // can exceed the 2s/10s intervals, and the pre-ownership code observed
        // its baselines from HERE — without this, the first loop iteration could
        // trigger an immediate remote sync / session save the old code never did.
        self.last_remote_sync = std::time::Instant::now();
        self.last_session_save = std::time::Instant::now();
        // #freeze-4 boot phase anchors (see `AppState::booting`).
        self.boot_start = std::time::Instant::now();
        self.attaches_expected = self.pending_fwd.len();
        attach_workers
    }

    pub(super) fn poll_restart(&mut self, deps: &AppDeps<'_>) -> LoopFlow {
        let AppDeps {
            app_restart_gate, ..
        } = *deps;
        // #2453 R2: poll the in-flight restart preflight probe (non-blocking) each
        // loop tick so the TUI never freezes on it.
        if self.restart.restart_probe.is_some() {
            use crate::api::app_restart::AppRestartVerdict;
            match poll_restart_probe(&mut self.restart.restart_probe, app_restart_gate) {
                ProbePoll::Prepared(reply, flush_ack) => {
                    // Probe passed; the gate is still Probing. Reply PREPARED, then park
                    // the transport ack (do NOT block): the handler returns the
                    // `prepared` reply, handle_session writes+flushes it, and its
                    // post-flush action sends `()` on `flush_ack`. We poll that below,
                    // non-blockingly, and CAS Probing→Committing only on the ack — so the
                    // UI stays responsive even if the API writer wedges. If the handler
                    // is already gone (send fails), roll the gate back now.
                    if reply.send(AppRestartVerdict::Prepared).is_ok() {
                        self.restart.restart_commit_pending = Some(CommitPending {
                            flush_ack,
                            deadline: std::time::Instant::now() + RESTART_COMMIT_WATCHDOG,
                        });
                    } else {
                        app_restart_gate.abort_to_serving();
                    }
                }
                ProbePoll::Abort(reply, reason) => {
                    let _ = reply.send(AppRestartVerdict::Aborted(reason));
                }
                ProbePoll::Pending => {}
            }
        }
        // #2453 R2 P0-2: poll the commit-pending state (NON-BLOCKING). The ack means
        // the `prepared` reply flushed → CAS Probing→Committing + break to teardown+
        // exec. A disconnect (reply not flushed) or the watchdog deadline aborts the
        // restart with the app intact and an observable log; the `prepared` reply is
        // an honest indeterminate attempt, so a watchdog abort stays truthful.
        if self.restart.restart_commit_pending.is_some() {
            let now = std::time::Instant::now();
            let poll = poll_commit_pending(
                self.restart
                    .restart_commit_pending
                    .as_ref()
                    .expect("commit-pending present (checked above)"),
                now,
            );
            match poll {
                CommitPoll::Commit => {
                    self.restart.restart_commit_pending = None;
                    if app_restart_gate.to_committing() {
                        self.restart.restart_outcome = RunOutcome::RestartRequested;
                        return LoopFlow::Break;
                    }
                    // Could not advance Probing→Committing (should not happen for the
                    // claim owner) — fail safe: roll back, keep serving.
                    app_restart_gate.abort_to_serving();
                }
                CommitPoll::Abort(reason) => {
                    self.restart.restart_commit_pending = None;
                    app_restart_gate.abort_to_serving();
                    tracing::warn!(
                        target: "handoff",
                        event = "app_restart_abort",
                        reason,
                        "#2453 app restart aborted before commit — app intact (retry)"
                    );
                }
                CommitPoll::Pending => {}
            }
        }
        LoopFlow::Continue
    }

    pub(super) fn close_dead_scratch_shell(&mut self, deps: &AppDeps<'_>) {
        let AppDeps { home, registry, .. } = *deps;
        // Auto-close the scratch shell overlay once its backing process
        // exits (user ran `exit`, hit Ctrl+D, or the shell crashed). The
        // 50ms `default` arm of the main `select!` below guarantees this
        // runs at least every 50ms even without new PTY output.
        if let Overlay::ScratchShell { pane } = &self.ui.overlay {
            if !agent_is_alive(registry, &pane.agent_name) {
                let name = pane.agent_name.clone();
                self.ui.overlay = Overlay::None;
                kill_agent(home, registry, &name);
            }
        }
    }

    pub(super) fn apply_pending_resize(
        &mut self,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps { registry, .. } = *deps;
        if self.needs_resize {
            let (c, r) = crossterm::terminal::size().unwrap_or((120, 40));
            let pane_area = ratatui::layout::Rect::new(0, 1, c, r.saturating_sub(2));
            crate::layout::resize_panes(pane_area, &mut self.ui.layout, registry);
            // #1140: force full redraw to clear wide-char ghost artifacts.
            // ratatui's Buffer::diff() can leave stale spacer cells when
            // wide chars are replaced by narrow chars across frames.
            let _ = terminal.clear();
            self.needs_resize = false;
        }
    }

    pub(super) fn sync_badges(&mut self, deps: &AppDeps<'_>) {
        let AppDeps { home, .. } = *deps;
        // #84833-15 R2 perf: throttle the per-wakeup notification-queue disk scan to
        // ≥1s (the badge it feeds tolerates staleness); see `should_sync_notifications`.
        let notif_now = std::time::Instant::now();
        if should_sync_notifications(self.last_notif_sync, notif_now, NOTIF_SYNC_INTERVAL) {
            self.last_notif_sync = Some(notif_now);
            sync_notification_state(home, &mut self.ui.layout);
        }
        // #2524 P2b / #2313: same throttle idiom, separate cadence state — the
        // decision-badge scan is independent of the notification scan above.
        if should_sync_notifications(self.last_decision_sync, notif_now, DECISION_SYNC_INTERVAL) {
            self.last_decision_sync = Some(notif_now);
            self.pending_decisions_total = sync_decision_badge_state(home, &mut self.ui.layout);
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
                flush_idle_notifications(home, &mut self.ui.layout);
                if let Ok(mut guard) = LAST_FLUSH.lock() {
                    *guard = Some(now);
                }
            }
        }
    }

    pub(super) fn render_frame(
        &mut self,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) -> Result<()> {
        let boot_start = self.boot_start;
        let attaches_expected = self.attaches_expected;
        let AppDeps {
            home,
            registry,
            telegram_status,
            daemon_binary_stale,
            size_debug,
            ..
        } = *deps;
        let repeat_mode = self.ui.key_handler.in_repeat();

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
                tabs = self.ui.layout.tabs.len(),
                "TUI draw size probe"
            );
        }

        // #t-84833-10 redraw-storm frame cap: draw at most once per FRAME_INTERVAL
        // and only when something changed (`self.dirty`). Input is NOT throttled — the
        // `event_rx` arm below processes keystrokes immediately; only the (expensive)
        // full-TUI redraw is rate-limited, which is what the boot flood saturated.
        let frame_now = std::time::Instant::now();
        if self.dirty && should_draw(self.last_draw, frame_now, FRAME_INTERVAL) {
            self.last_draw = Some(frame_now);
            self.dirty = false;
            // #freeze-4: during the bounded boot catch-up phase, drain the restart
            // flood with a per-frame TIME-capped drain — it clears the backlog fast
            // as a "loading" phase while still yielding to input every frame. Exit
            // to the steady path once all deferred attaches are applied AND every
            // pane's rx is drained, or after MAX_BOOT_CATCHUP (so boot can't hang).
            if self.booting {
                let backlog_remains =
                    render::drain_all_panes_until(&mut self.ui.layout, BOOT_FRAME_TIME_CAP);
                let timed_out = boot_start.elapsed() >= MAX_BOOT_CATCHUP;
                if (self.pending_fwd.is_empty() && !backlog_remains) || timed_out {
                    self.booting = false;
                    tracing::info!(
                        phase = "boot-catchup-complete",
                        elapsed_ms = boot_start.elapsed().as_millis() as u64,
                        attaches_expected = attaches_expected,
                        attaches_pending = self.pending_fwd.len(),
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
                render::drain_all_panes(&mut self.ui.layout);
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
                    &mut self.ui.layout,
                    repeat_mode,
                    registry,
                    telegram_status,
                    binary_stale,
                    self.pending_decisions_total,
                );
                // &mut because ScratchShell needs to drain output and maybe
                // resize its pane's VTerm/PTY during render.
                render_active_overlay(frame, &mut self.ui.overlay, &self.ui.layout, registry, home);
                // #freeze-4: loading indicator while the boot catch-up phase absorbs
                // the restart flood (so it reads as loading-with-progress, not a freeze).
                if self.booting {
                    render::render_boot_indicator(
                        frame,
                        attaches_expected.saturating_sub(self.pending_fwd.len()),
                        attaches_expected,
                    );
                }
            })?;
            // #freeze-4: while self.booting, keep cycling at frame cadence so the
            // time-capped catch-up runs every frame until the restart flood clears.
            // #freeze-2/#freeze-3: otherwise the budget-capped `drain_all_panes`
            // above may have left a backlog in a VISIBLE pane's channel — re-arm
            // `self.dirty` so the next frame continues the active tab's catch-up (the
            // select-timeout below shrinks to the frame boundary when self.dirty), clearing
            // over a few frames instead of one input-stalling mega-draw. Background
            // backlog needs no redraw: the loop's ≤50ms idle cadence + per-output
            // wakeups guarantee `drain_all_panes` runs again to bound every pane's `rx`.
            if self.booting || render::active_tab_has_pending_output(&self.ui.layout) {
                self.dirty = true;
            }
        }
        Ok(())
    }

    pub(super) fn select_timeout(&self) -> std::time::Duration {
        // #t-84833-10: wake the loop at the next frame boundary when a change is
        // pending but throttled (so it never stays stale beyond one frame), else
        // keep the cheap ~50ms idle refresh cadence (catches non-wakeup state like
        // status-bar / notification updates).
        if self.dirty {
            match self.last_draw {
                Some(t) => FRAME_INTERVAL
                    .saturating_sub(t.elapsed())
                    .max(std::time::Duration::from_millis(1)),
                None => std::time::Duration::from_millis(1),
            }
        } else {
            std::time::Duration::from_millis(50)
        }
    }

    pub(super) fn handle_restart_request(
        &mut self,
        req: Result<crate::api::app_restart::AppRestartRequest, crossbeam_channel::RecvError>,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps {
            app_restart_gate, ..
        } = *deps;
        if let Ok(req) = req {
            use crate::api::app_restart::AppRestartVerdict;
            if self.restart.restart_probe.is_some() {
                // Defensive: the gate should prevent this. Reject without
                // disturbing the in-flight probe's claim.
                let _ = req.reply.send(AppRestartVerdict::Aborted(
                    "a restart preflight is already in flight".into(),
                ));
            } else {
                match spawn_restart_probe() {
                    Ok(child) => {
                        self.restart.restart_probe = Some(RestartProbe {
                            child,
                            reply: req.reply,
                            flush_ack: req.flush_ack,
                            deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
                        });
                    }
                    Err(e) => {
                        app_restart_gate.abort_to_serving();
                        let _ = req.reply.send(AppRestartVerdict::Aborted(format!(
                            "could not start preflight probe: {e}"
                        )));
                    }
                }
            }
        }
    }

    pub(super) fn handle_crossterm_event(
        &mut self,
        ev: Result<Event, crossbeam_channel::RecvError>,
        terminal: &mut DefaultTerminal,
        deps: &AppDeps<'_>,
    ) -> LoopFlow {
        let AppDeps {
            home,
            fleet_path,
            registry,
            wakeup_tx,
            ..
        } = *deps;
        // #2453: loop-stable shared deps for handle_key_event/handle_mouse_event.
        let ui_deps = UiDeps {
            registry,
            home,
            fleet_path,
            wakeup_tx,
        };
        let ui_deps = &ui_deps;
        self.dirty = true; // input may change the display → redraw next due frame
        let ev = match ev {
            Ok(e) => e,
            Err(_) => return LoopFlow::Break,
        };
        match ev {
            // Windows crossterm emits both Press and Release events for every key;
            // Unix only emits Press. Ignoring non-Press avoids every keystroke
            // firing the handler twice (breaks Ctrl+B prefix state, etc.).
            Event::Key(key) if key.kind != KeyEventKind::Press => {}
            Event::Key(key) => {
                // #2453: former inline overlay/dispatch branch, now behind UiState.
                let out = self.ui.handle_key_event(key, ui_deps);
                if out.needs_resize {
                    self.needs_resize = true;
                }
                if out.should_break {
                    return LoopFlow::Break;
                }
            }
            // Overlays (Help, Tasks, Decisions, Command palette, rename,
            // etc.) are modal — mouse events must not reach hidden panes,
            // otherwise drag/selection state accumulates on panes the user
            // can't see. Swallow mouse events while any overlay is active.
            // #2453: overlays are modal — mouse must not reach hidden
            // panes. The modal-swallow guard + mouse routing now live in
            // UiState::handle_mouse_event (mirrors the Event::Key branch).
            Event::Mouse(mouse_evt) => {
                if self.ui.handle_mouse_event(mouse_evt, ui_deps) {
                    self.needs_resize = true;
                }
            }
            Event::Paste(text) => {
                match &mut self.ui.overlay {
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
                        pane.write_input(registry, text.as_bytes());
                    }
                    Overlay::None => {
                        write_to_focused(home, &mut self.ui.layout, registry, text.as_bytes());
                    }
                    _ => {} // ignore paste in non-input overlays
                }
            }
            Event::Resize(cols, rows) => {
                let pane_area = ratatui::layout::Rect::new(0, 1, cols, rows.saturating_sub(2));
                crate::layout::resize_panes(pane_area, &mut self.ui.layout, registry);
                // #1140: an interactive terminal resize reflows pane widths
                // (the wide→narrow transition that leaves stale wide-char
                // spacer cells in ratatui's Buffer::diff), so force the same
                // full clear the self.needs_resize path performs — otherwise the
                // ghost artifacts reappear after the resize.
                let _ = terminal.clear();
            }
            _ => {}
        }
        LoopFlow::Continue
    }

    pub(super) fn handle_attach_outcome(
        &mut self,
        outcome: Result<pane_factory::AttachOutcome, crossbeam_channel::RecvError>,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps {
            home,
            registry,
            wakeup_tx,
            ..
        } = *deps;
        self.dirty = true;
        // #render-first phase-(b): a background worker finished a deferred
        // attach. Apply it to its placeholder pane (instance id + dump +
        // forwarder on Ready, or a visible "failed to start" banner on
        // Failed). If the pane was closed before the attach completed, drop
        // the outcome. The keepalive `attach_tx` keeps this arm from
        // busy-spinning after all workers exit.
        if let Ok(outcome) = outcome {
            let pane_id = outcome.pane_id();
            if let Some(fwd_tx) = self.pending_fwd.remove(&pane_id) {
                if let Some(pane) = self.ui.layout.find_pane_mut(pane_id) {
                    pane_factory::apply_attach_outcome(pane, registry, outcome, fwd_tx, wakeup_tx);
                } else if let pane_factory::AttachOutcome::Ready { name, .. } = &outcome {
                    // F1 (r4): the pane was closed while the attach was in
                    // flight → the agent is already spawned + registered with
                    // no host pane. Kill it so it doesn't run orphaned until
                    // quit (fwd_tx already removed above → forwarder/rx drop).
                    kill_agent(home, registry, name);
                }
            }
        }
    }

    pub(super) fn handle_tui_event(
        &mut self,
        ev: Result<TuiEvent, crossbeam_channel::RecvError>,
        deps: &AppDeps<'_>,
    ) {
        let AppDeps {
            registry,
            wakeup_tx,
            ..
        } = *deps;
        self.dirty = true;
        if let Ok(event) = ev {
            tui_events::handle_tui_event(event, &mut self.ui.layout, registry, wakeup_tx);
            self.needs_resize = true;
        }
    }

    pub(super) fn handle_idle_tick(&mut self, deps: &AppDeps<'_>) {
        let AppDeps {
            home,
            fleet_path,
            wakeup_tx,
            attached_run_dir,
            ..
        } = *deps;
        // #t-84833-10: periodic idle refresh — mark self.dirty so the cap above
        // redraws (catches non-wakeup state changes; ~50ms cadence when idle).
        self.dirty = true;
        // #1479: throttled, change-gated session persistence (every
        // 10s). Cheap when the layout is unchanged (no write); on a
        // change it preserves the on-screen layout against a hard
        // crash. Graceful exit still saves unconditionally below.
        if self.last_session_save.elapsed() >= std::time::Duration::from_secs(10) {
            self.last_session_save = std::time::Instant::now();
            session::save_session_if_changed(home, &self.ui.layout, &mut self.last_session_json);
        }
        // Periodic redraw for state updates. In Attached mode, also
        // poll the daemon's `*.port` directory every 2s and open a
        // tab for each newly-appeared remote agent (hot-reload
        // Phase C). Matches the daemon's add-only policy: removed
        // agents are logged but their panes stay put so the user's
        // scrollback isn't destroyed mid-session.
        if attached_run_dir.is_some()
            && self.last_remote_sync.elapsed() >= std::time::Duration::from_secs(2)
        {
            {
                // #910 PR3 of 4: daemon-registry truth via runtime
                // helper. The state-transition log gate inside the
                // helper keeps this 2s-cadence call from spamming
                // daemon.log — only Live↔Fallback transitions
                // emit (validated by `runtime::tests::
                // helper_steady_state_does_not_log_per_call`).
                let current: std::collections::HashSet<String> =
                    crate::runtime::list_agents_with_fallback(home)
                        .into_iter()
                        .collect();
                let mut to_add: Vec<String> = current
                    .difference(&self.known_remote_agents)
                    .cloned()
                    .collect();
                to_add.sort();
                for name in &to_add {
                    let (dc, dr) = crossterm::terminal::size().unwrap_or((120, 40));
                    match pane_factory::create_remote_pane(
                        name,
                        home,
                        fleet_path,
                        &mut self.ui.layout,
                        dc.saturating_sub(2),
                        dr.saturating_sub(4),
                        wakeup_tx,
                    ) {
                        Ok(pane) => {
                            let tab_name = pane.agent_name.clone();
                            self.known_remote_agents.insert(tab_name.to_string());
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
                                self.ui.layout.single_pane_tab_index_for_agent(&tab_name)
                            {
                                self.ui.layout.tabs[idx] =
                                    crate::layout::Tab::new(tab_name.to_string(), pane);
                                tracing::info!(
                                    agent = %name,
                                    "reused retained tab for re-appeared remote agent (no duplicate)"
                                );
                            } else {
                                self.ui
                                    .layout
                                    .push_tab_preserve_focus(crate::layout::Tab::new(
                                        tab_name.to_string(),
                                        pane,
                                    ));
                                tracing::info!(
                                    agent = %name,
                                    "opened tab for newly-appeared remote agent"
                                );
                            }
                            self.needs_resize = true;
                        }
                        Err(e) => tracing::warn!(
                            agent = %name,
                            error = %e,
                            "remote pane attach failed during sync",
                        ),
                    }
                }
                let gone: Vec<String> = self
                    .known_remote_agents
                    .difference(&current)
                    .cloned()
                    .collect();
                for name in &gone {
                    tracing::warn!(
                        agent = %name,
                        "daemon-side agent gone; pane retained with stale output",
                    );
                    self.known_remote_agents.remove(name);
                }
                self.last_remote_sync = std::time::Instant::now();
            }
        }
    }

    /// Per-iteration housekeeping before the select!: scratch-shell reap,
    /// pending resize, and the badge/flush sync throttles.
    pub(super) fn pre_select(&mut self, terminal: &mut DefaultTerminal, deps: &AppDeps<'_>) {
        self.close_dead_scratch_shell(deps);
        self.apply_pending_resize(terminal, deps);
        self.sync_badges(deps);
    }

    /// Wakeup from PTY output — drain the whole burst so N output chunks
    /// coalesce into ONE redraw (the frame cap bounds the actual rate).
    pub(super) fn handle_wakeup(&mut self, wakeup_rx: &crossbeam_channel::Receiver<usize>) {
        while wakeup_rx.try_recv().is_ok() {}
        self.dirty = true;
    }

    /// One owned-mode maintenance tick (see `run_owned_maintenance_tick`).
    pub(super) fn handle_maintenance_tick(
        &mut self,
        app_cycle: &Option<crate::daemon::owned_maintenance::OwnedMaintenanceCycle>,
        deps: &AppDeps<'_>,
        app_externals: &crate::agent::ExternalRegistry,
        app_configs: &crate::api::ConfigRegistry,
    ) {
        let AppDeps { home, registry, .. } = *deps;
        self.dirty = true;
        run_owned_maintenance_tick(app_cycle, home, registry, app_externals, app_configs);
    }
}
