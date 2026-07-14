use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindingRemoval {
    Removed,
    Absent,
    Failed(String),
}

fn clear_binding_index(home: &Path, agent: &str) {
    if let Ok(mut map) = super::binding_index().write() {
        map.remove(&super::index_key(home, agent));
        if let Ok(canonical) = std::fs::canonicalize(home) {
            map.remove(&super::index_key(&canonical, agent));
        }
    }
}

pub(crate) fn unbind_with_permit(
    home: &Path,
    agent: &str,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> BindingRemoval {
    if !permit.authorizes(home, agent) {
        tracing::warn!(agent, "unbind refused: invalid lifecycle permit");
        return BindingRemoval::Failed("invalid lifecycle permit".to_string());
    }
    let dir = crate::paths::runtime_dir(home).join(agent);
    let binding_path = dir.join("binding.json");
    // #2158 PR2 (ii): audit the release/clear side of binding-change detection with
    // caller process context (read the prior branch before removal). Only logs when
    // a binding actually existed (a no-op unbind stays silent).
    let prev_branch = std::fs::read_to_string(&binding_path)
        .ok()
        .and_then(|c| super::parse_binding_guarded(&c))
        .and_then(|v| v.get("branch").and_then(|b| b.as_str()).map(String::from));
    if let Err(error) = std::fs::remove_file(&binding_path) {
        if error.kind() == std::io::ErrorKind::NotFound {
            clear_binding_index(home, agent);
            return BindingRemoval::Absent;
        }
        return BindingRemoval::Failed(format!(
            "remove binding {}: {error}",
            binding_path.display()
        ));
    }
    clear_binding_index(home, agent);
    for path in [
        super::binding_sig_path(&dir),
        dir.join(super::OUT_OF_DISPATCH_SIDECAR),
    ] {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return BindingRemoval::Failed(format!("remove {}: {error}", path.display()));
            }
        }
    }
    if let Some(prev_branch) = prev_branch {
        crate::event_log::log(
            home,
            "binding_released",
            agent,
            &format!(
                "prev_branch={prev_branch}; {}",
                crate::event_log::caller_process_context()
            ),
        );
    }
    BindingRemoval::Removed
}
