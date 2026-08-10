//! Shared typed-inject prepare/defer gate.
//!
//! [`prepare_inject`] keeps the daemon-auto marker and durable #1513 gate in one
//! place before a direct PTY caller performs its physical write. Backend-aware
//! notifications, including cron, use the transport scheduler instead and do not
//! enter this PTY path.

/// Result of the synchronous prepare/gate phase for a direct PTY inject.
pub(crate) enum InjectPrep {
    /// The #1513 gate fired: the wake was durably enqueued (synchronously) to the
    /// `notification_queue` for the per-tick flush. Carries the enqueue `Result`,
    /// which the caller surfaces unchanged.
    Deferred(crate::error::Result<()>),
    /// Not deferred — proceed to the physical inject with these marker-prepended,
    /// owned bytes.
    Proceed(Vec<u8>),
}

/// The synchronous prepare/gate phase: #1769 marker prepend + #1513 env-conditional
/// defer gate. SOLE implementation of both — callers must not duplicate either.
/// `force` skips the gate (operator relay / api INJECT / recovery); `auto_kind`
/// adds the daemon-auto marker (and routes a busy-agent defer through the
/// coalescing enqueue, keep-latest per #t-3558).
pub(crate) fn prepare_inject(
    name: &str,
    text: &[u8],
    force: bool,
    auto_kind: Option<&str>,
) -> InjectPrep {
    // #1769: daemon self-originated auto-injects carry an identifying marker so an
    // orchestrator can distinguish them from real operator/peer input. Prepended
    // HERE so the tag survives whichever direct PTY path runs. `None` (operator
    // relay / api INJECT / inbox — already carrying headers) is verbatim.
    let marked: Vec<u8> = match auto_kind {
        Some(kind) => [super::daemon_auto_prefix(kind).as_bytes(), text].concat(),
        None => text.to_vec(),
    };
    // #1513 PR-2: gate direct PTY injects like the notification path. Self-contained
    // via AGEND_HOME; AGEND_HOME absent (non-daemon / unit test) → gate skipped.
    if !force {
        if let Ok(home) = std::env::var("AGEND_HOME") {
            let home = std::path::Path::new(&home);
            if crate::inbox::notify::should_defer_direct_inject(home, name) {
                // Gated direct injects (replay/recovery) are UTF-8 text wakes;
                // enqueue ambient-class for the per-tick flush to drain once the
                // pane settles (the flush re-injects through api INJECT with
                // force=true). #1630: this enqueue IS the deferred-delivery path,
                // so its error is propagated. #t-3558 P2: an AGEND-AUTO nudge
                // routes through the coalescing keep-latest queue.
                let text_str = String::from_utf8_lossy(&marked);
                let enq = if auto_kind.is_some() {
                    crate::notification_queue::enqueue_coalesced_auto(home, name, &text_str)
                } else {
                    crate::notification_queue::enqueue_classified(home, name, &text_str, false)
                };
                return InjectPrep::Deferred(enq.map_err(|e| {
                    crate::error::AgendError::ApiError(format!("deferred enqueue: {e}"))
                }));
            }
        }
    }
    InjectPrep::Proceed(marked)
}
