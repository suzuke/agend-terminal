//! #3273 — recorded tool-call provenance and fail-closed orphan cleanup.
//!
//! §3.10 test-first: every test here MUST fail before
//! `admin::orphan_provenance` lands and pass after. At the RED commit the
//! module does not exist, so this target compile-fails — the accepted RED
//! signature per the reviewer RED-protocol ("the new tests compile-fail, fail
//! at runtime, or fail with the expected error signature").
//!
//! The contract under test is decision `d-20260814163653190377-20` as
//! corrected by the orchestrator's RED review: kill authority comes ONLY from
//! provenance agend recorded itself, and **selection is temporal, never
//! shape-based**.
//!
//! Selection (which process is a tool-call shell):
//! * an active shell-tool generation has a start boundary;
//! * a candidate is a direct backend child FIRST OBSERVED during that
//!   generation — children already present when the generation opened are
//!   pre-existing helpers (MCP servers, transport, plugin hosts) and are
//!   excluded;
//! * exactly one new candidate is required. Zero is a sampling miss, more than
//!   one is ambiguous, and both yield NO record and NO authority.
//! * `sid == pgid == pid` is NOT a selection test. agend's own code only
//!   `setsid`s the backend; whether an inner tool shell does is a per-backend,
//!   per-platform fact this repo does not control, so a shell that inherits the
//!   backend's session and group must still be recorded.
//!
//! Attribution (which orphan belongs to a recorded shell) uses the recorded
//! sid/pgid as a CONTAINMENT key only — never as authority on its own.
//! Authority comes from the temporal record; the container merely says which
//! surviving pids that record covers. An orphan is attributable only through a
//! container the recorded shell actually LED — its session if
//! `sid == shell_pid`, or its group if `pgid == shell_pid` — because only then
//! is the id unique to that tool call. A shell that led neither leaves orphans
//! indistinguishable from the backend's other descendants, so those stay
//! UNPROVEN. A process that merely happens to sit in some non-backend session
//! is never promoted: with no temporal record naming it, shape proves nothing.
//!
//! Everything else the RCAs agreed on:
//! * `PPID == 1`, argv, cwd, age and CPU are display columns and can never
//!   promote a candidate.
//! * A shell born and reaped between two samples stays UNPROVEN forever.
//! * Cleanup is dry-run -> `confirm_ids` -> `audit_reason`, and re-proves
//!   identity via the start token before EVERY signal, because the TERM->KILL
//!   grace window is exactly when a PID can be recycled.
//! * Where the platform cannot attest identity — or the ledger cannot be
//!   persisted — the surface says so and signals nothing.
//!
//! Sampling is owner-service/daemon-driven. It must NOT hang off
//! `instance_monitor::collect()`, which is subscriber-gated by
//! `LAST_METRICS_READ`: a headless daemon, or a TUI not on the Monitor pane,
//! skips that sweep entirely and would silently stop recording provenance.
//! Pinned three ways below — through the real per-tick handler pipeline, and
//! with two `syn` AST walks (negative coupling, positive wiring).
//!
//! Determinism: no real processes, no sleeps. The process world is a
//! `FakeOracle` the test mutates directly, so PID recycling and
//! SIGTERM-resistance are exact, not timing-dependent.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use syn::visit::{self, Visit};

use agend_terminal::admin::orphan_provenance::{
    classify, execute_cleanup, load_ledger, render_human, sample_tool_call_shells, BackendOwner,
    BackendOwnerSource, CandidateClass, CleanupOutcome, CleanupPolicy, CleanupRefusal,
    CleanupRequest, DisplayColumns, OrphanReport, PersistOutcome, ProcFacts, ProcessOracle,
    ProvenanceSupport, SampleMiss, SampleOutcome, SampleSkip, ShellToolEvidence, ShellToolKind,
    SignalAction, Signaller, SkipReason, ToolCallProvenanceHandler, UnprovenReason,
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
    home.join("state").join("tool_call_provenance.json")
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
    display: DisplayColumns,
}

impl FakeProc {
    /// The default shape: inherits the backend's session AND group. This is
    /// what the corrected contract must accept, because nothing in this repo
    /// proves an inner tool shell calls `setsid`.
    fn inherited(token: u64) -> Self {
        Self {
            ppid: None,
            sid: Some(BACKEND_SESSION),
            pgid: Some(BACKEND_SESSION),
            uid: Some(OUR_UID),
            start_token: Some(token),
            display: DisplayColumns::default(),
        }
    }

    /// Leads its own group but stays in the backend's session — the measured
    /// shape of every live `codex` helper on this machine.
    fn group_leader(pid: u32, token: u64) -> Self {
        Self {
            pgid: Some(pid),
            ..Self::inherited(token)
        }
    }

    /// Leads its own session and group — the measured shape of a live `claude`
    /// tool-call shell. Accepted, but never REQUIRED.
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

    fn session(mut self, sid: Option<u32>) -> Self {
        self.sid = sid;
        self
    }

    fn group(mut self, pgid: Option<u32>) -> Self {
        self.pgid = pgid;
        self
    }

    fn token(mut self, token: Option<u64>) -> Self {
        self.start_token = token;
        self
    }

    fn uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    fn display(mut self, d: DisplayColumns) -> Self {
        self.display = d;
        self
    }
}

/// The whole process world, mutable from the test body.
///
/// `term_resistant` models a `trap '' TERM; while :; do :; done` target: SIGTERM
/// is delivered and ignored, only SIGKILL reaps it.
struct FakeOracle {
    support: ProvenanceSupport,
    procs: RefCell<HashMap<u32, FakeProc>>,
    term_resistant: RefCell<HashSet<u32>>,
    /// Token rewrites applied lazily: `pid -> (after_n_reads, new_token)`.
    /// Models the kernel recycling a PID between two identity checks.
    recycle_after: RefCell<HashMap<u32, (u32, u64)>>,
    reads: RefCell<HashMap<u32, u32>>,
}

impl FakeOracle {
    fn supported() -> Self {
        Self {
            support: ProvenanceSupport::Supported,
            procs: RefCell::new(HashMap::new()),
            term_resistant: RefCell::new(HashSet::new()),
            recycle_after: RefCell::new(HashMap::new()),
            reads: RefCell::new(HashMap::new()),
        }
    }

    fn unsupported() -> Self {
        let mut me = Self::supported();
        me.support = ProvenanceSupport::Unsupported {
            platform: "windows",
            reason: "no POSIX session/reparent semantics; Job Objects are future work",
        };
        me
    }

    fn with(self, pid: u32, proc: FakeProc) -> Self {
        self.procs.borrow_mut().insert(pid, proc);
        self
    }

    fn resistant(self, pid: u32) -> Self {
        self.term_resistant.borrow_mut().insert(pid);
        self
    }

    /// After `n` further identity reads of `pid`, its start token becomes
    /// `token` — i.e. the kernel handed the number to somebody else.
    fn recycle_after(self, pid: u32, n: u32, token: u64) -> Self {
        self.recycle_after.borrow_mut().insert(pid, (n, token));
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
        let seen = {
            let mut reads = self.reads.borrow_mut();
            let counter = reads.entry(pid).or_insert(0);
            *counter += 1;
            *counter
        };
        let token = match self.recycle_after.borrow().get(&pid) {
            Some(&(after, new_token)) if seen > after => Some(new_token),
            _ => proc.start_token,
        };
        Some(ProcFacts {
            pid,
            ppid: proc.ppid,
            sid: proc.sid,
            pgid: proc.pgid,
            uid: proc.uid,
            start_token: token,
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

/// SIGTERM reaps the target unless it is `term_resistant`; SIGKILL always does.
/// The oracle is shared so the cleanup path observes the death it caused.
struct WorldSignaller<'a> {
    world: &'a FakeOracle,
    sent: RefCell<Vec<(&'static str, u32)>>,
}

impl<'a> WorldSignaller<'a> {
    fn new(world: &'a FakeOracle) -> Self {
        Self {
            world,
            sent: RefCell::new(Vec::new()),
        }
    }

    fn log(&self) -> Vec<(&'static str, u32)> {
        self.sent.borrow().clone()
    }
}

impl Signaller for WorldSignaller<'_> {
    fn term(&self, pid: u32) {
        self.sent.borrow_mut().push(("TERM", pid));
        if !self.world.term_resistant.borrow().contains(&pid) {
            self.world.reap(pid);
        }
    }

    fn kill(&self, pid: u32) {
        self.sent.borrow_mut().push(("KILL", pid));
        self.world.reap(pid);
    }
}

/// The owner service: what the daemon knows about live backends and the tool
/// call each one is currently running. Deliberately carries NO metrics handle.
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

fn policy() -> CleanupPolicy {
    CleanupPolicy {
        self_uid: Some(OUR_UID),
        excluded_pids: Vec::new(),
        term_grace_ms: 500,
    }
}

fn confirmed(ids: &[u32]) -> CleanupRequest {
    CleanupRequest {
        apply: true,
        confirm_ids: ids.to_vec(),
        audit_reason: Some("operator verified the leak in #3273".to_string()),
    }
}

fn proven_pids(report: &OrphanReport) -> Vec<u32> {
    report.proven.iter().map(|c| c.pid).collect()
}

fn unproven_reason(report: &OrphanReport, pid: u32) -> UnprovenReason {
    let candidate = report
        .unproven
        .iter()
        .find(|c| c.pid == pid)
        .unwrap_or_else(|| panic!("pid {pid} must still be visible as an UNPROVEN candidate"));
    match &candidate.class {
        CandidateClass::Unproven(reason) => reason.clone(),
        CandidateClass::Proven(_) => panic!("pid {pid} must not be PROVEN"),
    }
}

fn skip_reason(outcome: &SampleOutcome, pid: u32) -> SampleSkip {
    outcome
        .skipped
        .iter()
        .find(|(candidate, _)| *candidate == pid)
        .map(|(_, reason)| reason.clone())
        .unwrap_or_else(|| panic!("pid {pid} must be reported as a visible sampling skip"))
}

fn miss(outcome: &SampleOutcome, instance: &str) -> SampleMiss {
    outcome
        .misses
        .iter()
        .find(|(name, _)| name == instance)
        .map(|(_, m)| m.clone())
        .unwrap_or_else(|| panic!("instance {instance} must report a visible sampling miss"))
}

/// The continuously-maintained pre-tool baseline: a tick with NO active shell
/// tool records which direct children pre-date any subsequent generation.
fn baseline_tick(home: &Path, world: &FakeOracle, now_ms: i64) -> SampleOutcome {
    sample_tool_call_shells(home, world, &[owner("dev-1", 900, None)], now_ms)
}

/// The common fixture — deliberately the shape of the actual #3273 incident.
/// Backend 900 already owns two helpers when the Bash generation opens; shell
/// 910 then appears and is the unique new child; the SHELL exits leaving
/// orphan 911 in the group it led, while **the backend and the instance stay
/// alive**. Backend death is not part of the incident and must not be a
/// precondition for attribution.
fn recorded_world() -> (PathBuf, FakeOracle) {
    let home = tmp_home("recorded");
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900))
        .with(921, FakeProc::group_leader(921, 9_210).child_of(900));

    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    assert_eq!(
        outcome.recorded.len(),
        1,
        "the unique newly-observed child must be recorded"
    );

    // Only the tool-call shell dies. 900 keeps running, exactly as in #3273.
    world.reap(910);
    // The background job keeps the group its shell led.
    world.insert(911, FakeProc::inherited(9_110).group(Some(910)).orphan());
    (home, world)
}

// ---------------------------------------------------------------------------
// 1. Selection is temporal, not shape-based
// ---------------------------------------------------------------------------

#[test]
fn a_shell_inheriting_the_backend_session_and_group_is_still_recorded() {
    let home = tmp_home("inherited");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);

    // No setsid, no setpgid: the shell is indistinguishable from the backend by
    // shape alone. Temporal attribution must still record it.
    world.insert(910, FakeProc::inherited(9_100).child_of(900));
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert_eq!(outcome.recorded.len(), 1);
    let record = &outcome.recorded[0];
    assert_eq!(record.shell_pid, 910);
    assert_eq!(
        record.sid, BACKEND_SESSION,
        "the ACTUAL session is recorded, even when it is the backend's"
    );
    assert_eq!(
        record.pgid, BACKEND_SESSION,
        "the ACTUAL group is recorded, even when it is the backend's"
    );
    assert_eq!(record.shell_start_token, 9_100);
    assert_eq!(record.tool_generation, 7);
    assert_eq!(record.first_seen_ms, 1_000);
}

#[test]
fn a_pre_existing_helper_is_never_recorded() {
    let home = tmp_home("preexisting");
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        // The MCP servers, bridge and plugin host the backend started at boot.
        .with(920, FakeProc::inherited(9_200).child_of(900))
        .with(921, FakeProc::group_leader(921, 9_210).child_of(900));

    let baseline = baseline_tick(&home, &world, 0);
    assert!(
        baseline.recorded.is_empty(),
        "children that pre-date the generation are not candidates"
    );
    assert_eq!(skip_reason(&baseline, 920), SampleSkip::PreExistingChild);
    assert_eq!(skip_reason(&baseline, 921), SampleSkip::PreExistingChild);
    assert_eq!(miss(&baseline, "dev-1"), SampleMiss::NoNewCandidate);

    // A later tick in the SAME generation must not re-promote them.
    let again = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        5_000,
    );
    assert!(again.recorded.is_empty());
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn two_new_children_in_one_generation_are_ambiguous_and_record_nothing() {
    let home = tmp_home("ambiguous");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);

    // A tool shell and a helper appeared in the same window. Nothing in the
    // kernel state says which is which, so authority must not be invented.
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    world.insert(930, FakeProc::inherited(9_300).child_of(900));
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(
        outcome.recorded.is_empty(),
        "ambiguity must produce no record at all, not a guess"
    );
    assert_eq!(miss(&outcome, "dev-1"), SampleMiss::Ambiguous);
    assert_eq!(skip_reason(&outcome, 910), SampleSkip::AmbiguousCandidates);
    assert_eq!(skip_reason(&outcome, 930), SampleSkip::AmbiguousCandidates);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn no_new_child_in_a_generation_is_a_visible_sampling_miss() {
    let home = tmp_home("nocandidate");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.recorded.is_empty());
    assert_eq!(
        miss(&outcome, "dev-1"),
        SampleMiss::NoNewCandidate,
        "a generation whose shell was never observed must stay visible as a miss"
    );
}

#[test]
fn a_first_tick_while_a_generation_is_already_active_fails_closed() {
    let home = tmp_home("nobaseline");
    // The daemon restarted while a tool call was already in flight: 910 and 920
    // are both simply "there", and nothing says which pre-dates the tool. With
    // no inactive baseline the temporal contract has no input, so it must
    // record nothing rather than fall back to shape.
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(910, FakeProc::session_leader(910, 9_100).child_of(900))
        .with(920, FakeProc::inherited(9_200).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(
        outcome.recorded.is_empty(),
        "no baseline means no temporal evidence; a session-leader shape must not substitute for it"
    );
    assert_eq!(miss(&outcome, "dev-1"), SampleMiss::BaselineUnavailable);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn a_first_active_tick_with_exactly_one_child_is_still_unclassified() {
    let home = tmp_home("nobaseline-single");
    // The decisive case: one direct child and no inactive baseline. Nothing
    // distinguishes "the tool shell" from "a helper that was already running",
    // and there is no ambiguity for the ambiguity rule to catch — so only an
    // explicit baseline requirement keeps this from being falsely attributed.
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(
        outcome.recorded.is_empty(),
        "a single already-running child must not be attributed just because it is alone"
    );
    assert_eq!(miss(&outcome, "dev-1"), SampleMiss::BaselineUnavailable);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn a_first_active_tick_with_zero_children_establishes_an_empty_baseline() {
    let home = tmp_home("nobaseline-empty");
    // An empty child set IS a usable baseline: there is nothing that could
    // have pre-dated the generation, so the next new child is unambiguous.
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    let owners = [owner("dev-1", 900, bash_tool(7, 0))];

    let first = sample_tool_call_shells(&home, &world, &owners, 1_000);
    assert!(first.recorded.is_empty());
    assert_eq!(miss(&first, "dev-1"), SampleMiss::NoNewCandidate);

    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let second = sample_tool_call_shells(&home, &world, &owners, 2_000);

    assert_eq!(second.recorded.len(), 1);
    assert_eq!(second.recorded[0].shell_pid, 910);
    assert_eq!(second.recorded[0].tool_generation, 7);
}

#[test]
fn a_known_child_pid_reused_by_a_new_process_is_a_fresh_candidate() {
    let home = tmp_home("childreuse");
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900));
    baseline_tick(&home, &world, 0);

    // 920 died and the kernel handed its number to the tool shell. Known-child
    // state is keyed by pid AND start token, so this is a NEW identity, not an
    // inherited "already known" one.
    world.insert(920, FakeProc::group_leader(920, 5_555).child_of(900));
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert_eq!(outcome.recorded.len(), 1);
    assert_eq!(outcome.recorded[0].shell_pid, 920);
    assert_eq!(
        outcome.recorded[0].shell_start_token, 5_555,
        "the record must carry the NEW identity's token, not the retired one"
    );
}

#[test]
fn a_backend_pid_reused_by_a_different_process_invalidates_the_baseline() {
    let home = tmp_home("backendreuse");
    let world = FakeOracle::supported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(920, FakeProc::inherited(9_200).child_of(900));
    baseline_tick(&home, &world, 0);

    // Same backend PID, different process: the baseline was taken against a
    // backend that no longer exists, so its child set proves nothing about
    // this one. Keyed by backend pid AND start token, not pid alone.
    world.insert(900, FakeProc::session_leader(900, 4_242));
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));

    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    assert!(outcome.recorded.is_empty());
    assert_eq!(miss(&outcome, "dev-1"), SampleMiss::BaselineUnavailable);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn a_direct_backend_child_without_an_active_shell_tool_is_never_recorded() {
    let home = tmp_home("no-tool");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, None)], 0);

    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let outcome = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, None)], 1_000);

    assert!(
        outcome.recorded.is_empty(),
        "with no shell tool in flight there is no tool call to attribute to"
    );
    assert_eq!(skip_reason(&outcome, 910), SampleSkip::NoActiveShellTool);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn a_new_generation_does_not_inherit_the_previous_generations_child() {
    let home = tmp_home("generation");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );

    // Generation 8 opens back-to-back, with no intervening inactive tick: the
    // frozen pre-tool baseline still applies, and 910 is already attributed to
    // generation 7, so the only new candidate is 912.
    world.insert(912, FakeProc::group_leader(912, 9_120).child_of(900));
    let outcome = sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(8, 2_000))],
        2_500,
    );

    assert_eq!(outcome.recorded.len(), 1);
    assert_eq!(outcome.recorded[0].shell_pid, 912);
    assert_eq!(outcome.recorded[0].tool_generation, 8);
    assert_eq!(
        skip_reason(&outcome, 910),
        SampleSkip::AlreadyAttributed,
        "a pid already carrying a record must not be re-attributed to a later generation"
    );

    let ledger = load_ledger(&home);
    let gen_of_910 = ledger
        .iter()
        .find(|r| r.shell_pid == 910)
        .expect("910's record survives")
        .tool_generation;
    assert_eq!(gen_of_910, 7, "910 stays attributed to generation 7 only");
}

#[test]
fn a_child_missing_any_kernel_fact_is_skipped_not_half_recorded() {
    for (tag, proc, expected) in [
        (
            "no-sid",
            FakeProc::group_leader(910, 9_100)
                .child_of(900)
                .session(None),
            SampleSkip::MissingSessionId,
        ),
        (
            "no-pgid",
            FakeProc::group_leader(910, 9_100).child_of(900).group(None),
            SampleSkip::MissingProcessGroup,
        ),
        (
            "no-token",
            FakeProc::group_leader(910, 9_100).child_of(900).token(None),
            SampleSkip::MissingStartToken,
        ),
    ] {
        let home = tmp_home(tag);
        let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
        baseline_tick(&home, &world, 0);
        world.insert(910, proc);

        let outcome = sample_tool_call_shells(
            &home,
            &world,
            &[owner("dev-1", 900, bash_tool(7, 0))],
            1_000,
        );

        assert!(
            outcome.recorded.is_empty(),
            "{tag}: a partial identity must not be recorded — it would be unprovable later"
        );
        assert_eq!(skip_reason(&outcome, 910), expected, "{tag}");
        assert!(load_ledger(&home).is_empty(), "{tag}");
    }
}

#[test]
fn resampling_the_same_shell_preserves_first_seen_and_generation() {
    let home = tmp_home("resample");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    let owners = [owner("dev-1", 900, bash_tool(7, 0))];

    sample_tool_call_shells(&home, &world, &owners, 1_000);
    sample_tool_call_shells(&home, &world, &owners, 5_000);

    let ledger = load_ledger(&home);
    assert_eq!(ledger.len(), 1, "resampling must not duplicate the record");
    assert_eq!(ledger[0].first_seen_ms, 1_000);
    assert_eq!(ledger[0].last_seen_ms, 5_000);
    assert_eq!(ledger[0].tool_generation, 7);
}

// ---------------------------------------------------------------------------
// 2. Persistence failures fail closed — never a silent in-memory "record"
// ---------------------------------------------------------------------------

#[test]
fn an_unwritable_ledger_is_reported_and_never_yields_authority() {
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
    assert!(
        !detail.is_empty(),
        "the failure must carry an operator-readable reason"
    );
    assert!(
        outcome.recorded.is_empty(),
        "nothing was durably recorded, so nothing may be reported as recorded"
    );
}

#[test]
fn a_non_regular_ledger_yields_no_proven_candidates() {
    let home = tmp_home("nonregular");
    std::fs::create_dir_all(ledger_path(&home)).unwrap();
    let world = FakeOracle::supported().with(911, FakeProc::inherited(9_110).orphan());

    assert!(load_ledger(&home).is_empty());
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    assert!(
        proven_pids(&report).is_empty(),
        "an unreadable ledger must fail closed to UNPROVEN, never open to PROVEN"
    );
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::NoRecordedProvenance
    );
}

// ---------------------------------------------------------------------------
// 3. Sampling misses and heuristics stay UNPROVEN
// ---------------------------------------------------------------------------

#[test]
fn a_shell_that_died_between_samples_is_never_promoted_to_proven() {
    let home = tmp_home("miss");
    let world = FakeOracle::supported().with(801, FakeProc::inherited(8_010).orphan());

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(
        proven_pids(&report).is_empty(),
        "a sampling miss must never yield kill authority"
    );
    assert_eq!(
        unproven_reason(&report, 801),
        UnprovenReason::NoRecordedProvenance,
        "the miss must stay VISIBLE, not silently dropped"
    );
}

#[test]
fn argv_and_cwd_under_the_agend_home_never_make_a_candidate_proven() {
    let home = tmp_home("heuristic");
    let world = FakeOracle::supported().with(
        700,
        FakeProc::inherited(7_000).orphan().display(DisplayColumns {
            argv: Some("bash /Users/x/.agend-terminal/workspace/dev-1/probe.sh".to_string()),
            cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
            elapsed_secs: Some(40 * 3_600),
            cpu_percent: Some(96.0),
        }),
    );

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(proven_pids(&report).is_empty());
    assert_eq!(
        unproven_reason(&report, 700),
        UnprovenReason::NoRecordedProvenance
    );
}

#[test]
fn age_and_cpu_are_display_columns_and_never_filter() {
    let home = tmp_home("age");
    // The census in the RCA: the leak was 40h old, the false positives were
    // 7-44 DAYS old. Any age threshold that catches one catches the other.
    let world = FakeOracle::supported()
        .with(
            601,
            FakeProc::inherited(6_010).orphan().display(DisplayColumns {
                elapsed_secs: Some(40 * 3_600),
                cpu_percent: Some(96.0),
                ..DisplayColumns::default()
            }),
        )
        .with(
            602,
            FakeProc::inherited(6_020).orphan().display(DisplayColumns {
                elapsed_secs: Some(44 * 24 * 3_600),
                cpu_percent: Some(0.0),
                ..DisplayColumns::default()
            }),
        );

    let report = classify(&home, &world, &policy(), 44 * 24 * HOUR_MS);

    let listed: Vec<u32> = report.unproven.iter().map(|c| c.pid).collect();
    assert!(
        listed.contains(&601) && listed.contains(&602),
        "neither the 40-hour nor the 44-day candidate may be filtered out; got {listed:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Attribution — only through a container the recorded shell actually LED
// ---------------------------------------------------------------------------

/// THE INCIDENT. #3273's instance and backend never died — only the inner tool
/// shell did, and its background jobs reparented to init. If a live backend
/// erased attribution, the fix would miss the very leak it was filed for.
/// PROVEN means "attributable to a recorded tool generation", not "definitely
/// unwanted"; the safety against killing intentional detached work is the
/// dry-run -> confirm_ids -> audit_reason round-trip, asserted here to send
/// nothing on its own.
#[test]
fn a_live_backend_does_not_erase_attribution_of_its_dead_shells_orphans() {
    let (home, world) = recorded_world();
    assert!(
        world.is_alive(900),
        "fixture precondition: the backend is still running, as in the incident"
    );

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert_eq!(proven_pids(&report), vec![911]);
    let CandidateClass::Proven(evidence) = &report.proven[0].class else {
        panic!("911 must be PROVEN");
    };
    assert_eq!(evidence.instance, "dev-1");
    assert_eq!(evidence.recorded_shell_pid, 910);
    assert_eq!(evidence.recorded_pgid, 910);
    assert_eq!(evidence.shell_start_token, 9_100);

    // Listed, but nothing happens without an explicit confirmed apply.
    let signaller = WorldSignaller::new(&world);
    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &CleanupRequest {
            apply: false,
            confirm_ids: Vec::new(),
            audit_reason: None,
        },
        &policy(),
    );
    assert!(matches!(outcome, CleanupOutcome::DryRun { .. }));
    assert!(
        signaller.log().is_empty(),
        "attribution must never imply a signal"
    );
}

#[test]
fn a_shell_that_led_neither_container_leaves_unattributable_orphans() {
    let home = tmp_home("nocontainer");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    // Fully inherited: sid and pgid are the backend's, so nothing the shell
    // leaves behind carries an id unique to the tool call.
    world.insert(910, FakeProc::inherited(9_100).child_of(900));
    sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    world.reap(910);
    world.insert(911, FakeProc::inherited(9_110).orphan());

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(
        proven_pids(&report).is_empty(),
        "an orphan sharing the backend's session and group could be anything below the backend"
    );
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::NoDiscriminatingContainer
    );
}

#[test]
fn an_orphan_in_the_session_the_shell_led_is_proven() {
    let home = tmp_home("sessioncontainer");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    baseline_tick(&home, &world, 0);
    world.insert(910, FakeProc::session_leader(910, 9_100).child_of(900));
    sample_tool_call_shells(
        &home,
        &world,
        &[owner("dev-1", 900, bash_tool(7, 0))],
        1_000,
    );
    world.reap(910);
    world.insert(911, FakeProc::inherited(9_110).session(Some(910)).orphan());

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert_eq!(proven_pids(&report), vec![911]);
}

#[test]
fn a_live_shell_keeps_its_descendants_unproven() {
    let (home, world) = recorded_world();
    world.insert(910, FakeProc::group_leader(910, 9_100));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(proven_pids(&report).is_empty());
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::LeaderStillAlive
    );
}

#[test]
/// A different process now holding the recorded shell's number PROVES the
/// original shell is gone — which is the very condition attribution needs. Over
/// a 40-hour-old incident PID reuse is plausible, so letting it erase durable
/// attribution would defeat the fix in exactly the case it was written for.
#[test]
fn a_recycled_shell_pid_elsewhere_does_not_erase_durable_attribution() {
    let (home, world) = recorded_world();
    // The number came back as an unrelated process in an unrelated container.
    world.insert(910, FakeProc::inherited(4_242).group(Some(999)));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert_eq!(
        proven_pids(&report),
        vec![911],
        "the orphan's recorded container attribution survives reuse of the shell's number"
    );

    // And the innocent squatter is never touched: it is not in the inventory,
    // and the pre-signal identity check would refuse it anyway.
    let signaller = WorldSignaller::new(&world);
    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );
    assert!(matches!(outcome, CleanupOutcome::Applied { .. }));
    assert!(
        !signaller.log().iter().any(|(_, pid)| *pid == 910),
        "the process that inherited the shell's number must never be signalled"
    );
}

/// The separate, genuinely ambiguous case. A process group id IS the pid of its
/// leader, so a recycled 910 that leads its own group produces a group also
/// numbered 910 — the recorded container id no longer identifies one set of
/// processes. Membership cannot be decided, so it fails closed.
#[test]
fn a_container_id_collision_makes_the_orphan_unproven() {
    let (home, world) = recorded_world();
    world.insert(910, FakeProc::group_leader(910, 4_242));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(
        proven_pids(&report).is_empty(),
        "an ambiguous container must not authorize a kill"
    );
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::ContainerIdCollision
    );
}

// ---------------------------------------------------------------------------
// 5. Cleanup round-trip
// ---------------------------------------------------------------------------

#[test]
fn dry_run_is_the_default_and_sends_nothing() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &CleanupRequest {
            apply: false,
            confirm_ids: Vec::new(),
            audit_reason: None,
        },
        &policy(),
    );

    let CleanupOutcome::DryRun { candidate_ids } = outcome else {
        panic!("apply=false must produce a dry-run inventory, got {outcome:?}");
    };
    assert_eq!(candidate_ids, vec![911]);
    assert!(signaller.log().is_empty(), "a dry run must send no signal");
}

#[test]
fn apply_without_confirm_ids_is_refused() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &CleanupRequest {
            apply: true,
            confirm_ids: Vec::new(),
            audit_reason: Some("cleanup".to_string()),
        },
        &policy(),
    );

    assert!(matches!(
        outcome,
        CleanupOutcome::Refused(CleanupRefusal::MissingConfirmIds)
    ));
    assert!(signaller.log().is_empty());
}

#[test]
fn apply_without_a_non_empty_audit_reason_is_refused() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    for reason in [None, Some(String::new()), Some("   ".to_string())] {
        let outcome = execute_cleanup(
            &home,
            &world,
            &signaller,
            &report,
            &CleanupRequest {
                apply: true,
                confirm_ids: vec![911],
                audit_reason: reason.clone(),
            },
            &policy(),
        );
        assert!(
            matches!(
                outcome,
                CleanupOutcome::Refused(CleanupRefusal::EmptyAuditReason)
            ),
            "audit_reason {reason:?} must be refused, got {outcome:?}"
        );
    }
    assert!(signaller.log().is_empty());
}

#[test]
fn a_confirmed_id_outside_the_dry_run_inventory_is_skipped() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911, 4_242]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert!(actions.contains(&SignalAction::Skipped {
        pid: 4_242,
        reason: SkipReason::NotInInventory,
    }));
    assert_eq!(
        signaller.log(),
        vec![("TERM", 911)],
        "only the inventoried pid may be signalled"
    );
}

#[test]
fn an_unproven_candidate_is_never_signalled_even_when_confirmed() {
    let home = tmp_home("unproven-confirmed");
    let world = FakeOracle::supported().with(801, FakeProc::inherited(8_010).orphan());
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[801]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(
        actions,
        vec![SignalAction::Skipped {
            pid: 801,
            reason: SkipReason::NotProven,
        }]
    );
    assert!(signaller.log().is_empty());
}

#[test]
fn a_foreign_uid_is_never_signalled() {
    let (home, world) = recorded_world();
    world.insert(
        911,
        FakeProc::inherited(9_110).group(Some(910)).orphan().uid(0),
    );
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert!(actions.contains(&SignalAction::Skipped {
        pid: 911,
        reason: SkipReason::ForeignUid,
    }));
    assert!(signaller.log().is_empty());
}

#[test]
fn daemon_and_transport_pids_are_excluded_even_when_confirmed() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);
    let guarded = CleanupPolicy {
        excluded_pids: vec![911],
        ..policy()
    };

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &guarded,
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(
        actions,
        vec![SignalAction::Skipped {
            pid: 911,
            reason: SkipReason::DaemonOrTransport,
        }]
    );
    assert!(signaller.log().is_empty());
}

// ---------------------------------------------------------------------------
// 6. TOCTOU — identity is re-proved before EVERY signal
// ---------------------------------------------------------------------------

#[test]
fn a_pid_recycled_between_listing_and_term_is_dropped_unsignalled() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    world.recycle_after.borrow_mut().insert(911, (0, 1_234_567));
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(
        actions,
        vec![SignalAction::Skipped {
            pid: 911,
            reason: SkipReason::IdentityMismatch,
        }]
    );
    assert!(
        signaller.log().is_empty(),
        "an innocent process must not receive SIGTERM"
    );
}

#[test]
fn a_pid_recycled_inside_the_term_grace_window_never_receives_the_kill() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let world = world.resistant(911).recycle_after(911, 1, 1_234_567);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(
        signaller.log(),
        vec![("TERM", 911)],
        "the pre-TERM check passed, but the pre-KILL check must refuse"
    );
    assert!(actions.contains(&SignalAction::Skipped {
        pid: 911,
        reason: SkipReason::IdentityMismatch,
    }));
}

#[test]
fn a_sigterm_resistant_target_escalates_to_kill() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let world = world.resistant(911);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(signaller.log(), vec![("TERM", 911), ("KILL", 911)]);
    assert_eq!(actions, vec![SignalAction::ForceKilled { pid: 911 }]);
}

#[test]
fn a_target_that_exits_on_term_is_not_killed() {
    let (home, world) = recorded_world();
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    let signaller = WorldSignaller::new(&world);

    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[911]),
        &policy(),
    );

    let CleanupOutcome::Applied { actions } = outcome else {
        panic!("expected an applied outcome, got {outcome:?}");
    };
    assert_eq!(signaller.log(), vec![("TERM", 911)]);
    assert_eq!(actions, vec![SignalAction::Terminated { pid: 911 }]);
}

// ---------------------------------------------------------------------------
// 7. Platforms that cannot attest identity say so and signal nothing
// ---------------------------------------------------------------------------

#[test]
fn an_unsupported_platform_reports_unsupported_and_never_signals() {
    let home = tmp_home("unsupported");
    let world = FakeOracle::unsupported()
        .with(900, FakeProc::session_leader(900, 9_000))
        .with(910, FakeProc::group_leader(910, 9_100).child_of(900));

    let sample = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7, 0))], 0);
    assert!(matches!(
        sample.support,
        ProvenanceSupport::Unsupported { .. }
    ));
    assert!(
        sample.recorded.is_empty(),
        "an unattestable platform must record nothing rather than record something weak"
    );

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);
    assert!(matches!(
        report.support,
        ProvenanceSupport::Unsupported { .. }
    ));
    assert!(proven_pids(&report).is_empty());

    let signaller = WorldSignaller::new(&world);
    let outcome = execute_cleanup(
        &home,
        &world,
        &signaller,
        &report,
        &confirmed(&[910]),
        &policy(),
    );
    assert!(matches!(
        outcome,
        CleanupOutcome::Refused(CleanupRefusal::PlatformUnsupported)
    ));
    assert!(signaller.log().is_empty());
}

// ---------------------------------------------------------------------------
// 8. The report must not let a reader mistake a display column for evidence
// ---------------------------------------------------------------------------

#[test]
fn the_rendered_report_separates_proven_from_unproven() {
    let (home, world) = recorded_world();
    world.insert(
        801,
        FakeProc::inherited(8_010).orphan().display(DisplayColumns {
            cwd: Some("/Users/x/.agend-terminal/workspace/dev-1".to_string()),
            ..DisplayColumns::default()
        }),
    );
    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    let rendered = render_human(&report);
    let proven_at = rendered
        .find("PROVEN")
        .expect("the report must name the PROVEN section");
    let unproven_at = rendered
        .find("UNPROVEN")
        .expect("the report must name the UNPROVEN section");
    assert!(
        proven_at < unproven_at,
        "PROVEN candidates come first so the operator reads authority before guesses"
    );
    assert!(
        rendered.contains("display only"),
        "argv/cwd/age/CPU must be labelled non-authoritative in the rendered report"
    );
    assert!(rendered.contains("911") && rendered.contains("801"));
}

// ---------------------------------------------------------------------------
// 9. Entry point — daemon-driven, NOT gated on a metrics subscriber
// ---------------------------------------------------------------------------

#[test]
fn the_real_per_tick_pipeline_records_a_live_shell_with_no_metrics_subscriber() {
    let home = tmp_home("headless");
    let world = FakeOracle::supported().with(900, FakeProc::session_leader(900, 9_000));
    let owners = HeadlessOwners(vec![owner("dev-1", 900, bash_tool(7, 0))]);

    // Driven through the REAL handler pipeline, not a helper: the same
    // `run_handlers_with_panic_guard` the daemon loop calls each tick. Nothing
    // here reads, arms or depends on `instance_monitor::LAST_METRICS_READ`, so
    // a headless daemon — or a TUI parked on any pane but Monitor — samples
    // exactly as a subscribed one does.
    let mut handlers: Vec<Box<dyn PerTickHandler>> =
        vec![Box::new(ToolCallProvenanceHandler::new(&world, &owners))];
    run_handlers_with_panic_guard(&mut handlers, &home, 0);

    world.insert(910, FakeProc::group_leader(910, 9_100).child_of(900));
    run_handlers_with_panic_guard(&mut handlers, &home, 1_000);

    let ledger = load_ledger(&home);
    assert_eq!(
        ledger.len(),
        1,
        "provenance must be recorded by the daemon tick with no metrics subscriber"
    );
    assert_eq!(ledger[0].shell_pid, 910);
}

/// Structural half of the same constraint. The pipeline test above proves the
/// entry point works headless; this proves nobody can later re-hang it off the
/// subscriber-gated sysinfo sweep without the gate failing loudly.
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
             Monitor pane skips the whole sysinfo sweep and provenance sampling would silently \
             stop. Sampling is owner-service/daemon-driven and must stay that way.",
            finder.found.clone().unwrap_or_default()
        );
    }
}

/// The positive half: the sampler is actually on the daemon tick. Without this
/// the negative above is satisfiable by not sampling at all.
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
        "#3273: `build_default_handlers` must construct `ToolCallProvenanceHandler` — provenance \
         sampling belongs to the daemon's own tick, not to any UI subscription"
    );
}

// ---------------------------------------------------------------------------
// 10. Prevention — incidence reduction only, never authority
// ---------------------------------------------------------------------------

/// #3273 fix 2. The `trap` idiom is the only proposal in the issue that stops
/// this class of leak from being created, and it belongs in the instructions
/// agents actually receive rather than in a doc nobody reads. It is NOT
/// containment: nothing a shell installs on itself survives `SIGKILL`, so the
/// text must say so and must never be cited as proof or as reaper authority.
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

/// The text is worthless if it is an orphan constant. Pin that the real
/// instruction body — the one written into every agent's instructions file —
/// actually calls it.
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
