use super::envelope::DeliveryEnvelope;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Receipt metadata is intentionally bounded.  The first record for a
/// delivery owns the durable envelope body; compaction keeps that owner and
/// the latest receipt, while dropping superseded metadata records.
const MAX_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RETAINED_DELIVERIES: usize = 1024;
const OPENCODE_MESSAGE_ID_PREFIX: &str = "msg_";
const OPENCODE_MESSAGE_ID_NATIVE_SUFFIX_LEN: usize = 12 + 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DeliveryState {
    Queued,
    ProtocolAccepted,
    ObservedInSession,
    TurnStarted,
    Completed,
    Failed,
    Ambiguous,
    /// A self-kick exceeded its acknowledgement window but remains
    /// nonterminal so a truthful late current-session ack can still start it.
    AckOverdue,
}

impl DeliveryState {
    pub(crate) fn outcome_name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::ProtocolAccepted => "accepted",
            Self::ObservedInSession => "observed_in_session",
            Self::TurnStarted => "turn_started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Ambiguous => "ambiguous",
            Self::AckOverdue => "ack_overdue",
        }
    }
}

impl DeliveryState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Ambiguous)
    }
}

/// PR #3495: the durable INTENT to emit exactly one operator-facing notice for
/// this delivery.
///
/// It exists because the notice is enqueued by a DIFFERENT layer (the per-tick
/// handler owns the fleet, the recipient and the registry) than the one that
/// owns the durable state (this store). Persisting the intent BEFORE the
/// enqueue — and clearing it only AFTER the enqueue reports success — is what
/// makes the reconciliation crash-safe: a daemon that dies in the gap, or an
/// enqueue that returns `Err`, leaves the marker set and the next watchdog pass
/// replays it. The replay is safe because the enqueue is idempotent by key
/// (`self-kick:<delivery_id>:<kind>`, see `crate::inbox::idempotent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingNotice {
    /// `"ack_overdue"` or `"late_ack"` — also the `<kind>` half of the
    /// idempotency key, so the key is recomputable from the durable row alone.
    pub kind: String,
    /// The `AckLate` overshoot the notice must quote. Carried here because the
    /// same CAS that persists the intent CLEARS `late_ack_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_by_secs: Option<i64>,
    pub created_at: String,
}

impl PendingNotice {
    pub(crate) const ACK_OVERDUE: &'static str = "ack_overdue";
    pub(crate) const LATE_ACK: &'static str = "late_ack";

    pub(crate) fn new(kind: &str, late_by_secs: Option<i64>) -> Self {
        Self {
            kind: kind.to_string(),
            late_by_secs,
            created_at: Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeliveryReceipt {
    pub delivery_id: Uuid,
    pub state: DeliveryState,
    pub payload_digest: String,
    pub protocol_request_id: Option<String>,
    /// The backend session that actually accepted the request. This normally
    /// matches the queued envelope, but an OpenCode wrap rollover moves the
    /// same durable delivery to a freshly forked session.
    #[serde(default)]
    pub backend_session_id: Option<String>,
    pub backend_event: Option<String>,
    pub tui_visibility: Option<String>,
    pub detail: Option<String>,
    /// Route and attempt outcome are durable audit fields. They deliberately
    /// remain separate from `state`: a queued row is not backend acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// t-20260902165405106195-82348-105: set ONLY on a `TurnStarted` receipt
    /// that superseded an `AckOverdue` — the seconds by which the consumer's
    /// `ack_start` missed the acknowledgement window. Additive audit metadata
    /// that the daemon's per-tick watchdog consumes to emit exactly one
    /// late-ack reconciliation notice; it never participates in a state
    /// transition, and the CAS that clears it is state-preserving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_ack_secs: Option<i64>,
    /// PR #3495: an unfinished operator notice for this delivery — see
    /// [`PendingNotice`]. Set by the watchdog pass in the SAME compare-and-append
    /// that consumes the condition it reports, and cleared by a second CAS only
    /// after the notice is durably in the recipient's inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_pending: Option<PendingNotice>,
    pub recorded_at: String,
}

impl DeliveryReceipt {
    pub(crate) fn queued(envelope: &DeliveryEnvelope) -> Self {
        Self::for_state(envelope, DeliveryState::Queued)
    }

    pub(crate) fn for_state(envelope: &DeliveryEnvelope, state: DeliveryState) -> Self {
        Self {
            delivery_id: envelope.delivery_id,
            state,
            payload_digest: envelope.payload_digest.clone(),
            protocol_request_id: None,
            backend_session_id: None,
            backend_event: None,
            tui_visibility: None,
            detail: None,
            route: envelope.transport_mode.clone(),
            attempt: 1,
            outcome: Some(state.outcome_name().to_string()),
            late_ack_secs: None,
            notice_pending: None,
            recorded_at: Utc::now().to_rfc3339(),
        }
    }
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Retries receive a fresh envelope UUID, so only the explicit durable row
/// identity is used to number attempts across process restarts. Independent
/// deliveries never become retries merely because their payloads match.
fn attempt_key(envelope: &DeliveryEnvelope) -> String {
    envelope.logical_delivery_id.as_deref().map_or_else(
        || format!("delivery:{}", envelope.delivery_id),
        |id| format!("row:{id}"),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableRecord {
    envelope: Option<DeliveryEnvelope>,
    receipt: DeliveryReceipt,
}

/// Durable receipt/audit store with bounded compaction. The envelope is
/// included only on the initial queued record; later entries carry a digest,
/// never a second copy of the payload. A separate receipt record makes restart
/// reconciliation deterministic and avoids claiming exactly-once delivery.
#[derive(Debug, Clone)]
pub(crate) struct ReceiptStore {
    path: PathBuf,
}

fn is_native_opencode_protocol_request_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(OPENCODE_MESSAGE_ID_PREFIX) else {
        return false;
    };
    let Some(timestamp) = suffix.get(..12) else {
        return false;
    };
    let Some(random) = suffix.get(12..) else {
        return false;
    };
    suffix.len() == OPENCODE_MESSAGE_ID_NATIVE_SUFFIX_LEN
        && timestamp.bytes().all(|byte| byte.is_ascii_hexdigit())
        && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

impl ReceiptStore {
    pub(crate) fn for_instance(home: &Path, instance: &str) -> anyhow::Result<Self> {
        let dir = delivery_dir(home);
        std::fs::create_dir_all(&dir)?;
        restrict_permissions(&dir, 0o700)?;
        Ok(Self {
            path: delivery_path_for_instance(home, instance),
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn record_queued(
        &self,
        envelope: &DeliveryEnvelope,
    ) -> anyhow::Result<DeliveryReceipt> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if let Some(previous) = self.latest_locked(envelope.delivery_id)? {
            if previous.state == DeliveryState::Ambiguous {
                return Ok(previous);
            }
        }
        let mut receipt = DeliveryReceipt::queued(envelope);
        receipt.attempt = self.next_attempt_locked(envelope)?;
        self.append_locked(DurableRecord {
            envelope: Some(envelope.clone()),
            receipt: receipt.clone(),
        })?;
        Ok(receipt)
    }

    pub(crate) fn record(&self, mut receipt: DeliveryReceipt) -> anyhow::Result<()> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if let Some(previous) = self.latest_locked(receipt.delivery_id)? {
            receipt.attempt = previous.attempt;
            if receipt.route.is_none() {
                receipt.route = previous.route;
            }
        }
        self.append_locked(DurableRecord {
            envelope: None,
            receipt,
        })
    }

    /// Append `next` only when the latest durable state is `expected`.
    ///
    /// PR #3495 r3: TEST-ONLY. Every production writer now uses the
    /// marker-aware [`Self::record_if_marker`] instead — a state-only
    /// predicate cannot see a durable notice intent moving under it, which is
    /// exactly how a late `ack_start` used to destroy one. Fixtures that need
    /// to plant a receipt keep it.
    ///
    /// The compare-and-append is held under the same per-instance lock as
    /// receipt writes. The self-kick ack and timeout watchdog therefore
    /// linearize: an ack that wins prevents the watchdog from emitting a
    /// stale Ambiguous receipt, and a timeout that wins cannot be overwritten
    /// by a late ack.
    #[cfg(test)]
    pub(crate) fn record_if_latest_state(
        &self,
        delivery_id: Uuid,
        expected: DeliveryState,
        next: DeliveryReceipt,
    ) -> anyhow::Result<bool> {
        self.record_if(delivery_id, next, |latest| latest.state == expected)
    }

    /// PR #3495: MARKER-AWARE compare-and-append.
    ///
    /// `record_if_latest_state` compares only `state`, which is not a
    /// linearization point for a transition that PRESERVES the state and moves
    /// only a marker (the late-ack reconciliation is exactly that). Two
    /// scanners holding the same stale snapshot both pass a state-only
    /// predicate — sequentially, under this same lock — and both emit the
    /// outcome. Including the markers in the predicate makes the stale
    /// scanner's CAS fail, so the outcome is produced exactly once.
    pub(crate) fn record_if_marker(
        &self,
        delivery_id: Uuid,
        expected: DeliveryState,
        expected_late_ack_secs: Option<i64>,
        expected_notice: Option<&PendingNotice>,
        next: DeliveryReceipt,
    ) -> anyhow::Result<bool> {
        self.record_if(delivery_id, next, |latest| {
            latest.state == expected
                && latest.late_ack_secs == expected_late_ack_secs
                && latest.notice_pending.as_ref() == expected_notice
        })
    }

    /// The shared compare-and-append body: read the latest durable receipt
    /// under the per-instance lock, test `matches`, and append only on a hit.
    fn record_if(
        &self,
        delivery_id: Uuid,
        next: DeliveryReceipt,
        matches: impl Fn(&DeliveryReceipt) -> bool,
    ) -> anyhow::Result<bool> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        let latest = self.latest_locked(delivery_id)?;
        if !latest.as_ref().is_some_and(&matches) {
            return Ok(false);
        }
        let mut next = next;
        if let Some(previous) = latest {
            next.attempt = previous.attempt;
            if next.route.is_none() {
                next.route = previous.route;
            }
        }
        self.append_locked(DurableRecord {
            envelope: None,
            receipt: next,
        })?;
        Ok(true)
    }

    fn next_attempt_locked(&self, envelope: &DeliveryEnvelope) -> anyhow::Result<u32> {
        let key = attempt_key(envelope);
        if !self.path.exists() {
            return Ok(1);
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let mut latest = 0_u32;
        for line in BufReader::new(file).lines() {
            let record: DurableRecord = serde_json::from_str(&line?)?;
            let Some(previous) = record.envelope.as_ref() else {
                continue;
            };
            if attempt_key(previous) == key {
                latest = latest.max(record.receipt.attempt.max(1));
            }
        }
        Ok(latest.saturating_add(1))
    }

    pub(crate) fn latest(&self, delivery_id: Uuid) -> anyhow::Result<Option<DeliveryReceipt>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        self.latest_locked(delivery_id)
    }

    fn latest_locked(&self, delivery_id: Uuid) -> anyhow::Result<Option<DeliveryReceipt>> {
        if !self.path.exists() {
            return Ok(None);
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let mut latest = None;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let record: DurableRecord = serde_json::from_str(&line)?;
            if record.receipt.delivery_id == delivery_id {
                latest = Some(record.receipt);
            }
        }
        Ok(latest)
    }

    pub(crate) fn delivery(
        &self,
        delivery_id: Uuid,
    ) -> anyhow::Result<Option<(DeliveryEnvelope, DeliveryReceipt)>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if !self.path.exists() {
            return Ok(None);
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let mut envelope = None;
        let mut latest = None;
        for line in BufReader::new(file).lines() {
            let record: DurableRecord = serde_json::from_str(&line?)?;
            if record.receipt.delivery_id != delivery_id {
                continue;
            }
            if let Some(record_envelope) = record.envelope {
                envelope = Some(record_envelope);
            }
            latest = Some(record.receipt);
        }
        Ok(envelope.zip(latest))
    }

    /// Return durable envelopes whose latest receipt is not terminal.  A
    /// structured adapter is intentionally short-lived per worker job, so a
    /// fresh adapter must restore the in-flight turn before deciding whether a
    /// second prompt may be sent.
    pub(crate) fn pending_deliveries(
        &self,
    ) -> anyhow::Result<Vec<(DeliveryEnvelope, DeliveryReceipt)>> {
        self.deliveries_where(|receipt| !receipt.state.is_terminal())
    }

    /// PR #3495 r3: what the self-kick watchdog must still reconcile — every
    /// non-terminal delivery PLUS terminal ones that still carry an unfinished
    /// notice intent or an unreconciled `late_ack_secs`.
    ///
    /// The consumer can complete a self-kick inside the gap between the
    /// durable intent and the per-tick enqueue, and `Completed` is terminal:
    /// filtering on state alone would strand the notices the operator is owed.
    /// The markers are cleared only once their notices are durable, so this
    /// widening keeps exactly the rows with outstanding work.
    pub(crate) fn deliveries_owing_notices(
        &self,
    ) -> anyhow::Result<Vec<(DeliveryEnvelope, DeliveryReceipt)>> {
        self.deliveries_where(|receipt| {
            !receipt.state.is_terminal()
                || receipt.notice_pending.is_some()
                || receipt.late_ack_secs.is_some()
        })
    }

    fn deliveries_where(
        &self,
        keep: impl Fn(&DeliveryReceipt) -> bool,
    ) -> anyhow::Result<Vec<(DeliveryEnvelope, DeliveryReceipt)>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let mut histories: HashMap<Uuid, (Option<DeliveryEnvelope>, Option<DeliveryReceipt>)> =
            HashMap::new();
        for line in BufReader::new(file).lines() {
            let record: DurableRecord = serde_json::from_str(&line?)?;
            let entry = histories
                .entry(record.receipt.delivery_id)
                .or_insert_with(|| (None, None));
            if let Some(envelope) = record.envelope {
                entry.0 = Some(envelope);
            }
            entry.1 = Some(record.receipt);
        }
        Ok(histories
            .into_values()
            .filter_map(|(envelope, receipt)| {
                let receipt = receipt?;
                if !keep(&receipt) {
                    return None;
                }
                Some((envelope?, receipt))
            })
            .collect())
    }

    pub(crate) fn latest_opencode_protocol_request_id_for_session(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if !self.path.exists() {
            return Ok(None);
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let records = BufReader::new(file)
            .lines()
            .map(|line| -> anyhow::Result<DurableRecord> { Ok(serde_json::from_str(&line?)?) })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let envelope_sessions = records
            .iter()
            .filter_map(|record| {
                record.envelope.as_ref().and_then(|envelope| {
                    envelope
                        .session
                        .session_id
                        .as_ref()
                        .map(|session_id| (record.receipt.delivery_id, session_id.clone()))
                })
            })
            .collect::<HashMap<_, _>>();
        let mut latest = None;
        for record in records {
            let Some(request_id) = record.receipt.protocol_request_id.as_deref() else {
                continue;
            };
            let actual_session = record.receipt.backend_session_id.as_deref().or_else(|| {
                envelope_sessions
                    .get(&record.receipt.delivery_id)
                    .map(String::as_str)
            });
            if actual_session != Some(session_id)
                || !is_native_opencode_protocol_request_id(request_id)
            {
                continue;
            }
            if latest
                .as_deref()
                .is_none_or(|current: &str| request_id > current)
            {
                latest = Some(request_id.to_string());
            }
        }
        Ok(latest)
    }

    fn append_locked(&self, record: DurableRecord) -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        restrict_permissions(&self.path, 0o600)?;
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_data()?;
        let should_compact = file.metadata()?.len() > MAX_RECEIPT_BYTES;
        drop(file);
        if should_compact {
            self.compact_locked()?;
        }
        Ok(())
    }

    /// Compact the receipt log while the caller holds its per-instance flock.
    /// A delivery's envelope is retained exactly once as the body owner and
    /// only its latest receipt metadata is retained.  The newest deliveries
    /// win the byte and count budgets, so replay still has the latest durable
    /// body for every retained delivery.
    fn compact_locked(&self) -> anyhow::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let file = File::open(&self.path)?;
        let mut histories: HashMap<Uuid, DeliveryHistory> = HashMap::new();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let record: DurableRecord = serde_json::from_str(&line?)?;
            let delivery_id = record.receipt.delivery_id;
            let entry = histories
                .entry(delivery_id)
                .or_insert_with(|| DeliveryHistory {
                    envelope: None,
                    latest: None,
                    last_index: index,
                });
            if record.envelope.is_some() && entry.envelope.is_none() {
                entry.envelope = Some(record.clone());
            }
            entry.latest = Some(record);
            entry.last_index = index;
        }

        let mut histories: Vec<DeliveryHistory> = histories.into_values().collect();
        histories.sort_by_key(|history| std::cmp::Reverse(history.last_index));
        let mut retained = Vec::new();
        let mut retained_bytes = 0_u64;
        for history in histories.into_iter().take(MAX_RETAINED_DELIVERIES) {
            let Some(latest) = history.latest else {
                continue;
            };
            let records = if latest.envelope.is_some() {
                vec![latest]
            } else {
                let mut records = Vec::with_capacity(2);
                if let Some(envelope) = history.envelope {
                    records.push(envelope);
                }
                records.push(latest);
                records
            };
            let mut bytes = 0_u64;
            for record in &records {
                let mut line = serde_json::to_vec(record)?;
                line.push(b'\n');
                bytes = bytes.saturating_add(line.len() as u64);
            }
            // Always retain the newest delivery, even if its one required
            // envelope exceeds the normal aggregate byte budget.
            if !retained.is_empty() && retained_bytes.saturating_add(bytes) > MAX_RECEIPT_BYTES {
                continue;
            }
            retained_bytes = retained_bytes.saturating_add(bytes);
            retained.push((history.last_index, records));
        }
        retained.sort_by_key(|(index, _)| *index);

        let mut output = Vec::new();
        for (_, records) in retained {
            for record in records {
                serde_json::to_writer(&mut output, &record)?;
                output.push(b'\n');
            }
        }
        crate::store::atomic_write(&self.path, &output)?;
        restrict_permissions(&self.path, 0o600)?;
        Ok(())
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("jsonl.lock")
    }
}

#[derive(Debug)]
struct DeliveryHistory {
    envelope: Option<DurableRecord>,
    latest: Option<DurableRecord>,
    last_index: usize,
}

fn restrict_permissions(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn delivery_dir(home: &Path) -> PathBuf {
    home.join("transport").join("deliveries")
}

pub(crate) fn delivery_path_for_instance(home: &Path, instance: &str) -> PathBuf {
    delivery_dir(home).join(format!("{}.jsonl", safe_component(instance)))
}

/// Remove all durable delivery state for an instance, including the advisory
/// lock sidecar. The caller must hold the delivery-worker cleanup guard so no
/// queued or in-flight transport job can recreate the files after removal.
pub(crate) fn remove_instance_delivery_state(home: &Path, instance: &str) -> anyhow::Result<()> {
    let path = delivery_path_for_instance(home, instance);
    let lock_path = path.with_extension("jsonl.lock");
    for candidate in [&path, &lock_path] {
        match std::fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let dir = delivery_dir(home);
    if dir.exists() {
        let mut entries = std::fs::read_dir(&dir)?;
        if entries.next().is_none() {
            std::fs::remove_dir(&dir)?;
        }
    }
    Ok(())
}

pub(crate) fn safe_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::envelope::{DeliveryKind, SessionLocator};
    use std::path::PathBuf;

    #[test]
    fn receipt_store_survives_and_reconciles_state_transitions() {
        let home = std::env::temp_dir().join(format!("agend-transport-receipt-{}", Uuid::new_v4()));
        let store = ReceiptStore::for_instance(&home, "agent/one").expect("store");
        let mut envelope = DeliveryEnvelope::new(
            "agent/one",
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
            DeliveryKind::Prompt,
            "secret body",
            None,
        );
        envelope.transport_mode = Some("native_shared".to_string());
        envelope.logical_delivery_id = Some("row-retry".to_string());
        store.record_queued(&envelope).expect("queued");
        let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        accepted.protocol_request_id = Some("7".to_string());
        store.record(accepted).expect("accepted");
        let latest = store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("record");
        assert_eq!(latest.state, DeliveryState::ProtocolAccepted);
        assert_eq!(latest.payload_digest, envelope.payload_digest);
        let durable = std::fs::read_to_string(store.path()).expect("read");
        assert!(durable.contains("secret body"));
        assert!(
            durable.contains("\"route\""),
            "receipt must retain route evidence"
        );
        assert!(
            durable.contains("\"attempt\""),
            "receipt must retain attempt evidence"
        );
        assert!(
            durable.contains("\"outcome\":\"queued\""),
            "receipt must retain the queued outcome before backend acceptance"
        );

        let mut retry = envelope.clone();
        retry.delivery_id = Uuid::new_v4();
        let retry_queued = store.record_queued(&retry).expect("retry queued");
        assert_eq!(retry_queued.attempt, 2);
        store
            .record(DeliveryReceipt::for_state(&retry, DeliveryState::Completed))
            .expect("retry completed");
        assert_eq!(
            store
                .latest(retry.delivery_id)
                .expect("retry latest")
                .expect("retry receipt")
                .attempt,
            2
        );

        let independent = DeliveryEnvelope::new(
            "agent/one",
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
            DeliveryKind::Prompt,
            "secret body",
            None,
        );
        assert_eq!(
            store
                .record_queued(&independent)
                .expect("independent queued")
                .attempt,
            1
        );
        let mut same_thread_a = independent.clone();
        same_thread_a.delivery_id = Uuid::new_v4();
        same_thread_a.correlation_id = Some("same-thread".to_string());
        same_thread_a.logical_delivery_id = Some("row-a".to_string());
        let mut same_thread_b = same_thread_a.clone();
        same_thread_b.delivery_id = Uuid::new_v4();
        same_thread_b.logical_delivery_id = Some("row-b".to_string());
        assert_eq!(
            store
                .record_queued(&same_thread_a)
                .expect("row-a queued")
                .attempt,
            1
        );
        assert_eq!(
            store
                .record_queued(&same_thread_b)
                .expect("row-b queued")
                .attempt,
            1
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn receipt_store_compacts_and_keeps_one_body_owner_per_delivery() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-receipt-retention-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent/one").expect("store");
        let body_padding = "x".repeat(120_000);
        let mut delivery_ids = Vec::new();
        for index in 0..40 {
            let envelope = DeliveryEnvelope::new(
                "agent/one",
                SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
                DeliveryKind::Prompt,
                format!("body-{index}-{body_padding}"),
                None,
            );
            delivery_ids.push(envelope.delivery_id);
            store.record_queued(&envelope).expect("queued");
            store
                .record(DeliveryReceipt::for_state(
                    &envelope,
                    DeliveryState::Completed,
                ))
                .expect("completed");
        }
        let bytes = std::fs::read(store.path()).expect("receipt bytes");
        assert!(
            (bytes.len() as u64) <= MAX_RECEIPT_BYTES,
            "receipt retention must stay bounded: {} > {}",
            bytes.len(),
            MAX_RECEIPT_BYTES
        );
        let records: Vec<DurableRecord> = BufReader::new(bytes.as_slice())
            .lines()
            .map(|line| serde_json::from_str(&line.expect("line")))
            .collect::<Result<_, _>>()
            .expect("valid compacted records");
        let mut body_owners = HashMap::new();
        for record in &records {
            if record.envelope.is_some() {
                *body_owners
                    .entry(record.receipt.delivery_id)
                    .or_insert(0_u32) += 1;
            }
        }
        assert!(body_owners.values().all(|count| *count == 1));
        let receipt_text = std::str::from_utf8(&bytes).expect("utf8");
        for (index, body_marker) in [(33, "body-33-"), (39, "body-39-")] {
            assert_eq!(
                body_owners.get(&delivery_ids[index]),
                Some(&1_u32),
                "retained delivery {index} must have exactly one body owner"
            );
            assert!(
                receipt_text.contains(body_marker),
                "retained delivery {index} body must survive compaction"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("transport/deliveries"))
                    .expect("delivery dir")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(store.path())
                    .expect("receipt file")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(store.lock_path())
                    .expect("receipt lock")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn protocol_request_id_survives_terminal_compaction_and_reload() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-receipt-protocol-id-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("store");
        let body_padding = "x".repeat(120_000);
        for index in 0..40 {
            let envelope = DeliveryEnvelope::new(
                "agent",
                SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
                DeliveryKind::Prompt,
                format!("filler-{index}-{body_padding}"),
                None,
            );
            store.record_queued(&envelope).expect("filler queued");
            store
                .record(DeliveryReceipt::for_state(
                    &envelope,
                    DeliveryState::Completed,
                ))
                .expect("filler completed");
        }

        let target = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("thread".to_string()),
                "opencode".to_string(),
                "secret".to_string(),
            ),
            DeliveryKind::Prompt,
            format!("target-{body_padding}"),
            None,
        );
        let protocol_request_id = "msg_fdb5a96c3001auuSsUN6in29xP".to_string();
        store.record_queued(&target).expect("target queued");
        let mut accepted = DeliveryReceipt::for_state(&target, DeliveryState::ProtocolAccepted);
        accepted.protocol_request_id = Some(protocol_request_id.clone());
        store.record(accepted).expect("target accepted");
        let mut completed = DeliveryReceipt::for_state(&target, DeliveryState::Completed);
        completed.protocol_request_id = Some(protocol_request_id.clone());
        store.record(completed).expect("target completed");

        let tail = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("thread".to_string()),
                "opencode".to_string(),
                "secret".to_string(),
            ),
            DeliveryKind::Prompt,
            format!("tail-{body_padding}"),
            None,
        );
        store.record_queued(&tail).expect("tail queued");
        store
            .record(DeliveryReceipt::for_state(&tail, DeliveryState::Completed))
            .expect("tail completed");

        let reloaded = ReceiptStore::for_instance(&home, "agent").expect("reload store");
        assert_eq!(
            reloaded
                .latest(target.delivery_id)
                .expect("latest target")
                .expect("target receipt")
                .state,
            DeliveryState::Completed
        );
        assert_eq!(
            reloaded
                .latest_opencode_protocol_request_id_for_session("thread")
                .expect("latest protocol request id"),
            Some(protocol_request_id)
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn opencode_seed_lookup_scopes_session_and_ignores_legacy_ids_after_reload() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-receipt-opencode-seed-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("store");
        let body_padding = "x".repeat(120_000);
        for index in 0..40 {
            let envelope = DeliveryEnvelope::new(
                "agent",
                SessionLocator::opencode(
                    "http://127.0.0.1:4096".to_string(),
                    Some("session-a".to_string()),
                    "opencode".to_string(),
                    "secret".to_string(),
                ),
                DeliveryKind::Prompt,
                format!("filler-{index}-{body_padding}"),
                None,
            );
            store.record_queued(&envelope).expect("filler queued");
            store
                .record(DeliveryReceipt::for_state(
                    &envelope,
                    DeliveryState::Completed,
                ))
                .expect("filler completed");
        }

        let record_request = |session_id: &str, request_id: &str| {
            let envelope = DeliveryEnvelope::new(
                "agent",
                SessionLocator::opencode(
                    "http://127.0.0.1:4096".to_string(),
                    Some(session_id.to_string()),
                    "opencode".to_string(),
                    "secret".to_string(),
                ),
                DeliveryKind::Prompt,
                format!("request-{session_id}-{request_id}-{body_padding}"),
                None,
            );
            store.record_queued(&envelope)?;
            let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Completed);
            receipt.protocol_request_id = Some(request_id.to_string());
            store.record(receipt)
        };
        let legacy = "msg_ffffffffffffffffffffffffffffffff";
        let session_a_low = "msg_fdb5a96c3001auuSsUN6in29xP";
        let session_a_high = "msg_fdb5a96c3002auuSsUN6in29xP";
        let session_b_high = "msg_fdb5a96c4000auuSsUN6in29xP";
        record_request("session-a", legacy).expect("legacy request");
        record_request("session-a", session_a_low).expect("low request");
        record_request("session-a", session_a_high).expect("high request");
        record_request("session-b", session_b_high).expect("other-session request");

        let tail = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("session-a".to_string()),
                "opencode".to_string(),
                "secret".to_string(),
            ),
            DeliveryKind::Prompt,
            format!("tail-{body_padding}"),
            None,
        );
        store.record_queued(&tail).expect("tail queued");
        store
            .record(DeliveryReceipt::for_state(&tail, DeliveryState::Completed))
            .expect("tail completed");

        let reloaded = ReceiptStore::for_instance(&home, "agent").expect("reload store");
        assert_eq!(
            reloaded
                .latest_opencode_protocol_request_id_for_session("session-a")
                .expect("session A seed"),
            Some(session_a_high.to_string())
        );
        assert_eq!(
            reloaded
                .latest_opencode_protocol_request_id_for_session("session-b")
                .expect("session B seed"),
            Some(session_b_high.to_string())
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn opencode_seed_lookup_prefers_the_receipts_actual_backend_session() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-receipt-opencode-rollover-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("store");
        let envelope = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("source-session".to_string()),
                "opencode".to_string(),
                "secret".to_string(),
            ),
            DeliveryKind::Prompt,
            "rollover prompt",
            None,
        );
        store.record_queued(&envelope).expect("queued");
        let request_id = "msg_00000000000a00000000000000";
        let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        accepted.protocol_request_id = Some(request_id.to_string());
        accepted.backend_session_id = Some("target-session".to_string());
        store.record(accepted).expect("accepted");

        assert_eq!(
            store
                .latest_opencode_protocol_request_id_for_session("target-session")
                .expect("target seed")
                .as_deref(),
            Some(request_id)
        );
        assert_eq!(
            store
                .latest_opencode_protocol_request_id_for_session("source-session")
                .expect("source seed"),
            None
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_receipts_without_backend_session_id_still_deserialize() {
        let envelope = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("session".to_string()),
                "opencode".to_string(),
                "secret".to_string(),
            ),
            DeliveryKind::Prompt,
            "legacy receipt",
            None,
        );
        let receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Completed);
        let mut value = serde_json::to_value(receipt).expect("serialize");
        value
            .as_object_mut()
            .expect("receipt object")
            .remove("backend_session_id");
        let decoded: DeliveryReceipt = serde_json::from_value(value).expect("legacy receipt");
        assert!(decoded.backend_session_id.is_none());
    }

    #[test]
    fn opencode_durable_receipts_redact_password_and_restore_locator_identity() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-credential-receipt-opencode-credentials-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("receipt store");
        let mut locator = SessionLocator::opencode(
            "http://127.0.0.1:4096".to_string(),
            Some("session".to_string()),
            "opencode".to_string(),
            "do-not-persist".to_string(),
        );
        locator.model = Some("openai/gpt-5".to_string());
        locator.event_cursor = Some(17);
        locator.managed = false;
        locator.server_pid = Some(42);
        locator.server_start_token = Some(7);
        let envelope = DeliveryEnvelope::new(
            "agent",
            locator,
            DeliveryKind::Prompt,
            "recovery body",
            Some("correlation".to_string()),
        );

        let exported = serde_json::to_string(&envelope).expect("export envelope");
        assert!(!exported.contains("do-not-persist"));
        assert!(!exported.contains("\"password\""));
        store.record_queued(&envelope).expect("queued receipt");
        let durable = std::fs::read_to_string(store.path()).expect("read receipt");
        assert!(!durable.contains("do-not-persist"));
        assert!(!durable.contains("\"password\""));

        let (recovered, _) = store
            .delivery(envelope.delivery_id)
            .expect("load receipt")
            .expect("durable envelope");
        assert_eq!(recovered.session.password, None);
        assert_eq!(recovered.session.backend, "opencode");
        assert_eq!(
            recovered.session.endpoint_url.as_deref(),
            Some("http://127.0.0.1:4096")
        );
        assert_eq!(recovered.session.session_id.as_deref(), Some("session"));
        assert_eq!(recovered.session.username.as_deref(), Some("opencode"));
        assert_eq!(recovered.session.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(recovered.session.event_cursor, Some(17));
        assert!(!recovered.session.managed);
        assert_eq!(recovered.session.server_pid, Some(42));
        assert_eq!(recovered.session.server_start_token, Some(7));

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_opencode_receipt_with_password_still_recovers() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-credential-receipt-opencode-legacy-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("receipt store");
        let envelope = DeliveryEnvelope::new(
            "agent",
            SessionLocator::opencode(
                "http://127.0.0.1:4096".to_string(),
                Some("legacy-session".to_string()),
                "opencode".to_string(),
                "legacy-password".to_string(),
            ),
            DeliveryKind::Prompt,
            "legacy body",
            None,
        );
        let mut envelope_value = serde_json::to_value(&envelope).expect("serialize envelope");
        envelope_value["session"]["password"] = serde_json::json!("legacy-password");
        let record = serde_json::json!({
            "envelope": envelope_value,
            "receipt": DeliveryReceipt::queued(&envelope),
        });
        std::fs::write(
            store.path(),
            format!(
                "{}\n",
                serde_json::to_string(&record).expect("serialize legacy record")
            ),
        )
        .expect("write legacy receipt");

        let (recovered, _) = store
            .delivery(envelope.delivery_id)
            .expect("load legacy receipt")
            .expect("legacy envelope");
        assert_eq!(
            recovered.session.password.as_deref(),
            Some("legacy-password")
        );
        assert_eq!(
            recovered.session.session_id.as_deref(),
            Some("legacy-session")
        );
        assert_eq!(recovered.body, "legacy body");

        let _ = std::fs::remove_dir_all(home);
    }

    /// #40632-2 follow-up: the locator's `password` field carries OpenCode's
    /// server password AND the Claude ChannelBridge bearer token. Redacting only
    /// one backend left the other in the durable receipt, so this pins the
    /// property for Claude too.
    #[test]
    fn claude_durable_receipts_redact_bearer_token() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-credential-receipt-claude-credentials-{}",
            Uuid::new_v4()
        ));
        let store = ReceiptStore::for_instance(&home, "agent").expect("receipt store");
        let envelope = DeliveryEnvelope::new(
            "agent",
            SessionLocator::claude(
                "http://127.0.0.1:4096".to_string(),
                "claude-session".to_string(),
                "do-not-persist-bearer".to_string(),
            ),
            DeliveryKind::Prompt,
            "recovery body",
            None,
        );

        let exported = serde_json::to_string(&envelope).expect("export envelope");
        assert!(!exported.contains("do-not-persist-bearer"));
        assert!(!exported.contains("\"password\""));
        store.record_queued(&envelope).expect("queued receipt");
        let durable = std::fs::read_to_string(store.path()).expect("read receipt");
        assert!(!durable.contains("do-not-persist-bearer"));
        assert!(!durable.contains("\"password\""));

        // Non-secret identity still recovers.
        let (recovered, _) = store
            .delivery(envelope.delivery_id)
            .expect("load receipt")
            .expect("durable envelope");
        assert_eq!(recovered.session.password, None);
        assert_eq!(recovered.session.backend, "claude");
        assert_eq!(
            recovered.session.session_id.as_deref(),
            Some("claude-session")
        );
        assert_eq!(recovered.session.username.as_deref(), Some("bearer"));

        let _ = std::fs::remove_dir_all(home);
    }

    /// PR #3495 r4: the store-level non-regression GUARD.
    ///
    /// Every "unreachable" claim about an unconditional writer is replaced by
    /// this invariant: the lowest-level append REFUSES an unconditional write
    /// that would move a self-kick delivery BACKWARDS, or that would drop the
    /// durable notice markers the row is carrying. Only a marker-aware
    /// compare-and-append may change those markers.
    fn self_kick_home(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agend-transport-receipt-{tag}-{}", Uuid::new_v4()))
    }

    fn self_kick_envelope() -> DeliveryEnvelope {
        DeliveryEnvelope::self_kick(
            "claude-agent",
            SessionLocator::claude(
                "http://127.0.0.1:1".to_string(),
                "self-kick-session".to_string(),
                "token".to_string(),
            ),
            "[AGEND-RESUME] id=guard",
        )
    }

    /// (i) An unconditional `record` may not regress `TurnStarted` back to
    /// `ProtocolAccepted` — the shape `deliver_resident`'s post-202 write had.
    #[test]
    fn unconditional_record_refuses_to_regress_a_self_kick_row() {
        let home = self_kick_home("guard-regress");
        let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
        let envelope = self_kick_envelope();
        store.record_queued(&envelope).expect("queued");
        store
            .record(DeliveryReceipt::for_state(
                &envelope,
                DeliveryState::TurnStarted,
            ))
            .expect("turn started");

        let error = store
            .record(DeliveryReceipt::for_state(
                &envelope,
                DeliveryState::ProtocolAccepted,
            ))
            .expect_err("a regressing unconditional append must be refused");
        assert!(
            error.to_string().contains("would regress"),
            "the refusal must name the invariant: {error}"
        );
        assert_eq!(
            store
                .latest(envelope.delivery_id)
                .expect("latest")
                .expect("receipt")
                .state,
            DeliveryState::TurnStarted,
            "and must leave the row untouched"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// (ii) An unconditional append over an `AckOverdue` row carrying an
    /// unsent notice intent must be refused with the intent intact — dropping
    /// it is the loss bug the guard exists to make impossible.
    #[test]
    fn unconditional_record_refuses_to_drop_a_pending_notice_intent() {
        let home = self_kick_home("guard-intent");
        let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
        let envelope = self_kick_envelope();
        store.record_queued(&envelope).expect("queued");
        let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        accepted.protocol_request_id = Some(envelope.delivery_id.to_string());
        store.record(accepted).expect("accepted");
        let mut overdue = DeliveryReceipt::for_state(&envelope, DeliveryState::AckOverdue);
        let intent = PendingNotice::new(PendingNotice::ACK_OVERDUE, None);
        overdue.notice_pending = Some(intent.clone());
        assert!(store
            .record_if_marker(
                envelope.delivery_id,
                DeliveryState::ProtocolAccepted,
                None,
                None,
                overdue,
            )
            .expect("overdue CAS"));

        // A same-state unconditional append that simply forgets the marker.
        let mut forgetful = DeliveryReceipt::for_state(&envelope, DeliveryState::AckOverdue);
        forgetful.detail = Some("a writer that never heard of notice_pending".to_string());
        let error = store
            .record(forgetful)
            .expect_err("dropping a pending notice intent must be refused");
        assert!(
            error.to_string().contains("notice_pending"),
            "the refusal must name the marker: {error}"
        );
        assert_eq!(
            store
                .latest(envelope.delivery_id)
                .expect("latest")
                .expect("receipt")
                .notice_pending,
            Some(intent),
            "the unsent intent survives"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// (iii) The legitimate marker-aware transitions are unaffected: the
    /// guard fences unconditional writers, not the CAS that owns the markers.
    #[test]
    fn marker_aware_cas_transitions_still_apply() {
        let home = self_kick_home("guard-cas");
        let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
        let envelope = self_kick_envelope();
        store.record_queued(&envelope).expect("queued");
        let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        accepted.protocol_request_id = Some(envelope.delivery_id.to_string());
        store.record(accepted).expect("accepted");

        let mut overdue = DeliveryReceipt::for_state(&envelope, DeliveryState::AckOverdue);
        let intent = PendingNotice::new(PendingNotice::ACK_OVERDUE, None);
        overdue.notice_pending = Some(intent.clone());
        assert!(store
            .record_if_marker(
                envelope.delivery_id,
                DeliveryState::ProtocolAccepted,
                None,
                None,
                overdue,
            )
            .expect("overdue CAS"));

        // Clearing the marker is a state-preserving CAS — allowed.
        let mut cleared = DeliveryReceipt::for_state(&envelope, DeliveryState::AckOverdue);
        cleared.notice_pending = None;
        assert!(store
            .record_if_marker(
                envelope.delivery_id,
                DeliveryState::AckOverdue,
                None,
                Some(&intent),
                cleared,
            )
            .expect("clear CAS"));
        let latest = store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt");
        assert_eq!(latest.state, DeliveryState::AckOverdue);
        assert!(latest.notice_pending.is_none());
        let _ = std::fs::remove_dir_all(home);
    }

    /// (iv) Non-self-kick deliveries are untouched by the guard: every other
    /// transport still appends terminal receipts unconditionally, and the
    /// codex/opencode adapters legitimately overwrite a row in place.
    #[test]
    fn non_self_kick_deliveries_are_unaffected_by_the_guard() {
        let home = self_kick_home("guard-other");
        let store = ReceiptStore::for_instance(&home, "agent/one").expect("store");
        let envelope = DeliveryEnvelope::new(
            "agent/one",
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
            DeliveryKind::Prompt,
            "body",
            None,
        );
        assert!(!envelope.self_kick);
        store.record_queued(&envelope).expect("queued");
        store
            .record(DeliveryReceipt::for_state(
                &envelope,
                DeliveryState::TurnStarted,
            ))
            .expect("turn started");
        store
            .record(DeliveryReceipt::for_state(
                &envelope,
                DeliveryState::ProtocolAccepted,
            ))
            .expect("a non-self-kick row keeps its unconditional semantics");
        assert_eq!(
            store
                .latest(envelope.delivery_id)
                .expect("latest")
                .expect("receipt")
                .state,
            DeliveryState::ProtocolAccepted
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
