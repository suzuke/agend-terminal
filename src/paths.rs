//! Centralized path helpers — single source of truth for daemon directory layout.

use std::path::{Path, PathBuf};

/// `<home>/workspace/` — per-agent working directories.
pub fn workspace_dir(home: &Path) -> PathBuf {
    home.join("workspace")
}

/// `<home>/runtime/` — per-agent runtime state (binding.json, metadata).
pub fn runtime_dir(home: &Path) -> PathBuf {
    home.join("runtime")
}

/// `<home>/runtime/<agent>/binding.json`
pub fn binding_path(home: &Path, agent: &str) -> PathBuf {
    runtime_dir(home).join(agent).join(BINDING_FILENAME)
}

/// Binding state filename.
pub const BINDING_FILENAME: &str = "binding.json";

/// The working directory an instance actually uses: its explicit `working_directory`
/// when set (non-empty), else the daemon default `<home>/workspace/<name>`. Callers that
/// only have the fleet entry (which may store `working_directory: None`) use this so the
/// identity comparison sees the SAME path the daemon will actually spawn into.
pub fn effective_working_dir(home: &Path, name: &str, explicit: Option<&str>) -> PathBuf {
    match explicit {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => workspace_dir(home).join(name),
    }
}

/// Canonical IDENTITY of a workspace path, for collision detection across instances
/// (workspace-identity guard). Two paths that resolve to the SAME on-disk location — via
/// symlinked ancestors, case-only aliases, or a `.` remainder — share one identity, so a
/// duplicate is refused even when the target does not exist yet.
///
/// Because a fresh workspace dir may not exist at admission time, we canonicalize the
/// DEEPEST EXISTING ANCESTOR (which resolves symlinks) and re-append the lexically-
/// normalized remainder, then case-fold (conservative — lowercase) so case-only aliases
/// collide on ALL platforms. This is fail-closed: lowercasing may over-collide two
/// genuinely distinct case-sensitive paths, which only ever REFUSES a borderline duplicate
/// — never silently admits one. `..` is rejected upstream at admission, so the remainder
/// here is `.`-normalized only.
pub fn workspace_identity(path: &Path) -> String {
    // Peel non-existing tail components until an existing ancestor (or the root).
    let mut cur = path.to_path_buf();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        match (cur.file_name(), cur.parent()) {
            (Some(f), Some(p)) => {
                remainder.push(f.to_os_string());
                cur = p.to_path_buf();
            }
            // no file_name (root / prefix) or no parent — stop peeling.
            _ => break,
        }
    }
    // Canonicalize the existing base (symlink + `.` resolution); lexical fallback on error.
    let mut full = cur.canonicalize().unwrap_or(cur);
    // Re-append the remainder shallowest→deepest, dropping `.` segments.
    for comp in remainder.iter().rev() {
        if comp.as_os_str() != "." {
            full.push(comp);
        }
    }
    full.to_string_lossy().to_lowercase()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-wsid-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn identity_same_path_matches_and_distinct_paths_differ() {
        let base = tmp("same");
        let a = base.join("alpha");
        let b = base.join("beta");
        assert_eq!(workspace_identity(&a), workspace_identity(&a));
        assert_ne!(workspace_identity(&a), workspace_identity(&b));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn identity_case_only_alias_collides() {
        // A nonexistent target and its case-only alias share one identity (conservative
        // case-fold), so a case-only duplicate workspace is refused on all platforms.
        let base = tmp("case");
        let lower = base.join("workspace").join("dev");
        let upper = base.join("workspace").join("DEV");
        assert_eq!(
            workspace_identity(&lower),
            workspace_identity(&upper),
            "case-only aliases must share one identity"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn identity_nonexistent_tail_uses_deepest_existing_ancestor() {
        let base = tmp("tail"); // exists
        let deep = base.join("a").join("b").join("c"); // a/b/c do not exist
        let id = workspace_identity(&deep);
        let expected = format!(
            "{}/a/b/c",
            base.canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_lowercase()
        );
        assert_eq!(id, expected);
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn identity_symlinked_ancestor_collides_with_real() {
        use std::os::unix::fs::symlink;
        let base = tmp("symlink");
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        symlink(&real, &link).unwrap();
        // <base>/link/ws and <base>/real/ws (ws nonexistent) must share one identity.
        assert_eq!(
            workspace_identity(&link.join("ws")),
            workspace_identity(&real.join("ws")),
            "a symlinked ancestor must resolve to the same identity as the real dir"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn effective_working_dir_defaults_when_absent_or_empty() {
        let home = Path::new("/tmp/h");
        assert_eq!(
            effective_working_dir(home, "dev", None),
            workspace_dir(home).join("dev")
        );
        assert_eq!(
            effective_working_dir(home, "dev", Some("")),
            workspace_dir(home).join("dev")
        );
        assert_eq!(
            effective_working_dir(home, "dev", Some("/custom/wd")),
            PathBuf::from("/custom/wd")
        );
    }
}
