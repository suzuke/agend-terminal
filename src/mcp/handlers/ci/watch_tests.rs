use super::*;

/// #t-92758 P2: `ci unwatch` is the lead's dismiss path for a stuck ci-ready —
/// it must clear the caller's own ci-handoff track so the re-nudge watchdog
/// stops. Runs even when no watch file exists (the dismiss intent stands).
#[test]
fn unwatch_resolves_callers_ci_handoff_track() {
    let home = std::env::temp_dir().join(format!(
        "agend-92758-unwatch-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // A ci-ready obligation pointing at the caller, plus a co-subscriber's
    // track on the same branch that must survive (precise dismiss).
    crate::daemon::ci_handoff_track::record(
        &home,
        "lead",
        "o/r@b",
        "2026-06-10T00:00:00Z",
        None,
        None,
    );
    crate::daemon::ci_handoff_track::record(
        &home,
        "reviewer",
        "o/r@b",
        "2026-06-10T00:00:00Z",
        None,
        None,
    );

    let args = json!({"repository": "o/r", "branch": "b", "instance": "lead"});
    let _ = handle_unwatch_ci(&home, &args, "lead");

    let left = crate::daemon::ci_handoff_track::list(&home);
    assert_eq!(left.len(), 1, "only the caller's track is cleared");
    assert_eq!(
        left[0].1.target, "reviewer",
        "co-subscriber's track must survive unwatch dismiss"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #35896-11 ③: `ci action=status` must surface pending ci_handoff_track
/// sidecars (the renudge watchdog's source) so an agent can SEE why it's
/// renudged and what to discharge — pre-③ the sidecar was invisible even
/// when it drove a renudge (lead's 4.5h sample: empty `watches`, silent
/// renudge). Caller-scoped to the track TARGET: a named caller sees only its
/// OWN pending handoff; the anonymous CLI sees all. Crucially this holds with
/// NO ci-watches dir at all — the surface must not depend on a live watch.
#[test]
fn status_ci_surfaces_pending_handoffs_scoped_to_caller_35896_11() {
    let home = std::env::temp_dir().join(format!(
        "agend-35896-status-handoffs-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    // Two tracks on the same branch — the caller's + a co-subscriber's — and
    // deliberately NO ci-watches dir (the sample scenario: watch gone, track live).
    crate::daemon::ci_handoff_track::record(
        &home,
        "lead",
        "o/r@b",
        "2026-06-10T00:00:00Z",
        None,
        Some("t-42"),
    );
    crate::daemon::ci_handoff_track::record(
        &home,
        "reviewer",
        "o/r@b",
        "2026-06-10T00:00:00Z",
        None,
        None,
    );

    // Named caller sees ONLY its own pending handoff.
    let resp = handle_status_ci(&home, &json!({}), "lead");
    assert!(resp.get("error").is_none(), "status must not error: {resp}");
    assert!(
        resp["watches"].as_array().is_some_and(|w| w.is_empty()),
        "no ci-watches dir ⟹ empty watches, but the call must still render: {resp}"
    );
    let pending = resp["pending_handoffs"]
        .as_array()
        .expect("pending_handoffs must be present even with zero watches");
    assert_eq!(
        pending.len(),
        1,
        "caller sees only its OWN pending handoff, not the co-subscriber's: {resp}"
    );
    assert_eq!(pending[0]["target"], "lead");
    assert_eq!(pending[0]["correlation"], "o/r@b");
    assert_eq!(pending[0]["task_id"], "t-42");
    assert!(
        pending[0]["age_secs"].as_i64().is_some(),
        "age_secs must be derived from sent_at: {resp}"
    );

    // Anonymous CLI (empty instance) sees EVERY pending handoff.
    let resp_all = handle_status_ci(&home, &json!({}), "");
    assert_eq!(
        resp_all["pending_handoffs"].as_array().unwrap().len(),
        2,
        "anonymous CLI sees every pending handoff: {resp_all}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn status_message_id_matches_by_target_correlation_episode() {
    let home = std::env::temp_dir().join(format!(
        "agend-status-msgid-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let ep = "ep-match-test";
    let corr = "org/repo@feat/x";
    crate::daemon::ci_handoff_track::record_with_identity(
        &home,
        "lead",
        corr,
        "2026-07-18T00:00:00Z",
        None,
        Some("t-99"),
        Some(ep),
        None,
    );

    crate::inbox::enqueue(
        &home,
        "lead",
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-exact-match".into()),
            from: "system:ci".into(),
            text: "[ci-ready-for-action]".into(),
            kind: Some("ci-ready-for-action".into()),
            correlation_id: Some(corr.into()),
            ci_handoff_episode: Some(ep.into()),
            timestamp: "2026-07-18T00:00:00Z".into(),
            ..Default::default()
        },
    )
    .unwrap();

    let resp = handle_status_ci(&home, &json!({}), "lead");
    let pending = resp["pending_handoffs"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0]["message_id"], "m-exact-match",
        "message_id must match by target+correlation+episode: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn status_message_id_rejects_wrong_correlation() {
    let home = std::env::temp_dir().join(format!(
        "agend-status-msgid-wrongcorr-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();

    let ep = "ep-wrong-corr";
    crate::daemon::ci_handoff_track::record_with_identity(
        &home,
        "lead",
        "org/repo@feat/x",
        "2026-07-18T00:00:00Z",
        None,
        Some("t-99"),
        Some(ep),
        None,
    );

    // Same episode but DIFFERENT correlation — must NOT match.
    crate::inbox::enqueue(
        &home,
        "lead",
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-wrong-corr".into()),
            from: "system:ci".into(),
            text: "[ci-ready-for-action]".into(),
            kind: Some("ci-ready-for-action".into()),
            correlation_id: Some("org/repo@feat/WRONG".into()),
            ci_handoff_episode: Some(ep.into()),
            timestamp: "2026-07-18T00:00:00Z".into(),
            ..Default::default()
        },
    )
    .unwrap();

    let resp = handle_status_ci(&home, &json!({}), "lead");
    let pending = resp["pending_handoffs"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]["message_id"].is_null(),
        "same episode + different correlation must NOT match: {resp}"
    );
    std::fs::remove_dir_all(&home).ok();
}
