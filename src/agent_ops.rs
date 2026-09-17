//! Agent-level helpers shared between `ops.rs` and `mcp/handlers.rs`.
//!
//! These primitives were duplicated (with drift) between the two layers —
//! `cleanup_working_dir` in particular had a 14-entry copy in
//! `mcp/handlers.rs` that missed 5 Kiro paths present in the 19-entry
//! canonical version in `ops.rs` (introduced by 99e8590, 2026-04-14).
//!
//! Step 1 of Task #9 Option C (Commit 1): introduce canonical module +
//! characterization tests. Callers still use their inline copies; Step 2
//! (Commit 2) will delete the duplicates and switch imports, at which
//! point the drift is automatically fixed for MCP callers.

use crate::agent::{self, AgentRegistry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) mod cleanup_admission;
pub(crate) mod messaging;
pub(crate) mod spawn;
pub(crate) mod spawn_transaction;

#[cfg(all(test, unix))]
pub(crate) use spawn_transaction::lanes_are_the_same;
pub(crate) use spawn_transaction::{preset_submit_key, spawn_one_recording_config};

const UNVERIFIED_DELIVERY_MODE: &str = "transport_queued_unverified";

fn api_bridge_delivery_mode(resp: &Value) -> &str {
    resp["delivery_mode"]
        .as_str()
        .filter(|mode| !mode.is_empty())
        .unwrap_or(UNVERIFIED_DELIVERY_MODE)
}

// ---------------------------------------------------------------------------
// Messaging
// ---------------------------------------------------------------------------

pub(crate) fn send_via_api_bridge(home: &Path, request: &messaging::SendRequest) -> Value {
    let mut params = json!({
        "from": request.from,
        "target": request.target,
        "text": request.text,
    });
    let Some(obj) = params.as_object_mut() else {
        unreachable!()
    };
    if let Some(ref k) = request.kind {
        obj.insert("kind".into(), json!(k));
    }
    if let Some(ref v) = request.thread_id {
        obj.insert("thread_id".into(), json!(v));
    }
    if let Some(ref v) = request.parent_id {
        obj.insert("parent_id".into(), json!(v));
    }
    if let Some(ref v) = request.correlation_id {
        obj.insert("correlation_id".into(), json!(v));
    }
    if let Some(ref v) = request.reviewed_head {
        obj.insert("reviewed_head".into(), json!(v));
    }
    if let Some(ref v) = request.report_purpose {
        obj.insert("report_purpose".into(), json!(v));
    }
    if let Some(ref v) = request.code_review {
        obj.insert("code_review".into(), v.clone());
    }
    if let Some(v) = request.eta_minutes {
        obj.insert("eta_minutes".into(), json!(v));
    }
    if let Some(ref v) = request.reporting_cadence {
        obj.insert("reporting_cadence".into(), json!(v));
    }
    if let Some(v) = request.worktree_binding_required {
        obj.insert("worktree_binding_required".into(), json!(v));
    }
    if let Some(v) = request.expect_reply_within_secs {
        obj.insert("expect_reply_within_secs".into(), json!(v));
    }
    if let Some(v) = request.terminal {
        obj.insert("terminal".into(), json!(v));
    }
    if let Some(v) = request.no_report_expected {
        obj.insert("no_report_expected".into(), json!(v));
    }
    if let Some(ref v) = request.delivery_nonce {
        obj.insert("delivery_nonce".into(), json!(v));
    }
    if let Some(ref v) = request.task_id {
        obj.insert("task_id".into(), json!(v));
    }
    if let Some(ref v) = request.force_meta {
        obj.insert("force_meta".into(), v.clone());
    }
    if let Some(ref v) = request.provenance {
        obj.insert("provenance".into(), v.clone());
    }
    if let Some(ref v) = request.branch {
        obj.insert("branch".into(), json!(v));
    }
    if let Some(ref v) = request.priority {
        obj.insert("priority".into(), json!(v));
    }
    if let Some(ref v) = request.broadcast_context {
        obj.insert(
            "broadcast_context".into(),
            serde_json::to_value(v).unwrap_or_default(),
        );
    }
    match crate::api::call(
        home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "method": crate::api::method::SEND,
            "params": params,
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let dm = api_bridge_delivery_mode(&resp);
            let mut result = json!({"target": request.target, "delivery_mode": dm});
            if let Some(tid) = resp["task_id"].as_str() {
                result["auto_created_task_id"] = json!(tid);
            }
            result
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => json!({"error": format!("daemon API unavailable: {e}")}),
    }
}

// ---------------------------------------------------------------------------
// Blocked-reason (health) — #2454 in-process MCP→API service
// ---------------------------------------------------------------------------

/// Successful [`set_blocked_reason`] outcome (the agent's display state when the
/// reason was recorded).
#[derive(Debug)]
pub struct BlockedReasonSet {
    pub current_state: String,
}

/// Successful [`clear_blocked_reason`] outcome; `was` is the prior reason (or
/// `None` if the agent was not blocked).
#[derive(Debug)]
pub struct BlockedReasonCleared {
    pub was: Option<crate::health::BlockedReason>,
}

/// [`clear_blocked_reason`] failure. Distinct variants so the transport adapters
/// map each exhaustively (no wildcard). `set_blocked_reason` cannot mismatch, so
/// it returns `Option` rather than sharing this type.
#[derive(Debug)]
pub enum ClearBlockedError {
    /// No registry entry resolves for the name.
    NotFound,
    /// The filter kind did not match the current reason (left unchanged).
    FilterMismatch {
        current: Option<crate::health::BlockedReason>,
    },
}

/// #2454: set an agent's blocked reason IN-PROCESS against the live registry —
/// the transport-neutral owner shared by the API handler and the MCP `health
/// report` handler (previously reached over the MCP→API self-IPC loopback). Locks
/// registry (tier-0) then core (tier-1), callers hold neither. `None` = the
/// instance is not registered.
pub fn set_blocked_reason(
    registry: &AgentRegistry,
    home: &Path,
    name: &str,
    reason: crate::health::BlockedReason,
    note: Option<&str>,
) -> Option<BlockedReasonSet> {
    let reg = agent::lock_registry(registry);
    let handle = crate::fleet::resolve_uuid(home, name).and_then(|id| reg.get(&id))?;
    let mut core = handle.core.lock();
    let current_state = core.state.get_state().display_name().to_string();
    // set_blocked_reason resets the note, so apply the note AFTER (empty → none).
    core.health.set_blocked_reason(reason);
    core.health
        .set_blocked_note(note.filter(|n| !n.is_empty()).map(str::to_string));
    Some(BlockedReasonSet { current_state })
}

/// #2454: clear an agent's blocked reason IN-PROCESS (owner shared by the API and
/// MCP `health clear` handlers). `filter_kind` is a reason-KIND token compared to
/// [`crate::health::BlockedReason::kind_str`], NOT a full `BlockedReason`: an
/// unknown kind stays a legal never-match filter (a parsed reason would silently
/// make an unknown filter clear unconditionally). `None` = clear unconditionally.
/// Lock order as [`set_blocked_reason`].
pub fn clear_blocked_reason(
    registry: &AgentRegistry,
    home: &Path,
    name: &str,
    filter_kind: Option<&str>,
) -> Result<BlockedReasonCleared, ClearBlockedError> {
    let reg = agent::lock_registry(registry);
    let handle = crate::fleet::resolve_uuid(home, name)
        .and_then(|id| reg.get(&id))
        .ok_or(ClearBlockedError::NotFound)?;
    let mut core = handle.core.lock();
    let was = core.health.current_reason.clone();
    if let Some(filter) = filter_kind {
        let matches = was.as_ref().is_some_and(|r| r.kind_str() == filter);
        if !matches {
            return Err(ClearBlockedError::FilterMismatch { current: was });
        }
    }
    core.health.clear_blocked_reason();
    Ok(BlockedReasonCleared { was })
}

// ---------------------------------------------------------------------------
// Pane scrollback (pane_snapshot) — #2454 in-process MCP→API service
// ---------------------------------------------------------------------------

/// #2454: read an agent's PTY scrollback IN-PROCESS against the live registry —
/// the transport-neutral owner shared by the API `handle_pane_snapshot` adapter,
/// the MCP `pane_snapshot` tool, and the interrupt-snapshot (each previously
/// reached over the self-IPC loopback). Locks registry (tier-0) then core
/// (tier-1); callers hold neither. `lines` is already bounded by the transport
/// (MCP: explicit >10k reject; API: `min(10_000)`). `None` = not registered.
pub fn pane_scrollback(
    registry: &AgentRegistry,
    home: &Path,
    name: &str,
    lines: usize,
) -> Option<String> {
    let reg = agent::lock_registry(registry);
    let handle = crate::fleet::resolve_uuid(home, name).and_then(|id| reg.get(&id))?;
    let core = handle.core.lock();
    Some(core.vterm.read_scrollback(lines))
}

// ---------------------------------------------------------------------------
// Pane relocation (move_pane) — #2454 in-process service
// ---------------------------------------------------------------------------

/// Direction used by the transport-neutral pane relocation service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneMoveSplit {
    Horizontal,
    Vertical,
}

impl PaneMoveSplit {
    pub fn parse(value: &str) -> Self {
        match value {
            "vertical" | "v" => Self::Vertical,
            _ => Self::Horizontal,
        }
    }
}

/// Validated move request returned to API and MCP adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneMoveEvent {
    pub agent: String,
    pub target_tab: String,
    pub split_dir: PaneMoveSplit,
}

/// Validate a pane relocation request and append its audit event.
///
/// Layout mutation remains owned by the notifier/TUI event loop; this service
/// owns the shared validation, split parsing, and event-log side effect.
pub fn move_pane(
    home: &Path,
    agent_name: Option<&str>,
    target_tab: Option<&str>,
    split_dir: Option<&str>,
) -> Result<PaneMoveEvent, String> {
    let agent_name = agent_name.ok_or_else(|| "missing agent".to_string())?;
    let agent_name = agent::validate_name(agent_name)?.to_string();
    let target_tab = match target_tab {
        Some(tab) if !tab.is_empty() => tab.to_string(),
        _ => return Err("missing target_tab".to_string()),
    };
    let split_dir = PaneMoveSplit::parse(split_dir.unwrap_or("horizontal"));

    crate::event_log::log(
        home,
        "move_pane",
        &agent_name,
        &format!("target_tab={target_tab} split={split_dir:?}"),
    );
    Ok(PaneMoveEvent {
        agent: agent_name,
        target_tab,
        split_dir,
    })
}

// ---------------------------------------------------------------------------
// Instance deletion — shared API/MCP runtime service (#2454 Slice 10)
// ---------------------------------------------------------------------------

/// Runtime-owned state required by the managed DELETE operation.  The wire
/// adapters (API and MCP) build this value from their respective contexts;
/// the service itself does not know which transport invoked it.
pub struct DeleteContext<'a> {
    pub registry: &'a AgentRegistry,
    pub configs: &'a crate::api::ConfigRegistry,
    pub externals: &'a agent::ExternalRegistry,
    pub notifier: Option<&'a Arc<dyn crate::api::ApiNotifier>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Managed,
    External,
}

/// Perform the daemon-side portion of DELETE once, preserving the exact API
/// semantics for managed and external agents. Runtime callers use the live
/// registries directly; transport fallback belongs to the MCP routing layer.
///
/// #3505: merged into `delete_instance_with_exit_status` — the sole
/// production caller (`deployments::teardown_with_runtime`) needs the
/// exit-observation bit to name refused instances, and no other caller
/// remains. Callers that ignore the bit take `.0`.
pub(crate) fn delete_instance_with_exit_status(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
) -> (DeleteOutcome, bool) {
    delete_instance_with_exit_status_for_restart(home, name, context, skip_exit_wait, None)
}

/// Delete with optional internal restart correlation carried on the lifecycle
/// event. Public deletion callers leave this unset; restart callers provide the
/// daemon-generated id without exposing it in the MCP schema.
pub(crate) fn delete_instance_with_exit_status_for_restart(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
    restart_id: Option<&str>,
) -> (DeleteOutcome, bool) {
    // The public runtime entry owns the complete deletion fence even for an
    // external agent. External-first resolution must not bypass transport
    // invalidation: a queued job for the same name can otherwise outlive the
    // early return and recreate delivery state or reach an adapter. The fence
    // still fronts the whole window (it drops at the end of this function),
    // but the delivery-state removal itself must wait until teardown is
    // confirmed: a refused teardown (child-exit timeout) leaves the instance
    // alive and retained for retry, and that surviving recipient can still be
    // owed a parked operator notice — deleting the log unconditionally would
    // discard it silently.
    let _delete_fence = crate::daemon::lifecycle::DeleteFence::new(home, name, true);
    let (outcome, observed_exit) =
        delete_instance_impl(home, name, context, skip_exit_wait, restart_id);
    if observed_exit {
        if let Err(error) = crate::transport::remove_instance_delivery_state(home, name) {
            tracing::warn!(
                agent = %name,
                error = %error,
                "delete: transport delivery cleanup failed"
            );
        }
    }
    (outcome, observed_exit)
}

/// Delete through the shared transaction body when the caller already owns the
/// deleting mark and keyed transport cleanup guard (the full-delete path).
pub(crate) fn delete_instance_under_guard(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
) -> (DeleteOutcome, bool) {
    delete_instance_under_guard_for_restart(home, name, context, skip_exit_wait, None)
}

pub(crate) fn delete_instance_under_guard_for_restart(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
    restart_id: Option<&str>,
) -> (DeleteOutcome, bool) {
    // The full-delete caller already owns DeleteFence; do not nest a second
    // lifecycle or transport guard around this body.
    delete_instance_impl(home, name, context, skip_exit_wait, restart_id)
}

fn delete_instance_impl(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
    restart_id: Option<&str>,
) -> (DeleteOutcome, bool) {
    // Match the API adapter's external-first behavior.  External agents have
    // no managed registry/config entry and therefore need no notifier event.
    if agent::lock_external(context.externals)
        .remove(name)
        .is_some()
    {
        crate::event_log::log(home, "delete", name, "external agent deleted");
        return (DeleteOutcome::External, true);
    }

    let instance_ref = agent::instance_ref_for_name(context.registry, home, name);
    let observed_exit = crate::daemon::lifecycle::delete_transaction_under_guard(
        home,
        name,
        context.registry,
        Some(context.configs),
        skip_exit_wait,
    );
    crate::daemon::poll_reminder::remove_agent(name);
    if let Some(notifier) = context.notifier {
        tracing::info!(agent = name, "DELETE emitting InstanceDeleted");
        notifier.notify(crate::api::ApiEvent::InstanceDeleted {
            name: name.to_string(),
            instance_ref,
            restart_id: restart_id.map(str::to_string),
        });
    }
    (DeleteOutcome::Managed, observed_exit)
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Name-based metadata path (legacy).
pub fn metadata_path(home: &Path, name: &str) -> PathBuf {
    home.join("metadata").join(format!("{name}.json"))
}

/// Sprint 46 P2: resolve metadata path by InstanceId when available.
/// Migrates legacy name-based files to id-based on first access.
pub fn metadata_path_resolved(home: &Path, name: &str) -> PathBuf {
    // #perf-R4: per-tick hot path (supervisor reads metadata ~2×/agent/tick) →
    // load_arc (Arc refcount bump, not a deep clone of the whole fleet).
    let id = crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|c| {
            c.instances
                .get(name)
                .and_then(|i| i.id.as_deref())
                .and_then(crate::types::InstanceId::parse)
        });
    let Some(id) = id else {
        return metadata_path(home, name);
    };
    let id_path = home.join("metadata").join(format!("{}.json", id.full()));
    if id_path.exists() {
        return id_path;
    }
    let name_path = metadata_path(home, name);
    if name_path.exists() {
        if let Some(parent) = id_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&name_path, &id_path);
        }
        #[cfg(windows)]
        {
            let _ = std::fs::copy(&name_path, &id_path);
        }
        return id_path;
    }
    id_path
}

/// #1682: id-based metadata path (pure — no migration side effects, unlike
/// `metadata_path_resolved`). For cleanup paths where the `InstanceId` is known
/// directly, e.g. `full_delete_instance` after fleet.yaml has already been
/// removed (so a name→id lookup would fail).
pub fn metadata_path_for_id(home: &Path, id: &crate::types::InstanceId) -> PathBuf {
    home.join("metadata").join(format!("{}.json", id.full()))
}

/// #1682: resolve an instance's id-based metadata path from fleet.yaml WITHOUT
/// the symlink/dir migration side effects of `metadata_path_resolved`. `None`
/// when the name has no id mapping (returns to the caller, which falls back to
/// the name path). Mirrors the lookup in `metadata_path_resolved`.
fn id_metadata_path(home: &Path, name: &str) -> Option<PathBuf> {
    let id = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()?
        .instances
        .get(name)
        .and_then(|i| i.id.as_deref())
        .and_then(crate::types::InstanceId::parse)?;
    Some(metadata_path_for_id(home, &id))
}

/// #1682: remove an instance's metadata, covering BOTH the legacy name path and
/// the id-resolved path, so a delete / spawn-clear leaves no split copy behind.
/// Pure (no symlink creation). Replaces the hand-coded `remove_file` of just the
/// name file that, post-#1680, missed the `<uuid>.json` readers actually read.
pub fn remove_metadata(home: &Path, name: &str) {
    let _ = std::fs::remove_file(metadata_path(home, name));
    if let Some(id_path) = id_metadata_path(home, name) {
        let _ = std::fs::remove_file(id_path);
    }
}

/// #1682: does ANY metadata file exist for this instance — legacy name path OR
/// id-resolved path — WITHOUT the symlink/dir side effects of
/// `metadata_path_resolved`. For residual / cleanup-verification checks that
/// must not themselves create metadata.
pub fn metadata_exists(home: &Path, name: &str) -> bool {
    metadata_path(home, name).exists() || id_metadata_path(home, name).is_some_and(|p| p.exists())
}

/// Load metadata for an instance and merge it into the given JSON value.
pub fn merge_metadata(home: &Path, name: &str, info: &mut Value) {
    let meta_path = metadata_path_resolved(home, name);
    if let Ok(meta) = std::fs::read_to_string(&meta_path)
        .and_then(|c| serde_json::from_str::<Value>(&c).map_err(std::io::Error::other))
    {
        if let (Some(obj), Some(m)) = (info.as_object_mut(), meta.as_object()) {
            for (k, v) in m {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static METADATA_RMW_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_metadata_rmw() {
    METADATA_RMW_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn reset_metadata_rmw_count() {
    METADATA_RMW_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn take_metadata_rmw_count() -> usize {
    METADATA_RMW_COUNT.with(|count| count.replace(0))
}

/// Outcome of the non-blocking metadata batch used by periodic UI work.
/// Contention is expected; other errors are surfaced to the caller so drained
/// in-memory activity can be requeued instead of being silently discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TryMetadataBatchOutcome {
    Applied,
    Contended,
    Failed,
}

/// Persist a metadata batch without ever waiting for the instance lock.
///
/// The periodic TUI flush must not call `save_metadata_batch`: its blocking
/// flock would park the single input/render loop. The caller owns retry and
/// loss semantics, so this helper distinguishes expected contention from I/O
/// or serialization failure and never logs-and-forgets a drained batch.
pub(crate) fn try_save_metadata_batch(
    home: &Path,
    instance_name: &str,
    entries: &[(&str, Value)],
) -> TryMetadataBatchOutcome {
    let meta_dir = home.join("metadata");
    if let Err(error) = std::fs::create_dir_all(&meta_dir) {
        tracing::warn!(
            home = %home.display(),
            agent = %instance_name,
            error = %error,
            "nonblocking activity metadata setup failed"
        );
        return TryMetadataBatchOutcome::Failed;
    }
    let meta_path = metadata_path_resolved(home, instance_name);
    let lock_path = meta_path.with_extension("lock");
    let lock = match crate::store::try_acquire_file_lock(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => return TryMetadataBatchOutcome::Contended,
        Err(error) => {
            tracing::warn!(
                path = %lock_path.display(),
                error = %error,
                "nonblocking activity metadata lock failed"
            );
            return TryMetadataBatchOutcome::Failed;
        }
    };

    #[cfg(test)]
    note_metadata_rmw();
    let mut metadata = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        metadata = json!({});
    }
    let object = metadata
        .as_object_mut()
        .expect("metadata normalized to a JSON object");
    for (key, value) in entries {
        object.insert((*key).to_owned(), value.clone());
    }
    let body = match serde_json::to_string_pretty(&metadata) {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(
                path = %meta_path.display(),
                error = %error,
                "nonblocking activity metadata serialization failed"
            );
            drop(lock);
            return TryMetadataBatchOutcome::Failed;
        }
    };
    let outcome = match crate::store::atomic_write(&meta_path, body.as_bytes()) {
        Ok(()) => TryMetadataBatchOutcome::Applied,
        Err(error) => {
            tracing::warn!(
                path = %meta_path.display(),
                error = %error,
                "nonblocking activity metadata write failed"
            );
            TryMetadataBatchOutcome::Failed
        }
    };
    drop(lock);
    outcome
}

/// Persist a single metadata key/value for an instance.
///
/// #1886 C2: locked read-modify-write (flock spans load→modify→write) so two
/// concurrent `set_*` on the same instance can't each read the same object and
/// clobber the other's field. `with_json_state_or_create` also gives the same
/// atomic write (temp file + rename) the prior code had, so concurrent readers
/// (e.g. supervisor tick) still never see a half-written file.
pub fn save_metadata(home: &Path, instance_name: &str, key: &str, value: Value) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path_resolved(home, instance_name);
    #[cfg(test)]
    note_metadata_rmw();
    // #1647: log on failure — this metadata is read back by `merge_metadata`, and
    // the MCP set_* handlers return OK regardless, so a dropped write was a silent
    // operator-set-but-lost.
    persist_or_log!(
        crate::store::with_json_state_or_create::<Value, _, _, _>(
            &meta_path,
            || json!({}),
            |meta| {
                meta[key] = value;
            },
        ),
        "save_metadata"
    );
}

/// CR-2026-06-14 (concurrency): locked read-modify-write of a single metadata
/// key via a transform closure. The flock spans the whole load→modify→write, and
/// — unlike `save_metadata` (which overwrites a key with a precomputed value) —
/// the new value is DERIVED from the current on-disk value INSIDE the lock. Use
/// this when the write depends on the current value (e.g. filtering an array):
/// computing the remainder outside the lock and writing it back races with a
/// concurrent append, which the stale-remainder write then clobbers (the
/// `pending_pickup_ids` lost-update class). `current` is `Null` if the key is
/// absent.
pub fn update_metadata(
    home: &Path,
    instance_name: &str,
    key: &str,
    f: impl FnOnce(&Value) -> Value,
) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path_resolved(home, instance_name);
    #[cfg(test)]
    note_metadata_rmw();
    persist_or_log!(
        crate::store::with_json_state_or_create::<Value, _, _, _>(
            &meta_path,
            || json!({}),
            |meta| {
                let current = meta.get(key).cloned().unwrap_or(Value::Null);
                meta[key] = f(&current);
            },
        ),
        "update_metadata"
    );
}

/// Locked read-modify-write of an instance's full metadata object.
pub(crate) fn update_metadata_object(home: &Path, instance_name: &str, f: impl FnOnce(&mut Value)) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path_resolved(home, instance_name);
    #[cfg(test)]
    note_metadata_rmw();
    persist_or_log!(
        crate::store::with_json_state_or_create::<Value, _, _, _>(&meta_path, || json!({}), f,),
        "update_metadata_object"
    );
}

/// Persist multiple metadata key/value pairs in a single locked read-modify-write.
/// #1886 C2: the flock spans the whole load→modify→write (not just the write), so
/// concurrent `save_metadata`/`save_metadata_batch` on the same instance never read
/// stale data and lose each other's update (the prior comment's interleave race).
pub fn save_metadata_batch(home: &Path, instance_name: &str, entries: &[(&str, Value)]) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path_resolved(home, instance_name);
    #[cfg(test)]
    note_metadata_rmw();
    // #1647: log on failure — see save_metadata.
    persist_or_log!(
        crate::store::with_json_state_or_create::<Value, _, _, _>(
            &meta_path,
            || json!({}),
            |meta| {
                for (key, value) in entries {
                    meta[*key] = value.clone();
                }
            },
        ),
        "save_metadata_batch"
    );
}

// ---------------------------------------------------------------------------
// Git branch validation
// ---------------------------------------------------------------------------

/// Validate a git branch name. Allows the char set `[a-zA-Z0-9/_.-]`, rejects
/// `..` anywhere and a leading `-`, and enforces per-component refname/path
/// rules that matter because the branch doubles as a filesystem path component
/// in `worktree_path` (`home/worktrees/<agent>/<branch>`): every `/`-separated
/// component must be
/// - non-empty (rejects a trailing/leading/double `/`),
/// - not begin with `.` (a leading-dot component like `.git` / `.agend-managed`
///   collides with worktree-pool control files; a lone `.`/`..` is a no-op /
///   parent path component and an invalid git refname), and
/// - not end in `.lock` (git rejects `.lock`-suffixed refs).
///
/// Interior dots stay valid (`v1.0.0`, `release_2.0`).
pub fn validate_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.contains("..")
        && !branch.starts_with('-')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
        && branch.split('/').all(|component| {
            !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
        })
}

/// E4.5 protected-branch invariant — see `crate::protected_refs::is_protected_ref`
/// for the canonical definition + rationale. #2550 W4: re-exported here (not
/// redefined) so this module's existing public path (`agent_ops::is_protected_ref`)
/// is unchanged for callers, while the shim binary `#[path]`-includes the same
/// standalone source instead of hand-mirroring it.
pub use crate::protected_refs::is_protected_ref;

pub fn ensure_not_protected(branch: &str) -> Result<(), String> {
    if is_protected_ref(branch) {
        Err(format!(
            "E4.5 violation: protected branch '{branch}' cannot be used for agent worktrees"
        ))
    } else {
        Ok(())
    }
}

pub fn ensure_not_protected_json(branch: &str) -> Result<(), serde_json::Value> {
    if is_protected_ref(branch) {
        Err(serde_json::json!({
            "error": format!("E4.5 violation: protected branch '{branch}' rejected"),
            "code": "e4_5_protected_branch"
        }))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Working-directory cleanup (CANONICAL 19-entry list)
// ---------------------------------------------------------------------------

/// Clean up files generated by agend-terminal in an instance's working
/// directory.
///
/// If the directory is under `$AGEND_HOME/workspace/`, the entire directory
/// is removed. Otherwise (user-provided working dir), only agend-generated
/// files are removed to avoid deleting user code.
///
/// The 19-entry `agend_files` list below is the **canonical** superset.
/// The copy in `mcp/handlers.rs` drifted to 14 entries on 2026-04-14 and
/// is missing the 5 Kiro paths: `.kiro/agents/{agend.json,agend-prompt.md,
/// default.json}`, `.kiro/prompts/agend.md`, `.kiro/settings.json`.
pub fn cleanup_working_dir(home: &Path, name: &str, working_dir: &Path) -> Option<String> {
    // Workspace-identity guard (fail-closed): before removing anything under
    // `working_dir`, refuse if the directory's on-disk identity belongs to a
    // DIFFERENT instance (or is corrupt/unreadable). Deleting instance A must
    // never wipe a directory that identity artifacts (AGENTS.md block / `.codex`
    // stamp) say belongs to instance B — preserve the tree and emit a loud audit.
    // Metadata keyed by A's own name (the tail below) is still cleaned; only the
    // shared working directory is preserved.
    //
    // Held under the workspace-identity lock so the ownership CHECK and the
    // REMOVAL are atomic against a concurrent provision/delete of the same
    // directory. The SINGLE returned verdict is what `full_delete_instance`
    // reports — it does NOT probe a second (unlocked) time. A lock-acquire
    // failure is itself fail-closed: refuse and preserve.
    let id_lock = crate::store::acquire_workspace_identity_lock(home, working_dir);
    let conflict = match &id_lock {
        Ok(_) => working_dir_ownership_conflict(working_dir, name),
        Err(e) => Some(format!("could not acquire workspace-identity lock: {e}")),
    };
    if let Some(reason) = &conflict {
        tracing::error!(
            dir = %working_dir.display(), name, %reason,
            "cleanup refused: working directory identity belongs to a different instance — tree preserved"
        );
    } else {
        let workspaces = crate::paths::workspace_dir(home);

        // If under $AGEND_HOME/workspace/, remove the whole directory.
        // CR-2026-06-14 (security): a purely LEXICAL `starts_with` lets a symlink
        // under workspace/ whose real target is ELSEWHERE take this whole-dir
        // `remove_dir_all` and follow the symlink out of the workspace, destroying
        // real user data. Require the path to ALSO resolve canonically inside the
        // canonicalized workspace root (canonicalize BOTH so a symlinked
        // $AGEND_HOME — e.g. macOS /tmp→/private/tmp — still matches).
        let under_workspace = working_dir.starts_with(&workspaces)
            && match (
                dunce::canonicalize(working_dir),
                dunce::canonicalize(&workspaces),
            ) {
                (Ok(wd), Ok(ws)) => wd.starts_with(&ws),
                _ => false,
            };
        if under_workspace {
            // #2234 Phase 0: under cure-(B) the workspace dir IS a daemon-managed
            // canonical worktree (its `.git` is a gitlink FILE). A bare
            // remove_dir_all would destroy uncommitted/unpushed work AND orphan the
            // worktree registration in the canonical repo. Route a worktree through
            // `git worktree remove --force` (work-at-risk backed up first). A
            // standalone clone / plain dir (the pre-(B) state) returns false here →
            // the byte-identical remove_dir_all below still runs.
            if crate::worktree_pool::teardown_workspace_worktree(home, name, working_dir) {
                // handled (gitlink worktree): removal + registry cleanup done.
            } else if let Err(e) = std::fs::remove_dir_all(working_dir) {
                tracing::debug!(dir = %working_dir.display(), error = %e, "cleanup: remove workspace");
            } else {
                tracing::info!(dir = %working_dir.display(), "removed workspace");
            }
        } else {
            let worktrees = home.join("worktrees");
            let under_worktrees = working_dir.starts_with(&worktrees)
                && match (
                    dunce::canonicalize(working_dir),
                    dunce::canonicalize(&worktrees),
                ) {
                    (Ok(wd), Ok(wt)) => wd.starts_with(&wt),
                    _ => false,
                };
            if under_worktrees {
                if crate::worktree_pool::teardown_workspace_worktree(home, name, working_dir) {
                    // handled (gitlink worktree): removal + registry cleanup done.
                } else if let Err(e) = std::fs::remove_dir_all(working_dir) {
                    tracing::debug!(dir = %working_dir.display(), error = %e, "cleanup: remove managed worktree");
                } else {
                    tracing::info!(dir = %working_dir.display(), "removed managed worktree");
                }
            } else {
                // User-provided working directory: only remove agend-generated files
                let agend_files = [
                    // Claude
                    ".claude/settings.local.json",
                    "mcp-config.json",
                    "claude-settings.json",
                    "statusline.sh",
                    "statusline.json",
                    ".claude/rules/agend.md",
                    // Gemini
                    ".gemini/settings.json",
                    // OpenCode
                    "opencode.json",
                    "instructions/agend.md",
                    // Codex
                    ".codex/config.toml",
                    "AGENTS.md",
                    // Kiro
                    ".kiro/settings/mcp.json",
                    ".kiro/settings/agend-mcp-wrapper.sh",
                    ".kiro/steering/agend.md",
                    ".kiro/agents/agend.json",
                    ".kiro/agents/agend-prompt.md",
                    ".kiro/agents/default.json",
                    ".kiro/prompts/agend.md",
                    ".kiro/settings.json",
                ];
                for file in &agend_files {
                    let path = working_dir.join(file);
                    if path.exists() {
                        let _ = std::fs::remove_file(&path);
                    }
                }

                // Clean up worktree if exists
                let wt_dir = working_dir.join(".worktrees").join(name);
                if wt_dir.exists() {
                    // W1.2: LOCAL best-effort worktree-remove via the bypass+bounded
                    // helper (was a raw UNBOUNDED `.output()` whose result was already
                    // discarded). git_ok adds the LOCAL_GIT_TIMEOUT bound so a stuck
                    // remove can't hang teardown; the bypass env is a no-op in the
                    // daemon's shim-free PATH. Result stays discarded → same effect.
                    let _ = crate::git_helpers::git_ok(
                        working_dir,
                        &[
                            "worktree",
                            "remove",
                            "--force",
                            &wt_dir.display().to_string(),
                        ],
                    );
                    tracing::info!(dir = %wt_dir.display(), "removed worktree");
                }
            }
        }
    }

    // Always clean up metadata (regardless of workspace vs user dir)
    let meta_dir = home.join("metadata");
    // #1157: also clean id-based metadata (Sprint 46 P2 symlink/copy).
    // Best-effort: fleet.yaml may already be removed by caller.
    if let Some(id_path) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|c| {
            c.instances
                .get(name)
                .and_then(|i| i.id.as_deref())
                .map(|id| meta_dir.join(format!("{id}.json")))
        })
    {
        let _ = std::fs::remove_file(&id_path);
    }
    let _ = std::fs::remove_file(meta_dir.join(format!("{name}.json")));

    // #1547 (A): remove the non-hidden agy workspace link (no-op for non-agy
    // instances / when no link exists). Keyed by instance name, not by
    // working_dir, so it lives outside both cleanup branches above. Never
    // touches the real workspace — only the managed symlink/junction.
    crate::agy_workspace::remove_link(home, name);

    conflict
}

/// Cleanup entry point for a caller that already owns the per-agent lifecycle
/// permit. The permit covers the identity check and all path mutations below;
/// a missing or mismatched permit fails closed without touching the directory.
pub(crate) fn cleanup_working_dir_with_permit(
    home: &Path,
    name: &str,
    working_dir: &Path,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Option<String> {
    if !permit.authorizes(home, name) {
        return Some("working-directory cleanup refused: invalid lifecycle permit".to_string());
    }
    cleanup_working_dir(home, name, working_dir)
}

/// Apply a pre-delete admission derived from the FleetConfig snapshot.
/// `Preserve` is intentionally a complete path-local no-op: the shared
/// directory must not even enter the backend scrub path.
pub(crate) fn cleanup_working_dir_admitted(
    home: &Path,
    name: &str,
    working_dir: &Path,
    admission: &cleanup_admission::CleanupAdmission,
) -> Option<String> {
    match admission {
        cleanup_admission::CleanupAdmission::Preserve { reason } => {
            tracing::warn!(
                name,
                dir = %working_dir.display(),
                %reason,
                "pre-delete cleanup admission preserved working directory"
            );
            None
        }
        cleanup_admission::CleanupAdmission::NoOp { reason } => {
            tracing::debug!(
                name,
                dir = %working_dir.display(),
                %reason,
                "pre-delete cleanup admission found no working directory to mutate"
            );
            None
        }
        cleanup_admission::CleanupAdmission::Refuse { reason } => Some(reason.clone()),
        cleanup_admission::CleanupAdmission::RemoveOwned { canonical }
        | cleanup_admission::CleanupAdmission::ScrubExclusive { canonical } => {
            match dunce::canonicalize(working_dir) {
                Ok(actual) if actual == *canonical => cleanup_working_dir(home, name, working_dir),
                Ok(actual) => Some(format!(
                    "working directory changed after admission: {} now resolves to {}, expected {}",
                    working_dir.display(),
                    actual.display(),
                    canonical.display()
                )),
                Err(error) => Some(format!(
                    "working directory no longer canonicalizes after admission: {} ({error})",
                    working_dir.display()
                )),
            }
        }
    }
}

/// Whether `working_dir`'s on-disk identity artifacts name an instance OTHER
/// than `name` (or are corrupt) — in which case the caller must NOT remove the
/// tree. Returns `Some(reason)` to refuse (foreign owner / corrupt artifact),
/// `None` to proceed (no identity artifact, or the directory belongs to `name`).
/// Checks the AGENTS.md agend block (which records the SANITIZED identifier) and
/// the `.codex/config.toml` `AGEND_INSTANCE_NAME` stamp (which records the RAW
/// name) — the two durable identity artifacts the collision incident involved.
pub(crate) fn working_dir_ownership_conflict(working_dir: &Path, name: &str) -> Option<String> {
    // Fail-closed: `agents_md_identity` / `codex_config_identity` return
    // `Unreadable` (→ a conflict) for any non-`NotFound` I/O error, so an
    // unreadable artifact refuses the delete rather than being read as absent.
    if let Some(reason) = crate::instructions::agents_md_identity(&working_dir.join("AGENTS.md"))
        .conflict_with(&crate::instructions::sanitize_identifier(name))
    {
        return Some(format!("AGENTS.md {reason}"));
    }
    if let Some(reason) =
        crate::instructions::agents_md_identity(&working_dir.join(".agents").join("AGENTS.md"))
            .conflict_with(&crate::instructions::sanitize_identifier(name))
    {
        return Some(format!(".agents/AGENTS.md {reason}"));
    }
    if let Some(reason) =
        crate::mcp_config::codex_config_identity(&working_dir.join(".codex").join("config.toml"))
            .conflict_with(name)
    {
        return Some(format!(".codex/config.toml {reason}"));
    }
    if let Some(reason) =
        crate::mcp_config::codex_config_identity(&working_dir.join(".grok").join("config.toml"))
            .conflict_with(name)
    {
        return Some(format!(".grok/config.toml {reason}"));
    }
    for artifact in &[".claude/agend.md", ".kiro/steering/agend.md"] {
        let path = working_dir.join(artifact);
        if let Some(reason) = crate::instructions::nonshared_instructions_identity(&path)
            .conflict_with(&crate::instructions::sanitize_identifier(name))
        {
            return Some(format!("{artifact} {reason}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Agent enumeration
// ---------------------------------------------------------------------------

/// List agents — daemon registry truth-of-record via the
/// `runtime::list_agents_with_fallback` helper. Falls back to the
/// filesystem `.port` glob when the daemon API is unreachable.
///
/// MCP-facing: the `LIST` handler at `src/mcp/handlers/instance.rs:36/39`
/// wraps the result in `{"instances": [...]}` as the fallback when the
/// rich-info path fails.
///
/// #910 PR2 of 4: was a bespoke read_dir glob; now delegates to the
/// canonical helper landed in PR1 (#923).
pub fn list_agents() -> Vec<String> {
    crate::runtime::list_agents_with_fallback(&crate::home_dir())
}

/// #2454 S3: neutral typed list-snapshot service.  Owns the lock-drop-
/// before-disk-I/O ordering and the full/external agent serialisation.
/// Both the API LIST wire handler and the MCP instance-query path call
/// this — neither owns the logic.
pub(crate) fn list_snapshot(
    home: &Path,
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
) -> Value {
    let reg = agent::lock_registry(registry);
    let snapshot: Vec<(String, Value)> = reg
        .values()
        .map(|handle| {
            let name = handle.name.to_string();
            let (
                agent_state,
                health_state,
                blocked_reason,
                blocked_note,
                context,
                context_provider,
                context_meaning,
                api_in_flight,
                last_api_activity_at,
                observed_status,
                self_kick,
            ) = {
                let c = handle.core.lock();
                (
                    c.state.get_state().display_name().to_string(),
                    c.health.state.display_name().to_string(),
                    c.health.current_reason.as_ref().map(|r| r.to_string()),
                    c.health.current_note.clone(),
                    c.state.resolved_context(),
                    c.state.context_provider(),
                    c.state.context_meaning(),
                    c.api_activity.in_flight,
                    c.api_activity.last_active_epoch_ms,
                    c.observed_status.clone(),
                    c.health.last_self_kick.clone(),
                )
            };
            let entry = json!({
                "name": name.as_str(),
                // The UUID is the configured identity and generation fences
                // replacements of the same configured instance. Keep this
                // additive so older roster consumers remain compatible.
                "instance_ref": crate::types::InstanceRef::new(
                    handle.id,
                    handle.generation.value(),
                ),
                "backend": handle.backend_command,
                "submit_key": handle.submit_key,
                "inject_prefix": handle.inject_prefix,
                "agent_state": agent_state,
                "health_state": health_state,
                "blocked_reason": blocked_reason,
                "blocked_note": blocked_note,
                "context_pct": context.map(|(pct, _)| pct),
                "context_source": context.map(|(_, source)| source),
                "context_provider": context_provider.source_name(),
                // t-…-82348-36: what context_pct MEANS — "window_fill" (Claude:
                // fill of an auto-compacted context window, NOT remaining
                // session budget), "context_gauge" (kiro), null when the
                // backend scrapes no figure. Additive; context_pct /
                // context_source / context_provider are unchanged.
                "context_meaning": context_meaning,
                "api_in_flight": api_in_flight,
                "last_api_activity_at": last_api_activity_at,
                "observed_status": observed_status,
                // t-…-82348-105: the last RECONCILED fresh-restart self-kick
                // outcome, or null when nothing went wrong. Purely additive
                // evidence so an orchestrator sees an unacknowledged resume in
                // list_instances instead of only in the event log.
                "self_kick": self_kick.map(|e| json!({
                    "delivery_id": e.delivery_id,
                    "accepted_at": e.accepted_at.to_rfc3339(),
                    "state": e.state,
                    "turn_observed_since_kick": e.turn_observed_since_kick,
                    "at": e.at.to_rfc3339(),
                })),
                "kind": "managed",
            });
            (name, entry)
        })
        .collect();
    drop(reg);

    let mut agents: Vec<Value> = Vec::with_capacity(snapshot.len());
    for (name, mut entry) in snapshot {
        let (dispatched_waiting_for, pending_response_to) =
            crate::daemon::dispatch_idle::pending_for_instance(home, &name);
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "dispatched_waiting_for".into(),
                json!(dispatched_waiting_for),
            );
            obj.insert("pending_response_to".into(), json!(pending_response_to));
        }
        agents.push(entry);
    }
    let ext = agent::lock_external(externals);
    for (name, handle) in ext.iter() {
        let (dispatched_waiting_for, pending_response_to) =
            crate::daemon::dispatch_idle::pending_for_instance(home, name);
        agents.push(json!({
            "name": name,
            "backend": handle.backend_command,
            "agent_state": "external",
            "health_state": "connected",
            "kind": "external",
            "pid": handle.pid,
            "dispatched_waiting_for": dispatched_waiting_for,
            "pending_response_to": pending_response_to,
        }));
    }
    json!({"ok": true, "result": {"protocol_version": crate::framing::PROTOCOL_VERSION, "agents": agents}})
}

/// #2454 S4: neutral typed input-injection service.  Shared by the API
/// INJECT wire handler and the MCP interrupt path — neither owns the
/// registry lookup, operated-state gate, or PTY write logic.
pub(crate) fn inject_input(
    registry: &AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
    home: &std::path::Path,
    target: &str,
    data: &[u8],
    raw: bool,
) -> Result<usize, InjectError> {
    if let Err(e) = agent::validate_name(target) {
        return Err(InjectError::Validation(e));
    }
    let snap = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, target)
            .and_then(|id| reg.get(&id))
            .map(|handle| {
                let operated_state = {
                    let core = handle.core.lock();
                    crate::daemon::shadow::operated_state(
                        core.state.current,
                        core.observed_status.as_ref(),
                    )
                };
                (agent::InjectTarget::from_handle(handle), operated_state)
            })
    };
    match snap {
        Some((tgt, operated_state)) => {
            if operated_state.is_unavailable() {
                let state_name = operated_state.display_name();
                return Err(InjectError::Unavailable(format!(
                    "agent '{target}' is {state_name}, retry later"
                )));
            }
            let result = if raw {
                agent::write_to_pty(&tgt.pty_writer, data)
            } else {
                agent::inject_with_target_gated(&tgt, target, data, true, None)
            };
            match result {
                Ok(()) => Ok(data.len()),
                Err(e) => Err(InjectError::Write(format!("{e}"))),
            }
        }
        None => {
            let ext = agent::lock_external(externals);
            if ext.contains_key(target) {
                Err(InjectError::External(format!(
                    "agent '{target}' is external — use send instead of inject"
                )))
            } else {
                Err(InjectError::NotFound(format!("agent '{target}' not found")))
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum InjectError {
    Validation(String),
    Unavailable(String),
    External(String),
    NotFound(String),
    Write(String),
}

impl std::fmt::Display for InjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(e)
            | Self::Unavailable(e)
            | Self::External(e)
            | Self::NotFound(e)
            | Self::Write(e) => f.write_str(e),
        }
    }
}

/// Spawn a single agent into `registry` and start its TUI-serve thread.
/// Shared by the SPAWN and CREATE_TEAM API handlers.
///
/// `env` carries the resolved process env to apply on top of inherited
/// vars (post sensitive-env deny-list filter; see
/// `agent::is_sensitive_env_key`). Callers are expected to resolve from
/// `params.env` or `FleetConfig::resolve_instance(name).env` BEFORE
/// invoking — `spawn_one` is a pure data consumer here, not a re-resolver,
/// so a single canonical resolve site at the handler boundary stays
/// authoritative (#900 hybrid (b)+(c) design).
///
/// W1.3② (#2050): moved verbatim from `api/mod.rs` to its cohesive home next
/// to `remove_metadata` (which it calls) — `api/mod.rs` was the server file,
/// not the owner of agent-spawn primitives. Behavior unchanged.
#[allow(clippy::too_many_arguments)]
pub fn spawn_one(
    home: &Path,
    registry: &AgentRegistry,
    name: &str,
    backend: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
    work_dir: &Path,
    size: (u16, u16),
    env: Option<&std::collections::HashMap<String, String>>,
    declared_backend: Option<&crate::backend::Backend>,
) -> anyhow::Result<crate::backend::SpawnMode> {
    std::fs::create_dir_all(work_dir).ok();
    // #1080: skills auto-install for dynamically spawned instances.
    // spawn_one is the SPAWN-RPC choke point — without this, instances
    // created via create_instance / start_instance / restart_instance
    // never get skill symlinks (only cold-boot spawn_and_register_agent
    // called install_for_agent). Respects fleet.yaml `instance.<name>.skills:`
    // allowlist, same as cold-boot path.
    let skills_filter: Option<Vec<String>> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
    let custom_skills_source: Option<std::path::PathBuf> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
            .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
    let effective_backend = declared_backend
        .cloned()
        .or_else(|| crate::backend::Backend::from_command(backend));
    let backend_skill = effective_backend.clone().and_then(|b| b.skill_dir_name());
    match crate::skills::install_for_agent_backend_with_source(
        home,
        work_dir,
        skills_filter.as_deref(),
        backend_skill,
        custom_skills_source.as_deref(),
    ) {
        Ok(outcomes) => {
            let modes: Vec<(&str, crate::skills::InstallMode)> = outcomes
                .iter()
                .map(|o| (o.backend.as_str(), o.mode))
                .collect();
            tracing::info!(agent = %name, ?modes, "spawn_one skills auto-install complete");
        }
        Err(e) => {
            tracing::warn!(agent = %name, error = %e, "spawn_one skills auto-install failed, proceeding");
        }
    }
    // Sprint 34: clear stale metadata from a previous instance with the
    // same name. spawn_one is the true choke point — both handle_spawn
    // (direct) and team.rs (team-spawn) flow through here.
    // #1682: clear BOTH the legacy name file and the id-resolved file — post-#1680
    // readers use `<uuid>.json`, which the old name-only remove left stale.
    remove_metadata(home, name);
    let preset_submit_key = preset_submit_key(effective_backend.as_ref());
    // No-op when caller already passed Fresh; downgrades Resume → Fresh when
    // there is no resumable session in `work_dir` (see
    // `SpawnMode::downgraded_for`). Returned so callers (e.g. the
    // `create_instance` API handler) can see the actual mode used and gate
    // post-spawn behavior like the "skip broadcast on Resume" rule.
    let spawn_mode = spawn_mode.downgraded_for(backend, Some(work_dir));
    agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend: declared_backend,
            backend_command: backend,
            args,
            spawn_mode,
            cols: size.0,
            rows: size.1,
            env,
            working_dir: Some(work_dir),
            submit_key: preset_submit_key,
            home: Some(home),
            crash_tx: None,
            shutdown: None,
        },
        registry,
    )?;
    let rdir = crate::daemon::run_dir(home);
    let reg = Arc::clone(registry);
    let n = name.to_string();
    // fire-and-forget: per-agent TUI-socket server; runs for the agent's
    // lifetime and self-terminates when `serve_agent_tui` sees the agent leave
    // the registry / the run dir socket closes (no graceful-join needed —
    // mirrors the cold-boot `spawn_and_register_agent` TUI thread). §10.5: this
    // spawn previously rode `api/mod.rs`'s legacy exemption; the W1.3② move into
    // an in-scope file gives it a real rationale instead.
    std::thread::Builder::new()
        .name(format!("{n}_tui"))
        .spawn(move || crate::daemon::serve_agent_tui(&n, &rdir, &reg))
        .ok();
    Ok(spawn_mode)
}

// ---------------------------------------------------------------------------
// Tests (characterization — migrated from ops.rs + mcp/handlers.rs,
// plus a new drift-guard asserting the canonical 19-entry cleanup set.)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "agent_ops_tests.rs"]
mod tests;

#[cfg(test)]
mod review_repro_agent_binding;
#[cfg(test)]
mod review_repro_xcut_security;
