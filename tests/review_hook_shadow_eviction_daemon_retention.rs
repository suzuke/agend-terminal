//! Repro (daemon-retention batch): the process-global hook_shadow `store()`
//! `HashMap<String, HookShadow>` is keyed by agent name and only ever inserted
//! into (record_event). There is no removal on agent delete/redeploy/session-end
//! — even SessionEnd just records another event. For a long-running daemon that
//! churns through many distinctly-named agents the map grows monotonically, and
//! a redeployed same-name agent inherits the prior instance's last observation
//! until overwritten.
//!
//! Source-scanning invariant (mirrors tests/core_mutex_invariant.rs): assert
//! that src/daemon/hook_shadow.rs gains SOME eviction path — any of
//! `fn forget`, `fn evict`, `fn prune`, `fn retain_active`, or a `.retain(`
//! call (age-out). RED now (no eviction token present), GREEN once a bounded
//! eviction hook is added.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

#[test]
fn hook_shadow_store_has_an_eviction_path_daemon_retention() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/hook_shadow.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // Eviction-path needles. `backdate_for_test`/`set_user_prompt_submit_for_test`
    // are test seams (mutate in place, not evict) and do not match these.
    let needles = [
        "fn forget",
        "fn evict",
        "fn prune",
        "fn retain_active",
        ".retain(",
    ];

    let has_eviction = src.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            return false; // skip prose/doc mentions
        }
        needles.iter().any(|n| line.contains(n))
    });

    assert!(
        has_eviction,
        "daemon-retention: hook_shadow's global store has NO eviction path — it grows \
         one permanent entry per distinctly-named agent ever seen, and a same-name \
         redeploy inherits the prior instance's observation. Add a bounded-eviction \
         hook (e.g. `forget(name)` on agent deletion, or an age-out \
         `.retain(|_, s| now - s.at_ms <= HOOK_FRESHNESS)` sweep). \
         Looked for any of: {needles:?}"
    );
}
