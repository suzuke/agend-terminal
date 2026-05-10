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

struct ResolveContext<'a> {
    fleet_dir: &'a Path,
    /// Sprint 57 Wave 4 (#546 Item 4): AGEND_HOME for the new
    /// external worktree layout `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    home: &'a Path,
    peers: Vec<(String, Option<String>)>,
}

/// Resolve every instance in `config` into a spawn-ready [`AgentDef`].
pub(super) fn resolve(config: &FleetConfig, fleet_dir: &Path, home: &Path) -> Vec<AgentDef> {
    let ctx = ResolveContext {
        fleet_dir,
        home,
        peers: config
            .instances
            .iter()
            .map(|(n, c)| (n.clone(), c.role.clone()))
            .collect(),
    };
    config
        .instance_names()
        .into_iter()
        .filter_map(|name| resolve_one(config, &ctx, &name))
        .collect()
}

/// Resolve a single named instance into a spawn-ready [`AgentDef`].
///
/// Returns `None` if the instance is missing from `config` or cannot be
/// resolved. Side effects (worktree creation, instruction generation) mirror
/// [`resolve`] so hot-reload-added agents are set up identically to ones
/// materialized at startup.
fn resolve_one(config: &FleetConfig, ctx: &ResolveContext<'_>, name: &str) -> Option<AgentDef> {
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
                if let Some(info) = crate::worktree::create(ctx.home, base_dir, name, custom_branch)
                {
                    resolved.working_directory = Some(info.path);
                }
            }
        }
    } else if let Some(ref base_dir) = resolved.working_directory {
        // worktree: false — auto-prune existing worktree if present.
        // Pre-flight: reject if uncommitted changes exist (protect work).
        // Sprint 57 Wave 4 (#546 Item 4): the canonical worktree path
        // is now `$AGEND_HOME/worktrees/<agent>/<branch>/`. Auto-prune
        // here is scoped to the default-branch shape `agend/<agent>`
        // (the path `worktree::create` synthesizes when no custom
        // branch is supplied). Custom-branch worktrees that the
        // operator created via task dispatch / bind_self stay put on
        // worktree:false toggle; manual `release_worktree` is the
        // cleanup path for those. This matches the pre-Wave-4 narrow
        // scope (the legacy auto-prune also only knew the agent name).
        if crate::worktree::is_git_repo(base_dir) {
            let default_branch = format!("agend/{name}");
            let wt_path = crate::worktree::worktree_path(ctx.home, name, &default_branch);
            if wt_path.exists() {
                if crate::worktree::has_uncommitted_changes(&wt_path) {
                    tracing::error!(
                        instance = name,
                        worktree = %wt_path.display(),
                        "FATAL: worktree opt-out rejected — uncommitted changes in worktree. \
                         Commit or stash changes before setting worktree: false. \
                         Instance will NOT start."
                    );
                    return None; // Refuse instance start — operator must resolve
                } else if let Err(e) =
                    crate::worktree::remove_worktree(ctx.home, base_dir, name, &default_branch)
                {
                    tracing::warn!(instance = name, error = %e, "auto-prune worktree failed");
                }
            }
        }
    }

    if let Some(ref dir) = resolved.working_directory {
        let extra_instructions = crate::instructions::resolve_extra_for(&resolved, ctx.fleet_dir);
        let ctx = crate::instructions::AgentContext {
            name,
            role: resolved.role.as_deref(),
            fleet_peers: &ctx.peers,
            team: None,
            extra_instructions: extra_instructions.as_deref(),
        };
        crate::instructions::generate_with_context(dir, &resolved.backend_command, Some(&ctx));
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
            .args(["init", "-b", "main"])
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
        serde_yaml_ng::from_str(&yaml).unwrap()
    }

    #[test]
    fn resolve_one_worktree_false_skips_worktree_creation() {
        let dir = tmp_dir("wt-false-skip");
        init_git_repo(&dir);
        let config = make_config(&dir, Some(false));
        let peers: Vec<(String, Option<String>)> = config
            .instances
            .iter()
            .map(|(n, c)| (n.clone(), c.role.clone()))
            .collect();
        let ctx = ResolveContext {
            fleet_dir: &dir,
            home: &dir,
            peers,
        };
        let result = resolve_one(&config, &ctx, "test-agent");
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
        let peers: Vec<(String, Option<String>)> = config
            .instances
            .iter()
            .map(|(n, c)| (n.clone(), c.role.clone()))
            .collect();
        let ctx = ResolveContext {
            fleet_dir: &dir,
            home: &dir,
            peers,
        };
        let result = resolve_one(&config, &ctx, "test-agent");
        assert!(result.is_some());
        let (_, _, _, _, work_dir, _) = result.unwrap();
        let wd = work_dir.unwrap();
        // Default worktree: working_directory should be redirected to
        // the new external layout `$AGEND_HOME/worktrees/test-agent/<branch>/`
        // per Sprint 57 Wave 4 (#546 Item 4). The test home == dir,
        // so we look for `<dir>/worktrees/test-agent/...`.
        let wd_str = wd.to_string_lossy();
        assert!(
            wd_str.contains("worktrees") && wd_str.contains("test-agent"),
            "default worktree must redirect to $AGEND_HOME/worktrees/<agent>/<branch>/ \
             external layout, got: {}",
            wd.display()
        );
        // Regression-proof against the legacy in-repo layout: working_dir
        // must NOT live under `<source_repo>/.worktrees/`.
        assert!(
            !wd_str.contains(".worktrees"),
            "Wave 4: worktree must NOT live under <source_repo>/.worktrees/ \
             (legacy layout), got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_one_worktree_false_prunes_clean_existing_worktree() {
        let dir = tmp_dir("wt-false-prune");
        init_git_repo(&dir);
        // Sprint 57 Wave 4 (#546 Item 4): worktrees live external at
        // `$AGEND_HOME/worktrees/<agent>/<branch>/`. Set up the new
        // layout for the prune test rather than the legacy
        // `<source_repo>/.worktrees/`. The default branch is
        // `agend/<agent>` per `worktree::create`'s fallback.
        let wt_dir = dir
            .join("worktrees")
            .join("test-agent")
            .join("agend")
            .join("test-agent");
        std::fs::create_dir_all(&wt_dir).unwrap();
        init_git_repo(&wt_dir);
        assert!(wt_dir.exists(), "worktree must exist before prune");

        let config = make_config(&dir, Some(false));
        let peers: Vec<(String, Option<String>)> = config
            .instances
            .iter()
            .map(|(n, c)| (n.clone(), c.role.clone()))
            .collect();
        let ctx = ResolveContext {
            fleet_dir: &dir,
            home: &dir,
            peers,
        };
        let result = resolve_one(&config, &ctx, "test-agent");
        assert!(result.is_some(), "resolve_one must succeed after prune");
        // Verify prune was attempted — worktree dir should be removed
        // (or at minimum, resolve_one returned the base dir, not the worktree)
        let (_, _, _, _, work_dir, _) = result.unwrap();
        let wd = work_dir.unwrap();
        let wd_str = wd.to_string_lossy();
        assert!(
            !wd_str.contains("/worktrees/test-agent/"),
            "worktree:false must NOT redirect into the new external worktree layout, got: {}",
            wd.display()
        );
        assert!(
            !wd_str.contains(".worktrees"),
            "worktree:false must NOT use legacy <repo>/.worktrees/ path, got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_one_appends_instance_extra_instructions() {
        let dir = tmp_dir("extra-instructions");
        let work_dir = dir.join("work");
        std::fs::create_dir_all(dir.join("instructions")).unwrap();
        std::fs::write(
            dir.join("instructions").join("dev.md"),
            "# Extra\nAlways mention deployment checklist.",
        )
        .unwrap();
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  test-agent:\n    command: claude\n    working_directory: {}\n    instructions: ./instructions/dev.md\n",
            work_dir.display()
        );
        let config: FleetConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        let peers: Vec<(String, Option<String>)> = config
            .instances
            .iter()
            .map(|(n, c)| (n.clone(), c.role.clone()))
            .collect();
        let ctx = ResolveContext {
            fleet_dir: &dir,
            home: &dir,
            peers,
        };
        let result = resolve_one(&config, &ctx, "test-agent");
        assert!(result.is_some(), "resolve_one should succeed");
        let (_, _, _, _, wd, _) = result.unwrap();
        let generated = std::fs::read_to_string(wd.unwrap().join(".claude/agend.md"))
            .expect("generated instructions file");
        assert!(
            generated.contains("Always mention deployment checklist."),
            "extra instructions must be appended in daemon bootstrap path"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
