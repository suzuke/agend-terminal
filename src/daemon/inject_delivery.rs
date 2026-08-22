//! #2044: inject-delivery verification — a safety net against an actionable
//! dispatch wake being SWALLOWED by an operator-driven interactive dialog
//! (the incident: a `/model` picker was open in the agent's pane, the injected
//! dispatch's keystrokes went to the picker, the prompt never submitted, and
//! the dispatch was lost — discovered only because the operator noticed the
//! agent never reacted).
//!
//! Signal: a landed actionable inject submits a prompt → the backend fires a
//! `UserPromptSubmit` hook. A dialog-swallowed inject submits NOTHING → no
//! such hook. So: when an actionable wake is injected, record the time; if no
//! `UserPromptSubmit` is observed within [`VERIFY_WINDOW`], re-deliver ONCE and
//! WARN, then give up (latched — never a retry storm; noise discipline #2008).
//!
//! Per-backend honesty: this can only verify backends that emit hooks. The arm
//! is gated on the agent already having a hook-shadow entry (empirical proof
//! hooks flow for it — claude today). A non-hook backend never arms, so it can
//! never be falsely re-injected. The active timer is process-local, but
//! structured terminal latches and their bounded re-arm count are persisted by
//! durable row id. A daemon restart may lose an in-flight timer, but it cannot
//! turn a prior give-up into an unbounded retry storm; reclaim is the only path
//! that can consume the one persisted structured re-arm.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// No `UserPromptSubmit` within this wall-clock window after an actionable
/// inject ⇒ treat as not-delivered. 30s comfortably outlasts a normal
/// submit→hook round-trip while still reacting fast to a swallowed dispatch.
const VERIFY_WINDOW_MS: u64 = 30_000;
const MAX_STRUCTURED_REARMS: u8 = 1;

#[derive(Debug, Clone)]
struct Pending {
    /// Human agent name used for hook-shadow and transport admission checks.
    agent: String,
    /// When the (most recent) actionable wake was injected (epoch ms).
    injected_at_ms: u64,
    /// The wake text, re-injected verbatim on the one re-delivery attempt.
    text: String,
    /// #3324: the typed external-channel provenance the original wake was
    /// admitted with. A re-delivery is the SAME logical delivery, so it must
    /// carry the same origin: dropping it to `None` would hand the re-delivered
    /// copy the permissive (internal) classification the reply guard keys on.
    channel_origin: Option<crate::channel::ChannelKind>,
    /// True once the single re-delivery has fired (the latch).
    redelivered: bool,
    /// Transport generation that admitted the original actionable wake.
    /// Re-delivery must be admitted against this exact generation so a
    /// delete/redeploy cannot route the stale wake to a successor.
    transport_epoch: u64,
    /// Registry-selected route. Structured routes retain one explicit
    /// re-armable give-up state; LegacyPty keeps the historical dialog guard.
    transport_mode: crate::transport::TransportMode,
    gave_up: bool,
    rearm_count: u8,
    rearm_reserved: bool,
    rearm_pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableLatch {
    agent: String,
    row_id: String,
    text: String,
    /// #3324: provenance survives the restart hop too — a latch reloaded after
    /// a daemon restart rebuilds `Pending`, and an absent field there would
    /// silently downgrade a channel wake to internal. `serde default` keeps
    /// pre-#3324 latch files loading as internal, which is what they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel_origin: Option<crate::channel::ChannelKind>,
    transport_mode: crate::transport::TransportMode,
    #[serde(default)]
    transport_epoch: u64,
    #[serde(default)]
    rearm_pending: bool,
    gave_up: bool,
    rearm_count: u8,
}

pub(crate) struct PreparedArm {
    row_id: String,
    agent: String,
    text: String,
    /// #3324: carried from the admission call so the committed `Pending` keeps
    /// the origin the delivery was actually admitted with.
    channel_origin: Option<crate::channel::ChannelKind>,
    transport_mode: crate::transport::TransportMode,
    durable_rearm_count: u8,
    rearm_pending: bool,
}

fn store() -> &'static Mutex<HashMap<String, Pending>> {
    static S: std::sync::OnceLock<Mutex<HashMap<String, Pending>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    crate::daemon::heartbeat_pair::now_ms()
}

fn latch_path(home: &Path, agent: &str) -> PathBuf {
    home.join("transport")
        .join("verification")
        .join(format!("{}.json", crate::transport::safe_component(agent)))
}

fn latch_lock_path(home: &Path, agent: &str) -> PathBuf {
    latch_path(home, agent).with_extension("json.lock")
}

/// Remove the row-keyed verification state as part of the existing
/// delete/redeploy transport cleanup fence. A successor with the same name
/// must not inherit a predecessor's bounded re-arm budget.
pub(crate) fn remove_durable_latches(home: &Path, agent: &str) -> anyhow::Result<()> {
    let path = latch_path(home, agent);
    let lock_path = latch_lock_path(home, agent);
    {
        let _lock = crate::store::acquire_file_lock(&lock_path)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Keep the lock held while removing the data, then drop it before
        // unlinking the sidecar. Windows rejects removing an open lock file.
    }
    match std::fs::remove_file(&lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn durable_latch_state_exists(home: &Path, agent: &str) -> bool {
    latch_path(home, agent).exists() || latch_lock_path(home, agent).exists()
}

fn load_latches_unlocked(home: &Path, agent: &str) -> Option<Vec<DurableLatch>> {
    let path = latch_path(home, agent);
    if !path.exists() {
        return Some(Vec::new());
    }
    match std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<DurableLatch>>(&bytes).ok())
    {
        Some(latches) => Some(latches),
        None => {
            tracing::error!(
                agent = %agent,
                path = %path.display(),
                "#2044 durable verification latch is unreadable; refusing to re-arm"
            );
            None
        }
    }
}

fn write_latches_unlocked(
    home: &Path,
    agent: &str,
    latches: &[DurableLatch],
) -> anyhow::Result<()> {
    let path = latch_path(home, agent);
    let bytes = serde_json::to_vec(latches)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::store::atomic_write(&path, &bytes)
}

fn load_latches_for_epoch(
    home: &Path,
    agent: &str,
    transport_epoch: u64,
) -> Option<Vec<DurableLatch>> {
    #[cfg(test)]
    test_support::run_load_before_lock_hook(home, agent, transport_epoch);
    let lock_path = latch_lock_path(home, agent);
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                agent = %agent,
                path = %lock_path.display(),
                error = %error,
                "#2044 durable verification latch lock unavailable; refusing to re-arm"
            );
            return None;
        }
    };
    if crate::daemon::delivery_worker::current_transport_epoch(home, agent) != transport_epoch {
        return None;
    }
    let mut latches = load_latches_unlocked(home, agent)?;
    let original_len = latches.len();
    latches.retain(|latch| latch.transport_epoch == transport_epoch);
    if latches.len() != original_len {
        if let Err(error) = write_latches_unlocked(home, agent, &latches) {
            tracing::error!(
                agent = %agent,
                error = %error,
                "#2044 failed to purge stale transport-generation latches"
            );
            return None;
        }
    }
    Some(latches)
}

fn persist_latch(home: &Path, row_id: &str, pending: &Pending) -> anyhow::Result<bool> {
    if pending.transport_mode == crate::transport::TransportMode::LegacyPty {
        return Ok(true);
    }
    if crate::daemon::delivery_worker::current_transport_epoch(home, &pending.agent)
        != pending.transport_epoch
    {
        return Ok(false);
    }
    #[cfg(test)]
    test_support::run_persist_before_lock_hook(home, &pending.agent, pending.transport_epoch);
    let _lock = crate::store::acquire_file_lock(&latch_lock_path(home, &pending.agent))?;
    // The verifier may have been delayed while cleanup advanced the generation.
    // Re-check after taking the file lock so an old writer cannot resurrect a
    // predecessor latch after teardown.
    if crate::daemon::delivery_worker::current_transport_epoch(home, &pending.agent)
        != pending.transport_epoch
    {
        return Ok(false);
    }
    let mut latches = load_latches_unlocked(home, &pending.agent)
        .ok_or_else(|| anyhow::anyhow!("durable verification latch is unreadable"))?;
    latches.retain(|latch| latch.row_id != row_id);
    if pending.gave_up || pending.rearm_count > 0 {
        latches.push(DurableLatch {
            agent: pending.agent.clone(),
            row_id: row_id.to_string(),
            text: pending.text.clone(),
            // RED: nor does the durable latch.
            channel_origin: None,
            transport_mode: pending.transport_mode,
            transport_epoch: pending.transport_epoch,
            rearm_pending: pending.rearm_pending,
            gave_up: pending.gave_up,
            rearm_count: pending.rearm_count,
        });
    }
    let result = write_latches_unlocked(home, &pending.agent, &latches).map(|()| true);
    #[cfg(test)]
    if matches!(result, Ok(true)) {
        test_support::run_persist_after_write_hook(home, &pending.agent, pending.transport_epoch);
    }
    result
}

pub(crate) fn notify_arm_committed(agent: &str) {
    #[cfg(test)]
    test_support::run_arm_hook(agent);
    #[cfg(not(test))]
    let _ = agent;
}

/// Prepare the filesystem-backed part of an actionable verification arm.
/// Callers that also own a transport-generation mutex must do this before
/// acquiring that mutex; the commit phase is memory-only and epoch-checked.
pub(crate) fn prepare_arm(
    home: &Path,
    agent: &str,
    text: &str,
    transport_mode: crate::transport::TransportMode,
    transport_epoch: u64,
    channel_origin: Option<crate::channel::ChannelKind>,
) -> Option<PreparedArm> {
    crate::daemon::hook_shadow::snapshot_for(agent)?;
    let row_id = logical_row_id(agent, text);
    let latches = load_latches_for_epoch(home, agent, transport_epoch)?;
    let durable_latch = latches.into_iter().find(|latch| latch.row_id == row_id);
    let deferred_rearm = durable_latch.as_ref().is_some_and(|latch| {
        latch.rearm_pending
            && store()
                .lock()
                .get(&row_id)
                .is_some_and(|pending| pending.rearm_reserved && pending.rearm_pending)
    });
    if durable_latch
        .as_ref()
        .is_some_and(|latch| latch.gave_up && !deferred_rearm)
    {
        // A restart must preserve the terminal latch until the durable inbox
        // row is explicitly reclaimed.
        return None;
    }
    Some(PreparedArm {
        row_id,
        agent: agent.to_string(),
        text: text.to_string(),
        // RED: the arm accepts the origin and drops it.
        channel_origin: {
            let _ = channel_origin;
            None
        },
        transport_mode,
        durable_rearm_count: durable_latch.map_or(0, |latch| latch.rearm_count),
        rearm_pending: deferred_rearm,
    })
}

/// Commit a previously prepared arm without filesystem/configuration I/O.
/// The caller may hold the per-key transport-generation mutex while invoking
/// this function; the generation check itself belongs to that caller.
pub(crate) fn commit_prepared_arm(prepared: PreparedArm, transport_epoch: u64) -> bool {
    let mut guard = store().lock();
    let same_epoch = guard
        .get(&prepared.row_id)
        .is_some_and(|previous| previous.transport_epoch == transport_epoch);
    if let Some(previous) = guard.get(&prepared.row_id) {
        if same_epoch && previous.agent == prepared.agent && previous.gave_up {
            // A duplicate pointer after terminal give-up must not clear the
            // latch and create a retry storm. Reclaim calls the explicit
            // row-keyed re-arm below once the durable inbox row is eligible.
            return false;
        }
    }
    let rearm_count = if same_epoch {
        guard
            .get(&prepared.row_id)
            .map_or(prepared.durable_rearm_count, |pending| pending.rearm_count)
    } else {
        prepared.durable_rearm_count
    };
    let (rearm_reserved, rearm_pending) = if same_epoch {
        guard
            .get(&prepared.row_id)
            .map_or((false, prepared.rearm_pending), |pending| {
                (pending.rearm_reserved, pending.rearm_pending)
            })
    } else {
        (false, prepared.rearm_pending)
    };
    guard.insert(
        prepared.row_id,
        Pending {
            agent: prepared.agent,
            injected_at_ms: now_ms(),
            text: prepared.text,
            channel_origin: prepared.channel_origin,
            redelivered: false,
            transport_epoch,
            transport_mode: prepared.transport_mode,
            gave_up: false,
            rearm_count,
            rearm_reserved,
            rearm_pending,
        },
    );
    true
}

#[cfg(test)]
pub(crate) fn arm(agent: &str, text: &str) {
    arm_with_transport_epoch_for_test(agent, text, 0, crate::transport::TransportMode::LegacyPty);
}

#[cfg(test)]
fn arm_with_transport_epoch_for_test(
    agent: &str,
    text: &str,
    transport_epoch: u64,
    transport_mode: crate::transport::TransportMode,
) {
    if crate::daemon::hook_shadow::snapshot_for(agent).is_none() {
        return;
    }
    let row_id = logical_row_id(agent, text);
    store().lock().insert(
        row_id,
        Pending {
            agent: agent.to_string(),
            injected_at_ms: now_ms(),
            text: text.to_string(),
            channel_origin: None,
            redelivered: false,
            transport_epoch,
            transport_mode,
            gave_up: false,
            rearm_count: 0,
            rearm_reserved: false,
            rearm_pending: false,
        },
    );
    test_support::run_arm_hook(agent);
}

/// Forget any pending verification for a deleted agent so a same-name
/// redeploy cannot inherit a stale actionable wake.
pub(crate) fn forget(agent: &str) {
    store().lock().retain(|_, pending| pending.agent != agent);
}

#[cfg(test)]
pub(crate) fn is_armed_for_test(agent: &str) -> bool {
    store()
        .lock()
        .values()
        .any(|pending| pending.agent == agent)
}

#[cfg(test)]
pub(crate) fn rearm_state_for_test(agent: &str, row_id: &str) -> Option<(bool, bool, bool, u8)> {
    store().lock().get(row_id).and_then(|pending| {
        (pending.agent == agent).then_some((
            pending.gave_up,
            pending.rearm_reserved,
            pending.rearm_pending,
            pending.rearm_count,
        ))
    })
}

#[cfg(test)]
pub(crate) fn clear_for_test(agent: &str) {
    forget(agent);
}

/// Test-only production-entry fixture: persist a terminal structured latch and
/// leave the process-local map empty so reclaim must exercise the durable path.
#[cfg(test)]
pub(crate) fn seed_structured_terminal_for_test(
    home: &Path,
    agent: &str,
    row_id: &str,
    text: &str,
) {
    let pending = Pending {
        agent: agent.to_string(),
        injected_at_ms: now_ms(),
        text: text.to_string(),
        channel_origin: None,
        redelivered: true,
        transport_epoch: crate::daemon::delivery_worker::current_transport_epoch(home, agent),
        transport_mode: crate::transport::TransportMode::ChannelBridge,
        gave_up: true,
        rearm_count: 0,
        rearm_reserved: false,
        rearm_pending: false,
    };
    persist_latch(home, row_id, &pending).expect("seed terminal structured latch");
    forget(agent);
}

fn logical_row_id(agent: &str, text: &str) -> String {
    crate::daemon::notification_dedup::extract_msg_id_from_header(text)
        .unwrap_or_else(|| format!("agent:{agent}"))
}

/// A row-keyed, in-process reservation for the one structured re-arm budget.
/// The reservation is deliberately not persisted until routing reports a
/// durable defer or accepted transport admission.
#[derive(Debug, Clone)]
pub(crate) struct RearmReservation {
    agent: String,
    row_id: String,
    transport_epoch: u64,
    deferred: bool,
}

/// Reserve a structured re-arm without consuming its durable budget. The
/// in-memory `rearm_reserved` bit makes concurrent reclaim sweeps idempotent;
/// the durable terminal latch remains unchanged until commit.
pub(crate) fn reserve_rearm_after_reclaim(
    home: &Path,
    agent: &str,
    row_id: &str,
) -> Option<RearmReservation> {
    let transport_epoch = crate::daemon::delivery_worker::current_transport_epoch(home, agent);
    {
        let mut guard = store().lock();
        if let Some(pending) = guard.get_mut(row_id) {
            if pending.transport_epoch != transport_epoch {
                guard.remove(row_id);
            } else {
                if pending.agent != agent
                    || !pending.gave_up
                    || pending.rearm_pending
                    || pending.rearm_reserved
                    || pending.transport_mode == crate::transport::TransportMode::LegacyPty
                    || pending.rearm_count >= MAX_STRUCTURED_REARMS
                {
                    return None;
                }
                pending.gave_up = false;
                pending.rearm_reserved = true;
                pending.redelivered = false;
                pending.injected_at_ms = now_ms();
                pending.transport_epoch = transport_epoch;
                return Some(RearmReservation {
                    agent: agent.to_string(),
                    row_id: row_id.to_string(),
                    transport_epoch,
                    deferred: false,
                });
            }
        }
    }

    let latches = load_latches_for_epoch(home, agent, transport_epoch)?;
    let latch = latches.into_iter().find(|latch| latch.row_id == row_id)?;
    if latch.agent != agent
        || !latch.gave_up
        || latch.rearm_pending
        || latch.transport_mode == crate::transport::TransportMode::LegacyPty
        || latch.rearm_count >= MAX_STRUCTURED_REARMS
    {
        return None;
    }
    let pending = Pending {
        agent: agent.to_string(),
        injected_at_ms: now_ms(),
        text: latch.text,
        channel_origin: latch.channel_origin,
        redelivered: false,
        transport_epoch,
        transport_mode: latch.transport_mode,
        gave_up: false,
        rearm_count: latch.rearm_count,
        rearm_reserved: true,
        rearm_pending: false,
    };
    let mut guard = store().lock();
    if guard.contains_key(row_id) {
        return None;
    }
    guard.insert(row_id.to_string(), pending);
    Some(RearmReservation {
        agent: agent.to_string(),
        row_id: row_id.to_string(),
        transport_epoch,
        deferred: false,
    })
}

/// Keep a reclaimed row's sole structured re-arm budget terminal while its
/// pointer waits in the deferred notification queue. The flush path promotes
/// this marker back to an in-memory reservation only when physical transport
/// admission is about to begin.
pub(crate) fn defer_rearm_after_reclaim(home: &Path, reservation: &RearmReservation) -> bool {
    let updated = {
        let guard = store().lock();
        let Some(pending) = guard.get(&reservation.row_id) else {
            return false;
        };
        if pending.agent != reservation.agent
            || pending.transport_epoch != reservation.transport_epoch
            || !pending.rearm_reserved
            || reservation.deferred
        {
            return false;
        }
        let mut updated = pending.clone();
        updated.gave_up = true;
        updated.rearm_reserved = false;
        updated.rearm_pending = true;
        updated
    };
    match persist_latch(home, &reservation.row_id, &updated) {
        Ok(true) => {}
        Ok(false) | Err(_) => return false,
    }
    let mut guard = store().lock();
    let Some(current) = guard.get_mut(&reservation.row_id) else {
        return false;
    };
    if current.agent != reservation.agent
        || current.transport_epoch != reservation.transport_epoch
        || !current.rearm_reserved
    {
        return false;
    }
    *current = updated;
    true
}

/// Promote a durable deferred re-arm when the queue flusher has claimed its
/// pointer. This does not consume the budget; the caller commits only after
/// the transport scheduler accepts the pointer and verification is armed.
pub(crate) fn take_deferred_rearm_for_flush(
    home: &Path,
    agent: &str,
    row_id: &str,
) -> Option<RearmReservation> {
    let transport_epoch = crate::daemon::delivery_worker::current_transport_epoch(home, agent);
    let mut guard = store().lock();
    if guard
        .get(row_id)
        .is_some_and(|pending| pending.transport_epoch != transport_epoch)
    {
        guard.remove(row_id);
        return None;
    }
    if let Some(pending) = guard.get_mut(row_id) {
        if pending.agent != agent
            || !pending.rearm_pending
            || pending.rearm_reserved
            || pending.rearm_count >= MAX_STRUCTURED_REARMS
        {
            return None;
        }
        pending.gave_up = false;
        pending.rearm_reserved = true;
        return Some(RearmReservation {
            agent: agent.to_string(),
            row_id: row_id.to_string(),
            transport_epoch,
            deferred: true,
        });
    }
    drop(guard);

    let latch = load_latches_for_epoch(home, agent, transport_epoch)?
        .into_iter()
        .find(|latch| latch.row_id == row_id && latch.agent == agent && latch.rearm_pending)?;
    if latch.rearm_count >= MAX_STRUCTURED_REARMS {
        return None;
    }
    let pending = Pending {
        agent: agent.to_string(),
        injected_at_ms: now_ms(),
        text: latch.text,
        channel_origin: latch.channel_origin,
        redelivered: false,
        transport_epoch,
        transport_mode: latch.transport_mode,
        gave_up: false,
        rearm_count: latch.rearm_count,
        rearm_reserved: true,
        rearm_pending: true,
    };
    let mut guard = store().lock();
    if guard.contains_key(row_id) {
        return None;
    }
    guard.insert(row_id.to_string(), pending);
    Some(RearmReservation {
        agent: agent.to_string(),
        row_id: row_id.to_string(),
        transport_epoch,
        deferred: true,
    })
}

/// Commit a reserved re-arm after its pointer was durably deferred or admitted
/// to the transport scheduler. Filesystem I/O occurs outside the process-local
/// verification mutex; the final memory update is a reservation CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RearmCommitOutcome {
    /// The durable budget was not written; callers may cancel the exact arm
    /// and requeue the pointer for a later attempt.
    NotCommitted,
    /// The durable budget was written, but the process-local row disappeared
    /// or changed before the final CAS (for example, a prompt confirmation or
    /// cleanup won the race). Physical admission is already truthful, so the
    /// pointer must not be requeued.
    DurableCommitted,
    /// Both durable persistence and the process-local CAS completed.
    Committed,
}

pub(crate) fn commit_rearm_after_reclaim_outcome(
    home: &Path,
    reservation: &RearmReservation,
) -> RearmCommitOutcome {
    let updated = {
        let guard = store().lock();
        let Some(pending) = guard.get(&reservation.row_id) else {
            return RearmCommitOutcome::NotCommitted;
        };
        if pending.agent != reservation.agent
            || pending.transport_epoch != reservation.transport_epoch
            || !pending.rearm_reserved
        {
            return RearmCommitOutcome::NotCommitted;
        }
        let mut updated = pending.clone();
        updated.gave_up = false;
        updated.rearm_reserved = false;
        updated.rearm_count = updated.rearm_count.saturating_add(1);
        updated.rearm_pending = false;
        updated
    };
    match persist_latch(home, &reservation.row_id, &updated) {
        Ok(true) => {}
        Ok(false) => return RearmCommitOutcome::NotCommitted,
        Err(error) => {
            tracing::error!(
                agent = %reservation.agent,
                row_id = %reservation.row_id,
                error = %error,
                "#2044 failed to persist committed structured re-arm"
            );
            return RearmCommitOutcome::NotCommitted;
        }
    }
    let mut guard = store().lock();
    let Some(current) = guard.get_mut(&reservation.row_id) else {
        return RearmCommitOutcome::DurableCommitted;
    };
    if current.agent != reservation.agent
        || current.transport_epoch != reservation.transport_epoch
        || !current.rearm_reserved
    {
        return RearmCommitOutcome::DurableCommitted;
    }
    *current = updated;
    RearmCommitOutcome::Committed
}

pub(crate) fn commit_rearm_after_reclaim(home: &Path, reservation: &RearmReservation) -> bool {
    !matches!(
        commit_rearm_after_reclaim_outcome(home, reservation),
        RearmCommitOutcome::NotCommitted
    )
}

/// Restore a reserved re-arm to its durable terminal state after routing was
/// rejected or fenced. This keeps the sole budget available for a later
/// reclaim while preserving duplicate-sweep idempotency.
pub(crate) fn rollback_rearm_after_reclaim(home: &Path, reservation: &RearmReservation) -> bool {
    let restored = {
        let guard = store().lock();
        let Some(pending) = guard.get(&reservation.row_id) else {
            return false;
        };
        if pending.agent != reservation.agent
            || pending.transport_epoch != reservation.transport_epoch
            || !pending.rearm_reserved
        {
            return false;
        }
        let mut restored = pending.clone();
        restored.gave_up = true;
        restored.rearm_reserved = false;
        restored.rearm_pending = reservation.deferred;
        restored
    };
    match persist_latch(home, &reservation.row_id, &restored) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::error!(
                agent = %reservation.agent,
                row_id = %reservation.row_id,
                error = %error,
                "#2044 failed to persist rolled-back structured re-arm"
            );
            return false;
        }
    }
    let mut guard = store().lock();
    let Some(current) = guard.get_mut(&reservation.row_id) else {
        return false;
    };
    if current.agent != reservation.agent
        || current.transport_epoch != reservation.transport_epoch
        || !current.rearm_reserved
    {
        return false;
    }
    *current = restored;
    true
}

/// Cancel only the verifier arm created for a reserved re-arm when its
/// durable budget commit loses the epoch/persistence race. The notification
/// flusher may requeue the pointer after this failure, so leaving this exact
/// row armed would permit a duplicate verifier redelivery.
pub(crate) fn cancel_rearm_arm(reservation: &RearmReservation) -> bool {
    let mut guard = store().lock();
    let Some(pending) = guard.get(&reservation.row_id) else {
        return false;
    };
    if pending.agent != reservation.agent
        || pending.transport_epoch != reservation.transport_epoch
        || !pending.rearm_reserved
        || !pending.rearm_pending
    {
        return false;
    }
    guard.remove(&reservation.row_id).is_some()
}

/// Compatibility helper for callers/tests that only need the old boolean
/// contract; production reclaim uses the explicit reserve/route/commit path.
#[cfg(test)]
pub(crate) fn rearm_after_reclaim(home: &Path, agent: &str, row_id: &str) -> bool {
    let Some(reservation) = reserve_rearm_after_reclaim(home, agent, row_id) else {
        return false;
    };
    commit_rearm_after_reclaim(home, &reservation)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::sync::OnceLock;

    pub(crate) type ArmHook = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
    pub(crate) type VerifyBeforeRedeliveryHook =
        std::sync::Arc<dyn Fn(&Path, &str, u64) + Send + Sync>;
    pub(crate) type PersistBeforeLockHook = std::sync::Arc<dyn Fn(&Path, &str, u64) + Send + Sync>;
    pub(crate) type PersistAfterWriteHook = std::sync::Arc<dyn Fn(&Path, &str, u64) + Send + Sync>;
    pub(crate) type LoadBeforeLockHook = std::sync::Arc<dyn Fn(&Path, &str, u64) + Send + Sync>;

    static ARM_HOOK: OnceLock<parking_lot::Mutex<Option<ArmHook>>> = OnceLock::new();
    static VERIFY_BEFORE_REDELIVERY_HOOK: OnceLock<
        parking_lot::Mutex<Option<VerifyBeforeRedeliveryHook>>,
    > = OnceLock::new();
    static ARM_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static VERIFY_BEFORE_REDELIVERY_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static PERSIST_BEFORE_LOCK_HOOK: OnceLock<parking_lot::Mutex<Option<PersistBeforeLockHook>>> =
        OnceLock::new();
    static PERSIST_BEFORE_LOCK_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static PERSIST_AFTER_WRITE_HOOK: OnceLock<parking_lot::Mutex<Option<PersistAfterWriteHook>>> =
        OnceLock::new();
    static PERSIST_AFTER_WRITE_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static LOAD_BEFORE_LOCK_HOOK: OnceLock<parking_lot::Mutex<Option<LoadBeforeLockHook>>> =
        OnceLock::new();
    static LOAD_BEFORE_LOCK_HOOK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static TEST_GUARD: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    pub(crate) fn test_guard() -> parking_lot::MutexGuard<'static, ()> {
        TEST_GUARD.lock()
    }

    pub(crate) struct ArmHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn arm_hook_guard() -> ArmHookGuard {
        let lock = ARM_HOOK_LOCK.lock();
        set_arm_hook(None);
        ArmHookGuard { _lock: lock }
    }

    pub(crate) fn set_arm_hook(hook: Option<ArmHook>) {
        *ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for ArmHookGuard {
        fn drop(&mut self) {
            set_arm_hook(None);
        }
    }

    pub(crate) struct VerifyBeforeRedeliveryHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn verify_before_redelivery_hook_guard() -> VerifyBeforeRedeliveryHookGuard {
        let lock = VERIFY_BEFORE_REDELIVERY_HOOK_LOCK.lock();
        set_verify_before_redelivery_hook(None);
        VerifyBeforeRedeliveryHookGuard { _lock: lock }
    }

    pub(crate) fn set_verify_before_redelivery_hook(hook: Option<VerifyBeforeRedeliveryHook>) {
        *VERIFY_BEFORE_REDELIVERY_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for VerifyBeforeRedeliveryHookGuard {
        fn drop(&mut self) {
            set_verify_before_redelivery_hook(None);
        }
    }

    pub(crate) struct PersistBeforeLockHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn persist_before_lock_hook_guard() -> PersistBeforeLockHookGuard {
        let lock = PERSIST_BEFORE_LOCK_HOOK_LOCK.lock();
        set_persist_before_lock_hook(None);
        PersistBeforeLockHookGuard { _lock: lock }
    }

    pub(crate) fn set_persist_before_lock_hook(hook: Option<PersistBeforeLockHook>) {
        *PERSIST_BEFORE_LOCK_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for PersistBeforeLockHookGuard {
        fn drop(&mut self) {
            set_persist_before_lock_hook(None);
        }
    }

    pub(crate) struct PersistAfterWriteHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn persist_after_write_hook_guard() -> PersistAfterWriteHookGuard {
        let lock = PERSIST_AFTER_WRITE_HOOK_LOCK.lock();
        set_persist_after_write_hook(None);
        PersistAfterWriteHookGuard { _lock: lock }
    }

    pub(crate) fn set_persist_after_write_hook(hook: Option<PersistAfterWriteHook>) {
        *PERSIST_AFTER_WRITE_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for PersistAfterWriteHookGuard {
        fn drop(&mut self) {
            set_persist_after_write_hook(None);
        }
    }

    pub(crate) struct LoadBeforeLockHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn load_before_lock_hook_guard() -> LoadBeforeLockHookGuard {
        let lock = LOAD_BEFORE_LOCK_HOOK_LOCK.lock();
        set_load_before_lock_hook(None);
        LoadBeforeLockHookGuard { _lock: lock }
    }

    pub(crate) fn set_load_before_lock_hook(hook: Option<LoadBeforeLockHook>) {
        *LOAD_BEFORE_LOCK_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    impl Drop for LoadBeforeLockHookGuard {
        fn drop(&mut self) {
            set_load_before_lock_hook(None);
        }
    }

    pub(super) fn run_arm_hook(agent: &str) {
        let hook = ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(agent);
        }
    }

    pub(super) fn run_verify_before_redelivery_hook(home: &Path, agent: &str, epoch: u64) {
        let hook = VERIFY_BEFORE_REDELIVERY_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }

    pub(super) fn run_persist_before_lock_hook(home: &Path, agent: &str, epoch: u64) {
        let hook = PERSIST_BEFORE_LOCK_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }

    pub(super) fn run_persist_after_write_hook(home: &Path, agent: &str, epoch: u64) {
        let hook = PERSIST_AFTER_WRITE_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }

    pub(super) fn run_load_before_lock_hook(home: &Path, agent: &str, epoch: u64) {
        let hook = LOAD_BEFORE_LOCK_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }
}

/// Per-tick verification pass. For each armed agent:
/// - a `UserPromptSubmit` recorded AFTER the inject ⇒ delivered, clear silently.
/// - else past [`VERIFY_WINDOW_MS`] and not yet re-delivered ⇒ re-inject once,
///   WARN, latch (reset the clock so the re-delivery gets its own window).
/// - else past the window AND already re-delivered ⇒ final WARN, give up.
pub(crate) fn verify_pass(home: &Path) {
    let now = now_ms();
    // Decide under the lock, act (re-inject) after dropping it — the inject is a
    // self-IPC vector (#1492) and must not run while holding our mutex.
    let mut to_redeliver: Vec<(String, String, u64, Option<crate::channel::ChannelKind>)> =
        Vec::new();
    let mut gave_up: Vec<(String, String)> = Vec::new();
    let mut latches_to_persist: Vec<(String, Pending)> = Vec::new();
    {
        let mut guard = store().lock();
        guard.retain(|row_id, p| {
            let ups = crate::daemon::hook_shadow::last_user_prompt_submit_for(&p.agent);
            if ups.is_some_and(|t| t > p.injected_at_ms) {
                return false; // delivered — drop silently
            }
            if now.saturating_sub(p.injected_at_ms) < VERIFY_WINDOW_MS {
                return true; // still inside the window — keep waiting
            }
            if !p.redelivered {
                to_redeliver.push((
                    p.agent.clone(),
                    p.text.clone(),
                    p.transport_epoch,
                    p.channel_origin,
                ));
                p.redelivered = true;
                p.injected_at_ms = now; // fresh window for the re-delivery
                true
            } else {
                if p.transport_mode == crate::transport::TransportMode::LegacyPty {
                    gave_up.push((p.agent.clone(), row_id.clone()));
                    false // LegacyPty give-up remains terminal — no storm.
                } else {
                    // Structured transports retain a durable-row keyed latch
                    // so an inbox reclaim can make exactly one bounded re-arm.
                    p.gave_up = true;
                    latches_to_persist.push((row_id.clone(), p.clone()));
                    true
                }
            }
        });
    }
    for (row_id, pending) in latches_to_persist {
        if let Err(error) = persist_latch(home, &row_id, &pending) {
            tracing::error!(
                agent = %pending.agent,
                row_id = %row_id,
                error = %error,
                "#2044 failed to persist structured verification latch"
            );
        }
    }
    for (agent, text, transport_epoch, channel_origin) in to_redeliver {
        // Re-inject via the plain submit path — NOT compose_aware_inject — so the
        // re-delivery does not re-arm verification (the latch lives in `Pending`).
        #[cfg(test)]
        test_support::run_verify_before_redelivery_hook(home, &agent, transport_epoch);
        let result = crate::inbox::notify::inject_notification_with_submit_at_epoch(
            home,
            &agent,
            &text,
            transport_epoch,
            channel_origin,
        );
        match result {
            Ok(()) => {
                tracing::warn!(
                    agent = %agent,
                    tag = "#2044-inject-redeliver",
                    "actionable inject unconfirmed after {}s (no UserPromptSubmit) — re-delivering once \
                     (likely swallowed by an open interactive dialog)",
                    VERIFY_WINDOW_MS / 1000
                );
                crate::event_log::log(
                    home,
                    "inject_redelivered",
                    &agent,
                    "actionable inject unconfirmed (no UserPromptSubmit) — re-delivered once",
                );
            }
            Err(error) => {
                let fenced = error.to_string().contains("fenced");
                let (tag, kind, detail) = if fenced {
                    (
                        "#2044-inject-redeliver-suppressed",
                        "inject_redelivery_suppressed",
                        "redelivery admission fenced by a newer transport generation",
                    )
                } else {
                    (
                        "#2044-inject-redeliver-failed",
                        "inject_redelivery_failed",
                        "redelivery admission failed before adapter delivery",
                    )
                };
                tracing::warn!(agent = %agent, error = %error, tag, "{detail}");
                crate::event_log::log(home, kind, &agent, detail);
            }
        }
    }
    for (agent, row_id) in gave_up {
        tracing::warn!(
            agent = %agent,
            tag = "#2044-inject-undelivered",
            "re-delivered inject STILL unconfirmed after {}s — giving up (operator dialog may \
             still be open; check the pane)",
            VERIFY_WINDOW_MS / 1000
        );
        crate::event_log::log(
            home,
            "inject_undelivered",
            &agent,
            &format!(
                "re-delivered inject still unconfirmed — gave up row_id={row_id} (no retry storm)"
            ),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #2044 test isolation: these tests share the process-global `store()`
    /// AND drive `verify_pass`, which is a PRODUCTION whole-store pass
    /// (`retain` over every agent). Under plain `cargo test` (in-process
    /// parallel — the Coverage job's mode, run 27396184642), two tests'
    /// `verify_pass` calls interleave on the shared map and mutate each
    /// other's entries → the flaky `left:None right:Some(true)`. A unique
    /// agent name per test is NOT enough (verify_pass touches all agents), so
    /// serialize the whole group; nextest is unaffected (per-test process).
    fn test_guard() -> parking_lot::MutexGuard<'static, ()> {
        test_support::test_guard()
    }

    /// Remove ONLY this test's own agent (never a global wipe that would nuke
    /// a sibling's in-flight pending).
    fn forget(agent: &str) {
        super::forget(agent);
    }

    /// Test seam: arm with an EXPLICIT inject time so the verify window + the
    /// UserPromptSubmit ordering are deterministic (no clock-collision races).
    /// Bypasses the hook-history gate — the gate is covered separately.
    fn arm_at(agent: &str, text: &str, injected_at_ms: u64) {
        arm_at_with_epoch(agent, text, injected_at_ms, 0);
    }

    fn arm_at_with_epoch(agent: &str, text: &str, injected_at_ms: u64, transport_epoch: u64) {
        let row_id = logical_row_id(agent, text);
        store().lock().insert(
            row_id.clone(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms,
                text: text.to_string(),
                channel_origin: None,
                redelivered: false,
                transport_epoch,
                transport_mode: crate::transport::TransportMode::LegacyPty,
                gave_up: false,
                rearm_count: 0,
                rearm_reserved: false,
                rearm_pending: false,
            },
        );
    }

    fn pending_redelivered(agent: &str) -> Option<bool> {
        store()
            .lock()
            .values()
            .find(|pending| pending.agent == agent)
            .map(|p| p.redelivered)
    }

    /// #3324: the #2044 re-delivery is the SAME logical delivery as the wake it
    /// verifies, so the origin has to survive both hops it takes — the arm, and
    /// the durable latch a daemon restart reloads it from.
    ///
    /// It matters because the actionable classifier reads the notification
    /// TEXT: the inline inbound rendering embeds the sender's own words, so a
    /// channel message containing `kind=task ` is armed like any other
    /// actionable wake and reaches this path. Re-delivering it as `None` would
    /// hand the copy the permissive classification the reply guard keys on.
    #[test]
    fn the_2044_redelivery_path_preserves_channel_origin_3324() {
        let home = tmp_home("origin-carry-3324");
        let agent = "origin-carry-3324-agent";
        let text = "[user:alice] via telegram] please help kind=task ";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        crate::daemon::hook_shadow::record_event(agent, "UserPromptSubmit", None);
        let epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let row_id = logical_row_id(agent, text);

        let prepared = prepare_arm(
            &home,
            agent,
            text,
            crate::transport::TransportMode::ChannelBridge,
            epoch,
            Some(crate::channel::ChannelKind::Telegram),
        )
        .expect("arm");
        assert!(commit_prepared_arm(prepared, epoch));
        let armed = store().lock().get(&row_id).cloned().expect("armed row");
        assert_eq!(
            armed.channel_origin,
            Some(crate::channel::ChannelKind::Telegram),
            "#3324: the armed wake must remember the origin it was admitted with"
        );

        // Durable hop: persist the latch, drop the in-memory row the way a
        // restart would, then rebuild from disk.
        let mut latched = armed.clone();
        latched.gave_up = true;
        latched.rearm_pending = true;
        assert!(
            persist_latch(&home, &row_id, &latched).expect("persist latch"),
            "the latch must be written at the current epoch"
        );
        store().lock().remove(&row_id);

        take_deferred_rearm_for_flush(&home, agent, &row_id).expect("rebuild from latch");
        let rebuilt = store().lock().get(&row_id).cloned().expect("rebuilt row");
        assert_eq!(
            rebuilt.channel_origin,
            Some(crate::channel::ChannelKind::Telegram),
            "#3324: a latch reloaded after a restart must not downgrade the wake to internal"
        );

        forget(agent);
        let _ = std::fs::remove_dir_all(home);
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agend-2044-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).ok();
        d
    }

    /// Arm requires a hook-shadow entry — a non-hook backend (no entry) is
    /// never tracked, so it can never be falsely re-injected.
    #[test]
    fn arm_noop_without_hook_history() {
        let _g = test_guard();
        let agent = "no-hooks-2044";
        forget(agent);
        super::arm(agent, "wake");
        assert!(!is_armed_for_test(agent), "no hook history → not armed");
        forget(agent);
    }

    /// A UserPromptSubmit recorded AFTER the inject clears the pending silently
    /// — even when the window has elapsed (delivery beats the timeout).
    #[test]
    fn delivered_clears_without_redelivery() {
        let _g = test_guard();
        let home = tmp_home("delivered");
        let agent = "deliv-2044";
        forget(agent);
        let now = now_ms();
        let injected = now - VERIFY_WINDOW_MS - 1_000; // window already elapsed
        arm_at(agent, "wake", injected);
        // Agent submitted the prompt AFTER the inject.
        crate::daemon::hook_shadow::set_user_prompt_submit_for_test(agent, injected + 500);
        super::verify_pass(&home);
        assert!(
            !is_armed_for_test(agent),
            "UserPromptSubmit after inject ⇒ delivered, cleared (no re-delivery)"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// No UserPromptSubmit within the window ⇒ exactly one re-delivery, then
    /// (still unconfirmed) give up — never a storm.
    #[test]
    fn unconfirmed_redelivers_once_then_gives_up() {
        let _g = test_guard();
        let home = tmp_home("unconfirmed");
        let agent = "unconf-2044";
        forget(agent);
        let now = now_ms();
        // Fresh inject (inside the window) → no action yet.
        arm_at(agent, "wake", now);
        super::verify_pass(&home);
        assert_eq!(pending_redelivered(agent), Some(false), "still waiting");
        // Window elapsed, no UserPromptSubmit → re-deliver once (latch set).
        arm_at_elapsed(agent, "wake");
        super::verify_pass(&home);
        assert_eq!(
            pending_redelivered(agent),
            Some(true),
            "one re-delivery fired, latched"
        );
        // Window elapsed again, still no UserPromptSubmit → give up (cleared).
        store()
            .lock()
            .values_mut()
            .find(|pending| pending.agent == agent)
            .expect("pending")
            .injected_at_ms = now_ms() - VERIFY_WINDOW_MS - 1;
        super::verify_pass(&home);
        assert!(
            !is_armed_for_test(agent),
            "gave up after the single re-delivery — no storm"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// RED for #3303: verification state must follow the durable inbox row,
    /// not collapse every outstanding wake for an agent into one slot.
    #[test]
    fn distinct_durable_rows_have_independent_verification_latches() {
        let _g = test_guard();
        let agent = "row-keyed-2044";
        forget(agent);
        let now = now_ms();
        arm_at(
            agent,
            "[AGEND-MSG-PENDING] id=row-a kind=task from=lead inbox=1",
            now,
        );
        arm_at(
            agent,
            "[AGEND-MSG-PENDING] id=row-b kind=task from=lead inbox=1",
            now,
        );
        let guard = store().lock();
        assert!(guard.contains_key("row-a"), "row-a must have its own latch");
        assert!(guard.contains_key("row-b"), "row-b must have its own latch");
        drop(guard);
        forget(agent);
    }

    #[test]
    fn structured_give_up_rearm_is_bounded_and_cleanup_is_successor_safe() {
        let _g = test_guard();
        let home = tmp_home("structured-rearm");
        let agent = "structured-rearm-2044";
        let row_id = "row-structured";
        forget(agent);
        store().lock().insert(
            row_id.to_string(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms: now_ms() - VERIFY_WINDOW_MS - 1,
                text: format!("[AGEND-MSG-PENDING] id={row_id} kind=task from=lead inbox=1"),
                channel_origin: None,
                redelivered: true,
                transport_epoch: 0,
                transport_mode: crate::transport::TransportMode::ChannelBridge,
                gave_up: false,
                rearm_count: 0,
                rearm_reserved: false,
                rearm_pending: false,
            },
        );
        verify_pass(&home);
        assert!(
            store()
                .lock()
                .get(row_id)
                .is_some_and(|pending| pending.gave_up),
            "structured give-up must remain re-armable by durable row id"
        );
        // Simulate a daemon restart: the in-memory timer/latch disappears,
        // but the durable row-keyed give-up must still authorize one re-arm.
        store().lock().clear();
        assert!(rearm_after_reclaim(&home, agent, row_id));
        assert!(!rearm_after_reclaim(&home, agent, row_id));
        assert!(store()
            .lock()
            .get(row_id)
            .is_some_and(|pending| !pending.gave_up && pending.rearm_count == 1));
        assert!(latch_path(&home, agent).exists());
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup latches");
        store().lock().clear();
        assert!(!latch_path(&home, agent).exists());
        assert!(!rearm_after_reclaim(&home, agent, row_id));
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn reclaim_rearm_route_failure_rolls_back_and_duplicate_reserve_is_single() {
        let _g = test_guard();
        let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
        let home = tmp_home("rearm-transaction");
        let agent = "rearm-transaction-2044";
        let row_id = "row-rearm-transaction";
        forget(agent);
        store().lock().insert(
            row_id.to_string(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms: now_ms(),
                text: format!("[AGEND-MSG-PENDING] id={row_id} kind=task from=lead inbox=1"),
                channel_origin: None,
                redelivered: true,
                transport_epoch: 0,
                transport_mode: crate::transport::TransportMode::ChannelBridge,
                gave_up: true,
                rearm_count: 0,
                rearm_reserved: false,
                rearm_pending: false,
            },
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let barrier = std::sync::Arc::clone(&barrier);
            let worker_home = home.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                reserve_rearm_after_reclaim(&worker_home, agent, row_id)
            }));
        }
        let reservations: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("duplicate reclaim worker"))
            .collect();
        assert_eq!(reservations.iter().filter(|r| r.is_some()).count(), 1);
        let reservation = reservations
            .into_iter()
            .flatten()
            .next()
            .expect("one reservation");

        crate::daemon::delivery_worker::test_support::set_force_full(true);
        let routed =
            crate::inbox::notify::rearm_persisted_pointer(&home, agent, row_id, "task", "lead");
        assert_eq!(
            routed,
            crate::inbox::notify::ComposeInjectOutcome::Failed,
            "queue-full admission must be observable"
        );
        assert!(rollback_rearm_after_reclaim(&home, &reservation));
        assert!(store().lock().get(row_id).is_some_and(|pending| {
            pending.gave_up && !pending.rearm_reserved && pending.rearm_count == 0
        }));

        crate::daemon::delivery_worker::test_support::set_force_full(false);
        let reservation = reserve_rearm_after_reclaim(&home, agent, row_id).expect("retry reserve");
        let cleanup = crate::daemon::delivery_worker::begin_transport_cleanup(&home, agent);
        let fenced =
            crate::inbox::notify::rearm_persisted_pointer(&home, agent, row_id, "task", "lead");
        assert!(
            matches!(fenced, crate::inbox::notify::ComposeInjectOutcome::Failed),
            "cleanup fencing must reject pointer admission"
        );
        assert!(
            !rollback_rearm_after_reclaim(&home, &reservation),
            "a stale-generation rollback must not report persistence success"
        );
        drop(cleanup);
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// A UserPromptSubmit that PRE-dates the inject does NOT count as delivery
    /// (a stale earlier submit must not mask a swallowed new inject).
    #[test]
    fn stale_prior_user_prompt_submit_does_not_confirm() {
        let _g = test_guard();
        let home = tmp_home("stale-ups");
        let agent = "stale-2044";
        forget(agent);
        let now = now_ms();
        let injected = now - VERIFY_WINDOW_MS - 1_000; // window elapsed
                                                       // UserPromptSubmit BEFORE the inject (stale).
        crate::daemon::hook_shadow::set_user_prompt_submit_for_test(agent, injected - 5_000);
        arm_at(agent, "wake", injected);
        super::verify_pass(&home);
        assert_eq!(
            pending_redelivered(agent),
            Some(true),
            "the pre-inject UserPromptSubmit must not confirm the new inject"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// The verifier decides to redeliver outside its store lock. If destructive
    /// teardown wins in that gap, the original epoch must fence the later
    /// enqueue even after cleanup has made the name admissible again. This
    /// proves the stale verifier cannot call an adapter, create a receipt, or
    /// leave an arm behind for a same-name successor.
    #[test]
    fn stale_redelivery_after_delete_is_fenced_before_adapter_or_receipt() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _g = test_guard();
        let home = tmp_home("stale-redelivery-delete");
        let agent = "stale-redelivery-delete-2044";
        let _delivery_hook_guard = crate::transport::test_support::delivery_hook_guard();
        let _verify_hook_guard = test_support::verify_before_redelivery_hook_guard();
        forget(agent);
        let original_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let now = now_ms();
        arm_at_with_epoch(
            agent,
            "stale verifier wake",
            now - VERIFY_WINDOW_MS - 1,
            original_epoch,
        );

        let adapter_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let adapter_calls_hook = std::sync::Arc::clone(&adapter_calls);
        let expected_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
            move |called_home, called_agent, _body| {
                if called_home == expected_home.as_path() && called_agent == agent {
                    adapter_calls_hook.fetch_add(1, Ordering::SeqCst);
                    Some(Err(anyhow::anyhow!("stale verifier reached the adapter")))
                } else {
                    None
                }
            },
        )));

        let hook_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delete_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_ran_hook = std::sync::Arc::clone(&hook_ran);
        let delete_completed_hook = std::sync::Arc::clone(&delete_completed);
        let fence_home = home.clone();
        test_support::set_verify_before_redelivery_hook(Some(std::sync::Arc::new(
            move |hook_home, hook_agent, _epoch| {
                assert_eq!(hook_home, fence_home.as_path());
                assert_eq!(hook_agent, agent);
                hook_ran_hook.store(true, Ordering::SeqCst);
                let fence = crate::daemon::lifecycle::DeleteFence::new(hook_home, hook_agent, true);
                drop(fence);
                delete_completed_hook.store(true, Ordering::SeqCst);
            },
        )));

        verify_pass(&home);

        assert!(hook_ran.load(Ordering::SeqCst));
        assert!(delete_completed.load(Ordering::SeqCst));
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
        assert!(!is_armed_for_test(agent));
        assert!(
            !crate::transport::delivery_path_for_instance(&home, agent).exists(),
            "fenced verifier redelivery must not create a receipt"
        );
        assert!(
            !crate::transport::delivery_path_for_instance(&home, agent)
                .with_extension("jsonl.lock")
                .exists(),
            "fenced verifier redelivery must not create a receipt lock"
        );
        let event_log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(!event_log.contains("\"kind\":\"inject_redelivered\""));
        assert!(event_log.contains("inject_redelivery_suppressed"));
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn concurrent_structured_latch_persistence_keeps_both_rows() {
        let _g = test_guard();
        let home = tmp_home("concurrent-latches");
        let agent = "concurrent-latches-2044";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        let epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let pending = |row_id: &str| Pending {
            agent: agent.to_string(),
            injected_at_ms: now_ms(),
            text: format!("wake id={row_id}"),
            channel_origin: None,
            redelivered: true,
            transport_epoch: epoch,
            transport_mode: crate::transport::TransportMode::ChannelBridge,
            gave_up: true,
            rearm_count: 0,
            rearm_reserved: false,
            rearm_pending: false,
        };
        let home_a = home.clone();
        let pending_a = pending("m-latch-a");
        let thread_a = std::thread::spawn(move || {
            persist_latch(&home_a, "m-latch-a", &pending_a).expect("persist row a");
        });
        let home_b = home.clone();
        let pending_b = pending("m-latch-b");
        let thread_b = std::thread::spawn(move || {
            persist_latch(&home_b, "m-latch-b", &pending_b).expect("persist row b");
        });
        thread_a.join().expect("row a thread");
        thread_b.join().expect("row b thread");

        let latches = load_latches_for_epoch(&home, agent, epoch).expect("load latches");
        let row_ids: std::collections::HashSet<_> =
            latches.into_iter().map(|latch| latch.row_id).collect();
        assert_eq!(
            row_ids,
            ["m-latch-a", "m-latch-b"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_generation_persist_cannot_poison_same_name_successor() {
        let _g = test_guard();
        let home = tmp_home("stale-latch-generation");
        let agent = "stale-latch-generation-2044";
        let text = "[AGEND-MSG] id=m-stale-latch kind=task";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        let old_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        store().lock().insert(
            "m-stale-latch".to_string(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms: now_ms() - VERIFY_WINDOW_MS - 1,
                text: text.to_string(),
                channel_origin: None,
                redelivered: true,
                transport_epoch: old_epoch,
                transport_mode: crate::transport::TransportMode::ChannelBridge,
                gave_up: false,
                rearm_count: 0,
                rearm_reserved: false,
                rearm_pending: false,
            },
        );

        let _persist_guard = test_support::persist_before_lock_hook_guard();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let release_rx_hook = std::sync::Arc::clone(&release_rx);
        test_support::set_persist_before_lock_hook(Some(std::sync::Arc::new(
            move |_, hook_agent, _| {
                if hook_agent != agent {
                    return;
                }
                ready_tx.send(()).expect("persist hook ready receiver");
                release_rx_hook
                    .lock()
                    .expect("persist hook mutex")
                    .recv()
                    .expect("persist hook release");
            },
        )));
        let verify_home = home.clone();
        let verifier = std::thread::spawn(move || verify_pass(&verify_home));
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old-generation persist reaches barrier");

        let cleanup = crate::daemon::delivery_worker::begin_transport_cleanup(&home, agent);
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup state");
        drop(cleanup);
        crate::daemon::hook_shadow::record_event(agent, "UserPromptSubmit", None);
        let successor_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let prepared = prepare_arm(
            &home,
            agent,
            text,
            crate::transport::TransportMode::ChannelBridge,
            successor_epoch,
            None,
        )
        .expect("successor can arm after cleanup");
        assert!(commit_prepared_arm(prepared, successor_epoch));

        release_tx.send(()).expect("release old persist");
        verifier.join().expect("verifier thread");
        let stale_latch_bytes = std::fs::read(latch_path(&home, agent)).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&stale_latch_bytes).contains("m-stale-latch"),
            "post-lock epoch fence must prevent an old persist from recreating its latch"
        );
        assert!(
            prepare_arm(
                &home,
                agent,
                text,
                crate::transport::TransportMode::ChannelBridge,
                successor_epoch,
                None,
            )
            .is_some(),
            "a delayed predecessor persist must not recreate a terminal latch"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_generation_persist_rejects_before_lock_and_has_no_lock_side_effect() {
        let _g = test_guard();
        let home = tmp_home("stale-persist-before-lock");
        let agent = "stale-persist-before-lock-2044";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        let old_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let pending = Pending {
            agent: agent.to_string(),
            injected_at_ms: now_ms(),
            text: "wake id=m-stale-before-lock".to_string(),
            channel_origin: None,
            redelivered: true,
            transport_epoch: old_epoch,
            transport_mode: crate::transport::TransportMode::ChannelBridge,
            gave_up: true,
            rearm_count: 0,
            rearm_reserved: false,
            rearm_pending: false,
        };
        let cleanup = crate::daemon::delivery_worker::begin_transport_cleanup(&home, agent);
        drop(cleanup);

        let hook_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_called_by_hook = std::sync::Arc::clone(&hook_called);
        let _persist_guard = test_support::persist_before_lock_hook_guard();
        test_support::set_persist_before_lock_hook(Some(std::sync::Arc::new(
            move |_, hook_agent, _| {
                if hook_agent == agent {
                    hook_called_by_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            },
        )));

        assert!(matches!(
            persist_latch(&home, "m-stale-before-lock", &pending),
            Ok(false)
        ));
        assert!(
            !hook_called.load(std::sync::atomic::Ordering::SeqCst),
            "stale persistence must reject before entering the pre-lock seam"
        );
        assert!(
            !latch_lock_path(&home, agent).exists(),
            "stale persistence must not create a lock side effect"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cancel_rearm_arm_rejects_wrong_agent_and_epoch() {
        let _g = test_guard();
        let home = tmp_home("cancel-rearm-identity");
        let agent = "cancel-rearm-identity-2044";
        let row_id = "m-cancel-rearm-identity";
        forget(agent);
        let epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        store().lock().insert(
            row_id.to_string(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms: now_ms(),
                text: format!("wake id={row_id}"),
                channel_origin: None,
                redelivered: false,
                transport_epoch: epoch,
                transport_mode: crate::transport::TransportMode::ChannelBridge,
                gave_up: true,
                rearm_count: 0,
                rearm_reserved: true,
                rearm_pending: true,
            },
        );
        let wrong_agent = RearmReservation {
            agent: "successor-agent".to_string(),
            row_id: row_id.to_string(),
            transport_epoch: epoch,
            deferred: true,
        };
        assert!(!cancel_rearm_arm(&wrong_agent));
        assert!(rearm_state_for_test(agent, row_id).is_some());

        let wrong_epoch = RearmReservation {
            agent: agent.to_string(),
            row_id: row_id.to_string(),
            transport_epoch: epoch.saturating_add(1),
            deferred: true,
        };
        assert!(!cancel_rearm_arm(&wrong_epoch));
        assert!(rearm_state_for_test(agent, row_id).is_some());

        let correct = RearmReservation {
            agent: agent.to_string(),
            row_id: row_id.to_string(),
            transport_epoch: epoch,
            deferred: true,
        };
        assert!(cancel_rearm_arm(&correct));
        assert!(rearm_state_for_test(agent, row_id).is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stale_generation_load_cannot_purge_successor_latch() {
        let _g = test_guard();
        let home = tmp_home("stale-load-generation");
        let agent = "stale-load-generation-2044";
        let old_text = "wake id=m-old-loader";
        let successor_row = "m-successor-latch";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        let old_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        crate::daemon::hook_shadow::record_event(agent, "UserPromptSubmit", None);

        let _load_guard = test_support::load_before_lock_hook_guard();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let release_rx_hook = std::sync::Arc::clone(&release_rx);
        test_support::set_load_before_lock_hook(Some(std::sync::Arc::new(
            move |_, hook_agent, _| {
                if hook_agent != agent {
                    return;
                }
                ready_tx.send(()).expect("load hook ready receiver");
                release_rx_hook
                    .lock()
                    .expect("load hook mutex")
                    .recv()
                    .expect("load hook release");
            },
        )));
        let load_home = home.clone();
        let loader = std::thread::spawn(move || {
            prepare_arm(
                &load_home,
                agent,
                old_text,
                crate::transport::TransportMode::ChannelBridge,
                old_epoch,
                None,
            )
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old load reaches barrier");

        let cleanup = crate::daemon::delivery_worker::begin_transport_cleanup(&home, agent);
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup state");
        drop(cleanup);
        let successor_epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        let successor = Pending {
            agent: agent.to_string(),
            injected_at_ms: now_ms(),
            text: format!("wake id={successor_row}"),
            channel_origin: None,
            redelivered: true,
            transport_epoch: successor_epoch,
            transport_mode: crate::transport::TransportMode::ChannelBridge,
            gave_up: true,
            rearm_count: 0,
            rearm_reserved: false,
            rearm_pending: false,
        };
        persist_latch(&home, successor_row, &successor).expect("successor latch");

        release_tx.send(()).expect("release old load");
        assert!(loader.join().expect("old loader thread").is_none());
        test_support::set_load_before_lock_hook(None);
        let latches =
            load_latches_for_epoch(&home, agent, successor_epoch).expect("successor load");
        assert!(
            latches.iter().any(|latch| latch.row_id == successor_row),
            "old-generation load must not purge a successor-generation latch"
        );
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3303 review regression: a production arm attempt that cannot open its
    /// latch lock must fail closed with an operator-visible diagnostic rather
    /// than silently disabling structured verification.
    #[test]
    #[tracing_test::traced_test]
    fn latch_lock_failure_logs_before_fail_closed_arm() {
        let _g = test_guard();
        let home = tmp_home("latch-lock-failure-log");
        std::fs::remove_dir_all(&home).expect("remove fixture directory");
        std::fs::write(&home, b"not a directory").expect("create invalid home path");
        let agent = "latch-lock-failure-log-2044";
        let text = "wake id=m-latch-lock-failure";
        crate::daemon::hook_shadow::record_event(agent, "UserPromptSubmit", None);

        assert!(
            prepare_arm(
                &home,
                agent,
                text,
                crate::transport::TransportMode::ChannelBridge,
                0,
                None,
            )
            .is_none(),
            "unavailable latch lock must fail closed"
        );
        assert!(
            logs_contain("#2044 durable verification latch lock unavailable"),
            "latch lock failure must be visible in tracing output"
        );
        std::fs::remove_file(&home).ok();
    }

    /// #3303 blocker 12: durable persistence is authoritative even when a
    /// prompt confirmation or cleanup removes the process-local reservation
    /// before its final CAS. Pin the tri-state outcome so callers do not
    /// requeue a physically accepted pointer after the budget is already 1.
    #[test]
    fn durable_committed_outcome_survives_process_cas_loss() {
        let _g = test_guard();
        let home = tmp_home("durable-committed-cas-loss");
        let agent = "durable-committed-cas-loss-2044";
        let row_id = "m-durable-committed-cas-loss";
        forget(agent);
        remove_durable_latches(&home, agent).expect("clean latches");
        let epoch = crate::daemon::delivery_worker::current_transport_epoch(&home, agent);
        store().lock().insert(
            row_id.to_string(),
            Pending {
                agent: agent.to_string(),
                injected_at_ms: now_ms(),
                text: format!("[AGEND-MSG-PENDING] id={row_id} kind=task"),
                channel_origin: None,
                redelivered: false,
                transport_epoch: epoch,
                transport_mode: crate::transport::TransportMode::ChannelBridge,
                gave_up: false,
                rearm_count: 0,
                rearm_reserved: true,
                rearm_pending: true,
            },
        );
        let reservation = RearmReservation {
            agent: agent.to_string(),
            row_id: row_id.to_string(),
            transport_epoch: epoch,
            deferred: true,
        };
        let _persist_guard = test_support::persist_after_write_hook_guard();
        test_support::set_persist_after_write_hook(Some(std::sync::Arc::new(
            move |_, hook_agent, _| {
                forget(hook_agent);
            },
        )));

        assert_eq!(
            commit_rearm_after_reclaim_outcome(&home, &reservation),
            RearmCommitOutcome::DurableCommitted
        );
        let latches = load_latches_for_epoch(&home, agent, epoch).expect("durable latch");
        let latch = latches
            .iter()
            .find(|latch| latch.row_id == row_id)
            .expect("committed row latch");
        assert_eq!(latch.rearm_count, 1);
        assert!(!latch.gave_up);
        assert!(!latch.rearm_pending);
        assert!(rearm_state_for_test(agent, row_id).is_none());
        forget(agent);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Helper: re-stamp an armed inject so the verify window has elapsed.
    fn arm_at_elapsed(agent: &str, _text: &str) {
        if let Some(p) = store()
            .lock()
            .values_mut()
            .find(|pending| pending.agent == agent)
        {
            p.injected_at_ms = now_ms() - VERIFY_WINDOW_MS - 1;
        }
    }
}
