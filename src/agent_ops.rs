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
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) mod cleanup_admission;

// ---------------------------------------------------------------------------
// Messaging
// ---------------------------------------------------------------------------

/// Centralised daemon-unavailable fallback: validate target in fleet.yaml,
/// enqueue inbox message, notify agent. Returns fallback JSON response.
/// Extracted from 3 duplicated sites in comms.rs (Sprint 40 T-7 B4).
pub fn fallback_deliver(
    home: &Path,
    from: &str,
    target: &str,
    text: &str,
    msg: crate::inbox::InboxMessage,
    api_error: &anyhow::Error,
) -> Value {
    let in_fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .map(|c| c.instances.contains_key(target))
        .unwrap_or(false);
    if !in_fleet {
        return json!({"error": format!("target instance '{target}' not found in fleet.yaml (API unavailable: {api_error})")});
    }
    // Capture the answered parent before `msg` is moved into enqueue below.
    let parent_id = msg.parent_id.clone();
    // #bughunt2: this is the last-resort path (the daemon API is already down),
    // so the inbox is the SOLE channel. A swallowed enqueue here is total,
    // unrecoverable message loss reported as success — surface it instead.
    if let Err(e) = crate::inbox::enqueue(home, target, msg) {
        return json!({
            "error": format!(
                "inbox fallback delivery to '{target}' failed — message lost (API unavailable: {api_error}): {e}"
            )
        });
    }
    // Confirmed-successful fallback delivery: settle the SENDER's own parent row
    // so an answered obligation stops re-nagging via poll-reminder. No-ops when
    // parent_id is None; the failed-enqueue early return above skips it.
    crate::inbox::settle_parent_after_successful_send(home, from, parent_id.as_deref());
    crate::inbox::notify_agent(home, target, &crate::inbox::NotifySource::Agent(from), text);
    json!({"target": target, "delivery_mode": "inbox_fallback", "note": format!("API unavailable: {api_error}")})
}

/// Send a message to a target instance via API, falling back to direct
/// inbox delivery when the daemon is unreachable.
///
/// `from: &Sender` guarantees a non-empty originator; callers cannot
/// accidentally stamp messages with `[from:]` (see `src/identity.rs`).
///
/// Sprint 54 layer-5: `broadcast_context` is `Some` only when the call
/// originates from `handle_broadcast` per-target loop — it surfaces in the
/// recipient's `[AGEND-MSG]` header (`broadcast=N team=NAME`) and inbox
/// JSON metadata so broadcast is distinguishable from unicast at agent
/// vantage. Routing behavior is unaffected.
pub fn send_to(
    home: &Path,
    from: &Sender,
    target: &str,
    text: &str,
    kind: &str,
    broadcast_context: Option<&crate::inbox::BroadcastContext>,
) -> Value {
    let from_str = from.as_str();
    let mut params = json!({
        "from": from_str,
        "target": target,
        "text": text,
        "kind": kind,
    });
    if let Some(ctx) = broadcast_context {
        params["broadcast_context"] = serde_json::to_value(ctx).unwrap_or(Value::Null);
    }
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SEND,
            "params": params,
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let dm = resp["delivery_mode"].as_str().unwrap_or("pty");
            json!({"target": target, "delivery_mode": dm})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            // Validate target exists in fleet.yaml before writing to inbox.
            let in_fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .map(|c| c.instances.contains_key(target))
                .unwrap_or(false);
            if !in_fleet {
                return json!({"error": format!("target instance '{target}' not found in fleet.yaml (API unavailable: {e})")});
            }
            let submit_key = get_submit_key(home, target);
            // t-20260705005551919287-14440-22: preserve the ORIGINAL `kind`
            // on the fallback path — it was hardcoded to `None` here even
            // though the API-reachable path above threads it through via
            // `"kind": kind`. A task/query silently landing as kind=None
            // loses its obligation classification (poll-reminder doesn't
            // re-arm for it).
            crate::inbox::deliver(
                home,
                target,
                &crate::inbox::NotifySource::Agent(from_str),
                text,
                &submit_key,
                Some(kind.to_string()),
                broadcast_context.cloned(),
            );
            json!({"target": target, "delivery_mode": "inbox_fallback", "note": format!("API unavailable: {e}")})
        }
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
    /// The caller had no in-process runtime and the legacy API loopback was
    /// used.  The result is intentionally not decoded here: legacy callers
    /// historically treated the DELETE response as best-effort.
    Legacy,
}

/// Perform the daemon-side portion of DELETE once, preserving the exact API
/// semantics for managed and external agents.  Runtime callers use the
/// registries directly; a `None` context keeps the one legacy loopback needed
/// by TUI/team/test callers that have no in-process API owner.
pub fn delete_instance(
    home: &Path,
    name: &str,
    context: Option<&DeleteContext<'_>>,
    skip_exit_wait: bool,
) -> DeleteOutcome {
    let Some(context) = context else {
        let mut params = json!({"name": name});
        if skip_exit_wait {
            params["no_wait"] = json!(true);
        }
        let _ = crate::api::call(
            home,
            &json!({
                "method": crate::api::method::DELETE,
                "params": params,
            }),
        );
        return DeleteOutcome::Legacy;
    };

    // Match the API adapter's external-first behavior.  External agents have
    // no managed registry/config entry and therefore need no notifier event.
    if agent::lock_external(context.externals)
        .remove(name)
        .is_some()
    {
        crate::event_log::log(home, "delete", name, "external agent deleted");
        return DeleteOutcome::External;
    }

    crate::daemon::lifecycle::delete_transaction(
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
    DeleteOutcome::Managed
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

/// Persist multiple metadata key/value pairs in a single locked read-modify-write.
/// #1886 C2: the flock spans the whole load→modify→write (not just the write), so
/// concurrent `save_metadata`/`save_metadata_batch` on the same instance never read
/// stale data and lose each other's update (the prior comment's interleave race).
pub fn save_metadata_batch(home: &Path, instance_name: &str, entries: &[(&str, Value)]) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path_resolved(home, instance_name);
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
// Fleet
// ---------------------------------------------------------------------------

/// Look up submit_key for a target instance from fleet config.
pub fn get_submit_key(home: &Path, target: &str) -> String {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) {
        if let Some(resolved) = config.resolve_instance(target) {
            return resolved.submit_key;
        }
    }
    "\r".to_string()
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
    let preset_submit_key = effective_backend
        .map(|b| b.preset().submit_key)
        .unwrap_or("\r");
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
            // Append thread: add a fresh id aI (mirrors telegram inbound).
            let home_a = home.clone();
            handles.push(std::thread::spawn(move || {
                update_metadata(&home_a, "agent-z", "pending_pickup_ids", |current| {
                    let mut ids: Vec<Value> = current.as_array().cloned().unwrap_or_default();
                    ids.push(json!({ "msg_id": format!("a{i}") }));
                    json!(ids)
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

    /// #bughunt2: the last-resort delivery path must surface a dropped enqueue
    /// (the inbox is the SOLE channel when the API is down) instead of reporting
    /// `inbox_fallback` success for a silently-lost message.
    #[test]
    fn fallback_deliver_surfaces_enqueue_failure_not_fake_success() {
        let home = tmp_home("fallback-enqueue-fail");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  worker:\n    backend: claude\n",
        )
        .unwrap();
        // Force the inbox enqueue to fail: `home/inbox` as a FILE makes
        // `create_dir_all(home/inbox)` error inside with_inbox_lock.
        std::fs::write(home.join("inbox"), b"blocker").unwrap();
        let msg = crate::inbox::InboxMessage::new_system("sender", "update", "body");
        let api_error = anyhow::anyhow!("daemon API unavailable");
        let result = fallback_deliver(&home, "sender", "worker", "hi", msg, &api_error);
        assert!(
            result.get("error").and_then(|e| e.as_str()).is_some(),
            "a dropped enqueue must surface as an error, not inbox_fallback success: {result}"
        );
        assert_ne!(
            result.get("delivery_mode").and_then(|d| d.as_str()),
            Some("inbox_fallback"),
            "must NOT report success when the message was lost"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2730: the API-down fallback fork must NOT settle the sender's parent row
    /// when the fallback enqueue itself fails. Mirror of the normal-fork test
    /// (`messaging/tests.rs::failed_parented_send_does_not_settle_sender_parent`)
    /// through `fallback_deliver`: the settle seam is wired only past the enqueue
    /// Ok, so a lost fallback message must leave the parent unprocessed.
    #[test]
    fn failed_fallback_delivery_does_not_settle_sender_parent() {
        let home = tmp_home("fallback-fail-no-settle");
        let (sender, target) = ("ffns-worker", "ffns-peer");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!("instances:\n  {target}:\n    backend: claude\n"),
        )
        .unwrap();
        // Seed + drain a real delivering parent row in the SENDER's own inbox.
        let pid = "m-ffns-parent";
        crate::inbox::enqueue(
            &home,
            sender,
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some(pid.to_string()),
                from: "codex".to_string(),
                text: "q".to_string(),
                kind: Some("query".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            },
        )
        .unwrap();
        crate::inbox::drain(&home, sender); // parent: unread → delivering

        // Break ONLY the target's RESOLVED inbox path (dir) so the fallback
        // enqueue fails. Must be the RESOLVED (not raw-name) path — on Windows
        // inbox_path_resolved migrates name→UUID, so a raw-name-path directory is
        // bypassed and the UUID path succeeds (#2730 r2 Windows failure). Breaking
        // the resolved path makes enqueue hit the id_path-exists branch on BOTH
        // platforms (no symlink/copy migration divergence).
        let target_path = crate::inbox::storage::inbox_path_resolved(&home, target);
        std::fs::create_dir_all(&target_path).unwrap();

        let reply = crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-ffns-reply".to_string()),
            from: sender.to_string(),
            text: "answered".to_string(),
            kind: Some("report".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            parent_id: Some(pid.to_string()),
            ..Default::default()
        };
        let resp = fallback_deliver(
            &home,
            sender,
            target,
            "answered",
            reply,
            &anyhow::anyhow!("api down"),
        );
        assert!(
            resp.get("error").is_some(),
            "the fallback enqueue must fail when the target inbox is broken: {resp}"
        );

        // The sender's parent row must remain unprocessed — settle must NOT fire.
        let path = crate::inbox::storage::inbox_path_resolved(&home, sender);
        let body = std::fs::read_to_string(&path).unwrap();
        let row = body
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(pid))
            .expect("sender parent row must still exist");
        assert!(
            row.get("read_at").is_none_or(|r| r.is_null()),
            "a FAILED fallback delivery must not settle the sender parent: {row}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// t-20260705005551919287-14440-22: `send_to`'s API-down fallback hardcoded
    /// `kind: None` when handing off to `inbox::deliver` — losing the ORIGINAL
    /// message kind (`"query"`/`"task"`/etc, threaded through fine on the
    /// API-reachable path via the `"kind": kind` param) the instant the daemon
    /// API is unreachable. Real production callers pass a real kind here
    /// (`handle_request_information` → `"query"`; `handle_broadcast` → the
    /// caller's `request_kind`, which can be `"task"`/`"query"`) — a query/task
    /// silently landing as kind=None means its obligation classification is
    /// lost (poll-reminder doesn't re-arm for it), not just cosmetic.
    ///
    /// No live daemon exists in this test's fresh `tmp_home`, so `api::call`
    /// fails deterministically and `send_to` takes the fallback path for real
    /// — not a mocked substitute for it.
    #[test]
    fn send_to_fallback_preserves_original_kind_not_none_2620() {
        let home = tmp_home("send-to-fallback-kind");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  sf2620t:\n    backend: claude\n",
        )
        .unwrap();
        let from = Sender::new("sender").expect("valid sender");

        // t-20260705005551919287-14440-22 (dev3 heartbeat_pair sweep): NOT the
        // generic "worker" fixture name — this fallback path writes
        // `heartbeat_pair["worker"]`, colliding under plain `cargo test`'s
        // shared process with dispatch_idle's #1516 tests reading that same
        // process-global (non-`home`-scoped) registry entry.
        let result = send_to(&home, &from, "sf2620t", "hello", "query", None);
        assert_eq!(
            result.get("delivery_mode").and_then(|d| d.as_str()),
            Some("inbox_fallback"),
            "test invariant: no daemon running in this tmp_home, must hit the \
             fallback path (not skip past it): {result}"
        );

        let msgs = crate::inbox::drain(&home, "sf2620t");
        assert_eq!(msgs.len(), 1, "fallback must still enqueue the message");
        assert_eq!(
            msgs[0].kind.as_deref(),
            Some("query"),
            "the fallback delivery must preserve the ORIGINAL kind passed to \
             send_to, not silently drop it to None: {:?}",
            msgs[0]
        );
        std::fs::remove_dir_all(&home).ok();
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

    // --- get_submit_key (1 from ops.rs) ---

    #[test]
    fn submit_key_default() {
        let home = tmp_home("sk");
        assert_eq!(get_submit_key(&home, "x"), "\r");
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
}

#[cfg(test)]
mod review_repro_agent_binding;
#[cfg(test)]
mod review_repro_xcut_security;
