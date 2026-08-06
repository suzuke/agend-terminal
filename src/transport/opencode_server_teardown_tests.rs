use super::{launch_server, managed_servers, server_key, stop_instance_server};
use crate::transport::SessionLocator;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[test]
#[serial_test::serial]
fn managed_teardown_kills_isolated_term_ignoring_server() {
    let home = std::env::temp_dir().join(format!("agend-opencode-teardown-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let fake = home.join("fake-opencode.sh");
    let trap_ready = home.join("trap-ready");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\ntrap ':' TERM\ntouch '{}'\nwhile :; do :; done\n",
            trap_ready.display()
        ),
    )
    .expect("fake binary");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))
        .expect("fake executable");
    let previous_binary = std::env::var_os("AGEND_OPENCODE_BINARY");
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
    let managed = managed_servers()
        .lock()
        .remove(&key)
        .expect("managed server");
    // Retain and reap Child: otherwise Linux SIGKILL leaves a zombie /proc entry.
    let mut child = managed.child;

    stop_instance_server(&home, "agent");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut status = child
        .try_wait()
        .expect("probe isolated server after teardown");
    while status.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        status = child
            .try_wait()
            .expect("reap isolated server after teardown");
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("isolated term-ignoring server must be killed during teardown");
    }
    assert!(
        crate::process::process_start_token(pid).is_none(),
        "reaped isolated server must no longer have a start token"
    );
    match previous_binary {
        Some(value) => std::env::set_var("AGEND_OPENCODE_BINARY", value),
        None => std::env::remove_var("AGEND_OPENCODE_BINARY"),
    }
    let _ = std::fs::remove_dir_all(home);
}
