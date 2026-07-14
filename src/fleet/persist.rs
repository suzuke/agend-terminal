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
    if default_content.is_empty() && !fleet_path.exists() && fleet_path.symlink_metadata().is_err()
    {
        return Ok(());
    }
    let _lock = acquire_lock(home)?;
    let link_entry_exists = fleet_path.symlink_metadata().is_ok();
    let content = match std::fs::read_to_string(&fleet_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !link_entry_exists => {
            default_content.to_string()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::anyhow!(
                "fleet.yaml at {}: dangling symlink — filesystem evidence exists \
                 but target is missing, refusing to overwrite with defaults",
                fleet_path.display()
            ))
            .context("opaque I/O — refusing to default");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("fleet.yaml read: {e}"))
                .context("opaque I/O — refusing to default")
        }
    };
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

        // 1. Apply every insert/merge first.
        let mut conflicts: Vec<super::merge::FieldConflict> = Vec::new();
        for (name, config) in entries {
            let key = serde_yaml_ng::Value::String(name.to_string());
            if let Some(serde_yaml_ng::Value::Mapping(existing)) = instances.get_mut(&key) {
                match super::merge::merge_instance_into_existing(name, existing, config) {
                    Ok(()) => tracing::info!(%name, "merged instance update into fleet.yaml"),
                    Err(conflict) => conflicts.push(conflict),
                }
            } else {
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

        // 2. Whole-registry workspace-identity uniqueness (fail-closed, ATOMIC
        // under the fleet lock held by mutate_fleet_yaml). Runs for EVERY affected
        // instance — a NEW insert OR a merge that CHANGED `working_directory` — so
        // the existing-key merge path can no longer slip a colliding update past
        // admission (root finding 5). An instance must not resolve to the same
        // canonical working directory as any OTHER — including an explicit path
        // equal to another's DEFAULT, or a symlink/case-only/nonexistent alias
        // (see paths::workspace_identity). This is the split-brain incident guard.
        for (name, _) in entries {
            let key = serde_yaml_ng::Value::String(name.to_string());
            let explicit = instances
                .get(&key)
                .and_then(|v| v.get("working_directory"))
                .and_then(|x| x.as_str());
            let candidate_wd = crate::paths::effective_working_dir(home, name, explicit);
            if let Some(collider) =
                find_workspace_identity_collision(home, instances, name, &candidate_wd)
            {
                return Err(anyhow::anyhow!(
                    "workspace identity collision: instance '{name}' would resolve to the same \
                     canonical working directory as existing instance '{collider}' ({}). Refusing \
                     to admit a duplicate workspace identity (fail-closed).",
                    candidate_wd.display()
                ));
            }
        }
        Ok(true)
    })
}

/// Return the name of an existing instance whose EFFECTIVE working directory shares
/// `candidate_wd`'s canonical identity ([`crate::paths::workspace_identity`]), else `None`.
/// Excludes `candidate_name` itself (a same-name merge/update is not a duplicate). Read-only
/// over the parsed fleet mapping; the caller holds the fleet lock, so this is atomic w.r.t.
/// concurrent admissions.
fn expand_tilde(path: &Path) -> std::path::PathBuf {
    super::resolve::expand_tilde_path(&path.to_string_lossy())
}

fn find_workspace_identity_collision(
    home: &Path,
    instances: &serde_yaml_ng::Mapping,
    candidate_name: &str,
    candidate_wd: &Path,
) -> Option<String> {
    let candidate_id = crate::paths::workspace_identity(&expand_tilde(candidate_wd));
    for (k, v) in instances {
        let Some(existing_name) = k.as_str() else {
            continue;
        };
        if existing_name == candidate_name {
            continue;
        }
        let explicit = v.get("working_directory").and_then(|x| x.as_str());
        let existing_wd = crate::paths::effective_working_dir(home, existing_name, explicit);
        if crate::paths::workspace_identity(&expand_tilde(&existing_wd)) == candidate_id {
            return Some(existing_name.to_string());
        }
    }
    None
}

/// Read-only load of the parsed `instances` mapping from fleet.yaml.
/// Missing file → empty mapping (legitimate first-run).
/// Opaque I/O, parse error, or wrong-shape `instances` → `Err` (fail-closed).
fn load_instances_mapping(home: &Path) -> Result<serde_yaml_ng::Mapping> {
    let fleet_path = fleet_yaml_path(home);
    let c = match std::fs::read_to_string(&fleet_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_yaml_ng::Mapping::new());
        }
        Err(e) => {
            anyhow::bail!("fleet.yaml read: {}: {e}", fleet_path.display());
        }
    };
    let doc =
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&c).with_context(|| "fleet.yaml parse")?;
    match doc.get("instances") {
        None => Ok(serde_yaml_ng::Mapping::new()),
        Some(v) => v.as_mapping().cloned().ok_or_else(|| {
            anyhow::anyhow!("fleet.yaml: `instances` is not a mapping (wrong shape — refusing)")
        }),
    }
}

/// Read-only workspace-identity collision preflight: returns a DIFFERENT existing
/// instance sharing `name`'s canonical identity at `candidate_wd`, else `None`.
/// The create path uses this to refuse BEFORE creating any worktree/directory
/// (root finding 2 — no partial filesystem/git state on refusal); the atomic
/// `add_instances_to_yaml` remains the race-safe authority.
pub fn workspace_identity_collision(
    home: &Path,
    name: &str,
    candidate_wd: &Path,
) -> Option<String> {
    match load_instances_mapping(home) {
        Ok(mapping) => find_workspace_identity_collision(home, &mapping, name, candidate_wd),
        Err(e) => Some(format!("fleet unreadable — refusing collision check: {e}")),
    }
}

/// Boot/spawn admission for a PRE-EXISTING duplicate fleet (root finding 5): if
/// another instance shares `name`'s canonical workspace identity and sorts
/// EARLIER, return that earlier owner's name — the caller must then SKIP booting
/// `name`. The lexicographically-first instance in an identity group is the
/// single deterministic owner that boots; the rest defer. `None` = `name` is
/// unique or is itself the owner → boot it. The create path prevents NEW
/// duplicates, so this only fires for a legacy / hand-corrupted fleet.yaml.
pub fn duplicate_identity_owner_before(
    home: &Path,
    name: &str,
    candidate_wd: &Path,
) -> Option<String> {
    let candidate_id = crate::paths::workspace_identity(&expand_tilde(candidate_wd));
    let mapping = match load_instances_mapping(home) {
        Ok(m) => m,
        Err(e) => return Some(format!("fleet unreadable — refusing boot admission: {e}")),
    };
    let mut earliest: Option<String> = None;
    for (k, v) in &mapping {
        let Some(existing_name) = k.as_str() else {
            continue;
        };
        // Only an EARLIER-sorting instance can preempt this boot.
        if existing_name >= name {
            continue;
        }
        let explicit = v.get("working_directory").and_then(|x| x.as_str());
        let existing_wd = crate::paths::effective_working_dir(home, existing_name, explicit);
        if crate::paths::workspace_identity(&expand_tilde(&existing_wd)) == candidate_id
            && earliest.as_deref().is_none_or(|e| existing_name < e)
        {
            earliest = Some(existing_name.to_string());
        }
    }
    earliest
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

    #[test]
    fn merge_adding_colliding_working_directory_is_refused_whole_registry() {
        // finding 5: a MERGE that ADDS a working_directory (previously default →
        // absent) pointing at another instance's identity is refused by the
        // whole-registry post-pass. The pre-fix scan ran ONLY on the new-key
        // branch, so this existing-key merge slipped a colliding update past.
        let home = tmp_home("merge-add-collide");
        add_instance_to_yaml(&home, "alice", &InstanceYamlEntry::default()).unwrap();
        add_instance_to_yaml(&home, "bob", &InstanceYamlEntry::default()).unwrap();
        let alice_default = crate::paths::workspace_dir(&home)
            .join("alice")
            .display()
            .to_string();
        let err = add_instance_to_yaml(
            &home,
            "bob",
            &InstanceYamlEntry {
                working_directory: Some(alice_default),
                ..Default::default()
            },
        )
        .expect_err("a merge that repoints bob onto alice's workspace must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("workspace identity collision"),
            "collision error: {msg}"
        );
        assert!(
            msg.contains("bob") && msg.contains("alice"),
            "refusal names both instances: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn duplicate_identity_owner_before_admits_only_the_earliest() {
        // finding 5: a PRE-EXISTING duplicate fleet.yaml (two instances at ONE
        // workspace — legacy / hand-edited) boots only the lexicographically-first
        // "owner"; later ones defer. This is the boot/spawn admission helper.
        let home = tmp_home("dup-owner");
        let shared = home.join("shared-ws");
        // Hand-write a corrupt fleet the create path would never admit. Single-
        // quoted YAML keeps the (possibly backslashed) path literal cross-platform.
        let yaml = format!(
            "instances:\n  alice:\n    working_directory: '{p}'\n  bob:\n    working_directory: '{p}'\n",
            p = shared.display()
        );
        std::fs::write(fleet_yaml_path(&home), yaml).unwrap();
        // bob (later) defers to the earlier owner alice.
        assert_eq!(
            duplicate_identity_owner_before(&home, "bob", &shared).as_deref(),
            Some("alice")
        );
        // alice (earliest in its identity group) is the owner → boots.
        assert_eq!(
            duplicate_identity_owner_before(&home, "alice", &shared),
            None
        );
        std::fs::remove_dir_all(&home).ok();
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

    // --- root review r8 RED: 2 deterministic failures (task-46776) ---

    #[test]
    fn root_review_tilde_and_expanded_workspace_identity_collide() {
        let home = tmp_home("tilde-collide");
        let unique = format!("agend-tilde-test-{}", std::process::id());
        let tilde_path = format!("~/{unique}");
        let expanded = crate::fleet::resolve::expand_tilde_path(&tilde_path);

        add_instance_to_yaml(
            &home,
            "alice",
            &InstanceYamlEntry {
                working_directory: Some(tilde_path),
                ..Default::default()
            },
        )
        .unwrap();

        let err = add_instance_to_yaml(
            &home,
            "bob",
            &InstanceYamlEntry {
                working_directory: Some(expanded.display().to_string()),
                ..Default::default()
            },
        )
        .expect_err("tilde and expanded paths must collide as the same workspace identity");
        let msg = err.to_string();
        assert!(
            msg.contains("workspace identity collision"),
            "expected collision refusal: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn root_review_dangling_fleet_symlink_is_opaque_not_first_run() {
        use std::os::unix::fs::symlink;
        let home = tmp_home("dangling-symlink");
        let fleet_path = super::fleet_yaml_path(&home);
        symlink("/nonexistent/fleet.yaml", &fleet_path).unwrap();
        assert!(
            fleet_path.symlink_metadata().is_ok(),
            "symlink entry must exist"
        );
        assert!(
            !fleet_path.exists(),
            "symlink target must NOT exist (dangling)"
        );

        let err = add_instance_to_yaml(&home, "alpha", &InstanceYamlEntry::default())
            .expect_err("dangling fleet.yaml symlink must be refused as opaque evidence");
        let msg = err.to_string();
        assert!(
            msg.contains("dangling") || msg.contains("opaque") || msg.contains("refusing"),
            "structured refusal expected: {msg}"
        );
        assert!(
            fleet_path.symlink_metadata().is_ok(),
            "dangling symlink must not be removed"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- root review r9 RED: 2 deterministic failures (task-46776) ---

    #[test]
    fn root_review_r9_boot_tilde_and_expanded_duplicate_deferred() {
        let home = tmp_home("r10-tilde-boot");
        let unique = format!("agend-r10-tilde-boot-{}", std::process::id());
        let tilde_path = format!("~/{unique}");
        let expanded = crate::fleet::resolve::expand_tilde_path(&tilde_path);

        let yaml = format!(
            "instances:\n  alice:\n    working_directory: '{tilde_path}'\n  bob:\n    working_directory: '{}'\n",
            expanded.display()
        );
        std::fs::write(fleet_yaml_path(&home), yaml).unwrap();

        let result = duplicate_identity_owner_before(&home, "bob", &expanded);
        assert_eq!(
            result.as_deref(),
            Some("alice"),
            "boot admission must detect tilde-vs-expanded duplicate: \
             alice owns ~/{u} which is the same directory as {e}, \
             but duplicate_identity_owner_before does not normalize tilde \
             so the identity comparison misses the collision",
            u = unique,
            e = expanded.display(),
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn root_review_r9_remove_dangling_fleet_symlink_is_not_silent_success() {
        use std::os::unix::fs::symlink;
        let home = tmp_home("r10-dangling-rm");
        let fleet_path = super::fleet_yaml_path(&home);
        symlink("/nonexistent/fleet.yaml", &fleet_path).unwrap();
        assert!(
            fleet_path.symlink_metadata().is_ok(),
            "symlink entry must exist"
        );
        assert!(
            !fleet_path.exists(),
            "symlink target must NOT exist (dangling)"
        );

        let result = remove_instance_from_yaml(&home, "alpha");
        assert!(
            result.is_err(),
            "remove_instance on a dangling fleet.yaml symlink must refuse, \
             not silently return Ok — the empty-default fast path \
             (default_content.is_empty() && !fleet_path.exists()) bypasses \
             the post-lock dangling-symlink guard because .exists() follows \
             symlinks and returns false for dangling targets"
        );
        assert!(
            fleet_path.symlink_metadata().is_ok(),
            "dangling symlink must not be removed"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod review_repro_deployments_health_teams;
