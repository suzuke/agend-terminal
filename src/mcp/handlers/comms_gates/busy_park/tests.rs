#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod busy_park_tests {
    use super::super::*;
    use crate::identity::Sender;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-busy-park-{}-{tag}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Create a task owned by `assignee` (Open). Returns the task id.
    fn seed_open_task(home: &Path, caller: &str, title: &str, assignee: &str) -> String {
        let created = crate::tasks::handle(
            home,
            caller,
            &json!({
                "action": "create",
                "title": title,
                "assignee": assignee,
            }),
        );
        created["id"].as_str().expect("seeded task id").to_string()
    }

    /// Create + claim: the target owns claimed work, i.e. the busy gate fires.
    fn seed_claimed_task(home: &Path, caller: &str, title: &str, assignee: &str) -> String {
        let tid = seed_open_task(home, caller, title, assignee);
        let claim = crate::tasks::handle(home, assignee, &json!({"action": "claim", "id": &tid}));
        assert_eq!(claim["status"], "claimed", "claim must succeed: {claim}");
        tid
    }

    fn done_task(home: &Path, caller: &str, task_id: &str) {
        let done = crate::tasks::handle(home, caller, &json!({"action": "done", "id": task_id}));
        assert!(done["error"].is_null(), "done must succeed: {done}");
    }

    fn delegate_args(target: &str, task: &str, task_id: Option<&str>) -> Value {
        let mut args = json!({
            "instance": target,
            "task": task,
            "expect_reply_within_secs": 600,
        });
        if let Some(tid) = task_id {
            args["task_id"] = json!(tid);
        }
        args
    }

    fn delegate(home: &Path, sender: &Option<Sender>, args: &Value) -> Value {
        crate::mcp::handlers::comms::handle_delegate_task(home, args, sender, None)
    }

    fn inbox_texts(home: &Path, name: &str) -> Vec<(Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, name)
            .into_iter()
            .map(|m| (m.kind.clone(), m.text.clone(), m.task_id.clone()))
            .collect()
    }

    #[test]
    fn should_park_only_generic_busy_non_marker() {
        let busy = json!({"busy": true, "current_task": {"id": "t-a"}});
        // Busy + ordinary ⇒ park.
        assert!(should_park(&json!({"instance": "dev"}), &busy));
        // Busy + review-assignment marker ⇒ never park (exact-head workspace
        // authority cannot be rebuilt from a tick).
        assert!(!should_park(&json!({"review_assignment": true}), &busy));
        // Non-busy rejections (dedup / validation) ⇒ never park.
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"error": "dispatch rejected: dev already has active task t on branch b"})
        ));
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"error": "force=true requires a non-empty 'force_reason'"})
        ));
        assert!(!should_park(
            &json!({"instance": "dev"}),
            &json!({"ok": true})
        ));
    }

    #[test]
    fn target_is_busy_matches_gate_predicate() {
        let home = tmp_home("idle-check");
        assert!(!target_is_busy(&home, "dev"), "fresh home is idle");
        // Open work does NOT make the target busy (gate only sees
        // claimed/in-progress) — the redrive must fire for it.
        let open = seed_open_task(&home, "lead", "open work", "dev");
        assert!(!target_is_busy(&home, "dev"), "open task is not busy");
        let claim = crate::tasks::handle(&home, "dev", &json!({"action": "claim", "id": &open}));
        assert_eq!(claim["status"], "claimed");
        assert!(target_is_busy(&home, "dev"), "claimed task is busy");
        done_task(&home, "dev", &open);
        assert!(!target_is_busy(&home, "dev"), "done task is idle again");
        // Unreadable task view fails CLOSED (never redrive blind).
        std::fs::write(home.join("boards"), "not a directory").unwrap();
        assert!(
            target_is_busy(&home, "dev"),
            "unreadable view counts as busy"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3666 core: a busy-refused dispatch parks with an additive receipt and
    /// becomes visible to the stuck-dispatch sweep (no more silent loss).
    #[test]
    fn busy_reject_parks_with_additive_receipt() {
        let home = tmp_home("park-receipt");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );

        // The busy shape is preserved byte-for-byte…
        assert_eq!(out["busy"], true, "{out}");
        assert!(out["current_task"]["id"].is_string(), "{out}");
        assert!(out["options"].is_array(), "{out}");
        assert!(out["suggestion"].is_string(), "{out}");
        assert_eq!(
            out["current_task"]["id"].as_str(),
            Some(blocker.as_str()),
            "blocker identity preserved: {out}"
        );
        // …and the park receipt is purely additive.
        assert_eq!(out["parked"], true, "{out}");
        let park_id = out["park_id"].as_str().expect("park_id: {out}");
        assert!(!park_id.is_empty());

        let parks = list_parked(&home);
        assert_eq!(parks.len(), 1, "exactly one park row: {parks:?}");
        assert_eq!(parks[0].park_id, park_id);
        assert_eq!(parks[0].dispatcher, "lead");
        assert_eq!(parks[0].target, "dev");
        assert_eq!(parks[0].task_id.as_deref(), Some(parked_task.as_str()));
        assert_eq!(parks[0].blocker_task_id.as_deref(), Some(blocker.as_str()));
        assert_eq!(parks[0].attempts, 0);
        // A redrive is never forced — the stored snapshot cannot re-arm it.
        assert!(parks[0].args.get("force").is_none(), "{:?}", parks[0].args);
        assert!(parks[0].args.get("force_reason").is_none());

        // Visibility rung: the refused dispatch enters dispatch_tracking, so
        // `sweep_stuck` (and the reclaim reroute) sees it.
        let taken = crate::dispatch_tracking::take_pending_dispatchers_to(&home, "dev");
        assert!(
            taken
                .iter()
                .any(|e| e.task_id.as_deref() == Some(parked_task.as_str())
                    && e.from == "lead"
                    && e.status == "pending"),
            "refused dispatch must be tracked: {taken:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Retrying the same refused dispatch while parked dedups to the existing
    /// intent (no stacked rows, no stacked tracking entries).
    #[test]
    fn duplicate_park_returns_existing_receipt() {
        let home = tmp_home("park-dedup");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());
        let args = delegate_args("dev", "do stranded work", Some(&parked_task));

        let first = delegate(&home, &sender, &args);
        let second = delegate(&home, &sender, &args);
        assert_eq!(first["parked"], true);
        assert_eq!(second["parked"], true);
        assert_eq!(
            first["park_id"], second["park_id"],
            "dedup: {first} vs {second}"
        );
        assert_eq!(second["park_duplicate"], true);
        assert_eq!(list_parked(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3666 regression: refused-dispatch → target idle → automatic delivery
    /// with the idle-hint wake, watchdog armed from delivery, dispatcher told.
    #[test]
    fn refused_dispatch_redrives_on_idle_transition() {
        let home = tmp_home("redrive-e2e");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        // While the target is still busy the scan re-parks (attempt-capped,
        // never delivered, never dropped).
        let stats = scan_and_redrive(&home);
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.reparked, 1);
        assert_eq!(stats.delivered, 0);
        assert_eq!(list_parked(&home).len(), 1);
        assert_eq!(list_parked(&home)[0].attempts, 1);
        assert!(
            inbox_texts(&home, "dev").is_empty(),
            "no delivery while busy"
        );

        // The blocker completes → the target goes idle → the next scan
        // delivers automatically.
        done_task(&home, "dev", &blocker);
        assert!(!target_is_busy(&home, "dev"));
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.delivered, stats.dropped), (1, 1, 0));

        // The target's inbox holds the task row (kind=task, idle-hint wake —
        // not the wake-less inbox_only fallback).
        let rows = inbox_texts(&home, "dev");
        let delivered = rows.iter().find(|(kind, _, tid)| {
            kind.as_deref() == Some("task") && tid.as_deref() == Some(parked_task.as_str())
        });
        assert!(
            delivered.is_some(),
            "target must receive the parked task on idle: {rows:?}"
        );
        let (_, text, _) = delivered.unwrap();
        assert!(text.contains("do stranded work"), "{text}");
        assert!(
            text.contains(&format!("(task id: {parked_task})")),
            "{text}"
        );

        // The park row is gone (exactly-once per intent).
        assert!(list_parked(&home).is_empty());

        // The wait-for-reply watchdog is armed FROM DELIVERY…
        let sidecars = crate::daemon::dispatch_idle::list_pending(&home);
        assert!(
            sidecars.iter().any(
                |d| d.correlation_id.as_deref() == Some(parked_task.as_str())
                    && d.target == "dev"
                    && d.dispatcher == "lead"
            ),
            "dispatch_idle sidecar must arm on delivery: {sidecars:?}"
        );
        // …and the dispatcher gets a passive FYI (plain inbox row, no wake).
        let lead_rows = inbox_texts(&home, "lead");
        assert!(
            lead_rows
                .iter()
                .any(|(_, text, _)| text.contains("[busy-park]")
                    && text.contains("auto-delivered")
                    && text.contains("dev")),
            "dispatcher must be told about the redrive: {lead_rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A parked intent whose board task closes before the redrive drops
    /// silently (terminal cleanup already cleared tracking + sidecars).
    #[test]
    fn stale_parked_task_drops_without_delivery() {
        let home = tmp_home("stale-drop");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        // The parked task itself closes (claimed + done by its owner).
        let claim = crate::tasks::handle(
            &home,
            "dev",
            &json!({"action": "claim", "id": &parked_task}),
        );
        assert_eq!(claim["status"], "claimed");
        done_task(&home, "dev", &parked_task);

        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.dropped), (1, 1));
        assert!(list_parked(&home).is_empty());
        // Drained rows, if any, must not carry the stale task delivery.
        let rows = inbox_texts(&home, "dev");
        assert!(
            rows.iter().all(|(kind, _, tid)| {
                !(kind.as_deref() == Some("task") && tid.as_deref() == Some(parked_task.as_str()))
            }),
            "stale task must never deliver: {rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Still-busy intents fail closed past the cap: dropped + dispatcher paged
    /// (mirrors 71c85cc8's attempt cap), never retried forever.
    #[test]
    fn redrive_attempts_cap_fails_closed_with_notify() {
        let home = tmp_home("attempt-cap");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");

        for expected_attempts in 1..=MAX_PARKED_REDRIVE_ATTEMPTS {
            let stats = scan_and_redrive(&home);
            assert_eq!((stats.reparked, stats.dropped), (1, 0));
            assert_eq!(list_parked(&home)[0].attempts, expected_attempts);
        }
        // One more scan exhausts the cap: dropped, dispatcher notified, target
        // never received the work.
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.dropped, stats.delivered), (1, 0));
        assert!(list_parked(&home).is_empty());
        assert!(inbox_texts(&home, "dev").is_empty(), "never delivered");
        let lead_rows = inbox_texts(&home, "lead");
        assert!(
            lead_rows
                .iter()
                .any(|(_, text, _)| text.contains("dropped after")
                    && text.contains("re-dispatch or force")),
            "cap exhaustion must page the dispatcher: {lead_rows:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// `force=true` still bypasses the gate with zero park involvement.
    #[test]
    fn force_dispatch_never_parks() {
        let home = tmp_home("force-no-park");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "urgent work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &json!({
                "instance": "dev",
                "task": "urgent override",
                "task_id": parked_task,
                "force": true,
                "force_reason": "operator-approved interruption",
            }),
        );
        assert!(out.get("parked").is_none(), "force must not park: {out}");
        assert!(out.get("busy").is_none(), "force must bypass busy: {out}");
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A review-assignment marker refused by the busy gate keeps its pure
    /// reject (never parked — exact-head authority is tick-unrebuildable).
    #[test]
    fn marker_busy_reject_never_parks() {
        let home = tmp_home("marker-no-park");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &json!({
                "instance": "dev",
                "task": "review the PR",
                "task_id": "t-other",
                "review_assignment": true,
            }),
        );
        assert_eq!(out["busy"], true, "{out}");
        assert!(out.get("parked").is_none(), "marker must not park: {out}");
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A task-less dispatch parks the intent and auto-creates the board task
    /// at redrive time (same field shape as the live auto-create).
    #[test]
    fn taskless_park_auto_creates_on_redrive() {
        let home = tmp_home("taskless");
        let blocker = seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "ad-hoc stranded work", None),
        );
        assert_eq!(out["parked"], true, "{out}");
        assert!(list_parked(&home)[0].task_id.is_none());

        done_task(&home, "dev", &blocker);
        let stats = scan_and_redrive(&home);
        assert_eq!((stats.scanned, stats.delivered), (1, 1));

        let rows = inbox_texts(&home, "dev");
        let delivered = rows.iter().find(|(kind, text, _)| {
            kind.as_deref() == Some("task") && text.contains("ad-hoc stranded work")
        });
        assert!(delivered.is_some(), "{rows:?}");
        let (_, _, tid) = delivered.unwrap();
        let tid = tid.clone().expect("auto-created task id on the row");
        // The auto-created board task exists and is owned by the target.
        let routed = crate::tasks::load_routed(&home, &tid).expect("board task exists");
        assert_eq!(routed.task.assignee.as_deref(), Some("dev"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// A delivery through another path retires the park (no double delivery):
    /// driving the send-time `track_dispatch` choke point for the same
    /// (target, task) must invalidate the parked intent.
    #[test]
    fn manual_delivery_invalidates_park() {
        let home = tmp_home("invalidate");
        seed_claimed_task(&home, "lead", "in-flight work", "dev");
        let parked_task = seed_open_task(&home, "lead", "stranded work", "dev");
        let sender = Some(Sender::new("lead").unwrap());

        let out = delegate(
            &home,
            &sender,
            &delegate_args("dev", "do stranded work", Some(&parked_task)),
        );
        assert_eq!(out["parked"], true, "{out}");
        assert_eq!(list_parked(&home).len(), 1);

        // The same task goes out through another path (e.g. a force dispatch
        // that reached `execute_send`): the send-time track choke point fires…
        let req = crate::agent_ops::messaging::SendRequest {
            from: "lead".to_string(),
            target: "dev".to_string(),
            text: "do stranded work".to_string(),
            kind: Some("task".to_string()),
            thread_id: None,
            parent_id: None,
            correlation_id: None,
            reviewed_head: None,
            report_purpose: None,
            code_review: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
            expect_reply_within_secs: None,
            terminal: None,
            no_report_expected: None,
            delivery_nonce: None,
            task_id: Some(parked_task.clone()),
            force_meta: None,
            provenance: None,
            branch: None,
            broadcast_context: None,
            priority: None,
        };
        let msg = crate::inbox::InboxMessage {
            from: "from:lead".to_string(),
            text: "do stranded work".to_string(),
            kind: Some("task".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            task_id: Some(parked_task.clone()),
            correlation_id: Some(parked_task.clone()),
            ..Default::default()
        };
        crate::agent_ops::messaging::track_dispatch(&home, &req, "lead", "dev", &msg);
        // …and the parked intent is retired so the idle scan cannot deliver
        // the same work a second time.
        assert!(list_parked(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
