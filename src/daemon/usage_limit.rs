//! #1176: UsageLimit reaction pipeline.
//!
//! - State transition audit log (all transitions → state-transitions.jsonl)
//! - UsageLimit propagation (same-backend → QuotaExceeded)
//! - Telegram notify on UsageLimit events

use crate::state::AgentState;
use std::path::Path;

/// Log a state transition to `state-transitions.jsonl`.
pub fn log_state_transition(
    home: &Path,
    agent: &str,
    from: AgentState,
    to: AgentState,
    pty_snippet: &str,
) {
    let snippet: String = pty_snippet.chars().take(200).collect();
    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "agent": agent,
        "from": from.display_name(),
        "to": to.display_name(),
        "pty_snippet": snippet,
    });
    let path = home.join("state-transitions.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", entry);
    }
}

/// Propagate UsageLimit: set QuotaExceeded on all same-backend agents.
/// Returns the list of affected agent names.
pub fn propagate_usage_limit(
    home: &Path,
    source_agent: &str,
    source_backend: &crate::backend::Backend,
    registry: &crate::agent::AgentRegistry,
) -> Vec<String> {
    let mut affected = Vec::new();
    let reg = crate::agent::lock_registry(registry);
    for handle in reg.values() {
        if handle.name.as_str() == source_agent {
            continue;
        }
        let their_backend = crate::backend::Backend::from_command(&handle.backend_command);
        if their_backend.as_ref() == Some(source_backend) {
            let mut core = handle.core.lock();
            core.health
                .set_blocked_reason(crate::health::BlockedReason::QuotaExceeded);
            affected.push(handle.name.to_string());
        }
    }
    drop(reg);

    // Log propagation event
    crate::event_log::log(
        home,
        "usage_limit_propagated",
        source_agent,
        &format!(
            "backend={:?} affected=[{}]",
            source_backend,
            affected.join(", ")
        ),
    );
    affected
}

/// Notify operator via telegram about UsageLimit event.
/// Uses the reply channel if active, falls back to event log.
pub fn notify_operator_usage_limit(
    home: &Path,
    agent: &str,
    backend: &crate::backend::Backend,
    pty_snippet: &str,
    affected: &[String],
) {
    let snippet: String = pty_snippet.chars().take(200).collect();
    let affected_str = if affected.is_empty() {
        "propagation disabled".to_string()
    } else {
        affected.join(", ")
    };
    let text = format!(
        "[usage_limit] agent={agent} backend={backend:?} affected=[{affected_str}] snippet={snippet}"
    );
    crate::event_log::log(home, "usage_limit_detected", agent, &text);
    tracing::error!(
        agent,
        backend = ?backend,
        affected = ?affected,
        "UsageLimit detected — operator notification"
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn log_state_transition_creates_file() {
        let dir = std::env::temp_dir().join("agend-test-state-transitions");
        std::fs::create_dir_all(&dir).ok();
        std::fs::remove_file(dir.join("state-transitions.jsonl")).ok();

        log_state_transition(
            &dir,
            "dev",
            AgentState::Ready,
            AgentState::UsageLimit,
            "You've hit your limit",
        );

        let content = std::fs::read_to_string(dir.join("state-transitions.jsonl")).unwrap();
        assert!(content.contains("\"agent\":\"dev\""));
        assert!(content.contains("\"to\":\"usage_limit\""));
        assert!(content.contains("hit your limit"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
