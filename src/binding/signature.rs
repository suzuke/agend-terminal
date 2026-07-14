//! Binding signature diagnostics kept outside the core binding manager.

use std::path::Path;

/// Diagnostic: verify the on-disk HMAC sidecar against binding.json.
pub(crate) fn signature_valid(home: &Path, agent: &str) -> bool {
    let dir = crate::paths::runtime_dir(home).join(agent);
    let body = match std::fs::read(dir.join("binding.json")) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let tag = match std::fs::read_to_string(dir.join("binding.json.sig")) {
        Ok(t) => t,
        Err(_) => return false,
    };
    agentic_git_core::integrity_core::verify(home, &body, &tag).is_ok()
}
