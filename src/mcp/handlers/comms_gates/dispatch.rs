//! W2.2: `handle_delegate_task` pre-send gate chain, lifted out of the
//! inline handler body. These are the rejectable, side-effect-free checks
//! that run between target-resolution and the lease/create/send pipeline —
//! the dispatch dedup + busy gate (#1286 / #1496), the §3.5 dual-review
//! flag check, and the #812 dispatch-time test-name validation.
//!
//! ORDERING IS LOAD-BEARING. The checks short-circuit in exactly the order
//! the inline code used — #1286 branch-dedup → generic busy (+ force-reason)
//! → second-reviewer → #812 test-name — so a request that would trip two
//! gates surfaces the same first rejection it did before the extraction.
//! `claimed_tasks` is computed once and shared by both busy gates, and the
//! #812 gate FAILS OPEN (warn + proceed) when no PR tree resolves.

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// t-…-17 reviewer-assignment: the STRICT-typed principal that authored the code
/// under review. `Agent` is a fleet instance in the SAME namespace as the reviewer
/// target (so an `Agent(name) == target` marker is same-namespace self-review and
/// is rejected); `External` is a forge login — a distinct principal type that is
/// NEVER string-compared to the agent target (an agent reviewing external-authored
/// code is legitimate). Parsed once, fail-closed, via [`parse_review_author`].
// t-…-17 C8: `Serialize`/`Deserialize` are additive here so the durable
// `ActiveAssignment` record (assignment_authority.rs) can persist the authenticated
// author verbatim. The enum stays the single source of truth (no shadow copy); the
// derive is a no-behavior-change addition to slice-1's typed principal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ReviewAuthor {
    Agent(String),
    External(String),
}

/// t-…-17: STRICT parse of `args["review_author"]`. It must be an object with
/// EXACTLY ONE of the keys `{"agent", "external"}` whose value is a NONEMPTY
/// string. Absent / JSON-null ⇒ `Ok(None)` (review_author is optional). Both keys,
/// neither key, any extra key, a non-object, or an empty/non-string value ⇒
/// `Err(reject Value)` — fail-closed, never a lenient default. Only invoked on the
/// `review_assignment == true` path so a stray `review_author` on an ordinary
/// dispatch is byte-identically ignored.
pub(crate) fn parse_review_author(v: &Value) -> Result<Option<ReviewAuthor>, Value> {
    if v.is_null() {
        return Ok(None);
    }
    let invalid = || {
        json!({
            "error": "review_author must be an object with EXACTLY ONE of \
                      {\"agent\": <name>} or {\"external\": <login>} and a non-empty string value",
            "code": "review_author_invalid",
        })
    };
    let obj = v.as_object().ok_or_else(invalid)?;
    // len==1 rejects both-keys (2), any extra key (>=2), and neither (0) in one shot.
    if obj.len() != 1 {
        return Err(invalid());
    }
    let build =
        |key: &str, ctor: fn(String) -> ReviewAuthor| -> Result<Option<ReviewAuthor>, Value> {
            let s = obj
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(invalid)?;
            Ok(Some(ctor(s.to_string())))
        };
    if obj.contains_key("agent") {
        build("agent", ReviewAuthor::Agent)
    } else if obj.contains_key("external") {
        build("external", ReviewAuthor::External)
    } else {
        // single key, but neither agent nor external.
        Err(invalid())
    }
}

/// Scalars derived during the pre-check pass that the downstream pipeline
/// (message build, `force_meta`, lease auto-bind) reuses — returned here so
/// they are derived exactly once.
#[derive(Debug)]
pub(crate) struct DispatchPreChecks {
    pub force: bool,
    pub force_reason: Option<String>,
    /// §3.5: consumed at the lease site as `review_class = "dual"`.
    pub second_reviewer: bool,
    /// #2249: forwarded into the auto-created task's `create_args` when this
    /// dispatch auto-creates a task (empty `task_id`) — 0 (default) = gate
    /// off. Ignored when `task_id` already references an existing task (its
    /// plan_ack_required, if any, was fixed at that task's own create time).
    pub plan_ack_required: u64,
    /// t-…-17: true when this dispatch carries the durable reviewer-assignment
    /// marker (`args["review_assignment"] == true`). Gates the fail-closed marker
    /// validation in `handle_delegate_task`; false ⇒ byte-identical legacy path.
    pub review_assignment: bool,
    /// t-…-17: the strict-typed code author (see [`ReviewAuthor`]). Parsed (and
    /// fail-closed-validated) ONLY when `review_assignment` is true; `None`
    /// otherwise or when the marker omits it.
    pub review_author: Option<ReviewAuthor>,
    /// t-…-17 (B18): the PR generation identity. Additive/raw here; the
    /// mandatory-nonzero enforcement is the marker gate's step (a).
    pub pr_number: Option<u64>,
}

/// Run the delegate-task pre-send gates in their exact short-circuit order.
///
/// `Ok(DispatchPreChecks)` ⟹ every gate passed; the returned scalars feed the
/// inline pipeline. `Err(Value)` is the first rejection's response Value,
/// returned verbatim by the caller (a structured `{"busy":…}` / `{"error":…,
/// "code":…}` / `{"error":…}` shape — not flattened to a String, because
/// callers and tests pin those shapes).
pub(crate) fn run_dispatch_pre_checks(
    home: &Path,
    sender: &Sender,
    args: &Value,
    target: &str,
    task: &str,
) -> Result<DispatchPreChecks, Value> {
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let force_reason = args.get("force_reason").and_then(|v| v.as_str());
    let claimed_tasks: Vec<_> = crate::tasks::list_all(home)
        .into_iter()
        .filter(|t| {
            t.assignee.as_deref() == Some(target)
                && (t.status == crate::task_events::TaskStatus::Claimed
                    || t.status == crate::task_events::TaskStatus::InProgress)
        })
        .collect();
    // #1496 Option 1: a send(kind=task) whose `task_id` is already one of the
    // target's active tasks is ENRICHING that in-flight dispatch (finally
    // delivering its context), not opening a competing one — let it through the
    // busy-gate. Pairs with dropping task(create)'s premature auto-notify so the
    // create→send dispatch sequence no longer needs force=true (#1496 spike).
    let enriching_active = args["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .is_some_and(|tid| claimed_tasks.iter().any(|t| t.id.as_str() == tid));
    // #1286: branch-specific dispatch dedup — reject if target already has
    // an active task on the same branch (more specific than generic busy).
    if !force && !enriching_active {
        if let Some(branch) = args["branch"].as_str() {
            if let Some(dup) = claimed_tasks
                .iter()
                .find(|t| t.branch.as_deref() == Some(branch))
            {
                return Err(json!({
                    "error": format!(
                        "dispatch rejected: {} already has active task {} on branch {}",
                        target, dup.id, branch
                    )
                }));
            }
        }
    }
    if !claimed_tasks.is_empty() && !enriching_active {
        if force {
            if force_reason.is_none() || force_reason == Some("") {
                return Err(json!({"error": "force=true requires a non-empty 'force_reason'"}));
            }
        } else {
            let current = &claimed_tasks[0];
            let age_secs = chrono::DateTime::parse_from_rfc3339(&current.updated_at)
                .ok()
                .map(|dt| {
                    chrono::Utc::now()
                        .signed_duration_since(dt.with_timezone(&chrono::Utc))
                        .num_seconds()
                })
                .unwrap_or(0);
            return Err(json!({
                "busy": true,
                "current_task": {"id": current.id, "title": current.title, "age_seconds": age_secs},
                "options": ["force=true (with force_reason)"],
                "suggestion": format!("target busy on task {} ({}s old). Use force=true with force_reason to override.", current.id, age_secs)
            }));
        }
    }

    // Second reviewer flag validation (§3.5 dual review)
    let second_reviewer = args["second_reviewer"].as_bool().unwrap_or(false);
    if second_reviewer {
        let sr_reason = args["second_reviewer_reason"].as_str().unwrap_or("");
        if sr_reason.is_empty() {
            return Err(
                json!({"error": "second_reviewer=true requires non-empty second_reviewer_reason"}),
            );
        }
    }

    // #2249 pre-work alignment gate flag validation — same shape as
    // second_reviewer_reason above.
    let plan_ack_required = args["plan_ack_required"].as_u64().unwrap_or(0);
    if plan_ack_required > 0 {
        let par_reason = args["plan_ack_reason"].as_str().unwrap_or("");
        if par_reason.is_empty() {
            return Err(
                json!({"error": "plan_ack_required > 0 requires non-empty plan_ack_reason"}),
            );
        }
    }

    // #812: dispatch-time test-name validation. Extends §4.3
    // hallucinated-fn check to the dispatch path so `cargo test`
    // invocations naming a test that doesn't exist in the PR tree
    // are rejected BEFORE the reviewer wastes a cycle on
    // `no test matched`. Tree resolution priority: sender's bound
    // worktree → recipient's daemon-managed path. None → fail-open
    // with warn-log (don't block when only operator has the tree).
    let branch = args["branch"].as_str();
    if let Some(tree) =
        crate::claim_verifier::resolve_dispatch_tree(home, sender.as_str(), Some(target), branch)
    {
        if let Err(detail) = crate::claim_verifier::validate_dispatch_test_names(task, &tree) {
            return Err(json!({
                "error": detail,
                "code": "test_name_not_found",
            }));
        }
    } else {
        tracing::warn!(
            sender = %sender.as_str(),
            target = %target,
            branch = ?branch,
            "#812 dispatch test-name check skipped — no resolvable PR tree (sender unbound + no daemon worktree)"
        );
    }

    // t-…-17 reviewer-assignment marker parse (additive; enforcement lives in the
    // marker gate). `review_author` is strict-parsed ONLY on the marker path so a
    // stray key on an ordinary dispatch stays byte-identical.
    let review_assignment = args["review_assignment"].as_bool().unwrap_or(false);
    let review_author = if review_assignment {
        parse_review_author(&args["review_author"])?
    } else {
        None
    };
    let pr_number = args["pr_number"].as_u64();

    Ok(DispatchPreChecks {
        force,
        force_reason: force_reason.map(String::from),
        second_reviewer,
        plan_ack_required,
        review_assignment,
        review_author,
        pr_number,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    // run_dispatch_pre_checks takes `home` explicitly (no global AGEND_HOME
    // read), so each test is isolated by its own temp dir — no env mutex.
    fn gate_home(label: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-w22-gate-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        home
    }

    // Seed a Claimed task for `target` via the real task-board producer
    // (create → claim sets assignee=target, status=Claimed) — representative
    // of what list_all observes in production. Returns the task id.
    fn seed_claimed_task(home: &std::path::Path, target: &str, branch: Option<&str>) -> String {
        let mut create = json!({"action": "create", "title": "in-flight work"});
        if let Some(b) = branch {
            create["branch"] = json!(b);
        }
        crate::tasks::handle(home, target, &create);
        let tasks = crate::tasks::handle(home, target, &json!({"action": "list"}));
        let tid = tasks["tasks"][0]["id"]
            .as_str()
            .expect("seeded task id")
            .to_string();
        crate::tasks::handle(home, target, &json!({"action": "claim", "id": &tid}));
        tid
    }

    fn run(home: &std::path::Path, args: &serde_json::Value) -> Result<DispatchPreChecks, Value> {
        let sender = Sender::new("sender").expect("valid sender");
        run_dispatch_pre_checks(home, &sender, args, "target", "do the thing")
    }

    #[test]
    fn idle_target_passes_all_gates() {
        let home = gate_home("idle");
        let out = run(&home, &json!({"instance": "target"})).expect("idle target must pass");
        assert!(!out.force);
        assert!(out.force_reason.is_none());
        assert!(!out.second_reviewer);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn busy_target_rejects_with_structured_payload() {
        let home = gate_home("busy");
        seed_claimed_task(&home, "target", None);
        // Different task_id ⟹ not enriching; no force ⟹ busy reject.
        let err = run(&home, &json!({"instance": "target", "task_id": "t-other"}))
            .expect_err("busy target must reject");
        assert_eq!(err["busy"], true, "structured busy payload: {err}");
        assert!(err["current_task"]["id"].is_string(), "{err}");
        assert!(err["options"].is_array(), "{err}");
        assert!(err["suggestion"].is_string(), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_without_reason_rejected() {
        let home = gate_home("force-noreason");
        seed_claimed_task(&home, "target", None);
        let err = run(
            &home,
            &json!({"instance": "target", "task_id": "t-other", "force": true}),
        )
        .expect_err("force without reason must reject");
        assert!(
            err["error"].as_str().unwrap_or("").contains("force_reason"),
            "{err}"
        );
        assert!(
            err.get("busy").is_none(),
            "force path is not a busy reply: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_with_reason_bypasses_busy() {
        let home = gate_home("force-ok");
        seed_claimed_task(&home, "target", None);
        let out = run(
            &home,
            &json!({"instance": "target", "task_id": "t-other", "force": true, "force_reason": "urgent"}),
        )
        .expect("force+reason must bypass busy");
        assert!(out.force);
        assert_eq!(out.force_reason.as_deref(), Some("urgent"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn enriching_same_task_id_bypasses_busy() {
        let home = gate_home("enrich");
        let tid = seed_claimed_task(&home, "target", None);
        // task_id == the target's active task ⟹ enriching, not competing.
        let out = run(&home, &json!({"instance": "target", "task_id": tid}))
            .expect("enriching dispatch must bypass busy-gate (#1496)");
        assert!(!out.second_reviewer);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn branch_dedup_rejects_same_branch() {
        let home = gate_home("branch-dedup");
        seed_claimed_task(&home, "target", Some("feat/x"));
        // Same branch, different task_id, no force ⟹ #1286 dedup reject (fires
        // before the generic busy gate and before #812).
        let err = run(
            &home,
            &json!({"instance": "target", "task_id": "t-other", "branch": "feat/x"}),
        )
        .expect_err("same-branch dispatch must reject");
        assert!(
            err["error"]
                .as_str()
                .unwrap_or("")
                .contains("already has active task"),
            "{err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn second_reviewer_without_reason_rejected() {
        let home = gate_home("sr-noreason");
        let err = run(
            &home,
            &json!({"instance": "target", "second_reviewer": true}),
        )
        .expect_err("second_reviewer without reason must reject");
        assert!(
            err["error"]
                .as_str()
                .unwrap_or("")
                .contains("second_reviewer_reason"),
            "{err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn second_reviewer_with_reason_passes() {
        let home = gate_home("sr-ok");
        let out = run(
            &home,
            &json!({"instance": "target", "second_reviewer": true, "second_reviewer_reason": "risky"}),
        )
        .expect("second_reviewer + reason must pass");
        assert!(out.second_reviewer);
        std::fs::remove_dir_all(&home).ok();
    }

    // Combination short-circuit (INV-1 / INV-8): a request that trips BOTH the
    // busy gate AND the second-reviewer gate must surface the busy reply first
    // — busy (209-231) precedes second-reviewer (234-240). A reordered pipeline
    // would return the second_reviewer error instead.
    #[test]
    fn busy_short_circuits_before_second_reviewer() {
        let home = gate_home("busy-vs-sr");
        seed_claimed_task(&home, "target", None);
        let err = run(
            &home,
            &json!({"instance": "target", "task_id": "t-other", "second_reviewer": true}),
        )
        .expect_err("must reject");
        assert_eq!(
            err["busy"], true,
            "busy must win over second_reviewer: {err}"
        );
        assert!(
            err.get("error").is_none(),
            "must be the busy payload, not the second_reviewer error: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── t-…-17 (T8): review_author STRICT typed parse ──

    /// A well-formed single-key `review_author` parses to the matching typed
    /// variant; a JSON-null / absent value is `None` (review_author is optional).
    #[test]
    fn review_author_wellformed_parses_typed() {
        assert_eq!(
            parse_review_author(&json!({"agent": "dev-1"})).unwrap(),
            Some(ReviewAuthor::Agent("dev-1".to_string()))
        );
        assert_eq!(
            parse_review_author(&json!({"external": "octocat"})).unwrap(),
            Some(ReviewAuthor::External("octocat".to_string()))
        );
        // typed distinction: Agent(x) != External(x).
        assert_ne!(
            parse_review_author(&json!({"agent": "x"})).unwrap(),
            parse_review_author(&json!({"external": "x"})).unwrap()
        );
        // absent / null ⇒ None.
        assert_eq!(parse_review_author(&Value::Null).unwrap(), None);
        assert_eq!(parse_review_author(&json!(null)).unwrap(), None);
    }

    /// Fail-closed: both keys, neither key, an extra key, a non-object, or an
    /// empty/non-string value ⇒ a structured reject (never a lenient default).
    #[test]
    fn review_author_strict_rejects_malformed() {
        for bad in [
            json!({"agent": "a", "external": "b"}), // both
            json!({}),                              // neither
            json!({"agent": "a", "extra": 1}),      // extra key
            json!({"reviewer": "a"}),               // single wrong key
            json!({"agent": ""}),                   // empty value
            json!({"agent": 7}),                    // non-string value
            json!("dev-1"),                         // non-object
            json!(["dev-1"]),                       // non-object
        ] {
            let err = parse_review_author(&bad).expect_err(&format!("must reject {bad}"));
            assert_eq!(err["code"], "review_author_invalid", "for {bad}: {err}");
        }
    }

    /// The strict parse only runs on the marker path: an ordinary dispatch carrying
    /// a malformed `review_author` is byte-identically ignored (no new rejection),
    /// while the SAME malformed marker dispatch fails closed.
    #[test]
    fn review_author_strict_only_on_marker_path() {
        let home = gate_home("marker-parse-scope");
        // review_assignment absent ⇒ malformed review_author ignored, gates pass.
        let ok = run(
            &home,
            &json!({"instance": "target", "review_author": {"agent": "a", "external": "b"}}),
        )
        .expect("non-marker dispatch must ignore review_author");
        assert!(!ok.review_assignment);
        assert!(ok.review_author.is_none());
        // review_assignment=true ⇒ the same malformed review_author fails closed.
        let err = run(
            &home,
            &json!({"instance": "target", "review_assignment": true, "review_author": {"agent": "a", "external": "b"}}),
        )
        .expect_err("marker dispatch must strict-parse review_author");
        assert_eq!(err["code"], "review_author_invalid", "{err}");
        // review_assignment=true + well-formed marker fields parse through.
        let out = run(
            &home,
            &json!({"instance": "target", "review_assignment": true, "review_author": {"external": "octocat"}, "pr_number": 42}),
        )
        .expect("well-formed marker must pass pre-checks");
        assert!(out.review_assignment);
        assert_eq!(
            out.review_author,
            Some(ReviewAuthor::External("octocat".to_string()))
        );
        assert_eq!(out.pr_number, Some(42));
        std::fs::remove_dir_all(&home).ok();
    }
}
