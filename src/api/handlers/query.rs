//! Query handlers: LIST and STATUS.
//!
//! These are read-only handlers that return agent/snapshot information
//! without mutating state.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_list(_params: &Value, ctx: &HandlerCtx) -> Value {
    // H9: snapshot each managed agent's fields UNDER the tier-1 registry lock,
    // then drop(reg) BEFORE the per-agent dispatch_idle disk I/O. The original
    // called `pending_for_instance` (→ `read_dir` + a `read_to_string` +
    // `serde_json::from_str` per `.json` sidecar) inside the `.map()` while the
    // lock was still held, so a LIST with N agents performed N dir scans +
    // N*M file reads/parses entirely under the lock — blocking every other API
    // handler, the supervisor tick, crash-respawn, hang-detection and the TUI
    // render path on disk contention. Mirrors the external-agent loop below.
    let reg = agent::lock_registry(ctx.registry);
    let snapshot: Vec<(String, Value)> = reg
        .values()
        .map(|handle| {
            let name = handle.name.to_string();
            let (
                agent_state,
                health_state,
                blocked_reason,
                blocked_note,
                context,
                context_provider,
                api_in_flight,
                last_api_activity_at,
                observed_status,
            ) = {
                let c = handle.core.lock();
                (
                    c.state.get_state().display_name().to_string(),
                    c.health.state.display_name().to_string(),
                    // #1933: surface the self-reported blocked reason + its
                    // operator-readable note so an operator can see WHY an agent is
                    // blocked and its free-text annotation (previously internal-only).
                    c.health.current_reason.as_ref().map(|r| r.to_string()),
                    c.health.current_note.clone(),
                    // Context% telemetry: resolved usage + producing source
                    // ("statusline" = agent's own footer, "transcript" = token-cost
                    // estimate for codex/grok/agy). Absent = honestly unknown.
                    c.state.resolved_context(Some(ctx.home)),
                    // #2439: the backend's context-telemetry CAPABILITY. Always present.
                    c.state.context_provider(),
                    // #2413 Phase 1: out-of-path API-activity signal, read under the
                    // SAME lock as agent_state so a consumer can reconcile the two
                    // atomically (false-idle = agent_state=="idle" && api_in_flight).
                    c.api_activity.in_flight,
                    c.api_activity.last_active_epoch_ms,
                    // #2413 Phase B: the reducer's fused status, read under the SAME
                    // lock as agent_state so a consumer can diff them atomically (the
                    // observed-vs-raw diff IS the quantification). None only if the per-tick
                    // reduce didn't run (default-ON; off under AGEND_SHADOW_OBSERVER=0).
                    c.observed_status.clone(),
                )
            };
            let entry = json!({
                "name": name.as_str(),
                "backend": handle.backend_command,
                "submit_key": handle.submit_key,
                "inject_prefix": handle.inject_prefix,
                "agent_state": agent_state,
                "health_state": health_state,
                "blocked_reason": blocked_reason,
                "blocked_note": blocked_note,
                "context_pct": context.map(|(pct, _)| pct),
                "context_source": context.map(|(_, source)| source.source_name()),
                "context_provider": context_provider.source_name(),
                // #2413 Phase 1: live LLM-socket activity (out-of-path lsof probe).
                // api_in_flight=true while pattern-state is "idle" ⇒ false-idle.
                "api_in_flight": api_in_flight,
                "last_api_activity_at": last_api_activity_at,
                // #2413 Phase B: additive reducer status (state/confidence/authority/
                // evidence-trail/since_ms). null unless the Shadow Observer flag is on.
                "observed_status": observed_status,
                "kind": "managed",
            });
            (name, entry)
        })
        .collect();
    drop(reg);

    // Disk I/O now runs WITHOUT the registry lock held.
    let mut agents: Vec<Value> = Vec::with_capacity(snapshot.len());
    for (name, mut entry) in snapshot {
        let (dispatched_waiting_for, pending_response_to) =
            crate::daemon::dispatch_idle::pending_for_instance(ctx.home, &name);
        if let Some(obj) = entry.as_object_mut() {
            obj.insert(
                "dispatched_waiting_for".into(),
                json!(dispatched_waiting_for),
            );
            obj.insert("pending_response_to".into(), json!(pending_response_to));
        }
        agents.push(entry);
    }
    let ext = agent::lock_external(ctx.externals);
    for (name, handle) in ext.iter() {
        let (dispatched_waiting_for, pending_response_to) =
            crate::daemon::dispatch_idle::pending_for_instance(ctx.home, name);
        agents.push(json!({
            "name": name,
            "backend": handle.backend_command,
            "agent_state": "external",
            "health_state": "connected",
            "kind": "external",
            "pid": handle.pid,
            "dispatched_waiting_for": dispatched_waiting_for,
            "pending_response_to": pending_response_to,
        }));
    }
    json!({"ok": true, "result": {"protocol_version": crate::framing::PROTOCOL_VERSION, "agents": agents}})
}

pub(crate) fn handle_status(_params: &Value, ctx: &HandlerCtx) -> Value {
    match crate::snapshot::load(ctx.home) {
        Some(snapshot) => {
            json!({"ok": true, "result": {
                "protocol_version": crate::framing::PROTOCOL_VERSION,
                "timestamp": snapshot.timestamp,
                "agents": snapshot.agents.iter().map(|a| {
                    json!({
                        "name": a.name,
                        "backend": a.backend_command,
                        "args": a.args,
                        "working_dir": a.working_dir,
                        "submit_key": a.submit_key,
                        "health_state": a.health_state,
                        "agent_state": a.agent_state,
                    })
                }).collect::<Vec<_>>()
            }})
        }
        None => json!({"ok": true, "result": {"agents": [], "timestamp": null}}),
    }
}
