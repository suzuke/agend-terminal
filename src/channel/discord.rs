//! Discord adapter — behind the `discord` feature gate.
//!
//! The full outbound REST surface (send/edit/delete/create_binding/
//! remove_binding), gateway protocol parsing (HELLO/IDENTIFY/HEARTBEAT/READY/
//! MESSAGE_CREATE mapping), binding lifecycle, and capability matrix shipped
//! 2026-04-29 (PR1-4, #316-319). #2562 P0 adds the piece that was missing
//! since then: [`start_gateway`] actually opens the live WebSocket to
//! Discord's gateway (via `twilight_gateway::Shard`) and feeds real events
//! through the mapping functions above — see `DISCORD-COMPLETION-SPIKE.md`
//! for the full gap analysis. Bootstrap wiring (constructing a `DiscordChannel`
//! from `ChannelConfig::Discord` and calling `start_gateway`) is #2562 P1.

use crate::agent::AgentRegistry;
use crate::channel::{
    BindingRef, Channel, ChannelCapabilities, ChannelError, ChannelEvent, MarkdownDialect,
    MentionStyle, MsgRef, OutMsg, RateBudget,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// Binding payload
// ---------------------------------------------------------------------------

/// Discord-specific binding payload stored inside [`BindingRef`].
/// Holds the channel/thread snowflake that messages are sent to.
#[derive(Debug, Clone, Copy)]
pub struct DiscordBindingPayload {
    pub channel_id: u64,
}

/// Construct a [`BindingRef`] for the contract test harness.
/// Deterministic channel_id derived from the instance name.
#[cfg(test)]
pub(crate) fn discord_make_binding(name: &str) -> BindingRef {
    let id = 1_000_000 + name.bytes().map(|b| b as u64).sum::<u64>();
    BindingRef::new(
        "discord",
        Some(format!("DC#{id}")),
        DiscordBindingPayload { channel_id: id },
    )
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Mutable state for the Discord adapter.
pub struct DiscordState {
    /// Instance → channel_id binding registry.
    pub instance_to_channel: HashMap<String, u64>,
    /// Reverse: channel_id → instance name.
    pub channel_to_instance: HashMap<u64, String>,
    /// Submit key per instance (PTY metadata, unused by Discord but
    /// stored to satisfy the `record_binding` contract).
    pub submit_keys: HashMap<String, String>,
    /// Agent registry wired post-bootstrap.
    pub registry: Option<AgentRegistry>,
    /// User allowlist (Discord user snowflakes). `None` = fail-closed.
    pub user_allowlist: Option<Vec<i64>>,
    /// twilight HTTP client for REST API calls. `None` only in test
    /// harness — production `new` always populates it.
    pub http_client: Option<std::sync::Arc<twilight_http::Client>>,
    /// Guild (server) snowflake for binding creation.
    pub guild_id: u64,
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// Discord adapter implementing the `Channel` trait.
pub struct DiscordChannel {
    state: Mutex<DiscordState>,
    caps: ChannelCapabilities,
    /// Receiver end of the unbounded event channel (`std::sync::mpsc::
    /// channel`, not `sync_channel`). The gateway reader task pushes
    /// `ChannelEvent`s here; `poll_event` drains them.
    event_rx: Mutex<mpsc::Receiver<ChannelEvent>>,
}

impl DiscordChannel {
    /// Production constructor. `event_rx` is the receiving end of the
    /// mpsc channel fed by the gateway reader task.
    pub fn new(
        event_rx: mpsc::Receiver<ChannelEvent>,
        user_allowlist: Option<Vec<i64>>,
        http_client: std::sync::Arc<twilight_http::Client>,
        guild_id: u64,
    ) -> Self {
        Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist,
                http_client: Some(http_client),
                guild_id,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(event_rx),
        }
    }

    /// Test-only constructor that returns both the channel and the
    /// sender end so tests can inject events.
    #[cfg(test)]
    pub(crate) fn new_for_test() -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: None,
                http_client: None,
                guild_id: 0,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }

    /// Test-only constructor with a configured allowlist so
    /// `outbound_authorized()` returns `true`.
    #[cfg(test)]
    pub(crate) fn new_for_test_authorized() -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: Some(vec![1]),
                http_client: None,
                guild_id: 0,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }

    /// Test-only constructor with a custom twilight HTTP client (for
    /// mock-server tests that exercise the real send path).
    #[cfg(test)]
    pub(crate) fn new_for_test_with_http(
        http: std::sync::Arc<twilight_http::Client>,
    ) -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: Some(vec![1]),
                http_client: Some(http),
                guild_id: 987654321098765432,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }

    /// Resolve which fleet instance owns `channel_id`, via the same
    /// `channel_to_instance` reverse lookup Telegram's `resolve_topic` uses
    /// for `topic_id` (#2562 PR-1). Miss (channel_id not bound to any
    /// instance) falls back to `"general"` + a warn log — mirrors
    /// `telegram/inbound.rs`'s topic-miss fallback semantics.
    pub(crate) fn resolve_instance_for_channel(&self, channel_id: u64) -> String {
        let instance = self
            .state
            .lock()
            .channel_to_instance
            .get(&channel_id)
            .cloned();
        instance.unwrap_or_else(|| {
            tracing::warn!(
                channel_id,
                "discord inbound: no instance bound to this channel, falling back to 'general'"
            );
            "general".to_string()
        })
    }
}

/// Build the Discord capability matrix (pinned by S5 analysis).
fn discord_caps() -> ChannelCapabilities {
    ChannelCapabilities {
        // Transport
        emits_deletion_events: true,
        threads: true,
        buttons: false, // components deferred
        attachments: true,
        markdown: MarkdownDialect::DiscordMd,
        max_msg_bytes: 2000,
        rate_budget: RateBudget {
            per_second: 5,
            per_minute: 50,
        },
        // UX
        react: false, // M3: not implemented yet (returns NotSupported)
        edit: true,
        typing_indicator: true,
        receives_edit_events: true,
        mention_parsing_hint: MentionStyle::AtSnowflake,
        bot_sees_read_receipts: false,
        has_native_multi_thread_view: None,
        ephemeral: false,
    }
}

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
static GATEWAY_DEAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
const GATEWAY_RESTART_MAX_ATTEMPTS: u32 = 3;

/// Backoff before restart attempt number `attempt` (1-indexed: the delay
/// before the FIRST restart, i.e. the second overall try). Mirrors
/// Telegram's `poll_supervisor::backoff_delay` shape (exponential, base
/// 5s, cap 60s) — same constants for consistency, not because the two
/// domains share an error taxonomy (they don't; see PRERESEARCH §2).
fn discord_gateway_backoff_delay(attempt: u32) -> std::time::Duration {
    let exp = attempt.saturating_sub(1).min(20);
    let mult = 1u32 << exp;
    std::time::Duration::from_secs(5)
        .saturating_mul(mult)
        .min(std::time::Duration::from_secs(60))
}

/// Whether another restart attempt should be made, given how many restart
/// attempts have already happened (not counting the original attempt).
/// `false` once [`GATEWAY_RESTART_MAX_ATTEMPTS`] is reached.
fn should_restart_gateway(attempts_so_far: u32) -> bool {
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

// ---------------------------------------------------------------------------
// Bootstrap (#2562 P1)
// ---------------------------------------------------------------------------

/// Intents this adapter requests — matches the shape already pinned by
/// `discord_gateway_identify_shape_matches_spec` (PR1, 2026-04-29): guild
/// membership + message content, the minimum needed to receive
/// `MESSAGE_CREATE` for a bound channel.
fn discord_intents() -> twilight_model::gateway::Intents {
    twilight_model::gateway::Intents::GUILDS
        | twilight_model::gateway::Intents::GUILD_MESSAGES
        | twilight_model::gateway::Intents::MESSAGE_CONTENT
}

/// Construct the Discord outbound HTTP client.
///
/// Must run through [`discord_runtime()`] (via the local [`block_on_value`]),
/// not on a bare thread. `twilight_http::Client::new` builds a
/// `twilight_http_ratelimiting::RateLimiter` whose constructor does
/// `tokio::spawn(actor::runner(..))` internally — called with no active
/// Tokio runtime in scope (as `bootstrap::discord_init::init` does, since it
/// runs `init_from_config` on a plain `std::thread`) that panics with
/// "there is no reactor running, must be called from the context of a
/// Tokio 1.x runtime" (twilight-http-ratelimiting 0.17.1 src/lib.rs:279 —
/// found via #2562 P3 isolated-smoke-home boot, 2026-07-03: the daemon's
/// real bootstrap path never got an end-to-end run before, only
/// `#[tokio::test]`-shielded unit tests, which supply an ambient runtime
/// this call site doesn't have on real boot). Routing through the same
/// `discord_runtime()` the outbound `send`/`edit`/`delete` calls already use
/// keeps the Client's rate-limiter actor task on one runtime for its whole
/// lifetime instead of init and outbound each managing their own.
fn build_http_client(token: String) -> twilight_http::Client {
    block_on_value(async move { twilight_http::Client::new(token) })
}

/// Initialize Discord from fleet config: on `Some`, the gateway connection
/// is ALREADY running (via [`start_gateway`]) and the returned
/// [`DiscordChannel`] is ready to register. Returns `None` when Discord
/// isn't configured or the bot token env var isn't set — mirrors
/// `channel::telegram::init_from_config`'s not-configured contract so
/// callers can treat both channels symmetrically.
pub fn init_from_config(config: &crate::fleet::FleetConfig) -> Option<DiscordChannel> {
    let (bot_token_env, guild_id, user_allowlist) = match config.channel.as_ref()? {
        crate::fleet::ChannelConfig::Telegram { .. } => return None,
        crate::fleet::ChannelConfig::Discord {
            bot_token_env,
            guild_id,
            user_allowlist,
        } => (bot_token_env, *guild_id, user_allowlist.clone()),
    };
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            tracing::info!(env = %bot_token_env, "discord bot token env not set, skipping");
            return None;
        }
    };
    if user_allowlist.is_none() {
        tracing::warn!(
            "discord channel.user_allowlist is not set — fail-closed default: ALL inbound \
             messages are dropped. Set `user_allowlist: [123456789012345678]` in fleet.yaml \
             to enable the channel."
        );
    }

    // twilight_gateway::Config::new and twilight_http::Client::new each
    // add the `Bot ` prefix themselves if it's missing (checked, so this
    // stays correct even if an operator pastes an already-prefixed token)
    // — pass the raw token to both, per their documented contract.
    let (tx, rx) = mpsc::channel();
    spawn_gateway_supervisor(token.clone(), discord_intents(), user_allowlist.clone(), tx);
    let http_client = std::sync::Arc::new(build_http_client(token));
    Some(DiscordChannel::new(
        rx,
        user_allowlist,
        http_client,
        guild_id,
    ))
}

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

// ---------------------------------------------------------------------------
// Outbound request body construction
// ---------------------------------------------------------------------------

/// Build the JSON body for `POST /channels/{id}/messages` per Discord spec.
/// Ref: https://discord.com/developers/docs/resources/message#create-message-jsonform-params
///
/// This is the canonical shape our adapter transmits. The test suite
/// asserts this against the spec-quoted example (§3.5.10 outbound
/// request boundary).
pub(crate) fn build_create_message_body(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": text })
}

// ---------------------------------------------------------------------------
// Auto-archive keepalive
// ---------------------------------------------------------------------------

/// Keepalive interval for Discord thread auto-archive prevention.
/// Discord's shortest auto-archive is 1 hour; 30 min refresh is safe.
pub(crate) const KEEPALIVE_INTERVAL_SECS: u64 = 30 * 60;

/// Start a background thread that periodically PATCHes all bound
/// Discord threads to prevent auto-archive.
pub(crate) fn start_keepalive(state: std::sync::Arc<Mutex<DiscordState>>) {
    // fire-and-forget: keepalive thread runs for the adapter's lifetime.
    // Stops when the daemon process exits. No JoinHandle needed — the
    // thread is purely side-effecting (PATCH calls) with no return value.
    if let Err(e) = std::thread::Builder::new()
        .name("discord-keepalive".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("keepalive tokio runtime");
            loop {
                // CR-2026-06-14: capture the work snapshot FIRST so the very
                // first iteration issues its keepalive PATCHes promptly; the
                // interval sleep moves to the END of the loop body (below).
                // Previously the loop slept a full KEEPALIVE_INTERVAL_SECS (30
                // min) BEFORE the first refresh, so freshly-bound threads idled
                // toward the 60-min auto-archive boundary, eroding the 30-vs-60-
                // min margin the design assumes. `None` (no HTTP client yet)
                // falls through to the end-sleep — re-checking next interval
                // without busy-spinning (the old `continue` relied on the
                // loop-top sleep that no longer exists).
                let snapshot = {
                    let s = state.lock();
                    s.http_client.clone().map(|http| {
                        let ids: Vec<u64> = s.instance_to_channel.values().copied().collect();
                        (http, ids)
                    })
                };
                if let Some((http, channel_ids)) = snapshot {
                    for cid in channel_ids {
                        let id = twilight_model::id::Id::new(cid);
                        let http = http.clone();
                        rt.block_on(async {
                            if let Err(e) = http.update_thread(id).archived(false).await {
                                tracing::debug!(channel_id = cid, %e, "keepalive PATCH failed");
                            }
                        });
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
            }
        })
    {
        tracing::error!(error = %e, "failed to spawn keepalive thread");
    }
}

/// Send a single keepalive PATCH for a specific channel. Extracted for
/// testability — the production `start_keepalive` loop calls this per
/// binding; tests call it directly against a mock server.
pub(crate) fn send_keepalive_patch(
    http: &twilight_http::Client,
    channel_id: u64,
) -> anyhow::Result<()> {
    let id = twilight_model::id::Id::new(channel_id);
    block_on_value(async { http.update_thread(id).archived(false).await })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Channel trait impl
// ---------------------------------------------------------------------------

/// Shared tokio runtime for Discord sync→async calls.
fn discord_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| crate::shared_async::build_current_thread_runtime("discord tokio runtime"))
}

/// #1476: run a Discord async call to completion, safe even when already inside
/// a tokio runtime.
///
/// #1642: delegates to the shared [`crate::channel::shared_async::block_on_value`]
/// helper (deduped from the byte-identical telegram/discord copies — discord had
/// inherited telegram's nested-runtime panic AND its fix). See that helper for
/// the `current_thread` nested-runtime guard rationale.
fn block_on_value<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    crate::channel::shared_async::block_on_value(discord_runtime(), "discord", fut)
}

impl crate::channel::Channel for DiscordChannel {
    fn kind(&self) -> &'static str {
        "discord"
    }

    fn caps(&self) -> &ChannelCapabilities {
        &self.caps
    }

    fn poll_event(&self) -> Option<ChannelEvent> {
        self.event_rx.lock().try_recv().ok()
    }

    fn send(&self, binding: &BindingRef, msg: OutMsg) -> anyhow::Result<MsgRef> {
        let payload = binding
            .downcast::<DiscordBindingPayload>()
            .ok_or_else(|| anyhow::anyhow!("non-discord binding passed to send"))?;
        if msg.text.is_empty() {
            anyhow::bail!("OutMsg has no text (attachment-only sends deferred to PR3)");
        }
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id = twilight_model::id::Id::new(payload.channel_id);
        let text = msg.text;
        let cp = *payload;
        block_on_value(async {
            let response = http.create_message(channel_id).content(&text).await?;
            let sent = response.model().await?;
            Ok(MsgRef {
                binding: BindingRef::new("discord", Some(format!("DC#{}", cp.channel_id)), cp),
                id: sent.id.to_string(),
            })
        })
    }

    fn edit(&self, msg: &MsgRef, payload: OutMsg) -> anyhow::Result<()> {
        if payload.text.is_empty() {
            anyhow::bail!("OutMsg.text empty — Discord editMessage requires non-empty text");
        }
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id: u64 = msg
            .binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .ok_or_else(|| anyhow::anyhow!("non-discord binding in MsgRef"))?;
        let mid: u64 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid discord message_id: {}", msg.id))?;
        let text = payload.text;
        block_on_value(async {
            http.update_message(
                twilight_model::id::Id::new(channel_id),
                twilight_model::id::Id::new(mid),
            )
            .content(Some(&text))
            .await?;
            Ok(())
        })
    }

    fn delete(&self, msg: &MsgRef) -> anyhow::Result<()> {
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id: u64 = msg
            .binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .ok_or_else(|| anyhow::anyhow!("non-discord binding in MsgRef"))?;
        let mid: u64 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid discord message_id: {}", msg.id))?;
        block_on_value(async {
            http.delete_message(
                twilight_model::id::Id::new(channel_id),
                twilight_model::id::Id::new(mid),
            )
            .await?;
            Ok(())
        })
    }

    fn create_binding(
        &self,
        name: &str,
        opts: crate::channel::BindingOpts,
    ) -> anyhow::Result<BindingRef> {
        let (http, guild_id) = {
            let s = self.state.lock();
            let http = s
                .http_client
                .clone()
                .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
            (http, s.guild_id)
        };
        let display_name = opts.display_name.as_deref().unwrap_or(name);
        let parent_id = opts
            .extra
            .get("category_id")
            .and_then(|v| v.parse::<u64>().ok());
        let gid = twilight_model::id::Id::new(guild_id);
        block_on_value(async {
            let mut req = http.create_guild_channel(gid, display_name);
            if let Some(pid) = parent_id {
                req = req.parent_id(twilight_model::id::Id::new(pid));
            }
            let response = req.await?;
            let channel = response.model().await?;
            let cid = channel.id.get();
            Ok(BindingRef::new(
                "discord",
                Some(format!("DC#{cid}")),
                DiscordBindingPayload { channel_id: cid },
            ))
        })
    }

    fn remove_binding(&self, binding: &BindingRef) -> anyhow::Result<()> {
        let payload = binding
            .downcast::<DiscordBindingPayload>()
            .ok_or_else(|| anyhow::anyhow!("non-discord binding passed to remove_binding"))?;
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let cid = twilight_model::id::Id::new(payload.channel_id);
        block_on_value(async {
            http.delete_channel(cid).await?;
            Ok(())
        })
    }

    fn has_binding(&self, instance: &str) -> bool {
        self.state.lock().instance_to_channel.contains_key(instance)
    }

    fn record_binding(&self, instance: &str, binding: BindingRef, submit_key: String) {
        let Some(payload) = binding.downcast::<DiscordBindingPayload>() else {
            tracing::warn!(
                kind = binding.kind(),
                instance,
                "record_binding received non-discord binding — dropping"
            );
            return;
        };
        let cid = payload.channel_id;
        let mut s = self.state.lock();
        s.instance_to_channel.insert(instance.to_string(), cid);
        s.channel_to_instance.insert(cid, instance.to_string());
        s.submit_keys.insert(instance.to_string(), submit_key);
    }

    fn take_binding(&self, instance: &str) -> Option<BindingRef> {
        let mut s = self.state.lock();
        let cid = s.instance_to_channel.remove(instance)?;
        s.channel_to_instance.remove(&cid);
        s.submit_keys.remove(instance);
        drop(s);
        Some(BindingRef::new(
            "discord",
            Some(format!("DC#{cid}")),
            DiscordBindingPayload { channel_id: cid },
        ))
    }

    fn attach_registry(&self, registry: AgentRegistry) {
        self.state.lock().registry = Some(registry);
    }

    fn outbound_authorized(&self) -> bool {
        crate::channel::auth::is_outbound_authorized(&self.state.lock().user_allowlist)
    }

    fn create_topic(
        &self,
        name: &str,
    ) -> std::result::Result<crate::channel::TopicRef, ChannelError> {
        let binding = self
            .create_binding(name, crate::channel::BindingOpts::default())
            .map_err(ChannelError::Other)?;
        let cid = binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .unwrap_or(0);
        Ok(crate::channel::TopicRef {
            id: cid.to_string(),
            channel_kind: crate::channel::ChannelKind::Discord,
        })
    }

    fn notify(
        &self,
        instance: &str,
        _severity: crate::channel::NotifySeverity,
        message: &str,
        _silent: bool, // Discord has no per-message notification suppression
    ) -> std::result::Result<(), ChannelError> {
        let cid = self.state.lock().instance_to_channel.get(instance).copied();
        let cid = cid.ok_or_else(|| {
            ChannelError::Other(anyhow::anyhow!("no discord binding for '{instance}'"))
        })?;
        let binding = BindingRef::new(
            "discord",
            Some(format!("DC#{cid}")),
            DiscordBindingPayload { channel_id: cid },
        );
        self.send(&binding, OutMsg::text(message))
            .map_err(ChannelError::Other)?;
        Ok(())
    }

    fn send_from_agent(
        &self,
        agent: &str,
        op: crate::channel::AgentOutboundOp,
    ) -> std::result::Result<MsgRef, ChannelError> {
        // Step 1: adapter-level allowlist gate (PR #216 contract).
        if !self.outbound_authorized() {
            return Err(ChannelError::Other(anyhow::anyhow!(
                "outbound disabled — channel.user_allowlist not configured"
            )));
        }

        // Step 2: dispatch.
        match op {
            crate::channel::AgentOutboundOp::Reply { text } => {
                let cid = self.state.lock().instance_to_channel.get(agent).copied();
                let cid = cid.ok_or_else(|| {
                    ChannelError::Other(anyhow::anyhow!("no discord binding for '{agent}'"))
                })?;
                let binding = BindingRef::new(
                    "discord",
                    Some(format!("DC#{cid}")),
                    DiscordBindingPayload { channel_id: cid },
                );
                self.send(&binding, OutMsg::text(text))
                    .map_err(ChannelError::Other)
            }
            crate::channel::AgentOutboundOp::Edit {
                message_id,
                new_text,
            } => {
                let cid = self.state.lock().instance_to_channel.get(agent).copied();
                let cid = cid.ok_or_else(|| {
                    ChannelError::Other(anyhow::anyhow!("no discord binding for '{agent}'"))
                })?;
                let msg_ref = MsgRef {
                    binding: BindingRef::new(
                        "discord",
                        Some(format!("DC#{cid}")),
                        DiscordBindingPayload { channel_id: cid },
                    ),
                    id: message_id.clone(),
                };
                self.edit(&msg_ref, OutMsg::text(new_text))
                    .map_err(ChannelError::Other)?;
                Ok(MsgRef {
                    binding: BindingRef::new(
                        "discord",
                        None,
                        DiscordBindingPayload { channel_id: cid },
                    ),
                    id: message_id,
                })
            }
            _ => Err(ChannelError::NotSupported(
                "React/InjectProvenance deferred to TIER-C".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::channel::ChannelEvent;
    use serial_test::serial;

    /// §3.5.10 wire-format fixture: Discord Gateway READY payload
    /// (tests/fixtures/discord-gateway-ready.json) is deserialized via
    /// twilight-model and mapped to `ChannelEvent::Connected`.
    ///
    /// §3.5.11 test-first: this test was committed RED before the
    /// implementation existed. The GREEN commit adds `map_ready_to_connected`.
    #[test]
    fn discord_gateway_ready_emits_connected_event() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-ready.json");
        let frame: serde_json::Value =
            serde_json::from_str(fixture).expect("fixture must parse as JSON");
        let d = frame.get("d").expect("fixture must have 'd' field");
        let ready: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(d.clone()).expect("'d' must parse as Ready");

        let event = super::map_ready_to_connected(&ready);

        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "agend-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }
    }

    // ── #2562 P0: gateway_event_to_channel_event / should_stop_gateway_loop ──

    fn ready_fixture() -> twilight_model::gateway::payload::incoming::Ready {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-ready.json");
        let frame: serde_json::Value =
            serde_json::from_str(fixture).expect("fixture must parse as JSON");
        let d = frame.get("d").expect("fixture must have 'd' field");
        serde_json::from_value(d.clone()).expect("'d' must parse as Ready")
    }

    fn message_create_fixture() -> twilight_gateway::Event {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-message-create.json");
        let frame: serde_json::Value =
            serde_json::from_str(fixture).expect("fixture must parse as JSON");
        let d = frame.get("d").expect("fixture must have 'd' field");
        let msg: twilight_model::channel::Message =
            serde_json::from_value(d.clone()).expect("'d' must parse as Message");
        twilight_gateway::Event::MessageCreate(Box::new(
            twilight_model::gateway::payload::incoming::MessageCreate(msg),
        ))
    }

    /// `Event::Ready` reaches `gateway_event_to_channel_event` and comes out
    /// as `Connected` — proves the gateway-event dispatch wiring, not just
    /// the already-tested `map_ready_to_connected` in isolation.
    #[test]
    fn gateway_event_to_channel_event_ready_is_connected() {
        let event = twilight_gateway::Event::Ready(ready_fixture());
        let result = super::gateway_event_to_channel_event(event, &None);
        match result {
            Some(ChannelEvent::Connected { kind, who }) => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "agend-bot");
            }
            other => panic!("expected Some(Connected), got: {other:?}"),
        }
    }

    /// `Event::MessageCreate` with an allowlisted author reaches
    /// `gateway_event_to_channel_event` and comes out as `MessageIn`.
    #[test]
    fn gateway_event_to_channel_event_message_create_allowlisted_is_message_in() {
        let event = message_create_fixture();
        let allowlist = Some(vec![82198898841029460_i64]);
        let result = super::gateway_event_to_channel_event(event, &allowlist);
        assert!(
            matches!(result, Some(ChannelEvent::MessageIn { .. })),
            "expected Some(MessageIn), got: {result:?}"
        );
    }

    /// `Event::MessageCreate` with a non-allowlisted author is dropped —
    /// the allowlist gate must survive being routed through the gateway
    /// dispatcher, not just the underlying mapper.
    #[test]
    fn gateway_event_to_channel_event_message_create_not_allowlisted_is_dropped() {
        let event = message_create_fixture();
        let allowlist = Some(vec![999_i64]);
        let result = super::gateway_event_to_channel_event(event, &allowlist);
        assert!(result.is_none(), "expected None, got: {result:?}");
    }

    /// `Event::ChannelDelete` reaches `gateway_event_to_channel_event` and
    /// comes out as `BindingRevoked`.
    #[test]
    fn gateway_event_to_channel_event_channel_delete_is_binding_revoked() {
        let channel: twilight_model::channel::Channel =
            serde_json::from_value(serde_json::json!({"id": "223456789012345678", "type": 0}))
                .expect("minimal channel object must parse");
        let event = twilight_gateway::Event::ChannelDelete(Box::new(
            twilight_model::gateway::payload::incoming::ChannelDelete(channel),
        ));
        let result = super::gateway_event_to_channel_event(event, &None);
        match result {
            Some(ChannelEvent::BindingRevoked { binding, .. }) => {
                assert_eq!(binding.kind(), "discord");
            }
            other => panic!("expected Some(BindingRevoked), got: {other:?}"),
        }
    }

    /// Event types this adapter doesn't model (e.g. a bare heartbeat ack)
    /// are silently dropped, not an error — the dispatcher only forwards
    /// what it explicitly recognizes.
    #[test]
    fn gateway_event_to_channel_event_unmodeled_event_is_none() {
        let event = twilight_gateway::Event::GatewayHeartbeatAck;
        let result = super::gateway_event_to_channel_event(event, &None);
        assert!(result.is_none(), "expected None, got: {result:?}");
    }

    /// The one shard state that must stop the reader loop: a fatal close
    /// (bad token, invalid intents, etc.) that twilight's own reconnect
    /// logic cannot recover from.
    #[test]
    fn should_stop_gateway_loop_stops_on_fatally_closed() {
        assert!(super::should_stop_gateway_loop(
            twilight_gateway::ShardState::FatallyClosed
        ));
    }

    /// Every other shard state is something twilight will keep working on
    /// internally (reconnecting/resuming) — the loop must NOT give up.
    #[test]
    fn should_stop_gateway_loop_continues_on_recoverable_states() {
        assert!(!super::should_stop_gateway_loop(
            twilight_gateway::ShardState::Active
        ));
        assert!(!super::should_stop_gateway_loop(
            twilight_gateway::ShardState::Disconnected {
                reconnect_attempts: 3
            }
        ));
        assert!(!super::should_stop_gateway_loop(
            twilight_gateway::ShardState::Identifying
        ));
    }

    /// #2562 PR-3a: `gateway_is_dead()` reflects whatever `GATEWAY_DEAD`
    /// was last set to, and `reset_gateway_dead_for_test()` clears it back.
    /// `#[serial]` because the flag is a process-wide static (mirrors
    /// `daemon::mod::SHUTDOWN_REASON`'s shape) — parallel tests touching it
    /// would race. `start_gateway` itself sets this on its real exit paths
    /// (fatal close / receiver dropped / spawn failure); those paths need a
    /// live gateway attempt to reach, so this test exercises the static
    /// directly rather than driving a real connection.
    #[test]
    #[serial]
    fn gateway_is_dead_reflects_death_state() {
        super::reset_gateway_dead_for_test();
        assert!(!super::gateway_is_dead(), "must start alive after reset");

        super::GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(super::gateway_is_dead(), "must reflect a marked death");

        super::reset_gateway_dead_for_test();
        assert!(
            !super::gateway_is_dead(),
            "reset must clear it back to alive"
        );
    }

    /// #2562 PR-3b: exponential backoff, base 5s, cap 60s — same shape and
    /// same expected sequence as Telegram's `poll_supervisor::backoff_delay`
    /// test, confirming the constants chosen for consistency actually land
    /// on the same numbers.
    #[test]
    fn discord_gateway_backoff_delay_follows_expected_sequence() {
        let expected = [5u64, 10, 20, 40, 60, 60, 60];
        for (i, &secs) in expected.iter().enumerate() {
            let attempt = (i + 1) as u32;
            assert_eq!(
                super::discord_gateway_backoff_delay(attempt),
                std::time::Duration::from_secs(secs),
                "attempt {attempt} backoff mismatch"
            );
        }
        // Never panics / overflows for a pathologically large attempt
        // count — same saturating-exponent-cap shape as Telegram's
        // `poll_supervisor::backoff_delay`, same assertion.
        assert_eq!(
            super::discord_gateway_backoff_delay(u32::MAX),
            std::time::Duration::from_secs(60)
        );
    }

    /// #2562 PR-3b: restart attempts are allowed strictly below the cap,
    /// and refused at and beyond it — the supervisor must give up for good
    /// rather than hot-loop a still-bad config forever.
    #[test]
    fn should_restart_gateway_caps_at_max_attempts() {
        for n in 0..super::GATEWAY_RESTART_MAX_ATTEMPTS {
            assert!(
                super::should_restart_gateway(n),
                "attempt {n} should still be allowed to retry"
            );
        }
        assert!(!super::should_restart_gateway(
            super::GATEWAY_RESTART_MAX_ATTEMPTS
        ));
        assert!(!super::should_restart_gateway(
            super::GATEWAY_RESTART_MAX_ATTEMPTS + 5
        ));
    }

    /// Contract test: DiscordChannel satisfies the registry-side
    /// contract from `src/channel/contract.rs`.
    #[test]
    fn discord_channel_satisfies_contract() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        crate::channel::contract::run_registry_contract(ch, super::discord_make_binding);
    }

    /// Caps snapshot: pin the Discord capability matrix so reviewers
    /// can diff against the S5 analysis.
    #[test]
    fn discord_caps_match_s5_analysis() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        let caps = crate::channel::Channel::caps(&ch);

        assert!(caps.emits_deletion_events);
        assert!(caps.threads);
        assert!(caps.attachments);
        // M3: react support is `false` per production `discord_caps()` —
        // returns NotSupported until implemented. Sprint 54 P2-8b: test
        // updated to reflect production reality (was: stale aspirational
        // `assert!(caps.react)`).
        assert!(!caps.react);
        assert!(caps.edit);
        assert!(caps.typing_indicator);
        assert!(caps.receives_edit_events);
        assert_eq!(caps.max_msg_bytes, 2000);
        assert_eq!(caps.markdown, crate::channel::MarkdownDialect::DiscordMd);
        assert_eq!(
            caps.mention_parsing_hint,
            crate::channel::MentionStyle::AtSnowflake
        );
        assert!(!caps.bot_sees_read_receipts);
        assert!(caps.has_native_multi_thread_view.is_none());
        assert!(!caps.ephemeral);
    }

    /// poll_event drains the internal mpsc channel.
    #[test]
    fn poll_event_drains_mpsc() {
        let (ch, tx) = super::DiscordChannel::new_for_test();
        assert!(crate::channel::Channel::poll_event(&ch).is_none());

        tx.send(ChannelEvent::Connected {
            kind: "discord".into(),
            who: "test-bot".into(),
        })
        .expect("send");

        let event = crate::channel::Channel::poll_event(&ch).expect("should have event");
        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "test-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }

        assert!(crate::channel::Channel::poll_event(&ch).is_none());
    }

    // ── §3.5.10 expanded gateway handshake fixture tests ─────────────
    //
    // F1 fix: cover the full HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK
    // → READY sequence using Discord API spec payloads.
    //
    // §3.5.11 r3 empirical-revert exemption: impl already exists from
    // GREEN commit; tests depend on impl-provided fns. Reviewer can
    // revert impl to verify test failure.

    /// HELLO (opcode 10): server sends heartbeat_interval after WS connect.
    /// Fixture: tests/fixtures/discord-gateway-hello.json (Discord API spec).
    #[test]
    fn discord_gateway_hello_parsed_correctly() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-hello.json");

        // Opcode must be 10 (Hello).
        let frame = super::parse_gateway_opcode(fixture).expect("must parse");
        assert_eq!(frame.op, 10, "HELLO opcode must be 10");

        // heartbeat_interval must be extractable.
        let interval = super::parse_hello_interval(fixture).expect("must parse interval");
        assert_eq!(interval, 41250, "fixture heartbeat_interval");
    }

    /// IDENTIFY (opcode 2): client sends token + intents after receiving HELLO.
    /// Asserts the frame our adapter builds matches Discord spec shape.
    #[test]
    fn discord_gateway_identify_shape_matches_spec() {
        let intents = twilight_model::gateway::Intents::GUILDS
            | twilight_model::gateway::Intents::GUILD_MESSAGES
            | twilight_model::gateway::Intents::MESSAGE_CONTENT;

        let frame = super::build_identify_payload("Bot test-token-redacted", intents);

        // op must be 2
        assert_eq!(frame["op"], 2, "IDENTIFY opcode must be 2");

        // d.token present
        assert_eq!(frame["d"]["token"], "Bot test-token-redacted");

        // d.intents is a numeric bitfield
        assert!(frame["d"]["intents"].is_u64(), "intents must be numeric");

        // d.properties has required fields per Discord spec
        let props = &frame["d"]["properties"];
        assert!(props["os"].is_string(), "properties.os required");
        assert_eq!(props["browser"], "agend-terminal");
        assert_eq!(props["device"], "agend-terminal");
    }

    /// HEARTBEAT_ACK (opcode 11): server acknowledges client heartbeat.
    /// Fixture: tests/fixtures/discord-gateway-heartbeat-ack.json.
    #[test]
    fn discord_gateway_heartbeat_ack_recognized() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-heartbeat-ack.json");

        let frame = super::parse_gateway_opcode(fixture).expect("must parse");
        assert_eq!(frame.op, 11, "HEARTBEAT_ACK opcode must be 11");
        assert!(super::is_heartbeat_ack(fixture), "is_heartbeat_ack");
    }

    /// HEARTBEAT (opcode 1): client sends sequence number periodically.
    /// Spec shape: `{"op": 1, "d": <last_sequence_or_null>}`.
    #[test]
    fn discord_gateway_heartbeat_shape() {
        // First heartbeat (no sequence yet) — d is null per spec.
        let first = r#"{"op": 1, "d": null}"#;
        let frame = super::parse_gateway_opcode(first).expect("must parse");
        assert_eq!(frame.op, 1, "HEARTBEAT opcode must be 1");
        assert!(!super::is_heartbeat_ack(first), "heartbeat is not ack");

        // Subsequent heartbeat with sequence number.
        let subsequent = r#"{"op": 1, "d": 42}"#;
        let frame = super::parse_gateway_opcode(subsequent).expect("must parse");
        assert_eq!(frame.op, 1);
    }

    /// Full handshake sequence: HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK → READY.
    /// Asserts the correct opcode ordering and that each frame parses.
    #[test]
    fn discord_gateway_full_handshake_sequence() {
        let hello = include_str!("../../tests/fixtures/discord-gateway-hello.json");
        let heartbeat_ack = include_str!("../../tests/fixtures/discord-gateway-heartbeat-ack.json");
        let ready = include_str!("../../tests/fixtures/discord-gateway-ready.json");

        // Step 1: Server sends HELLO (op=10)
        let f1 = super::parse_gateway_opcode(hello).expect("hello");
        assert_eq!(f1.op, 10);

        // Step 2: Client sends IDENTIFY (op=2) — we build it
        let identify =
            super::build_identify_payload("Bot fake", twilight_model::gateway::Intents::GUILDS);
        assert_eq!(identify["op"], 2);

        // Step 3: Client sends HEARTBEAT (op=1)
        let hb = r#"{"op": 1, "d": null}"#;
        let f3 = super::parse_gateway_opcode(hb).expect("heartbeat");
        assert_eq!(f3.op, 1);

        // Step 4: Server sends HEARTBEAT_ACK (op=11)
        let f4 = super::parse_gateway_opcode(heartbeat_ack).expect("ack");
        assert_eq!(f4.op, 11);

        // Step 5: Server sends READY (op=0, t=READY)
        let f5 = super::parse_gateway_opcode(ready).expect("ready");
        assert_eq!(f5.op, 0);

        // Map READY to Connected event
        let frame: serde_json::Value = serde_json::from_str(ready).expect("json");
        let d = frame.get("d").expect("d");
        let ready_payload: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(d.clone()).expect("Ready");
        let event = super::map_ready_to_connected(&ready_payload);
        assert!(matches!(event, ChannelEvent::Connected { .. }));
    }

    // ── PR2 tests: MessageIn + send + notify ─────────────────────────

    /// §3.5.10 wire-format fixture: MESSAGE_CREATE gateway event
    /// parsed into `ChannelEvent::MessageIn`.
    #[test]
    fn discord_message_create_emits_message_in() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-message-create.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
        let d = frame.get("d").expect("d field");
        let msg: twilight_model::channel::Message =
            serde_json::from_value(d.clone()).expect("Message");

        // #bughunt-r3 #3: author on the allowlist → emitted.
        let allowlist = Some(vec![82198898841029460_i64]);
        let event = super::map_message_create_to_message_in(&msg, &allowlist)
            .expect("allowlisted author must emit MessageIn");

        match event {
            ChannelEvent::MessageIn {
                binding,
                from,
                payload,
                ts,
            } => {
                assert_eq!(binding.kind(), "discord");
                assert_eq!(from.id, "82198898841029460");
                assert_eq!(from.handle.as_deref(), Some("testoperator"));
                assert_eq!(payload.text, "hello from discord");
                // ts should be parseable (not epoch-zero)
                assert!(ts.timestamp() > 0);
            }
            other => panic!("expected MessageIn, got: {other:?}"),
        }
    }

    /// #bughunt-r3 #3: Discord inbound must be allowlist-gated like telegram.
    /// An author NOT on the allowlist (and the fail-closed `None` / empty cases)
    /// must be dropped — `map_message_create_to_message_in` returns `None`.
    #[test]
    fn discord_message_create_rejected_when_author_not_allowlisted() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-message-create.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
        let d = frame.get("d").expect("d field");
        let msg: twilight_model::channel::Message =
            serde_json::from_value(d.clone()).expect("Message");

        // Author 82198898841029460 is NOT in this list → dropped.
        assert!(
            super::map_message_create_to_message_in(&msg, &Some(vec![999_i64])).is_none(),
            "author absent from allowlist must be dropped"
        );
        // Fail-closed: unconfigured allowlist (None) → dropped.
        assert!(
            super::map_message_create_to_message_in(&msg, &None).is_none(),
            "None allowlist must fail-closed (drop)"
        );
        // Fail-closed: empty allowlist → dropped.
        assert!(
            super::map_message_create_to_message_in(&msg, &Some(vec![])).is_none(),
            "empty allowlist must reject all"
        );
    }

    /// Accept-path parity (#2562 PR-1): allowlisted messages must log for
    /// observability, same as the reject path already does. Regression
    /// guard for the asymmetry found during #2562 P3's live smoke test
    /// (the accept path produced zero log output before this PR).
    #[test]
    #[tracing_test::traced_test]
    fn discord_message_create_accept_path_logs_info() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-message-create.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
        let d = frame.get("d").expect("d field");
        let msg: twilight_model::channel::Message =
            serde_json::from_value(d.clone()).expect("Message");

        let allowlist = Some(vec![82198898841029460_i64]);
        super::map_message_create_to_message_in(&msg, &allowlist)
            .expect("allowlisted author must emit MessageIn");

        assert!(
            logs_contain("discord message accepted by user_allowlist"),
            "accept path must log for observability parity with the reject path"
        );
    }

    // ── #2562 PR-1: inbound dispatcher ──

    fn dispatch_test_home(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-discord-dispatch-test-{}-{label}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn message_in_event(channel_id: u64, text: &str) -> ChannelEvent {
        ChannelEvent::MessageIn {
            binding: crate::channel::BindingRef::new(
                "discord",
                Some(format!("DC#{channel_id}")),
                super::DiscordBindingPayload { channel_id },
            ),
            from: crate::channel::event::User {
                id: "999".to_string(),
                handle: Some("someuser".to_string()),
            },
            payload: crate::channel::event::MsgPayload {
                text: text.to_string(),
            },
            ts: chrono::Utc::now(),
        }
    }

    /// `resolve_instance_for_channel` returns the instance bound to a
    /// channel_id via `record_binding` — the reverse-lookup table
    /// Telegram's `resolve_topic` uses for `topic_id`.
    #[test]
    fn resolve_instance_for_channel_returns_bound_instance() {
        let (ch, _tx) = super::DiscordChannel::new_for_test();
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#111".into()),
            super::DiscordBindingPayload { channel_id: 111 },
        );
        crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());

        assert_eq!(ch.resolve_instance_for_channel(111), "dev-agent");
    }

    /// Unbound channel_id (no `record_binding` call ever happened) falls
    /// back to `"general"` + a warn log — mirrors `telegram/inbound.rs`'s
    /// topic-miss fallback semantics.
    #[test]
    #[tracing_test::traced_test]
    fn resolve_instance_for_channel_falls_back_to_general_when_unbound() {
        let (ch, _tx) = super::DiscordChannel::new_for_test();

        assert_eq!(ch.resolve_instance_for_channel(222), "general");
        assert!(
            logs_contain("no instance bound to this channel"),
            "unbound channel_id must warn so operators can trace the fallback"
        );
    }

    /// `dispatch_channel_event` end-to-end: extracts channel_id from the
    /// binding and routes via `resolve_instance_for_channel` — an
    /// integration check on top of the resolver unit tests above (proves
    /// the wiring, not just the resolver in isolation). Verified via the
    /// routing-decision log rather than `inbox::drain`, since the
    /// delivery layer below `notify_agent_with_attachments` depends on
    /// live daemon/PTY state this test environment doesn't have.
    #[test]
    #[tracing_test::traced_test]
    fn dispatch_channel_event_routes_to_bound_instance() {
        let (ch, _tx) = super::DiscordChannel::new_for_test();
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#111".into()),
            super::DiscordBindingPayload { channel_id: 111 },
        );
        crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
        let home = dispatch_test_home("routes");

        super::dispatch_channel_event(&ch, &home, message_in_event(111, "hello dev-agent"));

        assert!(
            logs_contain("routing message to instance") && logs_contain("dev-agent"),
            "dispatch must resolve and log the bound instance"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// PR-1 review fix: a message ≥ 200 chars must be persisted to the
    /// instance's inbox BEFORE the (truncating, pointer-only-for-long-text)
    /// PTY notification fires — otherwise the notification's "use the inbox
    /// MCP tool to read full message" pointer has nothing to point at.
    /// Verified directly via `inbox::drain` (unlike the short-message tests
    /// above, this doesn't depend on live daemon/PTY state — `enqueue` is a
    /// plain synchronous file write).
    #[test]
    fn dispatch_channel_event_persists_long_message_to_inbox() {
        let (ch, _tx) = super::DiscordChannel::new_for_test();
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#444".into()),
            super::DiscordBindingPayload { channel_id: 444 },
        );
        crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
        let home = dispatch_test_home("long-message");
        let long_text = "a".repeat(250);

        super::dispatch_channel_event(&ch, &home, message_in_event(444, &long_text));

        let msgs = crate::inbox::drain(&home, "dev-agent");
        assert!(
            msgs.iter().any(|m| m.text == long_text),
            "long message must be persisted in full to the bound instance's inbox; \
             got {} message(s), lengths: {:?}",
            msgs.len(),
            msgs.iter().map(|m| m.text.len()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Short message (< 200 chars) must NOT be persisted to the inbox —
    /// preserves Telegram's existing short-message behavior (PTY-only,
    /// no disk write) rather than accidentally persisting everything.
    #[test]
    fn dispatch_channel_event_does_not_persist_short_message_to_inbox() {
        let (ch, _tx) = super::DiscordChannel::new_for_test();
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#555".into()),
            super::DiscordBindingPayload { channel_id: 555 },
        );
        crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
        let home = dispatch_test_home("short-message");

        super::dispatch_channel_event(&ch, &home, message_in_event(555, "hi"));

        let msgs = crate::inbox::drain(&home, "dev-agent");
        assert!(
            msgs.iter().all(|m| m.text != "hi"),
            "short message must not be persisted to inbox (PTY-inject-only path); got: {:?}",
            msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #2562 PR-1 regression pin, same shape as `build_http_client_does_not_
    /// panic_on_bare_thread`: the dispatch path touches no tokio runtime, so
    /// calling it from a genuine bare `std::thread` must not panic.
    #[test]
    fn dispatch_channel_event_does_not_panic_on_bare_thread() {
        let home = dispatch_test_home("bare-thread");
        let home_for_thread = home.clone();
        let joined = std::thread::spawn(move || {
            let (ch, _tx) = super::DiscordChannel::new_for_test();
            super::dispatch_channel_event(
                &ch,
                &home_for_thread,
                message_in_event(333, "bare thread smoke"),
            );
        })
        .join();
        assert!(
            joined.is_ok(),
            "dispatch_channel_event must not panic when called from a bare std::thread \
             with no ambient Tokio runtime"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// §3.5.10 wire-format fixture: outbound POST /channels/{id}/messages
    /// response parsed into `MsgRef`.
    #[test]
    fn discord_create_message_response_parses_to_msg_ref() {
        let fixture =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let msg: twilight_model::channel::Message =
            serde_json::from_str(fixture).expect("response must parse as Message");

        let msg_ref = super::map_message_to_msg_ref(&msg);

        assert_eq!(msg_ref.id, "444385199974967099");
        assert_eq!(msg_ref.binding.kind(), "discord");
    }

    /// send_from_agent(Reply) on an authorized channel with no binding
    /// for the agent should error with "no discord binding".
    #[test]
    fn send_from_agent_reply_errors_on_unbound_instance() {
        let (ch, _rx) = super::DiscordChannel::new_for_test_authorized();
        // Authorized but no binding → should error about binding.
        let result = crate::channel::Channel::send_from_agent(
            &ch,
            "unknown-agent",
            crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
        );
        let err = result.expect_err("unbound instance must error");
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("no discord binding"),
            "error must mention binding, got: {err_msg}"
        );
    }

    /// F2 fix: send_from_agent must check outbound_authorized() gate.
    /// When user_allowlist is None (unconfigured), the gate drops the call.
    #[test]
    fn send_from_agent_blocked_by_outbound_gate() {
        let (ch, _rx) = super::DiscordChannel::new_for_test(); // allowlist=None → unauthorized
        let result = crate::channel::Channel::send_from_agent(
            &ch,
            "any-agent",
            crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
        );
        let err = result.expect_err("unauthorized channel must reject");
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("outbound disabled"),
            "error must mention outbound gate, got: {err_msg}"
        );
    }

    /// notify on an unbound instance should error gracefully.
    #[test]
    fn notify_errors_on_unbound_instance() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        let result = crate::channel::Channel::notify(
            &ch,
            "unknown-agent",
            crate::channel::NotifySeverity::Info,
            "test notification",
            false,
        );
        assert!(result.is_err(), "notify on unbound instance must error");
    }

    // ── F3 fix: §3.5.10 outbound request body shape assertion ────────
    //
    // Production-path-coupled: exercises the real Channel::send() →
    // twilight_http::create_message() path against a mock HTTP server.
    // The mock captures the request body twilight actually transmits
    // and asserts it matches the Discord spec shape.

    /// §3.5.10 wire-format: outbound POST /channels/{id}/messages request
    /// body transmitted by twilight-http matches Discord spec shape.
    ///
    /// Uses a raw TCP listener as mock Discord API server. The twilight
    /// client is pointed at it via `proxy()`. Channel::send() exercises
    /// the real production code path.
    #[test]
    fn discord_send_outbound_body_matches_spec() {
        use crate::channel::Channel;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // Step 1: Start a mock HTTP server on an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        // Step 2: Spawn a thread to handle one request.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let captured_clone = captured.clone();
        // fire-and-forget: test mock server thread — lives only for this test
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            // Extract body after the \r\n\r\n header separator.
            if let Some(idx) = request.find("\r\n\r\n") {
                let body = &request[idx + 4..];
                *captured_clone.lock().expect("lock") = Some(body.to_string());
            }

            // Respond with a minimal valid Discord Message JSON.
            let response_body =
                include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        // Step 3: Create twilight client pointed at mock server.
        // twilight-http 0.17's ratelimiter initialises inside `build()` and needs
        // a Tokio reactor in scope, so construct it within the shared discord
        // runtime (production builds the client in async context already).
        let client = super::block_on_value(async {
            twilight_http::Client::builder()
                .proxy(format!("127.0.0.1:{port}"), true)
                .build()
        });
        let client = std::sync::Arc::new(client);

        // Step 4: Create DiscordChannel with this client + a recorded binding.
        let (ch, _tx) = super::DiscordChannel::new_for_test_with_http(client);
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#290926798999357250".into()),
            super::DiscordBindingPayload {
                channel_id: 290926798999357250,
            },
        );
        ch.record_binding("test-agent", binding.clone(), "\r".into());

        // Step 5: Call the real production send() path.
        let result = crate::channel::Channel::send(
            &ch,
            &binding,
            crate::channel::OutMsg::text("Hello, World!"),
        );

        handle.join().expect("mock server thread");

        // Step 6: Assert the request body twilight transmitted.
        assert!(result.is_ok(), "send must succeed: {:?}", result.err());
        let body_str = captured
            .lock()
            .expect("lock")
            .take()
            .expect("body captured");
        let actual: serde_json::Value =
            serde_json::from_str(&body_str).expect("body must be valid JSON");
        let expected: serde_json::Value = serde_json::json!({"content": "Hello, World!"});
        assert_eq!(
            actual, expected,
            "outbound body must match Discord spec create-message shape"
        );
    }

    // ── PR3 tests: edit + delete production-path-coupled ─────────────

    /// Captured HTTP request from the mock server: method, path, body.
    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
    }

    /// Reusable mock HTTP server that captures one request and responds
    /// with a canned response. Returns (port, join_handle, captured_arc).
    fn mock_http_server(
        response_status: u16,
        response_body: &str,
    ) -> (
        u16,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Option<CapturedRequest>>>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<CapturedRequest>));
        let captured_clone = captured.clone();
        let resp_body = response_body.to_string();
        let status = response_status;

        // fire-and-forget: test mock server thread — lives only for this test
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            // Parse method + path from first line.
            let first_line = request.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();
            let method = parts.first().unwrap_or(&"").to_string();
            let path = parts.get(1).unwrap_or(&"").to_string();

            // Extract body after \r\n\r\n.
            let body = request
                .find("\r\n\r\n")
                .map(|idx| request[idx + 4..].to_string())
                .unwrap_or_default();

            *captured_clone.lock().expect("lock") = Some(CapturedRequest { method, path, body });

            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        (port, handle, captured)
    }

    fn make_test_channel_with_mock(
        port: u16,
    ) -> (super::DiscordChannel, std::sync::mpsc::Sender<ChannelEvent>) {
        // twilight-http 0.17's ratelimiter initialises inside `build()` and needs
        // a Tokio reactor in scope — build within the shared discord runtime.
        let client = super::block_on_value(async {
            twilight_http::Client::builder()
                .proxy(format!("127.0.0.1:{port}"), true)
                .build()
        });
        super::DiscordChannel::new_for_test_with_http(std::sync::Arc::new(client))
    }

    fn test_binding(channel_id: u64) -> crate::channel::BindingRef {
        crate::channel::BindingRef::new(
            "discord",
            Some(format!("DC#{channel_id}")),
            super::DiscordBindingPayload { channel_id },
        )
    }

    fn test_msg_ref(channel_id: u64, msg_id: &str) -> crate::channel::MsgRef {
        crate::channel::MsgRef {
            binding: test_binding(channel_id),
            id: msg_id.to_string(),
        }
    }

    /// §3.5.10 wire-format: PATCH /channels/{cid}/messages/{mid} request
    /// body transmitted by twilight-http matches Discord edit-message spec.
    /// Ref: https://discord.com/developers/docs/resources/message#edit-message
    #[test]
    fn discord_edit_outbound_body_matches_spec() {
        use crate::channel::Channel;

        // Edit response is the updated message — reuse create-message fixture.
        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
        let result = ch.edit(&msg_ref, crate::channel::OutMsg::text("edited text"));

        handle.join().expect("mock server");
        assert!(result.is_ok(), "edit must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH", "edit must use PATCH method");
        assert!(
            req.path.contains("/messages/444385199974967099"),
            "path must contain message id: {}",
            req.path
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert_eq!(
            body["content"], "edited text",
            "edit body must contain updated content"
        );
    }

    /// §3.5.10 wire-format: DELETE /channels/{cid}/messages/{mid}
    /// Ref: https://discord.com/developers/docs/resources/message#delete-message
    #[test]
    fn discord_delete_outbound_method_matches_spec() {
        use crate::channel::Channel;

        // DELETE returns 204 No Content with empty body per spec.
        let (port, handle, captured) = mock_http_server(204, "");
        let (ch, _tx) = make_test_channel_with_mock(port);

        let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
        let result = ch.delete(&msg_ref);

        handle.join().expect("mock server");
        assert!(result.is_ok(), "delete must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "DELETE", "delete must use DELETE method");
        assert!(
            req.path.contains("/messages/444385199974967099"),
            "path must contain message id: {}",
            req.path
        );
        assert!(
            req.body.is_empty() || req.body.trim().is_empty(),
            "DELETE body must be empty per spec, got: '{}'",
            req.body
        );
    }

    /// send_from_agent(Edit) wires through edit() with gate check.
    #[test]
    fn send_from_agent_edit_wires_through_edit() {
        use crate::channel::Channel;

        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        // Record a binding so the agent lookup succeeds.
        ch.record_binding("test-agent", test_binding(290926798999357250), "\r".into());

        let result = ch.send_from_agent(
            "test-agent",
            crate::channel::AgentOutboundOp::Edit {
                message_id: "444385199974967099".into(),
                new_text: "updated".into(),
            },
        );

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "send_from_agent Edit must succeed: {:?}",
            result.err()
        );

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH");
    }

    // ── PR4 tests: binding lifecycle + CHANNEL_DELETE + persistence ───

    /// §3.5.10 wire-format: POST /guilds/{gid}/channels request via
    /// production Channel::create_binding() path.
    #[test]
    fn discord_create_binding_outbound_matches_spec() {
        use crate::channel::Channel;

        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-guild-channel-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let result = ch.create_binding("test-agent", crate::channel::BindingOpts::default());

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "create_binding must succeed: {:?}",
            result.err()
        );

        let binding = result.expect("binding");
        assert_eq!(binding.kind(), "discord");
        // Channel ID from fixture response.
        assert_eq!(binding.display_tag(), Some("DC#555555555555555555"));

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "POST", "create_binding must use POST");
        assert!(
            req.path.contains("/guilds/"),
            "path must target guild: {}",
            req.path
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert!(
            body["name"].is_string(),
            "request body must have 'name' field"
        );
    }

    /// §3.5.10 wire-format: DELETE /channels/{id} via production
    /// Channel::remove_binding() path.
    #[test]
    fn discord_remove_binding_outbound_matches_spec() {
        use crate::channel::Channel;

        // DELETE returns the deleted channel object per spec.
        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-guild-channel-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let binding = test_binding(555555555555555555);
        let result = ch.remove_binding(&binding);

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "remove_binding must succeed: {:?}",
            result.err()
        );

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "DELETE", "remove_binding must use DELETE");
        assert!(
            req.path.contains("/channels/555555555555555555"),
            "path must contain channel id: {}",
            req.path
        );
    }

    /// §3.5.10 wire-format: CHANNEL_DELETE gateway event → BindingRevoked.
    #[test]
    fn discord_channel_delete_emits_binding_revoked() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-channel-delete.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");

        // Extract channel_id from the event payload.
        let channel_id: u64 = frame["d"]["id"]
            .as_str()
            .expect("id")
            .parse()
            .expect("parse id");

        let event = super::map_channel_delete_to_binding_revoked(channel_id);

        match event {
            ChannelEvent::BindingRevoked { binding, reason } => {
                assert_eq!(binding.kind(), "discord");
                assert_eq!(reason, crate::channel::event::RevokeReason::Deleted);
            }
            other => panic!("expected BindingRevoked, got: {other:?}"),
        }
    }

    /// CHANNEL_DELETE delivered via poll_event: gateway pushes event,
    /// poll_event drains it as BindingRevoked.
    #[test]
    fn discord_channel_delete_via_poll_event() {
        use crate::channel::Channel;

        let (ch, tx) = super::DiscordChannel::new_for_test();
        let event = super::map_channel_delete_to_binding_revoked(290926798999357250);
        tx.send(event).expect("send");

        let polled = ch.poll_event().expect("should have event");
        assert!(
            matches!(polled, ChannelEvent::BindingRevoked { .. }),
            "expected BindingRevoked, got: {polled:?}"
        );
    }

    /// §3.5.10 persistence-replay: binding registry round-trip.
    /// Write state → serialize → deserialize → verify bindings intact.
    #[test]
    fn discord_binding_registry_persistence_round_trip() {
        use crate::channel::Channel;

        let (ch, _tx) = super::DiscordChannel::new_for_test();

        // Record two bindings.
        ch.record_binding("agent-a", test_binding(111), "\r".into());
        ch.record_binding("agent-b", test_binding(222), "\r".into());

        // Serialize the binding registry to JSON (simulating disk write).
        let snapshot: std::collections::HashMap<String, u64> = {
            let s = ch.state.lock();
            s.instance_to_channel.clone()
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");

        // Simulate restart: deserialize and verify.
        let restored: std::collections::HashMap<String, u64> =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored["agent-a"], 111);
        assert_eq!(restored["agent-b"], 222);

        // Verify the live channel still has correct bindings.
        assert!(ch.has_binding("agent-a"));
        assert!(ch.has_binding("agent-b"));
        assert!(!ch.has_binding("agent-c"));

        // Take and verify round-trip.
        let taken = ch.take_binding("agent-a").expect("take");
        assert_eq!(taken.kind(), "discord");
        assert!(!ch.has_binding("agent-a"));
    }

    // ── F1 fix: auto-archive keepalive test ──────────────────────────

    /// §3.5.10 production-path-coupled: keepalive PATCH via
    /// send_keepalive_patch() against mock server.
    #[test]
    fn discord_keepalive_patch_method_matches_spec() {
        let (port, handle, captured) = mock_http_server(200, "{}");
        // twilight-http 0.17's ratelimiter needs a Tokio reactor at build().
        let client = super::block_on_value(async {
            twilight_http::Client::builder()
                .proxy(format!("127.0.0.1:{port}"), true)
                .build()
        });

        let result = super::send_keepalive_patch(&client, 290926798999357250);

        handle.join().expect("mock server");
        assert!(result.is_ok(), "keepalive must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH", "keepalive must use PATCH");
        assert!(
            req.path.contains("/channels/290926798999357250"),
            "path must target channel: {}",
            req.path
        );
        // Body must set archived=false per Discord thread update spec.
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert_eq!(body["archived"], false, "must set archived=false");
    }

    /// TLS smoke (network, manual): proves twilight-http 0.17's
    /// rustls-native-roots/ring stack actually completes a real TLS handshake —
    /// the one merge-gate CI can't cover (the spec tests use a plaintext mock
    /// server). `#[ignore]` so normal/CI runs skip it; run with
    /// `cargo test --features tray,discord -- --ignored tls_handshake_smoke`.
    ///
    /// A missing crypto provider would panic ("no process-level CryptoProvider")
    /// during the handshake. We hit `GET /gateway` (no valid token → 401 is fine);
    /// any HTTP/auth response proves the handshake succeeded. Auth is NOT tested.
    #[tokio::test]
    #[ignore = "network: real Discord TLS handshake smoke; run manually"]
    async fn tls_handshake_smoke_real_discord() {
        let client = twilight_http::Client::new("Bot tls-smoke-no-valid-token".to_string());
        // `.gateway()` GETs https://discord.com/api/v10/gateway. The handshake
        // happens before any auth check. A panic here = rustls/ring not wired.
        let outcome = client.gateway().await;
        // Reaching this line at all means no CryptoProvider panic. Surface the
        // result so the run log shows the handshake completed.
        match outcome {
            Ok(_) => eprintln!("TLS smoke: handshake + request OK (gateway responded)"),
            Err(e) => {
                eprintln!("TLS smoke: handshake OK, request returned (expected w/o token): {e}")
            }
        }
    }

    /// Keepalive interval constant is reasonable (≤ Discord's shortest
    /// auto-archive of 3600s). Compile-time check via `const {}` blocks —
    /// per `clippy::assertions_on_constants` and Rust 1.79+ const block
    /// support — so a regression in `KEEPALIVE_INTERVAL_SECS` fails the
    /// build, not just this test.
    #[test]
    fn discord_keepalive_interval_within_auto_archive_window() {
        const { assert!(super::KEEPALIVE_INTERVAL_SECS < 3600) };
        const { assert!(super::KEEPALIVE_INTERVAL_SECS >= 60) };
    }

    /// #2562 P3 regression pin: `build_http_client` must not panic when
    /// called from a bare `std::thread` with no ambient Tokio runtime —
    /// mirrors exactly how `bootstrap::discord_init::init` calls it on real
    /// daemon boot (a plain `std::thread::Builder::spawn`, not a
    /// `#[tokio::main]`/`#[tokio::test]` thread). Before the
    /// `discord_runtime()`/`block_on_value` fix, this panicked with "there
    /// is no reactor running, must be called from the context of a Tokio
    /// 1.x runtime" (twilight-http-ratelimiting's internal `tokio::spawn` in
    /// `Client::new`'s rate limiter) — caught only by an isolated
    /// smoke-home real boot, never by the `#[tokio::test]`-shielded tests
    /// above, which supply the missing runtime and hide the bug.
    #[test]
    fn build_http_client_does_not_panic_on_bare_thread() {
        let joined = std::thread::spawn(|| {
            super::build_http_client("Bot pin-test-not-a-real-token".to_string());
        })
        .join();
        assert!(
            joined.is_ok(),
            "build_http_client must not panic when called from a bare std::thread \
             with no ambient Tokio runtime (matches bootstrap::discord_init::init's \
             real call site)"
        );
    }
}
