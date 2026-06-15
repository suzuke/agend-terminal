//! Telegram notification helpers — daemon/supervisor notify to instance topics.

use crate::channel::dedup::DedupKey;
use crate::channel::telegram::error::*;
use crate::channel::telegram::send::*;
use crate::channel::telegram::state::*;

/// CR-2026-06-14: after a successful notify retry to a RECREATED topic, record a
/// dedup claim keyed on the NEW topic id. The original claim (recorded before the
/// first send) is keyed on the now-dead OLD topic id, so without this a
/// concurrent duplicate emission to the recreated topic finds no matching claim
/// and is NOT suppressed — the "same content to same topic within TTL" dedup
/// invariant would lapse precisely across the recreate window.
fn rekey_dedup_for_recreated_topic(
    home: &std::path::Path,
    instance: &str,
    new_tid: i32,
    text: &str,
) {
    let key = DedupKey::new("telegram:notify", instance, Some(i64::from(new_tid)), text);
    let _ = crate::channel::dedup::global(home).record_and_check(key);
}

/// Send a notification to Telegram (instance topic or general).
pub fn notify_telegram(home: &std::path::Path, instance_name: &str, text: &str) {
    let _ = notify_telegram_inner(home, instance_name, text, false);
}

/// Send a notification with Telegram's `disable_notification` flag set — the
/// message still appears in the topic but does not push/vibrate the operator.
pub fn notify_telegram_silent(home: &std::path::Path, instance_name: &str, text: &str) {
    let _ = notify_telegram_inner(home, instance_name, text, true);
}

/// Drives the Telegram send to completion synchronously, returning `Some(())`
/// when a send was attempted or `None` when nothing was sent (no telegram
/// channel / dedup-suppressed).
///
/// H8 (channel-HIGH-1): this MUST drive the send rather than schedule it with
/// `telegram_runtime().spawn(...)` and drop the `JoinHandle`. `telegram_runtime()`
/// is a `new_current_thread` runtime with no persistent driver thread, so a
/// spawned task makes no progress unless some later sync-context `block_on`
/// happens to cooperatively poll it — otherwise the notification is queued
/// forever while the dedup claim suppresses a re-emit for the whole TTL. We use
/// the Handle-guarded `block_on_value` (#1476), mirroring `reply.rs::send_reply`.
fn notify_telegram_inner(
    home: &std::path::Path,
    instance_name: &str,
    text: &str,
    disable_notification: bool,
) -> Option<()> {
    let config = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok()?;
    let (token, group_id, topic_id) = match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            ..
        }) => match std::env::var(bot_token_env) {
            Ok(t) => (
                t,
                *group_id,
                crate::channel::telegram::lookup_topic_for_instance(home, instance_name),
            ),
            Err(_) => return None,
        },
        Some(crate::fleet::ChannelConfig::Discord { .. }) => return None,
        None => return None,
    };

    // #969: channel-wide dedup. If this (telegram, instance, topic,
    // content) was just sent within TTL, suppress. Catches RC1 (dual
    // app/daemon ci_watch poll) and any future regression that fans
    // out the same notification through multiple paths. Cheap O(N)
    // scan on a bounded VecDeque; non-blocking; instrumented.
    let dedup_key = crate::channel::dedup::DedupKey::new(
        "telegram:notify",
        instance_name,
        topic_id.map(i64::from),
        text,
    );
    if !crate::channel::dedup::global(home).record_and_check(dedup_key.clone()) {
        return None;
    }

    let text = text.to_string();
    let home_owned = home.to_path_buf();
    let instance_owned = instance_name.to_string();
    // H8: drive the send to completion on the Telegram runtime via the
    // Handle-guarded `block_on_value` (#1476) — never fire-and-forget onto the
    // undriven current_thread runtime (see fn doc). Blocks the caller for one
    // send, exactly as `reply.rs::send_reply` does.
    block_on_value(async move {
        use teloxide::payloads::SendMessageSetters;
        use teloxide::prelude::Requester;
        let bot = teloxide::Bot::new(&token);
        let chat_id = teloxide::types::ChatId(group_id);
        let result: anyhow::Result<()> = async {
            // Test seam: force the first send to fail without a network call.
            #[cfg(test)]
            if let Some(err) = tests::take_forced_send_error() {
                return Err(err);
            }
            match topic_id {
                Some(tid) if tid != 1 => {
                    let mut req = bot.send_message(chat_id, &text).message_thread_id(
                        teloxide::types::ThreadId(teloxide::types::MessageId(tid)),
                    );
                    if disable_notification {
                        req = req.disable_notification(true);
                    }
                    req.await.map(|_| ()).map_err(anyhow::Error::from)
                }
                _ => {
                    let mut req = bot.send_message(chat_id, &text);
                    if disable_notification {
                        req = req.disable_notification(true);
                    }
                    req.await.map(|_| ()).map_err(anyhow::Error::from)
                }
            }
        }
        .await;
        if let Err(e) = result {
            if let Some(stale_tid) = topic_id {
                if is_topic_deleted_error(&e) {
                    // #969 RC3: pin the topic-deleted detection event for
                    // future-debugging visibility. Series-close defense-in-
                    // depth — old topic is gone so no user-visible duplicate
                    // today, but if a future retry path is added without the
                    // same idempotency guarantee, this log is the breadcrumb
                    // operator greps for to confirm the suspected retry-spam
                    // class.
                    tracing::info!(
                        instance = %instance_owned,
                        topic = stale_tid,
                        error = %e,
                        "#969 RC3: notify topic-deleted detected, recreating + retrying"
                    );
                    if let Some(new_tid) =
                        invalidate_and_recreate_topic(&home_owned, &instance_owned, stale_tid)
                    {
                        tracing::info!(
                            instance = %instance_owned,
                            old_topic = stale_tid,
                            new_topic = new_tid,
                            "notify: retrying with recreated topic"
                        );
                        let mut req = bot.send_message(chat_id, &text).message_thread_id(
                            teloxide::types::ThreadId(teloxide::types::MessageId(new_tid)),
                        );
                        if disable_notification {
                            req = req.disable_notification(true);
                        }
                        // MED-1: roll back the dedup claim if even the recreated-
                        // topic retry fails — a never-delivered notify must not
                        // suppress a same-text re-emit within the TTL (the un-
                        // patched twin of the reply.rs HIGH-2 evict-on-failure).
                        if req.await.is_err() {
                            crate::channel::dedup::global(&home_owned).evict(&dedup_key);
                        } else {
                            // CR-2026-06-14: retry to the recreated topic succeeded
                            // — re-key the dedup claim under the NEW topic id so a
                            // concurrent duplicate to it is still suppressed.
                            rekey_dedup_for_recreated_topic(
                                &home_owned,
                                &instance_owned,
                                new_tid,
                                &text,
                            );
                        }
                        return;
                    }
                }
            }
            // MED-1: terminal failure (send failed, no successful recovery) —
            // evict so a legitimate retry of the same text actually sends.
            crate::channel::dedup::global(&home_owned).evict(&dedup_key);
            tracing::warn!(error = %e, "telegram notify failed");
        }
    });
    Some(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    static FORCED_SEND_ERROR: parking_lot::Mutex<Option<anyhow::Error>> =
        parking_lot::Mutex::new(None);

    pub(super) fn take_forced_send_error() -> Option<anyhow::Error> {
        FORCED_SEND_ERROR.lock().take()
    }
    fn set_forced_send_error(err: anyhow::Error) {
        *FORCED_SEND_ERROR.lock() = Some(err);
    }

    /// Serialize against the process-global env + dedup cache + forced-error seam.
    fn guard() -> parking_lot::MutexGuard<'static, ()> {
        static G: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        G.lock()
    }

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-notify-med1-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn notify_key(home: &Path, instance: &str, text: &str) -> crate::channel::dedup::DedupKey {
        let topic = crate::channel::telegram::lookup_topic_for_instance(home, instance);
        crate::channel::dedup::DedupKey::new(
            "telegram:notify",
            instance,
            topic.map(i64::from),
            text,
        )
    }

    /// §3.9 (MED-1): a notify whose send FAILS must evict its dedup claim, so a
    /// retry of the same text within the TTL actually sends instead of being
    /// suppressed into a no-op. The twin of the reply.rs HIGH-2 fix.
    #[test]
    fn notify_failed_send_evicts_dedup_med1() {
        let _g = guard();
        let home = tmp_home("evict");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "channel:\n  type: telegram\n  bot_token_env: NOTIFY_MED1_TOKEN\n  group_id: -100777\n  mode: topic\ninstances:\n  C:\n    backend: claude\n",
        )
        .unwrap();
        std::env::set_var("NOTIFY_MED1_TOKEN", "fake");

        // First notify: records the dedup claim, then the (forced-failing) send.
        // H8: `notify_telegram_inner` now drives the send synchronously
        // (block_on_value), so by the time it returns the failed send has already
        // evicted the claim — no JoinHandle to drive.
        set_forced_send_error(anyhow::anyhow!("transient network error"));
        notify_telegram_inner(&home, "C", "hello operator", false).expect("send driven");

        // The failed send must have rolled back the claim: record_and_check is
        // fresh again (would be `false`/suppressed without the evict).
        let key = notify_key(&home, "C", "hello operator");
        assert!(
            crate::channel::dedup::global(&home).record_and_check(key),
            "MED-1: a failed notify must evict its dedup claim so a retry can send"
        );

        std::env::remove_var("NOTIFY_MED1_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }
}
