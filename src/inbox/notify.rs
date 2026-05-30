use std::path::Path;

use super::message::{BroadcastContext, InboxMessage, NotifySource};
use super::storage;

/// Size threshold for header-only PTY injection. Messages with body > this
/// value inject only a structured header line; the full body stays in inbox.
#[allow(dead_code)]
pub const HEADER_SIZE_THRESHOLD: usize = 300;

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
    let _ = storage::enqueue(home, agent_name, msg);
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
    // #1473: actionable work-delivery (ci-ready / task dispatch / query) MUST
    // wake the agent's PTY regardless of draft state. The #1457 draft-gate only
    // exists to stop low-priority notifications from clobbering an operator's
    // in-progress draft — it must never defer the work the agent is here to do.
    // (Regression: a never-submitted agent pane read as Abandoned, so ci-ready
    // for a codex reviewer was deferred to inbox-only and it never woke.)
    if notification_is_actionable_wake(notification) {
        let _ = inject_with_submit(home, agent_name, notification);
        return;
    }
    let _ = route_notification(home, agent_name, notification, |msg| {
        inject_with_submit(home, agent_name, msg)
    });
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
    // the registry lock deadlocks the daemon (the morning cron bug). Debug
    // builds panic here for an early, clear signal; release is a no-op.
    crate::sync_audit::assert_no_registry_lock_for_self_ipc("enqueue_with_idle_hint");
    enqueue_with_idle_hint_with_emitter(home, target, msg, |hint| {
        compose_aware_inject(home, target, hint);
    })
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
}
