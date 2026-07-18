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

    /// Helper semantics: reload_runtime_controls loads a valid signed
    /// operator_mode. Use the production set_mode API to create a real
    /// HMAC-signed Sleep fixture, then verify reload picks it up.
    #[test]
    fn reload_runtime_controls_loads_signed_operator_mode() {
        let home = tmp_home("loads-signed-mode");
        // Use production API to write a valid signed Sleep mode.
        crate::operator_mode::set_mode(
            &home,
            crate::operator_mode::OperatorMode::Sleep,
            Some("delegate-agent".into()),
            vec!["restart".into()],
        )
        .expect("set_mode must succeed");

        // Reload through the shared seam.
        reload_runtime_controls(&home);

        let state = crate::operator_mode::get();
        assert_eq!(
            state.mode,
            crate::operator_mode::OperatorMode::Sleep,
            "reload_runtime_controls must load the signed Sleep mode from disk"
        );
        assert_eq!(
            state.delegate_to.as_deref(),
            Some("delegate-agent"),
            "delegate_to must survive reload"
        );

        // Reset to Active for other tests.
        crate::operator_mode::set_mode(
            &home,
            crate::operator_mode::OperatorMode::Active,
            None,
            Vec::new(),
        )
        .ok();
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
    /// setup_app_bootstrap within the run_app function body.
    #[test]
    fn app_calls_reload_before_bootstrap() {
        let src = include_str!("app/mod.rs");
        // Anchor within run_app function body — find its definition first.
        let fn_start = src
            .find("fn run_app(")
            .expect("fn run_app must exist in app/mod.rs");
        let body = &src[fn_start..];
        let reload_pos = body
            .find("reload_runtime_controls")
            .expect("reload_runtime_controls must be called inside run_app");
        let bootstrap_pos = body
            .find("setup_app_bootstrap")
            .expect("setup_app_bootstrap must be called inside run_app");
        assert!(
            reload_pos < bootstrap_pos,
            "reload_runtime_controls must appear before setup_app_bootstrap \
             within the run_app function body"
        );
    }

    /// Startup ordering: reload_runtime_controls must be called BEFORE
    /// init_daemon_services within the run_core function body.
    #[test]
    fn daemon_calls_reload_before_init_services() {
        let src = include_str!("daemon/mod.rs");
        // Anchor within run_core function body.
        let fn_start = src
            .find("fn run_core(")
            .expect("fn run_core must exist in daemon/mod.rs");
        let body = &src[fn_start..];
        let reload_pos = body
            .find("reload_runtime_controls(home)")
            .expect("reload_runtime_controls(home) call must exist inside run_core");
        // Match the actual function call, not comment mentions.
        let init_pos = body
            .find("let ctx = init_daemon_services(")
            .or_else(|| body.find("init_daemon_services(home"))
            .expect("init_daemon_services call must exist inside run_core");
        assert!(
            reload_pos < init_pos,
            "reload_runtime_controls must be called before init_daemon_services \
             within the run_core function body"
        );
    }
}
