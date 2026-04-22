//! Discord adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Discord message → inbox + PTY notification
//! Outbound: reply(text) → Discord send_message to channel

use crate::agent::AgentRegistry;
use crate::fleet::ChannelConfig;
use crate::inbox::{self, InboxMessage};
use serenity::all::{
    ChannelId, ChannelType, Context, EventHandler, GatewayIntents, GuildChannel, GuildId, Http,
    Message, Ready,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Lock DiscordState, recovering from poison.
pub(crate) fn lock_state(
    ds: &Arc<Mutex<DiscordState>>,
) -> std::sync::MutexGuard<'_, DiscordState> {
    ds.lock().unwrap_or_else(|e| {
        tracing::warn!("DiscordState mutex poisoned, recovering");
        e.into_inner()
    })
}

// ---------------------------------------------------------------------------
// Channel registry — persists channel_id → instance_name in channels.json
// ---------------------------------------------------------------------------

fn channel_registry_path(home: &Path) -> PathBuf {
    home.join("channels.json")
}

fn load_channel_registry(home: &Path) -> HashMap<u64, String> {
    let path = channel_registry_path(home);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| k.parse::<u64>().ok().map(|id| (id, v)))
                .collect()
        })
        .unwrap_or_default()
}

fn save_channel_registry(home: &Path, registry: &HashMap<u64, String>) {
    let map: HashMap<String, &String> = registry.iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(channel_registry_path(home), json);
    }
}

fn register_channel(home: &Path, channel_id: u64, instance_name: &str) {
    let mut reg = load_channel_registry(home);
    reg.insert(channel_id, instance_name.to_string());
    save_channel_registry(home, &reg);
}

fn unregister_channel(home: &Path, channel_id: u64) {
    let mut reg = load_channel_registry(home);
    reg.remove(&channel_id);
    save_channel_registry(home, &reg);
}

/// Shared tokio runtime for all Discord sync→async calls.
fn discord_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("discord tokio runtime")
    })
}

// ---------------------------------------------------------------------------
// DiscordState
// ---------------------------------------------------------------------------

pub struct DiscordState {
    #[allow(dead_code)]
    pub http: Arc<Http>,
    #[allow(dead_code)]
    pub guild_id: GuildId,
    #[allow(dead_code)]
    pub category_id: ChannelId,
    pub channel_to_instance: HashMap<u64, String>,
    pub instance_to_channel: HashMap<String, u64>,
    pub home: PathBuf,
    pub submit_keys: HashMap<String, String>,
    pub user_allowlist: Option<Vec<u64>>,
    pub registry: Option<AgentRegistry>,
}

impl DiscordState {
    /// Return true if a sender is permitted by the allowlist.
    pub fn is_user_allowed(&self, user_id: u64) -> bool {
        match &self.user_allowlist {
            None => true,
            Some(list) => list.contains(&user_id),
        }
    }
}

/// Wire the in-process AgentRegistry into an already-initialized DiscordState.
#[allow(dead_code)]
pub fn attach_registry(state: &Arc<Mutex<DiscordState>>, registry: AgentRegistry) {
    let mut s = lock_state(state);
    s.registry = Some(registry);
}

/// Parse a snowflake string to u64 with a clear error message.
fn parse_snowflake(s: &str, label: &str) -> anyhow::Result<u64> {
    s.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("invalid {label} snowflake '{s}': expected a numeric u64 ID"))
}


// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize Discord from fleet config.
pub fn init_from_config(
    config: &crate::fleet::FleetConfig,
    home: &Path,
    submit_keys: HashMap<String, String>,
) -> Option<Arc<Mutex<DiscordState>>> {
    let ChannelConfig::Discord {
        bot_token_env,
        guild_id,
        category_name,
        user_allowlist,
    } = config.channel.as_ref()?
    else {
        return None;
    };
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => {
            tracing::info!(env = %bot_token_env, "discord bot token env not set, skipping");
            return None;
        }
    };
    let guild_id_u64 = match parse_snowflake(guild_id, "guild_id") {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "failed to parse guild_id");
            return None;
        }
    };
    let allowlist: Option<Vec<u64>> = user_allowlist.as_ref().map(|list| {
        list.iter()
            .filter_map(|s| match parse_snowflake(s, "user_allowlist entry") {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping invalid allowlist entry");
                    None
                }
            })
            .collect()
    });

    match allowlist {
        None => tracing::warn!(
            "discord channel.user_allowlist is not set — any server member can command the fleet."
        ),
        Some(ref list) if list.is_empty() => {
            tracing::info!("discord channel.user_allowlist is empty — all inbound messages will be rejected")
        }
        Some(ref list) => tracing::info!(count = list.len(), "discord user_allowlist active"),
    }

    let http = Arc::new(Http::new(&token));
    let gid = GuildId::new(guild_id_u64);
    let category_name = category_name.clone();

    // Find or create category + channels synchronously at startup
    let (category_id, channel_map, instance_map) = match discord_runtime().block_on(
        setup_channels(Arc::clone(&http), gid, &category_name, config, home),
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "discord channel setup failed");
            return None;
        }
    };

    let state = Arc::new(Mutex::new(DiscordState {
        http: Arc::clone(&http),
        guild_id: gid,
        category_id,
        channel_to_instance: channel_map,
        instance_to_channel: instance_map,
        home: home.to_path_buf(),
        submit_keys,
        user_allowlist: allowlist,
        registry: None,
    }));

    start_gateway(token, Arc::clone(&state));
    Some(state)
}

/// Find or create the category and per-instance channels.
async fn setup_channels(
    http: Arc<Http>,
    guild_id: GuildId,
    category_name: &str,
    config: &crate::fleet::FleetConfig,
    home: &Path,
) -> anyhow::Result<(ChannelId, HashMap<u64, String>, HashMap<String, u64>)> {
    let channels = guild_id.channels(&http).await?;

    // Find or create category
    let category_id = if let Some(cat) = channels
        .values()
        .find(|c| c.kind == ChannelType::Category && c.name == category_name)
    {
        cat.id
    } else {
        let cat = guild_id
            .create_channel(&http, serenity::all::CreateChannel::new(category_name).kind(ChannelType::Category))
            .await?;
        tracing::info!(name = %category_name, id = %cat.id, "created discord category");
        cat.id
    };

    // Load existing registry
    let mut reg = load_channel_registry(home);
    let instance_names: std::collections::HashSet<&String> = config.instances.keys().collect();

    // Clean up orphaned channels
    let orphans: Vec<(u64, String)> = reg
        .iter()
        .filter(|(_, name)| !instance_names.contains(name))
        .map(|(id, name)| (*id, name.clone()))
        .collect();
    for (cid, name) in &orphans {
        tracing::info!(channel_id = cid, instance = %name, "orphaned channel, deleting");
        let _ = ChannelId::new(*cid).delete(&http).await;
        reg.remove(cid);
    }
    if !orphans.is_empty() {
        save_channel_registry(home, &reg);
    }

    let mut channel_to_instance: HashMap<u64, String> = HashMap::new();
    let mut instance_to_channel: HashMap<String, u64> = HashMap::new();

    for name in config.instances.keys() {
        // Check if instance already has a channel_id in config
        let existing_cid = config
            .instances
            .get(name)
            .and_then(|i| i.channel_id.as_ref())
            .and_then(|s| s.parse::<u64>().ok());

        let cid = if let Some(cid) = existing_cid {
            // Verify channel still exists
            if channels.contains_key(&ChannelId::new(cid)) {
                cid
            } else {
                create_text_channel(&http, guild_id, category_id, name).await?
            }
        } else if name == "general" {
            // G6: find channel named "general" in category, or create it
            if let Some(ch) = channels.values().find(|c| {
                c.kind == ChannelType::Text
                    && c.parent_id == Some(category_id)
                    && c.name == "general"
            }) {
                ch.id.get()
            } else {
                create_text_channel(&http, guild_id, category_id, "general").await?
            }
        } else {
            create_text_channel(&http, guild_id, category_id, name).await?
        };

        channel_to_instance.insert(cid, name.clone());
        instance_to_channel.insert(name.clone(), cid);
        register_channel(home, cid, name);

        // Write channel_id back to fleet.yaml
        let _ = crate::fleet::update_instance_field(
            home,
            name,
            "channel_id",
            serde_yaml::Value::String(cid.to_string()),
        );
    }

    Ok((category_id, channel_to_instance, instance_to_channel))
}

async fn create_text_channel(
    http: &Http,
    guild_id: GuildId,
    category_id: ChannelId,
    name: &str,
) -> anyhow::Result<u64> {
    let ch = guild_id
        .create_channel(
            http,
            serenity::all::CreateChannel::new(name)
                .kind(ChannelType::Text)
                .category(category_id),
        )
        .await?;
    tracing::info!(instance = %name, channel_id = %ch.id, "created discord channel");
    Ok(ch.id.get())
}


// ---------------------------------------------------------------------------
// Gateway (inbound)
// ---------------------------------------------------------------------------

struct Handler {
    state: Arc<Mutex<DiscordState>>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, _ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let sender_id = msg.author.id.get();
        let username = &msg.author.name;
        let channel_id = msg.channel_id.get();

        // Authz
        {
            let s = lock_state(&self.state);
            if !s.is_user_allowed(sender_id) {
                tracing::warn!(from = %username, user_id = sender_id, "discord message rejected by user_allowlist");
                return;
            }
        }

        let text = msg.content.clone();
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let (instance_name, home, submit_key, registry) = {
            let mut s = lock_state(&self.state);
            let name = resolve_channel_to_instance(&mut s, channel_id);
            let sk = s
                .submit_keys
                .get(&name)
                .cloned()
                .unwrap_or_else(|| "\r".to_string());
            (name, s.home.clone(), sk, s.registry.clone())
        };

        tracing::info!(from = %username, to = %instance_name, %text, "discord inbound");

        // Save inbound message ID + channel_id for status reactions and react eyes
        let inbound_msg_id = msg.id.to_string();
        crate::ops::save_metadata(&home, &instance_name, "last_inbound_message_id", serde_json::json!(inbound_msg_id));
        crate::ops::save_metadata(&home, &instance_name, "last_inbound_channel_id", serde_json::json!(channel_id.to_string()));
        {
            let http = {
                let s = lock_state(&self.state);
                Arc::clone(&s.http)
            };
            let cid = serenity::all::ChannelId::new(channel_id);
            let mid = msg.id;
            let reaction = serenity::all::ReactionType::Unicode("\u{1f440}".to_string());
            let _ = cid.create_reaction(&http, mid, reaction).await;
        }

        // Handle attachments — download immediately (Discord CDN URLs expire)
        let mut full_text = text;
        for att in &msg.attachments {
            let download_dir = home.join("downloads").join(&instance_name);
            std::fs::create_dir_all(&download_dir).ok();
            let dest = download_dir.join(&att.filename);
            if let Ok(bytes) = att.download().await {
                let _ = std::fs::write(&dest, &bytes);
                full_text.push_str(&format!("\n[attachment: {}]", att.filename));
            }
        }

        // Route based on agent state
        if agent_wants_raw_keystrokes(registry.as_ref(), &instance_name) {
            let payload = format!("{full_text}\n");
            match crate::api::call(
                &home,
                &serde_json::json!({
                    "method": crate::api::method::INJECT,
                    "params": {"name": instance_name, "data": payload, "raw": true}
                }),
            ) {
                Ok(_) => tracing::info!(to = %instance_name, "routed raw keystrokes (discord)"),
                Err(e) => tracing::warn!(to = %instance_name, error = %e, "raw injection failed"),
            }
            return;
        }

        let msg_obj = InboxMessage {
            from: format!("user:{username}"),
            text: full_text.clone(),
            kind: Some("discord".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        let _ = inbox::enqueue(&home, &instance_name, msg_obj);
        inbox::notify_agent(
            &home,
            &instance_name,
            &inbox::NotifySource::Telegram(username), // reuse Telegram variant for now
            &full_text,
            &submit_key,
        );
    }

    async fn channel_delete(&self, _ctx: Context, channel: GuildChannel, _messages: Option<Vec<Message>>) {
        let cid = channel.id.get();
        let (instance_name, home) = {
            let s = lock_state(&self.state);
            match s.channel_to_instance.get(&cid) {
                Some(name) => (name.clone(), s.home.clone()),
                None => return,
            }
        };
        tracing::info!(channel_id = cid, instance = %instance_name, "discord channel deleted");
        cleanup_deleted_channel(&home, &instance_name, cid, Some(&self.state));
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(user = %ready.user.name, "discord gateway connected");
    }
}

fn resolve_channel_to_instance(state: &mut DiscordState, channel_id: u64) -> String {
    if let Some(name) = state.channel_to_instance.get(&channel_id).cloned() {
        return name;
    }
    // Reload from fleet.yaml
    if let Ok(config) = crate::fleet::FleetConfig::load(&state.home.join("fleet.yaml")) {
        for (inst_name, inst) in &config.instances {
            if let Some(ref cid_str) = inst.channel_id {
                if let Ok(cid) = cid_str.parse::<u64>() {
                    if cid == channel_id {
                        state.channel_to_instance.insert(cid, inst_name.clone());
                        state.instance_to_channel.insert(inst_name.clone(), cid);
                        return inst_name.clone();
                    }
                }
            }
        }
    }
    "general".to_string()
}

fn agent_wants_raw_keystrokes(registry: Option<&AgentRegistry>, instance_name: &str) -> bool {
    let Some(registry) = registry else { return false };
    let reg = crate::agent::lock_registry(registry);
    let Some(handle) = reg.get(instance_name) else { return false };
    let core = Arc::clone(&handle.core);
    drop(reg);
    let guard = crate::sync::lock_poisoned(&core, "agent_core");
    guard.state.current.wants_raw_keystrokes()
}

pub(crate) fn cleanup_deleted_channel(
    home: &Path,
    instance_name: &str,
    channel_id: u64,
    state: Option<&Arc<Mutex<DiscordState>>>,
) {
    if let Some(state) = state {
        let mut s = lock_state(state);
        s.channel_to_instance.remove(&channel_id);
        s.instance_to_channel.remove(instance_name);
        s.submit_keys.remove(instance_name);
    }
    let _ = crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": instance_name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, instance_name) {
        tracing::warn!(instance = %instance_name, error = %e, "failed to remove from fleet.yaml");
    }
    unregister_channel(home, channel_id);
}

/// Start Discord gateway in a dedicated thread.
fn start_gateway(token: String, state: Arc<Mutex<DiscordState>>) {
    if let Err(e) = std::thread::Builder::new()
        .name("discord".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                tracing::error!("failed to build discord tokio runtime");
                return;
            };
            rt.block_on(async {
                let intents = GatewayIntents::GUILDS
                    | GatewayIntents::GUILD_MESSAGES
                    | GatewayIntents::MESSAGE_CONTENT;
                let handler = Handler { state };
                let mut client = match serenity::Client::builder(&token, intents)
                    .event_handler(handler)
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to build discord client");
                        return;
                    }
                };
                tracing::info!("discord gateway polling started");
                if let Err(e) = client.start().await {
                    tracing::error!(error = %e, "discord gateway error");
                }
            });
        })
    {
        tracing::error!(error = %e, "failed to spawn discord gateway thread");
    }
}


// ---------------------------------------------------------------------------
// Outbound: reply / react / edit / download / channel lifecycle
// ---------------------------------------------------------------------------

/// Resolved Discord channel credentials.
struct DiscordChannel {
    token: String,
    guild_id: u64,
}

fn resolve_discord_channel() -> anyhow::Result<(DiscordChannel, crate::fleet::FleetConfig)> {
    let home = crate::home_dir();
    let config = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))?;
    match &config.channel {
        Some(ChannelConfig::Discord {
            bot_token_env,
            guild_id,
            ..
        }) => {
            let token = std::env::var(bot_token_env)
                .map_err(|_| anyhow::anyhow!("discord bot token env '{bot_token_env}' not set"))?;
            let gid = parse_snowflake(guild_id, "guild_id")?;
            Ok((DiscordChannel { token, guild_id: gid }, config))
        }
        _ => anyhow::bail!("No Discord channel configured"),
    }
}

/// Split text at 2000-char Discord limit, preferring newline boundaries.
pub(crate) fn split_message(text: &str, limit: usize) -> Vec<&str> {
    if text.len() <= limit {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = (start + limit).min(text.len());
        if end == text.len() {
            chunks.push(&text[start..]);
            break;
        }
        // Try to split on a newline boundary
        let slice = &text[start..end];
        let split_at = slice.rfind('\n').map(|i| start + i + 1).unwrap_or(end);
        chunks.push(&text[start..split_at]);
        start = split_at;
    }
    chunks
}

/// Send a reply from an instance to its Discord channel. Returns (message_id, channel_id).
pub fn try_discord_reply(instance_name: &str, text: &str) -> anyhow::Result<(String, String)> {
    let (ch, config) = resolve_discord_channel()?;
    let channel_id = config
        .instances
        .get(instance_name)
        .and_then(|i| i.channel_id.as_ref())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("no channel_id for '{instance_name}'"))?;

    let http = Http::new(&ch.token);
    let cid = ChannelId::new(channel_id);
    let chunks = split_message(text, 2000);
    let mut last_msg_id = String::new();

    discord_runtime().block_on(async {
        for chunk in chunks {
            let msg = cid
                .send_message(&http, serenity::all::CreateMessage::new().content(chunk))
                .await?;
            last_msg_id = msg.id.to_string();
        }
        Ok::<(), anyhow::Error>(())
    })?;

    Ok((last_msg_id, channel_id.to_string()))
}

/// React to a message with an emoji.
pub fn try_discord_react(
    instance_name: &str,
    emoji: &str,
    message_id: Option<&str>,
) -> anyhow::Result<()> {
    let (ch, config) = resolve_discord_channel()?;
    let channel_id = config
        .instances
        .get(instance_name)
        .and_then(|i| i.channel_id.as_ref())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("no channel_id for '{instance_name}'"))?;

    let home = crate::home_dir();
    let mid: u64 = message_id
        .and_then(|m| m.parse().ok())
        .unwrap_or_else(|| {
            let meta_path = home.join("metadata").join(format!("{instance_name}.json"));
            std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|m| m["last_message_id"].as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0)
        });
    if mid == 0 {
        anyhow::bail!("no message_id to react to");
    }

    let http = Http::new(&ch.token);
    let emoji_char = map_emoji_name(emoji);
    discord_runtime().block_on(async {
        let cid = ChannelId::new(channel_id);
        let mid = serenity::all::MessageId::new(mid);
        let reaction = serenity::all::ReactionType::Unicode(emoji_char.to_string());
        cid.create_reaction(&http, mid, reaction).await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Edit a previously sent message.
pub fn try_discord_edit(
    instance_name: &str,
    message_id: &str,
    text: &str,
) -> anyhow::Result<()> {
    let (ch, config) = resolve_discord_channel()?;
    let channel_id = config
        .instances
        .get(instance_name)
        .and_then(|i| i.channel_id.as_ref())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("no channel_id for '{instance_name}'"))?;

    let mid: u64 = parse_snowflake(message_id, "message_id")?;
    let http = Http::new(&ch.token);
    discord_runtime().block_on(async {
        let cid = ChannelId::new(channel_id);
        let mid = serenity::all::MessageId::new(mid);
        cid.edit_message(&http, mid, serenity::all::EditMessage::new().content(text))
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Download a Discord attachment by URL.
pub fn try_discord_download(instance_name: &str, url: &str) -> anyhow::Result<String> {
    let home = crate::home_dir();
    let download_dir = home.join("downloads").join(instance_name);
    std::fs::create_dir_all(&download_dir)?;
    let filename = url
        .rsplit('/')
        .next()
        .and_then(|s| s.split('?').next())
        .unwrap_or("attachment");
    let dest = download_dir.join(filename);

    discord_runtime().block_on(async {
        let resp = reqwest::get(url).await?;
        let bytes = resp.bytes().await?;
        tokio::fs::write(&dest, &bytes).await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(dest.display().to_string())
}

/// Create a text channel for a new instance under the AgEnD category.
pub fn create_channel_for_instance(home: &Path, instance_name: &str) -> Option<String> {
    let (ch, _config) = resolve_discord_channel().ok()?;
    let http = Http::new(&ch.token);
    let gid = GuildId::new(ch.guild_id);

    discord_runtime()
        .block_on(async {
            let channels = gid.channels(&http).await?;
            // Find the AgEnD category
            let category_id = channels
                .values()
                .find(|c| c.kind == ChannelType::Category && c.name.contains("AgEnD"))
                .map(|c| c.id)
                .ok_or_else(|| anyhow::anyhow!("AgEnD category not found"))?;

            let cid = create_text_channel(&http, gid, category_id, instance_name).await?;
            Ok::<u64, anyhow::Error>(cid)
        })
        .ok()
        .map(|cid| {
            let cid_str = cid.to_string();
            let _ = crate::fleet::update_instance_field(
                home,
                instance_name,
                "channel_id",
                serde_yaml::Value::String(cid_str.clone()),
            );
            register_channel(home, cid, instance_name);
            cid_str
        })
}

/// Delete a Discord channel.
pub fn delete_channel(home: &Path, channel_id: u64) {
    let (ch, _) = match resolve_discord_channel() {
        Ok(c) => c,
        Err(_) => return,
    };
    let http = Http::new(&ch.token);
    let _ = discord_runtime().block_on(async {
        ChannelId::new(channel_id).delete(&http).await
    });
    unregister_channel(home, channel_id);
    tracing::info!(channel_id, "deleted discord channel");
}

/// Map emoji name to Unicode character.
fn map_emoji_name(name: &str) -> &str {
    match name {
        "thumbsup" | "thumbs_up" => "\u{1F44D}",
        "thumbsdown" | "thumbs_down" => "\u{1F44E}",
        "heart" | "red_heart" => "\u{2764}",
        "fire" => "\u{1F525}",
        "clap" => "\u{1F44F}",
        "thinking" => "\u{1F914}",
        "pray" | "folded_hands" => "\u{1F64F}",
        "party" | "tada" => "\u{1F389}",
        "eyes" => "\u{1F440}",
        "100" => "\u{1F4AF}",
        "ok" | "ok_hand" => "\u{1F44C}",
        "rocket" => "\u{1F680}",
        "check" | "white_check_mark" => "\u{2705}",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn split_message_short() {
        let chunks = split_message("hello", 2000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_exact_limit() {
        let text = "a".repeat(2000);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn split_message_over_limit() {
        let text = format!("{}\n{}", "a".repeat(1500), "b".repeat(1000));
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= 2000);
        assert!(chunks[1].len() <= 2000);
        // Should split on newline
        assert!(chunks[0].ends_with('\n'));
    }

    #[test]
    fn split_message_no_newline() {
        let text = "a".repeat(3000);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[1].len(), 1000);
    }

    #[test]
    fn parse_snowflake_valid() {
        assert_eq!(parse_snowflake("123456789012345678", "test").unwrap(), 123456789012345678);
    }

    #[test]
    fn parse_snowflake_invalid() {
        let err = parse_snowflake("not_a_number", "guild_id").unwrap_err();
        assert!(err.to_string().contains("guild_id"));
        assert!(err.to_string().contains("not_a_number"));
    }

    #[test]
    fn user_allowlist_none_accepts_all() {
        let state = DiscordState {
            http: Arc::new(Http::new("fake")),
            guild_id: GuildId::new(1),
            category_id: ChannelId::new(1),
            channel_to_instance: HashMap::new(),
            instance_to_channel: HashMap::new(),
            home: PathBuf::from("/tmp"),
            submit_keys: HashMap::new(),
            user_allowlist: None,
            registry: None,
        };
        assert!(state.is_user_allowed(1));
    }

    #[test]
    fn user_allowlist_empty_rejects_all() {
        let state = DiscordState {
            http: Arc::new(Http::new("fake")),
            guild_id: GuildId::new(1),
            category_id: ChannelId::new(1),
            channel_to_instance: HashMap::new(),
            instance_to_channel: HashMap::new(),
            home: PathBuf::from("/tmp"),
            submit_keys: HashMap::new(),
            user_allowlist: Some(vec![]),
            registry: None,
        };
        assert!(!state.is_user_allowed(1));
    }

    #[test]
    fn user_allowlist_restricts() {
        let state = DiscordState {
            http: Arc::new(Http::new("fake")),
            guild_id: GuildId::new(1),
            category_id: ChannelId::new(1),
            channel_to_instance: HashMap::new(),
            instance_to_channel: HashMap::new(),
            home: PathBuf::from("/tmp"),
            submit_keys: HashMap::new(),
            user_allowlist: Some(vec![42, 100]),
            registry: None,
        };
        assert!(state.is_user_allowed(42));
        assert!(!state.is_user_allowed(99));
    }

    #[test]
    fn emoji_map_known() {
        assert_eq!(map_emoji_name("fire"), "\u{1F525}");
        assert_eq!(map_emoji_name("rocket"), "\u{1F680}");
    }

    #[test]
    fn emoji_map_passthrough() {
        assert_eq!(map_emoji_name("custom"), "custom");
    }
}
