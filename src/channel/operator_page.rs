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
//! # Trust model (decision d-20260902…-36)
//!
//! Every agent and the daemon run as ONE OS user. The `instance` a tool call
//! carries is therefore a CLAIM, and no gate inside this daemon can make that
//! claim spoof-proof — nothing here should be read as if it could. What the
//! authority gate below honestly does is narrower and still worth having: it
//! refuses names that do not resolve to a LIVE instance, refuses ambiguous ones,
//! refuses callers owned by more than one team, and requires the resolved live
//! instance to BE its team's current orchestrator. A caller that presents the
//! orchestrator's live name is admitted, by design.
//!
//! The controls that actually bound the damage are the ones that do not depend on
//! identity at all: the switch is default-OFF and lives where an agent cannot
//! write it, the budget is 3 per rolling hour, is seeded only by the operator and
//! fails CLOSED, delivery is confined to the dedicated topic inside the
//! allowlisted Telegram group, and the body is flattened to ONE line with the
//! daemon's own sender marker defanged inside it, so a forged
//! `[operator-page from …]` can neither begin a line nor reproduce the marker
//! verbatim. What that does NOT buy: a client that soft-wraps a long page can
//! still start a visual row mid-body, and a forger can still write prose that
//! merely resembles a sender. Flattening removes the mandatory breaks; it cannot
//! make text unimpersonatable.
//!
//! # Gate order
//!
//! Every operator control is evaluated BEFORE the send, so `Error` severity can
//! never let a page skip them (decision d-20260902104216571473-11, condition 2):
//!
//! 1. enabled — the runtime-config `operator_page.enabled` stanza, default OFF
//! 2. authority — the caller must resolve to a LIVE instance that is its team's
//!    sole orchestrator
//! 3. deliverable — a TELEGRAM channel must be authorized for outbound
//! 4. rate — 3 per orchestrator per rolling hour, excess DROPPED with retry-after
//!
//! ## Why the switch is not in fleet.yaml any more
//!
//! It used to be `channel.operator_page.enabled` in `fleet.yaml`. Agents can write
//! `fleet.yaml` (it is the fleet's working document, edited through ordinary tool
//! calls), so a master switch living there is a switch its subjects can flip. The
//! stanza now lives in `runtime-config.json`, whose only mutation surface is the
//! operator CLI — `agend-terminal admin config-set operator_page.enabled true`;
//! the MCP `config` tool's `set` action was retired in #2548 and agents can only
//! READ. This mirrors the `experimental` stanza precedent exactly. There is one
//! source of truth: a leftover fleet.yaml stanza grants nothing.
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

pub(crate) mod budget;

/// Hard cap on page text, applied after flattening and before the prefix. The
/// shared path bounds and redacts further (`gated_notify` →
/// `bound_remote_system_notice`); this cap is the tool's own contract so a caller
/// cannot page a wall of text.
pub(crate) const MAX_PAGE_CHARS: usize = 1000;

/// Pages allowed per orchestrator per rolling hour.
pub(crate) const RATE_LIMIT_PER_HOUR: usize = 3;

pub(crate) const RATE_WINDOW_SECS: i64 = 3600;

/// The marker the daemon stamps in front of every page. Only the daemon may
/// emit it; [`defang_sender_marker`] takes it away from anyone else.
pub(crate) const SENDER_MARKER: &str = "[operator-page from ";

/// Characters that may not survive into a page body.
///
/// Everything `char::is_control()` covers — LF, CR, TAB, VT (`U+000B`), FF
/// (`U+000C`), NEL (`U+0085`) and the rest of C0/C1 — plus `U+2028` LINE
/// SEPARATOR and `U+2029` PARAGRAPH SEPARATOR, which are not control characters
/// but ARE mandatory breaks under UAX#14 and do start a new line in Telegram
/// clients. A page body is plain text with no formatting passthrough, so nothing
/// in this set has a legitimate use here.
///
/// The first version of this predicate tested `'\r'` and `'\n'` and nothing
/// else. An adversarial verification pass proved that `U+2028`, `U+2029`, NEL, VT
/// and FF all travelled through verbatim, which put a forged marker on a line of
/// its own — the exact forgery the flattening exists to stop.
fn breaks_or_controls_rendering(ch: char) -> bool {
    ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}'
}

/// Flatten a body to a single line: every character matched by
/// [`breaks_or_controls_rendering`] becomes a space, then runs of spaces collapse
/// to one.
///
/// Applied BEFORE the cap and before the sender prefix. The prefix the daemon
/// stamps is only trustworthy while it is the only thing that can begin a line,
/// so with no break left in the payload a forged marker cannot open one.
fn flatten_to_single_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for ch in raw.chars() {
        if breaks_or_controls_rendering(ch) || ch == ' ' {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out
}

/// Take the daemon's exact sender marker away from the body.
///
/// Flattening alone leaves `"… [operator-page from ops] all clear"` sitting
/// inline, one delimiter away from reading as a second sender — and flattening
/// can even CREATE the marker, since `"[operator-page\u{2028}from ops]"` becomes
/// `"[operator-page from ops]"` once the break turns into a space. Run this
/// AFTER the flattening for that reason.
///
/// This is a narrow claim: after this, `SENDER_MARKER` appears exactly once in
/// the delivered text and it is the daemon's own. It does NOT make the page
/// unimpersonatable — a body is free to contain any other sender-looking prose,
/// and no sanitiser here can decide what a human will read as authoritative.
fn defang_sender_marker(body: &str) -> String {
    body.replace(SENDER_MARKER, "[quoted: operator-page from ")
}

/// The dedicated operator-notification topic, or the sender's own as fallback.
///
/// `create_topic_for_instance` is idempotent: it reuses the `topics.json` entry
/// when one exists and only calls the Telegram API on first use. Routing may
/// fail open — both topics live inside the same allowlisted group, so falling
/// back does not widen who can read the page — while the gates stay fail-closed.
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

/// The caller, bound to a live instance and to a single owning team.
struct Authority {
    /// The LIVE handle's OWN name. The string the caller supplied is display-only
    /// from here on: everything downstream (the rate budget key, the sender
    /// prefix, the routing fallback) keys off this.
    caller: String,
    /// The single team that owns the caller. Carried for the dispatch log.
    team: String,
}

/// Bind the call to a live requester and to that requester's sole orchestrator
/// role, or produce the refusal payload.
///
/// The pre-#3480 gate compared the caller-SUPPLIED name against
/// `team_orchestrator_for`, so any seat that passed the orchestrator's name in the
/// `instance` field was admitted with no check that the name meant anything at
/// all. Binding to the daemon-resolved live handle does not make the claim
/// unspoofable (see the module header), but it does mean a name has to belong to
/// a running, unambiguous instance before any team lookup happens.
fn resolve_authority(
    home: &Path,
    runtime: Option<&crate::mcp::handlers::dispatch::RuntimeContext>,
    claimed: &str,
) -> Result<Authority, Value> {
    // (a) No daemon runtime means no registry, so there is nothing to resolve the
    // claim against. A standalone bridge call cannot be authorized; fail CLOSED
    // rather than fall back to trusting the string.
    let Some(runtime) = runtime else {
        return Err(json!({
            "error": "operator paging needs a live daemon identity and this call arrived without one",
            "code": "no_live_identity",
            "hint": "call through the daemon's in-process MCP path; a standalone bridge call cannot be authorized",
        }));
    };
    // (b) Exact UUID, or exact UNIQUE live name. Unknown, already-exited and
    // AMBIGUOUS names all land here, and all fail closed.
    let unknown = || {
        json!({
            "error": "the calling instance does not resolve to a single live instance",
            "code": "unknown_caller",
            "hint": "an unknown, already-exited or ambiguous instance name cannot page the operator",
        })
    };
    let Some(id) = crate::api::handlers::mcp_proxy::live_requester_id(&runtime.registry, claimed)
    else {
        return Err(unknown());
    };
    // (c) From here on the authoritative identity is the live handle's own name.
    let Some(caller) = crate::agent::lock_registry(&runtime.registry)
        .get(&id)
        .map(|handle| handle.name.as_str().to_string())
    else {
        return Err(unknown());
    };
    // (d) Exactly one owning team. `team_orchestrator_for` answers with whichever
    // team a HashMap scan reaches first, which is fine for the watchdog it was
    // written for and unacceptable for an authority decision.
    let (team, orchestrator) = match crate::fleet::owning_team_for(home, &caller) {
        crate::fleet::OwningTeam::Sole { team, orchestrator } => (team, orchestrator),
        crate::fleet::OwningTeam::Ambiguous { teams } => {
            return Err(json!({
                "error": "the calling instance belongs to more than one team, so it has no single orchestrator",
                "code": "ambiguous_team",
                "teams": teams,
                "hint": "give the instance one owning team in fleet.yaml, then page again",
            }));
        }
        crate::fleet::OwningTeam::None => (String::new(), None),
    };
    // (e) …whose CURRENT orchestrator must be this caller. A team that names an
    // orchestrator which is not this caller — including one that no longer exists
    // — grants nobody the page; the vacancy is not filled by whoever asks.
    if orchestrator.as_deref() != Some(caller.as_str()) {
        return Err(json!({
            "error": "only your team's orchestrator may page the operator",
            "code": "not_orchestrator",
            "your_orchestrator": orchestrator,
            "hint": "ask your orchestrator to page, or write the milestone to SESSION-HANDOFF.md",
        }));
    }
    Ok(Authority { caller, team })
}

pub(crate) fn handle_operator_page(
    home: &Path,
    args: &Value,
    instance_name: &str,
    runtime: Option<&crate::mcp::handlers::dispatch::RuntimeContext>,
) -> Value {
    // (0) Content is sanitized FIRST so the emptiness test sees what will actually
    // be sent: a body of nothing but line breaks is an empty page.
    let flattened = flatten_to_single_line(args["message"].as_str().unwrap_or(""));
    let message = defang_sender_marker(flattened.trim());
    if message.is_empty() {
        return json!({"error": "missing 'message'", "code": "missing_message"});
    }

    // (1) Opt-in switch, read from the daemon-private runtime config. Absent
    // stanza and `enabled: false` are the same answer: the operator has not
    // turned this on.
    let page_config = crate::runtime_config::get().operator_page;
    if !page_config.enabled {
        return json!({
            "error": "operator paging is disabled",
            "code": "operator_page_disabled",
            "hint": "the operator enables it with: agend-terminal admin config-set operator_page.enabled true",
        });
    }

    // (2) Authority, bound to the daemon-resolved live requester.
    let authority = match resolve_authority(home, runtime, instance_name) {
        Ok(authority) => authority,
        Err(refusal) => return refusal,
    };
    let caller = authority.caller.as_str();

    // (3) Deliverability, checked before the budget is spent. It must be the
    // TELEGRAM channel specifically: this tool exists to reach the operator's
    // PHONE, and an `any()` over every registered channel let a Discord-only
    // allowlist answer `sent: true` (and spend a rate slot) while the phone stayed
    // silent. The fan-out helper reports how many channels it ATTEMPTED, not how
    // many passed the gate, so asking the channels directly is the only way to
    // tell the caller the truth instead of a hopeful "sent".
    let channels = crate::channel::resolve_escalation_channels();
    if !channels
        .iter()
        .any(|ch| ch.kind() == "telegram" && ch.outbound_authorized())
    {
        return json!({
            "sent": false,
            "error": "no telegram channel is authorized for outbound notices",
            "code": "not_delivered",
            "hint": "configure a telegram channel.user_allowlist in fleet.yaml; write the milestone to SESSION-HANDOFF.md meanwhile",
        });
    }

    // (4) Rate: claim a slot only once the page is actually deliverable.
    let now = chrono::Utc::now().timestamp();
    let remaining = match budget::claim(home, caller, now) {
        Ok(remaining) => remaining,
        Err(budget::ClaimError::RateLimited { retry_after_secs }) => {
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
        Err(budget::ClaimError::Unavailable { cause, reason }) => {
            // Deliberately NOT `rate_limited`: an untrustworthy budget and a spent
            // one call for different operator actions, so they must not look alike.
            // `cause` is the machine-readable half so an operator tool can tell an
            // absent snapshot from a corrupt one without matching on prose.
            return json!({
                "sent": false,
                "error": format!("operator paging is unavailable — {reason}"),
                "code": "budget_unavailable",
                "cause": cause,
                "hint": "the page was DROPPED. Only the operator can restore paging: re-run `agend-terminal admin config-set operator_page.enabled true`, which re-seeds $AGEND_HOME/operator_page_rate.json (repair or delete a corrupt snapshot first). Under one shared OS user that file is not tamper-proof — what this buys is that tampering DENIES, is logged, and is recoverable only by the operator.",
            });
        }
    };

    // (5) Content: capped body, and the sender's identity is not the caller's to
    // choose — the operator must always know who paged.
    let body: String = message.chars().take(MAX_PAGE_CHARS).collect();
    let text = format!("{SENDER_MARKER}{caller}] {body}");

    // (6) Send through the existing chokepoint. Error severity is the gate pass
    // for Away/Sleep and nothing more (see the module header).
    let routed_to = route_instance(home, &page_config.topic_name, caller);
    let dispatched = crate::channel::notify_all_escalation_channels(
        &routed_to,
        crate::channel::NotifySeverity::Error,
        &text,
        false,
    );
    if dispatched == 0 {
        // NOT a delivery proof, and nothing here should be read as one.
        // `notify_all_escalation_channels` returns how many channels it ATTEMPTED
        // (`channel/mod.rs:200`), and `gated_notify` returns `Ok(())` even when it
        // DROPS the notice (`channel/mod.rs:700`), so a nonzero count says only
        // that the page reached at least one registered channel. Zero is the one
        // thing it does prove: the registry emptied between the deliverability
        // gate and the fan-out, so the page reached NO channel at all and the
        // claim bought nothing. Roll back for that case only — a counted page may
        // still have been dropped downstream and this site cannot tell.
        budget::release(home, caller, now);
        return json!({
            "sent": false,
            "error": "no channel was reachable when the page was dispatched",
            "code": "not_delivered",
            "hint": "the rate slot was returned; write the milestone to SESSION-HANDOFF.md",
        });
    }

    tracing::info!(
        from = %caller, team = %authority.team, %routed_to, dispatched, remaining,
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
