use crate::backend::Backend;
use std::path::{Path, PathBuf};

pub(crate) fn expand_tilde_path(raw: &str) -> PathBuf {
    if raw == "~" {
        dirs_home().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs_home()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    }
}

fn dirs_home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn resolve_ready_pattern(
    inst: &super::InstanceConfig,
    defaults: &super::InstanceDefaults,
    preset: &crate::backend::BackendPreset,
    name: &str,
) -> Option<Option<String>> {
    let pattern = inst
        .ready_pattern
        .clone()
        .or_else(|| defaults.ready_pattern.clone())
        .or_else(|| {
            if preset.ready_pattern.is_empty() {
                None
            } else {
                Some(preset.ready_pattern.to_string())
            }
        });
    if let Some(ref pat) = pattern {
        if regex::RegexBuilder::new(pat)
            .size_limit(1 << 20)
            .build()
            .is_err()
        {
            tracing::error!(
                instance = name,
                pattern = pat,
                "invalid ready_pattern regex, skipping instance"
            );
            return None;
        }
    }
    Some(pattern)
}

fn resolve_working_directory(inst: &super::InstanceConfig, name: &str) -> Option<Option<PathBuf>> {
    Some(Some(if let Some(d) = inst.working_directory.as_ref() {
        let wd_path = Path::new(d);
        if wd_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            tracing::warn!(
                name,
                dir = d,
                "working_directory contains '..' (path traversal rejected)"
            );
            return None;
        }
        expand_tilde_path(d)
    } else {
        crate::home_dir().join("workspace").join(name)
    }))
}

fn resolve_tier_model(fleet: &super::FleetConfig, name: &str, tier: String) -> Option<String> {
    match fleet.model_tiers.get(&tier) {
        Some(model) if !model.is_empty() => Some(model.clone()),
        _ => {
            tracing::warn!(
                instance = name,
                tier = %tier,
                "fleet.yaml model_tier has no matching non-empty model_tiers entry; no --model will be added"
            );
            None
        }
    }
}

fn resolve_model(
    fleet: &super::FleetConfig,
    inst: &super::InstanceConfig,
    name: &str,
) -> Option<String> {
    if let Some(model) = inst.model.clone() {
        return Some(model);
    }

    if let Some(tier) = inst.model_tier.clone().or_else(|| {
        inst.role_kind
            .and_then(|role| fleet.role_model_tiers.get(&role).cloned())
    }) {
        return resolve_tier_model(fleet, name, tier);
    }

    if let Some(model) = fleet.defaults.model.clone() {
        return Some(model);
    }

    fleet
        .defaults
        .model_tier
        .clone()
        .and_then(|tier| resolve_tier_model(fleet, name, tier))
}

pub(super) fn resolve_instance(
    fleet: &super::FleetConfig,
    name: &str,
) -> Option<super::ResolvedInstance> {
    let inst = fleet.instances.get(name)?;
    let defaults = &fleet.defaults;

    let backend = inst
        .backend
        .clone()
        .or_else(|| defaults.backend.clone())
        .unwrap_or(Backend::ClaudeCode);
    let preset = backend.preset();

    let backend_cmd = inst
        .command
        .clone()
        .or_else(|| defaults.command.clone())
        .unwrap_or_else(|| backend.command_string());

    let args = if !inst.args.is_empty() {
        inst.args.clone()
    } else {
        defaults.args.clone()
    };

    let mut env = defaults.env.clone();
    env.extend(inst.env.clone());
    env.insert("AGEND_INSTANCE_NAME".to_string(), name.to_string());

    let ready_pattern = resolve_ready_pattern(inst, defaults, &preset, name)?;
    let working_directory = resolve_working_directory(inst, name)?;

    Some(super::ResolvedInstance {
        name: name.to_string(),
        backend_command: backend_cmd,
        args,
        env,
        working_directory,
        ready_pattern,
        submit_key: preset.submit_key.to_string(),
        role: inst.role.clone(),
        cols: inst.cols.or(defaults.cols),
        rows: inst.rows.or(defaults.rows),
        topic_id: fleet
            .home
            .as_ref()
            .and_then(|h| crate::channel::telegram::lookup_topic_for_instance(h, name))
            .or(inst.topic_id),
        git_branch: inst.git_branch.clone(),
        model: resolve_model(fleet, inst, name),
        worktree: inst.worktree,
        instructions: inst
            .instructions
            .clone()
            .or_else(|| defaults.instructions.clone()),
        source_repo: inst.source_repo.as_ref().map(|d| expand_tilde_path(d)),
        repo: inst.repo.clone(),
    })
}
