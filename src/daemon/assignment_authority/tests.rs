    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("agend-asgn-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mk_record(
        repo: &str,
        branch: &str,
        target: &str,
        pr: u64,
        created_at: &str,
    ) -> ActiveAssignment {
        ActiveAssignment::new_pending(
            repo,
            branch,
            target,
            pr,
            "lead",
            "t-orig-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            Some("thr-1".into()),
            Some("par-1".into()),
            created_at,
        )
    }

    /// Read all inbox rows for `target` (any state), for row-level assertions.
    fn inbox_rows(home: &Path, target: &str) -> Vec<crate::inbox::InboxMessage> {
        let path = crate::inbox::storage::inbox_path_resolved(home, target);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<crate::inbox::InboxMessage>(l).ok())
            .collect()
    }

    fn rows_with_nonce(home: &Path, target: &str, nonce: &str) -> Vec<crate::inbox::InboxMessage> {
        inbox_rows(home, target)
            .into_iter()
            .filter(|m| m.delivery_nonce.as_deref() == Some(nonce))
            .collect()
    }

    /// Simulate the reviewer having READ the row carrying `nonce` (set `read_at`).
    fn mark_row_read(home: &Path, target: &str, nonce: &str, ts: &str) {
        let path = crate::inbox::storage::inbox_path_resolved(home, target);
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut msg: crate::inbox::InboxMessage = serde_json::from_str(line).unwrap();
            if msg.delivery_nonce.as_deref() == Some(nonce) {
                msg.read_at = Some(ts.to_string());
            }
            out.push_str(&serde_json::to_string(&msg).unwrap());
            out.push('\n');
        }
        std::fs::write(&path, out).unwrap();
    }

    /// T11: a full-payload record round-trips through persist→get, and
    /// durable_enqueue REBUILDS the delivery message purely from the stored record
    /// (no bind/create) — every payload field is carried onto the inbox row.
    #[test]
    fn t11_record_roundtrip_and_message_rebuild() {
        let home = tmp_home("t11");
        let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();

        let got = get(&home, "o/r", "feat/x", "reviewer").expect("record persisted");
        assert_eq!(got.assignment_id, rec.assignment_id);
        assert_eq!(got.pr_number, 42);
        assert_eq!(got.sender, "lead");
        assert_eq!(got.task_id, "t-orig-1");
        assert_eq!(got.review_class, ReviewClass::Dual);
        assert_eq!(got.review_author, ReviewAuthor::External("octocat".into()));
        assert_eq!(got.text, "Please review PR");
        assert_eq!(got.thread_id.as_deref(), Some("thr-1"));
        assert_eq!(got.parent_id.as_deref(), Some("par-1"));
        assert_eq!(got.delivery_nonce, rec.delivery_nonce);
        assert_eq!(got.row, RowState::Pending);

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        let rows = rows_with_nonce(&home, "reviewer", &rec.delivery_nonce);
        assert_eq!(rows.len(), 1, "exactly one row carries the nonce");
        let row = &rows[0];
        // Rebuilt PURELY from the record's fields.
        assert_eq!(row.text, "Please review PR");
        assert_eq!(row.pr_number, Some(42));
        assert_eq!(row.thread_id.as_deref(), Some("thr-1"));
        assert_eq!(row.parent_id.as_deref(), Some("par-1"));
        assert_eq!(row.task_id.as_deref(), Some("t-orig-1"));
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &rec.delivery_nonce),
            "the rebuilt row is actionable"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A1 defense-in-depth: persisting a zero `pr_number` is a caller error and
    /// writes NOTHING (no unbound generation can exist — B18/B19).
    #[test]
    fn persist_rejects_zero_pr_number() {
        let home = tmp_home("zero-pr");
        let rec = mk_record("o/r", "feat/x", "reviewer", 0, "2026-07-13T00:00:00Z");
        assert!(persist(&home, &rec).is_err(), "zero pr_number rejected");
        assert!(
            get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "no record written on rejection"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A legacy schema row cannot look receipt-capable merely because partial
    /// rollout fields happen to be present. It must be re-dispatched before the
    /// reviewer receives a typed assignment envelope.
    #[test]
    fn legacy_schema_with_typed_fields_does_not_emit_assignment_envelope_2760() {
        let mut record = mk_record("o/r", "feat/legacy", "reviewer", 7, "2026-07-13T00:00:00Z");
        record.target_instance_id = Some(crate::types::InstanceId::new());
        record.reviewed_head = Some("a".repeat(40));
        record.review_slot = Some(crate::review_receipt::ReviewSlot::Primary);

        assert!(build_delivery_message(&record, "2026-07-13T00:00:01Z")
            .review_assignment
            .is_none());

        record.schema_version = SCHEMA_VERSION;
        assert!(build_delivery_message(&record, "2026-07-13T00:00:01Z")
            .review_assignment
            .is_some());
    }

    /// T12: crash BEFORE enqueue — `row = Pending`, nonce NOT in inbox. Recovery
    /// enqueues a row carrying the SAME nonce and marks Persisted.
    #[test]
    fn t12_crash_before_enqueue_reenqueues_same_nonce() {
        let home = tmp_home("t12");
        let rec = mk_record("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        assert!(
            !crate::inbox::storage::nonce_present_actionable(
                &home,
                "reviewer",
                &rec.delivery_nonce
            ),
            "no row before enqueue (crash window)"
        );

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1,
            "recovery enqueued exactly one row with the same nonce"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T13: enqueue succeeded but the row→Persisted mark crashed — the nonce IS
    /// actionable in the inbox, `row` still `Pending`. Recovery detects the
    /// nonce-hit and marks Persisted with NO duplicate row.
    #[test]
    fn t13_enqueue_ok_mark_fail_nonce_hit_no_duplicate() {
        let home = tmp_home("t13");
        let rec = mk_record("o/r", "feat/x", "reviewer", 9, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        // Simulate "enqueue succeeded, mark crashed": a row already carries the nonce.
        crate::inbox::storage::enqueue(
            &home,
            "reviewer",
            build_delivery_message(&rec, "2026-07-13T00:00:01Z"),
        )
        .unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1
        );

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1,
            "nonce-hit ⇒ NO duplicate row appended"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted,
            "row marked Persisted on the nonce-hit"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T26 (B9 ABA): `assignment_id` is the CAS identity. A record revoked (id X)
    /// then recreated at the SAME key with a NEW id (Y) must reject a stale op
    /// carrying X — it does NOT mutate Y.
    #[test]
    fn t26_aba_assignment_id_is_cas_identity() {
        let home = tmp_home("t26");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 5, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        let x = get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .assignment_id;

        // Revoke (removes id X), then recreate the SAME key with a fresh id Y.
        revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap();
        let r2 = mk_record("o/r", "feat/x", "reviewer", 5, "2026-07-13T00:01:00Z");
        persist(&home, &r2).unwrap();
        let y = get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .assignment_id;
        assert_ne!(x, y, "recreated record has a fresh assignment_id");

        // A STALE op carrying X must be rejected (does not touch Y).
        let path = record_file(&home, "o/r", "feat/x", "reviewer");
        assert!(
            !remove_if_assignment_matches(&path, x),
            "stale assignment_id X must NOT mutate the recreated record"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .assignment_id,
            y,
            "record with id Y survives the stale X op"
        );
        // The current id Y still CASes successfully.
        assert!(remove_if_assignment_matches(&path, y));
        assert!(get(&home, "o/r", "feat/x", "reviewer").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    /// T32 (B7/I21): revoke supersedes the current inbox row by its nonce
    /// (is_actionable ⇒ false). Case A (unread): no notice. Case B (already read):
    /// a durable revocation notice is enqueued.
    #[test]
    fn t32_revoke_supersedes_row_and_notice_only_if_read() {
        // Case A: unread row → superseded, NO revocation notice.
        let home = tmp_home("t32a");
        let rec = mk_record("o/r", "feat/x", "reviewer", 11, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert!(crate::inbox::storage::nonce_present_actionable(
            &home,
            "reviewer",
            &rec.delivery_nonce
        ));
        assert!(revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap());
        assert!(
            get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "record removed"
        );
        assert!(
            !crate::inbox::storage::nonce_present_actionable(
                &home,
                "reviewer",
                &rec.delivery_nonce
            ),
            "row superseded ⇒ not actionable"
        );
        assert_eq!(
            inbox_rows(&home, "reviewer")
                .iter()
                .filter(|m| m.kind.as_deref() == Some("review-assignment-revoked"))
                .count(),
            0,
            "an unread revoke needs no revocation notice"
        );
        std::fs::remove_dir_all(&home).ok();

        // Case B: row already READ → superseded AND a revocation notice enqueued.
        let home = tmp_home("t32b");
        let rec = mk_record("o/r", "feat/x", "reviewer", 11, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        mark_row_read(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            "2026-07-13T00:00:07Z",
        );
        assert!(revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap());
        assert_eq!(
            inbox_rows(&home, "reviewer")
                .iter()
                .filter(|m| m.kind.as_deref() == Some("review-assignment-revoked"))
                .count(),
            1,
            "a revoke of an already-read assignment enqueues a revocation notice"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T33: transfer old→new is atomic — old record removed + its row superseded,
    /// the new record has a FRESH assignment_id + nonce (same pr_number), and a
    /// co-target on the same branch is UNTOUCHED.
    #[test]
    fn t33_transfer_atomic_other_targets_untouched() {
        let home = tmp_home("t33");
        let a = mk_record("o/r", "feat/x", "rev-a", 21, "2026-07-13T00:00:00Z");
        let c = mk_record("o/r", "feat/x", "rev-c", 21, "2026-07-13T00:00:00Z");
        persist(&home, &a).unwrap();
        persist(&home, &c).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "rev-a", "2026-07-13T00:00:05Z").unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "rev-c", "2026-07-13T00:00:05Z").unwrap();

        transfer(
            &home,
            "o/r",
            "feat/x",
            "rev-a",
            "rev-b",
            "2026-07-13T00:00:10Z",
        )
        .unwrap();

        // Old gone + its row superseded.
        assert!(
            get(&home, "o/r", "feat/x", "rev-a").is_none(),
            "old removed"
        );
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "rev-a", &a.delivery_nonce),
            "old row superseded"
        );
        // New present, fresh identity, same pr_number.
        let b = get(&home, "o/r", "feat/x", "rev-b").expect("new target persisted");
        assert_eq!(b.pr_number, 21, "same pr_number carried over");
        assert_ne!(b.assignment_id, a.assignment_id, "fresh assignment_id");
        assert_ne!(b.delivery_nonce, a.delivery_nonce, "fresh nonce");
        assert_eq!(b.row, RowState::Pending, "new record starts Pending");
        // Co-target untouched.
        let c_after = get(&home, "o/r", "feat/x", "rev-c").expect("co-target survives");
        assert_eq!(c_after.assignment_id, c.assignment_id);
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "rev-c", &c.delivery_nonce),
            "co-target's row still actionable"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B18/B19/I18: a terminal marker tombstones ONLY records whose stored
    /// pr_number matches; a DIFFERENT-generation record on the same branch
    /// SURVIVES (no force-bind by branch, no unbound window).
    #[test]
    fn terminal_tombstones_only_matching_pr_number() {
        let home = tmp_home("terminal-pr");
        let g = mk_record("o/r", "feat/x", "rev-g", 30, "2026-07-13T00:00:00Z");
        let gp = mk_record("o/r", "feat/x", "rev-gp", 31, "2026-07-13T00:00:00Z");
        persist(&home, &g).unwrap();
        persist(&home, &gp).unwrap();

        let tombstoned = record_terminal(&home, "o/r", "feat/x", 30, TerminalKind::Merged).unwrap();
        assert_eq!(tombstoned, 1, "only the pr_number==30 record is tombstoned");
        assert!(
            get(&home, "o/r", "feat/x", "rev-g").is_none(),
            "the terminal generation's record is removed"
        );
        let survivor = get(&home, "o/r", "feat/x", "rev-gp").expect("different pr survives");
        assert_eq!(survivor.pr_number, 31);
        assert!(
            terminal_markers(&home, "o/r", "feat/x").contains(30),
            "the terminal pr_number is recorded in the retained marker set"
        );
        assert!(
            !terminal_markers(&home, "o/r", "feat/x").contains(31),
            "the surviving generation is NOT marked terminal"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B20/I19: terminal markers are RETAINED (no compaction/GC). After many
    /// generations terminate, EVERY marker is still present; re-recording the same
    /// pr_number does not duplicate it.
    #[test]
    fn markers_retained_no_compaction() {
        let home = tmp_home("markers-retained");
        for pr in 100u64..110 {
            record_terminal(&home, "o/r", "feat/x", pr, TerminalKind::Closed).unwrap();
        }
        let markers = terminal_markers(&home, "o/r", "feat/x");
        for pr in 100u64..110 {
            assert!(markers.contains(pr), "marker {pr} retained (no compaction)");
        }
        assert_eq!(markers.markers.len(), 10, "all ten generations retained");

        // Re-recording an existing terminal pr_number does not duplicate the marker.
        record_terminal(&home, "o/r", "feat/x", 105, TerminalKind::Closed).unwrap();
        assert_eq!(
            terminal_markers(&home, "o/r", "feat/x").markers.len(),
            10,
            "re-record is idempotent (no duplicate marker)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A4/I12: append-only repair rotates the nonce, supersedes the OLD row, keeps
    /// the new row actionable, and respects the FIXED-interval bound — a repair
    /// before `next_nudge_at` is a no-op; two repairs inside one interval fire at
    /// most once. `read_at` is never reset.
    ///
    /// #2914: repair only fires when the row is genuinely missing — NOT when
    /// merely read. A present row (read or unread) is healthy; a missing row
    /// triggers repair.
    #[test]
    fn repair_append_only_fixed_interval() {
        let home = tmp_home("repair");
        let rec = mk_record("o/r", "feat/x", "reviewer", 55, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n1 = rec.delivery_nonce.clone();

        // Before next_nudge_at (== created_at) → NOT eligible.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-12T23:59:59Z").unwrap(),
            "repair before the lease is a no-op (bounded)"
        );

        // Row still present (unread) → no repair even after the lease.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:01Z").unwrap(),
            "present (unread) row is healthy — no repair"
        );

        // Mark read — row is still present and not superseded → still no repair (#2914).
        mark_row_read(&home, "reviewer", &n1, "2026-07-13T00:00:05Z");
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:01:06Z").unwrap(),
            "#2914: read-but-present row is healthy — no repair"
        );

        // Wipe inbox (simulating a truly lost message) → repair fires.
        let inbox_path = crate::inbox::storage::inbox_path_resolved(&home, "reviewer");
        std::fs::remove_file(&inbox_path).ok();
        let cur = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        let repair_time = add_interval(&cur.next_nudge_at);
        assert!(
            repair_row(&home, "o/r", "feat/x", "reviewer", &repair_time).unwrap(),
            "missing row triggers repair"
        );
        let after = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        let n2 = after.delivery_nonce.clone();
        assert_ne!(n1, n2, "repair rotates the nonce");
        // NEW row is actionable.
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &n2),
            "fresh row carries the new nonce and is actionable"
        );

        // A second repair within the same interval must NOT fire.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", &repair_time).unwrap(),
            "≤ 1 repair per FIXED_INTERVAL"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n2,
            "no rotation on the throttled second repair"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2914: a reviewer who has READ the assignment but not yet submitted a
    /// verdict must NOT be re-nudged every 60s. The read row is still present
    /// and not superseded — the reviewer is actively working. Only truly
    /// missing rows should trigger repair.
    #[test]
    fn read_assignment_does_not_trigger_renudge() {
        let home = tmp_home("2914");
        let rec = mk_record("o/r", "feat/x", "reviewer", 99, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n1 = rec.delivery_nonce.clone();

        // Reviewer reads the assignment — actively working on the review.
        mark_row_read(&home, "reviewer", &n1, "2026-07-13T00:00:05Z");

        // After the 60s lease, repair_row must NOT re-nudge a read-but-present row.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:01:01Z").unwrap(),
            "#2914: read-but-present row must not trigger re-nudge"
        );
        // No new nonce rotation — the original nonce is still current.
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n1,
            "nonce must not rotate on a read-but-present row"
        );
        // Inbox must have exactly 1 row (the original), no duplicate.
        assert_eq!(
            inbox_rows(&home, "reviewer").len(),
            1,
            "no duplicate delivery for a read-but-present assignment"
        );

        // Positive control: a MISSING row (inbox wiped) must still trigger repair.
        let inbox_path = crate::inbox::storage::inbox_path_resolved(&home, "reviewer");
        std::fs::remove_file(&inbox_path).ok();
        let cur = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        let repair_time = add_interval(&cur.next_nudge_at);
        assert!(
            repair_row(&home, "o/r", "feat/x", "reviewer", &repair_time).unwrap(),
            "missing row must still trigger repair"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────────── C7: 4-state evidence classifier ───────────────────

    use crate::daemon::pr_state::{PrState, VerdictState};

    fn typed_record(head: &str) -> ActiveAssignment {
        let mut record = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        record.schema_version = SCHEMA_VERSION;
        record.target_instance_id = Some(crate::types::InstanceId::new());
        record.reviewed_head = Some(head.to_string());
        record.review_slot = Some(crate::review_receipt::ReviewSlot::Primary);
        record
    }

    fn prstate_with_receipt(
        record: &ActiveAssignment,
        head: &str,
        verdict: Option<crate::review_receipt::ReviewVerdict>,
    ) -> PrState {
        let mut ps =
            crate::daemon::pr_state::new_for_branch("o/r", "feat/x", head, ReviewClass::Dual);
        ps.pr_number = 42;
        if let Some(verdict) = verdict {
            ps.validated_review_receipts
                .push(crate::review_receipt::ReviewReceiptSummary {
                    receipt_id: "review-receipt:m-test".into(),
                    source_id: "m-test".into(),
                    evidence_digest: "a".repeat(64),
                    assignment_id: record.assignment_id,
                    reviewer_instance_id: record.target_instance_id.unwrap(),
                    reviewer_name: record.target.clone(),
                    repo: record.repo.clone(),
                    pr_number: record.pr_number,
                    branch: record.branch.clone(),
                    task_id: record.task_id.clone(),
                    reviewed_head: record.reviewed_head.clone().unwrap(),
                    review_class: record.review_class,
                    slot: record.review_slot.unwrap(),
                    verdict,
                });
        }
        ps
    }

    /// task66: only exact typed receipt evidence classifies an assignment. The
    /// collapsed legacy VerdictState and generic correlated ACK remain inert.
    #[test]
    fn c7_classify_only_typed_exact_receipt_evidence() {
        let head = "a".repeat(40);
        let rec = typed_record(&head);

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Verified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::SatisfiedExactHead,
            "exact typed VERIFIED receipt satisfies its assignment"
        );

        let mut wrong_identity = ps.clone();
        wrong_identity.validated_review_receipts[0].reviewer_instance_id =
            crate::types::InstanceId::new();
        assert_eq!(
            classify_assignment(&rec, Some(&wrong_identity)),
            AssignmentEvidence::Unengaged,
            "another stable reviewer identity cannot satisfy the assignment"
        );

        let advanced = "b".repeat(40);
        let ps = prstate_with_receipt(
            &rec,
            &advanced,
            Some(crate::review_receipt::ReviewVerdict::Verified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::Unengaged,
            "a stale typed receipt does not satisfy after head advance"
        );

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Rejected),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "exact typed REJECTED receipt is engaged-unsatisfied"
        );

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Unverified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "exact typed UNVERIFIED receipt is engaged-unsatisfied"
        );

        let mut legacy = prstate_with_receipt(&rec, &head, None);
        legacy.verdict_state = VerdictState::Verified {
            reviewers: vec![("reviewer".into(), head.clone())],
        };
        assert_eq!(
            classify_assignment(&rec, Some(&legacy)),
            AssignmentEvidence::Unengaged,
            "legacy collapsed verdict state is display-only"
        );

        let mut acked = rec.clone();
        acked.acked_at = Some("2026-07-13T00:00:05Z".into());
        assert_eq!(
            classify_assignment(&acked, Some(&prstate_with_receipt(&acked, &head, None))),
            AssignmentEvidence::Unengaged,
            "generic correlated ACK is not code-review evidence"
        );

        assert_eq!(
            classify_assignment(&rec, None),
            AssignmentEvidence::Unengaged,
            "no PR state means no receipt evidence"
        );
    }

    // ─────────────────── C9: authenticated correlated ACK ───────────────────

    /// C9 (T23): exactly-one match ⇒ `acked_at`/`acked_by` set as a SINGLE atomic
    /// write that PERSISTS across a re-read; a re-ack is an idempotent no-op success
    /// (never overwrites the original timestamp); a non-matching (target,task_id) is
    /// a no-op.
    #[test]
    fn c9_ack_exactly_one_idempotent_atomic() {
        let home = tmp_home("c9-ack");
        // mk_record ⇒ target "reviewer", task_id "t-orig-1".
        let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();

        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:05Z"),
            AckOutcome::Acked,
            "exactly one (target,task_id) match ⇒ Acked"
        );
        let got = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        assert_eq!(got.acked_at.as_deref(), Some("2026-07-13T00:00:05Z"));
        assert_eq!(got.acked_by.as_deref(), Some("reviewer"));
        // Legacy ACK persists for backwards-compatible audit display, but it no
        // longer counts as code-review evidence.
        assert_eq!(
            classify_assignment(&got, None),
            AssignmentEvidence::Unengaged,
            "acked record remains unengaged without a typed receipt"
        );

        // Re-ack is an idempotent success — the original timestamp is NOT overwritten.
        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T01:00:00Z"),
            AckOutcome::Acked,
            "re-ack is an idempotent success"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .acked_at
                .as_deref(),
            Some("2026-07-13T00:00:05Z"),
            "re-ack must NOT overwrite the original acked_at"
        );

        // Non-matching sender or task_id ⇒ no-op.
        assert_eq!(
            ack(&home, "someone-else", "t-orig-1", "2026-07-13T02:00:00Z"),
            AckOutcome::NoMatch,
            "a non-target sender never acks"
        );
        assert_eq!(
            ack(&home, "reviewer", "t-nope", "2026-07-13T02:00:00Z"),
            AckOutcome::NoMatch,
            "a non-matching task_id never acks"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C9 (T15 fail-closed): TWO active assignments with the SAME (target,task_id)
    /// on DIFFERENT branches ⇒ ack is AMBIGUOUS and sets NEITHER. Once the ambiguity
    /// is resolved (one revoked) the remaining single match acks.
    #[test]
    fn c9_ack_ambiguity_fails_closed() {
        let home = tmp_home("c9-ambig");
        let a = mk_record("o/r", "feat/a", "reviewer", 42, "2026-07-13T00:00:00Z");
        let b = mk_record("o/r", "feat/b", "reviewer", 43, "2026-07-13T00:00:00Z");
        persist(&home, &a).unwrap();
        persist(&home, &b).unwrap();

        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:05Z"),
            AckOutcome::Ambiguous,
            ">1 (target,task_id) match ⇒ FAIL CLOSED"
        );
        assert!(
            get(&home, "o/r", "feat/a", "reviewer")
                .unwrap()
                .acked_at
                .is_none(),
            "ambiguous ack sets NOTHING on branch a"
        );
        assert!(
            get(&home, "o/r", "feat/b", "reviewer")
                .unwrap()
                .acked_at
                .is_none(),
            "ambiguous ack sets NOTHING on branch b"
        );

        // Resolve the ambiguity: revoke one ⇒ the remaining single match acks.
        revoke(&home, "o/r", "feat/b", "reviewer", "2026-07-13T00:00:06Z").unwrap();
        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:07Z"),
            AckOutcome::Acked,
            "once unambiguous, the single match acks"
        );
        assert_eq!(
            get(&home, "o/r", "feat/a", "reviewer")
                .unwrap()
                .acked_at
                .as_deref(),
            Some("2026-07-13T00:00:07Z"),
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// task66/RED 11: the receipt-generation lookup is strict across the active
    /// store. Missing IDs, duplicate generations, and even an unrelated corrupt
    /// co-resident row all fail closed instead of degrading to "not found".
    #[test]
    fn strict_assignment_id_lookup_rejects_missing_duplicate_and_corrupt_2760() {
        let missing_home = tmp_home("strict-id-missing");
        assert!(lookup_by_assignment_id_strict(&missing_home, uuid::Uuid::new_v4()).is_err());

        let duplicate_home = tmp_home("strict-id-duplicate");
        let a = mk_record("o/r", "feat/a", "reviewer-a", 42, "2026-07-14T00:00:00Z");
        let mut b = mk_record("o/r", "feat/b", "reviewer-b", 43, "2026-07-14T00:00:00Z");
        b.assignment_id = a.assignment_id;
        persist(&duplicate_home, &a).unwrap();
        persist(&duplicate_home, &b).unwrap();
        let duplicate = lookup_by_assignment_id_strict(&duplicate_home, a.assignment_id)
            .expect_err("duplicate generation must reject");
        assert!(duplicate.to_string().contains("ambiguous"));

        let corrupt_home = tmp_home("strict-id-corrupt");
        let valid = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-14T00:00:00Z");
        persist(&corrupt_home, &valid).unwrap();
        std::fs::write(
            record_file(&corrupt_home, "o/r", "feat/x", "corrupt-row"),
            b"{not-json",
        )
        .unwrap();
        let corrupt = lookup_by_assignment_id_strict(&corrupt_home, valid.assignment_id)
            .expect_err("corrupt store must reject even when the target row parses");
        assert!(corrupt.to_string().contains("corrupt assignment record"));

        std::fs::remove_dir_all(missing_home).ok();
        std::fs::remove_dir_all(duplicate_home).ok();
        std::fs::remove_dir_all(corrupt_home).ok();
    }

    #[test]
    fn cutover_census_enumerates_only_active_receipt_incapable_rows_2760() {
        let home = tmp_home("legacy-census");
        let legacy = mk_record(
            "o/r",
            "feat/legacy",
            "reviewer-a",
            41,
            "2026-07-14T00:00:00Z",
        );
        let mut partial = mk_record(
            "o/r",
            "feat/partial",
            "reviewer-b",
            42,
            "2026-07-14T00:00:00Z",
        );
        partial.schema_version = SCHEMA_VERSION;
        partial.target_instance_id = Some(crate::types::InstanceId::new());
        let typed = ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/typed",
            "reviewer-c",
            crate::types::InstanceId::new(),
            43,
            "a".repeat(40),
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-typed",
            ReviewClass::Single,
            ReviewAuthor::External("octocat".into()),
            "review",
            None,
            None,
            "2026-07-14T00:00:00Z",
        );
        persist(&home, &legacy).unwrap();
        persist(&home, &partial).unwrap();
        persist(&home, &typed).unwrap();

        let rows = legacy_active_assignments_strict(&home).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.assignment_id).collect::<Vec<_>>(),
            vec![legacy.assignment_id, partial.assignment_id],
            "schema-1 and partially upgraded rows require audited re-dispatch; an exact typed row does not"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// B1 — persisting a NEW record (different `assignment_id`) at a key that ALREADY
    /// holds a record must RETIRE the prior record's actionable inbox row under the
    /// same branch lock (atomic revoke-and-replace). Otherwise the old actionable
    /// `delivery_nonce` row is ORPHANED forever — a stale actionable assignment the
    /// reviewer can still act on after it was superseded.
    #[test]
    fn b1_persist_supersedes_orphaned_prior_row() {
        let home = tmp_home("b1");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "R1's row is actionable after dispatch"
        );

        // A NEW dispatch at the SAME (repo,branch,target) with a DIFFERENT id.
        let r2 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:01:00Z");
        assert_ne!(
            r1.assignment_id, r2.assignment_id,
            "R2 is a distinct record"
        );
        persist(&home, &r2).unwrap();

        // R1's orphaned actionable row MUST be retired (superseded ⇒ not actionable).
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "R1's prior actionable row is superseded on re-persist (not orphaned)"
        );
        // R2 is the stored record.
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .assignment_id,
            r2.assignment_id,
            "R2 is the stored authority record"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B1 — re-persisting the SAME record (same `assignment_id`) is idempotent and
    /// must NOT retire its own still-actionable row.
    #[test]
    fn b1_persist_same_id_is_idempotent_no_supersede() {
        let home = tmp_home("b1-idem");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        // Re-persist the identical record (same assignment_id + nonce).
        persist(&home, &r1).unwrap();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "same-id re-persist keeps its own actionable row (idempotent, no self-supersede)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B4(b) — a CORRUPT markers file must NOT be silently read as an EMPTY
    /// (zero-terminals) set. `record_terminal` reads the RETAINED marker set to
    /// append to; if a corrupt file read as default-empty, the subsequent write
    /// would OVERWRITE the retained set with just the new marker, LOSING every
    /// prior terminal generation (old-generation replays would then no longer die
    /// — B20). Fail closed: `record_terminal` must surface the corruption (Err) and
    /// NOT overwrite. Pre-fix: `read_markers` `.unwrap_or_default()` empties on
    /// corrupt, so `record_terminal` returns `Ok` and clobbers the retained set.
    #[test]
    fn b4_corrupt_markers_fails_closed_not_silent_empty() {
        let home = tmp_home("b4-markers");
        // Retain two terminal generations first.
        record_terminal(&home, "o/r", "feat/x", 100, TerminalKind::Merged).unwrap();
        record_terminal(&home, "o/r", "feat/x", 101, TerminalKind::Closed).unwrap();
        // Corrupt the markers file on disk.
        let mpath = markers_file(&home, "o/r", "feat/x");
        std::fs::write(&mpath, b"{ this is not valid markers json").unwrap();

        // A subsequent record_terminal must FAIL CLOSED (not silently treat the
        // corrupt file as zero terminals and overwrite the retained set).
        let res = record_terminal(&home, "o/r", "feat/x", 102, TerminalKind::Merged);
        assert!(
            res.is_err(),
            "record_terminal must fail closed on a corrupt markers file, not overwrite the retained set"
        );
        std::fs::remove_dir_all(&home).ok();
    }
