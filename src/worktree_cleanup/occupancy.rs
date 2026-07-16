use std::path::{Path, PathBuf};

/// Read every binding with the identity fields required by destructive branch
/// cleanup. A syntactically valid but incomplete binding is still ambiguous.
pub(crate) fn binding_scan_all_strict(home: &Path) -> Result<Vec<(String, serde_json::Value)>, ()> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let entries = match std::fs::read_dir(&runtime_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(()),
    };
    let mut bindings = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let agent_name = entry.file_name().to_string_lossy().into_owned();
        let binding_path = crate::paths::binding_path(home, &agent_name);
        let content = match std::fs::read_to_string(&binding_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(()),
        };
        let value: serde_json::Value = serde_json::from_str(&content).map_err(|_| ())?;
        if value["branch"]
            .as_str()
            .filter(|branch| !branch.is_empty())
            .is_none()
            || value["source_repo"]
                .as_str()
                .filter(|source| !source.is_empty())
                .is_none()
        {
            return Err(());
        }
        bindings.push((agent_name, value));
    }
    Ok(bindings)
}

/// Normalize a path: strip Windows `\\?\` UNC prefix.
fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

/// Canonicalize `p`, or — when it does not exist yet — resolve symlinks on its
/// longest EXISTING ancestor and re-append the missing tail. Returns `None`
/// only when not even an existing ancestor canonicalizes.
fn canonicalize_lenient(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name);
        }
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            out.extend(tail.iter().rev());
            return Some(out);
        }
        cur = parent;
    }
    None
}

/// Distinct `worktree` paths of every LIVE `binding.json`, preserving the
/// distinction between a genuine absence and unreadable/corrupt evidence.
pub(crate) fn bound_worktree_paths_or_ambiguous(home: &Path) -> Result<Vec<PathBuf>, ()> {
    let entries = match std::fs::read_dir(crate::paths::runtime_dir(home)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| ())?;
        let agent = entry.file_name().to_string_lossy().into_owned();
        let content = match std::fs::read_to_string(crate::paths::binding_path(home, &agent)) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(()),
        };
        let v: serde_json::Value = serde_json::from_str(&content).map_err(|_| ())?;
        if let Some(wt) = v["worktree"].as_str() {
            let path = PathBuf::from(wt);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

/// Check if a worktree path is in use by any active agent.
pub(crate) fn is_in_use(wt_path: &Path, active_dirs: &[PathBuf]) -> bool {
    let wt_norm = normalize_path(
        &wt_path
            .canonicalize()
            .unwrap_or_else(|_| wt_path.to_path_buf()),
    );
    active_dirs.iter().any(|wd| match canonicalize_lenient(wd) {
        Some(canon) => {
            let wd_norm = normalize_path(&canon);
            wd_norm.starts_with(&wt_norm) || wd.starts_with(wt_path)
        }
        None => true,
    })
}
