use std::path::Path;

use super::message::{BroadcastContext, InboxMessage, NotifySource};
use super::storage;

/// Returns true when the `AGEND_POINTER_ONLY_INJECT` feature flag is set to "1".
/// When enabled, PTY injection uses header-only format for all messages,
/// forcing agents to call `inbox` to read content (solves dispatch non-FIFO).
pub fn pointer_only_inject() -> bool {
    // G3 H1: read DaemonConfig instead of env var (thread-safe)
    crate::daemon_config::get().pointer_only_inject
}

/// ANSI-colored header prefix for visual distinction in terminal.
pub const HEADER_PREFIX: &str = "\x1b[44;97m[AGEND-MSG]\x1b[0m";

/// #982: ANSI-colored prefix for idle-hint headers emitted alongside
/// daemon-side inbox enqueues. Distinct from [`HEADER_PREFIX`] so
/// recipients can distinguish the canonical message body inject from
/// the lightweight `(use inbox tool)` pointer signalling a new
/// pending entry. Background is yellow-on-black for contrast.
pub const PENDING_HEADER_PREFIX: &str = "\x1b[43;30m[AGEND-MSG-PENDING]\x1b[0m";

/// Plain-text system message prefix (without ANSI colors).
/// Used for detection/matching in agent PTY output parsing.
pub const SYSTEM_MSG_PREFIX: &str = "[AGEND-MSG]";

/// Agent-to-agent message prefix.
pub const AGENT_MSG_PREFIX: &str = "[from:";

/// Sanitize a value for header inclusion: replace control chars with space.
fn sanitize_header_value(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Format a single-line structured header for PTY injection.
/// Fields: from / id / kind / thread / parent / size.
/// Optional fields (thread/parent) omitted when None.
#[allow(dead_code)]
pub fn format_header(msg: &InboxMessage) -> String {
    // #761: strip the redundant `from:` prefix that `Source::Agent`'s
    // Display impl adds. `strip_prefix` returns `Some` for agent sources
    // and `None` otherwise, so `system:` / `user:` namespaces survive
    // untouched via the `unwrap_or` fallback.
    let from_value = msg.from.strip_prefix("from:").unwrap_or(&msg.from);
    let mut parts = vec![
        HEADER_PREFIX.to_string(),
        format!("from={}", sanitize_header_value(from_value)),
    ];
    if let Some(ref id) = msg.id {
        parts.push(format!("id={}", sanitize_header_value(id)));
    }
    if let Some(ref kind) = msg.kind {
        parts.push(format!("kind={}", sanitize_header_value(kind)));
    }
    if let Some(ref thread) = msg.thread_id {
        parts.push(format!("thread={}", sanitize_header_value(thread)));
    }
    if let Some(ref parent) = msg.parent_id {
        parts.push(format!("parent={}", sanitize_header_value(parent)));
    }
    parts.push(format!("size={}", msg.text.chars().count()));
    if !msg.attachments.is_empty() {
        let paths: Vec<&str> = msg
            .attachments
            .iter()
            .map(|a| a.path.to_str().unwrap_or("?"))
            .collect();
        parts.push(format!("attachments=[{}]", paths.join(",")));
    }
    // Sprint 54 layer-5 broadcast visibility: surface broadcast routing in
    // the PTY-inject header.
    if let Some(ref ctx) = msg.broadcast_context {
        parts.push(format!("broadcast={}", ctx.count));
        if let Some(ref team) = ctx.team {
            parts.push(format!("team={}", sanitize_header_value(team)));
        }
    }
    if let Some(ref excerpt) = msg.in_reply_to_excerpt {
        parts.push(format!(
            "reply_to_excerpt={}",
            sanitize_header_value(excerpt)
        ));
    }
    parts.push(operator_now_field()); // #1487
    parts.join(" ")
}

/// #1487: build the `now=<operator-TZ timestamp>` header field so an agent
/// sees the current operator-local time on EVERY message — fixing both the
/// daemon's UTC offset (operator is e.g. UTC+8) and the spawn-time date
/// freeze. The value is space-free RFC3339 + offset (`2026-05-30T16:12:34+08:00`):
/// the header is a space-delimited `key=value` list (agents tokenize on
/// spaces per agend.md), so a spaced value would shatter field parsing.
///
/// Reuses the operator display timezone (`display_timezone`) and the shared
/// [`crate::display_time::format_iso_offset`] formatter so there is a single
/// source of truth for operator-tz conversion; `None` → system local time.
pub(crate) fn operator_now_field() -> String {
    let tz = crate::daemon_config::get().display_timezone;
    let stamp = crate::display_time::format_iso_offset(chrono::Utc::now(), tz.as_deref());
    format!("now={stamp}")
}

/// Build the `in_reply_to_excerpt` value for inbound replies.
///
/// Returns `None` when `text` is empty so callers can `and_then` over an
/// optional reply target. Otherwise truncates to 200 chars (Unicode-safe via
/// `chars().take(...)`) and appends `…` if the original exceeded 200 chars.
pub(crate) fn build_excerpt(text: &str, author: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let truncated: String = text.chars().take(200).collect();
    let ellipsis = if text.chars().count() > 200 {
        "…"
    } else {
        ""
    };
    Some(format!("[{author}] {truncated}{ellipsis}"))
}

/// Format a single-line event header (non-message events like poll-reminder).
/// Uses the same ANSI prefix as [`format_header`] for visual consistency.
pub fn format_event_header(kind: &str, fields: &[(&str, &str)]) -> String {
    let mut parts = vec![
        HEADER_PREFIX.to_string(),
        format!("kind={}", sanitize_header_value(kind)),
    ];
    for (k, v) in fields {
        parts.push(format!("{}={}", k, sanitize_header_value(v)));
    }
    parts.push(operator_now_field()); // #1487
    parts.join(" ")
}

/// Deliver a message: always enqueue to inbox JSONL for persistence,
/// then inject to PTY (inline or pointer-only depending on feature flag).
pub fn deliver(
    home: &Path,
    agent_name: &str,
    source: &NotifySource<'_>,
    text: &str,
    _submit_key: &str,
    kind: Option<String>,
    broadcast_context: Option<BroadcastContext>,
) {
    let msg = InboxMessage {
        from: source.to_string(),
        text: text.to_string(),
        kind,
        timestamp: chrono::Utc::now().to_rfc3339(),
        broadcast_context,
        ..Default::default()
    };
    persist_or_log!(
        storage::enqueue(home, agent_name, msg),
        "deliver",
        agent_name
    );
    notify_agent(home, agent_name, source, text);
}

/// Sprint 54 silent-drop layer-4 hotfix: aggregate kind counts for the
/// PTY-inject header `attachments=[…]` field.
pub(crate) fn summarize_attachments_for_header(
    attachments: &[crate::channel::event::Attachment],
) -> Option<String> {
    if attachments.is_empty() {
        return None;
    }
    use crate::channel::event::AttachmentKind;
    let mut counts: [usize; 5] = [0; 5];
    for a in attachments {
        let i = match a.kind {
            AttachmentKind::Photo => 0,
            AttachmentKind::Voice => 1,
            AttachmentKind::Document => 2,
            AttachmentKind::Video => 3,
            AttachmentKind::Sticker => 4,
        };
        counts[i] += 1;
    }
    let labels = ["photo", "voice", "document", "video", "sticker"];
    let parts: Vec<String> = counts
        .iter()
        .zip(labels.iter())
        .filter(|(n, _)| **n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    Some(parts.join(", "))
}

/// Sprint 54 silent-drop layer-4 hotfix: human-readable body
/// placeholder used when the inbox message has no text but attachments
/// are present.
pub(crate) fn attachment_body_placeholder(
    attachments: &[crate::channel::event::Attachment],
) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let summary = summarize_attachments_for_header(attachments).unwrap_or_default();
    if attachments.len() == 1 {
        if let Some(name) = &attachments[0].original_filename {
            return format!("[{summary}: {name}]");
        }
    }
    format!("[{summary} attached]")
}

/// Pure function: build the notification string that will be injected to PTY.
/// `pointer_only=true` → header-only (no body); `false` → inline text.
pub fn format_notification_for_inject(
    pointer_only: bool,
    source: &NotifySource<'_>,
    text: &str,
    attachments: &[crate::channel::event::Attachment],
) -> String {
    let attach_summary = summarize_attachments_for_header(attachments);
    if pointer_only {
        let mut header = format!(
            "[{source}] [AGEND-MSG] size={} (use inbox tool)",
            text.len()
        );
        if let Some(s) = &attach_summary {
            header.push_str(&format!(" attachments=[{s}]"));
        }
        // #1509: stamp the operator-TZ `now=` on this pointer header too,
        // matching the other formatters (format_header / format_event_header /
        // enqueue_with_idle_hint). EVERY `notify_agent` path flows through here
        // — telegram-inbound, conflict_notify, supervisor state-change notices,
        // boot canonical-hygiene — so this single call closes the #1487 gap for
        // all of them at once. Placed before `reply_hint` so it stays in the
        // space-delimited `key=value` header region, not the prose hint.
        header.push(' ');
        header.push_str(&operator_now_field());
        header.push_str(&source.reply_hint());
        header
    } else {
        // Body-replace path: empty text + non-empty attachments would
        // produce a content-less inline notification pre-r0. Substitute
        // the placeholder so the agent at minimum sees what kind of
        // media is waiting.
        let display_text = if text.is_empty() && !attachments.is_empty() {
            attachment_body_placeholder(attachments)
        } else if text.chars().count() > 200 {
            let truncated: String = text.chars().take(200).collect();
            format!("{truncated}... (use the inbox MCP tool to read full message)")
        } else {
            text.to_string()
        };
        // #1509 NOTE: the body-replace inline form has NO `[AGEND-MSG]` header
        // to host a `now=` field, and deployments that want the timestamp run
        // pointer mode (the branch above carries it). Leaving body-mode
        // unstamped is intentional — revisit only if a header-less `now=` is
        // ever required.
        format!("[{source}] {display_text}{}", source.reply_hint())
    }
}

pub fn notify_agent(home: &Path, agent_name: &str, source: &NotifySource<'_>, text: &str) {
    notify_agent_with_attachments(home, agent_name, source, text, &[]);
}

/// Sprint 54 silent-drop layer-4 hotfix: variant of `notify_agent` that
/// carries attachment metadata into the PTY-inject formatter.
pub fn notify_agent_with_attachments(
    home: &Path,
    agent_name: &str,
    source: &NotifySource<'_>,
    text: &str,
    attachments: &[crate::channel::event::Attachment],
) {
    // Sprint 24 P1 (F-NEW-DAEMON-HEALTH-CLASSIFIER-1): record this
    // central inject point so the daemon health classifier can
    // distinguish "idle waiting (no input pending)" from "hung
    // unresponsive (input pending past last response)".
    crate::daemon::heartbeat_pair::update_with(agent_name, |p| {
        p.last_input_at_ms = crate::daemon::heartbeat_pair::now_ms();
    });
    let notification =
        format_notification_for_inject(pointer_only_inject(), source, text, attachments);
    compose_aware_inject(home, agent_name, &notification);
    // #836: record the (agent, msg_id) tuple in the dedup ledger
    if let Some(msg_id) =
        crate::daemon::notification_dedup::extract_msg_id_from_header(&notification)
    {
        crate::daemon::notification_dedup::global().record_inject(agent_name, &msg_id);
    }
}

/// Compose-aware notification delivery: gates on `draft_state` (the
/// input-vs-submit signal, #1457) and enqueues when the target agent has an
/// unsent draft, otherwise injects **with** submit_key so idle agents wake up.
/// Actionable work-delivery (`notification_is_actionable_wake`, #1473) bypasses
/// the gate and always injects.
/// #1513: operator-typing quiet window — an actionable wake injected within this
/// many ms of the operator's last keystroke would collide with their input, so
/// it is deferred and drained once the pane settles. Short by design (NOT the
/// #1457 full draft hold) so a reviewer's ci-ready never waits behind a long
/// operator draft (preserves #1473).
const TYPING_QUIET_WINDOW_MS: i64 = 1_500;

pub fn compose_aware_inject(home: &Path, agent_name: &str, notification: &str) {
    // #911 dedup gate
    if should_suppress_911_reinject_with_ledger(
        home,
        agent_name,
        notification,
        crate::daemon::notification_dedup::global(),
    ) {
        return;
    }
    let actionable = notification_is_actionable_wake(notification);
    // #1513: busy-gate BEFORE the actionable split so every path is covered.
    // agent_state is read LOCK-FREE from the per-tick snapshot — the inject path
    // must NEVER take the per-agent core lock (#1492 self-IPC-under-lock deadlock
    // class). ≤1-tick staleness is acceptable (busy states persist > a tick); the
    // time-critical "operator typing" signal uses the live keystroke metadata.
    if should_defer_inject(
        home,
        agent_name,
        crate::snapshot::agent_state_of(home, agent_name).as_deref(),
        actionable,
    ) {
        persist_or_log!(
            crate::notification_queue::enqueue_classified(
                home,
                agent_name,
                notification,
                actionable,
            ),
            "compose_aware_inject",
            agent_name
        );
        return;
    }
    // #1473: actionable work-delivery (ci-ready / task dispatch / query) wakes
    // the PTY regardless of an operator DRAFT — only the busy/typing gate above
    // defers it. Ambient stays behind the #1457 draft gate in route_notification.
    if actionable {
        let _ = inject_with_submit(home, agent_name, notification);
        // #2044: arm delivery-verification — if this actionable wake is
        // swallowed by an open operator dialog (no UserPromptSubmit follows),
        // the per-tick watchdog re-delivers it once. No-op for non-hook
        // backends (arm self-gates on hook history).
        crate::daemon::inject_delivery::arm(agent_name, notification);
        return;
    }
    let _ = route_notification(home, agent_name, notification, |msg| {
        inject_with_submit(home, agent_name, msg)
    });
}

/// #1513: should this notification be DEFERRED (enqueued) rather than injected
/// now? Pure decision over the lock-free snapshot state + live keystroke recency.
pub(crate) fn should_defer_inject(
    home: &Path,
    agent_name: &str,
    agent_state: Option<&str>,
    actionable: bool,
) -> bool {
    // An actionable wake must reach an agent STUCK waiting for input — a
    // permission / awaiting-operator pane is exactly where new work delivery
    // should land, never be held.
    if actionable && matches!(agent_state, Some("permission") | Some("awaiting_operator")) {
        return false;
    }
    // Agent actively generating → injecting now corrupts the PTY stream
    // (mid-token). Applies to BOTH classes.
    if matches!(agent_state, Some("thinking") | Some("tool_use")) {
        return true;
    }
    if actionable {
        // #1675: defer a wake while the operator has a LIVE unsubmitted draft.
        // `operator_typing_recent` is a 1.5s window — pause-blind, so a slow /
        // multi-line typist pausing >1.5s fell through and the inject's
        // submit_key force-submitted their half-typed line. The order-based
        // `Drafting` signal (#1457: typed > submit) is pause-immune (holds up to
        // the 5-min escape window), so it covers slow composition. Union keeps the
        // brief post-keystroke settle (window) AND the paused-but-live draft
        // (order). `None`/`Abandoned` still wake immediately, preserving #1473's
        // "wake the idle/abandoned agent" (a never-composed pane reads `None`);
        // the TUI flush drains the queue the instant the operator submits
        // (draft_state → `None`). `Drafting` is set ONLY by human TUI keystrokes
        // (record_input_activity), so this fires exactly when a person is
        // composing in this pane — agent PTY output never sets it.
        operator_typing_recent(home, agent_name)
            || crate::notification_queue::draft_state(home, agent_name)
                == crate::notification_queue::DraftState::Drafting
    } else {
        // Ambient: the #1457 full draft gate is applied downstream by
        // route_notification — don't double-gate here.
        false
    }
}

/// #1513: did the operator type into this pane within the quiet window? Uses the
/// LIVE keyboard-written `last_input_epoch_ms` (notification_queue metadata) —
/// NOT `heartbeat_pair.last_input_at_ms` (which is the daemon-INJECT timestamp).
///
/// `pub(crate)` so the drain-release path (`app::flush_release`, #1513 case A)
/// shares this ONE typing source-of-truth with the inject-time gate.
pub(crate) fn operator_typing_recent(home: &Path, agent_name: &str) -> bool {
    let (typed_ms, _) = crate::notification_queue::read_input_submit_timestamps(home, agent_name);
    typed_ms != 0
        && chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(typed_ms)
            < TYPING_QUIET_WINDOW_MS
}

/// #1513 PR-2: defer a DIRECT PTY inject (cron / schedule replay via
/// `inject_to_agent`, force=false) when the agent is mid-generation
/// (Thinking/ToolUse) or the operator is mid-keystroke — the same collision
/// avoidance as the notification path, minus the actionable / permission-bypass
/// nuances (a scheduled wake has no work-delivery urgency). Lock-free snapshot
/// read (#1492-safe).
pub(crate) fn should_defer_direct_inject(home: &Path, agent_name: &str) -> bool {
    crate::snapshot::agent_is_busy(home, agent_name) || operator_typing_recent(home, agent_name)
}

/// #1473/#1483: does this notification carry an actionable work-delivery
/// marker that must reach the PTY regardless of draft state? These are the
/// system/orchestrator wakes the agent is expected to act on (vs. ambient
/// notifications that may wait behind an operator draft).
///
/// #1483 fix: match the `kind=` field of the `[AGEND-MSG-PENDING] … kind=… …`
/// pointer that `compose_aware_inject` actually receives (built by
/// `enqueue_with_idle_hint`), NOT the bracketed body markers (`[delegate_task]`
/// etc.) which only appear in the message *text* — never in the PTY-wake
/// pointer. The pre-#1483 bracketed match never fired in production, so
/// actionable wakes could still be deferred behind a genuine operator draft.
pub(crate) fn notification_is_actionable_wake(notification: &str) -> bool {
    // The pointer renders `kind={msg.kind}` (see `enqueue_with_idle_hint`):
    // ci-ready → "ci-ready-for-action", kind=task dispatch → "task",
    // kind=query → "query". The trailing space (` from=` follows in the
    // pointer) keeps the match field-exact so a future "kind=task_foo" can't
    // false-positive on "kind=task".
    const ACTIONABLE_KINDS: &[&str] = &[
        "kind=ci-ready-for-action ", // daemon CI-pass handoff
        "kind=task ",                // kind=task dispatch
        "kind=query ",               // kind=query
    ];
    ACTIONABLE_KINDS.iter().any(|m| notification.contains(m))
}

/// #911 dedup gate predicate. Hybrid (A)+(B) suppression check for
/// a candidate PTY notification.
pub(crate) fn should_suppress_911_reinject_with_ledger(
    home: &Path,
    agent_name: &str,
    notification: &str,
    ledger: &crate::daemon::notification_dedup::Ledger,
) -> bool {
    // Reviewer condition C: only canonical `HEADER_PREFIX` headers gate
    if !notification.starts_with(HEADER_PREFIX) {
        return false;
    }
    let Some(msg_id) = crate::daemon::notification_dedup::extract_msg_id_from_header(notification)
    else {
        return false;
    };
    // Fast path (B): in-memory ledger.
    if ledger.should_suppress_reinject(agent_name, &msg_id) {
        tracing::info!(
            agent = %agent_name,
            msg_id = %msg_id,
            "#911 compose_aware_inject suppressed: dedup ledger hit"
        );
        return true;
    }
    // Fallback (A): JSONL read_at source-of-truth.
    if storage::msg_already_drained_in_jsonl(home, agent_name, &msg_id) {
        tracing::info!(
            agent = %agent_name,
            msg_id = %msg_id,
            "#911 compose_aware_inject suppressed: JSONL fallback (ledger MISS, read_at set)"
        );
        return true;
    }
    false
}

/// #982 Direction A: persist a message to the recipient's inbox AND emit a
/// best-effort `[AGEND-MSG-PENDING]` PTY hint so daemon-side events do not
/// strand silently when the recipient is at an idle prompt.
pub fn enqueue_with_idle_hint(home: &Path, target: &str, msg: InboxMessage) -> anyhow::Result<()> {
    // #1492: this is a self-IPC vector — the default emitter's PTY inject
    // reaches `api::call` over the loopback socket. Calling it while holding
    // the registry lock deadlocks the daemon (the morning cron bug). #1492-L2:
    // the guard is always-on and fail-fast — on a violation it logs + returns
    // `Err` here in every build, so the call is refused (the hint is not
    // emitted) and the daemon stays live instead of freezing.
    crate::sync_audit::assert_no_registry_lock_for_self_ipc("enqueue_with_idle_hint")?;
    enqueue_with_idle_hint_with_emitter(home, target, msg, |hint| {
        compose_aware_inject(home, target, hint);
    })
}

/// #1859 Fix A: daemon-side re-nudge of a target that has an UNREAD actionable
/// handoff still sitting in its inbox. The actionable `[ci-ready-for-action]`
/// wake from the poller is deferrable (`should_defer_inject`'s mid-token guard)
/// into the `notification_queue`, whose ONLY flush is the TUI loop
/// (`app/mod.rs::flush_idle_notifications`) — so when the operator TUI isn't
/// draining the target's pane, a deferred wake strands (inbox has it, no active
/// nudge: #1859 Scenario A). This re-fires an actionable PTY pointer directly,
/// BYPASSING the queue, so the redelivery doesn't depend on the TUI.
///
/// It is a pure WAKE pointer (`id="renudge"`), NOT a new inbox row — so it can
/// never self-amplify into another unread handoff. The CALLER (the handoff
/// watchdog) gates this on the target being idle AND on a re-nudge interval, so
/// it never injects mid-token and never storms; `inject_with_submit` is the same
/// submit-aware PTY path the normal idle-hint uses.
pub(crate) fn renudge_actionable_unread(
    home: &Path,
    target: &str,
    kind: &str,
    unread_count: usize,
) {
    let now_field = operator_now_field();
    let pointer = build_pending_pointer("renudge", kind, "system:ci", unread_count, &now_field);
    if let Err(e) = inject_with_submit(home, target, &pointer) {
        tracing::debug!(%target, error = %e, "renudge_actionable_unread: inject failed");
    }
}

/// #1335: convenience wrapper for the common daemon notification pattern:
/// `InboxMessage::new_system` + optional `delivery_mode` / `correlation_id` /
/// `task_id` + `enqueue_with_idle_hint`. Covers ~15 watchdog-class call sites.
pub fn notify_system(
    home: &Path,
    target: &str,
    source: &str,
    kind: &str,
    body: impl Into<String>,
    correlation_id: Option<&str>,
    task_id: Option<&str>,
) -> anyhow::Result<()> {
    let mut msg = InboxMessage::new_system(source, kind, body).with_delivery_mode("inbox_fallback");
    if let Some(cid) = correlation_id {
        msg = msg.with_correlation_id(cid);
    }
    if let Some(tid) = task_id {
        msg.task_id = Some(tid.to_owned());
    }
    enqueue_with_idle_hint(home, target, msg)
}

/// #1493: single source of truth for the `[AGEND-MSG-PENDING]` pointer line.
///
/// Both the producer ([`enqueue_with_idle_hint_with_emitter`]) and the
/// actionable-wake consumer tests build pointers through this fn, so a test
/// input can never *structurally* drift from the real wire shape. That drift
/// is the false-green class behind #1483: the consumer tests once hand-crafted
/// a pointer shape (missing the `now=` field, missing `sanitize_header_value`)
/// that production no longer emits — the matcher tests passed against a shape
/// the consumer never actually sees. Routing both through one builder closes
/// the gap: add a field here and every test input gets it for free.
///
/// `now_field` is the already-formatted `now=…` token (see
/// [`operator_now_field`]); it's passed in rather than computed so callers and
/// tests stay deterministic.
pub(crate) fn build_pending_pointer(
    id: &str,
    kind: &str,
    from_short: &str,
    inbox_count: usize,
    now_field: &str,
) -> String {
    format!(
        "{} id={} kind={} from={} inbox={} {} (use inbox tool)",
        PENDING_HEADER_PREFIX,
        sanitize_header_value(id),
        sanitize_header_value(kind),
        sanitize_header_value(from_short),
        inbox_count,
        now_field,
    )
}

/// Test-seam variant of [`enqueue_with_idle_hint`]. Accepts a closure
/// that receives the formatted hint string so unit tests can verify
/// the wire format without standing up the API loopback.
pub(crate) fn enqueue_with_idle_hint_with_emitter<F>(
    home: &Path,
    target: &str,
    mut msg: InboxMessage,
    emit_hint: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&str),
{
    // Pre-stamp the id so the hint can reference it. enqueue() leaves an
    // existing id untouched, so this stays a single auto-assignment.
    storage::ensure_msg_id(&mut msg);
    let id = msg.id.clone().unwrap_or_default();
    let from = msg.from.clone();
    let kind = msg.kind.clone();
    let msg_text = msg.text.clone();

    // Single lock scope: enqueue + count in one read, avoiding double I/O.
    let pending = storage::enqueue_returning_unread_count(home, target, msg)?;
    let from_short = from.strip_prefix("from:").unwrap_or(&from);
    let kind_str = kind.as_deref().unwrap_or("");
    // #1134: ci-watch messages whose headline is a CI conclusion
    // (pass/fail/ended) get a friendly inline format instead of the
    // generic AGEND-MSG-PENDING pointer, reducing dual-delivery feel.
    let is_ci_conclusion = kind_str == "ci-watch"
        && (msg_text.starts_with("[ci-pass]")
            || msg_text.starts_with("[ci-fail]")
            || msg_text.starts_with("[ci-ended]"));
    // #1487: `now=<operator-TZ timestamp>` so the agent sees fresh, correctly-
    // zoned time on the wake hint too (space-free value — see operator_now_field).
    let now_field = operator_now_field();
    let hint = if is_ci_conclusion {
        let first_line = msg_text.lines().next().unwrap_or(&msg_text);
        format!("{} (inbox={}) {}", first_line, pending, now_field)
    } else {
        // #1493: build via the shared `build_pending_pointer` so the consumer
        // tests exercise this exact shape (no hand-crafted drift).
        build_pending_pointer(&id, kind_str, from_short, pending, &now_field)
    };
    emit_hint(&hint);
    Ok(())
}

fn inject_with_submit(home: &Path, agent_name: &str, message: &str) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": agent_name, "data": message}
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("inject with submit failed")
        );
    }
}

/// #982 RC: submit-aware notification inject for the
/// `notification_queue` flush path.
pub fn inject_notification_with_submit(
    home: &Path,
    agent_name: &str,
    notification: &str,
) -> anyhow::Result<()> {
    inject_with_submit(home, agent_name, notification)
}

/// #1513: MAX_DEFER anti-starvation caps — once an item has been deferred this
/// long it is released even while the agent is still busy. Actionable wakes get
/// a tight cap (work delivery must land fast); ambient can wait longer.
pub(crate) const ACTIONABLE_MAX_DEFER_MS: i64 = 1_000;
pub(crate) const AMBIENT_MAX_DEFER_MS: i64 = 7_000;

/// #1513: should this queued item be RELEASED (injected) now vs HELD? Released
/// when the pane is SETTLED — the agent is not mid-generation AND the operator
/// isn't mid-keystroke — OR the item is past its MAX_DEFER cap.
///
/// #1513 case A: the drain previously held only on `agent_busy`, so the
/// anti-starvation release could land an inject on the operator's input line
/// mid-typing. `typing_recent` (the SAME `operator_typing_recent` live-keystroke
/// signal the inject-time gate uses — one source of truth) now also holds. The
/// MAX_DEFER cap stays the backstop: a perpetually-busy OR perpetually-typing
/// operator never traps the queue (actionable work still lands within
/// `ACTIONABLE_MAX_DEFER_MS`). Pure so the hold/release matrix is unit-testable.
pub(crate) fn flush_release(
    item: &crate::notification_queue::QueuedNotification,
    agent_busy: bool,
    typing_recent: bool,
    now_ms: i64,
) -> bool {
    let cap = if item.actionable {
        ACTIONABLE_MAX_DEFER_MS
    } else {
        AMBIENT_MAX_DEFER_MS
    };
    if now_ms.saturating_sub(item.deferred_since_ms) >= cap {
        return true; // MAX_DEFER backstop wins, even mid-generation / mid-keystroke.
    }
    !agent_busy && !typing_recent
}

/// Shared deferred-queue flush core — the single gating/delivery path used by
/// BOTH flushers: the TUI event loop (per visible pane,
/// `app::flush_notifications_for_pane`) and the daemon's per-tick
/// `notification_flush` handler (headless `run_core`, where no TUI loop
/// exists — the gap that stranded deferred operator messages, 2026-06-10).
///
/// Gating (#1457/#1513, behavior unchanged from the former app-only flush):
/// Drafting → defer everything; Abandoned → escape valve releases just the
/// oldest (trickle, no clobbering batch); None (clean buffer) → drain the
/// backlog, holding items while the agent is mid-generation or the operator
/// is mid-keystroke, bounded by the MAX_DEFER anti-starvation caps.
pub(crate) fn flush_agent_queue<F>(home: &Path, agent_name: &str, injector: F)
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    // Raw draft state only: the #1944/#1948 input-box probe needs the rendered
    // pane (TUI-owned vterm), so the headless caller conservatively honors the
    // timestamp heuristic — fail toward draft-protection. The TUI wrapper
    // (`app::flush_notifications_for_pane`) probes and passes the refined
    // state via `flush_agent_queue_with_state`.
    let state = crate::notification_queue::draft_state(home, agent_name);
    flush_agent_queue_with_state(home, agent_name, state, injector)
}

/// Shared flush core — the SAME gating for the TUI flush and the daemon's
/// per-tick headless flush: Drafting holds, Abandoned trickles the oldest,
/// None drains through the busy/typing holds + MAX_DEFER anti-starvation
/// caps (`flush_release`). The caller supplies the (possibly probe-refined)
/// draft state.
pub(crate) fn flush_agent_queue_with_state<F>(
    home: &Path,
    agent_name: &str,
    draft_state: crate::notification_queue::DraftState,
    mut injector: F,
) where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    use crate::notification_queue::{self, DraftState};
    match draft_state {
        DraftState::Drafting => {}
        DraftState::Abandoned => {
            if let Some(notification) = notification_queue::drain_one(home, agent_name) {
                if injector(&notification.text).is_err() {
                    notification_queue::requeue_all(home, agent_name, &[notification]);
                }
            }
        }
        DraftState::None => {
            let mut queued = notification_queue::drain(home, agent_name);
            if queued.is_empty() {
                return;
            }
            // #1513: actionable wakes drain FIRST, then ambient (stable by ts).
            queued.sort_by(|a, b| {
                b.actionable
                    .cmp(&a.actionable)
                    .then_with(|| a.timestamp.cmp(&b.timestamp))
            });
            // #1513: if the agent is mid-generation (Thinking/ToolUse), injecting
            // now would corrupt the PTY stream — HOLD non-expired items and only
            // release those past their MAX_DEFER cap (anti-starvation). The state
            // read is lock-free from the snapshot (inject path must not take the
            // core lock — #1492). Once any inject fails, preserve the remaining
            // order by holding the rest.
            let agent_busy = crate::snapshot::agent_is_busy(home, agent_name);
            // #1513 case A: same live-keystroke signal the inject-time gate uses,
            // so a queued item is held off the operator's input line at the drain
            // too (bounded by the MAX_DEFER cap below).
            let typing_recent = operator_typing_recent(home, agent_name);
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mut keep: Vec<notification_queue::QueuedNotification> = Vec::new();
            let mut inject_failed = false;
            for notification in queued {
                if inject_failed || !flush_release(&notification, agent_busy, typing_recent, now_ms)
                {
                    keep.push(notification);
                } else if injector(&notification.text).is_err() {
                    inject_failed = true;
                    keep.push(notification);
                }
            }
            if !keep.is_empty() {
                notification_queue::requeue_all(home, agent_name, &keep);
            }
        }
    }
}

pub(super) fn route_notification<F>(
    home: &Path,
    agent_name: &str,
    notification: &str,
    mut injector: F,
) -> anyhow::Result<()>
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    // #1457: queue (defer) whenever an unsent draft exists — both while the
    // operator is actively composing and after the escape window (the flush
    // path trickles the backlog out one-at-a-time once abandoned). Inject
    // directly only when the buffer is clean (all typed input was submitted).
    use crate::notification_queue::DraftState;
    if crate::notification_queue::draft_state(home, agent_name) != DraftState::None {
        crate::notification_queue::enqueue(home, agent_name, notification)?;
        return Ok(());
    }
    injector(notification)
}

#[cfg(test)]
mod flush_release_tests_1513 {
    use super::{flush_release, ACTIONABLE_MAX_DEFER_MS};
    use crate::notification_queue::QueuedNotification;

    fn item(actionable: bool, deferred_since_ms: i64) -> QueuedNotification {
        QueuedNotification {
            text: "x".into(),
            timestamp: String::new(),
            actionable,
            deferred_since_ms,
        }
    }

    #[test]
    fn not_busy_not_typing_always_releases() {
        let now = 10_000;
        assert!(
            flush_release(&item(true, now), false, false, now),
            "settled agent releases actionable"
        );
        assert!(
            flush_release(&item(false, now), false, false, now),
            "settled agent releases ambient"
        );
    }

    #[test]
    fn busy_holds_then_cap_releases() {
        let base = 100_000;
        // fresh defer while busy (not typing) → held
        assert!(
            !flush_release(&item(true, base), true, false, base),
            "busy holds fresh actionable"
        );
        assert!(
            !flush_release(&item(false, base), true, false, base),
            "busy holds fresh ambient"
        );
        // past the actionable cap (1s) but within ambient cap (7s) → actionable releases, ambient holds
        let mid = base + ACTIONABLE_MAX_DEFER_MS + 1;
        assert!(
            flush_release(&item(true, base), true, false, mid),
            "actionable releases past its 1s cap even while busy"
        );
        assert!(
            !flush_release(&item(false, base), true, false, mid),
            "ambient still held at ~1s while busy"
        );
        // well past ambient cap → ambient releases too
        let late = base + 8_000;
        assert!(
            flush_release(&item(false, base), true, true, late),
            "ambient releases past its cap even while typing (backstop)"
        );
    }

    /// #1513 case A: the operator-typing hold + its MAX_DEFER backstop, across
    /// the four scenarios in the fix spec.
    #[test]
    fn typing_holds_until_cap() {
        let base = 100_000;
        // (1) busy + actionable + typing + NOT past cap → defer (no collision).
        assert!(
            !flush_release(&item(true, base), true, true, base),
            "typing holds a fresh actionable wake off the input line"
        );
        // also holds when the agent is idle but the operator is mid-keystroke —
        // the case the old `!agent_busy` early-return missed.
        assert!(
            !flush_release(&item(true, base), false, true, base),
            "typing holds even when the agent is idle"
        );
        // (2) same but PAST MAX_DEFER → release (backstop; task dispatch 1s).
        let past = base + ACTIONABLE_MAX_DEFER_MS + 1;
        assert!(
            flush_release(&item(true, base), true, true, past),
            "actionable releases past its 1s cap despite typing"
        );
        // (3) NOT typing → unchanged from before (release when settled).
        assert!(
            flush_release(&item(true, base), false, false, base),
            "not typing + idle → release as before"
        );
        assert!(
            !flush_release(&item(true, base), true, false, base),
            "not typing + busy + fresh → held as before"
        );
        // (4) ambient honors typing too, bounded by its 7s cap.
        assert!(
            !flush_release(&item(false, base), false, true, base + 6_000),
            "ambient held while typing within its cap"
        );
        assert!(
            flush_release(&item(false, base), false, true, base + 7_001),
            "ambient releases past its 7s cap despite typing"
        );
    }
}

#[cfg(test)]
mod actionable_wake_tests {
    use super::{build_pending_pointer, notification_is_actionable_wake as is_actionable};

    // #1483/#1493: these assert against the REAL pointer shape
    // `compose_aware_inject` receives — built by the SAME `build_pending_pointer`
    // the producer uses, NOT a hand-crafted string. Hand-crafting was the
    // false-green trap: the pre-#1483 matcher keyed on bracketed body markers
    // that never appear in the pointer (dead code), and the old tests had since
    // drifted to a shape missing the #1487 `now=` field. Routing the test input
    // through the producer's builder means any future field addition is
    // exercised automatically and the input can never structurally diverge.

    /// A fixed `now=` token — the value is irrelevant to the matcher; using a
    /// constant keeps these tests deterministic while still including the field.
    const NOW: &str = "now=2026-05-30T16:12:34+08:00";

    /// ci-ready / task dispatch / query pointers → actionable → bypass draft-gate.
    #[test]
    fn actionable_kinds_recognized_in_real_pointer() {
        assert!(is_actionable(&build_pending_pointer(
            "m-1",
            "ci-ready-for-action",
            "system:ci",
            1,
            NOW
        )));
        assert!(is_actionable(&build_pending_pointer(
            "m-2",
            "task",
            "fixup-lead",
            3,
            NOW
        )));
        assert!(is_actionable(&build_pending_pointer(
            "m-3",
            "query",
            "fixup-lead",
            1,
            NOW
        )));
    }

    /// Regression-proof for #1483: the bracketed body markers (which live in
    /// the message TEXT, not the wake pointer) must NOT be how we match — that
    /// was the dead-code bug. A pointer is what production sends.
    #[test]
    fn bracketed_body_markers_alone_do_not_match() {
        // Raw body text (never reaches compose_aware_inject) — must be false.
        assert!(!is_actionable(
            "[ci-ready-for-action] owner/repo@br: CI passed."
        ));
        assert!(!is_actionable(
            "[from:lead] [delegate_task] do X (task id: t-1)"
        ));
    }

    /// Ambient / informational pointers (non-actionable kinds) stay draft-gated.
    #[test]
    fn non_actionable_kinds_stay_gated() {
        // report / update pointers — informational, not work-delivery.
        assert!(!is_actionable(&build_pending_pointer(
            "m-4",
            "report",
            "fixup-dev",
            1,
            NOW
        )));
        assert!(!is_actionable(&build_pending_pointer(
            "m-5",
            "update",
            "fixup-lead",
            1,
            NOW
        )));
        // ci-conclusion friendly hint (kind=ci-watch, no `kind=` actionable) — informational.
        assert!(!is_actionable(
            "[ci-pass] owner/repo@br: passed ✓ (inbox=1)"
        ));
        // field-exactness: a superstring kind must not false-match `kind=task`.
        assert!(!is_actionable(&build_pending_pointer(
            "m-6",
            "task_summary",
            "x",
            1,
            NOW
        )));
    }
}

#[cfg(test)]
mod operator_tz_tests {
    use super::{format_header, operator_now_field};
    use crate::inbox::message::InboxMessage;

    /// #1487: the `now=` field is space-free so it survives the header's
    /// space-delimited tokenization (the exact tz-conversion math is covered
    /// in `display_time::format_iso_offset` tests). Uses whatever
    /// `display_timezone` is configured in this process (None → system local).
    #[test]
    fn operator_now_field_is_space_free() {
        let field = operator_now_field();
        assert!(field.starts_with("now="), "must be a now= field: {field}");
        assert!(
            !field["now=".len()..].contains(' '),
            "now= value must not contain spaces (would break header tokenization): {field}"
        );
    }

    /// #1487: the [AGEND-MSG] header carries `now=` AND still carries the
    /// existing fields agents parse (id / kind / size) — additive, no break.
    #[test]
    fn message_header_includes_now_without_breaking_existing_fields() {
        let mut msg = InboxMessage {
            from: "from:lead".to_string(),
            text: "hello".to_string(),
            ..Default::default()
        };
        msg.id = Some("m-1".to_string());
        msg.kind = Some("task".to_string());
        let header = format_header(&msg);
        assert!(header.contains("now="), "header must carry now=: {header}");
        assert!(header.contains("id=m-1"), "id= preserved: {header}");
        assert!(header.contains("kind=task"), "kind= preserved: {header}");
        assert!(header.contains("size=5"), "size= preserved: {header}");
    }

    /// #1509: the `notify_agent` formatter must ALSO stamp `now=` in its
    /// pointer/header form. Every notify_agent path flows through it
    /// (telegram-inbound, conflict_notify, supervisor state-change notices, boot
    /// canonical-hygiene) — pre-#1509 it was the one formatter that dropped it.
    #[test]
    fn notify_agent_pointer_header_includes_now_1509() {
        use super::{format_notification_for_inject, NotifySource};
        let header = format_notification_for_inject(
            true, // pointer mode — the [AGEND-MSG] header form
            &NotifySource::System("telegram"),
            "an inbound operator message",
            &[],
        );
        assert!(
            header.contains("[AGEND-MSG]"),
            "pointer header expected: {header}"
        );
        assert!(
            header.contains("now="),
            "#1509: notify_agent pointer header must carry now= (was the gap): {header}"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod should_defer_inject_tests_1513 {
    use super::should_defer_inject;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CTR: AtomicU32 = AtomicU32::new(0);
    fn tmp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-1513-{}-{}-{}",
            tag,
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // Agent mid-generation → defer BOTH classes (injecting corrupts the PTY).
    #[test]
    fn busy_defers_both_classes() {
        let h = tmp_home("busy");
        for st in ["thinking", "tool_use"] {
            assert!(
                should_defer_inject(&h, "a", Some(st), true),
                "actionable defers when {st}"
            );
            assert!(
                should_defer_inject(&h, "a", Some(st), false),
                "ambient defers when {st}"
            );
        }
    }

    // Actionable wake must reach an agent STUCK waiting → bypass defer.
    #[test]
    fn actionable_bypasses_in_permission_and_awaiting() {
        let h = tmp_home("perm");
        for st in ["permission", "awaiting_operator"] {
            assert!(
                !should_defer_inject(&h, "a", Some(st), true),
                "actionable bypasses {st}"
            );
        }
    }

    // No typing + idle (or unknown/stale snapshot) → inject (no false defer).
    #[test]
    fn idle_and_stale_snapshot_do_not_defer() {
        let h = tmp_home("idle");
        assert!(
            !should_defer_inject(&h, "a", Some("idle"), true),
            "idle actionable injects"
        );
        assert!(
            !should_defer_inject(&h, "a", Some("idle"), false),
            "idle ambient injects (was 'ready' pre-merge)"
        );
        // stale/missing snapshot → state None → fail-open (no defer)
        assert!(
            !should_defer_inject(&h, "a", None, true),
            "missing snapshot does not defer actionable"
        );
        assert!(
            !should_defer_inject(&h, "a", None, false),
            "missing snapshot does not defer ambient"
        );
    }

    // Ambient is NEVER deferred by this gate when idle — the #1457 full draft
    // gate downstream (route_notification) owns operator-draft deferral (#1473).
    #[test]
    fn ambient_idle_falls_through_to_1457() {
        let h = tmp_home("amb");
        crate::notification_queue::record_input_activity(&h, "a"); // operator just typed
                                                                   // ambient + idle + recent typing → still NOT deferred here (route_notification handles it)
        assert!(
            !should_defer_inject(&h, "a", Some("idle"), false),
            "ambient defers via #1457 downstream, not here"
        );
    }

    // Actionable + recent operator keystroke → defer (short anti-collision window).
    #[test]
    fn actionable_defers_on_recent_typing() {
        let h = tmp_home("typing");
        crate::notification_queue::record_input_activity(&h, "a"); // now
        assert!(
            should_defer_inject(&h, "a", Some("idle"), true),
            "recent typing defers actionable"
        );
    }

    // #1675: a PAUSED-but-LIVE operator draft (typed >1.5s ago, never submitted →
    // `Drafting`) must STILL defer an actionable wake. Pre-#1675 the 1.5s
    // `operator_typing_recent` window let a paused draft fall through, so the
    // inject's submit_key force-submitted the operator's half-typed line. The
    // order-based `Drafting` check (pause-immune) is what closes that hole.
    #[test]
    fn actionable_defers_on_paused_live_draft_1675() {
        let h = tmp_home("paused-draft");
        // typed 3s ago (> TYPING_QUIET_WINDOW_MS=1.5s), no submit recorded:
        // operator_typing_recent is FALSE but draft_state is Drafting.
        std::fs::create_dir_all(h.join("metadata")).unwrap();
        let typed = chrono::Utc::now().timestamp_millis() - 3_000;
        std::fs::write(
            h.join("metadata").join("a.json"),
            format!("{{\"last_input_epoch_ms\":{typed}}}"),
        )
        .unwrap();
        // sanity: the old window-only signal would NOT defer this.
        assert!(
            !super::operator_typing_recent(&h, "a"),
            "3s-old keystroke is outside the 1.5s window"
        );
        assert!(
            should_defer_inject(&h, "a", Some("idle"), true),
            "#1675: a paused (3s) live operator draft must defer actionable, not force-submit it"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod snapshot_busy_tests_1513 {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CTR: AtomicU32 = AtomicU32::new(0);
    fn tmp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-1513snap-{}-{}-{}",
            tag,
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn snap(name: &str, state: &str) -> crate::snapshot::AgentSnapshot {
        crate::snapshot::AgentSnapshot {
            name: name.to_string(),
            backend_command: String::new(),
            args: vec![],
            working_dir: None,
            submit_key: "\r".to_string(),
            health_state: "healthy".to_string(),
            agent_state: state.to_string(),
            silent_secs: 0,
            output_silent_secs: 0,
        }
    }

    #[test]
    fn agent_is_busy_reads_snapshot_state() {
        let h = tmp_home("busy");
        crate::snapshot::save(&h, &[snap("a", "thinking"), snap("b", "idle")]);
        assert!(crate::snapshot::agent_is_busy(&h, "a"), "thinking → busy");
        assert!(!crate::snapshot::agent_is_busy(&h, "b"), "idle → not busy");
        // missing agent / missing snapshot → fail-open (not busy)
        assert!(
            !crate::snapshot::agent_is_busy(&h, "ghost"),
            "unknown agent → not busy"
        );
        assert!(
            !crate::snapshot::agent_is_busy(&tmp_home("empty"), "a"),
            "no snapshot → not busy"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod should_defer_direct_inject_tests_1513pr2 {
    use super::should_defer_direct_inject;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CTR: AtomicU32 = AtomicU32::new(0);
    fn tmp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-1513pr2-{}-{}-{}",
            tag,
            std::process::id(),
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn snap(name: &str, state: &str) -> crate::snapshot::AgentSnapshot {
        crate::snapshot::AgentSnapshot {
            name: name.to_string(),
            backend_command: String::new(),
            args: vec![],
            working_dir: None,
            submit_key: "\r".to_string(),
            health_state: "healthy".to_string(),
            agent_state: state.to_string(),
            silent_secs: 0,
            output_silent_secs: 0,
        }
    }

    // cron/replay direct inject while the agent is mid-generation → defer.
    #[test]
    fn busy_defers_direct_inject() {
        let h = tmp_home("busy");
        crate::snapshot::save(&h, &[snap("a", "thinking")]);
        assert!(
            should_defer_direct_inject(&h, "a"),
            "thinking → defer direct inject"
        );
        crate::snapshot::save(&h, &[snap("a", "tool_use")]);
        assert!(
            should_defer_direct_inject(&h, "a"),
            "tool_use → defer direct inject"
        );
    }

    // operator mid-keystroke → defer (collision avoidance).
    #[test]
    fn typing_defers_direct_inject() {
        let h = tmp_home("typing");
        crate::snapshot::save(&h, &[snap("a", "idle")]);
        crate::notification_queue::record_input_activity(&h, "a");
        assert!(
            should_defer_direct_inject(&h, "a"),
            "recent keystroke → defer direct inject"
        );
    }

    // quiet idle pane (and missing snapshot) → inject directly, no false defer.
    #[test]
    fn quiet_does_not_defer_direct_inject() {
        let h = tmp_home("quiet");
        crate::snapshot::save(&h, &[snap("a", "idle")]);
        assert!(
            !should_defer_direct_inject(&h, "a"),
            "idle + no typing → inject"
        );
        // missing snapshot → fail-open (not busy, no typing) → inject
        assert!(
            !should_defer_direct_inject(&tmp_home("nosnap"), "a"),
            "no snapshot → inject"
        );
    }
}
