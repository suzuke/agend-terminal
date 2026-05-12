//! Sprint 24 P0 PR2 — task auto-close sweep daemon.
//!
//! Periodically polls GitHub for recently merged PRs in a configured repo
//! and emits canonical `TaskEvent::Linked` + `TaskEvent::Done` events for
//! every valid `Closes t-XXX-N` marker observed. Mutations route through
//! [`crate::task_events::append_batch`]; the legacy `tasks.json` is **not**
//! touched by this module — sweep is forward-only by design (PR3 retires
//! `tasks.json` and the read path moves to `task_events::replay`).
//!
//! ## Sweep validation pipeline (5 dev-reviewer-2 must-haves)
//! 1. **HTML-comment injection sanitize** — `<!-- Closes t-victim -->`
//!    rejected (the regex never sees the directive).
//! 2. **Non-ASCII codepoint reject** on task-ID match — strict ASCII-digit
//!    regex `(?m)Closes\s+(t-[0-9]+-[0-9]+)` defeats zero-width-char
//!    homoglyph attacks.
//! 3. **PR.user.login authorship ONLY** (not git trailer co-author) —
//!    defends the pre-PR-220 `update_decision` bug class.
//! 4. **GitHub API schema-mismatch fail-closed** — missing required
//!    fields (`merge_commit_sha`, `merged_at`, `user.login`) cause the
//!    sweep to skip the PR rather than emit a half-formed event.
//! 5. **Squash-merge SHA captured at decision-time** — recorded inside
//!    `DoneSource::PrMerged.merge_sha` + `PrSnapshot.merge_sha` so the
//!    decision survives squash deletion / PR description edits.
//!
//! ## DaemonTicker integration
//! Spawned via [`crate::daemon::ticker::DaemonTicker`] with the standard
//! drop-on-shutdown contract. Forward-compat with Sprint 25+ graceful-
//! join refactor (caller can switch to `join_on_shutdown()` without
//! changing the spawn site).

#![allow(dead_code)]

use crate::daemon::ticker::DaemonTicker;
use crate::task_events::{
    self, DoneSource, InstanceName, LinkSource, PrId, PrSnapshot, TaskEvent, TaskId,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Default sweep tick interval. Operator can override via the
/// `task_sweep_config` MCP tool (`interval_secs`); the next tick after
/// the config save observes the new interval.
const DEFAULT_SWEEP_TICK_SECS: u64 = 300;

/// Emitter identity stamped on sweep-driven events. Distinct from any
/// real fleet instance so audit queries can filter sweep contributions
/// from operator manual transitions.
const SWEEP_EMITTER: &str = "system:task_sweep";

/// Per-tick PR list size. 30 is GitHub's default; we sort by `updated`
/// desc so recent merges land first.
const PR_LIST_LIMIT: u32 = 30;

/// Configuration persisted at `<home>/task_sweep.json`. Operator mutates
/// via the `task_sweep_config` MCP tool; sweep tick reads on each invocation.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct SweepConfig {
    /// `owner/repo` GitHub identifier (e.g. `"suzuke/agend-terminal"`).
    /// `None` = sweep disabled (tick is a no-op).
    pub repo: Option<String>,
    /// `true` = tick body short-circuits with no-op.
    #[serde(default)]
    pub paused: bool,
    /// `true` = log decisions but do not emit events.
    #[serde(default)]
    pub dry_run: bool,
    /// Compliance scanner mode: "off" | "warn" | "enforce". Default: "warn".
    /// - off: no compliance checks
    /// - warn: log violations, send telegram alert, but don't block
    /// - enforce: same as warn (future: block non-compliant merges)
    // "enforce" is reserved for future pre-merge gate integration.
    // Currently behaves identically to "warn" (post-merge alert only).
    #[serde(default = "default_compliance_mode")]
    pub compliance_mode: String,
    /// Cursor: last merged_at timestamp we've scanned for compliance.
    /// Prevents re-scanning old PRs on restart.
    #[serde(default)]
    pub last_seen_merged_at: Option<String>,
    /// PRs already alerted — prevents duplicate telegram notifications.
    #[serde(default)]
    pub alerted_prs: Vec<u64>,
}

fn config_path(home: &Path) -> PathBuf {
    home.join("task_sweep.json")
}

fn default_compliance_mode() -> String {
    "warn".to_string()
}

fn load_config(home: &Path) -> SweepConfig {
    crate::store::load(&config_path(home))
}

/// Sprint 56 Track F (#496): public loader for the doctor's D002 check.
/// Same implementation as the private `load_config` — exposed so
/// `bootstrap::doctor::check_task_sweep_github_login_mapping` can read
/// the sweep state without re-implementing the deserialization. Returns
/// the default (`repo: None`) when the file is missing, mirroring the
/// tick body's "no-op when unconfigured" semantics.
pub fn load_sweep_config_for_doctor(home: &Path) -> SweepConfig {
    load_config(home)
}

fn save_config(home: &Path, cfg: &SweepConfig) -> anyhow::Result<()> {
    crate::store::save_atomic(&config_path(home), cfg)
}

/// Holding-handle for the spawned sweep ticker. Drop is the existing
/// daemon "fire-and-forget" convention (the thread exits via the
/// shutdown atomic). Sprint 25+ graceful-join callers can switch to
/// `DaemonTicker::join_on_shutdown` without changing the spawn site.
pub struct TaskSweep {
    _ticker: DaemonTicker,
}

impl TaskSweep {
    /// Spawn the sweep tick thread. Reads `<home>/task_sweep.json` each
    /// tick; if `repo` is unset or `paused == true`, the body is a no-op.
    /// `body` is invoked once immediately at thread start (per
    /// [`DaemonTicker`] contract) — the operator sees an immediate sweep
    /// after enabling rather than waiting `tick_dur`.
    pub fn spawn(home: PathBuf, shutdown: Arc<AtomicBool>) -> Self {
        let ticker = DaemonTicker::spawn(
            "task_sweep",
            Duration::from_secs(DEFAULT_SWEEP_TICK_SECS),
            shutdown,
            move || {
                if let Err(e) = sweep_tick(&home) {
                    tracing::warn!(error = %e, "task_sweep tick failed");
                }
            },
        );
        Self { _ticker: ticker }
    }
}

// ── Tick body ───────────────────────────────────────────────────────

/// Sprint 56 Track F (#496): resolve an agend-local instance name to its
/// configured GitHub login via `fleet.yaml`'s per-instance
/// `github_login` field. Returns `None` when fleet config is absent /
/// malformed, the instance is not declared, or the field is omitted —
/// the sweep then falls back to direct string compare for backwards
/// compatibility with deployments where instance name happens to equal
/// the GitHub login.
fn resolve_github_login<'a>(
    fleet: Option<&'a crate::fleet::FleetConfig>,
    instance_name: &str,
) -> Option<&'a str> {
    fleet?.instances.get(instance_name)?.github_login.as_deref()
}

/// Sprint 56 Track F (#496): pure helper for the sweep's authorship
/// gate. Returns `true` iff `pr_login` matches either the task creator
/// or the task assignee, after each is resolved through the fleet's
/// `github_login` mapping (with a direct-compare fall-back for
/// instances that have no mapping configured).
///
/// Compat invariant: when no fleet config is loaded, or no instance has
/// `github_login` set, the helper degrades to the pre-Track-F behavior
/// — direct string compare against the agend instance name. This
/// preserves existing deployments where the instance name happens to
/// equal the operator's GitHub login. Operators can opt into the
/// stricter mapping per-instance.
fn compute_author_ok(
    pr_login: &str,
    task: &crate::tasks::Task,
    fleet: Option<&crate::fleet::FleetConfig>,
) -> bool {
    let creator = task.created_by.as_str();
    let creator_login = resolve_github_login(fleet, creator).unwrap_or(creator);
    if pr_login.eq_ignore_ascii_case(creator_login) {
        return true;
    }
    if let Some(assignee) = task.assignee.as_deref() {
        let assignee_login = resolve_github_login(fleet, assignee).unwrap_or(assignee);
        if pr_login.eq_ignore_ascii_case(assignee_login) {
            return true;
        }
    }
    false
}

fn sweep_tick(home: &Path) -> anyhow::Result<()> {
    let cfg = load_config(home);
    if cfg.paused {
        return Ok(());
    }
    let repo = match &cfg.repo {
        Some(r) if !r.is_empty() => r.clone(),
        _ => return Ok(()),
    };

    let prs = list_recently_merged_prs(&repo)?;
    if prs.is_empty() {
        return Ok(());
    }

    // Sprint 56 Track F (#496): load fleet config so the authorship gate
    // below can resolve `task.created_by` / `task.assignee` (agend-local
    // instance names) into `github_login` GitHub usernames before
    // comparing against `pr.author_login`. Pre-Track-F the gate compared
    // disjoint namespaces and silently rejected every cross-namespace
    // mismatch — see `docs/RCA-issue-496-task-sweep-no-auto-close-2026-05-08.md`.
    // `Option<FleetConfig>` because a missing/malformed fleet.yaml must
    // not abort the sweep — fall back to direct compare for compat.
    let fleet_cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();

    // Snapshot of currently-open tasks. Read from tasks::list_all (the
    // PR2 bridge-phase legacy reader); PR3 cutover switches this to
    // `task_events::replay` derived view.
    let open_tasks = crate::tasks::list_all(home);
    let open_ids: std::collections::HashMap<String, &crate::tasks::Task> = open_tasks
        .iter()
        .filter(|t| matches!(t.status.as_str(), "open" | "claimed" | "in_progress"))
        .map(|t| (t.id.clone(), t))
        .collect();
    if open_ids.is_empty() {
        return Ok(());
    }

    let emitter = InstanceName::from(SWEEP_EMITTER);
    let sweep_id = format!("sweep-{}", chrono::Utc::now().to_rfc3339());

    for pr in &prs {
        if !pr.merged {
            continue;
        }
        // Validation must-have #4: GitHub API schema-mismatch fail-closed
        // — a merged PR without merge_commit_sha or merged_at is malformed
        // (or the API contract changed); skip rather than emit a half-
        // formed event that downstream auditors would have to reverse-
        // engineer.
        let merge_sha = match pr.merge_commit_sha.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::warn!(
                    pr = pr.number,
                    "sweep: merged PR with empty merge_commit_sha — schema mismatch, skip"
                );
                continue;
            }
        };
        let merged_at = match pr.merged_at.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::warn!(
                    pr = pr.number,
                    "sweep: merged PR with empty merged_at — schema mismatch, skip"
                );
                continue;
            }
        };

        // Validation must-have #1: HTML-comment injection sanitize.
        let sanitized_body = crate::daemon::utils::strip_html_comments(&pr.body);
        // Validation must-have #2: strict ASCII regex rejects non-ASCII
        // homoglyphs in the task ID portion.
        let markers = extract_closes_markers(&sanitized_body);
        if markers.is_empty() {
            continue;
        }

        for marker in markers {
            let task = match open_ids.get(&marker) {
                Some(t) => t,
                None => {
                    // Marker doesn't reference a currently-open task —
                    // either typo, already-closed task, or attacker
                    // referencing a non-existent ID.
                    tracing::debug!(pr = pr.number, marker = %marker, "sweep: marker doesn't match any open task");
                    continue;
                }
            };
            // Validation must-have #3: PR.user.login authorship ONLY —
            // task creator OR assignee must match. Defends pre-PR-220
            // `update_decision` bug class where a malicious PR body could
            // close another agent's task.
            //
            // Sprint 56 Track F (#496): the comparison is against the
            // GitHub username, not the agend-local instance name. The
            // pure helper `compute_author_ok` resolves creator/assignee
            // via the fleet's `github_login` mapping with a fall-back to
            // a direct string compare for compat — see helper docs.
            let author_ok = compute_author_ok(&pr.author_login, task, fleet_cfg.as_ref());
            if !author_ok {
                tracing::warn!(
                    pr = pr.number,
                    marker = %marker,
                    pr_author = %pr.author_login,
                    task_creator = task.created_by.as_str(),
                    task_assignee = ?task.assignee.as_deref(),
                    "sweep: PR.user.login not authorised to close — rejected"
                );
                continue;
            }

            if cfg.dry_run {
                tracing::info!(
                    pr = pr.number,
                    marker = %marker,
                    "sweep dry-run: would auto-close (no event emitted)"
                );
                continue;
            }

            // Validation must-have #5: capture squash-merge SHA at
            // decision-time so the audit survives squash deletion / PR
            // body edits.
            let snapshot = PrSnapshot {
                pr_state: pr.state.clone(),
                merge_sha: Some(merge_sha.to_string()),
                api_response_hash: pr.api_response_hash.clone(),
                captured_at: chrono::Utc::now().to_rfc3339(),
            };
            let events = vec![
                TaskEvent::Linked {
                    task_id: TaskId(marker.clone()),
                    pr_id: PrId(pr.number),
                    source: LinkSource::SweepDiscovery {
                        sweep_id: sweep_id.clone(),
                    },
                    snapshot: snapshot.clone(),
                },
                TaskEvent::Done {
                    task_id: TaskId(marker.clone()),
                    by: InstanceName(pr.author_login.clone()),
                    source: DoneSource::PrMerged {
                        pr_id: PrId(pr.number),
                        merge_sha: merge_sha.to_string(),
                        merged_at: merged_at.to_string(),
                        snapshot,
                    },
                },
            ];
            task_events::append_batch(home, &emitter, events)?;
            tracing::info!(
                pr = pr.number,
                marker = %marker,
                "sweep: auto-closed (Linked + Done emitted)"
            );
        }
    }

    // Issue #664: run compliance checks on merged PRs
    if cfg.compliance_mode != "off" {
        let _ = compliance_sweep(home, &repo);
    }

    Ok(())
}

// ── PR body sanitisation + marker extraction ─────────────────────────

/// Strip every `<!-- ... -->` HTML comment. Pre-validation step so the
/// `Closes t-XXX` regex never observes a directive an adversary tried to
/// hide inside a comment.
///
/// The implementation walks bytes (ASCII-safe — we strip whole comments,
/// Extract every `Closes t-<digits>-<digits>` marker. Strict ASCII regex
/// rejects non-ASCII codepoints inside the task ID; combined with the
/// HTML-comment sanitiser this defends against zero-width-char +
/// HTML-injection adversary surface.
fn extract_closes_markers(body: &str) -> Vec<String> {
    static MARKER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MARKER.get_or_init(|| {
        // Multi-line; case-sensitive on `Closes` to match GitHub's
        // closing-keyword convention.
        regex::Regex::new(r"(?m)Closes\s+(t-[0-9]+-[0-9]+)\b").expect("static regex must compile")
    });
    re.captures_iter(body)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

// ── GitHub API ──────────────────────────────────────────────────────

/// PR metadata captured from the GitHub list-pulls response. Fields
/// chosen to satisfy the 5 sweep validation must-haves; intermediate
/// JSON parsing in [`parse_pr_meta`] flags schema mismatches.
struct PrMeta {
    number: u64,
    title: String,
    state: String,
    merged: bool,
    merge_commit_sha: Option<String>,
    merged_at: Option<String>,
    body: String,
    author_login: String,
    /// SHA-256 of the per-PR JSON object (hex). Forensic correlation
    /// fingerprint stamped onto the resulting `PrSnapshot`.
    api_response_hash: String,
}

fn list_recently_merged_prs(repo: &str) -> anyhow::Result<Vec<PrMeta>> {
    // Build a per-tick current-thread runtime so the sync DaemonTicker
    // body can call async reqwest. Pattern lifted from `ci_watch.rs`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("agend-terminal/task-sweep")
            .build()?;
        let url = format!(
            "https://api.github.com/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PR_LIST_LIMIT}"
        );
        let mut req = client
            .get(&url)
            .header("Accept", "application/vnd.github+json");
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
            // Per-PR response hash so each event's PrSnapshot fingerprints
            // the exact JSON object the sweep observed (squash deletions
            // + future PR body edits won't change this).
            let pr_json_bytes = serde_json::to_vec(&pr)?;
            let api_response_hash = crate::daemon::utils::sha256_hex(&pr_json_bytes);
            if let Some(meta) = parse_pr_meta(&pr, api_response_hash) {
                out.push(meta);
            }
        }
        Ok(out)
    })
}

fn parse_pr_meta(v: &serde_json::Value, api_response_hash: String) -> Option<PrMeta> {
    let number = v.get("number")?.as_u64()?;
    let title = v
        .get("title")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
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
    // Validation must-have #3 — PR.user.login is the authorship anchor.
    // Missing user.login = malformed PR (deleted account?); skip rather
    // than fall back to less reliable signals.
    let author_login = v
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(|s| s.as_str())?
        .to_string();
    Some(PrMeta {
        number,
        title,
        state,
        merged,
        merge_commit_sha,
        merged_at,
        body,
        author_login,
        api_response_hash,
    })
}

// ── Operator-facing MCP tool surface ────────────────────────────────

/// `task_sweep_config` MCP tool body. Args:
/// - `repo`: `"owner/repo"` to enable; empty string disables.
/// - `pause`: `true|false`.
/// - `dry_run`: `true|false`.
///
/// Returns the resulting [`SweepConfig`] state as JSON so the operator
/// can verify the change without a follow-up read.
pub fn handle_task_sweep_config(home: &Path, args: &serde_json::Value) -> serde_json::Value {
    let mut cfg = load_config(home);
    if let Some(repo) = args.get("repo").and_then(|v| v.as_str()) {
        cfg.repo = if repo.is_empty() {
            None
        } else {
            Some(repo.to_string())
        };
    }
    if let Some(p) = args.get("pause").and_then(|v| v.as_bool()) {
        cfg.paused = p;
    }
    if let Some(d) = args.get("dry_run").and_then(|v| v.as_bool()) {
        cfg.dry_run = d;
    }
    if let Err(e) = save_config(home, &cfg) {
        return serde_json::json!({"error": format!("save failed: {e}")});
    }
    serde_json::json!({
        "repo": cfg.repo,
        "paused": cfg.paused,
        "dry_run": cfg.dry_run,
        "compliance_mode": cfg.compliance_mode,
        "last_seen_merged_at": cfg.last_seen_merged_at,
    })
}

// ─── Issue #664: Post-merge compliance scanner ───────────────────────────────

/// Result of a single compliance check.
#[derive(Debug, Clone)]
pub(crate) struct ComplianceViolation {
    pub pr_number: u64,
    pub check_name: &'static str,
    pub detail: String,
}

/// Run compliance checks on a merged PR.
/// Returns a list of violations (empty = compliant).
fn check_pr_compliance(pr: &PrMeta, _home: &Path, repo: &str) -> Vec<ComplianceViolation> {
    let mut violations = Vec::new();

    // docs-only exception: skip compliance for PRs that only touch docs
    let files = get_pr_changed_files(pr.number, repo);
    if is_docs_only_pr(&files) {
        tracing::info!(pr = pr.number, "compliance: docs-only PR, skipping checks");
        return violations;
    }

    // Check 1: Review verdict (VERIFIED in PR body or comments)
    if !has_review_verdict(pr) {
        violations.push(ComplianceViolation {
            pr_number: pr.number,
            check_name: "review_verdict",
            detail: "No VERIFIED verdict found in PR body".to_string(),
        });
    }

    // Check 2: CI green confirmation
    if let Some(v) = check_ci_green(pr, repo) {
        violations.push(v);
    }

    // Check 3: Scope decision linkage (task board id or Closes #N)
    if !has_scope_linkage(pr) {
        violations.push(ComplianceViolation {
            pr_number: pr.number,
            check_name: "scope_linkage",
            detail: "PR body missing task board id (t-...) or Closes #N".to_string(),
        });
    }

    violations
}

/// Check if PR only touches docs (docs/**, *.md, no src/ changes).
fn is_docs_only_pr(files: &[String]) -> bool {
    !files.is_empty()
        && files
            .iter()
            .all(|f| f.starts_with("docs/") || f.ends_with(".md"))
}

/// Get changed files for a PR via GitHub API.
fn get_pr_changed_files(pr_number: u64, repo: &str) -> Vec<String> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            repo,
            "--json",
            "files",
            "--jq",
            ".files[].path",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Check 1: PR body contains "VERIFIED" verdict.
fn has_review_verdict(pr: &PrMeta) -> bool {
    let body_upper = pr.body.to_uppercase();
    body_upper.contains("VERIFIED")
}

/// Check 2: All CI checks passed (queries gh pr checks).
fn check_ci_green(pr: &PrMeta, repo: &str) -> Option<ComplianceViolation> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "checks",
            &pr.number.to_string(),
            "--repo",
            repo,
            "--json",
            "state",
            "--jq",
            "[.[] | select(.state != \"SUCCESS\" and .state != \"SKIPPED\")] | length",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let count: u32 = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(1);
            if count > 0 {
                Some(ComplianceViolation {
                    pr_number: pr.number,
                    check_name: "ci_green",
                    detail: format!("{count} check(s) not SUCCESS/SKIPPED"),
                })
            } else {
                None
            }
        }
        _ => {
            // Can't verify CI — treat as violation in enforce mode
            Some(ComplianceViolation {
                pr_number: pr.number,
                check_name: "ci_green",
                detail: "Unable to query CI status".to_string(),
            })
        }
    }
}

/// Check 3: PR body has task board id (t-...) or Closes #N.
fn has_scope_linkage(pr: &PrMeta) -> bool {
    let body = &pr.body;
    // Task board id pattern
    let has_task_id = body.contains("t-") && {
        let re = regex::Regex::new(r"t-[0-9]+-[0-9]+").expect("static regex");
        re.is_match(body)
    };
    // Closes #N pattern
    let has_closes = {
        let re = regex::Regex::new(r"(?i)closes?\s+#\d+").expect("static regex");
        re.is_match(body)
    };
    has_task_id || has_closes
}

/// Run compliance sweep on recently merged PRs.
/// Called from sweep_tick when compliance_mode != "off".
fn compliance_sweep(home: &Path, repo: &str) -> Vec<ComplianceViolation> {
    let mut cfg = load_config(home);
    if cfg.compliance_mode == "off" {
        return Vec::new();
    }

    let prs = match list_recently_merged_prs(repo) {
        Ok(prs) => prs,
        Err(_) => return Vec::new(),
    };

    let mut all_violations = Vec::new();
    let mut max_merged_at: Option<String> = None;

    for pr in &prs {
        if !pr.merged {
            continue;
        }
        // Skip PRs we've already scanned (cursor)
        if let Some(ref cursor) = cfg.last_seen_merged_at {
            if let Some(ref merged_at) = pr.merged_at {
                if merged_at <= cursor {
                    continue;
                }
            }
        }
        // Track max merged_at for cursor update
        if let Some(ref merged_at) = pr.merged_at {
            if max_merged_at
                .as_ref()
                .map(|m| merged_at > m)
                .unwrap_or(true)
            {
                max_merged_at = Some(merged_at.clone());
            }
        }

        let violations = check_pr_compliance(pr, home, repo);
        if !violations.is_empty() {
            for v in &violations {
                tracing::warn!(
                    pr = pr.number,
                    check = v.check_name,
                    detail = %v.detail,
                    "compliance violation"
                );
            }
            // Telegram alert (dedup: skip if already alerted)
            if !cfg.alerted_prs.contains(&pr.number) {
                crate::channel::telegram::notify::notify_telegram(
                    home,
                    SWEEP_EMITTER,
                    &format!(
                        "⚠️ Compliance violation PR #{}: {}",
                        pr.number,
                        violations
                            .iter()
                            .map(|v| v.check_name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
                cfg.alerted_prs.push(pr.number);
            }
        }
        all_violations.extend(violations);
    }

    // Single cursor + alerted_prs persistence at end
    if let Some(merged_at) = max_merged_at {
        cfg.last_seen_merged_at = Some(merged_at);
    }
    // Cap alerted_prs to last 100 to prevent unbounded growth
    if cfg.alerted_prs.len() > 100 {
        let drain = cfg.alerted_prs.len() - 100;
        cfg.alerted_prs.drain(..drain);
    }
    let _ = save_config(home, &cfg);

    all_violations
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-task-sweep-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── Sprint 56 Track F (#496): authorship gate via github_login ──

    fn task_with(created_by: &str, assignee: Option<&str>) -> crate::tasks::Task {
        crate::tasks::Task {
            id: "t-1-1".into(),
            title: "x".into(),
            description: String::new(),
            status: "open".into(),
            priority: "normal".into(),
            assignee: assignee.map(str::to_string),
            routed_to: None,
            created_by: created_by.into(),
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-05-08T00:00:00Z".into(),
            updated_at: "2026-05-08T00:00:00Z".into(),
            due_at: None,
            branch: None,
            dispatched_at: None,
            eta_secs: None,
        }
    }

    fn fleet_with_login(instance: &str, github_login: &str) -> crate::fleet::FleetConfig {
        let mut instances = std::collections::HashMap::new();
        instances.insert(
            instance.into(),
            crate::fleet::InstanceConfig {
                github_login: Some(github_login.into()),
                ..Default::default()
            },
        );
        crate::fleet::FleetConfig {
            instances,
            ..Default::default()
        }
    }

    /// Lead-spec #1: with `github_login: alice` mapped on instance `dev`,
    /// a PR by `alice` against a task created by `dev` must close.
    #[test]
    fn mapping_present_compares_against_github_login() {
        let task = task_with("dev", None);
        let fleet = fleet_with_login("dev", "alice");
        assert!(compute_author_ok("alice", &task, Some(&fleet)));
        // Case-insensitive — GitHub usernames are.
        assert!(compute_author_ok("Alice", &task, Some(&fleet)));
    }

    /// Lead-spec #2: no `github_login` mapping AND no fleet config — the
    /// helper falls back to direct string compare against the agend
    /// instance name. Compat path for deployments where instance name
    /// happens to equal the operator's GitHub login (the implicit
    /// assumption pre-Track-F).
    #[test]
    fn mapping_absent_falls_back_to_direct_compare() {
        let task = task_with("dev", None);
        // No fleet at all.
        assert!(compute_author_ok("dev", &task, None));
        assert!(!compute_author_ok("alice", &task, None));
        // Fleet present but no mapping for this instance.
        let mut fleet = crate::fleet::FleetConfig::default();
        fleet.instances.insert(
            "other".into(),
            crate::fleet::InstanceConfig {
                github_login: Some("bob".into()),
                ..Default::default()
            },
        );
        assert!(compute_author_ok("dev", &task, Some(&fleet)));
        assert!(!compute_author_ok("alice", &task, Some(&fleet)));
    }

    /// Lead-spec #3: when `github_login: alice` is mapped, a PR by `bob`
    /// must NOT close — the security gate (PR-220 defense) is preserved
    /// even after Track F's namespace fix.
    #[test]
    fn mapping_present_mismatch_does_not_close() {
        let task = task_with("dev", None);
        let fleet = fleet_with_login("dev", "alice");
        assert!(!compute_author_ok("bob", &task, Some(&fleet)));
        // The PR author equals the agend instance name string but the
        // mapping says the real login is `alice` — direct-name match
        // must NOT bypass the mapping when one is configured.
        assert!(!compute_author_ok("dev", &task, Some(&fleet)));
    }

    /// Mapping resolves through the assignee branch too — a task claimed
    /// by `dev-impl` with `github_login: bob` must accept a PR by `bob`
    /// even when the creator's mapping doesn't match.
    #[test]
    fn mapping_resolves_assignee_branch() {
        let task = task_with("lead", Some("dev-impl"));
        let mut fleet = crate::fleet::FleetConfig::default();
        fleet.instances.insert(
            "lead".into(),
            crate::fleet::InstanceConfig {
                github_login: Some("alice".into()),
                ..Default::default()
            },
        );
        fleet.instances.insert(
            "dev-impl".into(),
            crate::fleet::InstanceConfig {
                github_login: Some("bob".into()),
                ..Default::default()
            },
        );
        assert!(compute_author_ok("alice", &task, Some(&fleet))); // creator branch
        assert!(compute_author_ok("bob", &task, Some(&fleet))); // assignee branch
        assert!(!compute_author_ok("eve", &task, Some(&fleet))); // neither
    }

    /// Resolver: returns mapped login when fleet has it.
    #[test]
    fn resolve_github_login_returns_mapped() {
        let fleet = fleet_with_login("dev", "alice");
        assert_eq!(resolve_github_login(Some(&fleet), "dev"), Some("alice"));
    }

    /// Resolver: returns None when instance is not in the fleet (so the
    /// caller's fall-back path engages rather than a false-negative).
    #[test]
    fn resolve_github_login_returns_none_for_unknown_instance() {
        let fleet = fleet_with_login("dev", "alice");
        assert_eq!(resolve_github_login(Some(&fleet), "ghost"), None);
        assert_eq!(resolve_github_login(None, "dev"), None);
    }

    /// Must-have #1 — HTML-comment injection: a directive hidden inside
    /// a comment must NOT extract. Real-world task IDs use digit-only
    /// segments (`t-<ts>-<seq>`); the test fixture uses the same shape.
    #[test]
    fn html_comment_injection_stripped() {
        let body = "Closes t-12345-1\n<!-- Closes t-99999-2 -->";
        let sanitised = crate::daemon::utils::strip_html_comments(body);
        let markers = extract_closes_markers(&sanitised);
        assert_eq!(markers, vec!["t-12345-1".to_string()]);
    }

    /// Must-have #1 — unterminated comment drops tail entirely; even a
    /// trailing valid marker after a partial `<!--` is dropped (an
    /// adversary playing fuzzing tricks can't slip a directive through).
    #[test]
    fn html_comment_unterminated_drops_tail() {
        let body = "Closes t-1-1\n<!-- partial Closes t-2-2";
        let sanitised = crate::daemon::utils::strip_html_comments(body);
        let markers = extract_closes_markers(&sanitised);
        assert_eq!(markers, vec!["t-1-1".to_string()]);
    }

    /// Must-have #2 — non-ASCII codepoint inside the task ID portion
    /// (e.g. zero-width-joiner mimicking a digit) NEVER matches the
    /// strict ASCII-digit regex.
    #[test]
    fn non_ascii_task_id_rejected() {
        // U+200B (zero-width-space) between digits.
        let body = "Closes t-12\u{200B}3-4";
        let markers = extract_closes_markers(body);
        assert!(
            markers.is_empty(),
            "non-ASCII codepoint must defeat the regex"
        );
    }

    /// Multiple markers in one PR body are all extracted.
    #[test]
    fn multiple_markers_extracted() {
        let body = "Closes t-1-1\nCloses t-2-2\nCloses t-3-3";
        let markers = extract_closes_markers(body);
        assert_eq!(
            markers,
            vec![
                "t-1-1".to_string(),
                "t-2-2".to_string(),
                "t-3-3".to_string()
            ]
        );
    }

    /// `parse_pr_meta` rejects the PR if `user.login` is missing — our
    /// authorship anchor (must-have #3) requires it. Better to drop the
    /// PR than fall back to git trailer (the bug class we're defending).
    #[test]
    fn parse_rejects_missing_user_login() {
        let v = serde_json::json!({
            "number": 1,
            "state": "closed",
            "merge_commit_sha": "abc",
            "merged_at": "2026-04-27T00:00:00Z",
            "body": "",
            "user": {} // login missing
        });
        assert!(parse_pr_meta(&v, "h".into()).is_none());
    }

    /// Must-have #5 — `parse_pr_meta` carries the merge SHA so callers
    /// can stamp it onto `PrSnapshot.merge_sha` at decision-time.
    #[test]
    fn parse_carries_merge_sha() {
        let v = serde_json::json!({
            "number": 42,
            "state": "closed",
            "merge_commit_sha": "abcdef1234",
            "merged_at": "2026-04-27T00:00:00Z",
            "body": "Closes t-1-1",
            "user": {"login": "dev-impl-1"}
        });
        let meta = parse_pr_meta(&v, "fakehash".into()).unwrap();
        assert_eq!(meta.merge_commit_sha.as_deref(), Some("abcdef1234"));
        assert_eq!(meta.author_login, "dev-impl-1");
        assert!(meta.merged); // merged_at non-null
    }

    /// `task_sweep_config` MCP tool round-trip — operator sets repo, then
    /// pauses, then disables dry-run; final state matches.
    #[test]
    fn config_tool_round_trip() {
        let home = tmp_home("config_rt");
        let r1 =
            handle_task_sweep_config(&home, &serde_json::json!({"repo": "suzuke/agend-terminal"}));
        assert_eq!(r1["repo"], "suzuke/agend-terminal");
        assert_eq!(r1["paused"], false);

        let r2 = handle_task_sweep_config(&home, &serde_json::json!({"pause": true}));
        assert_eq!(r2["paused"], true);
        assert_eq!(r2["repo"], "suzuke/agend-terminal");

        let r3 = handle_task_sweep_config(&home, &serde_json::json!({"dry_run": true}));
        assert_eq!(r3["dry_run"], true);
        assert_eq!(r3["paused"], true);
        fs::remove_dir_all(&home).ok();
    }

    /// `task_sweep_config` empty-string `repo` disables sweep (sets to
    /// `None`) — operator's escape hatch when the repo identifier is
    /// wrong and they want to clear it without `unsetenv`.
    #[test]
    fn config_tool_empty_repo_disables() {
        let home = tmp_home("config_empty");
        handle_task_sweep_config(&home, &serde_json::json!({"repo": "x/y"}));
        let r = handle_task_sweep_config(&home, &serde_json::json!({"repo": ""}));
        assert_eq!(r["repo"], serde_json::Value::Null);
        fs::remove_dir_all(&home).ok();
    }

    /// `sweep_tick` short-circuits when the config is missing or has no
    /// repo set. Operator sees a no-op rather than a noisy GitHub call.
    #[test]
    fn tick_no_op_when_repo_unconfigured() {
        let home = tmp_home("tick_no_op");
        // No config file written. Tick must complete without error and
        // without writing to task_events.jsonl.
        sweep_tick(&home).unwrap();
        assert!(!home.join("task_events.jsonl").exists());
        fs::remove_dir_all(&home).ok();
    }

    /// `sha256_hex` produces a 64-char hex string. Forensic hash
    /// fingerprint must be a deterministic crypto-grade digest so a
    /// later auditor can correlate against an archived API response.
    #[test]
    fn sha256_hex_is_deterministic_64_hex() {
        let h1 = crate::daemon::utils::sha256_hex(b"hello");
        let h2 = crate::daemon::utils::sha256_hex(b"hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        let h3 = crate::daemon::utils::sha256_hex(b"world");
        assert_ne!(h1, h3);
    }

    /// REJECT criteria (per dev-reviewer m-25) — exercise the
    /// closes-marker extractor against the **actual `Closes t-` commit
    /// messages on origin/main**. Defends against three failure modes
    /// the unit-test fixtures alone can't catch:
    ///
    /// 1. Real PR descriptions contain prose around the marker that
    ///    might trip the regex if the boundary check is loose.
    /// 2. A future contributor edits a PR description AFTER merge to
    ///    add a malicious `Closes t-victim`; this real-repo audit lets
    ///    a subsequent CI run surface the regression.
    /// 3. The on-main commit messages prove that PR1 + the wider Sprint
    ///    24 P0 wave actually used the marker convention sweep depends
    ///    on, not just the unit-test mocks.
    ///
    /// Skipped (with diagnostic stderr) when `git` binary unavailable or
    /// the cwd isn't a git repo. Tests should never *fail* due to env
    /// shape — operator-run CI on a fresh checkout always has both.
    #[test]
    fn closes_markers_extract_cleanly_from_actual_main() {
        let output = match std::process::Command::new("git")
            .args([
                "log",
                "origin/main",
                "--grep=Closes t-",
                "--format=%B",
                "--all-match",
            ])
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("skip: git binary unavailable: {e}");
                return;
            }
        };
        if !output.status.success() {
            eprintln!(
                "skip: `git log` failed (likely no `origin/main` ref locally): exit={:?}",
                output.status.code()
            );
            return;
        }
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            eprintln!("skip: no `Closes t-` commits on origin/main yet");
            return;
        }
        let markers = extract_closes_markers(&text);
        // Every extracted marker MUST conform to strict format. A
        // regression where the extractor accepts (e.g.) `t-foo-1`
        // surfaces here.
        let strict = regex::Regex::new(r"^t-[0-9]+-[0-9]+$").unwrap();
        for m in &markers {
            assert!(
                strict.is_match(m),
                "marker `{m}` extracted from real-main commit but fails strict format check"
            );
        }
        eprintln!(
            "real-repo audit: extracted {} markers from {} bytes of `Closes t-` commit messages on origin/main",
            markers.len(),
            text.len()
        );
    }

    // ─── Issue #664 compliance scanner tests ─────────────────────────

    fn make_pr_meta(number: u64, title: &str, body: &str) -> PrMeta {
        PrMeta {
            number,
            title: title.to_string(),
            state: "closed".to_string(),
            merged: true,
            merge_commit_sha: Some("abc123".to_string()),
            merged_at: Some("2026-05-12T00:00:00Z".to_string()),
            body: body.to_string(),
            author_login: "test-user".to_string(),
            api_response_hash: "deadbeef".to_string(),
        }
    }

    #[test]
    fn compliance_review_verdict_detected() {
        let pr = make_pr_meta(1, "fix: something", "Review VERIFIED by reviewer-codex");
        assert!(has_review_verdict(&pr));
    }

    #[test]
    fn compliance_review_verdict_missing() {
        let pr = make_pr_meta(2, "fix: something", "No review info here");
        assert!(!has_review_verdict(&pr));
    }

    #[test]
    fn compliance_scope_linkage_task_id() {
        let pr = make_pr_meta(3, "fix: something", "Implements t-20260511-12");
        assert!(has_scope_linkage(&pr));
    }

    #[test]
    fn compliance_scope_linkage_closes_issue() {
        let pr = make_pr_meta(4, "fix: something", "Closes #664");
        assert!(has_scope_linkage(&pr));
    }

    #[test]
    fn compliance_scope_linkage_missing() {
        let pr = make_pr_meta(5, "fix: something", "Just a fix without linkage");
        assert!(!has_scope_linkage(&pr));
    }

    #[test]
    fn compliance_docs_only_exception() {
        let files = vec!["docs/SKILLS.md".to_string(), "README.md".to_string()];
        assert!(is_docs_only_pr(&files));
    }

    #[test]
    fn compliance_non_docs_pr() {
        let files = vec!["src/agent.rs".to_string(), "docs/README.md".to_string()];
        assert!(!is_docs_only_pr(&files));
    }

    #[test]
    fn compliance_feature_flag_off() {
        let home = tmp_home("compliance-off");
        fs::create_dir_all(&home).unwrap();
        let cfg = SweepConfig {
            repo: Some("test/repo".to_string()),
            paused: false,
            dry_run: false,
            compliance_mode: "off".to_string(),
            last_seen_merged_at: None,
            alerted_prs: Vec::new(),
        };
        save_config(&home, &cfg).unwrap();
        let violations = compliance_sweep(&home, "test/repo");
        assert!(
            violations.is_empty(),
            "off mode should produce no violations"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compliance_cursor_persisted() {
        let home = tmp_home("compliance-cursor");
        fs::create_dir_all(&home).unwrap();
        let mut cfg = SweepConfig {
            repo: Some("test/repo".to_string()),
            paused: false,
            dry_run: false,
            compliance_mode: "warn".to_string(),
            last_seen_merged_at: None,
            alerted_prs: Vec::new(),
        };
        // Simulate cursor update (done by compliance_sweep at end)
        cfg.last_seen_merged_at = Some("2026-05-12T00:00:00Z".to_string());
        cfg.alerted_prs.push(10);
        save_config(&home, &cfg).unwrap();

        let updated = load_config(&home);
        assert_eq!(
            updated.last_seen_merged_at.as_deref(),
            Some("2026-05-12T00:00:00Z")
        );
        assert!(updated.alerted_prs.contains(&10));
        fs::remove_dir_all(&home).ok();
    }
}
