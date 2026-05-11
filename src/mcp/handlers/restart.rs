use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_restart_daemon(home: &Path) -> Value {
    crate::daemon::RESTART_PENDING.store(true, std::sync::atomic::Ordering::Release);
    std::fs::write(home.join("restart-requested"), "").ok();
    let _ = crate::api::call(home, &json!({"method": crate::api::method::SHUTDOWN}));
    json!({"ok": true, "restart": "pending", "note": "daemon will exit(42) after graceful shutdown; wrapper script restarts"})
}
