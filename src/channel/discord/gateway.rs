use super::*;
use crate::channel::ChannelEvent;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// Gateway connection (#2562 P0)
// ---------------------------------------------------------------------------
//
// PR1-4 (2026-04-29) built the full outbound REST surface + gateway protocol
// PARSING layer (opcode/HELLO/IDENTIFY/READY/MESSAGE_CREATE mapping above),
// all fixture-tested — but never opened a live WebSocket to Discord's gateway.
// `DiscordChannel::new`'s own doc comment describes `event_rx` as "fed by the
// gateway reader task", but that task never existed. This section adds it,
// purely additively: it feeds the already-tested mapping functions above and
// changes none of them.

/// Translate a `twilight_gateway::Event` into our `ChannelEvent`, reusing the
/// existing (tested) protocol-mapping functions. Returns `None` for event
/// types we don't forward (Discord's gateway emits far more event types than
/// this adapter currently models — unmodeled ones are silently dropped, not
/// an error). This is the sole decision point for "which gateway events
/// become `ChannelEvent`s", kept as a pure function so it's unit-testable
/// without a live connection.
pub(crate) fn gateway_event_to_channel_event(
    event: twilight_gateway::Event,
    allowlist: &Option<Vec<i64>>,
) -> Option<ChannelEvent> {
    use twilight_gateway::Event;
    match event {
        Event::Ready(ready) => Some(map_ready_to_connected(&ready)),
        Event::MessageCreate(msg) => map_message_create_to_message_in(&msg, allowlist),
        Event::ChannelDelete(channel) => {
            Some(map_channel_delete_to_binding_revoked(channel.id.get()))
        }
        _ => None,
    }
}

/// Whether the shard's current state means the gateway connection loop
/// should give up (vs. let twilight's built-in reconnect keep working).
/// `twilight_gateway::Shard` reconnects and resumes sessions automatically
/// as long as `next_event` keeps being called — this only needs to catch
/// the ONE state that reconnecting can never fix: a fatal close (bad
/// token / invalid intents / etc., per twilight's `CloseCode::can_reconnect`).
/// Mirrors Telegram's `poll_supervisor::ConnectErrorClass::PermanentAuth`
/// stop behavior.
pub(crate) fn should_stop_gateway_loop(state: twilight_gateway::ShardState) -> bool {
    matches!(state, twilight_gateway::ShardState::FatallyClosed)
}

/// Start the Discord gateway connection: opens (and, via twilight's internal
/// reconnect/resume handling, maintains) the live WebSocket to Discord,
/// mapping real events into `tx` via [`gateway_event_to_channel_event`].
///
/// fire-and-forget (#10.5): this thread runs for the daemon's process
/// lifetime, mirroring `start_keepalive` above and
/// `channel::telegram::inbound::start_polling`'s existing rationale —
/// Telegram (the one complete channel) has no runtime shutdown capability
/// either, so adding one for Discord alone would be an asymmetric new
/// capability with no current consumer. No `JoinHandle` needed; process
/// exit cleans up the thread.
///
/// Runs on ITS OWN dedicated `current_thread` tokio runtime, deliberately
/// NOT `discord_runtime()` (reserved for the short-lived outbound
/// `block_on_value` calls in `send`/`edit`/`delete`/etc.) — this loop runs
/// forever, and sharing a single-threaded runtime with it would starve
/// every outbound call behind the permanently-running reader.
/// #2562 PR-3a: set each time the gateway connection thread stops (fatal
/// close, or its event receiver disappeared). `false` initially and while
/// the gateway is live — including while it's transiently reconnecting,
/// since `twilight_gateway::Shard` handles that internally (see
/// `should_stop_gateway_loop`'s doc comment) and the loop never reaches a
/// break point for a merely-transient disconnect. `spawn_gateway_supervisor`
/// (#2562 PR-3b) does a bounded number of automatic restarts (with
/// backoff) when this flips true; only once it gives up for good does an
/// operator need to fix the config / restart the daemon. Exposed so
/// status/MCP surfaces can show "Discord: dead" instead of requiring a
/// log grep.
pub(super) static GATEWAY_DEAD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the Discord gateway connection has permanently died — see
/// [`GATEWAY_DEAD`].
pub fn gateway_is_dead() -> bool {
    GATEWAY_DEAD.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn reset_gateway_dead_for_test() {
    GATEWAY_DEAD.store(false, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn start_gateway(
    token: String,
    intents: twilight_model::gateway::Intents,
    allowlist: Option<Vec<i64>>,
    tx: mpsc::Sender<ChannelEvent>,
) {
    // A restart (PR-3b) calling this again after a prior death should
    // clear the stale flag — the new attempt gets its own fair chance.
    GATEWAY_DEAD.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = std::thread::Builder::new()
        .name("discord-gateway".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("discord gateway: failed to build tokio runtime");
                GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            };
            rt.block_on(async move {
                use twilight_gateway::{EventTypeFlags, Shard, ShardId, StreamExt as _};
                let mut shard = Shard::new(ShardId::ONE, token, intents);
                loop {
                    match shard.next_event(EventTypeFlags::all()).await {
                        Some(Ok(event)) => {
                            if let Some(mapped) = gateway_event_to_channel_event(event, &allowlist)
                            {
                                if tx.send(mapped).is_err() {
                                    tracing::warn!(
                                        "discord gateway: event receiver dropped, stopping"
                                    );
                                    break;
                                }
                            }
                        }
                        Some(Err(source)) => {
                            tracing::warn!(error = %source, "discord gateway: receive error");
                        }
                        None => {
                            tracing::error!("discord gateway: shard stream ended");
                            break;
                        }
                    }
                    if should_stop_gateway_loop(shard.state()) {
                        tracing::error!(
                            state = ?shard.state(),
                            "discord gateway: fatal close, stopping (operator must fix config \
                             and restart)"
                        );
                        break;
                    }
                }
            });
            GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    {
        tracing::error!(error = %e, "failed to spawn discord gateway thread");
        GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// #2562 PR-3b: maximum number of restart attempts after `start_gateway`
/// dies, before giving up permanently. Bounded (unlike Telegram's
/// `poll_supervisor`, which retries most errors indefinitely) because
/// every condition that kills `start_gateway` is a config/auth problem
/// (see `should_stop_gateway_loop`'s doc comment) that won't self-resolve
/// by waiting — see `DISCORD-P2-RECONNECT-PRERESEARCH.md` §1. The cap
/// exists so a still-bad token/intents config doesn't hot-loop forever,
/// while still giving an operator who fixes the config WHILE the daemon
/// keeps running a chance at automatic recovery without a full restart.
pub(super) const GATEWAY_RESTART_MAX_ATTEMPTS: u32 = 3;

/// Backoff before restart attempt number `attempt` (1-indexed: the delay
/// before the FIRST restart, i.e. the second overall try). Mirrors
/// Telegram's `poll_supervisor::backoff_delay` shape (exponential, base
/// 5s, cap 60s) — same constants for consistency, not because the two
/// domains share an error taxonomy (they don't; see PRERESEARCH §2).
pub(super) fn discord_gateway_backoff_delay(attempt: u32) -> std::time::Duration {
    let exp = attempt.saturating_sub(1).min(20);
    let mult = 1u32 << exp;
    std::time::Duration::from_secs(5)
        .saturating_mul(mult)
        .min(std::time::Duration::from_secs(60))
}

/// Whether another restart attempt should be made, given how many restart
/// attempts have already happened (not counting the original attempt).
/// `false` once [`GATEWAY_RESTART_MAX_ATTEMPTS`] is reached.
pub(super) fn should_restart_gateway(attempts_so_far: u32) -> bool {
    attempts_so_far < GATEWAY_RESTART_MAX_ATTEMPTS
}

/// Supervises `start_gateway`: restarts it (up to
/// [`GATEWAY_RESTART_MAX_ATTEMPTS`] times, with backoff) if it dies. Polls
/// [`gateway_is_dead`] rather than holding a `JoinHandle` on
/// `start_gateway`'s inner thread, since `start_gateway` itself is
/// fire-and-forget by design (§10.5) and `GATEWAY_DEAD` already carries
/// exactly the "has it exited" signal this loop needs — no reason to widen
/// `start_gateway`'s contract just to duplicate that signal via a handle.
///
/// fire-and-forget (§10.5): mirrors every other Discord/Telegram
/// background thread — no `JoinHandle`, lives for the daemon's process
/// lifetime, process exit cleans it up.
pub(crate) fn spawn_gateway_supervisor(
    token: String,
    intents: twilight_model::gateway::Intents,
    allowlist: Option<Vec<i64>>,
    tx: mpsc::Sender<ChannelEvent>,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("discord-gateway-supervisor".into())
        .spawn(move || {
            let mut attempt: u32 = 0;
            loop {
                start_gateway(token.clone(), intents, allowlist.clone(), tx.clone());
                // start_gateway resets GATEWAY_DEAD to false synchronously
                // before it returns here (its own thread runs
                // independently) — poll until that thread's exit path
                // flips it back to true.
                while !gateway_is_dead() {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                if !should_restart_gateway(attempt) {
                    tracing::error!(
                        attempt,
                        "discord gateway: giving up after max restart attempts \
                         (operator must fix config and restart the daemon)"
                    );
                    break;
                }
                attempt += 1;
                let delay = discord_gateway_backoff_delay(attempt);
                tracing::warn!(
                    attempt,
                    delay_secs = delay.as_secs(),
                    "discord gateway: died, restarting after backoff"
                );
                std::thread::sleep(delay);
            }
        })
    {
        tracing::error!(error = %e, "failed to spawn discord gateway supervisor thread");
        GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
