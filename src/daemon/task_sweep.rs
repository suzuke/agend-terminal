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

use crate::daemon::ticker::DaemonTicker;
use crate::task_events::{
    self, DoneSource, InstanceName, LinkSource, PrId, PrSnapshot, TaskEvent, TaskId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Default sweep tick interval. Operator can override via the
/// `agend-terminal admin task-sweep-config` CLI (`interval_secs`); the next
/// tick after the config save observes the new interval.
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

/// #3316: provenance is retained as an append-only audit set. An inactive
/// mapping can retire only after this quiet period, unless an operator
/// explicitly acknowledges it through the task-sweep config command.
const LEGACY_RETIREMENT_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const PROVENANCE_FILE: &str = "task_sweep_provenance.json";
const HEALTH_FILE: &str = "task_sweep_health.json";

/// Configuration persisted at `<home>/task_sweep.json`. Operator mutates via
/// the `agend-terminal admin task-sweep-config` CLI (#2547: moved from the
/// `task_sweep_config` MCP tool); sweep tick reads on each invocation.
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
    /// #3316: provenance keys explicitly acknowledged by an operator. A key
    /// consumed while admitting an unverified baseline is removed; a key added
    /// after materialization authorizes immediate retirement of that mapping.
    #[serde(default)]
    pub provenance_acknowledgements: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct SweepProvenanceStore {
    /// Append-only history; retired entries remain for audit.
    #[serde(default)]
    entries: Vec<SweepProvenance>,
    #[serde(default)]
    next_generation: u64,
    /// Operator acknowledgements for explicitly named boards which have no
    /// deterministic team/repository mapping.  These records intentionally
    /// contain no repository or path-derived identity.
    #[serde(default)]
    manual_unmapped_acknowledgements: Vec<ManualUnmappedAcknowledgement>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SweepProvenance {
    project_id: String,
    repo: String,
    api_base: String,
    observed_at: String,
    config_generation: u64,
    /// #3316: first persisted inactive instant; TTL starts at this transition.
    #[serde(default)]
    legacy_since: Option<String>,
    #[serde(default)]
    retired_at: Option<String>,
    #[serde(default)]
    retirement_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ManualUnmappedAcknowledgement {
    project_id: String,
    actor: String,
    audit_reason: String,
    acknowledged_at: String,
    total_tasks: usize,
    non_terminal_tasks: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct SweepHealthEntry {
    code: String,
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    total_tasks: usize,
    non_terminal_tasks: usize,
    evidence: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct SweepHealthSnapshot {
    #[serde(default)]
    as_of: String,
    #[serde(default)]
    entries: Vec<SweepHealthEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SweepBoard {
    project_id: String,
    repo: String,
    api_base: String,
    legacy_compat: bool,
}

#[derive(Debug, Clone, Default)]
struct SweepPlan {
    boards: Vec<SweepBoard>,
    default_repo: Option<String>,
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

fn provenance_path(home: &Path) -> PathBuf {
    home.join(PROVENANCE_FILE)
}

fn health_path(home: &Path) -> PathBuf {
    home.join(HEALTH_FILE)
}

fn load_provenance(home: &Path) -> SweepProvenanceStore {
    crate::store::load(&provenance_path(home))
}

fn save_provenance(home: &Path, store: &SweepProvenanceStore) -> anyhow::Result<()> {
    crate::store::save_atomic(&provenance_path(home), store)
}

fn save_health(home: &Path, entries: Vec<SweepHealthEntry>) -> anyhow::Result<()> {
    crate::store::save_atomic(
        &health_path(home),
        &SweepHealthSnapshot {
            as_of: chrono::Utc::now().to_rfc3339(),
            entries,
        },
    )
}

fn canonical_project_id(project_id: &str) -> String {
    let project_id = project_id.trim();
    if project_id.is_empty()
        || project_id.eq_ignore_ascii_case(crate::task_events::DEFAULT_PROJECT)
        || project_id.eq_ignore_ascii_case("fleet")
    {
        crate::task_events::DEFAULT_PROJECT.to_string()
    } else {
        crate::task_events::project_slug(project_id)
    }
}

fn canonical_api_base(api_base: &str) -> String {
    let trimmed = api_base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_GITHUB_API_BASE.to_string()
    } else {
        trimmed.to_string()
    }
}

fn canonical_repo(repo: &str) -> Option<String> {
    crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(repo)
}

fn provenance_key(project_id: &str, repo: &str, api_base: &str) -> String {
    format!(
        "{}|{}|{}",
        canonical_project_id(project_id),
        repo,
        canonical_api_base(api_base)
    )
}

fn board_counts(home: &Path, project_id: &str) -> (usize, usize) {
    let board = crate::task_events::board_root(home, project_id);
    let tasks = crate::tasks::list_all_at(home, &board);
    let non_terminal = tasks
        .iter()
        .filter(|task| !task.status.is_terminal())
        .count();
    (tasks.len(), non_terminal)
}

fn active_team_claims(home: &Path, cfg: &SweepConfig) -> anyhow::Result<BTreeMap<String, String>> {
    let mut claims: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for team in crate::teams::list_all(home) {
        let Some(repo_path) = team.source_repo.as_deref() else {
            continue;
        };
        let Some(repo) =
            crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(repo_path)
        else {
            continue;
        };
        let repo = canonical_repo(&repo)
            .ok_or_else(|| anyhow::anyhow!("team '{}' has an invalid GitHub origin", team.name))?;
        let derived_project = crate::tasks::project_id_from_source_repo(repo_path);
        let project = team
            .project_id
            .as_deref()
            .map(canonical_project_id)
            .unwrap_or_else(|| canonical_project_id(&derived_project));
        claims
            .entry(project)
            .or_default()
            .entry(repo)
            .or_default()
            .push(team.name);
    }

    let mut resolved = BTreeMap::new();
    for (project, repos) in claims {
        if repos.len() > 1 {
            let details = repos
                .iter()
                .map(|(repo, teams)| format!("{repo} ({})", teams.join(",")))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("conflicting repo claims for project '{project}': {details}");
        }
        let (repo, _) = repos.into_iter().next().expect("non-empty repo claims");
        resolved.insert(project, repo);
    }

    // Team claims are authoritative; cfg.repo is the DEFAULT fallback only when unclaimed.
    if !resolved.contains_key(crate::task_events::DEFAULT_PROJECT) {
        if let Some(repo) = cfg.repo.as_deref().filter(|repo| !repo.trim().is_empty()) {
            let repo = canonical_repo(repo)
                .ok_or_else(|| anyhow::anyhow!("configured sweep repository is invalid: {repo}"))?;
            resolved.insert(crate::task_events::DEFAULT_PROJECT.to_string(), repo);
        }
    }
    Ok(resolved)
}

fn current_board_projects(home: &Path) -> HashSet<String> {
    crate::tasks::list_all_boards(home)
        .into_iter()
        .filter(|(_, tasks)| !tasks.is_empty())
        .map(|(project, _)| canonical_project_id(&project))
        .collect()
}

fn resolve_sweep_plan(home: &Path, cfg: &SweepConfig) -> anyhow::Result<SweepPlan> {
    let claims = active_team_claims(home, cfg)?;
    let api_base = canonical_api_base(
        cfg.api_base_url
            .as_deref()
            .unwrap_or(DEFAULT_GITHUB_API_BASE),
    );
    let mut store = load_provenance(home);
    let mut health = Vec::new();
    let now = chrono::Utc::now();
    let now_string = now.to_rfc3339();
    let mut boards = Vec::new();
    let mut current_keys = HashSet::new();
    let mut consumed_acknowledgements = Vec::new();

    for (project_id, repo) in &claims {
        let key = provenance_key(project_id, repo, &api_base);
        current_keys.insert(key.clone());
        let (total_tasks, non_terminal_tasks) = board_counts(home, project_id);
        let acknowledged = cfg
            .provenance_acknowledgements
            .iter()
            .any(|ack| ack == &key);
        let has_active_exact = store.entries.iter().any(|entry| {
            entry.retired_at.is_none()
                && provenance_key(&entry.project_id, &entry.repo, &entry.api_base) == key
        });

        if !has_active_exact {
            if total_tasks > 0 && !acknowledged {
                health.push(SweepHealthEntry {
                    code: "BASELINE_UNVERIFIED".to_string(),
                    project_id: project_id.clone(),
                    repo: Some(repo.clone()),
                    total_tasks,
                    non_terminal_tasks,
                    evidence: format!(
                        "board has {total_tasks} task(s) but no acknowledged provenance key {key}"
                    ),
                });
            } else {
                if acknowledged {
                    consumed_acknowledgements.push(key.clone());
                }
                store.next_generation = store.next_generation.saturating_add(1);
                store.entries.push(SweepProvenance {
                    project_id: project_id.clone(),
                    repo: repo.clone(),
                    api_base: api_base.clone(),
                    observed_at: now_string.clone(),
                    config_generation: store.next_generation,
                    legacy_since: None,
                    retired_at: None,
                    retirement_reason: None,
                });
                boards.push(SweepBoard {
                    project_id: project_id.clone(),
                    repo: repo.clone(),
                    api_base: api_base.clone(),
                    legacy_compat: false,
                });
            }
        } else {
            if let Some(entry) = store.entries.iter_mut().find(|entry| {
                entry.retired_at.is_none()
                    && provenance_key(&entry.project_id, &entry.repo, &entry.api_base) == key
            }) {
                // A mapping that becomes active again starts a fresh quiet
                // period if it later becomes legacy once more.
                entry.legacy_since = None;
            }
            boards.push(SweepBoard {
                project_id: project_id.clone(),
                repo: repo.clone(),
                api_base: api_base.clone(),
                legacy_compat: false,
            });
        }
    }

    for entry in &mut store.entries {
        let entry_key = provenance_key(&entry.project_id, &entry.repo, &entry.api_base);
        if entry.retired_at.is_some() || current_keys.contains(&entry_key) {
            continue;
        }
        let (total_tasks, non_terminal_tasks) = board_counts(home, &entry.project_id);
        let acknowledged = cfg
            .provenance_acknowledgements
            .iter()
            .any(|ack| ack == &entry_key);
        let legacy_since = entry
            .legacy_since
            .get_or_insert_with(|| now_string.clone())
            .clone();
        let age_elapsed = chrono::DateTime::parse_from_rfc3339(&legacy_since)
            .map(|observed| {
                now.signed_duration_since(observed.with_timezone(&chrono::Utc))
                    .num_seconds()
                    >= LEGACY_RETIREMENT_TTL_SECS
            })
            .unwrap_or(false);
        if acknowledged || (non_terminal_tasks == 0 && age_elapsed) {
            if acknowledged {
                consumed_acknowledgements.push(entry_key.clone());
            }
            entry.retired_at = Some(now_string.clone());
            entry.retirement_reason = Some(if acknowledged {
                "operator_acknowledged".to_string()
            } else {
                "quiet_ttl_elapsed".to_string()
            });
            continue;
        }
        // Every non-retired historical mapping remains an active compatibility
        // scan. Board emptiness and filesystem mtime are not retirement signals;
        // retirement is intentionally limited to the auditable conditions above.
        boards.push(SweepBoard {
            project_id: canonical_project_id(&entry.project_id),
            repo: entry.repo.clone(),
            api_base: canonical_api_base(&entry.api_base),
            legacy_compat: true,
        });
        health.push(SweepHealthEntry {
            code: "LEGACY_COMPAT".to_string(),
            project_id: canonical_project_id(&entry.project_id),
            repo: Some(entry.repo.clone()),
            total_tasks,
            non_terminal_tasks,
            evidence: format!(
                "retained provenance key {entry_key} until acknowledgement or quiet TTL"
            ),
        });
    }

    if !consumed_acknowledgements.is_empty() {
        let mut updated_cfg = cfg.clone();
        updated_cfg
            .provenance_acknowledgements
            .retain(|ack| !consumed_acknowledgements.iter().any(|key| key == ack));
        save_config(home, &updated_cfg)?;
    }

    // Explicitly-created boards that cannot be deterministically paired to a
    // team/repo stay visible but are never guessed into a sweep.
    for project_id in current_board_projects(home) {
        if !claims.contains_key(&project_id) {
            let (total_tasks, non_terminal_tasks) = board_counts(home, &project_id);
            health.push(SweepHealthEntry {
                code: "MANUAL_UNMAPPED".to_string(),
                project_id,
                repo: None,
                total_tasks,
                non_terminal_tasks,
                evidence: "explicit project board has no deterministic team/repo mapping"
                    .to_string(),
            });
        }
    }

    save_provenance(home, &store)?;
    save_health(home, health.clone())?;
    Ok(SweepPlan {
        default_repo: claims.get(crate::task_events::DEFAULT_PROJECT).cloned(),
        boards,
    })
}

/// Return the last persisted sweep diagnostics for `task action=health`.
pub fn task_sweep_health(home: &Path) -> serde_json::Value {
    serde_json::to_value(crate::store::load::<SweepHealthSnapshot>(&health_path(
        home,
    )))
    .unwrap_or_else(|_| serde_json::json!({"as_of": "", "entries": []}))
}

/// Explicit project names are operator-owned board selectors. Record an
/// auditable warning when the project cannot be paired to exactly one current
/// team/repo mapping; the board remains available to named-board CRUD.
pub fn note_explicit_project(home: &Path, project_id: &str) {
    let project_id = canonical_project_id(project_id);
    let cfg = load_config(home);
    let mapped = active_team_claims(home, &cfg)
        .ok()
        .and_then(|claims| claims.get(&project_id).cloned());
    if mapped.is_some() {
        return;
    }
    let (total_tasks, non_terminal_tasks) = board_counts(home, &project_id);
    let mut snapshot = crate::store::load::<SweepHealthSnapshot>(&health_path(home));
    snapshot
        .entries
        .retain(|entry| !(entry.code == "MANUAL_UNMAPPED" && entry.project_id == project_id));
    snapshot.entries.push(SweepHealthEntry {
        code: "MANUAL_UNMAPPED".to_string(),
        project_id: project_id.clone(),
        repo: None,
        total_tasks,
        non_terminal_tasks,
        evidence:
            "explicit project board retained as named-board access; no deterministic sweep mapping"
                .to_string(),
    });
    snapshot.as_of = chrono::Utc::now().to_rfc3339();
    if let Err(error) = crate::store::save_atomic(&health_path(home), &snapshot) {
        tracing::warn!(%error, project = %project_id, "task_sweep: failed to persist MANUAL_UNMAPPED evidence");
    }
    tracing::warn!(project = %project_id, "task_sweep: MANUAL_UNMAPPED explicit project board");
}

fn manual_unmapped_candidate(
    home: &Path,
    project_id: &str,
) -> anyhow::Result<(String, usize, usize)> {
    let project_id = canonical_project_id(project_id);
    anyhow::ensure!(
        !project_id.is_empty(),
        "manual provenance acknowledgement requires a project id"
    );

    let cfg = load_config(home);
    let claims = active_team_claims(home, &cfg)?;
    anyhow::ensure!(
        !claims.contains_key(&project_id),
        "project '{project_id}' has a deterministic team/repository mapping; use the mapped provenance acknowledgement"
    );

    // Only acknowledge a materialized, explicitly named board.  In
    // particular, do not accept a path or source repository and do not create
    // a board as a side effect of this read-only inspection.
    let explicit_projects = crate::tasks::explicit_project_ids(home)
        .map_err(|error| anyhow::anyhow!("enumerate explicit project boards: {error}"))?;
    anyhow::ensure!(
        explicit_projects
            .iter()
            .any(|candidate| canonical_project_id(candidate) == project_id),
        "project '{project_id}' is not an explicit materialized board"
    );
    let (total_tasks, non_terminal_tasks) = board_counts(home, &project_id);
    anyhow::ensure!(
        total_tasks > 0,
        "project '{project_id}' has no tasks to acknowledge"
    );
    Ok((project_id, total_tasks, non_terminal_tasks))
}

/// Explicitly acknowledge the provenance of a `MANUAL_UNMAPPED` board.
///
/// The acknowledgement is deliberately narrower than a sweep mapping: it
/// records only the named project and operator evidence.  It never supplies a
/// repository, never changes the sweep plan, and never mutates task events.
/// With `dry_run`, all checks and counts are performed but no file or audit
/// log is written.
pub fn acknowledge_manual_unmapped_provenance(
    home: &Path,
    project_id: &str,
    actor: &str,
    audit_reason: &str,
    dry_run: bool,
) -> serde_json::Value {
    let actor = actor.trim();
    let audit_reason = audit_reason.trim();
    if actor.is_empty() {
        return serde_json::json!({"error": "manual provenance acknowledgement requires a non-empty actor"});
    }
    if audit_reason.is_empty() {
        return serde_json::json!({"error": "manual provenance acknowledgement requires a non-empty audit reason"});
    }

    let (project_id, total_tasks, non_terminal_tasks) =
        match manual_unmapped_candidate(home, project_id) {
            Ok(candidate) => candidate,
            Err(error) => return serde_json::json!({"error": error.to_string()}),
        };
    let store = load_provenance(home);
    if let Some(existing) = store
        .manual_unmapped_acknowledgements
        .iter()
        .find(|ack| ack.project_id == project_id)
    {
        return serde_json::json!({
            "project_id": project_id,
            "outcome": "already_acknowledged",
            "acknowledged_at": existing.acknowledged_at,
            "actor": existing.actor,
            "audit_reason": existing.audit_reason,
            "total_tasks": existing.total_tasks,
            "non_terminal_tasks": existing.non_terminal_tasks,
            "board_mutation": "none",
            "task_mutation": "none",
        });
    }

    let response = serde_json::json!({
        "project_id": project_id,
        "outcome": if dry_run { "would_acknowledge" } else { "acknowledged" },
        "actor": actor,
        "audit_reason": audit_reason,
        "total_tasks": total_tasks,
        "non_terminal_tasks": non_terminal_tasks,
        "repo": serde_json::Value::Null,
        "board_mutation": "none",
        "task_mutation": "none",
        "dry_run": dry_run,
    });
    if dry_run {
        return response;
    }

    let acknowledged_at = chrono::Utc::now().to_rfc3339();
    let mut updated = store;
    updated
        .manual_unmapped_acknowledgements
        .push(ManualUnmappedAcknowledgement {
            project_id: project_id.clone(),
            actor: actor.to_string(),
            audit_reason: audit_reason.to_string(),
            acknowledged_at: acknowledged_at.clone(),
            total_tasks,
            non_terminal_tasks,
        });
    if let Err(error) = save_provenance(home, &updated) {
        return serde_json::json!({
            "error": format!("manual provenance acknowledgement save failed: {error}"),
            "project_id": project_id,
            "outcome": "not_acknowledged",
            "board_mutation": "none",
            "task_mutation": "none",
        });
    }

    // The provenance store is the durable audit record.  This event makes the
    // operator action visible in the normal daemon audit stream as well.
    crate::event_log::log(
        home,
        "manual_unmapped_provenance_acknowledged",
        actor,
        &serde_json::json!({
            "project_id": project_id,
            "audit_reason": audit_reason,
            "acknowledged_at": acknowledged_at,
            "total_tasks": total_tasks,
            "non_terminal_tasks": non_terminal_tasks,
            "repo": null,
        })
        .to_string(),
    );
    response
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
    let plan = match resolve_sweep_plan(home, &cfg) {
        Ok(plan) => plan,
        Err(error) => {
            let entry = SweepHealthEntry {
                code: "CONFLICT_FAIL_CLOSED".to_string(),
                project_id: crate::task_events::DEFAULT_PROJECT.to_string(),
                repo: None,
                total_tasks: 0,
                non_terminal_tasks: 0,
                evidence: error.to_string(),
            };
            save_health(home, vec![entry])?;
            tracing::warn!(%error, "task_sweep: board resolution failed closed");
            return Ok(());
        }
    };
    if plan.boards.is_empty() {
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
    let mut default_prs: Option<Arc<Vec<PrMeta>>> = None;
    let mut fetch_cache: HashMap<(String, String), Arc<anyhow::Result<Vec<PrMeta>>>> =
        HashMap::new();
    for board in &plan.boards {
        let cache_key = (board.api_base.clone(), board.repo.clone());
        let result = fetch_cache
            .entry(cache_key)
            .or_insert_with(|| Arc::new(list_recently_merged_prs(&board.repo, &board.api_base)))
            .clone();
        match result.as_ref() {
            Ok(prs) => {
                match sweep_board_with_prs(
                    home,
                    &board.project_id,
                    prs,
                    fleet_cfg.as_ref(),
                    cfg.dry_run,
                ) {
                    Ok(scanned) => {
                        if board.project_id == crate::task_events::DEFAULT_PROJECT
                            && !board.legacy_compat
                        {
                            default_scanned = scanned;
                            default_prs = Some(Arc::new(prs.clone()));
                        }
                    }
                    Err(error) => tracing::warn!(
                        project = %board.project_id,
                        repo = %board.repo,
                        api_base = %board.api_base,
                        error = %error,
                        "task_sweep: board close path failed"
                    ),
                }
            }
            Err(error) => tracing::warn!(
                project = %board.project_id,
                repo = %board.repo,
                api_base = %board.api_base,
                error = %error,
                "task_sweep: board scan failed"
            ),
        }
    }

    // Issue #664: compliance scan stays keyed on the operator's primary repo
    // (`cfg.repo`) and gated on the DEFAULT board's scan — #2117 P2 covers
    // auto-close routing, not compliance (out of scope). Byte-identical: in a
    // single-project deployment the DEFAULT board IS `cfg.repo`.
    if default_scanned && cfg.compliance_mode != "off" {
        if let Some(repo) = plan.default_repo.as_deref() {
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
#[allow(dead_code)]
fn resolve_sweep_boards(home: &Path, cfg: &SweepConfig) -> Vec<(String, String)> {
    match resolve_sweep_plan(home, cfg) {
        Ok(plan) => plan
            .boards
            .into_iter()
            .filter(|board| !board.legacy_compat)
            .map(|board| (board.project_id, board.repo))
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "task_sweep: compatibility board resolution failed closed");
            Vec::new()
        }
    }
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
                // #78445-2 (d): merged-PR auto-close is a terminal transition — clear
                // BOTH obligation stores (this path previously cleared NEITHER, so a
                // sweep-closed task's stuck-dispatch rows nagged the reviewer).
                crate::tasks::task_terminal_cleanup(home, &marker);
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
    // #2760 (codex ruling m-…-1154): EXTRACTION (this regex, which finds candidate
    // `Closes t-…` tokens bounded in prose) is separated from VALIDATION — every
    // extracted candidate is validated through the single authoritative
    // `TaskId::parse_canonical` grammar owner, so a task-id's validity is a type
    // invariant, not a per-site string pattern. (The extraction regex already
    // matches the canonical shape, so this is behavior-preserving; it makes the
    // parser the authority.)
    re.captures_iter(body)
        .filter_map(|c| c.get(1))
        .filter_map(|m| crate::task_events::TaskId::parse_canonical(m.as_str()).map(|t| t.0))
        .collect()
}

// ── GitHub API ──────────────────────────────────────────────────────

/// PR metadata captured from the GitHub list-pulls response. Fields
/// chosen to satisfy the 5 sweep validation must-haves; intermediate
/// JSON parsing in [`parse_pr_meta`] flags schema mismatches.
#[derive(Clone)]
struct PrMeta {
    number: u64,
    #[allow(dead_code)]
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

// ── Operator-facing CLI surface (#2547: moved from the task_sweep_config MCP tool) ──

/// `agend-terminal admin task-sweep-config` CLI body. Args:
/// - `repo`: `"owner/repo"` to enable; empty string disables.
/// - `pause`: `true|false`.
/// - `dry_run`: `true|false`.
///
/// Returns the resulting [`SweepConfig`] state as JSON so the operator
/// can verify the change without a follow-up read.
pub fn handle_task_sweep_config(home: &Path, args: &serde_json::Value) -> serde_json::Value {
    // Manual provenance acknowledgement is a separate, read-only-by-default
    // operator flow.  Keep it out of the normal sweep-config mutation path so
    // `provenance_dry_run` cannot accidentally persist a config change.
    if let Some(request) = args.get("acknowledge_manual_unmapped") {
        let (project_id, actor, audit_reason, request_dry_run) = match request {
            serde_json::Value::String(project_id) => (
                project_id.as_str(),
                args.get("provenance_actor")
                    .or_else(|| args.get("actor"))
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                args.get("provenance_reason")
                    .or_else(|| args.get("audit_reason"))
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                None,
            ),
            serde_json::Value::Object(request) => (
                request
                    .get("project_id")
                    .or_else(|| request.get("project"))
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                request
                    .get("actor")
                    .or_else(|| request.get("provenance_actor"))
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        args.get("provenance_actor")
                            .or_else(|| args.get("actor"))
                            .and_then(|value| value.as_str())
                    })
                    .unwrap_or(""),
                request
                    .get("audit_reason")
                    .or_else(|| request.get("reason"))
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        args.get("provenance_reason")
                            .or_else(|| args.get("audit_reason"))
                            .and_then(|value| value.as_str())
                    })
                    .unwrap_or(""),
                request.get("dry_run").and_then(|value| value.as_bool()),
            ),
            _ => {
                return serde_json::json!({
                    "error": "acknowledge_manual_unmapped must be a project id or request object"
                })
            }
        };
        let dry_run = args
            .get("provenance_dry_run")
            .and_then(|value| value.as_bool())
            .or_else(|| args.get("dry_run").and_then(|value| value.as_bool()))
            .or(request_dry_run)
            .unwrap_or(false);
        return acknowledge_manual_unmapped_provenance(
            home,
            project_id,
            actor,
            audit_reason,
            dry_run,
        );
    }

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
    if let Some(keys) = args
        .get("acknowledge_provenance")
        .and_then(|value| value.as_array())
    {
        for key in keys.iter().filter_map(|value| value.as_str()) {
            if !key.trim().is_empty() && !cfg.provenance_acknowledgements.iter().any(|v| v == key) {
                cfg.provenance_acknowledgements.push(key.to_string());
            }
        }
        cfg.provenance_acknowledgements.sort();
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
        "provenance_acknowledgements": cfg.provenance_acknowledgements,
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
                    pr = v.pr_number,
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
#[path = "task_sweep/tests.rs"]
mod tests;

#[cfg(test)]
mod review_repro_daemon_retention;
