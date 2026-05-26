//! Messaging handler: SEND.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_send(params: &Value, ctx: &HandlerCtx) -> Value {
    // Empty `from` would surface downstream as `[from:] {text}` with no
    // originator — reject at the boundary so misuse is loud rather than
    // silent. The MCP layer already guards this via the `Sender` newtype;
    // this covers direct API callers that bypass the typed path.
    let from = match params["from"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            return json!({
                "ok": false,
                "error": "send requires non-empty 'from' (sender identity)"
            });
        }
    };
    let (target, text) = (
        params["target"].as_str().unwrap_or(""),
        params["text"].as_str().unwrap_or(""),
    );
    if let Err(e) = agent::validate_name(target) {
        return json!({"ok": false, "error": e});
    }
    // Sprint 46 P2: self-route check by ID — prevents bypass via rename.
    // Resolve both sender and target; if both resolve to the same InstanceId, reject.
    let from_resolved = crate::agent::resolve_instance(ctx.home, from).ok();
    let target_resolved = crate::agent::resolve_instance(ctx.home, target).ok();
    if let (Some((from_id, _)), Some((target_id, _))) = (&from_resolved, &target_resolved) {
        if from_id == target_id {
            return json!({"ok": false, "error": "cannot send to self"});
        }
    } else if from == target {
        // Fallback: name comparison for instances not in fleet.yaml
        return json!({"ok": false, "error": "cannot send to self"});
    }

    // Validate target exists: check runtime registry OR fleet.yaml via resolve_instance.
    {
        let reg = agent::lock_registry(ctx.registry);
        let in_registry = reg.contains_key(target);
        drop(reg);
        if !in_registry && target_resolved.is_none() {
            // #785: when target is missing from BOTH registry and fleet.yaml
            // instances, it may still be registered as a team member (team
            // metadata persists in fleet.yaml `teams:` block independently
            // of `instances:`). Surface the team-desync state so operators
            // know which remediation paths apply — neutral wording, no
            // causal claim about HOW the desync arose (multiple sub-cases
            // possible: team-add without create_instance, manual yaml edit,
            // etc.).
            let msg = match crate::teams::find_team_for(ctx.home, target) {
                Some(team) => format!(
                    "target '{target}' is registered as a member of team '{team_name}' \
                     but no running instance exists. Either respawn via \
                     `create_instance(name={target}, ...)` or clean stale \
                     membership via `team(action=update, name={team_name}, remove={target})`.",
                    team_name = team.name
                ),
                None => {
                    format!("target instance '{target}' not found (not in registry or fleet.yaml)")
                }
            };
            return json!({"ok": false, "error": msg});
        }
    }

    // Sprint 37 team isolation gate — 3 rules, zero escape hatch.
    // Rule 1 (self-send) already rejected above.
    // Rule 2: general bus
    let is_general_bus = from == "general" || target == "general";
    // Rule 3: same-team via Option<Team> equality
    let from_team = crate::teams::find_team_for(ctx.home, from);
    let target_team = crate::teams::find_team_for(ctx.home, target);
    let same_team = match (&from_team, &target_team) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if !is_general_bus && !same_team {
        crate::event_log::log(
            ctx.home,
            "send_cross_team_blocked",
            from,
            &format!(
                "target={target}, sender_team={:?}, target_team={:?}",
                from_team.as_ref().map(|t| &t.name),
                target_team.as_ref().map(|t| &t.name),
            ),
        );
        return json!({
            "ok": false,
            "error": format!(
                "cross-team send blocked: '{from}' (team={:?}) → '{target}' (team={:?}). \
                 Route via general, or use create_instance(team=...) to grow your team.",
                from_team.as_ref().map(|t| &t.name),
                target_team.as_ref().map(|t| &t.name),
            )
        });
    }
    if is_general_bus && !same_team {
        crate::event_log::log(
            ctx.home,
            "send_cross_team_allowed_general",
            from,
            &format!("target={target}"),
        );
    }

    // #1176: Dispatch gate — reject kind=task to QuotaExceeded agents.
    if params["kind"].as_str() == Some("task") {
        let blocked = {
            let reg = agent::lock_registry(ctx.registry);
            reg.get(target).map(|h| {
                let core = h.core.lock();
                matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::QuotaExceeded)
                )
            })
        };
        if blocked == Some(true) {
            return json!({
                "ok": false,
                "error": "target backend quota exceeded",
                "code": "quota_exceeded"
            });
        }
    }

    // #1149: Auto-create task when kind=task + no task_id provided.
    let auto_task_id = if params["kind"].as_str() == Some("task")
        && params["task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .is_none()
    {
        let title = text
            .lines()
            .next()
            .unwrap_or(text)
            .chars()
            .take(80)
            .collect::<String>();
        use std::sync::atomic::{AtomicU64, Ordering};
        static AUTO_TASK_SEQ: AtomicU64 = AtomicU64::new(0);
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
        let seq = AUTO_TASK_SEQ.fetch_add(1, Ordering::Relaxed);
        let id = format!("t-{ts}-{seq}");
        let event = crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId(id.clone()),
            title,
            description: String::new(),
            priority: params["priority"].as_str().unwrap_or("normal").to_string(),
            owner: Some(crate::task_events::InstanceName(target.to_string())),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: params["branch"].as_str().map(String::from),
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        match crate::task_events::append(
            ctx.home,
            &crate::task_events::InstanceName(from.to_string()),
            event,
        ) {
            Ok(_) => Some(id),
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": format!("auto-create task failed: {e}"),
                    "code": "task_create_failed"
                });
            }
        }
    } else {
        None
    };

    let msg = {
        let mut thread_id = params["thread_id"].as_str().map(String::from);
        let parent_id = params["parent_id"].as_str().map(String::from);

        // Auto-inherit: if parent_id given but thread_id not, inherit from parent
        if thread_id.is_none() {
            if let Some(ref pid) = parent_id {
                if let Some(parent_msg) = crate::inbox::find_message(ctx.home, pid) {
                    thread_id = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
                    // parent becomes thread root
                }
            }
        }

        crate::inbox::InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            thread_id,
            parent_id,
            task_id: params["task_id"]
                .as_str()
                .map(String::from)
                .or_else(|| auto_task_id.clone()),
            force_meta: params
                .get("force_meta")
                .and_then(|v| serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()),
            correlation_id: params["correlation_id"].as_str().map(String::from),
            reviewed_head: params["reviewed_head"].as_str().map(String::from),
            // Sprint 46 P2: use already-resolved sender ID from resolve_instance.
            from: format!("from:{from}"),
            from_id: from_resolved.as_ref().map(|(id, _)| id.full()),
            text: text.to_string(),
            kind: params
                .get("kind")
                .and_then(|v| v.as_str())
                .map(String::from),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            delivery_mode: None,
            attachments: vec![],
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
            // Sprint 54 layer-5: surfaced when caller (handle_broadcast)
            // is fanning out — None for unicast SEND. Header formatter
            // emits `broadcast=N team=NAME` from this.
            broadcast_context: params.get("broadcast_context").and_then(|v| {
                serde_json::from_value::<crate::inbox::BroadcastContext>(v.clone()).ok()
            }),
            sequencing: params["sequencing"].as_str().map(String::from),
            eta_minutes: params["eta_minutes"].as_u64().map(|v| v as u32),
            reporting_cadence: params["reporting_cadence"].as_str().map(String::from),
            worktree_binding_required: params["worktree_binding_required"].as_bool(),
            pr_number: None,
            terminal: params["terminal"].as_bool(),
        }
    };

    // Issue #664 L4b: worktree marker check for high-risk ops.
    // When kind=task + worktree_binding_required=true, verify target is in
    // a daemon-managed worktree before dispatching.
    if params["kind"].as_str() == Some("task")
        && params["worktree_binding_required"].as_bool() == Some(true)
    {
        let mode =
            std::env::var("AGEND_WORKTREE_ENFORCEMENT").unwrap_or_else(|_| "warn".to_string());
        if mode != "off" && !crate::binding::is_agent_in_managed_worktree(ctx.home, target) {
            if mode == "enforce" {
                return json!({
                    "ok": false,
                    "error": "agent not bound in daemon-managed worktree",
                    "hint": "call bind_self first",
                    "code": "worktree_not_managed"
                });
            }
            // warn mode: log but allow
            tracing::warn!(
                target,
                "worktree marker check: agent not in managed worktree (warn mode, allowing)"
            );
        }
    }

    // #1065 unified routing: decide whether the recipient needs a PTY wake
    // hint, then route through either `enqueue` (inbox-only) or
    // `enqueue_with_idle_hint` (durable enqueue + short `[AGEND-MSG-PENDING]`
    // hint emit). Pre-#1065 the inject site here was `compose_aware_send`
    // which wrote the full `[AGEND-MSG]` header (or `[from:lead] body`
    // inline) — content-size pressure extended codex's typed-inject write
    // window past the 50ms pre-submit delay, race-condition on the `\r`.
    // Daemon-emitted auto-wake (`[ci-ready-for-action]`) already used
    // `enqueue_with_idle_hint` and was empirically reliable (4/4 fire +
    // execute); routing kind=task through the same path closes the
    // divergence. See /tmp/dialectic-1065-primary-dev.md §2 + §4.1.
    let reg = agent::lock_registry(ctx.registry);
    let target_in_registry = reg.contains_key(target);
    let is_codex = reg
        .get(target)
        .map(|h| h.backend_command == "codex")
        .unwrap_or(false);
    drop(reg);
    let kind = params["kind"].as_str().unwrap_or("");
    let is_cross_team = {
        let sender_team = crate::teams::find_team_for(ctx.home, from);
        let target_team = crate::teams::find_team_for(ctx.home, target);
        match (sender_team, target_team) {
            (Some(s), Some(t)) => s.name != t.name,
            _ => true, // no team = treat as cross-team (safe default)
        }
    };
    // Issue #656: orchestrators must always receive PTY inject so they
    // can react to status updates from team members.
    let is_orchestrator = crate::teams::find_team_for(ctx.home, target)
        .and_then(|t| t.orchestrator)
        .is_some_and(|orch| orch == target);
    // #982 B-narrow: override codex ack-absorption when the inbound
    // message replies to a blocking dispatch the recipient already
    // drained. Without this override, lead → codex query/task replies
    // strand silently while the codex one-shot waits for a wake.
    let is_reply_to_drained_blocker = matches!(kind, "update" | "report")
        && params["correlation_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .is_some_and(|corr| {
                crate::inbox::has_drained_blocker_for_correlation(ctx.home, target, corr)
            });
    // Issue #603: Codex is one-shot — skip PTY inject for messages that
    // don't require a reply (update/report), avoiding wasted turns.
    // Issue #612: Cross-team messages are NEVER silently absorbed.
    let skip_inject = is_codex
        && matches!(kind, "update" | "report")
        && !is_cross_team
        && !is_orchestrator
        && !is_reply_to_drained_blocker;

    let delivery_mode = if !target_in_registry {
        // Absent target — fleet-defined but no live registry entry; enqueue
        // to inbox JSONL only. dev-2 nit T6: PTY wake hint would be a
        // no-op anyway (no handle to inject into).
        let _ = crate::inbox::enqueue(ctx.home, target, msg.clone());
        "inbox_only"
    } else if skip_inject {
        // #982 ack absorption — inbox JSONL gets the entry, no PTY hint
        // so codex one-shots avoid a wasted turn.
        let _ = crate::inbox::enqueue(ctx.home, target, msg.clone());
        crate::event_log::log(
            ctx.home,
            "ack_absorbed",
            target,
            &format!("from={from} kind={kind}"),
        );
        "inbox_only"
    } else {
        // #1065 unified path — enqueue_with_idle_hint persists to inbox
        // AND emits the short `[AGEND-MSG-PENDING]` PTY hint via
        // `compose_aware_inject`. Same wake signal as the daemon-emit
        // auto-wake at `daemon::ci_watch::poller::ci_check_repo`.
        let _ = crate::inbox::enqueue_with_idle_hint(ctx.home, target, msg.clone());
        "pty"
    };

    // B1 boundary: provenance injection pushed from MCP comms to API SEND.
    // When the caller passes provenance metadata (delegate-task path), inject
    // it into the active channel so operators see task routing in Telegram/Discord.
    if let Some(prov) = params.get("provenance").and_then(|v| v.as_object()) {
        let prov_from = prov
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or(from)
            .to_string();
        let prov_task = prov
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(ch) = crate::channel::active_channel() {
            if let Err(e) = ch.send_from_agent(
                target,
                crate::channel::AgentOutboundOp::InjectProvenance {
                    from: prov_from,
                    task: prov_task,
                },
            ) {
                tracing::warn!(
                    %e, target, from,
                    "provenance injection failed — routing may be broken"
                );
            }
        } else {
            tracing::warn!(
                target,
                from,
                "provenance injection failed — no active channel"
            );
        }
    }

    // B2 boundary: worktree-checkout side-effect pushed from MCP comms to
    // API SEND. When delegate-task carries a branch param, checkout the
    // branch in the target's working directory (Sprint 31 task #52).
    let mut branch_checked_out: Option<&str> = None;
    if let Some(branch) = params["branch"].as_str().filter(|b| !b.is_empty()) {
        let fleet_path = crate::fleet::fleet_yaml_path(ctx.home);
        if let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) {
            if let Some(resolved) = config.resolve_instance(target) {
                if let Some(ref wd) = resolved.working_directory {
                    if crate::worktree::is_git_repo(wd) {
                        match crate::worktree::checkout_branch(wd, branch) {
                            Ok(()) => branch_checked_out = Some(branch),
                            Err(e) => {
                                tracing::warn!(
                                    target_name = target, branch, error = %e,
                                    "task.branch checkout failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // #870: enqueue an auto-release intent when this message is a
    // reviewer VERIFIED verdict. The predicate is intentionally tight
    // (kind=report + text starts "VERIFIED" + reviewed_head present +
    // correlation_id present) so REJECTED / UNVERIFIED / non-verdict
    // reports never trigger auto-release. The hook fires post-enqueue
    // so verdict delivery completes regardless of queue write failure.
    if crate::daemon::auto_release::is_verdict_message(&msg) {
        // correlation_id presence already validated by is_verdict_message.
        if let Some(task_id) = msg.correlation_id.as_ref() {
            let intent = crate::daemon::auto_release::AutoReleaseIntent {
                task_id: task_id.clone(),
                reviewer: from.to_string(),
                verdict_msg_id: msg.id.clone(),
                reviewed_head: msg.reviewed_head.clone(),
                enqueued_at: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(e) = crate::daemon::auto_release::enqueue_intent(ctx.home, &intent) {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "#870 auto_release: enqueue failed (verdict delivery unaffected)"
                );
            }

            // #972: ALSO record VERIFIED into pr_state aggregator. Best-
            // effort; pr_state looks up task → branch and updates any
            // matching pr_state file. If no file exists yet (CI hasn't
            // fired on this branch), the verdict is dropped silently —
            // auto_release still handles worktree release. v2 may
            // persist orphan verdicts in a sidecar.
            crate::daemon::pr_state::record_verdict(
                ctx.home,
                task_id,
                from,
                msg.reviewed_head.as_deref(),
                crate::daemon::pr_state::VerdictKind::Verified,
            );
        }
    }

    // #972: REJECTED / UNVERIFIED verdicts also feed pr_state (no
    // auto_release side effect — those keep the worktree bound for
    // dev iteration). Tight predicates mirror is_verdict_message.
    if msg.kind.as_deref() == Some("report") && msg.correlation_id.is_some() {
        let text = msg.text.trim_start();
        let task_id = msg.correlation_id.as_deref().unwrap_or("");
        if text.starts_with("REJECTED") {
            crate::daemon::pr_state::record_verdict(
                ctx.home,
                task_id,
                from,
                msg.reviewed_head.as_deref(),
                crate::daemon::pr_state::VerdictKind::Rejected { reason: None },
            );
        } else if text.starts_with("UNVERIFIED") {
            crate::daemon::pr_state::record_verdict(
                ctx.home,
                task_id,
                from,
                msg.reviewed_head.as_deref(),
                crate::daemon::pr_state::VerdictKind::Unverified,
            );
        }
    }

    // PR1 watchdog hook — post-enqueue, mirror auto_release ordering
    // (#870): dispatch tracking is defence-in-depth and must never
    // block the dispatch primitive. Two-way wiring:
    //   - Outbound `task` / `query` with a threshold (explicit OR
    //     L2-defaulted for fixup) records a pending sidecar.
    //   - Inbound `report` carrying a correlation_id resolves the
    //     matching sidecar by correlation_id (NOT sender — multi-
    //     pending-per-target requires correlation-keyed resolution).
    let kind_str = msg.kind.as_deref().unwrap_or("");
    if matches!(kind_str, "task" | "query") {
        // Correlation source: `correlation_id` if the caller set one,
        // else `task_id` (the kind=task convention — the task-board id
        // is what the reporter will set as `correlation_id` on the
        // matching kind=report).
        let outbound_corr = msg.correlation_id.as_deref().or(msg.task_id.as_deref());
        let explicit_threshold = params["expect_reply_within_secs"].as_i64();
        if let Some(threshold) =
            crate::daemon::dispatch_idle::fixup_nudge::resolve_threshold_for_dispatch(
                ctx.home,
                from,
                explicit_threshold,
            )
        {
            let outbound_corr = outbound_corr.map(String::from);
            let _ = crate::daemon::dispatch_idle::record_dispatch(
                ctx.home,
                from,
                target,
                outbound_corr.as_deref(),
                kind_str,
                threshold,
            );
        }
    } else if kind_str == "report" {
        if let Some(corr) = msg.correlation_id.as_deref() {
            let _ = crate::daemon::dispatch_idle::mark_resolved(ctx.home, corr);
            // #1228: auto-close task when assignee sends terminal report
            if corr.starts_with("t-") {
                let _ = crate::tasks::auto_close::auto_close_on_report(
                    ctx.home,
                    kind_str,
                    corr,
                    from,
                    &msg.text,
                    msg.terminal.unwrap_or(false),
                );
            }
        }
    } else if matches!(kind_str, "update" | "query") {
        // #1047: non-report messages from dispatchee signal liveness —
        // reset the threshold timer without closing the sidecar.
        if let Some(corr) = msg.correlation_id.as_deref() {
            let _ = crate::daemon::dispatch_idle::refresh_issued_at(ctx.home, corr);
        }
    }

    let mut resp = json!({"ok": true, "delivery_mode": delivery_mode});
    if let Some(branch) = branch_checked_out {
        resp["branch_checked_out"] = json!(branch);
    }
    if let Some(ref tid) = auto_task_id {
        resp["task_id"] = json!(tid);
    }
    resp
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        // Leak registries for 'static — acceptable in tests.
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
        }
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-msg-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_send_to_nonexistent_target_returns_error() {
        let home = tmp_home("nonexist");
        // No fleet.yaml → target not in registry or fleet
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "sender", "target": "ghost", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], false);
        assert!(
            result["error"].as_str().unwrap_or("").contains("not found"),
            "must return not-found error for nonexistent target: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_send_to_fleet_defined_instance_succeeds() {
        let home = tmp_home("fleet-defined");
        // Define instance in fleet.yaml but don't start it
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  offline-agent:\n    backend: claude\n",
        )
        .ok();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "sender", "target": "offline-agent", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "fleet.yaml-defined instance must be accepted: {result}"
        );
        // Not in registry → inbox_only (not pty)
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "inactive target must get inbox_only delivery: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_send_to_active_registry_target_returns_pty() {
        let home = tmp_home("active-pty");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  active-agent:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        // Spawn a real agent so it's in the registry
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "active-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        // Override backend_command to "codex" for ACK absorption check
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({"from": "sender", "target": "active-agent", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "active agent must get pty delivery: {result}"
        );
        // Cleanup
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("active-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_send_to_self_rejected() {
        let home = tmp_home("self-send");
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "agent1", "target": "agent1", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], false);
        assert!(result["error"].as_str().unwrap_or("").contains("self"));
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 37: team isolation gate tests ---

    /// Set up fleet.yaml with given instances and teams. Sprint 54
    /// fleet-yaml unification: teams now live in the `teams:` block of
    /// fleet.yaml directly (was: separate teams.json runtime store).
    fn setup_team_env(home: &std::path::Path, fleet_instances: &[&str], teams: &[(&str, &[&str])]) {
        let mut yaml = String::from("instances:\n");
        for n in fleet_instances {
            yaml.push_str(&format!("  {n}:\n    backend: claude\n"));
        }
        if !teams.is_empty() {
            yaml.push_str("teams:\n");
            for (name, members) in teams {
                yaml.push_str(&format!("  {name}:\n    members:\n"));
                for m in members.iter() {
                    yaml.push_str(&format!("      - {m}\n"));
                }
                yaml.push_str("    created_at: \"2026-01-01T00:00:00Z\"\n");
            }
        }
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();
    }

    fn audit_log_contains(home: &std::path::Path, kind: &str) -> bool {
        let path = home.join("event-log.jsonl");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .any(|l| l.contains(kind))
    }

    #[test]
    fn send_same_team_allowed() {
        let home = tmp_home("same-team");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice", "bob"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], true, "same-team send must succeed: {result}");
        assert!(!audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_cross_team_blocked() {
        let home = tmp_home("cross-team");
        setup_team_env(
            &home,
            &["alice", "bob"],
            &[("dev2", &["alice"]), ("dev", &["bob"])],
        );
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], false,
            "cross-team send must be blocked: {result}"
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap_or("")
                .contains("cross-team"),
            "error must mention cross-team: {result}"
        );
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_to_general_allowed_from_any_team() {
        let home = tmp_home("to-general");
        setup_team_env(&home, &["alice", "general"], &[("dev2", &["alice"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "general", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], true, "send to general must succeed: {result}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_from_general_to_any_team_allowed() {
        let home = tmp_home("from-general");
        setup_team_env(&home, &["general", "bob"], &[("dev", &["bob"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "general", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "send from general must succeed: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_self_already_blocked() {
        let home = tmp_home("self-block-team");
        setup_team_env(&home, &["alice"], &[("dev2", &["alice"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "alice", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], false);
        assert!(
            result["error"].as_str().unwrap_or("").contains("self"),
            "self-send must be caught by existing guard, not team gate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_no_team_to_no_team_allowed() {
        let home = tmp_home("no-team");
        setup_team_env(&home, &["alice", "bob"], &[]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "both teamless must be allowed: {result}"
        );
        assert!(!audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_team_to_no_team_blocked() {
        let home = tmp_home("team-to-none");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], false,
            "team→teamless must be blocked: {result}"
        );
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_no_team_to_team_blocked() {
        let home = tmp_home("none-to-team");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["bob"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "alice", "target": "bob", "text": "hi"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], false,
            "teamless→team must be blocked: {result}"
        );
        assert!(audit_log_contains(&home, "send_cross_team_blocked"));
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 40 T-5: provenance injection invariant at API boundary ---

    #[test]
    fn provenance_injection_no_active_channel_does_not_panic() {
        // DESIGN §4 Q4 invariant re-pinned at API SEND boundary (moved from
        // MCP comms layer in T-5). When provenance params are present but no
        // active channel exists, handle_send must not panic and must return
        // a successful delivery result (provenance is best-effort).
        let home = tmp_home("prov-no-ch");
        setup_team_env(&home, &["sender", "target"], &[]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "target",
                "text": "task text",
                "kind": "task",
                "provenance": {"from": "sender", "task": "do the thing"}
            }),
            &ctx,
        );
        // Send succeeds (inbox delivery); provenance silently skipped (no channel).
        assert_eq!(
            result["ok"], true,
            "send with provenance must succeed even without active channel: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// DESIGN §4 Q4 warn-observability invariant: provenance injection
    /// failure MUST produce a tracing::warn record, not a silent drop.
    /// Re-pinned at API SEND boundary after T-5 moved provenance from
    /// MCP comms layer.
    #[test]
    #[tracing_test::traced_test]
    fn provenance_injection_no_active_channel_logs_warn() {
        let home = tmp_home("prov-warn");
        setup_team_env(&home, &["sender", "target"], &[]);
        let ctx = test_ctx(&home);
        let _result = handle_send(
            &json!({
                "from": "sender",
                "target": "target",
                "text": "task text",
                "provenance": {"from": "sender", "task": "do the thing"}
            }),
            &ctx,
        );
        // No active channel → provenance injection fails → warn emitted.
        // The warn text at messaging.rs:185 is "provenance injection failed".
        assert!(
            logs_contain("provenance injection failed"),
            "DESIGN §4 Q4: provenance failure warn must be emitted at API boundary"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn provenance_params_passed_through_send() {
        // Verify that provenance field in SEND params is accepted and does
        // not cause errors. The actual channel injection is best-effort;
        // this test pins that the API layer processes the field without panic.
        let home = tmp_home("prov-pass");
        setup_team_env(&home, &["alice", "bob"], &[("dev2", &["alice", "bob"])]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "alice",
                "target": "bob",
                "text": "delegated task",
                "provenance": {"from": "alice", "task": "build feature X"}
            }),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "send with provenance params must succeed: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 40 T-6: worktree-checkout boundary invariant ---

    #[test]
    fn send_with_branch_param_does_not_panic() {
        // B2 boundary invariant (safety): branch param in SEND is accepted
        // without panic even when target has no working directory or is not
        // a git repo. Checkout is best-effort.
        let home = tmp_home("branch-safe");
        setup_team_env(&home, &["sender", "target"], &[]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "target",
                "text": "task with branch",
                "branch": "feat/test-branch"
            }),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "send with branch param must succeed (checkout best-effort): {result}"
        );
        // branch_checked_out absent when target has no working dir
        assert!(
            result.get("branch_checked_out").is_none(),
            "no checkout expected without working dir: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[tracing_test::traced_test]
    fn send_with_branch_non_git_dir_logs_no_panic() {
        // B2 boundary invariant (order-of-operations): branch checkout
        // happens AFTER delivery, not before. Even if checkout would fail,
        // the send itself succeeds.
        let home = tmp_home("branch-nongit");
        // Create fleet.yaml with working_directory pointing to a non-git dir
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sender:\n    backend: claude\n  target:\n    backend: claude\n    working_directory: {}\n",
                home.join("workspace/target").display()
            ),
        )
        .ok();
        std::fs::create_dir_all(home.join("workspace/target")).ok();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "target",
                "text": "task",
                "branch": "feat/x"
            }),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "send must succeed even when checkout skipped (non-git): {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[tracing_test::traced_test]
    fn send_with_branch_checkout_failure_logs_warn() {
        // B2 boundary invariant (observability): when checkout fails,
        // tracing::warn must fire. Parallel to DESIGN §4 Q4 pattern.
        let home = tmp_home("branch-fail");
        let wd = home.join("workspace/target");
        std::fs::create_dir_all(&wd).ok();
        // Init a git repo so is_git_repo returns true
        let _ = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&wd)
            .output();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  sender:\n    backend: claude\n  target:\n    backend: claude\n    working_directory: {}\n",
                wd.display()
            ),
        )
        .ok();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "target",
                "text": "task",
                "branch": "invalid..branch"
            }),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "send must succeed even when checkout fails: {result}"
        );
        // Observability pin: warn must fire on checkout failure
        assert!(
            logs_contain("task.branch checkout failed"),
            "B2 observability invariant: warn must fire on checkout failure"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Issue #643: cross-team ACK absorption tests ─────────────────

    #[test]
    fn same_team_codex_update_absorbed() {
        let home = tmp_home("codex-absorbed");
        setup_team_env(
            &home,
            &["codex-agent", "sender"],
            &[("dev", &["codex-agent", "sender"])],
        );
        // Override codex-agent backend to codex in fleet.yaml
        let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap();
        let yaml = yaml.replace(
            "  codex-agent:\n    backend: claude",
            "  codex-agent:\n    backend: codex",
        );
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        // Override backend_command to "codex" for ACK absorption check
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "same-team Codex update must be absorbed: {result}"
        );
        // Audit log must record absorption
        assert!(
            audit_log_contains(&home, "ack_absorbed"),
            "ack_absorbed event must be logged"
        );
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("codex-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cross_team_message_not_absorbed() {
        let home = tmp_home("cross-team-no-absorb");
        setup_team_env(
            &home,
            &["codex-agent", "general"],
            &[("team-a", &["general"]), ("team-b", &["codex-agent"])],
        );
        let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap();
        let yaml = yaml.replace(
            "  codex-agent:\n    backend: claude",
            "  codex-agent:\n    backend: codex",
        );
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        // Override backend_command to "codex" for ACK absorption check
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        // general can send cross-team; codex update should still inject (not absorbed)
        let result = handle_send(
            &json!({"from": "general", "target": "codex-agent", "text": "cross-team update", "kind": "update"}),
            &ctx,
        );
        assert_eq!(
            result["ok"], true,
            "cross-team via general must succeed: {result}"
        );
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "cross-team message must NOT be absorbed: {result}"
        );
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("codex-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn same_team_codex_update_orchestrator_not_skipped() {
        let home = tmp_home("orch-not-skip");
        // codex-agent is the orchestrator of team-a
        let yaml = "instances:\n  sender:\n    backend: claude\n  codex-agent:\n    backend: codex\n\
                    teams:\n  team-a:\n    members:\n      - sender\n      - codex-agent\n    orchestrator: codex-agent\n    created_at: \"2026-01-01T00:00:00Z\"\n";
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "orchestrator must NOT be skipped even for same-team codex update: {result}"
        );
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("codex-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn same_team_codex_update_non_orchestrator_skipped() {
        let home = tmp_home("non-orch-skip");
        // codex-agent is NOT the orchestrator (lead is)
        let yaml = "instances:\n  sender:\n    backend: claude\n  codex-agent:\n    backend: codex\n  lead:\n    backend: claude\n\
                    teams:\n  team-a:\n    members:\n      - sender\n      - codex-agent\n      - lead\n    orchestrator: lead\n    created_at: \"2026-01-01T00:00:00Z\"\n";
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({"from": "sender", "target": "codex-agent", "text": "status update", "kind": "update"}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "non-orchestrator codex must be skipped for same-team update: {result}"
        );
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("codex-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cross_team_codex_update_orchestrator_not_skipped() {
        let home = tmp_home("cross-orch-no-skip");
        // codex-agent is orchestrator, sender is "general" (cross-team bus)
        let yaml = "instances:\n  general:\n    backend: claude\n  codex-agent:\n    backend: codex\n\
                    teams:\n  team-a:\n    members:\n      - general\n    created_at: \"2026-01-01T00:00:00Z\"\n\
                    \n  team-b:\n    members:\n      - codex-agent\n    orchestrator: codex-agent\n    created_at: \"2026-01-01T00:00:00Z\"\n";
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-agent") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(300));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({"from": "general", "target": "codex-agent", "text": "cross-team update", "kind": "update"}),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "cross-team message must NOT be absorbed regardless of orchestrator: {result}"
        );
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get("codex-agent") {
            let _ = h.child.lock().kill();
        }
        drop(reg);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_to_team_member_missing_from_registry_returns_team_desync_error() {
        // #785 anchor: target is a team member (per fleet.yaml `teams:`
        // block) but no instance exists (never in registry, never in
        // `instances:` section). Error message must surface the team-
        // desync state with BOTH remediation paths so operators can
        // diagnose without code archaeology.
        //
        // Reviewer C5 fixture pattern: never call create_instance for
        // the target name; team membership set up directly via
        // `teams::create`. No mock plumbing.
        let home = tmp_home("785-desync");
        // Set up a team `dev` with member `ghost-member` — no instance.
        let _ = crate::teams::create(
            &home,
            &json!({
                "name": "dev",
                "members": ["ghost-member"],
                "orchestrator": "ghost-member",
                "source_repo": "/tmp/p785-desync",
            }),
        );

        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({"from": "sender", "target": "ghost-member", "text": "hi"}),
            &ctx,
        );
        assert_eq!(result["ok"], false);
        let err = result["error"].as_str().unwrap_or("");
        // Content invariants pin the operator-actionable contract
        // (prevent silent wording drift in future PRs).
        assert!(
            err.contains("ghost-member"),
            "error must name the target: {err}"
        );
        assert!(err.contains("dev"), "error must name the team: {err}");
        assert!(
            err.contains("create_instance"),
            "error must surface create_instance remediation path: {err}"
        );
        assert!(
            err.contains("team(action=update"),
            "error must surface team(action=update) cleanup path: {err}"
        );
        // Neutral wording — must NOT claim a specific causal hypothesis.
        assert!(
            !err.contains("likely daemon refresh"),
            "error must use neutral wording (no causal claim): {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── PR1 watchdog hook integration tests (C2 GREEN) ──
    //
    // These exercise the handle_send → dispatch_idle hook wiring.
    // The hook is post-enqueue (auto_release ordering precedent) so
    // any failure here doesn't surface to the dispatch primitive.

    fn write_fixup_fleet(home: &std::path::Path, members: &[&str]) {
        let list = members
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let orchestrator = members.first().copied().unwrap_or("fixup-lead");
        let yaml = format!(
            "schema_version: 1\n\
             instances:\n\
             {instances}\
             teams:\n  fixup:\n    members: [{list}]\n    orchestrator: {orchestrator}\n",
            instances = members
                .iter()
                .map(|m| format!("  {m}:\n    backend: claude\n"))
                .collect::<String>(),
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    #[test]
    fn hook_kind_report_resolves_pending_by_correlation_id() {
        let home = tmp_home("hook-report-resolves");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
        // Seed a pending sidecar (correlation_id = "t-hook").
        let id = crate::daemon::dispatch_idle::record_dispatch(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-hook"),
            "task",
            600,
        )
        .expect("seed sidecar");
        let ctx = test_ctx(&home);
        // Reviewer sends report with the matching correlation_id.
        let result = handle_send(
            &json!({
                "from": "fixup-reviewer",
                "target": "fixup-lead",
                "text": "VERIFIED",
                "kind": "report",
                "correlation_id": "t-hook",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "report send must succeed: {result}");
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        let entry = pending.iter().find(|p| p.dispatch_id == id).unwrap();
        assert_eq!(
            entry.status, "resolved",
            "kind=report with matching correlation_id must resolve the sidecar"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn hook_kind_update_does_not_resolve_pending() {
        // Load-bearing contract: BUSY / status updates must NOT
        // suppress the watchdog. Spike challenge #1.
        let home = tmp_home("hook-update-no-resolve");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
        let id = crate::daemon::dispatch_idle::record_dispatch(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-update"),
            "task",
            600,
        )
        .expect("seed sidecar");
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "fixup-reviewer",
                "target": "fixup-lead",
                "text": "BUSY working on the diff",
                "kind": "update",
                "correlation_id": "t-update",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        let entry = pending.iter().find(|p| p.dispatch_id == id).unwrap();
        assert_eq!(
            entry.status, "pending",
            "kind=update must NOT flip status (watchdog stays armed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn hook_fixup_team_dispatch_records_pending_via_default_threshold() {
        // L2 fixup default-threshold injection: sender in fixup team,
        // kind=task, no explicit expect_reply_within_secs → sidecar
        // recorded with the 600s default.
        let home = tmp_home("hook-fixup-default");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-reviewer"]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "fixup-lead",
                "target": "fixup-reviewer",
                "text": "[task] do the thing",
                "kind": "task",
                "task_id": "t-fixup-default",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "dispatch must succeed: {result}");
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        let entry = pending
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-fixup-default"))
            .expect("fixup-team dispatch must seed a sidecar via L2 default");
        assert_eq!(entry.dispatcher, "fixup-lead");
        assert_eq!(entry.target, "fixup-reviewer");
        assert_eq!(
            entry.threshold_secs, 600,
            "L2 must inject the 600s fixup default"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn hook_non_fixup_dispatch_no_recording_without_explicit_threshold() {
        // Cross-team-safe default-disabled invariant: non-fixup
        // dispatcher with no explicit threshold → NO sidecar.
        let home = tmp_home("hook-non-fixup-no-record");
        // Distinct team that ISN'T fixup.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "schema_version: 1\n\
             instances:\n  research-lead:\n    backend: claude\n  research-dev:\n    backend: claude\n\
             teams:\n  research:\n    members: [research-lead, research-dev]\n    orchestrator: research-lead\n",
        )
        .unwrap();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "research-lead",
                "target": "research-dev",
                "text": "[task] do the thing",
                "kind": "task",
                "task_id": "t-non-fixup",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        assert!(
            pending
                .iter()
                .all(|p| p.correlation_id.as_deref() != Some("t-non-fixup")),
            "non-fixup dispatch without explicit threshold must NOT record"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn hook_explicit_threshold_overrides_team_default() {
        // Explicit expect_reply_within_secs wins for any team
        // (including non-fixup). Other teams opt in this way.
        let home = tmp_home("hook-explicit-threshold");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "schema_version: 1\n\
             instances:\n  research-lead:\n    backend: claude\n  research-dev:\n    backend: claude\n\
             teams:\n  research:\n    members: [research-lead, research-dev]\n    orchestrator: research-lead\n",
        )
        .unwrap();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "research-lead",
                "target": "research-dev",
                "text": "[task] research thing",
                "kind": "task",
                "task_id": "t-explicit",
                "expect_reply_within_secs": 1200_i64,
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        let entry = pending
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-explicit"))
            .expect("explicit-threshold dispatch records sidecar");
        assert_eq!(
            entry.threshold_secs, 1200,
            "explicit threshold must override team default / absent state"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // -----------------------------------------------------------------------
    // #982 B-narrow — codex ack-absorption override for replies to drained
    // blocker dispatches. The empirical bisect found 8 ack_absorbed events
    // today (all target=fixup-reviewer codex / from=fixup-lead kind=update|
    // report), so the override predicate must distinguish:
    //   B1+B2 positive: drained query/task with matching correlation_id
    //                   → override absorption, PTY-surface the reply
    //   B3    negative: undrained query/task with matching correlation_id
    //                   → keep absorption (recipient hasn't read parent)
    //   B4    negative: no matching correlation_id in inbox
    //                   → keep absorption (no blocking context)
    //   B5    negative: correlation_id absent from inbound entirely
    //                   → keep absorption (cannot key the lookup)
    //   B6    invariant: non-codex backend unaffected by override path
    // -----------------------------------------------------------------------

    fn make_codex_ctx(
        home: &std::path::Path,
        codex_agent: &str,
        sender: &str,
    ) -> (
        &'static agent::AgentRegistry,
        HandlerCtx<'static>,
        std::path::PathBuf,
    ) {
        setup_team_env(
            home,
            &[codex_agent, sender],
            &[("dev", &[codex_agent, sender])],
        );
        // Flip the codex_agent backend in fleet.yaml.
        let yaml = std::fs::read_to_string(crate::fleet::fleet_yaml_path(home)).unwrap();
        let yaml = yaml.replace(
            &format!("  {codex_agent}:\n    backend: claude"),
            &format!("  {codex_agent}:\n    backend: codex"),
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: codex_agent,
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut(codex_agent) {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.to_path_buf()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        (registry, ctx, home.to_path_buf())
    }

    fn seed_drained_blocker(home: &std::path::Path, target: &str, kind: &str, corr: &str) {
        let msg = crate::inbox::InboxMessage {
            schema_version: 0,
            id: Some(format!("m-blocker-{corr}")),
            read_at: Some(chrono::Utc::now().to_rfc3339()),
            thread_id: None,
            parent_id: None,
            task_id: None,
            force_meta: None,
            correlation_id: Some(corr.to_string()),
            reviewed_head: None,
            from: "from:fixup-lead".to_string(),
            text: format!("seeded blocker {kind}"),
            kind: Some(kind.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            delivery_mode: None,
            attachments: vec![],
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
            from_id: None,
            broadcast_context: None,
            sequencing: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
            pr_number: None,
            terminal: None,
        };
        crate::inbox::enqueue(home, target, msg).expect("seed blocker");
    }

    fn cleanup_registry(registry: &agent::AgentRegistry, name: &str) {
        let reg = agent::lock_registry(registry);
        if let Some(h) = reg.get(name) {
            let _ = h.child.lock().kill();
        }
    }

    #[test]
    fn b1_codex_report_overrides_absorption_when_query_drained() {
        let home = tmp_home("982-b1");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
        seed_drained_blocker(&home_path, "codex-agent", "query", "corr-b1");

        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "codex-agent",
                "text": "reply to query",
                "kind": "report",
                "correlation_id": "corr-b1",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "B-narrow: report to codex must PTY-surface when matching drained query: {result}"
        );
        cleanup_registry(registry, "codex-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b2_codex_update_overrides_absorption_when_task_drained() {
        let home = tmp_home("982-b2");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
        seed_drained_blocker(&home_path, "codex-agent", "task", "corr-b2");

        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "codex-agent",
                "text": "phase-transition update",
                "kind": "update",
                "correlation_id": "corr-b2",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "B-narrow: update to codex must PTY-surface when matching drained task: {result}"
        );
        cleanup_registry(registry, "codex-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b3_codex_report_keeps_absorption_when_blocker_undrained() {
        let home = tmp_home("982-b3");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
        // Seed an UNDRAINED query.
        let mut msg = crate::inbox::InboxMessage {
            schema_version: 0,
            id: Some("m-undrained".to_string()),
            read_at: None, // ← key: not drained
            thread_id: None,
            parent_id: None,
            task_id: None,
            force_meta: None,
            correlation_id: Some("corr-b3".to_string()),
            reviewed_head: None,
            from: "from:fixup-lead".to_string(),
            text: "undrained query".to_string(),
            kind: Some("query".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            delivery_mode: None,
            attachments: vec![],
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
            from_id: None,
            broadcast_context: None,
            sequencing: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
            pr_number: None,
            terminal: None,
        };
        msg.read_at = None;
        crate::inbox::enqueue(&home_path, "codex-agent", msg).expect("seed");

        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "codex-agent",
                "text": "premature reply",
                "kind": "report",
                "correlation_id": "corr-b3",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "B-narrow: undrained blocker leaves codex absorption intact: {result}"
        );
        cleanup_registry(registry, "codex-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b4_codex_report_keeps_absorption_when_no_correlation_match() {
        let home = tmp_home("982-b4");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
        // Seed a drained query with a DIFFERENT correlation id.
        seed_drained_blocker(&home_path, "codex-agent", "query", "corr-OTHER");

        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "codex-agent",
                "text": "stray report",
                "kind": "report",
                "correlation_id": "corr-b4",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "B-narrow: no correlation match keeps absorption: {result}"
        );
        cleanup_registry(registry, "codex-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b5_codex_report_keeps_absorption_when_correlation_id_absent() {
        let home = tmp_home("982-b5");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-agent", "sender");
        seed_drained_blocker(&home_path, "codex-agent", "query", "corr-ANY");

        // Inbound omits correlation_id entirely.
        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "codex-agent",
                "text": "manual update",
                "kind": "update",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "B-narrow: no correlation_id on inbound keeps absorption: {result}"
        );
        cleanup_registry(registry, "codex-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b6_non_codex_backend_pty_path_unchanged_by_override() {
        // Sanity invariant: non-codex backends always PTY today (no absorption);
        // the override predicate must not redirect them through inbox_only.
        let home = tmp_home("982-b6");
        // Use the default claude-flavored spawn from setup_team_env.
        setup_team_env(
            &home,
            &["claude-agent", "sender"],
            &[("dev", &["claude-agent", "sender"])],
        );
        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "claude-agent",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(150));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        seed_drained_blocker(&home, "claude-agent", "query", "corr-b6");

        let result = handle_send(
            &json!({
                "from": "sender",
                "target": "claude-agent",
                "text": "reply for claude",
                "kind": "report",
                "correlation_id": "corr-b6",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "non-codex backend always PTY regardless of correlation predicate: {result}"
        );
        cleanup_registry(registry, "claude-agent");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1065 unified routing tests (kind=task → enqueue_with_idle_hint) ──
    //
    // Before #1065: handle_send used `enqueue` + `compose_aware_send(inject_msg)`
    // where inject_msg was the full `[AGEND-MSG] header (use inbox tool)` form
    // (or `[from:lead] body` for short messages). Operator-observed pattern:
    // ~10% reviewer dispatches via kind=task land but the agent never
    // executes — content-size pressure extends codex's typed-inject write
    // window past the 50ms pre-submit delay, race-condition on the `\r`.
    //
    // After #1065: handle_send routes through `enqueue_with_idle_hint`
    // (same path as daemon-emitted [ci-ready-for-action] auto-wake which has
    // empirically reliable 4/4 fire+execute). Both paths emit the SAME short
    // `[AGEND-MSG-PENDING]` hint. Body stays in inbox JSONL (durable).

    /// T1 (#1065 RED): structural pin — handle_send must route the PTY
    /// delivery path through `enqueue_with_idle_hint` (NOT
    /// `compose_aware_send`). Pre-fix code contains `compose_aware_send(`
    /// at the inject site; post-fix code uses `enqueue_with_idle_hint`.
    #[test]
    fn handle_send_routes_through_enqueue_with_idle_hint() {
        let source = include_str!("messaging.rs");
        // Strip the test module so we only inspect the production handler.
        // Tests pin the GREEN-side wiring; the structural-pin assertion
        // applies to handle_send's body, not to test fixture code.
        let prod_end = source
            .find("#[cfg(test)]")
            .expect("messaging.rs must have a #[cfg(test)] tests module");
        let prod_src = &source[..prod_end];
        assert!(
            prod_src.contains("enqueue_with_idle_hint"),
            "#1065 invariant: handle_send must route kind=task through \
             enqueue_with_idle_hint (same path as daemon auto-wake)"
        );
        assert!(
            !prod_src.contains("compose_aware_send("),
            "#1065 invariant: handle_send must NOT use compose_aware_send \
             for the inject site post-#1065 — the unified routing emits \
             [AGEND-MSG-PENDING] hint instead of [AGEND-MSG] header"
        );
    }

    /// T2 (#1065): kind=task body persists in inbox JSONL regardless of
    /// the routing path. Sanity guard: the durable inbox entry must
    /// survive the refactor.
    #[test]
    fn kind_task_body_persisted_in_inbox_jsonl() {
        let home = tmp_home("1065-t2-body");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  reviewer:\n    backend: claude\n  lead:\n    backend: claude\n",
        )
        .ok();
        let ctx = test_ctx(&home);
        let body = "[delegate_task] long task body".repeat(20);
        let result = handle_send(
            &json!({
                "from": "lead",
                "target": "reviewer",
                "text": body,
                "kind": "task",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "send must succeed: {result}");

        // Read whatever JSONL was written under <home>/inbox/. The path is
        // either name-based or id-based depending on whether fleet.yaml has
        // backfilled an InstanceId — collapse both into one read.
        let inbox_dir = home.join("inbox");
        let mut combined = String::new();
        if let Ok(rd) = std::fs::read_dir(&inbox_dir) {
            for e in rd.flatten() {
                if let Ok(c) = std::fs::read_to_string(e.path()) {
                    combined.push_str(&c);
                }
            }
        }
        assert!(
            combined.contains("delegate_task"),
            "kind=task body must persist in inbox JSONL post-#1065: {combined:?}"
        );
        assert!(
            combined.contains("\"kind\":\"task\""),
            "kind=task tag must be preserved in JSONL: {combined:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T3 (#1065 + #982 preservation): codex same-team kind=update
    /// remains ack-absorbed (inbox_only + ack_absorbed event log).
    /// The #982 contract must survive the routing refactor.
    #[test]
    fn kind_update_codex_same_team_remains_ack_absorbed() {
        let home = tmp_home("1065-t3-codex-update");
        let (registry, ctx, home_path) = make_codex_ctx(&home, "codex-rev", "lead");
        let result = handle_send(
            &json!({
                "from": "lead",
                "target": "codex-rev",
                "text": "status update",
                "kind": "update",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "codex same-team kind=update must remain ack-absorbed (#982): {result}"
        );
        assert!(
            audit_log_contains(&home_path, "ack_absorbed"),
            "ack_absorbed event must be logged"
        );
        cleanup_registry(registry, "codex-rev");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T4 (#1065 + #612 preservation): codex kind=report from "general"
    /// bus to a different-team codex target still injects (delivery_mode=pty).
    /// Cross-team unicast is blocked at Rule 3 (line 78+) so the only way
    /// to exercise the cross-team-codex-not-absorbed semantics is via the
    /// general bus (Rule 2). The #612 invariant must survive the routing
    /// refactor — `enqueue_with_idle_hint` must run, NOT ack-absorb.
    #[test]
    fn kind_report_cross_team_codex_via_general_still_injects() {
        let home = tmp_home("1065-t4-general");
        let yaml = "instances:\n  general:\n    backend: claude\n  \
                    codex-rev:\n    backend: codex\nteams:\n  \
                    team-a:\n    members:\n      - general\n    \
                    created_at: \"2026-01-01T00:00:00Z\"\n  \
                    team-b:\n    members:\n      - codex-rev\n    \
                    created_at: \"2026-01-01T00:00:00Z\"\n";
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).ok();

        let registry: &'static agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let spawn_cfg = crate::agent::SpawnConfig {
            name: "codex-rev",
            backend_command: crate::default_shell(),
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        crate::agent::spawn_agent(&spawn_cfg, registry).expect("spawn");
        {
            let mut reg = agent::lock_registry(registry);
            if let Some(h) = reg.get_mut("codex-rev") {
                h.backend_command = "codex".to_string();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));

        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let home_ref: &'static std::path::Path = Box::leak(Box::new(home.clone()));
        let ctx = HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home: home_ref,
        };
        let result = handle_send(
            &json!({
                "from": "general",
                "target": "codex-rev",
                "text": "cross-team report via general",
                "kind": "report",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "general → cross-team send: {result}");
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("pty"),
            "cross-team codex kind=report must still inject (#612): {result}"
        );
        assert!(
            !audit_log_contains(&home, "ack_absorbed"),
            "ack_absorbed must NOT be logged for cross-team report"
        );
        cleanup_registry(registry, "codex-rev");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T5 (#1065): probabilistic race regression — pinned at the unit-test
    /// level requires a real codex backend. Kept as documentation that an
    /// empirical reproduce protocol exists; runs only under `--ignored`.
    /// See /tmp/dialectic-1065-primary-dev.md §6 for the operator-side
    /// 10-trial reproduce plan.
    #[test]
    #[ignore = "requires real codex backend; runs on operator-side empirical protocol"]
    fn submit_race_regression_under_long_inject_documented() {
        // Placeholder: pin protocol via doc-comment + ignored marker. The
        // refactor is structurally GREEN per T1; the race regression is
        // observable only through real backend reproduce.
    }

    /// T6 (#1065 + dev-2 nit): absent target (fleet-defined but not in
    /// registry) → inbox_only with no PTY emit. Preserves the original
    /// fallback at messaging.rs's `else` branch.
    #[test]
    fn absent_target_falls_back_to_inbox_only() {
        let home = tmp_home("1065-t6-absent");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  offline-rev:\n    backend: claude\n  lead:\n    backend: claude\n",
        )
        .ok();
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "lead",
                "target": "offline-rev",
                "text": "[delegate_task] do X",
                "kind": "task",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_only"),
            "absent target must receive inbox_only delivery: {result}"
        );
        // Inbox JSONL still gets the entry — durable path preserved.
        // Read whatever JSONL was written; path may be name- or id-based.
        let inbox_dir = home.join("inbox");
        let mut combined = String::new();
        if let Ok(rd) = std::fs::read_dir(&inbox_dir) {
            for e in rd.flatten() {
                if let Ok(c) = std::fs::read_to_string(e.path()) {
                    combined.push_str(&c);
                }
            }
        }
        assert!(
            combined.contains("\"kind\":\"task\""),
            "inbox JSONL must persist the task entry for absent target: {combined:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1268: kind=query must NOT produce a dispatch_idle sidecar.
    /// (Replaces #1129 test — query sidecars caused false-positive
    /// watchdog nudges on broadcast queries.)
    #[test]
    fn hook_kind_query_does_not_create_dispatch_sidecar() {
        let home = tmp_home("1268-query-no-sidecar");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "fixup-lead",
                "target": "fixup-dev",
                "text": "what is the status?",
                "kind": "query",
                "expect_reply_within_secs": 300,
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "query must succeed: {result}");
        let pending = crate::daemon::dispatch_idle::list_pending(&home);
        assert!(
            pending.iter().all(|p| p.target != "fixup-dev"),
            "kind=query must not create a dispatch sidecar: {pending:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1149: send kind=task without task_id auto-creates a task and
    /// stamps it on the outbound message + response.
    #[test]
    fn auto_create_task_on_send_kind_task_without_task_id() {
        let home = tmp_home("1149-auto-create");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "fixup-lead",
                "target": "fixup-dev",
                "text": "[delegate_task] implement the widget\ndetailed description here",
                "kind": "task",
                "branch": "feat/widget",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true, "send must succeed: {result}");
        // Response must contain auto-generated task_id.
        let task_id = result["task_id"]
            .as_str()
            .expect("response must include task_id");
        assert!(
            task_id.starts_with("t-"),
            "auto-generated task_id must use t- prefix: {task_id}"
        );
        // Task must exist on the board.
        let tasks = crate::tasks::handle(
            &home,
            "fixup-lead",
            &json!({"action": "list", "include_history": true}),
        );
        let task_list = tasks["tasks"].as_array().expect("tasks array");
        let created = task_list
            .iter()
            .find(|t| t["id"].as_str() == Some(task_id))
            .expect("auto-created task must exist on board");
        assert_eq!(
            created["title"].as_str().unwrap(),
            "[delegate_task] implement the widget"
        );
        assert_eq!(created["branch"].as_str(), Some("feat/widget"));
        assert_eq!(created["assignee"].as_str(), Some("fixup-dev"));
        assert_eq!(created["status"].as_str().unwrap(), "open");
        // Inbox message must carry the task_id.
        let inbox = crate::inbox::drain(&home, "fixup-dev");
        let msg = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("task"))
            .expect("task message must be in inbox");
        assert_eq!(
            msg.task_id.as_deref(),
            Some(task_id),
            "outbound message must carry auto-generated task_id"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1149: send kind=task WITH task_id does NOT auto-create (backward compat).
    #[test]
    fn no_auto_create_when_task_id_provided() {
        let home = tmp_home("1149-no-auto");
        write_fixup_fleet(&home, &["fixup-lead", "fixup-dev"]);
        let ctx = test_ctx(&home);
        let result = handle_send(
            &json!({
                "from": "fixup-lead",
                "target": "fixup-dev",
                "text": "do the thing",
                "kind": "task",
                "task_id": "t-existing-123",
            }),
            &ctx,
        );
        assert_eq!(result["ok"], true);
        // Response must NOT contain auto-generated task_id.
        assert!(
            result.get("task_id").is_none(),
            "response must NOT include task_id when caller provided one: {result}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
