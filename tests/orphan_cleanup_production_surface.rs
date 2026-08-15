#![allow(clippy::unwrap_used, clippy::expect_used)]
//! #3273 V2 — the production surface: the exact-PID conversion, platform
//! support, and the structural guards that keep signal authority confined to
//! the manual executor.
//!
//! Consensus `d-20260814214253501210-22`. The contracts in
//! `orphan_cleanup_manual_authority.rs` prove the executor's *logic* against
//! injected seams; these prove that the real adapters cannot widen the blast
//! radius, and that nothing outside the explicit manual surface can reach them.

use agend_terminal::admin::orphan_cleanup::{platform_support, ExactPid, Support};
use syn::visit::{self, Visit};

// ---------------------------------------------------------------------------
// The exact-PID conversion. This is the single highest-consequence line in the
// change, because both failure modes are silent at the call site.
// ---------------------------------------------------------------------------

/// `kill(0, sig)` signals every process in the CALLER's process group — on a
/// daemon that is the daemon and everything it spawned. A pid of 0 must never
/// become a signal target.
#[test]
fn pid_zero_can_never_become_a_signal_target_3273() {
    assert_eq!(
        ExactPid::new(0),
        None,
        "pid 0 must be refused: kill(0, sig) signals the caller's entire process group"
    );
}

/// A pid above `i32::MAX` wraps NEGATIVE under `as i32`, and `kill(-n, sig)`
/// signals process group `n`. The cast turns an exact-pid contract into a group
/// signal with no diagnostic anywhere.
#[test]
fn a_pid_above_i32_max_can_never_wrap_into_a_group_signal_3273() {
    for pid in [
        i32::MAX as u32 + 1, // wraps to i32::MIN
        u32::MAX,            // wraps to -1 — kill(-1, sig) is "every process we may signal"
        0x8000_0000,
        0xFFFF_FFFE,
    ] {
        assert_eq!(
            ExactPid::new(pid),
            None,
            "pid {pid} must be refused rather than wrapped into a negative target"
        );
    }
}

/// Everything a real process can legitimately be keeps working, so the guard is
/// a guard and not a wall.
#[test]
fn ordinary_pids_convert_exactly_3273() {
    for pid in [1u32, 2, 4242, 99_999, i32::MAX as u32] {
        let exact = ExactPid::new(pid).unwrap_or_else(|| panic!("pid {pid} must be accepted"));
        assert_eq!(exact.get(), pid as i32, "the value must survive unchanged");
        assert!(exact.get() > 0, "an exact target is always strictly positive");
    }
}

/// On a platform with no identity oracle there is nothing to revalidate a
/// snapshot against, so the feature stays report-only rather than degrading to
/// "signal and hope".
#[test]
fn platform_support_is_explicit_3273() {
    let support = platform_support();
    if cfg!(unix) {
        assert_eq!(support, Support::Supported);
    } else {
        assert!(
            matches!(support, Support::Unsupported(_)),
            "a non-Unix build must be explicitly unsupported, not silently supported"
        );
    }
}

// ---------------------------------------------------------------------------
// Structural guards. Signal authority must stay where the consensus put it.
// ---------------------------------------------------------------------------

/// Collects every path segment mentioned in a file, so a guard can ask "does
/// this module even name that thing?" without pattern-matching on source text.
#[derive(Default)]
struct PathCollector {
    segments: Vec<String>,
}

impl<'ast> Visit<'ast> for PathCollector {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        for segment in &path.segments {
            self.segments.push(segment.ident.to_string());
        }
        visit::visit_path(self, path);
    }
}

fn segments_of(rel: &str) -> Vec<String> {
    let src = std::fs::read_to_string(rel).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    let file = syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    let mut collector = PathCollector::default();
    collector.visit_file(&file);
    collector.segments
}

/// V1 classification stays report-only. It may describe a candidate in any
/// detail; it may not be able to act on one, and it must not reach the executor
/// that can.
#[test]
fn the_v1_module_cannot_reach_the_manual_executor_3273() {
    let segments = segments_of("src/admin/orphan_provenance.rs");
    for forbidden in ["orphan_cleanup", "kill", "terminate", "kill_process"] {
        assert!(
            !segments.iter().any(|s| s == forbidden),
            "V1 must not name `{forbidden}` — observations never carry signal authority"
        );
    }
}

/// The manual executor is manual. A daemon tick reaching it would make cleanup
/// automatic, which the consensus defers until per-tool launch leases and
/// kernel-backed containment exist.
#[test]
fn no_daemon_tick_reaches_the_manual_executor_3273() {
    for rel in [
        "src/daemon/per_tick/assignment_reconcile.rs",
        "src/admin/cleanup_zombies.rs",
    ] {
        let segments = segments_of(rel);
        assert!(
            !segments.iter().any(|s| s == "orphan_cleanup"),
            "{rel} must not reference the manual executor: automatic cleanup is deferred"
        );
    }
}

/// `doctor` with no subcommand is a report. It must not prompt, consume a
/// confirmation, or signal anything, so it must not name the executor at all.
#[test]
fn default_doctor_stays_report_only_3273() {
    let src = std::fs::read_to_string("src/cli.rs").expect("read src/cli.rs");
    let file = syn::parse_file(&src).expect("parse src/cli.rs");
    let run_doctor = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == "run_doctor" => Some(f),
            _ => None,
        })
        .expect("run_doctor must exist");
    let mut collector = PathCollector::default();
    collector.visit_item_fn(run_doctor);
    assert!(
        !collector.segments.iter().any(|s| s == "orphan_cleanup"),
        "the default doctor report must not reach the manual executor"
    );
}
