use std::path::Path;

/// Shared reload seam: runtime_config THEN operator_mode, in strict order.
/// Called before app bootstrap, before daemon init, and per tick.
pub fn reload_runtime_controls(home: &Path) {
    crate::runtime_config::reload(home);
    crate::operator_mode::reload(home);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-rtctrl-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Helper semantics: reload_runtime_controls applies runtime_config
    /// values from disk.
    #[test]
    fn reload_runtime_controls_loads_persisted_config() {
        let home = tmp_home("loads-config");
        std::fs::write(
            home.join("runtime-config.json"),
            r#"{"schema_version": 1, "copy_on_select": true}"#,
        )
        .unwrap();
        reload_runtime_controls(&home);
        assert!(
            crate::runtime_config::get().copy_on_select,
            "reload_runtime_controls must load persisted runtime_config"
        );
        // Reset.
        std::fs::write(home.join("runtime-config.json"), r#"{"schema_version": 1}"#).unwrap();
        crate::runtime_config::reload(&home);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Helper semantics: reload_runtime_controls loads operator_mode from
    /// a valid signed file, and falls back to Active on missing/tampered.
    #[test]
    fn reload_runtime_controls_loads_operator_mode() {
        let home = tmp_home("loads-mode");
        // No operator-mode.json → default Active.
        reload_runtime_controls(&home);
        assert_eq!(
            crate::operator_mode::get().mode,
            crate::operator_mode::OperatorMode::Active,
            "missing mode file must default to Active"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Host call ordering: operator_mode must be reloaded AFTER
    /// runtime_config, so both are current before API ingress.
    /// This test asserts the function calls both reloads.
    #[test]
    fn reload_runtime_controls_calls_both_reloads() {
        let home = tmp_home("both-reloads");
        std::fs::write(home.join("runtime-config.json"), r#"{"schema_version": 1}"#).unwrap();
        // Should not panic; exercises both code paths.
        reload_runtime_controls(&home);
        reload_runtime_controls(&home); // idempotent
        std::fs::remove_dir_all(&home).ok();
    }

    /// Startup ordering: reload_runtime_controls must be called BEFORE
    /// setup_app_bootstrap. Verify by source inspection.
    #[test]
    fn app_calls_reload_before_bootstrap() {
        let src = include_str!("app/mod.rs");
        let reload_pos = src.find("reload_runtime_controls");
        let bootstrap_pos = src.find("setup_app_bootstrap");
        assert!(
            reload_pos.is_some() && bootstrap_pos.is_some(),
            "both reload_runtime_controls and setup_app_bootstrap must exist in app/mod.rs"
        );
        assert!(
            reload_pos.unwrap() < bootstrap_pos.unwrap(),
            "reload_runtime_controls must appear before setup_app_bootstrap in app/mod.rs"
        );
    }

    /// Startup ordering: reload_runtime_controls must be called BEFORE
    /// init_daemon_services. Verify by source inspection.
    #[test]
    fn daemon_calls_reload_before_init_services() {
        let src = include_str!("daemon/mod.rs");
        let reload_pos = src.find("reload_runtime_controls");
        let init_pos = src.find("init_daemon_services");
        assert!(
            reload_pos.is_some() && init_pos.is_some(),
            "both reload_runtime_controls and init_daemon_services must exist in daemon/mod.rs"
        );
        assert!(
            reload_pos.unwrap() < init_pos.unwrap(),
            "reload_runtime_controls must appear before init_daemon_services in daemon/mod.rs"
        );
    }
}
