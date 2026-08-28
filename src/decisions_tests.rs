//! Decision-store tests, re-homed from `src/decisions.rs` so the production
//! module remains below the repository's anti-monolith ceiling.

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
fn review_class_is_persisted_and_create_only_3419() {
    let home = tmp_home("review_class_3419");
    let posted = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "governance",
            "content": "dual review",
            "review_class": "dual",
        }),
    );
    let id = posted["id"].as_str().expect("decision id").to_string();
    let listed = list(&home, &serde_json::json!({"include_archived": true}));
    assert_eq!(listed["decisions"][0]["review_class"], "dual");

    let update = update(
        &home,
        "lead",
        &serde_json::json!({"id": id, "review_class": "single"}),
    );
    assert_eq!(
        update["code"], "decision_review_class_immutable",
        "review_class must not be mutable after decision creation: {update}"
    );
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
        review_class: None,
        schema_version: SCHEMA_VERSION,
        needs_answer: false,
        status: None,
        options: vec![],
        allow_free_text: false,
        answer: None,
        answered_by: None,
        answered_at: None,
        timeout_secs: None,
        timeout_default: None,
    }
}

fn write_named_decision(home: &Path, filename: &str, decision: &Decision) {
    crate::store::save_atomic(&decisions_dir(home).join(filename), decision).expect("write");
}

#[test]
fn bounded_list_charges_malformed_records_and_resumes_by_physical_name_3227() {
    let home = tmp_home("bounded-list-3227");
    std::fs::write(decisions_dir(&home).join("999.json"), b"{broken").unwrap();
    let mut decision = make_test_decision("alice");
    decision.id = "imported-id-not-filename".into();
    write_named_decision(&home, "001.json", &decision);

    let first = list(&home, &serde_json::json!({"scan_budget": 1, "limit": 10}));
    assert_eq!(first["scanned"], 1);
    assert!(first["decisions"].as_array().unwrap().is_empty());
    assert_eq!(first["errors"].as_array().unwrap().len(), 1);
    let raw_cursor = first["next_cursor"].as_str().expect("cursor");
    let decoded = cursor_decode(raw_cursor).expect("opaque cursor decodes internally");
    assert_eq!(decoded.physical_filename, "999.json");
    assert_eq!(
        decoded.parsed_id, None,
        "malformed records still advance cursor"
    );

    let second = list(
        &home,
        &serde_json::json!({"scan_budget": 1, "cursor": raw_cursor}),
    );
    assert_eq!(second["scanned"], 1);
    assert_eq!(second["decisions"][0]["id"], "imported-id-not-filename");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn snapshot_cursor_rejects_source_membership_change_3227() {
    let home = tmp_home("snapshot-list-3227");
    let mut first = make_test_decision("alice");
    first.id = "one".into();
    write_named_decision(&home, "001.json", &first);
    let page = list(
        &home,
        &serde_json::json!({"limit": 1, "consistency": "snapshot"}),
    );
    let cursor = page["next_cursor"].as_str().unwrap_or("");
    // Force a next page, then mutate membership between pages.
    let mut second = make_test_decision("alice");
    second.id = "two".into();
    write_named_decision(&home, "002.json", &second);
    if cursor.is_empty() {
        // The first source had one row and therefore no cursor. Add a second
        // row, restart, and capture a snapshot cursor with remaining work.
        let page = list(
            &home,
            &serde_json::json!({"limit": 1, "consistency": "snapshot"}),
        );
        let cursor = page["next_cursor"].as_str().expect("snapshot cursor");
        let mut third = make_test_decision("alice");
        third.id = "three".into();
        write_named_decision(&home, "003.json", &third);
        let stale = list(
            &home,
            &serde_json::json!({"limit": 1, "consistency": "snapshot", "cursor": cursor}),
        );
        assert!(stale["error"]
            .as_str()
            .unwrap()
            .contains("snapshot changed"));
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn audit_history_is_a_distinct_source_view_3227() {
    let home = tmp_home("history-view-3227");
    let mut live = make_test_decision("alice");
    live.id = "live".into();
    write_named_decision(&home, "live.json", &live);
    let archive = decisions_dir(&home).join(".archive");
    std::fs::create_dir_all(&archive).unwrap();
    let mut historical = make_test_decision("alice");
    historical.id = "historical".into();
    crate::store::save_atomic(&archive.join("physical-name.json"), &historical).unwrap();

    let logical = list(&home, &serde_json::json!({"include_archived": true}));
    assert_eq!(logical["source"], "live");
    assert_eq!(logical["decisions"].as_array().unwrap().len(), 1);
    let history = list(&home, &serde_json::json!({"view": "audit_history"}));
    assert_eq!(history["source"], "audit_history");
    assert_eq!(history["decisions"][0]["id"], "historical");
    std::fs::remove_dir_all(&home).ok();
}

fn old_batch_decision(author: &str, id: &str) -> Decision {
    let mut decision = make_test_decision(author);
    decision.id = id.to_string();
    decision.created_at = "2020-01-01T00:00:00Z".into();
    decision.updated_at = decision.created_at.clone();
    decision
}

fn batch_preview(home: &Path, caller: &str) -> Value {
    archive_batch(
        home,
        caller,
        &serde_json::json!({
            "until": "2021-01-01T00:00:00Z",
            "audit_reason": "test cleanup"
        }),
    )
}

#[test]
fn archive_batch_token_is_actor_and_exact_preview_bound_3227() {
    let home = tmp_home("batch-binding-3227");
    let decision = old_batch_decision("alice", "d-one");
    write_named_decision(&home, "imported.json", &decision);
    let preview = batch_preview(&home, "alice");
    assert_eq!(preview["candidate_ids"], serde_json::json!(["d-one"]));
    let token = preview["confirm_token"].as_str().unwrap();
    let wrong_actor = archive_batch(
        &home,
        "mallory",
        &serde_json::json!({
            "apply": true, "confirm_token": token, "confirm_ids": ["d-one"],
            "audit_reason": "test cleanup"
        }),
    );
    assert!(wrong_actor["error"]
        .as_str()
        .unwrap()
        .contains("binding mismatch"));
    let wrong_ids = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "apply": true, "confirm_token": token, "confirm_ids": [],
            "audit_reason": "test cleanup"
        }),
    );
    assert!(wrong_ids["error"]
        .as_str()
        .unwrap()
        .contains("exactly match"));
    assert!(decisions_dir(&home).join("imported.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_revalidates_content_and_fails_closed_3227() {
    let home = tmp_home("batch-revalidate-3227");
    let decision = old_batch_decision("alice", "d-one");
    write_named_decision(&home, "imported.json", &decision);
    let preview = batch_preview(&home, "alice");
    let token = preview["confirm_token"].as_str().unwrap();
    let mut changed = decision.clone();
    changed.content = "changed after preview".into();
    write_named_decision(&home, "imported.json", &changed);
    let applied = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "apply": true, "confirm_token": token, "confirm_ids": ["d-one"],
            "audit_reason": "test cleanup"
        }),
    );
    assert!(applied["error"]
        .as_str()
        .unwrap()
        .contains("source snapshot changed"));
    assert!(decisions_dir(&home).join("imported.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_success_is_idempotent_and_audit_proven_3227() {
    let home = tmp_home("batch-success-3227");
    let decision = old_batch_decision("alice", "d-one");
    write_named_decision(&home, "imported.json", &decision);
    let preview = batch_preview(&home, "alice");
    let token = preview["confirm_token"].as_str().unwrap();
    let args = serde_json::json!({
        "apply": true, "confirm_token": token, "confirm_ids": ["d-one"],
        "audit_reason": "test cleanup"
    });
    let first = archive_batch(&home, "alice", &args);
    assert_eq!(first["partial"], false);
    assert_eq!(first["outcomes"][0]["outcome"], "archived");
    assert_eq!(first["outcomes"][0]["audit_durable"], true);
    let second = archive_batch(&home, "alice", &args);
    assert_eq!(second["partial"], false);
    assert_eq!(second["outcomes"][0]["outcome"], "already_archived");
    assert_eq!(second["outcomes"][0]["audit_durable"], true);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_partial_apply_can_resume_with_same_confirmation_3227() {
    let home = tmp_home("batch-partial-resume-3227");
    write_named_decision(&home, "001.json", &old_batch_decision("alice", "d-a"));
    write_named_decision(&home, "002.json", &old_batch_decision("alice", "d-b"));
    let preview = batch_preview(&home, "alice");
    let token = preview["confirm_token"].as_str().unwrap();
    let args = serde_json::json!({
        "apply": true, "confirm_token": token, "confirm_ids": ["d-a", "d-b"],
        "audit_reason": "test cleanup"
    });

    let archive = decisions_dir(&home).join(".archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::write(archive.join("002.json"), b"collision").unwrap();
    let first = archive_batch(&home, "alice", &args);
    assert_eq!(
        first["partial"], true,
        "first apply must be partial: {first}"
    );
    assert_eq!(first["outcomes"][0]["outcome"], "archived");
    assert_eq!(first["outcomes"][1]["outcome"], "archive_collision");

    std::fs::remove_file(archive.join("002.json")).unwrap();
    let retried = archive_batch(&home, "alice", &args);
    assert!(
        retried.get("error").is_none(),
        "retry must reconcile: {retried}"
    );
    assert_eq!(retried["partial"], false);
    assert_eq!(retried["outcomes"][0]["outcome"], "already_archived");
    assert_eq!(retried["outcomes"][1]["outcome"], "archived");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_audit_lookup_uses_exact_fields_3227() {
    let home = tmp_home("batch-audit-exact-3227");
    std::fs::write(
        home.join("event-log.jsonl"),
        serde_json::json!({
            "kind": "decision_batch_archived",
            "detail": serde_json::json!({
                "id": "d-one-extra",
                "token": "12345678-1234-1234-1234-123456789abc",
                "audit_reason": "cleanup id=d-one"
            }).to_string()
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    assert!(!durable_batch_audit_exists(
        &home,
        "12345678-1234-1234-1234-123456789abc",
        "d-one"
    ));
    std::fs::write(
        home.join("event-log.jsonl"),
        serde_json::json!({
            "kind": "unrelated_event",
            "detail": serde_json::json!({
                "id": "d-one",
                "token": "12345678-1234-1234-1234-123456789abc"
            }).to_string()
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    assert!(!durable_batch_audit_exists(
        &home,
        "12345678-1234-1234-1234-123456789abc",
        "d-one"
    ));
    std::fs::write(
        home.join("event-log.jsonl"),
        serde_json::json!({
            "kind": "decision_batch_archived",
            "detail": serde_json::json!({
                "id": "d-one",
                "token": "12345678-1234-1234-1234-123456789abc",
                "audit_reason": "cleanup id=d-other"
            }).to_string()
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    assert!(durable_batch_audit_exists(
        &home,
        "12345678-1234-1234-1234-123456789abc",
        "d-one"
    ));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_reason_cannot_forge_another_candidates_audit_3227() {
    let home = tmp_home("batch-audit-reason-injection-3227");
    write_named_decision(&home, "001.json", &old_batch_decision("alice", "d-a"));
    write_named_decision(&home, "002.json", &old_batch_decision("alice", "d-b"));
    let audit_reason = "cleanup id=d-b";
    let preview = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "until": "2021-01-01T00:00:00Z",
            "audit_reason": audit_reason
        }),
    );
    let token = preview["confirm_token"].as_str().unwrap();
    let archive = decisions_dir(&home).join(".archive");
    std::fs::create_dir_all(&archive).unwrap();
    std::fs::rename(
        decisions_dir(&home).join("002.json"),
        archive.join("002.json"),
    )
    .unwrap();

    let applied = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "apply": true,
            "confirm_token": token,
            "confirm_ids": ["d-a", "d-b"],
            "audit_reason": audit_reason
        }),
    );
    assert_eq!(
        applied["partial"], false,
        "apply must repair d-b: {applied}"
    );
    assert_eq!(applied["outcomes"][0]["outcome"], "archived");
    assert_eq!(applied["outcomes"][1]["outcome"], "audit_repaired");
    assert!(durable_batch_audit_exists(&home, token, "d-a"));
    assert!(durable_batch_audit_exists(&home, token, "d-b"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_preview_discloses_candidate_cap_3227() {
    let home = tmp_home("batch-candidate-cap-3227");
    for index in 0..=MAX_BATCH_CANDIDATES {
        write_named_decision(
            &home,
            &format!("{index:03}.json"),
            &old_batch_decision("alice", &format!("d-{index:03}")),
        );
    }
    let preview = batch_preview(&home, "alice");
    assert_eq!(preview["candidate_count"], MAX_BATCH_CANDIDATES);
    assert_eq!(preview["candidate_cap"], MAX_BATCH_CANDIDATES);
    assert_eq!(preview["candidates_capped"], true);
    let token = preview["confirm_token"].as_str().unwrap();
    let confirmation: BatchConfirmation =
        serde_json::from_slice(&std::fs::read(confirmation_path(&home, token).unwrap()).unwrap())
            .unwrap();
    assert!(confirmation.candidates_capped);
    assert_eq!(confirmation.candidate_cap, MAX_BATCH_CANDIDATES);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_preview_reaps_expired_confirmations_3227() {
    let home = tmp_home("batch-confirmation-reap-3227");
    write_named_decision(&home, "001.json", &old_batch_decision("alice", "d-one"));
    let preview = batch_preview(&home, "alice");
    let expired_token = preview["confirm_token"].as_str().unwrap();
    let expired_path = confirmation_path(&home, expired_token).unwrap();
    let mut confirmation: BatchConfirmation =
        serde_json::from_slice(&std::fs::read(&expired_path).unwrap()).unwrap();
    confirmation.created_at =
        (chrono::Utc::now() - chrono::Duration::seconds(BATCH_CONFIRM_TTL_SECS + 1)).to_rfc3339();
    crate::store::save_atomic(&expired_path, &confirmation).unwrap();

    let fresh = batch_preview(&home, "alice");
    let fresh_path = confirmation_path(&home, fresh["confirm_token"].as_str().unwrap()).unwrap();
    assert!(!expired_path.exists());
    assert!(fresh_path.exists());
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_rejects_unknown_status_filter_3227() {
    let home = tmp_home("batch-status-3227");
    let result = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "until": "2021-01-01T00:00:00Z",
            "status": "mystery",
            "audit_reason": "test cleanup"
        }),
    );
    assert!(result["error"].as_str().unwrap().contains("invalid status"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_refuses_pending_and_newer_schema_3227() {
    let home = tmp_home("batch-fail-closed-3227");
    let mut pending = old_batch_decision("alice", "d-pending");
    pending.needs_answer = true;
    pending.status = Some(DecisionStatus::Pending);
    write_named_decision(&home, "001.json", &pending);
    let refused = batch_preview(&home, "alice");
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("unresolved question"));

    std::fs::remove_file(decisions_dir(&home).join("001.json")).unwrap();
    let mut future = old_batch_decision("alice", "d-future");
    future.schema_version = SCHEMA_VERSION + 1;
    write_named_decision(&home, "002.json", &future);
    let refused = batch_preview(&home, "alice");
    assert!(refused["error"].as_str().unwrap().contains("newer schema"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_refuses_question_with_missing_status_3227() {
    let home = tmp_home("batch-missing-question-status-3227");
    let mut unresolved = old_batch_decision("alice", "d-unresolved");
    unresolved.needs_answer = true;
    unresolved.status = None;
    write_named_decision(&home, "001.json", &unresolved);
    let refused = batch_preview(&home, "alice");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("unresolved question"),
        "missing question status must fail closed: {refused}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_refuses_non_regular_json_source_3227() {
    let home = tmp_home("batch-non-regular-source-3227");
    std::fs::create_dir(decisions_dir(&home).join("rogue.json")).unwrap();
    let refused = batch_preview(&home, "alice");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("non-regular JSON source"),
        "JSON-shaped directory must fail closed: {refused}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn archive_batch_refuses_symlink_json_source_3227() {
    use std::os::unix::fs::symlink;

    let home = tmp_home("batch-symlink-source-3227");
    write_named_decision(&home, "001.json", &old_batch_decision("alice", "d-one"));
    symlink("001.json", decisions_dir(&home).join("alias.json")).unwrap();
    let refused = batch_preview(&home, "alice");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("non-regular JSON source"),
        "symlink source must fail closed: {refused}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_fails_closed_on_bad_policy_and_skips_protected_3227() {
    let home = tmp_home("batch-policy-3227");
    let fleet = crate::fleet::fleet_yaml_path(&home);
    if let Some(parent) = fleet.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        &fleet,
        "retention:\n  protected_decision_tags: not-a-list\n",
    )
    .unwrap();
    let decision = old_batch_decision("alice", "d-one");
    write_named_decision(&home, "001.json", &decision);
    let refused = batch_preview(&home, "alice");
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("policy is unreadable"));

    std::fs::write(&fleet, "retention:\n  protected_decision_tags: [KEEP]\n").unwrap();
    let mut protected = decision;
    protected.tags = vec!["KEEP".into()];
    write_named_decision(&home, "001.json", &protected);
    let preview = batch_preview(&home, "alice");
    assert_eq!(preview["candidate_count"], 0);
    assert_eq!(preview["protected_ids"], serde_json::json!(["d-one"]));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn archive_batch_surfaces_audit_failure_after_archive_3227() {
    let home = tmp_home("batch-audit-failure-3227");
    let decision = old_batch_decision("alice", "d-one");
    write_named_decision(&home, "imported.json", &decision);
    let preview = batch_preview(&home, "alice");
    let token = preview["confirm_token"].as_str().unwrap();
    std::fs::create_dir(home.join("event-log.jsonl")).unwrap();
    let applied = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "apply": true, "confirm_token": token, "confirm_ids": ["d-one"],
            "audit_reason": "test cleanup"
        }),
    );
    assert_eq!(applied["partial"], true);
    assert_eq!(applied["outcomes"][0]["outcome"], "archived_audit_failed");
    assert_eq!(applied["outcomes"][0]["audit_durable"], false);
    assert!(decisions_dir(&home).join(".archive/imported.json").exists());

    std::fs::remove_dir(home.join("event-log.jsonl")).unwrap();
    let retried = archive_batch(
        &home,
        "alice",
        &serde_json::json!({
            "apply": true, "confirm_token": token, "confirm_ids": ["d-one"],
            "audit_reason": "test cleanup"
        }),
    );
    assert_eq!(retried["partial"], false);
    assert_eq!(retried["outcomes"][0]["outcome"], "audit_repaired");
    assert_eq!(retried["outcomes"][0]["audit_durable"], true);
    assert!(durable_batch_audit_exists(&home, token, "d-one"));
    std::fs::remove_dir_all(&home).ok();
}

#[cfg(unix)]
#[test]
fn batch_regular_file_reader_refuses_symlink_3227() {
    use std::os::unix::fs::symlink;

    let home = tmp_home("batch-regular-reader-3227");
    let target = home.join("target.json");
    std::fs::write(&target, b"{}").unwrap();
    let link = home.join("link.json");
    symlink(&target, &link).unwrap();
    assert!(read_stable_regular_file(&link).is_err());
    std::fs::remove_dir_all(&home).ok();
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

/// Active pending questions — delegates to the prod `list_pending` (added in
/// PR2 now that the answer overlay calls it).
fn pending_questions(home: &Path) -> Vec<Decision> {
    list_pending(home)
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
fn answer_refuses_author_self_answer_2305() {
    // r2 hardening: the question's own author must not self-answer (would
    // bypass the operator). A different caller (the operator) still can.
    let home = tmp_home("dec_q_self_answer");
    let id = post_question(
        &home,
        serde_json::json!({"title": "Q", "content": "?", "needs_answer": true, "allow_free_text": true}),
    );
    // post_question authors as "lead" — lead answering its own question is refused.
    let r = answer(&home, "lead", &serde_json::json!({"id": id, "answer": "x"}));
    assert!(
        r.get("error")
            .is_some_and(|e| e.as_str().unwrap_or("").contains("cannot answer its own")),
        "author self-answer must be refused: {r}"
    );
    assert_eq!(
        pending_questions(&home).len(),
        1,
        "still pending after refused self-answer"
    );
    // The operator (different identity) answers fine.
    assert_eq!(
        answer(
            &home,
            "operator",
            &serde_json::json!({"id": id, "answer": "x"})
        )["status"],
        "answered"
    );
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

// ── #2524 P2b / #2313 — count_pending badge helper ──

// #3031: `count_pending` memoizes on the decisions-dir mtime in a single
// process-global slot, so concurrent `count_pending` calls from other tests
// would evict the entry mid-test and make the cache-reuse assertion flaky.
// Every `count_pending` caller in the suite shares this serial group (the
// two #2313 tests below included) — they are the complete set.
use serial_test::serial;

#[test]
#[serial(decisions_count_cache)]
fn count_pending_buckets_by_author_2313() {
    let home = tmp_home("count-pending-buckets-2313");
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q1", "content": "?", "needs_answer": true}),
    );
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q2", "content": "?", "needs_answer": true}),
    );
    post(
        &home,
        "bob",
        &serde_json::json!({"title": "Q3", "content": "?", "needs_answer": true}),
    );
    // Plain scope record (not a question) — must not count toward the badge.
    post(
        &home,
        "carol",
        &serde_json::json!({"title": "note", "content": "fyi"}),
    );

    let counts = count_pending(&home);
    assert_eq!(counts.total, 3, "3 pending questions across alice+bob");
    assert_eq!(counts.by_author.get("alice"), Some(&2));
    assert_eq!(counts.by_author.get("bob"), Some(&1));
    assert_eq!(
        counts.by_author.get("carol"),
        None,
        "a plain scope record must not appear in the badge tally"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[serial(decisions_count_cache)]
fn count_pending_excludes_answered_2313() {
    let home = tmp_home("count-pending-answered-2313");
    let created = post(
        &home,
        "alice",
        &serde_json::json!({
            "title": "Q", "content": "?", "needs_answer": true, "allow_free_text": true
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    answer(
        &home,
        "operator",
        &serde_json::json!({"id": id, "answer": "yes"}),
    );

    let counts = count_pending(&home);
    assert_eq!(
        counts.total, 0,
        "answered question must drop out of the tally"
    );
    assert!(counts.by_author.is_empty());
    std::fs::remove_dir_all(&home).ok();
}

// ── #3031 — count_pending memoization keyed by decisions-dir mtime ──

/// Two `count_pending` calls over a directory whose mtime has not moved must
/// run exactly one underlying scan.
///
/// Observed black-box, with no counter or production instrumentation: the
/// decision file is rewritten IN PLACE (`fs::write`), deliberately bypassing
/// `store::atomic_write`. A plain overwrite moves the FILE's mtime but not
/// the DIRECTORY's, so a second scan would see the edit and report a
/// different tally. Reporting the first call's tally is therefore only
/// possible if the second call did not scan at all.
///
/// This pins cache reuse only. It is not writer support: mutating a decision
/// outside `atomic_write` is unsupported, and the stale read below is the
/// accepted consequence of that, not a behaviour callers may rely on.
#[test]
#[serial(decisions_count_cache)]
fn count_pending_reuses_load_when_directory_unchanged_3031() {
    let home = tmp_home("count-pending-cache-reuse-3031");
    let created = post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q1", "content": "?", "needs_answer": true}),
    );
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q2", "content": "?", "needs_answer": true}),
    );
    let id = created["id"].as_str().expect("id").to_string();

    let first = count_pending(&home);
    assert_eq!(first.total, 2, "precondition: two pending questions");

    let path = decision_path(&home, &id);
    let dir_mtime_before = std::fs::metadata(decisions_dir(&home))
        .and_then(|m| m.modified())
        .expect("decisions dir mtime");
    let raw = std::fs::read_to_string(&path).expect("read decision");
    let mut doc: serde_json::Value = serde_json::from_str(&raw).expect("parse decision");
    doc["archived"] = serde_json::Value::Bool(true);
    std::fs::write(&path, serde_json::to_string(&doc).expect("serialize")).expect("in-place write");
    let dir_mtime_after = std::fs::metadata(decisions_dir(&home))
        .and_then(|m| m.modified())
        .expect("decisions dir mtime");
    assert_eq!(
        dir_mtime_before, dir_mtime_after,
        "test precondition: an in-place file rewrite must not move the directory mtime"
    );

    let second = count_pending(&home);
    assert_eq!(
        second.total, 2,
        "unchanged directory mtime must reuse the first scan's counts \
             (a re-scan would see the in-place edit and report 1)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// A canonical decision write (`post` → `save` → `store::atomic_write`, i.e.
/// temp file in the same directory + `rename`) moves the directory mtime and
/// must invalidate the memoized tally. Guards the cache against serving
/// stale counts on the real mutation path; passes with or without the cache.
#[test]
#[serial(decisions_count_cache)]
fn count_pending_refreshes_after_atomic_mutation_3031() {
    let home = tmp_home("count-pending-cache-refresh-3031");
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q1", "content": "?", "needs_answer": true}),
    );
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q2", "content": "?", "needs_answer": true}),
    );

    let first = count_pending(&home);
    assert_eq!(first.total, 2, "precondition: two pending questions");
    let dir_mtime_before = std::fs::metadata(decisions_dir(&home))
        .and_then(|m| m.modified())
        .expect("decisions dir mtime");

    post(
        &home,
        "bob",
        &serde_json::json!({"title": "Q3", "content": "?", "needs_answer": true}),
    );

    let dir_mtime_after = std::fs::metadata(decisions_dir(&home))
        .and_then(|m| m.modified())
        .expect("decisions dir mtime");
    assert_ne!(
        dir_mtime_before, dir_mtime_after,
        "test precondition: an atomic decision write must move the directory \
             mtime (if this fires, the filesystem's mtime granularity is too \
             coarse to distinguish the two writes, not a cache defect)"
    );

    let second = count_pending(&home);
    assert_eq!(
        second.total, 3,
        "atomic decision write must refresh the memoized counts"
    );
    assert_eq!(second.by_author.get("bob"), Some(&1));

    std::fs::remove_dir_all(&home).ok();
}

/// When the decisions directory has no readable metadata (it does not exist
/// yet), the memoization is bypassed entirely: the call still reports a real
/// tally, and nothing is stored that could shadow a later valid directory.
#[test]
#[serial(decisions_count_cache)]
fn count_pending_bypasses_cache_when_metadata_unreadable_3031() {
    let missing = std::env::temp_dir().join(format!(
        "agend-decisions-missing-{}-3031",
        std::process::id()
    ));
    std::fs::remove_dir_all(&missing).ok();

    let counts = count_pending(&missing);
    assert_eq!(counts.total, 0, "absent decisions dir tallies zero");
    assert!(counts.by_author.is_empty());

    let home = tmp_home("count-pending-bypass-3031");
    post(
        &home,
        "alice",
        &serde_json::json!({"title": "Q", "content": "?", "needs_answer": true}),
    );
    let counts = count_pending(&home);
    assert_eq!(
        counts.total, 1,
        "a valid directory must not be shadowed by the earlier metadata failure"
    );

    std::fs::remove_dir_all(&home).ok();
}

// ── #2524 P2c / #2313 — timeout+default validation + auto_answer_timeout ──

#[test]
fn post_timeout_secs_without_needs_answer_rejected_2313() {
    let home = tmp_home("timeout-needs-answer-2313");
    let result = post(
        &home,
        "lead",
        &serde_json::json!({"title": "x", "content": "?", "timeout_secs": 60}),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("needs_answer"),
        "timeout_secs without needs_answer must error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn post_timeout_secs_without_default_or_recommended_rejected_2313() {
    let home = tmp_home("timeout-no-default-2313");
    let result = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60
        }),
    );
    assert!(
        result["error"]
            .as_str()
            .unwrap_or("")
            .contains("timeout_default"),
        "timeout_secs without a resolvable default must error: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn post_timeout_secs_derives_default_from_recommended_option_2313() {
    let home = tmp_home("timeout-derives-default-2313");
    let result = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60,
            "options": [{"label": "proceed", "recommended": true}, {"label": "abort"}]
        }),
    );
    assert_eq!(result["status"], "posted", "post must succeed: {result}");
    let id = result["id"].as_str().expect("id").to_string();
    let listed = list(&home, &serde_json::json!({}));
    let decisions = listed["decisions"].as_array().expect("array");
    let d = decisions.iter().find(|d| d["id"] == id).expect("found");
    assert_eq!(d["timeout_secs"], 60);
    assert_eq!(d["timeout_default"], "proceed");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn post_timeout_secs_accepts_explicit_default_without_recommended_2313() {
    let home = tmp_home("timeout-explicit-default-2313");
    let result = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60,
            "timeout_default": "proceed"
        }),
    );
    assert_eq!(result["status"], "posted", "post must succeed: {result}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn auto_answer_timeout_answers_pending_decision_2313() {
    let home = tmp_home("auto-answer-2313");
    let created = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60,
            "timeout_default": "proceed-with-lean"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();

    let result = auto_answer_timeout(&home, &id);
    let (author, title) = result.expect("must auto-answer a pending timeout decision");
    assert_eq!(author, "lead");
    assert_eq!(title, "x");

    let listed = list(&home, &serde_json::json!({}));
    let decisions = listed["decisions"].as_array().expect("array");
    let d = decisions.iter().find(|d| d["id"] == id).expect("found");
    assert_eq!(d["answer"], "proceed-with-lean");
    assert_eq!(d["answered_by"], "timeout-default");
    assert_eq!(d["status"], "answered");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn auto_answer_timeout_is_idempotent_2313() {
    let home = tmp_home("auto-answer-idempotent-2313");
    let created = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60,
            "timeout_default": "proceed"
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();

    let first = auto_answer_timeout(&home, &id);
    assert!(first.is_some(), "first call must answer: {first:?}");
    let second = auto_answer_timeout(&home, &id);
    assert!(
        second.is_none(),
        "already-answered decision must not be re-answered: {second:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn auto_answer_timeout_none_for_operator_answered_decision_2313() {
    let home = tmp_home("auto-answer-operator-beat-2313");
    let created = post(
        &home,
        "lead",
        &serde_json::json!({
            "title": "x", "content": "?", "needs_answer": true, "timeout_secs": 60,
            "timeout_default": "proceed", "allow_free_text": true
        }),
    );
    let id = created["id"].as_str().expect("id").to_string();
    // Operator answers first (the race the timeout tracker must lose to).
    answer(
        &home,
        "operator",
        &serde_json::json!({"id": id, "answer": "operator-choice"}),
    );

    let result = auto_answer_timeout(&home, &id);
    assert!(
        result.is_none(),
        "must not clobber an already-operator-answered decision: {result:?}"
    );
    let listed = list(&home, &serde_json::json!({}));
    let decisions = listed["decisions"].as_array().expect("array");
    let d = decisions.iter().find(|d| d["id"] == id).expect("found");
    assert_eq!(
        d["answer"], "operator-choice",
        "operator's answer must survive"
    );
    std::fs::remove_dir_all(&home).ok();
}
