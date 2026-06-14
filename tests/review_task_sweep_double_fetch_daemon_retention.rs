//! Repro (daemon-retention batch): task_sweep fetches the merged-PR list from
//! GitHub TWICE per tick. `sweep_tick` calls `list_recently_merged_prs` once for
//! auto-close, then unconditionally calls `compliance_sweep`, which calls
//! `list_recently_merged_prs` AGAIN — a second full GitHub REST list-pulls
//! request building its own fresh tokio current-thread runtime, for identical
//! data. On every tick with compliance_mode != "off" (default "warn") this
//! doubles the GitHub API volume and runtime-build cost, accelerating
//! rate-limit exhaustion for no benefit.
//!
//! Source-scanning invariant (mirrors tests/core_mutex_invariant.rs): count the
//! `list_recently_merged_prs(` CALL-sites in src/daemon/task_sweep.rs (excluding
//! the `fn` definition). There must be at most ONE (in sweep_tick). RED now
//! (two call-sites), GREEN once the list is fetched once and the `&[PrMeta]`
//! slice is threaded into compliance_sweep.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

#[test]
#[ignore = "daemon-retention task_sweep-double-fetch: red until fix; remove #[ignore] after fix to confirm"]
fn compliance_sweep_does_not_refetch_merged_prs_daemon_retention() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/task_sweep.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    let mut call_sites = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment/doc lines that merely mention the function name.
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        // Skip the function DEFINITION line — only invocations count.
        if t.starts_with("fn list_recently_merged_prs")
            || t.starts_with("pub fn list_recently_merged_prs")
            || t.starts_with("pub(crate) fn list_recently_merged_prs")
        {
            continue;
        }
        if line.contains("list_recently_merged_prs(") {
            call_sites.push(format!("{}: {}", i + 1, line.trim()));
        }
    }

    assert!(
        call_sites.len() <= 1,
        "daemon-retention: `list_recently_merged_prs` must be called AT MOST ONCE per \
         sweep tick (fetch once in sweep_tick, thread the &[PrMeta] slice into \
         compliance_sweep). Found {} call-sites — the second (in compliance_sweep) is a \
         duplicate GitHub list-pulls request + runtime build for identical data:\n{}",
        call_sites.len(),
        call_sites.join("\n")
    );
}
