//! Typed report-purpose and code-review receipt containment (#2760/task66).
//!
//! External callers may submit a [`CodeReviewRequest`], but only the daemon API
//! sink can turn it into a [`ValidatedCodeReviewReceipt`]. All authority fields
//! are derived from the still-active reviewer assignment; caller text,
//! `reviewed_head`, names, and correlation strings are display/routing data only.

use crate::daemon::pr_state::{PrState, ReviewClass};
use crate::types::InstanceId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportPurpose {
    TaskResult,
    AnalysisDecision,
    SourceSpike,
    Rca,
    CodeReview,
    /// Missing on old durable rows and callers. It remains a normal report but
    /// has no code-review authority.
    #[default]
    LegacyUntyped,
}

impl ReportPurpose {
    fn parse_external(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None => Ok(Self::LegacyUntyped),
            Some("task_result") => Ok(Self::TaskResult),
            Some("analysis_decision") => Ok(Self::AnalysisDecision),
            Some("source_spike") => Ok(Self::SourceSpike),
            Some("rca") => Ok(Self::Rca),
            Some("code_review") => Ok(Self::CodeReview),
            Some(other) => Err(format!(
                "invalid report_purpose '{other}'; expected task_result, analysis_decision, source_spike, rca, or code_review"
            )),
        }
    }

    pub(crate) fn is_legacy(&self) -> bool {
        matches!(self, Self::LegacyUntyped)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewVerdict {
    Verified,
    Rejected,
    Unverified,
}

impl ReviewVerdict {
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Verified => "VERIFIED",
            Self::Rejected => "REJECTED",
            Self::Unverified => "UNVERIFIED",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewSlot {
    Primary,
    Secondary,
}

/// Caller-supplied, non-authoritative request. The API sink validates it against
/// the active assignment and constructs the private validated type below.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeReviewRequest {
    pub assignment_id: uuid::Uuid,
    pub verdict: ReviewVerdict,
    pub evidence_digest: String,
}

/// Durable subject included on a reviewer-assignment inbox row. This is the data
/// a reviewer uses to form a later request; it is not itself a verdict receipt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReviewAssignmentEnvelope {
    pub assignment_id: uuid::Uuid,
    pub repo: String,
    pub pr_number: u64,
    pub branch: String,
    pub task_id: String,
    pub reviewed_head: String,
    pub review_class: ReviewClass,
    pub slot: ReviewSlot,
    pub target_instance_id: InstanceId,
}

/// Durable summary retained in PR state/buffer. Reviewer name is display-only;
/// stable instance id + assignment id are the authority identities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReviewReceiptSummary {
    pub receipt_id: String,
    pub source_id: String,
    pub evidence_digest: String,
    pub assignment_id: uuid::Uuid,
    pub reviewer_instance_id: InstanceId,
    pub reviewer_name: String,
    pub repo: String,
    pub pr_number: u64,
    pub branch: String,
    pub task_id: String,
    pub reviewed_head: String,
    pub review_class: ReviewClass,
    pub slot: ReviewSlot,
    pub verdict: ReviewVerdict,
}

impl ReviewReceiptSummary {
    pub(crate) fn matches_state(&self, state: &PrState) -> bool {
        self.repo == state.repo
            && self.pr_number == state.pr_number
            && self.branch == state.branch
            && self.reviewed_head == state.head_sha
            && self.review_class == state.review_class
    }
}

/// Server-constructed proof. Fields stay private so transport/API callers cannot
/// manufacture one inside the Rust process; they can only submit the request
/// above and pass the authoritative sink.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct ValidatedCodeReviewReceipt(ReviewReceiptSummary);

impl ValidatedCodeReviewReceipt {
    pub(crate) fn summary(&self) -> &ReviewReceiptSummary {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(summary: ReviewReceiptSummary) -> Self {
        Self(summary)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ReportAuthorization {
    pub purpose: ReportPurpose,
    pub receipt: Option<ValidatedCodeReviewReceipt>,
}

const REVIEW_REQUEST_KEYS: &[&str] = &[
    "code_review",
    "assignment_id",
    "verdict",
    "evidence_digest",
    "receipt_id",
    "source_id",
    "source_ids",
    "review_receipt",
    "validated_code_review",
];

pub(crate) fn has_flat_review_smuggling_fields(params: &Value) -> bool {
    REVIEW_REQUEST_KEYS[1..]
        .iter()
        .any(|k| params.get(*k).is_some_and(|v| !v.is_null()))
}

/// Authoritative API-sink validation. Must run before delivery.
pub(crate) fn authorize_report(
    home: &Path,
    params: &Value,
    sender_name: &str,
    sender_id: Option<InstanceId>,
    recipient: &str,
    visible_text: &str,
    server_message_id: &str,
) -> Result<ReportAuthorization, String> {
    let is_report = params["kind"].as_str() == Some("report");
    let has_report_fields = params
        .get("report_purpose")
        .is_some_and(|value| !value.is_null())
        || REVIEW_REQUEST_KEYS
            .iter()
            .any(|k| params.get(*k).is_some_and(|value| !value.is_null()));
    if !is_report {
        if has_report_fields {
            return Err("report-purpose/code-review fields are only valid on kind=report".into());
        }
        return Ok(ReportAuthorization::default());
    }

    let purpose = ReportPurpose::parse_external(params["report_purpose"].as_str())?;
    let nested = params.get("code_review").filter(|v| !v.is_null());
    let has_flat_smuggle = REVIEW_REQUEST_KEYS[1..]
        .iter()
        .any(|k| params.get(*k).is_some_and(|value| !value.is_null()));
    if has_flat_smuggle {
        return Err(
            "code-review request fields must be inside the typed code_review object".into(),
        );
    }

    if !matches!(purpose, ReportPurpose::CodeReview) {
        if nested.is_some() {
            return Err(format!(
                "report purpose {:?} cannot carry code_review fields",
                purpose
            ));
        }
        return Ok(ReportAuthorization {
            purpose,
            receipt: None,
        });
    }

    let request: CodeReviewRequest =
        serde_json::from_value(nested.cloned().ok_or_else(|| {
            "code_review purpose requires a typed code_review object".to_string()
        })?)
        .map_err(|e| format!("invalid code_review request: {e}"))?;
    validate_request_shape(&request, visible_text)?;
    if server_message_id.is_empty() {
        return Err("server failed to assign a source message id".into());
    }
    let sender_id = sender_id.ok_or_else(|| {
        "code_review sender has no stable fleet InstanceId; authority denied".to_string()
    })?;
    let assignment = crate::daemon::assignment_authority::lookup_by_assignment_id_strict(
        home,
        request.assignment_id,
    )
    .map_err(|e| format!("code_review assignment rejected: {e}"))?;
    if !assignment.is_receipt_capable() {
        return Err("legacy assignment is not receipt-capable; re-dispatch required".into());
    }

    let assigned_instance_id = assignment.target_instance_id.ok_or_else(|| {
        "legacy assignment lacks target InstanceId; re-dispatch required".to_string()
    })?;
    let reviewed_head = assignment.reviewed_head.clone().ok_or_else(|| {
        "legacy assignment lacks exact reviewed head; re-dispatch required".to_string()
    })?;
    let slot = assignment
        .review_slot
        .ok_or_else(|| "legacy assignment lacks review slot; re-dispatch required".to_string())?;
    if assignment.target != sender_name || assigned_instance_id != sender_id {
        return Err("authenticated reviewer identity does not match active assignment".into());
    }
    if assignment.sender != recipient {
        return Err("report recipient does not match the assignment issuer".into());
    }
    if params["correlation_id"].as_str() != Some(assignment.task_id.as_str()) {
        return Err("report correlation_id does not match the assignment task".into());
    }
    if !is_full_head(&reviewed_head) {
        return Err("assignment reviewed head is not a full 40/64-hex SHA".into());
    }
    if matches!(assignment.review_class, ReviewClass::Unresolved) {
        return Err("assignment review class is unresolved".into());
    }

    let state = load_pr_state_strict(home, &assignment.repo, &assignment.branch)?;
    if state.repo != assignment.repo
        || state.branch != assignment.branch
        || state.pr_number != assignment.pr_number
        || state.head_sha != reviewed_head
        || state.review_class != assignment.review_class
    {
        return Err("active assignment subject no longer exactly matches PR state".into());
    }

    Ok(ReportAuthorization {
        purpose,
        receipt: Some(ValidatedCodeReviewReceipt(ReviewReceiptSummary {
            // A2: both exact-once identities originate at the authenticated
            // API sink. Callers cannot choose either key and therefore cannot
            // collide with, suppress, or replay another review's receipt.
            receipt_id: format!("review-receipt:{server_message_id}"),
            source_id: server_message_id.to_string(),
            evidence_digest: request.evidence_digest.to_ascii_lowercase(),
            assignment_id: assignment.assignment_id,
            reviewer_instance_id: sender_id,
            reviewer_name: assignment.target,
            repo: assignment.repo,
            pr_number: assignment.pr_number,
            branch: assignment.branch,
            task_id: assignment.task_id,
            reviewed_head,
            review_class: assignment.review_class,
            slot,
            verdict: request.verdict,
        })),
    })
}

fn validate_request_shape(request: &CodeReviewRequest, visible_text: &str) -> Result<(), String> {
    if request.evidence_digest.len() != 64
        || !request
            .evidence_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err("code_review evidence_digest must be a full 64-hex SHA-256".into());
    }
    let visible = visible_verdict(visible_text)
        .ok_or_else(|| "code_review text must begin with an explicit verdict token".to_string())?;
    if visible != request.verdict {
        return Err(format!(
            "code_review verdict mismatch: enum={} text={}",
            request.verdict.token(),
            visible.token()
        ));
    }
    let body = crate::daemon::auto_release::strip_report_wrapper(visible_text);
    if !matches!(request.verdict, ReviewVerdict::Unverified)
        && (!body.contains("### Evidence") || !(body.contains("ran:") || body.contains("cited:")))
    {
        return Err(
            "VERIFIED/REJECTED code_review requires an ### Evidence block with ran: or cited:"
                .into(),
        );
    }
    Ok(())
}

fn visible_verdict(text: &str) -> Option<ReviewVerdict> {
    let text = crate::daemon::auto_release::strip_report_wrapper(text).trim_start();
    for (token, verdict) in [
        ("VERIFIED", ReviewVerdict::Verified),
        ("REJECTED", ReviewVerdict::Rejected),
        ("UNVERIFIED", ReviewVerdict::Unverified),
    ] {
        if let Some(rest) = text.strip_prefix(token) {
            if rest.is_empty()
                || rest.starts_with(char::is_whitespace)
                || rest.starts_with('—')
                || rest.starts_with(':')
            {
                return Some(verdict);
            }
        }
    }
    None
}

pub(crate) fn is_full_head(head: &str) -> bool {
    matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Recheck the active assignment immediately before a PR-side effect or buffered
/// replay. A validation that raced revoke/transfer/terminal must become inert.
pub(crate) fn assignment_still_authorizes(home: &Path, summary: &ReviewReceiptSummary) -> bool {
    let Ok(assignment) = crate::daemon::assignment_authority::lookup_by_assignment_id_strict(
        home,
        summary.assignment_id,
    ) else {
        return false;
    };
    assignment.is_receipt_capable()
        && assignment.target_instance_id == Some(summary.reviewer_instance_id)
        && assignment.target == summary.reviewer_name
        && assignment.repo == summary.repo
        && assignment.pr_number == summary.pr_number
        && assignment.branch == summary.branch
        && assignment.task_id == summary.task_id
        && assignment.reviewed_head.as_deref() == Some(summary.reviewed_head.as_str())
        && assignment.review_class == summary.review_class
        && assignment.review_slot == Some(summary.slot)
}

pub(crate) fn load_pr_state_strict(
    home: &Path,
    repo: &str,
    branch: &str,
) -> Result<PrState, String> {
    let path = crate::daemon::pr_state::pr_state_dir(home)
        .join(crate::daemon::pr_state::pr_state_filename(repo, branch));
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("cannot read assigned PR state {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("cannot parse assigned PR state {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonreport_transport_nulls_are_inert_but_values_are_rejected_2760() {
        let mut params = serde_json::json!({
            "kind": "task",
            "report_purpose": null,
            "code_review": null,
        });
        assert!(authorize_report(
            Path::new("/unused"),
            &params,
            "sender",
            None,
            "target",
            "task body",
            "m-test",
        )
        .is_ok());

        params["report_purpose"] = serde_json::json!("analysis_decision");
        assert!(authorize_report(
            Path::new("/unused"),
            &params,
            "sender",
            None,
            "target",
            "task body",
            "m-test",
        )
        .is_err());
    }
}
