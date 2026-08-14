//! Decision storage — CRUD over JSON files in {home}/decisions/.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

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

    let all = load_all(home);
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|d| include_archived || !d.archived)
        .filter(|d| filter_tags.is_empty() || filter_tags.iter().any(|t| d.tags.contains(t)))
        .collect();

    serde_json::json!({"decisions": filtered})
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
