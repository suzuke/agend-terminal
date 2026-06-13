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

/// A recognized reviewer verdict. Detected (see [`detect_verdict`]) when the
/// verdict word is the LEADING token of the report — after stripping leading
/// whitespace + markdown line-prefixes. Leading-token (not a substring scan) is
/// what keeps the gate OFF non-verdict reports: a dev completion note mentioning
/// "dual-VERIFIED" mid-text is never gated. Mirrors the §3.12 verdict convention
/// (`auto_release::is_verdict_message`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Verified,
    Rejected,
    Unverified,
}

/// Detect the verdict a report carries, or `None` when it is not a verdict
/// report (so the gate never touches ordinary status/completion reports).
///
/// #1668 (codex catch): strip leading whitespace AND markdown line-prefixes
/// (`>`, `-`, `*`, `#`, in any combination) BEFORE the match — otherwise a
/// reviewer could evade the gate by markdown-prefixing the verdict
/// (`> VERIFIED` / `## VERIFIED`). After stripping, the verdict word must be the
/// leading TOKEN: it matches only when followed by end-of-string or a
/// non-alphanumeric boundary, so `VERIFIED`/`VERIFIED:`/`VERIFIED —` match while
/// `dual-VERIFIED` (no prefix to strip; doesn't start with the word) and
/// `#1604 … dual-VERIFIED` (strips `#` → leads with `1604`) stay `None`. This
/// preserves the original mid-text-false-positive protection.
pub(crate) fn detect_verdict(summary: &str) -> Option<Verdict> {
    let t = summary
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '>' | '-' | '*' | '#'));
    // UNVERIFIED first — it shares the "VERIFIED" tail (strip_prefix already
    // disambiguates, but keep the order explicit).
    for (word, verdict) in [
        ("UNVERIFIED", Verdict::Unverified),
        ("VERIFIED", Verdict::Verified),
        ("REJECTED", Verdict::Rejected),
    ] {
        if let Some(rest) = t.strip_prefix(word) {
            // Bounded token: EOL or a non-alphanumeric char (rejects `VERIFIEDx`).
            if rest.chars().next().is_none_or(|c| !c.is_alphanumeric()) {
                return Some(verdict);
            }
        }
    }
    None
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

// ── #1666 Phase B: WARN-first L3 truth cross-check ──────────────────────────
//
// Verifies the CHECKABLE evidence in a verdict that already PASSED the Phase-A
// presence gate, and returns human-readable warnings for claims that DON'T
// verify. WARN-FIRST: the gate still PASSES the verdict (no reject) — the caller
// only logs the warnings, so we can MEASURE the false-positive rate before
// deciding to harden. Pure + dependency-injected (mirrors sha_gate's `fetch`):
// the false-warn behavior is unit-tested without touching the filesystem or the
// ci_watch store. L4 (a reviewer's local `cargo test` in their own shell) is an
// out-of-scope, trust-based residual — the daemon cannot observe it.

/// Resolution of a `path:line` cite against the reviewed tree. The `RepoUnknown`
/// state is load-bearing: when we cannot resolve the repo we must NOT warn (it's
/// "can't check", not "checked and failed") — that is the false-warn boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CiteResolution {
    /// File found; it has this many lines.
    Lines(usize),
    /// Repo resolved, but the cited file does not exist → checked-and-failed.
    FileMissing,
    /// Could not resolve a repo to check against → SKIP (never warn).
    RepoUnknown,
}

/// Cross-check outcome for a "CI is green" claim. `Unknown` is the false-warn
/// boundary (no PR/repo determinable → can't check → never warn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CiCheck {
    /// A matching ci_watch record shows success.
    Green,
    /// A matching record exists but its conclusion is not success → the ONLY
    /// CI state that warns (the daemon checked and the claim contradicts it).
    NotGreen,
    /// No matching ci_watch record. #1671: this is "can't check", NOT "checked &
    /// absent" — the poller GCs records (72h TTL + PR-terminal auto-clear,
    /// #931), so a real late CI-green claim is indistinguishable from a
    /// fabricated one here → never warn.
    NoRecord,
    /// Could not determine the repo to check → SKIP (never warn).
    Unknown,
}

/// Extract `path:line` cites from the body (reuses the Phase-A cite shape).
pub(crate) fn extract_path_line_cites(body: &str) -> Vec<(String, usize)> {
    PATH_LINE_CITE
        .find_iter(body)
        .filter_map(|m| {
            let (path, line) = m.as_str().rsplit_once(':')?;
            Some((path.to_string(), line.parse().ok()?))
        })
        .collect()
}

/// Does the report ASSERT that CI is green? (A bare `gh pr checks` command
/// mention is NOT enough — it must claim a green/passing outcome, so we don't
/// false-warn on "ran gh pr checks → pending".)
pub(crate) fn claims_ci_green(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    const GREEN_CLAIMS: &[&str] = &[
        "ci green",
        "ci is green",
        "ci: green",
        "ci pass",
        "ci passed",
        "checks pass",
        "checks passed",
        "all checks pass",
        "all green",
        "ci success",
    ];
    GREEN_CLAIMS.iter().any(|c| b.contains(c))
}

/// #1666 Phase B: WARN-first truth cross-check. Returns warnings for checkable
/// claims that do not verify; an empty vec means "all checkable claims verified,
/// or nothing checkable". The caller logs these and STILL passes the verdict.
///
/// `resolve(path)` yields the repo-relative cite resolution; `ci_check()` yields
/// the CI-green cross-check. The deliberate false-warn boundary: only a
/// "checked & failed" outcome warns (`FileMissing`/out-of-range cite, or a live
/// `NotGreen` record). Every "can't check" outcome — `RepoUnknown`, and (post
/// #1671) BOTH `Unknown` and `NoRecord` for CI — never warns.
pub(crate) fn cross_check_warnings(
    body: &str,
    resolve: impl Fn(&str) -> CiteResolution,
    ci_check: impl Fn() -> CiCheck,
) -> Vec<String> {
    let mut warns = Vec::new();
    for (path, line) in extract_path_line_cites(body) {
        match resolve(&path) {
            CiteResolution::Lines(n) if line > n => warns.push(format!(
                "cited `{path}:{line}` but the reviewed file has only {n} lines"
            )),
            CiteResolution::FileMissing => warns.push(format!(
                "cited `{path}:{line}` but that file was not found in the reviewed tree"
            )),
            // in-range, or RepoUnknown (can't check) → no warning.
            CiteResolution::Lines(_) | CiteResolution::RepoUnknown => {}
        }
    }
    if claims_ci_green(body) {
        match ci_check() {
            // ONLY a live record that CONTRADICTS the claim warns.
            CiCheck::NotGreen => warns.push(
                "claims CI is green, but the ci_watch record shows a non-success conclusion".into(),
            ),
            // #1671 (codex catch): NoRecord must NOT warn. The ci_watch poller
            // legitimately GCs records (72h TTL + PR-terminal auto-clear, #931),
            // so a real CI-green claim arriving AFTER GC is indistinguishable
            // from a fabricated one — both present as NoRecord. "Can't check" ≠
            // "checked & failed". Consequence (honest L3 limitation): the
            // CI-green cross-check is only effective WHILE the record lives
            // (ephemeral, ≤72h); after GC we never warn. A weak-but-correct
            // check beats a false-warning one — warn-first must not cry wolf.
            CiCheck::Green | CiCheck::NoRecord | CiCheck::Unknown => {}
        }
    }
    warns
}

/// Production `resolve` dep for [`cross_check_warnings`]: resolve a cite against
/// the reporting reviewer's bound worktree (where the review happened), reusing
/// `claim_verifier::resolve_dispatch_tree`. `RepoUnknown` when the reviewer has
/// no resolvable worktree — so an unresolvable repo never warns.
pub(crate) fn resolve_cite_in_reviewer_tree(
    home: &std::path::Path,
    reviewer: &str,
    path: &str,
) -> CiteResolution {
    let Some(repo) = crate::claim_verifier::resolve_dispatch_tree(home, reviewer, None, None)
    else {
        return CiteResolution::RepoUnknown;
    };
    match std::fs::read_to_string(repo.join(path)) {
        Ok(c) => CiteResolution::Lines(c.lines().count()),
        Err(_) => CiteResolution::FileMissing,
    }
}

/// Production `ci_check` dep: cross-check a CI-green claim against the ci_watch
/// store. The repo is taken from the PR URL in `summary` (reusing sha_gate's
/// extractor); `Unknown` when no repo is determinable (never warn). Reads the
/// small `ci-watches/*.json` set — verdict reports are rare and these are local
/// file reads, so the per-verdict cost is negligible.
pub(crate) fn ci_check_for_report(home: &std::path::Path, summary: &str) -> CiCheck {
    let Some(pr_ref) = super::sha_gate::extract_pr_number(summary) else {
        return CiCheck::Unknown;
    };
    let repo = pr_ref.split('#').next().unwrap_or_default();
    if repo.is_empty() {
        return CiCheck::Unknown;
    }
    let Ok(entries) = std::fs::read_dir(crate::daemon::ci_watch::ci_watches_dir(home)) else {
        return CiCheck::NoRecord;
    };
    let mut found = false;
    for entry in entries.flatten() {
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(ws) = serde_json::from_str::<crate::daemon::ci_watch::WatchState>(&content) else {
            continue;
        };
        if ws.repo != repo {
            continue;
        }
        found = true;
        if ws.last_notified_conclusion.as_deref() == Some("success") {
            return CiCheck::Green;
        }
    }
    if found {
        CiCheck::NotGreen
    } else {
        CiCheck::NoRecord
    }
}

/// #1666 Phase B: run the WARN-first cross-check with the production deps and LOG
/// each warning. WARN-ONLY — the caller still delivers the verdict; this only
/// emits observability so we can measure the false-positive rate. Kept here
/// (beside the pure [`cross_check_warnings`] + the prod deps) so the comms hook
/// stays a one-liner.
pub(crate) fn cross_check_and_log(
    home: &std::path::Path,
    reviewer: &str,
    summary: &str,
    evidence_body: &str,
    verdict: Verdict,
) {
    let warns = cross_check_warnings(
        evidence_body,
        |path| resolve_cite_in_reviewer_tree(home, reviewer, path),
        || ci_check_for_report(home, summary),
    );
    for w in &warns {
        tracing::warn!(
            target: "evidence_crosscheck",
            reviewer = %reviewer,
            verdict = ?verdict,
            warning = %w,
            "#1666 Phase B: verdict evidence did not cross-verify (WARN-only; verdict delivered)"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── detect_verdict: leading-token match, UNVERIFIED-first, no prose FP ──
    #[test]
    fn detect_verdict_leading_token() {
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
        assert_eq!(detect_verdict("dual-VERIFIED"), None);
        assert_eq!(detect_verdict("task done"), None);
        assert_eq!(detect_verdict("ratio 12:34 ok"), None);
        // `VERIFIEDx` is not the verdict word (alphanumeric boundary).
        assert_eq!(detect_verdict("VERIFIEDish maybe"), None);
    }

    /// #1668 (codex catch): markdown-prefixed verdicts must NOT evade the gate.
    /// Stripping leading `>`/`-`/`*`/`#`/whitespace (any combination) before the
    /// token match — WITHOUT reintroducing the mid-text false-positive.
    #[test]
    fn detect_verdict_strips_markdown_prefixes_1668() {
        // GATED now (were evading via trim_start()-only):
        for s in [
            "> VERIFIED",
            "- VERIFIED",
            "* VERIFIED",
            "## VERIFIED",
            ">VERIFIED",        // no space
            "  > VERIFIED",     // whitespace + prefix
            "> - * # VERIFIED", // any combination
            "- REJECTED: nope",
            "> UNVERIFIED",
        ] {
            assert!(
                detect_verdict(s).is_some(),
                "#1668: markdown-prefixed verdict must be detected: {s:?}"
            );
        }
        assert_eq!(detect_verdict("> VERIFIED"), Some(Verdict::Verified));
        assert_eq!(detect_verdict("## REJECTED — x"), Some(Verdict::Rejected));
        assert_eq!(detect_verdict("> UNVERIFIED"), Some(Verdict::Unverified));

        // Protection must SURVIVE: stripping leading `#` then leading with a
        // non-verdict token stays None (no new mid-text false-positive).
        assert_eq!(detect_verdict("#1604 COMPLETE — dual-VERIFIED"), None);
        assert_eq!(detect_verdict("- some bullet, later VERIFIED"), None);
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

    // ── #1666 Phase B: WARN-first cross-check (pure, injected deps) ──────────
    // Warnings are advisory only — cross_check_warnings NEVER rejects; the gate
    // (check_evidence_gate) is the only thing that can reject, and that's L2.

    #[test]
    fn crosscheck_cited_real_file_no_warn() {
        let warns = cross_check_warnings(
            "VERIFIED — cited: src/comms.rs:50",
            |_| CiteResolution::Lines(100),
            || CiCheck::Unknown,
        );
        assert!(warns.is_empty(), "in-range cite must not warn: {warns:?}");
    }

    #[test]
    fn crosscheck_cited_nonexistent_warns() {
        let warns = cross_check_warnings(
            "REJECTED — cited: src/ghost.rs:9",
            |_| CiteResolution::FileMissing,
            || CiCheck::Unknown,
        );
        assert_eq!(warns.len(), 1, "missing cited file must warn: {warns:?}");
    }

    #[test]
    fn crosscheck_cite_out_of_range_warns() {
        let warns = cross_check_warnings(
            "VERIFIED — cited: src/x.rs:500",
            |_| CiteResolution::Lines(10),
            || CiCheck::Unknown,
        );
        assert_eq!(warns.len(), 1, "out-of-range line must warn: {warns:?}");
    }

    #[test]
    fn crosscheck_repo_unknown_never_warns() {
        // can't resolve the repo → can't check → must NOT warn (FP boundary).
        let warns = cross_check_warnings(
            "VERIFIED — cited: src/x.rs:9999",
            |_| CiteResolution::RepoUnknown,
            || CiCheck::Unknown,
        );
        assert!(
            warns.is_empty(),
            "unresolvable repo must not warn: {warns:?}"
        );
    }

    #[test]
    fn crosscheck_ci_green_matches_record_no_warn() {
        let warns = cross_check_warnings(
            "VERIFIED — CI green, all checks pass",
            |_| CiteResolution::RepoUnknown,
            || CiCheck::Green,
        );
        assert!(
            warns.is_empty(),
            "green claim + green record must not warn: {warns:?}"
        );
    }

    #[test]
    fn crosscheck_ci_green_no_record_no_warn_1671() {
        // #1671 (codex catch): NoRecord = can't-check (the watch may have been
        // GC'd — 72h TTL + PR-terminal auto-clear, #931) → must NOT warn; a real
        // late CI-green claim is indistinguishable from a fabricated one here.
        let warns = cross_check_warnings(
            "VERIFIED — CI green",
            |_| CiteResolution::RepoUnknown,
            || CiCheck::NoRecord,
        );
        assert!(
            warns.is_empty(),
            "no-record CI-green must NOT warn (GC boundary): {warns:?}"
        );
    }

    #[test]
    fn crosscheck_ci_green_contradicted_by_record_warns() {
        // The ONLY CI state that warns: a LIVE record whose conclusion ≠ success.
        let warns = cross_check_warnings(
            "VERIFIED — CI green",
            |_| CiteResolution::RepoUnknown,
            || CiCheck::NotGreen,
        );
        assert_eq!(
            warns.len(),
            1,
            "green claim contradicted by a live record must warn: {warns:?}"
        );
    }

    #[test]
    fn crosscheck_ci_unknown_never_warns() {
        // green claim present, but repo undeterminable → can't check → no warn.
        let warns = cross_check_warnings(
            "VERIFIED — CI green",
            |_| CiteResolution::RepoUnknown,
            || CiCheck::Unknown,
        );
        assert!(
            warns.is_empty(),
            "undeterminable CI repo must not warn: {warns:?}"
        );
    }

    #[test]
    fn crosscheck_no_checkable_claims_no_warn() {
        // no cite + no CI-green claim → nothing to cross-check.
        let warns = cross_check_warnings(
            "VERIFIED — looks good, ran cargo test",
            |_| CiteResolution::FileMissing,
            || CiCheck::NoRecord,
        );
        assert!(warns.is_empty(), "no checkable claim → no warn: {warns:?}");
    }

    #[test]
    fn crosscheck_extract_and_claim_helpers() {
        assert_eq!(
            extract_path_line_cites("see src/a.rs:12 and lib/b.rs:7"),
            vec![("src/a.rs".to_string(), 12), ("lib/b.rs".to_string(), 7)]
        );
        assert!(claims_ci_green("CI green"));
        assert!(claims_ci_green("all checks passed"));
        // a bare command mention is NOT a green claim (avoids false-warn).
        assert!(!claims_ci_green("ran `gh pr checks 1668` → pending"));
    }
}
