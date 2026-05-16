use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_restart_daemon(home: &Path) -> Value {
    // #851 fail-closed: refuse the request when no supervisor will
    // respawn the daemon. Without this check, `restart_daemon` on a
    // bare `agend-terminal start` invocation would set
    // `RESTART_PENDING`, the daemon would exit(42) on the next
    // supervisor tick, and operators would see
    // `Resource temporarily unavailable (os error 35)` on every
    // subsequent MCP call until they manually restart.
    if !crate::daemon::restart::is_restart_supervised() {
        return json!({
            "ok": false,
            "error": "restart_daemon requires a supervisor (launchd / systemd / scripts/agend-wrapper.sh / Task Scheduler). Detected: bare daemon invocation. Run `agend-terminal service install` to enable safe restart, or launch via `scripts/agend-wrapper.sh` for manual operation."
        });
    }
    crate::daemon::RESTART_PENDING.store(true, std::sync::atomic::Ordering::Release);
    std::fs::write(home.join("restart-requested"), "").ok();
    let _ = crate::api::call(home, &json!({"method": crate::api::method::SHUTDOWN}));
    json!({"ok": true, "restart": "pending", "note": "daemon will exit(42) after graceful shutdown; supervisor restarts"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, OnceLock};

    /// Tests touch the process-global `RESTART_PENDING` static and env
    /// state — serialise via a function-scoped mutex so parallel test
    /// threads can't race. Also resets `RESTART_PENDING` after each
    /// test so the next caller sees a clean slate.
    fn with_env_and_reset<R>(set: &[(&str, &str)], unset: &[&str], f: impl FnOnce() -> R) -> R {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

        let all_keys: Vec<&str> = set
            .iter()
            .map(|(k, _)| *k)
            .chain(unset.iter().copied())
            .collect();
        let prior: Vec<(String, Option<String>)> = all_keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        let prior_restart_pending = crate::daemon::RESTART_PENDING.load(Ordering::Acquire);

        // SAFETY: test-only mutation, serialised via the mutex above.
        unsafe {
            for k in unset {
                std::env::remove_var(k);
            }
            for (k, v) in set {
                std::env::set_var(k, v);
            }
        }
        crate::daemon::RESTART_PENDING.store(false, Ordering::Release);

        let result = f();

        unsafe {
            for (k, v) in &prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        crate::daemon::RESTART_PENDING.store(prior_restart_pending, Ordering::Release);

        result
    }

    /// Contract: when no supervisor signal env is set,
    /// `handle_restart_daemon` must fail-closed — return `ok: false`
    /// with an actionable error, and must NOT set `RESTART_PENDING`
    /// (which would trigger `exit(42)` on the next supervisor tick
    /// and leave the daemon dead with nothing to respawn it).
    #[test]
    fn handle_restart_daemon_fails_closed_when_unsupervised() {
        with_env_and_reset(
            &[],
            &["AGEND_WRAPPED", "XPC_SERVICE_NAME", "INVOCATION_ID"],
            || {
                // Manual tempdir — matches the std::env::temp_dir pattern
                // used elsewhere in this crate (e.g. claim_verifier.rs
                // tests). Avoids adding tempfile as a dev-dep just for
                // this one test.
                let tmp = std::env::temp_dir().join(format!(
                    "agend-restart-test-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                std::fs::create_dir_all(&tmp).expect("create tempdir");
                let response = handle_restart_daemon(&tmp);
                assert_eq!(
                    response["ok"], false,
                    "unsupervised invocation must return ok:false, got {response}"
                );
                let error = response["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("supervisor"),
                    "error string must name the missing supervisor — got {error:?}"
                );
                assert!(
                    !crate::daemon::RESTART_PENDING.load(Ordering::Acquire),
                    "RESTART_PENDING must NOT be set when fail-closed — \
                     otherwise the next supervisor tick will exit(42) \
                     and leave the daemon dead with no respawn"
                );
                // restart-requested marker file must also NOT be written
                // (it's both a side-effect and an integration signal).
                assert!(
                    !tmp.join("restart-requested").exists(),
                    "restart-requested marker file must NOT exist when fail-closed"
                );
                let _ = std::fs::remove_dir_all(&tmp);
            },
        );
    }
}
