//! #hook-state-poc: HOOK_EVENT — lifecycle-hook event ingestion (shadow-mode).
//!
//! Receives one event from a backend hook command (`agend-terminal hook-event
//! --instance <name>`, wired into the per-workspace Claude settings by
//! `mcp_config.rs` under `AGEND_HOOK_STATE_POC=1`). Records it in the
//! [`crate::daemon::hook_shadow`] store and emits the `#hook-shadow`
//! comparison log (hook-derived state vs the live screen-heuristic state) —
//! the agreement data that gates promoting hooks to authoritative.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_hook_event(params: &Value, ctx: &HandlerCtx) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(name) {
        return json!({"ok": false, "error": e});
    }
    let hook_event_name = match params["hook_event_name"].as_str() {
        Some(h) if !h.is_empty() => h,
        _ => return json!({"ok": false, "error": "missing 'hook_event_name'"}),
    };
    let notification_type = params["notification_type"].as_str();
    let tool_name = params["tool_name"].as_str();

    let derived =
        crate::daemon::hook_shadow::record_event(name, hook_event_name, notification_type);

    // #t-3558: a fresh failure hook (StopFailure → ApiError) signals a NEW error
    // episode. If the agent earlier self-cleared a rate-limit block, drop the
    // #2232 `rate_limit_self_cleared` liveness latch so the supervisor's
    // ServerRateLimit retry path re-arms. Without this the latch stays set until
    // the SCREEN exits ServerRateLimit (supervisor.rs:1955), but the stale
    // "Server is temporarily limiting" line keeps the screen pinned to SRL
    // forever (#1769 positional defeat) → the retry track is cleared every tick
    // and never re-injected: a permanent silent wedge (RCA t-…2345-2,
    // live-reproduced on fixup-dev-2). Re-arm is still gated on
    // screen==ServerRateLimit in the supervisor (:1994), so a non-SRL ApiError
    // (e.g. an auth error) only drops the latch and cannot trigger a spurious
    // inject. Only ApiError-derived hooks reset — a clean Stop / PreToolUse /
    // UserPromptSubmit during normal post-self-clear work never does.
    let is_apierror_hook = derived == Some(crate::state::AgentState::ApiError);

    // Shadow comparison: the screen-heuristic state at event receipt. This is
    // the PoC's primary output — production promotion is gated on this agreement
    // data. Doubles as the #t-3558 latch-reset site (same single core lock).
    let (screen_state, backend) = {
        let reg = agent::lock_registry(ctx.registry);
        match crate::fleet::resolve_uuid(ctx.home, name).and_then(|id| {
            reg.get(&id).map(|h| {
                let mut core = h.core.lock();
                if is_apierror_hook && core.health.rate_limit_self_cleared {
                    core.health.rate_limit_self_cleared = false;
                    tracing::info!(
                        agent = %name,
                        "#t-3558 rate-limit self-clear latch reset — fresh ApiError hook (new \
                         error episode); supervisor ServerRateLimit retry re-arms"
                    );
                }
                (core.state.get_state(), h.backend_command.clone())
            })
        }) {
            Some((s, b)) => (Some(s), Some(b)),
            None => (None, None),
        }
    };
    let agree = match (derived, screen_state) {
        (Some(d), Some(s)) => Some(d == s),
        _ => None,
    };
    // #2016: #1523 promoted hooks to authoritative, so the static "shadow-mode —
    // heuristic still drives" line is no longer true for a promoted backend.
    // Reflect the LIVE disposition (fields unchanged — text only).
    let drive = if backend
        .as_deref()
        .is_some_and(crate::daemon::hook_shadow::is_promoted)
    {
        "authoritative for this backend"
    } else {
        "shadow — heuristic drives"
    };
    tracing::info!(
        tag = "#hook-shadow",
        agent = %name,
        hook_event = hook_event_name,
        notification_type = ?notification_type,
        tool_name = ?tool_name,
        hook_state = ?derived,
        screen_state = ?screen_state,
        agree = ?agree,
        "hook event received ({drive})"
    );
    json!({"ok": true, "derived_state": derived.map(|s| format!("{s:?}"))})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_ctx(home: &std::path::Path) -> HandlerCtx<'_> {
        let registry: &'static crate::agent::AgentRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let configs: &'static crate::api::ConfigRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        let externals: &'static crate::agent::ExternalRegistry =
            Box::leak(Box::new(Arc::new(Mutex::new(HashMap::new()))));
        HandlerCtx {
            registry,
            configs,
            externals,
            notifier: None,
            home,
        }
    }

    #[test]
    fn hook_event_records_shadow_and_derives() {
        let home = std::env::temp_dir().join(format!(
            "agend-hookevent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();
        let ctx = test_ctx(&home);
        let resp = handle_hook_event(
            &json!({"name": "hooked", "hook_event_name": "PreToolUse", "tool_name": "Bash"}),
            &ctx,
        );
        assert_eq!(resp["ok"], true, "{resp}");
        let snap = crate::daemon::hook_shadow::snapshot_for("hooked").expect("recorded");
        assert_eq!(snap.last_event, "PreToolUse");
        assert_eq!(snap.derived_state, Some(crate::state::AgentState::Active));

        // Missing event name → honest error, nothing recorded for that call.
        let bad = handle_hook_event(&json!({"name": "hooked"}), &ctx);
        assert_eq!(bad["ok"], false);
        std::fs::remove_dir_all(&home).ok();
    }

    // #t-3558: seed a registered agent (fleet.yaml id ↔ registry handle) whose
    // `rate_limit_self_cleared` latch is set, then drive hook events through the
    // handler. Mirrors the external.rs handler-test seeding pattern.
    // `unix`-gated: `mk_test_handle` (the `true`-backed PTY handle) is
    // `#[cfg(all(test, unix))]`, so the helpers + tests that use it — and the
    // const only they reference — must be unix-only to keep the Windows build
    // warning-clean.
    #[cfg(unix)]
    const T3558_ID: &str = "0d0d0d0d-0000-4000-8000-0000035580a1";

    #[cfg(unix)]
    fn seed_latched_agent(ctx: &HandlerCtx<'_>, name: &str) {
        let id = crate::types::InstanceId::parse(T3558_ID).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(ctx.home),
            format!("instances:\n  {name}:\n    id: {T3558_ID}\n"),
        )
        .unwrap();
        let handle = agent::mk_test_handle(name, id);
        handle.core.lock().health.rate_limit_self_cleared = true;
        agent::lock_registry(ctx.registry).insert(id, handle);
    }

    #[cfg(unix)]
    fn latch_of(ctx: &HandlerCtx<'_>) -> bool {
        let id = crate::types::InstanceId::parse(T3558_ID).unwrap();
        agent::lock_registry(ctx.registry)
            .get(&id)
            .map(|h| h.core.lock().health.rate_limit_self_cleared)
            .expect("agent present")
    }

    #[cfg(unix)]
    fn set_latch(ctx: &HandlerCtx<'_>, val: bool) {
        let id = crate::types::InstanceId::parse(T3558_ID).unwrap();
        agent::lock_registry(ctx.registry)
            .get(&id)
            .expect("agent present")
            .core
            .lock()
            .health
            .rate_limit_self_cleared = val;
    }

    #[cfg(unix)]
    fn t3558_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-t3558-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();
        home
    }

    /// #t-3558 POSITIVE: a fresh `StopFailure` hook (→ ApiError) resets the
    /// `rate_limit_self_cleared` latch so the supervisor SRL retry re-arms.
    /// RED before the fix (the latch was only reset on a screen-state exit).
    #[cfg(unix)]
    #[test]
    fn apierror_hook_resets_rate_limit_self_clear_latch() {
        let home = t3558_home("reset");
        let ctx = test_ctx(&home);
        let name = "wedged";
        seed_latched_agent(&ctx, name);
        assert!(latch_of(&ctx), "precondition: latch set");

        let resp = handle_hook_event(
            &json!({"name": name, "hook_event_name": "StopFailure"}),
            &ctx,
        );
        assert_eq!(resp["ok"], true, "{resp}");
        assert!(
            !latch_of(&ctx),
            "StopFailure (→ApiError) must reset rate_limit_self_cleared so SRL retry re-arms"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #t-3558 NEGATIVE (mandatory — pins "normal completion never re-arms"):
    /// clean lifecycle hooks during normal post-self-clear work must NOT reset
    /// the latch — only a genuine failure hook does.
    #[cfg(unix)]
    #[test]
    fn normal_hooks_do_not_reset_rate_limit_self_clear_latch() {
        let home = t3558_home("noreset");
        let ctx = test_ctx(&home);
        let name = "working";
        seed_latched_agent(&ctx, name);

        for ev in ["Stop", "PreToolUse", "PostToolUse", "UserPromptSubmit"] {
            set_latch(&ctx, true);
            let resp = handle_hook_event(&json!({"name": name, "hook_event_name": ev}), &ctx);
            assert_eq!(resp["ok"], true, "{resp}");
            assert!(
                latch_of(&ctx),
                "normal hook {ev:?} must NOT reset the latch (no spurious re-arm on healthy work)"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
