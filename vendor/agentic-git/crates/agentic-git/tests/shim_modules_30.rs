//! #30 characterization: the shim's 3358-line `lib.rs` splits into five
//! internal modules — `classify.rs`, `push_guards.rs`, `paths.rs`, `exec.rs`,
//! `telemetry.rs` — with `lib.rs` reduced to entry wiring (env compat,
//! `shim_entry`/`shim_main`, bypass, binding read, module re-exports).
//!
//! Behavior-preserving: no policy, deny-string, event, or classification
//! change. The existing suites (phase1-5, exec-reachability, legacy-env,
//! submodule-policy, session-mode, snapshots, unit tests) pin behavior; this
//! file pins the STRUCTURE so the split cannot silently regress into a
//! re-merged monolith, and `equivalent_guard_strings_survive_split_30` keeps
//! the phase2-style source guards meaningful across the whole shim source.

use std::fs;
use std::path::{Path, PathBuf};

const MODULES: [&str; 5] = [
    "classify.rs",
    "push_guards.rs",
    "paths.rs",
    "exec.rs",
    "telemetry.rs",
];

/// One anchor definition per module: the item everyone would name first when
/// asked "what lives there". `lib.rs` must no longer define any of them.
const ANCHORS: [(&str, &str); 5] = [
    ("classify.rs", "fn classify("),
    ("push_guards.rs", "fn parse_push_argv("),
    ("paths.rs", "fn find_git_dir("),
    ("exec.rs", "fn exec_real_git("),
    ("telemetry.rs", "fn disposition_for("),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read_src(name: &str) -> String {
    let p = src_dir().join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The five module files exist and `lib.rs` declares each one.
#[test]
fn five_module_files_exist_and_declared_30() {
    let lib = read_src("lib.rs");
    for m in MODULES {
        assert!(
            src_dir().join(m).is_file(),
            "#30: src/{m} must exist (shim module split)"
        );
        let decl = format!("mod {};", m.trim_end_matches(".rs"));
        assert!(
            lib.contains(&decl),
            "#30: lib.rs must declare `{decl}` so the module is wired in"
        );
    }
}

/// `lib.rs` shrinks from 3358 lines to entry wiring. 800 is a generous
/// ceiling for doc + env compat + shim_entry/shim_main + bypass + binding +
/// module wiring; the pre-split monolith is 4x over it.
#[test]
fn lib_rs_shrinks_to_entry_wiring_30() {
    let lines = read_src("lib.rs").lines().count();
    assert!(
        lines <= 800,
        "#30: lib.rs must shrink to entry wiring (<= 800 lines), got {lines}"
    );
}

/// Each module owns its anchor definition, and `lib.rs` defines none of them.
#[test]
fn module_ownership_anchors_30() {
    let lib = read_src("lib.rs");
    for (m, anchor) in ANCHORS {
        let module_src = fs::read_to_string(src_dir().join(m)).unwrap_or_default();
        assert!(
            module_src.contains(anchor),
            "#30: src/{m} must own the definition `{anchor}...`"
        );
        assert!(
            !lib.contains(anchor),
            "#30: lib.rs must no longer define `{anchor}...` (moved to src/{m})"
        );
    }
}

/// Control (green before AND after the split): the load-bearing deny/event
/// source guards hold over the WHOLE shim source, not one file. This is the
/// split-proof equivalent of the phase2 single-file scans.
#[test]
fn equivalent_guard_strings_survive_split_30() {
    let mut all = read_src("lib.rs");
    for m in MODULES {
        // Pre-split the module files don't exist yet — the monolith carries
        // every guard string, so missing files contribute nothing.
        if src_dir().join(m).is_file() {
            all.push_str(&read_src(m));
        }
    }
    for needle in [
        "AGENTIC_GIT_BYPASS",
        "cross-branch",
        "unbound",
        "worktree lifecycle is session-managed",
        "fleet_events.jsonl",
        "\"git_event\"",
        "AGEND_REAL_GIT",
    ] {
        assert!(
            all.contains(needle),
            "#30: shim source must keep the guard string {needle:?} after the split"
        );
    }
}
