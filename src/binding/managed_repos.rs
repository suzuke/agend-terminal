use std::path::{Path, PathBuf};

pub fn bound_source_repos(home: &Path) -> Vec<PathBuf> {
    let mut repos: Vec<PathBuf> = Vec::new();
    for (_, v) in super::binding_scan_all(home) {
        if let Some(src) = v["source_repo"].as_str() {
            let path = PathBuf::from(src);
            if !repos.contains(&path) {
                repos.push(path);
            }
        }
    }
    repos
}

pub fn all_managed_repos(home: &Path) -> Vec<PathBuf> {
    let mut repos = bound_source_repos(home);
    for path in read_managed_repo_registry(home) {
        if !repos.contains(&path) {
            repos.push(path);
        }
    }
    for repo_str in crate::cleanup_intents::intent_repos(home) {
        let path = PathBuf::from(repo_str);
        if !repos.contains(&path) {
            repos.push(path);
        }
    }
    repos
}

fn managed_repo_registry_path(home: &Path) -> PathBuf {
    home.join("managed-repos.jsonl")
}

pub(crate) fn register_managed_repo(home: &Path, source_repo: &str) {
    if source_repo.is_empty() {
        return;
    }
    let path = managed_repo_registry_path(home);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == source_repo) {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{source_repo}");
    }
}

pub(crate) fn read_managed_repo_registry(home: &Path) -> Vec<PathBuf> {
    let path = managed_repo_registry_path(home);
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| PathBuf::from(l.trim()))
        .collect()
}
