use super::claude_channel;
use super::codex_app_server::CodexNativeShared;
use super::envelope::{DeliveryEnvelope, DeliveryKind, SessionLocator};
use super::legacy_pty::{LegacyPty, PtyInjector};
use super::opencode_server::OpenCodeNativeShared;
use super::{DeliveryReceipt, TransportMode};
use crate::backend::Backend;
use base64::Engine as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolve the declared backend. A missing or stale fleet entry is not enough
/// to guess NativeShared, but a persisted structured session artifact is an
/// explicit mode anchor and must never silently downgrade to PTY.
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
        Backend::Codex | Backend::OpenCode => TransportMode::NativeShared,
        Backend::ClaudeCode => TransportMode::ChannelBridge,
        Backend::Grok | Backend::KiroCli | Backend::Agy | Backend::Shell | Backend::Raw(_) => {
            TransportMode::LegacyPty
        }
    }
}

pub(crate) fn mode_for_instance(home: &Path, instance: &str) -> TransportMode {
    let backend = backend_for_instance(home, instance);
    if backend.as_ref() == Some(&Backend::ClaudeCode) {
        return if claude_channel::legacy_pty_opt_in(home, instance) {
            TransportMode::LegacyPty
        } else {
            TransportMode::ChannelBridge
        };
    }
    if persisted_native_shared_hint(home, instance) {
        return TransportMode::NativeShared;
    }
    backend
        .as_ref()
        .map(mode_for_backend)
        .unwrap_or(TransportMode::LegacyPty)
}

/// A session locator is written only after the structured adapter has selected
/// the Codex session. Keep that mode even when fleet.yaml is temporarily
/// missing, stale, or unreadable. If the artifact itself is malformed or has
/// an unsafe type, choose NativeShared so locator resolution fails closed
/// instead of falling back to a PTY write.
fn persisted_native_shared_hint(home: &Path, instance: &str) -> bool {
    let path = session_path(home, instance);
    match std::fs::metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return true;
            }
            // Any regular session artifact is an explicit structured-mode
            // anchor. The actual locator parse below remains fail-closed.
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn session_path(home: &Path, instance: &str) -> PathBuf {
    home.join("transport")
        .join("sessions")
        .join(format!("{}.json", super::receipt::safe_component(instance)))
}

pub(crate) fn claude_attach_locator(
    home: &Path,
    instance: &str,
) -> anyhow::Result<Option<SessionLocator>> {
    let path = session_path(home, instance);
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            let locator = load_session_locator(home, instance)?;
            if locator.backend == "claude" {
                Ok(Some(locator))
            } else {
                Err(anyhow::anyhow!(
                    "Claude session locator belongs to backend {}",
                    locator.backend
                ))
            }
        }
        Ok(_) => Err(anyhow::anyhow!(
            "Claude session locator is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "Claude session locator cannot be inspected at {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn remove_session_locator(home: &Path, instance: &str) -> anyhow::Result<()> {
    let path = session_path(home, instance);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn save_session_locator(
    home: &Path,
    instance: &str,
    locator: &SessionLocator,
) -> anyhow::Result<()> {
    let path = session_path(home, instance);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let bytes = serde_json::to_vec_pretty(locator)?;
    secure_atomic_write(&path, &bytes)?;
    Ok(())
}

#[cfg(unix)]
fn secure_atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use uuid::Uuid;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session locator has no parent directory"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("locator.json");
    let temp = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temp, path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        crate::store::fsync_parent_dir(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(not(unix))]
fn secure_atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::store::atomic_write(path, bytes)
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

fn default_opencode_locator(home: &Path, instance: &str) -> anyhow::Result<SessionLocator> {
    let endpoint = match std::env::var("AGEND_OPENCODE_SERVER_URL") {
        Ok(endpoint) => endpoint,
        Err(_) => {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
            let port = listener.local_addr()?.port();
            drop(listener);
            format!("http://127.0.0.1:{port}")
        }
    };
    let session_id = std::env::var("AGEND_OPENCODE_SESSION_ID").ok();
    let username =
        std::env::var("AGEND_OPENCODE_SERVER_USERNAME").unwrap_or_else(|_| "opencode".to_string());
    let external = std::env::var("AGEND_OPENCODE_EXTERNAL").ok().as_deref() == Some("1");
    let password = std::env::var("AGEND_OPENCODE_SERVER_PASSWORD")
        .ok()
        .or_else(|| {
            if external {
                return None;
            }
            let mut bytes = [0_u8; 32];
            getrandom::fill(&mut bytes).expect("OS randomness is required for OpenCode auth");
            Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
        });
    let mut locator = SessionLocator::opencode(
        endpoint,
        session_id,
        username,
        password.clone().unwrap_or_default(),
    );
    locator.password = password;
    locator.managed = !external;
    let _ = (home, instance);
    Ok(locator)
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match backend {
                Some(Backend::OpenCode) => default_opencode_locator(home, instance),
                _ => Ok(default_codex_locator(home, instance)),
            },
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
        endpoint_url: None,
        username: None,
        password: None,
        model: None,
        event_cursor: None,
        managed: false,
        server_pid: None,
        server_start_token: None,
    })
}

pub(crate) fn opencode_attach_locator(
    home: &Path,
    instance: &str,
) -> anyhow::Result<Option<SessionLocator>> {
    let path = session_path(home, instance);
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            let locator = load_session_locator(home, instance)?;
            if locator.backend == "opencode" {
                Ok(Some(locator))
            } else {
                Ok(None)
            }
        }
        Ok(_) => Err(anyhow::anyhow!(
            "OpenCode session locator is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "OpenCode session locator cannot be inspected at {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn codex_attach_locator(
    home: &Path,
    instance: &str,
) -> anyhow::Result<Option<SessionLocator>> {
    let path = session_path(home, instance);
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            let locator = load_session_locator(home, instance)?;
            if locator.backend == "codex" {
                Ok(Some(locator))
            } else {
                Ok(None)
            }
        }
        Ok(_) => Err(anyhow::anyhow!(
            "Codex session locator is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(
            "Codex session locator cannot be inspected at {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn prepare_codex_tui_session(
    home: &Path,
    instance: &str,
    codex: &str,
    working_dir: Option<&Path>,
    spawn_mode: crate::backend::SpawnMode,
) -> anyhow::Result<SessionLocator> {
    let mut locator = locator_for_instance(
        home,
        instance,
        Some(&Backend::Codex),
        TransportMode::NativeShared,
    )?;
    locator.thread_id = codex_thread_for_spawn(&locator, spawn_mode);
    super::codex_app_server::prepare_managed_tui(home, instance, codex, locator, working_dir)
}

pub(crate) fn codex_thread_for_spawn(
    locator: &SessionLocator,
    spawn_mode: crate::backend::SpawnMode,
) -> Option<String> {
    match spawn_mode {
        crate::backend::SpawnMode::Resume => locator
            .thread_id
            .as_deref()
            .filter(|thread_id| !thread_id.is_empty())
            .map(str::to_string),
        crate::backend::SpawnMode::Fresh => None,
    }
}

pub(crate) fn opencode_attach_args(locator: &SessionLocator) -> anyhow::Result<Vec<String>> {
    OpenCodeNativeShared::attach_args(locator)
}

pub(crate) fn codex_attach_args(locator: &SessionLocator) -> anyhow::Result<Vec<String>> {
    if locator.backend != "codex" {
        return Err(anyhow::anyhow!("Codex attach locator backend is not codex"));
    }
    if locator.endpoint.is_none() {
        return Err(anyhow::anyhow!("Codex NativeShared endpoint is missing"));
    }
    Ok(CodexNativeShared::remote_attach_args(locator))
}

pub(crate) fn prepare_opencode_tui_session(
    home: &Path,
    instance: &str,
    working_dir: Option<&Path>,
    args: &[String],
) -> anyhow::Result<SessionLocator> {
    let mut locator = locator_for_instance(
        home,
        instance,
        Some(&Backend::OpenCode),
        TransportMode::NativeShared,
    )?;
    if let Some(model) = parse_opencode_model_args(args)?.model {
        locator.model = Some(model);
    }
    super::opencode_server::prepare_resident_tui(home, instance, locator, working_dir)
}

pub(crate) fn parse_opencode_model_args(
    args: &[String],
) -> anyhow::Result<crate::backend_model::ParsedModelArgs> {
    Backend::OpenCode
        .model_capability()
        .ok_or_else(|| anyhow::anyhow!("OpenCode model capability is unavailable"))?
        .parse(args)
        .map_err(|error| anyhow::anyhow!("invalid OpenCode model argument: {error}"))
}

pub(crate) fn envelope_for_instance(
    home: &Path,
    instance: &str,
    body: &str,
) -> anyhow::Result<DeliveryEnvelope> {
    let mode = mode_for_instance(home, instance);
    envelope_for_mode(home, instance, body, mode, None)
}

fn envelope_for_mode(
    home: &Path,
    instance: &str,
    body: &str,
    mode: TransportMode,
    channel_origin: Option<crate::channel::ChannelKind>,
) -> anyhow::Result<DeliveryEnvelope> {
    let backend = backend_for_instance(home, instance);
    let locator = if mode == TransportMode::ChannelBridge {
        super::claude_channel::prepare_claude_channel(home, instance)?
    } else {
        locator_for_instance(home, instance, backend.as_ref(), mode)?
    };
    let mut envelope =
        DeliveryEnvelope::new(instance, locator, DeliveryKind::Notification, body, None);
    envelope.transport_mode = Some(mode.receipt_route().to_string());
    envelope.logical_delivery_id =
        crate::daemon::notification_dedup::extract_msg_id_from_header(body);
    // #3324: typed provenance, CARRIED from the inbound construction. Never
    // re-derived from the rendered header — a display name is free text and can
    // contain the `]` that closes it.
    envelope.channel_origin = channel_origin;
    Ok(envelope)
}

fn self_kick_envelope_for_mode(
    home: &Path,
    instance: &str,
    body: &str,
    mode: TransportMode,
    channel_origin: Option<crate::channel::ChannelKind>,
) -> anyhow::Result<DeliveryEnvelope> {
    let backend = backend_for_instance(home, instance);
    let locator = if mode == TransportMode::ChannelBridge {
        super::claude_channel::prepare_claude_channel(home, instance)?
    } else {
        locator_for_instance(home, instance, backend.as_ref(), mode)?
    };
    let mut envelope = DeliveryEnvelope::self_kick(instance, locator, body);
    envelope.transport_mode = Some(mode.receipt_route().to_string());
    envelope.logical_delivery_id =
        crate::daemon::notification_dedup::extract_msg_id_from_header(body);
    // #3324: the second envelope path carries it identically — see the first.
    envelope.channel_origin = channel_origin;
    Ok(envelope)
}

/// Wait only for the fresh-spawn ChannelBridge publication/health window.
/// Callers must do this before acquiring the keyed transport lane: ordinary
/// queued notifications stay fail-fast, and same-agent teardown is never
/// parked behind a cold bridge readiness poll.
pub(crate) fn wait_for_notification_readiness(
    home: &Path,
    instance: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    if mode_for_instance(home, instance) != TransportMode::ChannelBridge {
        return Ok(());
    }
    #[cfg(test)]
    if let Some(result) = test_support::run_readiness_hook(home, instance, timeout) {
        return result;
    }
    super::claude_channel::wait_for_ready_claude_channel(home, instance, timeout).map(|_| ())
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
/// #3324: `channel_origin` is the TYPED external-channel provenance, carried
/// from the inbound construction into the durable envelope so the ChannelBridge
/// reply guard can tell a forwarded Telegram message from AgEnD's own traffic
/// without reading the header text. It is a required argument rather than a
/// defaulted one: a permissive `None` must be a caller's stated claim about its
/// own delivery, never something a new call site inherits by picking the
/// shorter overload.
pub(crate) fn deliver_notification<F>(
    home: &Path,
    instance: &str,
    body: &str,
    channel_origin: Option<crate::channel::ChannelKind>,
    legacy_injector: F,
) -> anyhow::Result<DeliveryReceipt>
where
    F: Fn(&Path, &str, &str) -> anyhow::Result<()> + Send + Sync + 'static,
{
    deliver_notification_kind(home, instance, body, false, channel_origin, legacy_injector)
}

/// Deliver the fresh-restart self-kick through the same exact-target and keyed
/// transport lane as ordinary notifications, while persisting the distinct
/// consumer-ack contract in the envelope. Structured failures still never
/// fall through to PTY.
pub(crate) fn deliver_self_kick_notification<F>(
    home: &Path,
    instance: &str,
    body: &str,
    legacy_injector: F,
) -> anyhow::Result<DeliveryReceipt>
where
    F: Fn(&Path, &str, &str) -> anyhow::Result<()> + Send + Sync + 'static,
{
    deliver_notification_kind(home, instance, body, true, None, legacy_injector)
}

fn deliver_notification_kind<F>(
    home: &Path,
    instance: &str,
    body: &str,
    self_kick: bool,
    channel_origin: Option<crate::channel::ChannelKind>,
    legacy_injector: F,
) -> anyhow::Result<DeliveryReceipt>
where
    F: Fn(&Path, &str, &str) -> anyhow::Result<()> + Send + Sync + 'static,
{
    #[cfg(test)]
    if let Some(result) = test_support::run_delivery_hook(home, instance, body) {
        return result;
    }
    let mode = mode_for_instance(home, instance);
    let envelope = if self_kick {
        self_kick_envelope_for_mode(home, instance, body, mode, channel_origin)?
    } else {
        envelope_for_mode(home, instance, body, mode, channel_origin)?
    };
    match mode {
        TransportMode::NativeShared => match envelope.session.backend.as_str() {
            "codex" => {
                let mut adapter = CodexNativeShared::new(home, instance);
                adapter.deliver_blocking(envelope)
            }
            "opencode" => super::opencode_server::deliver_resident(home, instance, envelope),
            backend => Err(anyhow::anyhow!(
                "NativeShared backend {backend:?} has no registered adapter"
            )),
        },
        TransportMode::LegacyPty => {
            let injector: PtyInjector = Arc::new(legacy_injector);
            let mut adapter = LegacyPty::new(home, instance, injector);
            adapter.deliver_blocking(envelope)
        }
        TransportMode::ChannelBridge => {
            if envelope.session.backend == "claude" {
                super::claude_channel::deliver_resident(home, instance, envelope)
            } else {
                Err(anyhow::anyhow!(
                    "ChannelBridge backend {:?} has no registered adapter",
                    envelope.session.backend
                ))
            }
        }
        TransportMode::ManagedHeadless | TransportMode::ManualRequired => Err(anyhow::anyhow!(
            "transport mode {mode:?} has no adapter in this implementation"
        )),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::DeliveryReceipt;
    use std::path::Path;
    use std::sync::{Arc, OnceLock};

    pub(crate) type DeliveryHook =
        Arc<dyn Fn(&Path, &str, &str) -> Option<anyhow::Result<DeliveryReceipt>> + Send + Sync>;
    pub(crate) type ReadinessHook =
        Arc<dyn Fn(&Path, &str, std::time::Duration) -> Option<anyhow::Result<()>> + Send + Sync>;

    static DELIVERY_HOOK: OnceLock<parking_lot::Mutex<Option<DeliveryHook>>> = OnceLock::new();
    static READINESS_HOOK: OnceLock<parking_lot::Mutex<Option<ReadinessHook>>> = OnceLock::new();
    static DELIVERY_HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) struct DeliveryHookGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    pub(crate) fn delivery_hook_guard() -> DeliveryHookGuard {
        let lock = DELIVERY_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_delivery_hook(None);
        set_readiness_hook(None);
        DeliveryHookGuard { _lock: lock }
    }

    fn hook() -> &'static parking_lot::Mutex<Option<DeliveryHook>> {
        DELIVERY_HOOK.get_or_init(|| parking_lot::Mutex::new(None))
    }

    pub(crate) fn set_delivery_hook(next: Option<DeliveryHook>) {
        *hook().lock() = next;
    }

    fn readiness_hook() -> &'static parking_lot::Mutex<Option<ReadinessHook>> {
        READINESS_HOOK.get_or_init(|| parking_lot::Mutex::new(None))
    }

    pub(crate) fn set_readiness_hook(next: Option<ReadinessHook>) {
        *readiness_hook().lock() = next;
    }

    impl Drop for DeliveryHookGuard {
        fn drop(&mut self) {
            set_delivery_hook(None);
            set_readiness_hook(None);
        }
    }

    pub(crate) fn run_delivery_hook(
        home: &Path,
        instance: &str,
        body: &str,
    ) -> Option<anyhow::Result<DeliveryReceipt>> {
        let delivery_hook = hook().lock().as_ref().cloned();
        delivery_hook.and_then(|hook| hook(home, instance, body))
    }

    pub(crate) fn run_readiness_hook(
        home: &Path,
        instance: &str,
        timeout: std::time::Duration,
    ) -> Option<anyhow::Result<()>> {
        let readiness_hook = readiness_hook().lock().as_ref().cloned();
        readiness_hook.and_then(|hook| hook(home, instance, timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// #3324: the durable envelope records the origin the CALLER passed, for
    /// both channel kinds. This is the value the ChannelBridge reply guard
    /// reads, and it now arrives as a typed argument from the inbound
    /// construction rather than being recovered from the rendered header.
    #[test]
    fn envelope_records_the_caller_supplied_channel_origin_3324() {
        let home = registry_test_home("origin-typed-3324");
        for kind in [
            crate::channel::ChannelKind::Telegram,
            crate::channel::ChannelKind::Discord,
        ] {
            for pointer_only in [false, true] {
                let body = crate::inbox::notify::format_notification_for_inject(
                    pointer_only,
                    &crate::inbox::NotifySource::Channel("chiachenghuang", kind),
                    "please research this",
                    &[],
                );
                let envelope = envelope_for_mode(
                    &home,
                    "claude-agent",
                    &body,
                    TransportMode::LegacyPty,
                    Some(kind),
                )
                .expect("envelope");
                assert_eq!(
                    envelope.channel_origin,
                    Some(kind),
                    "#3324: the envelope must carry the origin it was given \
                     (kind={kind:?}, pointer_only={pointer_only})"
                );
            }
        }
        let _ = std::fs::remove_dir_all(home);
    }

    /// #3324 REGRESSION GUARD: the envelope must NOT consult the body text.
    ///
    /// The rejected first cut recovered the origin by reverse-parsing the
    /// `[{source}]` header, which made a security classification depend on
    /// operator- and sender-authored free text — a display name containing the
    /// header's own `]` classified an external delivery as internal, the
    /// permissive side of the guard. Both directions are pinned here: a body
    /// that LOOKS external stays unstamped when the caller says internal, and a
    /// body that looks internal is stamped when the caller says external. Any
    /// reintroduced parser fails one of the two.
    #[test]
    fn envelope_origin_ignores_the_body_text_entirely_3324() {
        let home = registry_test_home("origin-not-parsed-3324");
        // Looks external, caller says internal (e.g. an agent quoting a user).
        for body in [
            "[user:alice via telegram] quoted by an internal sender",
            "[user:bob via discord] quoted by an internal sender",
        ] {
            let envelope =
                envelope_for_mode(&home, "claude-agent", body, TransportMode::LegacyPty, None)
                    .expect("envelope");
            assert_eq!(
                envelope.channel_origin, None,
                "#3324: the header text must not stamp an origin the caller did \
                 not claim; body={body:?}"
            );
        }
        // Looks internal, caller says external — the bracketed-display-name
        // case reaches the guard through exactly this shape.
        let envelope = envelope_for_mode(
            &home,
            "claude-agent",
            "[from:codex-125550] nothing here says telegram",
            TransportMode::LegacyPty,
            Some(crate::channel::ChannelKind::Telegram),
        )
        .expect("envelope");
        assert_eq!(
            envelope.channel_origin,
            Some(crate::channel::ChannelKind::Telegram),
            "#3324: an external delivery must stay external even when its text \
             carries no channel marker"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// #3324: internal traffic must stay unstamped, or the guard would refuse
    /// every agent-to-agent and system delivery the bridge legitimately owns.
    #[test]
    fn envelope_leaves_channel_origin_absent_for_internal_bodies_3324() {
        let home = registry_test_home("origin-internal-3324");
        for body in [
            "[from:codex-125550] status?",
            "[system:ci] build finished",
            "plain text with no header at all",
        ] {
            let envelope =
                envelope_for_mode(&home, "claude-agent", body, TransportMode::LegacyPty, None)
                    .expect("envelope");
            assert_eq!(
                envelope.channel_origin, None,
                "#3324: internal delivery must not be stamped; body={body:?}"
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    /// #3324: the self-kick constructor is the second envelope path and must
    /// carry the origin identically — stamping only one constructor leaves the
    /// other silently unguarded, which is how a two-path defect survives a
    /// one-path fix. Production pins `None` there (the self-kick prompt is
    /// daemon-composed); this pins the PLUMBING, so the day a resumable
    /// external delivery needs it the value is not silently dropped.
    #[test]
    fn self_kick_envelope_carries_the_caller_supplied_origin_3324() {
        let home = registry_test_home("origin-self-kick-3324");
        let envelope = self_kick_envelope_for_mode(
            &home,
            "claude-agent",
            "[AGEND-RESUME] recover your own state",
            TransportMode::LegacyPty,
            Some(crate::channel::ChannelKind::Telegram),
        )
        .expect("envelope");
        assert_eq!(
            envelope.channel_origin,
            Some(crate::channel::ChannelKind::Telegram),
            "#3324: both envelope constructors must carry the origin"
        );
        let internal = self_kick_envelope_for_mode(
            &home,
            "claude-agent",
            "[AGEND-RESUME] x",
            TransportMode::LegacyPty,
            None,
        )
        .expect("envelope");
        assert_eq!(
            internal.channel_origin, None,
            "#3324: and the daemon-composed self-kick stays internal"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    fn registry_test_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-registry-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n",
        )
        .expect("fleet");
        home
    }

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
        let result = deliver_notification(&home, "codex-agent", "hello", None, move |_, _, _| {
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
        let result = deliver_notification(&home, "codex-agent", "hello", None, move |_, _, _| {
            pty_calls_for_injector.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(pty_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn persisted_structured_mode_does_not_downgrade_when_fleet_is_unreadable() {
        for (tag, fleet_contents) in [
            ("missing", None),
            ("corrupt", Some("instances: [")),
            (
                "stale",
                Some("instances:\n  codex-agent:\n    backend: claude\n"),
            ),
        ] {
            let home = std::env::temp_dir().join(format!(
                "agend-transport-persisted-mode-{tag}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(home.join("transport/sessions")).expect("home");
            if let Some(contents) = fleet_contents {
                std::fs::write(crate::fleet::fleet_yaml_path(&home), contents).expect("fleet");
            }
            std::fs::write(
                session_path(&home, "codex-agent"),
                serde_json::to_vec(&SessionLocator::codex(
                    PathBuf::from("/tmp/missing-codex.sock"),
                    Some("thread".to_string()),
                ))
                .expect("locator"),
            )
            .expect("session locator");
            let pty_calls = Arc::new(AtomicUsize::new(0));
            let pty_calls_for_injector = Arc::clone(&pty_calls);
            let result =
                deliver_notification(&home, "codex-agent", "hello", None, move |_, _, _| {
                    pty_calls_for_injector.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            assert!(
                result.is_err(),
                "structured resolution must fail closed: {tag}"
            );
            assert_eq!(
                pty_calls.load(Ordering::SeqCst),
                0,
                "PTY fallback for {tag}"
            );
            let _ = std::fs::remove_dir_all(home);
        }
    }

    #[test]
    fn malformed_persisted_locator_keeps_structured_mode_when_fleet_is_missing() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-malformed-mode-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(home.join("transport/sessions")).expect("home");
        std::fs::write(session_path(&home, "codex-agent"), b"{not valid json")
            .expect("corrupt locator");
        let pty_calls = Arc::new(AtomicUsize::new(0));
        let pty_calls_for_injector = Arc::clone(&pty_calls);
        let result = deliver_notification(&home, "codex-agent", "hello", None, move |_, _, _| {
            pty_calls_for_injector.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(pty_calls.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn opencode_model_arg_contract_matches_declared_backend_policy() {
        assert_eq!(
            parse_opencode_model_args(&["--model=anthropic/opus".to_string()])
                .expect("long model")
                .model,
            Some("anthropic/opus".to_string())
        );
        assert_eq!(
            parse_opencode_model_args(&["-m".to_string(), "openai/gpt-5".to_string()])
                .expect("short model")
                .model,
            Some("openai/gpt-5".to_string())
        );
        assert!(parse_opencode_model_args(&["-m=openai/gpt-5".to_string()]).is_err());
        assert!(parse_opencode_model_args(&["-mopenai/gpt-5".to_string()]).is_err());
        assert!(parse_opencode_model_args(&[
            "--model".to_string(),
            "--".to_string(),
            "payload".to_string(),
        ])
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn session_locator_and_parent_are_private_before_secret_is_persisted() {
        use std::os::unix::fs::PermissionsExt;

        let home = std::env::temp_dir().join(format!(
            "agend-transport-private-locator-{}",
            uuid::Uuid::new_v4()
        ));
        let locator = SessionLocator::opencode(
            "http://127.0.0.1:4096".to_string(),
            Some("session".to_string()),
            "opencode".to_string(),
            "secret".to_string(),
        );
        save_session_locator(&home, "agent", &locator).expect("save locator");
        let path = session_path(&home, "agent");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("locator metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().expect("parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
