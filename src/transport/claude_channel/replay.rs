use super::*;

pub(super) fn restore_prepared(
    home: &Path,
    instance: &str,
    inbound: &mut InboundIndex,
    prepared: HashMap<Uuid, InboundIdentity>,
) -> anyhow::Result<()> {
    let receipt_store = ReceiptStore::for_instance(home, instance)?;
    let total = prepared.len();
    let mut stamped = 0usize;
    for (delivery_id, identity) in prepared {
        inbound
            .by_chat
            .insert(identity.chat_id.clone(), delivery_id);
        inbound.by_delivery.insert(delivery_id, identity);
        if let Some((envelope, current)) = receipt_store.delivery(delivery_id)? {
            if matches!(
                current.state,
                DeliveryState::Queued
                    | DeliveryState::ProtocolAccepted
                    | DeliveryState::ObservedInSession
            ) {
                let mut ambiguous = DeliveryReceipt::for_state(&envelope, DeliveryState::Ambiguous);
                ambiguous.protocol_request_id = Some(delivery_id.to_string());
                ambiguous.backend_event = Some("channel_prepared_replay".to_string());
                ambiguous.detail = Some(
                    "prepared admission survived bridge restart; notification may have been emitted, so automatic replay is suppressed"
                        .to_string(),
                );
                // PR #3495 r3: carry any durable notice markers across, and
                // compare them in the CAS. No row in the three states above
                // can carry one today, so this is behaviour-preserving — it
                // just makes the sweep incapable of silently discarding an
                // intent a future writer parks on a pre-terminal receipt.
                ambiguous.late_ack_secs = current.late_ack_secs;
                ambiguous.notice_pending = current.notice_pending.clone();
                if receipt_store.record_if_marker(
                    delivery_id,
                    current.state,
                    current.late_ack_secs,
                    current.notice_pending.as_ref(),
                    ambiguous,
                )? {
                    stamped += 1;
                }
            }
        }
        // #3310: RETIRE the row in the journal so the next restart nets it
        // out of `prepared` instead of replaying it forever (765 legacy rows
        // on the owner host replayed on every restart; the one-shot sweep
        // stamped 1636 receipts fleet-wide). `InboundRejected` is reused
        // deliberately: its load semantic — `prepared.remove` — is exactly
        // the retirement wanted here, and pre-#3310 readers already
        // understand the variant. Stamp-then-retire order keeps the crash
        // window convergent: a crash between the two leaves an Ambiguous
        // receipt that the next sweep skips (state filter) and re-retires.
        append_log(
            home,
            instance,
            &ChannelLogRecord::InboundRejected {
                delivery_id,
                recorded_at: Utc::now().to_rfc3339(),
            },
        )?;
    }
    if total > 0 {
        // #3310: the sweep used to be silent — 1636 receipt-state flips in
        // ten seconds with no operator-visible trace. One line per pass.
        tracing::warn!(
            instance = %instance,
            total,
            stamped,
            "#3310 bridge restart retired {total} replayed prepared admission(s) \
             ({stamped} stamped Ambiguous; automatic replay suppressed)"
        );
    }
    Ok(())
}
