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
//!    regex `(?m)Closes\s+(t-[0-9]+-[0-9]+(?:-[0-9]+)?)` defeats zero-width-char
//!    homoglyph attacks (accepts the legacy two-segment id and the
//!    three-segment cross-process-unique `t-<ts>-<pid>-<seq>`).
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

/// #1619: default GitHub REST API base. Overridable via
/// `SweepConfig.api_base_url` so self-hosted GitHub Enterprise
/// (`https://ghe.example.com/api/v3`) works instead of being pinned to
/// github.com — mirrors `CiProvider::with_base_url`'s configurable base.
const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";

/// Configuration persisted at `<home>/task_sweep.json`. Operator mutates
/// via the `task_sweep_config` MCP tool; sweep tick reads on each invocation.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct SweepConfig {
    /// `owner/repo` GitHub identifier (e.g. `"suzuke/agend-terminal"`).
    /// `None` = sweep disabled (tick is a no-op).
    pub repo: Option<String>,
    /// #1619: REST API base URL (e.g. `https://ghe.example.com/api/v3`
    /// for self-hosted GitHub Enterprise). `None` → `https://api.github.com`.
    #[serde(default)]
    pub api_base_url: Option<String>,
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
    let api_base = cfg
        .api_base_url
        .as_deref()
        .unwrap_or(DEFAULT_GITHUB_API_BASE);

    // #2117 P2: sweep each project board against ITS OWN repo. fleet.yaml teams
    // contribute their per-project boards; `cfg.repo` is the operator override /
    // single-project fallback for the DEFAULT (home) board. A merged PR in repo A
    // is matched ONLY against board A's open tasks, so it can never auto-close a
    // task that lives on board B (#2105). Single-project deployments (no per-team
    // `source_repo`) resolve to exactly `[(DEFAULT, cfg.repo)]` → board == home →
    // byte-identical to the pre-P2 single-repo tick.
    let boards = resolve_sweep_boards(home, &cfg);
    if boards.is_empty() {
        return Ok(());
    }

    // Sprint 56 Track F (#496): load fleet config once so the per-board
    // authorship gate can resolve `task.created_by` / `task.assignee` (agend-local
    // instance names) into `github_login` GitHub usernames before comparing
    // against `pr.author_login`. `Option<FleetConfig>` because a missing/malformed
    // fleet.yaml must not abort the sweep — fall back to direct compare for compat.
    let fleet_cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();

    // The DEFAULT board's scan gates the (unchanged-scope) compliance pass below,
    // exactly as the pre-P2 single-repo tick did (compliance ran only once both
    // the PR list AND the open-task set were non-empty).
    let mut default_scanned = false;
    // CR-2026-06-14: capture the DEFAULT board's merged-PR list so the compliance
    // pass below reuses it instead of issuing a second identical GitHub fetch.
    let mut default_prs: Option<Vec<PrMeta>> = None;
    for (project_id, repo) in &boards {
        match sweep_board(
            home,
            project_id,
            repo,
            api_base,
            fleet_cfg.as_ref(),
            cfg.dry_run,
        ) {
            Ok((scanned, prs)) => {
                if project_id == crate::task_events::DEFAULT_PROJECT {
                    default_scanned = scanned;
                    default_prs = Some(prs);
                }
            }
            // Per-board isolation: a repo-A API/append failure must NOT abort the
            // repo-B board scan (the #2105 multi-board goal). Log and continue.
            Err(e) => tracing::warn!(
                project = %project_id, repo = %repo, error = %e,
                "task_sweep: board scan failed"
            ),
        }
    }

    // Issue #664: compliance scan stays keyed on the operator's primary repo
    // (`cfg.repo`) and gated on the DEFAULT board's scan — #2117 P2 covers
    // auto-close routing, not compliance (out of scope). Byte-identical: in a
    // single-project deployment the DEFAULT board IS `cfg.repo`.
    if default_scanned && cfg.compliance_mode != "off" {
        if let Some(repo) = cfg.repo.as_deref().filter(|s| !s.is_empty()) {
            // Reuse the DEFAULT board's already-fetched list (set together with
            // `default_scanned` above, so it is always `Some` here).
            if let Some(prs) = default_prs.as_deref() {
                let _ = compliance_sweep(home, repo, prs);
            }
        }
    }

    Ok(())
}

/// #2117 P2: the set of `(project_id, github "owner/repo")` boards to sweep.
///
/// fleet.yaml teams contribute their per-project boards — each team's
/// `source_repo` yields the board's `project_id` (the same slug
/// [`crate::tasks::project_id_from_source_repo`] feeds `board_root`) and its
/// GitHub slug (`derive_repo_from_remote`). `cfg.repo` contributes the DEFAULT
/// (home) board as the operator override / single-project fallback. A
/// `source_repo` with no GitHub `origin` remote (or a non-GitHub remote) is
/// skipped — the poller only knows GitHub Actions. Order is deterministic
/// (BTreeMap by project_id); distinct `source_repo`s that collapse to one
/// project (a project can back multiple teams) dedupe to a single board.
fn resolve_sweep_boards(home: &Path, cfg: &SweepConfig) -> Vec<(String, String)> {
    let mut boards: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for team in crate::teams::list_all(home) {
        let Some(repo_path) = team.source_repo.as_deref() else {
            continue;
        };
        let Some(slug) =
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(repo_path)
        else {
            continue;
        };
        let project_id = crate::tasks::project_id_from_source_repo(repo_path);
        boards.entry(project_id).or_insert(slug);
    }
    if let Some(repo) = cfg.repo.as_deref().filter(|s| !s.is_empty()) {
        boards
            .entry(crate::task_events::DEFAULT_PROJECT.to_string())
            .or_insert_with(|| repo.to_string());
    }
    boards.into_iter().collect()
}

/// #2117 P2: scan ONE project board against ITS repo for `Closes t-…` markers in
/// recently-merged PRs and auto-close the matching open tasks. Mutations route to
/// the board via `append_done_if_legal_at` (the P0/P1 `_at` seam). Returns
/// `Ok(true)` if a full scan ran (PR list AND open-task set both non-empty),
/// `Ok(false)` if it short-circuited — the caller uses the DEFAULT board's flag
/// to gate the compliance pass exactly as the pre-P2 single-repo tick did.
fn sweep_board(
    home: &Path,
    project_id: &str,
    repo: &str,
    api_base: &str,
    fleet_cfg: Option<&crate::fleet::FleetConfig>,
    dry_run: bool,
) -> anyhow::Result<(bool, Vec<PrMeta>)> {
    // #2117 P3a: the `list_recently_merged_prs` network fetch is the ONLY thing
    // this adds over the testable close logic; delegate the rest to
    // `sweep_board_with_prs` (the injectable seam).
    // CR-2026-06-14: this is now the SINGLE merged-PR fetch per tick — the list
    // is returned so `sweep_tick` can thread the DEFAULT board's slice into
    // `compliance_sweep` instead of it re-fetching the identical data.
    let prs = list_recently_merged_prs(repo, api_base)?;
    let scanned = sweep_board_with_prs(home, project_id, &prs, fleet_cfg, dry_run)?;
    Ok((scanned, prs))
}

/// #2117 P3a: the close logic of [`sweep_board`] with the PR list INJECTED. Seam
/// for the close-isolation integration test without a GitHub round-trip — a merged
/// PR scanned against board A is matched ONLY against board A's open tasks (read
/// via the P1 `list_all_at` `_at` variant), so it can never auto-close a task that
/// lives on board B (#2105), even if the PR body's `Closes t-…` marker references
/// it. Returns `Ok(true)` if a full scan ran (PR list AND open-task set both
/// non-empty), `Ok(false)` if it short-circuited.
fn sweep_board_with_prs(
    home: &Path,
    project_id: &str,
    prs: &[PrMeta],
    fleet_cfg: Option<&crate::fleet::FleetConfig>,
    dry_run: bool,
) -> anyhow::Result<bool> {
    if prs.is_empty() {
        return Ok(false);
    }

    // P0/P1 board seam: a single-project deployment resolves `project_id` to
    // DEFAULT → `board == home` → `list_all_at`/`append_done_if_legal_at` are the
    // byte-identical home-board paths.
    let board = crate::task_events::board_root(home, project_id);

    // Snapshot of THIS board's currently-open tasks (read via the P1 `_at`
    // variant so a merged PR is matched only against tasks on its own board).
    let open_tasks = crate::tasks::list_all_at(home, &board);
    let open_ids: std::collections::HashMap<String, &crate::tasks::Task> = open_tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                crate::task_events::TaskStatus::Open
                    | crate::task_events::TaskStatus::Claimed
                    | crate::task_events::TaskStatus::InProgress
            )
        })
        .map(|t| (t.id.clone(), t))
        .collect();
    if open_ids.is_empty() {
        return Ok(false);
    }

    let emitter = InstanceName::from(SWEEP_EMITTER);
    let sweep_id = format!("sweep-{}", chrono::Utc::now().to_rfc3339());

    for pr in prs {
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
            let author_ok = compute_author_ok(&pr.author_login, task, fleet_cfg);
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

            if dry_run {
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
            // #1873: re-validate →Done UNDER the lock. `open_ids` was snapshotted
            // at sweep start; a marker task cancelled since must NOT be flipped to
            // Done (the whole Linked+Done batch is skipped — a cancelled task drops
            // out of `open_ids` next cycle, so no re-attempt).
            let closed = task_events::append_done_if_legal_at(&board, &emitter, &marker, events)?;
            if closed {
                tracing::info!(
                    pr = pr.number,
                    marker = %marker,
                    "sweep: auto-closed (Linked + Done emitted)"
                );
            }
        }
    }

    Ok(true)
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
        // closing-keyword convention. CR-2026-06-14: accept BOTH the legacy
        // two-segment id `t-<ts>-<seq>` and the three-segment cross-process-
        // unique id `t-<ts>-<pid>-<seq>` (the optional third numeric group). The
        // trailing `\b` still bounds the marker so it doesn't swallow following
        // prose.
        regex::Regex::new(r"(?m)Closes\s+(t-[0-9]+-[0-9]+(?:-[0-9]+)?)\b")
            .expect("static regex must compile")
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

/// #1619: build the merged-PR list URL from a configurable API base so
/// self-hosted GitHub Enterprise works. Trailing slashes on the base are
/// trimmed so `https://ghe/api/v3` and `https://ghe/api/v3/` both yield a
/// single-slash join. Pure + testable seam (the live call shells out).
fn build_merged_prs_url(api_base: &str, repo: &str) -> String {
    let base = api_base.trim_end_matches('/');
    format!(
        "{base}/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PR_LIST_LIMIT}"
    )
}

fn list_recently_merged_prs(repo: &str, api_base: &str) -> anyhow::Result<Vec<PrMeta>> {
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
        let url = build_merged_prs_url(api_base, repo);
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
    if let Some(repo) = args.get("repository").and_then(|v| v.as_str()) {
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
    // #1619: self-hosted GitHub Enterprise API base (empty string resets
    // to the github.com default).
    if let Some(base) = args.get("api_base_url").and_then(|v| v.as_str()) {
        cfg.api_base_url = if base.is_empty() {
            None
        } else {
            Some(base.to_string())
        };
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
        "api_base_url": cfg.api_base_url,
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

/// Get changed files for a PR via the [`crate::scm::ScmProvider`]
/// abstraction (#PR-C; was a direct `gh pr view ... --jq .files[].path`).
fn get_pr_changed_files(pr_number: u64, repo: &str) -> Vec<String> {
    // #PR-C: behavior-identical. The prior call used `--jq .files[].path`
    // to print one path per line server-side; the typed `pr_view` returns
    // the parsed `files` paths instead (the `--jq` gh-ism is abstracted
    // away — same path list). argv delta: `--jq .files[].path` removed.
    // Failure → empty Vec (unchanged from the prior `_ => Vec::new()`).
    match crate::scm::make_scm_provider(repo, None).pr_view(repo, pr_number, &["files"]) {
        Ok(summary) => summary.files.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Check 1: PR body contains a genuine passing `VERIFIED` verdict.
///
/// Word-anchored, NOT a bare substring: `"VERIFIED"` is a substring of
/// `"UNVERIFIED"` and appears in `"NOT VERIFIED"`, so the old
/// `to_uppercase().contains("VERIFIED")` passed an explicitly *rejected* review
/// — the exact rejected-but-merged false-negative Issue #664 exists to surface.
/// A `VERIFIED` token counts only when it is (a) bounded by non-alphanumeric
/// chars (rejects `UNVERIFIED` / `VERIFIEDx`) and (b) not immediately preceded
/// by a `NOT` word (rejects `NOT VERIFIED`). Scans every occurrence so a body
/// that mentions both (`was UNVERIFIED, now VERIFIED`) still passes on the
/// genuine one. Mirrors the auto-release word-anchoring (auto_release.rs:432)
/// rather than a loose substring.
fn has_review_verdict(pr: &PrMeta) -> bool {
    const WORD: &str = "VERIFIED";
    let body_upper = pr.body.to_uppercase();
    let mut from = 0;
    while let Some(rel) = body_upper[from..].find(WORD) {
        let start = from + rel;
        let end = start + WORD.len();
        let preceded_by_alnum = body_upper[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric());
        let followed_by_alnum = body_upper[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric());
        if !preceded_by_alnum && !followed_by_alnum {
            // Reject an explicit `NOT` negation directly before the token
            // (the immediately preceding word, ignoring punctuation/space).
            let prev_word: String = body_upper[..start]
                .chars()
                .rev()
                .skip_while(|c| !c.is_alphanumeric())
                .take_while(|c| c.is_alphanumeric())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if prev_word != "NOT" {
                return true;
            }
        }
        from = end;
    }
    false
}

/// #PR-C: client-side reproduction of site-6's prior `gh` `--jq`:
///   `[.[] | select(.state != "SUCCESS" and .state != "SKIPPED")] | length`
/// Counts checks whose `state` is NEITHER "SUCCESS" NOR "SKIPPED"
/// (case-sensitive; null/empty/unknown states all count as not-passed,
/// matching jq's `!=` on a null `.state`). This is the byte-for-byte
/// behavioral equivalent of the dropped server-side jq.
fn count_checks_not_passed(checks: &[crate::scm::CheckState]) -> usize {
    checks
        .iter()
        .filter(|c| c.state != "SUCCESS" && c.state != "SKIPPED")
        .count()
}

/// Check 2: All CI checks passed (queries `gh pr checks` via the
/// [`crate::scm::ScmProvider`] abstraction — #PR-C).
fn check_ci_green(pr: &PrMeta, repo: &str) -> Option<ComplianceViolation> {
    // #PR-C: behavior-identical, FAIL-CLOSED preserved. The prior call
    // pushed the count server-side via
    //   --jq '[.[] | select(.state != "SUCCESS" and .state != "SKIPPED")] | length'
    // then `unwrap_or(1)` (unparseable → treat as 1 = not green). The
    // typed `pr_checks` returns every check (a null/absent state is kept
    // as "" — see scm::parse_checks) and we reproduce the jq filter
    // client-side: any state that is NEITHER "SUCCESS" NOR "SKIPPED"
    // counts as not-passed (case-sensitive, null/unknown → not-passed).
    // Fail-closed direction unchanged: a gh failure / unparseable
    // response → Err → counted as a violation (was the prior `_ =>` arm +
    // `unwrap_or(1)`); the ONLY None (passed) path is "all checks
    // SUCCESS/SKIPPED", identical to before. argv delta: `--jq <expr>`
    // removed + `--json state` → `name,state` (pr_checks' fixed field
    // set; the count reads only `state`, so the result is identical).
    match crate::scm::make_scm_provider(repo, None).pr_checks(repo, pr.number) {
        Ok(checks) => {
            let count = count_checks_not_passed(&checks);
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
        Err(_) => {
            // Can't verify CI — treat as violation (fail-closed), same as
            // the prior gh-failure / unparseable path.
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
///
/// CR-2026-06-14: the merged-PR list is now fetched ONCE per tick (in
/// `sweep_board` for the DEFAULT board) and threaded in as `prs` — this fn no
/// longer issues its own duplicate `list_recently_merged_prs` GitHub request.
/// Behaviour is unchanged: it operates on the same DEFAULT-board merged-PR list
/// it used to re-fetch (DEFAULT board repo == `cfg.repo`).
fn compliance_sweep(home: &Path, repo: &str, prs: &[PrMeta]) -> Vec<ComplianceViolation> {
    let mut cfg = load_config(home);
    if cfg.compliance_mode == "off" {
        return Vec::new();
    }

    let mut all_violations = Vec::new();
    let mut max_merged_at: Option<String> = None;

    for pr in prs {
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
            // Telegram alert (dedup: skip if already alerted).
            //
            // #1339 PR-2: route through `gated_notify` (the single operator-mode
            // chokepoint) instead of `notify_telegram` directly. This is a
            // fleet-initiated daemon job, so it MUST honor operator mode —
            // `Sleep` suppresses this `Warn`-tier ping (the violation is still
            // recorded by the `tracing::warn!` above, so nothing is lost) and
            // it also picks up the outbound-allowlist gate. Compliance is
            // important but not P0-crash class, so `Warn` (not `Error`).
            if !cfg.alerted_prs.contains(&pr.number) {
                let msg = format!(
                    "⚠️ Compliance violation PR #{}: {}",
                    pr.number,
                    violations
                        .iter()
                        .map(|v| v.check_name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if let Some(ch) = crate::channel::active_channel() {
                    let _ = crate::channel::gated_notify(
                        ch.as_ref(),
                        SWEEP_EMITTER,
                        crate::channel::NotifySeverity::Warn,
                        &msg,
                        false,
                    );
                }
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

    // ── #2117 P2: per-board sweep enumeration ──────────────────────

    /// Init a git repo at `dir` with a single GitHub `origin` remote so
    /// `derive_repo_from_remote` resolves a slug (the sweep's board→repo map).
    fn git_repo_with_origin(dir: &Path, origin: &str) {
        fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .ok();
        };
        git(&["init", "-b", "main"]);
        git(&["remote", "add", "origin", origin]);
    }

    /// #2117 P2: the sweep enumerates ONE board per project — each fleet team's
    /// `source_repo` board paired with ITS OWN derived GitHub repo, plus the
    /// DEFAULT (home) board paired with `cfg.repo`. This 1:1 (board, repo)
    /// pairing is what makes a merged PR in repo A match only board A's tasks
    /// (#2105): the sweep never scans board B against repo A.
    #[test]
    fn resolve_sweep_boards_pairs_each_project_with_its_own_repo_2117_p2() {
        let home = tmp_home("p2-sweep-boards");
        let repo_a = home.join("srcA");
        let repo_b = home.join("srcB");
        git_repo_with_origin(&repo_a, "https://github.com/orgA/projA.git");
        git_repo_with_origin(&repo_b, "https://github.com/orgB/projB.git");
        fs::write(
            crate::fleet::fleet_yaml_path(&home),
            format!(
                "instances:\n  devA:\n    backend: claude\n  devB:\n    backend: claude\n\
                 teams:\n  teamA:\n    members:\n      - devA\n    source_repo: {}\n\
                 \x20 teamB:\n    members:\n      - devB\n    source_repo: {}\n",
                repo_a.display(),
                repo_b.display()
            ),
        )
        .unwrap();

        let cfg = SweepConfig {
            repo: Some("operator/primary".to_string()),
            ..Default::default()
        };
        let boards: std::collections::HashMap<String, String> =
            resolve_sweep_boards(&home, &cfg).into_iter().collect();

        // DEFAULT (home) board ← cfg.repo (operator override / single-project
        // fallback) — NEVER a project repo.
        assert_eq!(
            boards
                .get(crate::task_events::DEFAULT_PROJECT)
                .map(String::as_str),
            Some("operator/primary"),
            "default board must map to cfg.repo: {boards:?}"
        );
        // Each project board ← ITS OWN derived (canonical, lowercased) repo.
        let pa = crate::tasks::project_id_from_source_repo(&repo_a);
        let pb = crate::tasks::project_id_from_source_repo(&repo_b);
        assert_eq!(
            boards.get(&pa).map(String::as_str),
            Some("orga/proja"),
            "teamA board must pair with orgA/projA: {boards:?}"
        );
        assert_eq!(
            boards.get(&pb).map(String::as_str),
            Some("orgb/projb"),
            "teamB board must pair with orgB/projB: {boards:?}"
        );
        assert_eq!(
            boards.len(),
            3,
            "exactly default + 2 project boards: {boards:?}"
        );

        fs::remove_dir_all(&home).ok();
    }

    /// #2117 P2 byte-identical: with NO per-team `source_repo` (the
    /// single-project shape every pre-P2 test assumes), the board set is exactly
    /// `[(DEFAULT, cfg.repo)]` → board == home → the legacy single-repo tick.
    #[test]
    fn resolve_sweep_boards_single_project_is_default_only_2117_p2() {
        let home = tmp_home("p2-sweep-single");
        let cfg = SweepConfig {
            repo: Some("operator/primary".to_string()),
            ..Default::default()
        };
        let boards = resolve_sweep_boards(&home, &cfg);
        assert_eq!(
            boards,
            vec![(
                crate::task_events::DEFAULT_PROJECT.to_string(),
                "operator/primary".to_string()
            )],
            "single-project must resolve to exactly the DEFAULT board ← cfg.repo"
        );
        // And no repo configured at all → empty → tick is a no-op.
        let empty = resolve_sweep_boards(&home, &SweepConfig::default());
        assert!(
            empty.is_empty(),
            "no repo + no teams → no boards: {empty:?}"
        );
        fs::remove_dir_all(&home).ok();
    }

    /// #2117 close-isolation e2e (the #2125-review gap reviewer-2+4 both flagged):
    /// a merged PR swept against board A auto-closes ONLY board A's task, even when
    /// its body's `Closes t-…` marker also references a task on board B (#2105).
    /// Exercises the real close path through the `sweep_board_with_prs` seam (no
    /// GitHub round-trip) with a representative two-board fixture built via the real
    /// `tasks::handle` create path.
    #[test]
    fn sweep_closes_only_same_board_task_2117_close_isolation() {
        let home = tmp_home("close-isolation");
        // Two project boards, each with an OPEN task created + assigned to
        // "test-user" (so the sweep's authorship gate passes via direct-name
        // compare — `make_pr_meta` stamps `author_login = "test-user"`).
        let mk = |project: &str| -> String {
            crate::tasks::handle(
                &home,
                "test-user",
                &serde_json::json!({"action": "create", "title": "t", "assignee": "test-user", "project": project}),
            )["id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let ta = mk("orgA/projA");
        let tb = mk("orgB/projB");

        // A merged PR in repo A whose body references BOTH boards' tasks.
        let pr = make_pr_meta(1, "fix", &format!("Closes {ta}\nCloses {tb}"));

        // Sweep board A only.
        let scanned =
            sweep_board_with_prs(&home, "orgA/projA", std::slice::from_ref(&pr), None, false)
                .unwrap();
        assert!(
            scanned,
            "board A had a merged PR + an open task → full scan ran"
        );

        let status_on = |project: &str, id: &str| -> crate::task_events::TaskStatus {
            crate::tasks::list_all_at(&home, &crate::task_events::board_root(&home, project))
                .into_iter()
                .find(|t| t.id == id)
                .unwrap_or_else(|| panic!("task {id} not found on board {project}"))
                .status
        };
        // Board A's task auto-closed; board B's task UNTOUCHED — the sweep matched
        // the markers only against board A's `open_ids` (#2105 cross-board isolation).
        assert_eq!(
            status_on("orgA/projA", &ta),
            crate::task_events::TaskStatus::Done,
            "board A's task must auto-close"
        );
        assert_ne!(
            status_on("orgB/projB", &tb),
            crate::task_events::TaskStatus::Done,
            "board B's task must NOT be closed by repo A's PR (#2105 isolation)"
        );

        fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 56 Track F (#496): authorship gate via github_login ──

    fn task_with(created_by: &str, assignee: Option<&str>) -> crate::tasks::Task {
        crate::tasks::Task {
            id: "t-1-1".into(),
            title: "x".into(),
            description: String::new(),
            status: crate::task_events::TaskStatus::Open,
            priority: crate::task_events::TaskPriority::Normal,
            assignee: assignee.map(str::to_string),
            routed_to: None,
            created_by: created_by.into(),
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-05-08T00:00:00Z".into(),
            updated_at: "2026-05-08T00:00:00Z".into(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            auto_release_on_verdict: None,
            tags: vec![],
            parent_id: None,
            metadata: std::collections::BTreeMap::new(),
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

    /// CR-2026-06-14 regression pin: a task id minted by the REAL
    /// `tasks::handle(create)` path (now the three-segment cross-process-unique
    /// `t-<ts>-<pid>-<seq>`) must round-trip through the `Closes`-marker
    /// extractor WHOLE. With the pre-fix two-group regex the `\b` truncated it to
    /// `t-<ts>-<pid>` (≠ the real id) → `open_ids.get(marker)` missed → the
    /// PR-merge auto-close never fired for any new-format task. The narrow
    /// id-format unit repro missed this cross-module consumer; this pins it.
    #[test]
    fn closes_marker_round_trips_real_minted_three_segment_id() {
        let home = std::env::temp_dir().join(format!(
            "agend-sweep-closes-roundtrip-{}-{}",
            std::process::id(),
            "rt"
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        let created = crate::tasks::handle(
            &home,
            "operator",
            &serde_json::json!({ "action": "create", "title": "round-trip me" }),
        );
        let id = created["id"]
            .as_str()
            .expect("create returns id")
            .to_string();
        // Sanity: the minted id is the new three-segment shape.
        assert_eq!(
            id.matches('-').count(),
            3,
            "expected three-segment id `t-<ts>-<pid>-<seq>`, got {id}"
        );

        let body = format!("Closes {id}");
        let markers = extract_closes_markers(&body);
        assert_eq!(
            markers,
            vec![id.clone()],
            "the extractor must return the WHOLE minted id, not a truncated prefix"
        );

        std::fs::remove_dir_all(&home).ok();
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
        let r1 = handle_task_sweep_config(
            &home,
            &serde_json::json!({"repository": "suzuke/agend-terminal"}),
        );
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
        handle_task_sweep_config(&home, &serde_json::json!({"repository": "x/y"}));
        let r = handle_task_sweep_config(&home, &serde_json::json!({"repository": ""}));
        assert_eq!(r["repo"], serde_json::Value::Null);
        fs::remove_dir_all(&home).ok();
    }

    /// #1619: the merged-PR list URL is built from a configurable API
    /// base (self-hosted GitHub Enterprise support) — NOT pinned to
    /// #PR-C site-6: the client-side `count_checks_not_passed` must
    /// reproduce the prior `--jq`
    /// `[.[] | select(.state != "SUCCESS" and .state != "SKIPPED")] | length`
    /// EXACTLY — incl. the fail-closed treatment of null / empty / unknown
    /// states as not-passed. (gh-failure / unparseable → pr_checks Err →
    /// the `Err(_)` violation arm in check_ci_green, also fail-closed.)
    #[test]
    fn count_checks_not_passed_reproduces_jq() {
        use crate::scm::CheckState;
        let mk = |state: &str| CheckState {
            name: "c".to_string(),
            state: state.to_string(),
        };
        // empty → 0 (all-passed → None violation).
        assert_eq!(count_checks_not_passed(&[]), 0);
        // all SUCCESS / SKIPPED → 0.
        assert_eq!(
            count_checks_not_passed(&[mk("SUCCESS"), mk("SKIPPED"), mk("SUCCESS")]),
            0
        );
        // mixed: one FAILURE among greens → 1.
        assert_eq!(
            count_checks_not_passed(&[mk("SUCCESS"), mk("FAILURE"), mk("SKIPPED")]),
            1
        );
        // unknown states count as not-passed.
        assert_eq!(
            count_checks_not_passed(&[mk("PENDING"), mk("NEUTRAL"), mk("CANCELLED")]),
            3
        );
        // null/empty state (parse_checks keeps it as "") → not-passed.
        assert_eq!(count_checks_not_passed(&[mk(""), mk("SUCCESS")]), 1);
        // case-sensitive: lowercase "success" is NOT the green sentinel.
        assert_eq!(count_checks_not_passed(&[mk("success")]), 1);
    }

    /// github.com. Trailing slashes on the base are trimmed.
    #[test]
    fn build_merged_prs_url_uses_configurable_base() {
        // Default github.com base — byte-identical to the pre-#1619 URL.
        assert_eq!(
            build_merged_prs_url(DEFAULT_GITHUB_API_BASE, "suzuke/agend-terminal"),
            "https://api.github.com/repos/suzuke/agend-terminal/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
        // Self-hosted GHE base.
        assert_eq!(
            build_merged_prs_url("https://ghe.corp.example.com/api/v3", "team/proj"),
            "https://ghe.corp.example.com/api/v3/repos/team/proj/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
        // Trailing slash on the base is trimmed (no double slash).
        assert_eq!(
            build_merged_prs_url("https://ghe.corp.example.com/api/v3/", "team/proj"),
            "https://ghe.corp.example.com/api/v3/repos/team/proj/pulls?state=closed&sort=updated&direction=desc&per_page=30"
        );
    }

    /// #1619: `api_base_url` round-trips through the config tool and an
    /// empty string resets it to the github.com default (`None`).
    #[test]
    fn config_tool_api_base_url_round_trip() {
        let home = tmp_home("config_api_base");
        let r1 = handle_task_sweep_config(
            &home,
            &serde_json::json!({"api_base_url": "https://ghe.corp.example.com/api/v3"}),
        );
        assert_eq!(r1["api_base_url"], "https://ghe.corp.example.com/api/v3");
        // Reset to default with empty string.
        let r2 = handle_task_sweep_config(&home, &serde_json::json!({"api_base_url": ""}));
        assert_eq!(r2["api_base_url"], serde_json::Value::Null);
        // Default (unset) deserializes to None.
        assert!(SweepConfig::default().api_base_url.is_none());
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
        // surfaces here. CR-2026-06-14: accept both the legacy two-segment id
        // and the three-segment cross-process-unique id `t-<ts>-<pid>-<seq>`.
        let strict = regex::Regex::new(r"^t-[0-9]+-[0-9]+(?:-[0-9]+)?$").unwrap();
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
            api_base_url: None,
        };
        save_config(&home, &cfg).unwrap();
        let violations = compliance_sweep(&home, "test/repo", &[]);
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
            api_base_url: None,
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

#[cfg(test)]
mod review_repro_daemon_retention;
