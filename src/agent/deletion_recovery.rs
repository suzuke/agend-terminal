//! Durable delete/recovery fences for bound worktrees.
//!
//! The in-memory deleting set protects one daemon process.  This sidecar keeps
//! the same name fenced across a daemon restart when teardown had to retain a
//! markerless bound worktree for operator recovery.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Deleting,
    RecoveryRequired,
    Recovered,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Tombstone {
    pub(crate) schema_version: u32,
    pub(crate) state: State,
    pub(crate) instance: String,
    pub(crate) branch: String,
    pub(crate) worktree: String,
    pub(crate) source_repo: String,
    pub(crate) binding_sha256: String,
    pub(crate) binding_signature_sha256: String,
    #[serde(default)]
    pub(crate) archive: Option<String>,
}

pub(crate) fn path(home: &Path, instance: &str) -> PathBuf {
    home.join("deletion-recovery")
        .join(format!("{instance}.json"))
}

pub(crate) fn read(home: &Path, instance: &str) -> Result<Option<Tombstone>, String> {
    let path = path(home, instance);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "read deletion tombstone {}: {error}",
                path.display()
            ));
        }
    };
    let tombstone: Tombstone = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse deletion tombstone {}: {error}", path.display()))?;
    if tombstone.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported deletion tombstone schema {} (expected {})",
            tombstone.schema_version, SCHEMA_VERSION
        ));
    }
    Ok(Some(tombstone))
}

pub(crate) fn is_blocking(home: &Path, instance: &str) -> bool {
    let path = path(home, instance);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Tombstone>(&bytes)
            .map(|tombstone| {
                tombstone.schema_version != SCHEMA_VERSION || tombstone.state != State::Recovered
            })
            .unwrap_or(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

pub(crate) fn begin_from_binding(home: &Path, instance: &str) -> Result<Option<Tombstone>, String> {
    let binding_path = crate::paths::binding_path(home, instance);
    let binding_body = match std::fs::read(&binding_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read binding for deletion fence: {error}")),
    };
    let binding: Value = serde_json::from_slice(&binding_body)
        .map_err(|error| format!("recovery_required: binding is not valid JSON: {error}"))?;
    let object = binding
        .as_object()
        .ok_or_else(|| "recovery_required: binding is not an object".to_string())?;
    let branch = object
        .get("branch")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "recovery_required: binding branch is missing".to_string())?;
    let worktree = object
        .get("worktree")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "recovery_required: binding worktree is missing".to_string())?;
    // Legacy test/compatibility bindings predating signed source identity do
    // not participate in this recovery lane; the normal release path keeps
    // its existing behavior for them.  Production task bindings always carry
    // source_repo and a signature, which is the only state this tombstone can
    // safely preserve for operator recovery.
    let Some(source_repo) = object
        .get("source_repo")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let signature_path = crate::paths::runtime_dir(home)
        .join(instance)
        .join("binding.json.sig");
    let signature = std::fs::read(&signature_path)
        .map_err(|error| format!("recovery_required: read binding signature: {error}"))?;
    if !crate::binding::signature_valid(home, instance) {
        return Err("recovery_required: binding signature is invalid".to_string());
    }

    let tombstone = Tombstone {
        schema_version: SCHEMA_VERSION,
        state: State::Deleting,
        instance: instance.to_string(),
        branch: branch.to_string(),
        worktree: worktree.to_string(),
        source_repo: source_repo.to_string(),
        binding_sha256: crate::daemon::utils::sha256_hex(&binding_body),
        binding_signature_sha256: crate::daemon::utils::sha256_hex(&signature),
        archive: None,
    };
    write(home, &tombstone)?;
    Ok(Some(tombstone))
}

pub(crate) fn mark_recovery_required(
    home: &Path,
    instance: &str,
    archive: Option<&Path>,
) -> Result<(), String> {
    let mut tombstone = read(home, instance)?
        .ok_or_else(|| "recovery_required: delete tombstone is missing".to_string())?;
    tombstone.state = State::RecoveryRequired;
    tombstone.archive = archive.map(|path| path.display().to_string());
    write(home, &tombstone)
}

pub(crate) fn mark_recovered(home: &Path, instance: &str, archive: &Path) -> Result<(), String> {
    let mut tombstone = read(home, instance)?
        .ok_or_else(|| "recovery_required: delete tombstone is missing".to_string())?;
    tombstone.state = State::Recovered;
    tombstone.archive = Some(archive.display().to_string());
    write(home, &tombstone)
}

pub(crate) fn clear(home: &Path, instance: &str) -> Result<(), String> {
    let path = path(home, instance);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove deletion tombstone {}: {error}",
            path.display()
        )),
    }
}

fn write(home: &Path, tombstone: &Tombstone) -> Result<(), String> {
    let path = path(home, &tombstone.instance);
    let body = serde_json::to_vec_pretty(tombstone)
        .map_err(|error| format!("serialize deletion tombstone: {error}"))?;
    crate::store::atomic_write(&path, &body)
        .map_err(|error| format!("write deletion tombstone {}: {error}", path.display()))
}
