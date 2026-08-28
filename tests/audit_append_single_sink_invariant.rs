//! #3416 structural invariant: every in-repo `fleet_events.jsonl` writer goes
//! through the one shared serialized appender, and none of them touches the file
//! (or its name) directly.
//!
//! ## Why this exists
//! All four sinks used `writeln!(f, "{event}")` on an UNBUFFERED `File`.
//! `File::write_fmt` issues one `write()` syscall per formatting fragment — for a
//! real record that is ~64 syscalls — and `O_APPEND` only makes a SINGLE syscall
//! atomic with respect to other writers. Concurrent appenders therefore interleave
//! *inside* a record and destroy both. Measured on the live fleet log: 14.05% of
//! all records unparseable, 44.61% across the most recent 20k.
//!
//! Two of the four sinks are fail-closed audit gates (force-merge and creator
//! force-delete refuse when the write errors), so a corrupted-but-`Ok` write let
//! those gates believe a destructive bypass was audited when the record on disk
//! was garbage. That is what makes this structural rather than cosmetic.
//!
//! ## What it checks
//! For the PRODUCTION region of each of the four sinks (everything before the
//! first `#[cfg(test)]`):
//!   (a) it must NOT contain the `fleet_events.jsonl` literal — the shared crate
//!       owns the path, so a sink naming the file is a sink bypassing the crate;
//!   (b) it MUST call the shared appender.
//! Scoping to the production region is deliberate: a test fixture may legitimately
//! name the file. Scoping the NEGATIVE assertion to a narrower slice than the whole
//! production region is the #3421 defect — a helper outside the slice then defeats
//! it — so the region here runs to the `#[cfg(test)]` boundary and no further.
//!
//! ## How the regression-proof works
//! `checker_fires_on_direct_write` and `checker_fires_on_missing_appender` feed the
//! SAME two predicates a hand-poisoned production region and assert they report a
//! violation. No mock-only branch: the poisoned input goes through the identical
//! functions the real assertions use.

use std::path::{Path, PathBuf};

/// The four sinks named by decision `d-20260828052146598850-31`.
const SINKS: &[&str] = &[
    "vendor/agentic-git/crates/agentic-git/src/telemetry.rs",
    "src/bin/agend-git/kill_guard.rs",
    "src/mcp/handlers/ci/merge.rs",
    "src/mcp/handlers/instance_state/mod.rs",
];

/// Substring proving a sink routes through the shared crate. Matches both the
/// `agentic_audit_append::append_*` path form and a `use`-imported bare call.
const APPENDER_MARKER: &str = "append_audit_line";

/// The audit file name. A production region naming it is writing to it directly.
const AUDIT_FILE_LITERAL: &str = "fleet_events.jsonl";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Everything before the first `#[cfg(test)]`. Returns the whole source when the
/// file has no test module.
fn production_region(src: &str) -> &str {
    src.split_once("#[cfg(test)]")
        .map_or(src, |(before, _)| before)
}

/// (a) The production region must not name the audit file.
fn names_audit_file_directly(production: &str) -> bool {
    production.contains(AUDIT_FILE_LITERAL)
}

/// (b) The production region must call the shared appender.
fn routes_through_shared_appender(production: &str) -> bool {
    production.contains(APPENDER_MARKER)
}

#[test]
fn every_sink_routes_through_the_shared_serialized_appender() {
    let root = repo_root();
    for sink in SINKS {
        let path = root.join(sink);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("sink source must be readable at {}: {e}", path.display()));
        let production = production_region(&src);

        assert!(
            !names_audit_file_directly(production),
            "{sink}: production region still names `{AUDIT_FILE_LITERAL}` directly. \
             The shared appender owns the path; a sink naming the file is a sink \
             that can still open and write it unlocked."
        );
        assert!(
            routes_through_shared_appender(production),
            "{sink}: production region does not call `{APPENDER_MARKER}`. \
             Every fleet_events.jsonl write must go through the one shared \
             serialized appender — no unlocked fallback."
        );
    }
}

/// No sink may format directly into the audit file handle. This is the specific
/// construct that caused #3416: `write!`/`writeln!` on an unbuffered `File` emit
/// one syscall per fragment, so the append is not atomic no matter how small the
/// record is.
#[test]
fn no_sink_formats_directly_into_a_file_handle() {
    let root = repo_root();
    for sink in SINKS {
        let path = root.join(sink);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("sink source must be readable at {}: {e}", path.display()));
        let production = production_region(&src);
        for bad in ["writeln!(f,", "write!(f,"] {
            assert!(
                !production.contains(bad),
                "{sink}: production region still contains `{bad}` — formatting into \
                 an unbuffered File issues one write() syscall per fragment and \
                 cannot be an atomic append."
            );
        }
    }
}

#[test]
fn checker_fires_on_direct_write() {
    let poisoned = r#"
        fn append(home: &str) {
            let p = PathBuf::from(home).join("fleet_events.jsonl");
            let _ = writeln!(f, "{event}");
        }
        #[cfg(test)]
        mod tests {}
    "#;
    let production = production_region(poisoned);
    assert!(
        names_audit_file_directly(production),
        "the (a) predicate must flag a production region that names the audit file"
    );
    assert!(
        production.contains("writeln!(f,"),
        "the direct-format scan must flag `writeln!(f,` in a production region"
    );
}

#[test]
fn checker_fires_on_missing_appender() {
    let poisoned = "fn append(home: &str) { /* routes nowhere */ }";
    assert!(
        !routes_through_shared_appender(production_region(poisoned)),
        "the (b) predicate must flag a production region that never calls the shared appender"
    );
}

/// The production-region split must not silently swallow the whole file when the
/// marker is absent in a way that hides a violation, and must not let a test
/// module's contents satisfy the positive assertion.
#[test]
fn production_region_stops_at_the_test_boundary() {
    let src = "fn prod() {}\n#[cfg(test)]\nmod tests { fn t() { append_audit_line(); } }";
    let production = production_region(src);
    assert!(
        !routes_through_shared_appender(production),
        "a call that exists only inside the test module must NOT satisfy the \
         shared-appender assertion"
    );
    assert!(
        production.contains("fn prod()"),
        "the production region must still contain the production code"
    );
}
