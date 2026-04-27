//! Channel auth predicates — fail-closed gates for inbound + outbound.
//!
//! Phase 1 (Sprint 21) introduces these predicates and applies the
//! outbound gate at daemon notify call sites. Phase 2 will refactor
//! `TelegramState::is_user_allowed` to use [`is_authorized_recipient`],
//! reversing the legacy `None` → accept-all semantics to fail-closed.
//! Phase 5b (post triangulation audit C1) adds [`ChannelOpKind`] + the
//! per-instance `outbound_capabilities` declarative gate consumed by
//! `Channel::send_from_agent` to cover the four MCP→Channel bridge
//! fns (`reply` / `react` / `edit_message` / `delegate_task`
//! provenance).
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

// ---------------------------------------------------------------------------
// Phase 5b: per-instance outbound capability gate (covers MCP→Channel bridge
// surface that bypassed Phase 1's daemon-only `gated_notify`).
// ---------------------------------------------------------------------------

/// Kind discriminator for agent-callable outbound operations. Per-instance
/// `outbound_capabilities: Option<Vec<ChannelOpKind>>` in `fleet.yaml`
/// declares which kinds an instance is allowed to emit.
///
/// `None` (field absent) → gradual-migration permissive default + once-per-
/// instance deprecation warn. Sprint 22 hard-cut PR will flip default to
/// fail-closed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, std::hash::Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChannelOpKind {
    /// `reply` MCP tool — agent sends a free-form message into its bound topic.
    Reply,
    /// `react` MCP tool — agent attaches an emoji reaction to an existing message.
    React,
    /// `edit_message` MCP tool — agent edits a previously-sent message.
    Edit,
    /// `delegate_task` provenance side-channel — daemon-internal injection of
    /// a "who delegated this" tag to the receiving agent's topic.
    InjectProvenance,
}

impl ChannelOpKind {
    /// Stable, log-friendly token for use in tracing fields and error
    /// messages. Matches the YAML serialised form (snake_case).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::React => "react",
            Self::Edit => "edit",
            Self::InjectProvenance => "inject_provenance",
        }
    }
}

/// Decision returned by [`evaluate_outbound_capability`] — explicit enum so
/// callers can distinguish "permitted (configured)" from "permitted under
/// gradual-migration grace" (the latter must emit the deprecation warn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundCapabilityDecision {
    /// `outbound_capabilities` includes the requested op — proceed normally.
    Allowed,
    /// `outbound_capabilities` is set but does NOT include the op — reject.
    Rejected,
    /// `outbound_capabilities` is `None` (config absent) — Phase 5b
    /// gradual-migration permissive default. Caller MUST emit the
    /// once-per-instance deprecation warn so operators see the migration
    /// template before Sprint 22's hard-cut.
    PermissiveLegacyMissing,
}

/// Pure decision: given the configured `outbound_capabilities` Vec for an
/// instance and the requested op, return whether to allow.
///
/// Decoupled from fleet.yaml IO so it's unit-testable in isolation. Real
/// callers in `Channel::send_from_agent` impls load the config from disk
/// then dispatch to this fn for the policy decision.
pub fn evaluate_outbound_capability(
    capabilities: Option<&[ChannelOpKind]>,
    requested: ChannelOpKind,
) -> OutboundCapabilityDecision {
    match capabilities {
        Some(caps) if caps.contains(&requested) => OutboundCapabilityDecision::Allowed,
        Some(_) => OutboundCapabilityDecision::Rejected,
        None => OutboundCapabilityDecision::PermissiveLegacyMissing,
    }
}

/// Once-per-instance deprecation-warn guard — `Channel::send_from_agent`
/// impls call this on every `PermissiveLegacyMissing` decision; the first
/// call per instance emits the migration template, subsequent calls log
/// at debug level.
///
/// Backed by a global `Mutex<HashSet<String>>` so the once-per-process
/// guarantee is per-instance-name, not per-call.
pub fn warn_once_outbound_capabilities_missing(instance: &str, op: ChannelOpKind) {
    use std::sync::Mutex;
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let first_time = match seen.lock() {
        Ok(mut set) => set.insert(instance.to_string()),
        // Poisoned lock — fall through to "log every time" rather than
        // silently drop the migration warn (deprecation visibility wins
        // over log spam).
        Err(_) => true,
    };
    if first_time {
        // Sprint 22 P0 (Phase 5b hard-cut, 2-stage transition per
        // d-20260427042738203707-13): elevated to `error!` for FATAL
        // visibility — Sprint 23 will switch to a hard parse error.
        // Operator MUST add the field this sprint window.
        tracing::error!(
            instance,
            op = op.as_str(),
            "FATAL (warn-but-permit one daemon cycle): instance '{instance}' \
             outbound_capabilities NOT SET. Sprint 22 P0 grants this {op_str} call \
             under gradual-migration grace. Sprint 23 will fail-closed (hard parse \
             error on missing field). Add to fleet.yaml NOW:\n  \
             instances.{instance}.outbound_capabilities: \
             [reply, react, edit, inject_provenance]\nSee docs/USAGE.md \"Channel: \
             Telegram\" + docs/MIGRATION-OUTBOUND-CAPS.md for details.",
            op_str = op.as_str()
        );
    } else {
        tracing::debug!(
            instance,
            op = op.as_str(),
            "outbound_capabilities still not set (already warned once)"
        );
    }
}

/// Sprint 22 P1.5 (Candidate 4) — sibling of
/// [`warn_once_outbound_capabilities_missing`] for the *channel-level*
/// `user_allowlist` gate, fired by [`super::gated_notify`] when the
/// channel reports `outbound_authorized() == false`.
///
/// Why a separate helper: `outbound_capabilities` is per-instance
/// per-op (`ChannelOpKind`) gating; `user_allowlist` is per-channel
/// (entire daemon-driven outbound surface). Different config, different
/// op shape, different copy-paste fix. Keeping them as siblings — same
/// once-per-process Mutex+HashSet pattern, different keying — beats
/// shoehorning a single generic helper.
///
/// Pattern alignment: P0's
/// [`warn_once_outbound_capabilities_missing`] established the FATAL
/// visibility convention (per-instance Mutex<HashSet>, `error!` not
/// `debug!`, copy-paste fleet.yaml stanza in the message body). This
/// helper mirrors that shape so operators see the same operator-
/// actionable shape regardless of which gate fired.
///
/// Backed by a global `Mutex<HashSet<String>>` keyed on
/// `(channel_kind, instance)` so the once-per-process guarantee is
/// per-channel-kind-per-instance-name (the gate fires per-instance per
/// daemon cycle but a single FATAL line per restart is enough — repeats
/// would spam without adding info).
pub fn warn_once_user_allowlist_unconfigured(channel_kind: &str, instance: &str) {
    use std::sync::Mutex;
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let key = format!("{channel_kind}:{instance}");
    let first_time = match seen.lock() {
        Ok(mut set) => set.insert(key),
        // Poisoned lock — fall through to "log every time" rather than
        // silently drop the visibility (mirrors the P0 helper's bias).
        Err(_) => true,
    };
    if first_time {
        // Sprint 22 P1.5 (Candidate 4 from PR #229 P1 dispatch): the
        // existing `tracing::debug!` was invisible at default
        // `RUST_LOG=info`, so operators couldn't see the gate firing
        // when stall / crash / CI notices were silently dropped. The
        // FATAL severity matches P0's outbound-caps helper — a fully
        // unconfigured deployment is a noisy bring-up issue, not a
        // background warning.
        tracing::error!(
            channel_kind,
            instance,
            "FATAL: channel '{channel_kind}' notify dropped for instance '{instance}' — \
             user_allowlist NOT CONFIGURED. The channel cannot deliver outbound \
             notifications (stall / recovery / crash / CI alerts) until the operator \
             allowlist is set. Add to fleet.yaml NOW:\n  \
             channel:\n    type: {channel_kind}\n    user_allowlist:\n      - <YOUR_TELEGRAM_USER_ID>\n\
             See docs/USAGE.md \"Channel: Telegram\" section for details (PR #216 + Sprint 22 P0)."
        );
    } else {
        tracing::debug!(
            channel_kind,
            instance,
            "user_allowlist still not configured (already warned once for this channel:instance pair)"
        );
    }
}

/// Sprint 22 P0 — shared `gate_outbound_for_agent` helper consumed by every
/// `Channel::send_from_agent` impl (Telegram + future Discord/Slack/Teams).
///
/// Centralises the per-agent capability check so:
/// 1. Future channel adapters cannot accidentally bypass the gate by
///    forgetting to call `evaluate_outbound_capability` in their impl.
/// 2. The `PermissiveLegacyMissing` deprecation warn is consistent across
///    adapters (one helper, one warn line, one migration message).
/// 3. The (PermissiveLegacyMissing → permit + warn) branch is testable in
///    isolation without spinning up a real channel.
///
/// Returns `Ok(())` when the agent is permitted to emit `op` (either
/// explicitly listed OR under gradual-migration grace). Returns
/// `Err(ChannelError::Other)` with a typed error message when explicitly
/// rejected (capability list set but does NOT include `op`).
///
/// IO note: reads `<home>/fleet.yaml` on each call. For per-call frequency
/// in production this is acceptable (operator config rarely changes
/// mid-flight); a future hot-reload optimisation is tracked in dispatch
/// d-20260427042738203707-13 deferred items (Sprint 23+ candidate).
pub fn gate_outbound_for_agent(
    home: &std::path::Path,
    agent: &str,
    op: ChannelOpKind,
) -> std::result::Result<(), super::ChannelError> {
    let caps = lookup_outbound_capabilities(home, agent);
    match evaluate_outbound_capability(caps.as_deref(), op) {
        OutboundCapabilityDecision::Allowed => Ok(()),
        OutboundCapabilityDecision::Rejected => Err(super::ChannelError::Other(anyhow::anyhow!(
            "instance '{agent}' outbound_capabilities does not include '{op_str}' \
             — add to fleet.yaml or remove the explicit list to opt into the \
             gradual-migration permissive default",
            op_str = op.as_str()
        ))),
        OutboundCapabilityDecision::PermissiveLegacyMissing => {
            warn_once_outbound_capabilities_missing(agent, op);
            Ok(())
        }
    }
}

/// Look up `outbound_capabilities` for `instance` from `fleet.yaml`.
/// Returns `None` when the file / instance / field is missing — caller
/// treats `None` as the gradual-migration permissive default.
///
/// Sprint 22 P0: extracted from `src/channel/telegram.rs` so all
/// `Channel::send_from_agent` impls share one lookup path.
fn lookup_outbound_capabilities(
    home: &std::path::Path,
    instance: &str,
) -> Option<Vec<ChannelOpKind>> {
    let fleet_path = home.join("fleet.yaml");
    let cfg = crate::fleet::FleetConfig::load(&fleet_path).ok()?;
    cfg.instances
        .get(instance)
        .and_then(|i| i.outbound_capabilities.clone())
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

    // ---- Phase 5b: ChannelOpKind + outbound capability decision ----

    #[test]
    fn channel_op_kind_as_str_matches_serde_form() {
        // `as_str` is consumed by tracing fields / error messages; must
        // match the snake_case form used by serde so log lines and
        // YAML stay aligned.
        assert_eq!(ChannelOpKind::Reply.as_str(), "reply");
        assert_eq!(ChannelOpKind::React.as_str(), "react");
        assert_eq!(ChannelOpKind::Edit.as_str(), "edit");
        assert_eq!(
            ChannelOpKind::InjectProvenance.as_str(),
            "inject_provenance"
        );
    }

    #[test]
    fn evaluate_outbound_capability_allowed_when_listed() {
        let caps = vec![ChannelOpKind::Reply, ChannelOpKind::React];
        assert_eq!(
            evaluate_outbound_capability(Some(&caps), ChannelOpKind::Reply),
            OutboundCapabilityDecision::Allowed
        );
        assert_eq!(
            evaluate_outbound_capability(Some(&caps), ChannelOpKind::React),
            OutboundCapabilityDecision::Allowed
        );
    }

    #[test]
    fn evaluate_outbound_capability_rejected_when_not_listed() {
        let caps = vec![ChannelOpKind::Reply];
        assert_eq!(
            evaluate_outbound_capability(Some(&caps), ChannelOpKind::Edit),
            OutboundCapabilityDecision::Rejected
        );
        assert_eq!(
            evaluate_outbound_capability(Some(&caps), ChannelOpKind::InjectProvenance),
            OutboundCapabilityDecision::Rejected
        );
    }

    #[test]
    fn evaluate_outbound_capability_permissive_legacy_when_missing() {
        // No config: gradual-migration permissive grace. Sprint 22
        // hard-cut PR will flip this to Rejected.
        assert_eq!(
            evaluate_outbound_capability(None, ChannelOpKind::Reply),
            OutboundCapabilityDecision::PermissiveLegacyMissing
        );
    }

    #[test]
    fn evaluate_outbound_capability_empty_list_rejects_all() {
        // `Some([])` is "explicitly no operations allowed" — distinct from
        // `None` (legacy missing). This matches Sprint 21 Phase 1's
        // `is_outbound_authorized` semantics for the allowlist case.
        let caps = vec![];
        assert_eq!(
            evaluate_outbound_capability(Some(&caps), ChannelOpKind::Reply),
            OutboundCapabilityDecision::Rejected
        );
    }

    #[test]
    fn channel_op_kind_yaml_round_trip_realistic_capabilities() {
        // Operator-pitfall regression (per Phase 5b dispatch constraint #3):
        // YAML round-trip must deserialise as Vec<ChannelOpKind>, not
        // Vec<String>. Future serde refactor that loses #[serde(rename_all)]
        // would silently truncate operator config.
        let yaml = r#"
- reply
- react
- edit
- inject_provenance
"#;
        let parsed: Vec<ChannelOpKind> = serde_yaml::from_str(yaml).expect("yaml deserialise");
        assert_eq!(
            parsed,
            vec![
                ChannelOpKind::Reply,
                ChannelOpKind::React,
                ChannelOpKind::Edit,
                ChannelOpKind::InjectProvenance,
            ]
        );

        // Round-trip back to YAML and re-parse to lock contract.
        let serialized = serde_yaml::to_string(&parsed).expect("yaml serialise");
        let reparsed: Vec<ChannelOpKind> =
            serde_yaml::from_str(&serialized).expect("yaml round-trip deserialise");
        assert_eq!(reparsed, parsed);
    }

    #[test]
    fn warn_once_outbound_capabilities_missing_does_not_panic() {
        // Smoke test: helper must be re-entrant + survive concurrent calls.
        // The actual once-per-instance behaviour is verified by reading
        // tracing output (operator-visible), not by this unit test.
        warn_once_outbound_capabilities_missing("test_agent_phase5b", ChannelOpKind::Reply);
        warn_once_outbound_capabilities_missing("test_agent_phase5b", ChannelOpKind::React);
        warn_once_outbound_capabilities_missing("another_agent", ChannelOpKind::Edit);
    }

    #[test]
    fn warn_once_user_allowlist_unconfigured_does_not_panic() {
        // Smoke test (Sprint 22 P1.5 Candidate 4): sister helper for the
        // user_allowlist gate must be re-entrant + survive concurrent
        // calls, same shape as
        // `warn_once_outbound_capabilities_missing`. Once-per-pair
        // behaviour is observable in tracing output (operator-visible
        // `error!` on first call per `(channel_kind, instance)` pair,
        // `debug!` on subsequent calls); not verified at unit-test
        // level because tracing-test fixture would couple this to the
        // global subscriber.
        warn_once_user_allowlist_unconfigured("telegram", "test_agent_p1_5");
        warn_once_user_allowlist_unconfigured("telegram", "test_agent_p1_5");
        warn_once_user_allowlist_unconfigured("telegram", "another_agent");
        // Different channel_kind same instance is a distinct key — must
        // also not panic (covers future Discord / Slack adapter shape).
        warn_once_user_allowlist_unconfigured("discord", "test_agent_p1_5");
    }
}
