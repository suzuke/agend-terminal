use super::envelope::DeliveryEnvelope;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

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

/// Append-only receipt/audit store. The envelope is included only on the
/// initial queued record; later entries carry a digest, never a second copy of
/// the payload. A separate append-only record makes restart reconciliation
/// deterministic and avoids claiming exactly-once delivery.
#[derive(Debug, Clone)]
pub(crate) struct ReceiptStore {
    path: PathBuf,
}

impl ReceiptStore {
    pub(crate) fn for_instance(home: &Path, instance: &str) -> anyhow::Result<Self> {
        let dir = home.join("transport").join("deliveries");
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            path: dir.join(format!("{}.jsonl", safe_component(instance))),
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
        if !self.path.exists() {
            return Ok(None);
        }
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

    fn append(&self, record: DurableRecord) -> anyhow::Result<()> {
        let _lock = crate::store::acquire_file_lock(&self.lock_path())?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("jsonl.lock")
    }
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
}
