//! Channel auth predicates — fail-closed gates for inbound + outbound.
//!
//! Single-source-of-truth for "operator allowlist configured" semantics
//! so the outbound gate and inbound gate stay aligned (both gates
//! closing the same attack class via the same predicate makes the
//! cascade-closed boundary explicit).

/// Inbound auth predicate: is the given user authorised to send commands?
/// Fail-closed: returns `false` when `allowlist` is `None` (unconfigured)
/// or when `user_id` is absent from the configured list.
pub fn is_authorized_recipient(allowlist: &Option<Vec<i64>>, user_id: i64) -> bool {
    match allowlist {
        Some(list) => list.contains(&user_id),
        None => false,
    }
}

/// Outbound notify gate: returns `true` iff an explicit non-empty
/// operator allowlist is configured.
///
/// When this returns `false`, [`super::gated_notify`] drops outbound
/// info-bearing notifications to avoid leaking PTY tails to anyone
/// added to a bound Telegram group that has not been auth-configured.
pub fn is_outbound_authorized(allowlist: &Option<Vec<i64>>) -> bool {
    matches!(allowlist, Some(list) if !list.is_empty())
}

/// Warn once per `(channel_kind, instance)` pair when the channel-level
/// `user_allowlist` gate fires (i.e. `outbound_authorized() == false`).
///
/// Backed by a global `Mutex<HashSet<String>>` so the once-per-process
/// guarantee is per-channel-kind-per-instance-name.
pub fn warn_once_user_allowlist_unconfigured(channel_kind: &str, instance: &str) {
    use parking_lot::Mutex;
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let key = format!("{channel_kind}:{instance}");
    let first_time = seen.lock().insert(key);
    if first_time {
        tracing::error!(
            channel_kind,
            instance,
            "FATAL: channel '{channel_kind}' notify dropped for instance '{instance}' — \
             user_allowlist NOT CONFIGURED. The channel cannot deliver outbound \
             notifications (stall / recovery / crash / CI alerts) until the operator \
             allowlist is set. Add to fleet.yaml NOW:\n  \
             channel:\n    type: {channel_kind}\n    user_allowlist:\n      - <YOUR_TELEGRAM_USER_ID>\n\
             See docs/USAGE.md \"Channel: Telegram\" section for details."
        );
    } else {
        tracing::debug!(
            channel_kind,
            instance,
            "user_allowlist still not configured (already warned once for this channel:instance pair)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_authorized_recipient_fail_closed_on_none() {
        assert!(!is_authorized_recipient(&None, 42));
    }

    #[test]
    fn is_authorized_recipient_rejects_unlisted_user() {
        assert!(!is_authorized_recipient(&Some(vec![1, 2, 3]), 42));
    }

    #[test]
    fn is_authorized_recipient_accepts_listed_user() {
        assert!(is_authorized_recipient(&Some(vec![1, 42, 3]), 42));
    }

    #[test]
    fn is_authorized_recipient_empty_list_rejects_all() {
        assert!(!is_authorized_recipient(&Some(vec![]), 42));
    }

    #[test]
    fn is_outbound_authorized_fails_closed_on_none() {
        assert!(!is_outbound_authorized(&None));
    }

    #[test]
    fn is_outbound_authorized_fails_closed_on_empty() {
        assert!(!is_outbound_authorized(&Some(vec![])));
    }

    #[test]
    fn is_outbound_authorized_passes_on_non_empty() {
        assert!(is_outbound_authorized(&Some(vec![1])));
    }

    #[test]
    fn warn_once_user_allowlist_unconfigured_does_not_panic() {
        warn_once_user_allowlist_unconfigured("telegram", "test_agent_p1_5");
        warn_once_user_allowlist_unconfigured("telegram", "test_agent_p1_5");
        warn_once_user_allowlist_unconfigured("telegram", "another_agent");
        warn_once_user_allowlist_unconfigured("discord", "test_agent_p1_5");
    }
}
