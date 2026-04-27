//! Channel auth predicates — fail-closed gates for inbound + outbound.
//!
//! Phase 1 (Sprint 21) introduces these predicates and applies the
//! outbound gate at daemon notify call sites. Phase 2 will refactor
//! `TelegramState::is_user_allowed` to use [`is_authorized_recipient`],
//! reversing the legacy `None` → accept-all semantics to fail-closed.
//!
//! Single-source-of-truth for "operator allowlist configured" semantics
//! so Phase 1 outbound gate and Phase 2 inbound gate stay aligned (per
//! Sprint 21 challenge round impl-1 #6 "operator perception risk":
//! both gates closing the same attack class via the same predicate
//! makes the cascade-closed boundary explicit).

/// Inbound auth predicate: is the given user authorised to send commands?
/// Fail-closed: returns `false` when `allowlist` is `None` (unconfigured)
/// or when `user_id` is absent from the configured list.
///
/// **Phase 1 usage**: not yet wired. `TelegramState::is_user_allowed`
/// (`src/channel/telegram.rs:199-204`) retains legacy `None` → `true`
/// semantics until Phase 2 swaps to this fn. Introducing the predicate
/// in Phase 1 lets the outbound gate and the future inbound reform
/// share one definition of "authorised user" rather than two parallel
/// implementations.
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
/// info-bearing notifications to avoid leaking PTY tails (40 lines per
/// stall, plus full crash error output, plus CI rate-limit run urls)
/// to anyone added to a bound Telegram group that has not been
/// auth-configured.
///
/// Closes the Sprint 20.5 cross-validation outbound info-leak finding
/// (Track B peer-pass to Track A `DAEMON.md` §7 critique 1).
pub fn is_outbound_authorized(allowlist: &Option<Vec<i64>>) -> bool {
    matches!(allowlist, Some(list) if !list.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_authorized_recipient_fail_closed_on_none() {
        // Unconfigured allowlist must reject all users (Phase 2 reform
        // direction; not yet active in inbound path).
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
        // `Some([])` semantically equivalent to "explicitly no users
        // allowed" (different from `None` = unconfigured, but observable
        // outcome is the same: nobody passes).
        assert!(!is_authorized_recipient(&Some(vec![]), 42));
    }

    #[test]
    fn is_outbound_authorized_fails_closed_on_none() {
        // Legacy unconfigured deployment: outbound disabled, no leak.
        assert!(!is_outbound_authorized(&None));
    }

    #[test]
    fn is_outbound_authorized_fails_closed_on_empty() {
        // Operator explicitly cleared the allowlist — outbound also off.
        assert!(!is_outbound_authorized(&Some(vec![])));
    }

    #[test]
    fn is_outbound_authorized_passes_on_non_empty() {
        // Operator opted in by configuring at least one authorised user.
        // Outbound notifications now permitted to the bound group.
        assert!(is_outbound_authorized(&Some(vec![1])));
    }
}
