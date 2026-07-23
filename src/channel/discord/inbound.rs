use super::*;
use crate::channel::{Channel, ChannelEvent};

// ---------------------------------------------------------------------------
// Inbound dispatcher (#2562 PR-1)
// ---------------------------------------------------------------------------

/// Drain-loop body: route one already-polled `ChannelEvent` to its target
/// agent's inbox. Only `MessageIn` results in an inject — other event kinds
/// (`Connected`, `BindingRevoked`, `ButtonClick`, ...) are intentionally
/// no-ops here; this dispatcher's scope is inbound message routing only.
///
/// Session-agnostic by design (#2562 P2 boundary): this function has no
/// notion of "which gateway connection" an event came from, so it needs no
/// changes when P2 adds gateway reconnect — it just keeps draining whatever
/// the queue has.
pub(crate) fn dispatch_channel_event(
    channel: &DiscordChannel,
    home: &std::path::Path,
    event: ChannelEvent,
) {
    let ChannelEvent::MessageIn {
        binding,
        from,
        payload,
        ts,
    } = event
    else {
        return;
    };
    let Some(discord_binding) = binding.downcast::<DiscordBindingPayload>() else {
        tracing::warn!(
            "discord inbound: MessageIn binding is not a DiscordBindingPayload, dropping"
        );
        return;
    };
    let instance = channel.resolve_instance_for_channel(discord_binding.channel_id);
    tracing::info!(
        instance = %instance,
        channel_id = discord_binding.channel_id,
        "discord inbound: routing message to instance"
    );
    let display_name = from.handle.as_deref().unwrap_or(&from.id);

    // #1352 parity with telegram/inbound.rs's short/long split: short
    // messages go straight to the PTY-notification layer (no persistence).
    // Long messages MUST be persisted first — `notify_agent_with_attachments`
    // truncates and points at "use the inbox MCP tool to read full message",
    // and under `AGEND_POINTER_ONLY_INJECT=1` EVERY message is pointer-only.
    // Skipping the enqueue here left the pointer with nothing in the inbox
    // to point at (silent-loss class, found in PR-1 review).
    //
    // Residual window (pre-existing, not introduced by this PR): a short
    // message still has no inbox fallback if the live PTY inject fails
    // (e.g. stale agent_state snapshot + genuinely dead daemon) — same
    // limitation Telegram's short-message path already has. Not fixed
    // here; would be a cross-channel behavior change.
    let is_short = payload.text.chars().count() < 200;
    let pointer_only = crate::inbox::notify::pointer_only_inject();
    if !is_short || pointer_only {
        let msg_obj = crate::inbox::InboxMessage {
            from: format!("user:{display_name}"),
            text: payload.text.clone(),
            timestamp: ts.to_rfc3339(),
            channel: Some(crate::channel::ChannelKind::Discord),
            ..Default::default()
        };
        persist_or_log!(
            crate::inbox::enqueue(home, &instance, msg_obj),
            "discord_dispatch_enqueue",
            instance
        );
    }
    crate::inbox::notify_agent_with_attachments(
        home,
        &instance,
        &crate::inbox::NotifySource::Channel(display_name, crate::channel::ChannelKind::Discord),
        &payload.text,
        &[],
    );
}

/// Start the inbound dispatcher: a plain `std::thread` poll loop, no tokio
/// runtime needed (`poll_event` is a synchronous, non-blocking `try_recv`;
/// unlike Telegram's `start_polling`, Discord's network I/O already lives on
/// its own thread in `start_gateway` — this loop only drains and routes).
///
/// fire-and-forget (§10.5): mirrors every other Discord/Telegram background
/// thread in this codebase (`start_gateway`, `start_keepalive`,
/// `telegram::inbound::start_polling`) — no `JoinHandle`, lives for the
/// daemon's process lifetime, process exit cleans it up. Telegram's own
/// dispatcher has no shutdown signal either (see PRERESEARCH §1e); this
/// doesn't introduce a new inconsistency.
pub(crate) fn spawn_inbound_dispatcher(
    channel: std::sync::Arc<DiscordChannel>,
    home: std::path::PathBuf,
) {
    std::thread::spawn(move || loop {
        if let Some(event) = channel.poll_event() {
            dispatch_channel_event(&channel, &home, event);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
}
