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
//! identity at all AND do not live in an agent-writable file: the switch is
//! default-OFF and lives where an agent cannot write it, delivery is confined to
//! the dedicated topic inside the allowlisted Telegram group, every page is
//! attributed and lands in front of the operator, and the body is flattened to
//! ONE line and then REFUSED outright if what is left still carries the daemon's
//! own sender marker.
//!
//! The 3-per-rolling-hour budget is deliberately NOT on that list. It is seeded
//! only by the operator and fails CLOSED on a snapshot it cannot parse, which is
//! real defence in depth against an accident, a clumsy reset and a crash — but
//! its state lives in an agent-writable file, and a syntactically VALID rewrite
//! plus a daemon restart resets the window with nothing able to prevent or detect
//! it. The full shape × lifetime matrix, and why an integrity mechanism cannot
//! help under one shared UID, are in [`budget`].
//!
//! Exactly what that flattening covers, and what it does not, because an earlier
//! version of this paragraph claimed more than the code delivers:
//!
//!   * COVERED — every mandatory line break and every character that cannot
//!     survive verbatim: control characters (Cc), Unicode `White_Space`
//!     including NBSP, format characters (Cf) such as ZWSP and the bidi
//!     overrides, and the `Default_Ignorable_Code_Point` set (CGJ `U+034F`, the
//!     variation selectors). So the marker cannot begin a line, and it cannot be
//!     spelled with an invisible-format character or a look-alike space.
//!   * NOT COVERED — a marker spelled with HOMOGLYPHS. `[\u{043E}perator-page
//!     from ops]`, with Cyrillic `о` for Latin `o`, is NOT detected and IS
//!     delivered. Every character in it renders, so invisibility is the wrong
//!     test for it, and confusable folding is not attempted here.
//!   * WHY THE RESIDUAL IS BOUNDED, as a mitigation and not more: the body is
//!     flattened to ONE line and the daemon's own prefix is always first, so a
//!     homoglyph forgery can only ever appear MID-LINE after a genuine
//!     `[operator-page from <caller>]` prefix. It cannot open a line and it
//!     cannot displace the real sender.
//!
//! A client that soft-wraps a long page can still start a visual row mid-body,
//! and a forger can still write prose that merely resembles a sender. None of
//! this makes text unimpersonatable.
//!
//! ## Why a forged marker is REFUSED and not rewritten
//!
//! The previous fix rewrote a literal `[operator-page from ` in the body to
//! `[quoted: operator-page from ` and delivered the page anyway. That is
//! withdrawn, for three reasons a second adversarial pass made plain:
//!
//!   * It MUTATED operator-visible message text with no signal anywhere. The
//!     success payload carried no "body was modified" flag and nothing was
//!     logged, so the operator read altered words and could not tell.
//!   * A legitimate page essentially never contains the literal marker, so
//!     refusing costs nothing, while refusing turns an attack attempt into a
//!     DETECTABLE event — a `warn!` naming the caller — instead of one the daemon
//!     quietly absorbs.
//!   * The rewrite was defeated by spellings it never saw anyway: case variants
//!     (`[Operator-Page From ops]`) passed it untouched, and NBSP, ZWSP and RLO
//!     reproduced the marker's appearance without its bytes.
//!
//! The refusal is reached only while paging is ON — see the gate order below.
//!
//! # Gate order
//!
//! Every operator control is evaluated BEFORE the send, so `Error` severity can
//! never let a page skip them (decision d-20260902104216571473-11, condition 2):
//!
//! 0. shape — the body is normalised to one line and an empty page is refused as
//!    `missing_message`. Argument validation, not a control: it writes nothing,
//!    logs nothing, and is the only step that runs ahead of the switch
//! 1. enabled — the runtime-config `operator_page.enabled` stanza, default OFF.
//!    It is the FIRST control for a reason: default-OFF has to mean the tool is
//!    INERT, so while it is off no later gate runs at all and no agent can drive
//!    a daemon log line through a feature the operator never turned on
//! 2. content — a body that still reads as the daemon's sender marker is REFUSED
//!    here, before authority, before deliverability and before any budget claim,
//!    so a forged body can never cost the orchestrator a rate slot and the
//!    attempt is logged whatever the later gates would have said
//! 3. authority — the caller must resolve to a LIVE instance that is its team's
//!    sole orchestrator
//! 4. deliverable — a TELEGRAM channel must be authorized for outbound
//! 5. rate — 3 per orchestrator per rolling hour, excess DROPPED with retry-after
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
/// emit it; a body that still reads as this after normalisation is REFUSED —
/// see [`carries_sender_marker`].
///
/// Kept lowercase because [`carries_sender_marker`] compares against a lowercased
/// body.
pub(crate) const SENDER_MARKER: &str = "[operator-page from ";

/// Characters that have no visible rendering of their own: Unicode general
/// category **Cf** (format characters) UNION the binary property
/// **`Default_Ignorable_Code_Point`**.
///
/// Cf is ZWSP `U+200B`, ZWNJ/ZWJ, the bidi set LRM/RLM/LRE/RLE/PDF/LRO/RLO, the
/// `U+2066`–`U+2069` isolates, `U+FEFF` and the rest.
/// `Default_Ignorable_Code_Point` is Unicode's own name for "should have no
/// visible rendering", and it reaches what Cf does not: CGJ `U+034F`, the
/// variation selectors `U+FE00`–`U+FE0F` and `U+180B`–`U+180F` (all category
/// **Mn**), the Hangul fillers `U+115F`/`U+1160`/`U+3164`/`U+FFA0` (category
/// **Lo**) and the Khmer invisible vowels `U+17B4`/`U+17B5`. Neither set
/// contains the other, so both are asked.
///
/// Only the default-IGNORABLE part of Mn is taken. Category Mn as a whole is
/// deliberately NOT stripped: combining marks are how Vietnamese, Hebrew,
/// Devanagari and many other scripts are written, and stripping them wholesale
/// would corrupt legitimate page bodies — a worse trade than the residual it
/// would close. The one accepted cost of taking the ignorable part is that
/// `U+FE0F` is stripped from an emoji, so an emoji in a page may render in its
/// text presentation rather than its emoji presentation.
///
/// `char` has no `is_format()`, and `char::is_control()` is Cc ONLY. The category
/// and property data come from `regex`, which is already a direct dependency of
/// this crate (`Cargo.toml`, `regex = "1"`, default features, so both
/// `unicode-gencat` and `unicode-bool` are on) — NO dependency and no feature is
/// added for this, and the tables track the `regex` crate's Unicode version
/// rather than a hand-copied list that would silently go stale.
/// The pattern is a constant and the `expect` follows the convention already used
/// for static patterns elsewhere in the crate (`task_events.rs:103`,
/// `backend.rs:2401`): a build that could not classify these must fail loudly,
/// never silently classify nothing and let the invisibles through.
fn has_no_visible_rendering(ch: char) -> bool {
    static INVISIBLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let invisible = INVISIBLE.get_or_init(|| {
        regex::Regex::new(r"^[\p{Cf}\p{Default_Ignorable_Code_Point}]$")
            .expect("the static \\p{Cf} + \\p{Default_Ignorable_Code_Point} pattern must compile")
    });
    let mut buf = [0u8; 4];
    invisible.is_match(ch.encode_utf8(&mut buf))
}

/// Characters that may not survive verbatim into a page body. Each one becomes a
/// single ordinary space.
///
/// Three families, and every one of them can make a forged sender read as the
/// daemon's own:
///
///   * `char::is_control()` — category Cc: LF, CR, TAB, VT (`U+000B`), FF
///     (`U+000C`), NEL (`U+0085`) and the rest of C0/C1. These start a new line.
///   * `char::is_whitespace()` — the Unicode `White_Space` property: NBSP
///     (`U+00A0`), `U+1680`, `U+2000`–`U+200A`, `U+202F`, `U+205F`, `U+3000`, and
///     the two mandatory UAX#14 breaks `U+2028`/`U+2029`. A space look-alike is
///     not a cosmetic problem: `[operator-page\u{00A0}from ops]` renders
///     pixel-identically to the real marker.
///   * [`has_no_visible_rendering`] — category Cf and the
///     `Default_Ignorable_Code_Point` property. ZWSP inside the marker
///     (`[operator-page fr\u{200B}om ops]`) is invisible; RLO makes
///     `\u{202E}]spo morf egap-rotarepo[` display as the marker, read backwards;
///     CGJ (`[operator-page f\u{034F}rom ops]`) and the variation selectors are
///     defined never to render at all.
///
/// What this set does NOT reach, stated plainly because the prose used to claim
/// otherwise: a marker spelled with HOMOGLYPHS — Cyrillic `о` `U+043E` for Latin
/// `o`, say — is not detected here and is delivered verbatim. Every character in
/// such a body renders, so no predicate over invisibility can see it, and
/// normalising confusables is a different (and much larger) mechanism than this
/// one. What bounds it is structural rather than lexical: the body is flattened
/// to ONE line and the daemon's own prefix is always first, so a homoglyph
/// forgery can only ever appear MID-LINE after a genuine
/// `[operator-page from <caller>]` prefix. It cannot open a line and it cannot
/// displace the real sender. That is a mitigation, not a fix.
///
/// The ordinary space is deliberately NOT in this set: it is what everything
/// here becomes, and `flatten_to_single_line` collapses runs of it separately.
///
/// Three adversarial passes shaped this. The first predicate tested `'\r'` and
/// `'\n'` only, and `U+2028`, `U+2029`, NEL, VT and FF travelled through
/// verbatim. The second predicate was `is_control()` + `U+2028`/`U+2029`, which
/// is Cc-only, and NBSP, ZWSP and RLO travelled through verbatim. The third
/// predicate was Cc + `White_Space` + Cf, and CGJ `U+034F` and the variation
/// selectors `U+FE00`/`U+FE0F` — invisible, but category Mn — travelled through
/// verbatim; `Default_Ignorable_Code_Point` closes exactly those. A page body is
/// plain text with no formatting passthrough, so nothing in this set has a
/// legitimate use here.
fn must_not_survive_verbatim(ch: char) -> bool {
    ch != ' ' && (ch.is_control() || ch.is_whitespace() || has_no_visible_rendering(ch))
}

/// Flatten a body to a single line: every character matched by
/// [`must_not_survive_verbatim`] becomes a space, then runs of spaces collapse
/// to one.
///
/// Applied BEFORE the marker check, the cap and the sender prefix, in that
/// order. The prefix the daemon stamps is only trustworthy while it is the only
/// thing that can begin a line, so with no break left in the payload a forged
/// marker cannot open one — and normalising FIRST is also what lets the marker
/// check see through `[operator-page\u{00A0}from ops]`.
fn flatten_to_single_line(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_space = false;
    for ch in raw.chars() {
        if ch == ' ' || must_not_survive_verbatim(ch) {
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

/// Does the NORMALISED body still read as the daemon's own sender marker?
///
/// Run AFTER [`flatten_to_single_line`], because flattening can CREATE the marker
/// as well as reveal one: `"[operator-page\u{2028}from ops]"` and
/// `"[operator-page\u{00A0}from ops]"` both become the literal marker once the
/// separator turns into a space.
///
/// Case-INSENSITIVE. `[Operator-Page From ops]` reads as authoritative to a human
/// exactly as the lowercase form does, and an adversarial pass proved case
/// variants walked past the byte-exact rewrite this replaces.
fn carries_sender_marker(body: &str) -> bool {
    body.to_lowercase().contains(SENDER_MARKER)
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
    let message = flattened.trim();
    if message.is_empty() {
        return json!({"error": "missing 'message'", "code": "missing_message"});
    }

    // (1) Opt-in switch, read from the daemon-private runtime config. Absent
    // stanza and `enabled: false` are the same answer: the operator has not
    // turned this on. It is the FIRST control, ahead of the content refusal
    // below: while paging is off the tool must be INERT, and a refusal that ran
    // earlier let any agent drive a `warn!` line through a feature the operator
    // never enabled.
    let page_config = crate::runtime_config::get().operator_page;
    if !page_config.enabled {
        return json!({
            "error": "operator paging is disabled",
            "code": "operator_page_disabled",
            "hint": "the operator enables it with: agend-terminal admin config-set operator_page.enabled true",
        });
    }

    // (2) A body that still reads as the daemon's own sender marker is REFUSED
    // here: after the switch, but before authority, before deliverability and
    // before any budget claim, so a forged body can never cost the orchestrator a
    // rate slot. Placing it ahead of those gates also means the attempt is LOGGED
    // whatever they would have said — the whole point of refusing instead of
    // rewriting is that the operator gets to see it. `instance_name` is the
    // CLAIMED name (authority has not run yet); it is a claim, not proof, and is
    // logged as one.
    if carries_sender_marker(message) {
        tracing::warn!(
            claimed_caller = %instance_name,
            "operator page REFUSED: the body carries the daemon's sender marker — a forged-sender attempt, or a page quoting one"
        );
        return json!({
            "sent": false,
            "error": "the page body may not contain the daemon's sender marker",
            "code": "marker_in_body",
            "hint": format!(
                "`{SENDER_MARKER}…]` is stamped by the daemon and identifies who paged; a body containing it after normalisation — in any case, or spelled with a look-alike space or an invisible character (Cf or default-ignorable, e.g. NBSP, ZWSP, RLO, CGJ, a variation selector) — is refused so it cannot read as a second sender. A marker spelled with homoglyphs is NOT detected and is not what this refusal covers. Reword the message and page again — no rate slot was spent."
            ),
        });
    }

    // (3) Authority, bound to the daemon-resolved live requester.
    let authority = match resolve_authority(home, runtime, instance_name) {
        Ok(authority) => authority,
        Err(refusal) => return refusal,
    };
    let caller = authority.caller.as_str();

    // (4) Deliverability, checked before the budget is spent. It must be the
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

    // (5) Rate: claim a slot only once the page is actually deliverable.
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
            //
            // The remedy is the same command in every case, but its CONSEQUENCE is
            // not, and the operator is entitled to know before they run it. When
            // the poison was latched at INITIALISATION — `snapshot_absent` (the
            // file was gone before this process started) or `snapshot_corrupt` —
            // this process holds no stamps for the home, so re-seeding writes `{}`
            // and the rolling hour STARTS OVER from that moment. The information
            // needed to preserve the hour was destroyed with the snapshot; the
            // reset is accepted because it is operator-gated and denies by
            // default, but it must not be silent. `snapshot_missing` and
            // `snapshot_unwritable` are different: memory still holds the true
            // spent count, so re-seeding writes it back and the hour survives.
            let resets_the_hour = if matches!(cause, "snapshot_absent" | "snapshot_corrupt") {
                " WARNING: this daemon no longer holds the spent count for this home, so re-seeding STARTS A NEW ROLLING HOUR — pages already spent inside the current hour are forgotten and the caller gets a full budget again."
            } else {
                ""
            };
            return json!({
                "sent": false,
                "error": format!("operator paging is unavailable — {reason}"),
                "code": "budget_unavailable",
                "cause": cause,
                "hint": format!("the page was DROPPED. Only the operator can restore paging: re-run `agend-terminal admin config-set operator_page.enabled true`, which re-seeds $AGEND_HOME/operator_page_rate.json (repair or delete a corrupt snapshot first).{resets_the_hour} Under one shared OS user that file is not tamper-proof: an ABSENT, CORRUPT, TRUNCATED or UNREADABLE snapshot denies and is logged, which is what you are seeing, but a syntactically VALID rewrite plus a restart resets the window and can be neither prevented nor detected. This cap bounds accident, not a hostile seat — see docs/architecture.md §1.2."),
            });
        }
    };

    // (6) Content: capped body, and the sender's identity is not the caller's to
    // choose — the operator must always know who paged.
    let body: String = message.chars().take(MAX_PAGE_CHARS).collect();
    let text = format!("{SENDER_MARKER}{caller}] {body}");

    // (7) Send through the existing chokepoint. Error severity is the gate pass
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
