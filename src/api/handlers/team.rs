//! Team handlers: UPDATE_TEAM (Slice C1), CREATE_TEAM (Slice C2).

use super::HandlerCtx;
use crate::api::ApiEvent;
use serde_json::{json, Value};

pub(crate) fn handle_update_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let team_name = match params["name"].as_str() {
        Some(n) => n.to_string(),
        None => return json!({"ok": false, "error": "missing name"}),
    };
    let before = crate::teams::get_members(ctx.home, &team_name);
    // Snapshot the pre-mutation roster so the TUI event carries the
    // *effective* diff (noop adds like re-adding an existing member
    // must not trigger a pane move).
    let result = crate::teams::update(ctx.home, params);
    let after = crate::teams::get_members(ctx.home, &team_name);
    let before_set: std::collections::HashSet<&String> = before.iter().collect();
    let after_set: std::collections::HashSet<&String> = after.iter().collect();
    let added: Vec<String> = after
        .iter()
        .filter(|m| !before_set.contains(m))
        .cloned()
        .collect();
    let removed: Vec<String> = before
        .iter()
        .filter(|m| !after_set.contains(m))
        .cloned()
        .collect();
    let diff_nonempty = !added.is_empty() || !removed.is_empty();
    if let Some(n) = ctx.notifier {
        if diff_nonempty {
            tracing::info!(team = %team_name, added = ?added, removed = ?removed, "UPDATE_TEAM emitting TeamMembersChanged");
            n.notify(ApiEvent::TeamMembersChanged {
                name: team_name.clone(),
                added: added.clone(),
                removed: removed.clone(),
            });
        }
    }
    // Same condition as the TUI notification: an empty diff means a
    // noop update (e.g. `update_team add` with members already on the
    // roster), no reason to broadcast anything either.
    json!({"ok": true, "result": result})
}

/// #1964 Bug 1: plan `count` member names for `team` as `<team>-N`. Names
/// were index-based (`{team}-{i+1}`), restarting at 1 on EVERY call — a second
/// `create_instance(team=X)` re-picked X-1 and failed "agent already exists"
/// instead of incrementing. Numbering starts at one past the max existing
/// `<team>-N` in fleet.yaml (never re-filling gaps, so a freed number is not
/// resurrected) and skips any candidate still held by fleet.yaml (an entry
/// that exists but is not running would be silently overwritten by
/// `add_instances_to_yaml`) or by the live registry (`taken`, #1441
/// UUID-keyed).
fn plan_member_names(
    fleet: &crate::fleet::FleetConfig,
    team: &str,
    count: usize,
    taken: impl Fn(&str) -> bool,
) -> Vec<String> {
    let prefix = format!("{team}-");
    let mut next_n: u64 = fleet
        .instances
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix)?.parse::<u64>().ok())
        .max()
        .map_or(1, |m| m + 1);
    let mut names = Vec::with_capacity(count);
    while names.len() < count {
        let candidate = format!("{team}-{next_n}");
        next_n += 1;
        if fleet.instances.contains_key(&candidate) || taken(&candidate) {
            tracing::info!(
                team,
                member = %candidate,
                "CREATE_TEAM: name taken — advancing to the next number (#1964)"
            );
            continue;
        }
        names.push(candidate);
    }
    names
}

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_create_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let team_name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing name"}),
    };
    // `backends: [..]` — per-member backend (heterogeneous team).
    // Falls back to repeating `backend` `count` times when absent.
    let per_member_backends: Vec<String> = if let Some(arr) = params["backends"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        let count = params["count"].as_u64().unwrap_or(0) as usize;
        let backend = params["backend"].as_str().unwrap_or("claude").to_string();
        vec![backend; count]
    };
    let count = per_member_backends.len();
    tracing::info!(
        team = team_name,
        count,
        backends = ?per_member_backends,
        "CREATE_TEAM begin"
    );

    // #1964: snapshot fleet.yaml once — Bug-1 numbering scans its instance
    // names, Bug-2 roster routing checks team existence against it.
    let fleet_snapshot = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
        .unwrap_or_default();
    let team_already_exists = fleet_snapshot.teams.contains_key(team_name);

    // Phase 1 — plan every member's fleet.yaml entry (name, backend, dir)
    // before any spawn happens. The full list is written to fleet.yaml
    // before Phase 2 so prepare_instructions sees the complete peer set
    // when generating each member's agend.md.
    //
    // #1964 Bug 1: member names were index-based (`{team}-{i+1}`), restarting
    // at 1 on EVERY call — a second `create_instance(team=X)` re-picked X-1
    // and failed "agent already exists" instead of incrementing. Start from
    // (max existing `<team>-N` in fleet.yaml) + 1 and skip any name still
    // held by fleet.yaml (an entry that exists but isn't running would be
    // silently overwritten by add_instances_to_yaml) or by the live registry
    // (#1441: UUID-keyed, resolve via fleet.yaml).
    let names = plan_member_names(&fleet_snapshot, team_name, per_member_backends.len(), |c| {
        crate::fleet::resolve_uuid(ctx.home, c)
            .is_some_and(|id| crate::agent::lock_registry(ctx.registry).contains_key(&id))
    });
    let mut planned: Vec<(String, String, std::path::PathBuf)> = Vec::new(); // (name, backend, work_dir)
    let mut failed: Vec<String> = Vec::new();
    for (inst_name, backend) in names.into_iter().zip(per_member_backends.iter()) {
        let work_dir = crate::paths::workspace_dir(ctx.home).join(&inst_name);
        planned.push((inst_name, backend.clone(), work_dir));
    }

    if !planned.is_empty() {
        let entries: Vec<(String, crate::fleet::InstanceYamlEntry)> = planned
            .iter()
            .map(|(name, be, wd)| {
                (
                    name.clone(),
                    crate::fleet::InstanceYamlEntry {
                        backend: Some(be.clone()),
                        working_directory: Some(wd.display().to_string()),
                        role: None,
                        instructions: None,
                        // Sprint 54 P1-B Bug 2 fix: see instance.rs:593.
                        source_repo: None,
                        // Sprint 55 P0-B EC4: see instance.rs (gradient).
                        repo: None,
                        github_login: None,
                        args: None,
                        model: None,
                        env: None,
                        ready_pattern: None,
                        command: None,
                        worktree: None,
                        topic_binding_mode: None,
                    },
                )
            })
            .collect();
        let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
            entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
        if let Err(e) = crate::fleet::add_instances_to_yaml(ctx.home, &refs) {
            tracing::warn!(error = %e, "failed to persist team to fleet.yaml");
        }
    }

    // Phase 2 — generate instructions and spawn each planned member. The
    // helper reads fleet.yaml (now complete) for the peer list, so every
    // member boots with a full Identity/Peers block in its agend.md.
    let mut spawned: Vec<(String, String)> = Vec::new();
    let size = crossterm::terminal::size().unwrap_or((120, 40));
    for (inst_name, backend, work_dir) in &planned {
        super::prepare_instructions(ctx.home, inst_name, backend, work_dir, None);
        // #900: resolve the just-written fleet.yaml entry so any
        // operator-supplied `env:` (or `defaults.env`) reaches the
        // spawned process. The entry is in fleet.yaml at this point
        // (Phase 1 above wrote it), so resolve_instance returns the
        // merged defaults+instance map. CREATE_TEAM-time team members
        // currently have env: None at write-time (line 110), but
        // operator hand-edits and downstream replace_instance flows
        // can populate env on the entry between the write and this
        // re-read — we honour whatever the disk says.
        let resolved = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(ctx.home))
            .ok()
            .and_then(|f| f.resolve_instance(inst_name));
        let resolved_env = resolved.as_ref().map(|r| r.env.clone());
        // #2038: boot parity for args + model, same rule as handle_spawn's
        // fleet fallback. CREATE_TEAM-time entries are written with
        // args/model: None (Phase 1 above), but `defaults.args` /
        // `defaults.model` and operator hand-edits between the write and
        // this re-read merge in via resolve_instance.
        let mut member_args = resolved
            .as_ref()
            .map(|r| r.args.clone())
            .unwrap_or_default();
        if let Some(model) = resolved.as_ref().and_then(|r| r.model.as_deref()) {
            crate::backend::Backend::push_model_arg(&mut member_args, backend, model);
        }
        match crate::api::spawn_one(
            ctx.home,
            ctx.registry,
            inst_name,
            backend,
            &member_args,
            crate::backend::SpawnMode::Fresh,
            work_dir,
            size,
            resolved_env.as_ref(),
        ) {
            Ok(_) => {
                tracing::info!(team = team_name, member = %inst_name, backend = %backend, "CREATE_TEAM spawn ok");
                // #966: Every successful spawn gets a channel topic via
                // the hub `ensure_topic_for`. NoChannel is the happy path
                // when no channel is configured; Failed is surfaced via
                // warn (Pushback B: prior `let _ = ch.create_topic()`
                // swallowed errors, same antipattern as pre-#962 silent
                // persist).
                match crate::channel::ensure_topic_for(inst_name) {
                    crate::channel::TopicOutcome::Created(_)
                    | crate::channel::TopicOutcome::NoChannel => {}
                    crate::channel::TopicOutcome::Failed(err) => {
                        tracing::warn!(
                            team = team_name,
                            member = %inst_name,
                            error = %err,
                            "CREATE_TEAM: channel exists but create_topic failed; \
                             member spawn proceeds without topic"
                        );
                    }
                }
                spawned.push((inst_name.clone(), backend.clone()));
            }
            Err(e) => {
                tracing::warn!(team = team_name, member = %inst_name, backend = %backend, error = %e, "CREATE_TEAM spawn failed");
                failed.push(format!("{inst_name}: {e}"));
            }
        }
    }
    tracing::info!(
        team = team_name,
        spawned = spawned.len(),
        failed = failed.len(),
        "CREATE_TEAM spawn phase done"
    );
    if count > 0 && spawned.is_empty() {
        return json!({"ok": false, "error": format!("all {} spawns failed: {}", count, failed.join("; "))});
    }

    let existing: Vec<String> = params["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let spawned_names: Vec<String> = spawned.iter().map(|(n, _)| n.clone()).collect();
    let all_members: Vec<String> = existing
        .into_iter()
        .chain(spawned_names.iter().cloned())
        .collect();

    // Preserve orchestrator from the caller. Without this, routing
    // deploy_template / MCP create_team through CREATE_TEAM would silently
    // drop the orchestrator designation — teams::create accepts it but
    // this handler never forwarded the field.
    let mut team_params = json!({
        "name": team_name,
        "members": all_members,
        "description": params["description"],
    });
    if let Some(orch) = params.get("orchestrator").and_then(|v| v.as_str()) {
        team_params["orchestrator"] = json!(orch);
    }
    // #1833: forward `repository_path` (team source_repo) — same allowlist-drop
    // class as the orchestrator preservation above. #1329 made `deployments.rs`
    // set `team_args["repository_path"]` and route through CREATE_TEAM, but this
    // handler re-marshals `params` into a fresh `team_params` and dropped the
    // field, so every deploy/`api::call(CREATE_TEAM)` produced a team with
    // `source_repo=null` regardless of the template/caller (`teams::create`
    // reads `repository_path`, teams.rs).
    if let Some(repo) = params.get("repository_path").and_then(|v| v.as_str()) {
        team_params["repository_path"] = json!(repo);
    }
    // #1837 (reviewer-2): `accept_from` is the SAME re-marshal allowlist-drop —
    // a documented CREATE_TEAM param (mcp/tools.rs cross-team allowlist, "empty =
    // deny all") that `teams::create` reads from `args["accept_from"]` (teams.rs)
    // but this handler never forwarded. The main path `team(action=create,
    // accept_from=[...])` → `api::call(CREATE_TEAM)` → here → dropped → the
    // operator's cross-team allowlist was silently emptied (fail-closed, but a
    // real functional loss). Forward it to close the root-cause pattern entirely.
    if let Some(af) = params.get("accept_from") {
        team_params["accept_from"] = af.clone();
    }
    // #1964 Bug 2: when the team ALREADY exists (the create_instance(team=X)
    // grow-the-team use case), `teams::create` errors "team already exists" —
    // and that error was swallowed into `result` while the members had ALREADY
    // spawned: named `X-N` but never written to the roster, so
    // `find_team_for` read team=None and the send policy cross-team-blocked
    // same-team traffic. Route the roster write through `teams::update(add)`
    // instead (same fleet.yaml `teams:` store the send policy reads — one
    // write path, one read path). New-team creation is unchanged.
    let result = if team_already_exists {
        tracing::info!(
            team = team_name,
            adding = ?all_members,
            "CREATE_TEAM: team exists — extending roster (#1964)"
        );
        let mut update_params = json!({
            "name": team_name,
            "add": all_members,
        });
        if let Some(orch) = params.get("orchestrator").and_then(|v| v.as_str()) {
            update_params["orchestrator"] = json!(orch);
        }
        crate::teams::update(ctx.home, &update_params)
    } else {
        crate::teams::create(ctx.home, &team_params)
    };
    // Surface a roster-write failure honestly: the members are spawned but
    // team-less (the exact #1964 silent shape) — ok:false lets the caller see
    // it instead of a fake success.
    if let Some(err) = result.get("error").and_then(|e| e.as_str()) {
        tracing::warn!(team = team_name, error = %err, "CREATE_TEAM roster write failed — spawned members are NOT on the team roster");
        return json!({
            "ok": false,
            "error": format!("members spawned but roster write failed: {err}"),
            "spawned": &spawned_names,
        });
    }

    if let Some(n) = ctx.notifier {
        if team_already_exists {
            if !spawned_names.is_empty() {
                tracing::info!(team = team_name, added = ?spawned_names, "CREATE_TEAM emitting TeamMembersChanged (extend)");
                n.notify(ApiEvent::TeamMembersChanged {
                    name: team_name.to_string(),
                    added: spawned_names.clone(),
                    removed: Vec::new(),
                });
            }
        } else if !all_members.is_empty() {
            tracing::info!(team = team_name, members = ?all_members, "CREATE_TEAM emitting TeamCreated");
            n.notify(ApiEvent::TeamCreated {
                name: team_name.to_string(),
                members: all_members.clone(),
            });
        }
    }
    // Broadcast team context to every member so their running prompts
    // learn about the team's name / orchestrator / roster without a
    // respawn. Guard on the same empty-members condition as the TUI
    // event — an empty team would have no targets anyway.
    let mut resp = json!({"ok": true, "result": result, "spawned": &spawned_names});
    if !failed.is_empty() {
        resp["failed"] = json!(failed);
    }
    resp
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
        let names = plan_member_names(&fleet, "sqd", 2, |_| false);
        assert_eq!(names, vec!["sqd-8".to_string(), "sqd-9".to_string()]);

        // A registry-held (running but yaml-less) candidate is skipped too.
        let names = plan_member_names(&fleet, "sqd", 1, |c| c == "sqd-8");
        assert_eq!(names, vec!["sqd-9".to_string()]);

        // Fresh team: starts at 1 and increments WITHIN one call.
        let names = plan_member_names(&fleet, "fresh", 2, |_| false);
        assert_eq!(names, vec!["fresh-1".to_string(), "fresh-2".to_string()]);
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
