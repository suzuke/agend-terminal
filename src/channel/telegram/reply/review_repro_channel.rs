//! channel-LOW-2 repro (behavioral, real registry path): the no-cleanup
//! provenance path must leave the PRODUCTION topic registry (`home/topics.json`)
//! untouched on a failed send.
//!
//! WHY THIS EXISTS: the sibling test
//! `inject_provenance_failure_does_not_mutate_fleet_or_topic_registry` hand-writes
//! and inspects `home/channel/topics.json` — but production
//! (`topic_registry.rs::topic_registry_path` → `home.join("topics.json")`) NEVER
//! reads or writes that `channel/`-prefixed file. So that test's registry-side
//! assertion is VACUOUS: it would pass even if the code unregistered the REAL
//! `home/topics.json`, because it inspects a fixture the production path never
//! touches.
//!
//! CORRECT BEHAVIOR: seed the registry through the production helper
//! (`register_topic`, which writes `home/topics.json`), drive the no-cleanup
//! provenance path to FAILURE, then assert via the production reader
//! (`load_topic_registry`, which resolves `home/topics.json`) that the real
//! registry still maps 42 → "B". This exercises the actual path
//! `try_telegram_reply_no_cleanup` is contracted to leave untouched.
//!
//! The production no-cleanup behavior is ALREADY correct (the no-cleanup error
//! branch performs no `unregister_topic`), so this faithful test is GREEN now —
//! it replaces a vacuous assertion with one that exercises the real registry
//! path. `already_fixed=true`.
//!
//! Determinism / offline: the failure is forced by configuring NO `channel:`
//! block, so `resolve_channel_from` returns `Err("No Telegram channel
//! configured")` with no env dependency and no network call — yet the registry
//! seeded via `register_topic` is independent of channel config and must survive.

use std::path::PathBuf;

/// Serialize this test's process-global env / cwd usage. (Distinct temp homes
/// already isolate the per-home topic registry; this guard is belt-and-braces.)
fn repro_guard() -> parking_lot::MutexGuard<'static, ()> {
    static G: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    G.lock()
}

fn repro_tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let id = C.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-channel-low2-{}-{}-{}",
        tag,
        std::process::id(),
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

#[test]
#[ignore = "channel-LOW-2: already-fixed fidelity check (real topics.json path); green now"]
fn inject_provenance_failure_leaves_real_topic_registry_untouched_channel() {
    use crate::channel::telegram::topic_registry::{
        load_topic_registry, register_topic, topic_registry_path,
    };

    let _g = repro_guard();
    let home = repro_tmp_home("inject_prov_real_registry");

    // Sanity-pin the finding's core claim: the production registry lives at
    // `home/topics.json`, NOT `home/channel/topics.json`. The original test
    // inspected the latter (a path production never touches), making its
    // registry assertion vacuous.
    assert_eq!(
        topic_registry_path(&home),
        home.join("topics.json"),
        "channel-LOW-2: production topic registry must be home/topics.json (no \
         `channel/` segment) — the path the no-cleanup contract is about"
    );

    // A fleet.yaml WITHOUT a `channel:` block: `resolve_channel_from` then
    // returns Err deterministically (offline, env-independent), so
    // `inject_provenance_from` fails before any send is attempted.
    let yaml = "\
instances:
  B:
    command: /bin/true
    topic_id: 42
";
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).expect("write fleet.yaml");

    // Seed the REAL registry via the production helper (writes home/topics.json).
    register_topic(&home, 42, "B").expect("register_topic must seed the real registry");
    assert_eq!(
        load_topic_registry(&home).get(&42).map(String::as_str),
        Some("B"),
        "precondition: real registry seeded with 42 -> B"
    );

    // Drive the no-cleanup provenance path to FAILURE.
    let res = super::inject_provenance_from(&home, "B", "sender", "do the thing");
    assert!(
        res.is_err(),
        "channel-LOW-2: inject_provenance should bubble the failure: {res:?}"
    );

    // The REAL registry (home/topics.json) must be untouched — assert via the
    // production reader, the path the original vacuous test failed to check.
    let reg = load_topic_registry(&home);
    assert_eq!(
        reg.get(&42).map(String::as_str),
        Some("B"),
        "channel-LOW-2: the no-cleanup provenance failure must NOT unregister the \
         target's topic from the REAL registry (home/topics.json); reg={reg:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}
