//! Compatibility wrapper for callers that do not already own a lifecycle permit.

/// Clear a binding for an agent (task completed/released).
#[allow(dead_code)]
pub fn unbind(home: &std::path::Path, agent: &str) {
    let Ok(permit) = crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Delete,
    ) else {
        return;
    };
    super::unbind_with_permit(home, agent, &permit);
}
