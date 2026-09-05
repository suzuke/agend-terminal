use super::super::reconcile_all;
use super::{mk, tmp_home};
use crate::daemon::assignment_authority as store;
use std::path::Path;

fn install_test_claude_locator(home: &Path, instance: &str) {
    let mut locator = crate::transport::SessionLocator::claude(
        "http://127.0.0.1:43123".to_string(),
        "assignment-wake-test".to_string(),
        "test-token".to_string(),
    );
    locator.managed = true;
    locator.server_pid = Some(std::process::id());
    locator.server_start_token = crate::process::process_start_token(std::process::id());
    crate::transport::save_session_locator(home, instance, &locator).unwrap();
}

/// #3504 RED: a queued wake must not advance the lease before the delivery
/// worker reports the adapter outcome. A dead ChannelBridge/adapter failure
/// must produce the bounded retry lease from the real `reconcile_all` entry.
#[test]
fn adapter_failure_keeps_wake_visible_until_worker_records_failure() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let home = tmp_home("adapter-failure-wake");
    let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
    store::persist(&home, &rec).unwrap();
    store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: channel_bridge\n",
    )
    .unwrap();
    install_test_claude_locator(&home, "reviewer");

    let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(false);
    let _transport = crate::transport::test_support::delivery_hook_guard();
    let entered = std::sync::Arc::new(AtomicBool::new(false));
    let release = std::sync::Arc::new(AtomicBool::new(false));
    let expected_home = home.clone();
    let entered_hook = std::sync::Arc::clone(&entered);
    let release_hook = std::sync::Arc::clone(&release);
    crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
        move |called_home, _name, _body| {
            if called_home != expected_home.as_path() {
                return None;
            }
            entered_hook.store(true, Ordering::SeqCst);
            while !release_hook.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Some(Err(anyhow::anyhow!("dead ChannelBridge locator")))
        },
    )));

    reconcile_all(&home, "2026-07-13T00:00:01Z");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "wake must reach the delivery worker"
    );

    let before_outcome = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
    assert_eq!(
        before_outcome.next_nudge_at, rec.next_nudge_at,
        "adapter outcome is not known while the worker is blocked"
    );

    release.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, "reviewer")
        == 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let after_failure = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
    assert_eq!(
        after_failure.next_nudge_at, "2026-07-13T00:00:06+00:00",
        "adapter failure must leave a bounded short retry lease"
    );
    let events = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap();
    assert!(
        events.contains("transport_delivery_failed"),
        "adapter failure must leave a durable inspectable failure event"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn accepted_assignment_wake_advances_lease_after_worker_outcome() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let home = tmp_home("accepted-assignment-wake");
    let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
    store::persist(&home, &rec).unwrap();
    store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: channel_bridge\n",
    )
    .unwrap();
    install_test_claude_locator(&home, "reviewer");

    let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(false);
    let _transport = crate::transport::test_support::delivery_hook_guard();
    let entered = std::sync::Arc::new(AtomicBool::new(false));
    let release = std::sync::Arc::new(AtomicBool::new(false));
    let expected_home = home.clone();
    let entered_hook = std::sync::Arc::clone(&entered);
    let release_hook = std::sync::Arc::clone(&release);
    crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
        move |called_home, name, body| {
            if called_home != expected_home.as_path() {
                return None;
            }
            entered_hook.store(true, Ordering::SeqCst);
            while !release_hook.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let envelope = crate::transport::DeliveryEnvelope::new(
                name,
                crate::transport::SessionLocator::claude(
                    "http://127.0.0.1:43123".to_string(),
                    "assignment-wake-test".to_string(),
                    "test-token".to_string(),
                ),
                crate::transport::DeliveryKind::Notification,
                body,
                None,
            );
            Some(Ok(crate::transport::DeliveryReceipt::for_state(
                &envelope,
                crate::transport::DeliveryState::ProtocolAccepted,
            )))
        },
    )));

    reconcile_all(&home, "2026-07-13T00:00:01Z");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "wake must reach the delivery worker"
    );
    assert_eq!(
        store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .next_nudge_at,
        rec.next_nudge_at,
        "accepted queue admission must not advance the lease early"
    );

    release.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, "reviewer")
        == 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .next_nudge_at,
        "2026-07-13T00:01:01+00:00",
        "accepted adapter outcome advances the bounded lease"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn queue_full_assignment_wake_records_failure_and_short_retry() {
    let home = tmp_home("queue-full-assignment-wake");
    let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
    store::persist(&home, &rec).unwrap();
    store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: channel_bridge\n",
    )
    .unwrap();
    install_test_claude_locator(&home, "reviewer");

    let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(true);
    reconcile_all(&home, "2026-07-13T00:00:01Z");
    crate::daemon::delivery_worker::test_support::set_force_full(false);

    let after = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
    assert_eq!(
        after.next_nudge_at, "2026-07-13T00:00:06+00:00",
        "queue admission failure must use the bounded retry lease"
    );
    let receipts = std::fs::read_to_string(crate::transport::delivery_path_for_instance(
        &home, "reviewer",
    ))
    .unwrap();
    assert!(receipts.contains("\"state\":\"Failed\""));
    assert!(receipts.contains("bounded transport delivery queue full"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn fenced_assignment_wake_records_failure_and_short_retry() {
    let home = tmp_home("fenced-assignment-wake");
    let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
    store::persist(&home, &rec).unwrap();
    store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: channel_bridge\n",
    )
    .unwrap();
    install_test_claude_locator(&home, "reviewer");

    let cleanup = crate::daemon::delivery_worker::begin_transport_cleanup(&home, "reviewer");
    reconcile_all(&home, "2026-07-13T00:00:01Z");

    let after = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
    assert_eq!(
        after.next_nudge_at, "2026-07-13T00:00:06+00:00",
        "fenced admission must use the bounded retry lease"
    );
    let receipts = std::fs::read_to_string(crate::transport::delivery_path_for_instance(
        &home, "reviewer",
    ))
    .unwrap();
    assert!(receipts.contains("\"state\":\"Failed\""));
    assert!(receipts.contains("fenced during generation transition"));
    drop(cleanup);
    std::fs::remove_dir_all(&home).ok();
}

/// #3504 R2: LegacyPty successful-but-unproven must advance the normal
/// lease and must NOT be recorded as fenced/queue-full. LegacyPty
/// always returns Ambiguous on success (legacy_pty.rs:47-57), and the
/// delivery_worker's success test must treat that as successful on
/// LegacyPty (advance full Interval, no short-retry event). This case
/// fails on the pre-R1 code where every Ambiguous was a short-retry.
#[test]
fn legacy_pty_ambiguous_advances_normal_lease_without_fenced_event() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let home = tmp_home("legacy-pty-ambiguous");
    let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
    store::persist(&home, &rec).unwrap();
    store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
    // agy maps to LegacyPty via mode_for_backend (registry.rs:29)
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  reviewer:\n    backend: agy\n",
    )
    .unwrap();

    let _full = crate::daemon::delivery_worker::test_support::force_full_guard();
    crate::daemon::delivery_worker::test_support::set_force_full(false);
    let _transport = crate::transport::test_support::delivery_hook_guard();
    let entered = std::sync::Arc::new(AtomicBool::new(false));
    let release = std::sync::Arc::new(AtomicBool::new(false));
    let expected_home = home.clone();
    let entered_hook = std::sync::Arc::clone(&entered);
    let release_hook = std::sync::Arc::clone(&release);
    crate::transport::test_support::set_delivery_hook(Some(std::sync::Arc::new(
        move |called_home, name, body| {
            if called_home != expected_home.as_path() {
                return None;
            }
            entered_hook.store(true, Ordering::SeqCst);
            while !release_hook.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let envelope = crate::transport::DeliveryEnvelope::new(
                name,
                crate::transport::SessionLocator::codex(
                    std::path::PathBuf::from("/tmp/legacy-pty-ambiguous.sock"),
                    Some("legacy-pty-thread".to_string()),
                ),
                crate::transport::DeliveryKind::Notification,
                body,
                None,
            );
            // LegacyPty success is Ambiguous (unproven acceptance) — the
            // delivery_worker must treat this as successful-but-unproven.
            Some(Ok(crate::transport::DeliveryReceipt::for_state(
                &envelope,
                crate::transport::DeliveryState::Ambiguous,
            )))
        },
    )));

    reconcile_all(&home, "2026-07-13T00:00:01Z");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "wake must reach the delivery worker"
    );

    release.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while crate::daemon::delivery_worker::test_support::transport_dispatch_count(&home, "reviewer")
        == 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let after = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
    assert_eq!(
        after.next_nudge_at, "2026-07-13T00:01:01+00:00",
        "LegacyPty Ambiguous (successful-but-unproven) must advance the normal 60s lease, not the 5s short retry"
    );
    let events = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        !events.contains("review_assignment_wake_retry"),
        "LegacyPty successful-but-unproven must not write a short-retry failure event; got: {events}"
    );
    std::fs::remove_dir_all(&home).ok();
}
