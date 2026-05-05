//! Sprint 52 router-layer — observes agent PTY output and mirrors to
//! the originating channel when `reply_to` is set.
//!
//! PR-A: skeleton thread + subscriber consumer. No mirror dispatch yet (PR-B).
//!
//! Lock ordering: router thread NEVER acquires L1 (registry) or L2 (agent_core).
//! It reads from crossbeam subscriber channels (no lock) and writes only to
//! L3 (heartbeat_pair, leaf-level). Mirror dispatch uses channel::send_from_agent
//! (no daemon lock involved).

use crate::agent::AgentRegistry;
use std::path::PathBuf;

/// Spawn the router observer thread. Subscribes to all agents' PTY output
/// and processes mirror events.
///
/// // fire-and-forget: router thread runs for the daemon process lifetime.
/// // Terminates implicitly on process exit. No graceful-stop needed because
/// // the thread is read-only (subscriber channel consumer + heartbeat_pair
/// // leaf writes). Same shutdown contract as supervisor (see supervisor.rs L58).
pub fn spawn(home: PathBuf, registry: AgentRegistry) {
    let _ = std::thread::Builder::new()
        .name("router".into())
        .spawn(move || run_loop(home, registry));
}

fn run_loop(_home: PathBuf, _registry: AgentRegistry) {
    let _census = crate::thread_census::register("router");
    crate::sync_audit::mark_router_thread();
    // PR-A: skeleton loop. PR-B adds subscriber registration + mirror dispatch.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
}
