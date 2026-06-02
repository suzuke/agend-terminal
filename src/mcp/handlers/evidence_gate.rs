//! #1666 Phase A: reviewer-evidence gate. A `VERIFIED`/`REJECTED` verdict must
//! carry recognizable evidence — a command actually run, or a `path:line`
//! source citation — else the daemon rejects it back to the reviewer (reusing
//! sha_gate's reject path). Modeled on `sha_gate` (pure fn + injected-free unit
//! tests).
//!
//! ⚠ LENIENT BY DESIGN. False-reject is the real risk here: this is a forcing
//! function on the review pipeline, so an over-strict parser would block legit
//! verdicts. The gate accepts ANY recognized evidence token and rejects ONLY
//! when none is present — it does NOT enforce a format. `UNVERIFIED` ("claimed
//! but unproven") is exempt.

use std::sync::LazyLock;

/// A recognized reviewer verdict. Detected via the §3.12 canonical convention —
/// the report text (trimmed) STARTS WITH the verdict word. Starts-with (not a
/// substring scan) is what keeps the gate OFF non-verdict reports: a dev
/// completion note mentioning "dual-VERIFIED" does not *start* with it, so it is
/// never gated. Mirrors `auto_release::is_verdict_message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Verified,
    Rejected,
    Unverified,
}

/// Detect the verdict a report carries, or `None` when it is not a verdict
/// report (so the gate never touches ordinary status/completion reports).
pub(crate) fn detect_verdict(summary: &str) -> Option<Verdict> {
    let t = summary.trim_start();
    // UNVERIFIED first — it contains "VERIFIED" as a substring.
    if t.starts_with("UNVERIFIED") {
        Some(Verdict::Unverified)
    } else if t.starts_with("VERIFIED") {
        Some(Verdict::Verified)
    } else if t.starts_with("REJECTED") {
        Some(Verdict::Rejected)
    } else {
        None
    }
}

/// `filename.ext:line` citation, e.g. `src/comms.rs:464`. The `.<ext>:<digits>`
/// shape distinguishes a real source cite from `key: value` or a `12:34` time.
static PATH_LINE_CITE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[A-Za-z0-9_./-]+\.[A-Za-z][A-Za-z0-9]*:\d+")
        .expect("BUG: evidence path:line cite regex must compile")
});

/// LENIENT evidence detector — any ONE of these counts as evidence:
/// - a structured prefix the L1 doc asks for (`ran:` / `cited:` / `### Evidence`)
/// - a command actually run (the doc lists `cargo`|`gh`|`clippy`|`grep`; `rg` is
///   grep's sibling)
/// - a `path:line` source citation
pub(crate) fn has_evidence_token(body: &str) -> bool {
    if body.contains("ran:") || body.contains("cited:") || body.contains("### Evidence") {
        return true;
    }
    const CMD_TOKENS: &[&str] = &["cargo ", "cargo\n", "clippy", "grep", "rg ", "gh ", "gh\n"];
    if CMD_TOKENS.iter().any(|t| body.contains(t)) {
        return true;
    }
    PATH_LINE_CITE.is_match(body)
}

/// #1666: gate a verdict report on evidence. `VERIFIED`/`REJECTED` must carry a
/// recognizable evidence token; `UNVERIFIED` is exempt. Returns `Err(msg)` to
/// reject the verdict back to the reviewer.
pub(crate) fn check_evidence_gate(body: &str, verdict: Verdict) -> Result<(), String> {
    if matches!(verdict, Verdict::Unverified) {
        return Ok(()); // "claimed but unproven" — evidence not required.
    }
    if has_evidence_token(body) {
        return Ok(());
    }
    Err(format!(
        "{verdict:?} verdict carries no evidence — add an `### Evidence` block with \
         `ran: <cmd> → <result>` (e.g. cargo test / clippy / `gh pr checks`) and/or \
         `cited: path:line — quote`. Re-send with evidence. (UNVERIFIED is exempt — \
         use it for a claimed-but-unproven finding.)"
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── detect_verdict: starts-with, UNVERIFIED-first, no prose false-fire ──
    #[test]
    fn detect_verdict_starts_with_only() {
        assert_eq!(
            detect_verdict("VERIFIED — looks good"),
            Some(Verdict::Verified)
        );
        assert_eq!(detect_verdict("  REJECTED: bug"), Some(Verdict::Rejected));
        // UNVERIFIED must win over its VERIFIED substring.
        assert_eq!(
            detect_verdict("UNVERIFIED — claimed but unproven"),
            Some(Verdict::Unverified)
        );
        // A non-verdict report mentioning the word mid-text is NOT gated.
        assert_eq!(
            detect_verdict("#1604 COMPLETE — dual-VERIFIED, merged"),
            None
        );
        assert_eq!(detect_verdict("task done"), None);
    }

    // ── the lead's required matrix ──
    #[test]
    fn verified_with_cargo_passes() {
        let body = "VERIFIED\n### Evidence\nran: cargo test → 263 passed";
        assert!(check_evidence_gate(body, Verdict::Verified).is_ok());
    }

    #[test]
    fn verified_with_path_line_passes() {
        let body = "VERIFIED — cited: src/comms.rs:464 — gate hooked beside sha_gate";
        assert!(check_evidence_gate(body, Verdict::Verified).is_ok());
    }

    #[test]
    fn verified_with_no_evidence_rejects() {
        let body = "VERIFIED — looks fine to me, lgtm";
        let r = check_evidence_gate(body, Verdict::Verified);
        assert!(r.is_err(), "no-evidence VERIFIED must reject: {r:?}");
    }

    #[test]
    fn unverified_with_no_evidence_passes_exempt() {
        let body = "UNVERIFIED — couldn't reproduce, claimed but unproven";
        assert!(
            check_evidence_gate(body, Verdict::Unverified).is_ok(),
            "UNVERIFIED is exempt"
        );
    }

    #[test]
    fn rejected_with_evidence_passes() {
        let body = "REJECTED\nran: cargo clippy → error[E0382] at src/foo.rs:12";
        assert!(check_evidence_gate(body, Verdict::Rejected).is_ok());
    }

    // ── leniency: each token shape independently counts ──
    #[test]
    fn each_evidence_token_shape_counts() {
        assert!(has_evidence_token("ran: cargo test"));
        assert!(has_evidence_token("cited: src/x.rs:9"));
        assert!(has_evidence_token("### Evidence\n..."));
        assert!(has_evidence_token("checked via gh pr checks 123"));
        assert!(has_evidence_token("grep -rn foo src/"));
        assert!(has_evidence_token("ran clippy clean"));
        assert!(has_evidence_token("see src/state/mod.rs:314"));
        // path:line shape is specific — these are NOT cites.
        assert!(!has_evidence_token("lgtm, ship it"));
        assert!(!has_evidence_token("ratio 12:34 looks ok"));
    }
}
