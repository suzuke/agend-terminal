//! #46776 r3 RED tests — real-entry-point tests for the 6 findings from the
//! dual-review rejection at 3d442d0a. Each test asserts the CORRECT
//! (post-fix) behavior and FAILS at the current HEAD because the fix is
//! not yet implemented. The GREEN commit makes them pass.
//!
//! G6 (test methodology) is satisfied by these tests themselves: real entry
//! points, deterministic synchronization (barriers), NO sleeps.
//!
//! Sibling-file placement (same precedent as `instance_964_tests.rs`) to
//! keep handler source files under the 750-LOC file_size_invariant.

use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::agent;
use crate::api::handlers::instance::handle_spawn;
use crate::api::handlers::HandlerCtx;
use parking_lot::Mutex;

fn tmp_home(slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("agend-r3-{slug}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn seed_agents_owned_by(dir: &Path, owner: &str) {
    std::fs::write(
        dir.join("AGENTS.md"),
        format!("<!-- agend:start -->\n## Identity\n\n- **Name**: `{owner}`\n<!-- agend:end -->\n"),
    )
    .unwrap();
}

fn seed_agy_agents_owned_by(dir: &Path, owner: &str) {
    let agy_dir = dir.join(".agents");
    std::fs::create_dir_all(&agy_dir).unwrap();
    std::fs::write(
        agy_dir.join("AGENTS.md"),
        format!("<!-- agend:start -->\n## Identity\n\n- **Name**: `{owner}`\n<!-- agend:end -->\n"),
    )
    .unwrap();
}

fn fleet_with_instances(home: &Path, yaml: &str) {
    std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
}

// ---------------------------------------------------------------------------
// G1 — prepare_instructions contextless fallback on named managed paths
// ---------------------------------------------------------------------------

/// G1: When fleet.yaml load fails and no explicit role is given,
/// `prepare_instructions` falls back to contextless `generate()` which skips
/// the workspace-identity preflight. A named managed instance can then
/// overwrite a foreign identity without any check.
///
/// RED: FAILS at 3d442d0a because `prepare_instructions` calls contextless
/// `generate(work_dir, command)` on the Err(_) fallback path, which calls
/// `generate_with_context(_, _, None)` — the None ctx skips
/// `workspace_provision_preflight`.
#[test]
fn g1_prepare_instructions_contextless_fallback_refuses_foreign_identity() {
    let home = tmp_home("g1-ctx");
    let ws = home.join("workspace").join("new-agent");
    std::fs::create_dir_all(&ws).unwrap();

    seed_agents_owned_by(&ws, "existing-owner");

    let result = crate::api::handlers::prepare_instructions(
        &home,
        "new-agent",
        "codex",
        &ws,
        None, // no explicit role → Err(_) fallback to contextless generate()
    );

    assert!(
        result.is_err(),
        "G1: prepare_instructions must refuse to provision when the working dir \
         has a foreign AGENTS.md identity, even on the contextless fallback path. \
         Currently the contextless generate() skips the identity preflight. Got Ok."
    );
    let content = std::fs::read_to_string(ws.join("AGENTS.md")).unwrap();
    assert!(
        content.contains("existing-owner"),
        "G1: the foreign AGENTS.md must not be overwritten. Content: {content}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// G2 — mutate_fleet_yaml opaque read error must fail-closed
// ---------------------------------------------------------------------------

/// G2: When fleet.yaml exists but is unreadable (opaque I/O error — not
/// NotFound), `mutate_fleet_yaml` must return Err, NOT silently fall back to
/// `default_content`. The current code uses `unwrap_or_else(|_| default)` which
/// treats permission-denied the same as file-absent → data loss.
///
/// RED: This test FAILS at 3d442d0a because `mutate_fleet_yaml` succeeds
/// despite the opaque read error, silently replacing the existing fleet with
/// the default content.
#[test]
fn g2_mutate_fleet_yaml_opaque_read_refuses_not_defaults() {
    let home = tmp_home("g2-opaque");
    let fleet_path = crate::fleet::fleet_yaml_path(&home);
    std::fs::write(
        &fleet_path,
        "instances:\n  existing-agent:\n    backend: claude\n",
    )
    .unwrap();

    // Make fleet.yaml a directory to force an opaque read error (not NotFound).
    std::fs::remove_file(&fleet_path).unwrap();
    std::fs::create_dir_all(&fleet_path).unwrap();

    let result =
        crate::fleet::persist::mutate_fleet_yaml(&home, "instances: {}\n", |_doc| Ok(false));

    assert!(
        result.is_err(),
        "G2: mutate_fleet_yaml must return Err on an opaque read error \
         (not NotFound), not silently fall back to default_content. \
         Data loss: the existing fleet would be replaced with an empty default."
    );

    std::fs::remove_dir_all(&fleet_path).ok();
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// G3 — spawn rollback must remove external branch worktree
// ---------------------------------------------------------------------------

/// G3: `cleanup_working_dir` must be able to clean dirs under
/// `$AGEND_HOME/worktrees/` (external branch worktrees), not just under
/// `$AGEND_HOME/workspace/`. When a spawn's `add_instance_to_yaml` fails
/// (racing admission refusal), the rollback calls `cleanup_working_dir` on
/// the worktree path, which currently does NOT remove it because it's outside
/// `workspace/`.
///
/// RED: FAILS at 3d442d0a because `cleanup_working_dir` only removes dirs
/// under `$AGEND_HOME/workspace/`; external worktrees leak.
#[test]
fn g3_cleanup_removes_external_branch_worktree_dir() {
    let home = tmp_home("g3-wt-rollback");
    fleet_with_instances(&home, "instances: {}\n");

    let wt_path = crate::worktree::worktree_path(&home, "racer", "feat/test");
    std::fs::create_dir_all(&wt_path).unwrap();
    std::fs::write(wt_path.join("marker.txt"), "worktree content").unwrap();

    let _conflict = crate::agent_ops::cleanup_working_dir(&home, "racer", &wt_path);

    assert!(
        !wt_path.exists(),
        "G3: cleanup_working_dir must remove newly-created external worktree \
         dirs (under $AGEND_HOME/worktrees/), not just workspace-local dirs. \
         The worktree dir still exists at: {}",
        wt_path.display()
    );

    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// G4 — working_dir_ownership_conflict must check ALL backend identity paths
// ---------------------------------------------------------------------------

/// G4: The delete guard must detect a foreign identity in `.agents/AGENTS.md`
/// (Agy backend's shared-instructions path), not just `AGENTS.md` and
/// `.codex/config.toml`.
///
/// RED: FAILS at 3d442d0a because `working_dir_ownership_conflict` only
/// checks `AGENTS.md` + `.codex/config.toml`.
#[test]
fn g4_ownership_conflict_detects_agy_agents_md_foreign_identity() {
    let dir = tmp_home("g4-agy");
    let ws = dir.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();

    seed_agy_agents_owned_by(&ws, "owner-instance");

    let conflict = crate::agent_ops::working_dir_ownership_conflict(&ws, "intruder-instance");

    assert!(
        conflict.is_some(),
        "G4: working_dir_ownership_conflict must detect a foreign identity in \
         .agents/AGENTS.md (Agy backend). Got None."
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// G4: `.grok/config.toml` with a foreign AGEND_INSTANCE_NAME stamp must also
/// be detected. Currently only `.codex/config.toml` is checked.
#[test]
fn g4_ownership_conflict_detects_grok_config_foreign_identity() {
    let dir = tmp_home("g4-grok");
    let ws = dir.join("workspace");
    let grok_dir = ws.join(".grok");
    std::fs::create_dir_all(&grok_dir).unwrap();

    std::fs::write(
        grok_dir.join("config.toml"),
        "# managed by agend\nAGEND_INSTANCE_NAME = \"owner-grok\"\n",
    )
    .unwrap();

    let conflict = crate::agent_ops::working_dir_ownership_conflict(&ws, "intruder-grok");

    assert!(
        conflict.is_some(),
        "G4: working_dir_ownership_conflict must detect a foreign identity in \
         .grok/config.toml, not just .codex/config.toml. Got None."
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// G4 (real entry): `full_delete_instance` must refuse cleanup when the working
/// directory has a foreign `.agents/AGENTS.md` (Agy backend).
///
/// RED: FAILS at 3d442d0a because `cleanup_working_dir` → `working_dir_ownership_conflict`
/// misses `.agents/AGENTS.md`.
#[test]
fn g4_full_delete_refuses_agy_foreign_identity() {
    let home = tmp_home("g4-del-agy");
    let ws = home.join("workspace").join("victim");
    std::fs::create_dir_all(&ws).unwrap();

    fleet_with_instances(
        &home,
        &format!(
            "instances:\n  victim:\n    backend: agy\n    working_directory: {}\n",
            ws.display()
        ),
    );

    seed_agy_agents_owned_by(&ws, "real-owner");

    let result =
        crate::mcp::handlers::instance_state::lifecycle::full_delete_instance(&home, "victim");

    assert!(
        result.is_err(),
        "G4: full_delete_instance must report Err when the working dir has a \
         foreign .agents/AGENTS.md — the tree must be preserved. Got Ok."
    );
    assert!(
        ws.join(".agents").join("AGENTS.md").exists(),
        "G4: the foreign Agy identity file must NOT be deleted"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// G5 — handle_spawn runtime duplicate workspace admission
// ---------------------------------------------------------------------------

/// G5: `handle_spawn` (the daemon SPAWN RPC handler) must reject a spawn when
/// the requested working directory collides with an EXISTING fleet entry's
/// working directory. Currently it has NO workspace-identity collision check —
/// `duplicate_identity_owner_before` is boot-only, and `handle_spawn` only
/// checks name collision (external + registry), not workspace collision.
///
/// RED: FAILS at 3d442d0a because `handle_spawn` proceeds past name checks
/// without any workspace-identity check. The error (if any) comes from a
/// downstream spawn failure, not from a collision refusal.
#[test]
fn g5_handle_spawn_rejects_workspace_identity_collision() {
    let home = Box::new(tmp_home("g5-dup-spawn"));
    let shared_dir = home.join("workspace").join("shared");
    std::fs::create_dir_all(&shared_dir).unwrap();

    // Pre-register "alice" with the shared working directory.
    fleet_with_instances(
        &home,
        &format!(
            "instances:\n  alice:\n    backend: claude\n    working_directory: {}\n",
            shared_dir.display()
        ),
    );

    let registry: &'static agent::AgentRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let configs: &'static crate::api::ConfigRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let externals: &'static agent::ExternalRegistry =
        Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
    let home_ref: &'static Path = Box::leak(home.clone());
    let ctx = HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home: home_ref,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
    };

    let resp = handle_spawn(
        &json!({
            "name": "bob",
            "backend": "claude",
            "working_directory": shared_dir.display().to_string()
        }),
        &ctx,
    );

    assert_eq!(
        resp["ok"],
        json!(false),
        "G5: handle_spawn must reject a spawn whose working directory collides \
         with an existing fleet entry: {resp:?}"
    );
    let error_msg = resp["error"].as_str().unwrap_or_default();
    assert!(
        error_msg.contains("collision") || error_msg.contains("workspace identity"),
        "G5: the rejection must cite the workspace identity collision, not a \
         downstream spawn failure. Got error: {error_msg}"
    );

    std::fs::remove_dir_all(home_ref).ok();
}
