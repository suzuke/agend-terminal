//! Per-instance spawn preparation: take a normalized [`FleetConfig`] and walk
//! each instance, producing a spawn-ready [`AgentDef`]. For each instance we
//! ensure the working directory, create a git worktree when applicable,
//! generate instruction + MCP config files, and append resume / model /
//! Claude-specific flags when their files are present.

use crate::backend;
use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Agent spawn tuple consumed by `daemon::run_with_prepared`. Matches the
/// shape of `daemon::AgentDef`.
pub type AgentDef = (
    String,
    String,
    Vec<String>,
    Option<HashMap<String, String>>,
    Option<PathBuf>,
    String,
);

/// Resolve every instance in `config` into a spawn-ready [`AgentDef`].
pub(super) fn resolve(config: &FleetConfig, home: &Path) -> Vec<AgentDef> {
    let mut agents = Vec::new();
    for name in config.instance_names() {
        let Some(mut resolved) = config.resolve_instance(&name) else {
            continue;
        };

        if let Some(ref base_dir) = resolved.working_directory {
            std::fs::create_dir_all(base_dir).ok();
        }

        // Auto-create git worktree when working_directory is inside a repo.
        // Redirects `resolved.working_directory` to the worktree path for the
        // rest of the pipeline.
        if let Some(ref base_dir) = resolved.working_directory {
            if crate::worktree::is_git_repo(base_dir) {
                let custom_branch = resolved.git_branch.as_deref();
                if let Some(info) = crate::worktree::create(base_dir, &name, custom_branch) {
                    resolved.working_directory = Some(info.path);
                }
            }
        }

        if let Some(ref dir) = resolved.working_directory {
            crate::instructions::generate(dir, &resolved.backend_command);
        }

        let mut args = resolved.args;
        if let Some(ref b) = backend::Backend::from_command(&resolved.backend_command) {
            let p = b.preset();
            args.extend(p.resume_mode.args_for(home, &name));
        }

        if let Some(ref model) = resolved.model {
            let model_val = backend::Backend::from_command(&resolved.backend_command)
                .map(|b| b.format_model_arg(model))
                .unwrap_or_else(|| model.clone());
            args.push("--model".to_string());
            args.push(model_val);
        }

        // Claude-specific flags: only append when the matching file exists in
        // the working directory. `agent::spawn_agent` no-ops when already set.
        if let Some(ref dir) = resolved.working_directory {
            if resolved.backend_command.contains("claude") {
                let mcp_config = dir.join("mcp-config.json");
                if mcp_config.exists() {
                    args.push("--mcp-config".to_string());
                    args.push(mcp_config.display().to_string());
                }
                let settings = dir.join("claude-settings.json");
                if settings.exists() {
                    args.push("--settings".to_string());
                    args.push(settings.display().to_string());
                }
            }
        }

        agents.push((
            resolved.name,
            resolved.backend_command,
            args,
            Some(resolved.env),
            resolved.working_directory,
            resolved.submit_key,
        ));
    }
    agents
}
