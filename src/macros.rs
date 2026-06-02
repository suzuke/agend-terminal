//! #1630: macros that make silently dropping a persistence/enqueue `Result`
//! a deliberate, compile-visible choice rather than an invisible `let _ = …`.

/// Log-and-continue for a persistence/enqueue call that is genuinely
/// fire-and-forget — it runs inside a per-tick loop / event handler whose
/// caller has no meaningful way to handle the `Err` (the #1630 silent-message-
/// loss class: a dropped inbox `enqueue` `Result` means the message never lands
/// on disk and the recipient never sees it).
///
/// On `Err`, emit a `tracing::error!` (not `warn!`) tagged with the `op` label
/// — and the `target` recipient where one is in scope — so a dropped message is
/// POST-HOC DIAGNOSABLE in the daemon log. This is a diagnosis breadcrumb, not
/// loss-prevention; **where the enclosing fn returns `Result`, propagate the
/// `Err` with `?`/`return` instead of using this macro.**
///
/// The happy path is unchanged — `Ok(_)` is discarded exactly as the prior
/// `let _ = …` did; this only upgrades the silent drop to a logged drop.
///
/// Enforced by `tests/enqueue_drop_invariant.rs`: a bare `let _ = …enqueue…(`
/// (or `.ok()` / bare-`;`) in production code fails CI — route it through this
/// macro (or propagate) instead.
///
/// ```ignore
/// persist_or_log!(crate::inbox::enqueue_with_idle_hint(home, target, msg), "schedule_replay", target);
/// persist_or_log!(crate::inbox::enqueue(home, "general", msg), "boot_orphan_sweep");
/// ```
macro_rules! persist_or_log {
    ($call:expr, $op:expr $(,)?) => {
        if let Err(e) = $call {
            tracing::error!(error = %e, op = $op, "enqueue failed — message dropped (silent-loss #1630)");
        }
    };
    ($call:expr, $op:expr, $target:expr $(,)?) => {
        if let Err(e) = $call {
            tracing::error!(error = %e, op = $op, target = %$target, "enqueue failed — message dropped (silent-loss #1630)");
        }
    };
}
