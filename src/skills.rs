//! Sprint 60 W2 PR-1 (#P1-1 Skills System Plan IMPL) — 4-backend
//! community skill discovery via unified source + symlink (Windows
//! copy fallback). (Originally 5 backends; #1580 retired gemini-cli —
//! see `BACKEND_SKILL_DIRS`.)
//!
//! ## Architecture
//!
//! ```text
//! ~/.agend-terminal/skills/          ← unified source
//!   ├── skill-forge/SKILL.md
//!   ├── opencli-adapter-author/SKILL.md
//!   └── ...
//!
//! agent-working-dir/
//!   ├── .claude/skills/  → symlink → ~/.agend-terminal/skills/
//!   ├── .codex/skills/   → symlink → ~/.agend-terminal/skills/
//!   ├── .opencode/skills/→ symlink → ~/.agend-terminal/skills/
//!   └── .kiro/skills/    → symlink → ~/.agend-terminal/skills/
//! ```
//!
//! Unified source means: one skill file, all 4 backends discover.
//! No file copies on Unix (symlink = zero maintenance). Windows
//! falls back to copy with hash-compare staleness detection on
//! subsequent installs (file-watch infra is out of scope per
//! Sprint 60 P2-C deferral).
//!
//! ## skills-lock.json
//!
//! Tracks the source + install metadata for each skill so `update`
//! knows where to refetch and so cross-machine state is auditable.
//! Schema mirrors `package-lock.json` — opaque `version` (commit SHA
//! for git sources, mtime for path sources) lets future tooling
//! detect drift without re-shelling-out for every check.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Per-backend conventional skills directories inside the agent's working
/// tree; the daemon installs a symlink (or copy on Windows) pointing at the
/// unified source. (Originally 5 backends per dispatch
/// m-20260509214553949181-385; #1580 retired gemini-cli's `.gemini/skills`.)
pub const BACKEND_SKILL_DIRS: &[(&str, &str)] = &[
    ("claude", ".claude/skills"),
    ("codex", ".codex/skills"),
    ("opencode", ".opencode/skills"),
    ("kiro", ".kiro/skills"),
];

/// Unified skill source root: `<home>/skills/`.
pub fn skills_root(home: &Path) -> PathBuf {
    home.join("skills")
}

/// Ensure the unified source root exists. Idempotent.
pub fn ensure_skills_root(home: &Path) -> Result<PathBuf> {
    let root = skills_root(home);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("ensure_skills_root: create_dir_all {}", root.display()))?;
    Ok(root)
}

/// Path to the skills lock file: `<home>/skills-lock.json`.
pub fn skills_lock_path(home: &Path) -> PathBuf {
    home.join("skills-lock.json")
}

/// Per-skill lock entry. Schema mirrors `package-lock.json`'s
/// dependency entries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillLockEntry {
    /// Where the skill came from: `path:<abs path>` for local copies,
    /// `git:<url>` for git clones.
    #[serde(default)]
    pub source: String,
    /// Opaque version pin. Git sources: commit SHA. Path sources:
    /// SOURCE-DIR mtime in RFC 3339. Empty when not yet pinned.
    #[serde(default)]
    pub version: String,
    /// RFC 3339 install timestamp.
    #[serde(default)]
    pub installed_at: String,
}

/// `skills-lock.json` shape: `{"skills": {"<name>": SkillLockEntry, ...}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsLock {
    #[serde(default)]
    pub skills: BTreeMap<String, SkillLockEntry>,
}

impl SkillsLock {
    pub fn read(home: &Path) -> Result<Self> {
        let path = skills_lock_path(home);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read skills-lock.json at {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("parse skills-lock.json at {}", path.display()))
    }

    pub fn write(&self, home: &Path) -> Result<()> {
        let path = skills_lock_path(home);
        let body = serde_json::to_string_pretty(self).context("serialize skills-lock.json")?;
        // Atomic write to avoid partial-state corruption on crash.
        crate::store::atomic_write(&path, body.as_bytes())
            .with_context(|| format!("atomic_write skills-lock.json at {}", path.display()))?;
        Ok(())
    }
}

/// One installed skill — name (directory name under `<home>/skills/`)
/// + lock metadata (or default if no lock entry exists).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub source: String,
    pub version: String,
    pub installed_at: String,
}

/// List skills currently in the unified source. Cross-references the
/// lock file for source/version metadata; skills that exist on disk
/// but not in the lock surface with empty source/version (operator
/// dropped a directory in by hand).
pub fn list(home: &Path) -> Result<Vec<Skill>> {
    let root = skills_root(home);
    let mut names = Vec::new();
    if root.exists() {
        for entry in std::fs::read_dir(&root)
            .with_context(|| format!("read_dir {}", root.display()))?
            .flatten()
        {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    let lock = SkillsLock::read(home).unwrap_or_default();
    Ok(names
        .into_iter()
        .map(|name| {
            let entry = lock.skills.get(&name).cloned().unwrap_or_default();
            Skill {
                name,
                source: entry.source,
                version: entry.version,
                installed_at: entry.installed_at,
            }
        })
        .collect())
}

/// Add a skill from a local directory or git URL. The skill's
/// directory name is derived from the source basename (final URL
/// path component or directory name). Idempotent: re-adding an
/// existing name overwrites it (caller can drive `update` semantics
/// from the same path).
pub fn add(home: &Path, source: &str) -> Result<Skill> {
    let root = ensure_skills_root(home)?;
    let (name, source_kind) = classify_source(source)?;
    let dest = root.join(&name);

    match source_kind {
        SourceKind::Path(p) => {
            if !p.exists() {
                return Err(anyhow!("skill source path does not exist: {}", p.display()));
            }
            // Wipe destination first so re-add is deterministic.
            if dest.exists() {
                std::fs::remove_dir_all(&dest)
                    .with_context(|| format!("clean dest {}", dest.display()))?;
            }
            copy_dir_recursive(&p, &dest)
                .with_context(|| format!("copy {} → {}", p.display(), dest.display()))?;
        }
        SourceKind::Git(url, subdir) => {
            if dest.exists() {
                std::fs::remove_dir_all(&dest)
                    .with_context(|| format!("clean dest {}", dest.display()))?;
            }
            if let Some(sub) = subdir {
                let tmp = root.join(".git-clone-tmp");
                if tmp.exists() {
                    std::fs::remove_dir_all(&tmp)
                        .with_context(|| format!("clean clone tmp {}", tmp.display()))?;
                }
                let status = std::process::Command::new("git")
                    .args(["clone", "--depth=1", url.as_str()])
                    .arg(&tmp)
                    .status()
                    .context("spawn git clone")?;
                if !status.success() {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(anyhow!("git clone failed for {url}: status={status}"));
                }
                let sub_path = tmp.join(&sub);
                let resolved = sub_path.canonicalize().unwrap_or_default();
                let tmp_canon = tmp.canonicalize().unwrap_or_default();
                if !resolved.starts_with(&tmp_canon) || resolved == tmp_canon {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(anyhow!(
                        "subdir path '{sub}' escapes clone root — path traversal rejected"
                    ));
                }
                if !resolved.is_dir() {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(anyhow!(
                        "subdirectory '{sub}' not found in cloned repo {url}"
                    ));
                }
                copy_dir_recursive(&sub_path, &dest)
                    .with_context(|| format!("copy subdir {sub} → {}", dest.display()))?;
                let _ = std::fs::remove_dir_all(&tmp);
            } else {
                let status = std::process::Command::new("git")
                    .args(["clone", "--depth=1", url.as_str()])
                    .arg(&dest)
                    .status()
                    .context("spawn git clone")?;
                if !status.success() {
                    return Err(anyhow!("git clone failed for {url}: status={status}"));
                }
            }
        }
    }

    // Write lock entry. Version pin is best-effort: git → HEAD SHA,
    // path → mtime. Failures fall back to empty string (lock entry
    // still records the source for `update`).
    let version = compute_version(&dest, source);
    let installed_at = chrono::Utc::now().to_rfc3339();
    let entry = SkillLockEntry {
        source: source.to_string(),
        version,
        installed_at: installed_at.clone(),
    };
    let mut lock = SkillsLock::read(home).unwrap_or_default();
    lock.skills.insert(name.clone(), entry.clone());
    lock.write(home)?;

    Ok(Skill {
        name,
        source: entry.source,
        version: entry.version,
        installed_at,
    })
}

/// Remove a skill from the unified source + drop its lock entry.
/// Idempotent: returns Ok even if the skill doesn't exist (operator
/// already cleaned up).
pub fn remove(home: &Path, name: &str) -> Result<()> {
    let dest = skills_root(home).join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .with_context(|| format!("remove_dir_all {}", dest.display()))?;
    }
    let mut lock = SkillsLock::read(home).unwrap_or_default();
    if lock.skills.remove(name).is_some() {
        lock.write(home)?;
    }
    Ok(())
}

/// Update a skill — re-runs `add` against the lock-recorded source.
/// Returns `Err` if the skill has no lock entry (caller doesn't know
/// where to refetch from).
pub fn update(home: &Path, name: &str) -> Result<Skill> {
    let lock = SkillsLock::read(home).unwrap_or_default();
    let entry = lock
        .skills
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("no lock entry for skill '{name}' — use `add` first"))?;
    if entry.source.is_empty() {
        return Err(anyhow!(
            "skill '{name}' has no recorded source — manual reinstall required"
        ));
    }
    add(home, &entry.source)
}

/// Update every skill that has a recorded source. Returns the per-
/// skill outcome list (Ok/Err) so callers can surface partial
/// failures.
pub fn update_all(home: &Path) -> Vec<(String, Result<Skill>)> {
    let lock = SkillsLock::read(home).unwrap_or_default();
    let mut out = Vec::new();
    for name in lock.skills.keys() {
        out.push((name.clone(), update(home, name)));
    }
    out
}

/// Install symlinks (or copy fallback on Windows) for every backend
/// in `BACKEND_SKILL_DIRS` so each backend's skill discovery path
/// resolves to the unified source. Idempotent: pre-existing daemon-
/// managed symlinks/copies are replaced; pre-existing non-managed
/// directories are left alone with a tracing::warn.
///
/// Sprint 61 W1 PR-2 (#P0-2): when `filter` is `Some(allowlist)`, only
/// the named skills land in each backend's path — built by staging a
/// scratch source dir under `<home>/.skills-stage/<digest>/` containing
/// only the allowlisted entries from the unified source, then installing
/// from the stage. `Some(vec![])` is meaningful and explicit — agent
/// gets per-backend dirs that contain only the daemon-managed marker
/// (no skills). `None` preserves the W1 PR-1 #585 install-all default.
pub fn install_for_agent(
    home: &Path,
    working_dir: &Path,
    filter: Option<&[String]>,
) -> Result<Vec<InstallOutcome>> {
    install_for_agent_backend(home, working_dir, filter, None)
}

/// Backend-scoped variant: when `backend` is `Some`, only install
/// skills for the matching backend directory. When `None`, install all.
pub fn install_for_agent_backend(
    home: &Path,
    working_dir: &Path,
    filter: Option<&[String]>,
    backend: Option<&str>,
) -> Result<Vec<InstallOutcome>> {
    let source = ensure_skills_root(home)?;
    let staged_source = match filter {
        None => source,
        Some(allowlist) => stage_filtered_source(home, &source, allowlist)?,
    };
    let dirs: Vec<_> = BACKEND_SKILL_DIRS
        .iter()
        .filter(|(name, _)| backend.is_none_or(|b| *name == b))
        .collect();
    let mut outcomes = Vec::with_capacity(dirs.len());
    for (name, rel) in dirs {
        let target = working_dir.join(rel);
        let outcome = install_one(&staged_source, &target, name);
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Stage a filtered copy of the unified source containing only the
/// allowlisted skills. Stage path is `<home>/.skills-stage/<digest>/`
/// where `digest` is a stable per-allowlist SHA-256 prefix so multiple
/// distinct filters coexist without overwrite. Rebuilt every call
/// (idempotent + cheap — copies only of allowlisted names, not the
/// full unified source).
///
/// Sprint 62 W1 PR-1 (#P0-1): replaces the Sprint 61 #586 FNV-1a
/// digest with SHA-256-prefix per #586 reviewer minor caveat (FNV non-
/// cryptographic). 16-hex-char prefix keeps directory names short
/// while giving 64 bits of collision resistance. Pre-existing
/// FNV-named stages become legacy on next daemon start; W1 PR-2
/// skills-stage GC will sweep them.
fn stage_filtered_source(home: &Path, source: &Path, allowlist: &[String]) -> Result<PathBuf> {
    let mut digest_input = String::new();
    let mut sorted: Vec<&String> = allowlist.iter().collect();
    sorted.sort();
    for name in &sorted {
        digest_input.push_str(name);
        digest_input.push('\n');
    }
    let digest = stage_digest(digest_input.as_bytes());
    let stage = home.join(".skills-stage").join(&digest);
    if stage.exists() {
        std::fs::remove_dir_all(&stage)
            .with_context(|| format!("clean stage {}", stage.display()))?;
    }
    std::fs::create_dir_all(&stage).with_context(|| format!("create stage {}", stage.display()))?;
    for name in allowlist {
        let from = source.join(name);
        if !from.exists() {
            tracing::warn!(skill = %name, source = %source.display(),
                "stage_filtered_source: allowlisted skill not present, skipping");
            continue;
        }
        let to = stage.join(name);
        copy_dir_recursive(&from, &to)?;
    }
    Ok(stage)
}

/// Sprint 62 W1 PR-2 (#P0-2): GC report — what `cleanup_stale_stages`
/// found and acted on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageGcReport {
    pub candidates: usize,
    pub deleted: usize,
    pub preserved_recent: usize,
    pub preserved_excluded: usize,
}

/// Sprint 62 W1 PR-2 (#P0-2): GC stale `<home>/.skills-stage/<digest>/`
/// directories. Closes the Sprint 61 #586 reviewer minor caveat
/// (stage dirs accumulate without GC) and the Sprint 62 PLAN review
/// reviewer minor add (TOCTOU safety via same-run exclusion).
///
/// - `retention_secs`: stages with mtime older than this threshold are
///   eligible for deletion. Recommended 7-day window for daemon-init
///   invocation.
/// - `exclude_digests`: stage dir names to never delete (currently-
///   resolved stages from same run, per reviewer minor add). Empty
///   when invoked at daemon-init (no installs have happened yet);
///   non-empty for any future periodic-GC site.
///
/// Returns counts so callers can log/test the GC outcome. Fail-open:
/// IO failures during a single dir removal log warn + continue;
/// missing stage root short-circuits with empty report.
pub fn cleanup_stale_stages(
    home: &Path,
    retention_secs: u64,
    exclude_digests: &[String],
) -> Result<StageGcReport> {
    let stage_root = home.join(".skills-stage");
    let mut report = StageGcReport::default();
    let entries = match std::fs::read_dir(&stage_root) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => {
            return Err(anyhow!("read_dir {}: {}", stage_root.display(), e));
        }
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        report.candidates += 1;
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if exclude_digests.iter().any(|d| d == &name) {
            report.preserved_excluded += 1;
            continue;
        }
        let mtime = entry.metadata().and_then(|m| m.modified()).ok();
        let elapsed = mtime
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if elapsed < retention_secs {
            report.preserved_recent += 1;
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.deleted += 1;
                tracing::info!(stage = %name, elapsed_secs = elapsed,
                    "skills-stage GC: removed stale stage dir");
            }
            Err(e) => {
                tracing::warn!(stage = %name, error = %e,
                    "skills-stage GC: removal failed, skipping");
            }
        }
    }
    Ok(report)
}

/// SHA-256 prefix digest used for the filtered-source stage path.
/// 16 hex chars = first 8 bytes of SHA-256 output → 64 bits of
/// collision resistance, sufficient at the daemon's scale (collision
/// probability is birthday-bound at ~2^32 distinct allowlists; an
/// AgEnD home holding billions of distinct skill filters is not a
/// realistic scenario). Cryptographic in the underlying primitive,
/// matching the Sprint 61 #586 reviewer caveat resolution.
fn stage_digest(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let full = Sha256::digest(data);
    hex::encode(&full[..8])
}

/// Per-backend install result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub backend: String,
    pub target: PathBuf,
    pub mode: InstallMode,
    pub skipped_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMode {
    Symlink,
    Copy,
    Skipped,
}

fn install_one(source: &Path, target: &Path, backend: &str) -> InstallOutcome {
    let parent = match target.parent() {
        Some(p) => p,
        None => {
            return InstallOutcome {
                backend: backend.to_string(),
                target: target.to_path_buf(),
                mode: InstallMode::Skipped,
                skipped_reason: Some("target has no parent".to_string()),
            }
        }
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        return InstallOutcome {
            backend: backend.to_string(),
            target: target.to_path_buf(),
            mode: InstallMode::Skipped,
            skipped_reason: Some(format!("create_dir_all {}: {e}", parent.display())),
        };
    }

    // Pre-existing non-managed directory: don't overwrite operator's
    // hand-crafted skills dir. Symlinks + previously-managed copies
    // are safe to replace.
    //
    // #1229 Bug 2: use symlink_metadata to detect broken symlinks
    // (target.exists() follows symlinks and returns false for dangling).
    let entry_exists = target.symlink_metadata().is_ok();
    if entry_exists {
        let is_symlink = target
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let is_managed_copy = target.join(".agend-skills-managed").exists();
        if !is_symlink && !is_managed_copy {
            return InstallOutcome {
                backend: backend.to_string(),
                target: target.to_path_buf(),
                mode: InstallMode::Skipped,
                skipped_reason: Some(
                    "target exists and is not daemon-managed (no .agend-skills-managed marker)"
                        .to_string(),
                ),
            };
        }
        if is_symlink {
            // Cross-platform symlink removal: Unix uses `remove_file`
            // (symlink is a regular file from rmdir's perspective);
            // Windows directory symlinks require `remove_dir`. Try
            // `remove_file` first (Unix happy path), fall back to
            // `remove_dir` for Windows directory symlinks. Either
            // succeeds or both have already-gone semantics.
            let _ = std::fs::remove_file(target).or_else(|_| std::fs::remove_dir(target));
        } else {
            let _ = std::fs::remove_dir_all(target);
        }
    }

    match try_symlink(source, target) {
        Ok(()) => InstallOutcome {
            backend: backend.to_string(),
            target: target.to_path_buf(),
            mode: InstallMode::Symlink,
            skipped_reason: None,
        },
        Err(_e) => match copy_with_marker(source, target) {
            Ok(()) => InstallOutcome {
                backend: backend.to_string(),
                target: target.to_path_buf(),
                mode: InstallMode::Copy,
                skipped_reason: None,
            },
            Err(copy_err) => InstallOutcome {
                backend: backend.to_string(),
                target: target.to_path_buf(),
                mode: InstallMode::Skipped,
                skipped_reason: Some(format!("symlink + copy fallback both failed: {copy_err}")),
            },
        },
    }
}

#[cfg(unix)]
fn try_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn try_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(source, target)
}

fn copy_with_marker(source: &Path, target: &Path) -> Result<()> {
    copy_dir_recursive(source, target)?;
    std::fs::write(target.join(".agend-skills-managed"), b"daemon-managed\n")
        .with_context(|| format!("write marker {}/.agend-skills-managed", target.display()))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("create_dir_all {}", dst.display()))?;
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("read_dir {}", src.display()))?
        .flatten()
    {
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
        // Symlinks inside the source are not duplicated — keep MVP
        // simple. Operators wanting symlinks-inside-skill should
        // file a follow-up.
    }
    Ok(())
}

#[derive(Debug)]
enum SourceKind {
    Path(PathBuf),
    /// (clone_url, optional subdirectory path within the repo)
    Git(String, Option<String>),
}

fn classify_source(source: &str) -> Result<(String, SourceKind)> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty skill source"));
    }
    let (url_part, fragment) = match trimmed.split_once('#') {
        Some((u, f)) if !f.is_empty() => (u, Some(f)),
        _ => (trimmed, None),
    };
    let is_git = url_part.starts_with("git@")
        || url_part.starts_with("https://")
        || url_part.starts_with("http://")
        || url_part.starts_with("ssh://")
        || url_part.ends_with(".git");
    if is_git {
        let name = if let Some(sub) = fragment {
            Path::new(sub)
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .ok_or_else(|| anyhow!("could not derive skill name from subdir: {sub}"))?
        } else {
            git_repo_name(url_part)
                .ok_or_else(|| anyhow!("could not derive skill name from URL: {url_part}"))?
        };
        Ok((
            name,
            SourceKind::Git(url_part.to_string(), fragment.map(String::from)),
        ))
    } else {
        let path = PathBuf::from(trimmed);
        let abs = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        };
        let name = abs
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("could not derive skill name from path: {}", abs.display()))?;
        Ok((name, SourceKind::Path(abs)))
    }
}

fn git_repo_name(url: &str) -> Option<String> {
    // Trim trailing slashes + `.git` suffix, then take the last
    // path component. Handles `https://github.com/foo/bar`,
    // `git@github.com:foo/bar.git`, `ssh://git@host/foo/bar`.
    let stripped = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let after_colon = stripped.rsplit(':').next().unwrap_or(stripped);
    let last = after_colon.rsplit('/').next()?;
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

fn compute_version(dest: &Path, source: &str) -> String {
    if source.starts_with("http") || source.starts_with("git@") || source.starts_with("ssh://") {
        // Git source — read HEAD SHA from the cloned tree.
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dest)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    } else {
        // Path source — use destination dir mtime as an opaque pin.
        std::fs::metadata(dest)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs().to_string())
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-skills-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn seed_skill_source(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("# {name}\n\nminimal skill for testing.\n"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn add_from_local_path_copies_directory_and_records_lock() {
        let home = tmp_home("add-path");
        let stage = home.join("stage");
        let src = seed_skill_source(&stage, "greeter");
        let added = add(&home, src.to_str().unwrap()).unwrap();
        assert_eq!(added.name, "greeter");
        assert_eq!(added.source, src.to_str().unwrap());
        let dest = skills_root(&home).join("greeter");
        assert!(dest.exists());
        assert!(dest.join("SKILL.md").exists());
        let lock = SkillsLock::read(&home).unwrap();
        assert!(lock.skills.contains_key("greeter"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn add_overwrites_existing_skill() {
        let home = tmp_home("add-overwrite");
        let stage_a = home.join("stage-a");
        seed_skill_source(&stage_a, "greeter");
        let stage_b = home.join("stage-b");
        let src_b = stage_b.join("greeter");
        std::fs::create_dir_all(&src_b).unwrap();
        std::fs::write(src_b.join("SKILL.md"), "# greeter v2\n").unwrap();

        add(&home, stage_a.join("greeter").to_str().unwrap()).unwrap();
        add(&home, src_b.to_str().unwrap()).unwrap();
        let dest = skills_root(&home).join("greeter").join("SKILL.md");
        let body = std::fs::read_to_string(&dest).unwrap();
        assert!(
            body.contains("v2"),
            "second add must overwrite first: {body}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn list_returns_alphabetical_skills_with_lock_metadata() {
        let home = tmp_home("list");
        let stage = home.join("stage");
        seed_skill_source(&stage, "alpha");
        seed_skill_source(&stage, "beta");
        seed_skill_source(&stage, "gamma");
        add(&home, stage.join("alpha").to_str().unwrap()).unwrap();
        add(&home, stage.join("gamma").to_str().unwrap()).unwrap();
        // Manually drop in 'beta' to simulate operator hand-edit
        // (no lock entry → empty source/version).
        std::fs::create_dir_all(skills_root(&home).join("beta")).unwrap();

        let listed = list(&home).unwrap();
        let names: Vec<_> = listed.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        // beta has no lock entry → empty source.
        assert!(listed[1].source.is_empty());
        assert!(!listed[0].source.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn remove_clears_dir_and_lock_entry_idempotent() {
        let home = tmp_home("remove");
        let stage = home.join("stage");
        seed_skill_source(&stage, "doomed");
        add(&home, stage.join("doomed").to_str().unwrap()).unwrap();
        let dest = skills_root(&home).join("doomed");
        assert!(dest.exists());

        remove(&home, "doomed").unwrap();
        assert!(!dest.exists());
        let lock = SkillsLock::read(&home).unwrap();
        assert!(!lock.skills.contains_key("doomed"));

        // Idempotent — second call is a no-op.
        remove(&home, "doomed").unwrap();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_replays_lock_recorded_source() {
        let home = tmp_home("update");
        let stage = home.join("stage");
        let src = seed_skill_source(&stage, "evolving");
        add(&home, src.to_str().unwrap()).unwrap();
        // Mutate the source post-add.
        std::fs::write(src.join("SKILL.md"), "# evolving v2\n").unwrap();

        update(&home, "evolving").unwrap();
        let body =
            std::fs::read_to_string(skills_root(&home).join("evolving").join("SKILL.md")).unwrap();
        assert!(body.contains("v2"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_returns_error_for_unknown_skill() {
        let home = tmp_home("update-unknown");
        let r = update(&home, "ghost");
        assert!(r.is_err(), "update of unknown skill must error");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_for_agent_creates_per_backend_paths() {
        let home = tmp_home("install");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();

        let outcomes = install_for_agent(&home, &working, None).unwrap();
        assert_eq!(outcomes.len(), BACKEND_SKILL_DIRS.len());
        for outcome in &outcomes {
            assert!(
                matches!(outcome.mode, InstallMode::Symlink | InstallMode::Copy),
                "expected real install for backend {} got {:?}",
                outcome.backend,
                outcome
            );
            assert!(
                outcome.target.exists(),
                "target must exist post-install: {:?}",
                outcome.target
            );
            // The unified source's anchor skill should be reachable
            // through the per-backend path.
            assert!(
                outcome.target.join("anchor").join("SKILL.md").exists(),
                "skill not visible at {:?}",
                outcome.target
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_skips_pre_existing_non_managed_directory() {
        // Operator hand-crafted .claude/skills must not be clobbered.
        let home = tmp_home("install-skip");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();
        let claude = working.join(".claude/skills");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("operator-skill.md"), "hand-crafted\n").unwrap();

        let outcomes = install_for_agent(&home, &working, None).unwrap();
        let claude_outcome = outcomes.iter().find(|o| o.backend == "claude").unwrap();
        assert_eq!(
            claude_outcome.mode,
            InstallMode::Skipped,
            "non-managed dir must be skipped: {claude_outcome:?}"
        );
        assert!(
            claude.join("operator-skill.md").exists(),
            "operator's file must be preserved"
        );
        // Other backends still install normally.
        let codex_outcome = outcomes.iter().find(|o| o.backend == "codex").unwrap();
        assert!(matches!(
            codex_outcome.mode,
            InstallMode::Symlink | InstallMode::Copy
        ));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_replaces_pre_existing_managed_symlink_or_copy() {
        let home = tmp_home("install-replace");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();

        // First install.
        install_for_agent(&home, &working, None).unwrap();
        // Second install must succeed (idempotent).
        let outcomes = install_for_agent(&home, &working, None).unwrap();
        for outcome in &outcomes {
            assert!(matches!(
                outcome.mode,
                InstallMode::Symlink | InstallMode::Copy
            ));
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn classify_source_distinguishes_git_and_path() {
        let (name, kind) = classify_source("https://github.com/foo/bar.git").unwrap();
        assert_eq!(name, "bar");
        assert!(matches!(kind, SourceKind::Git(..)));
        let (name, kind) = classify_source("git@github.com:foo/bar").unwrap();
        assert_eq!(name, "bar");
        assert!(matches!(kind, SourceKind::Git(..)));
        let (name, kind) = classify_source("/tmp/local-skill").unwrap();
        assert_eq!(name, "local-skill");
        assert!(matches!(kind, SourceKind::Path(_)));
    }

    #[test]
    fn classify_source_git_subdir_fragment() {
        let (name, kind) =
            classify_source("https://github.com/foo/bar#skills/setup-telegram").unwrap();
        assert_eq!(name, "setup-telegram");
        assert!(matches!(kind, SourceKind::Git(_, Some(_))));
        if let SourceKind::Git(url, Some(sub)) = kind {
            assert_eq!(url, "https://github.com/foo/bar");
            assert_eq!(sub, "skills/setup-telegram");
        }
    }

    #[test]
    fn classify_source_git_no_fragment() {
        let (name, kind) = classify_source("https://github.com/foo/bar").unwrap();
        assert_eq!(name, "bar");
        assert!(matches!(kind, SourceKind::Git(_, None)));
    }

    #[test]
    fn classify_source_rejects_empty() {
        assert!(classify_source("").is_err());
        assert!(classify_source("   ").is_err());
    }

    #[test]
    fn skills_lock_round_trips_through_disk() {
        let home = tmp_home("lock-roundtrip");
        let mut lock = SkillsLock::default();
        lock.skills.insert(
            "skill-a".to_string(),
            SkillLockEntry {
                source: "/tmp/skill-a".to_string(),
                version: "abc123".to_string(),
                installed_at: "2026-05-09T00:00:00Z".to_string(),
            },
        );
        lock.write(&home).unwrap();
        let loaded = SkillsLock::read(&home).unwrap();
        assert_eq!(loaded, lock);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn list_empty_when_no_skills_root() {
        let home = tmp_home("list-empty");
        let listed = list(&home).unwrap();
        assert!(listed.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 61 W1 PR-1 (#P0-1) — auto-install pre-spawn smoke ──────────
    //
    // The daemon's spawn_and_register_agent calls install_for_agent
    // synchronously BEFORE agent::spawn_agent so SKILL.md files are in
    // place at the agent's first read. These tests verify the contract
    // properties that wiring depends on:
    // - install_for_agent succeeds with an empty (no skills added) source,
    //   so a fresh daemon launch with no `agend skills add` history
    //   doesn't fail agent boot.
    // - The wiring's best-effort Err handling is testable here because
    //   install_for_agent's only Err return is `ensure_skills_root`
    //   filesystem failure; on success it always returns `Ok(Vec)`.

    #[test]
    fn install_for_agent_succeeds_with_empty_source_root() {
        // Empty source: install_for_agent ensures the source root exists
        // (via ensure_skills_root) then iterates 4 backends; each install
        // creates an empty symlink/copy target with the marker. The
        // outcomes vec has one entry per backend, all in Symlink/Copy
        // mode (no Skipped from missing source). This is the daemon's
        // first-launch case: no skills configured → still safe to wire
        // pre-spawn.
        let home = tmp_home("install-empty-source");
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();
        let outcomes = install_for_agent(&home, &working, None).unwrap();
        assert_eq!(outcomes.len(), BACKEND_SKILL_DIRS.len());
        for outcome in &outcomes {
            assert!(
                matches!(outcome.mode, InstallMode::Symlink | InstallMode::Copy),
                "empty-source install must succeed for backend {}: {:?}",
                outcome.backend,
                outcome
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_for_agent_creates_working_dir_parents_when_needed() {
        // Daemon's spawn_and_register_agent may pass a working_dir whose
        // .claude/ etc. parents don't exist yet (fresh agent worktree).
        // install_for_agent's per-backend create_dir_all(parent) handles
        // this — verify by passing a working_dir with no .claude/ pre-
        // existing and checking the backend dirs land cleanly.
        let home = tmp_home("install-create-parents");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("fresh-wd");
        std::fs::create_dir_all(&working).unwrap();
        // Sanity: no backend parents yet.
        assert!(!working.join(".claude").exists());
        let outcomes = install_for_agent(&home, &working, None).unwrap();
        // Every backend's parent dir must now exist + the install
        // landed at the conventional path.
        for (backend, rel) in BACKEND_SKILL_DIRS {
            let target = working.join(rel);
            let outcome = outcomes
                .iter()
                .find(|o| o.backend == *backend)
                .expect("outcome present for backend");
            assert!(
                matches!(outcome.mode, InstallMode::Symlink | InstallMode::Copy),
                "fresh-parent install must succeed for {backend}"
            );
            assert!(target.exists(), "target landed for {backend}");
            assert!(
                target.join("anchor").join("SKILL.md").exists(),
                "skill visible at {target:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 61 W1 PR-2 (#P0-2) — fleet.yaml per-instance override ─────

    #[test]
    fn install_for_agent_with_filter_includes_only_allowlisted_skills() {
        // Stage 3 skills in unified source; install only `alpha` + `gamma`
        // for the agent. `beta` must NOT appear at any backend path.
        let home = tmp_home("filter-allowlist");
        let stage = home.join("stage");
        seed_skill_source(&stage, "alpha");
        seed_skill_source(&stage, "beta");
        seed_skill_source(&stage, "gamma");
        add(&home, stage.join("alpha").to_str().unwrap()).unwrap();
        add(&home, stage.join("beta").to_str().unwrap()).unwrap();
        add(&home, stage.join("gamma").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();
        let allowlist = vec!["alpha".to_string(), "gamma".to_string()];

        let outcomes = install_for_agent(&home, &working, Some(&allowlist)).unwrap();
        for outcome in &outcomes {
            assert!(matches!(
                outcome.mode,
                InstallMode::Symlink | InstallMode::Copy
            ));
            assert!(outcome.target.join("alpha").join("SKILL.md").exists());
            assert!(outcome.target.join("gamma").join("SKILL.md").exists());
            assert!(
                !outcome.target.join("beta").exists(),
                "beta must NOT appear under {} (filter excluded)",
                outcome.target.display()
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_for_agent_with_empty_filter_opts_out_of_all_skills() {
        // Some(empty) is meaningful — agent gets per-backend dirs but
        // none of the skills from the unified source.
        let home = tmp_home("filter-empty");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();
        let empty: Vec<String> = Vec::new();

        let outcomes = install_for_agent(&home, &working, Some(&empty)).unwrap();
        for outcome in &outcomes {
            assert!(matches!(
                outcome.mode,
                InstallMode::Symlink | InstallMode::Copy
            ));
            assert!(
                !outcome.target.join("anchor").exists(),
                "anchor must NOT appear under {} (empty filter)",
                outcome.target.display()
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_for_agent_with_filter_skips_unknown_skill_names() {
        // Allowlist references a skill not in the unified source — the
        // helper logs warn + skips silently rather than failing the
        // whole install. Other allowlisted skills land normally.
        let home = tmp_home("filter-unknown");
        let stage = home.join("stage");
        seed_skill_source(&stage, "real");
        add(&home, stage.join("real").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();
        let allowlist = vec!["real".to_string(), "ghost".to_string()];

        let outcomes = install_for_agent(&home, &working, Some(&allowlist)).unwrap();
        for outcome in &outcomes {
            assert!(matches!(
                outcome.mode,
                InstallMode::Symlink | InstallMode::Copy
            ));
            assert!(outcome.target.join("real").join("SKILL.md").exists());
            assert!(!outcome.target.join("ghost").exists());
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 62 W1 PR-1 (#P0-1) — SHA-256 digest determinism ─────────

    #[test]
    fn stage_digest_is_deterministic_across_calls() {
        // Same input → same output — load-bearing for stage-dir
        // reuse semantics. SHA-256 is byte-deterministic by spec;
        // this test pins the contract against accidental refactor.
        let a = stage_digest(b"alpha\nbeta\n");
        let b = stage_digest(b"alpha\nbeta\n");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16, "16 hex chars = 8 bytes prefix");
        // Different input → different output.
        let c = stage_digest(b"alpha\ngamma\n");
        assert_ne!(a, c);
    }

    #[test]
    fn stage_digest_pinned_value_for_known_input() {
        // SHA-256 of "alpha\nbeta\n" is byte-deterministic across
        // platforms. Pin the first-8-bytes-as-hex prefix so a future
        // refactor that swapped digest length / encoding would
        // surface here, not as a stage-dir cache miss in production.
        // Reference: `printf 'alpha\nbeta\n' | shasum -a 256` first
        // 16 hex chars.
        let digest = stage_digest(b"alpha\nbeta\n");
        assert_eq!(digest, "e49c81e2d2f84e25");
    }

    #[test]
    fn stage_filtered_source_uses_sha256_digest_naming() {
        // End-to-end: stage_filtered_source's directory name should
        // be the SHA-256 prefix of the sorted-allowlist-newline-joined
        // bytes. Verifies the wire-up between the helper and the
        // digest function.
        let home = tmp_home("digest-stage-naming");
        let stage = home.join("stage");
        seed_skill_source(&stage, "alpha");
        seed_skill_source(&stage, "beta");
        add(&home, stage.join("alpha").to_str().unwrap()).unwrap();
        add(&home, stage.join("beta").to_str().unwrap()).unwrap();
        let allowlist = vec!["alpha".to_string(), "beta".to_string()];
        // Pre-compute expected dir name.
        let expected = stage_digest(b"alpha\nbeta\n");
        let stage_dir = stage_filtered_source(&home, &skills_root(&home), &allowlist).unwrap();
        assert!(
            stage_dir.ends_with(&expected),
            "stage dir {:?} must end with SHA-256 prefix {expected}",
            stage_dir
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 62 W1 PR-2 (#P0-2) — skills-stage GC ────────────────────

    fn seed_aged_stage(home: &Path, name: &str, age_secs: u64) {
        let path = home.join(".skills-stage").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("marker"), b"stage").unwrap();
        let now = std::time::SystemTime::now();
        let back = now - std::time::Duration::from_secs(age_secs);
        let secs = back
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let dt = chrono::DateTime::<chrono::Utc>::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        );
        // touch -t format: [[CC]YY]MMDDhhmm[.ss]
        let arg = dt.format("%Y%m%d%H%M.%S").to_string();
        let _ = std::process::Command::new("touch")
            .args(["-t", &arg])
            .arg(&path)
            .status();
    }

    #[test]
    fn cleanup_stale_stages_deletes_aged_dirs_above_threshold() {
        let home = tmp_home("gc-aged");
        std::fs::create_dir_all(home.join(".skills-stage").join("fresh")).unwrap();
        std::fs::write(
            home.join(".skills-stage").join("fresh").join("marker"),
            b"x",
        )
        .unwrap();
        seed_aged_stage(&home, "ancient", 8 * 24 * 60 * 60);

        let report = cleanup_stale_stages(&home, 7 * 24 * 60 * 60, &[]).unwrap();
        assert_eq!(report.candidates, 2);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.preserved_recent, 1);
        assert_eq!(report.preserved_excluded, 0);
        assert!(home.join(".skills-stage").join("fresh").exists());
        assert!(!home.join(".skills-stage").join("ancient").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_stale_stages_excludes_named_digests_from_deletion() {
        // Reviewer minor add: same-run exclusion prevents TOCTOU
        // deletion of just-staged. Excluded dirs preserved even if
        // older than retention threshold.
        let home = tmp_home("gc-exclude");
        seed_aged_stage(&home, "active-stage", 8 * 24 * 60 * 60);
        seed_aged_stage(&home, "stale-stage", 8 * 24 * 60 * 60);

        let exclude = vec!["active-stage".to_string()];
        let report = cleanup_stale_stages(&home, 7 * 24 * 60 * 60, &exclude).unwrap();
        assert_eq!(report.candidates, 2);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.preserved_recent, 0);
        assert_eq!(report.preserved_excluded, 1);
        assert!(home.join(".skills-stage").join("active-stage").exists());
        assert!(!home.join(".skills-stage").join("stale-stage").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_stale_stages_returns_empty_report_when_root_missing() {
        // Daemon-init invocation on fresh AgEnD home (no .skills-
        // stage dir created yet). Must return empty report, not error.
        let home = tmp_home("gc-no-root");
        let report = cleanup_stale_stages(&home, 7 * 24 * 60 * 60, &[]).unwrap();
        assert_eq!(report, StageGcReport::default());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1080 regression: pre-existing backend config dirs must not
    //    block skills symlink creation ────────────────────────────

    #[test]
    fn install_succeeds_when_backend_config_dir_preexists_without_skills() {
        let home = tmp_home("1080-preexist");
        let stage = home.join("stage");
        seed_skill_source(&stage, "anchor");
        add(&home, stage.join("anchor").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();

        // Simulate a codex backend that created .codex/ with config
        // but no skills/ subdir (the #1080 scenario).
        let codex_dir = working.join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(codex_dir.join("config.toml"), "# codex config").unwrap();

        // Simulate kiro backend with .kiro/settings/ but no skills/.
        let kiro_dir = working.join(".kiro");
        std::fs::create_dir_all(kiro_dir.join("settings")).unwrap();

        let outcomes = install_for_agent(&home, &working, None).unwrap();
        for outcome in &outcomes {
            assert!(
                matches!(outcome.mode, InstallMode::Symlink | InstallMode::Copy),
                "#1080: backend {} must install even when parent config dir preexists: {:?}",
                outcome.backend,
                outcome
            );
        }
        // Codex config file must be preserved alongside the new skills symlink.
        assert!(
            codex_dir.join("config.toml").exists(),
            "codex config.toml must not be clobbered"
        );
        assert!(
            working.join(".codex/skills").exists(),
            ".codex/skills must exist"
        );
        assert!(
            working.join(".kiro/skills").exists(),
            ".kiro/skills must exist"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn install_with_filter_only_installs_allowlisted_skills() {
        let home = tmp_home("1080-filter");
        let stage = home.join("stage");
        seed_skill_source(&stage, "allowed-skill");
        seed_skill_source(&stage, "blocked-skill");
        add(&home, stage.join("allowed-skill").to_str().unwrap()).unwrap();
        add(&home, stage.join("blocked-skill").to_str().unwrap()).unwrap();
        let working = home.join("agent-wd");
        std::fs::create_dir_all(&working).unwrap();

        let filter = vec!["allowed-skill".to_string()];
        let outcomes = install_for_agent(&home, &working, Some(&filter)).unwrap();
        for outcome in &outcomes {
            assert!(
                matches!(outcome.mode, InstallMode::Symlink | InstallMode::Copy),
                "backend {} must install: {:?}",
                outcome.backend,
                outcome
            );
            let skill_dir = outcome.target.join("allowed-skill");
            assert!(
                skill_dir.join("SKILL.md").exists(),
                "allowed-skill must be visible at {:?}",
                skill_dir
            );
            let blocked = outcome.target.join("blocked-skill");
            assert!(
                !blocked.exists(),
                "blocked-skill must NOT be visible at {:?}",
                blocked
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
