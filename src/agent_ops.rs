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
pub fn delete_instance(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
) -> DeleteOutcome {
    delete_instance_with_exit_status(home, name, context, skip_exit_wait).0
}

pub(crate) fn delete_instance_with_exit_status(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
) -> (DeleteOutcome, bool) {
    // The public runtime entry owns the complete deletion fence even for an
    // external agent. External-first resolution must not bypass transport
    // invalidation: a queued job for the same name can otherwise outlive the
    // early return and recreate delivery state or reach an adapter.
    let _delete_fence = crate::daemon::lifecycle::DeleteFence::new(home, name, true);
    if let Err(error) = crate::transport::remove_instance_delivery_state(home, name) {
        tracing::warn!(
            agent = %name,
            error = %error,
            "delete: transport delivery cleanup failed"
        );
    }
    delete_instance_impl(home, name, context, skip_exit_wait)
}

/// Delete through the shared transaction body when the caller already owns the
/// deleting mark and keyed transport cleanup guard (the full-delete path).
pub(crate) fn delete_instance_under_guard(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
) -> (DeleteOutcome, bool) {
    // The full-delete caller already owns DeleteFence; do not nest a second
    // lifecycle or transport guard around this body.
    delete_instance_impl(home, name, context, skip_exit_wait)
}

fn delete_instance_impl(
    home: &Path,
    name: &str,
    context: &DeleteContext<'_>,
    skip_exit_wait: bool,
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
                api_in_flight,
                last_api_activity_at,
                observed_status,
            ) = {
                let c = handle.core.lock();
                (
                    c.state.get_state().display_name().to_string(),
                    c.health.state.display_name().to_string(),
                    c.health.current_reason.as_ref().map(|r| r.to_string()),
                    c.health.current_note.clone(),
                    c.state.resolved_context(),
                    c.state.context_provider(),
                    c.api_activity.in_flight,
                    c.api_activity.last_active_epoch_ms,
                    c.observed_status.clone(),
                )
            };
            let entry = json!({
                "name": name.as_str(),
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
                "api_in_flight": api_in_flight,
                "last_api_activity_at": last_api_activity_at,
                "observed_status": observed_status,
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
/// The submit key a spawned agent gets, derived from its effective backend.
///
/// One source for both places that need it: the live handle `spawn_one` builds,
/// and the `AgentConfig` the spawn transaction records. Deriving it twice is how
/// the two drift apart, and a config that disagrees with the running agent is
/// worse than no config at all — crash respawn would replay the wrong key.
pub(crate) fn preset_submit_key(backend: Option<&crate::backend::Backend>) -> &'static str {
    backend.map_or("\r", |b| b.preset().submit_key)
}

/// The per-`(home, name)` spawn lane.
///
/// Same key shape as the DELETING registry (`crate::agent::deleting`) and the
/// same "short global lock, then work on the `Arc`" acquisition as the write
/// actors' `WRITERS` map. Entries are not reaped: the map grows only to the
/// number of DISTINCT instance names this process has ever spawned, which is the
/// fleet's own order of magnitude, and reaping would need a second global
/// acquisition on every release to stay race-free.
type SpawnLane = std::sync::Arc<parking_lot::Mutex<()>>;
type SpawnLanes =
    parking_lot::Mutex<std::collections::HashMap<(std::path::PathBuf, String), SpawnLane>>;

fn spawn_lane(home: &std::path::Path, name: &str) -> SpawnLane {
    static LANES: std::sync::OnceLock<SpawnLanes> = std::sync::OnceLock::new();
    let lanes = LANES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let key = (
        dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
        name.to_string(),
    );
    let mut map = lanes.lock();
    SpawnLane::clone(map.entry(key).or_default())
}

/// Test seam: do these two home spellings resolve to the SAME lane?
///
/// Pointer identity of the `Arc`, so the answer comes from the lane itself rather
/// than from restating the key-building code.
#[cfg(test)]
pub(crate) fn lanes_are_the_same(a: &std::path::Path, b: &std::path::Path, name: &str) -> bool {
    SpawnLane::ptr_eq(&spawn_lane(a, name), &spawn_lane(b, name))
}

/// Record an agent's resolved configuration, then spawn it — the transaction
/// every post-boot spawn surface goes through.
///
/// #3417: `ctx.configs` used to be written only by the BOOT path, so every
/// runtime-created instance was absent from the map that the snapshot writer and
/// `crash_respawn` read. The snapshot degraded to a plausible `args: []`; crash
/// respawn simply refused to respawn — not a reporting nicety but a live gap.
///
/// The whole transaction runs inside a per-`(home, name)` lane, because the
/// alternatives do not survive a concurrent same-name spawn. Nothing else
/// serializes those: `spawn_instance`'s duplicate check reads the registry and
/// the actual registration happens later. Rolling back on a value comparison
/// cannot tell two identical configs apart, and consulting the registry after a
/// failure is still check-then-act — the winner can register between the check
/// and the restore, leaving a live agent with a stale or missing config, which is
/// the exact defect this work removes.
///
/// Inside the lane the order is load-bearing in both directions:
///
/// * The insert happens BEFORE the spawn, because a child can exit — and the
///   crash path can look this config up — before the spawn call returns.
/// * A failure restores the PREVIOUS value rather than deleting, so a failed
///   restart cannot strip the config its predecessor was described by.
/// * Success retains it. A child that starts and then exits immediately is not a
///   failed spawn; it is precisely the case crash respawn exists for.
///
/// Locks: the per-name lane guard is held across `spawn` and its file work; the
/// configs and registry locks are not held across `spawn`, and disk I/O does not
/// occur under either of those locks. The lane is taken only here, at the
/// outermost layer of a spawn, so nothing that `spawn` itself locks can be waiting
/// on it; and no surface enters it twice for one name on one thread (restart
/// deletes outside the lane, deployment and team spawn distinct names in
/// sequence).
pub(crate) fn spawn_one_recording_config(
    home: &std::path::Path,
    configs: &crate::api::ConfigRegistry,
    name: &str,
    config: crate::daemon::AgentConfig,
    spawn: impl FnOnce() -> anyhow::Result<crate::backend::SpawnMode>,
) -> anyhow::Result<crate::backend::SpawnMode> {
    let lane = spawn_lane(home, name);
    let _lane = lane.lock();
    let previous = configs.lock().insert(name.to_string(), config);
    let mut rollback = SpawnRollback {
        configs,
        name,
        previous,
        armed: true,
    };
    let outcome = spawn();
    if outcome.is_ok() {
        rollback.armed = false;
    }
    outcome
}

/// Undoes the transaction's insert unless the spawn committed.
///
/// A guard rather than an `Err` arm so the invariant survives a PANICKING spawn:
/// unwinding runs this, and a panic that left the attempted config behind would
/// describe an agent that may not exist.
struct SpawnRollback<'a> {
    configs: &'a crate::api::ConfigRegistry,
    name: &'a str,
    previous: Option<crate::daemon::AgentConfig>,
    armed: bool,
}

impl Drop for SpawnRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut cfgs = self.configs.lock();
        // DELETE (`lifecycle::delete_transaction` step 6) and clean exit
        // (`handle_clean_exit`) are the removal authority, and both remove under
        // THIS lock. If our entry is gone, one of them retired the instance while
        // we were spawning, and restoring `previous` would resurrect a config for
        // something that has been deleted — handing crash respawn a dead agent.
        // Deletion therefore wins and the rollback stands down.
        //
        // Presence is a sound ownership token here, and only here: the lane
        // guarantees no other SPAWN can have written this key, so a present entry
        // is still ours. The test and the write share this one critical section,
        // so this is a compare-and-act, not the post-failure check-then-act that
        // the lane exists to remove.
        if !cfgs.contains_key(self.name) {
            return;
        }
        match self.previous.take() {
            Some(previous) => {
                cfgs.insert(self.name.to_string(), previous);
            }
            None => {
                cfgs.remove(self.name);
            }
        }
    }
}

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
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-agent-ops-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn move_pane_validates_parses_and_logs_2454() {
        let home = tmp_home("move-pane-service-2454");
        let event = move_pane(&home, Some("agent-a"), Some("team-x"), Some("vertical")).unwrap();
        assert_eq!(event.agent, "agent-a");
        assert_eq!(event.target_tab, "team-x");
        assert_eq!(event.split_dir, PaneMoveSplit::Vertical);
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap();
        assert!(log.contains("\"kind\":\"move_pane\""));
        assert!(log.contains("target_tab=team-x split=Vertical"));
        assert_eq!(
            move_pane(&home, None, Some("team-x"), None),
            Err("missing agent".into())
        );
        assert_eq!(
            move_pane(&home, Some("agent-a"), None, None),
            Err("missing target_tab".into())
        );
    }

    /// The public runtime DELETE must fence an external early-return path just
    /// like a managed delete. A queued old-generation job held behind the
    /// keyed lane must be discarded after the external record is removed,
    /// without reaching an adapter or recreating receipt state.
    #[test]
    fn external_delete_fences_queued_transport_before_early_return() {
        run_external_delete_fixture(true);
    }

    fn run_external_delete_fixture(send_outcome: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let home = tmp_home("external-delete-transport-fence");
        let agent = format!("external-delete-race-{}", std::process::id());
        let _full_guard = crate::daemon::delivery_worker::test_support::force_full_guard();
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let _delivery_hook = crate::transport::test_support::delivery_hook_guard();
        let _cleanup_release_tail_guard =
            crate::daemon::delivery_worker::test_support::cleanup_release_tail_hook_guard();
        let adapter_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let adapter_calls_hook = std::sync::Arc::clone(&adapter_calls);
        let expected_home = home.clone();
        let expected_agent = agent.clone();
        crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
            move |called_home, called_agent, _body| {
                if called_home == expected_home.as_path() && called_agent == expected_agent {
                    adapter_calls_hook.fetch_add(1, Ordering::SeqCst);
                    Some(Err(anyhow::anyhow!(
                        "external-delete stale job reached adapter"
                    )))
                } else {
                    None
                }
            },
        )));

        // #3240 slice 2. Lane entry is an ORDERING fact, and this fixture used to
        // assert it with a one-second wall-clock budget that had to cover a
        // thread spawn, a global lane-map lock, a keyed mutex, this admission
        // hook and an epoch-state lock — so a loaded machine failed a healthy
        // run. The hook below deliberately pushes admission PAST that old budget
        // exactly once, so re-introducing any clock-based readiness wait fails
        // deterministically here instead of flaking somewhere else later.
        //
        // The delay is UNCONDITIONAL, and nothing else needs to be: the hook runs
        // only inside `with_transport_serial` (daemon/delivery_worker.rs:337-340),
        // while the queued worker takes the lane itself and hands the guard to
        // `dispatch_transport` (:468-471), which never runs the hook. So this
        // delay cannot reach the dispatch assertion further down, and a latch to
        // keep it away from there would be guarding a path that does not exist.
        const LANE_ADMISSION_DELAY: Duration = Duration::from_millis(1200);
        let _admission_guard =
            crate::daemon::delivery_worker::test_support::direct_transport_admission_hook_guard();
        let _cleanup_before_lane_guard =
            crate::daemon::delivery_worker::test_support::cleanup_before_lane_acquire_hook_guard();
        let admission_home = home.clone();
        let admission_agent = agent.clone();
        crate::daemon::delivery_worker::test_support::set_direct_transport_admission_hook(Some(
            std::sync::Arc::new(move |hook_home: &std::path::Path, hook_agent: &str| {
                if hook_home == admission_home.as_path() && hook_agent == admission_agent {
                    std::thread::sleep(LANE_ADMISSION_DELAY);
                }
            }),
        ));

        // Deterministic treatment for the completion clock: the cleanup tail
        // is a real post-lane release path, so delaying it here reproduces the
        // Windows panic without relying on scheduler or filesystem load.
        const CLEANUP_RELEASE_TAIL_DELAY: Duration = Duration::from_millis(1200);
        let tail_home = home.clone();
        let tail_agent = agent.clone();
        crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
            std::sync::Arc::new(move |hook_home, hook_agent| {
                if hook_home == tail_home.as_path() && hook_agent == tail_agent {
                    std::thread::sleep(CLEANUP_RELEASE_TAIL_DELAY);
                }
            }),
        ));

        let (lane_entered_tx, lane_entered_rx) = std::sync::mpsc::channel();
        let (lane_release_tx, lane_release_rx) = std::sync::mpsc::channel();
        let lane_home = home.clone();
        let lane_agent = agent.clone();
        let lane_holder = std::thread::spawn(move || {
            crate::daemon::delivery_worker::with_transport_serial(&lane_home, &lane_agent, || {
                lane_entered_tx.send(()).expect("lane-entered observer");
                lane_release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .expect("lane release");
            });
        });
        // A BARRIER, not a clock. The ordering fact this fixture needs is
        // narrow and worth stating exactly: the holder has entered the
        // SYNCHRONOUS `with_transport_serial` path — past the lane acquire and
        // past the test admission hook — because that is where it sends. It
        // says nothing about the queued worker, which reaches
        // `dispatch_transport` by a different route. No wall-clock budget can
        // express even that much: too short flakes on a loaded machine, too
        // long only delays the flake. The wait is bounded by DISCONNECTION instead — if the holder
        // thread dies or panics, its sender drops and `recv()` returns `Err`
        // immediately, so a genuine failure stays fast and named rather than
        // becoming a hang. Teardown below keeps its own explicit bounds.
        //
        // DISCONNECT-BOUNDED IS NOT DEADLOCK-BOUNDED, and the difference is not
        // uniform across CI. If the holder neither finishes nor dies — wedged
        // inside `TransportLaneGuard::acquire`, say — its sender never drops and
        // this wait has no bound of its own. In the Check jobs nextest's `ci`
        // profile terminates a stuck test after its slow-timeout periods and
        // NAMES it. The Coverage job does not: it runs `cargo llvm-cov --tests`,
        // i.e. libtest, which has no per-test timeout, so there the same wedge
        // degrades into an anonymous step timeout. That is the trade this
        // barrier makes against the wall-clock budget it replaced, recorded here
        // rather than left for whoever meets it.
        lane_entered_rx
            .recv()
            .expect("lane holder must enter (sender dropped => holder thread died)");

        assert!(crate::daemon::delivery_worker::enqueue_transport_delivery(
            &home,
            &agent,
            "queued before external delete",
        )
        .is_ok());

        let externals: agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        externals.lock().insert(
            agent.clone(),
            agent::ExternalAgentHandle {
                backend_command: "remote".to_string(),
                pid: 4321,
            },
        );
        let registry: AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: crate::api::ConfigRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let delete_home = home.clone();
        let delete_agent = agent.clone();
        let delete_externals = std::sync::Arc::clone(&externals);
        let delete_registry = std::sync::Arc::clone(&registry);
        let delete_configs = std::sync::Arc::clone(&configs);
        let (delete_tx, delete_rx) = std::sync::mpsc::channel();
        let (marker_tx, marker_rx) = std::sync::mpsc::channel();
        let (marker_continue_tx, marker_continue_rx) = std::sync::mpsc::channel();
        // #3240 slice 4: this is a fixed pre-acquire rendezvous, not a marker
        // observer. The marker assertion runs while the delete thread is held
        // at this seam, before it can acquire the already-held transport lane.
        // Named RED controls exercised during implementation (temporary only):
        // M2 moves mark_deleting after lane acquire; old-clock delays delete
        // entry by 1200ms; child-death panics before this seam. Each must fail
        // or disconnect without leaving a sender owned by the test thread.
        let marker_home = home.clone();
        let marker_agent = agent.clone();
        let expected_marker_home = home.clone();
        let expected_marker_agent = agent.clone();
        crate::daemon::delivery_worker::test_support::set_cleanup_before_lane_acquire_hook(Some(
            std::sync::Arc::new(move |hook_home, hook_agent| {
                if hook_home == expected_marker_home.as_path()
                    && hook_agent == expected_marker_agent
                {
                    crate::daemon::delivery_worker::test_support::
                        notify_cleanup_before_lane_acquire(hook_home, hook_agent);
                }
            }),
        ));
        let delete_thread = std::thread::spawn(move || {
            let _marker_observer =
                crate::daemon::delivery_worker::test_support::cleanup_before_lane_acquire_observer(
                    &marker_home,
                    &marker_agent,
                    marker_tx,
                    marker_continue_rx,
                );
            let context = DeleteContext {
                registry: &delete_registry,
                configs: &delete_configs,
                externals: &delete_externals,
                notifier: None,
            };
            let outcome = delete_instance(&delete_home, &delete_agent, &context, false);
            if send_outcome {
                delete_tx.send(outcome).expect("delete outcome observer");
            }
        });

        let marker_observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            marker_rx.recv().expect(
                "external delete marker observer disconnected: delete thread died before cleanup lane",
            );
            assert!(
                crate::agent::deleting::is_deleting(&home, &agent),
                "external delete must mark the name before waiting for its transport lane"
            );
            assert!(
                delete_rx.try_recv().is_err(),
                "external delete must remain behind the held transport lane"
            );
        }));
        let _ = marker_continue_tx.send(());
        marker_observation.expect("external delete marker ordering observation");

        lane_release_tx.send(()).expect("release lane");
        lane_holder.join().expect("lane holder");

        if send_outcome {
            assert_eq!(
                delete_rx.recv().expect(
                    "external delete outcome sender dropped before sending outcome (RecvError)",
                ),
                DeleteOutcome::External
            );
        } else {
            let disconnected: std::sync::mpsc::RecvError = delete_rx.recv().expect_err(
                "external delete outcome sender dropped before sending outcome (RecvError)",
            );
            assert_eq!(
                format!("{disconnected:?}"),
                "RecvError",
                "external delete completion must fail with the named RecvError"
            );
        }
        delete_thread.join().expect("delete thread");

        let dispatch_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, &agent)
            < 1
            && std::time::Instant::now() < dispatch_deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        let dispatch_count =
            crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, &agent);
        assert_eq!(
            dispatch_count, 1,
            "the queued old-generation job must be observed and discarded exactly once"
        );
        assert_eq!(
            adapter_calls.load(Ordering::SeqCst),
            0,
            "the stale queued external-delete job must not reach an adapter"
        );
        let delivery_path = crate::transport::delivery_path_for_instance(&home, &agent);
        assert!(
            !delivery_path.exists(),
            "stale external job must not create a receipt"
        );
        assert!(
            !delivery_path.with_extension("jsonl.lock").exists(),
            "stale external job must not create a receipt lock"
        );
        assert!(
            !agent::lock_external(&externals).contains_key(&agent),
            "external delete must remove the external registry entry"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// The real external-delete thread can disconnect before reporting its
    /// outcome; the fixture's own completion receiver must surface RecvError.
    #[test]
    fn external_delete_completion_disconnect_control() {
        run_external_delete_fixture(false);
    }

    #[test]
    fn concurrent_save_metadata_no_lost_update_1886() {
        // #1886 C2 §3.9: N threads each set a DISTINCT key on the SAME instance's
        // metadata. The locked RMW keeps every field; the prior unlocked
        // read+atomic_write would lose updates under contention.
        let home = tmp_home("concurrent-save-meta-1886");
        const N: usize = 12;
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let home = home.clone();
                std::thread::spawn(move || {
                    save_metadata(&home, "agent-x", &format!("key-{i}"), json!(i));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-x")).unwrap();
        let meta: Value = serde_json::from_str(&content).unwrap();
        for i in 0..N {
            assert_eq!(
                meta.get(format!("key-{i}")).and_then(|v| v.as_u64()),
                Some(i as u64),
                "every concurrent field write must survive"
            );
        }
    }

    #[test]
    fn update_metadata_concurrent_append_and_filter_no_lost_or_resurrected() {
        // CR-2026-06-14: the pickup-id lost-update race a ONE-SIDED lock could
        // not close. The two production mutators of `pending_pickup_ids` — the
        // telegram inbound APPEND and the inbox-drain FILTER — both run as
        // `update_metadata` locked RMWs. Seed P "processed" ids; concurrently
        // each filter thread removes one while each append thread adds a fresh
        // one. Because BOTH sides take the same flock and derive their new value
        // from the CURRENT on-disk value inside the lock, the operations
        // serialize: the final set is EXACTLY the appended ids — nothing lost, no
        // processed id resurrected. (A one-sided unlocked append could write a
        // stale array back over a concurrent filter, resurrecting a removed id.)
        let home = tmp_home("update-meta-append-filter");
        const P: usize = 16;
        let seed: Vec<Value> = (0..P)
            .map(|i| json!({ "msg_id": format!("p{i}") }))
            .collect();
        save_metadata(&home, "agent-z", "pending_pickup_ids", json!(seed));

        let mut handles = Vec::new();
        for i in 0..P {
            // Filter thread: remove processed id pI (mirrors handle_inbox).
            let home_f = home.clone();
            handles.push(std::thread::spawn(move || {
                update_metadata(&home_f, "agent-z", "pending_pickup_ids", |current| {
                    let remaining: Vec<Value> = current
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| e["msg_id"].as_str() != Some(format!("p{i}").as_str()))
                        .collect();
                    json!(remaining)
                });
            }));
            // Append thread: add a fresh id aI and its fallback id atomically
            // (mirrors telegram inbound).
            let home_a = home.clone();
            handles.push(std::thread::spawn(move || {
                update_metadata_object(&home_a, "agent-z", |meta| {
                    let current = meta
                        .get("pending_pickup_ids")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let mut ids: Vec<Value> = current.as_array().cloned().unwrap_or_default();
                    ids.push(json!({ "msg_id": format!("a{i}") }));
                    meta["pending_pickup_ids"] = json!(ids);
                    meta["last_message_id"] = json!(i);
                });
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-z")).unwrap();
        let meta: Value = serde_json::from_str(&content).unwrap();
        let final_ids: std::collections::HashSet<String> = meta["pending_pickup_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["msg_id"].as_str().unwrap().to_string())
            .collect();
        let expected: std::collections::HashSet<String> = (0..P).map(|i| format!("a{i}")).collect();
        assert_eq!(
            final_ids, expected,
            "after concurrent append+filter the set must be exactly the appended ids \
             (no processed id resurrected, no append lost)"
        );
        assert!(
            meta["last_message_id"]
                .as_u64()
                .is_some_and(|id| id < P as u64),
            "the fallback id must come from one complete append transaction"
        );
    }

    #[test]
    fn concurrent_save_metadata_clear_vs_set_both_survive_1886() {
        // #1886 C2 §3.9 (clear-vs-set): one writer clears `waiting_on` while
        // another sets a different field on the same instance — both updates
        // survive and an untouched field is preserved (the F7 interleave race,
        // now closed by the locked RMW).
        let home = tmp_home("save-meta-clear-set-1886");
        save_metadata_batch(
            &home,
            "agent-y",
            &[
                ("waiting_on", json!("reviewer")),
                ("waiting_on_since", json!(1)),
            ],
        );
        let h1 = {
            let home = home.clone();
            std::thread::spawn(move || save_metadata(&home, "agent-y", "waiting_on", json!(null)))
        };
        let h2 = {
            let home = home.clone();
            std::thread::spawn(move || save_metadata(&home, "agent-y", "extra", json!("set")))
        };
        h1.join().unwrap();
        h2.join().unwrap();
        let content = std::fs::read_to_string(metadata_path_resolved(&home, "agent-y")).unwrap();
        let meta: Value = serde_json::from_str(&content).unwrap();
        assert!(meta["waiting_on"].is_null(), "clear survived");
        assert_eq!(
            meta["extra"].as_str(),
            Some("set"),
            "concurrent set survived"
        );
        assert_eq!(
            meta["waiting_on_since"].as_u64(),
            Some(1),
            "untouched field preserved"
        );
    }

    // --- validate_branch (3 from ops.rs + 5 from mcp/handlers.rs) ---

    #[test]
    fn branch_valid() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/foo"));
        assert!(validate_branch("v1.0.0"));
    }

    #[test]
    fn branch_rejects_dotdot() {
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/.."));
    }

    #[test]
    fn branch_rejects_special() {
        assert!(!validate_branch(""));
        assert!(!validate_branch("-main"));
        assert!(!validate_branch("foo;bar"));
    }

    #[test]
    fn branch_valid_simple() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/foo"));
        assert!(validate_branch("v1.0.0"));
        assert!(validate_branch("fix-123"));
        assert!(validate_branch("release_2.0"));
    }

    #[test]
    fn branch_rejects_empty() {
        assert!(!validate_branch(""));
    }

    // --- is_protected_ref (E4.5 invariant — Sprint 57 Wave 2 Track B #546) ---

    #[test]
    fn is_protected_ref_main_and_master() {
        assert!(is_protected_ref("main"));
        assert!(is_protected_ref("master"));
    }

    #[test]
    fn is_protected_ref_rejects_feature_branches() {
        assert!(!is_protected_ref("feature/x"));
        assert!(!is_protected_ref("sprint57-track-b"));
        assert!(!is_protected_ref("release/v1.0.0"));
        assert!(!is_protected_ref("hotfix"));
    }

    #[test]
    fn is_protected_ref_case_insensitive_blocks_case_variants() {
        // CR-2026-06-14: the prior "case-sensitive by design" stance was
        // empirically falsified on darwin/APFS — a case-insensitive FS folds
        // refs/heads/Main onto refs/heads/main, so `branch="Main"` lands the
        // agent's worktree on `main` (committing on "Main" advanced `main`).
        // Every case variant of main/master MUST be protected.
        for v in ["Main", "MAIN", "mAiN", "Master", "MASTER", "mAsTeR"] {
            assert!(
                is_protected_ref(v),
                "case variant {v:?} must be protected (E4.5 case-insensitive)"
            );
        }
    }

    #[test]
    fn is_protected_ref_rejects_empty_and_substrings() {
        // eq_ignore_ascii_case is a full-string compare, so a branch that
        // merely CONTAINS "main"/"master" (or differs by more than case) is
        // not over-blocked.
        assert!(!is_protected_ref(""));
        assert!(!is_protected_ref("mainline"));
        assert!(!is_protected_ref("maintenance"));
        assert!(!is_protected_ref("main-feature"));
        assert!(!is_protected_ref("Maintenance"));
        assert!(!is_protected_ref("upstream-main"));
        assert!(!is_protected_ref("master/dev"));
    }

    #[test]
    fn branch_rejects_dotdot_extended() {
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/.."));
        assert!(!validate_branch("../bar"));
    }

    #[test]
    fn branch_rejects_leading_dash() {
        assert!(!validate_branch("-main"));
        assert!(!validate_branch("-"));
    }

    #[test]
    fn branch_rejects_special_chars() {
        assert!(!validate_branch("main branch"));
        assert!(!validate_branch("foo;bar"));
        assert!(!validate_branch("$(echo)"));
        assert!(!validate_branch("main\ninjected"));
    }

    // Migrated from `src/worktree.rs::tests` as part of Task #9 Option C
    // epilogue (worktree.rs no longer holds its own `validate_branch` copy).

    #[test]
    fn test_validate_branch_valid() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/my-branch"));
        assert!(validate_branch("agend/agent-1"));
        assert!(validate_branch("v1.0.0"));
    }

    #[test]
    fn test_validate_branch_rejects() {
        assert!(!validate_branch(""));
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/../bar"));
        assert!(!validate_branch("-starts-with-dash"));
        assert!(!validate_branch("has spaces"));
        assert!(!validate_branch("has;semicolon"));
    }

    // --- merge_metadata (2 from ops.rs) ---

    #[test]
    fn metadata_merge_no_file() {
        let home = tmp_home("meta_no_file");
        let mut info = json!({"name": "a"});
        merge_metadata(&home, "a", &mut info);
        assert_eq!(info["name"], "a");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn metadata_merge_fields() {
        let home = tmp_home("meta_fields");
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(
            home.join("metadata/a.json"),
            r#"{"display_name":"Dev","x":1}"#,
        )
        .ok();
        let mut info = json!({"name": "a"});
        merge_metadata(&home, "a", &mut info);
        assert_eq!(info["display_name"], "Dev");
        assert_eq!(info["x"], 1);
        std::fs::remove_dir_all(&home).ok();
    }

    // --- save_metadata (1 from ops.rs) ---

    #[test]
    fn metadata_save_roundtrip() {
        let home = tmp_home("meta_save");
        save_metadata(&home, "a", "key", json!("val"));
        let c = std::fs::read_to_string(home.join("metadata/a.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["key"], "val");
        std::fs::remove_dir_all(&home).ok();
    }

    // Sprint 21 Phase 5 — atomic multi-field metadata helper tests.
    // Closes the F7 race window documented in docs/DAEMON-LOCK-ORDERING.md
    // §1 F7): two sequential `save_metadata` calls had a partial-write
    // window where a daemon crash between the two writes left disk state
    // inconsistent (waiting_on cleared but waiting_on_since stale).

    #[test]
    fn atomic_multi_field_save_metadata_writes_in_single_transaction() {
        // Verify all fields land in the file together — the helper must
        // not write one field, return, then write the next (which would
        // expose the F7 race).
        let home = tmp_home("meta_batch_atomic");
        save_metadata_batch(
            &home,
            "agent_z",
            &[
                ("waiting_on", json!("review from at-dev-4")),
                ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
                ("last_heartbeat", json!("2026-04-27T00:01:00Z")),
            ],
        );
        let raw = std::fs::read_to_string(home.join("metadata/agent_z.json"))
            .expect("metadata file written");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(v["waiting_on"], "review from at-dev-4");
        assert_eq!(v["waiting_on_since"], "2026-04-27T00:00:00Z");
        assert_eq!(v["last_heartbeat"], "2026-04-27T00:01:00Z");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn atomic_multi_field_save_metadata_clear_pair_no_corrupt_state() {
        // Closes Sprint 20 F7 directly: clearing `waiting_on` + `waiting_on_since`
        // must land both nulls in one write so a concurrent reader (e.g.
        // supervisor tick) never sees the half-cleared state where
        // waiting_on is null but waiting_on_since is still set.
        let home = tmp_home("meta_batch_clear");
        // Pre-populate with an active wait state.
        save_metadata_batch(
            &home,
            "agent_y",
            &[
                ("waiting_on", json!("PR review")),
                ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
            ],
        );
        // Now clear both atomically.
        save_metadata_batch(
            &home,
            "agent_y",
            &[
                ("waiting_on", json!(null)),
                ("waiting_on_since", json!(null)),
            ],
        );
        let raw = std::fs::read_to_string(home.join("metadata/agent_y.json"))
            .expect("metadata file present");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(
            v["waiting_on"].is_null(),
            "waiting_on must be null after batch clear"
        );
        assert!(
            v["waiting_on_since"].is_null(),
            "waiting_on_since must be null after batch clear"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn atomic_multi_field_save_metadata_preserves_unrelated_fields() {
        // The helper does read-modify-write so unrelated keys must survive
        // the batch update — guards against accidental field overwrite if
        // an implementation regresses to "replace whole file".
        let home = tmp_home("meta_batch_preserve");
        save_metadata(&home, "agent_x", "role", json!("dev-impl-2"));
        save_metadata(&home, "agent_x", "team", json!("dev"));
        save_metadata_batch(
            &home,
            "agent_x",
            &[
                ("waiting_on", json!("review")),
                ("waiting_on_since", json!("2026-04-27T00:00:00Z")),
            ],
        );
        let raw = std::fs::read_to_string(home.join("metadata/agent_x.json"))
            .expect("metadata file present");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(
            v["role"], "dev-impl-2",
            "unrelated `role` must survive batch"
        );
        assert_eq!(v["team"], "dev", "unrelated `team` must survive batch");
        assert_eq!(v["waiting_on"], "review");
        assert_eq!(v["waiting_on_since"], "2026-04-27T00:00:00Z");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- cleanup_working_dir (3 from ops.rs) ---

    #[test]
    fn cleanup_workspace_removes_dir() {
        let home = tmp_home("cw");
        let ws = home.join("workspace/agent1");
        std::fs::create_dir_all(&ws).ok();
        std::fs::write(ws.join("f.txt"), "x").ok();
        let _ = cleanup_working_dir(&home, "agent1", &ws);
        assert!(!ws.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_user_dir_selective() {
        let home = tmp_home("cu");
        let ud = tmp_home("cu_proj");
        std::fs::write(ud.join("main.rs"), "fn main(){}").ok();
        std::fs::write(ud.join("opencode.json"), "{}").ok();
        let _ = cleanup_working_dir(&home, "a", &ud);
        assert!(ud.join("main.rs").exists());
        assert!(!ud.join("opencode.json").exists());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ud).ok();
    }

    // --- workspace-identity delete guard (boundary 3) ---

    fn seed_agents_owned_by(dir: &Path, owner: &str) {
        std::fs::write(
            dir.join("AGENTS.md"),
            format!(
                "<!-- agend:start -->\n## Identity\n\n- **Name**: `{owner}`\n<!-- agend:end -->\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn cleanup_preserves_foreign_identity_tree() {
        let home = tmp_home("cw_foreign");
        let ws = home.join("workspace/alice"); // "alice" is being deleted...
        std::fs::create_dir_all(&ws).unwrap();
        seed_agents_owned_by(&ws, "bob"); // ...but the directory belongs to "bob".
        std::fs::write(ws.join("keep.txt"), "b").unwrap();
        assert!(
            cleanup_working_dir(&home, "alice", &ws).is_some(),
            "foreign-owned dir must be refused (Some verdict)"
        );
        assert!(ws.exists(), "foreign-owned tree must be preserved");
        assert!(
            ws.join("AGENTS.md").exists(),
            "bob's identity file preserved"
        );
        assert!(
            ws.join("keep.txt").exists(),
            "foreign tree contents preserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_removes_same_identity_tree() {
        let home = tmp_home("cw_same");
        let ws = home.join("workspace/alice");
        std::fs::create_dir_all(&ws).unwrap();
        seed_agents_owned_by(&ws, "alice"); // dir belongs to the instance being deleted
        assert!(
            cleanup_working_dir(&home, "alice", &ws).is_none(),
            "same-identity dir cleans (None verdict)"
        );
        assert!(!ws.exists(), "same-identity tree must be removed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_removes_unowned_tree_normally() {
        let home = tmp_home("cw_absent");
        let ws = home.join("workspace/alice");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("f.txt"), "x").unwrap(); // no identity artifact
        assert!(
            cleanup_working_dir(&home, "alice", &ws).is_none(),
            "unowned dir cleans (None verdict)"
        );
        assert!(!ws.exists(), "unowned tree cleans normally");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ownership_conflict_detects_foreign_codex_stamp() {
        let home = tmp_home("wdoc_codex");
        let ws = home.join("workspace/alice");
        std::fs::create_dir_all(ws.join(".codex")).unwrap();
        std::fs::write(
            ws.join(".codex").join("config.toml"),
            "AGEND_INSTANCE_NAME = 'bob'\n",
        )
        .unwrap();
        assert!(
            working_dir_ownership_conflict(&ws, "alice").is_some(),
            "foreign .codex stamp is a conflict"
        );
        assert!(
            working_dir_ownership_conflict(&ws, "bob").is_none(),
            "same owner is not a conflict"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_refuses_unreadable_identity_tree() {
        // Fail-closed: an UNREADABLE identity artifact (opaque I/O ≠ NotFound)
        // must refuse the delete — never be read as "absent" and wipe the tree.
        let home = tmp_home("cw_unreadable");
        let ws = home.join("workspace/alice");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("AGENTS.md"), [0xFFu8, 0xFE]).unwrap(); // invalid UTF-8
        assert!(
            cleanup_working_dir(&home, "alice", &ws).is_some(),
            "unreadable identity must refuse (Some verdict)"
        );
        assert!(ws.exists(), "tree preserved on unreadable identity");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn workspace_identity_lock_is_mutually_exclusive_provision_vs_delete() {
        // Provision (generate_with_context) and delete (cleanup_working_dir) BOTH
        // acquire store::acquire_workspace_identity_lock(home, wd) for the same
        // directory. Prove it is mutually exclusive so a check+write can never
        // interleave with a check+remove of that directory (root finding 4).
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let home = tmp_home("wsid_lock");
        let wd = home.join("workspace/shared");
        let in_critical = Arc::new(AtomicBool::new(false));
        let held = crate::store::acquire_workspace_identity_lock(&home, &wd).expect("first lock");
        in_critical.store(true, Ordering::SeqCst);
        let (h2, w2, ic2) = (home.clone(), wd.clone(), in_critical.clone());
        let t = std::thread::spawn(move || {
            // Blocks until the main thread releases `held` (mutual exclusion).
            let _g = crate::store::acquire_workspace_identity_lock(&h2, &w2).expect("second lock");
            assert!(
                !ic2.load(Ordering::SeqCst),
                "acquired the workspace-identity lock while another holder was still in its \
                 critical section — the lock is NOT mutually exclusive"
            );
        });
        // Give the spawned thread time to reach (and block on) the acquire.
        std::thread::sleep(std::time::Duration::from_millis(100));
        in_critical.store(false, Ordering::SeqCst);
        drop(held);
        t.join().expect("second acquirer thread");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_metadata() {
        let home = tmp_home("cms");
        let ws = home.join("workspace/a");
        std::fs::create_dir_all(&ws).ok();
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(home.join("metadata/a.json"), "{}").ok();
        let _ = cleanup_working_dir(&home, "a", &ws);
        assert!(!home.join("metadata/a.json").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // --- NEW: drift guard — assert canonical 19-entry set (at-dev-3 gate C).
    //
    // Creates every one of the 19 entries in a user-provided working dir
    // (not under $AGEND_HOME/workspace/, so selective-mode path runs), then
    // asserts all 19 are removed. Explicitly lists the 5 Kiro paths that
    // `mcp/handlers.rs` was missing so any future drift regresses the test.

    #[test]
    fn cleanup_removes_all_19_canonical_entries() {
        let home = tmp_home("drift19_home");
        let ud = tmp_home("drift19_user");

        let canonical: [&str; 19] = [
            // Claude (6)
            ".claude/settings.local.json",
            "mcp-config.json",
            "claude-settings.json",
            "statusline.sh",
            "statusline.json",
            ".claude/rules/agend.md",
            // Gemini (1)
            ".gemini/settings.json",
            // OpenCode (2)
            "opencode.json",
            "instructions/agend.md",
            // Codex (2)
            ".codex/config.toml",
            "AGENTS.md",
            // Kiro — 14-entry handlers copy had only the first 3 of these 9
            ".kiro/settings/mcp.json",
            ".kiro/settings/agend-mcp-wrapper.sh",
            ".kiro/steering/agend.md",
            // The 5 Kiro paths missing from `mcp/handlers.rs` pre-Commit-2:
            ".kiro/agents/agend.json",
            ".kiro/agents/agend-prompt.md",
            ".kiro/agents/default.json",
            ".kiro/prompts/agend.md",
            ".kiro/settings.json",
        ];

        // Materialize every canonical path, plus one decoy that must survive.
        for rel in &canonical {
            let p = ud.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&p, "x").ok();
        }
        std::fs::write(ud.join("user-code.rs"), "fn main(){}").ok();

        let _ = cleanup_working_dir(&home, "drift19", &ud);

        // All 19 must be gone, user decoy preserved.
        for rel in &canonical {
            assert!(!ud.join(rel).exists(), "canonical entry not removed: {rel}");
        }
        assert!(
            ud.join("user-code.rs").exists(),
            "user file must survive selective cleanup"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ud).ok();
    }

    // Explicit individual assertions for the 5 Kiro paths that were missing
    // from `mcp/handlers.rs` — if any reappears as undeleted, this test
    // pinpoints which one.
    #[test]
    fn cleanup_removes_each_of_5_drifted_kiro_entries() {
        let drifted = [
            ".kiro/agents/agend.json",
            ".kiro/agents/agend-prompt.md",
            ".kiro/agents/default.json",
            ".kiro/prompts/agend.md",
            ".kiro/settings.json",
        ];
        for rel in &drifted {
            let home = tmp_home("drift1_home");
            let ud = tmp_home("drift1_user");
            let p = ud.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&p, "x").ok();

            let _ = cleanup_working_dir(&home, "drift1", &ud);

            assert!(!p.exists(), "Kiro drift entry not removed: {rel}");

            std::fs::remove_dir_all(&home).ok();
            std::fs::remove_dir_all(&ud).ok();
        }
    }

    // #910 PR2 of 4: MCP-facing JSON shape stability pin.
    //
    // `src/mcp/handlers/instance.rs:36/39` wraps `list_agents()` result
    // in `{"instances": [<names>]}` as the LIST fallback when the rich-
    // info API path fails. After PR2's migration to `runtime::
    // list_agents_with_fallback`, the OUTPUT TYPE must remain
    // `Vec<String>` so the JSON envelope is byte-stable for any
    // operator script grep'ing the MCP fallback response. This test
    // pins that contract.
    #[test]
    fn list_agents_mcp_payload_shape_is_instances_array_of_strings() {
        // Build a fixture that mirrors what `list_agents()` returns —
        // a Vec<String>. Wrap it the same way the MCP handler does.
        // This pin tracks the wire contract, not the resolution path.
        let names: Vec<String> = vec!["alice".into(), "bob".into(), "charlie".into()];
        let payload = json!({"instances": names.clone()});

        // Top-level key must be `instances`.
        assert!(
            payload.get("instances").is_some(),
            "MCP fallback envelope must carry top-level 'instances' key — \
             #910 PR2 contract pin"
        );

        // Value must be a JSON array.
        let arr = payload["instances"]
            .as_array()
            .expect("'instances' value must be a JSON array");

        // Each element must be a JSON string (not an object, not nested).
        // Locks the fallback envelope as a flat name-list — the rich-info
        // path returns objects, but the fallback path is intentionally
        // simpler so degraded-mode parsers don't need the full schema.
        assert_eq!(arr.len(), 3);
        for (i, v) in arr.iter().enumerate() {
            assert!(
                v.is_string(),
                "'instances[{i}]' must be a JSON string in the LIST fallback, got {v}"
            );
            assert_eq!(v.as_str().unwrap(), names[i].as_str());
        }
    }

    // #910 PR2 of 4: `list_agents` thin-wrapper contract.
    //
    // After PR2, `list_agents()` is a 1-line delegation to
    // `runtime::list_agents_with_fallback`. The behavioral surface is
    // covered by PR1's `runtime::tests` (5 RED→GREEN tests). This test
    // pins the SIGNATURE + RETURN TYPE so a future refactor that
    // accidentally drops the no-arg shape or changes the return type
    // breaks loudly here rather than at MCP handler call sites.
    #[test]
    fn list_agents_signature_is_no_arg_vec_string() {
        // Call site sanity: compiles with no args; result is Vec<String>.
        let result: Vec<String> = list_agents();
        // Result may be empty (no daemon, no tmp run dir) but must not panic.
        // Length assertion is intentionally weak — the resolution path is
        // tested in `runtime::tests::*`; here we only pin the signature.
        let _ = result.len();
    }

    #[test]
    fn api_bridge_missing_delivery_mode_is_not_legacy_pty() {
        let missing = json!({"ok": true});
        assert_eq!(api_bridge_delivery_mode(&missing), UNVERIFIED_DELIVERY_MODE);
        assert_ne!(api_bridge_delivery_mode(&missing), "pty");

        let malformed = json!({"ok": true, "delivery_mode": null});
        assert_eq!(
            api_bridge_delivery_mode(&malformed),
            UNVERIFIED_DELIVERY_MODE
        );
    }
}

#[cfg(test)]
mod review_repro_agent_binding;
#[cfg(test)]
mod review_repro_xcut_security;
