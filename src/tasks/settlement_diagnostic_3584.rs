#![allow(clippy::unwrap_used)]
use serde_json::{json, Value};
use std::path::PathBuf;

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let guard = Self(std::env::temp_dir().join(format!("agend-3584-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&guard.0).unwrap();
        guard
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        crate::binding::unbind(&self.0, "dev");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn task(home: &Home) -> String {
    let created = super::handle(
        &home.0,
        "lead",
        &json!({"action":"create", "title":"receipt diagnostic", "branch":"fix/evidence", "assignee":"dev"}),
    );
    let id = created["id"].as_str().unwrap().to_string();
    let claimed = super::handle(&home.0, "dev", &json!({"action":"claim", "id":id}));
    assert!(claimed.get("error").is_none(), "{claimed}");
    id
}
fn receipt(id: &str) -> crate::merge_receipt::MergeReceipt {
    let now = chrono::Utc::now();
    crate::merge_receipt::MergeReceipt {
        repo: "suzuke/agend-terminal".into(),
        merge_sha: "a".repeat(40),
        task_id: id.into(),
        task_assignee: "dev".into(),
        merge_authority: "lead".into(),
        pr_number: 1,
        created_at: now.to_rfc3339(),
        expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
    }
}
fn done(home: &Home, id: &str) -> Value {
    super::handle(
        &home.0,
        "dev",
        &json!({"action":"done", "id":id, "result":"isolated diagnostic"}),
    )
}

fn release_intents(home: &Home) -> Vec<(PathBuf, Vec<u8>)> {
    let entries = match std::fs::read_dir(home.0.join("auto_release_queue")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("cannot inspect release intents: {error}"),
    };
    let mut rows: Vec<_> = entries
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn concurrent_done_and_report_append_once_3584() {
    // Start together; this is a contention control, not a deterministic
    // checkpoint inside receipt lookup or the append transaction.
    for _ in 0..8 {
        let home = Home::new();
        let id = task(&home);
        let proof = receipt(&id);
        crate::merge_receipt::persist(&home.0, &proof).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (manual, report) = std::thread::scope(|scope| {
            let manual = scope.spawn(|| {
                barrier.wait();
                done(&home, &id)["status"] == "done"
            });
            let report = scope.spawn(|| {
                barrier.wait();
                matches!(
                    super::auto_close::auto_close_on_report(
                        &home.0,
                        "report",
                        &id,
                        "dev",
                        "concurrent evidence",
                        true,
                    ),
                    Ok(true)
                )
            });
            (manual.join().unwrap(), report.join().unwrap())
        });
        assert_eq!(usize::from(manual) + usize::from(report), 1);
        let routed = super::load_routed(&home.0, &id).unwrap();
        let events = crate::task_events::stream_envelopes_at(routed.board().path()).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    &e.event, crate::task_events::TaskEvent::Done { task_id, .. } if task_id.0 == id
                ))
                .count(),
            1
        );
        assert!(crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_none());
        assert!(crate::merge_receipt::find(&home.0, &proof.repo, &proof.merge_sha, &id).is_some());
        let settlements = std::fs::read_dir(home.0.join("merge-receipts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("task-completion"))
            .count();
        assert_eq!(settlements, 1);
        assert!(release_intents(&home).is_empty());
    }
}

fn opaque_binding_denies_receipt(report: bool, future: bool, unreadable: bool) {
    let home = Home::new();
    let id = task(&home);
    let proof = receipt(&id);
    crate::merge_receipt::persist(&home.0, &proof).unwrap();
    let runtime = crate::paths::runtime_dir(&home.0).join("dev");
    std::fs::create_dir_all(&runtime).unwrap();
    let path = runtime.join("binding.json");
    let bytes = if future {
        br#"{"version":18446744073709551615,"agent":"dev","task_id":"other"}"#.as_slice()
    } else {
        b"{incomplete binding".as_slice()
    };
    if unreadable {
        // A directory reliably fails read() even when tests run as root;
        // permission-bit fixtures do not provide that guarantee.
        std::fs::create_dir(&path).unwrap();
        assert_ne!(
            std::fs::read(&path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    } else {
        std::fs::write(&path, bytes).unwrap();
    }
    let task_before =
        serde_json::to_value(super::load_routed(&home.0, &id).unwrap().record()).unwrap();
    let intents_before = release_intents(&home);
    if report {
        let result = super::auto_close::auto_close_on_report(
            &home.0,
            "report",
            &id,
            "dev",
            "opaque must deny",
            true,
        );
        assert!(result.is_err(), "opaque binding was accepted: {result:?}");
    } else {
        let result = done(&home, &id);
        assert_eq!(result["code"], "assignee_completion_blocked", "{result}");
    }
    if unreadable {
        assert!(path.is_dir());
    } else {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    assert_eq!(release_intents(&home), intents_before);
    assert_eq!(
        serde_json::to_value(super::load_routed(&home.0, &id).unwrap().record()).unwrap(),
        task_before
    );
    assert!(crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_some());
}

#[test]
fn malformed_binding_denies_done_3584() {
    opaque_binding_denies_receipt(false, false, false);
}
#[test]
fn malformed_binding_denies_report_3584() {
    opaque_binding_denies_receipt(true, false, false);
}
#[test]
fn future_binding_denies_done_3584() {
    opaque_binding_denies_receipt(false, true, false);
}
#[test]
fn future_binding_denies_report_3584() {
    opaque_binding_denies_receipt(true, true, false);
}

#[test]
fn unreadable_binding_denies_done_3584() {
    opaque_binding_denies_receipt(false, false, true);
}
#[test]
fn unreadable_binding_denies_report_3584() {
    opaque_binding_denies_receipt(true, false, true);
}

fn signed_other_task_receipt_completion_3584(
    report: bool,
    invalid_signature: bool,
    same_task: bool,
    rebind: bool,
) {
    let home = Home::new();
    let id = task(&home);
    let proof = receipt(&id);
    crate::merge_receipt::persist(&home.0, &proof).unwrap();
    let other = super::handle(
        &home.0,
        "lead",
        &json!({"action":"create", "title":"actual other task", "branch":"fix/other", "assignee":"dev"}),
    );
    let other_id = other["id"].as_str().unwrap();
    let claimed = super::handle(&home.0, "dev", &json!({"action":"claim", "id":other_id}));
    assert!(claimed.get("error").is_none(), "{claimed}");
    let source = home.0.join("source");
    std::fs::create_dir_all(&source).unwrap();
    crate::git_helpers::git_cmd(&source, &["init", "-b", "main"]).unwrap();
    crate::git_helpers::git_cmd(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    )
    .unwrap();
    let origin = home.0.join("origin.git");
    crate::git_helpers::git_cmd(&source, &["clone", "--bare", ".", origin.to_str().unwrap()])
        .unwrap();
    crate::git_helpers::git_cmd(
        &source,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    )
    .unwrap();
    let checkout = crate::mcp::handlers::ci::handle_checkout_repo(
        &home.0,
        &json!({"repository_path":source, "branch":"fix/other", "from_ref":"main", "bind":true, "task_id":other_id}),
        "dev",
    );
    assert_eq!(checkout["bound"], true, "{checkout}");
    if same_task {
        let current = crate::binding::read(&home.0, "dev").unwrap();
        crate::binding::bind_full(
            &home.0,
            "dev",
            &id,
            "fix/other",
            std::path::Path::new(current["worktree"].as_str().unwrap()),
            &source,
            false,
        )
        .unwrap();
    }
    assert!(crate::binding::signature_valid(&home.0, "dev"));
    let binding = crate::binding::read(&home.0, "dev").unwrap();
    assert_eq!(
        binding["task_id"],
        if same_task { id.as_str() } else { other_id }
    );
    let worktree = std::path::Path::new(binding["worktree"].as_str().unwrap());
    assert!(crate::worktree_pool::is_daemon_managed(worktree));
    assert_eq!(
        crate::git_helpers::git_cmd(worktree, &["symbolic-ref", "--short", "HEAD"]).unwrap(),
        "fix/other"
    );
    assert_ne!(worktree, source);
    let binding_path = crate::paths::runtime_dir(&home.0).join("dev/binding.json");
    let before_bytes = std::fs::read(&binding_path).unwrap();
    let signature_path = binding_path.with_file_name("binding.json.sig");
    if invalid_signature {
        std::fs::write(&signature_path, b"invalid signature").unwrap();
        assert!(!crate::binding::signature_valid(&home.0, "dev"));
    }
    let signature_before = std::fs::read(&signature_path).unwrap();
    let task_before =
        serde_json::to_value(super::load_routed(&home.0, &id).unwrap().record()).unwrap();
    let other_before =
        serde_json::to_value(super::load_routed(&home.0, other_id).unwrap().record()).unwrap();
    crate::daemon::auto_release::enqueue_release_recompute(
        &home.0,
        "",
        "fix/other",
        "existing-evidence",
    );
    let intents_before = release_intents(&home);
    assert!(
        !intents_before.is_empty(),
        "must preserve an actual existing intent"
    );
    let rebound = std::rc::Rc::new(std::cell::RefCell::new(None));
    let wip = worktree.join("current-wip.txt");
    if rebind {
        std::fs::write(&wip, b"preserve new work").unwrap();
        crate::binding::unbind(&home.0, "dev");
        let hook_home = home.0.clone();
        let hook_worktree = worktree.to_path_buf();
        let hook_source = source.clone();
        let hook_task = other_id.to_string();
        let captured = std::rc::Rc::clone(&rebound);
        super::set_before_mutation_commit_hook_for_test(move || {
            crate::binding::bind_full(
                &hook_home,
                "dev",
                &hook_task,
                "fix/other",
                &hook_worktree,
                &hook_source,
                false,
            )
            .unwrap();
            let path = crate::paths::runtime_dir(&hook_home).join("dev/binding.json");
            *captured.borrow_mut() = Some((
                std::fs::read(&path).unwrap(),
                std::fs::read(path.with_file_name("binding.json.sig")).unwrap(),
                crate::binding::read(&hook_home, "dev").unwrap(),
            ));
        });
    }
    if report {
        let accepted = super::auto_close::auto_close_on_report(
            &home.0,
            "report",
            &id,
            "dev",
            "isolated completion evidence",
            true,
        );
        if invalid_signature || same_task {
            assert!(accepted.is_err(), "{accepted:?}");
        } else {
            assert!(matches!(accepted, Ok(true)), "{accepted:?}");
        }
    } else {
        let accepted = done(&home, &id);
        if invalid_signature || same_task {
            assert_eq!(
                accepted["code"], "assignee_completion_blocked",
                "{accepted}"
            );
        } else {
            assert_eq!(accepted["status"], "done", "{accepted}");
        }
    }
    let (before_bytes, signature_before, binding) = if rebind {
        assert_eq!(std::fs::read(&wip).unwrap(), b"preserve new work");
        rebound
            .borrow_mut()
            .take()
            .expect("rebind hook must execute")
    } else {
        (before_bytes, signature_before, binding)
    };
    assert_eq!(std::fs::read(&binding_path).unwrap(), before_bytes);
    assert_eq!(std::fs::read(&signature_path).unwrap(), signature_before);
    assert_eq!(crate::binding::read(&home.0, "dev").unwrap(), binding);
    assert_eq!(
        crate::binding::signature_valid(&home.0, "dev"),
        !invalid_signature
    );
    assert_eq!(
        serde_json::to_value(super::load_routed(&home.0, other_id).unwrap().record()).unwrap(),
        other_before
    );
    assert_eq!(release_intents(&home), intents_before);
    assert_eq!(
        crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_some(),
        invalid_signature || same_task
    );
    if invalid_signature || same_task {
        assert_eq!(
            serde_json::to_value(super::load_routed(&home.0, &id).unwrap().record()).unwrap(),
            task_before
        );
    }
    crate::binding::unbind(&home.0, "dev");
}

#[test]
fn corrective_done_preserves_signed_other_task_3584() {
    signed_other_task_receipt_completion_3584(false, false, false, false);
}

#[test]
fn corrective_report_preserves_signed_other_task_3584() {
    signed_other_task_receipt_completion_3584(true, false, false, false);
}

#[test]
fn invalid_signature_denies_done_3584() {
    signed_other_task_receipt_completion_3584(false, true, false, false);
}

#[test]
fn invalid_signature_denies_report_3584() {
    signed_other_task_receipt_completion_3584(true, true, false, false);
}

#[test]
fn same_task_branch_mismatch_denies_done_3584() {
    signed_other_task_receipt_completion_3584(false, false, true, false);
}
#[test]
fn same_task_branch_mismatch_denies_report_3584() {
    signed_other_task_receipt_completion_3584(true, false, true, false);
}

#[test]
fn rebind_before_done_preserves_new_work_3584() {
    signed_other_task_receipt_completion_3584(false, false, false, true);
}
#[test]
fn rebind_before_report_preserves_new_work_3584() {
    signed_other_task_receipt_completion_3584(true, false, false, true);
}

#[test]
fn valid_receipt_unrelated_binding_denies_but_unbound_permits_3584() {
    let home = Home::new();
    let id = task(&home);
    let proof = receipt(&id);
    crate::merge_receipt::persist(&home.0, &proof).unwrap();
    let runtime = crate::paths::runtime_dir(&home.0).join("dev");
    let binding = runtime.join("binding.json");
    std::fs::create_dir_all(&runtime).unwrap();
    crate::store::save_atomic(&binding, &json!({"version":1,"agent":"dev","task_id":"unrelated","branch":"fix/unrelated","worktree":home.0,"source_repo":home.0})).unwrap();
    assert!(crate::binding::read(&home.0, "dev").is_some());
    let denied = done(&home, &id);
    assert_eq!(denied["code"], "assignee_completion_blocked", "{denied}");
    assert!(crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_some());
    // Same receipt and task, only the unrelated isolated binding removed.
    crate::binding::unbind(&home.0, "dev");
    assert!(crate::binding::read(&home.0, "dev").is_none());
    let accepted = done(&home, &id);
    assert_eq!(accepted["status"], "done", "{accepted}");
    assert!(crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_none());
}

#[test]
fn invalid_receipts_deny_unbound_completion_3584() {
    invalid_receipts_deny_unbound_completion(false);
}

#[test]
fn invalid_receipts_deny_unbound_report_3584() {
    invalid_receipts_deny_unbound_completion(true);
}

fn invalid_receipts_deny_unbound_completion(report: bool) {
    for case in [
        "wrong-task",
        "wrong-assignee",
        "expired",
        "consumed",
        "ambiguous",
    ] {
        let home = Home::new();
        let id = task(&home);
        let mut proof = receipt(&id);
        match case {
            "wrong-task" => proof.task_id = "another-task".into(),
            "wrong-assignee" => proof.task_assignee = "another-agent".into(),
            "expired" => {
                proof.created_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
                proof.expires_at = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
            }
            _ => {}
        }
        crate::merge_receipt::persist(&home.0, &proof).unwrap();
        if case == "consumed" {
            crate::merge_receipt::settle_task_completion(&home.0, &proof).unwrap();
        }
        if case == "ambiguous" {
            let mut other = proof.clone();
            other.merge_sha = "b".repeat(40);
            crate::merge_receipt::persist(&home.0, &other).unwrap();
        }
        if report {
            let denied = super::auto_close::auto_close_on_report(
                &home.0,
                "report",
                &id,
                "dev",
                "invalid proof",
                true,
            );
            assert!(denied.is_err(), "{case}: {denied:?}");
        } else {
            let denied = done(&home, &id);
            assert_eq!(
                denied["code"], "assignee_completion_blocked",
                "{case}: {denied}"
            );
        }
    }
}

#[test]
fn valid_unbound_report_consumes_completion_not_ci_proof_3584() {
    let home = Home::new();
    let id = task(&home);
    let proof = receipt(&id);
    crate::merge_receipt::persist(&home.0, &proof).unwrap();
    let accepted = super::auto_close::auto_close_on_report(
        &home.0,
        "report",
        &id,
        "dev",
        "isolated valid proof",
        true,
    );
    assert!(matches!(accepted, Ok(true)), "{accepted:?}");
    assert!(crate::merge_receipt::find_for_task_completion(&home.0, &id, "dev").is_none());
    assert!(crate::merge_receipt::find(&home.0, &proof.repo, &proof.merge_sha, &id).is_some());
}

#[test]
fn post_merge_absent_link_skips_explicit_task_persists_3584() {
    let home = Home::new();
    let id = task(&home);
    let proof = receipt(&id);
    let skipped = crate::mcp::handlers::ci::post_merge_receipt_and_watch(
        &home.0,
        &proof.repo,
        &proof.merge_sha,
        1,
        "fix/evidence",
        "lead",
        None,
    );
    assert_eq!(skipped["skipped"], "no task-linked binding for PR branch");
    assert!(crate::merge_receipt::find(&home.0, &proof.repo, &proof.merge_sha, &id).is_none());
    let explicit = crate::mcp::handlers::ci::post_merge_receipt_and_watch(
        &home.0,
        &proof.repo,
        &proof.merge_sha,
        1,
        "fix/evidence",
        "lead",
        Some(&id),
    );
    assert_eq!(explicit["receipt"], "persisted", "{explicit}");
    let saved = crate::merge_receipt::find(&home.0, &proof.repo, &proof.merge_sha, &id).unwrap();
    let lifetime = chrono::DateTime::parse_from_rfc3339(&saved.expires_at).unwrap()
        - chrono::DateTime::parse_from_rfc3339(&saved.created_at).unwrap();
    assert!((3599..=3600).contains(&lifetime.num_seconds()));
}
