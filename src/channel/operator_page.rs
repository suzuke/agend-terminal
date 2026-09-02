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
//! Gate order is deliberate and all three of the operator's controls are
//! evaluated BEFORE the send, so `Error` severity can never let a page skip
//! them (decision d-20260902104216571473-11, condition 2):
//!
//! 1. enabled — fleet.yaml `channel.operator_page.enabled`, default OFF
//! 2. authority — only the caller's own team orchestrator may page
//! 3. rate — 3 per orchestrator per rolling hour, excess DROPPED with retry-after
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

pub(crate) fn handle_operator_page(home: &Path, args: &Value, instance_name: &str) -> Value {
    let _ = (home, args, instance_name);
    json!({
        "error": "operator_page is not implemented yet",
        "code": "not_implemented",
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "operator_page/tests.rs"]
mod tests;
