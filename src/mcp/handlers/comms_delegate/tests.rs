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
// Architecture-14 Item 7 — Merge Train Slice 1: admission RED tests.
//
// Frozen contract: d-20260720234700739461-7 (corrected seam).
// These tests exercise `handle_delegate_task` on branch-producing
// non-review dispatches and assert merge-train admission behavior
// that does NOT yet exist in production — they must RED stably.
// ─────────────────────────────────────────────────────────────────
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod merge_train_admission_tests {
    use super::super::handle_delegate_task;
    use crate::identity::Sender;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    // ── helpers ──────────────────────────────────────────────────

    fn mt_home(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let id = CTR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-mt-{}-{label}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_repo(home: &Path, name: &str, slug: &str) -> PathBuf {
        let repo = home.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ]);
        run(&[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{slug}.git"),
        ]);
        let head = std::process::Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap();
        run(&["update-ref", "refs/remotes/origin/main", head.trim()]);
        repo.canonicalize().unwrap()
    }

    fn seed_fleet(home: &Path, repo: &Path) {
        let yaml = format!(
            "instances:\n\
             \x20 lead:\n\
             \x20   backend: claude\n\
             \x20   id: 11111111-1111-4111-8111-111111111111\n\
             \x20 dev-a:\n\
             \x20   backend: claude\n\
             \x20   id: 22222222-2222-4222-8222-222222222222\n\
             \x20 dev-b:\n\
             \x20   backend: claude\n\
             \x20   id: 33333333-3333-4333-8333-333333333333\n\
             teams:\n\
             \x20 core:\n\
             \x20   orchestrator: lead\n\
             \x20   members:\n\
             \x20     - lead\n\
             \x20     - dev-a\n\
             \x20     - dev-b\n\
             \x20   source_repo: {}\n",
            repo.display()
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    fn fixture(home: &Path) -> PathBuf {
        let repo = init_repo(home, "canonical", "owner/repo");
        seed_fleet(home, &repo);
        repo
    }

    fn seed_two_repo_fleet(home: &Path, repo_a: &Path, repo_b: &Path) {
        let yaml = format!(
            "instances:\n  lead: {{ backend: claude, id: 11111111-1111-4111-8111-111111111111 }}\n  dev-a: {{ backend: claude, id: 22222222-2222-4222-8222-222222222222 }}\n  dev-b: {{ backend: claude, id: 33333333-3333-4333-8333-333333333333 }}\nteams:\n  repo-a:\n    orchestrator: lead\n    members: [lead, dev-a]\n    source_repo: {}\n  repo-b:\n    orchestrator: lead\n    members: [lead, dev-b]\n    source_repo: {}\n",
            repo_a.display(), repo_b.display()
        );
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    fn create_task(home: &Path, task_id: &str) {
        let event = crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId(task_id.into()),
            title: format!("task {task_id}"),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(crate::task_events::InstanceName("lead".into())),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("lead".into()),
            event,
        )
        .unwrap();
        set_meta(home, task_id, "review_class", json!("single"));
    }

    fn create_task_on_board(home: &Path, task_id: &str, project: &str) {
        let board = crate::task_events::board_root(home, project);
        std::fs::create_dir_all(&board).ok();
        let event = crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId(task_id.into()),
            title: format!("task {task_id}"),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(crate::task_events::InstanceName("lead".into())),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
        };
        crate::task_events::append_at(
            &board,
            &crate::task_events::InstanceName("lead".into()),
            event,
        )
        .unwrap();
        crate::task_events::append_at(
            &board,
            &crate::task_events::InstanceName("lead".into()),
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(task_id.into()),
                by: crate::task_events::InstanceName("lead".into()),
                key: "review_class".into(),
                value: json!("single"),
            },
        )
        .unwrap();
    }

    fn set_meta(home: &Path, task_id: &str, key: &str, value: Value) {
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("lead".into()),
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(task_id.into()),
                by: crate::task_events::InstanceName("lead".into()),
                key: key.into(),
                value,
            },
        )
        .unwrap();
    }

    fn dispatch(home: &Path, target: &str, task_id: &str, branch: &str) -> Value {
        let sender = Some(Sender::new("lead").unwrap());
        let runtime = crate::mcp::handlers::minimal_test_runtime();
        handle_delegate_task(
            home,
            &json!({"instance": target, "task": "implement", "task_id": task_id, "branch": branch}),
            &sender,
            Some(&runtime),
        )
    }

    fn dispatch_with_repo(
        home: &Path,
        target: &str,
        task_id: &str,
        branch: &str,
        repo: &str,
    ) -> Value {
        let sender = Some(Sender::new("lead").unwrap());
        let runtime = crate::mcp::handlers::minimal_test_runtime();
        handle_delegate_task(
            home,
            &json!({"instance": target, "task": "implement", "task_id": task_id, "branch": branch, "repository": repo}),
            &sender,
            Some(&runtime),
        )
    }

    fn read_meta(home: &Path, task_id: &str, key: &str) -> Option<Value> {
        crate::task_events::replay(home)
            .ok()?
            .tasks
            .get(&crate::task_events::TaskId(task_id.into()))?
            .metadata
            .get(key)
            .cloned()
    }

    const TRAIN_KEYS: [&str; 4] = [
        "merge_train_repository",
        "merge_train_domain",
        "merge_train_position",
        "merge_train_queue_seq",
    ];

    fn train_event_count(home: &Path, task_id: &str) -> usize {
        let mut count = 0;
        let default_log = home.join("task_events.jsonl");
        if let Ok(content) = std::fs::read_to_string(&default_log) {
            count += content
                .lines()
                .filter(|l| l.contains(task_id) && l.contains("merge_train_"))
                .count();
        }
        let boards_dir = home.join("boards");
        if boards_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&boards_dir) {
                for e in entries.flatten() {
                    let p = e.path().join("task_events.jsonl");
                    if let Ok(c) = std::fs::read_to_string(&p) {
                        count += c
                            .lines()
                            .filter(|l| l.contains(task_id) && l.contains("merge_train_"))
                            .count();
                    }
                }
            }
        }
        count
    }

    // ════════════════════════════════════════════════════════════════
    // Group A — concurrent same-key admission
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn a_concurrent_same_key_one_front_one_queued() {
        let home = mt_home("a-conc");
        fixture(&home);
        create_task(&home, "t-a1");
        create_task(&home, "t-a2");
        set_meta(&home, "t-a1", "conflict_domain", json!("core"));
        set_meta(&home, "t-a2", "conflict_domain", json!("core"));

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (home1, home2) = (home.clone(), home.clone());
        let (b1, b2) = (Arc::clone(&barrier), Arc::clone(&barrier));

        let h1 = std::thread::spawn(move || {
            b1.wait();
            dispatch(&home1, "dev-a", "t-a1", "feat/a1")
        });
        let h2 = std::thread::spawn(move || {
            b2.wait();
            dispatch(&home2, "dev-b", "t-a2", "feat/a2")
        });
        let _r1 = h1.join().unwrap();
        let _r2 = h2.join().unwrap();

        let pos1 = read_meta(&home, "t-a1", "merge_train_position");
        let pos2 = read_meta(&home, "t-a2", "merge_train_position");
        let positions: Vec<&str> = [&pos1, &pos2]
            .iter()
            .filter_map(|v| v.as_ref().and_then(|v| v.as_str()))
            .collect();
        assert!(
            positions.contains(&"Front") && positions.contains(&"Queued"),
            "concurrent admission must produce exactly one Front and one Queued, \
             got t-a1={pos1:?} t-a2={pos2:?}"
        );

        let seq1 = read_meta(&home, "t-a1", "merge_train_queue_seq");
        let seq2 = read_meta(&home, "t-a2", "merge_train_queue_seq");
        let seqs: Vec<u64> = [&seq1, &seq2]
            .iter()
            .filter_map(|v| v.as_ref().and_then(|v| v.as_u64()))
            .collect();
        assert_eq!(
            seqs.len(),
            2,
            "both tasks must have queue_seq: {seq1:?}, {seq2:?}"
        );
        assert!(
            seqs.contains(&1) && seqs.contains(&2),
            "Front=seq 1, Queued=seq 2, got {seqs:?}"
        );

        for tid in ["t-a1", "t-a2"] {
            for key in &TRAIN_KEYS {
                assert!(
                    read_meta(&home, tid, key).is_some(),
                    "{tid} missing durable metadata {key}"
                );
            }
        }

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_readmission_emits_zero_new_train_events() {
        let home = mt_home("a-readmit");
        fixture(&home);
        create_task(&home, "t-r1");
        set_meta(&home, "t-r1", "conflict_domain", json!("core"));

        dispatch(&home, "dev-a", "t-r1", "feat/r1");
        assert!(
            read_meta(&home, "t-r1", "merge_train_position").is_some(),
            "first dispatch must write merge_train_position"
        );

        let events_before = train_event_count(&home, "t-r1");
        dispatch(&home, "dev-a", "t-r1", "feat/r1");
        let events_after = train_event_count(&home, "t-r1");
        assert_eq!(
            events_before, events_after,
            "re-admission must emit zero new merge_train_* events"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ════════════════════════════════════════════════════════════════
    // Group B — queued suppresses all downstream side effects
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn b_queued_zero_side_effects() {
        let home = mt_home("b-queued");
        let repo = fixture(&home);
        create_task(&home, "t-front");
        create_task(&home, "t-q1");
        set_meta(&home, "t-front", "conflict_domain", json!("core"));
        set_meta(&home, "t-q1", "conflict_domain", json!("core"));

        dispatch(&home, "dev-a", "t-front", "feat/front");
        assert!(
            crate::binding::read(&home, "dev-a").is_some(),
            "Front fixture must reach auto-bind"
        );
        let inbox_before = crate::inbox::unread_count(&home, "dev-b");
        let ci_before =
            crate::binding::agent_has_active_ci_watch_on_branch(&home, "dev-b", "feat/q1");
        let tracking_before = crate::dispatch_tracking::has_for_instance(&home, "dev-b");
        let assignment_before = home.join("reviewer-assignments").exists();

        let result = dispatch(&home, "dev-b", "t-q1", "feat/q1");
        assert_eq!(
            result.get("merge_train_position").and_then(|v| v.as_str()),
            Some("Queued"),
            "second dispatch same repo+domain must be Queued: {result}"
        );

        // 1. inbox
        assert_eq!(
            crate::inbox::unread_count(&home, "dev-b"),
            inbox_before,
            "Queued must not deliver to inbox"
        );

        // 2. binding
        assert!(
            crate::binding::read(&home, "dev-b").is_none(),
            "Queued must not create binding"
        );

        // 3. worktree
        let wt = crate::worktree::worktree_path(&home, "dev-b", "feat/q1");
        assert!(!wt.exists(), "Queued must not create worktree");

        // 4. CI watch
        assert_eq!(
            crate::binding::agent_has_active_ci_watch_on_branch(&home, "dev-b", "feat/q1"),
            ci_before,
            "Queued must not arm CI watch"
        );

        // 5. review assignment
        assert_eq!(
            home.join("reviewer-assignments").exists(),
            assignment_before,
            "Queued must not create review assignment"
        );

        // 6. dispatch tracking
        assert_eq!(
            crate::dispatch_tracking::has_for_instance(&home, "dev-b"),
            tracking_before,
            "Queued must not write dispatch tracking"
        );

        let ref_status = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/feat/q1"])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .status()
            .unwrap();
        assert!(
            !ref_status.success(),
            "Queued must not create its canonical branch ref"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b_disjoint_repo_is_front() {
        let home = mt_home("b-disjoint-repo");
        let repo_a = init_repo(&home, "canonical-a", "owner/repo-a");
        let repo_b = init_repo(&home, "canonical-b", "owner/repo-b");
        seed_two_repo_fleet(&home, &repo_a, &repo_b);
        create_task(&home, "t-dr1");
        create_task(&home, "t-dr2");
        set_meta(&home, "t-dr1", "conflict_domain", json!("core"));
        set_meta(&home, "t-dr2", "conflict_domain", json!("core"));
        // Simulate repo-A identity via dispatch arg (future admission reads this).
        dispatch_with_repo(&home, "dev-a", "t-dr1", "feat/dr1", "owner/repo-a");

        // Second dispatch with a different repo identity.
        dispatch_with_repo(&home, "dev-b", "t-dr2", "feat/dr2", "owner/repo-b");

        let pos = read_meta(&home, "t-dr2", "merge_train_position");
        assert_eq!(
            pos.as_ref().and_then(|v| v.as_str()),
            Some("Front"),
            "disjoint repo must be Front (independent train): {pos:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b_disjoint_domain_is_front() {
        let home = mt_home("b-disjoint-dom");
        fixture(&home);
        create_task(&home, "t-dd1");
        create_task(&home, "t-dd2");
        set_meta(&home, "t-dd1", "conflict_domain", json!("backend"));
        set_meta(&home, "t-dd2", "conflict_domain", json!("frontend"));

        dispatch(&home, "dev-a", "t-dd1", "feat/dd1");
        dispatch(&home, "dev-b", "t-dd2", "feat/dd2");

        let pos = read_meta(&home, "t-dd2", "merge_train_position");
        assert_eq!(
            pos.as_ref().and_then(|v| v.as_str()),
            Some("Front"),
            "disjoint domain same repo must be Front: {pos:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b_absent_domain_falls_back_to_repo() {
        let home = mt_home("b-absent-dom");
        fixture(&home);
        create_task(&home, "t-no1");
        create_task(&home, "t-no2");
        // No conflict_domain set — must fall back to __repo__.

        dispatch(&home, "dev-a", "t-no1", "feat/no1");
        dispatch(&home, "dev-b", "t-no2", "feat/no2");

        let dom1 = read_meta(&home, "t-no1", "merge_train_domain");
        let dom2 = read_meta(&home, "t-no2", "merge_train_domain");
        assert_eq!(
            dom1.as_ref().and_then(|v| v.as_str()),
            Some("__repo__"),
            "absent domain falls back to __repo__: {dom1:?}"
        );
        assert_eq!(
            dom2.as_ref().and_then(|v| v.as_str()),
            Some("__repo__"),
            "absent domain falls back to __repo__: {dom2:?}"
        );
        let pos = read_meta(&home, "t-no2", "merge_train_position");
        assert_eq!(
            pos.as_ref().and_then(|v| v.as_str()),
            Some("Queued"),
            "second absent-domain task must be Queued: {pos:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn b_named_domain_disjoint_from_repo_fallback() {
        let home = mt_home("b-named-vs-repo");
        fixture(&home);
        create_task(&home, "t-bare");
        create_task(&home, "t-named");
        // t-bare: no domain → __repo__ fallback.
        // t-named: explicit "backend" domain.
        set_meta(&home, "t-named", "conflict_domain", json!("backend"));

        dispatch(&home, "dev-a", "t-bare", "feat/bare");
        dispatch(&home, "dev-b", "t-named", "feat/named");

        let pos = read_meta(&home, "t-named", "merge_train_position");
        assert_eq!(
            pos.as_ref().and_then(|v| v.as_str()),
            Some("Front"),
            "named domain disjoint from __repo__ fallback: {pos:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ════════════════════════════════════════════════════════════════
    // Group C — structured refusal on invalid pre-existing state
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn c_partial_train_metadata_refuses() {
        let home = mt_home("c-partial");
        fixture(&home);
        create_task(&home, "t-part");
        // Pre-seed only ONE of the four required train keys.
        set_meta(
            &home,
            "t-part",
            "merge_train_repository",
            json!("forge:owner/repo"),
        );

        let events_before = train_event_count(&home, "t-part");
        let result = dispatch(&home, "dev-a", "t-part", "feat/part");
        let code = result.get("code").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "partial train metadata must produce a merge_train_* refusal, got: {result}"
        );

        let events_after = train_event_count(&home, "t-part");
        assert_eq!(
            events_after, events_before,
            "partial-refusal must append zero new merge_train_* events"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c_mismatched_metadata_refuses() {
        let home = mt_home("c-mismatch");
        fixture(&home);
        create_task(&home, "t-mis");
        // Pre-seed all four train keys but with a WRONG repository.
        set_meta(
            &home,
            "t-mis",
            "merge_train_repository",
            json!("forge:wrong/repo"),
        );
        set_meta(&home, "t-mis", "merge_train_domain", json!("core"));
        set_meta(&home, "t-mis", "merge_train_position", json!("Front"));
        set_meta(&home, "t-mis", "merge_train_queue_seq", json!(1));

        let events_before = train_event_count(&home, "t-mis");
        let result = dispatch(&home, "dev-a", "t-mis", "feat/mis");
        let code = result.get("code").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "mismatched repo identity must produce merge_train_* refusal, got: {result}"
        );
        assert_eq!(
            train_event_count(&home, "t-mis"),
            events_before,
            "mismatch refusal must append zero new merge_train_* events"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c_ambiguous_cross_board_task_refuses() {
        let home = mt_home("c-ambig");
        fixture(&home);
        // Create same task_id on default board AND a project board.
        create_task(&home, "t-dup");
        create_task_on_board(&home, "t-dup", "other/project");

        let events_before = train_event_count(&home, "t-dup");
        let result = dispatch(&home, "dev-a", "t-dup", "feat/dup");
        let code = result.get("code").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "ambiguous task_id across boards must produce merge_train_* refusal, got: {result}"
        );

        for key in &TRAIN_KEYS {
            assert!(
                read_meta(&home, "t-dup", key).is_none(),
                "ambiguous refusal must not write {key}"
            );
        }
        assert_eq!(
            train_event_count(&home, "t-dup"),
            events_before,
            "ambiguous refusal must append zero new merge_train_* events"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c_ghost_task_refuses() {
        let home = mt_home("c-ghost");
        fixture(&home);
        // Do NOT create any task — dispatch a non-existent task_id.

        let result = dispatch(&home, "dev-a", "t-nonexistent", "feat/ghost");
        let code = result.get("code").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            code.starts_with("merge_train_"),
            "ghost task_id must produce merge_train_* refusal, got: {result}"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
