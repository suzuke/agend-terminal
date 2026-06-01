//! M3: Reviewer SHA-staleness gate — validates reviewed_head against current PR HEAD.

/// Check that a reviewer's claimed SHA matches the current PR HEAD.
/// Returns Ok(()) to proceed, Err(message) to reject the verdict.
pub(crate) fn check_sha_gate(
    reviewed_head: &str,
    summary: &str,
    fetch: impl Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    if reviewed_head.len() < 7 {
        return Err(format!(
            "reviewed_head '{}' is too short ({} chars, minimum 7)",
            reviewed_head,
            reviewed_head.len()
        ));
    }
    let pr_ref = match extract_pr_number(summary) {
        Some(pr) => pr,
        None => {
            // B2: reviewed_head provided but no PR URL → incomplete attestation
            return Err("reviewed_head provided but no GitHub PR URL in summary; \
                 include PR URL (https://github.com/owner/repo/pull/N) \
                 so daemon can verify SHA against current PR head"
                .to_string());
        }
    };
    let current_sha = fetch(&pr_ref)?;
    // N1: strict 40-char compare when full SHA provided; prefix match for short SHAs
    let matches = if reviewed_head.len() >= 40 && current_sha.len() >= 40 {
        reviewed_head == current_sha
    } else {
        current_sha.starts_with(reviewed_head) || reviewed_head.starts_with(&current_sha)
    };
    if !matches {
        return Err(format!(
            "verdict reviewed_head={reviewed_head} but PR is at {current_sha}; \
             please git fetch -f && re-review against current head"
        ));
    }
    Ok(())
}

/// Extract PR number from text containing a GitHub PR URL.
/// Returns `Some("owner/repo#N")` style string for `gh pr view`.
pub(crate) fn extract_pr_number(text: &str) -> Option<String> {
    let marker = "/pull/";
    let idx = text.find(marker)?;
    let before = &text[..idx];
    let gh_idx = before.rfind("github.com/")?;
    let repo_path = &before[gh_idx + "github.com/".len()..];
    let after = &text[idx + marker.len()..];
    let pr_num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if pr_num.is_empty() {
        return None;
    }
    Some(format!("{repo_path}#{pr_num}"))
}

/// Fetch the current HEAD SHA of a PR via the [`crate::scm::ScmProvider`]
/// abstraction (#PR-C; was a direct `gh pr view ... -q .headRefOid`).
pub(crate) fn fetch_pr_head_sha(pr_ref: &str) -> Result<String, String> {
    let parts: Vec<&str> = pr_ref.splitn(2, '#').collect();
    if parts.len() != 2 {
        return Err(format!("invalid PR ref: {pr_ref}"));
    }
    let (repo, number) = (parts[0], parts[1]);
    // #PR-C: behavior-identical conversion. The prior call used gh's
    // `-q .headRefOid` to print the SHA string server-side; the typed
    // `pr_view` returns the parsed `head_ref_oid` field instead — the
    // `-q` gh-ism is intentionally abstracted away (same SHA). argv
    // delta: `-q .headRefOid` removed (only difference). Return contract
    // unchanged: Err on gh failure, Err on empty SHA, Ok(sha) otherwise.
    let num: u64 = number
        .parse()
        .map_err(|_| format!("invalid PR number in ref: {pr_ref}"))?;
    let summary = crate::scm::make_scm_provider(repo, None)
        .pr_view(repo, num, &["headRefOid"])
        .map_err(|e| e.to_string())?;
    let sha = summary.head_ref_oid.unwrap_or_default();
    if sha.is_empty() {
        return Err("gh pr view returned empty SHA".to_string());
    }
    Ok(sha)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn extract_pr_number_from_url() {
        let text = "Review of https://github.com/suzuke/agend-terminal/pull/384 complete";
        let pr = extract_pr_number(text);
        assert_eq!(pr, Some("suzuke/agend-terminal#384".to_string()));
    }

    #[test]
    fn extract_pr_number_no_url() {
        let text = "Just a plain report with no PR link";
        assert_eq!(extract_pr_number(text), None);
    }

    #[test]
    fn sha_gate_green_matching_sha() {
        let sha = "abc123def456789012345678901234567890abcd";
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate(sha, summary, |_| Ok(sha.to_string()));
        assert!(result.is_ok(), "matching SHA should pass: {result:?}");
    }

    #[test]
    fn sha_gate_green_prefix_match() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("abc1234", summary, |_| Ok("abc1234def456".to_string()));
        assert!(result.is_ok(), "prefix match should pass: {result:?}");
    }

    #[test]
    fn sha_gate_red_mismatch() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("old_sha_111", summary, |_| Ok("new_sha_222".to_string()));
        assert!(result.is_err(), "mismatch should reject");
        let err = result.unwrap_err();
        assert!(err.contains("verdict reviewed_head=old_sha_111 but PR is at new_sha_222"));
    }

    #[test]
    fn sha_gate_red_fetch_failure() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("abc1234", summary, |_| {
            Err("gh: not authenticated".to_string())
        });
        assert!(result.is_err(), "fetch failure should reject (fail-closed)");
        assert!(result.unwrap_err().contains("not authenticated"));
    }

    #[test]
    fn sha_gate_red_no_pr_url_with_reviewed_head() {
        let summary = "Just a plain report with no PR link";
        let result = check_sha_gate("abc1234", summary, |_| unreachable!());
        assert!(
            result.is_err(),
            "no PR URL with reviewed_head should reject"
        );
        assert!(result.unwrap_err().contains("no GitHub PR URL"));
    }

    // #1177 characterization tests: empty / too-short reviewed_head

    #[test]
    fn sha_gate_red_empty_reviewed_head() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("", summary, |_| Ok("abc123def456".to_string()));
        assert!(result.is_err(), "empty reviewed_head must be rejected");
        assert!(result.unwrap_err().contains("too short"));
    }

    #[test]
    fn sha_gate_red_short_reviewed_head() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("abc12", summary, |_| Ok("abc123def456".to_string()));
        assert!(result.is_err(), "5-char reviewed_head must be rejected");
        assert!(result.unwrap_err().contains("too short"));
    }

    #[test]
    fn sha_gate_green_7char_reviewed_head() {
        let summary = "Review of https://github.com/owner/repo/pull/42 done";
        let result = check_sha_gate("abc1234", summary, |_| Ok("abc1234def456".to_string()));
        assert!(
            result.is_ok(),
            "7-char reviewed_head should pass: {result:?}"
        );
    }
}
