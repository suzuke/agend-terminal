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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DeliveryState {
    Queued,
    ProtocolAccepted,
    ObservedInSession,
    TurnStarted,
    Completed,
    Failed,
    Ambiguous,
}

impl DeliveryState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Ambiguous)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeliveryReceipt {
    pub delivery_id: Uuid,
    pub state: DeliveryState,
    pub payload_digest: String,
    pub protocol_request_id: Option<String>,
    pub backend_event: Option<String>,
    pub tui_visibility: Option<String>,
    pub detail: Option<String>,
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
            backend_event: None,
            tui_visibility: None,
            detail: None,
            recorded_at: Utc::now().to_rfc3339(),
        }
    }
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
        let receipt = DeliveryReceipt::queued(envelope);
        self.append(DurableRecord {
            envelope: Some(envelope.clone()),
            receipt: receipt.clone(),
        })?;
        Ok(receipt)
    }

    pub(crate) fn record(&self, receipt: DeliveryReceipt) -> anyhow::Result<()> {
        self.append(DurableRecord {
            envelope: None,
            receipt,
        })
    }

    pub(crate) fn latest(&self, delivery_id: Uuid) -> anyhow::Result<Option<DeliveryReceipt>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
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

    /// Return durable envelopes whose latest receipt is not terminal.  A
    /// structured adapter is intentionally short-lived per worker job, so a
    /// fresh adapter must restore the in-flight turn before deciding whether a
    /// second prompt may be sent.
    pub(crate) fn pending_deliveries(
        &self,
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
                if receipt.state.is_terminal() {
                    return None;
                }
                Some((envelope?, receipt))
            })
            .collect())
    }

    pub(crate) fn latest_protocol_request_id_with_prefix(
        &self,
        prefix: &str,
    ) -> anyhow::Result<Option<String>> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
        if !self.path.exists() {
            return Ok(None);
        }
        restrict_permissions(&self.path, 0o600)?;
        let file = File::open(&self.path)?;
        let mut latest = None;
        for line in BufReader::new(file).lines() {
            let record: DurableRecord = serde_json::from_str(&line?)?;
            let Some(request_id) = record.receipt.protocol_request_id.as_deref() else {
                continue;
            };
            if !request_id.starts_with(prefix) {
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

    fn append(&self, record: DurableRecord) -> anyhow::Result<()> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        restrict_permissions(&self.lock_path(), 0o600)?;
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
        let envelope = DeliveryEnvelope::new(
            "agent/one",
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
            DeliveryKind::Prompt,
            "secret body",
            None,
        );
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
        assert!(std::fs::read_to_string(store.path())
            .expect("read")
            .contains("secret body"));
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
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
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
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("thread".to_string())),
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
                .latest_protocol_request_id_with_prefix("msg_")
                .expect("latest protocol request id"),
            Some(protocol_request_id)
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
