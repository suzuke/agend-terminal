//! Everything a restart settles BEFORE it kills the old instance: the params
//! the replacement is spawned with, and the gate that decides when the kill may
//! proceed.
//!
//! Split out of `mod.rs` (#3414/#3415 branch) to hold that file under the
//! 750-LOC handler bound `tests/file_size_invariant.rs` enforces. Behaviour is
//! unchanged — the items are the same, only their home and visibility moved.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

type RestartKey = (std::path::PathBuf, String);

static RESTART_ADMISSIONS: OnceLock<Mutex<HashMap<RestartKey, String>>> = OnceLock::new();

pub(super) struct RestartAdmission {
    key: RestartKey,
    restart_id: String,
}

impl Drop for RestartAdmission {
    fn drop(&mut self) {
        if let Ok(mut admissions) = admissions().lock() {
            if admissions.get(&self.key) == Some(&self.restart_id) {
                admissions.remove(&self.key);
            }
        }
    }
}

fn admissions() -> &'static Mutex<HashMap<RestartKey, String>> {
    RESTART_ADMISSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Admit one restart per configured target. The guard is held through draft
/// grace, DELETE, SPAWN, and handoff settlement; dropping it is the bounded
/// terminal cleanup, so retries do not accumulate replay state.
pub(super) fn try_admit_restart(
    home: &Path,
    name: &str,
    restart_id: &str,
) -> Result<RestartAdmission, String> {
    let key = (home.to_path_buf(), name.to_string());
    let mut current = admissions()
        .lock()
        .map_err(|_| "restart admission lock poisoned".to_string())?;
    if let Some(existing) = current.get(&key) {
        return Err(format!(
            "restart already in progress for '{name}' (restart_id={existing}, request_id={restart_id})"
        ));
    }
    current.insert(key.clone(), restart_id.to_string());
    Ok(RestartAdmission {
        key,
        restart_id: restart_id.to_string(),
    })
}

/// #1625: assemble the SPAWN params for a restart. Tags `layout: same-tab` so
/// the respawned pane returns to the tab the killed pane occupied (recorded
/// on its DELETE) instead of opening a fresh tab. `mode` only toggles backend
/// resume args — placement is identical for resume and fresh restarts — so
/// the hint is applied unconditionally.
// The existing restart parameter helper carries the complete spawn shape;
// correlation adds two lifecycle fields without changing its call contract.
#[allow(clippy::too_many_arguments)]
pub(super) fn restart_spawn_params(
    name: &str,
    backend_command: &str,
    args: &[String],
    working_directory: Option<&Path>,
    env: &std::collections::HashMap<String, String>,
    mode: &str,
    restart_id: &str,
    old_instance_ref: Option<crate::types::InstanceRef>,
) -> Value {
    let mut spawn_params = json!({
        "name": name,
        "backend": backend_command,
        "args": args.join(" "),
        "working_directory": working_directory.map(|p| p.display().to_string()),
        "env": serde_json::to_value(env).unwrap_or(serde_json::Value::Null),
        "layout": "same-tab",
        "restart_id": restart_id,
        "old_instance_ref": old_instance_ref,
    });
    if mode == "resume" {
        spawn_params["mode"] = json!("resume");
    } else {
        // fresh restart only: arm the daemon's first-turn self-kick so the
        // respawned (context-lost) instance runs its recovery sequence instead of
        // sitting idle until an operator happens to type (the overnight
        // restart-strands-the-fleet failure). INDEPENDENT flag — the SPAWN handler
        // must NOT derive self-kick from SpawnMode::Fresh, which initial fleet
        // spawns also map to; only THIS restart-fresh path sets it.
        spawn_params["self_kick_on_ready"] = json!(true);
    }
    spawn_params
}

/// #3538: resume-availability gate outcome for a `mode=resume` restart.
/// `Refused` carries the already-rendered fail-closed response; `Proceed`
/// carries whether an exact Codex thread was confirmed (for the
/// `resumed_thread` success signal — boolean only, never the id).
pub(super) enum ResumeGate {
    Proceed { codex_thread: bool },
    Refused { response: Value },
}

/// #3538: refuse a `mode=resume` restart that cannot resume, BEFORE any
/// destructive or mutating step (caller must invoke this right after
/// `resolve_instance`, before escalation-clear/draft-wait/DELETE). A resume
/// restart on a managed Codex instance whose locator has no thread_id would
/// silently start a FRESH session while reporting `spawned:true` — fail
/// closed with the live instance untouched. Instance-scoped only: no global
/// `resume --last` fallback (#3396/#3398).
pub(super) fn resume_availability_gate(
    home: &Path,
    name: &str,
    reason: &str,
    mode: &str,
    backend: &crate::backend::Backend,
) -> ResumeGate {
    if mode != "resume" || *backend != crate::backend::Backend::Codex {
        return ResumeGate::Proceed {
            codex_thread: false,
        };
    }
    if crate::transport::codex_resume_available(home, name) {
        return ResumeGate::Proceed { codex_thread: true };
    }
    tracing::warn!(
        agent = %name,
        "refusing resume restart: managed Codex locator has no thread_id — \
         resume is unavailable (a spawn now would silently start a fresh session)"
    );
    crate::event_log::log(
        home,
        "restart_instance",
        name,
        "resume_unavailable backend=codex thread_id=missing",
    );
    ResumeGate::Refused {
        response: json!({
            "name": name,
            "reason": reason,
            "mode": mode,
            "spawned": false,
            "code": "resume_unavailable",
            "error": format!(
                "refusing resume restart for '{name}': no exact Codex thread is \
                 available for this instance (managed locator has no thread_id), \
                 so resume would silently start a fresh session. The running \
                 instance was left untouched — restart with mode=fresh for an \
                 explicit fresh session."
            ),
        }),
    }
}

/// Grace ceiling for [`await_unsent_draft_or_grace`]: even while the operator
/// keeps typing, force the restart after this long so a context-full / stuck
/// agent can't be deferred indefinitely. The primary release is the operator
/// submitting (draft clears well before this); the ceiling only bounds the
/// pathological continuous-typing case. Tunable.
pub(super) const RESTART_DRAFT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// Re-check cadence while deferring — silent (no per-poll event / nudge).
const RESTART_DRAFT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DraftGate {
    Proceed,
    Defer,
}

/// Pure restart-gate decision (unit-tested exhaustively): proceed with the kill
/// iff `force`, there is no live operator draft, or the grace ceiling has
/// elapsed; otherwise keep deferring. Kept pure (no clock / no IO) so the whole
/// decision matrix is deterministic without real sleeps.
pub(super) fn restart_draft_gate(
    force: bool,
    has_live_draft: bool,
    elapsed: std::time::Duration,
    grace: std::time::Duration,
) -> DraftGate {
    if force || !has_live_draft || elapsed >= grace {
        DraftGate::Proceed
    } else {
        DraftGate::Defer
    }
}

/// Block the restart while the operator has unsent keystrokes in `name`'s input
/// line, releasing the instant the draft is submitted/cleared or after
/// [`RESTART_DRAFT_GRACE`]. Emits exactly two log lines (defer-start, proceed) —
/// no per-poll noise. Thread-safety rationale is at the call site.
pub(super) fn await_unsent_draft_or_grace(home: &Path, name: &str, force: bool) {
    if restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        std::time::Duration::ZERO,
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Proceed
    {
        return; // fast path: force, or no live draft — no wait, no log.
    }
    tracing::info!(%name, "restart deferred: operator has an unsent draft in the input line");
    let start = std::time::Instant::now();
    while restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        start.elapsed(),
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Defer
    {
        std::thread::sleep(RESTART_DRAFT_POLL);
    }
    tracing::info!(
        %name,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "restart proceeding: draft submitted/cleared or grace ceiling reached"
    );
}

/// Restart/TUI 交接確認 budget（秒）。App 側 2s 輪詢 + pane 重建 + 連接，
/// 10s 寬裕；MCP SLOW timeout 60s 內，不會拖死調用方。
pub(super) const TUI_HANDOFF_BUDGET_SECS: u64 = 10;
pub(super) const TUI_HANDOFF_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(TUI_HANDOFF_BUDGET_SECS);

/// 等新 generation 的 TUI listener 被 client 連上（bounded poll）。
/// 以 spawn 時刻為 generation 邊界：只認此後連上**當前 port 文件所載 port**
/// 的連接，舊 generation 的殘留連接不算。port 文件缺失/不可讀 → false
///（交接未確認，不說謊）。`budget` 可注入，測試用短 budget。
pub(super) fn await_tui_handoff(home: &Path, name: &str, budget: std::time::Duration) -> bool {
    await_tui_handoff_at(home, name, budget, std::time::Instant::now())
}

/// 結算交接回報：spawn 成功只證明 process 層起來了（20:24 事件：daemon 側
/// spawned=true、新 TUI socket 就緒，但 app 側全程無接管）。等新 generation
/// 的 TUI listener 被 client 連上（bounded poll）才算交接；超時則如實報
/// tui_handoff:false（process 事實 spawned 保留，不說謊）。
pub(super) fn settle_tui_handoff(home: &Path, name: &str, spawned: bool) -> (bool, Option<String>) {
    let tui_handoff = spawned && await_tui_handoff(home, name, TUI_HANDOFF_BUDGET);
    let warning = if spawned && !tui_handoff {
        Some(format!(
            "instance '{name}' spawned but no TUI client connected to its new listener within {TUI_HANDOFF_BUDGET_SECS}s — the pane may be stale; check the TUI roster sync"
        ))
    } else {
        None
    };
    (tui_handoff, warning)
}

/// #t-777-3: daemon-autonomic self-heal entry — the respawn-stuck watchdog's
/// narrow path to a **Fresh** restart. Wraps `handle_restart_instance(mode=fresh)`,
/// which round-trips the PROVEN direct `DELETE`(no_wait)+`SPAWN` api::calls →
/// `ApiEvent::InstanceCreated` → app pane Fresh respawn (the same path the
/// operator's manual `restart_instance fresh` takes, working in the live
/// app-mode daemon where the crash_tx→respawn machinery is inert).
///
/// **Gate-exempt BY CONSTRUCTION** (no new operator-gate surface): the inner
/// `DELETE`/`SPAWN` are DIRECT api methods — operator-transport, which
/// `operator_gate::check_operation_allowed` returns `Ok` for before `classify`
/// is consulted. Reached ONLY from the per-tick hang-detection watchdog (never
/// agent-invocable), so the narrowness is enforced by the trigger, exactly like
/// crash-respawn / hang-recovery (`operator_gate` module scope note). Returns
/// whether the SPAWN succeeded so the caller can escalate a failed recovery.
pub(crate) fn restart_instance_autonomic(
    home: &Path,
    name: &str,
    reason: &str,
    old_instance_ref: Option<crate::types::InstanceRef>,
) -> bool {
    let restart_id = crate::types::InstanceId::new().full();
    let result = super::handle_restart_instance(
        home,
        &json!({
            "name": name,
            "mode": "fresh",
            "reason": reason,
            "restart_id": restart_id,
            "old_instance_ref": old_instance_ref,
        }),
    );
    result
        .get("spawned")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn await_tui_handoff_at(
    home: &Path,
    name: &str,
    budget: std::time::Duration,
    since: std::time::Instant,
) -> bool {
    let deadline = since + budget;
    loop {
        let run_dir = crate::daemon::run_dir(home);
        if let Some(port) = crate::ipc::read_port(&run_dir, name) {
            if crate::daemon::tui_bridge::tui_client_connected_since(port, since) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
