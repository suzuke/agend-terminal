//! Opt-in business delivery for a job worker. Distinct from the status-only
//! notification: the worker supplies the content, no header budget applies, and
//! long content is split into transport-sized chunks so nothing is truncated.
//! The destination is only ever the validated `job.worker_topic`; no topic is
//! created and no registry row is owned, so worker teardown can never delete a
//! shared topic.
use super::{config::JobNotification, Run};
use std::path::Path;

/// Transport chunk size in chars. Telegram's hard limit is 4096; stay below it
/// with headroom for the platform's own framing. This bounds each message, not
/// the delivery: the full content is sent across as many chunks as needed.
const CHUNK_CHARS: usize = 4000;

pub(crate) fn send(home: &Path, run: &Run, content: &str) -> anyhow::Result<Vec<String>> {
    send_with(home, run, content, |endpoint, text| {
        super::notification::send_telegram_text(home, endpoint, text)
    })
}

fn send_with(
    _home: &Path,
    run: &Run,
    content: &str,
    mut transport: impl FnMut(&JobNotification, &str) -> anyhow::Result<String>,
) -> anyhow::Result<Vec<String>> {
    let endpoint = run
        .config
        .worker_topic
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no business delivery endpoint configured"))?;
    endpoint.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(!content.trim().is_empty(), "empty delivery content");
    let mut receipts = Vec::new();
    for chunk in chunk_text(content, CHUNK_CHARS) {
        receipts.push(transport(endpoint, chunk)?);
    }
    Ok(receipts)
}

/// Split `text` into pieces of at most `limit` chars without breaking UTF-8 or
/// losing a byte. Prefers the last newline inside a window so posts stay
/// readable; falls back to a hard char cut when one line exceeds the limit.
fn chunk_text(text: &str, limit: usize) -> Vec<&str> {
    assert!(limit > 0);
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > limit {
        let byte_limit = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..byte_limit];
        let cut = window
            .rfind('\n')
            .map(|i| i + 1)
            .filter(|&i| i > 0)
            .unwrap_or(byte_limit);
        chunks.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        chunks.push(rest);
    }
    chunks
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::schedule_jobs::config::JobNotification;
    use crate::schedule_jobs::{DeliveryState, NotificationState, Phase};

    fn run_with(content_topic: Option<i32>) -> Run {
        Run {
            id: "r-delivery".into(),
            schedule_id: "s-delivery".into(),
            scheduled_at: 1,
            created_by: "operator".into(),
            message: "task".into(),
            config: super::super::config::JobConfig {
                backends: vec!["codex".into()],
                artifact_directory: std::path::PathBuf::from("/tmp/artifacts"),
                timeout_secs: 60,
                max_attempts: 1,
                retry_delay_secs: 1,
                output_context: String::new(),
                notification: None,
                auto_cleanup: false,
                cleanup_retry_secs: 60,
                worker_topic: content_topic.map(|topic_id| JobNotification::Telegram {
                    chat_id: -100123,
                    topic_id: Some(topic_id),
                }),
            },
            phase: Phase::Running,
            revision: 0,
            dispatch_intent: None,
            attempt: None,
            previous_attempts: vec![],
            task_id: None,
            result: None,
            error: None,
            next_attempt_at: 1,
            deadline: 100,
            cleanup_pending: false,
            cleanup_started_at: None,
            recovery_required: false,
            recovery_resolution: None,
            task_settled: false,
            notification: NotificationState::NotRequested,
            notification_receipt: None,
            notification_error: None,
            delivery: DeliveryState::NotRequested,
        }
    }

    #[test]
    fn long_content_is_delivered_chunked_to_the_explicit_topic_without_a_3500_cap() {
        let run = run_with(Some(12153));
        let content = "摘要".repeat(6000); // 12000 chars, well past the notice cap
        let mut seen: Vec<(i32, usize)> = Vec::new();
        let receipts = send_with(
            Path::new("/nonexistent"),
            &run,
            &content,
            |endpoint, text| {
                let JobNotification::Telegram { chat_id, topic_id } = endpoint;
                assert_eq!(*chat_id, -100123);
                seen.push((topic_id.unwrap(), text.chars().count()));
                Ok(format!("id-{}", seen.len()))
            },
        )
        .unwrap();
        assert!(seen.iter().all(|(topic, _)| *topic == 12153));
        assert_eq!(
            seen.iter().map(|(_, len)| *len).sum::<usize>(),
            content.chars().count(),
            "every char must be delivered across chunks"
        );
        assert!(seen.iter().all(|(_, len)| *len <= 4096));
        assert!(seen.len() >= 3, "12000 chars needs several chunks");
        assert_eq!(receipts.len(), seen.len());
    }

    #[test]
    fn unconfigured_worker_topic_refuses_to_send() {
        let run = run_with(None);
        let calls = std::cell::Cell::new(0);
        let error = send_with(Path::new("/nonexistent"), &run, "hello", |_, _| {
            calls.set(calls.get() + 1);
            Ok("id".into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("no business delivery endpoint"));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn chunking_preserves_exact_bytes_and_prefers_newlines() {
        let text = format!("{}\n{}", "a".repeat(2500), "b".repeat(2500));
        let chunks = chunk_text(&text, 3000);
        assert_eq!(chunks.concat(), text);
        assert!(chunks[0].ends_with('\n'));
    }
}
