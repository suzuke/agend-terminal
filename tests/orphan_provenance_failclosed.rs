//! #3273 — recorded tool-call provenance and fail-closed orphan cleanup.
//!
//! §3.10 test-first: every test here MUST fail before
//! `admin::orphan_provenance` lands and pass after. At the RED commit the
//! module does not exist, so this target compile-fails — the accepted RED
//! signature per the reviewer RED-protocol ("the new tests compile-fail, fail
//! at runtime, or fail with the expected error signature").
//!
//! The contract under test is decision `d-20260814163653190377-20`, which the
//! primary and adversarial RCAs agreed on:
//!
//! * Kill authority comes ONLY from provenance agend itself recorded while the
//!   tool-call shell was a direct child of a backend — never from `PPID == 1`,
//!   argv, cwd, age or CPU. Those four are display columns.
//! * "Direct backend child" is not sufficient on its own: a backend also owns
//!   transport, helper and plugin subprocesses. A tool-call shell is
//!   distinguished by kernel-attested facts (it leads its OWN session and
//!   process group, because the backend `setsid`s it) taken together with
//!   active shell-tool evidence for that instance. Both are required.
//! * A shell that lived and died between two samples is a sampling miss. It
//!   stays visible as UNPROVEN forever and is never promoted.
//! * Cleanup is dry-run -> `confirm_ids` -> `audit_reason`, and re-proves
//!   identity via the start token before EVERY signal, because the TERM->KILL
//!   grace window is exactly when a PID can be recycled.
//! * Where the platform cannot attest the identity — or where the ledger
//!   cannot be persisted — the surface says so and signals nothing.
//!
//! Sampling is owner-service/daemon-driven. It must NOT hang off
//! `instance_monitor::collect()`, which is subscriber-gated by
//! `LAST_METRICS_READ`: a headless daemon, or a TUI not on the Monitor pane,
//! skips that sweep entirely and would silently stop recording provenance.
//! Pinned two ways below — behaviourally through the daemon-tick entry point,
//! and structurally with a `syn` AST walk.
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
    ProvenanceSupport, SampleSkip, ShellToolEvidence, ShellToolKind, SignalAction, Signaller,
    SkipReason, ToolCallProvenanceHandler, UnprovenReason,
};

const HOUR_MS: i64 = 3_600_000;
const OUR_UID: u32 = 501;

/// #3245: never wipe a shared fixture path on entry — one unique dir per test.
fn tmp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agend-3273-{}-{}-{}", tag, std::process::id(), id));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
    /// A session/group leader — the shape the backend gives a tool-call shell.
    fn leader(pid: u32, token: u64) -> Self {
        Self {
            ppid: None,
            sid: Some(pid),
            pgid: Some(pid),
            uid: Some(OUR_UID),
            start_token: Some(token),
            display: DisplayColumns::default(),
        }
    }

    /// A helper that stayed in its parent's session — transport, plugin, MCP
    /// server. Never a tool-call shell.
    fn follower(session_of: u32, token: u64) -> Self {
        Self {
            ppid: None,
            sid: Some(session_of),
            pgid: Some(session_of),
            uid: Some(OUR_UID),
            start_token: Some(token),
            display: DisplayColumns::default(),
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

fn bash_tool(generation: u64) -> Option<ShellToolEvidence> {
    Some(ShellToolEvidence {
        kind: ShellToolKind::Bash,
        generation,
        started_ms: 0,
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

fn skip_reason(
    outcome: &agend_terminal::admin::orphan_provenance::SampleOutcome,
    pid: u32,
) -> SampleSkip {
    outcome
        .skipped
        .iter()
        .find(|(candidate, _)| *candidate == pid)
        .map(|(_, reason)| reason.clone())
        .unwrap_or_else(|| panic!("pid {pid} must be reported as a visible sampling skip"))
}

/// The common fixture: instance `dev-1`'s backend 900 runs a Bash tool call in
/// shell 910 (its own session), which is sampled while alive, then leaves
/// orphan 911 behind and dies.
fn recorded_world() -> (PathBuf, FakeOracle) {
    let home = tmp_home("recorded");
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));

    let outcome = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 0);
    assert_eq!(
        outcome.recorded.len(),
        1,
        "the live tool-call shell must be recorded while it is samplable"
    );

    // The shell exits; its background job is reparented but keeps the session.
    world.reap(910);
    world.reap(900);
    world.insert(911, FakeProc::follower(910, 9_110).orphan());
    (home, world)
}

// ---------------------------------------------------------------------------
// 1. Sampling records kernel-attested identity — and only for tool-call shells
// ---------------------------------------------------------------------------

#[test]
fn sampling_records_session_group_and_token_of_a_tool_call_shell() {
    let home = tmp_home("sample");
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));

    let outcome =
        sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 1_700);

    assert!(matches!(outcome.support, ProvenanceSupport::Supported));
    assert!(matches!(outcome.persisted, PersistOutcome::Written));
    let ledger = load_ledger(&home);
    assert_eq!(ledger.len(), 1, "the sample must be durable, not in-memory");
    let record = &ledger[0];
    assert_eq!(record.instance, "dev-1");
    assert_eq!(record.shell_pid, 910);
    assert_eq!(
        record.sid, 910,
        "the tool-call shell's OWN session is the attribution key; the backend's is not inherited"
    );
    assert_eq!(
        record.pgid, 910,
        "the process group must be recorded too — consensus requires sid AND pgid where supported"
    );
    assert_eq!(record.leader_start_token, 9_100);
    assert_eq!(
        record.tool_generation, 7,
        "the record must name WHICH tool call it belongs to, so a later call cannot inherit it"
    );
    assert_eq!(record.first_seen_ms, 1_700);
}

#[test]
fn a_direct_backend_child_without_an_active_shell_tool_is_never_recorded() {
    let home = tmp_home("no-tool");
    // Same process shape as a tool-call shell, but the instance is not running
    // a shell-shaped tool call: this is a helper the backend started for its
    // own reasons. Recording it would authorize killing backend machinery.
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));

    let outcome = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, None)], 1_700);

    assert!(
        outcome.recorded.is_empty(),
        "no active shell tool means no tool-call shell to attribute"
    );
    assert_eq!(skip_reason(&outcome, 910), SampleSkip::NoActiveShellTool);
    assert!(load_ledger(&home).is_empty());
}

#[test]
fn a_transport_or_plugin_child_is_never_recorded_even_while_a_shell_tool_runs() {
    let home = tmp_home("transport");
    // 910 is the tool-call shell: the backend `setsid`s it, so it leads its own
    // session. 920 is a transport/MCP/plugin helper started by the same backend
    // in the SAME session. Both are direct children; only one is a tool call.
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900))
        .with(920, FakeProc::follower(900, 9_200).child_of(900));

    let outcome =
        sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 1_700);

    let recorded: Vec<u32> = outcome.recorded.iter().map(|r| r.shell_pid).collect();
    assert_eq!(
        recorded,
        vec![910],
        "only the child that leads its own session may be recorded; got {recorded:?}"
    );
    assert_eq!(skip_reason(&outcome, 920), SampleSkip::NotSessionLeader);
}

#[test]
fn a_child_missing_any_kernel_fact_is_skipped_not_half_recorded() {
    for (tag, proc, expected) in [
        (
            "no-sid",
            FakeProc::leader(910, 9_100).child_of(900).session(None),
            SampleSkip::MissingSessionId,
        ),
        (
            "no-pgid",
            FakeProc::leader(910, 9_100).child_of(900).group(None),
            SampleSkip::MissingProcessGroup,
        ),
        (
            "no-token",
            FakeProc::leader(910, 9_100).child_of(900).token(None),
            SampleSkip::MissingStartToken,
        ),
        (
            "group-follower",
            FakeProc::leader(910, 9_100).child_of(900).group(Some(900)),
            SampleSkip::NotProcessGroupLeader,
        ),
    ] {
        let home = tmp_home(tag);
        let world = FakeOracle::supported()
            .with(900, FakeProc::leader(900, 9_000))
            .with(910, proc);

        let outcome =
            sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 1_700);

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
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));
    let owners = [owner("dev-1", 900, bash_tool(7))];

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
    // A directory where the ledger file belongs: writing must fail, and the
    // sampler must say so instead of reporting a record it did not persist.
    std::fs::create_dir_all(ledger_path(&home)).unwrap();
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));

    let outcome =
        sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 1_700);

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
    let world = FakeOracle::supported().with(911, FakeProc::follower(910, 9_110).orphan());

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

/// The ledger's canonical location. Pinned here because the fail-closed tests
/// above must be able to make it unwritable; a rename is a contract change.
fn ledger_path(home: &Path) -> PathBuf {
    home.join("state").join("tool_call_provenance.json")
}

// ---------------------------------------------------------------------------
// 3. Sampling misses and heuristics stay UNPROVEN
// ---------------------------------------------------------------------------

#[test]
fn a_shell_that_died_between_samples_is_never_promoted_to_proven() {
    let home = tmp_home("miss");
    // 800 was born and reaped between two ticks: nothing was ever recorded for
    // it. Its orphan 801 inherits a session agend has no record of.
    let world = FakeOracle::supported().with(801, FakeProc::follower(800, 8_010).orphan());

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
    // Exactly the shape the issue's rejected detector would have selected: an
    // orphan whose argv and cwd both point inside the agend home.
    let world = FakeOracle::supported().with(
        700,
        FakeProc::leader(700, 7_000)
            .orphan()
            .display(DisplayColumns {
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
    // 7-44 DAYS old. Any age threshold that catches one catches the other, so
    // there must be no threshold at all.
    let world = FakeOracle::supported()
        .with(
            601,
            FakeProc::leader(601, 6_010)
                .orphan()
                .display(DisplayColumns {
                    elapsed_secs: Some(40 * 3_600),
                    cpu_percent: Some(96.0),
                    ..DisplayColumns::default()
                }),
        )
        .with(
            602,
            FakeProc::leader(602, 6_020)
                .orphan()
                .display(DisplayColumns {
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
// 4. Recorded provenance becomes PROVEN only once the owner and leader are gone
// ---------------------------------------------------------------------------

#[test]
fn recorded_provenance_with_owner_and_leader_gone_is_proven() {
    let (home, world) = recorded_world();

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert_eq!(proven_pids(&report), vec![911]);
    let CandidateClass::Proven(evidence) = &report.proven[0].class else {
        panic!("911 must be PROVEN");
    };
    assert_eq!(evidence.instance, "dev-1");
    assert_eq!(evidence.recorded_sid, 910);
    assert_eq!(evidence.recorded_pgid, 910);
    assert_eq!(evidence.leader_start_token, 9_100);
}

#[test]
fn a_live_owner_keeps_its_descendants_unproven() {
    let (home, world) = recorded_world();
    // The backend is back (or never died): its work is not abandoned.
    world.insert(900, FakeProc::leader(900, 9_000));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(proven_pids(&report).is_empty());
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::OwnerStillAlive
    );
}

#[test]
fn a_live_session_leader_keeps_its_descendants_unproven() {
    let (home, world) = recorded_world();
    // The recorded leader pid is alive again with its ORIGINAL token: the
    // session is still running, so nothing below it is abandoned.
    world.insert(910, FakeProc::leader(910, 9_100));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(proven_pids(&report).is_empty());
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::LeaderStillAlive
    );
}

#[test]
fn a_recycled_leader_pid_does_not_revive_the_record() {
    let (home, world) = recorded_world();
    // Same number, different process: the token proves it is not our leader.
    // That must NOT read as "leader alive", and must NOT read as PROVEN either
    // — the recorded session is unrecoverable, so the record is worthless.
    world.insert(910, FakeProc::leader(910, 4_242));

    let report = classify(&home, &world, &policy(), 40 * HOUR_MS);

    assert!(
        proven_pids(&report).is_empty(),
        "a recycled leader pid must never authorize a kill"
    );
    assert_eq!(
        unproven_reason(&report, 911),
        UnprovenReason::IdentityChanged
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
    let world = FakeOracle::supported().with(801, FakeProc::follower(800, 8_010).orphan());
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
    world.insert(911, FakeProc::follower(910, 9_110).orphan().uid(0));
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
    // Every read after classification returns a different token: between the
    // operator seeing the list and confirming it, the kernel handed 911 to
    // somebody else.
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
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));

    let sample = sample_tool_call_shells(&home, &world, &[owner("dev-1", 900, bash_tool(7))], 0);
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
        FakeProc::follower(800, 8_010)
            .orphan()
            .display(DisplayColumns {
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
fn the_daemon_tick_records_a_live_shell_with_no_metrics_subscriber() {
    let home = tmp_home("headless");
    let world = FakeOracle::supported()
        .with(900, FakeProc::leader(900, 9_000))
        .with(910, FakeProc::leader(910, 9_100).child_of(900));
    // The owner service is the ONLY input besides the process oracle. Nothing
    // here reads, arms or depends on `instance_monitor::LAST_METRICS_READ`, so
    // a headless daemon — or a TUI parked on any pane but Monitor — samples
    // exactly the same as a subscribed one.
    let owners = HeadlessOwners(vec![owner("dev-1", 900, bash_tool(7))]);
    let handler = ToolCallProvenanceHandler::new();

    let outcome = handler.tick(&home, &world, &owners, 1_700);

    assert_eq!(
        outcome.recorded.len(),
        1,
        "provenance must be recorded on the daemon tick without any metrics subscriber"
    );
    assert_eq!(load_ledger(&home).len(), 1);
}

/// Structural half of the same constraint. The behavioural test above proves
/// the entry point works headless; this proves nobody can later re-hang it off
/// the subscriber-gated sysinfo sweep without the gate failing loudly.
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
