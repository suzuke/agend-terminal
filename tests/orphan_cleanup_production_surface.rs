#![allow(clippy::unwrap_used, clippy::expect_used)]
//! #3273 V2 — the production surface: the exact-PID conversion, platform
//! support, and the structural guards that keep signal authority confined to
//! the manual executor.
//!
//! Consensus `d-20260814214253501210-22`. The contracts in
//! `orphan_cleanup_manual_authority.rs` prove the executor's *logic* against
//! injected seams; these prove that the real adapters cannot widen the blast
//! radius, and that nothing outside the explicit manual surface can reach them.

use agend_terminal::admin::orphan_cleanup::{
    platform_support, signal_outcome_from_syscall, AuditStore, BoundedWaiter, ExactPid, ExitWaiter,
    FreshIdentityOracle, IdentityOracle, JsonlAuditStore, LivenessProbe, PreSignalAudit,
    ProcIdentity, RawIdentityReader, SignalOutcome, Sleeper, Support,
};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use syn::visit::{self, Visit};

// ---------------------------------------------------------------------------
// The exact-PID conversion. This is the single highest-consequence line in the
// change, because both failure modes are silent at the call site.
// ---------------------------------------------------------------------------

/// `kill(0, sig)` signals every process in the CALLER's process group — on a
/// daemon that is the daemon and everything it spawned. A pid of 0 must never
/// become a signal target.
#[test]
fn pid_zero_can_never_become_a_signal_target_3273() {
    assert_eq!(
        ExactPid::new(0),
        None,
        "pid 0 must be refused: kill(0, sig) signals the caller's entire process group"
    );
}

/// A pid above `i32::MAX` wraps NEGATIVE under `as i32`, and `kill(-n, sig)`
/// signals process group `n`. The cast turns an exact-pid contract into a group
/// signal with no diagnostic anywhere.
#[test]
fn a_pid_above_i32_max_can_never_wrap_into_a_group_signal_3273() {
    for pid in [
        i32::MAX as u32 + 1, // wraps to i32::MIN
        u32::MAX,            // wraps to -1 — kill(-1, sig) is "every process we may signal"
        0x8000_0000,
        0xFFFF_FFFE,
    ] {
        assert_eq!(
            ExactPid::new(pid),
            None,
            "pid {pid} must be refused rather than wrapped into a negative target"
        );
    }
}

/// Everything a real process can legitimately be keeps working, so the guard is
/// a guard and not a wall.
#[test]
fn ordinary_pids_convert_exactly_3273() {
    for pid in [1u32, 2, 4242, 99_999, i32::MAX as u32] {
        let exact = ExactPid::new(pid).unwrap_or_else(|| panic!("pid {pid} must be accepted"));
        assert_eq!(exact.get(), pid as i32, "the value must survive unchanged");
        assert!(
            exact.get() > 0,
            "an exact target is always strictly positive"
        );
    }
}

/// On a platform with no identity oracle there is nothing to revalidate a
/// snapshot against, so the feature stays report-only rather than degrading to
/// "signal and hope".
#[test]
fn platform_support_is_explicit_3273() {
    let support = platform_support();
    if cfg!(unix) {
        assert_eq!(support, Support::Supported);
    } else {
        assert!(
            matches!(support, Support::Unsupported(_)),
            "a non-Unix build must be explicitly unsupported, not silently supported"
        );
    }
}

// ---------------------------------------------------------------------------
// Structural guards. Signal authority must stay where the consensus put it.
// ---------------------------------------------------------------------------

/// Collects every path segment mentioned in a file, so a guard can ask "does
/// this module even name that thing?" without pattern-matching on source text.
#[derive(Default)]
struct PathCollector {
    segments: Vec<String>,
}

impl<'ast> Visit<'ast> for PathCollector {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        for segment in &path.segments {
            self.segments.push(segment.ident.to_string());
        }
        visit::visit_path(self, path);
    }
}

fn segments_of(rel: &str) -> Vec<String> {
    let src = std::fs::read_to_string(rel).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    let file = syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    let mut collector = PathCollector::default();
    collector.visit_file(&file);
    collector.segments
}

/// V1 classification stays report-only. It may describe a candidate in any
/// detail; it may not be able to act on one, and it must not reach the executor
/// that can.
#[test]
fn the_v1_module_cannot_reach_the_manual_executor_3273() {
    let segments = segments_of("src/admin/orphan_provenance.rs");
    for forbidden in ["orphan_cleanup", "kill", "terminate", "kill_process"] {
        assert!(
            !segments.iter().any(|s| s == forbidden),
            "V1 must not name `{forbidden}` — observations never carry signal authority"
        );
    }
}

/// The manual executor is manual. A daemon tick reaching it would make cleanup
/// automatic, which the consensus defers until per-tool launch leases and
/// kernel-backed containment exist.
#[test]
fn no_daemon_tick_reaches_the_manual_executor_3273() {
    for rel in [
        "src/daemon/per_tick/assignment_reconcile.rs",
        "src/admin/cleanup_zombies.rs",
    ] {
        let segments = segments_of(rel);
        assert!(
            !segments.iter().any(|s| s == "orphan_cleanup"),
            "{rel} must not reference the manual executor: automatic cleanup is deferred"
        );
    }
}

/// `doctor` with no subcommand is a report. It must not prompt, consume a
/// confirmation, or signal anything, so it must not name the executor at all.
#[test]
fn default_doctor_stays_report_only_3273() {
    let src = std::fs::read_to_string("src/cli.rs").expect("read src/cli.rs");
    let file = syn::parse_file(&src).expect("parse src/cli.rs");
    let run_doctor = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == "run_doctor" => Some(f),
            _ => None,
        })
        .expect("run_doctor must exist");
    let mut collector = PathCollector::default();
    collector.visit_item_fn(run_doctor);
    assert!(
        !collector.segments.iter().any(|s| s == "orphan_cleanup"),
        "the default doctor report must not reach the manual executor"
    );
}

// ---------------------------------------------------------------------------
// Production adapters, exercised through injected seams rather than asserted
// about with a source-text guard. A guard can only say the code mentions the
// right names; these say it behaves.
// ---------------------------------------------------------------------------

/// Counts raw reads so "never cached" is a measurement, not a claim.
struct CountingReader {
    /// Shared, because the oracle takes the reader BY VALUE. Counting through a
    /// field the test no longer owns is how the first version of this contract
    /// passed while the stub was still caching.
    reads: std::sync::Arc<AtomicU32>,
    answer: Option<ProcIdentity>,
    uid: Option<u32>,
}

impl RawIdentityReader for CountingReader {
    fn read(&self, _pid: u32) -> Option<ProcIdentity> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
    fn self_uid(&self) -> Option<u32> {
        self.uid
    }
}

/// Every question must be answered by a NEW read. A cached identity is a stale
/// identity, and the whole re-read discipline — preflight, immediate pre-TERM,
/// again before KILL — is worthless if the second and third reads are served
/// from the first.
#[test]
fn the_production_oracle_never_answers_from_memory_3273() {
    let reads = std::sync::Arc::new(AtomicU32::new(0));
    let oracle = FreshIdentityOracle::new(CountingReader {
        reads: std::sync::Arc::clone(&reads),
        answer: Some(ProcIdentity {
            start_token: 111_000,
            uid: 501,
        }),
        uid: Some(501),
    });

    let _ = oracle.identity(4242);
    let _ = oracle.identity(4242);
    let _ = oracle.identity(4242);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        3,
        "three questions must mean three reads: a cached identity is a stale identity, \
         and the preflight/pre-TERM/pre-KILL re-reads are worthless if served from the first"
    );

    // An unreadable identity must propagate as None every time, never be
    // remembered as "already answered".
    let unknown_reads = std::sync::Arc::new(AtomicU32::new(0));
    let unknown = FreshIdentityOracle::new(CountingReader {
        reads: std::sync::Arc::clone(&unknown_reads),
        answer: None,
        uid: None,
    });
    assert_eq!(unknown.identity(4242), None);
    assert_eq!(unknown.identity(4242), None);
    assert_eq!(
        unknown_reads.load(Ordering::SeqCst),
        2,
        "an unknown answer must be re-asked, not cached as known-unknown"
    );
}

/// An unknown uid fails closed at the source: the oracle reports what the OS
/// said, and `None` is never smoothed into a value.
#[test]
fn an_unknown_self_uid_propagates_as_none_3273() {
    let oracle = FreshIdentityOracle::new(CountingReader {
        reads: std::sync::Arc::new(AtomicU32::new(0)),
        answer: None,
        uid: None,
    });
    assert_eq!(oracle.self_uid(), None);
}

/// The errno mapping is where "the signal was sent" stops being an assumption.
/// `process::terminate` returns `()`, which is exactly why this exists.
#[test]
fn the_syscall_mapping_reports_what_the_kernel_said_3273() {
    assert_eq!(
        signal_outcome_from_syscall(0, 0),
        SignalOutcome::Delivered,
        "rc 0 is the only success"
    );
    assert_eq!(
        signal_outcome_from_syscall(-1, libc::ESRCH),
        SignalOutcome::NoSuchProcess,
        "ESRCH is 'already gone', which is success-equivalent but NOT delivery"
    );
    assert_eq!(
        signal_outcome_from_syscall(-1, libc::EPERM),
        SignalOutcome::PermissionDenied,
        "EPERM must be distinguishable: it is a fail-stop, ESRCH is not"
    );
    match signal_outcome_from_syscall(-1, libc::EINVAL) {
        SignalOutcome::Failed(_) => {}
        other => panic!("an unexpected errno must surface as Failed, got {other:?}"),
    }
}

#[derive(Default)]
struct RecordingSleeper {
    total_ms: AtomicU64,
}

impl Sleeper for RecordingSleeper {
    fn sleep_ms(&self, ms: u64) {
        self.total_ms.fetch_add(ms, Ordering::SeqCst);
    }
}

struct ScriptedProbe {
    /// number of probes before the process is reported gone; u32::MAX = never
    exits_after: AtomicU32,
    probes: AtomicU32,
}

impl LivenessProbe for ScriptedProbe {
    fn is_alive(&self, _pid: ExactPid) -> bool {
        self.probes.fetch_add(1, Ordering::SeqCst);
        let remaining = self.exits_after.load(Ordering::SeqCst);
        if remaining == 0 {
            return false;
        }
        if remaining != u32::MAX {
            self.exits_after.store(remaining - 1, Ordering::SeqCst);
        }
        true
    }
}

/// A process that never exits must not extend the wait past its bound. The
/// grace window is a promise to the operator about how long `apply` can block,
/// and an unbounded poll would quietly break it.
#[test]
fn the_bounded_waiter_never_outstays_its_bound_3273() {
    let waiter = BoundedWaiter {
        probe: ScriptedProbe {
            exits_after: AtomicU32::new(u32::MAX),
            probes: AtomicU32::new(0),
        },
        sleeper: RecordingSleeper::default(),
        poll_ms: 50,
    };
    let exited = waiter.wait_for_exit(ExactPid::new(4242).unwrap(), 200);
    assert!(
        !exited,
        "a process that never exits must be reported as surviving"
    );
    assert!(
        waiter.sleeper.total_ms.load(Ordering::SeqCst) <= 200,
        "slept {}ms against a 200ms bound",
        waiter.sleeper.total_ms.load(Ordering::SeqCst)
    );
    assert!(
        waiter.sleeper.total_ms.load(Ordering::SeqCst) > 0,
        "a bounded wait must actually wait; returning instantly is not a grace window"
    );
}

/// And it must return as soon as the process is gone, rather than always
/// spending the whole bound.
#[test]
fn the_bounded_waiter_returns_as_soon_as_the_process_exits_3273() {
    let waiter = BoundedWaiter {
        probe: ScriptedProbe {
            exits_after: AtomicU32::new(1),
            probes: AtomicU32::new(0),
        },
        sleeper: RecordingSleeper::default(),
        poll_ms: 50,
    };
    assert!(waiter.wait_for_exit(ExactPid::new(4242).unwrap(), 5_000));
    assert!(
        waiter.probe.probes.load(Ordering::SeqCst) > 0,
        "returning true without ever asking whether the process is alive is not a wait"
    );
    assert!(
        waiter.sleeper.total_ms.load(Ordering::SeqCst) < 5_000,
        "an early exit must not still burn the full grace window"
    );
}

/// The audit is the record of intent that licenses a signal. If it is not on
/// disk, the signal must not happen — so the store reports durability only when
/// it has actually established it.
#[test]
fn the_audit_store_appends_durably_and_privately_3273() {
    let dir = std::env::temp_dir().join(format!("agend-audit-{}-ok", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("orphan-cleanup-audit.jsonl");
    let store = JsonlAuditStore { path: path.clone() };

    let record = |pid: u32| PreSignalAudit {
        token: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
        actor: "operator".to_string(),
        audit_reason: "manual cleanup".to_string(),
        candidate_id: format!("id-{pid}"),
        pid,
        start_token: 111_000,
        uid: 501,
        at_ms: 1_700_000_000_000,
    };

    store
        .record_pre_signal(&record(4242))
        .expect("first record must persist");
    store
        .record_pre_signal(&record(4343))
        .expect("second record must persist");

    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("audit must exist at {}: {e}", path.display()));
    let lines: Vec<_> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        2,
        "append-only: one line per record, got {lines:?}"
    );
    let first: PreSignalAudit = serde_json::from_str(lines[0]).expect("line 1 must be a record");
    assert_eq!(
        first.pid, 4242,
        "the first record must survive the second write"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the audit names exact pids; keep it owner-only, got {mode:o}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// A store that cannot write must SAY so. Reporting success it never achieved
/// is the one failure that turns the whole pre-signal audit into decoration.
#[test]
fn an_unwritable_audit_store_reports_failure_3273() {
    let dir = std::env::temp_dir().join(format!("agend-audit-{}-fail", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // Occupy the audit's own parent path with a regular file: deterministic,
    // and no permissions assumptions.
    let occupied = dir.join("blocked");
    std::fs::write(&occupied, b"not a directory").unwrap();
    let store = JsonlAuditStore {
        path: occupied.join("orphan-cleanup-audit.jsonl"),
    };

    let record = PreSignalAudit {
        token: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
        actor: "operator".to_string(),
        audit_reason: "manual cleanup".to_string(),
        candidate_id: "id".to_string(),
        pid: 4242,
        start_token: 111_000,
        uid: 501,
        at_ms: 1_700_000_000_000,
    };
    assert!(
        store.record_pre_signal(&record).is_err(),
        "a store that cannot write must not report durability"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// The CLI surface. Manual means an operator typed something explicit; these
// contracts run the real binary so the clap wiring itself is the thing pinned,
// not a helper that happens to exist beside it.
// ---------------------------------------------------------------------------

fn agend() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("agend-terminal").expect("binary must exist")
}

/// `doctor orphans preview` must exist as an explicit subcommand. The manual
/// surface is the whole safety story: cleanup that can only be reached by
/// typing it cannot happen by accident.
#[test]
fn doctor_orphans_preview_is_an_explicit_subcommand_3273() {
    let out = agend()
        .args(["doctor", "orphans", "preview", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`doctor orphans preview` must parse; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `apply` must refuse to parse without its authority arguments. Making them
/// required at the parser is a cheaper guarantee than checking them later:
/// there is no code path in which they can be absent.
#[test]
fn doctor_orphans_apply_requires_its_authority_arguments_3273() {
    let help = agend()
        .args(["doctor", "orphans", "apply", "--help"])
        .output()
        .unwrap();
    assert!(
        help.status.success(),
        "`doctor orphans apply` must parse; stderr: {}",
        String::from_utf8_lossy(&help.stderr)
    );

    // Nothing supplied at all.
    let bare = agend()
        .args(["doctor", "orphans", "apply"])
        .output()
        .unwrap();
    assert!(
        !bare.status.success(),
        "apply with no token, reason or confirm-id must refuse"
    );

    // Each argument missing in turn.
    for missing in [
        vec!["--audit-reason", "cleanup", "--confirm-id", "abc"],
        vec![
            "--token",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "--confirm-id",
            "abc",
        ],
        vec![
            "--token",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "--audit-reason",
            "cleanup",
        ],
    ] {
        let mut args = vec!["doctor", "orphans", "apply"];
        args.extend(missing.iter().copied());
        let out = agend().args(&args).output().unwrap();
        assert!(
            !out.status.success(),
            "apply must refuse when an authority argument is missing: {args:?}"
        );
    }
}

/// `doctor` with no subcommand stays a report. It must not prompt, must not
/// consume a confirmation, and must not signal — so it must not even mention
/// the manual surface in its output.
#[test]
fn bare_doctor_remains_report_only_3273() {
    let home = std::env::temp_dir().join(format!("agend-doctor-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = agend()
        .args(["doctor"])
        .env("AGEND_HOME", &home)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for forbidden in ["confirm_token", "confirm-id", "SIGTERM", "SIGKILL"] {
        assert!(
            !combined.contains(forbidden),
            "the default doctor report must not mention `{forbidden}`: it reports, it does not act"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// The concrete Unix adapters, around an injected syscall seam so the target and
// signal number are observable without touching a real process.
// ---------------------------------------------------------------------------

use agend_terminal::admin::orphan_cleanup::{
    signal_kill, signal_term, KillSyscall, Signaler as _, UnixSignaler,
};

#[derive(Default)]
struct RecordingSyscall {
    calls: std::sync::Mutex<Vec<(i32, i32)>>,
    rc: i32,
    errno: i32,
}

impl KillSyscall for RecordingSyscall {
    fn kill(&self, pid: i32, signal: i32) -> (i32, i32) {
        self.calls.lock().unwrap().push((pid, signal));
        (self.rc, self.errno)
    }
}

/// The target must be the exact positive pid. A NEGATIVE argument to `kill(2)`
/// means "process group", which is precisely the blast radius the consensus
/// forbids — and the mistake is invisible unless the target is asserted.
#[test]
fn the_unix_signaler_targets_one_exact_positive_pid_3273() {
    let signaler = UnixSignaler {
        syscall: RecordingSyscall::default(),
    };
    let pid = ExactPid::new(4242).expect("4242 is a valid exact pid");

    let _ = signaler.term(pid);
    let _ = signaler.kill(pid);

    let calls = signaler.syscall.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(4242, signal_term()), (4242, signal_kill())],
        "TERM then KILL, each to the exact POSITIVE pid; a negative target is a process group"
    );
    for (target, _) in &calls {
        assert!(
            *target > 0,
            "kill({target}, …) targets a process group, not a process"
        );
    }
}

/// EPERM means the process exists but is not ours. For a grace window that is
/// "alive": treating it as gone would escalate against something we already
/// know we cannot signal.
#[cfg(unix)]
#[test]
fn the_liveness_probe_treats_eperm_as_alive_3273() {
    // Imported here rather than at module scope: this contract is Unix-only, so
    // a top-level import would be an unused one off Unix and `-D warnings`
    // rejects it. Same placement as `LiveIdentityReader` below.
    use agend_terminal::admin::orphan_cleanup::KillProbe;

    let gone = KillProbe {
        syscall: RecordingSyscall {
            rc: -1,
            errno: libc::ESRCH,
            ..Default::default()
        },
    };
    let forbidden = KillProbe {
        syscall: RecordingSyscall {
            rc: -1,
            errno: libc::EPERM,
            ..Default::default()
        },
    };
    let alive = KillProbe {
        syscall: RecordingSyscall::default(),
    };
    let pid = ExactPid::new(4242).unwrap();

    assert!(!gone.is_alive(pid), "ESRCH is gone");
    assert!(alive.is_alive(pid), "rc 0 is alive");
    assert!(
        forbidden.is_alive(pid),
        "EPERM means it EXISTS and is not ours — reporting it dead would escalate blindly"
    );
    // And the probe must use signal 0, not a real signal.
    let calls = alive.syscall.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(4242, 0)],
        "a liveness probe must not send a real signal"
    );
}

/// The production reader must actually read. Our own process is the one pid we
/// can assert about without racing anything.
#[cfg(unix)]
#[test]
fn the_live_reader_identifies_this_process_3273() {
    use agend_terminal::admin::orphan_cleanup::LiveIdentityReader;
    let reader = LiveIdentityReader;

    let uid = reader.self_uid().expect("our own uid must be readable");
    let me = reader
        .read(std::process::id())
        .expect("our own process must be identifiable");
    assert!(
        me.start_token != 0,
        "a start token of 0 is indistinguishable from 'unknown'"
    );
    assert_eq!(me.uid, uid, "our own process is owned by us");

    assert_eq!(
        reader.read(0),
        None,
        "pid 0 is never a real tracked process"
    );
}

// ---------------------------------------------------------------------------
// The CLI handler seam. clap parsing can go green while the command is a no-op
// or wired to the wrong authority, so what the handler FORWARDS is pinned, not
// just that the subcommand exists.
// ---------------------------------------------------------------------------

use agend_terminal::admin::orphan_cleanup::{
    handle_apply_command, handle_preview_command, ApplyOutcome, ApplyRequest, OrphanCleanupService,
    Preview, PreviewError, PreviewRequest, RefusalReason,
};

/// Records exactly what it was asked to do, so "forwarded once, unchanged" is a
/// measurement rather than a reading of the handler's source.
/// (actor, audit_reason, proposed_pids)
type PreviewCall = (String, String, Vec<u32>);
/// (token, actor, audit_reason, confirm_ids)
type ApplyCall = (String, String, String, Vec<String>);

#[derive(Default)]
struct RecordingService {
    previews: std::sync::Mutex<Vec<PreviewCall>>,
    applies: std::sync::Mutex<Vec<ApplyCall>>,
    apply_outcome: Option<ApplyOutcome>,
}

impl OrphanCleanupService for RecordingService {
    fn preview(&self, request: PreviewRequest<'_>) -> Result<Preview, PreviewError> {
        self.previews.lock().unwrap().push((
            request.actor.to_string(),
            request.audit_reason.to_string(),
            request.proposed_pids.to_vec(),
        ));
        Err(PreviewError::PersistFailed("stub".into()))
    }
    fn apply(&self, request: ApplyRequest<'_>) -> ApplyOutcome {
        self.applies.lock().unwrap().push((
            request.token.to_string(),
            request.actor.to_string(),
            request.audit_reason.to_string(),
            request.confirm_ids.to_vec(),
        ));
        self.apply_outcome
            .clone()
            .unwrap_or(ApplyOutcome::Refused(RefusalReason::ExecutorUnavailable))
    }
}

/// The preview command must forward the operator's actor, a non-empty reason
/// and the exact proposed pid list — once. Forwarding a subset, or twice, or a
/// list the operator did not give, would mean the printed snapshot describes
/// something other than what was asked for.
#[test]
fn the_preview_handler_forwards_the_operators_request_once_3273() {
    let service = RecordingService::default();
    let _ = handle_preview_command(&service, "operator", "two stuck tool shells", &[4242, 4343]);

    let calls = service.previews.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        1,
        "the service must be called exactly once: {calls:?}"
    );
    assert_eq!(
        calls[0],
        (
            "operator".to_string(),
            "two stuck tool shells".to_string(),
            vec![4242, 4343]
        ),
        "actor, reason and the exact pid list must arrive unchanged"
    );
}

/// The apply command must forward the token, actor, reason and the EXACT
/// confirm-id set. A handler that dropped or reordered an id would be asking
/// the executor to act on a set the operator never confirmed.
#[test]
fn the_apply_handler_forwards_the_exact_confirmed_set_once_3273() {
    let service = RecordingService::default();
    let ids = vec!["id-a".to_string(), "id-b".to_string()];
    let _ = handle_apply_command(
        &service,
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "operator",
        "two stuck tool shells",
        &ids,
    );

    let calls = service.applies.lock().unwrap().clone();
    assert_eq!(
        calls.len(),
        1,
        "the service must be called exactly once: {calls:?}"
    );
    assert_eq!(
        calls[0],
        (
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            "operator".to_string(),
            "two stuck tool shells".to_string(),
            ids
        ),
        "token, actor, reason and the exact confirm-id set must arrive unchanged"
    );
}

/// A refusal must reach the operator AS a refusal. Reporting success for a
/// confirmation that was rejected is the worst available outcome: the operator
/// believes the processes are gone and stops looking.
#[test]
fn the_apply_handler_propagates_a_refusal_3273() {
    let service = RecordingService {
        apply_outcome: Some(ApplyOutcome::Refused(RefusalReason::Expired)),
        ..Default::default()
    };
    let result = handle_apply_command(
        &service,
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "operator",
        "cleanup",
        &["id-a".to_string()],
    );
    match result {
        Err(message) => assert!(
            message.to_lowercase().contains("expired"),
            "the refusal must name its cause to the operator, got {message:?}"
        ),
        Ok(rendered) => panic!("a refused apply must not be reported as success: {rendered:?}"),
    }
}

/// A preview that could not be issued must not print a token-shaped success.
#[test]
fn the_preview_handler_propagates_a_failure_3273() {
    let service = RecordingService::default(); // its preview always fails
    match handle_preview_command(&service, "operator", "cleanup", &[4242]) {
        Err(_) => {}
        Ok(rendered) => panic!("a failed preview must not be reported as success: {rendered:?}"),
    }
}

// ---------------------------------------------------------------------------
// End to end: parser → handler → concrete service. clap and the handler can
// each pass in isolation while dispatch never connects them, so the wiring
// itself gets a contract.
// ---------------------------------------------------------------------------

/// `preview` must run the real path and leave the real artefact: a token and at
/// least one candidate id on stdout, and the matching private sidecar on disk.
/// Preview signals nothing by construction — it is the dry run — so this is
/// safe to execute against our own pid.
#[test]
fn preview_end_to_end_emits_a_token_and_persists_its_sidecar_3273() {
    let home = std::env::temp_dir().join(format!("agend-orphans-preview-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let out = agend()
        .args([
            "doctor",
            "orphans",
            "preview",
            "--actor",
            "operator",
            "--audit-reason",
            "end to end contract",
            "--pid",
            &std::process::id().to_string(),
        ])
        .env("AGEND_HOME", &home)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "preview must succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // A token, in the exact shape apply will accept.
    let token = stdout
        .split_whitespace()
        .find(|word| word.len() == 36 && word.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .unwrap_or_else(|| panic!("preview must print a confirmation token; stdout: {stdout}"));

    // And the sidecar it names must actually exist, privately.
    let sidecar = home.join("orphan-cleanup").join(format!("{token}.json"));
    assert!(
        sidecar.exists(),
        "the printed token must name a persisted confirmation at {}",
        sidecar.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the sidecar is authority material; got {mode:o}"
        );
    }

    // At least one candidate id, or the operator has nothing to confirm.
    assert!(
        stdout.contains("candidate"),
        "preview must name its candidates; stdout: {stdout}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// `apply` must reach the executor and refuse. A well-shaped token with no
/// confirmation behind it can never signal anything, so this exercises the real
/// dispatch path with no risk to any process.
#[test]
fn apply_end_to_end_refuses_an_unknown_confirmation_3273() {
    let home = std::env::temp_dir().join(format!("agend-orphans-apply-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let out = agend()
        .args([
            "doctor",
            "orphans",
            "apply",
            "--token",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "--actor",
            "operator",
            "--audit-reason",
            "end to end contract",
            "--confirm-id",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ])
        .env("AGEND_HOME", &home)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "a refusal must be a non-zero exit, or a script cannot tell it from success"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    assert!(
        combined.contains("confirmation") || combined.contains("unavailable"),
        "the refusal must name its cause to the operator; output: {combined}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Off Unix there is no identity oracle and no signal adapter, so the manual
/// path is report-only. Only Windows CI executes this; it is named so its
/// absence from a local run is visible rather than assumed.
#[cfg(not(unix))]
#[test]
fn apply_is_unsupported_off_unix_3273() {
    assert!(
        matches!(platform_support(), Support::Unsupported(_)),
        "a non-Unix build must be explicitly unsupported"
    );
}
