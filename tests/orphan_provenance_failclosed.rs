//! #3273 V1 — report-only orphan visibility and a non-authoritative
//! tool-call observation ledger.
//!
//! §3.10 test-first: every test here MUST fail before
//! `admin::orphan_provenance` lands and pass after. At the RED commit the
//! module does not exist, so this target compile-fails — the accepted RED
//! signature per the reviewer RED-protocol ("the new tests compile-fail, fail
//! at runtime, or fail with the expected error signature").
//!
//! ## Why this is report-only
//!
//! Decision `d-20260814163653190377-20` ordered V1 reporting before any kill
//! authority, and the adversarial addendum then showed with live evidence that
//! V2 attribution is not obtainable today:
//!
//! * **Codex** runs tool commands from an app-server SIBLING, not from a
//!   backend child — the backend has zero children to sample.
//! * **Claude** spawns recurring direct children that are not tool shells at
//!   all (a `caffeinate` reappears alongside the MCP servers), so "the unique
//!   new direct child" is routinely not unique.
//! * **Parallel** shell tool calls make the same window ambiguous by design.
//! * **OpenCode** topology is unknown.
//!
//! So this PR ships visibility, not authority. The ledger is an OBSERVATION
//! log: it may make a report more informative, and it may never make a process
//! killable. Every candidate the doctor lists is UNPROVEN, and there is no
//! production signalling path in this module at all — asserted structurally
//! below, not merely by omission.
//!
//! ## What V1 must actually do
//!
//! Report the real incident: twelve `PPID == 1` busy loops left by a tool shell
//! that died while its instance and backend stayed ALIVE. Backend liveness is
//! not a filter. Every miss — restart, dropped sample, sibling topology,
//! ambiguity — stays visible as an explicit UNPROVEN reason rather than a
//! silent zero, because a hygiene tool that quietly reports nothing is worse
//! than one that reports "I cannot tell".
//!
//! Heuristics (argv, cwd, age, CPU) are display columns. The RCA census found
//! 7 matches for the argv/cwd predicate on one machine, including a 44-day
//! `ngrok` and a 44-day `python3` that are load-bearing, and no age threshold
//! separates the leak from them.
//!
//! Sampling is owner-service/daemon-driven and must NOT hang off
//! `instance_monitor::collect()`, which is subscriber-gated by
//! `LAST_METRICS_READ`: a headless daemon, or a TUI not on the Monitor pane,
//! skips that sweep entirely.
//!
//! Determinism: no real processes, no sleeps. The process world is a
//! `FakeOracle` the test mutates directly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use syn::visit::{self, Visit};

use agend_terminal::admin::orphan_provenance::{
    classify, load_ledger, render_human, sample_tool_call_shells, BackendOwner, BackendOwnerSource,
    DisplayColumns, ObservationMiss, OrphanReport, PersistOutcome, ProcFacts, ProcessOracle,
    ProvenanceSupport, SampleOutcome, ShellToolEvidence, ShellToolKind, ToolCallProvenanceHandler,
    UnprovenReason,
};
use agend_terminal::daemon::per_tick::{run_handlers_with_panic_guard, PerTickHandler};
use agend_terminal::instructions::background_process_guidance;

const HOUR_MS: i64 = 3_600_000;
const OUR_UID: u32 = 501;
/// The session every backend-owned process shares unless something `setsid`s.
const BACKEND_SESSION: u32 = 63_542;

/// #3245: never wipe a shared fixture path on entry — one unique dir per test.
fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agend-3273-{}-{}-{}", tag, std::process::id(), id));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The ledger's canonical location. Pinned here because the fail-closed tests
/// must be able to make it unwritable; a rename is a contract change.
fn ledger_path(home: &Path) -> PathBuf {
    home.join("state").join("tool_call_observations.json")
}

// ---------------------------------------------------------------------------
// Fake process world
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeProc {
    ppid: Option<u32>,
    sid: Option<u32>,
    pgid: Option<u32>,
    uid: Option<u32>,
    start_token: Option<u64>,
    lstart_ms: Option<i64>,
    display: DisplayColumns,
}

impl FakeProc {
    /// Inherits the backend's session AND group — the shape nothing in this
    /// repo can rule out for an inner tool shell.
    fn inherited(token: u64) -> Self {
        Self {
            ppid: None,
            sid: Some(BACKEND_SESSION),
            pgid: Some(BACKEND_SESSION),
            uid: Some(OUR_UID),
            start_token: Some(token),
            lstart_ms: Some(0),
            display: DisplayColumns::default(),
        }
    }

    fn group_leader(pid: u32, token: u64) -> Self {
        Self {
            pgid: Some(pid),
            ..Self::inherited(token)
        }
    }

    fn session_leader(pid: u32, token: u64) -> Self {
        Self {
            sid: Some(pid),
            pgid: Some(pid),
            ..Self::inherited(token)
        }
    }

    fn child_of(mut self, ppid: u32) -> Self {
        self.ppid = Some(ppid);
        self
    }

    fn orphan(mut self) -> Self {
        self.ppid = Some(1);
        self
    }

    fn started_at(mut self, lstart_ms: i64) -> Self {
        self.lstart_ms = Some(lstart_ms);
        self
    }

    fn group(mut self, pgid: Option<u32>) -> Self {
        self.pgid = pgid;
        self
    }

    fn display(mut self, d: DisplayColumns) -> Self {
        self.display = d;
        self
    }
}

struct FakeOracle {
    support: ProvenanceSupport,
    procs: RefCell<HashMap<u32, FakeProc>>,
}

impl FakeOracle {
    fn supported() -> Self {
        Self {
            support: ProvenanceSupport::Supported,
            procs: RefCell::new(HashMap::new()),
        }
    }

    fn unsupported() -> Self {
        Self {
            support: ProvenanceSupport::Unsupported {
                platform: "windows",
                reason: "no POSIX session/reparent semantics; Job Objects are future work",
            },
            procs: RefCell::new(HashMap::new()),
        }
    }

    fn with(self, pid: u32, proc: FakeProc) -> Self {
        self.procs.borrow_mut().insert(pid, proc);
        self
    }

    fn insert(&self, pid: u32, proc: FakeProc) {
        self.procs.borrow_mut().insert(pid, proc);
    }

    fn reap(&self, pid: u32) {
        self.procs.borrow_mut().remove(&pid);
    }
}

impl ProcessOracle for FakeOracle {
    fn support(&self) -> ProvenanceSupport {
        self.support.clone()
    }

    fn facts(&self, pid: u32) -> Option<ProcFacts> {
        let proc = self.procs.borrow().get(&pid).cloned()?;
        Some(ProcFacts {
            pid,
            ppid: proc.ppid,
            sid: proc.sid,
            pgid: proc.pgid,
            uid: proc.uid,
            start_token: proc.start_token,
            lstart_ms: proc.lstart_ms,
            display: proc.display.clone(),
        })
    }

    fn children_of(&self, pid: u32) -> Vec<u32> {
        let mut kids: Vec<u32> = self
            .procs
            .borrow()
            .iter()
            .filter(|(_, p)| p.ppid == Some(pid))
            .map(|(child, _)| *child)
            .collect();
        kids.sort_unstable();
        kids
    }

    fn reparented(&self) -> Vec<u32> {
        if !matches!(self.support, ProvenanceSupport::Supported) {
            return Vec::new();
        }
        let mut orphans: Vec<u32> = self
            .procs
            .borrow()
            .iter()
            .filter(|(_, p)| p.ppid == Some(1))
            .map(|(pid, _)| *pid)
            .collect();
        orphans.sort_unstable();
        orphans
    }

    fn is_alive(&self, pid: u32) -> bool {
        self.procs.borrow().contains_key(&pid)
    }
}

/// The owner service: what the daemon knows about live backends and the tool
/// call each one is running. Deliberately carries NO metrics handle.
struct HeadlessOwners(Vec<BackendOwner>);

impl BackendOwnerSource for HeadlessOwners {
    fn live_backends(&self) -> Vec<BackendOwner> {
        self.0.clone()
    }
}

fn bash_tool(generation: u64, started_ms: i64) -> Option<ShellToolEvidence> {
    Some(ShellToolEvidence {
        kind: ShellToolKind::Bash,
        generation,
        started_ms,
    })
}

fn owner(instance: &str, backend_pid: u32, tool: Option<ShellToolEvidence>) -> BackendOwner {
    BackendOwner {
        instance: instance.to_string(),
        backend_pid,
        active_shell_tool: tool,
    }
}

fn baseline_tick(home: &Path, world: &FakeOracle, now_ms: i64) -> SampleOutcome {
    sample_tool_call_shells(home, world, &[owner("dev-1", 900, None)], now_ms)
}

fn miss(outcome: &SampleOutcome, instance: &str) -> ObservationMiss {
    outcome
        .misses
        .iter()
        .find(|(name, _)| name == instance)
        .map(|(_, m)| m.clone())
        .unwrap_or_else(|| panic!("instance {instance} must report a visible observation miss"))
}

fn candidate<'a>(
    report: &'a OrphanReport,
    pid: u32,
) -> &'a agend_terminal::admin::orphan_provenance::OrphanCandidate {
    report
        .candidates
        .iter()
        .find(|c| c.pid == pid)
        .unwrap_or_else(|| panic!("pid {pid} must be listed"))
}

fn reason(report: &OrphanReport, pid: u32) -> UnprovenReason {
    candidate(report, pid).unproven_reason.clone()
}

// ---------------------------------------------------------------------------
// 1. THE INCIDENT — reported while the backend is alive
// ---------------------------------------------------------------------------

/// #3273 itself: twelve busy loops reparented to init when their tool shell
/// died, while the instance and backend kept running. Backend liveness must not
/// filter anything, or V1 misses the case it exists for.
#[test]
fn the_twelve_orphaned_busy_loops_are_reported_while_the_backend_is_alive() {
    let home = tmp_home("incident");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    for i in 0..12u32 {
        world.insert(
            2_000 + i,
            FakeProc::inherited(20_000 + u64::from(i))
                .orphan()
                .group(Some(910))
                .started_at(0)
                .display(DisplayColumns {
                    argv: Some(format!("zsh -c ( while : ; do : ; done ) & # loop {i}")),
                    cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
                    elapsed_secs: Some(40 * 3_600),
                    cpu_percent: Some(96.0),
                }),
        );
    }

    let report = classify(&home, &world, 40 * HOUR_MS);

    assert!(world.is_alive(900), "precondition: the backend never died");
    let listed: Vec<u32> = report.candidates.iter().map(|c| c.pid).collect();
    assert_eq!(
        listed.len(),
        12,
        "all twelve orphans must be reported, backend liveness notwithstanding: {listed:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. Every candidate is UNPROVEN, and the report carries the decision columns
// ---------------------------------------------------------------------------

#[test]
fn every_candidate_is_unproven_and_carries_the_operator_decision_columns() {
    let home = tmp_home("columns");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    world.reap(910);
    world.insert(
        911,
        FakeProc::inherited(9_110)
            .orphan()
            .group(Some(910))
            .started_at(1_500)
            .display(DisplayColumns {
                argv: Some("sleep 100000".to_string()),
                cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
                elapsed_secs: Some(40 * 3_600),
                cpu_percent: Some(0.0),
            }),
    );

    let report = classify(&home, &world, 40 * HOUR_MS);
    let c = candidate(&report, 911);

    assert_eq!(c.start_token, Some(9_110));
    assert_eq!(c.lstart_ms, Some(1_500));
    assert_eq!(c.sid, Some(BACKEND_SESSION));
    assert_eq!(c.pgid, Some(910));
    assert_eq!(
        c.leader_alive,
        Some(false),
        "leader liveness is a decision column the operator needs"
    );
    assert!(c.argv.is_some() && c.cwd.is_some());
    assert_eq!(
        c.suggested_instance.as_deref(),
        Some("dev-1"),
        "an observation may SUGGEST an owner — that is help, not authority"
    );
}

#[test]
fn an_observation_never_promotes_a_candidate_out_of_unproven() {
    let home = tmp_home("neverproven");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let sampled = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    assert_eq!(sampled.observed.len(), 1, "the observation was recorded");
    world.reap(910);
    world.insert(911, FakeProc::inherited(9_110).orphan().group(Some(910)));

    let report = classify(&home, &world, 40 * HOUR_MS);

    // The strongest evidence V1 can have — a durable observation, the shell
    // gone, the container matching — still does not create authority.
    assert_eq!(
        reason(&report, 911),
        UnprovenReason::ObservedButNotAuthoritative
    );
}

#[test]
fn heuristics_are_display_only_and_never_change_the_class() {
    let home = tmp_home("heuristics");
    // The RCA census shape: a 44-day ngrok-like service and a 40-hour leak,
    // both under the agend home. Nothing about argv/cwd/age/CPU may separate
    // them into different classes.
    let world = FakeOracle::supported()
        .with(
            601,
            FakeProc::inherited(6_010)
                .orphan()
                .started_at(0)
                .display(DisplayColumns {
                    argv: Some("ngrok http 8080".to_string()),
                    cwd: Some("/Users/x/.agend-terminal/workspace/podcast".to_string()),
                    elapsed_secs: Some(44 * 24 * 3_600),
                    cpu_percent: Some(0.0),
                }),
        )
        .with(
            602,
            FakeProc::inherited(6_020)
                .orphan()
                .started_at(0)
                .display(DisplayColumns {
                    argv: Some("zsh -c ( while : ; do : ; done ) &".to_string()),
                    cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
                    elapsed_secs: Some(40 * 3_600),
                    cpu_percent: Some(96.0),
                }),
        );

    let report = classify(&home, &world, 44 * 24 * HOUR_MS);

    let listed: Vec<u32> = report.candidates.iter().map(|c| c.pid).collect();
    assert!(
        listed.contains(&601) && listed.contains(&602),
        "no age or CPU threshold may drop either: {listed:?}"
    );
    assert_eq!(
        reason(&report, 601),
        UnprovenReason::NoObservation,
        "the 44-day service is UNPROVEN — killing it would be an outage"
    );
    assert_eq!(reason(&report, 602), UnprovenReason::NoObservation);
}

// ---------------------------------------------------------------------------
// 3. Topology and sampling misses stay explicitly visible
// ---------------------------------------------------------------------------

#[test]
fn a_backend_with_no_children_reports_a_sibling_topology_miss() {
    let home = tmp_home("codex-sibling");
    // Measured: Codex runs tool commands from an app-server SIBLING, so the
    // backend has no children to sample at all. Reporting zero observations
    // silently would read as "nothing to see"; it must read as "I cannot see".
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("codex-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(
        miss(&outcome, "codex-1"),
        ObservationMiss::ExecutionOwnerNotABackendChild
    );
}

#[test]
fn a_recurring_non_tool_child_makes_the_window_ambiguous() {
    let home = tmp_home("caffeinate");
    // Measured on a live Claude backend: a `caffeinate` appears as a direct
    // child alongside the MCP servers, so "the unique new direct child" is
    // routinely not unique. This is the concrete reason V2 is not shipped.
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    world.insert(
        940,
        FakeProc::inherited(9_400)
            .child_of(900)
            .display(DisplayColumns {
                argv: Some("caffeinate -dims".to_string()),
                ..DisplayColumns::default()
            }),
    );

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(
        miss(&outcome, "dev-1"),
        ObservationMiss::AmbiguousCandidates
    );
}

#[test]
fn parallel_shell_tool_calls_are_ambiguous_by_construction() {
    let home = tmp_home("parallel");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    world.insert(911, FakeProc::group_leader(911, 9_110).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(
        miss(&outcome, "dev-1"),
        ObservationMiss::AmbiguousCandidates
    );
}

#[test]
fn a_first_tick_while_a_generation_is_already_active_is_a_visible_miss() {
    let home = tmp_home("restart");
    // Daemon restarted mid-tool-call: no inactive baseline exists, so even a
    // single child cannot be told apart from a helper that was already there.
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(
        miss(&outcome, "dev-1"),
        ObservationMiss::BaselineUnavailable
    );
}

#[test]
fn a_generation_whose_shell_was_never_sampled_is_a_visible_miss() {
    let home = tmp_home("dropped");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);

    // The shell was born and reaped entirely between two ticks.
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(miss(&outcome, "dev-1"), ObservationMiss::NoNewCandidate);
}

#[test]
fn an_unknown_backend_topology_is_reported_rather_than_guessed() {
    let home = tmp_home("opencode");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[BackendOwner {
            instance: "opencode-1".to_string(),
            backend_pid: 900,
            active_shell_tool: None,
        }],
        1_000,
    );

    assert!(outcome.observed.is_empty());
    assert_eq!(
        miss(&outcome, "opencode-1"),
        ObservationMiss::NoActiveShellTool,
        "with no tool evidence there is nothing to observe, and that is stated"
    );
}

#[test]
fn observations_record_the_actual_kernel_values_without_requiring_any_shape() {
    let home = tmp_home("shape-agnostic");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    // Fully inherited session AND group: no `setsid`, no `setpgid`.
    world.insert(910, FakeProc::inherited(9_100).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert_eq!(outcome.observed.len(), 1);
    let o = &outcome.observed[0];
    assert_eq!(o.shell_pid, 910);
    assert_eq!(o.sid, BACKEND_SESSION);
    assert_eq!(o.pgid, BACKEND_SESSION);
    assert_eq!(o.shell_start_token, 9_100);
    assert_eq!(o.tool_generation, 7);
    assert_eq!(o.first_seen_ms, 1_000);
}

// ---------------------------------------------------------------------------
// 4. Persistence and platform failures fail closed
// ---------------------------------------------------------------------------

#[test]
fn an_unwritable_ledger_is_reported_and_records_nothing() {
    let home = tmp_home("unwritable");
    std::fs::create_dir_all(ledger_path(&home)).unwrap();
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    let PersistOutcome::Failed(detail) = &outcome.persisted else {
        panic!("a ledger that cannot be written must report PersistOutcome::Failed");
    };
    assert!(!detail.is_empty());
    assert!(
        outcome.observed.is_empty(),
        "nothing durable was written, so nothing may be reported as observed"
    );
}

#[test]
fn an_unreadable_ledger_still_lists_candidates_without_suggestions() {
    let home = tmp_home("nonregular");
    std::fs::create_dir_all(ledger_path(&home)).unwrap();
    let world = FakeOracle::supported().with(911, FakeProc::inherited(9_110).orphan());

    assert!(load_ledger(&home).is_empty());
    let report = classify(&home, &world, 40 * HOUR_MS);

    assert_eq!(reason(&report, 911), UnprovenReason::NoObservation);
    assert!(
        candidate(&report, 911).suggested_instance.is_none(),
        "with no ledger there is no owner to suggest, and none may be invented"
    );
}

#[test]
fn an_unsupported_platform_reports_unsupported_and_lists_nothing() {
    let home = tmp_home("unsupported");
    let world = FakeOracle::unsupported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(910, FakeProc::group_leader(910, 9_100).child_of(900));

    let sample = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7, 0))], 0);
    assert!(matches!(
        sample.support,
        ProvenanceSupport::Unsupported { .. }
    ));
    assert!(sample.observed.is_empty());

    let report = classify(&home, &world, 40 * HOUR_MS);
    assert!(matches!(
        report.support,
        ProvenanceSupport::Unsupported { .. }
    ));
    assert!(
        report.candidates.is_empty(),
        "a platform without reparent semantics must say so rather than match nothing silently"
    );
}

// ---------------------------------------------------------------------------
// 5. The rendered report reads as evidence, not as a kill list
// ---------------------------------------------------------------------------

#[test]
fn the_rendered_report_marks_every_candidate_unproven_and_offers_no_kill_path() {
    let home = tmp_home("render");
    let world = FakeOracle::supported().with(
        911,
        FakeProc::inherited(9_110).orphan().display(DisplayColumns {
            cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
            elapsed_secs: Some(40 * 3_600),
            ..DisplayColumns::default()
        }),
    );
    let report = classify(&home, &world, 40 * HOUR_MS);

    let rendered = render_human(&report);

    assert!(
        rendered.contains("UNPROVEN"),
        "the section heading must state the epistemic status: {rendered}"
    );
    assert!(
        !rendered.contains("PROVEN\n") && !rendered.contains("PROVEN "),
        "V1 has no PROVEN class; the word must not appear as a status: {rendered}"
    );
    assert!(
        rendered.contains("display only"),
        "argv/cwd/age/CPU must be labelled non-authoritative: {rendered}"
    );
    assert!(
        rendered.contains("911"),
        "the candidate itself must be listed: {rendered}"
    );
    for forbidden in ["--kill", "confirm_ids", "--cleanup", "will be terminated"] {
        assert!(
            !rendered.contains(forbidden),
            "V1 must not advertise a cleanup affordance ({forbidden}): {rendered}"
        );
    }
}

/// Structural proof that "report-only" is a property of the code, not of this
/// commit's restraint. No signalling symbol may exist in the module at all.
#[test]
fn the_provenance_module_contains_no_signalling_path() {
    #[derive(Default)]
    struct SignalFinder {
        found: Vec<String>,
    }
    impl<'ast> Visit<'ast> for SignalFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            for seg in &p.segments {
                let ident = seg.ident.to_string();
                if matches!(
                    ident.as_str(),
                    "kill"
                        | "kill_process"
                        | "kill_process_tree"
                        | "terminate"
                        | "SIGKILL"
                        | "SIGTERM"
                        | "TerminateProcess"
                ) {
                    self.found.push(ident);
                }
            }
            visit::visit_path(self, p);
        }
        fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
            let name = f.sig.ident.to_string();
            if name.contains("cleanup") || name.contains("signal") || name.contains("kill") {
                self.found.push(name);
            }
            visit::visit_item_fn(self, f);
        }
    }

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/admin/orphan_provenance.rs");
    let src = std::fs::read_to_string(&path).expect("read src/admin/orphan_provenance.rs");
    let file = syn::parse_file(&src).expect("parse src/admin/orphan_provenance.rs");
    let mut finder = SignalFinder::default();
    finder.visit_file(&file);

    assert!(
        finder.found.is_empty(),
        "#3273 V1 is report-only per decision d-20260814163653190377-20: no signalling path may \
         exist in this module. Found: {:?}",
        finder.found
    );
}

// ---------------------------------------------------------------------------
// 6. Entry point — daemon-driven, NOT gated on a metrics subscriber
// ---------------------------------------------------------------------------

#[test]
fn the_real_per_tick_pipeline_observes_with_no_metrics_subscriber() {
    let home = tmp_home("headless");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    let owners = HeadlessOwners(vec![owner("dev-1", 900, bash_tool(7, 0))]);

    let mut handlers: Vec<Box<dyn PerTickHandler>> =
        vec![Box::new(ToolCallProvenanceHandler::new(&world, &owners))];
    run_handlers_with_panic_guard(&mut handlers, &home, 0);

    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    run_handlers_with_panic_guard(&mut handlers, &home, 1_000);

    let ledger = load_ledger(&home);
    assert_eq!(
        ledger.len(),
        1,
        "the daemon tick observes with no metrics subscriber in play"
    );
    assert_eq!(ledger[0].shell_pid, 910);
}

#[test]
fn provenance_sampling_never_references_the_metrics_subscriber_gate() {
    #[derive(Default)]
    struct MonitorCoupling {
        found: Option<String>,
    }
    impl<'ast> Visit<'ast> for MonitorCoupling {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            for seg in &p.segments {
                let ident = seg.ident.to_string();
                if ident == "instance_monitor" || ident == "LAST_METRICS_READ" {
                    self.found = Some(ident);
                }
            }
            visit::visit_path(self, p);
        }
    }

    for rel in [
        "src/admin/orphan_provenance.rs",
        "src/daemon/per_tick/tool_call_provenance.rs",
    ] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        let file = syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
        let mut finder = MonitorCoupling::default();
        finder.visit_file(&file);
        assert!(
            finder.found.is_none(),
            "#3273: {rel} must not reference `{}` — `instance_monitor::collect` is \
             subscriber-gated by `LAST_METRICS_READ`, so a headless daemon or a TUI not on the \
             Monitor pane skips the whole sysinfo sweep and observation would silently stop.",
            finder.found.clone().unwrap_or_default()
        );
    }
}

#[test]
fn the_provenance_handler_is_wired_into_the_default_per_tick_handlers() {
    #[derive(Default)]
    struct HandlerFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for HandlerFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            if p.segments
                .iter()
                .any(|s| s.ident == "ToolCallProvenanceHandler")
            {
                self.found = true;
            }
            visit::visit_path(self, p);
        }
    }

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/per_tick/mod.rs");
    let src = std::fs::read_to_string(&path).expect("read src/daemon/per_tick/mod.rs");
    let file = syn::parse_file(&src).expect("parse src/daemon/per_tick/mod.rs");
    let build = file
        .items
        .iter()
        .find_map(|it| match it {
            syn::Item::Fn(f) if f.sig.ident == "build_default_handlers" => Some(f),
            _ => None,
        })
        .expect("fn build_default_handlers not found in per_tick/mod.rs");

    let mut finder = HandlerFinder::default();
    finder.visit_item_fn(build);
    assert!(
        finder.found,
        "#3273: `build_default_handlers` must construct `ToolCallProvenanceHandler` — observation \
         belongs to the daemon's own tick, not to any UI subscription"
    );
}

// ---------------------------------------------------------------------------
// 7. Prevention — incidence reduction only, never authority
// ---------------------------------------------------------------------------

/// #3273 fix 2, and the only proposal in the issue that stops this class from
/// being created. It is NOT containment: nothing a shell installs on itself
/// survives `SIGKILL`, so the text must say so.
#[test]
fn agent_instructions_carry_the_background_job_cleanup_trap_idiom() {
    let text = background_process_guidance();

    assert!(
        text.contains("trap 'kill $LOAD 2>/dev/null' EXIT INT TERM"),
        "the guidance must give the exact working idiom, not a description of one: {text}"
    );
    assert!(
        text.contains("LOAD=$!"),
        "the idiom is useless without capturing the job's pid first: {text}"
    );
    assert!(
        text.contains("SIGKILL"),
        "the guidance must state the limit it cannot cover, or it reads as a guarantee: {text}"
    );
    assert!(
        !text.to_lowercase().contains("guarantee"),
        "incidence reduction must not be sold as containment: {text}"
    );
}

#[test]
fn the_background_job_guidance_is_wired_into_the_real_instruction_body() {
    #[derive(Default)]
    struct GuidanceFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for GuidanceFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            if p.segments
                .iter()
                .any(|s| s.ident == "background_process_guidance")
            {
                self.found = true;
            }
            visit::visit_path(self, p);
        }
    }

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/instructions.rs");
    let src = std::fs::read_to_string(&path).expect("read src/instructions.rs");
    let file = syn::parse_file(&src).expect("parse src/instructions.rs");
    let body = file
        .items
        .iter()
        .find_map(|it| match it {
            syn::Item::Fn(f) if f.sig.ident == "build_instructions_body" => Some(f),
            _ => None,
        })
        .expect("fn build_instructions_body not found in src/instructions.rs");

    let mut finder = GuidanceFinder::default();
    finder.visit_item_fn(body);
    assert!(
        finder.found,
        "#3273: `build_instructions_body` must emit `background_process_guidance()` — prevention \
         only reduces incidence if it reaches the instructions agents are actually given"
    );
}
