#![allow(clippy::unwrap_used)]

#[cfg(test)]
mod tests {
    use super::super::tests::invoke_runtime_mcp_tool;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        home: PathBuf,
        registry: crate::agent::AgentRegistry,
        old: Option<crate::daemon::ChildHandle>,
        previous_home: Option<std::ffi::OsString>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            *crate::api::FORBID_LOOPBACK_3573.lock() = None;
            let children: Vec<_> = crate::agent::lock_registry(&self.registry)
                .drain()
                .map(|(_, handle)| handle.child)
                .collect();
            for child in children.into_iter().chain(self.old.take()) {
                let mut child = child.lock();
                if matches!(child.try_wait(), Ok(None)) {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
            match self.previous_home.take() {
                Some(value) => std::env::set_var("AGEND_HOME", value),
                None => std::env::remove_var("AGEND_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    #[serial_test::serial]
    fn set_model_real_mcp_success_one_owned_successor_3573() {
        use std::os::unix::fs::PermissionsExt;
        let _serial = crate::mcp::handlers::fleet_test_guard();
        let mut fixture = Fixture {
            home: std::env::temp_dir().join(format!("agend-3573-{}", uuid::Uuid::new_v4())),
            registry: Default::default(),
            old: None,
            previous_home: std::env::var_os("AGEND_HOME"),
        };
        std::fs::create_dir_all(&fixture.home).unwrap();
        std::env::set_var("AGEND_HOME", &fixture.home);
        let script = fixture.home.join("controlled-backend");
        // exec keeps one direct PTY child; no shell-background descendants.
        std::fs::write(&script, "#!/bin/sh\nexec /bin/sleep 300\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = crate::types::InstanceId::new();
        let cwd = fixture.home.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&fixture.home), format!(
        "instances:\n  restart-evidence:\n    id: {}\n    backend: claude\n    command: {}\n    working_directory: {}\n",
        id.full(), script.display(), cwd.display())).unwrap();
        let configs: crate::api::ConfigRegistry = Default::default();
        let externals: crate::agent::ExternalRegistry = Default::default();
        let config = crate::agent::SpawnConfig {
            name: "restart-evidence",
            backend: Some(&crate::backend::Backend::ClaudeCode),
            backend_command: script.to_str().unwrap(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: Some(&cwd),
            submit_key: "\r",
            home: Some(&fixture.home),
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&config, &fixture.registry).unwrap();
        fixture.old = Some(
            crate::agent::lock_registry(&fixture.registry)
                .get(&id)
                .unwrap()
                .child
                .clone(),
        );
        let old_pid = fixture.old.as_ref().unwrap().lock().process_id().unwrap();
        *crate::api::FORBID_LOOPBACK_3573.lock() = Some(fixture.home.clone());
        let trap_home = fixture.home.clone();
        let trap_control = std::thread::spawn(move || {
            std::panic::catch_unwind(|| crate::api::call(&trap_home, &json!({}))).is_err()
        });
        assert!(
            trap_control.join().unwrap(),
            "cross-thread loopback trap must fire"
        );
        let response = invoke_runtime_mcp_tool(
            &fixture.home,
            &fixture.registry,
            &configs,
            &externals,
            "set_model",
            "restart-evidence",
            json!({"instance":"restart-evidence", "model":"test-model-3573", "restart":true}),
        );
        assert_eq!(response["result"]["persisted"], true, "{response}");
        // Restart/TUI 交接語義：此 fixture 真 spawn 成功（successor 存活見下），
        // 但測試環境沒有 TUI client 連新 listener，故交接未確認、
        // restart_ok 為 false（不再誤報成功），persist 與 successor 不變。
        assert_eq!(response["result"]["restart_ok"], false, "{response}");
        assert!(
            response["result"]["restart_error"]
                .as_str()
                .is_some_and(|e| e.contains("TUI did not take over")),
            "restart_error 必須說明交接未確認：{response}"
        );
        let registry = crate::agent::lock_registry(&fixture.registry);
        assert_eq!(registry.len(), 1);
        let successor = registry.get(&id).unwrap();
        assert_ne!(successor.child.lock().process_id().unwrap(), old_pid);
        assert!(matches!(successor.child.lock().try_wait(), Ok(None)));
        drop(registry);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if fixture
                .old
                .as_ref()
                .unwrap()
                .lock()
                .try_wait()
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "old owned child did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(configs.lock().len(), 1);
        let configs = configs.lock();
        let recorded = &configs["restart-evidence"];
        assert_eq!(recorded.backend_command, script.to_str().unwrap());
        assert!(recorded
            .args
            .windows(2)
            .any(|args| args == ["--model", "test-model-3573"]));
        drop(configs);
        assert!(crate::agent::lock_external(&externals).is_empty());
        let fleet =
            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&fixture.home)).unwrap();
        assert_eq!(
            fleet.instances["restart-evidence"].model.as_deref(),
            Some("test-model-3573")
        );
    }
}
