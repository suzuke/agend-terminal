#![allow(clippy::unwrap_used, clippy::expect_used)]
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
    classify, load_ledger, render_human, sample_from_owner_source, sample_tool_call_shells,
    scope_reparented, BackendOwner, BackendOwnerSource, DisplayColumns, ObservationMiss,
    OrphanReport, PersistOutcome, ProcFacts, ProcessOracle, ProvenanceSupport, SampleOutcome,
    ScopeCounts, ScopeInput, ShellToolEvidence, ShellToolKind, UnprovenReason,
};
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

fn candidate(
    report: &OrphanReport,
    pid: u32,
) -> &agend_terminal::admin::orphan_provenance::OrphanCandidate {
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
    // The backend HAS a child — a pre-existing helper — so "nothing new
    // appeared" is a different fact from "nothing is below this backend at
    // all", which is the sibling-topology case pinned separately below.
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900));
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

/// The Codex app-server sibling topology is a standing fact about that backend,
/// not a one-off first-tick condition. Every window where the backend has no
/// children at all must say so, including after a baseline exists — otherwise
/// the second and later windows silently downgrade to "nothing new appeared"
/// and the operator never learns that agend cannot see the execution owner.
#[test]
fn every_window_with_no_backend_children_reports_the_sibling_topology() {
    let home = tmp_home("sibling-standing");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));

    let first = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    assert_eq!(
        miss(&first, "dev-1"),
        ObservationMiss::ExecutionOwnerNotABackendChild
    );

    // A baseline (empty) now exists. The topology has not changed, so neither
    // may the diagnosis.
    baseline_tick(&home, &world, 2_000);
    let later = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(8, 3_000))],
        3_000,
    );
    assert!(later.observed.is_empty());
    assert_eq!(
        miss(&later, "dev-1"),
        ObservationMiss::ExecutionOwnerNotABackendChild,
        "a backend with no children is a topology fact on every window, not just the first"
    );
}

/// An observation records the shell's ACTUAL sid and pgid, which are not its
/// pid when the shell inherited them. Relating an orphan through `shell_pid`
/// therefore misses exactly the inherited case the contract insists on
/// supporting, and the suggestion — the only thing the ledger is for — never
/// appears.
#[test]
fn an_observation_relates_through_its_recorded_container_not_the_shell_pid() {
    let home = tmp_home("container-key");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    // shell_pid 910, but it leads neither: pgid 777, sid the backend's.
    world.insert(
        910,
        FakeProc::inherited(9_100).child_of(900).group(Some(777)),
    );
    let sampled = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    assert_eq!(sampled.observed.len(), 1);
    assert_eq!(sampled.observed[0].shell_pid, 910);
    assert_eq!(sampled.observed[0].pgid, 777);

    world.reap(910);
    // The job it left behind carries the GROUP, which is not the shell's pid.
    world.insert(911, FakeProc::inherited(9_110).group(Some(777)).orphan());

    let report = classify(&home, &world, 40 * HOUR_MS);

    assert_eq!(
        candidate(&report, 911).suggested_instance.as_deref(),
        Some("dev-1"),
        "the ledger must relate through the recorded pgid/sid, not through shell_pid"
    );
    assert_eq!(
        reason(&report, 911),
        UnprovenReason::ObservedButNotAuthoritative,
        "relating it is a suggestion; it must not become anything stronger"
    );
}

/// The report promises that elapsed and CPU are display columns. A promise
/// about columns it never prints is worse than silence: the operator is told
/// the data was considered and shown, and it was not.
#[test]
fn the_report_carries_and_prints_elapsed_and_cpu() {
    let home = tmp_home("agecpu");
    let world = FakeOracle::supported().with(
        911,
        FakeProc::inherited(9_110).orphan().display(DisplayColumns {
            argv: Some("zsh -c ( while : ; do : ; done ) &".to_string()),
            cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
            elapsed_secs: Some(40 * 3_600),
            cpu_percent: Some(96.5),
        }),
    );

    let report = classify(&home, &world, 40 * HOUR_MS);
    let c = candidate(&report, 911);
    assert_eq!(c.elapsed_secs, Some(40 * 3_600));
    assert_eq!(c.cpu_percent, Some(96.5));

    let rendered = render_human(&report);
    assert!(
        rendered.contains("144000"),
        "elapsed must reach the rendered report: {rendered}"
    );
    assert!(
        rendered.contains("96.5"),
        "CPU must reach the rendered report: {rendered}"
    );
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

/// The owner-service seam the handler runs on, driven behaviourally. It takes a
/// `BackendOwnerSource` and a `ProcessOracle` and NOTHING else — no metrics
/// handle exists to pass, so a headless daemon and a subscribed TUI observe
/// identically. The handler itself is pinned to this seam structurally below,
/// because `PerTickHandler`/`TickContext` are crate-private and a `tests/` file
/// cannot construct an `AgentRegistry` to drive the loop for real.
#[test]
fn the_owner_service_seam_observes_with_no_metrics_input() {
    let home = tmp_home("headless");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    let owners = HeadlessOwners(vec![owner("dev-1", 900, bash_tool(7, 0))]);

    sample_from_owner_source(&home, &world, &owners, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let outcome = sample_from_owner_source(&home, &world, &owners, 1_000);

    assert_eq!(outcome.observed.len(), 1);
    let ledger = load_ledger(&home);
    assert_eq!(
        ledger.len(),
        1,
        "the owner-service seam observes with no metrics subscriber in play"
    );
    assert_eq!(ledger[0].shell_pid, 910);
}

/// Structural half: the daemon handler is a REAL `PerTickHandler` and its `run`
/// goes through the seam above. Without this, the behavioural test proves only
/// that a free function works.
#[test]
fn the_provenance_handler_is_a_real_per_tick_handler_that_uses_the_seam() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/daemon/per_tick/tool_call_provenance.rs");
    let src =
        std::fs::read_to_string(&path).expect("read src/daemon/per_tick/tool_call_provenance.rs");
    let file = syn::parse_file(&src).expect("parse tool_call_provenance.rs");

    let impl_block = file
        .items
        .iter()
        .find_map(|it| match it {
            syn::Item::Impl(i) => {
                let trait_is_per_tick = i.trait_.as_ref().is_some_and(|(path, _)| {
                    path.segments
                        .last()
                        .is_some_and(|s| s.ident == "PerTickHandler")
                });
                let self_is_handler = matches!(&*i.self_ty, syn::Type::Path(tp)
                    if tp.path.segments.last()
                        .is_some_and(|s| s.ident == "ToolCallProvenanceHandler"));
                (trait_is_per_tick && self_is_handler).then_some(i)
            }
            _ => None,
        })
        .expect(
            "#3273: `impl PerTickHandler for ToolCallProvenanceHandler` must exist — the sampler \
             has to be a real daemon handler, not a look-alike with its own loop",
        );

    #[derive(Default)]
    struct SeamFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for SeamFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            if p.segments
                .iter()
                .any(|s| s.ident == "sample_from_owner_source")
            {
                self.found = true;
            }
            visit::visit_path(self, p);
        }
    }
    let run = impl_block
        .items
        .iter()
        .find_map(|it| match it {
            syn::ImplItem::Fn(f) if f.sig.ident == "run" => Some(f),
            _ => None,
        })
        .expect("the handler must implement `run`");

    let mut finder = SeamFinder::default();
    finder.visit_impl_item_fn(run);
    assert!(
        finder.found,
        "#3273: `PerTickHandler::run` must call `sample_from_owner_source` — the tested seam and \
         the production tick must be the same code path"
    );
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
    // The handler list is built with `vec![...]`, and syn does not parse a
    // macro's token stream into expressions — a `visit_path`-only walk is blind
    // to every entry in it. So inspect macro tokens too, still scoped to this
    // one function rather than grepping the file.
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
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if m.tokens.to_string().contains("ToolCallProvenanceHandler") {
                self.found = true;
            }
            visit::visit_macro(self, m);
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
fn agent_instructions_carry_a_multi_job_safe_cleanup_trap_idiom() {
    let text = background_process_guidance();

    // The collection is initialised and the trap installed BEFORE anything is
    // launched, so a job that starts and the shell that dies in between are
    // still covered. A single-PID `LOAD=$!` idiom is unsafe: it remembers only
    // the last job and leaks every earlier one.
    let init_at = text
        .find("LOAD=\"\"")
        .expect("guidance must initialise the pid collection empty");
    let trap_at = text.find("trap '").expect("guidance must install a trap");
    let launch_at = text
        .find(" & LOAD=")
        .or_else(|| text.find("& LOAD="))
        .expect("guidance must show appending each job's pid after launching it");
    assert!(
        init_at < trap_at && trap_at < launch_at,
        "order matters: initialise, then trap, then launch. got init@{init_at} trap@{trap_at} \
         launch@{launch_at} in: {text}"
    );

    assert!(
        text.contains("LOAD=\"$LOAD $!\""),
        "every job's pid must be APPENDED, not overwritten: {text}"
    );
    assert!(
        text.contains("test -z \"$LOAD\" || kill $LOAD 2>/dev/null"),
        "the trap must be a no-op when nothing was launched, and must not word-split badly: {text}"
    );
    assert!(
        text.contains("EXIT") && text.contains("INT") && text.contains("TERM"),
        "the trap must cover normal exit, INT and TERM: {text}"
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

// ---------------------------------------------------------------------------
// 8. §3.9 real entry — the user-facing `doctor` command must actually show this
// ---------------------------------------------------------------------------

/// A module that samples and renders but is never reached by `doctor` is dead
/// code with tests. This drives the REAL compiled binary through its real
/// subcommand dispatch — no injected report, no mid-pipeline seam.
#[test]
fn the_doctor_command_emits_the_orphan_provenance_section() {
    let home = tmp_home("doctor-entry");
    let output = assert_cmd::Command::cargo_bin("agend-terminal")
        .expect("binary must exist")
        .arg("doctor")
        .env("AGEND_HOME", &home)
        .output()
        .expect("doctor must run");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // Without this, a doctor that printed the section and then died would pass:
    // partial stdout is not a working command.
    assert!(
        output.status.success(),
        "`agend-terminal doctor` must exit 0; status={:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Orphaned agent-attributable processes"),
        "`agend-terminal doctor` must emit the orphan section: {stdout}"
    );
    assert!(
        stdout.contains("UNPROVEN"),
        "the section must state the epistemic status at the real entry too: {stdout}"
    );
    assert!(
        stdout.contains("display only"),
        "the display-only label must survive to the real entry: {stdout}"
    );
    for forbidden in ["--kill", "--cleanup", "confirm_ids"] {
        assert!(
            !stdout.contains(forbidden),
            "V1 must not advertise a cleanup affordance ({forbidden}) at the real entry: {stdout}"
        );
    }
}

/// Structural companion: the section the binary prints must come FROM this
/// module, so a hard-coded placeholder cannot satisfy the test above.
#[test]
fn run_doctor_invokes_the_orphan_provenance_module() {
    // Deliberately NOT "mentions orphan_provenance anywhere": a `use` line or a
    // doc reference would satisfy that. Require the two calls that actually
    // produce the section — classify it, then render it.
    #[derive(Default)]
    struct ProvenanceCallFinder {
        classifies: bool,
        renders: bool,
    }
    impl<'ast> Visit<'ast> for ProvenanceCallFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
            for pair in segs.windows(2) {
                if pair[0] == "orphan_provenance" && pair[1] == "classify" {
                    self.classifies = true;
                }
                if pair[0] == "orphan_provenance" && pair[1] == "render_human" {
                    self.renders = true;
                }
            }
            visit::visit_path(self, p);
        }
    }

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/cli.rs");
    let src = std::fs::read_to_string(&path).expect("read src/cli.rs");
    let file = syn::parse_file(&src).expect("parse src/cli.rs");
    let run_doctor = file
        .items
        .iter()
        .find_map(|it| match it {
            syn::Item::Fn(f) if f.sig.ident == "run_doctor" => Some(f),
            _ => None,
        })
        .expect("fn run_doctor not found in src/cli.rs");

    let mut finder = ProvenanceCallFinder::default();
    finder.visit_item_fn(run_doctor);
    assert!(
        finder.classifies && finder.renders,
        "#3273 §3.9: `run_doctor` must call BOTH `orphan_provenance::classify` and \
         `orphan_provenance::render_human` — a sampler whose report nothing reaches is not a \
         reported orphan, and a mere import proves nothing. classify={} render={}",
        finder.classifies,
        finder.renders
    );
}

/// And the subcommand really is wired to `run_doctor`, so the AST pin above
/// cannot be satisfied by an unreachable function.
#[test]
fn the_doctor_subcommand_dispatches_to_run_doctor() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&path).expect("read src/main.rs");
    let file = syn::parse_file(&src).expect("parse src/main.rs");

    #[derive(Default)]
    struct DispatchFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for DispatchFinder {
        fn visit_expr_match(&mut self, m: &'ast syn::ExprMatch) {
            for arm in &m.arms {
                let pattern = quote_pattern(&arm.pat);
                if pattern.contains("Doctor") {
                    let mut call = CallFinder::default();
                    call.visit_expr(&arm.body);
                    if call.found {
                        self.found = true;
                    }
                }
            }
            visit::visit_expr_match(self, m);
        }
    }
    #[derive(Default)]
    struct CallFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for CallFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            if p.segments.iter().any(|s| s.ident == "run_doctor") {
                self.found = true;
            }
            visit::visit_path(self, p);
        }
    }
    fn quote_pattern(pat: &syn::Pat) -> String {
        let mut out = String::new();
        if let syn::Pat::TupleStruct(ts) = pat {
            for seg in &ts.path.segments {
                out.push_str(&seg.ident.to_string());
            }
            for inner in &ts.elems {
                out.push_str(&quote_pattern(inner));
            }
        } else if let syn::Pat::Struct(s) = pat {
            for seg in &s.path.segments {
                out.push_str(&seg.ident.to_string());
            }
        } else if let syn::Pat::Path(p) = pat {
            for seg in &p.path.segments {
                out.push_str(&seg.ident.to_string());
            }
        }
        out
    }

    let mut finder = DispatchFinder::default();
    finder.visit_file(&file);
    assert!(
        finder.found,
        "#3273 §3.9: the `Doctor` subcommand arm must call `run_doctor`, or the doctor entry pin \
         proves nothing about what a user actually runs"
    );
}

// ---------------------------------------------------------------------------
// 11. The scope filter is a pinned pure function, and the report admits its scope
// ---------------------------------------------------------------------------

/// `PPID == 1` on its own makes an unreadable list: on macOS launchd is the
/// parent of hundreds of per-user agents and XPC services. This view therefore
/// narrows to processes holding a session id they did not create, which is the
/// shape the #3273 RCA measured for a background job outliving its tool-call
/// shell.
///
/// That narrowing is a SCOPING CHOICE, not a finding about ownership. A process
/// in session 1, or one that leads its own session, is not thereby proven to be
/// nobody's leftover — it is a topology category this view does not cover, and
/// therefore a known blind spot. Nothing here concludes anything about who owns
/// what; every survivor is still UNPROVEN.
///
/// This is production's real filter, so it is pinned as a pure function with
/// every exclusion counted. Counting matters as much as filtering: an excluded
/// process that is never counted is indistinguishable from one that does not
/// exist, and the point of this report is to be honest about what it did not
/// look at.
#[test]
fn the_scope_filter_includes_only_processes_holding_a_session_they_did_not_create() {
    let rows = vec![
        // Kept: reparented, ours, and holding a session it did not create.
        ScopeInput {
            pid: 911,
            ppid: 1,
            uid: OUR_UID,
            sid: Some(910),
        },
        // Session 1: outside this view's scope, and a known blind spot.
        ScopeInput {
            pid: 571,
            ppid: 1,
            uid: OUR_UID,
            sid: Some(1),
        },
        // Leads its own session: outside this view's scope. Says nothing
        // about whether anything lost it — see the setsid blind spot.
        ScopeInput {
            pid: 396,
            ppid: 1,
            uid: OUR_UID,
            sid: Some(396),
        },
        // Another user's process: outside our capability entirely.
        ScopeInput {
            pid: 42,
            ppid: 1,
            uid: 0,
            sid: Some(41),
        },
        // Still has a living parent, so it is not reparented at all.
        ScopeInput {
            pid: 1234,
            ppid: 900,
            uid: OUR_UID,
            sid: Some(910),
        },
        // getsid failed: we cannot tell, so we must not guess either way.
        ScopeInput {
            pid: 777,
            ppid: 1,
            uid: OUR_UID,
            sid: None,
        },
    ];

    let (included, counts) = scope_reparented(&rows, Some(OUR_UID));

    assert_eq!(
        included,
        vec![911],
        "only the inherited-session candidate is in scope for this view"
    );
    assert_eq!(counts.scanned, 6);
    assert_eq!(counts.included, 1);
    assert_eq!(counts.excluded_init_session, 1, "sid == 1 must be counted");
    assert_eq!(
        counts.excluded_session_leader, 1,
        "sid == pid must be counted"
    );
    assert_eq!(counts.excluded_foreign_uid, 1);
    assert_eq!(counts.excluded_not_reparented, 1);
    assert_eq!(
        counts.excluded_session_unknown, 1,
        "a process whose session could not be read must be counted, not silently dropped"
    );
    // The buckets are mutually exclusive and exhaustive: every scanned process
    // lands in exactly one. Without this, a miscounted bucket could hide
    // processes that were neither reported nor accounted for.
    assert_eq!(
        counts.scanned,
        counts.included
            + counts.excluded_not_reparented
            + counts.excluded_foreign_uid
            + counts.excluded_session_leader
            + counts.excluded_init_session
            + counts.excluded_session_unknown,
        "scope accounting must balance: {counts:?}"
    );
}

/// The accounting invariant is the property, not the six numbers in the table
/// above: any input must balance.
#[test]
fn the_scope_filter_accounting_always_balances() {
    let rows: Vec<ScopeInput> = (0..40u32)
        .map(|i| ScopeInput {
            pid: 1_000 + i,
            ppid: if i % 5 == 0 { 900 } else { 1 },
            uid: if i % 7 == 0 { 0 } else { OUR_UID },
            sid: match i % 4 {
                0 => None,
                1 => Some(1),
                2 => Some(1_000 + i),
                _ => Some(500 + i),
            },
        })
        .collect();

    let (included, counts) = scope_reparented(&rows, Some(OUR_UID));

    assert_eq!(counts.scanned, 40);
    assert_eq!(counts.included, included.len());
    assert_eq!(
        counts.scanned,
        counts.included
            + counts.excluded_not_reparented
            + counts.excluded_foreign_uid
            + counts.excluded_session_leader
            + counts.excluded_init_session
            + counts.excluded_session_unknown,
        "scope accounting must balance for any input: {counts:?}"
    );
}

/// The filter under test must be the filter that ships. Without this, the pure
/// function above could be a well-tested function nothing calls.
#[test]
fn the_platform_oracle_uses_the_pinned_scope_filter() {
    #[derive(Default)]
    struct ScopeCallFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for ScopeCallFinder {
        fn visit_path(&mut self, p: &'ast syn::Path) {
            if p.segments.iter().any(|s| s.ident == "scope_reparented") {
                self.found = true;
            }
            visit::visit_path(self, p);
        }
        fn visit_macro(&mut self, m: &'ast syn::Macro) {
            if m.tokens.to_string().contains("scope_reparented") {
                self.found = true;
            }
            visit::visit_macro(self, m);
        }
    }

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/daemon/per_tick/tool_call_provenance.rs");
    let src = std::fs::read_to_string(&path).expect("read tool_call_provenance.rs");
    let file = syn::parse_file(&src).expect("parse tool_call_provenance.rs");
    let oracle_impl = file
        .items
        .iter()
        .find_map(|it| match it {
            syn::Item::Impl(i) => {
                let is_oracle = i.trait_.as_ref().is_some_and(|(path, _)| {
                    path.segments
                        .last()
                        .is_some_and(|s| s.ident == "ProcessOracle")
                });
                is_oracle.then_some(i)
            }
            _ => None,
        })
        .expect("`impl ProcessOracle for PlatformOracle` must exist");
    let reparented = oracle_impl
        .items
        .iter()
        .find_map(|it| match it {
            syn::ImplItem::Fn(f) if f.sig.ident == "reparented" => Some(f),
            _ => None,
        })
        .expect("the oracle must implement `reparented`");

    let mut finder = ScopeCallFinder::default();
    finder.visit_impl_item_fn(reparented);
    assert!(
        finder.found,
        "#3273: `PlatformOracle::reparented` must delegate to `scope_reparented` — the scoping \
         rule that ships has to be the one the tests pin, not a second copy of it"
    );
}

/// An empty list is the most dangerous output this command can produce: read
/// carelessly it says "your machine is clean". It is not — it says nothing was
/// found INSIDE a deliberately narrow scope, with known blind spots.
#[test]
fn an_empty_report_says_it_is_a_scoped_snapshot_not_a_clean_bill() {
    let home = tmp_home("emptyscope");
    let world = FakeOracle::supported();
    let mut report = classify(&home, &world, 40 * HOUR_MS);
    report.scope = ScopeCounts {
        scanned: 1_012,
        included: 0,
        excluded_not_reparented: 183,
        excluded_foreign_uid: 316,
        excluded_session_leader: 215,
        excluded_init_session: 294,
        excluded_session_unknown: 4,
    };
    assert_eq!(
        report.scope.scanned,
        report.scope.included
            + report.scope.excluded_not_reparented
            + report.scope.excluded_foreign_uid
            + report.scope.excluded_session_leader
            + report.scope.excluded_init_session
            + report.scope.excluded_session_unknown,
        "fixture must be a state the filter could actually produce"
    );

    let rendered = render_human(&report);

    assert!(
        rendered.contains("none found in this scoped snapshot (not a global clean result)"),
        "an empty list must not read as a clean bill of health: {rendered}"
    );
    assert!(
        rendered.contains("setsid"),
        "the blind spot must be stated wherever the result is stated: {rendered}"
    );
    for count in ["294", "215", "4"] {
        assert!(
            rendered.contains(count),
            "excluded counts for session-1, session-leader and unreadable-session must be \
             printed ({count} missing): {rendered}"
        );
    }
}

/// The same scope accounting has to appear when candidates DO exist, or an
/// operator reading a short list cannot tell whether it is short because the
/// machine is quiet or because the scope threw most of it away.
#[test]
fn a_non_empty_report_still_prints_its_scope_accounting() {
    let home = tmp_home("scopeaccounting");
    let world = FakeOracle::supported().with(911, FakeProc::inherited(9_110).orphan());
    let mut report = classify(&home, &world, 40 * HOUR_MS);
    report.scope = ScopeCounts {
        scanned: 1_013,
        included: 1,
        excluded_not_reparented: 183,
        excluded_foreign_uid: 316,
        excluded_session_leader: 215,
        excluded_init_session: 294,
        excluded_session_unknown: 4,
    };
    assert_eq!(
        report.scope.scanned,
        report.scope.included
            + report.scope.excluded_not_reparented
            + report.scope.excluded_foreign_uid
            + report.scope.excluded_session_leader
            + report.scope.excluded_init_session
            + report.scope.excluded_session_unknown,
        "fixture must be a state the filter could actually produce"
    );

    let rendered = render_human(&report);

    assert!(rendered.contains("911"));
    assert!(
        rendered.contains("1013"),
        "scanned total must be printed: {rendered}"
    );
    assert!(rendered.contains("294") && rendered.contains("215") && rendered.contains("4"));
    assert!(rendered.contains("setsid"));
}
