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
//! For the PRODUCTION region of each of the four sinks — everything up to the
//! first INLINE test module body, see `production_region` for why that is not the
//! same as the first `#[cfg(test)]` line:
//!   (a) no line of CODE may contain the `fleet_events.jsonl` literal — the shared
//!       crate owns the path, so a sink naming the file is a sink bypassing it;
//!   (b) it MUST call the shared appender.
//! Scoping to the production region is deliberate: a test fixture may legitimately
//! name the file. Getting that scope wrong in either direction is the #3421 defect
//! class — too narrow and a helper outside the slice defeats the scan, too wide and
//! the test module satisfies it. Both directions are pinned by
//! `production_region_stops_at_the_test_boundary`.
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

/// Everything before the first test-gated item. Returns the whole source when the
/// file has no test module.
///
/// Matches the cfg attribute in ANY spelling — `#[cfg(test)]`,
/// `#[cfg(all(test, unix))]`, `#[cfg(any(test, …))]` — rather than one literal.
/// Splitting on the exact string `#[cfg(test)]` is how a test module written with a
/// compound cfg silently lands INSIDE the region being scanned as production, which
/// is the #3421 defect class: the scan then passes or fails for the wrong reason.
/// `not(test)` is excluded because that gates production code, not tests.
fn production_region(src: &str) -> &str {
    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let mut offset = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let is_test_gate = trimmed.starts_with("#[cfg(")
            && trimmed.contains("test")
            && !trimmed.contains("not(test");
        if is_test_gate {
            // Only an INLINE test module body ends the production region. A
            // `#[cfg(test)] mod x;` DECLARATION brings no code into this file, and
            // treating it as the boundary truncates the scan to whatever precedes
            // it — which in one of these sinks is the first eight lines, silently
            // passing a file that was never examined.
            // Skip any further attribute lines between the cfg gate and the item —
            // these modules commonly carry `#[allow(...)]` as well — then decide on
            // the item itself. `contains('{')` rather than `ends_with`, so a
            // single-line `mod tests { .. }` is recognised too.
            let next = lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty() && !l.starts_with("#["))
                .unwrap_or("");
            if next.starts_with("mod ") && next.contains('{') {
                return &src[..offset];
            }
        }
        offset += line.len();
    }
    src
}

/// (a) The production region must not name the audit file **in code**.
///
/// Comments are excluded on purpose. The invariant is about who may open and write
/// the file, not about whether prose may name it — every one of these sinks has a
/// doc comment explaining what it appends to, and forcing those to talk around the
/// filename would trade real documentation for a scan that is no stronger.
fn names_audit_file_directly(production: &str) -> bool {
    production
        .lines()
        .map(str::trim_start)
        .filter(|l| !(l.starts_with("//") || l.starts_with('*')))
        .any(|l| l.contains(AUDIT_FILE_LITERAL))
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

/// The comment exclusion must not become a hole: a doc comment naming the file is
/// fine, the same name in code is not. Both directions are asserted so a future
/// change to the filter cannot quietly disable the scan.
#[test]
fn checker_ignores_comments_but_not_code() {
    let doc_only = "/// appends to fleet_events.jsonl\n//! see fleet_events.jsonl\nfn f() {}";
    assert!(
        !names_audit_file_directly(doc_only),
        "a doc comment naming the audit file must not trip the scan"
    );

    let in_code = "/// appends to fleet_events.jsonl\nlet p = home.join(\"fleet_events.jsonl\");";
    assert!(
        names_audit_file_directly(in_code),
        "the same name in CODE must trip the scan"
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
    for gate in [
        "#[cfg(test)]",
        "#[cfg(all(test, unix))]",
        "#[cfg(any(test, feature = \"x\"))]",
        // The real shim module carries a second attribute between the gate and
        // the item; the lookahead must step over it.
        "#[cfg(all(test, unix))]\n#[allow(clippy::unwrap_used)]",
    ] {
        let src =
            format!("fn prod() {{}}\n{gate}\nmod tests {{ fn t() {{ append_audit_line(); }} }}");
        let production = production_region(&src);
        assert!(
            !routes_through_shared_appender(production),
            "{gate}: a call that exists only inside the test module must NOT satisfy \
             the shared-appender assertion"
        );
        assert!(
            production.contains("fn prod()"),
            "{gate}: the production region must still contain the production code"
        );
    }

    // `not(test)` gates PRODUCTION code — it must not be mistaken for the boundary.
    let src = "#[cfg(not(test))]\nfn prod() { append_audit_line(); }";
    assert!(
        routes_through_shared_appender(production_region(src)),
        "a `not(test)` item is production and must stay inside the scanned region"
    );

    // A `#[cfg(test)] mod x;` DECLARATION is not the boundary: it pulls in a
    // separate file, so production code after it must still be scanned. One of the
    // real sinks has exactly this at line 9, and treating it as the boundary made
    // the scan pass on eight lines of imports.
    let src = "#[cfg(test)]\npub(crate) mod helper;\n\nfn prod() { append_audit_line(); }";
    assert!(
        routes_through_shared_appender(production_region(src)),
        "a cfg(test) module DECLARATION must not truncate the production region"
    );
}
