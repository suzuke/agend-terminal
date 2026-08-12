//! #3230: durable Claude ChannelBridge self-kick acknowledgement watchdog.
//!
//! The bridge persists the accepted delivery before it can receive the
//! consumer's exact-ID `ack_start`. This per-tick scan replays that durable
//! state after daemon/bridge restarts and converts only an old, still-
//! ProtocolAccepted self-kick into one Ambiguous operator alert. It never
//! retries, reads screen state, or treats a hook/time correlation as a turn.

use super::{PerTickHandler, TickContext};

pub(crate) struct ClaudeSelfKickHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl ClaudeSelfKickHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for ClaudeSelfKickHandler {
    fn name(&self) -> &'static str {
        "claude_self_kick"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let Ok(fleet) =
            crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(ctx.home))
        else {
            return;
        };
        for name in fleet.instances.keys() {
            if crate::transport::mode_for_instance(ctx.home, name)
                != crate::transport::TransportMode::ChannelBridge
            {
                continue;
            }
            if let Err(error) =
                crate::transport::claude_channel::self_kick_watchdog_pass(ctx.home, name)
            {
                tracing::warn!(agent = %name, error = %error, "Claude self-kick watchdog scan failed");
            }
        }
    }
}
