//! #2524 P6 / #2537: `send.triaged` pre-send validation gate. Mirrors
//! #2249's `plan_ack_required` "companion field required" validation shape
//! (`comms_gates/dispatch.rs`), applied to the `triaged` object instead of a
//! task-create arg. Consumed by `handle_send_to_instance` (kind=update) and
//! `handle_report_result` (kind=report) — `handle_delegate_task` (kind=task)
//! and `handle_request_information` (kind=query) never call this; `triaged`
//! is meaningless on those paths.

use serde_json::{json, Value};

/// Parsed + validated `triaged` directive, ready to forward into
/// [`crate::daemon::discharge_ledger::record_discharge`].
#[derive(Debug)]
pub(crate) struct TriagedDirective {
    pub head: String,
    pub job: String,
    pub reason: Option<String>,
}

/// `Ok(None)` — `triaged` absent, null, or an empty object (no-op, byte-
/// identical to pre-#2537 behavior). `Ok(Some(_))` — both `head` and `job`
/// are non-empty, ready to record. `Err` — one of `head`/`job` present
/// without its companion (mirrors `plan_ack_required`'s "N>0 requires
/// non-empty reason" shape).
pub(crate) fn validate_triaged(args: &Value) -> Result<Option<TriagedDirective>, Value> {
    let Some(triaged) = args.get("triaged").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let head = triaged.get("head").and_then(|v| v.as_str()).unwrap_or("");
    let job = triaged.get("job").and_then(|v| v.as_str()).unwrap_or("");
    if head.is_empty() && job.is_empty() {
        return Ok(None);
    }
    if head.is_empty() || job.is_empty() {
        return Err(json!({
            "error": "triaged.head and triaged.job must both be non-empty (or both omitted)"
        }));
    }
    let reason = triaged
        .get("reason")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    Ok(Some(TriagedDirective {
        head: head.to_string(),
        job: job.to_string(),
        reason,
    }))
}

/// Forward a validated `triaged` directive into the discharge ledger. Call
/// only after the send/report actually succeeded (mirrors `ack_inbox`'s
/// only-on-success contract) — a no-op when `triaged` is `None`. Best-effort:
/// a ledger write failure is logged, not surfaced to the caller, since the
/// message has already gone out by this point.
pub(crate) fn record_triaged_if_present(
    home: &std::path::Path,
    sender: &str,
    triaged: Option<TriagedDirective>,
) {
    let Some(t) = triaged else { return };
    if let Err(e) = crate::daemon::discharge_ledger::record_discharge(
        home,
        &t.head,
        &t.job,
        sender,
        t.reason.as_deref(),
    ) {
        tracing::warn!(head = %t.head, job = %t.job, error = %e, "#2537 discharge ledger write failed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn absent_is_none() {
        assert!(validate_triaged(&json!({})).unwrap().is_none());
    }

    #[test]
    fn null_is_none() {
        assert!(validate_triaged(&json!({"triaged": null}))
            .unwrap()
            .is_none());
    }

    #[test]
    fn empty_object_is_none() {
        assert!(validate_triaged(&json!({"triaged": {}})).unwrap().is_none());
    }

    #[test]
    fn head_present_job_missing_rejects() {
        let err = validate_triaged(&json!({"triaged": {"head": "abc123"}})).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("both"));
    }

    #[test]
    fn job_present_head_missing_rejects() {
        let err = validate_triaged(&json!({"triaged": {"job": "ci-build"}})).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("both"));
    }

    #[test]
    fn both_present_is_ok_with_optional_reason() {
        let t = validate_triaged(
            &json!({"triaged": {"head": "abc123", "job": "ci-build", "reason": "flaky"}}),
        )
        .unwrap()
        .expect("expected Some");
        assert_eq!(t.head, "abc123");
        assert_eq!(t.job, "ci-build");
        assert_eq!(t.reason.as_deref(), Some("flaky"));
    }

    #[test]
    fn both_present_no_reason_is_ok_with_none_reason() {
        let t = validate_triaged(&json!({"triaged": {"head": "abc123", "job": "ci-build"}}))
            .unwrap()
            .expect("expected Some");
        assert_eq!(t.reason, None);
    }

    #[test]
    fn empty_string_reason_normalizes_to_none() {
        let t = validate_triaged(
            &json!({"triaged": {"head": "abc123", "job": "ci-build", "reason": ""}}),
        )
        .unwrap()
        .expect("expected Some");
        assert_eq!(t.reason, None);
    }

    /// Zero-behavior-change invariance (PR-1 hard requirement): when a
    /// caller doesn't pass `triaged` at all — every existing `send`/`report`
    /// call site, pre-#2537 — `record_triaged_if_present` must not touch the
    /// filesystem. Makes the "byte-identical when triaged is absent" claim
    /// empirical for the exact code path wired into
    /// `handle_send_to_instance`/`handle_report_result`, rather than only
    /// asserted in prose.
    #[test]
    fn record_triaged_if_present_with_none_touches_nothing_on_disk() {
        let home =
            std::env::temp_dir().join(format!("agend-triaged-gate-noop-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();

        record_triaged_if_present(&home, "dev-1", None);

        assert!(
            !crate::daemon::discharge_ledger::discharge_ledger_dir(&home).exists(),
            "no `triaged` directive ⟹ the discharge-ledger dir must never be created \
             (zero behavior change when the field is absent)"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
