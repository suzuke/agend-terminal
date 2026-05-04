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

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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
    let in_fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .map(|c| c.instances.contains_key(target))
        .unwrap_or(false);
    if !in_fleet {
        return json!({"error": format!("target instance '{target}' not found in fleet.yaml (API unavailable: {api_error})")});
    }
    let _ = crate::inbox::enqueue(home, target, msg);
    crate::inbox::notify_agent(home, target, &crate::inbox::NotifySource::Agent(from), text);
    json!({"target": target, "delivery_mode": "inbox_fallback", "note": format!("API unavailable: {api_error}")})
}

/// Send a message to a target instance via API, falling back to direct
/// inbox delivery when the daemon is unreachable.
///
/// `from: &Sender` guarantees a non-empty originator; callers cannot
/// accidentally stamp messages with `[from:]` (see `src/identity.rs`).
pub fn send_to(home: &Path, from: &Sender, target: &str, text: &str, kind: &str) -> Value {
    let from_str = from.as_str();
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SEND,
            "params": { "from": from_str, "target": target, "text": text, "kind": kind }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let dm = resp["delivery_mode"].as_str().unwrap_or("pty");
            json!({"target": target, "delivery_mode": dm})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            // Validate target exists in fleet.yaml before writing to inbox.
            let in_fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
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
/// Infrastructure for Sprint 47 file path migration — callers adopt incrementally.
#[allow(dead_code)]
pub fn metadata_path_resolved(home: &Path, name: &str) -> PathBuf {
    let id = crate::agent::resolve_instance(home, name)
        .ok()
        .map(|(id, _)| id);
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

/// Load metadata for an instance and merge it into the given JSON value.
pub fn merge_metadata(home: &Path, name: &str, info: &mut Value) {
    let meta_path = metadata_path(home, name);
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
/// Uses atomic write (temp file + rename) so concurrent readers
/// (e.g. supervisor tick) never see a half-written file.
pub fn save_metadata(home: &Path, instance_name: &str, key: &str, value: Value) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path(home, instance_name);
    let mut meta: Value = std::fs::read_to_string(&meta_path)
        .map(|c| serde_json::from_str(&c).unwrap_or(json!({})))
        .unwrap_or(json!({}));
    meta[key] = value;
    let content = serde_json::to_string_pretty(&meta).unwrap_or_default();
    // M1: atomic write with fsync
    let _ = crate::store::atomic_write(&meta_path, content.as_bytes());
}

/// Persist multiple metadata key/value pairs in a single atomic write.
/// Avoids the race condition where two sequential `save_metadata` calls
/// can interleave on Windows (the second read-modify-write reads stale data).
pub fn save_metadata_batch(home: &Path, instance_name: &str, entries: &[(&str, Value)]) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = metadata_path(home, instance_name);
    let mut meta: Value = std::fs::read_to_string(&meta_path)
        .map(|c| serde_json::from_str(&c).unwrap_or(json!({})))
        .unwrap_or(json!({}));
    for (key, value) in entries {
        meta[*key] = value.clone();
    }
    let content = serde_json::to_string_pretty(&meta).unwrap_or_default();
    // M1: atomic write with fsync
    let _ = crate::store::atomic_write(&meta_path, content.as_bytes());
}

// ---------------------------------------------------------------------------
// Fleet
// ---------------------------------------------------------------------------

/// Look up submit_key for a target instance from fleet config.
pub fn get_submit_key(home: &Path, target: &str) -> String {
    let fleet_path = home.join("fleet.yaml");
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
    let workspaces = home.join("workspace");

    // If under $AGEND_HOME/workspace/, remove the whole directory
    if working_dir.starts_with(&workspaces) {
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
            let _ = std::process::Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &wt_dir.display().to_string(),
                ])
                .current_dir(working_dir)
                .output();
            tracing::info!(dir = %wt_dir.display(), "removed worktree");
        }
    }

    // Always clean up metadata (regardless of workspace vs user dir)
    let meta = home.join("metadata").join(format!("{name}.json"));
    let _ = std::fs::remove_file(&meta);
}

// ---------------------------------------------------------------------------
// Agent enumeration
// ---------------------------------------------------------------------------

/// List agents published in the active daemon's run directory.
pub fn list_agents() -> Vec<String> {
    let home = crate::home_dir();
    let run = match crate::daemon::find_active_run_dir(&home) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&run) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".port") && name != "api.port" {
                agents.push(name[..name.len() - 5].to_string());
            }
        }
    }
    agents
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
}
