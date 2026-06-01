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

/// #888: pure predicate deciding whether `resolve_one` should
/// auto-create a worktree for this instance.
///
/// Three inputs:
/// - `worktree: false` is a hard veto regardless of other flags
///   (Sprint 28 opt-out semantic preserved).
/// - **`source_repo` OR `git_branch` is the affirmative signal**
///   (#888). Either one declares the instance as a source-repo-bound
///   dev / reviewer / task-dispatch worker that legitimately wants a
///   per-agent worktree.
/// - Anything else (orchestrator / admin / quickstart-default
///   instances) → NO auto-worktree. The `workspace/<name>/` dir
///   stays the project-root, `claude --continue` finds prior
///   session state across app restarts, Gemini/Codex hierarchical
///   AGENTS.md scoping still works via
///   `instructions::ensure_project_root`'s git-init.
///
/// Pre-#888 the gate was just "`worktree != Some(false)` AND
/// working_dir is a git repo". That interacted with
/// `ensure_project_root` (`src/instructions.rs:63`), which runs
/// tail-side of `resolve_one`, in a way that triggered the
/// CONTEXT-LOST bug on every SECOND launch:
///
/// 1. First launch: `workspace/<name>/` just created → no `.git`
///    → no worktree → `ensure_project_root` then runs → leaves
///    `workspace/<name>/.git` on disk.
/// 2. Second launch: `workspace/<name>/.git` is present →
///    `is_git_repo` returns true → `worktree::create` redirects
///    `working_directory` to `worktrees/<name>/...` → `claude
///    --continue` finds no prior session at the new path →
///    operator's conversation history vanishes.
///
/// Pure helper so the unit tests can pin the contract directly
/// without spinning up a real fixture for every variant.
fn wants_auto_worktree(resolved: &crate::fleet::ResolvedInstance) -> bool {
    if resolved.worktree == Some(false) {
        return false;
    }
    resolved.source_repo.is_some() || resolved.git_branch.is_some()
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

    // Auto-create git worktree when working_directory is inside a repo
    // AND the instance has an explicit `source_repo` or `git_branch`
    // config (`#888` — see `wants_auto_worktree` for the full rationale).
    // Redirects `resolved.working_directory` to the worktree path for the
    // rest of the pipeline. Skipped when `worktree: false` (Sprint 28)
    // OR when neither `source_repo` nor `git_branch` is set (#888).
    if wants_auto_worktree(&resolved) {
        if let Some(ref base_dir) = resolved.working_directory {
            if crate::worktree::is_git_repo(base_dir) {
                let custom_branch = resolved.git_branch.as_deref();
                if let Some(info) = crate::worktree::create(ctx.home, base_dir, name, custom_branch)
                {
                    resolved.working_directory = Some(info.path);
                }
            }
        }
    } else if resolved.worktree == Some(false) {
        if let Some(ref base_dir) = resolved.working_directory {
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
        // else: worktree opt-in case with no source_repo / git_branch
        // (orchestrator / admin / quickstart-default) — no auto-create,
        // no prune. workspace dir stays as project-root for `claude
        // --continue` session preservation across app restarts (#888).
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
        // #1463: this scratch-repo commit must BYPASS the agend-git shim — when
        // an agent runs the suite its env carries AGEND_INSTANCE_NAME, so a
        // non-bypassed `commit` would be ChdirPass'd into the bound worktree
        // (the init-pile pollution). Layer A also catches the current_dir form,
        // but the explicit bypass is the durable, form-agnostic guard (enforced
        // by tests/git_test_bypass_invariant.rs).
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
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

    /// #888 positive control: dev / reviewer with explicit
    /// `source_repo` (or `git_branch`) → existing auto-worktree
    /// behavior preserved. This was previously
    /// `resolve_one_worktree_default_creates_worktree` (pre-#888
    /// the test required only `worktree != Some(false)` + `is_git_repo`).
    /// After #888 the gate ALSO requires `source_repo.is_some()` OR
    /// `git_branch.is_some()` so we set `source_repo` here to
    /// preserve the test's positive-path intent.
    #[test]
    fn resolve_one_creates_worktree_when_source_repo_set() {
        let dir = tmp_dir("wt-source-repo");
        init_git_repo(&dir);
        // #888: explicit source_repo signals dev/reviewer intent so
        // the gate fires.
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  test-agent:\n    command: /bin/true\n    working_directory: {dir}\n    source_repo: {dir}\n",
            dir = dir.display()
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
        assert!(result.is_some());
        let (_, _, _, _, work_dir, _) = result.unwrap();
        let wd = work_dir.unwrap();
        // working_directory should be redirected to the new external
        // layout `$AGEND_HOME/worktrees/test-agent/<branch>/` per
        // Sprint 57 Wave 4 (#546 Item 4). The test home == dir, so we
        // look for `<dir>/worktrees/test-agent/...`.
        let wd_str = wd.to_string_lossy();
        assert!(
            wd_str.contains("worktrees") && wd_str.contains("test-agent"),
            "with source_repo set, worktree must redirect to \
             $AGEND_HOME/worktrees/<agent>/<branch>/ external layout, \
             got: {}",
            wd.display()
        );
        // Regression-proof against the legacy in-repo layout.
        assert!(
            !wd_str.contains(".worktrees"),
            "Wave 4: worktree must NOT live under <source_repo>/.worktrees/ \
             (legacy layout), got: {}",
            wd.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #888 LOAD-BEARING: orchestrator / admin instance with NO
    /// `source_repo` and NO `git_branch` → working_directory stays at
    /// `workspace/<name>/` (or whatever the operator configured),
    /// NO worktree gets auto-created. This is the contract the
    /// CONTEXT-LOST bug violated pre-fix: second-launch worktree
    /// creation redirected `claude --continue` to a path that had
    /// no prior session.
    ///
    /// The `ensure_project_root` git-init that runs tail-side of
    /// `generate_with_context` is STILL useful for Gemini/Codex
    /// hierarchical AGENTS.md scoping — so this test verifies the
    /// `.git` exists at `workspace/<orch>/.git` AFTER the call (which
    /// is what motivates the bug in the first place — the .git's
    /// presence on second launch is what triggered the worktree
    /// auto-create). The fix gates auto-create on explicit
    /// `source_repo` / `git_branch`, so the .git's presence is no
    /// longer sufficient.
    #[test]
    fn resolve_one_skips_worktree_for_orchestrator_without_source_repo() {
        let dir = tmp_dir("888-orch");
        let work_dir = dir.join("workspace").join("orch");
        std::fs::create_dir_all(&work_dir).unwrap();
        // Simulate the "second launch" state from #888: a prior pass
        // already created `.git` via `ensure_project_root`. With this
        // present, pre-fix `is_git_repo` returns true → auto-worktree.
        // Post-fix, the gate requires `source_repo` / `git_branch` so
        // the worktree must NOT be created regardless.
        init_git_repo(&work_dir);
        assert!(
            work_dir.join(".git").exists(),
            "fixture must seed `.git` to reproduce the second-launch state"
        );

        // Minimal fleet.yaml: orchestrator with NO source_repo, NO
        // git_branch, default worktree value (None).
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  orch:\n    command: /bin/true\n    working_directory: {}\n",
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
        let result = resolve_one(&config, &ctx, "orch");
        assert!(
            result.is_some(),
            "resolve_one must succeed for orchestrator"
        );
        let (_, _, _, _, returned_work_dir, _) = result.unwrap();
        let wd = returned_work_dir.unwrap();

        // #888 LOAD-BEARING: working_directory must NOT be redirected
        // into `$AGEND_HOME/worktrees/<name>/...`. The orchestrator's
        // workspace dir is the project-root for the agent; redirecting
        // would lose `claude --continue` session state across restarts.
        let wd_str = wd.to_string_lossy();
        assert!(
            !wd_str.contains("/worktrees/orch/"),
            "#888: orchestrator without source_repo / git_branch MUST NOT \
             have its working_directory redirected to worktrees/<name>/. \
             Got: {}",
            wd.display()
        );

        // Working_directory should still be the original workspace dir
        // (canonical path comparison via fs::canonicalize handles macOS's
        // `/private/var ↔ /var` symlink + temp dir resolution).
        let canon_returned = std::fs::canonicalize(&wd).unwrap_or_else(|_| wd.clone());
        let canon_expected = std::fs::canonicalize(&work_dir).unwrap_or(work_dir.clone());
        assert_eq!(
            canon_returned, canon_expected,
            "#888: working_directory must remain the configured workspace \
             path for orchestrators (no auto-worktree redirect)"
        );

        // Gemini/Codex AGENTS.md scoping load-bearer: `.git` must still
        // exist at the workspace path (either from the fixture's seed
        // OR from `ensure_project_root`'s tail-side git-init — both
        // count). The fix MUST NOT regress this; without `.git` the
        // hierarchical AGENTS.md search walks up into `$HOME`.
        assert!(
            work_dir.join(".git").exists(),
            "#888: workspace/<orch>/.git must remain for Gemini/Codex \
             hierarchical AGENTS.md scoping"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Pure unit test on the `wants_auto_worktree` predicate.
    /// Covers all four input shapes without filesystem fixtures.
    #[test]
    fn wants_auto_worktree_truth_table() {
        let mk = |worktree: Option<bool>,
                  source_repo: Option<PathBuf>,
                  git_branch: Option<String>|
         -> crate::fleet::ResolvedInstance {
            crate::fleet::ResolvedInstance {
                name: "t".into(),
                backend_command: "claude".into(),
                args: vec![],
                env: HashMap::new(),
                working_directory: None,
                ready_pattern: None,
                submit_key: "\r".into(),
                role: None,
                cols: None,
                rows: None,
                topic_id: None,
                git_branch,
                model: None,
                worktree,
                instructions: None,
                source_repo,
                repo: None,
            }
        };
        // worktree: false → ALWAYS false regardless of other flags.
        assert!(!wants_auto_worktree(&mk(
            Some(false),
            Some(PathBuf::from("/x")),
            Some("main".into())
        )));
        // No source_repo, no git_branch → false (the #888 fix).
        assert!(!wants_auto_worktree(&mk(None, None, None)));
        assert!(!wants_auto_worktree(&mk(Some(true), None, None)));
        // source_repo set → true.
        assert!(wants_auto_worktree(&mk(
            None,
            Some(PathBuf::from("/x")),
            None
        )));
        // git_branch set → true.
        assert!(wants_auto_worktree(&mk(None, None, Some("main".into()))));
        // Both set → true.
        assert!(wants_auto_worktree(&mk(
            None,
            Some(PathBuf::from("/x")),
            Some("main".into())
        )));
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
