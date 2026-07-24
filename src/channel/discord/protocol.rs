use super::*;
use crate::channel::{BindingRef, ChannelEvent};

// ---------------------------------------------------------------------------
// Gateway frame parsing — maps raw JSON to typed payloads / events
// ---------------------------------------------------------------------------

/// Opcode extracted from a raw gateway JSON frame.
/// Used by the gateway reader to dispatch on frame type before
/// deserializing the inner `d` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GatewayFrame {
    pub(crate) op: u8,
}

/// Parse the opcode from a raw gateway JSON frame.
/// Returns `None` if the frame is not valid JSON or lacks an `op` field.
pub(crate) fn parse_gateway_opcode(raw: &str) -> Option<GatewayFrame> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let op = v.get("op")?.as_u64()? as u8;
    Some(GatewayFrame { op })
}

/// Parse a HELLO frame (opcode 10) and return the heartbeat interval in ms.
pub(crate) fn parse_hello_interval(raw: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let d = v.get("d")?;
    let hello: twilight_model::gateway::payload::incoming::Hello =
        serde_json::from_value(d.clone()).ok()?;
    Some(hello.heartbeat_interval)
}

/// Build the IDENTIFY payload our adapter sends to the gateway.
/// Returns the full JSON frame (op=2 + d={token, intents, properties}).
pub(crate) fn build_identify_payload(
    token: &str,
    intents: twilight_model::gateway::Intents,
) -> serde_json::Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents.bits(),
            "properties": {
                "os": std::env::consts::OS,
                "browser": "agend-terminal",
                "device": "agend-terminal"
            }
        }
    })
}

/// Returns `true` if the frame is a HEARTBEAT_ACK (opcode 11).
pub(crate) fn is_heartbeat_ack(raw: &str) -> bool {
    parse_gateway_opcode(raw).is_some_and(|f| f.op == 11)
}

/// Map a twilight `Ready` payload to `ChannelEvent::Connected`.
pub(crate) fn map_ready_to_connected(
    ready: &twilight_model::gateway::payload::incoming::Ready,
) -> ChannelEvent {
    ChannelEvent::Connected {
        kind: "discord".into(),
        who: ready.user.name.clone(),
    }
}

/// Map a twilight `Message` (from MESSAGE_CREATE dispatch) to
/// `ChannelEvent::MessageIn`, gated on the operator `user_allowlist`.
///
/// #bughunt-r3 #3: returns `None` (message dropped) when the author is not
/// authorised — the gate is baked into the mapper, NOT left to the (still
/// scaffold) dispatch loop, so no future wiring can emit an un-gated MessageIn.
/// Mirrors the telegram inbound allowlist gate (`telegram/inbound.rs`).
/// Fail-closed: `None` / empty / not-listed allowlist → dropped. Discord author
/// ids are u64 snowflakes; the allowlist is `i64` (matches `ChannelConfig`), so
/// an id that doesn't fit `i64` also fails closed.
pub(crate) fn map_message_create_to_message_in(
    msg: &twilight_model::channel::Message,
    allowlist: &Option<Vec<i64>>,
) -> Option<ChannelEvent> {
    use crate::channel::event::{MsgPayload, User};

    let author_id = msg.author.id.get();
    let allowed = i64::try_from(author_id)
        .ok()
        .is_some_and(|id| crate::channel::auth::is_authorized_recipient(allowlist, id));
    if !allowed {
        tracing::warn!(
            author = %msg.author.name,
            user_id = author_id,
            "discord message rejected by user_allowlist"
        );
        return None;
    }

    tracing::info!(
        author = %msg.author.name,
        user_id = author_id,
        channel_id = msg.channel_id.get(),
        "discord message accepted by user_allowlist"
    );

    Some(ChannelEvent::MessageIn {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{}", msg.channel_id)),
            DiscordBindingPayload {
                channel_id: msg.channel_id.get(),
            },
        ),
        from: User {
            id: msg.author.id.to_string(),
            handle: Some(msg.author.name.clone()),
        },
        payload: MsgPayload {
            text: msg.content.clone(),
        },
        ts: chrono::DateTime::parse_from_rfc3339(&msg.timestamp.iso_8601().to_string())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
    })
}

/// Map a twilight `Message` (from REST response) to `MsgRef`.
pub(crate) fn map_message_to_msg_ref(
    msg: &twilight_model::channel::Message,
) -> crate::channel::MsgRef {
    crate::channel::MsgRef {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{}", msg.channel_id)),
            DiscordBindingPayload {
                channel_id: msg.channel_id.get(),
            },
        ),
        id: msg.id.to_string(),
    }
}

/// Map a Discord CHANNEL_DELETE gateway event to `ChannelEvent::BindingRevoked`.
/// `channel_id` is the deleted channel's snowflake.
pub(crate) fn map_channel_delete_to_binding_revoked(channel_id: u64) -> ChannelEvent {
    ChannelEvent::BindingRevoked {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{channel_id}")),
            DiscordBindingPayload { channel_id },
        ),
        reason: crate::channel::event::RevokeReason::Deleted,
    }
}
