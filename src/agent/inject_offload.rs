//! AUDIT2-006 C: cron PTY-inject offload.
//!
//! `inject_with_target_gated` historically did three things inline on the caller's
//! (tick) thread: prepend the #1769 daemon-auto marker, run the #1513 defer gate
//! (a *durable* `notification_queue` enqueue when the agent is busy/typing), and —
//! if not deferred — perform the blocking physical PTY write (typed-inject
//! readback, ≤5s per write, possibly several writes).
//!
//! This module splits the SYNCHRONOUS prepare/gate phase ([`prepare_inject`]) from
//! the physical delivery, so cron can offload ONLY the physical write to the
//! bounded delivery worker (AUDIT2-006 A+B) while the durable defer-gate stays on
//! the caller's thread. `inject_with_target_gated` (the inline variant) is
//! unchanged for every existing caller — API / supervisor / per-tick / replay all
//! keep byte-equivalent synchronous delivery. Only cron opts into
//! [`inject_with_target_gated_offload`].
//!
//! [`prepare_inject`] is the SOLE implementation of the marker + #1513 gate; the
//! offload path must never re-prepend the marker or re-check the gate.

/// Result of the synchronous prepare/gate phase shared by the inline and offload
/// inject paths.
pub(crate) enum InjectPrep {
    /// The #1513 gate fired: the wake was durably enqueued (synchronously) to the
    /// `notification_queue` for the per-tick flush. Carries the enqueue `Result`,
    /// which the caller surfaces unchanged.
    Deferred(crate::error::Result<()>),
    /// Not deferred — proceed to the physical inject with these (marker-prepended,
    /// owned) bytes. The caller chooses inline vs offloaded delivery.
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
    // HERE so the tag survives whichever delivery path runs. `None` (operator relay
    // / api INJECT / inbox — already carrying their own headers) is verbatim.
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
                // Gated direct injects (cron / replay) are UTF-8 text wakes; enqueue
                // ambient-class for the per-tick flush to drain once the pane settles
                // (the flush re-injects via the api INJECT path with force=true —
                // byte-equivalent). #1630: this enqueue IS the deferred-delivery path
                // — if it fails the wake is lost, so the Result is propagated, not
                // swallowed. #t-3558 P2: an AGEND-AUTO nudge (auto_kind set) routes
                // through the coalescing enqueue (keep-latest) so a non-draining agent
                // can't stack identical same-kind retry nudges.
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

/// Outcome of [`inject_with_target_gated_offload`]. `Queued` means "accepted by the
/// bounded delivery queue", NOT "physically delivered": if the target agent is
/// deleted before the worker dispatches, the physical inject no-ops (the captured
/// `InjectTarget.deleted` flag) and the recorded status stays at the queued value —
/// queue-accept is the durability boundary by design.
pub(crate) enum InjectDispatch {
    /// The #1513 gate fired — durably enqueued (synchronous). Carries the enqueue
    /// `Result` so the caller maps Ok/Err exactly as the inline path would.
    Deferred(crate::error::Result<()>),
    /// The physical write was accepted by the bounded delivery worker (not yet
    /// delivered).
    Queued,
    /// The bounded delivery queue was full — the wake was dropped. The caller
    /// records a drop status and WARNs.
    QueueFull,
}

/// AUDIT2-006 C: cron-only variant of [`super::inject_with_target_gated`] that
/// OFFLOADS the physical PTY write to the bounded delivery worker, so the tick
/// thread never blocks on the typed-inject readback. The #1513 defer gate runs
/// SYNCHRONOUSLY here (its enqueue is a durable source-of-truth write); only the
/// physical inject moves off-thread.
///
/// The worker uses the CAPTURED `InjectTarget`, never re-resolving the name — a
/// same-name redeploy must not receive a stale cron fire. `Queued` is NOT
/// "delivered" (see [`InjectDispatch`]).
///
/// CRON ONLY. Recovery / force / API paths need synchronous delivery and must keep
/// using `inject_with_target_gated`; `tests/cron_offload_caller_invariant.rs` pins
/// this fn to its single cron call site.
pub(crate) fn inject_with_target_gated_offload(
    target: &super::InjectTarget,
    name: &str,
    text: &[u8],
) -> InjectDispatch {
    match prepare_inject(name, text, false, None) {
        InjectPrep::Deferred(r) => InjectDispatch::Deferred(r),
        InjectPrep::Proceed(marked) => {
            match crate::daemon::delivery_worker::enqueue_cron_inject(target.clone(), name, marked)
            {
                Ok(()) => InjectDispatch::Queued,
                Err(()) => InjectDispatch::QueueFull,
            }
        }
    }
}

/// Physical-only inject for the delivery worker. The caller MUST have already run
/// the prepare/gate phase (marker + #1513 gate) — this performs ONLY the physical
/// PTY write, on the worker thread. Distinct from `run_ephemeral_inject` (which is
/// bound to the headless ephemeral driver); reusing that would pollute its
/// semantics.
pub(crate) fn inject_target_physical(
    target: &super::InjectTarget,
    text: &[u8],
) -> crate::error::Result<()> {
    super::inject_with_target(target, text)
}
