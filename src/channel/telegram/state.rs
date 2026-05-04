use crate::agent::AgentRegistry;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use teloxide::prelude::*;

use super::send::send_with_topic;

/// Lock TelegramState, recovering from poison.
/// With parking_lot::Mutex, lock never fails (no poisoning).
pub(crate) fn lock_state(
    tg: &Arc<Mutex<TelegramState>>,
) -> parking_lot::MutexGuard<'_, TelegramState> {
    tg.lock()
}

/// Shared tokio runtime for all Telegram sync→async calls.
pub(super) fn telegram_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("telegram tokio runtime")
    })
}

/// Run a future on the Telegram runtime. If already inside an async context
/// (e.g. Telegram polling → emit path), spawns a fire-and-forget task on the
/// current runtime to avoid `block_on`-inside-runtime panic. Returns `Ok(())`
/// for the spawned path since the result is not awaited.
///
/// H3: the spawn path returns Ok(()) immediately — errors are logged but not
/// propagated. This is intentional: the caller is in a sync context and cannot
/// await the spawned task. Callers must not assume Ok(()) means the operation
/// succeeded — it means the task was submitted. Check tracing logs for failures.
pub(super) fn spawn_or_block_on<F>(fut: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = fut.await {
                tracing::warn!(%e, "telegram spawn task failed");
            }
        });
        Ok(())
    } else {
        telegram_runtime().block_on(fut)
    }
}

pub struct TelegramState {
    /// `None` only inside the contract-test harness — production `new`
    /// always populates it via `Bot::new`. Transport methods unwrap with
    /// `.expect("telegram bot not initialized")`; contract tests never
    /// reach those paths (see `src/channel/contract.rs` scope comment).
    pub bot: Option<Bot>,
    #[allow(dead_code)]
    pub group_id: ChatId,
    pub topic_to_instance: HashMap<i32, String>,
    #[allow(dead_code)]
    pub instance_to_topic: HashMap<String, i32>,
    pub home: PathBuf,
    /// Submit key per instance (for PTY notification injection).
    pub submit_keys: HashMap<String, String>,
    /// Allowlist of Telegram user IDs permitted to command the fleet.
    /// See [`crate::fleet::ChannelConfig::Telegram::user_allowlist`] for
    /// semantics of `None` vs `Some(empty)` vs `Some([...])`.
    pub user_allowlist: Option<Vec<i64>>,
    /// Wired in post-bootstrap by [`attach_registry`]; lets inbound message
    /// routing read `agent_state` directly instead of via the `LIST` RPC.
    pub registry: Option<AgentRegistry>,
    /// Resolved `fleet_binding` target for cross-instance fleet activity
    /// rendering (Stage B-UX, `docs/archived/DESIGN-stage-b-ux.md` §3/§5). `None`
    /// means no mirror is configured — [`TelegramChannel::apply_fleet_action`]
    /// returns early. Resolution happens in [`init_from_config`] from the
    /// config's `fleet_binding` block plus the on-disk topic registry
    /// sentinel `"__fleet__"`.
    pub fleet_binding_topic_id: Option<i32>,
}

impl TelegramState {
    pub fn new(
        token: &str,
        group_id: i64,
        topic_map: HashMap<String, i32>,
        home: PathBuf,
        submit_keys: HashMap<String, String>,
        user_allowlist: Option<Vec<i64>>,
    ) -> Self {
        let topic_to_instance: HashMap<i32, String> = topic_map
            .iter()
            .map(|(name, &tid)| (tid, name.clone()))
            .collect();
        Self {
            bot: Some(Bot::new(token)),
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            home,
            submit_keys,
            user_allowlist,
            registry: None,
            fleet_binding_topic_id: None,
        }
    }

    /// Build a `TelegramState` without constructing a `teloxide::Bot` —
    /// used by the `src/channel/contract.rs` harness, which only exercises
    /// registry-side methods (`kind`, `has_binding`, `take_binding`,
    /// `record_binding`, `attach_registry`). `Bot::new` eagerly initializes
    /// reqwest + `system-configuration` proxy state and panics on some
    /// macOS setups, so the harness must not go through it. If a test
    /// triggers a transport path (`send_to_topic`, `send_reply`, polling),
    /// the `.expect("telegram bot not initialized")` unwrap will fire.
    #[cfg(test)]
    pub(crate) fn new_for_contract_test(
        group_id: i64,
        topic_map: HashMap<String, i32>,
        home: PathBuf,
        submit_keys: HashMap<String, String>,
        user_allowlist: Option<Vec<i64>>,
    ) -> Self {
        let topic_to_instance: HashMap<i32, String> = topic_map
            .iter()
            .map(|(name, &tid)| (tid, name.clone()))
            .collect();
        Self {
            bot: None,
            group_id: ChatId(group_id),
            topic_to_instance,
            instance_to_topic: topic_map,
            home,
            submit_keys,
            user_allowlist,
            registry: None,
            fleet_binding_topic_id: None,
        }
    }

    /// Return true if a sender is permitted by the allowlist.
    ///
    /// **Sprint 21 Phase 2 fail-closed swap (PR #216 + #217 cascade
    /// auth)**: previously `None` allowlist returned `true` (legacy
    /// accept-all); now it returns `false`. The implementation
    /// delegates to `crate::channel::auth::is_authorized_recipient`,
    /// which is the single source of truth shared with Phase 1's
    /// outbound notify gate. Operators must configure
    /// `user_allowlist: [user_id, ...]` in `fleet.yaml` to enable
    /// inbound (and outbound) traffic — see `docs/USAGE.md` "Channel:
    /// Telegram" section for the migration steps.
    pub fn is_user_allowed(&self, user_id: i64) -> bool {
        crate::channel::auth::is_authorized_recipient(&self.user_allowlist, user_id)
    }

    /// Send a message to an instance's Telegram topic.
    #[allow(dead_code)]
    pub async fn send_to_topic(&self, instance_name: &str, text: &str) -> anyhow::Result<()> {
        let topic_id = self
            .instance_to_topic
            .get(instance_name)
            .ok_or_else(|| anyhow::anyhow!("No topic for '{instance_name}'"))?;
        let bot = self
            .bot
            .as_ref()
            .expect("telegram bot not initialized (contract-test construction?)");
        send_with_topic(bot, self.group_id, Some(*topic_id), text, None).await
    }
}
