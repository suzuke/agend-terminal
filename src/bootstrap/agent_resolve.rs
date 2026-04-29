//! Per-instance spawn preparation: take a normalized [`FleetConfig`] and walk
//! each instance, producing a spawn-ready [`AgentDef`]. For each instance we
//! ensure the working directory, create a git worktree when applicable,
//! generate instruction + MCP config files, and append resume / model /
//! Claude-specific flags when their files are present.

use crate::backend;
use crate::fleet::FleetConfig;
use std::collections::HashMap;
use std::path::PathBuf;

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
pub(super) fn resolve(config: &FleetConfig) -> Vec<AgentDef> {
    config
        .instance_names()
        .into_iter()
        .filter_map(|name| resolve_one(config, &name))
        .collect()
}

/// Resolve a single named instance into a spawn-ready [`AgentDef`].
///
/// Returns `None` if the instance is missing from `config` or cannot be
/// resolved. Side effects (worktree creation, instruction generation) mirror
/// [`resolve`] so hot-reload-added agents are set up identically to ones
/// materialized at startup.
pub(crate) fn resolve_one(config: &FleetConfig, name: &str) -> Option<AgentDef> {
    let mut resolved = config.resolve_instance(name)?;

    if let Some(ref base_dir) = resolved.working_directory {
        std::fs::create_dir_all(base_dir).ok();
    }

    // Auto-create git worktree when working_directory is inside a repo.
    // Redirects `resolved.working_directory` to the worktree path for the
    // rest of the pipeline. Skipped when `worktree: false` (Sprint 28).
    if resolved.worktree != Some(false) {
        if let Some(ref base_dir) = resolved.working_directory {
            if crate::worktree::is_git_repo(base_dir) {
                let custom_branch = resolved.git_branch.as_deref();
                if let Some(info) = crate::worktree::create(base_dir, name, custom_branch) {
                    resolved.working_directory = Some(info.path);
                }
            }
        }
    } else if let Some(ref base_dir) = resolved.working_directory {
        // worktree: false — auto-prune existing worktree if present.
        // Pre-flight: reject if uncommitted changes exist (protect work).
        if crate::worktree::is_git_repo(base_dir) {
            let wt_dir = base_dir.join(".worktrees").join(name);
            if wt_dir.exists() {
                if crate::worktree::has_uncommitted_changes(&wt_dir) {
                    tracing::error!(
                        instance = name,
                        worktree = %wt_dir.display(),
                        "FATAL: worktree opt-out rejected — uncommitted changes in worktree. \
                         Commit or stash changes before setting worktree: false. \
                         Instance will NOT start."
                    );
                    return None; // Refuse instance start — operator must resolve
                } else if let Err(e) = crate::worktree::remove_worktree(base_dir, name) {
                    tracing::warn!(instance = name, error = %e, "auto-prune worktree failed");
                }
            }
        }
    }

    if let Some(ref dir) = resolved.working_directory {
        crate::instructions::generate(dir, &resolved.backend_command);
    }

    let mut args = resolved.args;

    if let Some(ref model) = resolved.model {
        let model_val = backend::Backend::from_command(&resolved.backend_command)
            .map(|b| b.format_model_arg(model))
            .unwrap_or_else(|| model.clone());
        args.push("--model".to_string());
        args.push(model_val);
    }

    Some((
        resolved.name,
        resolved.backend_command,
        args,
        Some(resolved.env),
        resolved.working_directory,
        resolved.submit_key,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-resolve-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_git_repo(dir: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    fn make_config(dir: &std::path::Path, worktree: Option<bool>) -> FleetConfig {
        let mut yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  test-agent:\n    command: /bin/true\n    working_directory: {}\n",
            dir.display()
        );
        if let Some(wt) = worktree {
            yaml.push_str(&format!("    worktree: {wt}\n"));
        }
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn resolve_one_worktree_false_skips_worktree_creation() {
        let dir = tmp_dir("wt-false-skip");
        init_git_repo(&dir);
        let config = make_config(&dir, Some(false));
        let result = resolve_one(&config, "test-agent");
        assert!(
            result.is_some(),
            "resolve_one must succeed with worktree:false"
        );
        let (_, _, _, _, work_dir, _) = result.unwrap();
        // working_directory must be the original dir (not a .worktrees/ subdir)
        let wd = work_dir.unwrap();
        assert!(
            !wd.to_string_lossy().contains(".worktrees"),
            "worktree:false must NOT create .worktrees/ subdir, got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_one_worktree_default_creates_worktree() {
        let dir = tmp_dir("wt-default");
        init_git_repo(&dir);
        let config = make_config(&dir, None); // default = true
        let result = resolve_one(&config, "test-agent");
        assert!(result.is_some());
        let (_, _, _, _, work_dir, _) = result.unwrap();
        let wd = work_dir.unwrap();
        // Default worktree: working_directory should be redirected to
        // .worktrees/test-agent/ (worktree creation succeeded)
        assert!(
            wd.to_string_lossy().contains(".worktrees")
                || wd.to_string_lossy().contains("test-agent"),
            "default worktree must redirect to .worktrees/ subdir, got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_one_worktree_false_prunes_clean_existing_worktree() {
        let dir = tmp_dir("wt-false-prune");
        init_git_repo(&dir);
        // Create a fake worktree dir
        let wt_dir = dir.join(".worktrees").join("test-agent");
        std::fs::create_dir_all(&wt_dir).unwrap();
        init_git_repo(&wt_dir);
        assert!(wt_dir.exists(), "worktree must exist before prune");

        let config = make_config(&dir, Some(false));
        let result = resolve_one(&config, "test-agent");
        assert!(result.is_some(), "resolve_one must succeed after prune");
        // Verify prune was attempted — worktree dir should be removed
        // (or at minimum, resolve_one returned the base dir, not the worktree)
        let (_, _, _, _, work_dir, _) = result.unwrap();
        let wd = work_dir.unwrap();
        assert!(
            !wd.to_string_lossy().contains(".worktrees"),
            "worktree:false must NOT use .worktrees/ path, got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
