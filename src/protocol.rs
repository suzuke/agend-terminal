//! Fleet protocol extraction and resolution.
//!
//! Two-layer fallback: binary-embedded default (always overwritten on startup)
//! lives in `AGEND_HOME/protocol/.default/`. User overrides go in the parent
//! `AGEND_HOME/protocol/` directory and are never touched by the daemon.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const FILENAME: &str = "FLEET-DEV-PROTOCOL.md";

/// Embedded default protocol (compile-time).
const DEFAULT_PROTOCOL: &str = include_str!("../docs/FLEET-DEV-PROTOCOL.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolIdentity {
    pub source_kind: String,
    pub path: PathBuf,
    pub content_sha256: String,
    pub embedded_sha256: String,
    pub build_sha: String,
    pub build_dirty: bool,
    /// `ready` means the bytes equal the current embedded protocol. A stale
    /// but complete daemon-owned artifact is `degraded_serviceable`: it can
    /// still be inspected, but provisioning remains fail-closed.
    pub state: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolStatus {
    pub state: String,
    pub source_kind: Option<String>,
    pub path: Option<PathBuf>,
    pub content_sha256: Option<String>,
    pub embedded_sha256: String,
    pub build_sha: String,
    pub build_dirty: bool,
    pub error: Option<String>,
    pub workspaces: Vec<WorkspaceProtocolStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolDeliveryStamp {
    pub source_kind: String,
    pub path: PathBuf,
    pub content_sha256: String,
    pub embedded_sha256: String,
    pub build_sha: String,
    pub build_dirty: bool,
    pub delivery_state: String,
    pub consumption_state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceProtocolStatus {
    pub workspace: PathBuf,
    pub instruction_path: Option<PathBuf>,
    pub state: String,
    pub delivery_state: String,
    pub consumption_state: String,
    pub stamp: Option<ProtocolDeliveryStamp>,
    pub error: Option<String>,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn embedded_sha256() -> String {
    digest(DEFAULT_PROTOCOL.as_bytes())
}

fn build_sha() -> String {
    option_env!("AGEND_BUILD_SHA")
        .unwrap_or("unknown")
        .to_owned()
}

fn build_dirty() -> bool {
    option_env!("AGEND_BUILD_DIRTY") == Some("1")
}

fn identity_from_bytes_with_state(
    source_kind: &str,
    path: &Path,
    bytes: &[u8],
    state: &str,
    error: Option<String>,
) -> Result<ProtocolIdentity> {
    if bytes.is_empty() {
        bail!("protocol artifact {} is empty", path.display());
    }
    std::str::from_utf8(bytes)
        .with_context(|| format!("protocol artifact {} is not valid UTF-8", path.display()))?;
    Ok(ProtocolIdentity {
        source_kind: source_kind.to_owned(),
        path: path.to_path_buf(),
        content_sha256: digest(bytes),
        embedded_sha256: embedded_sha256(),
        build_sha: build_sha(),
        build_dirty: build_dirty(),
        state: state.to_owned(),
        error,
    })
}

fn identity_from_bytes(source_kind: &str, path: &Path, bytes: &[u8]) -> Result<ProtocolIdentity> {
    identity_from_bytes_with_state(source_kind, path, bytes, "ready", None)
}

fn repair_hint(home: &Path, path: &Path) -> String {
    let override_path = home.join("protocol").join(FILENAME);
    if path == override_path {
        format!(
            "repair with `agend-terminal doctor protocol --format json`; operator action: remove or replace the invalid override at {} with a regular non-empty UTF-8 file, then restart agend-terminal",
            path.display()
        )
    } else {
        format!(
            "repair with `agend-terminal doctor protocol --format json`; operator action: restart agend-terminal to retry atomic extraction of the daemon-owned default at {} (or restore it as a regular non-empty UTF-8 file)",
            path.display()
        )
    }
}

fn protocol_error(home: &Path, path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{error}; {}", repair_hint(home, path))
}

fn read_identity(path: &Path, source_kind: &str) -> Result<ProtocolIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat protocol artifact {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "protocol artifact {} is not a regular file (symlinks and directories are refused)",
            path.display()
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("read protocol artifact {}", path.display()))?;
    identity_from_bytes(source_kind, path, &bytes)
}

fn default_path(home: &Path) -> PathBuf {
    home.join("protocol").join(".default").join(FILENAME)
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct DefaultHealedAudit {
    target: &'static str,
    message: &'static str,
    path: PathBuf,
    from_digest: String,
    to_digest: String,
    build_sha: String,
}

#[cfg(test)]
#[derive(Default)]
struct DefaultHealedAuditFields {
    path: Option<PathBuf>,
    from_digest: Option<String>,
    to_digest: Option<String>,
    build_sha: Option<String>,
    message: Option<String>,
}

#[cfg(test)]
impl tracing::field::Visit for DefaultHealedAuditFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        match field.name() {
            "path" => self.path = Some(PathBuf::from(value)),
            "from_digest" => self.from_digest = Some(value),
            "to_digest" => self.to_digest = Some(value),
            "build_sha" => self.build_sha = Some(value),
            "message" => self.message = Some(value),
            _ => {}
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct DefaultHealedAuditLayer {
    events: std::sync::Arc<std::sync::Mutex<Vec<DefaultHealedAudit>>>,
}

#[cfg(test)]
impl<S> tracing_subscriber::layer::Layer<S> for DefaultHealedAuditLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != "agend_terminal::protocol" {
            return;
        }

        let mut fields = DefaultHealedAuditFields::default();
        event.record(&mut fields);
        let (Some(path), Some(from_digest), Some(to_digest), Some(build_sha), Some(message)) = (
            fields.path,
            fields.from_digest,
            fields.to_digest,
            fields.build_sha,
            fields.message,
        ) else {
            return;
        };
        if message != "protocol default healed" {
            return;
        }

        self.events
            .lock()
            .expect("default healed audit capture lock")
            .push(DefaultHealedAudit {
                target: "agend_terminal::protocol",
                message: "protocol default healed",
                path,
                from_digest,
                to_digest,
                build_sha,
            });
    }
}

#[cfg(test)]
fn capture_default_healed_audit<T>(f: impl FnOnce() -> T) -> (T, Vec<DefaultHealedAudit>) {
    use tracing_subscriber::prelude::*;

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(DefaultHealedAuditLayer {
        events: events.clone(),
    });
    let result = tracing::subscriber::with_default(subscriber, f);
    let events = std::sync::Arc::try_unwrap(events)
        .expect("default healed audit capture still referenced")
        .into_inner()
        .expect("default healed audit capture lock");
    (result, events)
}

fn emit_default_healed_audit(path: &Path, from_digest: &str, identity: &ProtocolIdentity) {
    tracing::info!(
        target: "agend_terminal::protocol",
        path = %path.display(),
        from_digest = %from_digest,
        to_digest = %identity.content_sha256,
        build_sha = %identity.build_sha,
        "protocol default healed"
    );
}

/// Extract embedded protocol to `AGEND_HOME/protocol/.default/`.
/// Always overwrites — `.default/` is daemon-owned. Replacement is atomic and
/// fallible, so a failed write cannot leave a partial protocol artifact.
pub fn extract_default(home: &Path) -> anyhow::Result<ProtocolIdentity> {
    let dir = home.join("protocol").join(".default");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create protocol directory {}", dir.display()))?;
    let path = dir.join(FILENAME);
    crate::store::atomic_write(&path, DEFAULT_PROTOCOL.as_bytes())
        .with_context(|| format!("atomically extract protocol to {}", path.display()))?;
    identity_from_bytes("default", &path, DEFAULT_PROTOCOL.as_bytes())
}

/// Resolve the artifact that may be relied on by a provisioned agent.
/// Explicit overrides are authoritative but must be structurally valid. The
/// daemon-owned default is repaired to the exact embedded bytes before it is
/// returned, including when an older complete artifact is present.
pub fn resolve_protocol(home: &Path) -> Result<ProtocolIdentity> {
    let override_path = home.join("protocol").join(FILENAME);
    match std::fs::symlink_metadata(&override_path) {
        Ok(_) => {
            return read_identity(&override_path, "override")
                .map_err(|error| protocol_error(home, &override_path, error));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(protocol_error(
                home,
                &override_path,
                format_args!("stat {}: {error}", override_path.display()),
            ));
        }
        Err(_) => {}
    }

    let default = default_path(home);
    match std::fs::symlink_metadata(&default) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            extract_default(home).map_err(|error| protocol_error(home, &default, error))
        }
        Err(error) => Err(protocol_error(
            home,
            &default,
            format_args!("stat {}: {error}", default.display()),
        )),
        Ok(metadata) if !metadata.file_type().is_file() => Err(protocol_error(
            home,
            &default,
            format_args!(
                "protocol default {} is not a regular file (directories and symlinks are refused)",
                default.display()
            ),
        )),
        Ok(_) => {
            let bytes = std::fs::read(&default).map_err(|error| {
                protocol_error(
                    home,
                    &default,
                    format_args!("read protocol default {}: {error}", default.display()),
                )
            })?;
            if bytes == DEFAULT_PROTOCOL.as_bytes() {
                identity_from_bytes("default", &default, &bytes)
            } else {
                let from_digest = digest(&bytes);
                match extract_default(home) {
                    Ok(_) => match read_identity(&default, "default") {
                        Ok(identity) if identity.content_sha256 == embedded_sha256() => {
                            emit_default_healed_audit(&default, &from_digest, &identity);
                            Ok(identity)
                        }
                        Ok(identity) => Err(protocol_error(
                            home,
                            &default,
                            format_args!(
                                "protocol default {} remains mismatched after refresh (sha {})",
                                default.display(),
                                identity.content_sha256
                            ),
                        )),
                        Err(error) => Err(protocol_error(
                            home,
                            &default,
                            format_args!("re-read after protocol refresh failed: {error}"),
                        )),
                    },
                    Err(refresh_error) => Err(protocol_error(
                        home,
                        &default,
                        format_args!(
                            "default refresh failed: {refresh_error}; existing complete artifact is degraded_serviceable but new provisioning is refused"
                        ),
                    )),
                }
            }
        }
    }
}

fn status_for_identity(identity: ProtocolIdentity, state: &str) -> ProtocolStatus {
    ProtocolStatus {
        state: state.to_owned(),
        source_kind: Some(identity.source_kind),
        path: Some(identity.path),
        content_sha256: Some(identity.content_sha256),
        embedded_sha256: identity.embedded_sha256,
        build_sha: identity.build_sha,
        build_dirty: identity.build_dirty,
        error: identity.error,
        workspaces: Vec::new(),
    }
}

fn invalid_status(state: &str, path: Option<PathBuf>, error: impl Into<String>) -> ProtocolStatus {
    ProtocolStatus {
        state: state.to_owned(),
        source_kind: None,
        path,
        content_sha256: None,
        embedded_sha256: embedded_sha256(),
        build_sha: build_sha(),
        build_dirty: build_dirty(),
        error: Some(error.into()),
        workspaces: Vec::new(),
    }
}

fn status_absent(default: PathBuf) -> ProtocolStatus {
    ProtocolStatus {
        state: "absent".into(),
        source_kind: None,
        path: Some(default),
        content_sha256: None,
        embedded_sha256: embedded_sha256(),
        build_sha: build_sha(),
        build_dirty: build_dirty(),
        error: None,
        workspaces: Vec::new(),
    }
}

const DELIVERY_STAMP_START: &str = "<!-- agend:protocol-delivery -->";
const DELIVERY_STAMP_END: &str = "<!-- /agend:protocol-delivery -->";

pub fn delivery_stamp(identity: &ProtocolIdentity) -> ProtocolDeliveryStamp {
    ProtocolDeliveryStamp {
        source_kind: identity.source_kind.clone(),
        path: identity.path.clone(),
        content_sha256: identity.content_sha256.clone(),
        embedded_sha256: identity.embedded_sha256.clone(),
        build_sha: identity.build_sha.clone(),
        build_dirty: identity.build_dirty,
        delivery_state: "delivered".into(),
        // A file being written proves delivery only. The agent's later read or
        // acknowledgement is a separate protocol and is deliberately not
        // inferred here.
        consumption_state: "not_proven".into(),
    }
}

pub fn format_delivery_stamp(identity: &ProtocolIdentity) -> Result<String> {
    Ok(format!(
        "{DELIVERY_STAMP_START}\n{}\n{DELIVERY_STAMP_END}",
        serde_json::to_string(&delivery_stamp(identity))?
    ))
}

/// Parse the machine-readable stamp embedded in an agend-managed instruction
/// file. The explicit markers keep status independent of human prose wording.
pub fn parse_delivery_stamp(content: &str) -> Result<ProtocolDeliveryStamp> {
    if content.matches(DELIVERY_STAMP_START).count() != 1
        || content.matches(DELIVERY_STAMP_END).count() != 1
    {
        bail!("protocol delivery stamp must contain exactly one marked block")
    }
    let start = content
        .find(DELIVERY_STAMP_START)
        .ok_or_else(|| anyhow!("protocol delivery stamp is absent"))?;
    let body_start = start + DELIVERY_STAMP_START.len();
    let end = content[body_start..]
        .find(DELIVERY_STAMP_END)
        .ok_or_else(|| anyhow!("protocol delivery stamp is unterminated"))?
        + body_start;
    let body = content[body_start..end].trim();
    let stamp: ProtocolDeliveryStamp =
        serde_json::from_str(body).with_context(|| "protocol delivery stamp is not valid JSON")?;
    if stamp.delivery_state != "delivered" {
        bail!("protocol delivery stamp has invalid delivery_state")
    }
    if stamp.consumption_state != "not_proven" {
        bail!("protocol delivery stamp has invalid consumption_state")
    }
    Ok(stamp)
}

fn workspace_candidates(home: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut candidates = Vec::new();
    if let Ok(config) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        for name in config.instance_names() {
            let Some(resolved) = config.resolve_instance(&name) else {
                continue;
            };
            let relative = resolved.backend.preset().instructions_path;
            if relative.is_empty() {
                continue;
            }
            let workspace = resolved
                .working_directory
                .unwrap_or_else(|| home.join("workspace").join(&name));
            candidates.push((workspace.clone(), Some(workspace.join(relative))));
        }
    }

    // A fleet can be absent or temporarily unparseable during setup. Include
    // existing workspace directories as a read-only fallback, and inspect all
    // known instruction locations so status still exposes stale delivery.
    if candidates.is_empty() {
        let root = home.join("workspace");
        if let Ok(entries) = std::fs::read_dir(&root) {
            let relative_paths = [
                ".claude/agend.md",
                ".kiro/steering/agend.md",
                "AGENTS.md",
                ".agents/AGENTS.md",
            ];
            for entry in entries.flatten() {
                if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }
                let workspace = entry.path();
                let existing: Vec<PathBuf> = relative_paths
                    .iter()
                    .map(|relative| workspace.join(relative))
                    .filter(|path| path.exists())
                    .collect();
                if existing.is_empty() {
                    candidates.push((workspace.clone(), None));
                } else {
                    candidates.extend(
                        existing
                            .into_iter()
                            .map(|path| (workspace.clone(), Some(path))),
                    );
                }
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn workspace_statuses(home: &Path, current: &ProtocolStatus) -> Vec<WorkspaceProtocolStatus> {
    workspace_candidates(home)
        .into_iter()
        .map(|(workspace, instruction_path)| {
            let Some(path) = instruction_path.clone() else {
                return WorkspaceProtocolStatus {
                    workspace,
                    instruction_path: None,
                    state: "not_delivered".into(),
                    delivery_state: "not_delivered".into(),
                    consumption_state: "not_proven".into(),
                    stamp: None,
                    error: None,
                };
            };
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return WorkspaceProtocolStatus {
                        workspace,
                        instruction_path: Some(path),
                        state: "not_delivered".into(),
                        delivery_state: "not_delivered".into(),
                        consumption_state: "not_proven".into(),
                        stamp: None,
                        error: None,
                    };
                }
                Err(error) => {
                    return WorkspaceProtocolStatus {
                        workspace,
                        instruction_path: Some(path),
                        state: "invalid_stamp".into(),
                        delivery_state: "unknown".into(),
                        consumption_state: "not_proven".into(),
                        stamp: None,
                        error: Some(error.to_string()),
                    };
                }
            };
            let stamp = match parse_delivery_stamp(&content) {
                Ok(stamp) => stamp,
                Err(error) => {
                    return WorkspaceProtocolStatus {
                        workspace,
                        instruction_path: Some(path),
                        state: "invalid_stamp".into(),
                        delivery_state: "unknown".into(),
                        consumption_state: "not_proven".into(),
                        stamp: None,
                        error: Some(error.to_string()),
                    };
                }
            };
            let current_matches = current.source_kind.as_deref()
                == Some(stamp.source_kind.as_str())
                && current.path.as_ref() == Some(&stamp.path)
                && current.content_sha256.as_deref() == Some(stamp.content_sha256.as_str())
                && current.embedded_sha256 == stamp.embedded_sha256
                && current.build_sha == stamp.build_sha
                && current.build_dirty == stamp.build_dirty;
            WorkspaceProtocolStatus {
                workspace,
                instruction_path: Some(path),
                state: if current_matches { "current" } else { "stale" }.into(),
                delivery_state: stamp.delivery_state.clone(),
                consumption_state: stamp.consumption_state.clone(),
                stamp: Some(stamp),
                error: None,
            }
        })
        .collect()
}

/// Return human- and machine-readable protocol delivery state without
/// mutating the workspace. This deliberately ignores temporary siblings and
/// reports the exact named override/default artifact only.
pub fn status(home: &Path) -> ProtocolStatus {
    let override_path = home.join("protocol").join(FILENAME);
    let mut result = match std::fs::symlink_metadata(&override_path) {
        Ok(_) => match read_identity(&override_path, "override") {
            Ok(identity) => status_for_identity(identity, "ready"),
            Err(error) => invalid_status(
                "invalid_override",
                Some(override_path.clone()),
                format!("{error}; {}", repair_hint(home, &override_path)),
            ),
        },
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => invalid_status(
            "invalid_override",
            Some(override_path.clone()),
            format!("{error}; {}", repair_hint(home, &override_path)),
        ),
        Err(_) => {
            let default = default_path(home);
            match std::fs::symlink_metadata(&default) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    status_absent(default)
                }
                Err(error) => invalid_status(
                    "invalid_default",
                    Some(default.clone()),
                    format!("{error}; {}", repair_hint(home, &default)),
                ),
                Ok(metadata) if !metadata.file_type().is_file() => invalid_status(
                    "invalid_default",
                    Some(default.clone()),
                    format!(
                        "artifact is not a regular file (directories and symlinks are refused); {}",
                        repair_hint(home, &default)
                    ),
                ),
                Ok(_) => match std::fs::read(&default) {
                    Ok(bytes) if bytes.is_empty() => invalid_status(
                        "invalid_default",
                        Some(default.clone()),
                        format!("artifact is empty; {}", repair_hint(home, &default)),
                    ),
                    Ok(bytes) => match identity_from_bytes("default", &default, &bytes) {
                        Ok(identity) if identity.content_sha256 == identity.embedded_sha256 => {
                            status_for_identity(identity, "ready")
                        }
                        Ok(identity) => {
                            let mut status = status_for_identity(identity, "degraded_serviceable");
                            status.error = Some(format!(
                                "degraded_serviceable: default artifact digest differs from embedded protocol; {}",
                                repair_hint(home, &default)
                            ));
                            status
                        }
                        Err(error) => invalid_status(
                            "invalid_default",
                            Some(default.clone()),
                            format!("{error}; {}", repair_hint(home, &default)),
                        ),
                    },
                    Err(error) => invalid_status(
                        "invalid_default",
                        Some(default.clone()),
                        format!("{error}; {}", repair_hint(home, &default)),
                    ),
                },
            }
        }
    };
    result.workspaces = workspace_statuses(home, &result);
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-protocol-test-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn extract_default_creates_file() {
        let home = tmp_home("extract");
        extract_default(&home).expect("extract");
        let path = default_path(&home);
        assert!(path.exists(), ".default/ file must exist after extract");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(
            content.contains("Fleet Development Protocol"),
            "extracted content must match embedded protocol"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn override_wins_over_default() {
        let home = tmp_home("override");
        extract_default(&home).expect("extract");
        let override_dir = home.join("protocol");
        std::fs::write(override_dir.join(FILENAME), "custom protocol").expect("write override");
        let path = resolve_protocol(&home).expect("valid override").path;
        assert_eq!(path, override_dir.join(FILENAME));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_override_falls_back_to_default() {
        let home = tmp_home("fallback");
        extract_default(&home).expect("extract");
        let path = resolve_protocol(&home).expect("valid default").path;
        assert_eq!(
            path,
            default_path(&home),
            "must fall back to .default/ when no override"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn missing_both_extracts_and_returns() {
        let home = tmp_home("empty");
        // Neither exists — resolve_protocol should extract and return .default/.
        let path = resolve_protocol(&home)
            .expect("missing protocol is extracted")
            .path;
        assert_eq!(path, default_path(&home));
        assert!(path.exists(), "must auto-extract when both missing");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_default_is_healed_before_selection() {
        let home = tmp_home("stale");
        let default_path = default_path(&home);
        std::fs::create_dir_all(default_path.parent().expect("default parent")).unwrap();
        std::fs::write(&default_path, "historically stale protocol").unwrap();

        let selected = resolve_protocol(&home)
            .expect("stale default is healed")
            .path;

        assert_eq!(selected, default_path);
        assert_eq!(
            std::fs::read_to_string(&selected).unwrap(),
            DEFAULT_PROTOCOL,
            "a stale daemon-owned default must self-heal before reliance"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_default_heal_emits_audit_fields() {
        let home = tmp_home("stale-audit");
        let default_path = default_path(&home);
        std::fs::create_dir_all(default_path.parent().expect("default parent")).unwrap();
        let stale = b"historically stale protocol";
        std::fs::write(&default_path, stale).unwrap();

        let (identity, events) = capture_default_healed_audit(|| {
            resolve_protocol(&home).expect("stale default is healed")
        });
        assert_eq!(events.len(), 1, "protocol default healed event");
        let event = &events[0];
        assert_eq!(event.target, "agend_terminal::protocol");
        assert_eq!(event.message, "protocol default healed");
        assert_eq!(event.path, default_path);
        assert_eq!(event.from_digest, digest(stale));
        assert_eq!(event.to_digest, identity.content_sha256);
        assert_eq!(event.build_sha, identity.build_sha);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn structural_override_artifacts_are_refused() {
        let home = tmp_home("override-structural");
        extract_default(&home).expect("extract");
        let override_path = home.join("protocol").join(FILENAME);
        std::fs::create_dir(&override_path).unwrap();

        let error = resolve_protocol(&home).expect_err("directory override must be refused");
        assert!(error.to_string().contains("doctor protocol"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_override_is_refused() {
        let home = tmp_home("override-symlink");
        extract_default(&home).expect("extract");
        let override_path = home.join("protocol").join(FILENAME);
        let target = home.join("outside-protocol.md");
        std::fs::write(&target, "outside").unwrap();
        std::os::unix::fs::symlink(&target, &override_path).unwrap();

        let error = resolve_protocol(&home).expect_err("symlink override must be refused");
        assert!(error.to_string().contains("doctor protocol"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_and_invalid_utf8_overrides_are_refused_and_reported() {
        let home = tmp_home("override-content-invalid");
        extract_default(&home).expect("extract");
        let override_path = home.join("protocol").join(FILENAME);

        std::fs::write(&override_path, []).unwrap();
        assert!(
            resolve_protocol(&home).is_err(),
            "empty override must refuse"
        );
        assert_eq!(status(&home).state, "invalid_override");

        std::fs::write(&override_path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(
            resolve_protocol(&home).is_err(),
            "invalid UTF-8 override must refuse"
        );
        assert_eq!(status(&home).state, "invalid_override");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn failed_atomic_replacement_preserves_complete_default() {
        let home = tmp_home("atomic-failure");
        extract_default(&home).expect("extract");
        let default_path = default_path(&home);
        let before = std::fs::read(&default_path).unwrap();
        crate::store::fail_next_atomic_write_for_test(&default_path);

        let _ = extract_default(&home);

        assert_eq!(
            std::fs::read(&default_path).unwrap(),
            before,
            "failed replacement must preserve the prior complete artifact"
        );
        assert!(
            crate::store::atomic_write(&default_path, b"probe").is_ok(),
            "extract_default must consume the deterministic failure seam"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn failed_refresh_refuses_provision_but_reports_degraded_serviceable() {
        let home = tmp_home("degraded-serviceable");
        let default_path = default_path(&home);
        std::fs::create_dir_all(default_path.parent().expect("default parent")).unwrap();
        let old = b"historically stale but complete protocol";
        std::fs::write(&default_path, old).unwrap();
        crate::store::fail_next_atomic_write_for_test(&default_path);

        let error = resolve_protocol(&home).expect_err("stale default must refuse reliance");
        assert_eq!(std::fs::read(&default_path).unwrap(), old);
        assert!(error.to_string().contains("doctor protocol"));
        assert_eq!(status(&home).state, "degraded_serviceable");
        assert!(status(&home)
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("degraded_serviceable"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn delivery_stamp_parser_rejects_malformed_duplicate_and_unterminated_blocks() {
        let identity = ProtocolIdentity {
            source_kind: "default".into(),
            path: PathBuf::from("protocol").join(FILENAME),
            content_sha256: "content".into(),
            embedded_sha256: "embedded".into(),
            build_sha: "build".into(),
            build_dirty: false,
            state: "ready".into(),
            error: None,
        };
        let stamp = format_delivery_stamp(&identity).expect("stamp");
        assert!(parse_delivery_stamp("not a stamp").is_err());
        assert!(parse_delivery_stamp(&format!("{stamp}\n{stamp}")).is_err());
        assert!(parse_delivery_stamp(&stamp.replace(DELIVERY_STAMP_END, "")).is_err());
        assert!(parse_delivery_stamp(&stamp.replace("\"delivered\"", "\"unknown\"")).is_err());
    }

    #[test]
    fn workspace_status_distinguishes_current_stale_and_consumption() {
        let home = tmp_home("workspace-status");
        let identity = extract_default(&home).expect("extract");
        let workspace = home.join("workspace").join("agent");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(home.join("workspace").join("undelivered")).unwrap();
        let instruction_path = workspace.join("AGENTS.md");
        let mut current = delivery_stamp(&identity);
        let content = format_delivery_stamp(&identity).expect("stamp");
        std::fs::write(&instruction_path, content).unwrap();

        let report = status(&home);
        assert_eq!(report.workspaces.len(), 2);
        let current_status = report
            .workspaces
            .iter()
            .find(|status| status.workspace.ends_with("agent"))
            .expect("current workspace status");
        assert_eq!(current_status.state, "current");
        assert_eq!(current_status.delivery_state, "delivered");
        assert_eq!(current_status.consumption_state, "not_proven");

        current.content_sha256 = "stale".into();
        std::fs::write(
            &instruction_path,
            format!(
                "{}\n{}\n{}",
                DELIVERY_STAMP_START,
                serde_json::to_string(&current).unwrap(),
                DELIVERY_STAMP_END
            ),
        )
        .unwrap();
        let report = status(&home);
        assert_eq!(
            report
                .workspaces
                .iter()
                .find(|status| status.workspace.ends_with("agent"))
                .expect("stale workspace status")
                .state,
            "stale"
        );
        assert_eq!(
            report
                .workspaces
                .iter()
                .find(|status| status.workspace.ends_with("undelivered"))
                .expect("undelivered workspace status")
                .state,
            "not_delivered"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
