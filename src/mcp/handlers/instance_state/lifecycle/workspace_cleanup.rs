use crate::agent_ops::{cleanup_admission, cleanup_working_dir_admitted};
use std::path::Path;

/// Keep shared workspace cleanup side effects serialized with both fleet
/// admission and backend provisioning. Lock order is fleet → workspace
/// identity, matching deployment cleanup. The post-delete snapshot prevents a
/// newly admitted overlapping workspace from losing its shared Claude config;
/// the identity lock remains held through config mutation and path cleanup.
pub(super) fn cleanup_deleted_workspace(
    home: &Path,
    name: &str,
    working_dir: &Path,
    admission: &cleanup_admission::CleanupAdmission,
    is_claude: bool,
) -> Option<String> {
    if !matches!(
        admission,
        cleanup_admission::CleanupAdmission::RemoveOwned { .. }
            | cleanup_admission::CleanupAdmission::ScrubExclusive { .. }
    ) {
        return cleanup_working_dir_admitted(home, name, working_dir, admission);
    }

    let _fleet_lock = match crate::fleet::persist::acquire_fleet_lock(home) {
        Ok(lock) => lock,
        Err(error) => return Some(format!("could not acquire fleet lock for cleanup: {error}")),
    };
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let fleet = match crate::fleet::FleetConfig::load_snapshot_under_lock(&fleet_path) {
        Ok(config) => config,
        Err(_)
            if std::fs::symlink_metadata(&fleet_path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            crate::fleet::FleetConfig::default()
        }
        Err(error) => {
            return Some(format!(
                "could not load fresh fleet snapshot for workspace cleanup: {error}"
            ));
        }
    };
    for (survivor_name, entry) in &fleet.instances {
        let survivor = entry
            .working_directory
            .as_deref()
            .map(crate::fleet::resolve::expand_tilde_path)
            .unwrap_or_else(|| crate::paths::workspace_dir(home).join(survivor_name));
        match crate::paths::workspace_paths_overlap(working_dir, &survivor) {
            Ok(false) => {}
            Ok(true) => {
                tracing::warn!(
                    name,
                    survivor_name,
                    path = %working_dir.display(),
                    "workspace cleanup preserved: fresh fleet snapshot contains an overlapping survivor"
                );
                return None;
            }
            Err(error) => {
                return Some(format!(
                    "fresh survivor workspace '{survivor_name}' is ambiguous: {error}"
                ));
            }
        }
    }

    crate::agent_ops::cleanup_working_dir_admitted_with_pre_cleanup(
        home,
        name,
        working_dir,
        admission,
        || {
            if is_claude {
                crate::mcp_config::remove_claude_channel_config(working_dir)
                    .map_err(|error| format!("Claude ChannelBridge config cleanup: {error}"))?;
            }
            Ok(())
        },
    )
}
