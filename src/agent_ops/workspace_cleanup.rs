//! #2764: proof-carrying destructive workspace cleanup.
//!
//! Whole-tree removal / scrub of an instance's working directory used to be
//! authorized by mere *containment* under `$AGEND_HOME/workspace/` — with no
//! proof the directory belonged EXCLUSIVELY to the instance being deleted. A
//! victim whose `working_directory` pointed at a sibling's dir (or a symlink
//! alias) had that sibling's real tree removed (2026-07-13 incident: deleting
//! `archfix-fable` recursively removed live `codex-125550`'s workspace).
//!
//! This module is the single planner + executor. A destructive action is
//! representable ONLY through an opaque proof minted by [`plan_cleanup`], which
//! derives its decision from an **immutable pre-removal `FleetConfig`
//! snapshot** and fails closed on every ambiguity → a complete path-local
//! no-op. The proofs carry no public constructor, so no caller can whole-tree
//! remove (or scrub) without routing through the planner.

use crate::fleet::FleetConfig;
use std::path::{Component, Path, PathBuf};

/// Opaque authorization to remove an ENTIRE directory tree. Constructible only
/// inside this module — [`execute_remove_owned`] consumes it by reference, so a
/// whole-tree remove cannot be written without first passing [`plan_cleanup`].
#[derive(Debug, Clone)]
pub struct RemoveOwnedProof {
    /// The path exactly as the caller passed it (used to REVALIDATE just before
    /// destruction — catches a symlink swap between plan and execute).
    original: PathBuf,
    /// The canonical target captured at plan time.
    canonical: PathBuf,
}

impl RemoveOwnedProof {
    /// The canonical directory the proof authorizes removing. Git registers
    /// worktrees by realpath, so destruction targets this canonical path (a raw
    /// `/var` original could miss a `/private/var` realpath registration on
    /// macOS, orphaning it). `original` is retained only to REVALIDATE the proof
    /// (that `original` still canonicalizes here) immediately before removal.
    pub fn canonical(&self) -> &Path {
        &self.canonical
    }
}

/// Opaque authorization to scrub agend-generated files from an EXCLUSIVE
/// user-provided directory (never a whole-tree remove).
#[derive(Debug, Clone)]
pub struct ScrubExclusiveProof {
    original: PathBuf,
    canonical: PathBuf,
}

/// The planner's verdict for one `(victim, candidate)` pair. Only the two
/// `*Proof`-carrying variants authorize any filesystem mutation; both
/// `Preserve*` variants mean a COMPLETE path-local no-op (no scrub, no
/// worktree teardown, no `remove_dir_all`).
#[derive(Debug)]
pub enum CleanupPlan {
    /// Candidate is the victim's exclusive canonical default dir → whole-tree remove.
    RemoveOwned(RemoveOwnedProof),
    /// Candidate is an exclusive user-provided dir → agend-file scrub only.
    ScrubExclusive(ScrubExclusiveProof),
    /// A surviving instance resolves to an overlapping dir → complete no-op.
    PreserveShared { reason: String },
    /// Ownership unprovable (dotdot, canonicalize/fleet-read failure, or a
    /// non-default workspace path) → fail closed, complete no-op.
    PreserveAmbiguous { reason: String },
}

/// Symmetric containment overlap: `a == b`, `a` inside `b`, OR `b` inside `a`.
/// Deleting a nested candidate that is *part of* a survivor's tree overlaps,
/// and so does deleting a parent that CONTAINS a survivor.
fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn has_dotdot(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Resolve an instance's RAW working directory from the snapshot WITHOUT
/// depending on the full `resolve_instance` (which returns `None` for unrelated
/// reasons — ready-pattern failures, `..` rejection — and would silently DROP a
/// survivor from the sharing set, re-exposing its dir). Explicit
/// `working_directory` (tilde-expanded) else the default `workspace/<name>`.
fn instance_wd(snapshot: &FleetConfig, home: &Path, name: &str) -> Option<PathBuf> {
    let inst = snapshot.instances.get(name)?;
    Some(match inst.working_directory.as_deref() {
        Some(d) => crate::fleet::resolve::expand_tilde_path(d),
        None => crate::paths::workspace_dir(home).join(name),
    })
}

/// Plan the destructive cleanup of `victim`'s `candidate` working dir using the
/// IMMUTABLE pre-removal fleet `snapshot`. `cohort` names are excluded from the
/// survivor set (the victim itself plus any co-deleted deployment instances).
///
/// Positive default ownership is `canonical(candidate) ==
/// canonical(workspace_root).join(victim)` — the workspace ROOT is
/// canonicalized and the RAW victim name joined; the leaf is never
/// canonicalized (that would make a symlink alias tautologically pass). Every
/// ambiguity (dotdot, unreadable fleet/root/candidate/survivor, non-default
/// workspace path) fails closed to a `Preserve*` no-op.
pub fn plan_cleanup(
    snapshot: Option<&FleetConfig>,
    home: &Path,
    cohort: &[&str],
    victim: &str,
    candidate: &Path,
) -> CleanupPlan {
    if has_dotdot(candidate) {
        return CleanupPlan::PreserveAmbiguous {
            reason: format!("candidate path contains '..': {}", candidate.display()),
        };
    }
    let candidate_canonical = match dunce::canonicalize(candidate) {
        Ok(c) => c,
        Err(e) => {
            return CleanupPlan::PreserveAmbiguous {
                reason: format!(
                    "candidate {} does not canonicalize ({e})",
                    candidate.display()
                ),
            };
        }
    };
    let Some(snapshot) = snapshot else {
        return CleanupPlan::PreserveAmbiguous {
            reason: "fleet snapshot unavailable — cannot prove exclusive ownership".to_string(),
        };
    };

    // --- Survivor sharing: symmetric canonical overlap with ANY live sibling ---
    for name in snapshot.instances.keys() {
        if cohort.contains(&name.as_str()) {
            continue;
        }
        let Some(swd) = instance_wd(snapshot, home, name) else {
            continue;
        };
        match dunce::canonicalize(&swd) {
            Ok(sc) => {
                if paths_overlap(&sc, &candidate_canonical) {
                    return CleanupPlan::PreserveShared {
                        reason: format!(
                            "surviving instance '{name}' resolves to overlapping dir {}",
                            sc.display()
                        ),
                    };
                }
            }
            Err(_) => {
                // Un-canonicalizable survivor: a missing default dir cannot alias
                // an existing candidate EXCEPT via symlink games. Fail closed only
                // when the RAW survivor path lexically overlaps the candidate
                // (raw or canonical) — an unrelated missing sibling is not a risk.
                if paths_overlap(&swd, candidate) || paths_overlap(&swd, &candidate_canonical) {
                    return CleanupPlan::PreserveAmbiguous {
                        reason: format!(
                            "surviving instance '{name}' working dir {} is un-canonicalizable and may alias the candidate",
                            swd.display()
                        ),
                    };
                }
            }
        }
    }

    // --- Unshared: prove exclusive ownership ---
    let workspace_root = crate::paths::workspace_dir(home);
    let ws_root_canon = match dunce::canonicalize(&workspace_root) {
        Ok(c) => c,
        Err(e) => {
            // Workspace root itself un-canonicalizable. If the candidate is
            // lexically under the RAW workspace root we cannot prove default
            // ownership → ambiguous. Otherwise it is an external user dir.
            if candidate.starts_with(&workspace_root) {
                return CleanupPlan::PreserveAmbiguous {
                    reason: format!(
                        "workspace root {} does not canonicalize ({e}); candidate is under the raw workspace root",
                        workspace_root.display()
                    ),
                };
            }
            return CleanupPlan::ScrubExclusive(ScrubExclusiveProof {
                original: candidate.to_path_buf(),
                canonical: candidate_canonical,
            });
        }
    };
    let owned_default = ws_root_canon.join(victim);
    if candidate_canonical == owned_default {
        return CleanupPlan::RemoveOwned(RemoveOwnedProof {
            original: candidate.to_path_buf(),
            canonical: candidate_canonical,
        });
    }
    if candidate_canonical.starts_with(&ws_root_canon) {
        // Under the workspace root but NOT the victim's exact canonical default
        // (e.g. an explicit `working_directory: workspace/<sibling>`, or a
        // symlink whose target lands elsewhere under workspace). Not provably
        // owned → fail closed.
        return CleanupPlan::PreserveAmbiguous {
            reason: format!(
                "candidate {} is under the workspace root but is not victim '{victim}' canonical default {}",
                candidate_canonical.display(),
                owned_default.display()
            ),
        };
    }
    // External, unshared → user-provided dir → agend-file scrub only.
    CleanupPlan::ScrubExclusive(ScrubExclusiveProof {
        original: candidate.to_path_buf(),
        canonical: candidate_canonical,
    })
}

/// Revalidate a proof's ORIGINAL path still canonicalizes to the captured
/// target immediately before destruction (TOCTOU guard against a symlink swap
/// between plan and execute). `Ok(())` when still valid.
fn revalidate(original: &Path, canonical: &Path) -> Result<(), String> {
    match dunce::canonicalize(original) {
        Ok(now) if now == canonical => Ok(()),
        Ok(now) => Err(format!(
            "revalidation failed: {} now resolves to {} (was {})",
            original.display(),
            now.display(),
            canonical.display()
        )),
        Err(e) => Err(format!(
            "revalidation failed: {} no longer canonicalizes ({e})",
            original.display()
        )),
    }
}

/// Execute a proven whole-tree removal of the victim's owned workspace dir.
/// Revalidates first; routes a daemon-managed gitlink workspace worktree
/// through `git worktree remove` (work-at-risk backed up), else `remove_dir_all`.
pub fn execute_remove_owned(
    home: &Path,
    agent: &str,
    proof: &RemoveOwnedProof,
) -> Result<(), String> {
    revalidate(&proof.original, &proof.canonical)?;
    // #2234 Phase 0: a workspace dir that IS a git worktree (its `.git` is a
    // gitlink FILE) must be torn down via `git worktree remove` so no orphan
    // registration survives; a plain dir returns false → byte-identical
    // remove_dir_all below. Operate on the ORIGINAL path (git's registration +
    // binding.json are keyed on it), not the canonical alias.
    if crate::worktree_pool::teardown_workspace_worktree_proven(home, agent, proof) {
        return Ok(());
    }
    std::fs::remove_dir_all(&proof.canonical)
        .map_err(|e| format!("remove_dir_all {} failed: {e}", proof.canonical.display()))
}

/// The 19-entry CANONICAL superset of agend-generated files scrubbed from a
/// user-provided (external, exclusive) working dir. Kept here so the scrub
/// list has one home (the 2026-04-14 drift was a duplicated copy).
pub(crate) const AGEND_FILES: &[&str] = &[
    // Claude
    ".claude/settings.local.json",
    "mcp-config.json",
    "claude-settings.json",
    "statusline.sh",
    "statusline.json",
    ".claude/rules/agend.md",
    // Gemini
    ".gemini/settings.json",
    // OpenCode
    "opencode.json",
    "instructions/agend.md",
    // Codex
    ".codex/config.toml",
    "AGENTS.md",
    // Kiro
    ".kiro/settings/mcp.json",
    ".kiro/settings/agend-mcp-wrapper.sh",
    ".kiro/steering/agend.md",
    ".kiro/agents/agend.json",
    ".kiro/agents/agend-prompt.md",
    ".kiro/agents/default.json",
    ".kiro/prompts/agend.md",
    ".kiro/settings.json",
];

/// Execute a proven exclusive scrub: remove only agend-generated files (never a
/// whole-tree remove) plus the `.worktrees/<agent>` helper worktree, from an
/// external user-provided dir proven unshared. Revalidates first.
pub fn execute_scrub_exclusive(home: &Path, agent: &str, proof: &ScrubExclusiveProof) {
    let _ = home; // reserved for symmetry with execute_remove_owned
    if let Err(e) = revalidate(&proof.original, &proof.canonical) {
        tracing::warn!(agent, error = %e, "#2764 scrub: revalidation failed — skipping");
        return;
    }
    let dir = &proof.canonical;
    for file in AGEND_FILES {
        let path = dir.join(file);
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
    // Clean up the helper worktree if present (best-effort, bounded).
    let wt_dir = dir.join(".worktrees").join(agent);
    if wt_dir.exists() {
        let _ = crate::git_helpers::git_ok(
            dir,
            &[
                "worktree",
                "remove",
                "--force",
                &wt_dir.display().to_string(),
            ],
        );
        tracing::info!(dir = %wt_dir.display(), "removed worktree");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// #2764 D — scoped source invariant (defense-in-depth behind the type-level
    /// forcing function). The PRIMARY guard is the opaque `RemoveOwnedProof` /
    /// `ScrubExclusiveProof` (private fields → whole-tree removal is
    /// unrepresentable without [`plan_cleanup`]). This tripwire additionally
    /// pins that the RAW `teardown_workspace_worktree` — the other destructive
    /// workspace-worktree entry the contract names — is only ever CALLED from
    /// the proof-gated wrapper (`teardown_workspace_worktree_proven`) or its own
    /// definition module. The literal `teardown_workspace_worktree(` token does
    /// NOT match `teardown_workspace_worktree_proven(` (the `_proven` sits
    /// between the name and the paren), so proof-gated calls are excluded. A new
    /// un-gated caller anywhere else FAILS this test, forcing the author to route
    /// through a proof.
    #[test]
    fn raw_workspace_worktree_teardown_is_confined_to_proof_gate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Files allowed to name the raw call: the proof-gated wrapper lives in
        // worktree_pool.rs; the fn is defined in worktree_pool/workspace.rs; and
        // this module (workspace_cleanup.rs) hosts the invariant test itself,
        // whose diagnostic strings mention the token (its production code calls
        // only the `_proven` wrapper).
        let allow: &[&str] = &["worktree_pool.rs", "workspace.rs", "workspace_cleanup.rs"];
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if allow.contains(&fname) {
                    continue;
                }
                // Test modules legitimately exercise the pub(crate) primitive
                // directly — the invariant targets PRODUCTION bypasses only.
                if fname == "tests.rs"
                    || fname.ends_with("_tests.rs")
                    || fname.starts_with("review_repro")
                    || path.components().any(|c| c.as_os_str() == "tests")
                {
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in body.lines().enumerate() {
                    // Skip comments / doc references — only real call syntax.
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") || trimmed.starts_with("*") {
                        continue;
                    }
                    if line.contains("teardown_workspace_worktree(") {
                        offenders.push(format!(
                            "{}:{}: {}",
                            path.strip_prefix(&root).unwrap_or(&path).display(),
                            i + 1,
                            trimmed.trim_end()
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "#2764: raw teardown_workspace_worktree() called outside the proof gate — \
             route it through teardown_workspace_worktree_proven (which requires a \
             RemoveOwnedProof). Offending call sites:\n{}",
            offenders.join("\n")
        );
    }

    fn tmp_home(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-2764-plan-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Build a FleetConfig with the given `(name, explicit_working_directory)`
    /// instances via the real YAML loader — exercises the same parse path
    /// production uses.
    fn fleet_with(home: &Path, entries: &[(&str, Option<&str>)]) -> FleetConfig {
        let mut yaml = String::from("instances:\n");
        for (name, wd) in entries {
            yaml.push_str(&format!("  {name}:\n    backend: claude\n"));
            if let Some(d) = wd {
                yaml.push_str(&format!("    working_directory: {d}\n"));
            }
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
        FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap()
    }

    fn ws(home: &Path, name: &str) -> PathBuf {
        crate::paths::workspace_dir(home).join(name)
    }

    /// R5: the victim's own exact canonical default dir is RemoveOwned.
    #[test]
    fn exact_owned_default_is_remove_owned() {
        let home = tmp_home("owned");
        let fleet = fleet_with(&home, &[("victim", None)]);
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &vdir);
        assert!(
            matches!(plan, CleanupPlan::RemoveOwned(_)),
            "exact owned default must be RemoveOwned, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// R1 core: victim.wd aliases a LIVE sibling's default dir → PreserveShared.
    #[test]
    fn sibling_default_alias_is_preserve_shared() {
        let home = tmp_home("sibling");
        let sib = ws(&home, "sibling");
        std::fs::create_dir_all(&sib).unwrap();
        // victim explicitly points at the sibling's default dir.
        let fleet = fleet_with(
            &home,
            &[("victim", Some(sib.to_str().unwrap())), ("sibling", None)],
        );
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &sib);
        assert!(
            matches!(plan, CleanupPlan::PreserveShared { .. }),
            "victim aliasing a live sibling dir must PreserveShared, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Symmetric overlap, NESTED: candidate is a subdir INSIDE a survivor's dir
    /// → deleting it removes part of the survivor → PreserveShared.
    #[test]
    fn candidate_nested_inside_survivor_is_preserve_shared() {
        let home = tmp_home("nested");
        let sib = ws(&home, "sibling");
        let nested = sib.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let fleet = fleet_with(
            &home,
            &[
                ("victim", Some(nested.to_str().unwrap())),
                ("sibling", None),
            ],
        );
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &nested);
        assert!(
            matches!(plan, CleanupPlan::PreserveShared { .. }),
            "candidate nested inside a survivor must PreserveShared, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// External shared dir (two instances, same external wd) → PreserveShared
    /// (no scrub of the survivor's backend config).
    #[test]
    fn external_shared_is_preserve_shared() {
        let home = tmp_home("extshared");
        let shared = tmp_home("shared-target");
        let fleet = fleet_with(
            &home,
            &[
                ("victim", Some(shared.to_str().unwrap())),
                ("survivor", Some(shared.to_str().unwrap())),
            ],
        );
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &shared);
        assert!(
            matches!(plan, CleanupPlan::PreserveShared { .. }),
            "external dir shared with a survivor must PreserveShared, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&shared).ok();
    }

    /// External, UNSHARED user dir → ScrubExclusive (agend-files only, never whole-tree).
    #[test]
    fn external_unshared_is_scrub_exclusive() {
        let home = tmp_home("extexcl");
        let ext = tmp_home("ext-target");
        let fleet = fleet_with(&home, &[("victim", Some(ext.to_str().unwrap()))]);
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &ext);
        assert!(
            matches!(plan, CleanupPlan::ScrubExclusive(_)),
            "unshared external user dir must ScrubExclusive, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ext).ok();
    }

    /// Fail-closed: no fleet snapshot → PreserveAmbiguous.
    #[test]
    fn no_snapshot_is_preserve_ambiguous() {
        let home = tmp_home("nosnap");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        let plan = plan_cleanup(None, &home, &["victim"], "victim", &vdir);
        assert!(
            matches!(plan, CleanupPlan::PreserveAmbiguous { .. }),
            "missing snapshot must fail closed, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Fail-closed: candidate contains '..' → PreserveAmbiguous.
    #[test]
    fn dotdot_candidate_is_preserve_ambiguous() {
        let home = tmp_home("dotdot");
        let fleet = fleet_with(&home, &[("victim", None)]);
        let candidate = crate::paths::workspace_dir(&home).join("victim/../escape");
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &candidate);
        assert!(
            matches!(plan, CleanupPlan::PreserveAmbiguous { .. }),
            "dotdot candidate must fail closed, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Under workspace root but NOT the victim's exact default (aliasing a
    /// NON-live sibling dir) → PreserveAmbiguous (unproven, not removed).
    #[test]
    fn under_workspace_nondefault_is_preserve_ambiguous() {
        let home = tmp_home("nondefault");
        let other = ws(&home, "not-victim");
        std::fs::create_dir_all(&other).unwrap();
        // Only the victim is in the fleet; `not-victim` dir exists but no instance.
        let fleet = fleet_with(&home, &[("victim", Some(other.to_str().unwrap()))]);
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &other);
        assert!(
            matches!(plan, CleanupPlan::PreserveAmbiguous { .. }),
            "non-default workspace path must fail closed, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Plain leaf symlink whose target is a live sibling's dir → PreserveShared
    /// (canonicalization resolves the alias to the shared target).
    #[test]
    #[cfg(unix)]
    fn plain_leaf_symlink_alias_is_preserve_shared() {
        let home = tmp_home("symlink");
        let sib = ws(&home, "sibling");
        std::fs::create_dir_all(&sib).unwrap();
        let link = ws(&home, "victim");
        std::os::unix::fs::symlink(&sib, &link).unwrap();
        let fleet = fleet_with(
            &home,
            &[("victim", Some(link.to_str().unwrap())), ("sibling", None)],
        );
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &link);
        assert!(
            matches!(plan, CleanupPlan::PreserveShared { .. }),
            "leaf symlink to a live sibling must PreserveShared, got {plan:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
