//! smells#2 (de2eb8): single source for the SEND directive set, ending the
//! 6-place hand-marshal drift the SEND handlers' comments record as real bugs
//! (#1024 `reviewed_head`, #1833/HIGH-1 dispatch directives). Each of the three
//! SEND handlers in `comms.rs` builds ONE [`SendEnvelope`] from `args`, then
//! projects it to BOTH the daemon SEND `params` ([`SendEnvelope::to_send_params`])
//! and the API-down fallback `InboxMessage` ([`SendEnvelope::to_inbox_message`])
//! — the two can no longer diverge. Adding a directive? Add the field + handle
//! it in both projections; the exhaustive `let SendEnvelope { .. }` destructure
//! (no `..`) in each projection turns "added a field, forgot a projection" into
//! a COMPILE error (the drift-guard, alongside the tests below).

use serde_json::{json, Value};

#[derive(Default, Clone)]
pub(super) struct SendEnvelope {
    // routing (every handler sets these)
    pub(super) from: String,
    pub(super) target: String,
    pub(super) text: String,
    pub(super) kind: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) parent_id: Option<String>,
    // dispatch directives (the historically-drifting set; emitted uniformly so
    // a null directive reads as absent — daemon reads them by value, not presence)
    pub(super) correlation_id: Option<String>,
    pub(super) reviewed_head: Option<String>,
    pub(super) sequencing: Option<String>,
    pub(super) eta_minutes: Option<u64>,
    pub(super) reporting_cadence: Option<String>,
    pub(super) worktree_binding_required: Option<bool>,
    pub(super) expect_reply_within_secs: Option<i64>,
    pub(super) terminal: Option<bool>,
    pub(super) no_report_expected: Option<bool>,
    // task-only extras (delegate_task) — emitted only when Some, so send/report
    // params keep their current shape (no null task keys → no presence-check risk)
    pub(super) task_id: Option<String>,
    pub(super) force_meta: Option<Value>,
    pub(super) provenance: Option<Value>,
    pub(super) branch: Option<String>,
}

impl SendEnvelope {
    /// Read the common dispatch-directive set from `args` — the read that used to
    /// be hand-copied into every SEND site. Routing + task-extras are set by the
    /// caller (spread `..SendEnvelope::directives_from_args(args)`).
    pub(super) fn directives_from_args(args: &Value) -> SendEnvelope {
        SendEnvelope {
            correlation_id: args["correlation_id"].as_str().map(String::from),
            reviewed_head: args["reviewed_head"].as_str().map(String::from),
            sequencing: args["sequencing"].as_str().map(String::from),
            eta_minutes: args["eta_minutes"].as_u64(),
            reporting_cadence: args["reporting_cadence"].as_str().map(String::from),
            worktree_binding_required: args["worktree_binding_required"].as_bool(),
            expect_reply_within_secs: args["expect_reply_within_secs"].as_i64(),
            terminal: args["terminal"].as_bool(),
            no_report_expected: args["no_report_expected"].as_bool(),
            ..SendEnvelope::default()
        }
    }

    /// Project to the daemon SEND `params` JSON. Exhaustive destructure (no `..`)
    /// = drift-guard: a new field won't compile until handled here.
    pub(super) fn to_send_params(&self) -> Value {
        let SendEnvelope {
            from,
            target,
            text,
            kind,
            thread_id,
            parent_id,
            correlation_id,
            reviewed_head,
            sequencing,
            eta_minutes,
            reporting_cadence,
            worktree_binding_required,
            expect_reply_within_secs,
            terminal,
            no_report_expected,
            task_id,
            force_meta,
            provenance,
            branch,
        } = self;
        let mut params = json!({
            "from": from,
            "target": target,
            "text": text,
            "kind": kind,
            "thread_id": thread_id,
            "parent_id": parent_id,
            "correlation_id": correlation_id,
            "reviewed_head": reviewed_head,
            "sequencing": sequencing,
            "eta_minutes": eta_minutes,
            "reporting_cadence": reporting_cadence,
            "worktree_binding_required": worktree_binding_required,
            "expect_reply_within_secs": expect_reply_within_secs,
            "terminal": terminal,
            "no_report_expected": no_report_expected,
        });
        // task-only extras: insert only when present (keeps send/report params
        // identical to their pre-refactor shape — no null task keys).
        let obj = params.as_object_mut().expect("json! object");
        if let Some(v) = task_id {
            obj.insert("task_id".into(), json!(v));
        }
        if let Some(v) = force_meta {
            obj.insert("force_meta".into(), v.clone());
        }
        if let Some(v) = provenance {
            obj.insert("provenance".into(), v.clone());
        }
        if let Some(v) = branch {
            obj.insert("branch".into(), json!(v));
        }
        params
    }

    /// Project to the API-down fallback `InboxMessage`. Exhaustive destructure
    /// (no `..`) = drift-guard. Directives with no `InboxMessage` field
    /// (`expect_reply_within_secs` / `no_report_expected` / `provenance` /
    /// `branch` — daemon SEND-path-only, and the fallback bypasses that path)
    /// are explicitly dropped.
    pub(super) fn to_inbox_message(&self) -> crate::inbox::InboxMessage {
        let SendEnvelope {
            from,
            target: _,
            text,
            kind,
            thread_id,
            parent_id,
            correlation_id,
            reviewed_head,
            sequencing,
            eta_minutes,
            reporting_cadence,
            worktree_binding_required,
            expect_reply_within_secs: _,
            terminal,
            no_report_expected: _,
            task_id,
            force_meta,
            provenance: _,
            branch: _,
        } = self;
        crate::inbox::InboxMessage {
            from: format!("from:{from}"),
            text: text.clone(),
            kind: kind.clone(),
            thread_id: thread_id.clone(),
            parent_id: parent_id.clone(),
            correlation_id: correlation_id.clone(),
            reviewed_head: reviewed_head.clone(),
            sequencing: sequencing.clone(),
            eta_minutes: eta_minutes.map(|v| v as u32),
            reporting_cadence: reporting_cadence.clone(),
            worktree_binding_required: *worktree_binding_required,
            terminal: *terminal,
            task_id: task_id.clone(),
            force_meta: force_meta
                .as_ref()
                .and_then(|v| serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()),
            delivery_mode: Some("inbox_fallback".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A fully-populated envelope (every directive Some) for the drift-guard.
    fn full_envelope() -> SendEnvelope {
        SendEnvelope {
            from: "dev-1".to_string(),
            target: "lead".to_string(),
            text: "hi".to_string(),
            kind: Some("report".to_string()),
            thread_id: Some("t-thread".to_string()),
            parent_id: Some("m-parent".to_string()),
            correlation_id: Some("t-corr".to_string()),
            reviewed_head: Some("deadbeef".to_string()),
            sequencing: Some("sequential".to_string()),
            eta_minutes: Some(42),
            reporting_cadence: Some("per-pr".to_string()),
            worktree_binding_required: Some(true),
            expect_reply_within_secs: Some(600),
            terminal: Some(true),
            no_report_expected: Some(true),
            task_id: Some("t-99".to_string()),
            force_meta: None,
            provenance: Some(json!({"from": "dev-1", "task": "do x"})),
            branch: Some("feat/x".to_string()),
        }
    }

    /// DRIFT-GUARD (params): every directive the SEND handlers historically
    /// hand-copied must appear in `to_send_params` with its value. Neuter
    /// (drop any field from `to_send_params`) → this goes RED.
    #[test]
    fn to_send_params_carries_full_directive_set() {
        let p = full_envelope().to_send_params();
        assert_eq!(p["from"], "dev-1");
        assert_eq!(p["target"], "lead");
        assert_eq!(p["text"], "hi");
        assert_eq!(p["kind"], "report");
        assert_eq!(p["thread_id"], "t-thread");
        assert_eq!(p["parent_id"], "m-parent");
        assert_eq!(p["correlation_id"], "t-corr");
        assert_eq!(p["reviewed_head"], "deadbeef");
        assert_eq!(p["sequencing"], "sequential");
        assert_eq!(p["eta_minutes"], 42);
        assert_eq!(p["reporting_cadence"], "per-pr");
        assert_eq!(p["worktree_binding_required"], true);
        assert_eq!(p["expect_reply_within_secs"], 600);
        assert_eq!(p["terminal"], true);
        assert_eq!(p["no_report_expected"], true);
        // task-extras present when Some
        assert_eq!(p["task_id"], "t-99");
        assert_eq!(p["provenance"]["task"], "do x");
        assert_eq!(p["branch"], "feat/x");
    }

    /// DRIFT-GUARD (fallback) + #1024/#1833 FIXED-GAP PIN: the API-down fallback
    /// `InboxMessage` carries the SAME directive set as params — in particular
    /// `reviewed_head` (the #1024 verdict-correlation field that the
    /// `send_to_instance` fallback used to drop). Neuter (drop reviewed_head from
    /// `to_inbox_message`) → RED, proving the fix and preventing regression.
    #[test]
    fn to_inbox_message_carries_full_directive_set_fixed_gap_1024_1833() {
        let m = full_envelope().to_inbox_message();
        assert_eq!(m.from, "from:dev-1");
        assert_eq!(m.kind.as_deref(), Some("report"));
        assert_eq!(m.thread_id.as_deref(), Some("t-thread"));
        assert_eq!(m.parent_id.as_deref(), Some("m-parent"));
        assert_eq!(m.correlation_id.as_deref(), Some("t-corr"));
        // THE fixed gap: reviewed_head must survive into the fallback.
        assert_eq!(m.reviewed_head.as_deref(), Some("deadbeef"));
        assert_eq!(m.sequencing.as_deref(), Some("sequential"));
        assert_eq!(m.eta_minutes, Some(42u32));
        assert_eq!(m.reporting_cadence.as_deref(), Some("per-pr"));
        assert_eq!(m.worktree_binding_required, Some(true));
        assert_eq!(m.terminal, Some(true));
        assert_eq!(m.delivery_mode.as_deref(), Some("inbox_fallback"));
    }

    /// #1024 / #1002 ROOT 2 (behavioral; replaces the prior brittle source-grep
    /// `handle_send_forwards_reviewed_head_to_api_params`): a verdict's
    /// `reviewed_head` from args must reach the SEND `params` (else
    /// `record_verdict` never fires). Routes args through the shared projection.
    #[test]
    fn reviewed_head_from_args_reaches_send_params_1024() {
        let args = json!({ "reviewed_head": "deadbeef" });
        let env = SendEnvelope {
            from: "dev".to_string(),
            target: "lead".to_string(),
            text: "VERIFIED".to_string(),
            kind: Some("report".to_string()),
            ..SendEnvelope::directives_from_args(&args)
        };
        assert_eq!(
            env.to_send_params()["reviewed_head"],
            "deadbeef",
            "MCP-send verdicts must forward reviewed_head (#1024)"
        );
    }

    /// report-style envelope (only correlation_id/reviewed_head/terminal set):
    /// the unset directives appear as JSON null and read back as None — i.e. a
    /// null directive is BEHAVIORALLY identical to an absent one (the inert delta
    /// from giving report_result's params the uniform directive shape).
    #[test]
    fn unset_directives_emit_null_and_read_as_absent_inert() {
        let env = SendEnvelope {
            from: "dev-1".to_string(),
            target: "lead".to_string(),
            text: "VERIFIED ...".to_string(),
            kind: Some("report".to_string()),
            correlation_id: Some("t-corr".to_string()),
            reviewed_head: Some("cafe".to_string()),
            terminal: Some(false),
            ..SendEnvelope::default()
        };
        let p = env.to_send_params();
        // present-but-null
        assert!(p["sequencing"].is_null());
        assert!(p["worktree_binding_required"].is_null());
        assert!(p["eta_minutes"].is_null());
        // …and the daemon's value-read yields None (== absent): inert.
        assert_eq!(p["sequencing"].as_str(), None);
        assert_eq!(p["worktree_binding_required"].as_bool(), None);
        assert_eq!(p["eta_minutes"].as_u64(), None);
    }

    /// task-extras are emitted ONLY when present, so a non-task send/report keeps
    /// its pre-refactor param shape (no null `task_id`/`provenance`/`branch` keys
    /// → no risk a daemon presence-check mis-routes a plain send as a task).
    #[test]
    fn task_extras_omitted_when_none() {
        let env = SendEnvelope {
            from: "dev-1".to_string(),
            target: "lead".to_string(),
            text: "hi".to_string(),
            kind: Some("report".to_string()),
            ..SendEnvelope::default()
        };
        let p = env.to_send_params();
        let obj = p.as_object().unwrap();
        assert!(
            !obj.contains_key("task_id"),
            "no null task_id key on a non-task send"
        );
        assert!(!obj.contains_key("force_meta"));
        assert!(!obj.contains_key("provenance"));
        assert!(!obj.contains_key("branch"));
    }

    /// `directives_from_args` reads the whole directive set from args (the read
    /// that used to be hand-copied into every SEND site).
    #[test]
    fn directives_from_args_reads_all() {
        let args = json!({
            "correlation_id": "t-c", "reviewed_head": "sha", "sequencing": "parallel",
            "eta_minutes": 7, "reporting_cadence": "wave-end", "worktree_binding_required": true,
            "expect_reply_within_secs": 120, "terminal": true, "no_report_expected": true,
        });
        let e = SendEnvelope::directives_from_args(&args);
        assert_eq!(e.correlation_id.as_deref(), Some("t-c"));
        assert_eq!(e.reviewed_head.as_deref(), Some("sha"));
        assert_eq!(e.sequencing.as_deref(), Some("parallel"));
        assert_eq!(e.eta_minutes, Some(7));
        assert_eq!(e.reporting_cadence.as_deref(), Some("wave-end"));
        assert_eq!(e.worktree_binding_required, Some(true));
        assert_eq!(e.expect_reply_within_secs, Some(120));
        assert_eq!(e.terminal, Some(true));
        assert_eq!(e.no_report_expected, Some(true));
        // routing + task-extras are caller-set, not read here.
        assert!(e.from.is_empty());
        assert!(e.task_id.is_none());
    }
}
