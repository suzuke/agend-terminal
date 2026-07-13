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

/// Symmetric-canonical-overlap survivor check shared by the workspace and
/// deployment planners. Returns `Some(Preserve*)` when any live instance NOT in
/// `cohort` resolves to (or overlaps, or ambiguously might alias) the candidate;
/// `None` when the candidate is provably unshared.
fn survivor_conflict(
    snapshot: &FleetConfig,
    home: &Path,
    cohort: &[&str],
    candidate: &Path,
    candidate_canonical: &Path,
) -> Option<CleanupPlan> {
    for name in snapshot.instances.keys() {
        if cohort.contains(&name.as_str()) {
            continue;
        }
        let Some(swd) = instance_wd(snapshot, home, name) else {
            continue;
        };
        match dunce::canonicalize(&swd) {
            Ok(sc) => {
                if paths_overlap(&sc, candidate_canonical) {
                    return Some(CleanupPlan::PreserveShared {
                        reason: format!(
                            "surviving instance '{name}' resolves to overlapping dir {}",
                            sc.display()
                        ),
                    });
                }
            }
            Err(e) => {
                // Un-canonicalizable survivor. `NotFound` is definitive absence —
                // no data of the survivor's exists to destroy through the
                // candidate — so it only fails closed when the RAW survivor path
                // lexically overlaps the candidate (its FUTURE home would sit
                // inside the tree being removed). EVERY other failure
                // (permission-hidden parent, symlink loop, I/O) leaves the
                // survivor's real location unknowable: it may alias the candidate
                // invisibly → PreserveAmbiguous (R5 blocker 2: the old
                // lexical-overlap-only gate missed permission-hidden aliases).
                if e.kind() != std::io::ErrorKind::NotFound
                    || paths_overlap(&swd, candidate)
                    || paths_overlap(&swd, candidate_canonical)
                {
                    return Some(CleanupPlan::PreserveAmbiguous {
                        reason: format!(
                            "surviving instance '{name}' working dir {} is un-canonicalizable ({e}) and may alias the candidate",
                            swd.display()
                        ),
                    });
                }
            }
        }
    }
    None
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

    if let Some(preserve) =
        survivor_conflict(snapshot, home, cohort, candidate, &candidate_canonical)
    {
        return preserve;
    }

    // --- Unshared: prove exclusive ownership ---
    let workspace_root = crate::paths::workspace_dir(home);
    let ws_root_canon = match dunce::canonicalize(&workspace_root) {
        Ok(c) => c,
        Err(e) => {
            // Workspace root itself un-canonicalizable. `NotFound` is definitive:
            // no workspace root exists, so no default ownership is possible and
            // nothing under it exists to alias — a candidate that canonicalized
            // is genuinely external → scrub-only remains provable. EVERY other
            // failure (permission-hidden root, I/O) is ambiguous → fail closed
            // (R5 blocker 2: the old arm authorized ScrubExclusive for any
            // lexically-external candidate even when the root was unreadable).
            if e.kind() != std::io::ErrorKind::NotFound || candidate.starts_with(&workspace_root) {
                return CleanupPlan::PreserveAmbiguous {
                    reason: format!(
                        "workspace root {} does not canonicalize ({e})",
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
/// external user-provided dir proven unshared. Revalidates first. #2764 E: every
/// fs/git failure propagates — a swallowed error must not read as a clean scrub.
pub fn execute_scrub_exclusive(
    home: &Path,
    agent: &str,
    proof: &ScrubExclusiveProof,
) -> Result<(), String> {
    let _ = home; // reserved for symmetry with execute_remove_owned
    revalidate(&proof.original, &proof.canonical)?;
    let dir = &proof.canonical;
    let mut errors: Vec<String> = Vec::new();
    for file in AGEND_FILES {
        let path = dir.join(file);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(format!("remove {}: {e}", path.display())),
        }
    }
    // Clean up the helper worktree if present.
    let wt_dir = dir.join(".worktrees").join(agent);
    if wt_dir.exists() {
        if crate::git_helpers::git_ok(
            dir,
            &[
                "worktree",
                "remove",
                "--force",
                &wt_dir.display().to_string(),
            ],
        ) {
            tracing::info!(dir = %wt_dir.display(), "removed worktree");
        } else {
            errors.push(format!("git worktree remove {} failed", wt_dir.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

// ---------------------------------------------------------------------------
// #2764 E: id-anchored full-delete destructive phase
// ---------------------------------------------------------------------------

/// Typed outcome of [`full_delete_destructive_phase`] — the ONLY destructive
/// path for an instance's working directory and the ONLY remover of its
/// fleet.yaml entry (decision d-20260713091213053694-25).
#[derive(Debug)]
pub enum CleanupOutcome {
    /// Path destruction completed (or nothing existed to destroy) AND — when a
    /// fleet entry existed — that entry was removed via the exact-id CAS.
    Clean,
    /// Fail-closed complete no-op: authority unprovable (fleet unreadable, no
    /// raw entry for an existing dir, no parseable durable id, dotdot,
    /// canonicalization ambiguity, shared/overlapping survivor, or the fresh
    /// pre-mutation recheck no longer authorizes). NOTHING was mutated — the
    /// fleet entry (if any) remains.
    Preserved { reason: String },
    /// Destruction was authorized and attempted, but an operation failed
    /// (worktree/fs/scrub error, or the exact-id fleet CAS was refused).
    /// Never reported as success; the fleet entry is gone only when the CAS
    /// itself succeeded.
    Failed { reason: String },
}

/// #2764 E: the id-anchored destructive phase of `full_delete_instance`.
///
/// Authority chain, all fail-closed:
/// 1. The candidate path comes from the RAW immutable snapshot entry — never
///    `resolve_instance`, whose unrelated failures (ready-pattern, `..`
///    rejection) fall back to a DIFFERENT default target (R5 blocker 1).
/// 2. The entry must carry a parseable durable [`crate::types::InstanceId`]
///    (the generation anchor); legacy/no-id preserves.
/// 3. A missing filesystem entry at the raw candidate is vacuously clean —
///    nothing to destroy — and proceeds straight to the fleet-entry CAS.
/// 4. Otherwise [`plan_cleanup`] must authorize on the raw snapshot, the fleet
///    is re-loaded IMMEDIATELY before mutation, the victim entry must still
///    match its exact id (same-name replacement = survivor → preserve), and a
///    fresh re-plan must re-authorize the SAME action on the SAME canonical
///    target before the (self-revalidating) executor runs.
/// 5. The fleet entry is removed only via the exact-id generation-CAS
///    ([`crate::fleet::remove_instance_from_yaml_cas`]); refusal → `Failed`,
///    and the entry stays (never `Ok`/provenance-erasure on failure).
pub fn full_delete_destructive_phase(
    home: &Path,
    name: &str,
    raw: Option<&FleetConfig>,
) -> CleanupOutcome {
    // Fleet unreadable/corrupt → an entry may exist that we cannot see →
    // nothing is provable → complete no-op.
    let Some(raw) = raw else {
        return CleanupOutcome::Preserved {
            reason: "fleet.yaml unreadable — cannot derive raw entry authority".to_string(),
        };
    };

    let Some(entry) = raw.instances.get(name) else {
        // No raw entry: nothing to CAS-remove. The default dir is only
        // vacuously clean when NOTHING exists at it — an existing dir without
        // a fleet entry has no ownership anchor → preserve.
        let default_dir = crate::paths::workspace_dir(home).join(name);
        return match std::fs::symlink_metadata(&default_dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CleanupOutcome::Clean,
            Ok(_) => CleanupOutcome::Preserved {
                reason: format!(
                    "no fleet entry for '{name}' but {} exists — ownership unprovable",
                    default_dir.display()
                ),
            },
            Err(e) => CleanupOutcome::Preserved {
                reason: format!(
                    "no fleet entry for '{name}' and {} is unreadable ({e})",
                    default_dir.display()
                ),
            },
        };
    };

    // The durable generation anchor: the entry's exact persisted InstanceId.
    let Some(expected_id) = entry
        .id
        .as_deref()
        .and_then(crate::types::InstanceId::parse)
    else {
        return CleanupOutcome::Preserved {
            reason: format!("fleet entry '{name}' has no parseable durable id (legacy entry)"),
        };
    };

    // RAW candidate from the snapshot entry itself (tilde-expanded explicit
    // working_directory, else the default `workspace/<name>`).
    let candidate = match entry.working_directory.as_deref() {
        Some(d) => crate::fleet::resolve::expand_tilde_path(d),
        None => crate::paths::workspace_dir(home).join(name),
    };
    if has_dotdot(&candidate) {
        return CleanupOutcome::Preserved {
            reason: format!(
                "raw working_directory contains '..': {}",
                candidate.display()
            ),
        };
    }

    // Vacuously clean: no filesystem entry at the raw candidate → nothing to
    // destroy; fall through to the fleet-entry CAS.
    let candidate_present = match std::fs::symlink_metadata(&candidate) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return CleanupOutcome::Preserved {
                reason: format!("candidate {} unreadable ({e})", candidate.display()),
            };
        }
    };

    if candidate_present {
        let stale_plan = plan_cleanup(Some(raw), home, &[name], name, &candidate);
        let stale_canonical = match &stale_plan {
            CleanupPlan::RemoveOwned(p) => p.canonical.clone(),
            CleanupPlan::ScrubExclusive(p) => p.canonical.clone(),
            CleanupPlan::PreserveShared { reason } | CleanupPlan::PreserveAmbiguous { reason } => {
                return CleanupOutcome::Preserved {
                    reason: reason.clone(),
                };
            }
        };

        // Immediately re-load the fleet before mutation. The victim is
        // excluded from survivors ONLY because its exact durable id still
        // matches; a same-name replacement (different/absent id) means the
        // snapshot's authority is stale → preserve.
        let fresh = match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
            Ok(f) => f,
            Err(e) => {
                return CleanupOutcome::Preserved {
                    reason: format!("pre-mutation fleet re-load failed ({e})"),
                };
            }
        };
        let fresh_id = fresh
            .instances
            .get(name)
            .and_then(|i| i.id.as_deref())
            .and_then(crate::types::InstanceId::parse);
        if fresh_id != Some(expected_id) {
            return CleanupOutcome::Preserved {
                reason: format!(
                    "victim entry '{name}' no longer matches its durable id \
                     (replaced or removed since the snapshot)"
                ),
            };
        }
        // Fresh re-plan: re-canonicalization + symmetric overlap vs ALL fresh
        // survivors + root/default ownership. It must re-authorize the SAME
        // action on the SAME canonical target; the executor then revalidates
        // original→canonical once more immediately before destruction.
        match plan_cleanup(Some(&fresh), home, &[name], name, &candidate) {
            CleanupPlan::RemoveOwned(fresh_proof)
                if matches!(stale_plan, CleanupPlan::RemoveOwned(_))
                    && fresh_proof.canonical == stale_canonical =>
            {
                if let Err(e) = execute_remove_owned(home, name, &fresh_proof) {
                    return CleanupOutcome::Failed {
                        reason: format!("remove owned workspace: {e}"),
                    };
                }
            }
            CleanupPlan::ScrubExclusive(fresh_proof)
                if matches!(stale_plan, CleanupPlan::ScrubExclusive(_))
                    && fresh_proof.canonical == stale_canonical =>
            {
                if let Err(e) = execute_scrub_exclusive(home, name, &fresh_proof) {
                    return CleanupOutcome::Failed {
                        reason: format!("scrub exclusive dir: {e}"),
                    };
                }
            }
            other => {
                return CleanupOutcome::Preserved {
                    reason: format!(
                        "fresh pre-mutation recheck no longer authorizes the planned action \
                         (was {stale_plan:?}, now {other:?})"
                    ),
                };
            }
        }
    }

    // Path phase clean → the fleet entry may now be removed, ONLY via the
    // exact-id generation-CAS. Refusal (same-name replacement landed, entry
    // vanished, read/write failure) → Failed; the entry is never force-removed
    // and the caller never reports success.
    if let Err(e) = crate::fleet::remove_instance_from_yaml_cas(home, name, &expected_id) {
        return CleanupOutcome::Failed {
            reason: format!("fleet entry exact-id CAS removal refused: {e}"),
        };
    }
    CleanupOutcome::Clean
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

    /// R5 blocker 2: an UN-CANONICALIZABLE survivor for a reason OTHER than
    /// NotFound (permission-hidden parent) must PreserveAmbiguous even when the
    /// raw paths do not lexically overlap — the survivor may alias the
    /// candidate invisibly.
    #[test]
    #[cfg(unix)]
    fn permission_hidden_survivor_is_preserve_ambiguous() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("permsurv");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        // Survivor's wd sits under a 0o000 parent OUTSIDE the workspace — no
        // lexical overlap with the candidate.
        let hidden_parent = home.join("hidden-parent");
        std::fs::create_dir_all(hidden_parent.join("survivor-wd")).unwrap();
        let fleet = fleet_with(
            &home,
            &[
                ("victim", None),
                (
                    "survivor",
                    Some(&hidden_parent.join("survivor-wd").display().to_string()),
                ),
            ],
        );
        std::fs::set_permissions(&hidden_parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can traverse anything — skip when the hide didn't take.
        let hidden = dunce::canonicalize(hidden_parent.join("survivor-wd")).is_err();
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &vdir);
        std::fs::set_permissions(&hidden_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if hidden {
            assert!(
                matches!(plan, CleanupPlan::PreserveAmbiguous { .. }),
                "permission-hidden survivor must fail closed, got {plan:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// R5 blocker 2: a workspace ROOT that fails to canonicalize for a reason
    /// OTHER than NotFound (permission-hidden home) must PreserveAmbiguous —
    /// never fall through to ScrubExclusive of a lexically-external candidate.
    #[test]
    #[cfg(unix)]
    fn permission_hidden_workspace_root_is_preserve_ambiguous() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("permroot");
        std::fs::create_dir_all(crate::paths::workspace_dir(&home)).unwrap();
        let ext = tmp_home("permroot-ext");
        let fleet = fleet_with(&home, &[("victim", Some(ext.to_str().unwrap()))]);
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hidden = dunce::canonicalize(crate::paths::workspace_dir(&home)).is_err();
        let plan = plan_cleanup(Some(&fleet), &home, &["victim"], "victim", &ext);
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        if hidden {
            assert!(
                matches!(plan, CleanupPlan::PreserveAmbiguous { .. }),
                "permission-hidden workspace root must fail closed, got {plan:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ext).ok();
    }

    // ── #2764 E: full_delete_destructive_phase (id-anchored) ────────────────

    /// Write fleet.yaml with EXPLICIT ids and load it. Returns the loaded
    /// snapshot plus each instance's id in entry order.
    fn fleet_with_ids(
        home: &Path,
        entries: &[(&str, Option<&str>)],
    ) -> (FleetConfig, Vec<crate::types::InstanceId>) {
        let mut yaml = String::from("instances:\n");
        let mut ids = Vec::new();
        for (name, wd) in entries {
            let id = crate::types::InstanceId::new();
            yaml.push_str(&format!(
                "  {name}:\n    backend: claude\n    id: {}\n",
                id.full()
            ));
            if let Some(d) = wd {
                yaml.push_str(&format!("    working_directory: {d}\n"));
            }
            ids.push(id);
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
        let fleet = FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap();
        (fleet, ids)
    }

    fn fleet_still_has(home: &Path, name: &str) -> bool {
        FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .map(|c| c.instances.contains_key(name))
            .unwrap_or(false)
    }

    /// Happy path: exclusive canonical default dir → whole tree removed AND the
    /// fleet entry is removed via the exact-id CAS.
    #[test]
    fn phase_exact_owned_default_removes_and_cas_removes_entry() {
        let home = tmp_home("ph-owned");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("f.txt"), "x").unwrap();
        let (fleet, _) = fleet_with_ids(&home, &[("victim", Some(vdir.to_str().unwrap()))]);
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(matches!(out, CleanupOutcome::Clean), "got {out:?}");
        assert!(!vdir.exists(), "owned default dir must be removed");
        assert!(
            !fleet_still_has(&home, "victim"),
            "fleet entry must be CAS-removed after a Clean path phase"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// External exclusive user dir → EVERY canonical scrub entry is removed,
    /// user files survive, entry CAS-removed. Port of the legacy 19-entry
    /// drift guard onto the E entry: the list is spelled out LITERALLY
    /// (independent of `AGEND_FILES`) so dropping an entry from the canonical
    /// constant regresses this test — including the 5 Kiro paths the 2026-04-14
    /// `mcp/handlers.rs` copy drifted on.
    #[test]
    fn phase_external_scrub_removes_all_19_canonical_entries_keeps_user_files() {
        let home = tmp_home("ph-scrub");
        let ext = tmp_home("ph-scrub-ext");
        let canonical: [&str; 19] = [
            // Claude (6)
            ".claude/settings.local.json",
            "mcp-config.json",
            "claude-settings.json",
            "statusline.sh",
            "statusline.json",
            ".claude/rules/agend.md",
            // Gemini (1)
            ".gemini/settings.json",
            // OpenCode (2)
            "opencode.json",
            "instructions/agend.md",
            // Codex (2)
            ".codex/config.toml",
            "AGENTS.md",
            // Kiro (8) — the last 5 are the paths the drifted 14-entry copy missed
            ".kiro/settings/mcp.json",
            ".kiro/settings/agend-mcp-wrapper.sh",
            ".kiro/steering/agend.md",
            ".kiro/agents/agend.json",
            ".kiro/agents/agend-prompt.md",
            ".kiro/agents/default.json",
            ".kiro/prompts/agend.md",
            ".kiro/settings.json",
        ];
        assert_eq!(
            AGEND_FILES.len(),
            canonical.len(),
            "AGEND_FILES drifted from the canonical 19-entry list"
        );
        for rel in &canonical {
            let p = ext.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, "x").unwrap();
        }
        std::fs::write(ext.join("user-code.rs"), "fn main(){}").unwrap();
        let (fleet, _) = fleet_with_ids(&home, &[("victim", Some(ext.to_str().unwrap()))]);
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(matches!(out, CleanupOutcome::Clean), "got {out:?}");
        for rel in &canonical {
            assert!(
                !ext.join(rel).exists(),
                "canonical entry not removed: {rel}"
            );
        }
        assert!(
            ext.join("user-code.rs").exists(),
            "user file must survive the selective scrub"
        );
        assert!(!fleet_still_has(&home, "victim"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ext).ok();
    }

    /// Legacy/no-id entry → Preserved, dir untouched, entry retained
    /// (decision d-20260713090932932337-24: the durable InstanceId is the
    /// generation anchor; without it nothing may be destroyed). Hand-built
    /// snapshot — the YAML loader auto-backfills ids, so a no-id entry can
    /// only reach the phase when that backfill could not persist.
    #[test]
    fn phase_no_id_entry_preserves() {
        let home = tmp_home("ph-noid");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("f.txt"), "x").unwrap();
        let mut fleet = FleetConfig::default();
        fleet.instances.insert(
            "victim".to_string(),
            crate::fleet::InstanceConfig {
                working_directory: Some(vdir.display().to_string()),
                ..Default::default()
            },
        );
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "no-id entry must preserve, got {out:?}"
        );
        assert!(vdir.join("f.txt").exists(), "dir must be untouched");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Raw working_directory containing '..' → Preserved before any mutation
    /// (R5 blocker 1: the raw record's own path is the authority — no
    /// resolver fallback may substitute a different deletion target).
    #[test]
    fn phase_dotdot_raw_wd_preserves() {
        let home = tmp_home("ph-dotdot");
        let escape = ws(&home, "escape");
        std::fs::create_dir_all(&escape).unwrap();
        std::fs::write(escape.join("f.txt"), "x").unwrap();
        let wd = ws(&home, "victim").join("..").join("escape");
        let (fleet, _) = fleet_with_ids(&home, &[("victim", Some(&wd.display().to_string()))]);
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "dotdot raw wd must preserve, got {out:?}"
        );
        assert!(escape.join("f.txt").exists());
        assert!(fleet_still_has(&home, "victim"), "entry must be retained");
        std::fs::remove_dir_all(&home).ok();
    }

    /// No fleet entry + nothing on disk at the default dir → vacuously Clean
    /// (ghost delete keeps working); no CAS is attempted.
    #[test]
    fn phase_no_entry_no_dir_is_vacuously_clean() {
        let home = tmp_home("ph-ghost");
        let fleet = FleetConfig::default();
        let out = full_delete_destructive_phase(&home, "ghost", Some(&fleet));
        assert!(matches!(out, CleanupOutcome::Clean), "got {out:?}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// No fleet entry but the default workspace dir EXISTS → Preserved (an
    /// existing dir without a raw-entry ownership anchor may not be removed).
    #[test]
    fn phase_no_entry_existing_default_dir_preserves() {
        let home = tmp_home("ph-ghostdir");
        let gdir = ws(&home, "ghost");
        std::fs::create_dir_all(&gdir).unwrap();
        std::fs::write(gdir.join("f.txt"), "x").unwrap();
        let fleet = FleetConfig::default();
        let out = full_delete_destructive_phase(&home, "ghost", Some(&fleet));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "entry-less existing dir must preserve, got {out:?}"
        );
        assert!(gdir.join("f.txt").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Entry with id but NOTHING at the raw candidate → vacuously Clean and
    /// the fleet entry is still CAS-removed (deleting an instance whose
    /// workspace never materialized must succeed).
    #[test]
    fn phase_missing_candidate_is_clean_and_cas_removes_entry() {
        let home = tmp_home("ph-vacuous");
        let (fleet, _) = fleet_with_ids(&home, &[("victim", None)]);
        assert!(!ws(&home, "victim").exists(), "precondition: no dir");
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(matches!(out, CleanupOutcome::Clean), "got {out:?}");
        assert!(!fleet_still_has(&home, "victim"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// ABA: between the raw snapshot and the phase, the victim was replaced by
    /// a SAME-NAME instance with a DIFFERENT id. The fresh exact-id recheck
    /// must preserve — the replacement is a survivor, its dir and entry stay.
    #[test]
    fn phase_same_name_different_id_replacement_preserves() {
        let home = tmp_home("ph-aba");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("f.txt"), "x").unwrap();
        let (raw, _) = fleet_with_ids(&home, &[("victim", Some(vdir.to_str().unwrap()))]);
        // Same-name replacement lands after the snapshot (new id).
        let replacement = crate::types::InstanceId::new();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  victim:\n    backend: claude\n    id: {}\n    working_directory: {}\n",
                replacement.full(),
                vdir.display()
            ),
        )
        .unwrap();
        let out = full_delete_destructive_phase(&home, "victim", Some(&raw));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "same-name different-id replacement must preserve, got {out:?}"
        );
        assert!(
            vdir.join("f.txt").exists(),
            "replacement's dir must survive"
        );
        assert!(
            fleet_still_has(&home, "victim"),
            "replacement's entry must survive"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Victim entry REMOVED between snapshot and phase → preserve (no stale
    /// destruction on vanished authority).
    #[test]
    fn phase_entry_vanished_since_snapshot_preserves() {
        let home = tmp_home("ph-vanish");
        let vdir = ws(&home, "victim");
        std::fs::create_dir_all(&vdir).unwrap();
        let (raw, _) = fleet_with_ids(&home, &[("victim", Some(vdir.to_str().unwrap()))]);
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        let out = full_delete_destructive_phase(&home, "victim", Some(&raw));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "vanished entry must preserve, got {out:?}"
        );
        assert!(vdir.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Shared alias (victim wd = live sibling's default) → Preserved AND the
    /// victim's fleet entry is retained (no CAS on a non-Clean path phase).
    #[test]
    fn phase_shared_alias_preserves_and_retains_entry() {
        let home = tmp_home("ph-shared");
        let sib = ws(&home, "sibling");
        std::fs::create_dir_all(&sib).unwrap();
        std::fs::write(sib.join("f.txt"), "x").unwrap();
        let (fleet, _) = fleet_with_ids(
            &home,
            &[("victim", Some(sib.to_str().unwrap())), ("sibling", None)],
        );
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "sibling alias must preserve, got {out:?}"
        );
        assert!(sib.join("f.txt").exists(), "sibling dir must be untouched");
        assert!(
            fleet_still_has(&home, "victim"),
            "victim entry must be retained on Preserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Dangling-symlink candidate: something EXISTS at the raw path (the link)
    /// but it does not canonicalize → Preserved (not vacuous-clean).
    #[test]
    #[cfg(unix)]
    fn phase_dangling_symlink_candidate_preserves() {
        let home = tmp_home("ph-dangle");
        let link = ws(&home, "victim");
        std::fs::create_dir_all(crate::paths::workspace_dir(&home)).unwrap();
        std::os::unix::fs::symlink(home.join("nowhere"), &link).unwrap();
        let (fleet, _) = fleet_with_ids(&home, &[("victim", Some(link.to_str().unwrap()))]);
        let out = full_delete_destructive_phase(&home, "victim", Some(&fleet));
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "dangling symlink candidate must preserve, got {out:?}"
        );
        assert!(fleet_still_has(&home, "victim"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// Unreadable fleet (None snapshot) → Preserved (an entry may exist that
    /// we cannot see; nothing is provable).
    #[test]
    fn phase_unreadable_fleet_preserves() {
        let home = tmp_home("ph-nofleet");
        let out = full_delete_destructive_phase(&home, "victim", None);
        assert!(
            matches!(out, CleanupOutcome::Preserved { .. }),
            "unreadable fleet must preserve, got {out:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
