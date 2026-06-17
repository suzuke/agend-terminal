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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-decisions-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(dir.join("decisions")).ok();
        dir
    }

    #[test]
    fn test_post_and_list() {
        let home = tmp_home("post_and_list");
        let result = post(
            &home,
            "test-agent",
            &serde_json::json!({
                "title": "Test Decision", "content": "We use Rust", "scope": "fleet"
            }),
        );
        assert!(result["id"].as_str().is_some());
        assert_eq!(result["status"], "posted");

        let listed = list(&home, &serde_json::json!({}));
        let decisions = listed["decisions"].as_array().expect("array");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["title"], "Test Decision");
        assert_eq!(decisions[0]["author"], "test-agent");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_and_archive() {
        let home = tmp_home("update_archive");
        let result = post(
            &home,
            "a",
            &serde_json::json!({"title": "D1", "content": "v1"}),
        );
        let id = result["id"].as_str().expect("id");

        let upd = update(&home, "a", &serde_json::json!({"id": id, "content": "v2"}));
        assert_eq!(upd["status"], "updated");

        let listed = list(&home, &serde_json::json!({}));
        assert_eq!(listed["decisions"][0]["content"], "v2");

        // Archive
        update(&home, "a", &serde_json::json!({"id": id, "archive": true}));
        let listed = list(&home, &serde_json::json!({}));
        assert!(listed["decisions"].as_array().expect("arr").is_empty());

        // Include archived
        let listed = list(&home, &serde_json::json!({"include_archived": true}));
        assert_eq!(listed["decisions"].as_array().expect("arr").len(), 1);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_nonexistent() {
        let home = tmp_home("update_nonexistent");
        let result = update(&home, "anyone", &serde_json::json!({"id": "no-such-id"}));
        assert!(result["error"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_supersede_archives_old() {
        let home = tmp_home("supersede");
        let old = post(
            &home,
            "a",
            &serde_json::json!({"title": "old", "content": "v1"}),
        );
        let old_id = old["id"].as_str().expect("id").to_string();
        // New decision supersedes the old one.
        let new = post(
            &home,
            "a",
            &serde_json::json!({"title": "new", "content": "v2", "supersedes": old_id}),
        );
        assert_eq!(new["status"], "posted");

        // Old must now be archived.
        let listed = list(&home, &serde_json::json!({"include_archived": true}));
        let arr = listed["decisions"].as_array().expect("arr");
        let old_rec = arr
            .iter()
            .find(|d| d["id"].as_str() == Some(&old_id))
            .expect("old decision present");
        assert_eq!(old_rec["archived"], true);

        // Default list (non-archived) excludes it.
        let active = list(&home, &serde_json::json!({}));
        let active_ids: Vec<_> = active["decisions"]
            .as_array()
            .expect("arr")
            .iter()
            .map(|d| d["id"].as_str().unwrap_or(""))
            .collect();
        assert!(!active_ids.contains(&old_id.as_str()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_concurrent_updates_no_loss() {
        // Load-modify-save without a lock would let the two updates race:
        // both read the same starting record, each flips a different
        // field, whichever writes last silently drops the other's change.
        // Per-decision flock must serialize them so both writes land.
        let home = tmp_home("concurrent");
        let posted = post(
            &home,
            "a",
            &serde_json::json!({"title": "T", "content": "c0", "tags": []}),
        );
        let id = posted["id"].as_str().expect("id").to_string();

        let home_arc = std::sync::Arc::new(home.clone());
        let id_arc = std::sync::Arc::new(id.clone());

        let h1 = {
            let h = home_arc.clone();
            let i = id_arc.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    update(
                        &h,
                        "a",
                        &serde_json::json!({"id": (*i).clone(), "content": "from_thread_1"}),
                    );
                }
            })
        };
        let h2 = {
            let h = home_arc.clone();
            let i = id_arc.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    update(
                        &h,
                        "a",
                        &serde_json::json!({"id": (*i).clone(), "tags": ["from_thread_2"]}),
                    );
                }
            })
        };
        h1.join().expect("t1");
        h2.join().expect("t2");

        // Final state: last-writer-wins on each field is expected, but the
        // *file* must be valid JSON (no interleaved bytes) and must still
        // deserialize as a Decision. Without the lock, atomic_write guards
        // the write but load_all-based update would re-serialize the
        // *entire list*, losing fields written between load and save.
        let listed = list(&home, &serde_json::json!({"include_archived": true}));
        let decisions = listed["decisions"].as_array().expect("arr");
        assert_eq!(decisions.len(), 1, "decision must still exist intact");
        let d = &decisions[0];
        // Final state: updated_at must be populated (both threads always
        // update it), tags/content are whichever thread wrote last.
        assert!(d["updated_at"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    // ─── Sprint 21 Phase 2 D1: cascade auth gate (can_mutate_decision) ───
    //
    // Closes the cascade attack chain headline (Sprint 20 Track D MCP audit
    // C1 + Sprint 20.5 Track 6 cross-validation): without this gate a
    // prompt-injected agent could silently archive operator strategic
    // decisions. Mirror the `tasks::can_mutate_task` ownership pattern
    // (Sprint 20 Track D Praise replicate identification).

    fn make_test_decision(author: &str) -> Decision {
        Decision {
            id: "d-test".into(),
            title: "T".into(),
            content: "c".into(),
            scope: "fleet".into(),
            author: author.into(),
            tags: vec![],
            ttl_days: None,
            created_at: "2026-04-27T00:00:00Z".into(),
            updated_at: "2026-04-27T00:00:00Z".into(),
            archived: false,
            supersedes: None,
            working_directory: None,
            schema_version: SCHEMA_VERSION,
            needs_answer: false,
            status: None,
            options: vec![],
            allow_free_text: false,
            answer: None,
            answered_by: None,
            answered_at: None,
        }
    }

    #[test]
    fn can_mutate_decision_owner_pass() {
        let home = tmp_home("can_mutate_owner");
        let decision = make_test_decision("dev-lead");
        assert!(can_mutate_decision(&home, "dev-lead", &decision));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_decision_non_owner_reject() {
        let home = tmp_home("can_mutate_reject");
        let decision = make_test_decision("dev-lead");
        // No teams configured → no orchestrator override path → caller
        // mismatch must reject.
        assert!(!can_mutate_decision(&home, "dev-impl-1", &decision));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_decision_string_compare_no_numeric_coerce() {
        // Operator-known-pitfall (Telegram alert): caller string vs numeric
        // user_id. Decision.author is `String` (e.g. "dev-impl-1"); the
        // gate compares strings, never parses an int. Verify that an
        // alphabetically-similar but non-equal caller does NOT pass, and
        // that numeric-suffixed names compare verbatim.
        let home = tmp_home("can_mutate_string_compare");
        let decision = make_test_decision("dev-impl-1");
        // Exact string match — passes.
        assert!(can_mutate_decision(&home, "dev-impl-1", &decision));
        // Suffix mismatch — rejects (no int coerce to "1 == 1").
        assert!(!can_mutate_decision(&home, "dev-impl-2", &decision));
        // Bare numeric caller — rejects (would only "match" under int coerce
        // path, which we explicitly do not have).
        assert!(!can_mutate_decision(&home, "1", &decision));
        // Substring of author — rejects (no prefix-match path).
        assert!(!can_mutate_decision(&home, "dev-impl", &decision));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_decision_non_owner_returns_authz_error() {
        let home = tmp_home("update_non_owner");
        let posted = post(
            &home,
            "dev-lead",
            &serde_json::json!({"title": "Strategic", "content": "c"}),
        );
        let id = posted["id"].as_str().expect("id");

        // dev-impl-1 (not the author, no orchestrator override) attempts to
        // archive — must be rejected with descriptive error.
        let result = update(
            &home,
            "dev-impl-1",
            &serde_json::json!({"id": id, "archive": true}),
        );
        let err = result["error"].as_str().expect("error string");
        assert!(
            err.contains("not authorized"),
            "expected authz rejection, got: {err}"
        );
        assert!(
            err.contains("dev-lead"),
            "error must surface decision.author for diagnostics, got: {err}"
        );
        assert!(
            err.contains("dev-impl-1"),
            "error must surface caller for diagnostics, got: {err}"
        );

        // Verify the decision was NOT mutated despite the attempt.
        let listed = list(&home, &serde_json::json!({}));
        let arr = listed["decisions"].as_array().expect("arr");
        assert_eq!(arr.len(), 1, "decision still active (not archived)");
        assert_eq!(arr[0]["archived"], false);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_decision_owner_succeeds() {
        let home = tmp_home("update_owner");
        let posted = post(
            &home,
            "dev-lead",
            &serde_json::json!({"title": "T", "content": "v1"}),
        );
        let id = posted["id"].as_str().expect("id");

        let result = update(
            &home,
            "dev-lead",
            &serde_json::json!({"id": id, "content": "v2"}),
        );
        assert_eq!(result["status"], "updated");

        let listed = list(&home, &serde_json::json!({}));
        assert_eq!(listed["decisions"][0]["content"], "v2");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1990 additive: a pre-#1990 decision file (no `schema_version`) must still
    /// load (the field defaults to 0 ≤ current).
    #[test]
    fn old_decision_without_schema_version_is_listed() {
        let home = tmp_home("dec_oldver");
        let dir = decisions_dir(&home);
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("d-old.json"),
            r#"{"id":"d-old","title":"T","content":"c","scope":"fleet","author":"a","tags":[],"ttl_days":null,"created_at":"2026-04-27T00:00:00Z","updated_at":"2026-04-27T00:00:00Z","archived":false,"supersedes":null,"working_directory":null}"#,
        )
        .expect("write old fixture");
        assert!(
            list_all(&home).iter().any(|d| d.id == "d-old"),
            "a pre-#1990 decision (no schema_version) must still load"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1990 fail-closed: a decision a newer daemon wrote (schema_version > current)
    /// is skipped on read and refused for update — never silently downgraded.
    #[test]
    fn future_schema_version_decision_skipped_and_update_refused() {
        let home = tmp_home("dec_futurever");
        let dir = decisions_dir(&home);
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("d-future.json"),
            r#"{"id":"d-future","title":"T","content":"c","scope":"fleet","author":"a","tags":[],"ttl_days":null,"created_at":"2026-04-27T00:00:00Z","updated_at":"2026-04-27T00:00:00Z","archived":false,"supersedes":null,"working_directory":null,"schema_version":999}"#,
        )
        .expect("write future fixture");
        assert!(
            list_all(&home).iter().all(|d| d.id != "d-future"),
            "a future-schema decision must be skipped, not listed"
        );
        let resp = update(
            &home,
            "a",
            &serde_json::json!({"id":"d-future","content":"x"}),
        );
        assert!(
            resp.get("error").is_some(),
            "updating a future-schema decision must be refused: {resp}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ─── #2305 async decision board: pending questions + answer ───

    fn post_question(home: &Path, args: serde_json::Value) -> String {
        let r = post(home, "lead", &args);
        r["id"].as_str().expect("question id").to_string()
    }

    /// Active pending questions (the PR2 overlay's prod helper lands next PR; here
    /// we filter inline so PR1 carries no unused prod code).
    fn pending_questions(home: &Path) -> Vec<Decision> {
        list_all(home)
            .into_iter()
            .filter(|d| d.needs_answer && d.status == Some(DecisionStatus::Pending))
            .collect()
    }

    #[test]
    fn pre_2305_decision_loads_as_plain_non_question() {
        // A pre-#2305 record (none of the new fields) must load with needs_answer
        // false / status None — i.e. behave exactly as a plain scope decision.
        let home = tmp_home("dec_pre2305");
        let dir = decisions_dir(&home);
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join("d-plain.json"),
            r#"{"id":"d-plain","title":"T","content":"c","scope":"fleet","author":"a","tags":[],"ttl_days":null,"created_at":"2026-04-27T00:00:00Z","updated_at":"2026-04-27T00:00:00Z","archived":false,"supersedes":null,"working_directory":null,"schema_version":1}"#,
        )
        .expect("write pre-2305 fixture");
        let all = list_all(&home);
        let d = all.iter().find(|d| d.id == "d-plain").expect("loads");
        assert!(!d.needs_answer, "pre-2305 record is not a question");
        assert_eq!(d.status, None);
        assert!(d.options.is_empty() && d.answer.is_none());
        assert!(
            pending_questions(&home).is_empty(),
            "a plain decision must not appear as pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn post_question_appears_pending_then_answered() {
        let home = tmp_home("dec_q_lifecycle");
        let id = post_question(
            &home,
            serde_json::json!({
                "title": "Deploy now?", "content": "ship v2?",
                "needs_answer": true,
                "options": [{"label": "yes", "recommended": true}, "no"],
            }),
        );
        // Pending until answered.
        let pending = pending_questions(&home);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].status, Some(DecisionStatus::Pending));
        assert!(
            pending[0].options[0].recommended,
            "recommended-first preserved"
        );
        assert!(
            !pending[0].options[1].recommended,
            "bare-string option = not recommended"
        );

        // Answer with a valid option.
        let r = answer(
            &home,
            "operator",
            &serde_json::json!({"id": id, "answer": "yes"}),
        );
        assert_eq!(r["status"], "answered");
        assert_eq!(r["author"], "lead", "answer surfaces author for notify");

        // No longer pending; fields recorded.
        assert!(pending_questions(&home).is_empty());
        let all = list_all(&home);
        let d = all.iter().find(|d| d.id == id).expect("present");
        assert_eq!(d.status, Some(DecisionStatus::Answered));
        assert_eq!(d.answer.as_deref(), Some("yes"));
        assert_eq!(d.answered_by.as_deref(), Some("operator"));
        assert!(d.answered_at.is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn answer_rejects_non_option_when_free_text_disallowed() {
        let home = tmp_home("dec_q_optonly");
        let id = post_question(
            &home,
            serde_json::json!({
                "title": "Q", "content": "?", "needs_answer": true,
                "options": ["a", "b"], "allow_free_text": false,
            }),
        );
        let bad = answer(
            &home,
            "operator",
            &serde_json::json!({"id": id, "answer": "zzz"}),
        );
        assert!(
            bad.get("error").is_some(),
            "off-option answer must be refused: {bad}"
        );
        // Still pending (not consumed by the rejected attempt).
        assert_eq!(pending_questions(&home).len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn answer_allows_free_text_when_enabled() {
        let home = tmp_home("dec_q_freetext");
        let id = post_question(
            &home,
            serde_json::json!({
                "title": "Q", "content": "?", "needs_answer": true,
                "options": ["a"], "allow_free_text": true,
            }),
        );
        let r = answer(
            &home,
            "operator",
            &serde_json::json!({"id": id, "answer": "something custom"}),
        );
        assert_eq!(r["status"], "answered");
        let d = list_all(&home)
            .into_iter()
            .find(|d| d.id == id)
            .expect("present");
        assert_eq!(d.answer.as_deref(), Some("something custom"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn answer_refuses_non_question_and_already_answered() {
        let home = tmp_home("dec_q_guards");
        // Plain decision (not a question) → refused.
        let plain = post(
            &home,
            "lead",
            &serde_json::json!({"title": "T", "content": "c"}),
        );
        let pid = plain["id"].as_str().expect("id");
        assert!(answer(
            &home,
            "operator",
            &serde_json::json!({"id": pid, "answer": "x"})
        )
        .get("error")
        .is_some());

        // Question → answer once OK, second answer refused (not Pending).
        let id = post_question(
            &home,
            serde_json::json!({"title": "Q", "content": "?", "needs_answer": true, "allow_free_text": true}),
        );
        assert_eq!(
            answer(
                &home,
                "operator",
                &serde_json::json!({"id": id, "answer": "first"})
            )["status"],
            "answered"
        );
        let second = answer(
            &home,
            "operator",
            &serde_json::json!({"id": id, "answer": "second"}),
        );
        assert!(
            second.get("error").is_some(),
            "re-answer must be refused: {second}"
        );
        // The first answer stands.
        let d = list_all(&home)
            .into_iter()
            .find(|d| d.id == id)
            .expect("present");
        assert_eq!(d.answer.as_deref(), Some("first"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn concurrent_answers_exactly_one_wins() {
        // Two threads answer the same pending question; the per-decision flock
        // serializes read→validate→write, so the second sees Answered (not
        // Pending) and is refused. Exactly one answer is recorded.
        let home = tmp_home("dec_q_concurrent");
        let id = post_question(
            &home,
            serde_json::json!({"title": "Q", "content": "?", "needs_answer": true, "allow_free_text": true}),
        );
        let home_arc = std::sync::Arc::new(home.clone());
        let id_arc = std::sync::Arc::new(id.clone());
        let mk = |ans: &'static str| {
            let h = home_arc.clone();
            let i = id_arc.clone();
            std::thread::spawn(move || {
                answer(
                    &h,
                    "operator",
                    &serde_json::json!({"id": (*i).clone(), "answer": ans}),
                )
            })
        };
        // Spawn BOTH, then join — they contend on the same per-decision flock.
        let (t1, t2) = (mk("A"), mk("B"));
        let r1 = t1.join().expect("t1");
        let r2 = t2.join().expect("t2");
        let successes = [&r1, &r2]
            .iter()
            .filter(|r| r["status"] == "answered")
            .count();
        assert_eq!(successes, 1, "exactly one answer must win: {r1} | {r2}");
        assert!(
            pending_questions(&home).is_empty(),
            "question is answered, not pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
