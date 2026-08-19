use super::*;

pub(super) fn restore_prepared(
    home: &Path,
    instance: &str,
    inbound: &mut InboundIndex,
    prepared: HashMap<Uuid, InboundIdentity>,
) -> anyhow::Result<()> {
    let receipt_store = ReceiptStore::for_instance(home, instance)?;
    for (delivery_id, identity) in prepared {
        inbound
            .by_chat
            .insert(identity.chat_id.clone(), delivery_id);
        inbound.by_delivery.insert(delivery_id, identity);
        let Some((envelope, current)) = receipt_store.delivery(delivery_id)? else {
            continue;
        };
        if !matches!(
            current.state,
            DeliveryState::Queued
                | DeliveryState::ProtocolAccepted
                | DeliveryState::ObservedInSession
        ) {
            continue;
        }
        let mut ambiguous = DeliveryReceipt::for_state(&envelope, DeliveryState::Ambiguous);
        ambiguous.protocol_request_id = Some(delivery_id.to_string());
        ambiguous.backend_event = Some("channel_prepared_replay".to_string());
        ambiguous.detail = Some(
            "prepared admission survived bridge restart; notification may have been emitted, so automatic replay is suppressed"
                .to_string(),
        );
        let _ = receipt_store.record_if_latest_state(delivery_id, current.state, ambiguous)?;
    }
    Ok(())
}
