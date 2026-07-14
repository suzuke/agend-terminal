use super::{fleet_yaml_path, InstanceYamlEntry, TeamConfig};
use anyhow::{Context, Result};
use std::path::Path;

fn atomic_write_yaml(home: &Path, doc: &serde_yaml_ng::Value) -> Result<()> {
    let yaml = serde_yaml_ng::to_string(doc).context("Failed to serialize fleet.yaml")?;
    let fleet_path = fleet_yaml_path(home);
    let result = crate::store::atomic_write(&fleet_path, yaml.as_bytes())
        .context("Failed to atomic-write fleet.yaml");
    super::invalidate_cache();
    result
}

fn acquire_lock(home: &Path) -> Result<crate::store::FileFlockGuard> {
    let lock_path = home.join(".fleet.yaml.lock");
    crate::store::acquire_file_lock(&lock_path).context("failed to acquire fleet lock")
}

pub(crate) fn mutate_fleet_yaml(
    home: &Path,
    default_content: &str,
    mutate: impl FnOnce(&mut serde_yaml_ng::Value) -> Result<bool>,
) -> Result<()> {
    let fleet_path = fleet_yaml_path(home);
    if default_content.is_empty() && !fleet_path.exists() {
        return Ok(());
    }
    let _lock = acquire_lock(home)?;
    let content =
        std::fs::read_to_string(&fleet_path).unwrap_or_else(|_| default_content.to_string());
    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&content).context("Failed to parse fleet.yaml")?;
    let changed = mutate(&mut doc)?;
    if changed {
        atomic_write_yaml(home, &doc)
    } else {
        Ok(())
    }
}

pub fn add_instance_to_yaml(home: &Path, name: &str, config: &InstanceYamlEntry) -> Result<()> {
    add_instances_to_yaml(home, &[(name, config)])
}

pub fn add_instances_to_yaml(home: &Path, entries: &[(&str, &InstanceYamlEntry)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    mutate_fleet_yaml(home, "instances: {}\n", |doc| {
        if doc.get("instances").is_none() {
            doc["instances"] = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        let instances = doc
            .get_mut("instances")
            .and_then(|v| v.as_mapping_mut())
            .context("instances is not a mapping")?;

        let mut conflicts: Vec<super::merge::FieldConflict> = Vec::new();

        for (name, config) in entries {
            let key = serde_yaml_ng::Value::String(name.to_string());
            if let Some(serde_yaml_ng::Value::Mapping(existing)) = instances.get_mut(&key) {
                match super::merge::merge_instance_into_existing(name, existing, config) {
                    Ok(()) => tracing::info!(%name, "merged instance update into fleet.yaml"),
                    Err(conflict) => conflicts.push(conflict),
                }
            } else {
                // Workspace-identity uniqueness (fail-closed, ATOMIC under the fleet lock
                // held by mutate_fleet_yaml): a NEW instance must not resolve to the same
                // canonical working directory as any existing one — including an explicit
                // path equal to another instance's DEFAULT, or a symlink/case-only/
                // nonexistent alias of it (see paths::workspace_identity). This is the
                // incident guard: two instances sharing a workspace produced split-brain
                // identity when project provisioning rewrote it last-writer-wins.
                let candidate_wd = crate::paths::effective_working_dir(
                    home,
                    name,
                    config.working_directory.as_deref(),
                );
                if let Some(collider) =
                    find_workspace_identity_collision(home, instances, name, &candidate_wd)
                {
                    return Err(anyhow::anyhow!(
                        "workspace identity collision: new instance '{name}' would resolve to the \
                         same canonical working directory as existing instance '{collider}' ({}). \
                         Refusing to admit a duplicate workspace identity (fail-closed).",
                        candidate_wd.display()
                    ));
                }
                let inst = super::merge::build_instance_mapping(config);
                instances.insert(key, serde_yaml_ng::Value::Mapping(inst));
                tracing::info!(%name, "added new instance to fleet.yaml");
            }
        }

        if !conflicts.is_empty() {
            let mut diff_lines: Vec<String> = Vec::with_capacity(conflicts.len());
            for c in &conflicts {
                diff_lines.push(format!("  - {c}"));
            }
            return Err(anyhow::anyhow!(
                "fleet.yaml merge conflict ({} field(s)):\n{}",
                conflicts.len(),
                diff_lines.join("\n")
            ));
        }
        Ok(true)
    })
}

/// Return the name of an existing instance whose EFFECTIVE working directory shares
/// `candidate_wd`'s canonical identity ([`crate::paths::workspace_identity`]), else `None`.
/// Excludes `candidate_name` itself (a same-name merge/update is not a duplicate). Read-only
/// over the parsed fleet mapping; the caller holds the fleet lock, so this is atomic w.r.t.
/// concurrent admissions.
fn find_workspace_identity_collision(
    home: &Path,
    instances: &serde_yaml_ng::Mapping,
    candidate_name: &str,
    candidate_wd: &Path,
) -> Option<String> {
    let candidate_id = crate::paths::workspace_identity(candidate_wd);
    for (k, v) in instances {
        let Some(existing_name) = k.as_str() else {
            continue;
        };
        if existing_name == candidate_name {
            continue;
        }
        let explicit = v.get("working_directory").and_then(|x| x.as_str());
        let existing_wd = crate::paths::effective_working_dir(home, existing_name, explicit);
        if crate::paths::workspace_identity(&existing_wd) == candidate_id {
            return Some(existing_name.to_string());
        }
    }
    None
}

pub fn remove_instance_from_yaml(home: &Path, name: &str) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            instances.remove(serde_yaml_ng::Value::String(name.to_string()));
        }
        tracing::info!(%name, "removed instance from fleet.yaml");
        Ok(true)
    })
}

pub fn remove_instances_from_yaml(home: &Path, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            for name in names {
                instances.remove(serde_yaml_ng::Value::String(name.clone()));
            }
        }
        Ok(true)
    })
}

fn mapping_is_telegram(m: &serde_yaml_ng::Mapping) -> bool {
    m.get(serde_yaml_ng::Value::String("type".into()))
        .and_then(|v| v.as_str())
        == Some("telegram")
}

fn set_group_id(m: &mut serde_yaml_ng::Mapping, new_group_id: i64) {
    m.insert(
        serde_yaml_ng::Value::String("group_id".into()),
        serde_yaml_ng::Value::Number(new_group_id.into()),
    );
}

pub fn update_channel_telegram_group_id(home: &Path, new_group_id: i64) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        // Singular `channel:` form.
        if let Some(channel) = doc.get_mut("channel").and_then(|v| v.as_mapping_mut()) {
            if mapping_is_telegram(channel) {
                set_group_id(channel, new_group_id);
                tracing::info!(new_group_id, "fleet.yaml channel.group_id rewritten");
                return Ok(true);
            }
        }
        // MED-3: plural `channels:` form. `normalize()` collapses the first entry
        // by sorted name into the active channel, so that entry is the telegram
        // channel the supergroup migration actually applies to. Mirror that
        // selection and persist `new_group_id` there. Without this branch a
        // channels-only fleet silently returned Ok(false) (treated as success),
        // so the new group_id was never written and the stale id reloaded on the
        // next restart — re-triggering the migration error in a loop.
        let first_name = doc
            .get("channels")
            .and_then(|v| v.as_mapping())
            .and_then(|m| {
                let mut names: Vec<&str> = m.keys().filter_map(|k| k.as_str()).collect();
                names.sort();
                names.first().map(|s| s.to_string())
            });
        if let Some(first) = first_name {
            if let Some(entry) = doc
                .get_mut("channels")
                .and_then(|v| v.as_mapping_mut())
                .and_then(|m| m.get_mut(serde_yaml_ng::Value::String(first.clone())))
                .and_then(|v| v.as_mapping_mut())
            {
                if mapping_is_telegram(entry) {
                    set_group_id(entry, new_group_id);
                    tracing::info!(
                        new_group_id,
                        channel = %first,
                        "fleet.yaml channels.<name>.group_id rewritten"
                    );
                    return Ok(true);
                }
            }
        }
        // No telegram channel matched in either form — surface the miss instead
        // of returning a silent Ok(false) the caller mistakes for a persisted
        // migration.
        tracing::warn!(
            new_group_id,
            "update_channel_telegram_group_id: no telegram channel found in fleet.yaml \
             (`channel:` or `channels:`) — group_id NOT persisted"
        );
        Ok(false)
    })
}

#[allow(dead_code)]
pub fn update_instance_field(
    home: &Path,
    name: &str,
    field: &str,
    value: serde_yaml_ng::Value,
) -> Result<bool> {
    let fleet_path = fleet_yaml_path(home);
    if !fleet_path.exists() {
        tracing::warn!(
            instance = name,
            field = field,
            reason = "fleet_yaml_missing",
            "update_instance_field skipped — silent no-op detected"
        );
        return Ok(false);
    }
    let mut persisted = false;
    let mut skip_reason: Option<&'static str> = None;
    mutate_fleet_yaml(home, "", |doc| {
        let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) else {
            skip_reason = Some("instances_section_missing");
            return Ok(false);
        };
        let key = serde_yaml_ng::Value::String(name.to_string());
        let Some(inst_val) = instances.get_mut(&key) else {
            skip_reason = Some("instance_entry_missing");
            return Ok(false);
        };
        let Some(inst) = inst_val.as_mapping_mut() else {
            skip_reason = Some("instance_entry_not_mapping");
            return Ok(false);
        };
        inst.insert(serde_yaml_ng::Value::String(field.to_string()), value);
        persisted = true;
        Ok(true)
    })?;
    if !persisted {
        tracing::warn!(
            instance = name,
            field = field,
            reason = skip_reason.unwrap_or("unknown"),
            "update_instance_field skipped — silent no-op detected"
        );
    }
    Ok(persisted)
}

/// #2744: multi-field companion to [`update_instance_field`] — sets and
/// REMOVES keys on one instance entry inside a SINGLE lock/write transaction
/// (set_model's atomic mutual clearing: a concurrent resolve must never see
/// `model` and `model_tier` both present mid-update). Same skip semantics:
/// `Ok(false)` + warn when the file/section/entry is missing — callers MUST
/// escalate that to a hard error, never ignore it.
pub fn update_instance_fields(
    home: &Path,
    name: &str,
    set: &[(&str, serde_yaml_ng::Value)],
    remove: &[&str],
) -> Result<bool> {
    let fleet_path = fleet_yaml_path(home);
    if !fleet_path.exists() {
        tracing::warn!(
            instance = name,
            reason = "fleet_yaml_missing",
            "update_instance_fields skipped — silent no-op detected"
        );
        return Ok(false);
    }
    let mut persisted = false;
    let mut skip_reason: Option<&'static str> = None;
    mutate_fleet_yaml(home, "", |doc| {
        let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) else {
            skip_reason = Some("instances_section_missing");
            return Ok(false);
        };
        let key = serde_yaml_ng::Value::String(name.to_string());
        let Some(inst_val) = instances.get_mut(&key) else {
            skip_reason = Some("instance_entry_missing");
            return Ok(false);
        };
        let Some(inst) = inst_val.as_mapping_mut() else {
            skip_reason = Some("instance_entry_not_mapping");
            return Ok(false);
        };
        for (field, value) in set {
            inst.insert(
                serde_yaml_ng::Value::String((*field).to_string()),
                value.clone(),
            );
        }
        for field in remove {
            inst.remove(serde_yaml_ng::Value::String((*field).to_string()));
        }
        persisted = true;
        Ok(true)
    })?;
    if !persisted {
        tracing::warn!(
            instance = name,
            reason = skip_reason.unwrap_or("unknown"),
            "update_instance_fields skipped — silent no-op detected"
        );
    }
    Ok(persisted)
}

fn team_config_to_mapping(config: &TeamConfig) -> serde_yaml_ng::Mapping {
    let mut team = serde_yaml_ng::Mapping::new();
    let members_seq: Vec<serde_yaml_ng::Value> = config
        .members
        .iter()
        .map(|m| serde_yaml_ng::Value::String(m.clone()))
        .collect();
    team.insert(
        "members".into(),
        serde_yaml_ng::Value::Sequence(members_seq),
    );
    if let Some(ref orch) = config.orchestrator {
        team.insert(
            "orchestrator".into(),
            serde_yaml_ng::Value::String(orch.clone()),
        );
    }
    if let Some(ref desc) = config.description {
        team.insert(
            "description".into(),
            serde_yaml_ng::Value::String(desc.clone()),
        );
    }
    if let Some(ref ts) = config.created_at {
        team.insert(
            "created_at".into(),
            serde_yaml_ng::Value::String(ts.clone()),
        );
    }
    if let Some(ref sr) = config.source_repo {
        team.insert(
            "source_repo".into(),
            serde_yaml_ng::Value::String(sr.display().to_string()),
        );
    }
    if let Some(ref pid) = config.project_id {
        team.insert(
            "project_id".into(),
            serde_yaml_ng::Value::String(pid.clone()),
        );
    }
    if !config.accept_from.is_empty() {
        let seq: Vec<serde_yaml_ng::Value> = config
            .accept_from
            .iter()
            .map(|s| serde_yaml_ng::Value::String(s.clone()))
            .collect();
        team.insert("accept_from".into(), serde_yaml_ng::Value::Sequence(seq));
    }
    team
}

/// First `(new_member, existing_team)` where one of `new_members` already
/// belongs to a team in `teams`. Drives the one-agent-one-team invariant from
/// INSIDE the fleet.yaml lock so a concurrent `teams::create`/`teams::update`
/// cannot double-book a member past the stale pre-write snapshot
/// (#CR-2026-06-14 teams.rs:174 create; t-50 update). `exclude_team` skips the
/// team being UPDATED — its own current members are not a conflict with itself.
fn first_member_conflict(
    teams: &serde_yaml_ng::Mapping,
    new_members: &[String],
    exclude_team: Option<&str>,
) -> Option<(String, String)> {
    for (team_key, team_val) in teams {
        let Some(team_name) = team_key.as_str() else {
            continue;
        };
        if Some(team_name) == exclude_team {
            continue;
        }
        let Some(members) = team_val.get("members").and_then(|m| m.as_sequence()) else {
            continue;
        };
        for existing in members.iter().filter_map(|v| v.as_str()) {
            if new_members.iter().any(|m| m == existing) {
                return Some((existing.to_string(), team_name.to_string()));
            }
        }
    }
    None
}

/// Outcome of a lock-held team write (#t-91 F1/F2). Distinguishes the two
/// rejection reasons so callers log accurate messages instead of a blanket
/// "team already exists". `MemberConflict` carries the offending member and the
/// team it already belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamWriteOutcome {
    /// The team was created (add) or updated (update).
    Written,
    /// add only: a team with this name already exists.
    NameExists,
    /// update only: no team with this name exists to update.
    NotFound,
    /// One-agent-one-team rejection: `member` already belongs to `team`.
    MemberConflict { member: String, team: String },
}

pub fn add_team_to_yaml(home: &Path, name: &str, config: &TeamConfig) -> Result<TeamWriteOutcome> {
    let mut outcome = TeamWriteOutcome::Written;
    mutate_fleet_yaml(home, "teams: {}\n", |doc| {
        if doc.get("teams").is_none() {
            doc["teams"] = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        let teams = doc
            .get_mut("teams")
            .and_then(|v| v.as_mapping_mut())
            .context("teams is not a mapping")?;
        let key = serde_yaml_ng::Value::String(name.to_string());
        if teams.contains_key(&key) {
            outcome = TeamWriteOutcome::NameExists;
            return Ok(false);
        }
        // #CR-2026-06-14 (teams.rs:174 TOCTOU): enforce one-agent-one-team INSIDE
        // the lock-held closure, not only on the pre-write snapshot in
        // `teams::create`. Two concurrent creates of different team names that
        // share a member both pass the stale snapshot check; this lock-held
        // re-check is the authoritative guard that stops the second write from
        // double-booking the member.
        if let Some((member, existing_team)) = first_member_conflict(teams, &config.members, None) {
            tracing::info!(
                %member, %existing_team, attempted_team = %name,
                "team create rejected under lock: member already in another team (one-agent-one-team)"
            );
            outcome = TeamWriteOutcome::MemberConflict {
                member,
                team: existing_team,
            };
            return Ok(false);
        }
        teams.insert(
            key,
            serde_yaml_ng::Value::Mapping(team_config_to_mapping(config)),
        );
        outcome = TeamWriteOutcome::Written;
        tracing::info!(%name, "added team to fleet.yaml");
        Ok(true)
    })?;
    Ok(outcome)
}

pub fn remove_team_from_yaml(home: &Path, name: &str) -> Result<bool> {
    let mut removed = false;
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(teams) = doc.get_mut("teams").and_then(|v| v.as_mapping_mut()) {
            if teams
                .remove(serde_yaml_ng::Value::String(name.to_string()))
                .is_some()
            {
                removed = true;
                tracing::info!(%name, "removed team from fleet.yaml");
            }
        }
        Ok(removed)
    })?;
    Ok(removed)
}

pub fn update_team_in_yaml(
    home: &Path,
    name: &str,
    config: &TeamConfig,
) -> Result<TeamWriteOutcome> {
    let mut outcome = TeamWriteOutcome::NotFound;
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(teams) = doc.get_mut("teams").and_then(|v| v.as_mapping_mut()) {
            let key = serde_yaml_ng::Value::String(name.to_string());
            if teams.contains_key(&key) {
                // CR-2026-06-14 (t-50): enforce one-agent-one-team UNDER the lock
                // on the UPDATE path too. `teams::update`'s pre-write
                // `find_team_for_member` runs on a stale snapshot (TOCTOU), so two
                // concurrent updates adding the same member to different teams
                // could both pass it and both write (sibling of the #2189 create
                // TOCTOU). Exclude THIS team — its own members aren't a conflict.
                if let Some((member, existing_team)) =
                    first_member_conflict(teams, &config.members, Some(name))
                {
                    tracing::info!(
                        %member, %existing_team, attempted_team = %name,
                        "team update rejected under lock: member already in another team (one-agent-one-team)"
                    );
                    outcome = TeamWriteOutcome::MemberConflict {
                        member,
                        team: existing_team,
                    };
                    return Ok(false);
                }
                teams.insert(
                    key,
                    serde_yaml_ng::Value::Mapping(team_config_to_mapping(config)),
                );
                outcome = TeamWriteOutcome::Written;
                return Ok(true);
            }
        }
        // team absent → outcome stays NotFound, no write
        Ok(false)
    })?;
    Ok(outcome)
}

pub fn migrate_teams_json_to_yaml(home: &Path) -> Result<()> {
    use serde::Deserialize;

    let teams_json = home.join("teams.json");
    if !teams_json.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&teams_json)
        .with_context(|| format!("read teams.json: {}", teams_json.display()))?;

    #[derive(Deserialize)]
    struct LegacyTeamStore {
        #[serde(default)]
        teams: Vec<LegacyTeam>,
    }
    #[derive(Deserialize)]
    struct LegacyTeam {
        name: String,
        #[serde(default)]
        members: Vec<String>,
        #[serde(default)]
        orchestrator: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
    }

    let store: LegacyTeamStore = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %teams_json.display(),
                "teams.json migration: parse failed, leaving file in place");
            return Ok(());
        }
    };
    if store.teams.is_empty() {
        let migrated = home.join("teams.json.migrated");
        std::fs::rename(&teams_json, &migrated)
            .with_context(|| format!("rename {} → {}", teams_json.display(), migrated.display()))?;
        tracing::info!("teams.json migration: empty store, renamed to .migrated");
        return Ok(());
    }
    for team in &store.teams {
        let cfg = TeamConfig {
            members: team.members.clone(),
            orchestrator: team.orchestrator.clone(),
            description: team.description.clone(),
            created_at: team.created_at.clone(),
            source_repo: None,
            project_id: None,
            accept_from: Vec::new(),
        };
        match add_team_to_yaml(home, &team.name, &cfg) {
            Ok(TeamWriteOutcome::Written) => {
                tracing::info!(name = %team.name, "migrated team to fleet.yaml");
                tracing::warn!(
                    name = %team.name,
                    "migrated team from legacy teams.json without source_repo — \
                     set via `team(action=update, name={}, source_repo=...)` or \
                     daemon will fall to working_directory/stub Tier 3/4 in dispatch_auto_bind_lease",
                    team.name
                );
                crate::event_log::log(
                    home,
                    "team_migration_missing_source_repo",
                    &team.name,
                    "legacy teams.json schema had no source_repo; \
                     set via team(action=update) to avoid Tier 4 stub fallback",
                );
            }
            Ok(TeamWriteOutcome::NameExists) => tracing::info!(name = %team.name,
                "team already in fleet.yaml, skipping migration entry"),
            // #t-91 F2: accurate message — a member-conflict skip is NOT a
            // name-collision. (Behavior unchanged: the second team is still
            // dropped from the migration; the source rows survive in
            // teams.json.migrated. Making it non-lossy is a separate change.)
            Ok(TeamWriteOutcome::MemberConflict {
                member,
                team: existing,
            }) => {
                tracing::warn!(name = %team.name, %member, existing_team = %existing,
                    "migration skipped team: member already in another team (one-agent-one-team); entry dropped (recoverable from teams.json.migrated)")
            }
            // update-only outcome; unreachable on the add path, logged defensively.
            Ok(TeamWriteOutcome::NotFound) => tracing::warn!(name = %team.name,
                "unexpected NotFound from add_team_to_yaml during migration"),
            Err(e) => {
                tracing::warn!(name = %team.name, error = %e,
                    "team migration failed, leaving teams.json in place");
                return Err(e);
            }
        }
    }
    let migrated = home.join("teams.json.migrated");
    std::fs::rename(&teams_json, &migrated)
        .with_context(|| format!("rename {} → {}", teams_json.display(), migrated.display()))?;
    tracing::info!(
        count = store.teams.len(),
        "teams.json migration complete, renamed to .migrated"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-persist-med3-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// §3.9 (MED-3): a channels-only (plural) fleet must persist a telegram
    /// supergroup migration. Pre-fix, `update_channel_telegram_group_id` only
    /// handled the singular `channel:` form and returned a silent `Ok(false)`
    /// for `channels:` → the new group_id was never written → the stale id
    /// reloaded on restart, re-triggering the migration error in a loop.
    /// Regression-proof: drop the plural branch and the persisted id stays stale.
    #[test]
    fn update_group_id_persists_into_channels_plural_med3() {
        let home = tmp_home("plural");
        std::fs::write(
            fleet_yaml_path(&home),
            "channels:\n  tg:\n    type: telegram\n    group_id: -100111\n\
             instances:\n  A:\n    backend: claude\n",
        )
        .unwrap();

        update_channel_telegram_group_id(&home, -100999).expect("update must succeed");

        // Persisted under channels.tg.group_id (not silently dropped).
        let doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&std::fs::read_to_string(fleet_yaml_path(&home)).unwrap())
                .unwrap();
        assert_eq!(
            doc["channels"]["tg"]["group_id"].as_i64(),
            Some(-100999),
            "MED-3: channels-only group_id must be rewritten"
        );

        // And the real loader+normalize surfaces the new id as the active channel
        // (the on-restart path that previously reloaded the stale id).
        let cfg = crate::fleet::FleetConfig::load(&fleet_yaml_path(&home)).unwrap();
        match cfg.channel {
            Some(crate::fleet::ChannelConfig::Telegram { group_id, .. }) => assert_eq!(
                group_id, -100999,
                "MED-3: normalize must surface the persisted id, not the stale one"
            ),
            other => panic!("expected a telegram active channel, got {other:?}"),
        }

        std::fs::remove_dir_all(&home).ok();
    }

    /// A fleet with NO telegram channel must not silently report success: the
    /// helper returns `Ok` (best-effort) but logs a miss and writes nothing.
    #[test]
    fn update_group_id_no_telegram_channel_writes_nothing_med3() {
        let home = tmp_home("none");
        let yaml = "instances:\n  A:\n    backend: claude\n";
        std::fs::write(fleet_yaml_path(&home), yaml).unwrap();

        update_channel_telegram_group_id(&home, -100999).expect("best-effort Ok");

        // No channel/channels key was synthesized.
        let after = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
        assert!(
            !after.contains("group_id"),
            "MED-3: no telegram channel → no group_id written, got:\n{after}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Admission guard (workspace-identity): a NEW instance whose EXPLICIT working_directory
    /// equals another instance's DEFAULT (`<home>/workspace/<name>`) is refused fail-closed,
    /// and the structured refusal NAMES BOTH instances.
    #[test]
    fn add_instance_refuses_explicit_colliding_with_another_default() {
        let home = tmp_home("collide-default");
        // beta with NO explicit working_directory → default <home>/workspace/beta.
        add_instance_to_yaml(&home, "beta", &InstanceYamlEntry::default()).unwrap();
        let beta_default = crate::paths::workspace_dir(&home)
            .join("beta")
            .display()
            .to_string();
        let err = add_instance_to_yaml(
            &home,
            "alpha",
            &InstanceYamlEntry {
                working_directory: Some(beta_default),
                ..Default::default()
            },
        )
        .expect_err("explicit-vs-default collision must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("alpha") && msg.contains("beta"),
            "refusal must name BOTH instances: {msg}"
        );
        assert!(
            msg.contains("workspace identity collision"),
            "structured refusal expected: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Admission guard: two instances with the SAME explicit working_directory refuse.
    #[test]
    fn add_instance_refuses_duplicate_explicit_working_directory() {
        let home = tmp_home("collide-explicit");
        let shared = home.join("shared-ws").display().to_string();
        add_instance_to_yaml(
            &home,
            "beta",
            &InstanceYamlEntry {
                working_directory: Some(shared.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = add_instance_to_yaml(
            &home,
            "alpha",
            &InstanceYamlEntry {
                working_directory: Some(shared),
                ..Default::default()
            },
        )
        .expect_err("duplicate explicit working_directory must be refused");
        assert!(
            err.to_string().contains("alpha") && err.to_string().contains("beta"),
            "refusal must name both: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Admission guard: a symlinked-parent alias of another instance's dir is refused
    /// (nonexistent leaf; deepest-existing-ancestor canonicalization resolves the symlink).
    #[cfg(unix)]
    #[test]
    fn add_instance_refuses_symlinked_parent_alias() {
        use std::os::unix::fs::symlink;
        let home = tmp_home("collide-symlink");
        let real = home.join("real");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, home.join("link")).unwrap();
        add_instance_to_yaml(
            &home,
            "beta",
            &InstanceYamlEntry {
                working_directory: Some(real.join("ws").display().to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        // alpha targets the SAME dir via the symlinked parent (leaf "ws" doesn't exist).
        let err = add_instance_to_yaml(
            &home,
            "alpha",
            &InstanceYamlEntry {
                working_directory: Some(home.join("link").join("ws").display().to_string()),
                ..Default::default()
            },
        )
        .expect_err("symlinked-parent alias must be refused");
        assert!(
            err.to_string().contains("alpha") && err.to_string().contains("beta"),
            "refusal must name both: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Admission guard: a case-only alias of another instance's dir is refused (conservative
    /// case-fold), on case-sensitive and case-insensitive filesystems alike.
    #[test]
    fn add_instance_refuses_case_only_alias() {
        let home = tmp_home("collide-case");
        add_instance_to_yaml(
            &home,
            "beta",
            &InstanceYamlEntry {
                working_directory: Some(home.join("ws").display().to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let err = add_instance_to_yaml(
            &home,
            "alpha",
            &InstanceYamlEntry {
                working_directory: Some(home.join("WS").display().to_string()),
                ..Default::default()
            },
        )
        .expect_err("case-only alias must be refused");
        assert!(
            err.to_string().contains("alpha") && err.to_string().contains("beta"),
            "refusal must name both: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Admission guard is ATOMIC: two concurrent creates targeting the SAME working directory
    /// admit EXACTLY ONE (the fleet lock serializes check-and-insert); the other refuses.
    #[test]
    fn add_instance_concurrent_creates_admit_exactly_one() {
        let home = tmp_home("collide-concurrent");
        let shared = home.join("race-ws").display().to_string();
        let (h1, h2) = (home.clone(), home.clone());
        let (s1, s2) = (shared.clone(), shared.clone());
        let t1 = std::thread::spawn(move || {
            add_instance_to_yaml(
                &h1,
                "one",
                &InstanceYamlEntry {
                    working_directory: Some(s1),
                    ..Default::default()
                },
            )
        });
        let t2 = std::thread::spawn(move || {
            add_instance_to_yaml(
                &h2,
                "two",
                &InstanceYamlEntry {
                    working_directory: Some(s2),
                    ..Default::default()
                },
            )
        });
        let (r1, r2) = (t1.join().unwrap(), t2.join().unwrap());
        assert_eq!(
            r1.is_ok() as u8 + r2.is_ok() as u8,
            1,
            "exactly one concurrent create may win the shared workspace: r1={r1:?} r2={r2:?}"
        );
        // And fleet.yaml persisted exactly one of them.
        let names = crate::fleet::FleetConfig::load(&fleet_yaml_path(&home))
            .unwrap()
            .instance_names();
        let persisted = names.iter().filter(|n| *n == "one" || *n == "two").count();
        assert_eq!(persisted, 1, "exactly one instance must persist: {names:?}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression: distinct working directories (two DEFAULTS) are ADMITTED — the identity
    /// guard must not over-refuse legitimate non-colliding instances.
    #[test]
    fn add_instance_allows_distinct_working_directories() {
        let home = tmp_home("distinct");
        add_instance_to_yaml(&home, "beta", &InstanceYamlEntry::default()).unwrap();
        add_instance_to_yaml(&home, "alpha", &InstanceYamlEntry::default())
            .expect("distinct default workspaces must be admitted");
        let names = crate::fleet::FleetConfig::load(&fleet_yaml_path(&home))
            .unwrap()
            .instance_names();
        assert!(
            names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()),
            "both distinct instances must persist: {names:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod review_repro_deployments_health_teams;
