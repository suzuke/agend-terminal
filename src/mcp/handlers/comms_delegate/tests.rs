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
    use crate::scm::{PrSummary, ScmProvider};
    use serde_json::{json, Value};
    use std::path::Path;
    use std::sync::Arc;

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
        seed_exact_subject_with_owner(home, "reviewer");
    }

    fn seed_exact_subject_with_owner(home: &std::path::Path, owner: &str) {
        let event = crate::task_events::TaskEvent::Created {
            task_id: crate::task_events::TaskId("t-rev-1".into()),
            title: "review task".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: Some(crate::task_events::InstanceName(owner.into())),
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: Some("feat/x".into()),
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
        };
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("system:test".into()),
            event,
        )
        .unwrap();
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("system:test".into()),
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId("t-rev-1".into()),
                by: crate::task_events::InstanceName("system:test".into()),
                key: "review_class".into(),
                value: json!("dual"),
            },
        )
        .unwrap();

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

    fn seed_provisional_subject(home: &std::path::Path) {
        seed_exact_subject(home);
        let mut state =
            crate::daemon::pr_state::load(home, "owner/repo", "feat/x").expect("seeded pr-state");
        state.pr_number = 0;
        state.pr_author.clear();
        crate::daemon::pr_state::save(home, &state).unwrap();
    }

    struct ProvisionalPrMock {
        summary: PrSummary,
        before_return: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    impl ScmProvider for ProvisionalPrMock {
        fn pr_view(&self, _repo: &str, _pr: u64, fields: &[&str]) -> anyhow::Result<PrSummary> {
            assert_eq!(
                fields,
                &["number", "headRefOid", "headRefName", "author"],
                "provisional hydration must request every persisted identity field"
            );
            if let Some(hook) = &self.before_return {
                hook();
            }
            Ok(self.summary.clone())
        }

        fn pr_checks(&self, _repo: &str, _pr: u64) -> anyhow::Result<Vec<crate::scm::CheckState>> {
            unimplemented!()
        }

        fn pr_list(
            &self,
            _repo: &str,
            _filter: &crate::scm::ListFilter,
            _fields: &[&str],
            _cwd: Option<&Path>,
        ) -> anyhow::Result<Vec<PrSummary>> {
            unimplemented!()
        }

        fn pr_merge(
            &self,
            _repo: &str,
            _pr: u64,
            _opts: &crate::scm::MergeOpts,
        ) -> anyhow::Result<crate::scm::MergeOutcome> {
            unimplemented!()
        }

        fn issue_view(
            &self,
            _repo: &str,
            _number: u64,
            _fields: &[&str],
        ) -> anyhow::Result<crate::scm::IssueSummary> {
            unimplemented!()
        }

        fn compare(
            &self,
            _repo: &str,
            _base: &str,
            _head: &str,
        ) -> anyhow::Result<crate::scm::CompareResult> {
            unimplemented!()
        }
    }

    fn seed_review_task(home: &std::path::Path, task_id: &str) {
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("system:test".into()),
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId(task_id.into()),
                title: "review task".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(crate::task_events::InstanceName("reviewer".into())),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: Some("feat/x".into()),
                bind: None,
                eta_secs: None,
                tags: Vec::new(),
                parent_id: None,
                governing_decision_id: None,
                review_class: None,
            },
        )
        .unwrap();
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("system:test".into()),
            crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(task_id.into()),
                by: crate::task_events::InstanceName("system:test".into()),
                key: "review_class".into(),
                value: json!("dual"),
            },
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

    fn seed_real_review_assignment_home(label: &str) -> std::path::PathBuf {
        let home = tmp_home(label);
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n      - reviewer\n    source_repo: owner/repo\n",
        );
        seed_provisional_subject(&home);
        home
    }

    fn run_real_review_assignment(home: &std::path::Path) -> Value {
        let sender = Some(Sender::new("lead").unwrap());
        handle_delegate_task(
            home,
            &json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "task_id": "t-rev-1",
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
                "reviewed_head": EXACT_HEAD,
                "review_author": {"external": "octocat"},
                "force": true,
                "force_reason": "3168 regression"
            }),
            &sender,
            None,
        )
    }

    #[test]
    fn real_review_assignment_hydrates_existing_provisional_state_3168() {
        let home = seed_real_review_assignment_home("3168-provisional");
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 42,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some(EXACT_HEAD.into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: None,
        }));
        let out = run_real_review_assignment(&home);
        assert_eq!(out["review_assignment"], true, "{out}");
        let state = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("hydrated pr-state");
        assert_eq!(state.pr_number, 42);
        assert_eq!(state.pr_author, "octocat");
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_some(),
            "real marker path must persist the assignment after hydration"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_review_assignment_rejects_scm_mismatch_3168() {
        let home = seed_real_review_assignment_home("3168-mismatch");
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 42,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: None,
        }));

        let out = run_real_review_assignment(&home);

        assert_eq!(
            out["code"], "review_assignment_subject_unavailable",
            "{out}"
        );
        let state = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("provisional state remains after mismatch");
        assert_eq!(state.pr_number, 0);
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_none(),
            "mismatch must not persist an assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_review_assignment_rejects_scm_number_mismatch_3168() {
        let home = seed_real_review_assignment_home("3168-number-mismatch");
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 41,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some(EXACT_HEAD.into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: None,
        }));

        let out = run_real_review_assignment(&home);

        assert_eq!(
            out["code"], "review_assignment_subject_unavailable",
            "{out}"
        );
        let state = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("number mismatch must leave the provisional state unchanged");
        assert_eq!(state.pr_number, 0);
        assert_eq!(state.head_sha, EXACT_HEAD);
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_none(),
            "number mismatch must not persist an assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3419 B2: the reviewer-assignment production entry must reload the
    /// exact governing decision for an existing task. A superseded decision is
    /// not authority and must be rejected before assignment-store delivery.
    #[test]
    #[serial_test::serial]
    fn real_review_assignment_rejects_superseded_governing_authority_3419() {
        let home = seed_real_review_assignment_home("3419-superseded-authority");
        let decision = crate::decisions::post(
            &home,
            "lead",
            &json!({
                "title": "review authority",
                "content": "dual review required",
                "review_class": "dual",
            }),
        );
        let decision_id = decision["id"].as_str().expect("decision id").to_string();
        let created = crate::tasks::handle(
            &home,
            "lead",
            &json!({
                "action": "create",
                "title": "governed review task",
                "assignee": "reviewer",
                "branch": "feat/x",
                "governing_decision_id": decision_id,
                "review_class": "dual",
            }),
        );
        let task_id = created["id"].as_str().expect("task id").to_string();
        let superseded = crate::decisions::post(
            &home,
            "lead",
            &json!({
                "title": "replacement review authority",
                "content": "single review required",
                "review_class": "single",
                "supersedes": decision_id,
            }),
        );
        assert_eq!(
            superseded["status"], "posted",
            "supersede decision: {superseded}"
        );

        let sender = Some(Sender::new("lead").unwrap());
        let out = handle_delegate_task(
            &home,
            &json!({
                "instance": "reviewer",
                "task": "review the governed PR",
                "review_assignment": true,
                "task_id": task_id,
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
                "reviewed_head": EXACT_HEAD,
                "review_author": {"external": "octocat"},
                "force": true,
                "force_reason": "3419 authority reload",
            }),
            &sender,
            None,
        );
        assert_eq!(
            out["code"], "task_governing_decision_unresolved",
            "superseded authority must reject reviewer assignment: {out}"
        );
        assert!(
            crate::daemon::assignment_authority::active_branches(&home).is_empty(),
            "rejected reviewer assignment must not persist an assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3419 B2 mutation proof: mutate the exact decision after the marker's
    /// first authority check. The later branch preflight (and, if reached, the
    /// store dispatch) must reload it and refuse before persisting delivery.
    #[test]
    #[serial_test::serial]
    fn real_review_assignment_reloads_authority_after_marker_check_3419() {
        let home = seed_real_review_assignment_home("3419-marker-reload");
        let decision = crate::decisions::post(
            &home,
            "lead",
            &json!({
                "title": "review authority",
                "content": "dual review required",
                "review_class": "dual",
            }),
        );
        let decision_id = decision["id"].as_str().expect("decision id").to_string();
        let created = crate::tasks::handle(
            &home,
            "lead",
            &json!({
                "action": "create",
                "title": "governed review task",
                "assignee": "reviewer",
                "branch": "feat/x",
                "governing_decision_id": decision_id,
                "review_class": "dual",
            }),
        );
        let task_id = created["id"].as_str().expect("task id").to_string();
        let decision_path = crate::decisions::decision_path(&home, &decision_id);
        super::super::review_assignment::install_marker_authority_validated_hook(Box::new(
            move |_| {
                std::fs::write(&decision_path, b"{tampered decision")
                    .expect("tamper decision after first authority check");
            },
        ));
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 42,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some(EXACT_HEAD.into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: None,
        }));

        let sender = Some(Sender::new("lead").unwrap());
        let out = handle_delegate_task(
            &home,
            &json!({
                "instance": "reviewer",
                "task": "review the governed PR",
                "review_assignment": true,
                "task_id": task_id,
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
                "reviewed_head": EXACT_HEAD,
                "review_author": {"external": "octocat"},
                "force": true,
                "force_reason": "3419 marker authority reload",
            }),
            &sender,
            None,
        );
        assert_eq!(
            out["code"], "task_governing_decision_unresolved",
            "marker mutation must be caught before reviewer delivery: {out}"
        );
        assert!(
            crate::daemon::assignment_authority::active_branches(&home).is_empty(),
            "authority mutation must not persist a reviewer assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_review_assignment_rejects_cas_only_mutation_3168() {
        let home = seed_real_review_assignment_home("3168-concurrent");
        let hook_home = home.clone();
        let hook = Arc::new(move || {
            let mut changed =
                crate::daemon::pr_state::load(&hook_home, "owner/repo", "feat/x").unwrap();
            // Change only a field outside the identity re-check. This control
            // must exercise the in-flock snapshot CAS independently.
            changed.review_class = crate::daemon::pr_state::ReviewClass::Single;
            crate::daemon::pr_state::save(&hook_home, &changed).unwrap();
        });
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 42,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some(EXACT_HEAD.into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: Some(hook),
        }));

        let out = run_real_review_assignment(&home);

        assert_eq!(
            out["code"], "review_assignment_subject_unavailable",
            "{out}"
        );
        let state = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("concurrent state remains after rejection");
        assert_eq!(state.pr_number, 0);
        assert_eq!(state.head_sha, EXACT_HEAD);
        assert_eq!(
            state.review_class,
            crate::daemon::pr_state::ReviewClass::Single
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn real_review_assignment_rejects_identity_only_mismatch_3168() {
        let home = seed_real_review_assignment_home("3168-identity-only");
        let mut stale = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("seeded provisional state");
        // The provisional snapshot is stable, but its persisted identity does
        // not match the exact subject supplied by the review assignment.
        stale.head_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        crate::daemon::pr_state::save(&home, &stale).unwrap();
        let _scm = crate::scm::set_test_scm_provider(Arc::new(ProvisionalPrMock {
            summary: PrSummary {
                number: 42,
                head_ref: Some("feat/x".into()),
                head_ref_oid: Some(EXACT_HEAD.into()),
                author_login: Some("octocat".into()),
                ..Default::default()
            },
            before_return: None,
        }));

        let out = run_real_review_assignment(&home);

        assert_eq!(
            out["code"], "review_assignment_subject_unavailable",
            "{out}"
        );
        let state = crate::daemon::pr_state::load(&home, "owner/repo", "feat/x")
            .expect("identity mismatch must leave the provisional state unchanged");
        assert_eq!(state.pr_number, 0);
        assert_eq!(state.head_sha, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_none(),
            "identity mismatch must not persist an assignment"
        );
        std::fs::remove_dir_all(&home).ok();
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

    #[test]
    fn review_assignment_rejects_task_owned_by_non_target_without_side_effects() {
        let home = tmp_home("owner-mismatch");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject_with_owner(&home, "lead");
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
                "review_author": {"external": "octocat"}
            }),
            &sender,
            None,
        );
        assert_eq!(
            out["code"], "review_assignment_task_owner_mismatch",
            "review dispatch must reject an implementation-owned task: {out}"
        );
        assert!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .is_none(),
            "owner mismatch must not persist assignment authority"
        );
        assert!(
            crate::daemon::assignment_authority::active_branches(&home).is_empty(),
            "owner mismatch must not create a store branch"
        );
        assert_eq!(
            crate::task_events::replay(&home)
                .unwrap()
                .tasks
                .get(&crate::task_events::TaskId("t-rev-1".into()))
                .map(|task| task.status),
            Some(crate::task_events::TaskStatus::Open),
            "the existing implementation task remains untouched"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3309: a typed review_assignment dispatch whose target task is already
    /// TERMINAL (Done / Cancelled / Superseded) must be rejected atomically,
    /// BEFORE any assignment is minted or enqueued. The reconcile pass already
    /// encodes this invariant (assignment_reconcile.rs task-terminal gate,
    /// #2878-16: "a cancelled/done task cannot produce a valid review — retire
    /// the assignment instead of re-nudging it"); without the dispatch-side
    /// twin, the caller receives a live-looking assignment_id that reconcile
    /// retires on a later tick — a success-shaped response for an assignment
    /// that can never produce a review.
    #[test]
    fn review_assignment_rejects_terminal_task_atomically_3309() {
        let by = crate::task_events::InstanceName("system:test".into());
        let terminal_events: Vec<(&str, crate::task_events::TaskEvent)> = vec![
            (
                "cancelled",
                crate::task_events::TaskEvent::Cancelled {
                    task_id: crate::task_events::TaskId("t-rev-1".into()),
                    by: by.clone(),
                    reason: "head advanced after a REJECTED verdict".into(),
                },
            ),
            (
                "done",
                crate::task_events::TaskEvent::Done {
                    task_id: crate::task_events::TaskId("t-rev-1".into()),
                    by: by.clone(),
                    source: crate::task_events::DoneSource::OperatorManual {
                        authored_at: "2026-08-19T00:00:00Z".into(),
                        result: None,
                    },
                },
            ),
            (
                "superseded",
                crate::task_events::TaskEvent::Superseded {
                    task_id: crate::task_events::TaskId("t-rev-1".into()),
                    by: by.clone(),
                    successor_id: crate::task_events::TaskId("t-rev-2".into()),
                },
            ),
        ];
        for (label, terminal) in terminal_events {
            let home = tmp_home(&format!("terminal-{label}"));
            seed_fleet(
                &home,
                "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
            );
            // Task owned by the reviewer target, complete pr_state on disk: the
            // dispatch clears every existing gate, so the ONLY thing standing
            // between it and a minted assignment is the terminal-status check.
            seed_exact_subject_with_owner(&home, "reviewer");
            crate::task_events::append(&home, &by, terminal).unwrap();
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
                    "review_author": {"external": "octocat"}
                }),
                &sender,
                None,
            );
            assert_eq!(
                out["code"], "review_assignment_task_terminal",
                "a {label} task must reject the marker dispatch atomically: {out}"
            );
            assert!(
                out.get("assignment_id").is_none(),
                "no assignment_id may be returned against a {label} task: {out}"
            );
            assert!(
                crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                    .is_none(),
                "terminal-task rejection must not persist assignment authority ({label})"
            );
            assert!(
                crate::daemon::assignment_authority::active_branches(&home).is_empty(),
                "terminal-task rejection must not create a store branch ({label})"
            );
            std::fs::remove_dir_all(&home).ok();
        }
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

    /// The real durable store dispatch path replaces a reviewer assignment at the
    /// same repo/branch/target key. The predecessor's distinct task must be
    /// cancelled before the replacement overwrites its authority; the successor
    /// remains actionable. This is the RED reproduction for the system invariant.
    #[test]
    fn c11_double_dispatch_cancels_predecessor_only() {
        use super::super::review_assignment::dispatch_review_assignment_via_store;
        use super::super::ComposedDelegate;
        let home = tmp_home("c11-double-dispatch");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        seed_review_task(&home, "t-rev-2");
        let sender = Sender::new("lead").unwrap();
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));
        let composed = ComposedDelegate {
            msg: "[delegate_task] review the PR".to_string(),
            force_meta_json: None,
            second_reviewer: false,
            plan_ack_required: 0,
        };

        let first_args = marker_args("owner/repo", 42);
        dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &first_args,
            &checks,
            &composed,
            "owner/repo",
        );
        let mut successor_args = marker_args("owner/repo", 42);
        successor_args["task_id"] = Value::String("t-rev-2".into());
        dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &successor_args,
            &checks,
            &composed,
            "owner/repo",
        );

        let state = crate::task_events::replay(&home).unwrap();
        assert_eq!(
            state
                .tasks
                .get(&crate::task_events::TaskId("t-rev-1".into()))
                .map(|task| task.status),
            Some(crate::task_events::TaskStatus::Cancelled),
            "distinct predecessor task must be cancelled"
        );
        assert_eq!(
            state
                .tasks
                .get(&crate::task_events::TaskId("t-rev-2".into()))
                .map(|task| task.status),
            Some(crate::task_events::TaskStatus::Open),
            "successor task remains actionable"
        );
        assert_eq!(
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .map(|record| record.task_id),
            Some("t-rev-2".into()),
            "replacement authority belongs to the successor task"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn c11_double_dispatch_same_task_preserves_task() {
        use super::super::review_assignment::dispatch_review_assignment_via_store;
        use super::super::ComposedDelegate;
        let home = tmp_home("c11-double-same-task");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        seed_exact_subject(&home);
        let sender = Sender::new("lead").unwrap();
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));
        let composed = ComposedDelegate {
            msg: "[delegate_task] review the PR".to_string(),
            force_meta_json: None,
            second_reviewer: false,
            plan_ack_required: 0,
        };
        let args = marker_args("owner/repo", 42);

        dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &args,
            &checks,
            &composed,
            "owner/repo",
        );
        dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &args,
            &checks,
            &composed,
            "owner/repo",
        );

        assert_eq!(
            crate::task_events::replay(&home)
                .unwrap()
                .tasks
                .get(&crate::task_events::TaskId("t-rev-1".into()))
                .map(|task| task.status),
            Some(crate::task_events::TaskStatus::Open),
            "same-task replacement must preserve the owned task"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
