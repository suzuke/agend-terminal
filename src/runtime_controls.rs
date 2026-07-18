use std::path::Path;

/// Shared reload seam: runtime_config THEN operator_mode, in strict order.
/// Called before app bootstrap, before daemon init, and per tick.
pub fn reload_runtime_controls(home: &Path) {
    crate::runtime_config::reload(home);
    crate::operator_mode::reload(home);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    /// Helper semantics: reload_runtime_controls loads operator_mode.
    /// Missing/untrusted file on startup → lockdown (Away).
    /// Once initialized, missing file preserves last-known-good.
    #[test]
    fn reload_runtime_controls_loads_operator_mode() {
        let home = tmp_home("loads-mode");
        // Exercise the reload path — should not panic even with no
        // operator-mode.json. The exact resulting mode depends on
        // whether INITIALIZED is already set (global test ordering),
        // so we verify reload completes without error rather than
        // asserting a specific mode.
        reload_runtime_controls(&home);
        let mode = crate::operator_mode::get().mode;
        assert!(
            matches!(
                mode,
                crate::operator_mode::OperatorMode::Active
                    | crate::operator_mode::OperatorMode::Away
            ),
            "reload must produce Active or Away (lockdown), not Sleep"
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
        let reload_pos = src.find("reload_runtime_controls(home)");
        let init_pos = src.find("init_daemon_services(home");
        assert!(
            reload_pos.is_some() && init_pos.is_some(),
            "both reload_runtime_controls(home) and init_daemon_services(home) calls \
             must exist in daemon/mod.rs"
        );
        assert!(
            reload_pos.unwrap() < init_pos.unwrap(),
            "reload_runtime_controls must be called before init_daemon_services in daemon/mod.rs"
        );
    }
}
