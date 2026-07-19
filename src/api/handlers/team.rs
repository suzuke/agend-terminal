//! Team handlers: UPDATE_TEAM (Slice C1), CREATE_TEAM (Slice C2).
//!
//! #2454 Slice 13: CREATE_TEAM logic moved to `crate::team_ops` (neutral
//! typed owner). This file retains the API adapter (param parsing +
//! delegation) and UPDATE_TEAM (unchanged).

use super::HandlerCtx;
use crate::api::ApiEvent;
use serde_json::{json, Value};

pub(crate) fn handle_update_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let team_name = match params["name"].as_str() {
        Some(n) => n.to_string(),
        None => return json!({"ok": false, "error": "missing name"}),
    };
    let outcome = crate::teams::update_with_diff(ctx.home, params);
    if outcome.result.get("error").is_none() {
        if let Some(n) = ctx.notifier {
            let diff_nonempty = !outcome.added.is_empty() || !outcome.removed.is_empty();
            if diff_nonempty {
                tracing::info!(team = %team_name, added = ?outcome.added, removed = ?outcome.removed, "UPDATE_TEAM emitting TeamMembersChanged");
                n.notify(ApiEvent::TeamMembersChanged {
                    name: team_name.clone(),
                    added: outcome.added.clone(),
                    removed: outcome.removed.clone(),
                });
            }
        }
    }
    json!({"ok": true, "result": outcome.result})
}

/// #2454 Slice 13: thin API adapter — parses transport-specific params and
/// delegates to the neutral typed `team_ops::create` owner.
pub(crate) fn handle_create_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = match params["name"].as_str() {
        Some(n) => n.to_string(),
        None => return json!({"ok": false, "error": "missing name"}),
    };
    let per_member_backends: Vec<String> = if let Some(arr) = params["backends"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        let count = params["count"].as_u64().unwrap_or(0) as usize;
        let backend = params["backend"].as_str().unwrap_or("claude").to_string();
        vec![backend; count]
    };
    let topic_binding_mode: Option<String> = params["topic_binding"]
        .as_str()
        .filter(|s| matches!(*s, "skip" | "deferred"))
        .map(String::from);
    let existing_members: Vec<String> = params["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let accept_from: Vec<String> = params["accept_from"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    crate::team_ops::create(
        ctx.home,
        crate::team_ops::CreateTeamRequest {
            name,
            per_member_backends,
            existing_members,
            topic_binding_mode,
            orchestrator: params["orchestrator"].as_str().map(String::from),
            description: params["description"].as_str().map(String::from),
            repository_path: params["repository_path"].as_str().map(String::from),
            project_id: params["project_id"].as_str().map(String::from),
            accept_from,
        },
        ctx.registry,
        ctx.notifier.map(|n| n.as_ref()),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-team-1833-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        // Leak empty registries for 'static — acceptable in tests (mirrors the
        // messaging-handler test scaffold).
        let registry: &'static crate::agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static crate::agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        }
    }

    /// §3.9 regression (#1833 + #1837): CREATE_TEAM through the real
    /// `handle_create_team` entry must persist BOTH `repository_path` (→ team
    /// `source_repo`) and `accept_from` (→ team `accept_from`) into the TEAM
    /// block. Pre-fix, the handler re-marshaled `params` into a fresh
    /// `team_params` from an allowlist that dropped both fields, so every deploy
    /// / `api::call(CREATE_TEAM)` produced `source_repo=null` (a #1329 regression)
    /// and an emptied cross-team allowlist (#1837 sibling, fail-closed but a real
    /// functional loss). Regression-proof: delete either forward in
    /// `handle_create_team` and the matching assertion below FAILS.
    #[test]
    fn create_team_persists_repository_path_and_accept_from_to_team_block_1833() {
        let home = tmp_home("create-srcrepo");
        // Minimal fleet.yaml so FleetConfig round-trips cleanly.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances: {}\nteams: {}\n",
        )
        .unwrap();
        let ctx = test_ctx(&home);

        // No `backends`/`count` → no spawn; pre-listed members drive the exact
        // param-marshaling path the bug lives in (the real CREATE_TEAM entry).
        let params = json!({
            "name": "sqdteam",
            "members": ["sqdteam-1", "sqdteam-2"],
            "description": "deploy",
            "repository_path": "/srv/canonical-repo",
            "accept_from": ["peer-team-a", "peer-team-b"],
        });
        let resp = handle_create_team(&params, &ctx);
        assert_eq!(resp["ok"], true, "create must succeed: {resp}");

        // The TEAM block (not just instances) must carry both fields on disk.
        let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let team = cfg
            .teams
            .get("sqdteam")
            .expect("team persisted to fleet.yaml");
        assert_eq!(
            team.source_repo.as_deref(),
            Some(std::path::Path::new("/srv/canonical-repo")),
            "#1833: repository_path must reach the team block's source_repo, got {:?}",
            team.source_repo
        );
        assert_eq!(
            team.accept_from,
            vec!["peer-team-a".to_string(), "peer-team-b".to_string()],
            "#1837: accept_from must reach the team block (not be silently emptied), got {:?}",
            team.accept_from
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_team_error_preserves_outer_success_envelope_2454() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountNotifier(AtomicUsize);

        impl crate::api::ApiNotifier for CountNotifier {
            fn notify(&self, _event: crate::api::ApiEvent) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let home = tmp_home("update-error-envelope");
        crate::teams::create(
            &home,
            &json!({
                "name": "devs",
                "members": ["lead"],
                "orchestrator": "lead",
            }),
        );
        let notifier = Arc::new(CountNotifier(AtomicUsize::new(0)));
        let notifier_trait: Arc<dyn crate::api::ApiNotifier> = notifier.clone();
        let mut ctx = test_ctx(&home);
        ctx.notifier = Some(&notifier_trait);
        let response = handle_update_team(&json!({"name": "devs", "remove": ["lead"]}), &ctx);
        assert_eq!(
            response["ok"], true,
            "API wire envelope changed: {response}"
        );
        assert!(response.get("error").is_none());
        assert!(response["result"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("cannot remove orchestrator")));
        assert_eq!(notifier.0.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests_1964 {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-1964-{}-{}-{}", tag, std::process::id(), id));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        let registry: &'static crate::agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static crate::agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        }
    }

    /// #1964 Bug 1: numbering starts past the max existing `<team>-N` in
    /// fleet.yaml (no per-call restart at 1), never re-fills gaps (a freed
    /// number is not resurrected), skips registry-held names, and ignores
    /// non-numeric member names.
    #[test]
    fn plan_member_names_increments_past_existing_1964() {
        let mut fleet = crate::fleet::FleetConfig::default();
        for existing in ["sqd-1", "sqd-7", "sqd-lead", "other-3"] {
            fleet.instances.insert(
                existing.to_string(),
                crate::fleet::InstanceConfig::default(),
            );
        }
        // Consecutive two-member plan: 8, 9 (max=7 → +1; the gap 2-6 is NOT
        // reused; "sqd-lead"/"other-3" don't poison the parse).
        let names = crate::team_ops::plan_member_names(&fleet, "sqd", 2, |_| false);
        assert_eq!(names, vec!["sqd-8".to_string(), "sqd-9".to_string()]);

        // A registry-held (running but yaml-less) candidate is skipped too.
        let names = crate::team_ops::plan_member_names(&fleet, "sqd", 1, |c| c == "sqd-8");
        assert_eq!(names, vec!["sqd-9".to_string()]);

        // Fresh team: starts at 1 and increments WITHIN one call.
        let names = crate::team_ops::plan_member_names(&fleet, "fresh", 2, |_| false);
        assert_eq!(names, vec!["fresh-1".to_string(), "fresh-2".to_string()]);
    }

    /// #991 PR-B: every planned member's fleet.yaml entry carries the
    /// team-level `topic_binding_mode` (or None when the caller didn't opt
    /// out — unchanged auto-create default).
    #[test]
    fn build_member_entries_carries_topic_binding_mode_991() {
        let planned = vec![
            (
                "sqd-1".to_string(),
                "claude".to_string(),
                std::path::PathBuf::from("/tmp/sqd-1"),
            ),
            (
                "sqd-2".to_string(),
                "claude".to_string(),
                std::path::PathBuf::from("/tmp/sqd-2"),
            ),
        ];

        let skip_entries = crate::team_ops::build_member_entries(&planned, Some("skip"));
        assert_eq!(skip_entries.len(), 2);
        for (_, e) in &skip_entries {
            assert_eq!(e.topic_binding_mode.as_deref(), Some("skip"));
        }

        let auto_entries = crate::team_ops::build_member_entries(&planned, None);
        for (_, e) in &auto_entries {
            assert!(
                e.topic_binding_mode.is_none(),
                "omitted topic_binding must stay None (auto default)"
            );
        }
    }

    /// #1964 Bug 2 (the live repro shape): CREATE_TEAM on an EXISTING team
    /// extends the roster via teams::update(add) — the member lands where the
    /// send policy reads (`teams::find_team_for` over the same fleet.yaml
    /// `teams:` block), so same-team direct send is no longer cross-team
    /// blocked. Pre-fix `teams::create` errored "already exists", the error
    /// was swallowed, and the member stayed team=None. Real fleet.yaml WITH
    /// ids (#1680 lesson: an id-less home lets split read/write paths
    /// converge and hide).
    #[test]
    fn create_team_on_existing_team_extends_roster_1964() {
        let home = tmp_home("extend");
        let lead_id = crate::types::InstanceId::new();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sqd-lead:\n    id: {}\nteams:\n  sqd:\n    members: [sqd-lead]\n    orchestrator: sqd-lead\n",
                lead_id.full()
            ),
        )
        .unwrap();
        let ctx = test_ctx(&home);

        // count=0 + pre-listed member = the no-spawn seam the #1833 test uses;
        // the roster-routing under test is identical for spawned members
        // (all_members = existing ++ spawned).
        let resp = handle_create_team(&json!({"name": "sqd", "members": ["sqd-9"]}), &ctx);
        assert_eq!(resp["ok"], true, "extend must succeed: {resp}");

        // The send policy's read path (find_team_for over fleet.yaml teams)
        // must see the new member on the SAME team as the lead.
        let member_team = crate::teams::find_team_for(&home, "sqd-9")
            .expect("#1964: extended member must be on a team (was team=None)");
        assert_eq!(member_team.name, "sqd");
        let lead_team = crate::teams::find_team_for(&home, "sqd-lead").unwrap();
        assert_eq!(
            lead_team.name, member_team.name,
            "#1964: lead and new member same team → same-team send not blocked"
        );
        // Existing roster untouched (extend, not clobber).
        assert!(member_team.members.contains(&"sqd-lead".to_string()));
        assert_eq!(
            member_team.orchestrator.as_deref(),
            Some("sqd-lead"),
            "existing orchestrator preserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1964: a roster-write failure no longer masquerades as success — the
    /// handler returns ok:false naming the spawned-but-rosterless members
    /// (pre-fix: ok:true with the error buried in `result`, which the MCP
    /// layer then dropped entirely).
    #[test]
    fn create_team_roster_write_failure_surfaces_1964() {
        let home = tmp_home("surface");
        // Member already on ANOTHER team → teams::update(add) rejects
        // (one-agent-one-team) → the handler must surface ok:false.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances: {}\nteams:\n  sqd:\n    members: []\n  rival:\n    members: [taken-1]\n",
        )
        .unwrap();
        let ctx = test_ctx(&home);
        let resp = handle_create_team(&json!({"name": "sqd", "members": ["taken-1"]}), &ctx);
        assert_eq!(
            resp["ok"], false,
            "#1964: roster-write failure must not report success: {resp}"
        );
        assert!(
            resp["error"]
                .as_str()
                .unwrap_or("")
                .contains("roster write failed"),
            "error names the failure: {resp}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests_2525 {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-2525-{}-{}-{}", tag, std::process::id(), id));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        let registry: &'static crate::agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static crate::agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
            capability: crate::api::RestartCapability::Unsupported,
            app_restart: None,
            post_flush: crate::api::app_restart::PostFlushSlot::new(),
            shutdown: None,
        }
    }

    /// #2525: CREATE_TEAM on an EXISTING team (the #1964 already-exists
    /// branch, routed through `teams::update`) must forward `repository_path`
    /// the same way the new-team path does (#1833 above) — same
    /// allowlist-drop class, just never fixed for this branch. Pre-fix,
    /// `update_params` only carries `name`/`add`/`orchestrator`, so a second
    /// `create_instance(team=X, repository_path=Y)` call silently drops Y and
    /// `teams::update`'s set-or-preserve keeps the team's prior (possibly
    /// absent) source_repo.
    #[test]
    fn create_team_on_existing_team_forwards_repository_path_2525() {
        let home = tmp_home("repo-forward");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances: {}\nteams:\n  sqd:\n    members: [sqd-lead]\n    orchestrator: sqd-lead\n",
        )
        .unwrap();
        let ctx = test_ctx(&home);

        let resp = handle_create_team(
            &json!({
                "name": "sqd",
                "members": ["sqd-9"],
                "repository_path": "/srv/canonical-repo",
            }),
            &ctx,
        );
        assert_eq!(resp["ok"], true, "extend must succeed: {resp}");

        let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let team = cfg.teams.get("sqd").expect("team still present");
        assert_eq!(
            team.source_repo.as_deref(),
            Some(std::path::Path::new("/srv/canonical-repo")),
            "#2525: repository_path must reach the team block on the already-exists \
             (teams::update) branch, got {:?}",
            team.source_repo
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
