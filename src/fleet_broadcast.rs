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
//! Delivery semantics:
//!   - Raw inject (no submit_key) — fleet updates are informational
//!     context, not prompts. The agent picks them up on its next natural
//!     submit rather than auto-responding to each one.
//!   - Compose-aware: when the target pane has received keyboard input
//!     within the last 3 s (`notification_queue::is_composing`), the
//!     update is queued in the pane's notification_queue instead of
//!     hitting the PTY mid-keystroke. The TUI drains the queue once the
//!     pane goes idle.
//!   - Opt-out: `fleet.yaml` instances can set
//!     `receive_fleet_updates: false` to skip broadcasts — used for
//!     user-facing agents (e.g. `general`) where the marker would be
//!     conversational noise.

use crate::agent::AgentRegistry;
use serde_json::json;
use std::path::Path;

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
        let payload = match self {
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
        };
        format!("<fleet-update>\n{payload}\n</fleet-update>\n")
    }
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
/// interrupted. No-op when the candidate list ends up empty (e.g. a
/// solo-deploy without peers).
pub fn broadcast(home: &Path, registry: &AgentRegistry, update: &FleetUpdate) {
    let registered: Vec<String> = {
        let reg = crate::agent::lock_registry(registry);
        reg.keys().cloned().collect()
    };
    let targets = compute_targets(home, &registered, update);
    if targets.is_empty() {
        return;
    }
    let marker = update.render_marker();
    for target in &targets {
        if crate::notification_queue::is_composing(home, target) {
            if let Err(e) = crate::notification_queue::enqueue(home, target, &marker) {
                tracing::warn!(%target, error = %e, "fleet_broadcast queue enqueue failed");
            }
            continue;
        }
        // Raw inject: no inject_prefix, no submit_key. The agent sees
        // the marker in its prompt buffer but nothing submits it; the
        // marker is folded into context on the next natural prompt.
        if let Err(e) = crate::api::call(
            home,
            &json!({
                "method": crate::api::method::INJECT,
                "params": { "name": target, "data": &marker, "raw": true }
            }),
        ) {
            tracing::warn!(%target, error = %e, "fleet_broadcast inject failed");
        }
    }
    tracing::info!(
        kind = ?std::mem::discriminant(update),
        targets = targets.len(),
        "fleet_broadcast delivered"
    );
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

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("agend-broadcast-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
