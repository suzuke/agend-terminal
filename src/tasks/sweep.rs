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
}

impl Categories {
    pub fn all_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .shipped
            .iter()
            .chain(self.superseded.iter())
            .chain(self.team_disbanded.iter())
            .chain(self.validation_leftovers.iter())
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
        })
    }
}

/// Scan the task board and bucket non-terminal tasks into the 4
/// hygiene categories. Tasks already in `done`/`cancelled`/
/// `verified` are skipped — they're already cleaned up. Each task
/// lands in at most one category (first match wins, order:
/// validation_leftovers → team_disbanded → shipped/superseded).
///
/// `now` is parameterized so tests can fast-forward age thresholds
/// without forging event-log timestamps.
pub(super) fn scan_categories(
    home: &Path,
    live_instances: &HashSet<String>,
    pr_lookup: PrLookup,
    repo: Option<&str>,
    now: DateTime<Utc>,
) -> Categories {
    let tasks = list_all(home);
    let mut cats = Categories::default();
    let mut pr_cache: HashMap<u32, PrState> = HashMap::new();
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
        // (3) shipped / (4) superseded — extract PR ref + query.
        let Some(repo) = repo else { continue };
        let search_text = format!("{}\n{}", t.title, t.description);
        let Some(pr_num) = extract_pr_number(&search_text) else {
            continue;
        };
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
            }
            PrState::Open | PrState::Unknown => {}
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
        "validation_leftovers"
    };
    for id in confirm_ids {
        let category = lookup_category(id);
        events.push(TaskEvent::Cancelled {
            task_id: TaskId(id.clone()),
            by: emitter.clone(),
            reason: format!("sweep:{category}: {audit_reason}"),
        });
        crate::event_log::log(
            home,
            "task_sweep_apply",
            "system:task_sweep",
            &format!("task={id} category={category} reason={audit_reason}"),
        );
    }
    let count = events.len();
    if count == 0 {
        return Ok(0);
    }
    crate::task_events::append_batch(home, &emitter, events)
        .map(|_| count)
        .map_err(|e| e.to_string())
}
