//! #3627: confirmed-retirement pane removal. Panes are removed only on
//! definitive proof — an exact `InstanceDeleted` event or a poll fallback that
//! agrees twice at one daemon generation. Transient disconnects, daemon
//! fallback, and fleet read failures must all retain the pane.
//!
//! Kept outside `app_state.rs` so the production owner stays below the
//! repository's anti-monolith source-size ceiling.

use super::*;
use crate::layout::Tab;
use crate::types::InstanceRef;

/// A valid fleet file so the "fleet + registry read success" precondition of
/// the poll fallback holds.
const VALID_FLEET: &str = "instances:\n  solo:\n    backend: shell\n  gone:\n    backend: shell\n  kept:\n    backend: shell\n";

struct RetirementDeps {
    base: std::path::PathBuf,
    home: std::path::PathBuf,
    run_dir: Option<std::path::PathBuf>,
    fleet_path: std::path::PathBuf,
    registry: AgentRegistry,
    wakeup_tx: crossbeam_channel::Sender<usize>,
    _wakeup_rx: crossbeam_channel::Receiver<usize>,
    app_restart_gate: crate::api::app_restart::AppRestartGate,
    daemon_binary_stale: Arc<std::sync::atomic::AtomicBool>,
    task_rpc_tx: crossbeam_channel::Sender<rpc::TaskRequest>,
    _task_rpc_rx: crossbeam_channel::Receiver<rpc::TaskRequest>,
    remote_state_rpc_tx: crossbeam_channel::Sender<rpc::AgentStateRequest>,
    _remote_state_rpc_rx: crossbeam_channel::Receiver<rpc::AgentStateRequest>,
    remote_restart_request_tx: crossbeam_channel::Sender<commands::RemoteRestartRequest>,
    _remote_restart_request_rx: crossbeam_channel::Receiver<commands::RemoteRestartRequest>,
    remote_restart_worker_tx: crossbeam_channel::Sender<commands::RemoteRestartRequest>,
    _remote_restart_worker_rx: crossbeam_channel::Receiver<commands::RemoteRestartRequest>,
}

impl RetirementDeps {
    fn new(tag: &str) -> Self {
        Self::with_fleet_and_home(tag, VALID_FLEET, false)
    }

    /// `home_as_file` makes `home/session.json` unwritable on every platform
    /// (the parent is a regular file), exercising the session-flush failure
    /// path without unix-only permission APIs.
    fn with_fleet_and_home(tag: &str, fleet_yaml: &str, home_as_file: bool) -> Self {
        let base = std::env::temp_dir().join(format!(
            "agend-test-3627-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp base");
        let home = base.join("home");
        if home_as_file {
            std::fs::write(&home, b"not a directory").expect("home file");
        } else {
            std::fs::create_dir_all(&home).expect("home dir");
        }
        let run_dir = base.join("run");
        std::fs::create_dir_all(&run_dir).expect("run dir");
        std::fs::write(run_dir.join(".daemon"), "4242:1700000000:999").expect(".daemon identity");
        let fleet_path = base.join("fleet.yaml");
        std::fs::write(&fleet_path, fleet_yaml).expect("fleet.yaml");
        let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
        let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded();
        let (remote_state_rpc_tx, _remote_state_rpc_rx) = crossbeam_channel::unbounded();
        let (remote_restart_request_tx, _remote_restart_request_rx) =
            crossbeam_channel::unbounded();
        let (remote_restart_worker_tx, _remote_restart_worker_rx) = crossbeam_channel::unbounded();
        Self {
            base,
            home,
            run_dir: Some(run_dir),
            fleet_path,
            registry: Arc::new(Mutex::new(HashMap::new())),
            wakeup_tx,
            _wakeup_rx,
            app_restart_gate: crate::api::app_restart::AppRestartGate::new(),
            daemon_binary_stale: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            task_rpc_tx,
            _task_rpc_rx,
            remote_state_rpc_tx,
            _remote_state_rpc_rx,
            remote_restart_request_tx,
            _remote_restart_request_rx,
            remote_restart_worker_tx,
            _remote_restart_worker_rx,
        }
    }

    fn deps(&self) -> AppDeps<'_> {
        AppDeps {
            home: &self.home,
            fleet_path: &self.fleet_path,
            registry: &self.registry,
            wakeup_tx: &self.wakeup_tx,
            app_restart_gate: &self.app_restart_gate,
            daemon_binary_stale: &self.daemon_binary_stale,
            telegram_status: TelegramStatus::NotConfigured,
            attached_run_dir: &self.run_dir,
            attached_mode: true,
            size_debug: false,
            task_rpc_tx: &self.task_rpc_tx,
            remote_state_rpc_tx: &self.remote_state_rpc_tx,
            remote_restart_request_tx: &self.remote_restart_request_tx,
            remote_restart_worker_tx: &self.remote_restart_worker_tx,
        }
    }
}

impl Drop for RetirementDeps {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

fn state_with_pane(name: &str, reference: InstanceRef) -> AppState {
    let mut state = AppState::new();
    let mut pane = test_remote_pane(&mut state.ui.layout, name).expect("test pane");
    pane.instance_ref = Some(reference);
    state.ui.layout.add_tab(Tab::new(name.to_string(), pane));
    state
}

fn live_result(entries: &[(&str, InstanceRef)]) -> rpc::AgentStateSnapshotResult {
    let mut names = std::collections::HashSet::new();
    let mut instance_refs = HashMap::new();
    for (name, reference) in entries {
        names.insert((*name).to_string());
        instance_refs.insert((*name).to_string(), *reference);
    }
    rpc::AgentStateSnapshotResult {
        snapshot: HashMap::new(),
        names,
        instance_refs,
        mode: crate::runtime::AgentListMode::Live,
    }
}

fn poll_once(state: &mut AppState, deps: &AppDeps<'_>, entries: &[(&str, InstanceRef)]) {
    state.handle_agent_state_rpc_outcome(Ok(Ok(live_result(entries))));
    state.reconcile_pending_remote_roster(deps);
}

fn new_ref(generation: u64) -> InstanceRef {
    InstanceRef::new(crate::types::InstanceId::new(), generation)
}

// ── pure policy ──────────────────────────────────────────────────────────

#[test]
fn single_snapshot_never_confirms_retirement_3627() {
    let reference = new_ref(1);
    let rendered = vec![reference];
    let mut candidates = HashMap::new();
    let confirmed = confirmed_retirements(
        &mut candidates,
        &rendered,
        &std::collections::HashSet::new(),
        "gen-a",
        &std::collections::HashSet::new(),
    );
    assert!(
        confirmed.is_empty(),
        "one absent snapshot is not definitive proof"
    );
    assert_eq!(candidates[&reference].confirmations, 1);
}

#[test]
fn two_consecutive_same_generation_snapshots_confirm_3627() {
    let reference = new_ref(1);
    let rendered = vec![reference];
    let mut candidates = HashMap::new();
    let live = std::collections::HashSet::new();
    let restart = std::collections::HashSet::new();
    assert!(confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart).is_empty());
    assert_eq!(
        confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart),
        vec![reference],
        "two consecutive same-generation absences confirm retirement"
    );
}

#[test]
fn generation_change_resets_confirmation_3627() {
    let reference = new_ref(1);
    let rendered = vec![reference];
    let mut candidates = HashMap::new();
    let live = std::collections::HashSet::new();
    let restart = std::collections::HashSet::new();
    assert!(confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart).is_empty());
    assert!(
        confirmed_retirements(&mut candidates, &rendered, &live, "gen-b", &restart).is_empty(),
        "a daemon generation change must not inherit the previous confirmations"
    );
    assert_eq!(candidates[&reference].confirmations, 1);
}

#[test]
fn reappearance_and_restart_in_flight_retain_3627() {
    let reference = new_ref(1);
    let rendered = vec![reference];
    let mut candidates = HashMap::new();
    let restart = std::collections::HashSet::new();
    let empty = std::collections::HashSet::new();
    assert!(
        confirmed_retirements(&mut candidates, &rendered, &empty, "gen-a", &restart).is_empty()
    );
    // Reappears in the live roster → candidate dropped, never confirmed.
    let live = std::collections::HashSet::from([reference]);
    assert!(confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart).is_empty());
    assert!(!candidates.contains_key(&reference));
    // Absent again but a restart is in flight → retained.
    let in_flight = std::collections::HashSet::from([reference]);
    assert!(
        confirmed_retirements(&mut candidates, &rendered, &empty, "gen-a", &in_flight).is_empty()
    );
    assert!(!candidates.contains_key(&reference));
}

#[test]
fn candidate_is_purged_when_ref_leaves_the_layout_3627() {
    let reference = new_ref(1);
    let mut candidates = HashMap::new();
    let live = std::collections::HashSet::new();
    let restart = std::collections::HashSet::new();
    confirmed_retirements(&mut candidates, &[reference], &live, "gen-a", &restart);
    assert!(candidates.contains_key(&reference));
    confirmed_retirements(&mut candidates, &[], &live, "gen-a", &restart);
    assert!(
        candidates.is_empty(),
        "a ref no longer rendered must not keep a retirement candidate"
    );
}

#[test]
fn duplicate_rendered_ref_does_not_fast_forward_confirmation_3627() {
    let reference = new_ref(1);
    let rendered = vec![reference, reference];
    let mut candidates = HashMap::new();
    let live = std::collections::HashSet::new();
    let restart = std::collections::HashSet::new();
    assert!(
        confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart).is_empty(),
        "two panes sharing one ref are still one snapshot of one ref"
    );
    assert_eq!(
        confirmed_retirements(&mut candidates, &rendered, &live, "gen-a", &restart),
        vec![reference]
    );
}

// ── poll fallback (Live, definitive) ─────────────────────────────────────

#[test]
fn poll_fallback_removes_mixed_and_target_only_tabs_after_confirmation_3627() {
    let deps = RetirementDeps::new("poll-remove");
    let solo = new_ref(1);
    let gone = new_ref(2);
    let kept = new_ref(3);
    let mut layout = Layout::new();
    let mut state = AppState::new();
    let mut solo_pane = test_remote_pane(&mut layout, "solo").expect("solo pane");
    solo_pane.instance_ref = Some(solo);
    layout.add_tab(Tab::new("solo".into(), solo_pane));
    let mut gone_pane = test_remote_pane(&mut layout, "gone").expect("gone pane");
    gone_pane.instance_ref = Some(gone);
    let mut kept_pane = test_remote_pane(&mut layout, "kept").expect("kept pane");
    kept_pane.instance_ref = Some(kept);
    layout.add_tab(Tab::new("team".into(), gone_pane));
    layout.tabs[1].split_focused(crate::layout::SplitDir::Vertical, kept_pane);
    layout.active = 0;
    state.ui.layout = layout;
    // "kept" is already known, so the roster tick does not attempt an attach.
    state.known_remote_agents.insert("kept".to_string());

    let live = [("kept", kept)];
    poll_once(&mut state, &deps.deps(), &live);
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some()
            && state.ui.layout.find_agent_pane("gone").is_some(),
        "a single absent snapshot must retain both panes"
    );

    poll_once(&mut state, &deps.deps(), &live);
    assert!(
        state.ui.layout.find_agent_pane("solo").is_none(),
        "a target-only tab must close after confirmed retirement"
    );
    assert!(
        state.ui.layout.find_agent_pane("gone").is_none(),
        "the retired pane must leave the mixed tab"
    );
    assert!(
        state.ui.layout.find_agent_pane("kept").is_some(),
        "the still-live pane in the mixed tab must survive"
    );
    assert_eq!(
        state.ui.layout.tabs.len(),
        1,
        "mixed tab retained, solo closed"
    );
}

#[test]
fn poll_fallback_retains_on_fleet_parse_failure_3627() {
    let deps = RetirementDeps::with_fleet_and_home(
        "poll-fleet-fail",
        "instances: [ this is not valid yaml",
        false,
    );
    let reference = new_ref(9);
    let mut state = state_with_pane("solo", reference);

    for _ in 0..3 {
        poll_once(&mut state, &deps.deps(), &[]);
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "an unreadable fleet means config deletion cannot be proven — retain"
    );
    assert!(state.retirement_candidates.is_empty());
}

#[test]
fn poll_fallback_retains_instance_still_defined_in_fleet_3627() {
    let deps = RetirementDeps::new("poll-configured");
    // The pane carries the exact instance identity the fleet config declares,
    // so a momentary registry absence is a spawn/bridge gap, not retirement.
    let fleet = crate::fleet::FleetConfig::load(&deps.fleet_path).expect("fleet config");
    let id = fleet
        .instances
        .get("solo")
        .and_then(|instance| instance.id.as_deref())
        .and_then(crate::types::InstanceId::parse)
        .expect("solo has a backfilled id");
    let mut state = state_with_pane("solo", InstanceRef::new(id, 3));

    for _ in 0..3 {
        poll_once(&mut state, &deps.deps(), &[]);
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "a still-configured instance must be retained until its config is deleted"
    );
    assert!(state.retirement_candidates.is_empty());
}

#[test]
fn poll_fallback_retains_on_daemon_fallback_3627() {
    let deps = RetirementDeps::new("poll-fallback");
    let reference = new_ref(4);
    let mut state = state_with_pane("solo", reference);

    for _ in 0..3 {
        state.handle_agent_state_rpc_outcome(Ok(Err(rpc::AgentStateError {
            error: "daemon stuck".into(),
            mode: crate::runtime::AgentListMode::FallbackDaemonStuck,
        })));
        state.reconcile_pending_remote_roster(&deps.deps());
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "daemon fallback is never definitive retirement proof"
    );
    assert!(state.retirement_candidates.is_empty());
}

#[test]
fn transient_disconnect_retains_pane_3627() {
    let deps = RetirementDeps::new("poll-transient");
    let reference = new_ref(5);
    let mut state = state_with_pane("solo", reference);
    {
        let (_, pane_id) = state
            .ui
            .layout
            .find_agent_pane("solo")
            .expect("pane present");
        state.ui.layout.tabs[0]
            .root_mut()
            .find_pane_mut(pane_id)
            .expect("pane")
            .mark_disconnected();
    }

    // The instance is still in the live roster — only the bridge is down.
    for _ in 0..3 {
        poll_once(&mut state, &deps.deps(), &[("solo", reference)]);
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "a disconnected-but-live instance must be retained for reconnect"
    );
    assert!(state.retirement_candidates.is_empty());
}

// ── immediate event path ─────────────────────────────────────────────────

fn delete_event(sequence: u64, name: &str, reference: InstanceRef) -> rpc::EventStreamOutcome {
    rpc::EventStreamOutcome::Event(crate::daemon::event_hub::DaemonEvent {
        source: "daemon".into(),
        sequence,
        event: crate::api::ApiEvent::InstanceDeleted {
            name: name.to_string(),
            instance_ref: Some(reference),
            restart_id: None,
        },
    })
}

#[test]
fn exact_delete_removes_mixed_and_target_only_and_flushes_session_3627() {
    let deps = RetirementDeps::new("event-remove");
    let solo = new_ref(11);
    let gone = new_ref(12);
    let kept = new_ref(13);
    let mut layout = Layout::new();
    let mut state = AppState::new();
    let mut solo_pane = test_remote_pane(&mut layout, "solo").expect("solo pane");
    solo_pane.instance_ref = Some(solo);
    layout.add_tab(Tab::new("solo".into(), solo_pane));
    let mut gone_pane = test_remote_pane(&mut layout, "gone").expect("gone pane");
    gone_pane.instance_ref = Some(gone);
    let mut kept_pane = test_remote_pane(&mut layout, "kept").expect("kept pane");
    kept_pane.instance_ref = Some(kept);
    layout.add_tab(Tab::new("team".into(), gone_pane));
    layout.tabs[1].split_focused(crate::layout::SplitDir::Vertical, kept_pane);
    layout.active = 0;
    state.ui.layout = layout;

    state.handle_event_stream_outcome(Ok(delete_event(1, "gone", gone)), &deps.deps());
    state.handle_event_stream_outcome(Ok(delete_event(2, "solo", solo)), &deps.deps());

    assert!(state.ui.layout.find_agent_pane("gone").is_none());
    assert!(state.ui.layout.find_agent_pane("solo").is_none());
    assert!(state.ui.layout.find_agent_pane("kept").is_some());
    assert_eq!(state.ui.layout.tabs.len(), 1);

    // Successful commit flushes the session immediately (no 10s throttle).
    let session = std::fs::read_to_string(deps.home.join("session.json")).expect("session flushed");
    assert!(!session.contains(&gone.instance_id.full()));
    assert!(!session.contains(&solo.instance_id.full()));
}

#[test]
fn duplicate_and_late_delete_events_are_idempotent_and_ref_scoped_3627() {
    let deps = RetirementDeps::new("event-idempotent");
    let old_ref = new_ref(21);
    let successor = new_ref(22);
    let mut state = state_with_pane("agent", successor);

    // A late event for the OLD incarnation must not touch the new pane.
    state.handle_event_stream_outcome(Ok(delete_event(1, "agent", old_ref)), &deps.deps());
    assert!(
        state.ui.layout.find_agent_pane("agent").is_some(),
        "a stale deletion for an old ref must not remove the successor pane"
    );

    // Duplicate exact deletes: first removes, second is a harmless no-op.
    state.handle_event_stream_outcome(Ok(delete_event(2, "agent", successor)), &deps.deps());
    state.handle_event_stream_outcome(Ok(delete_event(3, "agent", successor)), &deps.deps());
    assert!(state.ui.layout.find_agent_pane("agent").is_none());
}

#[test]
fn session_write_failure_surfaces_retryable_notice_3627() {
    let deps = RetirementDeps::with_fleet_and_home("event-write-fail", VALID_FLEET, true);
    let reference = new_ref(31);
    let mut state = state_with_pane("agent", reference);

    state.handle_event_stream_outcome(Ok(delete_event(1, "agent", reference)), &deps.deps());

    assert!(state.ui.layout.find_agent_pane("agent").is_none());
    match &state.ui.overlay {
        Overlay::ReconnectNotice { message } => assert!(
            message.contains("retry"),
            "the notice must tell the operator retirement will be retried: {message}"
        ),
        _ => panic!("expected a retryable retirement notice on session write failure"),
    }
}

// ── team semantics ───────────────────────────────────────────────────────

#[test]
fn team_member_removed_from_team_retains_pane_3627() {
    let deps = RetirementDeps::new("team-member");
    let reference = new_ref(41);
    let mut state = state_with_pane("m1", reference);

    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Event(
            crate::daemon::event_hub::DaemonEvent {
                source: "daemon".into(),
                sequence: 1,
                event: crate::api::ApiEvent::TeamMembersChanged {
                    name: "svc".into(),
                    added: Vec::new(),
                    removed: vec!["m1".into()],
                },
            },
        )),
        &deps.deps(),
    );
    assert!(
        state.ui.layout.find_agent_pane("m1").is_some(),
        "being removed from a team is not instance retirement"
    );
}

#[test]
fn team_cascade_delete_cleans_each_exact_ref_3627() {
    let deps = RetirementDeps::new("team-cascade");
    let m1 = new_ref(51);
    let m2 = new_ref(52);
    let mut layout = Layout::new();
    let mut state = AppState::new();
    let mut p1 = test_remote_pane(&mut layout, "m1").expect("m1 pane");
    p1.instance_ref = Some(m1);
    let mut p2 = test_remote_pane(&mut layout, "m2").expect("m2 pane");
    p2.instance_ref = Some(m2);
    layout.add_tab(Tab::new("svc".into(), p1));
    layout.tabs[0].split_focused(crate::layout::SplitDir::Vertical, p2);
    state.ui.layout = layout;

    // The daemon's team cascade emits one exact InstanceDeleted per member.
    state.handle_event_stream_outcome(Ok(delete_event(1, "m1", m1)), &deps.deps());
    assert!(state.ui.layout.find_agent_pane("m1").is_none());
    assert!(state.ui.layout.find_agent_pane("m2").is_some());
    state.handle_event_stream_outcome(Ok(delete_event(2, "m2", m2)), &deps.deps());
    assert!(
        state.ui.layout.tabs.is_empty(),
        "the target-only team tab must close once every member is retired"
    );
}

// ── #3627 review hardening ───────────────────────────────────────────────

fn poll_snapshot(
    state: &mut AppState,
    deps: &AppDeps<'_>,
    snapshot: rpc::AgentStateSnapshotResult,
) {
    state.handle_agent_state_rpc_outcome(Ok(Ok(snapshot)));
    state.reconcile_pending_remote_roster(deps);
}

/// A retirement flush can fail while an unrelated overlay is open. The notice
/// must be queued and then delivered once the overlay clears — never lost.
#[test]
fn retirement_notice_queued_while_overlay_open_surfaces_after_3627() {
    let deps = RetirementDeps::with_fleet_and_home("notice-queued", VALID_FLEET, true);
    let reference = new_ref(71);
    let mut state = state_with_pane("agent", reference);
    state.ui.overlay = Overlay::Help;

    state.handle_event_stream_outcome(Ok(delete_event(1, "agent", reference)), &deps.deps());

    assert!(state.ui.layout.find_agent_pane("agent").is_none());
    assert!(
        matches!(state.ui.overlay, Overlay::Help),
        "an already-open overlay must not be clobbered"
    );
    assert!(
        state.pending_retirement_notice.is_some(),
        "the retryable notice must be queued, not silently dropped"
    );

    // The operator closes the overlay: the queued notice is delivered.
    state.ui.overlay = Overlay::None;
    state.promote_pending_retirement_notice();
    match &state.ui.overlay {
        Overlay::ReconnectNotice { message } => assert!(
            message.contains("retry"),
            "queued notice must still name the retry: {message}"
        ),
        _ => panic!("queued retirement notice must surface once the overlay clears"),
    }
    assert!(state.pending_retirement_notice.is_none());
}

/// A daemon that drops `instance_ref` fields must not be read as "every ref is
/// gone" — that would mis-retire panes after two polls. Fail toward retention.
#[test]
fn poll_fallback_retains_when_snapshot_lacks_refs_3627() {
    let deps = RetirementDeps::new("snapshot-no-refs");
    let reference = new_ref(81);
    let mut state = state_with_pane("solo", reference);

    // (a) Names present, every ref missing (daemon downgrade/regression).
    for _ in 0..3 {
        let mut snapshot = live_result(&[("ghost", new_ref(82))]);
        snapshot.instance_refs.clear();
        poll_snapshot(&mut state, &deps.deps(), snapshot);
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "a ref-less snapshot cannot prove any exact ref is absent"
    );
    assert!(state.retirement_candidates.is_empty());

    // (b) Partial refs: one unparseable live name is still not trustworthy.
    let parseable = new_ref(83);
    for _ in 0..3 {
        let mut snapshot = live_result(&[("ghost", parseable), ("opaque", new_ref(84))]);
        snapshot.instance_refs.remove("opaque");
        poll_snapshot(&mut state, &deps.deps(), snapshot);
    }
    assert!(
        state.ui.layout.find_agent_pane("solo").is_some(),
        "a partially-ref'd snapshot must not be treated as authoritative"
    );
    assert!(state.retirement_candidates.is_empty());
}

/// Hard restart: a retired pane still present in the on-disk session (because
/// the flush failed and only the retired ref was durably recorded) must not be
/// revived by the real session reload path.
#[test]
fn hard_restart_does_not_revive_retired_pane_3627() {
    let deps = RetirementDeps::new("hard-restart");
    let stale = new_ref(91);
    let live = new_ref(92);

    // Pre-crash session.json still holds BOTH panes.
    let mut seeded = Layout::new();
    let mut stale_pane = test_remote_pane(&mut seeded, "gone").expect("stale pane");
    stale_pane.instance_ref = Some(stale);
    let mut live_pane = test_remote_pane(&mut seeded, "kept").expect("live pane");
    live_pane.instance_ref = Some(live);
    seeded.add_tab(Tab::new("team".into(), stale_pane));
    seeded.tabs[0].split_focused(crate::layout::SplitDir::Vertical, live_pane);
    assert!(session::save_session(&deps.home, &seeded));

    let session_path = deps.home.join("session.json");
    crate::store::fail_next_atomic_write_for_test(&session_path);
    let mut state = AppState::new();
    state.ui.layout = seeded;
    state.handle_event_stream_outcome(Ok(delete_event(1, "gone", stale)), &deps.deps());

    // The flush failed, so the stale leaf is still on disk — but the retired
    // ref is recorded durably.
    let on_disk = std::fs::read_to_string(&session_path).expect("session.json remains");
    assert!(
        on_disk.contains(&stale.instance_id.full()),
        "a failed flush leaves the stale leaf on disk for this test to exercise"
    );
    assert!(
        deps.home.join("session.retired.json").exists(),
        "the retired ref must be durably recorded when the session flush fails"
    );

    // HARD RESTART: reload the real session layout with a registry that STILL
    // names the retired instance (worst case). The retired ref must not return.
    let agent_source: std::collections::HashSet<String> = ["gone".to_string(), "kept".to_string()]
        .into_iter()
        .collect();
    let mut reloaded = Layout::new();
    let mut builder =
        |sp: &crate::app::session::SessionPane, layout: &mut Layout| match sp.instance_ref {
            Some(reference) if reference == stale => {
                let mut pane = test_remote_pane(layout, "gone").expect("stale builder");
                pane.instance_ref = Some(stale);
                Some(pane)
            }
            Some(reference) if reference == live => {
                let mut pane = test_remote_pane(layout, "kept").expect("live builder");
                pane.instance_ref = Some(live);
                Some(pane)
            }
            // Rule-3 synthetic entries carry no ref; no successor to build here.
            _ => None,
        };
    assert!(crate::app::session::apply_session_layout_for_test(
        &deps.home,
        &agent_source,
        &mut builder,
        &mut reloaded,
    ));
    assert!(
        reloaded.find_agent_pane("gone").is_none(),
        "a retired ref must not be revived from session.json after a hard restart"
    );
    assert!(
        reloaded.find_agent_pane("kept").is_some(),
        "the live sibling pane must still restore"
    );
}
