//! W2.2: unified home for the comms-handler gates.
//!
//! Folds the former sibling files — `sha_gate` (reviewer SHA-staleness),
//! `evidence_gate` (reviewer-evidence), and `anti_stall` (dispatch
//! `task_id` structural gate) — into one module, alongside the
//! `handle_delegate_task` pre-send gate chain (`dispatch`) that used to
//! inline in `comms.rs`. The handlers reach every gate through this facade.

mod anti_stall;
mod dispatch;
mod evidence_gate;
mod sha_gate;

// Report-path gates (handle_report_result).
pub(super) use evidence_gate::{check_evidence_gate, cross_check_and_log, detect_verdict};
pub(super) use sha_gate::{check_sha_gate, fetch_pr_head_sha};

// Send-invariant gate (handle_unified_send).
pub(super) use anti_stall::enforce_send_invariants;

// Delegate-task pre-send gates (handle_delegate_task).
pub(super) use dispatch::run_dispatch_pre_checks;
