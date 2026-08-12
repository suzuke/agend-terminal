use serde_json::{json, Value};
use std::path::Path;

/// #2059: the canonical report-message text producer. Prepends the
/// `[report_result] ` wrapper to the summary and appends the optional
/// `correlation_id:` / `Artifacts:` lines. The single builder that BOTH
/// production (`comms::handle_report_result`) and the verdict-matcher tests
/// route through — the #1493 producer-fed-fixture discipline that keeps the
/// downstream verdict detector
/// ([`crate::daemon::auto_release::is_terminal_verdict_text`]) tested against
/// the REAL wire text (incl. the wrapper that previously defeated it, #2059).
/// Lives here (a `comms.rs` overflow sibling) because that file is at the
/// 750-LOC handler cap.
pub(crate) fn build_report_text(
    summary: &str,
    correlation_id: Option<&str>,
    artifacts: Option<&str>,
) -> String {
    let mut msg = format!("[report_result] {summary}");
    if let Some(cid) = correlation_id {
        msg.push_str(&format!("\ncorrelation_id: {cid}"));
    }
    if let Some(artifacts) = artifacts {
        msg.push_str(&format!("\nArtifacts: {artifacts}"));
    }
    msg
}

pub fn handle_describe_message(home: &Path, args: &Value, instance_name: &str) -> Value {
    let msg_id = match args["message_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'message_id'"}),
    };
    let target = args["instance"].as_str().unwrap_or(instance_name);
    let status = crate::inbox::describe_message(home, msg_id, target);
    match status {
        crate::inbox::MessageStatus::ReadAt(t, dm) => {
            let mut resp = json!({"status": "read", "read_at": t});
            if let Some(mode) = dm {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(msg) = crate::inbox::find_message(home, msg_id) {
                if let Some(ref cid) = msg.correlation_id {
                    resp["correlation_id"] = json!(cid);
                }
                if let Some(ref rh) = msg.reviewed_head {
                    resp["reviewed_head"] = json!(rh);
                    resp["stale_possible"] = json!(true);
                }
            }
            resp
        }
        crate::inbox::MessageStatus::Unread {
            delivery_mode,
            correlation_id,
        } => {
            // #bughunt-r2 #3: a live, un-drained message — distinct from
            // not_found so delivery audit sees it was delivered but not yet read.
            let mut resp = json!({"status": "unread"});
            if let Some(mode) = delivery_mode {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(cid) = correlation_id {
                resp["correlation_id"] = json!(cid);
            }
            resp
        }
        crate::inbox::MessageStatus::Delivering {
            delivery_mode,
            correlation_id,
        } => {
            // #2299: delivered to the agent, not yet confirmed processed. Report
            // `delivering` (not `unread`) so a delivery audit does not re-send.
            let mut resp = json!({"status": "delivering"});
            if let Some(mode) = delivery_mode {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(cid) = correlation_id {
                resp["correlation_id"] = json!(cid);
            }
            resp
        }
        crate::inbox::MessageStatus::UnreadExpired => {
            json!({"status": "unread_expired"})
        }
        crate::inbox::MessageStatus::NotFound => {
            json!({"status": "not_found"})
        }
    }
}

pub fn handle_describe_thread(home: &Path, args: &Value) -> Value {
    let thread_id = match args["thread_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'thread_id'"}),
    };
    let instance = args["instance"].as_str();
    let msgs = crate::inbox::get_thread(home, thread_id, instance);
    json!({"thread_id": thread_id, "messages": msgs, "count": msgs.len()})
}

/// #2299/#3228 explicit ack (C): confirm current `delivering` messages as
/// `processed`, or target a previously delivered/reclaimed row when durable
/// history proves prior delivery.
/// `message_id` present → ack that one message (including a requeued row whose
/// durable delivery history proves it was previously delivered); omitted → ack
/// the caller's whole in-flight batch. The response includes a distinguishable
/// outcome so callers can tell an ack-after-reclaim from a current in-flight
/// ack, a never-delivered conservative no-op, or an already-processed row.
pub fn handle_inbox_ack(home: &Path, args: &Value, instance_name: &str) -> Value {
    let msg_id = args["message_id"].as_str();
    let outcome = crate::inbox::ack_with_outcome(home, instance_name, msg_id);
    json!({
        "acked": outcome.acked,
        "scope": if msg_id.is_some() { "targeted" } else { "batch" },
        "outcome": outcome.kind.as_str(),
    })
}

/// #inbox-gc part a: quiet compact-clear. Marks non-obligation messages read
/// and returns bounded compact summaries; obligations stay unread + surface in
/// `requires_response`. Does NOT drain (no reply-ledger arm / heartbeat touch).
pub fn handle_inbox_clear(home: &Path, instance_name: &str) -> Value {
    // #t-…61487: `obligation_reason` is the SHARED KEEP-set predicate — the same one the
    // reclaim re-nudge gate (`reclaim_renudge_worthy` in `reclaim_stale_delivering`) uses,
    // so clear and the reclaim re-nudge can never drift. (decision d-20260607081209372642-1.)
    let result = crate::inbox::clear_compact(home, instance_name, |msg| {
        crate::inbox::obligation_reason(home, msg)
    });
    serde_json::to_value(&result).unwrap_or_else(|e| json!({"error": format!("serialize: {e}")}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #2059 §3.9 — the PRODUCER-FED FIXTURE (#1493 discipline). Routes the REAL
    /// producer output (`build_report_text`, the exact builder
    /// `handle_report_result` uses on the wire) through the downstream verdict
    /// matcher (`auto_release::is_terminal_verdict_text`). Pre-#2059 the matcher
    /// checked `starts_with("VERIFIED")` on the wrapped text → false →
    /// record_verdict never fired (the pipeline-wide silence). The point of
    /// co-locating producer + fixture is that the matcher can never again be
    /// tested against a hand-crafted `"VERIFIED …"` the wire never sends.
    #[test]
    fn matcher_sees_through_report_result_wrapper_producer_fed_2059() {
        use crate::daemon::auto_release::is_terminal_verdict_text;
        for verdict in ["VERIFIED", "REJECTED", "UNVERIFIED"] {
            let summary = format!("{verdict} — ran: cargo test → 3702 passed");
            // Bare wrapper (no suffix).
            let wire = build_report_text(&summary, None, None);
            assert!(
                is_terminal_verdict_text(&wire),
                "{verdict}: producer-wrapped text must be detected (got {wire:?})"
            );
            // With correlation_id + Artifacts suffix lines — the verdict is still
            // at the start, the suffix must not break detection.
            let wire2 = build_report_text(
                &summary,
                Some("t-20260612055508677007-13"),
                Some("https://github.com/suzuke/agend-terminal/pull/2058"),
            );
            assert!(
                is_terminal_verdict_text(&wire2),
                "{verdict}: wrapped + suffixed producer text must be detected"
            );
        }
        // A non-verdict report produced the same way must NOT be a false verdict.
        let progress = build_report_text("Done — pushed PR #2058, CI running", None, None);
        assert!(
            !is_terminal_verdict_text(&progress),
            "a non-verdict report must not read as a verdict (got {progress:?})"
        );
    }

    /// #bughunt-r2 #3: querying a LIVE, un-drained message by id must report
    /// `status: "unread"` (with its delivery_mode + correlation_id) — NOT
    /// `not_found`, which previously broke delivery audit of undelivered work.
    #[test]
    fn describe_live_unread_message_returns_status_unread() {
        let home = std::env::temp_dir().join(format!(
            "agend-bughunt-r2-unread-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let msg = format!(
            r#"{{"schema_version":1,"id":"m-live","from":"lead","text":"hi","kind":"task","timestamp":"{now}","delivery_mode":"pty","correlation_id":"t-abc"}}"#
        );
        std::fs::write(inbox_dir.join("agent1.jsonl"), format!("{msg}\n")).unwrap();

        let resp = handle_describe_message(
            &home,
            &json!({"message_id": "m-live", "instance": "agent1"}),
            "caller",
        );
        assert_eq!(
            resp["status"], "unread",
            "live unread must report status=unread, got {resp}"
        );
        assert_eq!(resp["delivery_mode"], "pty");
        assert_eq!(resp["correlation_id"], "t-abc");

        // A genuinely-absent id stays distinct.
        let nf = handle_describe_message(
            &home,
            &json!({"message_id": "m-nope", "instance": "agent1"}),
            "caller",
        );
        assert_eq!(nf["status"], "not_found");

        std::fs::remove_dir_all(&home).ok();
    }

    /// Targeted ack after reclaim is a distinct contract outcome, so callers
    /// can tell it apart from the normal in-flight batch acknowledgement.
    #[test]
    fn targeted_ack_after_reclaim_reports_distinct_outcome() {
        let home = std::env::temp_dir().join(format!(
            "agend-3228-targeted-ack-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let agent = "agent1";
        crate::inbox::enqueue(
            &home,
            agent,
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some("m-3228-targeted".into()),
                from: "lead".into(),
                text: "ack after reclaim".into(),
                kind: Some("query".into()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(crate::inbox::drain(&home, agent).len(), 1);
        crate::inbox::storage::set_row_delivering_at_for_test(
            &home,
            agent,
            "m-3228-targeted",
            &(chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339(),
        );
        crate::inbox::reclaim_stale_delivering(&home);

        let result = handle_inbox_ack(&home, &json!({"message_id": "m-3228-targeted"}), agent);
        assert_eq!(result["acked"], 1);
        assert_eq!(result["outcome"], "acked-after-reclaim");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3228: the production inbox response, rather than a dead formatter,
    /// must make durable redelivery history visible without changing the
    /// canonical stored message identity or text.
    #[test]
    fn inbox_response_surfaces_real_redelivery_history_3228() {
        let home = std::env::temp_dir().join(format!(
            "agend-3228-inbox-redelivery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let agent = "agent1";
        crate::inbox::enqueue(
            &home,
            agent,
            crate::inbox::InboxMessage {
                schema_version: 1,
                id: Some("m-3228-response".into()),
                from: "lead".into(),
                text: "canonical response text".into(),
                kind: Some("query".into()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(crate::inbox::drain(&home, agent).len(), 1);
        crate::inbox::storage::set_row_delivering_at_for_test(
            &home,
            agent,
            "m-3228-response",
            &(chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339(),
        );
        crate::inbox::reclaim_stale_delivering(&home);

        let response = super::super::handle_inbox(&home, agent);
        assert_eq!(response["messages"][0]["id"], "m-3228-response");
        assert_eq!(response["messages"][0]["text"], "canonical response text");
        assert_eq!(response["messages"][0]["delivery_count"], 2);
        assert_eq!(
            response["redelivery_history"][0]["message_id"],
            "m-3228-response"
        );
        assert_eq!(response["redelivery_history"][0]["delivery_count"], 2);
        assert!(response["redelivery_history"][0]["first_delivered_at"].is_string());

        let stored = crate::inbox::find_message(&home, "m-3228-response").unwrap();
        assert_eq!(stored.id.as_deref(), Some("m-3228-response"));
        assert_eq!(stored.text, "canonical response text");
        std::fs::remove_dir_all(&home).ok();
    }
}
