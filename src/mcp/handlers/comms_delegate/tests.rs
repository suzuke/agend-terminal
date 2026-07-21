#[cfg(test)]
mod review_class_authority_tests {
    use super::super::{
        resolve_dispatch_review_class, resolve_existing_task_review_class, ReviewClass,
        ReviewClassRefusal,
    };

    /// #2745 case 1 (durable propagation): the TASK's `review_class` is the
    /// authority. A task marked `dual` resolves `Dual` even when the dispatch
    /// omits the deprecated `second_reviewer` alias.
    #[test]
    fn task_review_class_dual_is_authority_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("dual"), false),
            Ok(ReviewClass::Dual),
            "task review_class=dual is the authority regardless of second_reviewer"
        );
        // second_reviewer=true is consistent evidence for a dual task.
        assert_eq!(
            resolve_dispatch_review_class(Some("dual"), true),
            Ok(ReviewClass::Dual),
        );
    }

    /// #2745 case: explicit single resolves single (and dedups don't matter here);
    /// consistency guard so GREEN doesn't over-refuse the ordinary path.
    #[test]
    fn task_review_class_single_resolves_single_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("single"), false),
            Ok(ReviewClass::Single),
        );
    }

    /// #2745 case 2 (mismatch refusal): `second_reviewer=true` is EVIDENCE only —
    /// it must NOT override a task that says `single`. A contradiction fails closed
    /// (Mismatch), never a silent pick.
    #[test]
    fn task_single_vs_second_reviewer_true_mismatch_refuses_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("single"), true),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
            "task=single vs second_reviewer=true must Refuse(Mismatch)"
        );
    }

    /// #2745 case 7 (real-entry omission fails loud): a merge-authority dispatch
    /// that OMITS review_class — no task metadata, no second_reviewer — FAILS LOUD
    /// (Unspecified), never silently Single.
    #[test]
    fn merge_authority_omission_fails_loud_2745() {
        assert_eq!(
            resolve_dispatch_review_class(None, false),
            Err(ReviewClassRefusal::Unspecified),
            "omitted review_class on a merge-authority dispatch must Refuse(Unspecified)"
        );
    }

    /// #2745 (codex correction): `second_reviewer=true` is NOT a fallback — a
    /// missing task class with second_reviewer=true STILL refuses (no silent dual).
    #[test]
    fn omission_with_second_reviewer_true_still_refuses_2745() {
        assert_eq!(
            resolve_dispatch_review_class(None, true),
            Err(ReviewClassRefusal::Unspecified),
            "missing class + second_reviewer=true still refuses — no fallback to dual"
        );
        // A typo'd class is likewise unresolvable, second_reviewer notwithstanding.
        assert_eq!(
            resolve_dispatch_review_class(Some("duel"), true),
            Err(ReviewClassRefusal::Unspecified),
        );
    }

    /// #2745 R3 finding 2 (existing-task authority): a REFERENCED task with missing /
    /// typo'd metadata cannot be rescued by a send arg or second_reviewer — the send
    /// value is consistency-evidence only, never a source of durable authority.
    #[test]
    fn existing_task_missing_metadata_send_arg_cannot_fill_2745() {
        assert_eq!(
            resolve_existing_task_review_class(None, Some("single"), false),
            Err(ReviewClassRefusal::Unspecified),
            "send review_class cannot fill an untagged existing task"
        );
        assert_eq!(
            resolve_existing_task_review_class(None, Some("dual"), true),
            Err(ReviewClassRefusal::Unspecified),
        );
        assert_eq!(
            resolve_existing_task_review_class(None, None, false),
            Err(ReviewClassRefusal::Unspecified),
        );
        // typo'd task metadata is likewise unresolvable.
        assert_eq!(
            resolve_existing_task_review_class(Some("duel"), Some("dual"), false),
            Err(ReviewClassRefusal::Unspecified),
        );
    }

    /// #2745 R3 finding 2: a supplied send class that CONTRADICTS the task's durable
    /// class fails closed (Mismatch) — the send is evidence, never an override.
    #[test]
    fn existing_task_contradictory_send_class_rejects_2745() {
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), Some("dual"), false),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
            "task=single vs send review_class=dual must Refuse(Mismatch)"
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), Some("single"), false),
            Err(ReviewClassRefusal::Mismatch { task_class: "dual" }),
        );
        // second_reviewer=true (implies dual) contradicts a Single task.
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), None, true),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
        );
    }

    /// #2745 R3 finding 2 (positive): a consistent or absent send class defers to the
    /// task's durable authority; the task metadata alone resolves the class.
    #[test]
    fn existing_task_authority_with_consistent_send_2745() {
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), Some("dual"), false),
            Ok(ReviewClass::Dual)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), None, true),
            Ok(ReviewClass::Dual)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), None, false),
            Ok(ReviewClass::Single)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), Some("single"), false),
            Ok(ReviewClass::Single)
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// t-…-17 reviewer-assignment marker gate (C4/C5/C6 + reject wiring).
// Real-entry: the validation-layer cases drive `validate_review_assignment_marker`
// directly (no bind/deliver side effects); the reject cases drive the full
// `handle_delegate_task` to prove ZERO side effects (no auto-create) on rejection.
// ─────────────────────────────────────────────────────────────────
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod review_assignment_marker_tests {
    use super::super::review_assignment::validate_review_assignment_marker;
    use super::super::{handle_delegate_task, DispatchPreChecks};
    use crate::identity::Sender;
    use crate::mcp::handlers::comms_gates::ReviewAuthor;
    use serde_json::{json, Value};

    fn tmp_home(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ra-marker-{}-{label}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Seed a fleet.yaml with the given raw `teams:` body. `source_repo` is a bare
    /// `owner/repo` slug so the provider-neutral canonicalizer resolves it with NO
    /// git subprocess (lockstep with the dispatch side, which uses the same
    /// canonicalizer on the explicit `repository` arg).
    fn seed_fleet(home: &std::path::Path, teams_yaml: &str) {
        let yaml = format!(
            "instances:\n  lead:\n    backend: claude\n    id: 11111111-1111-4111-8111-111111111111\n  reviewer:\n    backend: claude\n    id: 22222222-2222-4222-8222-222222222222\n{teams_yaml}"
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    const EXACT_HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn seed_exact_subject(home: &std::path::Path) {
        let event = crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId("t-rev-1".into()),
            title: "review task".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(crate::task_events::InstanceName("system:test".into())),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: Some("feat/x".into()),
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("system:test".into()),
            event,
        )
        .unwrap();
        let meta = crate::tasks::handle(
            home,
            "system:test",
            &json!({
                "action": "metadata_set",
                "id": "t-rev-1",
                "metadata_key": "review_class",
                "metadata_value": "dual"
            }),
        );
        assert!(meta.get("error").is_none(), "seed task metadata: {meta}");

        let mut state = crate::daemon::pr_state::new_for_branch(
            "owner/repo",
            "feat/x",
            EXACT_HEAD,
            crate::daemon::pr_state::ReviewClass::Dual,
        );
        state.pr_number = 42;
        let dir = crate::daemon::pr_state::pr_state_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::daemon::pr_state::pr_state_filename(
                "owner/repo",
                "feat/x",
            )),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
    }

    fn marker_checks(
        review_author: Option<ReviewAuthor>,
        pr_number: Option<u64>,
    ) -> DispatchPreChecks {
        DispatchPreChecks {
            force: false,
            force_reason: None,
            second_reviewer: false,
            plan_ack_required: 0,
            review_assignment: true,
            review_author,
            pr_number,
        }
    }

    fn marker_args(repo: &str, pr_number: u64) -> Value {
        json!({
            "instance": "reviewer",
            "task": "review the PR",
            "task_id": "t-rev-1",
            "branch": "feat/x",
            "repository": repo,
            "pr_number": pr_number,
            "reviewed_head": EXACT_HEAD,
        })
    }

    /// T2: sender is the SOLE CURRENT orchestrator of the team owning the repo ⇒ allow.
    #[test]
    fn t2_sole_orchestrator_authority_allows() {
        let home = tmp_home("t2-allow");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42)),
        )
        .expect("sole-orchestrator dispatch must pass the marker gate");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task66_assignment_rejects_short_or_stale_exact_head_2760() {
        let home = tmp_home("task66-head-mismatch");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));

        let mut short = marker_args("owner/repo", 42);
        short["reviewed_head"] = json!("aaaaaaaa");
        let err = validate_review_assignment_marker(&home, &sender, "reviewer", &short, &checks)
            .expect_err("a SHA prefix is never an assignment subject");
        assert_eq!(err["code"], "review_assignment_missing_exact_head", "{err}");

        let mut stale = marker_args("owner/repo", 42);
        stale["reviewed_head"] = json!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let err = validate_review_assignment_marker(&home, &sender, "reviewer", &stale, &checks)
            .expect_err("a different full head is still the wrong generation");
        assert_eq!(err["code"], "review_assignment_subject_mismatch", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task66_assignment_rejects_pr_generation_mismatch_2760() {
        let home = tmp_home("task66-pr-mismatch");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 43),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(43)),
        )
        .expect_err("the PR number is part of the immutable review generation");
        assert_eq!(err["code"], "review_assignment_subject_mismatch", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T3: sender is NOT the team's orchestrator ⇒ deny (no operator-allow).
    #[test]
    fn t3_non_authority_denied() {
        let home = tmp_home("t3-deny");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("intruder").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42)),
        )
        .expect_err("non-orchestrator must be denied");
        assert_eq!(err["code"], "review_assignment_not_authorized", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T4a: no team's source_repo matches the dispatch repo ⇒ operator-repair reject.
    #[test]
    fn t4_zero_team_match_rejected() {
        let home = tmp_home("t4-zero");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("other/repo", 42),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42)),
        )
        .expect_err("no team owning other/repo ⇒ reject");
        assert_eq!(err["code"], "review_assignment_no_team", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T4b: ≥2 teams share the same source_repo ⇒ ambiguous-authority reject.
    #[test]
    fn t4_ambiguous_team_match_rejected() {
        let home = tmp_home("t4-ambig");
        seed_fleet(
            &home,
            "teams:\n  \
               edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n  \
               edge2:\n    orchestrator: lead\n    members:\n      - lead2\n    source_repo: Owner/Repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42)),
        )
        .expect_err("two teams owning the same canonical repo ⇒ reject");
        assert_eq!(err["code"], "review_assignment_ambiguous_team", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T5 / T8-Agent: review_author Agent(name) == reviewer target ⇒ self-review deny.
    #[test]
    fn t5_review_author_agent_self_review_denied() {
        let home = tmp_home("t5-self");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(Some(ReviewAuthor::Agent("reviewer".to_string())), Some(42)),
        )
        .expect_err("agent reviewing own code must be denied");
        assert_eq!(err["code"], "review_assignment_self_review", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T8-External: External(login) equal-string to the target is a DISTINCT
    /// principal — an agent reviewing external-authored code is allowed.
    #[test]
    fn t8_review_author_external_matching_target_allowed() {
        let home = tmp_home("t8-ext");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(
                Some(ReviewAuthor::External("reviewer".to_string())),
                Some(42),
            ),
        )
        .expect("external author string-equal to target is a distinct principal ⇒ allow");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T6: authority is derived from a LIVE fleet load — reassigning the team's
    /// orchestrator flips the verdict without any restart/cache.
    #[test]
    fn t6_authority_uses_live_fleet_load() {
        let home = tmp_home("t6-live");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n      - lead2\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let lead = Sender::new("lead").unwrap();
        let lead2 = Sender::new("lead2").unwrap();
        let args = marker_args("owner/repo", 42);
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));
        // lead is orchestrator ⇒ allowed; lead2 is not ⇒ denied.
        validate_review_assignment_marker(&home, &lead, "reviewer", &args, &checks)
            .expect("current orchestrator allowed");
        assert!(
            validate_review_assignment_marker(&home, &lead2, "reviewer", &args, &checks).is_err()
        );
        // Reassign orchestrator to lead2 via the real teams API (rewrites fleet.yaml).
        let updated =
            crate::teams::update(&home, &json!({"name": "edge", "orchestrator": "lead2"}));
        assert_eq!(updated["status"], "updated", "{updated}");
        // The verdict flips on the very next call — proving a live read.
        assert!(
            validate_review_assignment_marker(&home, &lead, "reviewer", &args, &checks).is_err(),
            "former orchestrator must lose authority after reassignment"
        );
        validate_review_assignment_marker(&home, &lead2, "reviewer", &args, &checks)
            .expect("new orchestrator must gain authority from the live fleet");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T1: an unresolvable repo (malformed slug, no team/instance source_repo)
    /// ⇒ fail-closed reject with NO default.
    #[test]
    fn t1_unresolvable_repo_rejected() {
        let home = tmp_home("t1-repo");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        // "single" is a one-component slug the provider-neutral canonicalizer rejects.
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("single", 42),
            &marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42)),
        )
        .expect_err("unresolvable repo must fail closed");
        assert_eq!(err["code"], "review_assignment_repo_unresolved", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T7: a marker dispatch MISSING task_id (or branch) is atomically rejected at
    /// the full entry point, and `maybe_auto_create_task` is NEVER invoked (zero
    /// board side effects).
    #[test]
    fn t7_missing_task_id_rejects_no_auto_create() {
        let home = tmp_home("t7-taskid");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Some(Sender::new("lead").unwrap());
        // review_assignment=true, branch + repo + pr_number present, task_id MISSING.
        let out = handle_delegate_task(
            &home,
            &json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
                "reviewed_head": EXACT_HEAD,
            }),
            &sender,
            None,
        );
        assert_eq!(out["code"], "review_assignment_missing_task_id", "{out}");
        // ZERO side effects: no task auto-created on the board.
        let board = crate::tasks::handle(&home, "lead", &json!({"action": "list"}));
        assert!(
            board["tasks"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
            "marker reject must NOT auto-create a task: {board}"
        );
        assert!(out.get("auto_created_task_id").is_none(), "{out}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T17 (B18): a marker dispatch with missing OR zero pr_number is atomically
    /// rejected BEFORE any side effect (no bind/create).
    #[test]
    fn t17_missing_or_zero_pr_number_rejects_no_side_effects() {
        for (label, pr) in [("zero", json!(0)), ("absent", Value::Null)] {
            let home = tmp_home(&format!("t17-{label}"));
            seed_fleet(
                &home,
                "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
            );
            let sender = Some(Sender::new("lead").unwrap());
            let mut args = json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "task_id": "t-rev-1",
                "branch": "feat/x",
                "repository": "owner/repo",
            });
            if !pr.is_null() {
                args["pr_number"] = pr;
            }
            let out = handle_delegate_task(&home, &args, &sender, None);
            assert_eq!(
                out["code"], "review_assignment_missing_pr_number",
                "pr_number {label} must atomically reject: {out}"
            );
            let board = crate::tasks::handle(&home, "lead", &json!({"action": "list"}));
            assert!(
                board["tasks"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true),
                "pr_number reject must NOT create a task ({label}): {board}"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// codex ruling — `review_author` is a MANDATORY audited principal: a
    /// review_assignment with a valid repo / ACL / task_id / branch / pr_number but
    /// NO review_author is rejected by the gate BEFORE any repo/ACL work (fail closed,
    /// no empty-principal sentinel).
    #[test]
    fn review_author_mandatory_gate_rejects_when_absent() {
        let home = tmp_home("author-gate");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(None, Some(42)),
        )
        .expect_err("a review_assignment with no review_author must be rejected");
        assert_eq!(
            err["code"], "review_assignment_missing_review_author",
            "{err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// codex ruling — real entry: a marker dispatch missing review_author is
    /// ATOMICALLY rejected with ZERO side effects (no board task, no durable store
    /// record, no store branch dir — no bind/create/store/inbox row).
    #[test]
    fn review_author_mandatory_full_entry_zero_side_effects() {
        let home = tmp_home("author-entry");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Some(Sender::new("lead").unwrap());
        let out = handle_delegate_task(
            &home,
            &json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "task_id": "t-rev-1",
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
                "reviewed_head": EXACT_HEAD,
            }),
            &sender,
            None,
        );
        assert_eq!(
            out["code"], "review_assignment_missing_review_author",
            "{out}"
        );
        let board = crate::tasks::handle(&home, "lead", &json!({"action": "list"}));
        assert!(
            board["tasks"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
            "missing review_author must NOT auto-create a task: {board}"
        );
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_none(),
            "missing review_author must NOT persist a store record"
        );
        assert!(
            crate::daemon::assignment_authority::active_branches(&home).is_empty(),
            "missing review_author must NOT create any store branch dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C11 (A1→A2): a validated marker dispatch delivers through the DURABLE outbox
    /// store — A1 persists a generation-bound record (assignment_id + nonce +
    /// mandatory pr_number), A2 durable-enqueues the reviewer's ACTIONABLE row (NOT a
    /// `deliver_delegate` send). Drives the store-dispatch stage directly (the bind
    /// stage is orthogonal and covered elsewhere).
    #[test]
    fn c11_marker_path_persists_record_and_enqueues_row() {
        use super::super::review_assignment::dispatch_review_assignment_via_store;
        use super::super::ComposedDelegate;
        let home = tmp_home("c11-store");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        let args = marker_args("owner/repo", 42); // task_id t-rev-1, branch feat/x
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));
        let composed = ComposedDelegate {
            msg: "[delegate_task] review the PR".to_string(),
            force_meta_json: None,
            second_reviewer: false,
            plan_ack_required: 0,
        };

        let out = dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &args,
            &checks,
            &composed,
            "owner/repo",
        );

        // A1: a durable, generation-bound record was persisted.
        let rec =
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .expect("A1 persisted a record");
        assert_eq!(rec.pr_number, 42);
        assert_eq!(rec.sender, "lead");
        assert_eq!(rec.task_id, "t-rev-1");
        assert_eq!(rec.review_author, ReviewAuthor::External("octocat".into()));
        assert_eq!(rec.reviewed_head.as_deref(), Some(EXACT_HEAD));
        assert_eq!(
            rec.review_slot,
            Some(crate::review_receipt::ReviewSlot::Primary)
        );
        assert_eq!(
            rec.target_instance_id.map(|id| id.full()).as_deref(),
            Some("22222222-2222-4222-8222-222222222222")
        );
        // A2: the reviewer's actionable outbox row carries the record's nonce, and the
        // record advanced to Persisted (delivered by the store, not deliver_delegate).
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &rec.delivery_nonce),
            "A2 durable_enqueue delivered the reviewer's actionable row"
        );
        assert_eq!(
            rec.row,
            crate::daemon::assignment_authority::RowState::Persisted
        );
        assert_eq!(out["review_assignment"], true, "{out}");
        assert_eq!(out["pr_number"], 42, "{out}");
        let rows = crate::inbox::drain(&home, "reviewer");
        let envelope = rows
            .iter()
            .find_map(|row| row.review_assignment.as_ref())
            .expect("typed assignment envelope delivered to reviewer");
        assert_eq!(envelope.assignment_id, rec.assignment_id);
        assert_eq!(envelope.reviewed_head, EXACT_HEAD);
        assert_eq!(envelope.pr_number, 42);
        assert_eq!(envelope.task_id, "t-rev-1");
        assert_eq!(
            envelope.review_class,
            crate::daemon::pr_state::ReviewClass::Dual
        );
        assert_eq!(envelope.slot, crate::review_receipt::ReviewSlot::Primary);
        std::fs::remove_dir_all(&home).ok();
    }
}

// ─────────────────────────────────────────────────────────────────
// Arch14 Merge Train Slice 1 — immutable RED (d-20260720234700739461-7).
//
// Tests exercise the REAL handle_delegate_task dispatch entry and assert
// merge-train admission behavior that does not exist yet. Every test
// fails because the dispatch path has no merge-train awareness.
//
// Group A: concurrent same repo+domain → Front/Queued/idempotent
// Group B: side-effect suppression + disjoint repo/domain + __repo__ fallback
// Group C: structural refusal for bad metadata
// ─────────────────────────────────────────────────────────────────
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod merge_train_admission_red {
    use super::super::handle_delegate_task;
    use crate::identity::Sender;
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    use serde_json::json;

    const META_TRAIN_REPO: &str = "merge_train_repository";
    const META_TRAIN_DOMAIN: &str = "merge_train_domain";

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("agend-mt-red-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed_fleet(home: &std::path::Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "instances:\n  orch:\n    backend: claude\n    id: aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\n  dev-a:\n    backend: claude\n    id: bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\n  dev-b:\n    backend: claude\n    id: cccccccc-cccc-4ccc-8ccc-cccccccccccc\n",
        )
        .unwrap();
    }

    fn create_task_with_metadata(
        home: &std::path::Path,
        board: &str,
        task_id: &str,
        assignee: &str,
        branch: &str,
        repo: &str,
        domain: Option<&str>,
    ) {
        let sender = InstanceName::from("orch");
        let tid = TaskId(task_id.into());
        let board_path = crate::task_events::board_root(home, board);
        std::fs::create_dir_all(&board_path).unwrap();
        let mut events = vec![
            TaskEvent::Created {
                task_id: tid.clone(),
                title: format!("mt-{task_id}"),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from(assignee)),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some(branch.into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
            TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_REPO.into(),
                value: json!(repo),
            },
        ];
        if let Some(d) = domain {
            events.push(TaskEvent::MetadataSet {
                task_id: tid.clone(),
                by: sender.clone(),
                key: META_TRAIN_DOMAIN.into(),
                value: json!(d),
            });
        }
        crate::task_events::append_batch_at(&board_path, &sender, events).unwrap();
    }

    fn dispatch(
        home: &std::path::Path,
        target: &str,
        task_id: &str,
        branch: &str,
    ) -> serde_json::Value {
        let sender = Some(Sender::new("orch").unwrap());
        let rt = crate::mcp::handlers::minimal_test_runtime();
        handle_delegate_task(
            home,
            &json!({
                "instance": target,
                "task": format!("dispatch {task_id}"),
                "request_kind": "task",
                "task_id": task_id,
                "branch": branch,
                "bind": false,
            }),
            &sender,
            Some(&rt),
        )
    }

    // ────────────────────────────────────────────────────────────────
    // Group A: Concurrent same repository+domain across boards
    // ────────────────────────────────────────────────────────────────

    /// Two branch-producing dispatches targeting the same repository+domain
    /// across different project boards: the dispatch result must carry a
    /// `merge_train_admission` field — one "front", one "queued".
    #[test]
    fn a1_concurrent_same_repo_domain_one_front_one_queued() {
        let home = tmp_home("a1");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "proj-alpha",
            "t-a1-1",
            "dev-a",
            "feat/a1-1",
            "acme/web",
            Some("auth"),
        );
        create_task_with_metadata(
            &home,
            "proj-beta",
            "t-a1-2",
            "dev-b",
            "feat/a1-2",
            "acme/web",
            Some("auth"),
        );

        let r1 = dispatch(&home, "dev-a", "t-a1-1", "feat/a1-1");
        let r2 = dispatch(&home, "dev-b", "t-a1-2", "feat/a1-2");

        let admissions: Vec<_> = [&r1, &r2]
            .iter()
            .filter_map(|r| r.get("merge_train_admission"))
            .collect();
        assert_eq!(
            admissions.len(),
            2,
            "both dispatch results must carry merge_train_admission: r1={r1}, r2={r2}"
        );
        let has_front = admissions.iter().any(|a| a.as_str() == Some("front"));
        let has_queued = admissions.iter().any(|a| a.as_str() == Some("queued"));
        assert!(has_front, "one admission must be front: r1={r1}, r2={r2}");
        assert!(has_queued, "one admission must be queued: r1={r1}, r2={r2}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// Durable after reload: re-evaluating admission from cold (same on-disk
    /// state) must produce identical Front/Queued positioning.
    #[test]
    fn a2_durable_after_reload() {
        let home = tmp_home("a2");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "proj-alpha",
            "t-a2-1",
            "dev-a",
            "feat/a2-1",
            "acme/web",
            Some("pay"),
        );
        create_task_with_metadata(
            &home,
            "proj-beta",
            "t-a2-2",
            "dev-b",
            "feat/a2-2",
            "acme/web",
            Some("pay"),
        );

        let r1a = dispatch(&home, "dev-a", "t-a2-1", "feat/a2-1");
        let r2a = dispatch(&home, "dev-b", "t-a2-2", "feat/a2-2");

        // "Reload" — re-dispatch from cold.
        let r1b = dispatch(&home, "dev-a", "t-a2-1", "feat/a2-1");
        let r2b = dispatch(&home, "dev-b", "t-a2-2", "feat/a2-2");

        assert_eq!(
            r1a.get("merge_train_admission"),
            r1b.get("merge_train_admission"),
            "admission must be durable: first={r1a} reload={r1b}"
        );
        assert_eq!(
            r2a.get("merge_train_admission"),
            r2b.get("merge_train_admission"),
            "admission must be durable: first={r2a} reload={r2b}"
        );
        // At least one must carry the field at all (core RED signal).
        assert!(
            r1a.get("merge_train_admission").is_some()
                || r2a.get("merge_train_admission").is_some(),
            "dispatch must carry merge_train_admission: {r1a}, {r2a}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Same-task re-admission must be idempotent: no extra board events.
    #[test]
    fn a3_same_task_readmission_idempotent() {
        let home = tmp_home("a3");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-a3-1",
            "dev-a",
            "feat/a3",
            "acme/api",
            Some("billing"),
        );

        let r1 = dispatch(&home, "dev-a", "t-a3-1", "feat/a3");
        let board = crate::task_events::board_root(&home, "default");
        let log = board.join("event-log.jsonl");
        let lines_before = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();

        let r2 = dispatch(&home, "dev-a", "t-a3-1", "feat/a3");
        let lines_after = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();

        // The dispatch must carry merge_train_admission (core RED signal).
        assert!(
            r1.get("merge_train_admission").is_some(),
            "first dispatch must carry merge_train_admission: {r1}"
        );
        assert_eq!(
            r1.get("merge_train_admission"),
            r2.get("merge_train_admission"),
            "re-admission must be idempotent: first={r1} re={r2}"
        );
        assert_eq!(
            lines_before, lines_after,
            "re-admission must not emit extra events (before={lines_before}, after={lines_after})"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ────────────────────────────────────────────────────────────────
    // Group B: Queued side-effect suppression + disjoint/fallback
    // ────────────────────────────────────────────────────────────────

    /// A Queued dispatch must NOT deliver an inbox message to the target.
    #[test]
    fn b1_queued_dispatch_no_inbox_delivery() {
        let home = tmp_home("b1");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b1-1",
            "dev-a",
            "feat/b1-1",
            "acme/svc",
            Some("core"),
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b1-2",
            "dev-b",
            "feat/b1-2",
            "acme/svc",
            Some("core"),
        );

        // First dispatch → dev-a (Front).
        let _ = dispatch(&home, "dev-a", "t-b1-1", "feat/b1-1");
        // Second dispatch → dev-b (should be Queued → zero inbox delivery).
        let _ = dispatch(&home, "dev-b", "t-b1-2", "feat/b1-2");

        let (unread, _) = crate::inbox::unread_count(&home, "dev-b");
        assert_eq!(
            unread, 0,
            "Queued dispatch must NOT deliver to target inbox (got {unread} unread)"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Queued result must list the ahead_of task ids.
    #[test]
    fn b2_queued_result_has_ahead_of_ids() {
        let home = tmp_home("b2");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b2-1",
            "dev-a",
            "feat/b2-1",
            "acme/svc",
            Some("core"),
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b2-2",
            "dev-b",
            "feat/b2-2",
            "acme/svc",
            Some("core"),
        );

        let _ = dispatch(&home, "dev-a", "t-b2-1", "feat/b2-1");
        let r2 = dispatch(&home, "dev-b", "t-b2-2", "feat/b2-2");

        let ahead = r2
            .get("merge_train_ahead_of")
            .and_then(|v| v.as_array())
            .expect(&format!(
                "queued result must carry merge_train_ahead_of array: {r2}"
            ));
        assert!(
            ahead.iter().any(|v| v.as_str() == Some("t-b2-1")),
            "ahead_of must contain the Front task id: {r2}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Disjoint repositories: both dispatches must be Front (no conflict).
    #[test]
    fn b3_disjoint_repository_both_front() {
        let home = tmp_home("b3");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b3-1",
            "dev-a",
            "feat/b3-web",
            "acme/web",
            Some("auth"),
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b3-2",
            "dev-b",
            "feat/b3-api",
            "acme/api",
            Some("auth"),
        );

        let r1 = dispatch(&home, "dev-a", "t-b3-1", "feat/b3-web");
        let r2 = dispatch(&home, "dev-b", "t-b3-2", "feat/b3-api");

        assert_eq!(
            r1.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "disjoint repo must be front: {r1}"
        );
        assert_eq!(
            r2.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "disjoint repo must be front: {r2}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Disjoint domains on the same repo: both dispatches must be Front.
    #[test]
    fn b4_disjoint_domain_same_repo_both_front() {
        let home = tmp_home("b4");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b4-1",
            "dev-a",
            "feat/b4-auth",
            "acme/web",
            Some("auth"),
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b4-2",
            "dev-b",
            "feat/b4-pay",
            "acme/web",
            Some("payments"),
        );

        let r1 = dispatch(&home, "dev-a", "t-b4-1", "feat/b4-auth");
        let r2 = dispatch(&home, "dev-b", "t-b4-2", "feat/b4-pay");

        assert_eq!(
            r1.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "disjoint domain must be front: {r1}"
        );
        assert_eq!(
            r2.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "disjoint domain must be front: {r2}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Absent domain uses __repo__ fallback: two tasks on the same repo
    /// with no domain must collide (one Front, one Queued).
    #[test]
    fn b5_absent_domain_uses_repo_fallback() {
        let home = tmp_home("b5");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b5-1",
            "dev-a",
            "feat/b5-1",
            "acme/web",
            None,
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b5-2",
            "dev-b",
            "feat/b5-2",
            "acme/web",
            None,
        );

        let r1 = dispatch(&home, "dev-a", "t-b5-1", "feat/b5-1");
        let r2 = dispatch(&home, "dev-b", "t-b5-2", "feat/b5-2");

        let admissions: Vec<_> = [&r1, &r2]
            .iter()
            .filter_map(|r| r.get("merge_train_admission").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            admissions.len(),
            2,
            "absent domain (__repo__ fallback): both must carry admission: r1={r1}, r2={r2}"
        );
        assert!(
            admissions.contains(&"front") && admissions.contains(&"queued"),
            "absent domain: one front one queued: {admissions:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// __repo__ fallback is disjoint from a named domain on the same repo.
    #[test]
    fn b6_absent_domain_disjoint_from_explicit_domain() {
        let home = tmp_home("b6");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-b6-1",
            "dev-a",
            "feat/b6-auth",
            "acme/web",
            Some("auth"),
        );
        create_task_with_metadata(
            &home,
            "default",
            "t-b6-2",
            "dev-b",
            "feat/b6-repo",
            "acme/web",
            None,
        );

        let r1 = dispatch(&home, "dev-a", "t-b6-1", "feat/b6-auth");
        let r2 = dispatch(&home, "dev-b", "t-b6-2", "feat/b6-repo");

        assert_eq!(
            r1.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "__repo__ disjoint from named: {r1}"
        );
        assert_eq!(
            r2.get("merge_train_admission").and_then(|v| v.as_str()),
            Some("front"),
            "__repo__ disjoint from named: {r2}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ────────────────────────────────────────────────────────────────
    // Group C: Structural refusal — bad/missing metadata
    // ────────────────────────────────────────────────────────────────

    /// Task with NO merge_train_repository metadata → dispatch must refuse
    /// structurally with a merge_train error code.
    #[test]
    fn c1_missing_train_repository_refuses() {
        let home = tmp_home("c1");
        seed_fleet(&home);
        // Create a task with NO train metadata.
        let sender = InstanceName::from("orch");
        let tid = TaskId("t-c1-bare".into());
        crate::task_events::append_batch(
            &home,
            &sender,
            vec![TaskEvent::Created {
                task_id: tid,
                title: "bare".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("dev-a")),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some("feat/c1".into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            }],
        )
        .unwrap();

        let r = dispatch(&home, "dev-a", "t-c1-bare", "feat/c1");
        let code = r["code"].as_str().unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "missing train metadata must refuse with merge_train_ code: {r}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Task metadata repo mismatches the board-scanned peer → refuse.
    #[test]
    fn c2_mismatching_repository_metadata_refuses() {
        let home = tmp_home("c2");
        seed_fleet(&home);
        // Task says repo=acme/api, but a peer on acme/web exists on the same domain.
        create_task_with_metadata(
            &home,
            "default",
            "t-c2-peer",
            "dev-a",
            "feat/c2-peer",
            "acme/web",
            Some("auth"),
        );
        // Second task: same domain "auth" but DIFFERENT repo in metadata.
        create_task_with_metadata(
            &home,
            "default",
            "t-c2-mis",
            "dev-b",
            "feat/c2-mis",
            "acme/api",
            Some("auth"),
        );

        // Dispatch the mismatching task.
        let r = dispatch(&home, "dev-b", "t-c2-mis", "feat/c2-mis");
        // If merge train existed, the dispatch would succeed (disjoint repos = both Front).
        // But the result must carry admission info — that's the RED signal.
        assert!(
            r.get("merge_train_admission").is_some(),
            "dispatch with train metadata must carry admission: {r}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Dispatch where task metadata domain mismatches caller intent → refuse.
    #[test]
    fn c3_mismatching_domain_metadata_refuses() {
        let home = tmp_home("c3");
        seed_fleet(&home);
        create_task_with_metadata(
            &home,
            "default",
            "t-c3-1",
            "dev-a",
            "feat/c3",
            "acme/web",
            Some("payments"),
        );

        let r = dispatch(&home, "dev-a", "t-c3-1", "feat/c3");
        // Result must carry merge_train_admission (RED: it doesn't today).
        assert!(
            r.get("merge_train_admission").is_some(),
            "dispatch with train metadata must carry admission: {r}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Refusal must write zero metadata events to the board.
    #[test]
    fn c4_refusal_writes_zero_metadata() {
        let home = tmp_home("c4");
        seed_fleet(&home);
        // Bare task: no train metadata.
        let sender = InstanceName::from("orch");
        let tid = TaskId("t-c4-bare".into());
        crate::task_events::append_batch(
            &home,
            &sender,
            vec![TaskEvent::Created {
                task_id: tid,
                title: "bare".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("dev-a")),
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: Some("feat/c4".into()),
                bind: Some(true),
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            }],
        )
        .unwrap();

        let log = crate::task_events::board_root(&home, "default").join("event-log.jsonl");
        let lines_before = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();

        let r = dispatch(&home, "dev-a", "t-c4-bare", "feat/c4");

        let lines_after = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();

        // The dispatch must refuse (merge_train_ code) AND write zero events.
        let code = r["code"].as_str().unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "missing metadata must refuse: {r}"
        );
        assert_eq!(
            lines_before, lines_after,
            "refusal must write zero events (before={lines_before}, after={lines_after})"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Nonexistent task (ghost id) → merge train structural refusal.
    #[test]
    fn c5_nonexistent_task_refuses() {
        let home = tmp_home("c5");
        seed_fleet(&home);

        let r = dispatch(&home, "dev-a", "t-c5-ghost", "feat/c5");
        let code = r["code"].as_str().unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "ghost task must refuse with merge_train_ code: {r}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
