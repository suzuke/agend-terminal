//! #1547 (A): non-hidden workspace link so daemon-spawned agy loads the
//! project-local `.agents/` fleet MCP.
//!
//! agy (Antigravity CLI) refuses to add a workspace folder whose path has ANY
//! dot-prefixed (hidden) ancestor — `addWorkspaceFolder` logs
//! `is hidden: ignore uri` (`root.go:132`) and never reads the
//! workspace-scoped `.agents/mcp_config.json`. Every daemon agent workspace
//! lives under `$AGEND_HOME` (`~/.agend-terminal`, dot-prefixed) so it is
//! "hidden" to agy → no fleet `send`/`inbox`/`task` tools.
//!
//! **Mechanism (operator e2e-verified):** agy reads the workspace path from
//! the `$PWD` env var, NOT from `getcwd()`/realpath. So the daemon spawns agy
//! with its CWD at the real hidden workspace (the validated allowed root) but
//! its `$PWD` pointed at a NON-hidden link to that workspace. The hidden check
//! passes; project discovery still resolves the realpath (via
//! `.antigravitycli`), so it is the SAME antigravity project — no duplication —
//! and `.agents/` loads.
//!
//! The link lives at `<base>/<instance>` where `<base>` defaults to
//! `<user_home>/agend-ws` (configurable via fleet.yaml
//! `agy_workspace_link_base`). Unix: a symlink. Windows: a **directory
//! junction** (NOT `symlink_dir`, which needs Developer Mode / admin
//! privilege; a junction needs neither).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Default non-hidden base when fleet.yaml does not set
/// `agy_workspace_link_base`: `<user_home>/agend-ws`.
fn default_link_base(home: &Path) -> PathBuf {
    // Prefer the real user home (guaranteed non-hidden on a normal install).
    // Fall back to `$AGEND_HOME`'s parent only if home resolution fails.
    dirs::home_dir()
        .or_else(|| home.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agend-ws")
}

/// Resolve the non-hidden link base: fleet.yaml `agy_workspace_link_base` (if
/// set) else `<user_home>/agend-ws`. Best-effort fleet-config load; a missing
/// or unparsable fleet.yaml falls back to the default.
pub fn link_base(home: &Path) -> PathBuf {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|f| f.agy_workspace_link_base)
        .unwrap_or_else(|| default_link_base(home))
}

/// The link path for an instance: `<base>/<instance>`.
pub fn link_path(home: &Path, instance: &str) -> PathBuf {
    link_base(home).join(instance)
}

/// #1597: whether any component of `p` is dot-prefixed (hidden). This is the
/// same "ANY dot-prefixed ancestor makes the whole path hidden" rule agy
/// applies in `addWorkspaceFolder` (`root.go:132`). When this is `false`, agy
/// accepts the path as a workspace directly — so the non-hidden link is NOT
/// needed and we can point `$PWD` at the real dir (avoids shadowing an explicit
/// non-hidden `working_directory` + a stray link). When `true` (e.g. the
/// default `$AGEND_HOME/workspace/<name>` under `~/.agend-terminal`), the link
/// is required — that is the whole reason #1547/#1582 exists.
pub fn path_has_hidden_component(p: &Path) -> bool {
    use std::path::Component;
    p.components().any(|c| match c {
        // Only real path segments can be "hidden"; RootDir, the Windows
        // `Prefix` (`C:\`), `.` and `..` never count.
        Component::Normal(seg) => seg.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

/// Create (or refresh) the non-hidden link `<base>/<instance>` →
/// `real_workspace` and return the link path to set as agy's `$PWD`.
///
/// Idempotent: an existing managed link (symlink on Unix, junction on Windows)
/// is removed and recreated each spawn so a stale target cannot survive. If a
/// NON-link entry already occupies the path (a real dir/file the operator put
/// there), this errors rather than clobber it.
pub fn ensure_link(home: &Path, instance: &str, real_workspace: &Path) -> Result<PathBuf> {
    let link = link_path(home, instance);
    let base = link
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&base)
        .with_context(|| format!("create agy link base {}", base.display()))?;

    if let Ok(meta) = link.symlink_metadata() {
        if is_managed_link(&link, &meta) {
            remove_link_path(&link);
        } else {
            anyhow::bail!(
                "agy workspace link {} already exists and is not a daemon-managed \
                 link (refusing to clobber); choose a different agy_workspace_link_base \
                 or remove it manually",
                link.display()
            );
        }
    }

    create_dir_link(real_workspace, &link).with_context(|| {
        format!(
            "create agy workspace link {} -> {}",
            link.display(),
            real_workspace.display()
        )
    })?;
    Ok(link)
}

/// Best-effort teardown: remove an instance's managed link. Never removes a
/// real directory — only a symlink/junction. Safe to call for any backend
/// (a no-op when no link exists).
pub fn remove_link(home: &Path, instance: &str) {
    let link = link_path(home, instance);
    if let Ok(meta) = link.symlink_metadata() {
        if is_managed_link(&link, &meta) {
            remove_link_path(&link);
        }
    }
}

/// Whether `link` is a daemon-managed link we may remove/recreate. On Unix a
/// symlink; on Windows a junction (reparse point) — std reports junctions via
/// the `FILE_ATTRIBUTE_REPARSE_POINT` flag, which `is_symlink()` does not
/// cover, so the platform check below uses the reparse flag.
#[cfg(unix)]
fn is_managed_link(_link: &Path, meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

#[cfg(windows)]
fn is_managed_link(_link: &Path, meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Remove a managed link. Unix symlinks delete via `remove_file`; Windows
/// junctions are directory reparse points and delete via `remove_dir`. Try
/// both so the call is robust across platforms. Best-effort + WARN.
fn remove_link_path(link: &Path) {
    let res = std::fs::remove_file(link).or_else(|_| std::fs::remove_dir(link));
    if let Err(e) = res {
        tracing::warn!(
            path = %link.display(), error = %e,
            "agy_workspace: failed to remove managed workspace link"
        );
    }
}

#[cfg(unix)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    // Directory junction — no privilege required (unlike `symlink_dir`).
    junction::create(target, link)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("agy_ws_test_{}_{}", std::process::id(), next_id()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn next_id() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn default_base_is_non_hidden() {
        let home = PathBuf::from("/Users/x/.agend-terminal");
        let base = default_link_base(&home);
        // Last component is `agend-ws` and no path component is dot-prefixed
        // (that is the whole point — a hidden ancestor is what agy rejects).
        assert_eq!(base.file_name().unwrap(), "agend-ws");
        assert!(
            !base
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.')),
            "default link base must have no hidden component: {}",
            base.display()
        );
    }

    #[test]
    fn path_has_hidden_component_detects_dot_prefixed_segments() {
        // Hidden: leaf, ancestor, or the default agend workspace.
        assert!(path_has_hidden_component(Path::new(
            "/Users/x/.agend-terminal/workspace/agy"
        )));
        assert!(path_has_hidden_component(Path::new("/Users/x/.config/agy"))); // hidden ancestor
        assert!(path_has_hidden_component(Path::new(
            "/Users/x/proj/.hidden"
        ))); // hidden leaf
             // Not hidden: ordinary absolute path, no dot-prefixed segment.
        assert!(!path_has_hidden_component(Path::new(
            "/Users/x/projects/agy-ws"
        )));
        assert!(!path_has_hidden_component(Path::new(
            "/var/folders/ab/cd/T/x"
        )));
        // `..` / `.` path operators are not "hidden" segments.
        assert!(!path_has_hidden_component(Path::new("/Users/x/./proj")));
    }

    #[test]
    fn link_path_joins_instance() {
        let home = tmp_home();
        let p = link_path(&home, "agy-1");
        assert_eq!(p.file_name().unwrap(), "agy-1");
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_link_creates_and_resolves_to_real_workspace() {
        let home = tmp_home();
        let real = home.join("workspace").join("agy-1");
        std::fs::create_dir_all(&real).unwrap();
        // Marker file inside the real workspace, reachable through the link.
        std::fs::write(real.join("marker"), b"hi").unwrap();

        // Point the base inside our tmp home (no fleet.yaml → default base is
        // the user home; override via a fleet.yaml under `home`).
        write_fleet_link_base(&home, &home.join("ws-links"));

        let link = ensure_link(&home, "agy-1", &real).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        // The link resolves to the real workspace (marker reachable).
        assert_eq!(std::fs::read(link.join("marker")).unwrap(), b"hi");
        // Idempotent re-create.
        let link2 = ensure_link(&home, "agy-1", &real).unwrap();
        assert_eq!(link, link2);

        // Teardown removes the link but NOT the real workspace.
        remove_link(&home, "agy-1");
        assert!(link.symlink_metadata().is_err());
        assert!(real.join("marker").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_link_refuses_to_clobber_real_dir() {
        let home = tmp_home();
        let real = home.join("workspace").join("agy-2");
        std::fs::create_dir_all(&real).unwrap();
        write_fleet_link_base(&home, &home.join("ws-links"));

        // Pre-create a REAL dir where the link would go.
        let link = link_path(&home, "agy-2");
        std::fs::create_dir_all(&link).unwrap();
        let err = ensure_link(&home, "agy-2", &real).unwrap_err();
        assert!(
            err.to_string().contains("not a daemon-managed link"),
            "should refuse to clobber a real dir: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Write a minimal fleet.yaml under `home` that sets the link base, so the
    /// test exercises the configurable-base path deterministically (and does
    /// not write into the real user home).
    #[cfg(unix)]
    fn write_fleet_link_base(home: &Path, base: &Path) {
        let yaml = format!(
            "instances: {{}}\nteams: {{}}\nagy_workspace_link_base: {}\n",
            base.display()
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }
}
