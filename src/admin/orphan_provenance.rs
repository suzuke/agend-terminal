//! #3273 V1 — report-only visibility for orphaned agent-attributable
//! processes, plus a non-authoritative tool-call observation ledger.
//!
//! Decision `d-20260814163653190377-20` (both RCAs, plus the adversarial
//! addendum) ordered reporting BEFORE any authority, because attribution good
//! enough to authorise a signal is not obtainable today:
//!
//! * Codex executes tool commands from an app-server sibling, so its backend
//!   has no children to sample at all.
//! * Claude spawns recurring direct children that are not tool shells (a
//!   `caffeinate` alongside the MCP servers), so "the unique new direct child"
//!   is routinely not unique.
//! * Parallel shell tool calls make the same window ambiguous by construction.
//! * OpenCode's topology is unknown.
//!
//! So this module reports and never acts. The ledger holds OBSERVATIONS: they
//! may make a report more informative (a suggested owner), and they may never
//! make a process actionable. Every candidate is UNPROVEN. There is
//! deliberately no signalling surface here at all — an invariant test walks
//! this file's AST and rejects one.
//!
//! `PPID == 1`, argv, cwd, elapsed and CPU are display columns. The RCA census
//! found seven matches for the argv/cwd predicate on one machine, including a
//! 44-day `ngrok` and a 44-day `python3` that were load-bearing, and no age
//! threshold separates those from a 40-hour leak.
//!
//! Self-contained by construction: this file is compiled into both the binary
//! crate and the library shim (`src/lib.rs` re-exports it for integration
//! tests), so it depends on `std` and `serde` only. Platform knowledge lives
//! behind [`ProcessOracle`], implemented by the daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Platform seam
// ---------------------------------------------------------------------------

/// Whether this platform can attest the facts V1 reports on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceSupport {
    Supported,
    /// Windows has no POSIX reparenting or session semantics, so "orphan" is
    /// not expressible the way this report means it. Say so rather than match
    /// nothing and read as "all clear".
    Unsupported {
        platform: &'static str,
        reason: &'static str,
    },
}

/// Columns an operator reads to decide. NONE of them is evidence of ownership.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayColumns {
    pub argv: Option<String>,
    pub cwd: Option<String>,
    pub elapsed_secs: Option<u64>,
    pub cpu_percent: Option<f32>,
}

/// Kernel-attested facts about one process, plus its display columns.
#[derive(Debug, Clone)]
pub struct ProcFacts {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub sid: Option<u32>,
    pub pgid: Option<u32>,
    pub uid: Option<u32>,
    /// OS start-time token — the identity that survives PID reuse.
    pub start_token: Option<u64>,
    /// Process start time in epoch milliseconds (the `lstart` column).
    pub lstart_ms: Option<i64>,
    pub display: DisplayColumns,
}

/// One process-table row as the scope filter sees it.
pub struct ScopeInput {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    /// `None` when the session could not be read at all.
    pub sid: Option<u32>,
}

/// What the scope filter kept and, more importantly, what it threw away. An
/// exclusion that is never counted is indistinguishable from a process that
/// does not exist, and this report exists to be honest about what it cannot
/// see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeCounts {
    pub scanned: usize,
    pub included: usize,
    pub excluded_not_reparented: usize,
    pub excluded_foreign_uid: usize,
    /// Leads its own session (`sid == pid`). A scope boundary, not a verdict
    /// about the process.
    pub excluded_session_leader: usize,
    /// Session 1. A scope boundary, not a verdict about the process.
    pub excluded_init_session: usize,
    /// `getsid` failed. Excluded because we cannot tell, never guessed.
    pub excluded_session_unknown: usize,
}

/// The scoping rule, as a pure function so it can be pinned exactly.
///
/// `PPID == 1` on its own makes an unreadable list: on macOS launchd is the
/// parent of hundreds of per-user agents and XPC services. This view therefore
/// narrows to processes holding a session id they did not create — the shape
/// the #3273 RCA measured for a background job outliving its tool-call shell.
///
/// That narrowing is a SCOPING CHOICE, not a finding about ownership. A process
/// in session 1, or one leading its own session, is not thereby shown to be
/// nobody's leftover; it is a topology category this view does not cover, and
/// therefore a blind spot — the `setsid` case is the obvious counterexample.
/// Nothing here concludes anything about who owns what: every survivor is still
/// UNPROVEN, and every exclusion is counted and printed.
///
/// Same-uid is a capability boundary, not a guess about ownership: agend could
/// not act on another user's process even if it wanted to.
pub fn scope_reparented(rows: &[ScopeInput], self_uid: Option<u32>) -> (Vec<u32>, ScopeCounts) {
    let mut counts = ScopeCounts {
        scanned: rows.len(),
        ..ScopeCounts::default()
    };
    let mut included = Vec::new();
    for row in rows {
        if row.ppid != 1 {
            counts.excluded_not_reparented += 1;
            continue;
        }
        if self_uid.is_some_and(|me| me != row.uid) {
            counts.excluded_foreign_uid += 1;
            continue;
        }
        let Some(sid) = row.sid else {
            counts.excluded_session_unknown += 1;
            continue;
        };
        if sid == 1 {
            counts.excluded_init_session += 1;
            continue;
        }
        if sid == row.pid {
            counts.excluded_session_leader += 1;
            continue;
        }
        counts.included += 1;
        included.push(row.pid);
    }
    included.sort_unstable();
    (included, counts)
}

/// Everything this module needs to know about the process table. Injected so
/// the contract is testable without spawning anything.
pub trait ProcessOracle {
    fn support(&self) -> ProvenanceSupport;
    fn facts(&self, pid: u32) -> Option<ProcFacts>;
    fn children_of(&self, pid: u32) -> Vec<u32>;
    /// Processes reparented to init (`PPID == 1`) that survived scoping. Empty
    /// when unsupported.
    fn reparented(&self) -> Vec<u32>;
    /// What scoping kept and discarded, for the report to state. Defaults to
    /// all-zero so an oracle that does not scope (the test doubles) needs no
    /// bookkeeping; call it AFTER `reparented`.
    fn reparented_scope(&self) -> ScopeCounts {
        ScopeCounts::default()
    }
    fn is_alive(&self, pid: u32) -> bool;
}

// ---------------------------------------------------------------------------
// Owner service
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellToolKind {
    Bash,
    Shell,
    ExecCommand,
}

/// Evidence that an instance currently has a shell-shaped tool call in flight.
/// `generation` is monotonic per instance, so successive calls are distinct.
#[derive(Debug, Clone)]
pub struct ShellToolEvidence {
    pub kind: ShellToolKind,
    pub generation: u64,
    pub started_ms: i64,
}

/// One live backend and what it is currently running.
#[derive(Debug, Clone)]
pub struct BackendOwner {
    pub instance: String,
    pub backend_pid: u32,
    pub active_shell_tool: Option<ShellToolEvidence>,
}

/// The daemon's view of live backends. This is the ONLY input besides the
/// process oracle, which is what keeps sampling independent of the
/// subscriber-gated metrics sweep.
pub trait BackendOwnerSource {
    fn live_backends(&self) -> Vec<BackendOwner>;
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// One observed tool-call shell. NOT a claim of ownership: it records what was
/// seen, so a later report can SUGGEST an instance and an operator can judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellObservation {
    pub instance: String,
    pub shell_pid: u32,
    /// The ACTUAL session id, whatever it was — no shape is required.
    pub sid: u32,
    /// The ACTUAL process group id.
    pub pgid: u32,
    pub shell_start_token: u64,
    pub tool_generation: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// Why a generation produced no observation. Every one of these is REPORTED:
/// a hygiene tool that silently observes nothing reads as "nothing is wrong".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationMiss {
    /// The instance has no shell-shaped tool call in flight.
    NoActiveShellTool,
    /// No pre-tool baseline exists for this backend (daemon restarted while the
    /// call was already running, or the backend pid is a new process), so a
    /// child cannot be told apart from one that was already there.
    BaselineUnavailable,
    /// A baseline exists and nothing new appeared — the shell was born and
    /// reaped between two ticks, or it is not a child at all.
    NoNewCandidate,
    /// More than one new child appeared in the window (parallel tool calls, or
    /// a helper such as `caffeinate` starting alongside the shell).
    AmbiguousCandidates,
    /// The backend has no children whatsoever, so whatever runs the tool
    /// command is not below it (measured: the Codex app-server sibling).
    ExecutionOwnerNotABackendChild,
    /// The backend's OS start token could not be read, so its identity is
    /// unknown. `src/process.rs` documents `None` as "identity unknown, fail
    /// closed", and keying a baseline as `(pid, None)` would let a recycled
    /// backend pid inherit the previous generation's child list — after which a
    /// fresh child could be recorded as an observation for the wrong owner.
    BackendIdentityUnknown,
}

/// Whether the ledger write actually landed. A sample that could not be
/// persisted reports nothing as observed — claiming otherwise would be a
/// durable-looking record that does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    Written,
    Failed(String),
}

/// Result of one sampling pass.
#[derive(Debug)]
pub struct SampleOutcome {
    pub support: ProvenanceSupport,
    pub observed: Vec<ShellObservation>,
    pub misses: Vec<(String, ObservationMiss)>,
    pub persisted: PersistOutcome,
}

/// The pre-tool child set for one backend identity, maintained continuously
/// while no shell tool is active. Keyed by pid AND start token so a reused
/// backend pid cannot inherit a stale baseline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BackendBaseline {
    backend_pid: u32,
    /// Never optional: an unknown backend identity is refused before a baseline
    /// is read or written, so `(pid, unknown)` cannot exist.
    backend_start_token: u64,
    /// `(pid, start_token)` of children present before the current generation.
    known: Vec<(u32, u64)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    observations: Vec<ShellObservation>,
    #[serde(default)]
    baselines: Vec<BackendBaseline>,
}

/// Bounded diagnostic horizon for keeping observations — a storage decision,
/// NOT a claim about how long an orphan lives. Orphans routinely outlive it:
/// the #3273 RCA census found 44-day-old processes. When an observation ages
/// out, the process it related to is still listed, still UNPROVEN, and simply
/// loses its suggested owner; expiry never means the orphan is gone.
/// Seven days covers the 40-hour incident with wide margin while bounding the
/// file, and it is what makes `last_seen_ms` load-bearing rather than
/// decorative.
pub const OBSERVATION_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

/// Hard cap so a pathological burst inside the retention window cannot outgrow
/// it. Oldest-by-`last_seen_ms` are dropped first.
pub const MAX_OBSERVATIONS: usize = 4_096;

/// Canonical ledger location. The fail-closed tests make this path unwritable,
/// so a rename is a contract change.
fn ledger_path(home: &Path) -> PathBuf {
    home.join("state").join("tool_call_observations.json")
}

fn read_ledger(home: &Path) -> LedgerFile {
    let path = ledger_path(home);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return LedgerFile::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn write_ledger(home: &Path, ledger: &LedgerFile) -> PersistOutcome {
    let path = ledger_path(home);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return PersistOutcome::Failed(format!("create {}: {e}", parent.display()));
        }
    }
    let body = match serde_json::to_string_pretty(ledger) {
        Ok(body) => body,
        Err(e) => return PersistOutcome::Failed(format!("encode ledger: {e}")),
    };
    match atomic_write(&path, body.as_bytes()) {
        Ok(()) => PersistOutcome::Written,
        Err(e) => PersistOutcome::Failed(format!("write {}: {e}", path.display())),
    }
}

/// Write via a sibling temp file, fsync, then rename, so a reader never sees a
/// half-written ledger. `read_ledger` recovers from a corrupt file by returning
/// an empty one, which would silently discard every observation and baseline —
/// so the write must not be able to tear in the first place.
///
/// Deliberately local rather than `crate::store::atomic_write`, which has the
/// same semantics: this module is compiled into the library shim as well as the
/// binary (see the module docs), and `store` depends on `event_log`,
/// `sync_audit` and `paths`, none of which exist there. The duplication is the
/// price of that self-containment and is called out here so it is a choice
/// rather than an oversight.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    // Deterministic failure seam, mirroring the repo's `AGEND_FORCE_*` idiom, so
    // the "a failed write preserves the last valid ledger" contract can be
    // tested without ambient filesystem permissions — those can lie under
    // elevated or ACL-governed environments and are Unix-shaped besides.
    //
    // Scoped to an exact path rather than a bare on/off flag: the variable is
    // process-global, and an integration-test binary runs its tests as threads,
    // so a boolean seam would make unrelated concurrent tests fail. Keyed by
    // path, only the fixture that opted in is affected — the same shape as
    // `store::atomic_write`'s path-keyed failure registry.
    if std::env::var_os("AGEND_ORPHAN_LEDGER_FAIL_WRITE")
        .is_some_and(|target| target == path.as_os_str())
    {
        return Err(std::io::Error::other(
            "forced ledger write failure (AGEND_ORPHAN_LEDGER_FAIL_WRITE)",
        ));
    }

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Unique per process AND per call, so concurrent writers cannot collide on
    // the temp name — same shape as `store::atomic_write`.
    let tmp = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    let write_then_rename = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // `std::fs::rename` replaces an existing destination on every supported
        // platform (MoveFileExW on Windows), which is what `store::atomic_write`
        // relies on too.
        std::fs::rename(&tmp, path)?;
        // Durability parity with `store::atomic_write`: fsync the parent so the
        // new directory entry survives a crash right after the rename. Unix
        // idiom; std has no clean Windows equivalent, so it is skipped there.
        #[cfg(unix)]
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    };
    match write_then_rename() {
        Ok(()) => Ok(()),
        Err(e) => {
            // Never leave the temp behind on failure.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Bound the ledger. Nothing else in the crate evicts from it, so without this
/// it grows once per backend generation and once per tool-call shell, forever,
/// and every sample pays an O(n) read + serialize + write on the daemon tick.
/// A diagnostic that exists to catch unbounded resource growth must not be an
/// instance of it.
fn prune(ledger: &mut LedgerFile, live_owners: &[(u32, u64)], now_ms: i64) {
    // Retained against the CURRENT OWNER identities, not merely "some live
    // process still has that pid": an instance that was removed while its
    // backend process kept running would otherwise leave its baseline in the
    // file forever, which is the same unbounded growth by another route.
    ledger
        .baselines
        .retain(|b| live_owners.contains(&(b.backend_pid, b.backend_start_token)));
    // Observations outlive their shell on purpose — a dead shell's orphans are
    // exactly what the report relates — so age them out rather than dropping
    // them when the pid dies.
    prune_observations(&mut ledger.observations, now_ms);
}

/// Age-out then cap, as a pure function so the bounds are testable without
/// driving thousands of samples through the oracle.
///
/// Deliberately NOT "drop it when the shell exits": a dead shell's observation
/// is exactly what relates its surviving orphans, which is the whole point of
/// the ledger. Recency wins on the cap for the same reason.
pub fn prune_observations(observations: &mut Vec<ShellObservation>, now_ms: i64) {
    observations.retain(|o| now_ms.saturating_sub(o.last_seen_ms) <= OBSERVATION_RETENTION_MS);
    if observations.len() > MAX_OBSERVATIONS {
        observations.sort_by_key(|o| o.last_seen_ms);
        let excess = observations.len() - MAX_OBSERVATIONS;
        observations.drain(..excess);
    }
}

/// Ledger size, so a caller can prove what pruning did rather than infer it
/// from downstream behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerStats {
    pub observations: usize,
    pub baselines: usize,
}

/// Counts of what is currently persisted.
pub fn ledger_stats(home: &Path) -> LedgerStats {
    let ledger = read_ledger(home);
    LedgerStats {
        observations: ledger.observations.len(),
        baselines: ledger.baselines.len(),
    }
}

/// Every observation recorded so far. An unreadable or non-regular ledger reads
/// as empty — the report then lists candidates with no suggested owner, which
/// is the honest degradation.
pub fn load_ledger(home: &Path) -> Vec<ShellObservation> {
    read_ledger(home).observations
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

/// Sample every live backend from an owner service. This is the seam the daemon
/// per-tick handler runs on: an oracle and an owner source, and nothing else —
/// there is no metrics handle to pass, so a headless daemon and a subscribed
/// TUI observe identically.
pub fn sample_from_owner_source(
    home: &Path,
    oracle: &dyn ProcessOracle,
    owners: &dyn BackendOwnerSource,
    now_ms: i64,
) -> SampleOutcome {
    sample_tool_call_shells(home, oracle, &owners.live_backends(), now_ms)
}

/// One sampling pass over the given backends.
pub fn sample_tool_call_shells(
    home: &Path,
    oracle: &dyn ProcessOracle,
    owners: &[BackendOwner],
    now_ms: i64,
) -> SampleOutcome {
    let support = oracle.support();
    if !matches!(support, ProvenanceSupport::Supported) {
        return SampleOutcome {
            support,
            observed: Vec::new(),
            misses: Vec::new(),
            persisted: PersistOutcome::Written,
        };
    }

    let mut ledger = read_ledger(home);
    let mut observed = Vec::new();
    let mut misses = Vec::new();
    // Exact identities of the backends this tick actually owns; anything else
    // in the ledger is unreachable state.
    let mut live_owner_identities: Vec<(u32, u64)> = Vec::new();

    for owner in owners {
        let Some(backend_token) = oracle.facts(owner.backend_pid).and_then(|f| f.start_token)
        else {
            // Fail closed BEFORE touching the ledger, in either direction: no
            // baseline read, no baseline write, no observation. A visible miss
            // rather than a silent skip.
            misses.push((
                owner.instance.clone(),
                ObservationMiss::BackendIdentityUnknown,
            ));
            continue;
        };
        live_owner_identities.push((owner.backend_pid, backend_token));
        let children: Vec<ProcFacts> = oracle
            .children_of(owner.backend_pid)
            .into_iter()
            .filter_map(|pid| oracle.facts(pid))
            .collect();

        let Some(tool) = owner.active_shell_tool.as_ref() else {
            // Continuously maintained pre-tool baseline: this is the only place
            // a baseline is established, so a generation can never baseline
            // away the very shell it is looking for.
            upsert_baseline(&mut ledger, owner.backend_pid, backend_token, &children);
            misses.push((owner.instance.clone(), ObservationMiss::NoActiveShellTool));
            continue;
        };

        if children.is_empty() {
            // A backend with nothing below it cannot be hiding a tool shell:
            // whatever executes the command is elsewhere (measured: the Codex
            // app-server sibling). That is a STANDING fact about the backend,
            // so it is reported on every such window, not only the first —
            // otherwise the second window downgrades to "nothing new appeared"
            // and the operator never learns agend cannot see the executor.
            // The empty baseline is still recorded, so a backend that later
            // grows a child has something to diff against.
            upsert_baseline(&mut ledger, owner.backend_pid, backend_token, &children);
            misses.push((
                owner.instance.clone(),
                ObservationMiss::ExecutionOwnerNotABackendChild,
            ));
            continue;
        }

        let baseline = ledger
            .baselines
            .iter()
            .find(|b| b.backend_pid == owner.backend_pid && b.backend_start_token == backend_token)
            .cloned();

        let Some(baseline) = baseline else {
            // No pre-tool baseline. Record what is there so the NEXT window has
            // one, and report why this window cannot be attributed.
            upsert_baseline(&mut ledger, owner.backend_pid, backend_token, &children);
            misses.push((owner.instance.clone(), ObservationMiss::BaselineUnavailable));
            continue;
        };

        let fresh: Vec<&ProcFacts> = children
            .iter()
            .filter(|f| {
                let Some(token) = f.start_token else {
                    return false;
                };
                let known = baseline
                    .known
                    .iter()
                    .any(|(p, t)| *p == f.pid && *t == token);
                let already = ledger
                    .observations
                    .iter()
                    .any(|o| o.shell_pid == f.pid && o.shell_start_token == token);
                !known && !already && f.sid.is_some() && f.pgid.is_some()
            })
            .collect();

        match fresh.len() {
            0 => misses.push((owner.instance.clone(), ObservationMiss::NoNewCandidate)),
            1 => {
                let facts = fresh[0];
                let record = ShellObservation {
                    instance: owner.instance.clone(),
                    shell_pid: facts.pid,
                    sid: facts.sid.unwrap_or_default(),
                    pgid: facts.pgid.unwrap_or_default(),
                    shell_start_token: facts.start_token.unwrap_or_default(),
                    tool_generation: tool.generation,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                };
                upsert_observation(&mut ledger, record.clone(), now_ms);
                observed.push(record);
            }
            _ => misses.push((owner.instance.clone(), ObservationMiss::AmbiguousCandidates)),
        }
    }

    prune(&mut ledger, &live_owner_identities, now_ms);
    let persisted = write_ledger(home, &ledger);
    if matches!(persisted, PersistOutcome::Failed(_)) {
        // Nothing durable landed, so nothing may be reported as observed.
        observed.clear();
    }

    SampleOutcome {
        support,
        observed,
        misses,
        persisted,
    }
}

fn upsert_baseline(
    ledger: &mut LedgerFile,
    backend_pid: u32,
    backend_start_token: u64,
    children: &[ProcFacts],
) {
    let known: Vec<(u32, u64)> = children
        .iter()
        .filter_map(|f| f.start_token.map(|t| (f.pid, t)))
        .collect();
    if let Some(existing) = ledger
        .baselines
        .iter_mut()
        .find(|b| b.backend_pid == backend_pid && b.backend_start_token == backend_start_token)
    {
        existing.known = known;
        return;
    }
    ledger.baselines.push(BackendBaseline {
        backend_pid,
        backend_start_token,
        known,
    });
}

fn upsert_observation(ledger: &mut LedgerFile, record: ShellObservation, now_ms: i64) {
    if let Some(existing) = ledger.observations.iter_mut().find(|o| {
        o.shell_pid == record.shell_pid && o.shell_start_token == record.shell_start_token
    }) {
        existing.last_seen_ms = now_ms;
        return;
    }
    ledger.observations.push(record);
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Why a candidate is not actionable. Both variants are UNPROVEN — V1 has no
/// other class, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnprovenReason {
    /// Nothing in the ledger relates to this process.
    NoObservation,
    /// The ledger relates an observed tool-call shell to it. That is the
    /// strongest evidence V1 can hold, and it is still not authority: the
    /// observation itself may have been a mis-attribution, and the container
    /// ids it matches on are reusable.
    ObservedButNotAuthoritative,
}

/// One reported process. Everything here is for a human to read.
#[derive(Debug, Clone)]
pub struct OrphanCandidate {
    pub pid: u32,
    pub start_token: Option<u64>,
    pub lstart_ms: Option<i64>,
    pub sid: Option<u32>,
    pub pgid: Option<u32>,
    /// Whether the process group leader is still running.
    pub leader_alive: Option<bool>,
    pub argv: Option<String>,
    pub cwd: Option<String>,
    pub elapsed_secs: Option<u64>,
    pub cpu_percent: Option<f32>,
    /// Suggested from the observation ledger when one relates. A suggestion,
    /// never a claim.
    pub suggested_instance: Option<String>,
    pub unproven_reason: UnprovenReason,
}

#[derive(Debug)]
pub struct OrphanReport {
    pub support: ProvenanceSupport,
    pub candidates: Vec<OrphanCandidate>,
    /// How the candidate list was scoped. Printed with every result, empty or
    /// not, so a short list is never mistaken for a quiet machine.
    pub scope: ScopeCounts,
}

/// Enumerate reparented processes and relate them to observations where
/// possible. `_now_ms` is accepted for symmetry with the sampler and to keep
/// the surface stable if a future version wants an "as of" line; age is a
/// display column and must never filter, so nothing here uses it to select.
pub fn classify(home: &Path, oracle: &dyn ProcessOracle, _now_ms: i64) -> OrphanReport {
    let support = oracle.support();
    if !matches!(support, ProvenanceSupport::Supported) {
        return OrphanReport {
            support,
            candidates: Vec::new(),
            scope: ScopeCounts::default(),
        };
    }

    let observations = load_ledger(home);
    // Keyed by the CONTAINER the shell was recorded in — its actual pgid and
    // sid — not by its pid. A shell that inherited its session and group has a
    // pid that appears in neither, and that is exactly the case the ledger
    // exists to help with; keying on the pid silently drops it. `pgid` is
    // inserted first so the more specific container wins a collision.
    let mut by_container: HashMap<u32, &ShellObservation> = HashMap::new();
    for observation in &observations {
        by_container.entry(observation.pgid).or_insert(observation);
        by_container.entry(observation.sid).or_insert(observation);
    }

    let mut candidates = Vec::new();
    for pid in oracle.reparented() {
        let Some(facts) = oracle.facts(pid) else {
            continue;
        };
        // An observation relates when the process sits in a container the
        // observed shell led — its group or its session carries that shell's
        // pid. This is how the report SUGGESTS an owner; it grants nothing.
        let related = facts
            .pgid
            .and_then(|g| by_container.get(&g))
            .or_else(|| facts.sid.and_then(|s| by_container.get(&s)))
            .copied();
        let leader_alive = facts
            .pgid
            .filter(|g| *g != pid)
            .map(|g| oracle.is_alive(g))
            .or_else(|| facts.sid.filter(|s| *s != pid).map(|s| oracle.is_alive(s)));

        candidates.push(OrphanCandidate {
            pid,
            start_token: facts.start_token,
            lstart_ms: facts.lstart_ms,
            sid: facts.sid,
            pgid: facts.pgid,
            leader_alive,
            argv: facts.display.argv.clone(),
            cwd: facts.display.cwd.clone(),
            elapsed_secs: facts.display.elapsed_secs,
            cpu_percent: facts.display.cpu_percent,
            suggested_instance: related.map(|o| o.instance.clone()),
            unproven_reason: match related {
                Some(_) => UnprovenReason::ObservedButNotAuthoritative,
                None => UnprovenReason::NoObservation,
            },
        });
    }

    let scope = oracle.reparented_scope();
    OrphanReport {
        support,
        candidates,
        scope,
    }
}

/// Render for `doctor`. Deliberately offers no action: there is no flag to
/// pass, no id list to confirm, and nothing here that a script could read as
/// permission. The heading is `UNPROVEN:` with no separating space so a reader
/// scanning for a "PROVEN" section finds none.
pub fn render_human(report: &OrphanReport) -> String {
    let mut out = String::new();
    // The heading claims only what the filter establishes. It is NOT
    // "agent-attributable": the scope is reparented + same-uid + holding a
    // session it did not create, and on a real machine most of what that
    // catches belongs to the operator or to third-party apps.
    out.push_str("Reparented processes in scope (attribution UNPROVEN)\n");
    out.push_str(
        "  Every entry is UNPROVEN: agend cannot prove it owns any of these. argv, cwd, \
         elapsed and CPU are display only and never evidence of ownership.\n",
    );
    out.push_str(
        "  Owner suggestions are kept for a bounded window (7 days). A process older than that \
         stays listed with suggested=unknown — an expired suggestion says nothing about the \
         process.\n",
    );

    if let ProvenanceSupport::Unsupported { platform, reason } = &report.support {
        // Still framed, so a Windows operator reads "this view does not apply"
        // rather than an unlabelled blank — and never "nothing was found".
        out.push_str(&format!("  unsupported on {platform}: {reason}\n"));
        out.push_str("  UNPROVEN: not available on this platform (not a global clean result)\n");
        return out;
    }

    if report.candidates.is_empty() {
        // The most dangerous output this command can produce: read carelessly
        // it says "your machine is clean". It says nothing of the sort.
        out.push_str(
            "  UNPROVEN: none found in this scoped snapshot (not a global clean result)\n",
        );
        out.push_str(&render_scope(&report.scope));
        return out;
    }

    out.push_str(&format!(
        "  UNPROVEN: {} candidate(s)\n",
        report.candidates.len()
    ));
    for c in &report.candidates {
        out.push_str(&format!(
            "    pid={} start_token={} lstart_ms={} sid={} pgid={} leader_alive={} suggested={}\n",
            c.pid,
            opt(c.start_token),
            opt(c.lstart_ms),
            opt(c.sid),
            opt(c.pgid),
            match c.leader_alive {
                Some(true) => "yes".to_string(),
                Some(false) => "no".to_string(),
                None => "unknown".to_string(),
            },
            c.suggested_instance.as_deref().unwrap_or("unknown"),
        ));
        out.push_str(&format!(
            "      why={} elapsed_secs={} cpu_percent={} argv={} cwd={}\n",
            match c.unproven_reason {
                UnprovenReason::NoObservation => "no observation relates to it",
                UnprovenReason::ObservedButNotAuthoritative =>
                    "an observation relates to it, which is not proof of ownership",
            },
            opt(c.elapsed_secs),
            opt(c.cpu_percent),
            c.argv.as_deref().unwrap_or("unknown"),
            c.cwd.as_deref().unwrap_or("unknown"),
        ));
    }
    out.push_str("  Disposition is an operator decision; agend takes none.\n");
    out.push_str(&render_scope(&report.scope));
    out
}

/// Scope accounting, printed with every result. Without it a short list cannot
/// be told apart from a quiet machine, and an empty one reads as all-clear.
fn render_scope(scope: &ScopeCounts) -> String {
    format!(
        "  Scope: scanned {} process(es); excluded {} not reparented, {} owned by another user, \
{} leading their own session (outside this view), {} in session 1 (outside this view), \
{} whose session could not be read.\n  Blind spot: a leaked process that called setsid() itself \
leads its own session and is therefore outside this view too. An exclusion here is a scope \
boundary, not a finding that the process is fine.\n",
        scope.scanned,
        scope.excluded_not_reparented,
        scope.excluded_foreign_uid,
        scope.excluded_session_leader,
        scope.excluded_init_session,
        scope.excluded_session_unknown,
    )
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "unknown".into())
}
