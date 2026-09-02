//! #3480 — orchestrator-only operator page, a thin wrapper on the existing
//! outbound chokepoint.
//!
//! The operator asked to be paged at overnight milestones and nothing reached
//! the phone: `reply` needs an inbound binding (none when they type in the TUI)
//! and `PushNotification`'s mobile leg needs Remote Control, which agend cannot
//! see. Spike `SPIKE-3480.md` established that the daemon already owns a
//! complete outbound Telegram path — [`crate::channel::notify_all_escalation_channels`]
//! → `gated_notify` → `notify_telegram_inner` — carrying the fail-closed
//! `user_allowlist` gate, the operator-mode gate, secret redaction, content
//! dedup and a bounded delivery worker. This module adds the missing entry
//! point and nothing else: it reuses that path rather than forking it.
//!
//! Gate order is deliberate and all of the operator's controls are evaluated
//! BEFORE the send, so `Error` severity can never let a page skip them
//! (decision d-20260902104216571473-11, condition 2):
//!
//! 1. enabled — fleet.yaml `channel.operator_page.enabled`, default OFF
//! 2. authority — only the caller's own team orchestrator may page
//! 3. deliverable — at least one registered channel is authorized for outbound
//! 4. rate — 3 per orchestrator per rolling hour, excess DROPPED with retry-after
//!
//! Severity is `Error` because the whole point is to survive `Away`/`Sleep`
//! (`should_notify_in_mode`, `channel/mod.rs:516`), where `Info`/`Warn` are
//! suppressed — a page the operator cannot receive while asleep is the original
//! bug wearing a new hat. Severity reaches only the gate: both adapters take it
//! as `_severity` (`telegram/adapter.rs:364`, `discord/adapter.rs:431`), so a
//! page is never formatted or routed as an error. Operator-mode therefore does
//! NOT suppress pages; the master control is the switch above.

use serde_json::{json, Value};
use std::path::Path;

/// Hard cap on page text, applied before the prefix. The shared path bounds
/// and redacts further (`gated_notify` → `bound_remote_system_notice`); this
/// cap is the tool's own contract so a caller cannot page a wall of text.
pub(crate) const MAX_PAGE_CHARS: usize = 1000;

/// Pages allowed per orchestrator per rolling hour.
pub(crate) const RATE_LIMIT_PER_HOUR: usize = 3;

const RATE_WINDOW_SECS: i64 = 3600;

/// Durable rate-counter sidecar. Under `$AGEND_HOME` so the count survives a
/// daemon restart — otherwise the restart itself is the bypass.
fn rate_store_path(home: &Path) -> std::path::PathBuf {
    home.join("operator_page_rate.json")
}

/// Claim one slot in the caller's rolling hour, or report how long to wait.
///
/// Read → prune → decide → write, all against the on-disk sidecar so there is
/// no in-memory state a restart could clear. `Err(retry_after_secs)` means the
/// budget is spent and the page is DROPPED, never queued: the caller is
/// expected to fall back to writing the milestone into SESSION-HANDOFF.md.
fn claim_rate_slot(home: &Path, orchestrator: &str, now: i64) -> Result<usize, i64> {
    let path = rate_store_path(home);
    let mut store: std::collections::BTreeMap<String, Vec<i64>> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let stamps = store.entry(orchestrator.to_string()).or_default();
    stamps.retain(|at| now.saturating_sub(*at) < RATE_WINDOW_SECS);

    if stamps.len() >= RATE_LIMIT_PER_HOUR {
        // The oldest stamp is what frees the next slot.
        let oldest = stamps.iter().copied().min().unwrap_or(now);
        let retry_after = RATE_WINDOW_SECS - now.saturating_sub(oldest);
        return Err(retry_after.max(1));
    }

    stamps.push(now);
    let remaining = RATE_LIMIT_PER_HOUR.saturating_sub(stamps.len());
    if let Ok(body) = serde_json::to_string_pretty(&store) {
        if let Err(error) = crate::store::atomic_write(&path, body.as_bytes()) {
            // Fail closed: a counter we cannot persist is a counter a restart
            // would reset, which is exactly the bypass this sidecar exists to
            // prevent. Refuse rather than send an uncounted page.
            tracing::warn!(%orchestrator, %error, "operator page rate counter unwritable — refusing");
            return Err(RATE_WINDOW_SECS);
        }
    }
    Ok(remaining)
}

/// The dedicated operator-notification topic, or the sender's own as fallback.
///
/// `create_topic_for_instance` is idempotent: it reuses the `topics.json` entry
/// when one exists and only calls the Telegram API on first use. Routing may
/// fail open — both topics live inside the same allowlisted group, so falling
/// back does not widen who can read the page — while gates 1-4 stay fail-closed.
fn route_instance(home: &Path, topic_name: &str, sender: &str) -> String {
    match crate::channel::telegram::topic_registry::create_topic_for_instance(home, topic_name) {
        Some(_) => topic_name.to_string(),
        None => {
            tracing::warn!(
                %topic_name, %sender,
                "operator page: dedicated topic unavailable — falling back to the sender topic"
            );
            sender.to_string()
        }
    }
}

pub(crate) fn handle_operator_page(home: &Path, args: &Value, instance_name: &str) -> Value {
    let message = args["message"].as_str().unwrap_or("").trim();
    if message.is_empty() {
        return json!({"error": "missing 'message'", "code": "missing_message"});
    }

    // (1) Opt-in switch. Absent stanza, absent telegram channel and
    // `enabled: false` are the same answer: the operator has not turned this on.
    let config = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let Some(page_config) = config
        .as_ref()
        .and_then(crate::fleet::FleetConfig::operator_page_config)
        .filter(|cfg| cfg.enabled)
    else {
        return json!({
            "error": "operator paging is disabled for this fleet",
            "code": "operator_page_disabled",
            "hint": "set channel.operator_page.enabled: true in fleet.yaml (operator decision)",
        });
    };

    // (2) Authority: the caller must BE its team's current orchestrator. Anyone
    // else is told who to route through, so the refusal is actionable.
    let orchestrator = crate::fleet::team_orchestrator_for(home, instance_name);
    if orchestrator.as_deref() != Some(instance_name) {
        return json!({
            "error": "only your team's orchestrator may page the operator",
            "code": "not_orchestrator",
            "your_orchestrator": orchestrator,
            "hint": "ask your orchestrator to page, or write the milestone to SESSION-HANDOFF.md",
        });
    }

    // (3) Deliverability, checked before the budget is spent: the fan-out helper
    // reports how many channels it ATTEMPTED, not how many passed the gate, so
    // asking the channels directly is the only way to tell the caller the truth
    // instead of a hopeful "sent".
    let channels = crate::channel::resolve_escalation_channels();
    if !channels.iter().any(|ch| ch.outbound_authorized()) {
        return json!({
            "sent": false,
            "error": "no channel is authorized for outbound notices",
            "code": "not_delivered",
            "hint": "configure channel.user_allowlist in fleet.yaml; write the milestone to SESSION-HANDOFF.md meanwhile",
        });
    }

    // (4) Rate: claim a slot only once the page is actually deliverable.
    let now = chrono::Utc::now().timestamp();
    let remaining = match claim_rate_slot(home, instance_name, now) {
        Ok(remaining) => remaining,
        Err(retry_after_secs) => {
            return json!({
                "sent": false,
                "error": format!(
                    "operator page budget spent ({RATE_LIMIT_PER_HOUR} per rolling hour)"
                ),
                "code": "rate_limited",
                "retry_after_secs": retry_after_secs,
                "hint": "the page was DROPPED, not queued — write the milestone to SESSION-HANDOFF.md",
            });
        }
    };

    // (5) Content: capped body, and the sender's identity is not the caller's to
    // choose — the operator must always know who paged.
    let body: String = message.chars().take(MAX_PAGE_CHARS).collect();
    let text = format!("[operator-page from {instance_name}] {body}");

    // (6) Send through the existing chokepoint. Error severity is the gate pass
    // for Away/Sleep and nothing more (see the module header).
    let routed_to = route_instance(home, &page_config.topic_name, instance_name);
    let dispatched = crate::channel::notify_all_escalation_channels(
        &routed_to,
        crate::channel::NotifySeverity::Error,
        &text,
        false,
    );

    tracing::info!(
        from = %instance_name, %routed_to, dispatched, remaining,
        "operator page dispatched"
    );
    json!({
        "sent": true,
        "routed_to": routed_to,
        "channels": dispatched,
        "remaining_this_hour": remaining,
        "chars_sent": body.chars().count(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "operator_page/tests.rs"]
mod tests;
