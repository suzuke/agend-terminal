//! AUDIT2-006 C invariant: `inject_with_target_gated_offload` — the cron-only PTY
//! offload that records `ok_queued` BEFORE physical delivery — must NOT be called
//! from recovery / force / API / per-tick paths, which need synchronous delivery
//! (they keep using `inject_with_target_gated`). The only production call site is
//! cron (`src/daemon/cron_tick.rs`); the definition lives in
//! `src/agent/inject_offload.rs` and its unit tests in `src/agent/tests.rs`.
//!
//! This is a static source-scan guard so the next person can't quietly wire the
//! offload (and its weaker "queued != delivered" semantics) into a recovery/force
//! path where synchronous delivery is expected.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn offload_inject_only_called_from_cron() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);

    // (definition, the sole production cron caller, the fn's unit tests)
    const ALLOWED: &[&str] = &[
        "src/agent/inject_offload.rs",
        "src/daemon/cron_tick.rs",
        "src/agent/tests.rs",
    ];

    let mut offenders = Vec::new();
    for f in &files {
        let content = std::fs::read_to_string(f).unwrap();
        if !content.contains("inject_with_target_gated_offload(") {
            continue;
        }
        let rel = format!("src/{}", f.strip_prefix(&src).unwrap().display()).replace('\\', "/");
        if !ALLOWED.contains(&rel.as_str()) {
            offenders.push(rel);
        }
    }

    assert!(
        offenders.is_empty(),
        "AUDIT2-006 C: `inject_with_target_gated_offload(` may only appear in {ALLOWED:?} \
         (cron-only; recovery/force/API/per-tick must use the synchronous \
         `inject_with_target_gated`). Offending files: {offenders:?}"
    );
}
