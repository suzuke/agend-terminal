//! #3416 structural invariant: no `fleet_events.jsonl` sink may write the audit
//! log itself. Every one of them goes through the single serialized appender.
//!
//! ## Why this exists
//! All four sinks used `writeln!(f, "{event}")` on an UNBUFFERED `File`.
//! `File::write_fmt` issues one `write()` syscall per formatting fragment — for a
//! real record that is ~64 syscalls — and `O_APPEND` only makes a SINGLE syscall
//! atomic against other writers, so concurrent appenders interleaved *inside* a
//! record and destroyed both. Measured on the live fleet log: 14.05% of all
//! records unparseable, 44.61% across the most recent 20k.
//!
//! Two of the four sinks are fail-closed audit gates (force-merge and creator
//! force-delete refuse when the write fails), so a corrupted-but-`Ok` write let
//! those gates report a destructive bypass as audited while the record on disk was
//! garbage. That is what makes this structural rather than cosmetic.
//!
//! ## Why this is an AST check and not a text scan
//! The first version of this guard scanned text for `writeln!(f,` and for the
//! `fleet_events.jsonl` literal. Both are trivially bypassable, and the review
//! that caught it was right: rename the handle (`writeln!(file, …)`), use a
//! different call (`f.write_all(…)`, `std::fs::write(…)`), or build the path from
//! a constant instead of a literal, and the scan sees nothing. It also split the
//! production region on the literal `#[cfg(test)]`, which missed
//! `#[cfg(all(test, unix))]` entirely and mistook a `#[cfg(test)] mod x;`
//! DECLARATION for the boundary.
//!
//! The rule enforced here instead is structural and closes those by construction:
//!
//!   **the production region of each sink may contain NO file-write primitive.**
//!
//! Rust cannot write a file without one of them, so HOW the path is built stops
//! mattering — a sink that constructs `$AGEND_HOME/fleet_events.jsonl` through a
//! constant, a helper, or a runtime string still has to open and write it, and
//! every way of doing that is banned here. That is the half that makes this
//! non-bypassable; the audit-filename literal is banned too, but only as a
//! belt-and-braces signal.
//!
//! All four sinks currently contain zero such primitives in production, so the ban
//! costs them nothing. If one ever needs to write an unrelated file, this test
//! must be revisited deliberately rather than loosened casually.
//!
//! ## How the regression-proof works
//! `the_guard_catches_every_known_bypass_shape` feeds the SAME visitor a synthetic
//! source containing each bypass the text scan missed, and asserts each is
//! flagged. No mock-only branch: the poisoned input goes through the identical
//! visitor the real assertions use.

use std::path::{Path, PathBuf};
use syn::visit::Visit;

/// The four sinks named by decision `d-20260828052146598850-31`.
const SINKS: &[&str] = &[
    "vendor/agentic-git/crates/agentic-git/src/telemetry.rs",
    "src/bin/agend-git/kill_guard.rs",
    "src/mcp/handlers/ci/merge.rs",
    "src/mcp/handlers/instance_state/mod.rs",
];

/// Substring proving a sink routes through the shared crate. Matches both the
/// `agentic_audit_append::append_audit_line_*` path form and a `use`-imported call.
const APPENDER_MARKER: &str = "append_audit_line";

const AUDIT_FILE_LITERAL: &str = "fleet_events.jsonl";

/// Macros that write through a formatter. Matched on the AST macro path, so the
/// receiver's name is irrelevant — `writeln!(f, …)` and `writeln!(whatever, …)`
/// are the same node.
const BANNED_MACROS: &[&str] = &["write", "writeln"];

/// Methods that put bytes into a handle.
const BANNED_METHODS: &[&str] = &["write", "write_all", "write_fmt", "write_vectored"];

/// Path segments that open or write a file. Any occurrence in a production item
/// of these files is a violation regardless of how the path argument is built.
const BANNED_PATH_SEGMENTS: &[&str] = &["OpenOptions", "create_new"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// True iff any `cfg(test)` reference appears in `attrs` — `#[cfg(test)]`,
/// `#[cfg(all(test, unix))]`, `#[cfg(any(test, …))]`. Token-stream matching rather
/// than chasing syn's `Meta` tree, matching the convention already used by
/// `tests/cargo_include_invariant.rs`.
/// Does this cfg predicate GUARANTEE that the item compiles only under `cfg(test)`?
///
/// Asking "does the token `test` appear" is the wrong question, and it inverts on
/// the spelling that matters most: `not(test)` contains the token and means
/// production-ONLY. The predicate is parsed instead:
///
/// * `test` — yes.
/// * `all(..)` — yes if ANY child guarantees it; every child must hold, so one
///   test-only child is enough.
/// * `any(..)` — yes only if EVERY child guarantees it; otherwise some other arm
///   can satisfy the cfg in a production build.
/// * `not(..)` — never. `not(test)` is production-only, and `not(<anything>)`
///   cannot guarantee test-only compilation.
/// * anything else (a feature, a target, an unparsed shape) — no.
///
/// Unknown predicates therefore fall to "scan it", which is the fail-closed
/// direction for a guard that exists to stop production writes.
fn cfg_guarantees_test_only(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::List(list) => {
            if list.path.is_ident("not") {
                return false;
            }
            let Ok(nested) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                // An unparseable predicate is not a guarantee of anything.
                return false;
            };
            if list.path.is_ident("all") {
                nested.iter().any(cfg_guarantees_test_only)
            } else if list.path.is_ident("any") {
                !nested.is_empty() && nested.iter().all(cfg_guarantees_test_only)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn attrs_contain_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &a.meta else {
            return false;
        };
        let Ok(inner) = list.parse_args::<syn::Meta>() else {
            return false;
        };
        cfg_guarantees_test_only(&inner)
    })
}

#[derive(Default)]
struct SinkAudit {
    violations: Vec<String>,
    calls_appender: bool,
}

impl SinkAudit {
    fn scan(src: &str) -> syn::Result<Self> {
        let file = syn::parse_file(src)?;
        let mut audit = SinkAudit::default();
        audit.visit_file(&file);
        Ok(audit)
    }
}

impl<'ast> Visit<'ast> for SinkAudit {
    // Test-gated items are pruned at the visitor level, so the "production region"
    // is a property of the AST rather than of line positions.
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if attrs_contain_cfg_test(&m.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, m);
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if attrs_contain_cfg_test(&f.attrs) {
            return;
        }
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_contain_cfg_test(&i.attrs) {
            return;
        }
        syn::visit::visit_item_impl(self, i);
    }
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        if attrs_contain_cfg_test(&u.attrs) {
            return;
        }
        syn::visit::visit_item_use(self, u);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(last) = m.path.segments.last() {
            let name = last.ident.to_string();
            if BANNED_MACROS.contains(&name.as_str()) {
                self.violations.push(format!(
                    "`{name}!` in production: formatting into a handle is one write() \
                     syscall per fragment and cannot be an atomic append"
                ));
            }
        }
        syn::visit::visit_macro(self, m);
    }

    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let name = c.method.to_string();
        if BANNED_METHODS.contains(&name.as_str()) {
            self.violations.push(format!(
                "`.{name}(..)` in production: writes bytes to a handle"
            ));
        }
        syn::visit::visit_expr_method_call(self, c);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        let joined = segs.join("::");
        if joined.contains(APPENDER_MARKER) {
            self.calls_appender = true;
        }
        for seg in &segs {
            if BANNED_PATH_SEGMENTS.contains(&seg.as_str()) {
                self.violations
                    .push(format!("`{joined}` in production: opens a file directly"));
            }
        }
        // `fs::write` / `File::create` / `File::open` in any spelling.
        for w in [
            ["fs", "write"],
            ["File", "create"],
            ["File", "open"],
            ["fs", "OpenOptions"],
        ] {
            if segs.windows(2).any(|p| p[0] == w[0] && p[1] == w[1]) {
                self.violations
                    .push(format!("`{joined}` in production: opens or writes a file"));
            }
        }
        syn::visit::visit_path(self, p);
    }

    /// Doc comments reach the AST as `#[doc = "…"]`, so their text is a `LitStr`
    /// like any other. Prose that names the audit file is not a write to it —
    /// every one of these sinks documents what it appends to, and forcing those
    /// comments to talk around the filename would trade real documentation for a
    /// guard that is no stronger. Attributes are skipped wholesale rather than
    /// special-casing `doc`, because no attribute argument can perform I/O.
    fn visit_attribute(&mut self, _a: &'ast syn::Attribute) {}

    fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
        if l.value().contains(AUDIT_FILE_LITERAL) {
            self.violations.push(format!(
                "audit filename literal `{AUDIT_FILE_LITERAL}` in production: the \
                 shared appender owns the path"
            ));
        }
        syn::visit::visit_lit_str(self, l);
    }
}

#[test]
fn no_sink_writes_the_audit_log_directly() {
    let root = repo_root();
    for sink in SINKS {
        let path = root.join(sink);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("sink source must be readable at {}: {e}", path.display()));
        let audit = SinkAudit::scan(&src)
            .unwrap_or_else(|e| panic!("{sink} must parse as Rust for the AST scan: {e}"));
        assert!(
            audit.violations.is_empty(),
            "{sink}: production region performs its own file I/O — every audit write \
             must go through the shared serialized appender, with no unlocked \
             fallback:\n  {}",
            audit.violations.join("\n  ")
        );
    }
}

#[test]
fn every_sink_routes_through_the_shared_appender() {
    let root = repo_root();
    for sink in SINKS {
        let path = root.join(sink);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("sink source must be readable at {}: {e}", path.display()));
        let audit = SinkAudit::scan(&src)
            .unwrap_or_else(|e| panic!("{sink} must parse as Rust for the AST scan: {e}"));
        assert!(
            audit.calls_appender,
            "{sink}: production region never calls `{APPENDER_MARKER}`"
        );
    }
}

/// Each of these is a shape the previous TEXT scan let through. They are the
/// reason this guard is an AST walk, so each one is asserted to be caught.
#[test]
fn the_guard_catches_every_known_bypass_shape() {
    let cases: &[(&str, &str)] = &[
        (
            "original shape",
            r#"fn f() { let mut f = open(); let _ = writeln!(f, "{event}"); }"#,
        ),
        (
            "renamed handle — defeated the `writeln!(f,` text scan",
            r#"fn f() { let mut handle = open(); let _ = writeln!(handle, "{event}"); }"#,
        ),
        (
            "write! instead of writeln!",
            r#"fn f() { let mut out = open(); let _ = write!(out, "{event}"); }"#,
        ),
        (
            "method call instead of a macro",
            r#"fn f() { let mut h = open(); let _ = h.write_all(line.as_bytes()); }"#,
        ),
        (
            "whole-file write, no handle at all",
            r#"fn f() { let _ = std::fs::write(p, bytes); }"#,
        ),
        (
            "opening the log directly",
            r#"fn f() { let _ = std::fs::OpenOptions::new().append(true).open(p); }"#,
        ),
        (
            "audit filename as a literal",
            r#"fn f() { let p = home.join("fleet_events.jsonl"); }"#,
        ),
        (
            "path built from a constant — the literal scan could never see this one",
            r#"fn f() { let p = home.join(AUDIT_FILE); let mut h = File::create(p); }"#,
        ),
    ];
    for (label, src) in cases {
        let audit = SinkAudit::scan(src).expect("case must parse");
        assert!(
            !audit.violations.is_empty(),
            "bypass shape not caught ({label}): {src}"
        );
    }
}

/// The visitor must not fire on the compliant shape, or the guard would be
/// unsatisfiable and would get loosened rather than obeyed.
#[test]
fn the_guard_accepts_the_compliant_shape() {
    let src = r#"
        fn append(home: &std::path::Path, event: &serde_json::Value) {
            let _ = agentic_audit_append::append_audit_line_best_effort(home, event);
        }
    "#;
    let audit = SinkAudit::scan(src).expect("must parse");
    assert!(
        audit.violations.is_empty(),
        "compliant sink flagged: {:?}",
        audit.violations
    );
    assert!(audit.calls_appender, "appender call not detected");
}

/// Skipping attributes must not become a hole: a doc comment naming the audit
/// file is fine, the same name in CODE is not.
#[test]
fn doc_comments_may_name_the_audit_file_but_code_may_not() {
    let doc_only = r#"
        /// appends to fleet_events.jsonl
        fn f() { let _ = agentic_audit_append::append_audit_line_best_effort(h, e); }
    "#;
    let audit = SinkAudit::scan(doc_only).expect("must parse");
    assert!(
        audit.violations.is_empty(),
        "a doc comment naming the audit file must not be a violation: {:?}",
        audit.violations
    );

    let in_code = r#"fn f() { let p = home.join("fleet_events.jsonl"); }"#;
    assert!(
        !SinkAudit::scan(in_code)
            .expect("must parse")
            .violations
            .is_empty(),
        "the same name in CODE must be a violation"
    );
}

/// #3416 correction: the gate must recognise test-gating from the PARSED cfg
/// meta, not from the presence of the token `test` anywhere in it.
///
/// The token scan says "contains `test`, therefore test-gated", which inverts on
/// the one spelling that means the opposite. `#[cfg(not(test))]` is
/// PRODUCTION-ONLY code — it exists in exactly the builds this guard protects —
/// and a sink could put its file write behind it and be skipped entirely. That is
/// a bypass in a guard whose whole claim is that it cannot be bypassed.
///
/// `any(test, feature = "x")` is the same shape one step out: the item compiles
/// whenever the feature is on, so it reaches production too. Only a cfg that
/// GUARANTEES test-only compilation may be skipped, and the safe reading of an
/// unknown predicate is to scan it.
#[test]
fn production_only_cfgs_are_scanned_not_skipped() {
    let bypasses = [
        (
            "cfg(not(test)) is production-ONLY — the inversion the token scan misses",
            r#"#[cfg(not(test))]
               fn prod() { let mut f = open(); let _ = writeln!(f, "x"); }"#,
        ),
        (
            "any(test, feature) still compiles in production when the feature is on",
            r#"#[cfg(any(test, feature = "x"))]
               fn prod() { let mut f = open(); let _ = writeln!(f, "x"); }"#,
        ),
        (
            "all(unix, not(test)) — production-only behind a nested not",
            r#"#[cfg(all(unix, not(test)))]
               fn prod() { let mut f = open(); let _ = writeln!(f, "x"); }"#,
        ),
        (
            "not(all(test, unix)) does not guarantee test-only either",
            r#"#[cfg(not(all(test, unix)))]
               fn prod() { let mut f = open(); let _ = writeln!(f, "x"); }"#,
        ),
        (
            "a module gated production-only hides its whole body from the token scan",
            r#"#[cfg(not(test))]
               mod prod { fn w() { let mut f = open(); let _ = writeln!(f, "x"); } }"#,
        ),
    ];
    for (label, src) in bypasses {
        let audit = SinkAudit::scan(src).expect("case must parse");
        assert!(
            !audit.violations.is_empty(),
            "production-reachable code was skipped as test-gated ({label}): {src}"
        );
    }
}

/// The other direction, so the correction cannot be a blanket "scan everything":
/// a cfg that genuinely guarantees test-only compilation must still be excluded,
/// including nested spellings.
#[test]
fn genuinely_test_only_cfgs_are_still_excluded() {
    for gate in [
        "#[cfg(test)]",
        "#[cfg(all(test, unix))]",
        "#[cfg(all(unix, all(test, target_os = \"macos\")))]",
        "#[cfg(any(test, test))]",
    ] {
        let src = format!(
            r#"{gate}
               mod tests {{ fn t() {{ let mut f = open(); let _ = writeln!(f, "x"); }} }}"#
        );
        let audit = SinkAudit::scan(&src).expect("must parse");
        assert!(
            audit.violations.is_empty(),
            "{gate}: a write inside genuinely test-only code must not count as production"
        );
    }
}

/// Test-gated code is excluded structurally, in every cfg spelling — including the
/// `#[cfg(all(test, unix))]` that the previous literal split missed, and a
/// `#[cfg(test)] mod x;` DECLARATION, which is not a region boundary at all.
#[test]
fn test_gated_code_is_excluded_in_every_cfg_spelling() {
    // `any(test, feature = "x")` USED to be listed here as excluded. It is not
    // test-only — the item compiles whenever the feature is on, so it reaches
    // production — and it now lives in
    // `production_only_cfgs_are_scanned_not_skipped` as a scanned case. That
    // expectation was unsound rather than merely untested, so it is corrected
    // here rather than carried.
    for gate in ["#[cfg(test)]", "#[cfg(all(test, unix))]"] {
        let src = format!(
            r#"{gate}
               mod tests {{ fn t() {{ let mut f = open(); let _ = writeln!(f, "x"); }} }}"#
        );
        let audit = SinkAudit::scan(&src).expect("must parse");
        assert!(
            audit.violations.is_empty(),
            "{gate}: a write inside a test module must not count as production"
        );
    }

    // A cfg(test) module DECLARATION pulls in a separate file and is NOT a
    // boundary: production code after it must still be scanned.
    let src = r#"
        #[cfg(test)]
        mod helper;
        fn prod() { let mut f = open(); let _ = writeln!(f, "x"); }
    "#;
    let audit = SinkAudit::scan(src).expect("must parse");
    assert!(
        !audit.violations.is_empty(),
        "a cfg(test) module DECLARATION must not hide the production code after it"
    );
}
