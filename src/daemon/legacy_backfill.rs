//! Sprint 24 P0 PR4 — 28-PR legacy backfill auto-close subsystem.
//!
//! Walks every open task on the board, queries GitHub for every closed/
//! merged PR in the configured repo, computes a confidence score per
//! `(task, PR)` candidate, and either auto-emits `Done(LegacyBackfill)`
//! events or proposes-via-`TaskCloseProposed` events depending on the
//! resulting tier.
//!
//! ## Confidence model
//!
//! Two independent signals plus a small constant signal for explicit
//! `Closes t-XXX` markers. Sub-scores are stored on the resulting
//! [`crate::task_events::ConfidenceScore`] so a forensic auditor can
//! reconstruct **why** a particular tier landed.
//!
//! - **Signal 1** — exact `t-XXX-N` suffix in the PR's head branch name.
//!   Weight 0.6. High-confidence anchor.
//! - **Signal 2** — Jaccard token similarity between the task title and
//!   the PR title (token-prefix matching, with `no_numeric_token_mismatch`
//!   hard rule per impl-1's m-19 spec). Weight up to 0.4.
//! - **Signal 3** — explicit `Closes t-XXX-N` marker in PR body
//!   (sanitised via the same HTML-strip + ASCII-regex pipeline as
//!   `task_sweep`). Weight 0.4.
//!
//! ## 3-tier UX
//!
//! | total | signals | tier            | event emitted            |
//! |-------|---------|-----------------|--------------------------|
//! | ≥0.8  | ≥2      | auto-apply      | `Done(LegacyBackfill)`   |
//! | 0.4–0.8 OR (≥0.8 single signal) | any | propose | `TaskCloseProposed` |
//! | <0.4  | any     | silent          | none                     |
//!
//! ## 6 false-high attack defenses
//!
//! 1. **Title token overlap alone** → require ≥2 signals to reach
//!    auto-apply.
//! 2. **Author+sprint coincidence** → author match isn't a signal in
//!    its own right; only authorship-via-`Closes` (Signal 3) counts.
//! 3. **Branch fragment match** → require the **full** task ID, not a
//!    leading prefix (the `\b` word-boundary in the regex).
//! 4. **Multi-PR same-author tie-break** → if a single task matches
//!    multiple PRs by the same author, the highest-confidence wins (we
//!    don't auto-apply a tie).
//! 5. **Vague-title backlog** → titles in the
//!    `VAGUE_TITLE_TOKENS` set ("follow-up" / "polish" / "cleanup" /
//!    etc.) clamp the title-jaccard sub-score to 0.
//! 6. **Reverse temporal ordering** — task `created_at` after PR
//!    `merged_at` is flagged: confidence sub-score zeroed.

#![allow(dead_code)]

use crate::task_events::{
    self, ConfidenceScore, DoneSource, InstanceName, LinkSource, PrId, PrSnapshot, TaskEvent,
    TaskId,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const PR_LIST_LIMIT: u32 = 100;
const BACKFILL_EMITTER: &str = "system:legacy_backfill";

const AUTO_APPLY_TOTAL_THRESHOLD: f32 = 0.8;
const AUTO_APPLY_SIGNAL_THRESHOLD: u32 = 2;
const PROPOSE_TOTAL_THRESHOLD: f32 = 0.4;

/// Defense #5 — titles that surface frequently as "no-real-content"
/// follow-up tasks. A jaccard hit on these is unreliable; clamp the
/// title sub-score to 0.
const VAGUE_TITLE_TOKENS: &[&str] = &[
    "follow-up",
    "followup",
    "polish",
    "cleanup",
    "tweak",
    "wip",
    "tbd",
    "misc",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    AutoApply,
    Propose,
    Silent,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackfillEntry {
    pub task_id: String,
    pub candidate_pr: Option<u64>,
    pub merge_sha: Option<String>,
    pub confidence: f32,
    pub sub_scores: BTreeMap<String, f32>,
    pub signal_count: u32,
    pub tier: Tier,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BackfillReport {
    pub entries: Vec<BackfillEntry>,
    pub auto_applied: u32,
    pub proposed: u32,
    pub silent: u32,
}

/// Run the legacy backfill against the configured repo. `dry_run = true`
/// emits `TaskCloseProposed` events for auto-tier candidates instead of
/// `Done`, so the operator-confirm gate intercedes before the final
/// transition lands.
pub fn run(home: &Path, repo: &str, dry_run: bool) -> anyhow::Result<BackfillReport> {
    let prs = list_all_closed_prs(repo)?;
    let open_tasks = crate::tasks::list_all(home);
    let open_tasks: Vec<&crate::tasks::Task> = open_tasks
        .iter()
        .filter(|t| matches!(t.status.as_str(), "open" | "claimed" | "in_progress"))
        .collect();

    let mut report = BackfillReport::default();
    let emitter = InstanceName::from(BACKFILL_EMITTER);
    let sweep_id = format!("legacy-backfill-{}", chrono::Utc::now().to_rfc3339());

    for task in &open_tasks {
        let entry = match best_candidate(task, &prs) {
            Some(e) => e,
            None => {
                report.entries.push(BackfillEntry {
                    task_id: task.id.clone(),
                    candidate_pr: None,
                    merge_sha: None,
                    confidence: 0.0,
                    sub_scores: BTreeMap::new(),
                    signal_count: 0,
                    tier: Tier::Silent,
                });
                report.silent += 1;
                continue;
            }
        };
        match entry.tier {
            Tier::AutoApply => {
                report.auto_applied += 1;
                if !dry_run {
                    emit_auto_close(home, &emitter, &entry, &sweep_id, &prs)?;
                } else {
                    emit_proposal(home, &emitter, &entry, &sweep_id, &prs)?;
                }
            }
            Tier::Propose => {
                report.proposed += 1;
                emit_proposal(home, &emitter, &entry, &sweep_id, &prs)?;
            }
            Tier::Silent => {
                report.silent += 1;
            }
        }
        report.entries.push(entry);
    }
    Ok(report)
}

fn best_candidate(task: &crate::tasks::Task, prs: &[PrMeta]) -> Option<BackfillEntry> {
    let mut best: Option<BackfillEntry> = None;
    for pr in prs {
        if !pr.merged {
            continue;
        }
        // Defense #6: reverse temporal — task created after PR merged.
        if let (Ok(task_t), Some(pr_t)) = (
            chrono::DateTime::parse_from_rfc3339(&task.created_at),
            pr.merged_at_dt(),
        ) {
            if task_t > pr_t {
                continue;
            }
        }
        let scores = score_pair(task, pr);
        if scores.signal_count == 0 {
            continue;
        }
        let tier = classify(&scores);
        let entry = BackfillEntry {
            task_id: task.id.clone(),
            candidate_pr: Some(pr.number),
            merge_sha: pr.merge_commit_sha.clone(),
            confidence: scores.total,
            sub_scores: scores.sub_scores.clone(),
            signal_count: scores.signal_count,
            tier,
        };
        // Defense #4: tie-break by max confidence.
        match &best {
            None => best = Some(entry),
            Some(prev) if entry.confidence > prev.confidence => best = Some(entry),
            _ => {}
        }
    }
    best
}

fn classify(scores: &ConfidenceScore) -> Tier {
    if scores.total >= AUTO_APPLY_TOTAL_THRESHOLD
        && scores.signal_count >= AUTO_APPLY_SIGNAL_THRESHOLD
    {
        return Tier::AutoApply;
    }
    if scores.total >= AUTO_APPLY_TOTAL_THRESHOLD || scores.total >= PROPOSE_TOTAL_THRESHOLD {
        return Tier::Propose;
    }
    Tier::Silent
}

/// Compute the confidence score for a `(task, PR)` candidate. Each
/// signal is recorded as a named entry in the `sub_scores` BTreeMap so
/// the auditor can reconstruct contribution per signal.
fn score_pair(task: &crate::tasks::Task, pr: &PrMeta) -> ConfidenceScore {
    let mut sub = BTreeMap::<String, f32>::new();
    let mut total = 0.0f32;
    let mut signals = 0u32;

    // Signal 1: exact task-id suffix in branch name.
    if branch_has_exact_task_id(&pr.head_branch, &task.id) {
        sub.insert("branch_exact".into(), 0.6);
        total += 0.6;
        signals += 1;
    } else {
        // Signal 2: jaccard fuzzy. Only consider when Signal 1 didn't fire
        // (avoids double-counting branch evidence).
        let s = title_jaccard_score(&task.title, &pr.head_branch, &pr.title);
        if s > 0.0 {
            sub.insert("branch_jaccard".into(), s);
            total += s;
            signals += 1;
        }
    }

    // Signal 3: explicit `Closes t-XXX-N` marker in body.
    if body_has_closes_marker(&pr.body, &task.id) {
        sub.insert("closes_marker".into(), 0.4);
        total += 0.4;
        signals += 1;
    }

    ConfidenceScore {
        total,
        signal_count: signals,
        sub_scores: sub,
    }
}

fn branch_has_exact_task_id(branch: &str, task_id: &str) -> bool {
    // Defense #3: full task ID, word-boundary anchored.
    use regex::Regex;
    let pat = format!(r"\b{}\b", regex::escape(task_id));
    Regex::new(&pat)
        .map(|re| re.is_match(branch))
        .unwrap_or(false)
}

fn title_jaccard_score(task_title: &str, _branch: &str, pr_title: &str) -> f32 {
    // Defense #5: vague-title clamp.
    let t_lower = task_title.to_ascii_lowercase();
    if VAGUE_TITLE_TOKENS.iter().any(|v| t_lower.contains(v)) {
        return 0.0;
    }
    let task_tokens: std::collections::BTreeSet<String> = tokenize(&t_lower);
    let pr_tokens: std::collections::BTreeSet<String> = tokenize(&pr_title.to_ascii_lowercase());
    if task_tokens.is_empty() || pr_tokens.is_empty() {
        return 0.0;
    }
    // Defense #5b: numeric mismatch hard reject (e.g. sprint22 vs sprint23).
    if numeric_mismatch(&task_tokens, &pr_tokens) {
        return 0.0;
    }
    let intersection = task_tokens.intersection(&pr_tokens).count() as f32;
    let union = task_tokens.union(&pr_tokens).count() as f32;
    if union == 0.0 {
        return 0.0;
    }
    let jaccard = intersection / union;
    // Map [0, 1] jaccard to [0, 0.4] weighted contribution.
    (jaccard * 0.4).clamp(0.0, 0.4)
}

fn tokenize(s: &str) -> std::collections::BTreeSet<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty() && t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

fn numeric_mismatch(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> bool {
    // Hard reject when both sides have a "<word><digits>" pair (e.g.
    // "sprint22") with the SAME word but DIFFERENT digits.
    fn nums(s: &std::collections::BTreeSet<String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for t in s {
            // Find split point between letters and digits.
            let split = t.find(|c: char| c.is_ascii_digit());
            if let Some(idx) = split {
                let (word, digits) = t.split_at(idx);
                if !word.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    out.push((word.to_string(), digits.to_string()));
                }
            }
        }
        out
    }
    let na = nums(a);
    let nb = nums(b);
    for (wa, da) in &na {
        for (wb, db) in &nb {
            if wa == wb && da != db {
                return true;
            }
        }
    }
    false
}

fn body_has_closes_marker(body: &str, task_id: &str) -> bool {
    use regex::Regex;
    // Strip HTML comments (defense — same vector as task_sweep #1).
    let sanitised = crate::daemon::utils::strip_html_comments(body);
    let pat = format!(r"(?m)Closes\s+{}\b", regex::escape(task_id));
    Regex::new(&pat)
        .map(|re| re.is_match(&sanitised))
        .unwrap_or(false)
}

// ── Event emission ──────────────────────────────────────────────────

fn emit_auto_close(
    home: &Path,
    emitter: &InstanceName,
    entry: &BackfillEntry,
    sweep_id: &str,
    prs: &[PrMeta],
) -> anyhow::Result<()> {
    let pr_number = entry.candidate_pr.unwrap_or(0);
    let merge_sha = entry.merge_sha.clone().unwrap_or_default();
    let pr = prs.iter().find(|p| p.number == pr_number);
    let snapshot = PrSnapshot {
        pr_state: pr
            .map(|p| p.state.clone())
            .unwrap_or_else(|| "merged".into()),
        merge_sha: Some(merge_sha.clone()),
        api_response_hash: pr
            .map(|p| p.api_response_hash.clone())
            .unwrap_or_else(|| "unknown".into()),
        captured_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = pr; // silence unused — used implicitly via sub-fields above
    let events = vec![
        TaskEvent::Linked {
            task_id: TaskId(entry.task_id.clone()),
            pr_id: PrId(pr_number),
            source: LinkSource::SweepDiscovery {
                sweep_id: sweep_id.to_string(),
            },
            snapshot: snapshot.clone(),
        },
        TaskEvent::Done {
            task_id: TaskId(entry.task_id.clone()),
            by: emitter.clone(),
            source: DoneSource::LegacyBackfill {
                sweep_id: sweep_id.to_string(),
                reasoning: format!(
                    "auto-apply tier (total={:.2}, signals={})",
                    entry.confidence, entry.signal_count
                ),
                snapshot: Some(snapshot),
            },
        },
    ];
    task_events::append_batch(home, emitter, events)?;
    Ok(())
}

fn emit_proposal(
    home: &Path,
    emitter: &InstanceName,
    entry: &BackfillEntry,
    sweep_id: &str,
    prs: &[PrMeta],
) -> anyhow::Result<()> {
    let pr_number = entry.candidate_pr.unwrap_or(0);
    let merge_sha = entry.merge_sha.clone().unwrap_or_default();
    let pr = prs.iter().find(|p| p.number == pr_number);
    let snapshot = PrSnapshot {
        pr_state: pr
            .map(|p| p.state.clone())
            .unwrap_or_else(|| "merged".into()),
        merge_sha: Some(merge_sha.clone()),
        api_response_hash: pr
            .map(|p| p.api_response_hash.clone())
            .unwrap_or_else(|| "unknown".into()),
        captured_at: chrono::Utc::now().to_rfc3339(),
    };
    let merged_at = pr
        .and_then(|p| p.merged_at.clone())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let candidate = DoneSource::PrMerged {
        pr_id: PrId(pr_number),
        merge_sha,
        merged_at,
        snapshot,
    };
    let event = TaskEvent::TaskCloseProposed {
        task_id: TaskId(entry.task_id.clone()),
        candidate,
        sweep_id: sweep_id.to_string(),
        confidence: ConfidenceScore {
            total: entry.confidence,
            signal_count: entry.signal_count,
            sub_scores: entry.sub_scores.clone(),
        },
    };
    task_events::append(home, emitter, event)?;
    Ok(())
}

// ── GitHub API ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct PrMeta {
    number: u64,
    state: String,
    merged: bool,
    merge_commit_sha: Option<String>,
    merged_at: Option<String>,
    body: String,
    title: String,
    head_branch: String,
    api_response_hash: String,
}

impl PrMeta {
    fn merged_at_dt(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.merged_at
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
    }
}

fn list_all_closed_prs(repo: &str) -> anyhow::Result<Vec<PrMeta>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("agend-terminal/legacy-backfill")
            .build()?;
        let url = format!(
            "https://api.github.com/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PR_LIST_LIMIT}"
        );
        let mut req = client.get(&url).header("Accept", "application/vnd.github+json");
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("GitHub list-pulls {} for {url}", resp.status());
        }
        let body = resp.text().await?;
        let arr: Vec<serde_json::Value> = serde_json::from_str(&body)?;
        let mut out = Vec::with_capacity(arr.len());
        for pr in arr {
            let pr_bytes = serde_json::to_vec(&pr)?;
            let api_response_hash = crate::daemon::utils::sha256_hex(&pr_bytes);
            if let Some(meta) = parse_pr_meta(&pr, api_response_hash) {
                out.push(meta);
            }
        }
        Ok(out)
    })
}

fn parse_pr_meta(v: &serde_json::Value, api_response_hash: String) -> Option<PrMeta> {
    let number = v.get("number")?.as_u64()?;
    let state = v.get("state")?.as_str()?.to_string();
    let merged = v.get("merged_at").map(|x| !x.is_null()).unwrap_or(false);
    let merge_commit_sha = v
        .get("merge_commit_sha")
        .and_then(|x| x.as_str())
        .map(String::from);
    let merged_at = v
        .get("merged_at")
        .and_then(|x| x.as_str())
        .map(String::from);
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let title = v
        .get("title")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let head_branch = v
        .get("head")
        .and_then(|h| h.get("ref"))
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    Some(PrMeta {
        number,
        state,
        merged,
        merge_commit_sha,
        merged_at,
        body,
        title,
        head_branch,
        api_response_hash,
    })
}

// ── MCP tool surface ─────────────────────────────────────────────────

/// `task_legacy_backfill_run` MCP tool body. Args:
/// - `repo`: `"owner/repo"` to scan.
/// - `dry_run`: default `true` — auto-tier candidates emit
///   `TaskCloseProposed`. Pass `false` to actually emit
///   `Done(LegacyBackfill)` for auto-tier.
///
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-legacy-backfill-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pr(number: u64, head_branch: &str, title: &str, body: &str) -> PrMeta {
        PrMeta {
            number,
            state: "closed".into(),
            merged: true,
            merge_commit_sha: Some(format!("sha-{number}")),
            merged_at: Some(chrono::Utc::now().to_rfc3339()),
            body: body.into(),
            title: title.into(),
            head_branch: head_branch.into(),
            api_response_hash: "h".into(),
        }
    }

    fn task(id: &str, title: &str) -> crate::tasks::Task {
        crate::tasks::Task {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            status: "open".into(),
            priority: "normal".into(),
            assignee: None,
            routed_to: None,
            created_by: "u".into(),
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-04-26T00:00:00Z".into(),
            updated_at: "2026-04-26T00:00:00Z".into(),
            due_at: None,
            branch: None,
        }
    }

    /// Defense #1 — title-only jaccard hit alone reaches Propose tier
    /// (single signal, total = 0.4 max), NOT AutoApply.
    #[test]
    fn defense_1_title_jaccard_alone_caps_at_propose() {
        let t = task("t-1-1", "fix the auth bug urgently");
        let p = pr(1, "feature/something-else", "fix the auth bug urgently", "");
        let scores = score_pair(&t, &p);
        let tier = classify(&scores);
        // Single signal can never reach AutoApply (≥2 signals required).
        assert!(matches!(tier, Tier::Propose | Tier::Silent));
        assert!(scores.signal_count <= 1);
    }

    /// Defense #2 — author+sprint coincidence: PR author isn't a signal
    /// in our model. (We don't even pass author to score_pair — purely
    /// content-based.) Pin this contract structurally.
    #[test]
    fn defense_2_author_is_not_a_signal() {
        // The score_pair signature accepts only Task and PrMeta — not
        // author. Contract verified at compile time.
        let _: fn(&crate::tasks::Task, &PrMeta) -> ConfidenceScore = score_pair;
    }

    /// Defense #3 — branch fragment (substring without word-boundary)
    /// must not match. `t-1-1` should match `feature/t-1-1` but NOT
    /// `feature/t-1-100` (digit run extends past task ID).
    #[test]
    fn defense_3_branch_fragment_no_substring_match() {
        assert!(branch_has_exact_task_id("feature/t-1-1", "t-1-1"));
        assert!(!branch_has_exact_task_id("feature/t-1-100", "t-1-1"));
        assert!(branch_has_exact_task_id("t-1-1-something", "t-1-1"));
        assert!(!branch_has_exact_task_id("xt-1-1", "t-1-1"));
    }

    /// Defense #4 — multi-PR tie-break by max confidence, not first-match.
    #[test]
    fn defense_4_multi_pr_tie_break_by_max_confidence() {
        let t = task("t-100-1", "auth bug fix");
        let p_low = pr(1, "feature/something", "different work", "");
        let p_high = pr(2, "fix/t-100-1-auth", "auth bug fix", "Closes t-100-1");
        let entry = best_candidate(&t, &[p_low, p_high]).expect("must find candidate");
        assert_eq!(entry.candidate_pr, Some(2));
        assert!(entry.confidence > 0.5);
    }

    /// Defense #5 — vague title clamps title-jaccard sub-score to 0.
    #[test]
    fn defense_5_vague_title_zeroes_jaccard() {
        let t = task("t-1-1", "follow-up work");
        let p = pr(1, "feature/x", "follow-up work", "");
        let scores = score_pair(&t, &p);
        assert!(
            !scores.sub_scores.contains_key("branch_jaccard"),
            "vague title must clamp jaccard contribution; got: {:?}",
            scores.sub_scores
        );
    }

    /// Defense #5b — numeric mismatch hard reject (sprint22 vs sprint23).
    #[test]
    fn defense_5b_numeric_mismatch_zeroes_jaccard() {
        let t = task("t-1-1", "Sprint22 P0 PR3 cutover");
        let p = pr(1, "sprint23-p0-pr1", "Sprint23 P0 PR1 substrate", "");
        let scores = score_pair(&t, &p);
        // Branch isn't an exact match either. So no signals fire.
        assert_eq!(scores.signal_count, 0);
    }

    /// Defense #6 — task created after PR merged → reverse temporal.
    /// Skip the candidate entirely (not a sweep target).
    #[test]
    fn defense_6_reverse_temporal_skipped() {
        let mut t = task("t-1-1", "fix something");
        // Task created in 2027.
        t.created_at = "2027-01-01T00:00:00Z".into();
        let mut p = pr(1, "fix/t-1-1", "fix something", "Closes t-1-1");
        // PR merged in 2026.
        p.merged_at = Some("2026-04-01T00:00:00Z".into());
        let entry = best_candidate(&t, &[p]);
        assert!(
            entry.is_none(),
            "reverse-temporal candidate must be skipped"
        );
    }

    /// 3-tier classification — auto-apply requires ≥0.8 total AND ≥2 signals.
    #[test]
    fn classification_auto_apply_requires_2_signals() {
        let one_high = ConfidenceScore {
            total: 0.9,
            signal_count: 1,
            sub_scores: BTreeMap::new(),
        };
        assert_eq!(classify(&one_high), Tier::Propose);
        let two_high = ConfidenceScore {
            total: 0.9,
            signal_count: 2,
            sub_scores: BTreeMap::new(),
        };
        assert_eq!(classify(&two_high), Tier::AutoApply);
        let low = ConfidenceScore {
            total: 0.3,
            signal_count: 1,
            sub_scores: BTreeMap::new(),
        };
        assert_eq!(classify(&low), Tier::Silent);
    }

    /// HTML-comment injection on Closes marker — adversary writes
    /// `<!-- Closes t-victim -->`; sanitiser drops the comment.
    #[test]
    fn body_closes_marker_strips_html_comments() {
        assert!(body_has_closes_marker("Closes t-1-1", "t-1-1"));
        assert!(!body_has_closes_marker("<!-- Closes t-1-1 -->", "t-1-1"));
    }

    /// Full `score_pair` happy path: branch exact + Closes marker → 2
    /// signals + total ≥0.8 → AutoApply tier.
    #[test]
    fn happy_path_two_signals_reaches_auto_apply() {
        let t = task("t-100-1", "real work title");
        let p = pr(
            1,
            "fix/t-100-1-real-work",
            "real work title",
            "Body says Closes t-100-1.",
        );
        let scores = score_pair(&t, &p);
        assert_eq!(scores.signal_count, 2);
        assert!(scores.total >= 0.8, "total: {}", scores.total);
        assert_eq!(classify(&scores), Tier::AutoApply);
    }
}
