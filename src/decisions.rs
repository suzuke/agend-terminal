//! Decision storage — CRUD over JSON files in {home}/decisions/.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

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
}

fn decisions_dir(home: &Path) -> std::path::PathBuf {
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
                        decisions.push(d);
                    }
                }
            }
        }
    }
    decisions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    decisions
}

fn decision_path(home: &Path, id: &str) -> std::path::PathBuf {
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
fn with_decision_lock<R>(home: &Path, id: &str, f: impl FnOnce() -> R) -> anyhow::Result<R> {
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
    let content = match args["content"].as_str() {
        Some(c) => c,
        None => return serde_json::json!({"error": "missing 'content'"}),
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

    let now = chrono::Utc::now().to_rfc3339();
    // The historical id format was seconds-precision only — two posts in the
    // same UTC second collided and the second silently overwrote the first.
    // Append nanoseconds + a process-local counter so no two posts from the
    // same process can share an id, even when issued back-to-back.
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
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
            old.archived = true;
            old.updated_at = now_c;
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
    };

    match save(home, &decision) {
        Ok(()) => serde_json::json!({"id": id, "status": "posted"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
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

        if let Some(content) = args["content"].as_str() {
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
}
