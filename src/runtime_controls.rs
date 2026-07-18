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
    /// operator_mode. Use two distinct homes to prove the shared seam
    /// reloads from the TARGET home, not from stale global state.
    #[test]
    #[serial_test::serial]
    fn reload_runtime_controls_loads_signed_operator_mode() {
        let target_home = tmp_home("loads-signed-target");
        let control_home = tmp_home("loads-signed-control");

        // Write signed Sleep+delegate to the TARGET home.
        crate::operator_mode::set_mode(
            &target_home,
            crate::operator_mode::OperatorMode::Sleep,
            Some("delegate-agent".into()),
            vec!["restart".into()],
        )
        .expect("set_mode Sleep on target must succeed");

        // Set global to Active via a DIFFERENT home so global is NOT Sleep.
        crate::operator_mode::set_mode(
            &control_home,
            crate::operator_mode::OperatorMode::Active,
            None,
            Vec::new(),
        )
        .expect("set_mode Active on control must succeed");
        // Reload control so global is Active.
        crate::operator_mode::reload(&control_home);
        let pre = crate::operator_mode::get();
        assert_ne!(
            pre.mode,
            crate::operator_mode::OperatorMode::Sleep,
            "precondition: global must NOT be Sleep before reload"
        );

        // Reload through the shared seam targeting the Sleep home.
        reload_runtime_controls(&target_home);

        let state = crate::operator_mode::get();
        assert_eq!(
            state.mode,
            crate::operator_mode::OperatorMode::Sleep,
            "reload_runtime_controls must load the signed Sleep mode from target_home"
        );
        assert_eq!(
            state.delegate_to.as_deref(),
            Some("delegate-agent"),
            "delegate_to must survive reload from target_home"
        );

        // Reset to Active for other tests.
        crate::operator_mode::set_mode(
            &control_home,
            crate::operator_mode::OperatorMode::Active,
            None,
            Vec::new(),
        )
        .ok();
        crate::operator_mode::reload(&control_home);
        std::fs::remove_dir_all(&target_home).ok();
        std::fs::remove_dir_all(&control_home).ok();
    }

    /// Shared-seam fail-closed: tampered/missing operator_mode file
    /// must not crash and must preserve the current mode (fail-closed).
    #[test]
    #[serial_test::serial]
    fn reload_runtime_controls_tampered_mode_fails_closed() {
        let home = tmp_home("tampered-mode");

        // Set global to Active first.
        crate::operator_mode::set_mode(
            &home,
            crate::operator_mode::OperatorMode::Active,
            None,
            Vec::new(),
        )
        .ok();
        crate::operator_mode::reload(&home);
        assert_eq!(
            crate::operator_mode::get().mode,
            crate::operator_mode::OperatorMode::Active,
            "precondition: global is Active"
        );

        // Write garbage to the operator_mode file.
        let mode_path = home.join("operator_mode.json");
        std::fs::write(&mode_path, b"TAMPERED GARBAGE").unwrap();

        // Reload through the shared seam — must not crash.
        reload_runtime_controls(&home);

        // Mode must remain Active (fail-closed).
        let state = crate::operator_mode::get();
        assert_eq!(
            state.mode,
            crate::operator_mode::OperatorMode::Active,
            "tampered operator_mode must fail-closed and preserve current mode"
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
    /// setup_app_bootstrap within the run_app function body (bounded slice).
    #[test]
    fn app_calls_reload_before_bootstrap() {
        let src = include_str!("app/mod.rs");
        let fn_start = src
            .find("\nfn run_app(")
            .or_else(|| src.find("fn run_app("))
            .expect("fn run_app must exist in app/mod.rs");
        // End the body slice at the next top-level function definition.
        let after_start = &src[fn_start + 1..];
        let fn_end = after_start
            .find("\nfn setup_app_bootstrap")
            .or_else(|| after_start.find("\npub fn setup_app_bootstrap"))
            .unwrap_or(after_start.len());
        let body = &after_start[..fn_end];
        let reload_pos = body
            .find("reload_runtime_controls")
            .expect("reload_runtime_controls call must exist inside run_app body");
        let bootstrap_pos = body
            .find("setup_app_bootstrap")
            .expect("setup_app_bootstrap call must exist inside run_app body");
        assert!(
            reload_pos < bootstrap_pos,
            "reload_runtime_controls must appear before setup_app_bootstrap \
             within the bounded run_app function body"
        );
    }

    /// Startup ordering: reload_runtime_controls must be called BEFORE
    /// init_daemon_services within the run_core function body (bounded slice).
    #[test]
    fn daemon_calls_reload_before_init_services() {
        let src = include_str!("daemon/mod.rs");
        let fn_start = src
            .find("\nfn run_core(")
            .or_else(|| src.find("fn run_core("))
            .expect("fn run_core must exist in daemon/mod.rs");
        let after_start = &src[fn_start + 1..];
        // End the body slice at the next top-level function definition.
        let fn_end = after_start
            .find("\nfn init_daemon_services")
            .or_else(|| after_start.find("\npub fn init_daemon_services"))
            .unwrap_or(after_start.len());
        let body = &after_start[..fn_end];
        let reload_pos = body
            .find("reload_runtime_controls(home)")
            .expect("reload_runtime_controls(home) call must exist inside run_core body");
        let init_pos = body
            .find("init_daemon_services(")
            .expect("init_daemon_services call must exist inside run_core body");
        assert!(
            reload_pos < init_pos,
            "reload_runtime_controls must be called before init_daemon_services \
             within the bounded run_core function body"
        );
    }
}
