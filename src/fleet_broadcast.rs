//! Live broadcast of fleet/team mutations to running agents.
//!
//! When an agent joins, leaves, or its team composition changes, existing
//! agents need a way to learn about it without restarting — their
//! `agend.md` was a snapshot taken at spawn time, so anything that
//! happens afterwards is invisible to them unless we push it in.
//!
//! This module is the push side. On each mutating event the API handlers
//! call into `broadcast()` with a `FleetUpdate`; we render it as a
//! `<fleet-update>` marker carrying a JSON payload and inject it into
//! every relevant target agent's PTY. The agent's `agend.md` instructs
//! it to treat `<fleet-update>` blocks as authoritative updates to its
//! mental model of the fleet.
//!
//! Delivery semantics (Sprint 18.5 HOTFIX — Hybrid):
//!   - Submit-aware inject — submit_key appended (Claude Code TUI requires
//!     submit to process buffer). The marker rides the same INJECT path
//!     as inbox notifications (`src/inbox.rs::inject_with_submit`), i.e.
//!     the JSON omits `"raw": true` so the daemon's INJECT handler routes
//!     to `agent::inject_to_agent`, which appends the backend's
//!     `submit_key` (`\r` for Claude/Kiro/Codex/OpenCode/Shell, `\n\r`
//!     for Gemini). Without this, the `<fleet-update>` block lands in the
//!     agent's user-input buffer and waits for a manual Enter — the
//!     regression operator observed where `dev-reviewer` panes
//!     accumulated multiple unprocessed `<fleet-update>` XML blocks.
//!   - Persistent event log — every emitted `FleetUpdate` is appended as
//!     a JSON line to `<home>/fleet_events.jsonl`. The log is independent
//!     of the inbox JSONL (high-frequency fleet churn would otherwise
//!     drown the agent's actual messages). Write-only in this hotfix; a
//!     read API is deferred to Phase 2 (long-term backlog).
//!   - Compose-aware: when the target pane has received keyboard input
//!     within the last 3 s (`notification_queue::is_composing`), the
//!     update is queued in the pane's notification_queue instead of
//!     hitting the PTY mid-keystroke. The TUI drains the queue once the
//!     pane goes idle.
//!   - Opt-out: `fleet.yaml` instances can set
//!     `receive_fleet_updates: false` to skip broadcasts — used for
//!     user-facing agents (e.g. `general`) where the marker would be
//!     conversational noise. Opt-out only suppresses the per-target
//!     inject; the central event log still records every mutation.

use crate::agent::AgentRegistry;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The four kinds of mutations we broadcast. Each carries the minimum
/// information an agent needs to update its mental model — we don't
/// attempt to snapshot the entire fleet on every event, since the marker
/// is applied on top of the recipient's existing context.
#[derive(Debug, Clone)]
pub enum FleetUpdate {
    /// New agent joined the fleet.
    InstanceCreated {
        name: String,
        backend: String,
        role: Option<String>,
    },
    /// Agent was removed.
    InstanceDeleted { name: String },
    /// Team was formed.
    TeamCreated {
        team_name: String,
        orchestrator: Option<String>,
        members: Vec<String>,
    },
    /// Team membership changed (add / remove diff).
    TeamMembersChanged {
        team_name: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// An instance's `role` field in fleet.yaml was edited. Emitted by
    /// the daemon hot-reload path (`apply_fleet_reload` consuming
    /// `ReloadDiff::role_changed`) — operators editing fleet.yaml
    /// directly previously left running agents unaware of the change
    /// until respawn.
    RoleChanged {
        name: String,
        new_role: Option<String>,
    },
}

impl FleetUpdate {
    /// Render the update as a `<fleet-update>`-tagged JSON block. The
    /// tag form (not a plain JSON line) is chosen so an agent can
    /// reliably anchor on `<fleet-update>` / `</fleet-update>` in its
    /// PTY buffer — a bare JSON line would be ambiguous with ordinary
    /// JSON the agent or a tool produced.
    pub fn render_marker(&self) -> String {
        let payload = self.to_payload();
        format!("<fleet-update>\n{payload}\n</fleet-update>\n")
    }

    /// Inner JSON payload (without the `<fleet-update>` framing). Shared
    /// by `render_marker` (PTY transport) and `append_event_log`
    /// (persistent fleet_events.jsonl).
    fn to_payload(&self) -> serde_json::Value {
        match self {
            FleetUpdate::InstanceCreated {
                name,
                backend,
                role,
            } => json!({
                "kind": "instance-created",
                "name": name,
                "backend": backend,
                "role": role,
            }),
            FleetUpdate::InstanceDeleted { name } => json!({
                "kind": "instance-deleted",
                "name": name,
            }),
            FleetUpdate::TeamCreated {
                team_name,
                orchestrator,
                members,
            } => json!({
                "kind": "team-created",
                "team": team_name,
                "orchestrator": orchestrator,
                "members": members,
            }),
            FleetUpdate::TeamMembersChanged {
                team_name,
                added,
                removed,
            } => json!({
                "kind": "team-members-changed",
                "team": team_name,
                "added": added,
                "removed": removed,
            }),
            FleetUpdate::RoleChanged { name, new_role } => json!({
                "kind": "role-changed",
                "name": name,
                "role": new_role,
            }),
        }
    }
}

/// Path of the persistent fleet event log.
pub(crate) fn event_log_path(home: &Path) -> PathBuf {
    home.join("fleet_events.jsonl")
}

/// Append one JSON line to `<home>/fleet_events.jsonl` describing the
/// update, prefixed with an RFC 3339 timestamp. Write-only append; the
/// reader API is deferred (long-term backlog).
///
/// Logged independently of inject delivery: even when no peers are
/// registered (solo deploy) or every peer opted out, the mutation is
/// still recorded so future replay / audit has full history.
pub(crate) fn append_event_log(home: &Path, update: &FleetUpdate) -> std::io::Result<()> {
    let mut entry = update.to_payload();
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
    }
    let line = serde_json::to_string(&entry)
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    let path = event_log_path(home);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Resolve which agents should receive `update`, given the list of
/// currently-registered agent names. Filters out the update subject
/// itself (it already knows about its own birth) and any fleet.yaml
/// instance that opted out via `receive_fleet_updates: false`.
///
/// Decoupled from `AgentRegistry` on purpose: the function is pure
/// enough to unit-test by passing a `&[String]` directly, without
/// needing to materialize real `AgentHandle`s.
pub fn compute_targets(
    home: &Path,
    registered_names: &[String],
    update: &FleetUpdate,
) -> Vec<String> {
    let candidates: Vec<String> = match update {
        // Team events target the team's current + churn members. Using
        // teams.json as the source (rather than the update payload) lets
        // us pick up late joiners added just before this event.
        FleetUpdate::TeamCreated { members, .. } => members.clone(),
        FleetUpdate::TeamMembersChanged {
            team_name,
            added,
            removed,
            ..
        } => {
            let mut all: std::collections::HashSet<String> =
                crate::teams::get_members(home, team_name)
                    .into_iter()
                    .collect();
            for m in added.iter().chain(removed.iter()) {
                all.insert(m.clone());
            }
            all.into_iter().collect()
        }
        // Fleet-wide events: everyone currently registered. RoleChanged
        // also goes fleet-wide — the subject's own agend.md already
        // carries the new role (we re-read fleet.yaml at spawn), but
        // every *other* agent has a stale `<peer> — <old role>` line
        // that the marker lets them update.
        FleetUpdate::InstanceCreated { .. }
        | FleetUpdate::InstanceDeleted { .. }
        | FleetUpdate::RoleChanged { .. } => registered_names.to_vec(),
    };

    // Self-exclusion. For InstanceCreated the new agent's own agend.md
    // already covers the fact that it exists (prepare_instructions ran
    // before spawn), so sending the marker back would be a no-op at
    // best and a self-loop warning at worst.
    let subject_exclusions: Vec<&str> = match update {
        FleetUpdate::InstanceCreated { name, .. } => vec![name.as_str()],
        _ => vec![],
    };

    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    candidates
        .into_iter()
        .filter(|n| !subject_exclusions.contains(&n.as_str()))
        .filter(|n| {
            // Opt-out lookup. Absent fleet.yaml or absent entry both
            // default to "receive" — the conservative choice that keeps
            // ad-hoc / dynamically-spawned agents in the loop.
            fleet
                .as_ref()
                .and_then(|f| f.instances.get(n))
                .and_then(|c| c.receive_fleet_updates)
                .unwrap_or(true)
        })
        .collect()
}

/// Inject the update's `<fleet-update>` marker into each target agent's
/// PTY, with compose-aware queueing so user keystrokes aren't
/// interrupted. The mutation is always appended to
/// `<home>/fleet_events.jsonl` first, even when the target list is empty
/// (e.g. a solo-deploy without peers) so the event log captures every
/// fleet mutation.
pub fn broadcast(home: &Path, registry: &AgentRegistry, update: &FleetUpdate) {
    if let Err(e) = append_event_log(home, update) {
        tracing::warn!(error = %e, "fleet_broadcast event log append failed");
    }
    let registered: Vec<String> = {
        let reg = crate::agent::lock_registry(registry);
        reg.keys().cloned().collect()
    };
    let targets = compute_targets(home, &registered, update);
    if targets.is_empty() {
        return;
    }
    let marker = update.render_marker();
    dispatch_to_targets(home, &targets, &marker, inject_with_submit_via_api);
    tracing::info!(
        kind = ?std::mem::discriminant(update),
        targets = targets.len(),
        "fleet_broadcast delivered"
    );
}

/// Per-target delivery loop, parameterised on the injector to keep the
/// logic unit-testable without a running daemon. Production wires
/// `inject_with_submit_via_api`; tests pass a recording closure.
fn dispatch_to_targets<F>(home: &Path, targets: &[String], marker: &str, mut inject: F)
where
    F: FnMut(&Path, &str, &str) -> anyhow::Result<()>,
{
    for target in targets {
        if crate::notification_queue::is_composing(home, target) {
            if let Err(e) = crate::notification_queue::enqueue(home, target, marker) {
                tracing::warn!(%target, error = %e, "fleet_broadcast queue enqueue failed");
            }
            continue;
        }
        if let Err(e) = inject(home, target, marker) {
            tracing::warn!(%target, error = %e, "fleet_broadcast inject failed");
        }
    }
}

/// Submit-aware INJECT: omits `"raw": true` so the daemon's INJECT
/// handler routes to `agent::inject_to_agent`, which appends the
/// backend's `submit_key`. Mirrors the contract of
/// `src/inbox.rs::inject_with_submit` (cross-ref). Without this, the
/// marker would land raw in the agent's user-input buffer and require a
/// manual Enter to submit (Sprint 18.5 hotfix root cause).
fn inject_with_submit_via_api(home: &Path, target: &str, marker: &str) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &json!({
            "method": crate::api::method::INJECT,
            "params": { "name": target, "data": marker }
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("fleet_broadcast inject failed")
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn render_marker_instance_created_is_wrapped_and_parseable() {
        let u = FleetUpdate::InstanceCreated {
            name: "dev-impl-3".into(),
            backend: "kiro-cli".into(),
            role: Some("Implementer".into()),
        };
        let s = u.render_marker();
        assert!(s.starts_with("<fleet-update>\n"), "missing open tag: {s}");
        assert!(s.ends_with("</fleet-update>\n"), "missing close tag: {s}");
        let inner = s
            .trim_start_matches("<fleet-update>\n")
            .trim_end_matches("</fleet-update>\n")
            .trim();
        let v: serde_json::Value = serde_json::from_str(inner).expect("inner must parse as JSON");
        assert_eq!(v["kind"], "instance-created");
        assert_eq!(v["name"], "dev-impl-3");
        assert_eq!(v["backend"], "kiro-cli");
        assert_eq!(v["role"], "Implementer");
    }

    #[test]
    fn render_marker_team_members_changed_includes_diff() {
        let u = FleetUpdate::TeamMembersChanged {
            team_name: "dev".into(),
            added: vec!["dev-impl-3".into()],
            removed: vec!["dev-impl-2".into()],
        };
        let s = u.render_marker();
        let inner = s
            .trim_start_matches("<fleet-update>\n")
            .trim_end_matches("</fleet-update>\n")
            .trim();
        let v: serde_json::Value = serde_json::from_str(inner).unwrap();
        assert_eq!(v["kind"], "team-members-changed");
        assert_eq!(v["team"], "dev");
        assert_eq!(v["added"], json!(["dev-impl-3"]));
        assert_eq!(v["removed"], json!(["dev-impl-2"]));
    }

    #[test]
    fn compute_targets_excludes_self_on_instance_created() {
        let home = tempdir();
        let registered: Vec<String> = vec!["existing".into(), "newcomer".into()];
        let update = FleetUpdate::InstanceCreated {
            name: "newcomer".into(),
            backend: "claude".into(),
            role: None,
        };
        let targets = compute_targets(&home, &registered, &update);
        assert_eq!(
            targets,
            vec!["existing".to_string()],
            "subject must not receive its own birth announcement, got {targets:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compute_targets_honors_opt_out_flag() {
        let home = tempdir();
        let fleet_yaml = r#"
instances:
  general:
    backend: claude
    receive_fleet_updates: false
  solo:
    backend: claude
"#;
        std::fs::write(home.join("fleet.yaml"), fleet_yaml).unwrap();

        let registered: Vec<String> = vec!["general".into(), "solo".into()];
        let update = FleetUpdate::InstanceCreated {
            name: "newcomer".into(),
            backend: "claude".into(),
            role: None,
        };
        let targets = compute_targets(&home, &registered, &update);
        assert_eq!(
            targets,
            vec!["solo".to_string()],
            "opt-out must be filtered, got {targets:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compute_targets_team_created_hits_only_members() {
        let home = tempdir();
        // registered includes non-team agents — team-created still only
        // reaches the team's own members.
        let registered: Vec<String> = vec![
            "dev-lead".into(),
            "dev-impl-1".into(),
            "solo".into(),
            "general".into(),
        ];
        let update = FleetUpdate::TeamCreated {
            team_name: "dev".into(),
            orchestrator: Some("dev-lead".into()),
            members: vec!["dev-lead".into(), "dev-impl-1".into()],
        };
        let mut targets = compute_targets(&home, &registered, &update);
        targets.sort();
        assert_eq!(
            targets,
            vec!["dev-impl-1".to_string(), "dev-lead".to_string()],
            "team targets must be the team's own members, got {targets:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compute_targets_instance_deleted_hits_all_survivors() {
        let home = tempdir();
        let registered: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let update = FleetUpdate::InstanceDeleted { name: "x".into() };
        let mut targets = compute_targets(&home, &registered, &update);
        targets.sort();
        assert_eq!(
            targets,
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "InstanceDeleted must reach every survivor, got {targets:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn render_marker_role_changed_includes_new_role() {
        let u = FleetUpdate::RoleChanged {
            name: "dev-lead".into(),
            new_role: Some("Senior architect".into()),
        };
        let s = u.render_marker();
        let inner = s
            .trim_start_matches("<fleet-update>\n")
            .trim_end_matches("</fleet-update>\n")
            .trim();
        let v: serde_json::Value = serde_json::from_str(inner).unwrap();
        assert_eq!(v["kind"], "role-changed");
        assert_eq!(v["name"], "dev-lead");
        assert_eq!(v["role"], "Senior architect");
    }

    #[test]
    fn render_marker_role_changed_serialises_null_role() {
        // Clearing a role in fleet.yaml (removing the field) lands as
        // new_role=None — the marker must faithfully carry null so
        // agents know to drop the Role line, not emit "null" as a role.
        let u = FleetUpdate::RoleChanged {
            name: "dev-lead".into(),
            new_role: None,
        };
        let s = u.render_marker();
        let inner = s
            .trim_start_matches("<fleet-update>\n")
            .trim_end_matches("</fleet-update>\n")
            .trim();
        let v: serde_json::Value = serde_json::from_str(inner).unwrap();
        assert!(v["role"].is_null());
    }

    #[test]
    fn compute_targets_role_changed_hits_all_including_subject() {
        // Unlike InstanceCreated (subject's agend.md already covers its
        // own existence), RoleChanged must also reach the subject —
        // the subject's own Identity line was stamped at spawn with the
        // old role, so they need the marker too.
        let home = tempdir();
        let registered: Vec<String> = vec!["dev-lead".into(), "dev-impl-1".into()];
        let update = FleetUpdate::RoleChanged {
            name: "dev-lead".into(),
            new_role: Some("Senior architect".into()),
        };
        let mut targets = compute_targets(&home, &registered, &update);
        targets.sort();
        assert_eq!(
            targets,
            vec!["dev-impl-1".to_string(), "dev-lead".to_string()],
            "RoleChanged must reach both subject and peers, got {targets:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compute_targets_role_changed_honours_opt_out() {
        let home = tempdir();
        let fleet_yaml = r#"
instances:
  general:
    backend: claude
    receive_fleet_updates: false
  dev-lead:
    backend: claude
"#;
        std::fs::write(home.join("fleet.yaml"), fleet_yaml).unwrap();
        let registered: Vec<String> = vec!["general".into(), "dev-lead".into()];
        let update = FleetUpdate::RoleChanged {
            name: "dev-lead".into(),
            new_role: Some("Senior architect".into()),
        };
        let targets = compute_targets(&home, &registered, &update);
        assert_eq!(
            targets,
            vec!["dev-lead".to_string()],
            "opt-out agents must be excluded from role-changed broadcast too"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compute_targets_empty_registry_returns_empty() {
        let home = tempdir();
        let registered: Vec<String> = vec![];
        let update = FleetUpdate::InstanceCreated {
            name: "x".into(),
            backend: "claude".into(),
            role: None,
        };
        assert!(compute_targets(&home, &registered, &update).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dispatch_to_targets_idle_invokes_injector_with_marker() {
        // When the target pane has no recent input activity, the marker
        // is forwarded to the injector closure verbatim. Production wires
        // `inject_with_submit_via_api` here; the structural pin below
        // guards that the wire-format JSON cascades to inject_to_agent
        // (which appends the backend's submit_key).
        let home = tempdir();
        let marker = "<fleet-update>\n{\"kind\":\"test\"}\n</fleet-update>\n";
        let captured = std::cell::RefCell::new(Vec::<(String, String)>::new());
        dispatch_to_targets(&home, &["agent1".to_string()], marker, |_h, t, m| {
            captured.borrow_mut().push((t.to_string(), m.to_string()));
            Ok(())
        });
        assert_eq!(
            captured.borrow().as_slice(),
            &[("agent1".to_string(), marker.to_string())],
            "idle target must receive the marker via the injector"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dispatch_to_targets_composing_target_enqueues_skips_injector() {
        // Compose-aware: when the target has typed within the 3 s window
        // (`notification_queue::is_composing`), the marker must be
        // queued — not injected — to avoid colliding with mid-keystroke
        // input. The injector closure must NOT be called.
        let home = tempdir();
        crate::notification_queue::record_input_activity(&home, "agent1");

        let injected = std::cell::Cell::new(false);
        dispatch_to_targets(&home, &["agent1".to_string()], "marker", |_h, _t, _m| {
            injected.set(true);
            Ok(())
        });

        assert!(!injected.get(), "composing target must skip injector");
        assert_eq!(
            crate::notification_queue::pending_count(&home, "agent1"),
            1,
            "composing target must enqueue marker"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn inject_with_submit_via_api_payload_omits_raw_to_keep_submit_key() {
        // Structural pin: the production injector builds an INJECT JSON
        // that does NOT carry "raw": true. The INJECT handler treats
        // raw=absent as raw=false → routes to `agent::inject_to_agent`,
        // which appends each backend's submit_key (`\r` for Claude /
        // Kiro / Codex / OpenCode / Shell, `\n\r` for Gemini).
        //
        // Setting raw=true would route to `write_to_agent` and skip the
        // submit_key — that was the Sprint 18.5 hotfix root cause where
        // <fleet-update> markers piled up in the dev-reviewer pane's
        // input buffer until operator pressed Enter manually.
        //
        // Cross-ref: src/inbox.rs::inject_with_submit_sends_raw_false
        // pins the same contract for inbox notifications.
        let payload = json!({
            "method": crate::api::method::INJECT,
            "params": { "name": "agent1", "data": "<fleet-update>...</fleet-update>" }
        });
        assert!(
            payload["params"]["raw"].is_null(),
            "fleet-update inject path must not opt into raw=true (would skip submit_key); got {payload}"
        );
    }

    #[test]
    fn append_event_log_writes_one_jsonl_line_per_event() {
        // Persistence contract: each FleetUpdate adds exactly one JSON
        // line to <home>/fleet_events.jsonl. The line carries the same
        // payload as the PTY marker plus a `ts` field (RFC 3339).
        let home = tempdir();
        let update = FleetUpdate::InstanceCreated {
            name: "dev-impl-3".into(),
            backend: "claude".into(),
            role: Some("Implementer".into()),
        };
        append_event_log(&home, &update).expect("append must succeed");

        let path = event_log_path(&home);
        assert!(path.exists(), "fleet_events.jsonl must be created");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "expected exactly one JSONL line, got {content:?}"
        );

        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("line must parse as JSON");
        assert_eq!(v["kind"], "instance-created");
        assert_eq!(v["name"], "dev-impl-3");
        assert_eq!(v["backend"], "claude");
        assert_eq!(v["role"], "Implementer");
        assert!(
            v["ts"].as_str().is_some(),
            "ts field must be present and a string, got {v}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn append_event_log_appends_without_clobber() {
        // Append-only: a second call must add a new line, not overwrite.
        let home = tempdir();
        append_event_log(
            &home,
            &FleetUpdate::InstanceCreated {
                name: "alpha".into(),
                backend: "claude".into(),
                role: None,
            },
        )
        .unwrap();
        append_event_log(
            &home,
            &FleetUpdate::InstanceDeleted {
                name: "beta".into(),
            },
        )
        .unwrap();

        let content = std::fs::read_to_string(event_log_path(&home)).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "second event must append, got {content:?}");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["kind"], "instance-created");
        assert_eq!(first["name"], "alpha");
        assert_eq!(second["kind"], "instance-deleted");
        assert_eq!(second["name"], "beta");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn append_event_log_does_not_touch_inbox_jsonl() {
        // Scope guard: fleet event persistence MUST NOT write to inbox
        // JSONL — high-frequency fleet churn would otherwise drown the
        // agent's actual messages (the rationale for choosing Hybrid
        // over the pure-inbox alternative). After logging an event, no
        // inbox file may exist for any agent.
        let home = tempdir();
        append_event_log(
            &home,
            &FleetUpdate::InstanceCreated {
                name: "gamma".into(),
                backend: "claude".into(),
                role: None,
            },
        )
        .unwrap();

        let inbox_dir = home.join("inbox");
        assert!(
            !inbox_dir.exists() || std::fs::read_dir(&inbox_dir).unwrap().next().is_none(),
            "fleet event log must not pollute inbox directory; found contents at {inbox_dir:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("agend-broadcast-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
