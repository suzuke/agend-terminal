//! #3273 V1 — per-tick tool-call observation, and the platform process oracle
//! the `doctor` orphan report runs on.
//!
//! Deliberately owned by the DAEMON tick, not by `instance_monitor::collect`:
//! that sweep is subscriber-gated by `LAST_METRICS_READ`, so a headless daemon
//! — or a TUI parked on any pane but Monitor — would skip it entirely and
//! observation would silently stop. An invariant test walks this file's AST and
//! rejects any reference to that gate.
//!
//! What this ships today, stated plainly: the tick maintains the
//! **pre-tool baseline** for every live backend. It records no observations,
//! because there is no trustworthy shell-tool generation signal in the daemon
//! yet — `AgentState`'s ToolUse is pattern-derived from the statusline and
//! carries no per-call generation, and inventing evidence is exactly what the
//! RCAs rejected. So `active_shell_tool` is `None` and every tick reports
//! `NoActiveShellTool`. The baseline is the half that cannot be reconstructed
//! after the fact, which is why it is worth collecting now.

use std::sync::atomic::{AtomicU64, Ordering};

use super::{PerTickHandler, TickContext};
use crate::admin::orphan_provenance::{
    sample_from_owner_source, scope_reparented, BackendOwner, BackendOwnerSource, DisplayColumns,
    ProcFacts, ProcessOracle, ProvenanceSupport, ScopeCounts, ScopeInput,
};

/// One `ps` invocation per sample is affordable; one per tick is not. The
/// baseline only has to be older than the tool call it protects, so a coarse
/// cadence is fine.
const SAMPLE_EVERY_N_TICKS: u64 = 10;

pub(crate) struct ToolCallProvenanceHandler {
    ticks: AtomicU64,
}

impl ToolCallProvenanceHandler {
    pub(crate) fn new() -> Self {
        Self {
            ticks: AtomicU64::new(0),
        }
    }
}

impl PerTickHandler for ToolCallProvenanceHandler {
    fn name(&self) -> &'static str {
        "tool_call_provenance"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        let n = self.ticks.fetch_add(1, Ordering::Relaxed);
        if !n.is_multiple_of(SAMPLE_EVERY_N_TICKS) {
            return;
        }

        let owners = RegistryOwners::from_registry(ctx.registry);
        if owners.0.is_empty() {
            return;
        }
        let oracle = PlatformOracle::snapshot();
        let now_ms = now_epoch_ms();
        let outcome = sample_from_owner_source(ctx.home, &oracle, &owners, now_ms);

        for (instance, miss) in &outcome.misses {
            tracing::trace!(
                agent = %instance,
                miss = ?miss,
                "tool-call provenance: nothing observable this window"
            );
        }
        if let crate::admin::orphan_provenance::PersistOutcome::Failed(detail) = &outcome.persisted
        {
            tracing::warn!(
                error = %detail,
                "tool-call provenance ledger write failed; nothing was recorded"
            );
        }
    }
}

/// Live backends, read once from the registry. No metrics handle is involved.
struct RegistryOwners(Vec<BackendOwner>);

impl RegistryOwners {
    fn from_registry(registry: &crate::agent::AgentRegistry) -> Self {
        let reg = crate::agent::lock_registry(registry);
        let owners = reg
            .iter()
            .filter_map(|(id, handle)| {
                let pid = handle.child.lock().process_id()?;
                Some(BackendOwner {
                    instance: id.to_string(),
                    backend_pid: pid,
                    // No per-call shell-tool generation exists in the daemon
                    // yet; see the module docs. Baseline maintenance only.
                    active_shell_tool: None,
                })
            })
            .collect();
        Self(owners)
    }
}

impl BackendOwnerSource for RegistryOwners {
    fn live_backends(&self) -> Vec<BackendOwner> {
        self.0.clone()
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Platform oracle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct PsRow {
    ppid: u32,
    pgid: u32,
    uid: u32,
    elapsed_secs: u64,
    cpu_percent: f32,
    argv: String,
}

/// A single process-table snapshot. Built once per use so a report or a sample
/// costs one `ps`, not one per pid.
pub(crate) struct PlatformOracle {
    rows: std::collections::HashMap<u32, PsRow>,
    now_ms: i64,
    /// Filled by `reparented`, read by `reparented_scope`.
    scope: std::sync::OnceLock<ScopeCounts>,
}

impl PlatformOracle {
    pub(crate) fn snapshot() -> Self {
        Self {
            rows: ps_snapshot(),
            now_ms: now_epoch_ms(),
            scope: std::sync::OnceLock::new(),
        }
    }
}

impl ProcessOracle for PlatformOracle {
    fn support(&self) -> ProvenanceSupport {
        #[cfg(unix)]
        {
            ProvenanceSupport::Supported
        }
        #[cfg(not(unix))]
        {
            ProvenanceSupport::Unsupported {
                platform: "windows",
                reason: "no POSIX reparenting or session semantics; Job Objects are future work",
            }
        }
    }

    fn facts(&self, pid: u32) -> Option<ProcFacts> {
        let row = self.rows.get(&pid)?;
        let elapsed = row.elapsed_secs;
        Some(ProcFacts {
            pid,
            ppid: Some(row.ppid),
            sid: session_of(pid),
            pgid: Some(row.pgid),
            uid: Some(row.uid),
            start_token: crate::process::process_start_token(pid),
            lstart_ms: Some(self.now_ms - (elapsed as i64) * 1_000),
            display: DisplayColumns {
                argv: Some(row.argv.clone()),
                // Resolving a cwd costs an `lsof` per pid; the doctor path fills
                // this in for the candidates it actually prints.
                cwd: None,
                elapsed_secs: Some(elapsed),
                cpu_percent: Some(row.cpu_percent),
            },
        })
    }

    fn children_of(&self, pid: u32) -> Vec<u32> {
        let mut kids: Vec<u32> = self
            .rows
            .iter()
            .filter(|(_, row)| row.ppid == pid)
            .map(|(child, _)| *child)
            .collect();
        kids.sort_unstable();
        kids
    }

    /// `PPID == 1` on its own makes an unreadable list on macOS: launchd is the
    /// parent of hundreds of per-user agents and XPC services. Measured on one
    /// live machine: 861 processes at `PPID == 1`, 559 of them same-uid.
    ///
    /// This view narrows to processes holding a session id they did not create
    /// — the shape #3273 is about (the RCA measured a background job keeping
    /// its dead tool-call shell's sid). That cut gives 47 on the same machine,
    /// and it contains every process the RCA census identified by hand: the
    /// 44-day `ngrok`/`python3` pair, both multi-week `tail`s, and the
    /// readerless-FIFO `bash`.
    ///
    /// The narrowing is a SCOPING CHOICE, not a finding about ownership.
    /// Session 1 and self-led sessions are topology categories this view does
    /// not cover, i.e. blind spots — a leaked process that calls `setsid()`
    /// itself becomes its own session leader and drops out. Every exclusion is
    /// counted via [`ScopeCounts`] and printed with the report, so a reader
    /// knows what was not looked at.
    fn reparented(&self) -> Vec<u32> {
        if !matches!(self.support(), ProvenanceSupport::Supported) {
            return Vec::new();
        }
        let inputs: Vec<ScopeInput> = self
            .rows
            .iter()
            .map(|(pid, row)| ScopeInput {
                pid: *pid,
                ppid: row.ppid,
                uid: row.uid,
                sid: session_of(*pid),
            })
            .collect();
        let (included, counts) = scope_reparented(&inputs, current_uid());
        let _ = self.scope.set(counts);
        included
    }

    fn reparented_scope(&self) -> ScopeCounts {
        self.scope.get().cloned().unwrap_or_default()
    }

    fn is_alive(&self, pid: u32) -> bool {
        crate::process::is_pid_alive(pid)
    }
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    // SAFETY: getuid is always safe; it reads a process attribute.
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

#[cfg(unix)]
fn session_of(pid: u32) -> Option<u32> {
    // SAFETY: getsid on a pid we may not own returns -1/ESRCH rather than
    // misbehaving; the cast is checked below.
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    (sid >= 0).then_some(sid as u32)
}

#[cfg(not(unix))]
fn session_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn ps_snapshot() -> std::collections::HashMap<u32, PsRow> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,uid=,etime=,%cpu=,args="])
        .output();
    let Ok(out) = out else {
        return std::collections::HashMap::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = std::collections::HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid), Some(uid), Some(etime), Some(cpu)) = (
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
            it.next(),
        ) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let argv = it.collect::<Vec<_>>().join(" ");
        rows.insert(
            pid,
            PsRow {
                ppid: ppid.parse().unwrap_or_default(),
                pgid: pgid.parse().unwrap_or_default(),
                uid: uid.parse().unwrap_or_default(),
                elapsed_secs: parse_etime(etime),
                cpu_percent: cpu.parse().unwrap_or_default(),
                argv,
            },
        );
    }
    rows
}

#[cfg(not(unix))]
fn ps_snapshot() -> std::collections::HashMap<u32, PsRow> {
    std::collections::HashMap::new()
}

/// `ps` elapsed time: `[[dd-]hh:]mm:ss`. Unix-only, matching its sole call site
/// in the `#[cfg(unix)]` `ps_snapshot`: on Windows there is no `ps` snapshot to
/// parse, and strict clippy correctly rejects it as dead code there.
#[cfg(unix)]
fn parse_etime(raw: &str) -> u64 {
    let (days, rest) = match raw.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().unwrap_or_default(), rest),
        None => (0, raw),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse::<u64>().unwrap_or_default())
        .collect();
    let hms = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => 0,
    };
    days * 86_400 + hms
}

/// Fill in `cwd` for the pids a report is about to print. One `lsof` for the
/// whole set rather than one per pid.
#[cfg(unix)]
pub(crate) fn resolve_cwds(pids: &[u32]) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(res) = std::process::Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
        .output()
    else {
        return out;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    let mut current: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('n') {
            if let Some(pid) = current {
                out.entry(pid).or_insert_with(|| rest.to_string());
            }
        }
    }
    out
}

#[cfg(not(unix))]
pub(crate) fn resolve_cwds(_pids: &[u32]) -> std::collections::HashMap<u32, String> {
    std::collections::HashMap::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn etime_parses_every_ps_shape() {
        assert_eq!(parse_etime("05"), 5);
        assert_eq!(parse_etime("01:30"), 90);
        assert_eq!(parse_etime("02:03:04"), 2 * 3600 + 3 * 60 + 4);
        assert_eq!(parse_etime("44-03:00:00"), 44 * 86_400 + 3 * 3600);
        assert_eq!(parse_etime("garbage"), 0);
    }
}
