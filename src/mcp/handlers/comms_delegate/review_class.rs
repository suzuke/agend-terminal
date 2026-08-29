//! #3419 review-class authority classification and refusal diagnostics.

use crate::daemon::pr_state::ReviewClass;

/// #2745 fail-closed (decision d-…-11 + codex seam correction): why a
/// merge-authority dispatch's `review_class` could NOT be resolved. The caller
/// refuses to arm the ci-watch and emits [`ReviewClassRefusal::diagnostic`] —
/// NEVER a silent Single/Dual default.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReviewClassRefusal {
    /// The task carried no resolvable `review_class` (absent / null / typo /
    /// wrong-type). `second_reviewer=true` alone is NOT a fallback — it still
    /// refuses.
    Unspecified,
    /// The task's explicit class contradicts the deprecated `second_reviewer`
    /// alias (task=`single` vs `second_reviewer=true`, which implies dual).
    Mismatch { task_class: &'static str },
}

#[cfg(test)]
#[allow(dead_code)]
impl ReviewClassRefusal {
    /// Actionable operator-facing diagnostic for the refused dispatch.
    pub(crate) fn diagnostic(&self, branch: &str) -> String {
        match self {
            ReviewClassRefusal::Unspecified => format!(
                "review_class unspecified for merge-authority dispatch on `{branch}` — \
                 set the task's `review_class` metadata to `single` or `dual` and \
                 re-dispatch. A PR-producing dispatch must declare its review threshold; \
                 the dispatch was refused (fail-closed #2745)."
            ),
            ReviewClassRefusal::Mismatch { task_class } => format!(
                "review_class MISMATCH for dispatch on `{branch}` — task authority is \
                 `{task_class}` but second_reviewer=true implies dual. second_reviewer \
                 cannot override the task's declared class; reconcile them and re-dispatch. \
                 the dispatch was refused (fail-closed #2745)."
            ),
        }
    }

    /// Stable machine code for the structured dispatch-refusal error — lets the
    /// caller distinguish "no class declared" from "class contradicted".
    pub(crate) fn code(&self) -> &'static str {
        match self {
            ReviewClassRefusal::Unspecified => "review_class_unspecified",
            ReviewClassRefusal::Mismatch { .. } => "review_class_mismatch",
        }
    }
}

/// #2745 (decision d-…-11 + codex seam correction): resolve the durable
/// `review_class` for a MERGE-AUTHORITY (PR-producing) dispatch. Called ONLY from
/// the merge-authority branch of [`maybe_auto_bind_lease`] — non-merge dispatches
/// bypass it structurally, so there is no `merge_authority` bool to get wrong.
///
/// The TASK's `review_class` metadata is the sole AUTHORITY — parsed exactly once
/// via [`ReviewClass::parse_fail_closed`]. `second_reviewer` is compatibility
/// EVIDENCE only, never an independent source of dual:
/// - task `dual` → `Ok(Dual)` (`second_reviewer` either value is consistent)
/// - task `single`, `sr=false` → `Ok(Single)`
/// - task `single`, `sr=true` → `Err(Mismatch)` (sr cannot override the task)
/// - task Unresolved (absent/typo), any `sr` → `Err(Unspecified)` (missing+true
///   still refuses; no fallback)
#[cfg(test)]
pub(crate) fn resolve_dispatch_review_class(
    task_review_class_raw: Option<&str>,
    second_reviewer: bool,
) -> Result<ReviewClass, ReviewClassRefusal> {
    match ReviewClass::parse_fail_closed(task_review_class_raw) {
        ReviewClass::Dual => Ok(ReviewClass::Dual),
        ReviewClass::Single if second_reviewer => Err(ReviewClassRefusal::Mismatch {
            task_class: "single",
        }),
        ReviewClass::Single => Ok(ReviewClass::Single),
        ReviewClass::Unresolved => Err(ReviewClassRefusal::Unspecified),
    }
}

/// #2745 R3 (root R2 finding 2): resolve the review_class for an EXISTING-TASK
/// merge-authority dispatch. The task's `review_class` metadata is the SOLE durable
/// AUTHORITY — a supplied `send review_class` arg (and `second_reviewer`) is
/// CONSISTENCY EVIDENCE only: it may confirm the task's class but can NEVER fill a
/// missing-metadata gap or contradict it. Closes the fallback where an untagged
/// existing task passed by supplying `send.review_class` (leaving the task
/// authority-less, against the schema + remediation contract).
/// - task Unresolved (absent/typo metadata), any arg → `Err(Unspecified)` (the arg
///   can't supply durable authority — the task must be tagged first).
/// - task `single`/`dual` + a DIFFERING arg → `Err(Mismatch)`.
/// - task `single` + `second_reviewer=true` (implies dual) → `Err(Mismatch)`.
/// - otherwise `Ok(task_class)`.
#[cfg(test)]
pub(crate) fn resolve_existing_task_review_class(
    task_review_class_raw: Option<&str>,
    arg_review_class_raw: Option<&str>,
    second_reviewer: bool,
) -> Result<ReviewClass, ReviewClassRefusal> {
    let resolved = match ReviewClass::parse_fail_closed(task_review_class_raw) {
        ReviewClass::Unresolved => return Err(ReviewClassRefusal::Unspecified),
        c => c,
    };
    // A supplied send review_class is consistency-evidence only — it must match the
    // task's durable class, never fill a gap or override it.
    if let Some(arg) = arg_review_class_raw.filter(|s| !s.is_empty()) {
        if ReviewClass::parse_fail_closed(Some(arg)) != resolved {
            return Err(ReviewClassRefusal::Mismatch {
                task_class: resolved.as_token(),
            });
        }
    }
    // second_reviewer=true implies dual; it must not contradict a Single task.
    if second_reviewer && resolved == ReviewClass::Single {
        return Err(ReviewClassRefusal::Mismatch {
            task_class: "single",
        });
    }
    Ok(resolved)
}
