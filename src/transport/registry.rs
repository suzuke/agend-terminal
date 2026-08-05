use super::codex_app_server::CodexNativeShared;
use super::envelope::{DeliveryEnvelope, DeliveryKind, SessionLocator};
use super::legacy_pty::{LegacyPty, PtyInjector};
use super::{DeliveryReceipt, TransportMode};
use crate::backend::Backend;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolve the declared backend.  A missing or stale fleet entry is not a
/// reason to guess NativeShared from a command basename, so the safe explicit
/// mode is LegacyPty.
pub(crate) fn backend_for_instance(home: &Path, instance: &str) -> Option<Backend> {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|fleet| {
            fleet
                .resolve_instance(instance)
                .map(|resolved| resolved.backend)
        })
}

pub(crate) fn mode_for_backend(backend: &Backend) -> TransportMode {
    match backend {
        Backend::Codex => TransportMode::NativeShared,
        // OpenCode and Claude are deliberately not implemented in this PR.
        Backend::ClaudeCode
        | Backend::OpenCode
        | Backend::Grok
        | Backend::KiroCli
        | Backend::Agy
        | Backend::Shell
        | Backend::Raw(_) => TransportMode::LegacyPty,
    }
}

pub(crate) fn mode_for_instance(home: &Path, instance: &str) -> TransportMode {
    backend_for_instance(home, instance)
        .as_ref()
        .map(mode_for_backend)
        .unwrap_or(TransportMode::LegacyPty)
}

fn session_path(home: &Path, instance: &str) -> PathBuf {
    home.join("transport")
        .join("sessions")
        .join(format!("{}.json", super::receipt::safe_component(instance)))
}

#[cfg(unix)]
pub(crate) fn save_session_locator(
    home: &Path,
    instance: &str,
    locator: &SessionLocator,
) -> anyhow::Result<()> {
    let path = session_path(home, instance);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(locator)?;
    crate::store::atomic_write(&path, &bytes)
}

pub(crate) fn load_session_locator(home: &Path, instance: &str) -> anyhow::Result<SessionLocator> {
    let path = session_path(home, instance);
    let bytes = std::fs::read(&path).map_err(|error| {
        anyhow::anyhow!(
            "NativeShared session locator is unavailable at {}: {error}",
            path.display()
        )
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn default_codex_locator(home: &Path, instance: &str) -> SessionLocator {
    let endpoint = std::env::var_os("AGEND_CODEX_APP_SERVER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.join("transport")
                .join("codex")
                .join(format!("{}.sock", super::receipt::safe_component(instance)))
        });
    let thread_id = std::env::var("AGEND_CODEX_THREAD_ID").ok();
    SessionLocator::codex(endpoint, thread_id)
}

fn locator_for_instance(
    home: &Path,
    instance: &str,
    backend: Option<&Backend>,
    mode: TransportMode,
) -> anyhow::Result<SessionLocator> {
    if mode == TransportMode::NativeShared {
        let path = session_path(home, instance);
        return match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => load_session_locator(home, instance),
            Ok(_) => Err(anyhow::anyhow!(
                "NativeShared session locator is not a regular file: {}",
                path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(default_codex_locator(home, instance))
            }
            Err(error) => Err(anyhow::anyhow!(
                "NativeShared session locator cannot be inspected at {}: {error}",
                path.display()
            )),
        };
    }
    Ok(SessionLocator {
        backend: backend
            .map(Backend::as_str)
            .unwrap_or("unknown")
            .to_string(),
        endpoint: None,
        thread_id: None,
        session_id: None,
    })
}

pub(crate) fn envelope_for_instance(
    home: &Path,
    instance: &str,
    body: &str,
) -> anyhow::Result<DeliveryEnvelope> {
    let backend = backend_for_instance(home, instance);
    let mode = mode_for_instance(home, instance);
    let locator = locator_for_instance(home, instance, backend.as_ref(), mode)?;
    Ok(DeliveryEnvelope::new(
        instance,
        locator,
        DeliveryKind::Notification,
        body,
        None,
    ))
}

/// Persist a local queue drop as a terminal failure. This is used when the
/// caller-facing bounded worker queue is full, before any backend request was
/// attempted; structured failures never become an invisible PTY fallback.
pub(crate) fn record_delivery_drop(
    home: &Path,
    instance: &str,
    body: &str,
    detail: &str,
) -> anyhow::Result<DeliveryReceipt> {
    let envelope = envelope_for_instance(home, instance, body)?;
    let store = super::ReceiptStore::for_instance(home, instance)?;
    store.record_queued(&envelope)?;
    let mut receipt = DeliveryReceipt::for_state(&envelope, super::DeliveryState::Failed);
    receipt.detail = Some(detail.to_string());
    store.record(receipt.clone())?;
    Ok(receipt)
}

/// Deliver one already-composed notification. The selected mode is persisted
/// before the physical/structured attempt; a Codex failure is returned as a
/// hard error and never invokes the LegacyPty closure.
pub(crate) fn deliver_notification<F>(
    home: &Path,
    instance: &str,
    body: &str,
    legacy_injector: F,
) -> anyhow::Result<DeliveryReceipt>
where
    F: Fn(&Path, &str, &str) -> anyhow::Result<()> + Send + Sync + 'static,
{
    let mode = mode_for_instance(home, instance);
    let envelope = envelope_for_instance(home, instance, body)?;
    match mode {
        TransportMode::NativeShared => {
            let mut adapter = CodexNativeShared::new(home, instance);
            adapter.deliver_blocking(envelope)
        }
        TransportMode::LegacyPty => {
            let injector: PtyInjector = Arc::new(legacy_injector);
            let mut adapter = LegacyPty::new(home, instance, injector);
            adapter.deliver_blocking(envelope)
        }
        TransportMode::ChannelBridge
        | TransportMode::ManagedHeadless
        | TransportMode::ManualRequired => Err(anyhow::anyhow!(
            "transport mode {mode:?} has no adapter in this implementation"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn codex_mode_failure_never_calls_pty_fallback() {
        let home =
            std::env::temp_dir().join(format!("agend-transport-registry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  codex-agent:\n    backend: codex\n",
        )
        .expect("fleet");
        let pty_calls = Arc::new(AtomicUsize::new(0));
        let pty_calls_for_injector = Arc::clone(&pty_calls);
        let result = deliver_notification(&home, "codex-agent", "hello", move |_, _, _| {
            pty_calls_for_injector.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(pty_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn corrupt_codex_locator_fails_closed_without_env_fallback() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-corrupt-locator-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(home.join("transport/sessions")).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  codex-agent:\n    backend: codex\n",
        )
        .expect("fleet");
        std::fs::write(session_path(&home, "codex-agent"), b"{not valid json")
            .expect("corrupt locator");
        let pty_calls = Arc::new(AtomicUsize::new(0));
        let pty_calls_for_injector = Arc::clone(&pty_calls);
        let result = deliver_notification(&home, "codex-agent", "hello", move |_, _, _| {
            pty_calls_for_injector.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(pty_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }
}
