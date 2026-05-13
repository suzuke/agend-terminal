//! External-agent liveness sweep: drop entries whose PID is no longer
//! alive. Extracted verbatim from `src/daemon/mod.rs:647-658` (pre-T-B3) —
//! same `retain` semantics, same logging, same `event_log::log` call.
//! Stateless handler: nothing to carry across ticks.

use super::{PerTickHandler, TickContext};

pub(crate) struct ExternalLivenessHandler;

impl ExternalLivenessHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for ExternalLivenessHandler {
    fn name(&self) -> &'static str {
        "external_liveness"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        let mut ext = crate::agent::lock_external(ctx.externals);
        ext.retain(|name, handle| {
            let alive = crate::process::is_pid_alive(handle.pid);
            if !alive {
                tracing::info!(
                    agent = %name,
                    pid = handle.pid,
                    "external agent gone, deregistering"
                );
                crate::event_log::log(ctx.home, "disconnect", name, "external agent PID gone");
            }
            alive
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalAgentHandle, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-extlive-handler-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn empty_ctx<'a>(
        home: &'a std::path::Path,
        registry: &'a AgentRegistry,
        externals: &'a ExternalRegistry,
        configs: &'a Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) -> TickContext<'a> {
        TickContext {
            home,
            registry,
            externals,
            configs,
        }
    }

    /// Empty registry — run() is a no-op and the registry stays empty.
    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = tmp_home("empty");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = empty_ctx(&home, &registry, &externals, &configs);

        let h = ExternalLivenessHandler::new();
        h.run(&ctx);

        assert!(externals.lock().is_empty());
    }

    /// Entry with a known-dead PID is evicted; entry with the current
    /// process PID survives. Pins the retain-by-`is_pid_alive` semantics.
    #[test]
    fn run_evicts_dead_pid_keeps_live_pid() {
        let home = tmp_home("evict");
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));

        // 999_999_999 is well above macOS's default pid_max (99998) and
        // Linux's default (32768), so `kill(999_999_999, 0)` returns
        // ESRCH on every platform we run on. (We can't use PID 0 — on
        // POSIX, `kill(0, 0)` is a process-group permission check that
        // returns 0/"alive" for any user.) Current process is provably
        // alive for the contrasting case.
        externals.lock().insert(
            "dead-agent".to_string(),
            ExternalAgentHandle {
                backend_command: "claude".to_string(),
                pid: 999_999_999,
            },
        );
        externals.lock().insert(
            "live-agent".to_string(),
            ExternalAgentHandle {
                backend_command: "claude".to_string(),
                pid: std::process::id(),
            },
        );

        let ctx = empty_ctx(&home, &registry, &externals, &configs);
        ExternalLivenessHandler::new().run(&ctx);

        let ext = externals.lock();
        assert!(!ext.contains_key("dead-agent"), "dead PID must be evicted");
        assert!(ext.contains_key("live-agent"), "live PID must be retained");
    }
}
