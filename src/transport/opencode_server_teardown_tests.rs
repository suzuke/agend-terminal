use super::{launch_server, managed_servers, server_key, stop_instance_server};
use crate::transport::SessionLocator;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use uuid::Uuid;

const FIXTURE_CPU_LIMIT_SECS: u64 = 30;

fn write_term_ignoring_fixture(fake: &Path, trap_ready: &Path, cpu_limit_secs: u64) {
    std::fs::write(
        fake,
        format!(
            "#!/bin/sh\nulimit -t {cpu_limit_secs}\ntrap ':' TERM\ntouch '{}'\nwhile :; do :; done\n",
            trap_ready.display()
        ),
    )
    .expect("fake binary");
    std::fs::set_permissions(fake, std::fs::Permissions::from_mode(0o700))
        .expect("fake executable");
}

struct FixtureCleanup {
    home: PathBuf,
    instance: &'static str,
    previous_binary: Option<OsString>,
}

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        stop_instance_server(&self.home, self.instance);
        match self.previous_binary.take() {
            Some(value) => std::env::set_var("AGEND_OPENCODE_BINARY", value),
            None => std::env::remove_var("AGEND_OPENCODE_BINARY"),
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn term_ignoring_fixture_has_a_bounded_lifetime() {
    let home = std::env::temp_dir().join(format!("agend-opencode-self-reap-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let fake = home.join("fake-opencode.sh");
    let trap_ready = home.join("trap-ready");
    write_term_ignoring_fixture(&fake, &trap_ready, 1);

    let mut child = Command::new(&fake).spawn().expect("bounded fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = child.try_wait().expect("probe bounded fixture");
    while status.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        status = child.try_wait().expect("reap bounded fixture");
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(home);
    assert!(
        status.is_some(),
        "TERM-ignoring fixture must self-reap when its harness disappears"
    );
}

#[test]
#[serial_test::serial]
fn managed_fixture_cleanup_reaps_during_unwind() {
    let home = std::env::temp_dir().join(format!("agend-opencode-unwind-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let fake = home.join("fake-opencode.sh");
    let trap_ready = home.join("trap-ready");
    write_term_ignoring_fixture(&fake, &trap_ready, FIXTURE_CPU_LIMIT_SECS);
    let previous_binary = std::env::var_os("AGEND_OPENCODE_BINARY");
    let expected_binary = previous_binary.clone();
    let mut fixture_pid = None;

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = FixtureCleanup {
            home: home.clone(),
            instance: "panic-agent",
            previous_binary,
        };
        std::env::set_var("AGEND_OPENCODE_BINARY", &fake);
        let mut locator = SessionLocator::opencode(
            "http://127.0.0.1:4096".to_string(),
            None,
            "opencode".to_string(),
            "secret".to_string(),
        );
        locator.managed = true;
        launch_server(&home, "panic-agent", &mut locator, None).expect("launch fake server");
        fixture_pid = locator.server_pid;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !trap_ready.exists() {
            assert!(
                Instant::now() < deadline,
                "unwind fixture did not install TERM trap"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("exercise fixture unwind cleanup");
    }));

    assert!(unwind.is_err(), "fixture must exercise the unwind path");
    let pid = fixture_pid.expect("fixture pid");
    assert!(
        crate::process::process_start_token(pid).is_none(),
        "unwind cleanup must reap the managed server"
    );
    assert!(
        !home.exists(),
        "unwind cleanup must remove the fixture home"
    );
    assert_eq!(
        std::env::var_os("AGEND_OPENCODE_BINARY"),
        expected_binary,
        "unwind cleanup must restore the process environment"
    );
}

#[test]
#[serial_test::serial]
fn managed_teardown_kills_isolated_term_ignoring_server() {
    let home = std::env::temp_dir().join(format!("agend-opencode-teardown-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let fake = home.join("fake-opencode.sh");
    let trap_ready = home.join("trap-ready");
    let previous_binary = std::env::var_os("AGEND_OPENCODE_BINARY");
    let _cleanup = FixtureCleanup {
        home: home.clone(),
        instance: "agent",
        previous_binary,
    };
    write_term_ignoring_fixture(&fake, &trap_ready, FIXTURE_CPU_LIMIT_SECS);
    std::env::set_var("AGEND_OPENCODE_BINARY", &fake);
    let mut locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        None,
        "opencode".to_string(),
        "secret".to_string(),
    );
    locator.managed = true;
    launch_server(&home, "agent", &mut locator, None).expect("launch fake server");
    let pid = locator.server_pid.expect("server pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !trap_ready.exists() {
        assert!(
            Instant::now() < deadline,
            "fake server did not install TERM trap"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let key = server_key(&home, "agent", &locator);
    assert!(
        managed_servers().lock().contains_key(&key),
        "fixture must remain registry-owned until teardown"
    );

    stop_instance_server(&home, "agent");
    let deadline = Instant::now() + Duration::from_secs(2);
    while crate::process::process_start_token(pid).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        crate::process::process_start_token(pid).is_none(),
        "reaped isolated server must no longer have a start token"
    );
}
