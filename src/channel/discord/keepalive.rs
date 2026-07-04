use super::*;
use crate::agent::AgentRegistry;
use crate::channel::{
    BindingRef, Channel, ChannelCapabilities, ChannelError, ChannelEvent, MarkdownDialect,
    MentionStyle, MsgRef, OutMsg, RateBudget,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc;

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
