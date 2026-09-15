//! Binding signature diagnostics kept outside the core binding manager.

use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignatureStatus {
    Valid,
    Missing,
    Invalid,
}

/// Diagnostic: verify the on-disk HMAC sidecar against binding.json.
pub(crate) fn signature_status(home: &Path, agent: &str) -> SignatureStatus {
    let dir = crate::paths::runtime_dir(home).join(agent);
    let body = match std::fs::read(dir.join("binding.json")) {
        Ok(b) => b,
        Err(_) => return SignatureStatus::Invalid,
    };
    let tag = match std::fs::read_to_string(dir.join("binding.json.sig")) {
        Ok(t) => t,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SignatureStatus::Missing;
        }
        Err(_) => return SignatureStatus::Invalid,
    };
    if agentic_git_core::integrity_core::verify(home, &body, &tag).is_ok() {
        SignatureStatus::Valid
    } else {
        SignatureStatus::Invalid
    }
}

pub(crate) fn signature_valid(home: &Path, agent: &str) -> bool {
    signature_status(home, agent) == SignatureStatus::Valid
}
