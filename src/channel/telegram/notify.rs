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

/// Claims the per-(instance, topic, content) dedup slot and ENQUEUES the Telegram
/// send onto the bounded delivery worker (AUDIT2-006), so the tick / main-loop
/// caller never blocks on the network round-trip. Returns `Some(())` when the send
/// was accepted for delivery, or `None` when nothing was enqueued (no telegram
/// channel / dedup-suppressed / delivery queue full → dedup claim evicted).
///
/// The actual send runs in [`send_telegram_job`] on the worker thread, which still
/// drives it to completion via the Handle-guarded `block_on_value` (#1476) — never
/// a fire-and-forget `telegram_runtime().spawn(...)` whose `JoinHandle` is dropped
/// onto the driver-less `new_current_thread` runtime (the H8 / channel-HIGH-1
/// invariant; `tests/notify_undriven_runtime_invariant_channel.rs` statically pins
/// it). Moving delivery to the worker thread does not weaken H8: the send is still
/// driven to completion, just off the caller's thread.
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

    // AUDIT2-006: offload the blocking Telegram send (the network round-trip) off
    // the tick / main-loop thread onto the bounded delivery worker. The dedup
    // claim above already ran synchronously, so a concurrent duplicate is still
    // suppressed; if the bounded queue is full the send never happens, so we evict
    // the claim — otherwise a never-delivered notify would suppress a legitimate
    // same-text re-emit for the whole TTL (the queue-full twin of the MED-1
    // send-failure evict below).
    let job = crate::daemon::delivery_worker::TelegramSendJob {
        home: home.to_path_buf(),
        instance: instance_name.to_string(),
        text: text.to_string(),
        disable_notification,
        token,
        group_id,
        topic_id,
        dedup_key: dedup_key.clone(),
    };
    if crate::daemon::delivery_worker::enqueue_telegram_send(job).is_err() {
        crate::channel::dedup::global(home).evict(&dedup_key);
        tracing::warn!(
            instance = %instance_name,
            "AUDIT2-006: telegram notify dropped — delivery queue full; dedup claim evicted"
        );
        return None;
    }
    Some(())
}

/// AUDIT2-006: the actual blocking Telegram send, run on the delivery worker
/// thread (see `daemon::delivery_worker`). The dedup / retry / topic-recreate
/// semantics are unchanged from the former inline `notify_telegram_inner` send —
/// the only behavioural change is the THREAD it runs on. It still drives the send
/// to completion via the Handle-guarded `block_on_value` (#1476) — never a fire-
/// and-forget `spawn` onto the undriven current_thread runtime (the H8 invariant).
pub(crate) fn send_telegram_job(job: crate::daemon::delivery_worker::TelegramSendJob) {
    let crate::daemon::delivery_worker::TelegramSendJob {
        home: home_owned,
        instance: instance_owned,
        text,
        disable_notification,
        token,
        group_id,
        topic_id,
        dedup_key,
    } = job;
    block_on_value(async move {
        use teloxide::payloads::SendMessageSetters;
        use teloxide::prelude::Requester;
        // AUDIT2-006 (A): give the Telegram client an explicit request timeout so
        // a black-holed API connection can't park this delivery indefinitely. We
        // start from teloxide's recommended settings (keep-alive etc.) and only
        // add the timeout. On the (effectively impossible) builder failure, fall
        // back to teloxide's default client — losing the timeout but never the
        // send.
        let bot = match teloxide::net::default_reqwest_settings()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(client) => teloxide::Bot::with_client(&token, client),
            Err(e) => {
                tracing::warn!(error = %e, "AUDIT2-006: telegram client builder failed — using default (no request timeout)");
                teloxide::Bot::new(&token)
            }
        };
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

    /// §3.9 (MED-1): a send that FAILS must evict its dedup claim, so a retry of
    /// the same text within the TTL actually sends instead of being suppressed
    /// into a no-op. The twin of the reply.rs HIGH-2 fix. AUDIT2-006 relocated the
    /// send (and this evict) into `send_telegram_job` on the delivery worker, so
    /// the test now drives that primitive directly (the caller already claimed the
    /// dedup slot, exactly as `notify_telegram_inner` does before enqueueing).
    #[test]
    fn send_failed_evicts_dedup_med1() {
        let _g = guard();
        let home = tmp_home("evict");
        std::env::set_var("NOTIFY_MED1_TOKEN", "fake");

        // Claim the dedup slot as the synchronous caller would, then run the
        // (forced-failing) send: it must roll the claim back.
        let key = notify_key(&home, "C", "hello operator");
        assert!(crate::channel::dedup::global(&home).record_and_check(key.clone()));

        set_forced_send_error(anyhow::anyhow!("transient network error"));
        super::send_telegram_job(crate::daemon::delivery_worker::TelegramSendJob {
            home: home.clone(),
            instance: "C".to_string(),
            text: "hello operator".to_string(),
            disable_notification: false,
            token: "fake".to_string(),
            group_id: -100_777,
            topic_id: crate::channel::telegram::lookup_topic_for_instance(&home, "C"),
            dedup_key: key.clone(),
        });

        // record_and_check is fresh again (would be `false`/suppressed w/o evict).
        assert!(
            crate::channel::dedup::global(&home).record_and_check(key),
            "MED-1: a failed send must evict its dedup claim so a retry can send"
        );

        std::env::remove_var("NOTIFY_MED1_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }

    /// AUDIT2-006 (codex edit 2): when the bounded delivery queue is FULL the send
    /// never happens, so `notify_telegram_inner` must evict the dedup claim it just
    /// recorded — otherwise a never-delivered notify would suppress a legitimate
    /// same-text re-emit for the whole TTL (a new silent-drop class).
    #[test]
    fn telegram_queue_full_evicts_dedup_audit2_006() {
        let _g = guard();
        let home = tmp_home("qfull");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "channel:\n  type: telegram\n  bot_token_env: NOTIFY_MED1_TOKEN\n  group_id: -100777\n  mode: topic\ninstances:\n  C:\n    backend: claude\n",
        )
        .unwrap();
        std::env::set_var("NOTIFY_MED1_TOKEN", "fake");

        // Force the delivery queue to report full: claim recorded, enqueue fails,
        // claim must be evicted; nothing was sent so the call returns None.
        crate::daemon::delivery_worker::test_support::set_force_full(true);
        let sent = notify_telegram_inner(&home, "C", "hello operator", false);
        crate::daemon::delivery_worker::test_support::set_force_full(false);
        assert!(
            sent.is_none(),
            "a full delivery queue means nothing was sent"
        );

        let key = notify_key(&home, "C", "hello operator");
        assert!(
            crate::channel::dedup::global(&home).record_and_check(key),
            "AUDIT2-006: a queue-full drop must evict the dedup claim so a retry can send"
        );

        std::env::remove_var("NOTIFY_MED1_TOKEN");
        std::fs::remove_dir_all(&home).ok();
    }
}
