use super::{parse_inbox_messages, with_inbox_lock};
use crate::inbox::message::{CiHandoffClass, InboxMessage};
use std::io::Write;
use std::path::Path;

/// Exact row state used by the CI-handoff crash reconciler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtectedHandoffRowState {
    Missing,
    Pending,
    /// Row looks processed (read_at set, delivering_at cleared) but was NOT
    /// explicitly settled via `ack_handoff` — generic drain/ack caused this.
    Processed,
    /// Row was explicitly settled via `settle_ci_handoff_row_exact` (carries
    /// `ci_handoff_settlement` provenance). Safe for reconciler cleanup.
    ExplicitlyAcked,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoffRowSettleOutcome {
    Settled,
    AlreadySettled,
    Missing,
    Ambiguous,
    WriteFailed,
    LockFailed,
}

pub(crate) fn handoff_row_state(
    home: &Path,
    target: &str,
    correlation: &str,
    episode: &str,
    class: CiHandoffClass,
) -> ProtectedHandoffRowState {
    let Ok(state) = with_inbox_lock(home, target, |path| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return ProtectedHandoffRowState::Missing;
        };
        let matches: Vec<InboxMessage> = parse_inbox_messages(&content)
            .filter(|msg| {
                msg.kind.as_deref() == Some("ci-ready-for-action")
                    && msg.correlation_id.as_deref() == Some(correlation)
                    && msg.ci_handoff_episode.as_deref() == Some(episode)
                    && msg.ci_handoff_class == Some(class)
            })
            .collect();
        match matches.as_slice() {
            [] => ProtectedHandoffRowState::Missing,
            [msg] if msg.read_at.is_some() && msg.delivering_at.is_none() => {
                if msg.ci_handoff_settlement.is_some() {
                    ProtectedHandoffRowState::ExplicitlyAcked
                } else {
                    ProtectedHandoffRowState::Processed
                }
            }
            [_] => ProtectedHandoffRowState::Pending,
            _ => ProtectedHandoffRowState::Ambiguous,
        }
    }) else {
        return ProtectedHandoffRowState::Missing;
    };
    state
}

/// Settle exactly one CI handoff row without touching the sidecar track.
/// The caller performs the episode-CAS track delete only after this durable
/// write completes, so the two file locks are never nested.
pub(crate) fn settle_ci_handoff_row_exact(
    home: &Path,
    target: &str,
    correlation: &str,
    episode: &str,
    class: CiHandoffClass,
) -> HandoffRowSettleOutcome {
    with_inbox_lock(home, target, |path| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return HandoffRowSettleOutcome::Missing;
        };
        let mut messages = Vec::new();
        let mut raw_lines = Vec::new();
        let mut match_indexes = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<InboxMessage>(line) {
                Ok(msg) => {
                    if msg.kind.as_deref() == Some("ci-ready-for-action")
                        && msg.correlation_id.as_deref() == Some(correlation)
                        && msg.ci_handoff_episode.as_deref() == Some(episode)
                        && msg.ci_handoff_class == Some(class)
                    {
                        match_indexes.push(messages.len());
                    }
                    messages.push(msg);
                }
                Err(_) => raw_lines.push(line.to_string()),
            }
        }
        let [index] = match_indexes.as_slice() else {
            return if match_indexes.is_empty() {
                HandoffRowSettleOutcome::Missing
            } else {
                HandoffRowSettleOutcome::Ambiguous
            };
        };
        let msg = &mut messages[*index];
        if msg.ci_handoff_settlement.is_some() {
            return HandoffRowSettleOutcome::AlreadySettled;
        }
        msg.read_at = Some(chrono::Utc::now().to_rfc3339());
        msg.delivering_at = None;
        msg.ci_handoff_settlement = Some("ack_handoff".to_string());
        let tmp = path.with_extension("jsonl.tmp");
        let write = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            for message in messages {
                writeln!(file, "{}", serde_json::to_string(&message)?)?;
            }
            for raw in raw_lines {
                writeln!(file, "{raw}")?;
            }
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            crate::store::fsync_parent_dir(path);
            Ok(())
        })();
        if write.is_ok() {
            HandoffRowSettleOutcome::Settled
        } else {
            HandoffRowSettleOutcome::WriteFailed
        }
    })
    .unwrap_or(HandoffRowSettleOutcome::LockFailed)
}

/// Return the exact protected CI-handoff identity carried by a row.
pub(crate) fn protected_settlement_identity(
    msg: &InboxMessage,
    target: &str,
) -> Option<(String, String, String)> {
    if msg.kind.as_deref() != Some("ci-ready-for-action")
        || msg.ci_handoff_class != Some(CiHandoffClass::Protected)
    {
        return None;
    }
    Some((
        target.to_string(),
        msg.correlation_id.clone()?,
        msg.ci_handoff_episode.clone()?,
    ))
}
