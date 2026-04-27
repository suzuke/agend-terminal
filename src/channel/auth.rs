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
/// callers can distinguish "permitted by explicit allow-list" from
/// "permitted by default-open" (the latter is now the canonical default,
/// not a transitional grace).
///
/// **Sprint 23 P1 (per operator philosophy override)** — semantics
/// inverted from Sprint 22 P0's fail-closed default:
/// - `outbound_capabilities` field absent → `OpenDefault` (PERMIT)
/// - `outbound_capabilities: []` → `Rejected` (explicit opt-out, retained)
/// - `outbound_capabilities: [reply]` → `Allowed`/`Rejected` per-op
///
/// The `PermissiveLegacyMissing` variant from Sprint 22 P0's hard-cut
/// transition is renamed to `OpenDefault` to reflect that default-open is
/// no longer a "grace" — it is the canonical posture for the single-
/// operator threat model. Cascade attack chain doesn't apply (TUI = full
/// machine access; operator explicit ack of the security trade-off via
/// telegram 11:00 UTC routed through `general` m-20260427115706155870-88).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundCapabilityDecision {
    /// `outbound_capabilities` includes the requested op — proceed normally.
    Allowed,
    /// `outbound_capabilities` is set but does NOT include the op — reject.
    /// Triggered by `Some(non_empty_list_without_op)` and by the explicit
    /// `Some(empty_list)` opt-out.
    Rejected,
    /// `outbound_capabilities` is `None` (field absent in fleet.yaml) —
    /// default-open per Sprint 23 P1 reversal. Operator declares the
    /// field only to opt out (`[]`) or restrict (selective list).
    OpenDefault,
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
        None => OutboundCapabilityDecision::OpenDefault,
    }
}

// `warn_once_outbound_capabilities_missing` retired — Sprint 23 P1
// reversal: missing `outbound_capabilities` is the **default-open posture**,
// no longer a deprecation-warned migration grace. The previous Sprint 22 P0
// FATAL `tracing::error!` line became misleading after the philosophy
// override (operator hits the warn on every restart of a fresh fleet,
// implying breakage where there is none). Removed entirely; the
// [`OutboundCapabilityDecision::OpenDefault`] branch is now silent.
//
// Sister helper [`warn_once_user_allowlist_unconfigured`] is kept — the
// channel-level allowlist gate is still fail-closed (different threat
// model: notification fan-out to operator; missing allowlist drops all
// notifications, surfacing as silent operator regression).

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

/// Sprint 22 P0 (introduced) + Sprint 23 P1 (default-open inversion) —
/// shared `gate_outbound_for_agent` helper consumed by every
/// `Channel::send_from_agent` impl (Telegram + future Discord/Slack/Teams).
///
/// Centralises the per-agent capability check so:
/// 1. Future channel adapters cannot accidentally bypass the gate by
///    forgetting to call `evaluate_outbound_capability` in their impl.
/// 2. The default-open semantic is consistent across adapters (one helper,
///    one decision matrix, one set of error messages).
/// 3. The (OpenDefault → permit) branch is testable in isolation without
///    spinning up a real channel.
///
/// Returns `Ok(())` when the agent is permitted to emit `op` (explicitly
/// listed OR field absent → default-open). Returns
/// `Err(ChannelError::Other)` with a typed error message when explicitly
/// rejected (capability list set but does NOT include `op`, including the
/// `[]` opt-out).
///
/// IO note: reads `<home>/fleet.yaml` on each call. For per-call frequency
/// in production this is acceptable (operator config rarely changes
/// mid-flight); a future hot-reload optimisation is tracked in dispatch
/// d-20260427042738203707-13 deferred items.
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
             — either add the op to the list, remove the entire field for default-open, \
             or change the empty list to declare the desired ops",
            op_str = op.as_str()
        ))),
        OutboundCapabilityDecision::OpenDefault => Ok(()),
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
    fn evaluate_outbound_capability_open_default_when_missing() {
        // **Sprint 23 P1 reversal** — missing `outbound_capabilities`
        // field is the default-open posture (was `PermissiveLegacyMissing`
        // in Sprint 22 P0 with FATAL warn). Operator declares the field
        // only to opt out (`[]`) or restrict (selective list).
        assert_eq!(
            evaluate_outbound_capability(None, ChannelOpKind::Reply),
            OutboundCapabilityDecision::OpenDefault
        );
        assert_eq!(
            evaluate_outbound_capability(None, ChannelOpKind::React),
            OutboundCapabilityDecision::OpenDefault
        );
        assert_eq!(
            evaluate_outbound_capability(None, ChannelOpKind::Edit),
            OutboundCapabilityDecision::OpenDefault
        );
        assert_eq!(
            evaluate_outbound_capability(None, ChannelOpKind::InjectProvenance),
            OutboundCapabilityDecision::OpenDefault
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

    // `warn_once_outbound_capabilities_missing_does_not_panic` removed —
    // the helper itself is gone (Sprint 23 P1 reversal: missing field is
    // the canonical default, not a deprecation-warned grace).

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

    // ── Sprint 23 P1 — gate_outbound_for_agent default-open contract ──

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn write_fleet_yaml(home: &std::path::Path, body: &str) {
        std::fs::create_dir_all(home).expect("create test home dir");
        std::fs::write(home.join("fleet.yaml"), body).expect("write fleet.yaml fixture");
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-auth-gate-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    /// Sprint 23 P1 — missing `outbound_capabilities` field permits
    /// every op (default-open per operator philosophy override).
    #[test]
    fn gate_outbound_for_agent_missing_field_returns_permitted() {
        let home = tmp_home("gate_missing");
        write_fleet_yaml(
            &home,
            r#"
instances:
  alpha:
    backend: claude
"#,
        );
        for op in [
            ChannelOpKind::Reply,
            ChannelOpKind::React,
            ChannelOpKind::Edit,
            ChannelOpKind::InjectProvenance,
        ] {
            assert!(
                gate_outbound_for_agent(&home, "alpha", op).is_ok(),
                "default-open: missing outbound_capabilities must permit {op:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sprint 23 P1 — explicit `outbound_capabilities: []` rejects all
    /// ops (operator opt-out preserved). Distinct from missing field.
    #[test]
    fn gate_outbound_for_agent_empty_list_rejects_all_ops() {
        let home = tmp_home("gate_empty");
        write_fleet_yaml(
            &home,
            r#"
instances:
  alpha:
    backend: claude
    outbound_capabilities: []
"#,
        );
        for op in [
            ChannelOpKind::Reply,
            ChannelOpKind::React,
            ChannelOpKind::Edit,
            ChannelOpKind::InjectProvenance,
        ] {
            let err = gate_outbound_for_agent(&home, "alpha", op).err();
            assert!(
                err.is_some(),
                "explicit empty list opt-out must reject {op:?}, got Ok"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sprint 23 P1 — explicit selective list permits only the listed
    /// ops; everything else rejected.
    #[test]
    fn gate_outbound_for_agent_selective_list_permits_only_listed() {
        let home = tmp_home("gate_selective");
        write_fleet_yaml(
            &home,
            r#"
instances:
  alpha:
    backend: claude
    outbound_capabilities: [reply]
"#,
        );
        assert!(
            gate_outbound_for_agent(&home, "alpha", ChannelOpKind::Reply).is_ok(),
            "selective list must permit listed op"
        );
        for op in [
            ChannelOpKind::React,
            ChannelOpKind::Edit,
            ChannelOpKind::InjectProvenance,
        ] {
            assert!(
                gate_outbound_for_agent(&home, "alpha", op).is_err(),
                "selective list must reject unlisted op {op:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
