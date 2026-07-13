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
mod request_kind_gate;
mod sha_gate;
mod triaged_gate;

// Report-path gates (handle_report_result).
pub(super) use evidence_gate::{check_evidence_gate, cross_check_and_log};
// #t-127: `detect_verdict` + `Verdict` are also consumed by the api-layer
// dispatch tracker (the verdict→review-task bridge in `track_dispatch`), so they
// are crate-visible, not just `pub(super)`.
pub(crate) use evidence_gate::{detect_verdict, Verdict};
pub(super) use sha_gate::{check_sha_gate, fetch_pr_head_sha, report_scan_body};

// Send-invariant gate (handle_unified_send).
pub(super) use anti_stall::enforce_send_invariants;

// t-20260705005551919287-14440-22: request_kind enum validation
// (handle_unified_send / handle_broadcast).
pub(super) use request_kind_gate::validate_request_kind;

// Delegate-task pre-send gates (handle_delegate_task / comms_delegate).
// t-…-17: `ReviewAuthor` is consumed by the reviewer-assignment marker gate in
// `comms_delegate`.
pub(super) use dispatch::{run_dispatch_pre_checks, DispatchPreChecks, ReviewAuthor};

// #2537/#2524 P6: triaged pre-send gate (handle_send_to_instance / handle_report_result).
pub(super) use triaged_gate::{record_triaged_if_present, validate_triaged};
