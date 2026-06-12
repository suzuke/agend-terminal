//! #914 hourly cleanup tick — hard backstop on the rolling appender's
//! disk footprint. `tracing_appender::rolling::Builder::max_log_files`
//! gives us a count cap; this handler gives us a bytes cap so a single
//! pathological day (~800 MB seen during heavy dev sessions, see #914)
//! cannot blow past the budget.
//!
//! Also maintains the Unix `daemon.log` symlink so operators tailing
//! that path keep tracking the active rotated file across midnight
//! boundaries.

use super::{PerTickHandler, TickContext};

pub(crate) struct LogRotationHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl LogRotationHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for LogRotationHandler {
    fn name(&self) -> &'static str {
        "log_rotation"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let max_bytes = std::env::var("AGEND_LOG_MAX_BYTES")
            .ok()
            .and_then(|v| crate::logging::parse_size(&v))
            .unwrap_or(crate::logging::DEFAULT_MAX_BYTES);
        let removed = crate::logging::cleanup_oversize_logs(ctx.home, max_bytes);
        if removed > 0 {
            tracing::info!(
                removed,
                max_bytes,
                "log_rotation: pruned oversize daemon.log.* entries"
            );
        }
        crate::logging::update_daemon_log_symlink_unix(ctx.home);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = LogRotationHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }
}
