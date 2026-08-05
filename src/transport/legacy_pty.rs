use super::{
    AgentDeliveryTransport, DeliveryEnvelope, DeliveryReceipt, DeliveryState, SessionLocator,
    TransportCapability, TransportMode,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) type PtyInjector = Arc<dyn Fn(&Path, &str, &str) -> anyhow::Result<()> + Send + Sync>;

/// Explicit compatibility adapter for backends without a verified shared
/// protocol. `typed_inject` and all existing PTY behavior stay behind this
/// boundary; no screen/readback result is promoted to a backend receipt.
pub(crate) struct LegacyPty {
    home: PathBuf,
    instance: String,
    injector: PtyInjector,
}

impl LegacyPty {
    pub(crate) fn new(home: &Path, instance: &str, injector: PtyInjector) -> Self {
        Self {
            home: home.to_path_buf(),
            instance: instance.to_string(),
            injector,
        }
    }

    fn capability(&self) -> TransportCapability {
        TransportCapability {
            backend: "legacy-pty".to_string(),
            mode: TransportMode::LegacyPty,
            ready: true,
            backend_version: None,
            degraded_reason: Some(
                "backend has no verified structured shared transport".to_string(),
            ),
        }
    }

    pub(crate) fn deliver_blocking(
        &mut self,
        envelope: DeliveryEnvelope,
    ) -> anyhow::Result<DeliveryReceipt> {
        let store = super::ReceiptStore::for_instance(&self.home, &self.instance)?;
        store.record_queued(&envelope)?;
        let result = (self.injector)(&self.home, &self.instance, &envelope.body);
        let mut receipt = DeliveryReceipt::for_state(
            &envelope,
            if result.is_ok() {
                DeliveryState::Ambiguous
            } else {
                DeliveryState::Failed
            },
        );
        receipt.tui_visibility = Some("unknown".to_string());
        receipt.detail = Some(if result.is_ok() {
            "legacy PTY write completed; backend acceptance is unproven".to_string()
        } else {
            "legacy PTY write failed".to_string()
        });
        if let Err(record_error) = store.record(receipt.clone()) {
            return Err(anyhow::anyhow!(
                "receipt persistence failed: {record_error}"
            ));
        }
        result.map(|()| receipt)
    }
}

#[async_trait::async_trait]
impl AgentDeliveryTransport for LegacyPty {
    fn mode(&self) -> TransportMode {
        TransportMode::LegacyPty
    }

    async fn start_or_attach(
        &mut self,
        _locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        Ok(self.capability())
    }

    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt> {
        self.deliver_blocking(envelope)
    }

    async fn next_event(&mut self) -> anyhow::Result<super::BackendEvent> {
        Err(anyhow::anyhow!("LegacyPty has no structured event stream"))
    }

    async fn reconcile(&mut self, delivery_id: uuid::Uuid) -> anyhow::Result<DeliveryState> {
        let store = super::ReceiptStore::for_instance(&self.home, &self.instance)?;
        Ok(store
            .latest(delivery_id)?
            .map(|receipt| receipt.state)
            .unwrap_or(DeliveryState::Ambiguous))
    }

    async fn health(&self) -> TransportCapability {
        self.capability()
    }
}
