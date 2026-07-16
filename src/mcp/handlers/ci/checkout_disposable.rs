use serde_json::{json, Value};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckoutPurpose {
    DisposableReview,
}

impl CheckoutPurpose {
    pub(super) fn provenance(
        self,
        provisioned_head: &str,
    ) -> crate::binding::BindingProvenance<'_> {
        match self {
            Self::DisposableReview => {
                crate::binding::BindingProvenance::DaemonProvisionedReview { provisioned_head }
            }
        }
    }
}

pub(super) fn parse(args: &Value, bind: bool) -> Result<Option<CheckoutPurpose>, Value> {
    let purpose = match args.get("checkout_purpose") {
        None => None,
        Some(Value::String(value)) if value == "disposable_review" => {
            Some(CheckoutPurpose::DisposableReview)
        }
        Some(Value::String(value)) => {
            return Err(json!({
                "error": format!("unsupported checkout_purpose '{value}'"),
                "code": "invalid_checkout_purpose",
            }));
        }
        Some(_) => {
            return Err(json!({
                "error": "checkout_purpose must be a string",
                "code": "invalid_checkout_purpose",
            }));
        }
    };
    if purpose == Some(CheckoutPurpose::DisposableReview) {
        if !bind {
            return Err(json!({
                "error": "checkout_purpose=disposable_review requires bind=true",
                "code": "disposable_review_requires_bind",
            }));
        }
        if args["task_id"].as_str().is_none_or(str::is_empty) {
            return Err(json!({
                "error": "checkout_purpose=disposable_review requires a non-empty task_id",
                "code": "disposable_review_requires_task_id",
            }));
        }
        if args["expected_head"].as_str().is_none_or(str::is_empty) {
            return Err(json!({
                "error": "checkout_purpose=disposable_review requires expected_head",
                "code": "disposable_review_requires_expected_head",
            }));
        }
    }
    Ok(purpose)
}

pub(super) fn preflight_branch(source_path: &Path, branch: &str) -> Result<(), Value> {
    let branch_ref = format!("refs/heads/{branch}");
    let already_exists =
        crate::git_helpers::git_cmd(source_path, &["rev-parse", "--verify", &branch_ref]).is_ok();
    // `ensure_branch_exists` also materializes a local branch from an
    // existing `origin/<branch>` tracking ref and reports `created=false`.
    // Reject that state before provisioning so a disposable checkout never
    // leaves a daemon-created local ref behind after discovering remote
    // provenance.
    let remote_tracking_ref = format!("refs/remotes/origin/{branch}");
    let remote_already_exists = crate::git_helpers::git_cmd(
        source_path,
        &["rev-parse", "--verify", &remote_tracking_ref],
    )
    .is_ok();
    if already_exists || remote_already_exists {
        return Err(json!({
            "error": format!("disposable review branch '{branch}' already exists"),
            "code": "disposable_review_requires_new_branch",
            "auto_created_branch": false,
            "branch": branch,
        }));
    }
    // Query the exact remote ref instead of relying on remote.origin.fetch:
    // narrow/negative refspecs may make a fetch succeed while omitting a live
    // review branch. ls-remote does not mutate local refs and proves absence
    // independently of the configured fetch refspec.
    match crate::git_helpers::git_bypass(source_path, &["remote", "get-url", "origin"]) {
        Ok(remote) if remote.status.success() => {
            let remote_ref = format!("refs/heads/{branch}");
            let queried = crate::git_helpers::git_bypass_timeout(
                source_path,
                &["ls-remote", "--heads", "origin", &remote_ref],
                std::time::Duration::from_secs(12),
            );
            let output = match queried {
                Ok(output) if output.status.success() => output,
                _ => {
                    return Err(json!({
                        "error": format!("could not prove disposable review branch '{branch}' is absent from origin"),
                        "code": "disposable_review_branch_absence_unproven",
                        "auto_created_branch": false,
                        "branch": branch,
                    }));
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut lines = stdout.lines();
            if let Some(line) = lines.next() {
                let mut fields = line.split_whitespace();
                let sha = fields.next().unwrap_or_default();
                let ref_name = fields.next().unwrap_or_default();
                let valid_sha = matches!(sha.len(), 40 | 64)
                    && sha.as_bytes().iter().all(u8::is_ascii_hexdigit)
                    && fields.next().is_none();
                if !valid_sha || ref_name != remote_ref {
                    return Err(json!({
                        "error": format!("could not prove disposable review branch '{branch}' is absent from origin"),
                        "code": "disposable_review_branch_absence_unproven",
                        "auto_created_branch": false,
                        "branch": branch,
                    }));
                }
                return Err(json!({
                    "error": format!("disposable review branch '{branch}' already exists"),
                    "code": "disposable_review_requires_new_branch",
                    "auto_created_branch": false,
                    "branch": branch,
                }));
            }
        }
        Ok(remote)
            if String::from_utf8_lossy(&remote.stderr)
                .to_ascii_lowercase()
                .contains("no such remote") => {}
        Ok(_) | Err(_) => {
            return Err(json!({
                "error": format!("could not resolve origin while proving disposable review branch '{branch}' is new"),
                "code": "disposable_review_branch_absence_unproven",
                "auto_created_branch": false,
                "branch": branch,
            }));
        }
    }
    Ok(())
}

pub(super) fn rollback_auto_created_branch(source_path: &Path, branch: &str, expected_head: &str) {
    let branch_ref = format!("refs/heads/{branch}");
    let _ = crate::git_helpers::git_bypass(
        source_path,
        &["update-ref", "-d", &branch_ref, expected_head],
    );
}
