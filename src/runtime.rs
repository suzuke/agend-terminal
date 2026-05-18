//! #830: shared runtime-registry helpers. Consolidates the
//! `api::call(LIST)` liveness-cross-ref pattern that was duplicated
//! across `src/teams.rs:200-212` (#785), `src/render/panels.rs::
//! fetch_live_agents` (#827), and `src/tasks.rs::fetch_live_agents`
//! (#829). The fourth consumer arriving in `task action=health`
//! (this PR) is the design-call threshold that justifies extraction.
//!
//! #910 PR1 (this PR) adds `list_agents_with_fallback`, the foundation
//! helper that PR2-4 migrate the 5 `.port`-glob consume sites onto.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Fetch the daemon's live runtime agent registry via
/// `api::call(LIST)`. Returns `Some(set)` on success (the set may
/// legitimately be empty when no agents are running) and `None`
/// when the daemon is offline / unreachable.
///
/// The `None`-vs-`Some(empty)` distinction lets callers degrade
/// differently: render-time filters (#827) keep all assignees on
/// `None` to avoid misleading "all idle" reports; the #829 boot
/// sweeper skips the orphan-clear entirely on `None` to avoid
/// over-orphaning during the daemon's own socket-bind race; #830's
/// health response surfaces a degraded `live_agents_available: false`
/// hint to the operator. Each caller chooses its own fallback by
/// matching on the `Option`.
pub fn list_live_agents(home: &Path) -> Option<HashSet<String>> {
    crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::LIST}),
    )
    .ok()
    .and_then(|r| {
        r["result"]["agents"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|a| a["name"].as_str().map(String::from))
                .collect()
        })
    })
}

// ──────────────────────────────────────────────────────────────────────
// #910 PR1 of 4: list_agents_with_fallback foundation helper
//
// Per `/tmp/dialectic-910-synthesis.md` lead decision. Zero caller-site
// changes in this PR; PR2-4 migrate the 5 existing .port-glob consume
// sites (`src/app/session.rs:274`, `src/app/mod.rs:621`, `src/cli.rs:155`,
// `src/main.rs:683`, `src/agent_ops.rs:339`) over to this helper in turn.
// ──────────────────────────────────────────────────────────────────────

/// Source of truth for the most-recent agent list resolution: API
/// or filesystem-glob fallback.
#[allow(dead_code)] // PR1 of 4 foundation — consumed by PR2 caller migration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentListMode {
    Live,
    Fallback,
}

/// Track the most-recent mode so we can log Live↔Fallback transitions
/// instead of every single call. Important because some callers
/// (e.g. `src/app/mod.rs:621` periodic sync) fire every 2 seconds —
/// per-call logging would dump ~1800 lines/hr of redundant info.
#[allow(dead_code)] // PR1 of 4 foundation
static LAST_MODE: OnceLock<Mutex<Option<AgentListMode>>> = OnceLock::new();

#[allow(dead_code)] // PR1 of 4 foundation
fn note_mode_transition(new_mode: AgentListMode) {
    let lock = LAST_MODE.get_or_init(|| Mutex::new(None));
    let Ok(mut guard) = lock.lock() else {
        return;
    };
    let prev = *guard;
    *guard = Some(new_mode);
    match (prev, new_mode) {
        (Some(AgentListMode::Live), AgentListMode::Fallback) => {
            tracing::info!(
                "#910 list_agents_with_fallback: daemon offline → falling back to .port glob"
            );
        }
        (Some(AgentListMode::Fallback), AgentListMode::Live) => {
            tracing::info!(
                "#910 list_agents_with_fallback: daemon reachable → registry is authoritative"
            );
        }
        (None, mode) => {
            // First call in process lifetime. One-time info-level entry
            // so operators grepping daemon.log can confirm the helper
            // initialized + observe the initial mode.
            tracing::info!(
                ?mode,
                "#910 list_agents_with_fallback: initial mode resolved"
            );
        }
        // Steady-state (Live → Live or Fallback → Fallback): no log.
        _ => {}
    }
}

/// Resolve the live agent list with daemon-API-first + filesystem-glob
/// fallback. **Use this in place of bare `ipc::list_agent_ports(run)`
/// in operator-facing surfaces.** The daemon's in-memory registry is
/// truth-of-record when reachable; the `.port` filesystem glob is the
/// best-effort fallback when the API call fails (daemon offline /
/// restarting / connect-refused).
///
/// Returns an alphabetically-sorted `Vec<String>`. Empty when neither
/// path yields anything (no daemon AND no run dir, or daemon reachable
/// but registry is empty).
///
/// **Why not just call `list_live_agents` directly?** Production CLI
/// surfaces (`agend-terminal list`, `agend-terminal doctor`) historically
/// scan `*.port` when the API is unresponsive — operators rely on that
/// fallback during daemon restart windows. This helper preserves the
/// behavior while making the daemon-registry path the default.
///
/// **Worst-case latency**: bounded by `api::call`'s connect attempt
/// (~1s default Unix-socket timeout). When the daemon is offline,
/// expect a predictable ~1s delay before fallback. Acceptable for the
/// 2s app-sync cadence at `src/app/mod.rs:621` and one-shot CLI use
/// cases; do NOT call from per-tick hot paths.
///
/// **Log cadence**: only Live↔Fallback transitions log at info level.
/// Steady-state Live → Live (or Fallback → Fallback) calls are silent.
///
/// **State leak across tests**: the in-process `LAST_MODE` tracker is a
/// `OnceLock<Mutex<Option<_>>>` singleton — tests that assert log
/// content must run with `cargo test -- --test-threads=1` or accept
/// log ordering uncertainty. The transition log is observability, not
/// a tested contract; cf. tests in this module assert RESULT not LOG.
///
/// #910 PR1: foundation helper, zero caller-site changes. PR2-4
/// migrate the 5 existing `.port`-glob consume sites.
#[allow(dead_code)] // PR1 of 4 foundation — consumed by PR2 caller migration
pub fn list_agents_with_fallback(home: &Path) -> Vec<String> {
    let live = list_live_agents(home);
    list_agents_with_fallback_using(home, live)
}

/// Factored variant of [`list_agents_with_fallback`] that accepts the
/// `live` `Option<HashSet>` directly. Lets tests inject the
/// daemon-registry result without spinning up an in-process API server.
/// Production callers use the wrapper [`list_agents_with_fallback`].
#[allow(dead_code)] // PR1 of 4 foundation
pub(crate) fn list_agents_with_fallback_using(
    home: &Path,
    live: Option<HashSet<String>>,
) -> Vec<String> {
    if let Some(live_set) = live {
        note_mode_transition(AgentListMode::Live);
        let mut v: Vec<String> = live_set.into_iter().collect();
        v.sort();
        return v;
    }
    note_mode_transition(AgentListMode::Fallback);
    let Some(run) = crate::daemon::find_active_run_dir(home) else {
        return Vec::new();
    };
    let mut v = crate::ipc::list_agent_ports(&run);
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_home(suffix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-runtime-910-{}-{}-{}",
            std::process::id(),
            suffix,
            id
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn pid_string() -> String {
        std::process::id().to_string()
    }

    fn setup_run_dir(home: &Path) -> PathBuf {
        let run = home.join("run").join(pid_string());
        fs::create_dir_all(&run).expect("create run_dir");
        fs::write(run.join(".daemon"), format!("{}:0", pid_string())).expect("write .daemon");
        run
    }

    fn write_port_file(run: &Path, name: &str) {
        fs::write(run.join(format!("{name}.port")), "12345").expect("write .port");
    }

    /// PR1 RED 1: daemon reachable → registry truth; stale `.port` ghosts
    /// MUST NOT surface in the result.
    #[test]
    fn list_agents_with_fallback_prefers_daemon_registry_when_reachable() {
        let home = tmp_home("prefers-registry");
        let run = setup_run_dir(&home);
        write_port_file(&run, "ghost-1"); // stale glob entry — must NOT surface

        let mut live: HashSet<String> = HashSet::new();
        live.insert("live-1".to_string());
        live.insert("live-2".to_string());

        let result = list_agents_with_fallback_using(&home, Some(live));

        assert_eq!(
            result,
            vec!["live-1".to_string(), "live-2".to_string()],
            "registry is truth-of-record when API reachable (alphabetical sort)"
        );
        assert!(
            !result.contains(&"ghost-1".to_string()),
            "stale .port file MUST NOT surface when daemon registry is the truth-of-record"
        );

        fs::remove_dir_all(&home).ok();
    }

    /// PR1 RED 2: API unreachable → fallback to filesystem `.port` glob.
    /// Preserves pre-Option-F UX (cli doctor / cli list still work briefly
    /// during daemon restart windows).
    #[test]
    fn list_agents_with_fallback_falls_back_to_glob_when_daemon_offline() {
        let home = tmp_home("fallback-glob");
        let run = setup_run_dir(&home);
        write_port_file(&run, "fallback-1");
        write_port_file(&run, "fallback-2");

        let result = list_agents_with_fallback_using(&home, None);

        assert_eq!(
            result.iter().cloned().collect::<HashSet<_>>(),
            ["fallback-1", "fallback-2"]
                .iter()
                .map(|s| s.to_string())
                .collect::<HashSet<_>>(),
            "daemon offline → .port glob is the fallback truth"
        );

        fs::remove_dir_all(&home).ok();
    }

    /// PR1 RED 3: no daemon AND no run_dir → empty `Vec`, NO panic.
    /// Cold-boot pre-bootstrap state must degrade gracefully.
    #[test]
    fn list_agents_with_fallback_empty_when_no_daemon_no_run_dir() {
        let home = tmp_home("empty-cold");
        // No run/ subdir created — pre-bootstrap state.

        let result = list_agents_with_fallback_using(&home, None);

        assert!(
            result.is_empty(),
            "no daemon + no run dir → empty Vec (graceful degrade, no panic)"
        );

        fs::remove_dir_all(&home).ok();
    }

    /// PR1 RED 4: API truth wins even when empty. Registry empty
    /// (post-daemon-boot, pre-auto-start window) → empty Vec, NOT a
    /// fallback to stale `.port` ghosts.
    #[test]
    fn list_agents_with_fallback_returns_empty_when_daemon_alive_but_registry_empty() {
        let home = tmp_home("alive-empty");
        let run = setup_run_dir(&home);
        write_port_file(&run, "ghost-stale"); // stale glob — must NOT surface

        let result = list_agents_with_fallback_using(&home, Some(HashSet::new()));

        assert!(
            result.is_empty(),
            "API truth empty wins over filesystem glob (Some(empty) != None)"
        );
        assert!(
            !result.contains(&"ghost-stale".to_string()),
            "empty registry must NOT fall through to stale .port ghosts"
        );

        fs::remove_dir_all(&home).ok();
    }

    /// PR1 RED 5: api.port file present but TCP listener refuses (e.g.
    /// daemon crashed mid-cycle, stale api.port lingers). Production
    /// `list_live_agents` returns None on connect-refused → helper falls
    /// back. This test exercises the FULL production
    /// `list_agents_with_fallback` (not the factored `_using` variant) to
    /// lock the connect-refused → None → fallback chain end-to-end.
    #[test]
    fn list_agents_with_fallback_falls_back_when_api_port_present_but_listener_refused() {
        let home = tmp_home("listener-refused");
        let run = setup_run_dir(&home);
        // Plant a stale api.port pointing at a port nothing is listening
        // on. Port 9 (Discard) is one of the legacy unassigned-in-userland
        // ports — unlikely to have a listener but well-defined behavior
        // (connect-refused) on Unix.
        fs::write(run.join("api.port"), "9").expect("write fake api.port");

        let result = list_agents_with_fallback(&home);

        // Behavioral assertion: no panic + empty Vec when there are no
        // `.port` files for actual agents (only the stale api.port we
        // planted, which list_agent_ports filters out).
        assert!(
            result.is_empty(),
            "connection-refused → treated as offline → fallback fires → empty agents (only stale api.port present, which is filtered)"
        );

        fs::remove_dir_all(&home).ok();
    }
}
