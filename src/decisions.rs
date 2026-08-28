//! Decision storage — CRUD over JSON files in {home}/decisions/.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 500;
const DEFAULT_SCAN_BUDGET: usize = 500;
const MAX_SCAN_BUDGET: usize = 5_000;
const MAX_BATCH_CANDIDATES: usize = 100;
const BATCH_CONFIRM_TTL_SECS: i64 = 15 * 60;

#[derive(Debug, Serialize, Deserialize)]
struct DecisionCursor {
    version: u8,
    namespace: String,
    physical_filename: String,
    parsed_id: Option<String>,
    consistency: String,
    snapshot_digest: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchCandidateSnapshot {
    id: String,
    physical_filename: String,
    content_digest: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct BatchSourceSnapshot {
    physical_filename: String,
    content_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchConfirmation {
    schema_version: u8,
    actor: String,
    created_at: String,
    audit_reason: String,
    policy_digest: String,
    source_digest: String,
    preview_digest: String,
    #[serde(default)]
    candidate_cap: usize,
    #[serde(default)]
    candidates_capped: bool,
    sources: Vec<BatchSourceSnapshot>,
    candidates: Vec<BatchCandidateSnapshot>,
}

/// #1990: on-disk schema version for a decision record (per-file store, so this
/// follows the per-record `task_progress` pattern — a module const + an explicit
/// read guard — rather than the whole-file `SchemaVersioned` trait). Stamped on
/// every write; a record with `schema_version > SCHEMA_VERSION` was written by a
/// newer daemon and is fail-closed on read (skipped in listings, refused for
/// update) rather than silently downgraded. Additive field adds (new fields with
/// serde defaults) do NOT need a bump.
const SCHEMA_VERSION: u32 = 1;

/// #2305: lifecycle of a decision that requires an operator answer. A plain
/// scope-record decision has `status: None`; a posted *question* is `Pending`
/// until answered (or expired).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Pending,
    Answered,
    Expired,
}

/// #2305: one selectable answer option for a pending decision. `recommended`
/// marks the suggested choice (poster convention: list the recommended option
/// first, like AskUserQuestion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOption {
    pub label: String,
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub content: String,
    pub scope: String, // "project" or "fleet"
    pub author: String,
    pub tags: Vec<String>,
    pub ttl_days: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
    pub supersedes: Option<String>,
    pub working_directory: Option<String>,
    /// Optional typed review authority. Additive and create-only; tasks may
    /// inherit this value when they name the decision as their governor.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::daemon::pr_state::review_class_serde"
    )]
    pub review_class: Option<crate::daemon::pr_state::ReviewClass>,
    /// #1990: see [`SCHEMA_VERSION`]. `#[serde(default)]` → a pre-#1990 record
    /// (no field) reads back as 0 (≤ current, loads normally).
    #[serde(default)]
    pub schema_version: u32,

    // ── #2305 async decision-board fields ──
    // All additive with serde defaults: a plain scope record leaves these at
    // their defaults and behaves EXACTLY as before. Per the `SCHEMA_VERSION`
    // doc, additive defaulted fields do NOT bump the version (and bumping would
    // make every new record invisible to a not-yet-upgraded reader, since
    // `load_all` skips `schema_version > SCHEMA_VERSION`).
    /// This decision is a question awaiting an operator answer.
    #[serde(default)]
    pub needs_answer: bool,
    /// `None` for a plain decision; `Pending`/`Answered`/`Expired` for a question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DecisionStatus>,
    /// Suggested answer options (recommended-first by convention).
    #[serde(default)]
    pub options: Vec<DecisionOption>,
    /// Whether a free-text answer (not matching any option) is accepted.
    #[serde(default)]
    pub allow_free_text: bool,
    /// The chosen option label or free-text, once answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Who answered (the operator, or the agent that recorded it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    /// RFC3339 time the answer was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,

    // ── #2524 P2c / #2313 optional per-decision timeout+default ──
    // Additive with serde defaults: a question posted without `timeout_secs`
    // leaves both at `None` and is untouched by `DecisionBoardTimeoutTracker`
    // — behaves exactly as before #2313 (indefinite wait, the default).
    /// Seconds after `created_at` before the tracker auto-answers with
    /// `timeout_default`. `None` = wait indefinitely (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// The option label auto-applied on timeout. Required (and validated at
    /// `post` time) whenever `timeout_secs` is set — either given explicitly
    /// or derived from the `recommended` option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_default: Option<String>,
}

pub(crate) fn decisions_dir(home: &Path) -> std::path::PathBuf {
    home.join("decisions")
}

/// Check if `caller` is allowed to mutate `decision`.
///
/// Mirrors the `tasks::can_mutate_task` gate (Sprint 20 Track D Praise replicate
/// pattern): the author of the decision can always mutate; an orchestrator of
/// the author's team can mutate as admin override; everyone else is rejected.
///
/// Closes the cascade auth chain headline finding (Sprint 20 Track D MCP audit
/// C1 + Sprint 20.5 Track 6 cross-validation): without this gate, a
/// prompt-injected agent could silently archive operator strategic decisions.
///
/// `decision.author` is `String` (always present) and `caller` is `&str` —
/// comparison is unambiguous string equality, no integer coercion path
/// (operator-known-pitfall: caller string with numeric-looking suffix like
/// `"dev-impl-1"` is not parsed as int when checking against `decision.author`).
pub fn can_mutate_decision(home: &Path, caller: &str, decision: &Decision) -> bool {
    if decision.author == caller {
        return true;
    }
    if crate::teams::is_orchestrator_of(home, caller, &decision.author) {
        return true;
    }
    false
}

fn load_all(home: &Path) -> Vec<Decision> {
    let dir = decisions_dir(home);
    let mut decisions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(d) = serde_json::from_str::<Decision>(&content) {
                        // #1990: skip a record a newer daemon wrote — we cannot
                        // be sure we understand all its fields, so don't surface
                        // (or risk re-saving and downgrading) it.
                        if d.schema_version > SCHEMA_VERSION {
                            tracing::warn!(
                                id = %d.id,
                                found = d.schema_version,
                                supported = SCHEMA_VERSION,
                                "skipping decision written by a newer schema version"
                            );
                            continue;
                        }
                        decisions.push(d);
                    }
                }
            }
        }
    }
    decisions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    decisions
}

pub(crate) fn decision_path(home: &Path, id: &str) -> std::path::PathBuf {
    decisions_dir(home).join(format!("{id}.json"))
}

/// The resolved authority named by a task create request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GoverningDecision {
    pub review_class: Option<crate::daemon::pr_state::ReviewClass>,
}

/// Resolve one exact decision as an active, unsuperseded leaf. The entire
/// decision directory is scanned so malformed/newer records or multiple
/// reverse `supersedes` links cannot be hidden by the normal list path.
pub(crate) fn resolve_governing_decision(
    home: &Path,
    id: &str,
) -> anyhow::Result<GoverningDecision> {
    if id.is_empty() || id == "." || id == ".." || id.contains('/') || id.contains('\\') {
        anyhow::bail!("governing decision id is not a safe exact identifier")
    }
    let dir = decisions_dir(home);
    let exact_path = decision_path(home, id);
    let exact_raw = std::fs::read_to_string(&exact_path)
        .map_err(|e| anyhow::anyhow!("governing decision '{id}' is missing: {e}"))?;
    let exact: Decision = serde_json::from_str(&exact_raw)
        .map_err(|e| anyhow::anyhow!("governing decision '{id}' is corrupt: {e}"))?;
    if exact.id != id {
        anyhow::bail!("governing decision '{id}' has mismatched record identity")
    }
    if exact.schema_version > SCHEMA_VERSION {
        anyhow::bail!("governing decision '{id}' uses a newer schema")
    }
    if exact.archived {
        anyhow::bail!("governing decision '{id}' is archived")
    }

    let mut reverse = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| anyhow::anyhow!("decision directory is unreadable: {e}"))?;
    for entry in entries {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let path = entry.path();
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!("decision record '{}' is unreadable: {e}", path.display())
        })?;
        let decision: Decision = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("decision record '{}' is corrupt: {e}", path.display()))?;
        if decision.schema_version > SCHEMA_VERSION {
            anyhow::bail!("decision record '{}' uses a newer schema", path.display())
        }
        if decision.supersedes.as_deref() == Some(id) {
            reverse.push(decision);
        }
    }
    if !reverse.is_empty() {
        if reverse.len() > 1 {
            anyhow::bail!("governing decision '{id}' has an ambiguous superseding leaf")
        }
        anyhow::bail!("governing decision '{id}' is superseded")
    }
    Ok(GoverningDecision {
        review_class: exact.review_class,
    })
}

fn decision_lock_path(home: &Path, id: &str) -> std::path::PathBuf {
    decisions_dir(home).join(format!("{id}.lock"))
}

/// Atomic save under a per-decision flock. Callers that also *read* the
/// current contents before mutating (see supersede / update flows) must
/// hold the lock across the whole read→mutate→save cycle via
/// [`with_decision_lock`] — this function acquires the lock only for the
/// write itself.
fn save(home: &Path, decision: &Decision) -> anyhow::Result<()> {
    let dir = decisions_dir(home);
    std::fs::create_dir_all(&dir)?;
    let _lock = crate::store::acquire_file_lock(&decision_lock_path(home, &decision.id))?;
    crate::store::save_atomic(&decision_path(home, &decision.id), decision)
}

/// Hold the per-decision flock for the duration of `f`. flock is not
/// re-entrant, so inside `f` callers must write via `save_atomic` directly
/// rather than calling [`save`], which would deadlock on the same path.
pub(crate) fn with_decision_lock<R>(
    home: &Path,
    id: &str,
    f: impl FnOnce() -> R,
) -> anyhow::Result<R> {
    let dir = decisions_dir(home);
    std::fs::create_dir_all(&dir)?;
    let _lock = crate::store::acquire_file_lock(&decision_lock_path(home, id))?;
    Ok(f())
}

pub fn post(home: &Path, author: &str, args: &Value) -> Value {
    let title = match args["title"].as_str() {
        Some(t) => t,
        None => return serde_json::json!({"error": "missing 'title'"}),
    };
    // #2037 (3): `text` accepted as alias — `send` calls its body `message`,
    // inbox renders `text`; `content` stays canonical for decisions.
    let content = match args["content"].as_str().or_else(|| args["text"].as_str()) {
        Some(c) => c,
        None => return serde_json::json!({"error": "missing 'content' (alias: text)"}),
    };
    let scope = args["scope"].as_str().unwrap_or("project");
    let tags: Vec<String> = args["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ttl_days = args["ttl_days"].as_u64();
    let supersedes = args["supersedes"].as_str().map(String::from);

    // #2305: optional pending-question fields. A normal `post` (no
    // `needs_answer`) leaves these defaulted → a plain scope record.
    let needs_answer = args["needs_answer"].as_bool().unwrap_or(false);
    let options = parse_options(&args["options"]);
    let allow_free_text = args["allow_free_text"].as_bool().unwrap_or(false);
    let status = needs_answer.then_some(DecisionStatus::Pending);

    // #2524 P2c / #2313: optional per-decision timeout+default. Validated
    // before any side effects (ID gen, supersede-archive) so a bad request
    // never partially commits. `timeout_secs` absent (the default) leaves
    // `timeout_default` at `None` too — untouched by
    // `DecisionBoardTimeoutTracker`, byte-identical to pre-#2313 behavior.
    let timeout_secs = args["timeout_secs"].as_u64();
    if timeout_secs.is_some() && !needs_answer {
        return serde_json::json!({
            "error": "timeout_secs is only valid when needs_answer=true"
        });
    }
    let timeout_default = if timeout_secs.is_some() {
        let explicit = args["timeout_default"].as_str().map(String::from);
        let derived = explicit.or_else(|| {
            options
                .iter()
                .find(|o| o.recommended)
                .map(|o| o.label.clone())
        });
        match derived {
            Some(d) => Some(d),
            None => {
                return serde_json::json!({
                    "error": "timeout_secs requires 'timeout_default' or an options \
                              entry with recommended=true to derive it from"
                })
            }
        }
    } else {
        None
    };
    let review_class = match args.get("review_class") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            match crate::daemon::pr_state::ReviewClass::parse_fail_closed(Some(raw)) {
                crate::daemon::pr_state::ReviewClass::Single => {
                    Some(crate::daemon::pr_state::ReviewClass::Single)
                }
                crate::daemon::pr_state::ReviewClass::Dual => {
                    Some(crate::daemon::pr_state::ReviewClass::Dual)
                }
                crate::daemon::pr_state::ReviewClass::Unresolved => {
                    return serde_json::json!({
                        "error": "review_class must be exactly 'single' or 'dual'",
                        "code": "invalid_review_class",
                    })
                }
            }
        }
        Some(_) => {
            return serde_json::json!({
                "error": "review_class must be a string ('single' or 'dual')",
                "code": "invalid_review_class",
            })
        }
    };

    let clock = chrono::Utc::now();
    let now = clock.to_rfc3339();
    // The historical id format was seconds-precision only — two posts in the
    // same UTC second collided and the second silently overwrote the first.
    // Append nanoseconds + a process-local counter so no two posts from the
    // same process can share an id, even when issued back-to-back.
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = clock.format("%Y%m%d%H%M%S%6f");
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("d-{ts}-{seq}");

    // Archive the superseded decision under its own flock. The previous
    // implementation read-all → mutated-one → saved outside any lock, so
    // two concurrent callers (post(supersedes=X) + update(X), or two
    // posts both superseding X) would race: both read the same old
    // record, both flip fields, whichever wrote last clobbered the other.
    if let Some(ref old_id) = supersedes {
        let old_id_c = old_id.clone();
        let now_c = now.clone();
        let _ = with_decision_lock(home, &old_id_c, || {
            let path = decision_path(home, &old_id_c);
            let Ok(content) = std::fs::read_to_string(&path) else {
                return;
            };
            let Ok(mut old) = serde_json::from_str::<Decision>(&content) else {
                return;
            };
            // #1990: don't archive (and thereby re-save/downgrade) a record a
            // newer daemon wrote.
            if old.schema_version > SCHEMA_VERSION {
                return;
            }
            old.archived = true;
            old.updated_at = now_c;
            old.schema_version = SCHEMA_VERSION;
            // Write inline; save() re-acquires the same (non-reentrant)
            // flock and would deadlock.
            if let Err(e) = crate::store::save_atomic(&path, &old) {
                tracing::warn!(id = %old_id_c, error = %e, "supersede archive write failed");
            }
        });
    }

    let working_dir = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());

    let decision = Decision {
        id: id.clone(),
        title: title.to_string(),
        content: content.to_string(),
        scope: scope.to_string(),
        author: author.to_string(),
        tags,
        ttl_days,
        created_at: now.clone(),
        updated_at: now,
        archived: false,
        supersedes,
        working_directory: working_dir,
        review_class,
        schema_version: SCHEMA_VERSION,
        needs_answer,
        status,
        options,
        allow_free_text,
        answer: None,
        answered_by: None,
        answered_at: None,
        timeout_secs,
        timeout_default,
    };

    match save(home, &decision) {
        Ok(()) => serde_json::json!({"id": id, "status": "posted"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// #2305: parse the `options` arg — accepts either `[{label, recommended}]`
/// objects or bare `["label", …]` strings (recommended=false). Unparseable
/// entries are dropped.
fn parse_options(v: &Value) -> Vec<DecisionOption> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    if let Some(s) = o.as_str() {
                        Some(DecisionOption {
                            label: s.to_string(),
                            recommended: false,
                        })
                    } else {
                        o.get("label")
                            .and_then(|l| l.as_str())
                            .map(|label| DecisionOption {
                                label: label.to_string(),
                                recommended: o
                                    .get("recommended")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Return active decisions as typed structs (no JSON round-trip).
pub fn list_all(home: &Path) -> Vec<Decision> {
    load_all(home).into_iter().filter(|d| !d.archived).collect()
}

/// #2305: active questions awaiting an operator answer (`needs_answer` &&
/// `status == Pending`), newest-first. Backs the interactive answer overlay.
pub fn list_pending(home: &Path) -> Vec<Decision> {
    load_all(home)
        .into_iter()
        .filter(|d| !d.archived && d.needs_answer && d.status == Some(DecisionStatus::Pending))
        .collect()
}

/// #2313 P2b: pending-question tally bucketed by author, computed with ONE
/// `list_pending` scan — feeds both the status-line total and the per-pane
/// "you asked something" badge without each caller re-scanning `decisions/`
/// (the same disk-I/O-storm shape `should_sync_notifications` already
/// documents and throttles for the notification badge).
#[derive(Clone)]
pub struct PendingDecisionCounts {
    pub total: usize,
    pub by_author: std::collections::HashMap<String, usize>,
}

/// #3031: memoized result of the last [`count_pending`] scan, keyed by the
/// decisions directory and its modified time.
///
/// The TUI badge re-tallies once a second (`app::DECISION_SYNC_INTERVAL`), and
/// each tally previously re-read and re-parsed every file in `decisions/` — a
/// cost that grows with total history, not with the pending count. Decisions
/// mutate rarely compared to that cadence, so nearly every tally can be served
/// from the previous one.
///
/// Keying on the directory mtime is sound because every canonical decision
/// write goes through [`save`] → `store::atomic_write`, which lands a temp file
/// in this same directory and `rename`s it into place; creates, updates and
/// deletes all move the directory's mtime, including ones made by another
/// process. A write that bypasses `atomic_write` does not, and is unsupported.
static PENDING_COUNT_CACHE: std::sync::Mutex<
    Option<(
        std::path::PathBuf,
        std::time::SystemTime,
        PendingDecisionCounts,
    )>,
> = std::sync::Mutex::new(None);

pub fn count_pending(home: &Path) -> PendingDecisionCounts {
    let dir = decisions_dir(home);
    // No readable mtime (directory absent, or a stat error): bypass the cache
    // entirely rather than guessing — scan, and leave any existing entry alone.
    let mtime = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();

    if let Some(mtime) = mtime {
        if let Ok(cache) = PENDING_COUNT_CACHE.lock() {
            if let Some((cached_dir, cached_mtime, counts)) = cache.as_ref() {
                if cached_dir == &dir && cached_mtime == &mtime {
                    return counts.clone();
                }
            }
        }
    }

    let mut by_author: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for d in list_pending(home) {
        *by_author.entry(d.author).or_insert(0) += 1;
    }
    let total = by_author.values().sum();
    let counts = PendingDecisionCounts { total, by_author };

    if let Some(mtime) = mtime {
        if let Ok(mut cache) = PENDING_COUNT_CACHE.lock() {
            *cache = Some((dir, mtime, counts.clone()));
        }
    }
    counts
}

fn cursor_encode(cursor: &DecisionCursor) -> anyhow::Result<String> {
    use base64::Engine as _;
    let bytes = serde_json::to_vec(cursor)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn cursor_decode(raw: &str) -> anyhow::Result<DecisionCursor> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw)?;
    let cursor: DecisionCursor = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(cursor.version == 1, "unsupported cursor version");
    Ok(cursor)
}

fn decision_source_dir(home: &Path, namespace: &str) -> Option<PathBuf> {
    match namespace {
        "live" => Some(decisions_dir(home)),
        "audit_history" => Some(decisions_dir(home).join(".archive")),
        _ => None,
    }
}

fn sorted_json_paths(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            anyhow::ensure!(
                entry.file_type()?.is_file(),
                "non-regular JSON source '{}'",
                path.display()
            );
            paths.push(path);
        }
    }
    // Decision IDs are timestamp-prefixed, so descending physical filename
    // order preserves the historical newest-first default without parsing the
    // whole store before returning the first bounded page.
    paths.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(paths)
}

fn batch_sources_with_archived_candidates(
    dir: &Path,
    archive: &Path,
    confirmation: &BatchConfirmation,
) -> anyhow::Result<Vec<BatchSourceSnapshot>> {
    let mut names = sorted_json_paths(dir)?
        .into_iter()
        .filter_map(|path| path.file_name()?.to_str().map(String::from))
        .collect::<Vec<_>>();
    for candidate in &confirmation.candidates {
        if dir.join(&candidate.physical_filename).exists() {
            continue;
        }
        let archived = archive.join(&candidate.physical_filename);
        if let Ok(metadata) = std::fs::symlink_metadata(&archived) {
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "non-regular archived JSON source '{}'",
                archived.display()
            );
            names.push(candidate.physical_filename.clone());
        }
    }
    names.sort_by(|left, right| right.cmp(left));
    names.dedup();
    names
        .into_iter()
        .take(confirmation.sources.len())
        .map(|physical_filename| {
            let live = dir.join(&physical_filename);
            let path = if live.exists() {
                live
            } else {
                archive.join(&physical_filename)
            };
            let metadata = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "non-regular JSON source '{}'",
                path.display()
            );
            let raw = std::fs::read(&path)?;
            Ok(BatchSourceSnapshot {
                physical_filename,
                content_digest: crate::daemon::utils::sha256_hex(&raw),
            })
        })
        .collect()
}

fn archive_would_orphan_question(decision: &Decision) -> bool {
    decision.status == Some(DecisionStatus::Pending)
        || (decision.needs_answer
            && !matches!(
                decision.status,
                Some(DecisionStatus::Answered | DecisionStatus::Expired)
            ))
}

fn source_digest(paths: &[PathBuf]) -> String {
    let mut bytes = Vec::new();
    for path in paths {
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
    }
    crate::daemon::utils::sha256_hex(&bytes)
}

fn parse_bound(
    value: &Value,
    key: &str,
) -> Result<Option<chrono::DateTime<chrono::FixedOffset>>, Value> {
    let Some(raw) = value[key].as_str() else {
        return Ok(None);
    };
    chrono::DateTime::parse_from_rfc3339(raw).map(Some).map_err(
        |e| serde_json::json!({"error": format!("invalid '{key}' RFC3339 timestamp: {e}")}),
    )
}

fn decision_matches(
    decision: &Decision,
    include_archived: bool,
    filter_tags: &[String],
    author: Option<&str>,
    status: Option<&str>,
    since: Option<chrono::DateTime<chrono::FixedOffset>>,
    until: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> bool {
    if !include_archived && decision.archived {
        return false;
    }
    if !filter_tags.is_empty() && !filter_tags.iter().any(|tag| decision.tags.contains(tag)) {
        return false;
    }
    if author.is_some_and(|wanted| decision.author != wanted) {
        return false;
    }
    let actual_status = decision.status.map(|value| match value {
        DecisionStatus::Pending => "pending",
        DecisionStatus::Answered => "answered",
        DecisionStatus::Expired => "expired",
    });
    if status.is_some_and(|wanted| actual_status != Some(wanted)) {
        return false;
    }
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&decision.created_at) else {
        return false;
    };
    if since.is_some_and(|bound| created < bound) || until.is_some_and(|bound| created > bound) {
        return false;
    }
    true
}

pub fn list(home: &Path, args: &Value) -> Value {
    let include_archived = args["include_archived"].as_bool().unwrap_or(false);
    let filter_tags: Vec<String> = args["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let namespace = args["view"].as_str().unwrap_or("live");
    let Some(dir) = decision_source_dir(home, namespace) else {
        return serde_json::json!({"error": "'view' must be 'live' or 'audit_history'"});
    };
    let consistency = args["consistency"].as_str().unwrap_or("live");
    if !matches!(consistency, "live" | "snapshot") {
        return serde_json::json!({"error": "'consistency' must be 'live' or 'snapshot'"});
    }
    let limit = args["limit"]
        .as_u64()
        .unwrap_or(DEFAULT_LIST_LIMIT as u64)
        .clamp(1, MAX_LIST_LIMIT as u64) as usize;
    let scan_budget = args["scan_budget"]
        .as_u64()
        .unwrap_or(DEFAULT_SCAN_BUDGET as u64)
        .clamp(1, MAX_SCAN_BUDGET as u64) as usize;
    let since = match parse_bound(args, "since") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let until = match parse_bound(args, "until") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let status = args["status"].as_str();
    if status.is_some_and(|value| !matches!(value, "pending" | "answered" | "expired")) {
        return serde_json::json!({"error": "'status' must be pending, answered, or expired"});
    }

    let paths = match sorted_json_paths(&dir) {
        Ok(paths) => paths,
        Err(error) => {
            return serde_json::json!({"error": format!("decision source is unreadable: {error}")})
        }
    };
    let digest = source_digest(&paths);
    let cursor = match args["cursor"].as_str() {
        Some(raw) => match cursor_decode(raw) {
            Ok(cursor) => Some(cursor),
            Err(e) => return serde_json::json!({"error": format!("invalid cursor: {e}")}),
        },
        None => None,
    };
    if let Some(cursor) = &cursor {
        if cursor.namespace != namespace || cursor.consistency != consistency {
            return serde_json::json!({"error": "cursor namespace/consistency does not match this query"});
        }
        if consistency == "snapshot" && cursor.snapshot_digest.as_deref() != Some(&digest) {
            return serde_json::json!({"error": "snapshot changed; restart pagination without the cursor"});
        }
        if let Some(path) = paths.iter().find(|path| {
            path.file_name().and_then(|value| value.to_str()) == Some(&cursor.physical_filename)
        }) {
            let current_id = std::fs::read(path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<Decision>(&raw).ok())
                .map(|decision| decision.id);
            if cursor.parsed_id.is_some() && current_id != cursor.parsed_id {
                return serde_json::json!({
                    "error": "cursor physical record identity changed; restart pagination"
                });
            }
        }
    }

    let start = cursor
        .as_ref()
        .map(|cursor| {
            paths.partition_point(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|filename| filename >= cursor.physical_filename.as_str())
            })
        })
        .unwrap_or(0);
    let mut decisions = Vec::new();
    let mut errors = Vec::new();
    let mut scanned = 0usize;
    let mut index = start;
    let mut last_cursor = None;
    while index < paths.len() && scanned < scan_budget && decisions.len() < limit {
        let path = &paths[index];
        index += 1;
        scanned += 1;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let mut parsed_id = None;
        match std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| serde_json::from_str::<Decision>(&raw).map_err(anyhow::Error::from))
        {
            Ok(decision) if decision.schema_version <= SCHEMA_VERSION => {
                parsed_id = Some(decision.id.clone());
                if decision_matches(
                    &decision,
                    namespace == "audit_history" || include_archived,
                    &filter_tags,
                    args["author"].as_str(),
                    status,
                    since,
                    until,
                ) {
                    decisions.push(decision);
                }
            }
            Ok(decision) => errors.push(serde_json::json!({
                "physical_filename": filename,
                "code": "newer_schema",
                "schema_version": decision.schema_version,
            })),
            Err(error) => errors.push(serde_json::json!({
                "physical_filename": filename,
                "code": "unreadable_or_malformed",
                "error": error.to_string(),
            })),
        }
        last_cursor = Some(DecisionCursor {
            version: 1,
            namespace: namespace.to_string(),
            physical_filename: filename,
            parsed_id,
            consistency: consistency.to_string(),
            snapshot_digest: (consistency == "snapshot").then(|| digest.clone()),
        });
    }
    let has_more = index < paths.len();
    let next_cursor = has_more
        .then(|| {
            last_cursor
                .as_ref()
                .and_then(|cursor| cursor_encode(cursor).ok())
        })
        .flatten();
    serde_json::json!({
        "decisions": decisions,
        "source": namespace,
        "consistency": consistency,
        "snapshot_scope": (consistency == "snapshot").then_some("directory_membership; record contents are read live per page"),
        "scanned": scanned,
        "scan_budget": scan_budget,
        "limit": limit,
        "scan_exhausted": scanned == scan_budget && has_more,
        "next_cursor": next_cursor,
        "errors": errors,
    })
}

fn protected_policy(home: &Path) -> anyhow::Result<(Vec<String>, String)> {
    let path = crate::fleet::fleet_yaml_path(home);
    if !path.exists() {
        return Ok((Vec::new(), crate::daemon::utils::sha256_hex(b"absent")));
    }
    let raw = std::fs::read(&path)?;
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&raw)?;
    let protected = doc
        .get("retention")
        .and_then(|retention| retention.get("protected_decision_tags"))
        .map(|value| {
            value
                .as_sequence()
                .ok_or_else(|| anyhow::anyhow!("retention.protected_decision_tags must be a list"))
        })
        .transpose()?
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("protected decision tags must be strings"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((protected, crate::daemon::utils::sha256_hex(&raw)))
}

fn confirmation_dir(home: &Path) -> PathBuf {
    decisions_dir(home).join(".batch-confirmations")
}

fn confirmation_path(home: &Path, token: &str) -> Option<PathBuf> {
    if token.len() == 36
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        Some(confirmation_dir(home).join(format!("{token}.json")))
    } else {
        None
    }
}

fn reap_expired_confirmations(home: &Path) {
    let Ok(entries) = std::fs::read_dir(confirmation_dir(home)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_regular_json = entry.file_type().is_ok_and(|kind| kind.is_file())
            && path.extension().and_then(|value| value.to_str()) == Some("json");
        if !is_regular_json {
            continue;
        }
        let age = std::fs::read(&path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<BatchConfirmation>(&raw).ok())
            .and_then(|confirmation| {
                chrono::DateTime::parse_from_rfc3339(&confirmation.created_at).ok()
            })
            .map(|created| {
                chrono::Utc::now()
                    .signed_duration_since(created)
                    .num_seconds()
            });
        if age.is_none_or(|age| !(0..=BATCH_CONFIRM_TTL_SECS).contains(&age)) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn durable_batch_audit_exists(home: &Path, token: &str, id: &str) -> bool {
    (0..=5).any(|generation| {
        let path = if generation == 0 {
            home.join("event-log.jsonl")
        } else {
            home.join(format!("event-log.jsonl.{generation}"))
        };
        std::fs::read_to_string(path).is_ok_and(|raw| {
            raw.lines().any(|line| {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                    return false;
                };
                if event["kind"] != "decision_batch_archived" {
                    return false;
                }
                let Some(detail) = event["detail"].as_str() else {
                    return false;
                };
                serde_json::from_str::<serde_json::Value>(detail).is_ok_and(|detail| {
                    detail["id"].as_str() == Some(id) && detail["token"].as_str() == Some(token)
                })
            })
        })
    })
}

fn batch_audit_detail(
    candidate: &BatchCandidateSnapshot,
    token: &str,
    confirmation: &BatchConfirmation,
    audit_reason: &str,
) -> String {
    serde_json::json!({
        "id": candidate.id,
        "token": token,
        "preview_digest": confirmation.preview_digest,
        "source_digest": confirmation.source_digest,
        "policy_digest": confirmation.policy_digest,
        "audit_reason": audit_reason,
    })
    .to_string()
}

fn write_batch_audit(
    home: &Path,
    caller: &str,
    candidate: &BatchCandidateSnapshot,
    token: &str,
    confirmation: &BatchConfirmation,
    audit_reason: &str,
) -> anyhow::Result<()> {
    crate::event_log::try_log(
        home,
        "decision_batch_archived",
        caller,
        &batch_audit_detail(candidate, token, confirmation, audit_reason),
    )
}

fn read_stable_regular_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let before = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "non-regular file '{}'",
        path.display()
    );
    let raw = std::fs::read(path)?;
    let after = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        after.file_type().is_file(),
        "non-regular file '{}'",
        path.display()
    );
    anyhow::ensure!(
        same_file_identity(&before, &after),
        "file identity changed for '{}'",
        path.display()
    );
    Ok(raw)
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

fn selected_for_batch(
    decision: &Decision,
    args: &Value,
    since: Option<chrono::DateTime<chrono::FixedOffset>>,
    until: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    let tags = args["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();
    if !tags.is_empty()
        && !tags
            .iter()
            .any(|tag| decision.tags.iter().any(|actual| actual == tag))
    {
        return false;
    }
    if args["author"]
        .as_str()
        .is_some_and(|author| decision.author != author)
    {
        return false;
    }
    let actual_status = decision.status.map(|value| match value {
        DecisionStatus::Pending => "pending",
        DecisionStatus::Answered => "answered",
        DecisionStatus::Expired => "expired",
    });
    if args["status"]
        .as_str()
        .is_some_and(|wanted| actual_status != Some(wanted))
    {
        return false;
    }
    chrono::DateTime::parse_from_rfc3339(&decision.created_at)
        .is_ok_and(|created| since.is_none_or(|lower| created >= lower) && created <= until)
}

pub fn archive_batch(home: &Path, caller: &str, args: &Value) -> Value {
    if caller.trim().is_empty() {
        return serde_json::json!({"error": "archive_batch requires an authenticated caller"});
    }
    let apply = args["apply"].as_bool().unwrap_or(false);
    let audit_reason = args["audit_reason"].as_str().unwrap_or("").trim();
    if audit_reason.is_empty() {
        return serde_json::json!({"error": "archive_batch requires non-empty 'audit_reason'"});
    }
    if apply {
        return archive_batch_apply(home, caller, args, audit_reason);
    }
    archive_batch_preview(home, caller, args, audit_reason)
}

fn archive_batch_preview(home: &Path, caller: &str, args: &Value, audit_reason: &str) -> Value {
    reap_expired_confirmations(home);
    if let Some(status) = args["status"].as_str() {
        if !matches!(status, "pending" | "answered" | "expired") {
            return serde_json::json!({"error": format!("invalid status filter '{status}'")});
        }
    }
    let until = match parse_bound(args, "until") {
        Ok(Some(value)) => value,
        Ok(None) => return serde_json::json!({"error": "archive_batch dry-run requires 'until'"}),
        Err(error) => return error,
    };
    let since = match parse_bound(args, "since") {
        Ok(value) => value,
        Err(error) => return error,
    };
    let (protected, policy_digest) = match protected_policy(home) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({
                "error": format!("protected decision policy is unreadable; refusing batch archive: {error}")
            })
        }
    };
    let scan_budget = args["scan_budget"]
        .as_u64()
        .unwrap_or(DEFAULT_SCAN_BUDGET as u64)
        .clamp(1, MAX_SCAN_BUDGET as u64) as usize;
    let paths = match sorted_json_paths(&decisions_dir(home)) {
        Ok(paths) => paths,
        Err(error) => {
            return serde_json::json!({"error": format!("decision source is unreadable; refusing batch archive: {error}")})
        }
    };
    let mut scanned = 0usize;
    let mut source_material = Vec::new();
    let mut sources = Vec::new();
    let mut candidates = Vec::new();
    let mut protected_ids = Vec::new();
    let mut candidates_capped = false;
    for path in paths.iter().take(scan_budget) {
        scanned += 1;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) => {
                return serde_json::json!({"error": format!("cannot read {filename}; refusing batch archive: {error}")})
            }
        };
        let content_digest = crate::daemon::utils::sha256_hex(&raw);
        source_material.extend_from_slice(filename.as_bytes());
        source_material.push(0);
        source_material.extend_from_slice(content_digest.as_bytes());
        source_material.push(0);
        sources.push(BatchSourceSnapshot {
            physical_filename: filename.clone(),
            content_digest: content_digest.clone(),
        });
        let decision: Decision = match serde_json::from_slice(&raw) {
            Ok(decision) => decision,
            Err(error) => {
                return serde_json::json!({"error": format!("malformed {filename}; refusing batch archive: {error}")})
            }
        };
        if decision.schema_version > SCHEMA_VERSION {
            return serde_json::json!({
                "error": format!("{filename} uses newer schema {}; refusing batch archive", decision.schema_version)
            });
        }
        if decision.archived || !selected_for_batch(&decision, args, since, until) {
            continue;
        }
        if archive_would_orphan_question(&decision) {
            return serde_json::json!({
                "error": format!("unresolved question '{}' matched; refusing batch archive", decision.id)
            });
        }
        if !can_mutate_decision(home, caller, &decision) {
            return serde_json::json!({
                "error": format!("decision '{}' owned by '{}'; caller '{caller}' not authorized", decision.id, decision.author)
            });
        }
        if decision.tags.iter().any(|tag| protected.contains(tag)) {
            protected_ids.push(decision.id);
            continue;
        }
        if candidates.len() == MAX_BATCH_CANDIDATES {
            candidates_capped = true;
            break;
        }
        candidates.push(BatchCandidateSnapshot {
            id: decision.id,
            physical_filename: filename,
            content_digest,
        });
    }
    let ids = candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect::<Vec<_>>();
    let preview_digest = crate::daemon::utils::sha256_hex(ids.join("\0").as_bytes());
    let source_digest = crate::daemon::utils::sha256_hex(&source_material);
    let token = uuid::Uuid::new_v4().to_string();
    let confirmation = BatchConfirmation {
        schema_version: 1,
        actor: caller.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        audit_reason: audit_reason.to_string(),
        policy_digest: policy_digest.clone(),
        source_digest: source_digest.clone(),
        preview_digest: preview_digest.clone(),
        candidate_cap: MAX_BATCH_CANDIDATES,
        candidates_capped,
        sources,
        candidates,
    };
    let dir = confirmation_dir(home);
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|_| {
        crate::store::save_atomic(&dir.join(format!("{token}.json")), &confirmation)
            .map_err(std::io::Error::other)
    }) {
        return serde_json::json!({"error": format!("persist confirmation failed: {error}")});
    }
    serde_json::json!({
        "apply": false,
        "candidate_ids": confirmation.candidates.iter().map(|candidate| &candidate.id).collect::<Vec<_>>(),
        "candidate_count": confirmation.candidates.len(),
        "candidate_cap": confirmation.candidate_cap,
        "candidates_capped": confirmation.candidates_capped,
        "protected_ids": protected_ids,
        "scanned": scanned,
        "scan_budget": scan_budget,
        "scan_exhausted": scanned == scan_budget && scanned < paths.len(),
        "confirm_token": token,
        "preview_digest": preview_digest,
        "policy_digest": policy_digest,
        "source_digest": source_digest,
    })
}

fn archive_batch_apply(home: &Path, caller: &str, args: &Value, audit_reason: &str) -> Value {
    let Some(token) = args["confirm_token"].as_str() else {
        return serde_json::json!({"error": "apply=true requires 'confirm_token'"});
    };
    let Some(path) = confirmation_path(home, token) else {
        return serde_json::json!({"error": "invalid confirm_token"});
    };
    let mut confirmation: BatchConfirmation = match std::fs::read(&path)
        .map_err(anyhow::Error::from)
        .and_then(|raw| serde_json::from_slice(&raw).map_err(anyhow::Error::from))
    {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({"error": format!("confirmation unavailable: {error}")})
        }
    };
    if confirmation.schema_version != 1
        || confirmation.actor != caller
        || confirmation.audit_reason != audit_reason
    {
        return serde_json::json!({"error": "confirmation actor/audit binding mismatch"});
    }
    let confirmation_age = chrono::DateTime::parse_from_rfc3339(&confirmation.created_at)
        .map(|created| {
            chrono::Utc::now()
                .signed_duration_since(created)
                .num_seconds()
        })
        .unwrap_or(BATCH_CONFIRM_TTL_SECS + 1);
    if !(0..=BATCH_CONFIRM_TTL_SECS).contains(&confirmation_age) {
        return serde_json::json!({"error": "confirmation expired; run dry-run again"});
    }
    let mut confirm_ids = args["confirm_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(String::from))
        .collect::<Vec<_>>();
    confirm_ids.sort();
    confirm_ids.dedup();
    let mut expected_ids = confirmation
        .candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    expected_ids.sort();
    if confirm_ids != expected_ids {
        return serde_json::json!({"error": "confirm_ids must exactly match the dry-run preview"});
    }
    let (protected, policy_digest) = match protected_policy(home) {
        Ok(value) => value,
        Err(error) => {
            return serde_json::json!({"error": format!("protected policy revalidation failed: {error}")})
        }
    };
    if policy_digest != confirmation.policy_digest {
        return serde_json::json!({"error": "protected decision policy changed; run dry-run again"});
    }
    let dir = decisions_dir(home);
    let archive = dir.join(".archive");
    let all_already_archived = confirmation.candidates.iter().all(|candidate| {
        read_stable_regular_file(&archive.join(&candidate.physical_filename))
            .ok()
            .is_some_and(|raw| crate::daemon::utils::sha256_hex(&raw) == candidate.content_digest)
            && std::fs::symlink_metadata(dir.join(&candidate.physical_filename))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    });
    if !all_already_archived {
        let current_sources = match batch_sources_with_archived_candidates(
            &dir,
            &archive,
            &confirmation,
        ) {
            Ok(sources) => sources,
            Err(error) => {
                return serde_json::json!({"error": format!("decision source revalidation failed: {error}")})
            }
        };
        if current_sources != confirmation.sources {
            return serde_json::json!({"error": "decision source snapshot changed; run dry-run again"});
        }
    }
    if let Err(error) = std::fs::create_dir_all(&archive) {
        return serde_json::json!({"error": format!("create archive directory failed: {error}")});
    }
    let _sentinel = match crate::store::acquire_file_lock(&dir.join(".archive.lock")) {
        Ok(lock) => lock,
        Err(error) => {
            return serde_json::json!({"error": format!("archive sentinel lock failed: {error}")})
        }
    };
    let mut ordered = std::mem::take(&mut confirmation.candidates);
    ordered.sort_by(|left, right| left.id.cmp(&right.id));
    let mut outcomes = Vec::new();
    let mut partial = false;
    for candidate in ordered {
        let outcome = with_decision_lock(home, &candidate.id, || {
            let src = dir.join(&candidate.physical_filename);
            let dst = archive.join(&candidate.physical_filename);
            let src_metadata = match std::fs::symlink_metadata(&src) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if read_stable_regular_file(&dst).ok().is_some_and(|raw| {
                        crate::daemon::utils::sha256_hex(&raw) == candidate.content_digest
                    }) {
                        if durable_batch_audit_exists(home, token, &candidate.id) {
                            return serde_json::json!({"id": candidate.id, "outcome": "already_archived", "audit_durable": true});
                        }
                        return match write_batch_audit(
                            home,
                            caller,
                            &candidate,
                            token,
                            &confirmation,
                            audit_reason,
                        ) {
                            Ok(()) => {
                                serde_json::json!({"id": candidate.id, "outcome": "audit_repaired", "audit_durable": true})
                            }
                            Err(error) => {
                                serde_json::json!({"id": candidate.id, "outcome": "archived_audit_failed", "error": error.to_string(), "audit_durable": false})
                            }
                        };
                    }
                    return serde_json::json!({"id": candidate.id, "outcome": "source_missing", "audit_durable": false});
                }
                Err(error) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "source_metadata_failed", "error": error.to_string(), "audit_durable": false})
                }
            };
            if !src_metadata.file_type().is_file() {
                return serde_json::json!({"id": candidate.id, "outcome": "revalidation_refused", "error": "source is not a regular file", "audit_durable": false});
            }
            match std::fs::symlink_metadata(&dst) {
                Ok(_) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "archive_collision", "audit_durable": false});
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "archive_metadata_failed", "error": error.to_string(), "audit_durable": false})
                }
            }
            let raw = match read_stable_regular_file(&src) {
                Ok(raw) => raw,
                Err(error) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "read_failed", "error": error.to_string(), "audit_durable": false})
                }
            };
            if crate::daemon::utils::sha256_hex(&raw) != candidate.content_digest {
                return serde_json::json!({"id": candidate.id, "outcome": "content_changed", "audit_durable": false});
            }
            let decision: Decision = match serde_json::from_slice(&raw) {
                Ok(decision) => decision,
                Err(error) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "malformed", "error": error.to_string(), "audit_durable": false})
                }
            };
            if decision.schema_version > SCHEMA_VERSION
                || archive_would_orphan_question(&decision)
                || decision.tags.iter().any(|tag| protected.contains(tag))
                || !can_mutate_decision(home, caller, &decision)
            {
                return serde_json::json!({"id": candidate.id, "outcome": "revalidation_refused", "audit_durable": false});
            }
            let final_metadata = match std::fs::symlink_metadata(&src) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return serde_json::json!({"id": candidate.id, "outcome": "source_metadata_failed", "error": error.to_string(), "audit_durable": false})
                }
            };
            if !final_metadata.file_type().is_file()
                || !same_file_identity(&src_metadata, &final_metadata)
            {
                return serde_json::json!({"id": candidate.id, "outcome": "revalidation_refused", "error": "source identity changed", "audit_durable": false});
            }
            if let Err(error) = std::fs::rename(&src, &dst) {
                return serde_json::json!({"id": candidate.id, "outcome": "archive_failed", "error": error.to_string(), "audit_durable": false});
            }
            match write_batch_audit(home, caller, &candidate, token, &confirmation, audit_reason) {
                Ok(()) => {
                    serde_json::json!({"id": candidate.id, "outcome": "archived", "audit_durable": true})
                }
                Err(error) => {
                    serde_json::json!({"id": candidate.id, "outcome": "archived_audit_failed", "error": error.to_string(), "audit_durable": false})
                }
            }
        });
        let value = match outcome {
            Ok(value) => value,
            Err(error) => {
                serde_json::json!({"id": candidate.id, "outcome": "lock_failed", "error": error.to_string(), "audit_durable": false})
            }
        };
        partial |= !value["audit_durable"].as_bool().unwrap_or(false);
        outcomes.push(value);
    }
    serde_json::json!({
        "apply": true,
        "partial": partial,
        "outcomes": outcomes,
        "preview_digest": confirmation.preview_digest,
        "candidate_cap": confirmation.candidate_cap,
        "candidates_capped": confirmation.candidates_capped,
        "source_digest": confirmation.source_digest,
        "policy_digest": confirmation.policy_digest,
    })
}

pub fn update(home: &Path, caller: &str, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    let args = args.clone();

    // Read+mutate+write must all happen under the same per-decision flock
    // so concurrent updates don't lose field changes. The previous code
    // load_all'd every decision on disk and clobbered whatever version
    // was there at save time.
    let locked = with_decision_lock(home, &id, || -> Value {
        let path = decision_path(home, &id);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return serde_json::json!({"error": format!("decision '{id}' not found")}),
        };
        let mut decision: Decision = match serde_json::from_str(&content) {
            Ok(d) => d,
            Err(e) => {
                return serde_json::json!({"error": format!("decision '{id}' corrupted: {e}")})
            }
        };
        // #1990: refuse to mutate a record a newer daemon wrote — re-saving it
        // here would downgrade it / drop fields we don't understand.
        if decision.schema_version > SCHEMA_VERSION {
            return serde_json::json!({
                "error": format!(
                    "decision '{id}' was written by a newer schema version ({} > {SCHEMA_VERSION}); update with a newer daemon",
                    decision.schema_version
                )
            });
        }

        // Cascade auth gate (Sprint 21 Phase 2 D1) — reject non-author
        // callers so prompt-injected agents cannot silently archive operator
        // strategic decisions. Mirrors `tasks::can_mutate_task` ownership rule.
        if !can_mutate_decision(home, caller, &decision) {
            return serde_json::json!({
                "error": format!(
                    "decision '{id}' owned by '{}', caller '{caller}' not authorized",
                    decision.author
                )
            });
        }

        if args.get("review_class").is_some() {
            return serde_json::json!({
                "error": "decision review_class is immutable after creation",
                "code": "decision_review_class_immutable",
            });
        }

        // #2037 (3): same content|text alias as `post` — the schema declares
        // `text` tool-wide, so update honoring only `content` was a silent lie.
        if let Some(content) = args["content"].as_str().or_else(|| args["text"].as_str()) {
            decision.content = content.to_string();
        }
        if let Some(tags) = args["tags"].as_array() {
            decision.tags = tags
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if let Some(ttl) = args["ttl_days"].as_u64() {
            decision.ttl_days = Some(ttl);
        }
        if args["archive"].as_bool() == Some(true) {
            decision.archived = true;
        }
        decision.updated_at = chrono::Utc::now().to_rfc3339();
        decision.schema_version = SCHEMA_VERSION;

        // Inline write — save() would try to re-acquire this same lock.
        match crate::store::save_atomic(&path, &decision) {
            Ok(()) => serde_json::json!({"id": id, "status": "updated"}),
            Err(e) => serde_json::json!({"error": format!("{e}")}),
        }
    });

    match locked {
        Ok(v) => v,
        Err(e) => serde_json::json!({"error": format!("lock acquisition failed: {e}")}),
    }
}

/// #2305: record an operator's answer to a pending decision.
///
/// Unlike [`update`], this is intentionally NOT gated by [`can_mutate_decision`]:
/// the *author* posts the question, but the *operator* (a different identity)
/// answers it — an author-only gate would reject the very caller we expect. The
/// answerer is recorded in `answered_by` (the TUI passes `"operator"`; an agent
/// recording on the operator's behalf is attributed by its own name, visible to
/// the author). Read→validate→write happens under the same per-decision flock as
/// `update`, so a concurrent second answer sees `Answered` (not `Pending`) and is
/// refused — exactly one answer wins.
pub fn answer(home: &Path, caller: &str, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    let ans = match args["answer"].as_str() {
        Some(a) => a.to_string(),
        None => return serde_json::json!({"error": "missing 'answer'"}),
    };

    let locked = with_decision_lock(home, &id, || -> Value {
        let path = decision_path(home, &id);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return serde_json::json!({"error": format!("decision '{id}' not found")}),
        };
        let mut decision: Decision = match serde_json::from_str(&content) {
            Ok(d) => d,
            Err(e) => {
                return serde_json::json!({"error": format!("decision '{id}' corrupted: {e}")})
            }
        };
        // #1990: refuse to touch a record a newer daemon wrote.
        if decision.schema_version > SCHEMA_VERSION {
            return serde_json::json!({
                "error": format!(
                    "decision '{id}' was written by a newer schema version ({} > {SCHEMA_VERSION})",
                    decision.schema_version
                )
            });
        }
        if !decision.needs_answer {
            return serde_json::json!({
                "error": format!("decision '{id}' is not a pending question (needs_answer=false)")
            });
        }
        // #2305 (r2): a question is for the OPERATOR to answer — its own author
        // must not self-answer (an agent answering its own question would bypass
        // the operator entirely). The TUI overlay answers as "operator" (never the
        // author), so this only blocks the MCP self-answer path.
        if decision.author == caller {
            return serde_json::json!({
                "error": format!(
                    "decision '{id}' author '{caller}' cannot answer its own question (operator answers)"
                )
            });
        }
        if decision.status != Some(DecisionStatus::Pending) {
            return serde_json::json!({
                "error": format!(
                    "decision '{id}' is not Pending (already answered or expired); cannot answer"
                )
            });
        }
        // When the poster constrained the answer to options (no free text), the
        // answer must match one of the option labels exactly.
        if !decision.allow_free_text
            && !decision.options.is_empty()
            && !decision.options.iter().any(|o| o.label == ans)
        {
            return serde_json::json!({
                "error": format!(
                    "answer for '{id}' must be one of the offered options (free text not allowed)"
                )
            });
        }

        let now = chrono::Utc::now().to_rfc3339();
        decision.answer = Some(ans.clone());
        decision.answered_by = Some(caller.to_string());
        decision.answered_at = Some(now.clone());
        decision.status = Some(DecisionStatus::Answered);
        decision.updated_at = now;
        decision.schema_version = SCHEMA_VERSION;

        // Inline write — save() would re-acquire this same (non-reentrant) flock.
        match crate::store::save_atomic(&path, &decision) {
            Ok(()) => serde_json::json!({
                "id": id,
                "status": "answered",
                "author": decision.author,
                "title": decision.title,
                "answer": ans,
            }),
            Err(e) => serde_json::json!({"error": format!("{e}")}),
        }
    });

    match locked {
        Ok(v) => v,
        Err(e) => serde_json::json!({"error": format!("lock acquisition failed: {e}")}),
    }
}

/// #2524 P2c / #2313: called by `daemon::decision_board_timeout`'s tracker
/// once a pending question's `timeout_secs` has elapsed. Idempotent —
/// returns `None` if the decision was already answered/expired/removed
/// under the lock (e.g. the operator answered it in the race window),
/// mirroring `answer`'s own re-check-under-lock shape. Returns
/// `(author, title)` on success, for the caller's notification text.
pub(crate) fn auto_answer_timeout(home: &Path, id: &str) -> Option<(String, String)> {
    let locked = with_decision_lock(home, id, || -> Option<(String, String)> {
        let path = decision_path(home, id);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return None;
        };
        let mut decision: Decision = serde_json::from_str(&content).ok()?;
        if decision.status != Some(DecisionStatus::Pending) {
            return None;
        }
        let default_label = decision.timeout_default.clone()?;
        let now = chrono::Utc::now().to_rfc3339();
        decision.answer = Some(default_label);
        decision.answered_by = Some("timeout-default".to_string());
        decision.answered_at = Some(now.clone());
        decision.status = Some(DecisionStatus::Answered);
        decision.updated_at = now;
        decision.schema_version = SCHEMA_VERSION;
        if crate::store::save_atomic(&path, &decision).is_ok() {
            Some((decision.author.clone(), decision.title.clone()))
        } else {
            None
        }
    });
    locked.ok().flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "decisions_tests.rs"]
mod tests;
