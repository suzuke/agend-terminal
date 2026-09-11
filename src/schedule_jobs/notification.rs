//! A single terminal notice attempt. The controller durably marks Sending
//! BEFORE invoking this service; an interrupted/ambiguous attempt is Unknown,
//! never blindly resent. This module does not retry transport requests.
use super::{config::JobNotification, Phase, Run};
use crate::channel::{BindingRef, Channel, OutMsg};
use std::path::Path;

pub(crate) fn send(home: &Path, run: &Run) -> anyhow::Result<String> {
    send_with(home, run, |endpoint, text| {
        let credentials = crate::channel::telegram::resolve_channel_only_from(home)?;
        let JobNotification::Telegram { chat_id, topic_id } = endpoint;
        // Resolve again at the send boundary: fleet reload must not redirect an
        // already admitted Run to a different group.
        anyhow::ensure!(
            *chat_id == credentials.group_id,
            "notification group changed"
        );
        let state = crate::channel::telegram::TelegramState::new(
            &credentials.token,
            *chat_id,
            Default::default(),
            home.into(),
            Default::default(),
            None,
        );
        let adapter = crate::channel::telegram::TelegramChannel::new(std::sync::Arc::new(
            parking_lot::Mutex::new(state),
        ));
        let binding = match topic_id {
            Some(topic_id) => BindingRef::new(
                "telegram",
                None,
                crate::channel::telegram::TelegramBindingPayload {
                    topic_id: *topic_id,
                },
            ),
            None => BindingRef::new("telegram", None, ()),
        };
        // Channel::send awaits Telegram's response and returns its message id.
        // No instance binding or create_topic path is involved.
        let sent = adapter.send(&binding, OutMsg::text(text))?;
        anyhow::ensure!(
            sent.id.parse::<i32>().is_ok_and(|id| id > 0),
            "Telegram returned no valid message id"
        );
        Ok(sent.id)
    })
}

fn send_with(
    home: &Path,
    run: &Run,
    transport: impl FnOnce(&JobNotification, &str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    let endpoint = run
        .config
        .notification
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no notification endpoint configured"))?;
    endpoint
        .validate_for_home(home)
        .map_err(anyhow::Error::msg)?;
    let status = match run.phase {
        Phase::Succeeded => "succeeded",
        Phase::Failed => "failed",
        _ if run.recovery_required => "recovery_required",
        _ => anyhow::bail!("only terminal or recovery-required runs may notify"),
    };
    let detail = if run.phase == Phase::Succeeded {
        run.result.as_deref()
    } else {
        run.error.as_deref()
    };
    let recovery = if run.recovery_required {
        "Manual recovery required: confirm worker and background tools stopped, reconcile delivery, delete the worker, then use the operator admin resolve-job-recovery command. No automatic handoff.\n"
    } else {
        ""
    };
    let header = format!(
        "Scheduled job {status}\n{recovery}Schedule: {}\nRun: {}\nTask: {}\n",
        run.schedule_id,
        run.id,
        run.task_id.as_deref().unwrap_or("none")
    );
    // One Telegram message only. Truncate the detail without splitting UTF-8;
    // complete output remains in Run.result and retained artifact files.
    let budget = 3500usize.saturating_sub(header.chars().count());
    let text = format!(
        "{header}{}",
        detail
            .unwrap_or("No details recorded")
            .chars()
            .take(budget)
            .collect::<String>()
    );
    transport(endpoint, &text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture() -> (super::super::tests::TempHome, Run) {
        let home = super::super::tests::TempHome::new();
        std::fs::write(crate::fleet::fleet_yaml_path(home.path()),
            "instances: {}\nchannel:\n  type: telegram\n  bot_token_env: JOB_NOTIFICATION_UNUSED_TEST_TOKEN\n  group_id: -100123\n").unwrap();
        let schedule: crate::schedules::Schedule = serde_json::from_value(serde_json::json!({
            "id":"notice-test", "message":"task", "created_at":"2026-01-01T00:00:00Z", "timezone":"UTC",
            "trigger":{"kind":"once", "at":"2026-01-02T00:00:00Z"},
            "job":{"backends":["codex"], "artifact_directory":home.path().join("artifacts"),
                "notification":{"channel":"telegram","chat_id":-100123,"topic_id":42}}
        })).unwrap();
        super::super::admit_due(home.path(), &schedule, chrono::Utc::now()).unwrap();
        let mut run = super::super::read(home.path()).unwrap().runs.remove(0);
        run.phase = Phase::Succeeded;
        run.result = Some("摘要已完成".repeat(2000));
        (home, run)
    }

    #[test]
    fn notification_uses_explicit_topic_and_captures_id_in_one_send() {
        let (home, run) = fixture();
        let id = send_with(home.path(), &run, |endpoint, text| {
            assert_eq!(
                *endpoint,
                JobNotification::Telegram {
                    chat_id: -100123,
                    topic_id: Some(42)
                }
            );
            assert!(text.contains(&run.id));
            assert!(text.contains(run.task_id.as_deref().unwrap()));
            assert!(text.contains("succeeded"));
            assert!(text.chars().count() <= 3500);
            Ok("321".into())
        })
        .unwrap();
        assert_eq!(id, "321");
    }

    #[test]
    fn notification_rejects_changed_group_before_transport() {
        let (home, mut run) = fixture();
        run.config.notification = Some(JobNotification::Telegram {
            chat_id: -999,
            topic_id: Some(42),
        });
        let error = send_with(home.path(), &run, |_, _| {
            panic!("must not send to another group")
        })
        .unwrap_err();
        assert!(error.to_string().contains("configured Telegram group"));
    }

    #[test]
    fn notification_propagates_ambiguous_error_without_retry() {
        let (home, mut run) = fixture();
        run.phase = Phase::Failed;
        run.error = Some("usage limit".into());
        let calls = std::cell::Cell::new(0);
        let error = send_with(home.path(), &run, |_, text| {
            calls.set(calls.get() + 1);
            assert!(text.contains("failed"));
            assert!(text.contains("usage limit"));
            anyhow::bail!("response lost after send")
        })
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(error.to_string().contains("response lost"));
    }
}
