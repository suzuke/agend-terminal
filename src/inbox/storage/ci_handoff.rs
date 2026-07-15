use super::{parse_inbox_messages, with_inbox_lock};
use crate::inbox::message::{CiHandoffClass, InboxMessage};
use std::path::Path;

/// Exact row state used by the CI-handoff crash reconciler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtectedHandoffRowState {
    Missing,
    Pending,
    Processed,
    Ambiguous,
}

/// Probe one target inbox under its flock for exactly one protected ci-ready
/// row carrying the requested correlation + episode.
pub(crate) fn protected_handoff_row_state(
    home: &Path,
    target: &str,
    correlation: &str,
    episode: &str,
) -> ProtectedHandoffRowState {
    let Ok(state) = with_inbox_lock(home, target, |path| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return ProtectedHandoffRowState::Missing;
        };
        let mut matches = 0usize;
        let mut processed = false;
        for msg in parse_inbox_messages(&content) {
            if msg.kind.as_deref() != Some("ci-ready-for-action")
                || msg.correlation_id.as_deref() != Some(correlation)
                || msg.ci_handoff_episode.as_deref() != Some(episode)
                || msg.ci_handoff_class != Some(CiHandoffClass::Protected)
            {
                continue;
            }
            matches += 1;
            processed = msg.read_at.is_some() && msg.delivering_at.is_none();
        }
        match (matches, processed) {
            (0, _) => ProtectedHandoffRowState::Missing,
            (1, true) => ProtectedHandoffRowState::Processed,
            (1, false) => ProtectedHandoffRowState::Pending,
            (_, _) => ProtectedHandoffRowState::Ambiguous,
        }
    }) else {
        return ProtectedHandoffRowState::Missing;
    };
    state
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
