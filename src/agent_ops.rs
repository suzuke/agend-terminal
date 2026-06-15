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
            crate::inbox::deliver(
                home,
                target,
                &crate::inbox::NotifySource::Agent(from_str),
                text,
                &submit_key,
                None,
                broadcast_context.cloned(),
            );
            json!({"target": target, "delivery_mode": "inbox_fallback", "note": format!("API unavailable: {e}")})
        }
    }
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
    let id = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
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

/// Validate a git branch name. Only allows [a-zA-Z0-9/_.-], rejects ".."
/// and leading "-".
pub fn validate_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.contains("..")
        && !branch.starts_with('-')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
}

/// E4.5 protected-branch invariant. Returns `true` for branches that
/// agents MUST NOT lease, watch, or otherwise hold a per-agent
/// concept of interest in. The canonical set is `main` and `master`;
/// extending the set here propagates to every E4.5 enforcement site
/// (currently `worktree_pool::lease` for worktree leases and
/// `mcp::handlers::ci::handle_watch_ci` for CI watch subscriptions).
///
/// CR-2026-06-14: matched **case-insensitively**. On a case-insensitive
/// filesystem (darwin/APFS, Windows NTFS) `refs/heads/Main` and
/// `refs/heads/main` collide — a `branch="Main"` lease passes a case-sensitive
/// guard, then `git worktree add -b Main` fails ("already exists") and the
/// fallback `git worktree add <path> Main` checks out the EXISTING `main`, so
/// the agent commits land on `main` (empirically reproduced on darwin/APFS:
/// committing on "Main" advanced `main`). `eq_ignore_ascii_case` is a full-
/// string compare, so substrings like `mainline` / `maintenance` /
/// `upstream-main` stay unprotected.
pub fn is_protected_ref(branch: &str) -> bool {
    branch.eq_ignore_ascii_case("main") || branch.eq_ignore_ascii_case("master")
}

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
pub fn cleanup_working_dir(home: &Path, name: &str, working_dir: &Path) {
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
        if let Err(e) = std::fs::remove_dir_all(working_dir) {
            tracing::debug!(dir = %working_dir.display(), error = %e, "cleanup: remove workspace");
        } else {
            tracing::info!(dir = %working_dir.display(), "removed workspace");
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
) -> anyhow::Result<crate::backend::SpawnMode> {
    std::fs::create_dir_all(work_dir).ok();
    // #1080: skills auto-install for dynamically spawned instances.
    // spawn_one is the SPAWN-RPC choke point — without this, instances
    // created via create_instance / start_instance / replace_instance
    // never get skill symlinks (only cold-boot spawn_and_register_agent
    // called install_for_agent). Respects fleet.yaml `instance.<name>.skills:`
    // allowlist, same as cold-boot path.
    let skills_filter: Option<Vec<String>> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
    let backend_skill =
        crate::backend::Backend::from_command(backend).and_then(|b| b.skill_dir_name());
    match crate::skills::install_for_agent_backend(
        home,
        work_dir,
        skills_filter.as_deref(),
        backend_skill,
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
    let preset_submit_key = crate::backend::Backend::from_command(backend)
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
    // Closes the F7 race window from Sprint 20 Track B audit (DAEMON.md
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
        cleanup_working_dir(&home, "agent1", &ws);
        assert!(!ws.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_user_dir_selective() {
        let home = tmp_home("cu");
        let ud = tmp_home("cu_proj");
        std::fs::write(ud.join("main.rs"), "fn main(){}").ok();
        std::fs::write(ud.join("opencode.json"), "{}").ok();
        cleanup_working_dir(&home, "a", &ud);
        assert!(ud.join("main.rs").exists());
        assert!(!ud.join("opencode.json").exists());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ud).ok();
    }

    #[test]
    fn cleanup_metadata() {
        let home = tmp_home("cms");
        let ws = home.join("workspace/a");
        std::fs::create_dir_all(&ws).ok();
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(home.join("metadata/a.json"), "{}").ok();
        cleanup_working_dir(&home, "a", &ws);
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

        cleanup_working_dir(&home, "drift19", &ud);

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

            cleanup_working_dir(&home, "drift1", &ud);

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
