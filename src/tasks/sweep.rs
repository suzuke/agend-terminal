use super::list_all;
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// State of a PR referenced by a task title/description.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PrState {
    /// PR was merged; carries the `mergedAt` timestamp.
    Merged { merged_at: String },
    /// PR was closed without merging — task is superseded.
    Closed,
    /// PR is still open — task may still be in flight.
    Open,
    /// PR doesn't exist or query failed — skip categorization.
    Unknown,
}

/// Function-pointer abstraction over `gh pr view`. Tests inject
/// a stub closure to bypass the shell-out. Production uses
/// `gh_pr_lookup` below.
pub(super) type PrLookup<'a> = &'a dyn Fn(&str, u32) -> Result<PrState, String>;

/// Production PR-state lookup — resolves `gh pr view` through the
/// [`crate::scm::ScmProvider`] abstraction (#PR-B; was a direct
/// `Command::new("gh")` shell-out).
pub(super) fn gh_pr_lookup(repo: &str, num: u32) -> Result<PrState, String> {
    // #PR-B: argv is byte-identical to the prior inline call
    // (`gh pr view <num> --repo R --json state,mergedAt`, pinned by
    // `scm::tests::pr_view_args_match_existing_gh_call`). The prior code
    // returned Ok(Unknown) on a non-zero exit (PR may not exist → skip,
    // don't abort the sweep); pr_view surfaces failures as Err, and the
    // sole caller already does `.unwrap_or(PrState::Unknown)`
    // (categorize), so the observable behavior is unchanged.
    let summary = crate::scm::make_scm_provider(repo, None)
        .pr_view(repo, num as u64, &["state", "mergedAt"])
        .map_err(|e| e.to_string())?;
    Ok(match summary.state.as_deref() {
        Some("MERGED") => PrState::Merged {
            merged_at: summary.merged_at.unwrap_or_else(|| "unknown".to_string()),
        },
        Some("CLOSED") => PrState::Closed,
        Some("OPEN") => PrState::Open,
        _ => PrState::Unknown,
    })
}

/// State of an issue referenced by a task title/description (#2061
/// `stale_open`). Only the terminal-vs-live distinction matters.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum IssueState {
    /// Issue is closed — terminal.
    Closed,
    /// Issue is still open — task may still be in flight.
    Open,
    /// Issue doesn't exist or query failed — treat as possibly-live (skip).
    Unknown,
}

/// Function-pointer abstraction over `gh issue view` (mirrors [`PrLookup`]).
/// Tests inject a stub; production uses [`gh_issue_lookup`].
pub(super) type IssueLookup<'a> = &'a dyn Fn(&str, u32) -> Result<IssueState, String>;

/// Production issue-state lookup via [`crate::scm::ScmProvider`] (mirrors
/// [`gh_pr_lookup`]). A non-zero exit / parse failure surfaces as `Err`; the
/// sole caller maps that to [`IssueState::Unknown`] (never abort the sweep).
pub(super) fn gh_issue_lookup(repo: &str, num: u32) -> Result<IssueState, String> {
    let summary = crate::scm::make_scm_provider(repo, None)
        .issue_view(repo, num as u64, &["state"])
        .map_err(|e| e.to_string())?;
    Ok(match summary.state.as_deref() {
        // CLOSED covers both COMPLETED and NOT_PLANNED — either way terminal.
        Some("CLOSED") => IssueState::Closed,
        Some("OPEN") => IssueState::Open,
        _ => IssueState::Unknown,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct Candidate {
    pub id: String,
    pub reason: String,
    pub owner: Option<String>,
    pub pr: Option<u32>,
}

#[derive(Debug, Default)]
pub(super) struct Categories {
    pub shipped: Vec<Candidate>,
    pub superseded: Vec<Candidate>,
    pub team_disbanded: Vec<Candidate>,
    pub validation_leftovers: Vec<Candidate>,
    /// #2061: open/backlog tasks whose referenced issue/PR are ALL terminal,
    /// or which carry no ref and are >14d stale.
    pub stale_open: Vec<Candidate>,
}

impl Categories {
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .shipped
            .iter()
            .chain(self.superseded.iter())
            .chain(self.team_disbanded.iter())
            .chain(self.validation_leftovers.iter())
            .chain(self.stale_open.iter())
            .map(|c| c.id.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn total(&self) -> usize {
        self.all_ids().len()
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "shipped": self.shipped,
            "superseded": self.superseded,
            "team_disbanded": self.team_disbanded,
            "validation_leftovers": self.validation_leftovers,
            "stale_open": self.stale_open,
        })
    }
}

/// Scan the task board and bucket non-terminal tasks into the 5
/// hygiene categories. Tasks already in `done`/`cancelled`/
/// `verified` are skipped — they're already cleaned up. Each task
/// lands in at most one category (first match wins, order:
/// validation_leftovers → team_disbanded → shipped/superseded →
/// stale_open).
///
/// `now` is parameterized so tests can fast-forward age thresholds
/// without forging event-log timestamps.
pub(super) fn scan_categories(
    home: &Path,
    live_instances: &HashSet<String>,
    pr_lookup: PrLookup,
    issue_lookup: IssueLookup,
    repo: Option<&str>,
    now: DateTime<Utc>,
) -> Categories {
    let tasks = list_all(home);
    let mut cats = Categories::default();
    let mut pr_cache: HashMap<u32, PrState> = HashMap::new();
    let mut issue_cache: HashMap<u32, IssueState> = HashMap::new();
    for t in &tasks {
        if matches!(
            t.status,
            crate::task_events::TaskStatus::Done
                | crate::task_events::TaskStatus::Cancelled
                | crate::task_events::TaskStatus::Verified
        ) {
            continue;
        }
        let age = chrono::DateTime::parse_from_rfc3339(&t.updated_at)
            .ok()
            .map(|dt| now.signed_duration_since(dt.with_timezone(&Utc)));
        // (1) validation_leftovers — title prefix match + 1d stale.
        let title_lc = t.title.to_lowercase();
        let is_validation = title_lc.starts_with("val-")
            || title_lc.starts_with("canary-")
            || title_lc.starts_with("test/")
            || title_lc.starts_with("test_")
            || t.branch
                .as_deref()
                .map(|b| b.starts_with("test/"))
                .unwrap_or(false);
        if is_validation {
            if let Some(a) = age {
                if a > Duration::days(1) {
                    cats.validation_leftovers.push(Candidate {
                        id: t.id.clone(),
                        reason: format!("validation/canary title prefix, {}d stale", a.num_days()),
                        owner: t.assignee.clone(),
                        pr: None,
                    });
                    continue;
                }
            }
        }
        // (2) team_disbanded — owner not in live fleet + 30d stale.
        if let (Some(owner), Some(a)) = (t.assignee.as_ref(), age) {
            if !live_instances.contains(owner) && a > Duration::days(30) {
                cats.team_disbanded.push(Candidate {
                    id: t.id.clone(),
                    reason: format!("owner '{owner}' not in live fleet, {}d stale", a.num_days()),
                    owner: Some(owner.clone()),
                    pr: None,
                });
                continue;
            }
        }
        let search_text = format!("{}\n{}", t.title, t.description);
        // (3) shipped / (4) superseded — first PR ref + query. Unchanged
        // predicates; only a `continue` is added after each push so an
        // already-bucketed task is not re-examined by the new stale_open arm
        // below (behaviour-identical: the loop body ended here previously).
        if let Some(repo) = repo {
            if let Some(pr_num) = extract_pr_number(&search_text) {
                let state = pr_cache
                    .entry(pr_num)
                    .or_insert_with(|| pr_lookup(repo, pr_num).unwrap_or(PrState::Unknown))
                    .clone();
                match state {
                    PrState::Merged { merged_at } => {
                        if let Some(a) = age {
                            if a > Duration::days(7) {
                                cats.shipped.push(Candidate {
                                    id: t.id.clone(),
                                    reason: format!(
                                        "PR #{pr_num} merged at {merged_at}, task {}d stale",
                                        a.num_days()
                                    ),
                                    owner: t.assignee.clone(),
                                    pr: Some(pr_num),
                                });
                                continue;
                            }
                        }
                    }
                    PrState::Closed => {
                        cats.superseded.push(Candidate {
                            id: t.id.clone(),
                            reason: format!("PR #{pr_num} closed without merge"),
                            owner: t.assignee.clone(),
                            pr: Some(pr_num),
                        });
                        continue;
                    }
                    PrState::Open | PrState::Unknown => {}
                }
            }
        }
        // (5) stale_open (#2061) — OPEN/Backlog tasks the shipped/superseded
        // arms didn't claim. Conservative (under-report): flag ONLY when EVERY
        // referenced issue/PR resolves terminal, OR there is no ref and the
        // task is >14d stale. Any non-terminal/unknown ref disqualifies the
        // whole task. Operator-gated downstream (dry-run + confirm_ids).
        if !matches!(
            t.status,
            crate::task_events::TaskStatus::Open | crate::task_events::TaskStatus::Backlog
        ) {
            continue;
        }
        let refs = extract_refs(&search_text);
        if refs.is_empty() {
            // No PARSEABLE #N / PR #N ref. Take the age-only fallback ONLY when
            // the task is GENUINELY ref-less (`!saw_token`): no #N / PR #N token
            // and no GitHub issue|pull URL at all. If a token IS present but
            // unverifiable here — an unparseable (overflowing) #N, or an
            // issue/PR named only by URL (which #2061 does not resolve) — the
            // task references work we cannot confirm is done, so do NOT
            // age-flag it; skip conservatively (better to MISS than to surface a
            // live ref-bearing task).
            if !refs.saw_token {
                if let Some(a) = age {
                    if a > Duration::days(14) {
                        cats.stale_open.push(Candidate {
                            id: t.id.clone(),
                            reason: format!("no PR/issue ref, open {}d stale", a.num_days()),
                            owner: t.assignee.clone(),
                            pr: None,
                        });
                    }
                }
            }
            continue;
        }
        // Has >=1 parseable ref → every one must resolve terminal. (A merged-PR
        // open task is flagged regardless of the shipped arm's 7d grace: that
        // grace governs the `shipped` category; for stale_open an all-terminal
        // ref means the work is done.) Without a repo we cannot verify → skip.
        let Some(repo) = repo else { continue };
        let all_terminal = refs.pr_nums.iter().all(|&n| {
            matches!(
                pr_cache
                    .entry(n)
                    .or_insert_with(|| pr_lookup(repo, n).unwrap_or(PrState::Unknown)),
                PrState::Merged { .. } | PrState::Closed
            )
        }) && refs.issue_nums.iter().all(|&n| {
            *issue_cache
                .entry(n)
                .or_insert_with(|| issue_lookup(repo, n).unwrap_or(IssueState::Unknown))
                == IssueState::Closed
        });
        if all_terminal {
            cats.stale_open.push(Candidate {
                id: t.id.clone(),
                reason: format!(
                    "all referenced issue/PR terminal (PR {:?}, issue {:?})",
                    refs.pr_nums, refs.issue_nums
                ),
                owner: t.assignee.clone(),
                pr: refs.pr_nums.first().copied(),
            });
        }
    }
    cats
}

/// Extract the first `PR #<digits>` (or `PR <digits>`) reference
/// from a haystack. Strict `PR ` prefix avoids false positives on
/// standalone `#NNN` issue references.
fn extract_pr_number(text: &str) -> Option<u32> {
    static PR_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PR_RE.get_or_init(|| regex::Regex::new(r"\bPR #?(\d+)\b").expect("pr regex"));
    re.captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u32>().ok())
}

/// All issue/PR references in a haystack, for the #2061 stale_open
/// ALL-quantifier check. PR refs use the same strict `PR #?N` form as
/// [`extract_pr_number`]; issue refs are bare `#N` MINUS any number already
/// captured as a PR (so `PR #12` is counted once, as a PR). De-duplicated.
///
/// `saw_token` records whether the text contained ANY recognised reference
/// token — a `#N` / `PR #N`, or a GitHub `/issues/N` / `/pull/N` URL —
/// INCLUDING tokens we could not turn into a verifiable number (an
/// absurdly-large `#N` that overflows, or an issue/PR named only by URL, which
/// #2061 deliberately does not resolve). It distinguishes a *genuinely* ref-less
/// task (eligible for the no-ref age-only fallback) from one that references
/// work we cannot fully verify (which must NOT be age-flagged — see
/// [`scan_categories`]). This is what keeps the sweep conservative: an
/// unrecognised/unparseable ref makes the sweep MISS a candidate (skip), never
/// wrongly age-flag a live ref-bearing task.
#[derive(Debug, Default, PartialEq)]
struct Refs {
    pr_nums: Vec<u32>,
    issue_nums: Vec<u32>,
    saw_token: bool,
}

fn extract_refs(text: &str) -> Refs {
    static PR_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static ISSUE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static URL_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pr_re = PR_RE.get_or_init(|| regex::Regex::new(r"\bPR #?(\d+)\b").expect("pr regex"));
    let issue_re = ISSUE_RE.get_or_init(|| regex::Regex::new(r"#(\d+)\b").expect("issue regex"));
    let url_re =
        URL_RE.get_or_init(|| regex::Regex::new(r"/(?:issues|pull)/\d+").expect("url regex"));
    let mut pr_nums: Vec<u32> = pr_re
        .captures_iter(text)
        .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .collect();
    pr_nums.sort_unstable();
    pr_nums.dedup();
    let mut issue_nums: Vec<u32> = issue_re
        .captures_iter(text)
        .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|n| !pr_nums.contains(n))
        .collect();
    issue_nums.sort_unstable();
    issue_nums.dedup();
    // Token PRESENCE is regex-match (not parse): a `#N` that overflows u32, or a
    // URL we don't resolve, still means the task references work — so it must
    // not look ref-less. GitHub numbers fit u32, so a `#N` that fails to parse
    // is malformed/unverifiable, not a smaller real ref.
    let saw_token = pr_re.is_match(text) || issue_re.is_match(text) || url_re.is_match(text);
    Refs {
        pr_nums,
        issue_nums,
        saw_token,
    }
}

impl Refs {
    fn is_empty(&self) -> bool {
        self.pr_nums.is_empty() && self.issue_nums.is_empty()
    }
}

/// Apply phase — emit `Cancelled` events for the `confirm_ids`
/// subset under the `system:task_sweep` identity (already in
/// `SYSTEM_IDENTITIES` bypass list). Each Cancelled carries the
/// audit_reason in its reason field; the event log records a
/// `task_sweep_apply` line per cancelled task for cross-board
/// audit.
pub(super) fn emit_cancelled_batch(
    home: &Path,
    categories: &Categories,
    confirm_ids: &HashSet<String>,
    audit_reason: &str,
) -> Result<usize, String> {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let emitter = InstanceName::from("system:task_sweep");
    let mut events: Vec<TaskEvent> = Vec::new();
    let lookup_category = |id: &str| -> &'static str {
        if categories.shipped.iter().any(|c| c.id == id) {
            return "shipped";
        }
        if categories.superseded.iter().any(|c| c.id == id) {
            return "superseded";
        }
        if categories.team_disbanded.iter().any(|c| c.id == id) {
            return "team_disbanded";
        }
        if categories.stale_open.iter().any(|c| c.id == id) {
            return "stale_open";
        }
        "validation_leftovers"
    };
    // Collect (id, category) so the `task_sweep_apply` audit lines are written
    // only AFTER the cancel actually commits — the in-lock guard below can
    // reject the batch, and an audit line for a cancel that never happened
    // would mislead.
    let mut audit: Vec<(String, &'static str)> = Vec::new();
    for id in confirm_ids {
        let category = lookup_category(id);
        events.push(TaskEvent::Cancelled {
            task_id: TaskId(id.clone()),
            by: emitter.clone(),
            reason: format!("sweep:{category}: {audit_reason}"),
        });
        audit.push((id.clone(), category));
    }
    let count = events.len();
    if count == 0 {
        return Ok(0);
    }
    // CR-2026-06-14 (row232): re-validate UNDER the append lock against FRESH
    // committed state. The dry-run that produced `confirm_ids` is out-of-lock, so
    // a task can race to a terminal state (Done/Cancelled) before apply. A bare
    // `append_batch` would clobber that terminal status (replay does not re-guard
    // transitions). Fail closed: if ANY confirmed task is no longer cancellable,
    // reject the whole batch — the operator re-runs the sweep, whose dry-run
    // re-scans and drops the now-terminal task. The closure only inspects the
    // replayed state — no `api::call` under the lock (#1629).
    let checked = crate::task_events::append_batch_checked(home, &emitter, events, |state| {
        for id in confirm_ids {
            if let Some(rec) = state.tasks.get(&TaskId(id.clone())) {
                if !rec
                    .status
                    .can_transition_to(crate::task_events::TaskStatus::Cancelled)
                {
                    return Err(format!(
                        "task '{id}' is no longer cancellable (now {}); sweep aborted to avoid \
                         clobbering a terminal task — re-run the sweep",
                        super::status_to_legacy_str(rec.status)
                    ));
                }
            }
        }
        Ok(())
    });
    match checked {
        Ok(Ok(_)) => {
            for (id, category) in &audit {
                crate::event_log::log(
                    home,
                    "task_sweep_apply",
                    "system:task_sweep",
                    &format!("task={id} category={category} reason={audit_reason}"),
                );
            }
            Ok(count)
        }
        Ok(Err(reason)) => Err(reason),
        Err(e) => Err(e.to_string()),
    }
}
