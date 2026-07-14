//! S1 release authority: permanent per-agent mutation lock plus disk-fresh
//! tri-state binding reads.

use std::path::{Path, PathBuf};

/// Exact identity of one on-disk binding generation.
///
/// `digest` covers the complete binding bytes, so every field participates in
/// CAS. `issued_at` and an optional future `generation` are also retained
/// explicitly to make the identity contract auditable rather than implicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingFingerprint {
    pub(crate) digest: String,
    pub(crate) issued_at: Option<String>,
    pub(crate) generation: Option<u64>,
}

/// Destructive readers must distinguish a genuinely missing binding from bytes
/// this daemon cannot safely understand.
#[derive(Clone, Debug)]
pub(crate) enum GuardedBinding {
    Known {
        value: serde_json::Value,
        fingerprint: BindingFingerprint,
    },
    Absent,
    Opaque(String),
}

/// RAII proof that the permanent per-agent mutation flock is held.
pub(crate) struct AgentMutationGuard {
    _flock: crate::store::FileFlockGuard,
}

/// Permanent lock path. It deliberately does not live under runtime/<agent>,
/// because that directory is removed during lifecycle cleanup.
pub(crate) fn agent_mutation_lock_path(home: &Path, agent: &str) -> PathBuf {
    let key = crate::daemon::utils::sha256_hex(agent.as_bytes());
    home.join(".agent-mutation-locks")
        .join(format!("{key}.lock"))
}

pub(crate) fn acquire_agent_mutation_lock(
    home: &Path,
    agent: &str,
) -> Result<AgentMutationGuard, String> {
    crate::store::acquire_file_lock(&agent_mutation_lock_path(home, agent))
        .map(|_flock| AgentMutationGuard { _flock })
        .map_err(|e| format!("acquire permanent mutation lock for '{agent}': {e}"))
}

pub(crate) fn binding_file_lock_path(home: &Path, agent: &str) -> PathBuf {
    crate::paths::runtime_dir(home)
        .join(agent)
        .join(".binding.json.lock")
}

pub(crate) fn acquire_binding_file_lock(
    home: &Path,
    agent: &str,
) -> Result<crate::store::FileFlockGuard, String> {
    let path = binding_file_lock_path(home, agent);
    crate::store::acquire_file_lock(&path)
        .map_err(|e| format!("acquire_file_lock {}: {e}", path.display()))
}

/// Read binding.json directly from disk. The caller must hold both the permanent
/// agent mutation lock and the legacy binding-file lock.
pub(crate) fn guarded_binding_disk_fresh(home: &Path, agent: &str) -> GuardedBinding {
    let path = crate::paths::binding_path(home, agent);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return GuardedBinding::Absent,
        Err(e) => return GuardedBinding::Opaque(format!("could not read {}: {e}", path.display())),
    };
    let value: serde_json::Value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) => return GuardedBinding::Opaque("binding JSON is not an object".to_string()),
        Err(e) => return GuardedBinding::Opaque(format!("binding JSON is invalid: {e}")),
    };
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version > super::BINDING_SCHEMA_VERSION {
        return GuardedBinding::Opaque(format!(
            "binding schema version {version} is newer than supported {}",
            super::BINDING_SCHEMA_VERSION
        ));
    }
    // `issued_at` predates the guarded reader but legacy raw bindings without it
    // still exist. Their complete bytes remain exactly identifiable by `digest`;
    // only a PRESENT-but-malformed value is ambiguous and therefore Opaque.
    let issued_at = match value.get("issued_at") {
        None => None,
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => {
                return GuardedBinding::Opaque(
                    "binding issued_at is present but invalid".to_string(),
                )
            }
        },
    };
    let generation = match value.get("generation") {
        None => None,
        Some(v) => match v.as_u64() {
            Some(generation) => Some(generation),
            None => {
                return GuardedBinding::Opaque(
                    "binding generation is present but is not a u64".to_string(),
                )
            }
        },
    };
    GuardedBinding::Known {
        value,
        fingerprint: BindingFingerprint {
            digest: crate::daemon::utils::sha256_hex(&body),
            issued_at,
            generation,
        },
    }
}

/// Acquire both binding mutation locks and return a disk-fresh tri-state
/// snapshot. The returned fingerprint remains usable after the guards drop.
pub(crate) fn snapshot_guarded_binding(home: &Path, agent: &str) -> Result<GuardedBinding, String> {
    let _agent = acquire_agent_mutation_lock(home, agent)?;
    let _binding = acquire_binding_file_lock(home, agent)?;
    Ok(guarded_binding_disk_fresh(home, agent))
}
