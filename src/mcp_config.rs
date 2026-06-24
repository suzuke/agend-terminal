//! Generate MCP server configuration for each backend.
//!
//! Reference: https://github.com/suzuke/AgEnD (TypeScript version)
//!
//! **Scope rule:** every CONFIG write here must land inside `$AGEND_HOME` or
//! inside the agent's `working_directory`. User-global tool configs
//! (`~/.codex/`, `~/.claude/`, etc.) are off-limits — mutating them risks
//! corrupting the user's personal CLI setup and can't be cleanly undone. If a
//! backend seems to need global state (codex trust prompt was the reason for
//! the old `codex_trust_directory` write), reach for a CLI flag or
//! `dismiss_patterns` in `src/backend.rs` instead.
//!
//! **#1547 narrow exception (`clear_agy_mcp_cache`):** the agy un-gate writes
//! its config to the in-`working_directory` `.agents/mcp_config.json` (rule
//! honored), but ALSO deletes agy's HOME-level *discovery cache* subdir
//! `~/.gemini/antigravity-cli/mcp/agend-terminal/`. This is **not** a config
//! write and does **not** touch the user's personal agy/gemini *settings* — it
//! removes only OUR server's transient discovery cache so agy re-discovers from
//! the project-local config on its next boot (the recovery-safety fix; agy
//! regenerates the cache itself). Deliberate, reviewed (operator-signed-off),
//! and idempotent — the only HOME-level mutation in this module.

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Get the agend-mcp-bridge binary path. Lives alongside the main binary.
///
/// Sprint 56 Track I-Phase2b (#531): the previous "fall back to
/// `agend-terminal mcp` if bridge missing" behaviour is removed. Phase
/// 1 RCA (`docs/RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md`)
/// identified the fallback as the load-bearing failure path: backends
/// spawned with `agend-terminal mcp` route through `mcp::run`'s
/// `proxy_or_local`, which for daemon-state-required tools (`reply` /
/// `react` / `download_attachment`) returns a structured error rather
/// than falling back to local. Operators on Windows experienced the
/// "no active channel" failure mode whenever the daemon's atomic
/// upsert overwrote their hand-edited mcp.json with the fallback
/// command.
///
/// Phase 2a shipped the bridge in release artifacts; Phase 2b drops
/// the fallback so a missing bridge is loud and explicit. Operators
/// who somehow ship without the bridge get an immediate FATAL log +
/// the daemon writes an unrunnable `command:` (intentional — better
/// than silently writing the broken local-mode command). Reinstall
/// from v0.7+ release artifacts (which Phase 2a ships) resolves.
fn bridge_binary_path() -> (String, Vec<&'static str>) {
    if let Ok(exe) = std::env::current_exe() {
        let bridge = exe.with_file_name(if cfg!(windows) {
            "agend-mcp-bridge.exe"
        } else {
            "agend-mcp-bridge"
        });
        if bridge.exists() {
            return (bridge.display().to_string(), vec![]);
        }
        tracing::error!(
            expected_path = %bridge.display(),
            "FATAL: agend-mcp-bridge binary not found alongside agend-terminal. \
             Reinstall from v0.7+ release artifacts which ship both binaries, \
             or ensure your build packaging includes agend-mcp-bridge."
        );
        eprintln!(
            "FATAL: agend-mcp-bridge missing at {}. Reinstall from v0.7+ release artifacts.",
            bridge.display()
        );
        // Return a clearly-broken command rather than silently falling
        // back to `agend-terminal mcp` (the pre-Phase-2b regression
        // path). Backends will fail loudly on the next mcp.json read.
        return (bridge.display().to_string(), vec![]);
    }
    // current_exe() failure is essentially unreachable in production
    // but the diagnostic still beats silent fallback.
    tracing::error!(
        "FATAL: std::env::current_exe() failed — cannot locate agend-mcp-bridge binary"
    );
    ("agend-mcp-bridge".to_string(), vec![])
}

/// Get the AGEND_HOME value.
fn home_path() -> String {
    crate::home_dir().display().to_string()
}

/// Standard MCP server entry with env vars.
fn mcp_server_entry(instance_name: Option<&str>) -> serde_json::Value {
    let mut env = json!({
        "AGEND_HOME": home_path()
    });
    if let Some(name) = instance_name {
        env["AGEND_INSTANCE_NAME"] = json!(name);
    }
    let (cmd, args) = bridge_binary_path();
    json!({
        "command": cmd,
        "args": args,
        "env": env
    })
}

/// Per-config flock path for a given config file. Two concurrent `configure`
/// calls targeting the same working_directory would otherwise interleave
/// their read→mutate→write cycles (one reads stale content, applies its
/// edit, overwrites the other's edit). We use a sibling `.lock` file so
/// the lock is local to the project dir and auto-released on drop.
fn config_lock_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    parent.join(format!(".{name}.lock"))
}

/// Upsert mcpServers.agend-terminal in a JSON file (Claude, Kiro format).
///
/// Flock-serialised + atomic write. Prior implementation `fs::write`'d
/// directly with no lock, so two concurrent `create_instance` calls
/// targeting the same working_directory could drop one of their edits.
fn upsert_mcp_servers(path: &Path, instance_name: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                let backup = path.with_extension(format!(
                    "corrupt.{}",
                    chrono::Utc::now().format("%Y%m%d%H%M%S")
                ));
                // Fail-CLOSED: the prior `let _ = std::fs::copy(path, &backup)`
                // discarded the copy result, so a copy failure (disk full,
                // permission) destroyed the user's ONLY copy on the atomic_write
                // below while the warn claimed a backup was made. Reuse store.rs's
                // robust rename-first backup; if it CANNOT preserve the corrupt
                // bytes, refuse to overwrite — return Err and leave the original
                // on disk for the operator to rescue.
                if !crate::store::backup_corrupt_file(path, &backup) {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "malformed MCP config JSON AND backup failed — refusing to overwrite (original preserved)"
                    );
                    anyhow::bail!(
                        "malformed MCP config at {} and its backup failed; refusing to overwrite (original preserved for rescue)",
                        path.display()
                    );
                }
                tracing::warn!(
                    path = %path.display(),
                    backup = %backup.display(),
                    error = %e,
                    "malformed MCP config JSON, backed up corrupt file and starting fresh"
                );
                json!({})
            }
        }
    } else {
        json!({})
    };

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = mcp_server_entry(instance_name);

    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// Claude Code: .claude/settings.local.json + mcp-config.json
fn configure_claude(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    // Ensure working dir is a git repo (Claude Code needs git root to find .claude/)
    let git_dir = working_dir.join(".git");
    if !git_dir.exists() {
        // W1.2: git_cmd bundles the bypass env + LOCAL timeout + process-group
        // kill; its GitError carries the stderr/spawn detail the warn logged by
        // hand (init is local, so the local timeout is ample).
        if let Err(e) = crate::git_helpers::git_cmd(working_dir, &["init"]) {
            tracing::warn!(dir = %working_dir.display(), error = %e, "git init failed");
        }
    }

    // Write project-local MCP config
    let path = working_dir.join(".claude").join("settings.local.json");
    upsert_mcp_servers(&path, instance_name)?;

    // #hook-state-poc (shadow-mode, flag-gated — default OFF, zero behavior
    // change): inject lifecycle-hook reporters into the SAME per-workspace
    // settings file (scope rule honored; user-global ~/.claude is untouched).
    // Empirically verified (2026-06-11 live fleet spawn): the events fire as
    // documented, the TUI shows no artifacts (async), and this upsert
    // preserves pre-existing keys.
    // #hook-state-poc OR #2413 Shadow Observer (local plane): either flag wants the
    // lifecycle hooks wired into the per-workspace settings (still NEVER ~/.claude).
    if std::env::var("AGEND_HOOK_STATE_POC").as_deref() == Ok("1")
        || crate::daemon::shadow::enabled()
    {
        if let Some(name) = instance_name {
            upsert_state_hooks(&path, name)?;
        }
    }

    // Write standalone mcp-config.json for --mcp-config flag
    let standalone = working_dir.join("mcp-config.json");
    upsert_mcp_servers(&standalone, instance_name)?;

    Ok(())
}

/// #hook-state-poc: upsert observe-only lifecycle-hook reporters into the
/// per-workspace Claude settings. Merge-preserving at three levels: other
/// top-level keys, other hook EVENTS, and other (user/project) hook entries
/// under the same event — our entry is identified by the
/// `hook-event --instance` marker and replaced idempotently. Every entry is
/// `async` (the TUI never waits) and the reporter always exits 0 (exit 2
/// would block the agent's action — observe-only by contract).
fn upsert_state_hooks(path: &Path, instance_name: &str) -> Result<()> {
    /// Events wired for the shadow PoC. Verified live: SessionStart /
    /// UserPromptSubmit / PreToolUse / PostToolUse / Stop /
    /// Notification(idle_prompt). Docs-sourced (not yet observed live):
    /// Notification(permission_prompt) — the fleet runs bypassPermissions so
    /// permission prompts cannot occur in-fleet; StopFailure / PreCompact /
    /// PermissionRequest / SessionEnd — shadow data will show whether and
    /// when they fire.
    const HOOK_EVENTS: &[&str] = &[
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "PermissionRequest",
        "StopFailure",
        "PreCompact",
        "SessionEnd",
    ];
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string());
    let marker = "hook-event --instance";
    let command = format!("{exe} hook-event --instance {instance_name}");
    let our_entry = json!({
        "hooks": [{
            "type": "command",
            "command": command,
            "async": true,
            "timeout": 10,
        }]
    });

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(path))?;
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                // Same fail-CLOSED contract as upsert_mcp_servers: the prior
                // `unwrap_or(json!({}))` SILENTLY discarded the operator's whole
                // shared settings file (permissions + other keys) with no backup
                // before atomic_write'ing a fresh document. Back up the corrupt
                // original (rename-first) and refuse to overwrite if it can't be
                // preserved.
                let backup = path.with_extension(format!(
                    "corrupt.{}",
                    chrono::Utc::now().format("%Y%m%d%H%M%S")
                ));
                if !crate::store::backup_corrupt_file(path, &backup) {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "malformed settings JSON (hook upsert) AND backup failed — refusing to overwrite (original preserved)"
                    );
                    anyhow::bail!(
                        "malformed settings at {} and its backup failed; refusing to overwrite (original preserved for rescue)",
                        path.display()
                    );
                }
                tracing::warn!(
                    path = %path.display(),
                    backup = %backup.display(),
                    error = %e,
                    "malformed settings JSON (hook upsert), backed up corrupt file and starting fresh"
                );
                json!({})
            }
        }
    } else {
        json!({})
    };
    if config.get("hooks").is_none() {
        config["hooks"] = json!({});
    }
    for event in HOOK_EVENTS {
        let groups = config["hooks"]
            .as_object_mut()
            .expect("hooks set above")
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if let Some(arr) = groups.as_array_mut() {
            // Idempotent replace-by-marker; user/project entries untouched.
            arr.retain(|g| !g.to_string().contains(marker));
            arr.push(our_entry.clone());
        }
    }
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured state hooks (shadow PoC)");
    Ok(())
}

/// Kiro: .kiro/settings/mcp.json — uses wrapper script because Kiro ignores env block.
///
/// All edits run under a per-path flock + atomic write so two concurrent
/// `create_instance` calls sharing a working_directory can't interleave
/// their read→mutate→write cycles into a corrupt mcp.json.
fn configure_kiro(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join(".kiro").join("settings").join("mcp.json");

    let (cmd, args) = bridge_binary_path();
    let mut env = json!({ "AGEND_HOME": home_path() });
    if let Some(name) = instance_name {
        env["AGEND_INSTANCE_NAME"] = json!(name);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Clean up old format: remove top-level "agend-terminal" key (pre-dates
    // the mcpServers schema). Done under the same lock as the upsert below.
    if let Some(obj) = config.as_object_mut() {
        obj.remove("agend-terminal");
    }

    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"]["agend-terminal"] = json!({
        "command": cmd,
        "args": args,
        "env": env,
        "autoApprove": ["*"]
    });
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");

    Ok(())
}

/// AGY (Google Antigravity CLI): write `<workdir>/.agents/mcp_config.json`.
///
/// #1547 un-gate: agy loads project-scoped MCP via the official Customization
/// Roots dir `<workspace>/.agents/` (operator-verified: plain + yolo
/// `--dangerously-skip-permissions` both load `✓ agend-terminal Tools`). This
/// replaces the dead `.antigravitycli/mcp_config.json` write (#995 Bug 3 — agy
/// ignored that file's `mcpServers`). **Self-contained** — #1580: the
/// gemini-cli MCP writer is retired; agy's writer never depended on it.
///
/// The file carries ONLY the `mcpServers` block and is written FRESH (not
/// merged) each spawn, so a stale/garbled prior file cannot poison discovery —
/// a property the recovery-safety below depends on.
///
/// #1547 M2 recovery-safety: agy persists a HOME-level MCP discovery cache at
/// `~/.gemini/antigravity-cli/mcp/<server>/`, shared across instances and
/// reused across restarts. A boot that ever found `.agents/` absent caches
/// "no MCP" and never re-discovers — including on the Stage-2/crash respawn
/// path (`daemon/mod.rs`). So this (1) clears our server's cache subdir on
/// every (re)configure to force re-discovery from the fresh config on agy's
/// next boot, and (2) verifies the write landed + warns (never silent) on
/// write/clear failure. The respawn path also calls `configure()` so recovery
/// re-runs both steps.
fn configure_agy(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join(".agents").join("mcp_config.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Flock + atomic write so concurrent spawns can't interleave their writes.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut server = mcp_server_entry(instance_name);
    server["trust"] = json!(true);
    let config = json!({ "mcpServers": { "agend-terminal": server } });
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;

    // #1547 M2(c): verify the write actually landed — never silently leave agy
    // bare (a missing config = agy boots without fleet tools, the exact
    // recovery-failure class this PR fixes).
    if !path.exists() {
        anyhow::bail!(
            "configure_agy: {} missing after atomic_write",
            path.display()
        );
    }

    // #1547 M2(a): bust agy's HOME-level discovery cache so its next boot
    // re-discovers from the config just written.
    clear_agy_mcp_cache();

    // #2413 Phase D (agy shadow plane): inject per-workspace lifecycle hooks into
    // `.agents/hooks.json` — agy/Antigravity's hook config (NEVER touches the
    // global `~/.gemini`, scope rule honored). Same flag gate as claude
    // (`configure_claude`); needs an instance name for the session↔agent
    // attribution embedded in the hook command. RE-spike (t-…39100-6) proved these
    // per-workspace hooks fire live mid-turn.
    if std::env::var("AGEND_HOOK_STATE_POC").as_deref() == Ok("1")
        || crate::daemon::shadow::enabled()
    {
        if let Some(name) = instance_name {
            let hooks_path = working_dir.join(".agents").join("hooks.json");
            upsert_agy_state_hooks(&hooks_path, name)?;
        }
    }

    tracing::debug!(path = %path.display(), "configured agy MCP (.agents/)");
    Ok(())
}

/// #2413 Phase D: upsert observe-only lifecycle-hook reporters into agy's
/// per-workspace `.agents/hooks.json` (agy/Antigravity hook config; the global
/// `~/.gemini` is NEVER touched — scope rule). Agy fires `PreInvocation` (per
/// model step), `PreTool`/`PostTool`, and `Stop`; each command hook runs the
/// shared `hook-event` reporter with an explicit `--event` carrying the
/// CLAUDE-COMPATIBLE name the shadow server already maps, so Evidence + the
/// reducer are reused UNCHANGED:
///   PreInvocation→UserPromptSubmit(TurnStarted), PreTool→PreToolUse(ToolStarted),
///   PostTool→PostToolUse(ToolEnded), Stop→Stop(TurnEnded→idle).
/// Merge-preserving + idempotent: our entry is identified by the
/// `hook-event --instance` marker and replaced on re-configure; any operator
/// hooks under the same event are preserved. The reporter always exits 0
/// (observe-only — never blocks agy's turn) and the socket write fast-fails when
/// the per-session socket is absent (shadow off), so this is spawn-safe.
fn upsert_agy_state_hooks(path: &Path, instance_name: &str) -> Result<()> {
    // (agy event name, claude-compatible event the shadow server maps).
    const EVENT_MAP: &[(&str, &str)] = &[
        ("PreInvocation", "UserPromptSubmit"),
        ("PreTool", "PreToolUse"),
        ("PostTool", "PostToolUse"),
        ("Stop", "Stop"),
    ];
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string());
    let marker = "hook-event --instance";

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::store::acquire_file_lock(&config_lock_path(path))?;
    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(path)?;
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                // Fail-CLOSED (same contract as upsert_state_hooks): back up the
                // corrupt file (rename-first) and refuse to silently clobber it.
                let backup = path.with_extension(format!(
                    "corrupt.{}",
                    chrono::Utc::now().format("%Y%m%d%H%M%S")
                ));
                if !crate::store::backup_corrupt_file(path, &backup) {
                    anyhow::bail!(
                        "malformed agy hooks at {} and its backup failed; refusing to overwrite (original preserved)",
                        path.display()
                    );
                }
                tracing::warn!(
                    path = %path.display(), backup = %backup.display(), error = %e,
                    "malformed agy hooks.json, backed up corrupt file and starting fresh"
                );
                json!({})
            }
        }
    } else {
        json!({})
    };
    if !config.is_object() {
        config = json!({});
    }
    if !config.get("hooks").map(|h| h.is_object()).unwrap_or(false) {
        config["hooks"] = json!({});
    }
    let hooks = config["hooks"]
        .as_object_mut()
        .expect("hooks set to object above");
    for (agy_event, claude_event) in EVENT_MAP {
        let command = format!("{exe} hook-event --instance {instance_name} --event {claude_event}");
        let our_entry = json!({ "name": "agend-shadow", "command": command });
        let arr = hooks
            .entry((*agy_event).to_string())
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let list = arr.as_array_mut().expect("event value set to array above");
        // Idempotent: drop our prior entry (marker), preserve operator hooks.
        list.retain(|e| {
            !e.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains(marker))
        });
        list.push(our_entry);
    }
    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(path, body.as_bytes())?;
    Ok(())
}

/// #1547 M2(a): remove agy's HOME-level MCP discovery cache subdir for the
/// `agend-terminal` server so a stale "no MCP" (or outdated) cache cannot
/// survive a (re)configure. Per the Q2 decision the server name is kept as the
/// operator-verified `agend-terminal` (shared across instances); clearing it
/// forces every agy's NEXT boot to re-discover from `.agents/` — a running agy
/// does not re-read mid-session, so the race is narrow and re-discovery is
/// idempotent. Best-effort + WARN (never fatal): a missing cache dir is the
/// normal first-spawn case, only a real removal error is surprising.
fn clear_agy_mcp_cache() {
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("configure_agy: cannot resolve user home — skipping agy MCP cache bust");
        return;
    };
    let cache = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("mcp")
        .join("agend-terminal");
    if !cache.exists() {
        return;
    }
    match std::fs::remove_dir_all(&cache) {
        Ok(()) => tracing::debug!(
            path = %cache.display(),
            "configure_agy: cleared agy MCP discovery cache"
        ),
        Err(e) => tracing::warn!(
            path = %cache.display(), error = %e,
            "configure_agy: failed to clear agy MCP discovery cache — a stale \
             entry may suppress fleet tools until removed manually"
        ),
    }
}

/// OpenCode: opencode.json — uses { "mcp": { ... } } with command as array.
///
/// Also sets the `permission` block to "allow" for the actions an autonomous
/// agent will hit (edit / bash / webfetch / external_directory). Each instance
/// has its own working_dir/opencode.json so this does not bleed into the
/// user's manual opencode usage.
fn configure_opencode(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    let path = working_dir.join("opencode.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Flock + atomic write so concurrent spawns can't interleave their
    // load-modify-save cycles on opencode.json.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&path))?;

    let mut config: serde_json::Value = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Remove old wrong format if present
    if let Some(obj) = config.as_object_mut() {
        obj.remove("mcpServers");
    }

    if config.get("mcp").is_none() {
        config["mcp"] = json!({});
    }
    let mut oc_env = json!({
        "AGEND_HOME": home_path()
    });
    if let Some(name) = instance_name {
        oc_env["AGEND_INSTANCE_NAME"] = json!(name);
    }
    let (oc_cmd, oc_args) = bridge_binary_path();
    let oc_command: Vec<String> = std::iter::once(oc_cmd)
        .chain(oc_args.iter().map(|s| s.to_string()))
        .collect();
    config["mcp"]["agend-terminal"] = json!({
        "type": "local",
        "command": oc_command,
        "enabled": true,
        "environment": oc_env
    });

    // Force `permission` to an object so we can insert keys; replaces any
    // pre-existing scalar form (e.g. "ask") since autonomous agents must
    // not block on prompts.
    if !config
        .get("permission")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        config["permission"] = json!({});
    }
    let perm = config["permission"]
        .as_object_mut()
        .expect("permission set to object above");
    for key in ["edit", "bash", "webfetch", "external_directory"] {
        perm.insert(key.to_string(), json!("allow"));
    }
    // #1657: auto-approve the agend-terminal MCP server's tools. Without this,
    // opencode raises a "Permission required" prompt on every MCP tool-call
    // (including `send` kind=report) and the call blocks until the 30s MCP
    // client timeout — so a reviewer's report never lands, its dispatch sidecar
    // stays Pending, and dispatch_idle false-positives fire (the lead force-
    // reassigns every review). opencode names MCP tools `<server>_<tool>`, and
    // its permission keys do simple `*` glob matching, so `agend-terminal_*`
    // covers all of this server's tools. This mirrors the other backends'
    // posture (claude `autoApprove:["*"]`, gemini `trust:true`, codex
    // `--dangerously-bypass`); on a single-user single-machine tool, trusting
    // the user's own fleet MCP server is consistent and necessary for autonomy.
    // Empirically verified against opencode 1.15.10: with this key a `send`
    // tool-call completes immediately; without it the call hangs on the prompt.
    perm.insert("agend-terminal_*".to_string(), json!("allow"));

    let body = serde_json::to_string_pretty(&config)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    tracing::debug!(path = %path.display(), "configured MCP");
    Ok(())
}

/// Codex: write .codex/config.toml per-project + trust in ~/.codex/config.toml.
///
/// `codex mcp add` only writes to global config and doesn't support per-project.
/// But Codex loads .codex/config.toml from the project root (trusted projects only).
/// This gives us per-instance AGEND_INSTANCE_NAME via project-level config.
/// Section headers owned by this writer — the two tables we strip+rewrite.
const CODEX_MCP_HEADER: &str = "[mcp_servers.agend-terminal]";
const CODEX_MCP_ENV_HEADER: &str = "[mcp_servers.agend-terminal.env]";

fn configure_codex(working_dir: &Path, instance_name: Option<&str>) -> Result<()> {
    configure_codex_with_home(working_dir, &home_path(), instance_name)
}

/// Split out so tests can drive a scratch `home` without mutating
/// process-wide `HOME` / `USERPROFILE`. `cargo test` runs tests in parallel
/// inside one process, and `user_home_dir()` is read by many backends — env
/// mutation here races with other tests.
fn configure_codex_with_home(
    working_dir: &Path,
    home: &str,
    instance_name: Option<&str>,
) -> Result<()> {
    let (bridge_cmd, bridge_args) = bridge_binary_path();
    let bin = bridge_cmd;

    let codex_dir = working_dir.join(".codex");
    std::fs::create_dir_all(&codex_dir)?;
    let config_path = codex_dir.join("config.toml");

    // Flock + atomic write so two concurrent spawns can't interleave their
    // strip→append cycles on the same config.toml.
    let _lock = crate::store::acquire_file_lock(&config_lock_path(&config_path))?;

    // Re-read under the lock. Any existing agend-terminal block is stripped
    // before we write a fresh one — otherwise a stale binary path from a
    // prior build (e.g. a worktree that has since been removed) would
    // silently persist and fail at codex MCP startup with ENOENT.
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut stripped = strip_agend_mcp_sections(&existing);
    // Normalize to exactly one trailing newline on non-empty content so the
    // next `\n` we emit produces a single blank-line separator.
    while stripped.ends_with("\n\n") {
        stripped.pop();
    }
    if !stripped.is_empty() && !stripped.ends_with('\n') {
        stripped.push('\n');
    }
    let separator = if stripped.is_empty() { "" } else { "\n" };

    // Single-quoted TOML literal strings preserve backslashes verbatim;
    // a double-quoted basic string interprets `\U` / `\n` / `\t` as escapes
    // and codex rejects its own config.toml on Windows when the binary path
    // happens to contain any of them. See `toml_string_value` for the
    // apostrophe fallback.
    let bin_lit = toml_string_value(&bin);
    let home_lit = toml_string_value(home);
    let instance_line = instance_name
        .map(|n| format!("AGEND_INSTANCE_NAME = {}\n", toml_string_value(n)))
        .unwrap_or_default();
    let args_toml = if bridge_args.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            bridge_args
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let body = format!(
        r#"{stripped}{separator}{CODEX_MCP_HEADER}
command = {bin_lit}
args = {args_toml}

{CODEX_MCP_ENV_HEADER}
AGEND_HOME = {home_lit}
{instance_line}"#
    );

    // Skip the atomic_write (temp file + fsync + rename) when the file is
    // already up to date. configure_codex runs on every codex pane spawn, so
    // the steady-state call is the no-op case.
    if existing != body {
        crate::store::atomic_write(&config_path, body.as_bytes())?;
    }
    tracing::debug!(path = %config_path.display(), "configured MCP");

    // NOTE: intentionally no `codex_trust_directory` write to
    // `~/.codex/config.toml`. That file is the user's personal codex config
    // and must stay untouched. The trust prompt is handled by
    // `--dangerously-bypass-approvals-and-sandbox` on the codex command line
    // (see `src/backend.rs`) plus the "Do you trust" dismiss_pattern as a
    // fallback. Writing here would pollute user state and has caused multiple
    // production bugs (see removed `codex_trust_directory` in git history).

    Ok(())
}

/// Render a string value as a TOML string, picking whichever quoting style
/// survives on the target. Windows paths routinely contain `\U` / `\d` / …
/// which a double-quoted basic string interprets as escapes and then fails
/// to parse. Single-quoted literal strings don't interpret anything, so they
/// round-trip any path. Fall back to an escaped basic string only if the
/// value contains a `'`, which a single-line literal can't represent.
fn toml_string_value(s: &str) -> String {
    if s.contains('\'') {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        format!("'{s}'")
    }
}

/// Remove any `[mcp_servers.agend-terminal]` / `[mcp_servers.agend-terminal.env]`
/// sections from a TOML string, preserving every other section and comment.
/// A section runs from its `[header]` line through the line before the next
/// `[header]` at the start of a line, or end-of-file.
fn strip_agend_mcp_sections(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_target = false;
    for raw_line in content.split_inclusive('\n') {
        let trimmed = raw_line.trim();
        // A section header is a line whose trimmed form is `[...]` — matches
        // both tables (`[foo]`) and array-of-tables (`[[foo]]`), and excludes
        // value lines that start with `[` inside a string.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_target = trimmed == CODEX_MCP_HEADER || trimmed == CODEX_MCP_ENV_HEADER;
            if in_target {
                continue;
            }
        }
        if !in_target {
            out.push_str(raw_line);
        }
    }
    out
}

/// Detect backend from command name and configure MCP.
pub fn configure(working_dir: &Path, command: &str, instance_name: Option<&str>) {
    let backend = crate::backend::Backend::from_command(command);
    let result = match backend {
        Some(crate::backend::Backend::ClaudeCode) => configure_claude(working_dir, instance_name),
        Some(crate::backend::Backend::KiroCli) => configure_kiro(working_dir, instance_name),
        Some(crate::backend::Backend::Agy) => configure_agy(working_dir, instance_name),
        Some(crate::backend::Backend::OpenCode) => configure_opencode(working_dir, instance_name),
        Some(crate::backend::Backend::Codex) => configure_codex(working_dir, instance_name),
        // Non-preset backends (Shell, Raw) have no MCP wiring.
        Some(crate::backend::Backend::Shell) | Some(crate::backend::Backend::Raw(_)) | None => {
            return
        }
    };

    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to configure MCP");
    }
}

/// Escape a string for use in a bash script.
#[allow(dead_code)] // Used by tests; was used by wrapper.sh (removed in Sprint 52)
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-mcp-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("/usr/bin/foo"), "'/usr/bin/foo'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(
            shell_escape("/path with spaces/bin"),
            "'/path with spaces/bin'"
        );
    }

    // --- OpenCode: must use "mcp" key, not "mcpServers" ---

    #[test]
    fn opencode_uses_mcp_key() {
        let dir = tmp_dir("oc_key");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(config.get("mcp").is_some(), "must have 'mcp' key");
        assert!(
            config.get("mcpServers").is_none(),
            "must NOT have 'mcpServers'"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_command_is_array() {
        let dir = tmp_dir("oc_cmd");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let cmd = &config["mcp"]["agend-terminal"]["command"];
        assert!(cmd.is_array(), "command must be array, got: {cmd}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_has_type_local() {
        let dir = tmp_dir("oc_type");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(config["mcp"]["agend-terminal"]["type"], "local");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_sets_permission_allow_for_autonomous_actions() {
        let dir = tmp_dir("oc_perm");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let perm = &config["permission"];
        for key in ["edit", "bash", "webfetch", "external_directory"] {
            assert_eq!(
                perm[key], "allow",
                "permission.{key} must be \"allow\" so autonomous agents don't block"
            );
        }
        // #1657: the agend-terminal MCP server's tools must be auto-approved
        // (`<server>_*` glob), else every MCP tool-call (incl. `send` report)
        // blocks on opencode's permission prompt until the 30s client timeout.
        assert_eq!(
            perm["agend-terminal_*"], "allow",
            "permission.\"agend-terminal_*\" must be \"allow\" so MCP tool-calls (send/report) don't block on a permission prompt (#1657)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_permission_replaces_scalar_form() {
        let dir = tmp_dir("oc_perm_scalar");
        // Pre-existing scalar form must be coerced to object — otherwise our
        // insert would silently fail and external_directory keeps prompting.
        let pre = json!({"permission": "ask"});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&pre).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            config["permission"].is_object(),
            "scalar permission must be replaced with object"
        );
        assert_eq!(config["permission"]["external_directory"], "allow");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_permission_preserves_unrelated_keys() {
        let dir = tmp_dir("oc_perm_preserve");
        let pre = json!({"permission": {"read": "deny", "edit": "deny"}});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&pre).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        // Our managed keys overwrite (autonomous context demands "allow").
        assert_eq!(config["permission"]["edit"], "allow");
        // Keys we don't manage stay untouched.
        assert_eq!(config["permission"]["read"], "deny");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_mcp_server_has_autoapprove_wildcard() {
        let dir = tmp_dir("kiro_autoapprove");
        configure_kiro(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let entry = &config["mcpServers"]["agend-terminal"];
        let auto = entry["autoApprove"]
            .as_array()
            .expect("autoApprove must be array");
        assert!(
            auto.iter().any(|v| v == "*"),
            "autoApprove must contain \"*\" wildcard so MCP tool calls don't prompt"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_autoapprove_idempotent_across_runs() {
        let dir = tmp_dir("kiro_autoapprove_idem");
        configure_kiro(&dir, None).expect("first configure");
        configure_kiro(&dir, None).expect("second configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let auto = config["mcpServers"]["agend-terminal"]["autoApprove"]
            .as_array()
            .expect("autoApprove must be array");
        assert_eq!(auto.len(), 1, "autoApprove must not duplicate on re-run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_uses_environment_not_env() {
        let dir = tmp_dir("oc_env");
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let entry = &config["mcp"]["agend-terminal"];
        assert!(
            entry.get("environment").is_some(),
            "must have 'environment'"
        );
        assert!(entry.get("env").is_none(), "must NOT have 'env'");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opencode_removes_old_mcpservers() {
        let dir = tmp_dir("oc_migrate");
        // Write old wrong format
        let old = json!({"mcpServers": {"agend-terminal": {"command": "old"}}});
        std::fs::write(
            dir.join("opencode.json"),
            serde_json::to_string(&old).expect("s"),
        )
        .ok();
        configure_opencode(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert!(
            config.get("mcpServers").is_none(),
            "old mcpServers must be removed"
        );
        assert!(config.get("mcp").is_some(), "new mcp key must exist");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Kiro: must use wrapper script ---

    #[test]
    fn kiro_creates_wrapper_script() {
        let dir = tmp_dir("kiro_env");
        configure_kiro(&dir, Some("dev")).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let entry = &config["mcpServers"]["agend-terminal"];
        // Must have env field with AGEND_HOME and AGEND_INSTANCE_NAME
        let env = entry.get("env").expect("env field must exist");
        assert!(
            env["AGEND_HOME"].as_str().is_some(),
            "env must contain AGEND_HOME"
        );
        assert_eq!(
            env["AGEND_INSTANCE_NAME"].as_str(),
            Some("dev"),
            "env must contain AGEND_INSTANCE_NAME"
        );
        // Command must NOT be a wrapper script
        let cmd = entry["command"].as_str().expect("command str");
        assert!(
            !cmd.contains("wrapper"),
            "must not use wrapper script, got: {cmd}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_mcp_json_uses_bridge_command() {
        let dir = tmp_dir("kiro_bridge");
        configure_kiro(&dir, None).expect("configure");
        let content = std::fs::read_to_string(dir.join(".kiro/settings/mcp.json")).expect("read");
        let config: serde_json::Value = serde_json::from_str(&content).expect("parse");
        let cmd = config["mcpServers"]["agend-terminal"]["command"]
            .as_str()
            .expect("command str");
        // Assert on the binary's FILE NAME, not a path substring. `cmd` is an
        // absolute path; under this repo it always contains "agend-terminal"
        // (the checkout dir name), so the old `cmd.contains("agend-terminal")`
        // disjunct was a tautology. The command must be the bridge binary
        // (`agend-mcp-bridge`) or the `agend-terminal` fallback by filename.
        let fname = std::path::Path::new(cmd)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        let stem = fname.strip_suffix(".exe").unwrap_or(fname);
        assert!(
            stem == "agend-mcp-bridge" || stem == "agend-terminal",
            "command must be the bridge binary or the agend-terminal fallback, \
             got filename {fname:?} (full path: {cmd})"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kiro_no_wrapper_script_created() {
        let dir = tmp_dir("kiro_nowrapper");
        configure_kiro(&dir, None).expect("configure");
        let ext = if cfg!(windows) { "cmd" } else { "sh" };
        let wrapper = dir.join(format!(".kiro/settings/agend-mcp-wrapper.{ext}"));
        assert!(
            !wrapper.exists(),
            "wrapper script must NOT be created: {}",
            wrapper.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- Claude: mcp-config.json + .claude/settings.local.json ---

    #[test]
    fn claude_creates_mcp_config() {
        let dir = tmp_dir("claude");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .ok();
        configure_claude(&dir, None).expect("configure");
        assert!(dir.join("mcp-config.json").exists());
        assert!(dir.join(".claude/settings.local.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- configure() dispatches correctly ---

    #[test]
    fn configure_dispatches_opencode() {
        let dir = tmp_dir("dispatch_oc");
        configure(&dir, "opencode", None);
        assert!(dir.join("opencode.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #987: configure dispatches Backend::Agy to write the standard
    /// #995 Bug 3: configure_agy is a no-op since empirical proof showed
    /// #1547: AGY now loads project-scoped MCP via the official Customization
    /// Roots file `<workspace>/.agents/mcp_config.json`. The dispatcher routes
    /// to `configure_agy`, which writes that file (self-contained — #1580: the
    /// gemini-cli MCP writer is retired) with the bridge entry + `trust:true` +
    /// per-instance env. The dead `.antigravitycli/` write (#995 Bug 3) is gone.
    #[test]
    fn configure_dispatches_agy_writes_agents_mcp() {
        let dir = tmp_dir("dispatch_agy");
        configure(&dir, "agy", Some("agy-1"));

        let cfg = dir.join(".agents").join("mcp_config.json");
        assert!(
            cfg.exists(),
            "#1547: configure_agy must write .agents/mcp_config.json"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let server = &v["mcpServers"]["agend-terminal"];
        assert!(
            server["command"].as_str().is_some_and(|c| !c.is_empty()),
            "bridge command must be present: {v}"
        );
        assert_eq!(
            server["trust"],
            json!(true),
            "agy entry must set trust:true"
        );
        assert_eq!(
            server["env"]["AGEND_INSTANCE_NAME"],
            json!("agy-1"),
            "per-instance AGEND_INSTANCE_NAME must be wired"
        );
        assert!(
            server["env"]["AGEND_HOME"].as_str().is_some(),
            "AGEND_HOME must be present in the bridge env"
        );
        // The dead legacy path must NOT be written, and the workdir `.gemini/`
        // must be untouched (#1580: configure_agy is self-contained — it never
        // wrote a project-local `.gemini/`).
        assert!(!dir.join(".antigravitycli").exists());
        assert!(!dir.join(".gemini").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configure_unknown_backend_no_crash() {
        let dir = tmp_dir("dispatch_unknown");
        configure(&dir, "unknown-tool", None);
        // Should not create any config files
        assert!(!dir.join("opencode.json").exists());
        assert!(!dir.join(".gemini").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- toml_string_value helper ---

    #[test]
    fn toml_string_value_uses_literal_for_paths_with_backslashes() {
        // Windows paths contain `\U` / `\d` / … which a TOML basic string
        // interprets as escape triggers. Literal (single-quoted) form is the
        // safe choice — it's what configure_codex emits for command/AGEND_HOME.
        assert_eq!(
            toml_string_value(r"C:\Users\alice\agend"),
            "'C:\\Users\\alice\\agend'"
        );
        assert_eq!(toml_string_value("/home/alice"), "'/home/alice'");
    }

    #[test]
    fn toml_string_value_escapes_basic_string_when_apostrophe_present() {
        // Single-line literal can't contain a `'` — fall back to basic string
        // with `\` and `"` escaped.
        assert_eq!(toml_string_value("it's mine"), "\"it's mine\"");
        assert_eq!(
            toml_string_value(r"C:\Program' Files\x"),
            r#""C:\\Program' Files\\x""#
        );
    }

    #[test]
    fn opencode_concurrent_configure_keeps_json_valid() {
        // Same race test against configure_opencode — opencode.json is
        // read→mutate→atomic_write under a flock.
        let dir = tmp_dir("opencode_concurrent");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    configure_opencode(&dir, None).expect("configure_opencode");
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread join");
        }

        let content = std::fs::read_to_string(dir.join("opencode.json")).expect("read");
        let config: serde_json::Value =
            serde_json::from_str(&content).expect("concurrent writes must leave valid JSON");
        // The agend-terminal entry must still be present and well-formed.
        assert!(config["mcp"]["agend-terminal"]["command"].is_array());
        assert_eq!(config["mcp"]["agend-terminal"]["type"], "local");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- configure_codex: MCP block must refresh, not skip-if-exists ---

    /// Regression for the `~/.codex/config.toml` write that was removed in
    /// this refactor. configure_codex used to trail with `codex_trust_directory`
    /// which mutated the user's global config — shipped two escape bugs into
    /// production, raced with the codex CLI's own writer, and left entries
    /// behind on uninstall. `agend must never touch user-global tool config`
    /// is now the rule (see the module doc comment).
    ///
    /// The test drives `configure_codex_with_home` directly with a scratch
    /// home path rather than mutating `$HOME` / `$USERPROFILE`. Env mutation
    /// would race with parallel tests that read `user_home_dir()`.
    #[test]
    fn configure_codex_writes_nothing_under_home() {
        let scratch = tmp_dir("no_home_write");
        let fake_home = scratch.join("agend_home");
        std::fs::create_dir_all(&fake_home).expect("mkdir fake_home");
        let work_dir = scratch.join("project");
        std::fs::create_dir_all(&work_dir).expect("mkdir project");

        configure_codex_with_home(
            &work_dir,
            &fake_home.display().to_string(),
            Some("test-instance"),
        )
        .expect("configure_codex");

        // Sanity: per-project config must exist.
        assert!(
            work_dir.join(".codex/config.toml").exists(),
            "per-project .codex/config.toml missing under working_dir"
        );
        // Guard: nothing may be written under the passed-in home. A regression
        // that reintroduces `codex_trust_directory` (writing to `home/.codex/`)
        // would land files here and fail the check.
        let entries: Vec<_> = std::fs::read_dir(&fake_home)
            .expect("read_dir fake_home")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert!(
            entries.is_empty(),
            "configure_codex must not write under its home arg, found: {entries:?}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn codex_config_refreshes_stale_binary_path() {
        // Regression guard: pre-fix code used an append-only write gated by
        // `!existing.contains("[mcp_servers.agend-terminal]")`, so a stale
        // binary path (e.g. from a removed worktree build) silently persisted
        // and codex MCP startup failed with ENOENT. The rewrite must replace
        // the `command` field with the current binary path.
        let dir = tmp_dir("codex_cfg_refresh");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[mcp_servers.agend-terminal]\n\
             command = \"/nonexistent/stale/binary\"\n\
             args = [\"mcp\"]\n\
             \n\
             [mcp_servers.agend-terminal.env]\n\
             AGEND_HOME = \"/old/home\"\n",
        )
        .expect("seed stale");

        configure_codex(&dir, None).expect("configure");

        let content = std::fs::read_to_string(&config_path).expect("read");
        assert!(
            !content.contains("/nonexistent/stale/binary"),
            "stale command must be overwritten:\n{content}"
        );
        assert!(
            !content.contains("/old/home"),
            "stale AGEND_HOME must be overwritten:\n{content}"
        );
        let parsed: toml::Value = toml::from_str(&content).expect("valid TOML after rewrite");
        let cmd = parsed["mcp_servers"]["agend-terminal"]["command"]
            .as_str()
            .expect("command string");
        assert_ne!(cmd, "/nonexistent/stale/binary");
        // Exactly one of each header — the stripper must not leave orphans.
        assert_eq!(content.matches(CODEX_MCP_HEADER).count(), 1);
        assert_eq!(content.matches(CODEX_MCP_ENV_HEADER).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_preserves_unrelated_sections() {
        // Other TOML sections (user settings, profiles) must survive the
        // strip+rewrite cycle. The stripper only targets the two agend-terminal
        // headers by exact match.
        let dir = tmp_dir("codex_cfg_preserve");
        let codex_dir = dir.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create .codex");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "model = \"gpt-5\"\n\
             \n\
             [mcp_servers.agend-terminal]\n\
             command = \"/old\"\n\
             args = [\"mcp\"]\n\
             \n\
             [mcp_servers.agend-terminal.env]\n\
             AGEND_HOME = \"/old\"\n\
             \n\
             [profile.custom]\n\
             model = \"other\"\n",
        )
        .expect("seed");

        configure_codex(&dir, None).expect("configure");

        let content = std::fs::read_to_string(&config_path).expect("read");
        let parsed: toml::Value = toml::from_str(&content).expect("valid TOML");
        assert_eq!(
            parsed["model"].as_str(),
            Some("gpt-5"),
            "top-level key dropped:\n{content}"
        );
        assert_eq!(
            parsed["profile"]["custom"]["model"].as_str(),
            Some("other"),
            "unrelated section dropped:\n{content}"
        );
        assert!(
            !content.contains("\"/old\""),
            "stale value leaked:\n{content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_idempotent_across_reruns() {
        // Re-running configure twice must leave the file byte-identical —
        // no duplicated headers, no drifting whitespace.
        //
        // Use `_with_home(scratch)` rather than `configure_codex` so the
        // emitted `AGEND_HOME = ...` value is fixed for both runs.
        // `configure_codex` reads `home_path()` (global `AGEND_HOME` env
        // var), which races with any other test in the process that
        // mutates that env — e.g. the `mcp::handlers::tests` Fleet
        // emission tests — and caused this test to drift across reruns.
        let dir = tmp_dir("codex_cfg_idem");
        std::fs::create_dir_all(dir.join(".codex")).expect("create .codex");
        let scratch_home = "/tmp/agend-test-home-codex-idem";

        configure_codex_with_home(&dir, scratch_home, Some("test-instance")).expect("first");
        let after_first =
            std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read first");
        configure_codex_with_home(&dir, scratch_home, Some("test-instance")).expect("second");
        let after_second =
            std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read second");

        assert_eq!(
            after_first, after_second,
            "second run drifted file:\nfirst:\n{after_first}\nsecond:\n{after_second}"
        );
        assert_eq!(after_second.matches(CODEX_MCP_HEADER).count(), 1);
        assert_eq!(after_second.matches(CODEX_MCP_ENV_HEADER).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_config_strips_only_agend_headers() {
        // Unit test for the stripper: only the two exact header names match.
        // A sibling `[mcp_servers.other]` must survive, and a comment that
        // mentions the header text but doesn't open a section must not
        // trigger the stripper.
        let input = "# [mcp_servers.agend-terminal] mentioned in top-level comment\n\
                     [other]\n\
                     value = 1\n\
                     \n\
                     [mcp_servers.agend-terminal]\n\
                     command = \"x\"\n\
                     \n\
                     [mcp_servers.other]\n\
                     command = \"y\"\n\
                     \n\
                     [mcp_servers.agend-terminal.env]\n\
                     AGEND_HOME = \"h\"\n";
        let out = strip_agend_mcp_sections(input);
        assert!(
            !out.contains("command = \"x\""),
            "target body leaked: {out}"
        );
        assert!(
            !out.contains("AGEND_HOME = \"h\""),
            "target env body leaked: {out}"
        );
        assert!(out.contains("[other]"), "unrelated section dropped: {out}");
        assert!(
            out.contains("[mcp_servers.other]"),
            "sibling mcp_servers dropped: {out}"
        );
        assert!(
            out.contains("# [mcp_servers.agend-terminal] mentioned in top-level comment"),
            "top-level comment dropped — stripper matched comment as header: {out}"
        );
    }

    // -----------------------------------------------------------------
    // AGEND_INSTANCE_NAME injection pins (fix/mcp-instance-name-env)
    // -----------------------------------------------------------------

    #[test]
    fn codex_config_includes_instance_name() {
        let scratch = tmp_dir("codex_inst_name");
        let dir = scratch.join("project");
        std::fs::create_dir_all(&dir).ok();
        configure_codex_with_home(&dir, "/fake/home", Some("my-agent")).expect("configure_codex");
        let content = std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read toml");
        assert!(
            content.contains("AGEND_INSTANCE_NAME"),
            "TOML env must contain AGEND_INSTANCE_NAME: {content}"
        );
        assert!(
            content.contains("my-agent"),
            "TOML env must contain the instance name value: {content}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn codex_idempotent_preserves_instance_name() {
        let scratch = tmp_dir("codex_idempotent");
        let dir = scratch.join("project");
        std::fs::create_dir_all(&dir).ok();
        configure_codex_with_home(&dir, "/fake/home", Some("agent-1")).expect("first");
        configure_codex_with_home(&dir, "/fake/home", Some("agent-1")).expect("second");
        let content = std::fs::read_to_string(dir.join(".codex/config.toml")).expect("read toml");
        // Must appear exactly once (strip+rewrite cycle doesn't duplicate)
        assert_eq!(
            content.matches("AGEND_INSTANCE_NAME").count(),
            1,
            "AGEND_INSTANCE_NAME must appear exactly once after idempotent rewrite: {content}"
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn json_backends_include_instance_name() {
        let entry = mcp_server_entry(Some("dev-2"));
        assert_eq!(
            entry["env"]["AGEND_INSTANCE_NAME"], "dev-2",
            "mcp_server_entry must include AGEND_INSTANCE_NAME in env"
        );
        // None case: no AGEND_INSTANCE_NAME key
        let entry_none = mcp_server_entry(None);
        assert!(
            entry_none["env"].get("AGEND_INSTANCE_NAME").is_none(),
            "mcp_server_entry(None) must not include AGEND_INSTANCE_NAME"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod hook_state_poc_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agend-hookpoc-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// #hook-state-poc: the hooks upsert is merge-preserving at all three
    /// levels (top-level keys / other events / foreign entries under the same
    /// event) and idempotent (re-run replaces our marker entry, no
    /// duplicates). Observe-only contract pinned: async:true on every entry.
    #[test]
    fn upsert_state_hooks_merge_preserving_and_idempotent() {
        let dir = tmp("merge");
        let path = dir.join(".claude").join("settings.local.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Pre-existing user content: a foreign top-level key, a foreign hook
        // event, and a foreign entry under an event we also wire.
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "permissions": {"allow": ["Bash(ls *)"]},
                "hooks": {
                    "FileChanged": [{"matcher": "*.rs", "hooks": [{"type": "command", "command": "user-watcher"}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": "user-stop-bell"}]}],
                }
            }))
            .unwrap(),
        )
        .unwrap();

        upsert_state_hooks(&path, "agent-x").unwrap();
        upsert_state_hooks(&path, "agent-x").unwrap(); // idempotent re-run

        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Foreign top-level key preserved.
        assert_eq!(cfg["permissions"]["allow"][0], "Bash(ls *)");
        // Foreign event preserved.
        assert_eq!(cfg["hooks"]["FileChanged"][0]["matcher"], "*.rs");
        // Foreign entry under a shared event preserved + exactly ONE of ours.
        let stop = cfg["hooks"]["Stop"].as_array().unwrap();
        assert!(stop
            .iter()
            .any(|g| g.to_string().contains("user-stop-bell")));
        assert_eq!(
            stop.iter()
                .filter(|g| g.to_string().contains("hook-event --instance"))
                .count(),
            1,
            "idempotent: exactly one of our entries after re-run"
        );
        // Observe-only contract: every injected entry is async.
        for ev in ["PreToolUse", "Notification", "Stop"] {
            let ours = cfg["hooks"][ev]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g.to_string().contains("hook-event --instance"))
                .unwrap_or_else(|| panic!("our entry present for {ev}"));
            assert_eq!(ours["hooks"][0]["async"], true, "{ev} must be async");
            assert!(
                ours["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .contains("--instance agent-x"),
                "instance embedded for attribution"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2413 Phase D (agy): the agy hooks upsert writes per-workspace
    /// `.agents/hooks.json` with agy's OWN event keys (PreInvocation/PreTool/
    /// PostTool/Stop) mapped to the CLAUDE-compatible `--event` the shadow server
    /// already understands (so Evidence + reducer are reused unchanged).
    /// Merge-preserving + idempotent. Reverse-mutation: break EVENT_MAP (e.g.
    /// PreTool→"ToolUse") → the `--event PreToolUse` assert fails; emit claude
    /// KEYS (PreToolUse) → the agy-key lookup + the "no claude key" assert fail.
    #[test]
    fn upsert_agy_state_hooks_maps_events_merges_and_is_idempotent() {
        let dir = tmp("agy-hooks");
        let path = dir.join(".agents").join("hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Pre-existing operator hook under a shared event (Stop) + a foreign event.
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "hooks": {
                    "Stop": [{"name": "user-bell", "command": "say done"}],
                    "SessionStart": [{"name": "user-greet", "command": "echo hi"}],
                }
            }))
            .unwrap(),
        )
        .unwrap();

        upsert_agy_state_hooks(&path, "agent-x").unwrap();
        upsert_agy_state_hooks(&path, "agent-x").unwrap(); // idempotent re-run

        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        for (agy_ev, claude_ev) in [
            ("PreInvocation", "UserPromptSubmit"),
            ("PreTool", "PreToolUse"),
            ("PostTool", "PostToolUse"),
            ("Stop", "Stop"),
        ] {
            let arr = cfg["hooks"][agy_ev]
                .as_array()
                .unwrap_or_else(|| panic!("event {agy_ev} present"));
            let ours: Vec<_> = arr
                .iter()
                .filter(|e| {
                    e["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("hook-event --instance"))
                })
                .collect();
            assert_eq!(ours.len(), 1, "{agy_ev}: exactly one of ours after re-run");
            let cmd = ours[0]["command"].as_str().unwrap();
            assert!(
                cmd.contains("--instance agent-x"),
                "{agy_ev}: instance embedded"
            );
            assert!(
                cmd.contains(&format!("--event {claude_ev}")),
                "{agy_ev} → --event {claude_ev}; got {cmd}"
            );
        }
        // Merge-preserving: operator + foreign entries survive.
        let stop = cfg["hooks"]["Stop"].as_array().unwrap();
        assert!(
            stop.iter().any(|e| e["command"] == "say done"),
            "operator Stop hook preserved"
        );
        assert_eq!(cfg["hooks"]["SessionStart"][0]["command"], "echo hi");
        // agy config must use AGY event KEYS, never claude's.
        assert!(
            cfg["hooks"].get("PreToolUse").is_none()
                && cfg["hooks"].get("UserPromptSubmit").is_none(),
            "agy hooks.json must key on agy events (PreTool/PreInvocation), not claude's"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #hook-state-poc: flag OFF (default) → configure_claude writes NO hooks
    /// key at all — zero behavior change without AGEND_HOOK_STATE_POC=1.
    /// The flag is now deployed FLEET-WIDE (the shadow experiment), so the
    /// test must clear it explicitly — same env-false-fail class as the
    /// AGEND_RESTART_HANDOFF fix in #1964 (a fleet agent's inherited env must
    /// not flip the contract under test).
    #[test]
    #[serial_test::serial(hook_state_poc)] // #2014: shares AGEND_HOOK_STATE_POC with hook_shadow's env test
    fn hooks_not_injected_without_flag() {
        // Serialize the process-global env flip; restore before asserting so
        // a panic can't leak the cleared flag to other tests. The named serial
        // group above coordinates this with hook_shadow's promotion env test
        // (which mutates the SAME var); the local mutex is the in-module backstop.
        static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("AGEND_HOOK_STATE_POC").ok();
        std::env::remove_var("AGEND_HOOK_STATE_POC");

        let dir = tmp("flag-off");
        // configure_claude needs a git repo; it git-inits itself.
        let result = configure_claude(&dir, Some("agent-y"));
        if let Some(v) = prev {
            std::env::set_var("AGEND_HOOK_STATE_POC", v);
        }
        result.unwrap();
        let cfg: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert!(
            cfg.get("hooks").is_none(),
            "no hooks without the PoC flag, got {cfg}"
        );
        assert!(cfg.get("mcpServers").is_some(), "mcp config still written");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod review_repro_bootstrap_config_cli;
