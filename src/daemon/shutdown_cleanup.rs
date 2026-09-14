//! #3508: managed-transport teardown helpers for daemon shutdown.
//!
//! Extracted from `daemon/mod.rs` to keep that grandfathered file under
//! its 3217-LOC ceiling (see `tests/src_file_size_invariant.rs`).
//! The helpers reuse the single audited `remove_instance_delivery_state`
//! entry, so the daemon shutdown path cannot drift from the instance-
//! deletion path.

use std::path::Path;

/// Discover and persist null Codex thread locators while the agent TUI and
/// managed app-server are still alive. Shutdown remains best-effort so one
/// broken backend cannot strand teardown; callers receive counts for the
/// operator-visible shutdown receipt/log.
pub(crate) fn checkpoint_codex_sessions(home: &Path, instances: &[String]) -> (usize, usize) {
    let mut checkpointed = 0usize;
    let mut failed = 0usize;
    for instance in instances {
        match crate::transport::checkpoint_codex_session(home, instance) {
            Ok(Some(thread_id)) => {
                tracing::info!(
                    instance = %instance,
                    thread_id = %thread_id,
                    "Codex thread checkpoint persisted before shutdown"
                );
                checkpointed += 1;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    error = %error,
                    "Codex thread checkpoint failed before shutdown"
                );
                failed += 1;
            }
        }
    }
    (checkpointed, failed)
}

/// Per-instance managed-transport teardown invoked from
/// `shutdown_sequence`. Reuses the single audited
/// `remove_instance_delivery_state` entry (codex_app_server +
/// opencode_server + ChannelBridge + durable latches + receipts) so
/// the daemon shutdown path cannot drift from the instance-deletion
/// path. PID-reuse safety is inside each adapter
/// (`process_start_token` + `server_start_token`), process_group kill
/// is inside `stop_owned_process` / `kill_process_tree`, and
/// ChannelBridge join + state-dir removal is inside
/// `stop_instance_state`.
///
/// Returns (cleaned, failed). Failures are logged as warn and
/// counted for the `daemon_stop` receipt — callers must not abort
/// the shutdown on one instance's failure.
pub(crate) fn cleanup_managed_transports(home: &Path, instances: &[String]) -> (usize, usize) {
    let mut cleaned = 0usize;
    let mut failed = 0usize;
    // #3515 follow-up: signal EVERY instance's transport workers before joining
    // any of them. A resident worker can take until its read timeout to notice a
    // stop flag, and the loop below waits for each instance in turn — so setting
    // the flags one at a time made that wait accumulate across the fleet. Setting
    // them all up front lets the waits overlap; the loop below is unchanged and
    // still removes and waits for every instance, so nothing is skipped.
    for instance in instances {
        crate::transport::signal_instance_transport_stop(home, instance);
    }
    for instance in instances {
        match crate::transport::remove_instance_delivery_state(home, instance) {
            Ok(dropped) => {
                if !dropped.is_empty() {
                    tracing::warn!(
                        instance = %instance,
                        dropped = ?dropped,
                        "transport cleanup discarded owed notice debt (audited)"
                    );
                }
                tracing::info!(instance = %instance, "transport cleanup complete");
                cleaned += 1;
            }
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    error = %error,
                    "transport cleanup failed (audited receipt retained)"
                );
                failed += 1;
            }
        }
    }
    (cleaned, failed)
}

/// Sweep daemon-owned transport state whose instance no longer
/// appears in the drained set (residual from an unclean prior
/// shutdown). Only scans the daemon-owned `transport/sessions/*.json`
/// that `remove_instance_delivery_state` is authoritative for;
/// it never scans argv/cwd or guesses arbitrary orphans per #3273.
pub(crate) fn sweep_residual_transports(home: &Path, live: &[String]) -> (usize, usize) {
    use std::collections::HashSet;
    let live_safe: HashSet<String> = live
        .iter()
        .map(|name| crate::transport::safe_component(name))
        .collect();
    let mut cleaned = 0usize;
    let mut failed = 0usize;

    let sessions_dir = home.join("transport").join("sessions");
    let residual: Vec<String> = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(|stem| stem.to_string())
                } else {
                    None
                }
            })
            .filter(|stem| !live_safe.contains(stem))
            .collect(),
        Err(_) => Vec::new(),
    };
    for stem in residual {
        // `stem` is already the safe_component; passing it as the
        // instance name still locates the correct files because the
        // cleanup path re-applies safe_component internally.
        match crate::transport::remove_instance_delivery_state(home, &stem) {
            Ok(dropped) => {
                if !dropped.is_empty() {
                    tracing::warn!(
                        instance = %stem,
                        dropped = ?dropped,
                        "residual transport sweep discarded owed notice debt"
                    );
                }
                tracing::info!(instance = %stem, "residual transport sweep cleaned");
                cleaned += 1;
            }
            Err(error) => {
                tracing::warn!(
                    instance = %stem,
                    error = %error,
                    "residual transport sweep failed"
                );
                failed += 1;
            }
        }
        // Also attempt to remove the session locator file itself if it
        // survived (remove_instance_delivery_state only removes delivery
        // receipts; the session locator is owned by the adapter's stop
        // helper, but for residual unknown instances there is no adapter
        // to call — so ensure the session file does not persist as a
        // stale locator that could cause a next-start collision).
        let session_path = sessions_dir.join(format!("{stem}.json"));
        if session_path.exists() {
            match std::fs::remove_file(&session_path) {
                Ok(()) => {
                    tracing::info!(instance = %stem, "residual session locator removed");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        instance = %stem,
                        error = %error,
                        "failed to remove residual session locator"
                    );
                }
            }
        }
    }
    (cleaned, failed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shutdown_checkpoints_codex_before_agent_termination() {
        let src = include_str!("./mod.rs");
        let prod_end = src
            .find("\n#[cfg(test)]\n#[allow(clippy::unwrap_used, clippy::expect_used)]\nmod tests {")
            .unwrap_or(src.len());
        let prod = &src[..prod_end];
        let checkpoint = prod
            .find("shutdown_cleanup::checkpoint_codex_sessions")
            .expect("shutdown must checkpoint Codex sessions");
        let terminate = prod
            .find("terminate_agents_parallel(agents_to_kill)")
            .expect("shutdown must terminate agents");
        assert!(
            checkpoint < terminate,
            "Codex thread checkpoint must precede agent termination"
        );
    }
}
