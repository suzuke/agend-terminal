use super::*;

fn seed_handoff_row_with_class(
    home: &Path,
    target: &str,
    correlation: &str,
    episode: &str,
    id: &str,
    class: crate::inbox::CiHandoffClass,
) {
    let mut msg = crate::inbox::InboxMessage::new_system(
        "system:ci",
        "ci-ready-for-action",
        format!("[ci-ready-for-action] {correlation}"),
    )
    .with_correlation_id(correlation.to_string());
    msg.id = Some(id.to_string());
    msg.ci_handoff_episode = Some(episode.to_string());
    msg.ci_handoff_class = Some(class);
    crate::inbox::enqueue(home, target, msg).unwrap();
}

fn seed_handoff_row(home: &Path, target: &str, correlation: &str, episode: &str, id: &str) {
    seed_handoff_row_with_class(
        home,
        target,
        correlation,
        episode,
        id,
        crate::inbox::CiHandoffClass::Feature,
    );
}

fn ack_row(home: &Path, target: &str, episode: &str) -> crate::inbox::InboxMessage {
    let content = std::fs::read_to_string(crate::inbox::inbox_path_resolved(home, target)).unwrap();
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<crate::inbox::InboxMessage>(line).ok())
        .find(|msg| msg.ci_handoff_episode.as_deref() == Some(episode))
        .expect("seeded ci-ready row")
}

#[test]
fn ack_handoff_settles_feature_row_after_track_resolved_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-ack-resolved-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("ci-watches")).unwrap();
    let watch = home.join("ci-watches").join("feature.json");
    std::fs::write(&watch, b"watch-must-survive").unwrap();
    let corr = "o/r@feat/resolved";
    let episode = "ep-resolved";
    seed_handoff_row(&home, "lead", corr, episode, "m-3179-resolved");

    // The Draft/REJECTED resolver has already removed the sidecar track.
    assert!(crate::daemon::ci_handoff_track::list(&home).is_empty());

    let response = handle_ack_handoff_ci(
        &home,
        &json!({"repository": "o/r", "branch": "feat/resolved", "episode": episode}),
        "lead",
    );
    assert_eq!(
        response["ok"], true,
        "exact row ACK must succeed: {response}"
    );
    assert_eq!(response["track_already_resolved"], true, "{response}");
    assert_eq!(response["watch_preserved"], true, "{response}");
    assert_eq!(
        crate::inbox::storage::handoff_row_state(
            &home,
            "lead",
            corr,
            episode,
            crate::inbox::CiHandoffClass::Feature,
        ),
        crate::inbox::storage::ProtectedHandoffRowState::ExplicitlyAcked
    );
    assert_eq!(std::fs::read(&watch).unwrap(), b"watch-must-survive");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ack_handoff_settles_processed_feature_row_after_track_resolved_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-ack-processed-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let corr = "o/r@feat/processed";
    let episode = "ep-processed";
    seed_handoff_row(&home, "lead", corr, episode, "m-3179-processed");
    let _ = crate::inbox::drain(&home, "lead");
    let _ = crate::inbox::drain(&home, "lead");
    assert!(ack_row(&home, "lead", episode).read_at.is_some());
    assert!(crate::daemon::ci_handoff_track::list(&home).is_empty());

    let response = handle_ack_handoff_ci(
        &home,
        &json!({"repository": "o/r", "branch": "feat/processed", "episode": episode}),
        "lead",
    );
    assert_eq!(
        response["ok"], true,
        "processed row ACK must succeed: {response}"
    );
    assert_eq!(response["track_already_resolved"], true, "{response}");
    assert_eq!(
        response["already_acked"], false,
        "generic processing is not explicit ACK: {response}"
    );
    assert_eq!(
        crate::inbox::storage::handoff_row_state(
            &home,
            "lead",
            corr,
            episode,
            crate::inbox::CiHandoffClass::Feature,
        ),
        crate::inbox::storage::ProtectedHandoffRowState::ExplicitlyAcked
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ack_handoff_missing_or_ambiguous_resolved_feature_row_fails_closed_3179() {
    let missing_home = std::env::temp_dir().join(format!(
        "agend-3179-ack-missing-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&missing_home);
    std::fs::create_dir_all(&missing_home).unwrap();
    let missing = handle_ack_handoff_ci(
        &missing_home,
        &json!({"repository": "o/r", "branch": "feat/missing", "episode": "ep-missing"}),
        "lead",
    );
    assert_eq!(
        missing["code"], "track_not_found",
        "missing row must fail closed: {missing}"
    );
    assert_ne!(missing["ok"], true);
    std::fs::remove_dir_all(&missing_home).ok();

    let ambiguous_home = std::env::temp_dir().join(format!(
        "agend-3179-ack-ambiguous-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&ambiguous_home);
    std::fs::create_dir_all(&ambiguous_home).unwrap();
    let corr = "o/r@feat/ambiguous";
    let episode = "ep-ambiguous";
    seed_handoff_row(&ambiguous_home, "lead", corr, episode, "m-3179-ambiguous-a");
    seed_handoff_row(&ambiguous_home, "lead", corr, episode, "m-3179-ambiguous-b");
    let ambiguous = handle_ack_handoff_ci(
        &ambiguous_home,
        &json!({"repository": "o/r", "branch": "feat/ambiguous", "episode": episode}),
        "lead",
    );
    assert_eq!(
        ambiguous["code"], "row_ambiguous",
        "ambiguous exact rows must fail closed: {ambiguous}"
    );
    assert!(ack_row(&ambiguous_home, "lead", episode).read_at.is_none());
    assert!(crate::daemon::ci_handoff_track::list(&ambiguous_home).is_empty());
    std::fs::remove_dir_all(&ambiguous_home).ok();
}

#[test]
fn ack_handoff_resolved_feature_episode_mismatch_leaves_row_untouched_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-ack-resolved-mismatch-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let corr = "o/r@feat/resolved-mismatch";
    let row_episode = "ep-row";
    seed_handoff_row(&home, "lead", corr, row_episode, "m-3179-resolved-mismatch");

    let response = handle_ack_handoff_ci(
        &home,
        &json!({
            "repository": "o/r",
            "branch": "feat/resolved-mismatch",
            "episode": "ep-request"
        }),
        "lead",
    );
    assert_eq!(response["code"], "track_not_found", "{response}");
    assert!(ack_row(&home, "lead", row_episode).read_at.is_none());
    assert_eq!(
        crate::inbox::storage::handoff_row_state(
            &home,
            "lead",
            corr,
            row_episode,
            crate::inbox::CiHandoffClass::Feature,
        ),
        crate::inbox::storage::ProtectedHandoffRowState::Pending
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ack_handoff_resolved_protected_row_is_not_feature_fallback_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-ack-resolved-protected-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let corr = "o/r@main";
    let episode = "ep-protected";
    seed_handoff_row_with_class(
        &home,
        "lead",
        corr,
        episode,
        "m-3179-resolved-protected",
        crate::inbox::CiHandoffClass::Protected,
    );

    let response = handle_ack_handoff_ci(
        &home,
        &json!({"repository": "o/r", "branch": "main", "episode": episode}),
        "lead",
    );
    assert_eq!(response["code"], "track_not_found", "{response}");
    assert!(ack_row(&home, "lead", episode).read_at.is_none());
    assert_eq!(
        crate::inbox::storage::handoff_row_state(
            &home,
            "lead",
            corr,
            episode,
            crate::inbox::CiHandoffClass::Protected,
        ),
        crate::inbox::storage::ProtectedHandoffRowState::Pending
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn settle_ci_handoff_row_exact_is_idempotent_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-settle-idempotent-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let corr = "o/r@feat/settle-idempotent";
    let episode = "ep-settle-idempotent";
    seed_handoff_row(&home, "lead", corr, episode, "m-3179-settle-idempotent");

    assert_eq!(
        crate::inbox::storage::settle_ci_handoff_row_exact(
            &home,
            "lead",
            corr,
            episode,
            crate::inbox::CiHandoffClass::Feature,
        ),
        crate::inbox::storage::HandoffRowSettleOutcome::Settled
    );
    assert_eq!(
        crate::inbox::storage::settle_ci_handoff_row_exact(
            &home,
            "lead",
            corr,
            episode,
            crate::inbox::CiHandoffClass::Feature,
        ),
        crate::inbox::storage::HandoffRowSettleOutcome::AlreadySettled
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ack_handoff_resolved_feature_lock_failure_is_reported_3179() {
    let home = std::env::temp_dir().join(format!(
        "agend-3179-ack-resolved-lock-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let corr = "o/r@feat/resolved-lock";
    let episode = "ep-resolved-lock";
    seed_handoff_row(&home, "lead", corr, episode, "m-3179-resolved-lock");

    let inbox_path = crate::inbox::inbox_path_resolved(&home, "lead");
    let lock_path = inbox_path.with_extension("jsonl.lock");
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();

    let response = handle_ack_handoff_ci(
        &home,
        &json!({"repository": "o/r", "branch": "feat/resolved-lock", "episode": episode}),
        "lead",
    );
    assert_eq!(response["code"], "row_lock_failed", "{response}");
    assert!(ack_row(&home, "lead", episode).read_at.is_none());
    std::fs::remove_dir_all(&home).ok();
}
