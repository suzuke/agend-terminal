//! #3682: the PTY-output broadcast fan-out and its subscriber liveness plumbing.
//!
//! Extracted from `agent/mod.rs` (which sits at its grandfathered size ceiling) so
//! the liveness-token fix could land without growing that file. See
//! `agent::broadcast::Subscription` for the tap contract.

use std::sync::{Arc, Weak};

/// #3682: one broadcast output tap — the per-subscriber `Sender` plus a liveness
/// token. The token's sole strong `Arc<()>` is held by the consumer (the
/// [`Subscription`] carrying the receiver, or the router's per-agent buffer), so
/// `Weak::strong_count() == 0` means that consumer is gone.
pub(crate) type SubscriberEntry = (crossbeam_channel::Sender<Vec<u8>>, Weak<()>);

/// #3682: a broadcast output tap handed to ONE consumer. Owns the receiver and
/// the only strong `Arc<()>` backing the matching [`SubscriberEntry`]'s `Weak`.
/// Dropping it lets the next broadcast OR the next `subscribe_with_dump` reclaim
/// the dead entry — even for a quiet agent that emits no PTY output.
pub struct Subscription {
    pub(crate) rx: crossbeam_channel::Receiver<Vec<u8>>,
    /// Held purely to anchor the matching `Weak` in `AgentCore::subscribers`.
    #[allow(dead_code)]
    pub(crate) live: Arc<()>,
}

impl Subscription {
    /// #3682: a standalone tap over a caller-built receiver, for tests that drive
    /// a consumer directly (no `AgentCore::subscribers` linkage). Production MUST
    /// go through `subscribe_with_dump`, the only path that links token ↔ `Weak`.
    #[cfg(test)]
    pub(crate) fn detached(rx: crossbeam_channel::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            live: Arc::new(()),
        }
    }
}

/// #3682: drop broadcast taps whose consumer is gone (`Weak` upgraded to nothing).
/// Called from the broadcast `retain` AND at `subscribe_with_dump` time, so a
/// connect/disconnect cycle reclaims the prior dead entry even when the agent
/// emits zero PTY output (the broadcast path never runs for a quiet agent).
pub(crate) fn retain_live_subscribers(subs: &mut Vec<SubscriberEntry>) {
    subs.retain(|(_, live)| live.strong_count() > 0);
}

/// Broadcast one PTY output chunk to all subscribers WITHOUT blocking.
///
/// The caller holds the agent's `core.lock()` (the broadcast is kept atomic
/// with `feed_with_fg` so a concurrent `subscribe_with_dump` can't interleave a
/// dump between process and broadcast). That makes blocking here lethal: a
/// blocking `send` on a full `bounded(1024)` subscriber channel would hold the
/// core lock forever and wedge every core-lock waiter — the main TUI
/// render/input thread, the supervisor, all of it. (Observed: two agents'
/// pty_read threads parked in a full-channel send while holding their core
/// locks; the TUI drains those very channels but was itself parked waiting for a
/// core lock — a deadlock cycle that froze the whole daemon.)
///
/// `try_send` never blocks. On `Full` the consumer is too far behind, so the
/// chunk is dropped (best-effort mirror; the consumer resyncs from the next
/// screen dump) and `dropped_chunks` is bumped + throttled-logged. On
/// `Disconnected` the subscriber is removed (the `retain` returns `false`), and
/// a tap whose liveness token is gone is dropped even when the send succeeded.
pub(crate) fn broadcast_pty_output(
    subscribers: &mut Vec<SubscriberEntry>,
    data: &[u8],
    dropped_chunks: &mut u64,
    agent: &str,
) {
    subscribers.retain(|(tx, live)| match tx.try_send(data.to_vec()) {
        Ok(()) => live.strong_count() > 0,
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            *dropped_chunks += 1;
            // Throttle: first drop, then powers-of-two, so a chronically-full
            // subscriber stays visible in the log without flooding it.
            if dropped_chunks.is_power_of_two() {
                tracing::warn!(
                    agent,
                    dropped_chunks = *dropped_chunks,
                    "pty broadcast: subscriber channel full — dropping output chunk (consumer \
                     stalled). Mirror is best-effort; the daemon is NOT blocked (was a freeze \
                     before this guard)."
                );
            }
            live.strong_count() > 0
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
    });
}
