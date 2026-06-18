#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Governance D3 invariant: an instrument/audit emit MUST NEVER affect the
//! control-flow or exit code of the real operation it observes. r4 hand-traced
//! this line-by-line three times (#2158 bypass-audit, #2310 restart-instrument,
//! #2234 drift-WARN) asking "can this audit block the real op / change the exit
//! code?"; this turns that hand-check into a mechanical gate.
//!
//! Two complementary, NARROW checks:
//!
//! ## (B) — primary forcing function: instrument/audit EMIT fns return `()`
//!
//! Every emit in [`INSTRUMENT_EMIT_FNS`] (the side-effect-only audit/instrument
//! emitters from the reviewed PRs + the shadow-telemetry family) MUST:
//!   - return `()` — i.e. its signature has no `->`. A `()` fn is COMPILER-
//!     guaranteed un-`?`-able (`()` is not `Try`), so it can never propagate an
//!     error out of the real-op path and divert the caller's control-flow; AND
//!   - contain no `std::process::exit(` in its body — so it can never change the
//!     process exit code.
//!
//! This is the ENCOURAGED pattern: a named `()`-returning emit fn is
//! automatically control-flow-inert, so new instrumentation should be a fn added
//! to this list rather than an inline block.
//!
//! ## (A) — opt-in marker for INLINE instrument blocks
//!
//! An inline instrument/audit block (not a named emit fn — e.g. a bare
//! `tracing::info!` prelude) may be tagged with a `// instrument-only:` marker;
//! the block it tags (the next braced `{ … }`, else the next statement up to its
//! `;`) MUST contain no try-`?`, no `return`, no `process::exit`, no `break`/
//! `continue` — i.e. it is provably control-flow-inert.
//!
//! ⚠ The (A) marker is OPT-IN: an UNMARKED inline block is NOT scanned. So (A)
//! does NOT cover all inline instrumentation — it locks the specific reviewed
//! sites + documents the convention. Prefer (B) named `()`-fns (auto-covered);
//! reach for an (A) marker only when an inline block is genuinely unavoidable.
//!
//! ## Exemptions
//!
//! - `#[cfg(test)]` modules + test-only files (`*_tests.rs`, `tests.rs`, any path
//!   under `tests/`, `review_repro*`) — fixtures legitimately do anything.
//! - comment lines (a token named in prose is a claim, not code).
//! - a `// instrument-cf-exempt: <rationale>` marker on the call's line or the
//!   contiguous comment block immediately above it.

use std::path::{Path, PathBuf};

/// Side-effect-only audit/instrument EMIT fns. Each MUST return `()` (no `->`)
/// and contain no `process::exit`. Curated (narrow) — the emitters r4 reviewed
/// plus the shadow-telemetry family. NOT pure builders (e.g.
/// `build_bypass_audit_event` returns a value and is fine).
const INSTRUMENT_EMIT_FNS: &[&str] = &[
    "flush_daemon_log",                // #2310 restart-instrument log flush
    "log_nonagent_canonical_checkout", // #2234/#2235 shim canonical-checkout instrument
    "log_bypass_mutating_op",          // #2158/#2234 bypass audit emit
    "record_shadow_telemetry",         // state-detection shadow telemetry
    "capture_turn_sentinel_shadow",    // state-detection sentinel shadow capture
    "record_recovery_shadow",          // #t-81376 529-recovery failed-turn shadow emit
    "arm_expectation",                 // #t-81376 recovery-turn expectation shadow-arm
];

const INLINE_MARKER: &str = "// instrument-only:";
const EXEMPT_MARKER: &str = "instrument-cf-exempt:";

fn repo_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// A dedicated test/fixture file is fully exempt.
fn is_test_only_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.contains("/tests/")
        || p.ends_with("_tests.rs")
        || p.ends_with("/tests.rs")
        || p.contains("review_repro")
}

/// Blank out `#[cfg(test)]` module bodies (brace-matched), preserving line count
/// so reported line numbers stay accurate.
fn strip_cfg_test_modules(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("#[cfg(test)]") {
            let mut depth = 0i32;
            let mut opened = false;
            while i < lines.len() {
                for ch in lines[i].chars() {
                    if ch == '{' {
                        depth += 1;
                        opened = true;
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                out.push(String::new());
                i += 1;
                if opened && depth <= 0 {
                    break;
                }
            }
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    out.join("\n")
}

/// Remove `// …` line comments AND `"…"` string literals from a single line so a
/// token (e.g. `?` or `return`) mentioned inside a log message or a comment is
/// not read as code. Naive but sufficient for the instrument blocks in scope
/// (no raw-string / multiline-string instrument bodies exist).
fn strip_strings_and_comments(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if c == '\\' {
                i += 2; // skip escaped char
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
            break; // rest of line is a comment
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Extract a `fn <name>(` definition's signature (up to the body's opening `{`)
/// and body (brace-matched), starting at `start`. Returns (signature, body,
/// end_line_idx) or None if no body brace is found within a bounded window.
fn gather_fn(lines: &[&str], start: usize) -> Option<(String, String, usize)> {
    let mut sig = String::new();
    let mut found_open = false;
    let mut j = start;
    // Signature: accumulate until the first `{` (the body open).
    while j < lines.len() && j < start + 30 {
        let code = strip_strings_and_comments(lines[j]);
        if let Some(idx) = code.find('{') {
            sig.push_str(&code[..idx]);
            found_open = true;
            break;
        }
        sig.push_str(&code);
        sig.push(' ');
        j += 1;
    }
    if !found_open {
        return None;
    }
    // Body: brace-match from line `j`.
    let mut depth = 0i32;
    let mut body = String::new();
    let mut started = false;
    let mut k = j;
    while k < lines.len() {
        let code = strip_strings_and_comments(lines[k]);
        for ch in code.chars() {
            if ch == '{' {
                depth += 1;
                started = true;
            } else if ch == '}' {
                depth -= 1;
            }
        }
        body.push_str(&code);
        body.push('\n');
        if started && depth <= 0 {
            return Some((sig, body, k));
        }
        k += 1;
    }
    None
}

/// (A) Gather the region a `// instrument-only:` marker tags: the next braced
/// `{ … }` block if the next code is a block, else the next statement up to its
/// depth-0 `;`. Returns the region's code (strings/comments stripped).
fn gather_marked_region(lines: &[&str], marker_line: usize) -> String {
    // Find the first subsequent line that has non-comment code.
    let mut i = marker_line + 1;
    while i < lines.len() && strip_strings_and_comments(lines[i]).trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return String::new();
    }
    let mut region = String::new();
    let mut depth = 0i32;
    let mut saw_brace = false;
    let mut saw_semi_at_zero = false;
    let mut k = i;
    while k < lines.len() && k < i + 40 {
        let code = strip_strings_and_comments(lines[k]);
        region.push_str(&code);
        region.push('\n');
        for ch in code.chars() {
            match ch {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                ';' if depth == 0 => saw_semi_at_zero = true,
                _ => {}
            }
            if ch == '{' {
                saw_brace = true;
            }
        }
        // Block form: closed all braces we opened. Statement form: hit a depth-0 `;`.
        if (saw_brace && depth <= 0) || (!saw_brace && saw_semi_at_zero) {
            break;
        }
        k += 1;
    }
    region
}

/// True if `region` (already string/comment-stripped) contains a control-flow
/// diversion forbidden inside an instrument block.
fn region_has_control_flow(region: &str) -> Vec<String> {
    let mut hits = Vec::new();
    if region.contains('?') {
        hits.push("try-operator `?`".to_string());
    }
    if region.contains("process::exit") {
        hits.push("`process::exit`".to_string());
    }
    // Word-boundary-ish keyword checks (region is code-only after stripping).
    for kw in ["return", "break", "continue"] {
        if region
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|tok| tok == kw)
        {
            hits.push(format!("`{kw}`"));
        }
    }
    hits
}

/// An `// instrument-cf-exempt:` marker on this line or the contiguous comment
/// block immediately above suppresses a finding (escape valve for a genuine
/// exception, with rationale).
fn is_exempted(lines: &[&str], idx: usize) -> bool {
    if lines.get(idx).is_some_and(|l| l.contains(EXEMPT_MARKER)) {
        return true;
    }
    let mut j = idx;
    while j > 0 {
        j -= 1;
        let t = lines[j].trim_start();
        if t.starts_with("//") {
            if t.contains(EXEMPT_MARKER) {
                return true;
            }
        } else if !t.is_empty() {
            break;
        }
    }
    false
}

/// Run both checks over a single file's content. Returns human-readable
/// violations. `scan_text` lets the self-test feed a synthetic body.
fn scan_content(rel_path: &str, content: &str) -> Vec<String> {
    let stripped = strip_cfg_test_modules(content);
    let lines: Vec<&str> = stripped.lines().collect();
    let mut violations = Vec::new();

    for (idx, raw) in lines.iter().enumerate() {
        let code = strip_strings_and_comments(raw);

        // (B) emit-fn def-site + body.
        for emit in INSTRUMENT_EMIT_FNS {
            let needle = format!("fn {emit}(");
            if code.contains(&needle) {
                if is_exempted(&lines, idx) {
                    continue;
                }
                if let Some((sig, body, _)) = gather_fn(&lines, idx) {
                    if sig.contains("->") {
                        violations.push(format!(
                            "{rel_path}:{}: instrument/audit emit `{emit}` MUST return `()` \
                             (no `->`) so it is compiler-guaranteed un-`?`-able and cannot \
                             divert the real-op control-flow",
                            idx + 1
                        ));
                    }
                    if body.contains("process::exit") {
                        violations.push(format!(
                            "{rel_path}:{}: instrument/audit emit `{emit}` body contains \
                             `process::exit` — an instrument must never change the exit code",
                            idx + 1
                        ));
                    }
                }
            }
        }

        // (A) inline marker region.
        if raw.contains(INLINE_MARKER) {
            let region = gather_marked_region(&lines, idx);
            for hit in region_has_control_flow(&region) {
                violations.push(format!(
                    "{rel_path}:{}: `// instrument-only:` block contains {hit} — an inline \
                     instrument block must be control-flow-inert (no `?`/return/exit/break/continue)",
                    idx + 1
                ));
            }
        }
    }
    violations
}

#[test]
fn instrument_audit_paths_never_affect_control_flow() {
    let src = repo_src_dir();
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "found no src/*.rs files to scan");

    let mut violations = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(&src)
            .unwrap_or(f)
            .to_string_lossy()
            .to_string();
        if is_test_only_file(&f.to_string_lossy()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        violations.extend(scan_content(&format!("src/{rel}"), &content));
    }

    assert!(
        violations.is_empty(),
        "Governance D3 — instrument/audit paths must never affect control-flow/exit-code:\n{}",
        violations.join("\n")
    );
}

// ── Self-tests: prove the scan is NOT vacuous (it actually flags violations). ──

#[test]
fn selftest_b_flags_emit_fn_returning_non_unit() {
    // A listed emit fn declared `-> bool` must be flagged.
    let bad = "fn flush_daemon_log() -> bool {\n    true\n}\n";
    let hits = scan_content("src/fake.rs", bad);
    assert!(
        hits.iter()
            .any(|h| h.contains("flush_daemon_log") && h.contains("()")),
        "B-check must flag a non-`()` emit fn; got {hits:?}"
    );
}

#[test]
fn selftest_b_flags_emit_fn_with_process_exit() {
    let bad = "fn log_bypass_mutating_op() {\n    std::process::exit(1);\n}\n";
    let hits = scan_content("src/fake.rs", bad);
    assert!(
        hits.iter()
            .any(|h| h.contains("log_bypass_mutating_op") && h.contains("exit")),
        "B-check must flag `process::exit` in an emit fn body; got {hits:?}"
    );
}

#[test]
fn selftest_a_flags_try_in_marked_block() {
    let bad = "    // instrument-only: observe then proceed\n    {\n        do_audit()?;\n    }\n";
    let hits = scan_content("src/fake.rs", bad);
    assert!(
        hits.iter()
            .any(|h| h.contains("instrument-only") && h.contains("try-operator")),
        "A-check must flag a `?` inside a marked block; got {hits:?}"
    );
}

#[test]
fn selftest_a_flags_return_in_marked_statement() {
    let bad = "    // instrument-only: log\n    return audit_value();\n";
    let hits = scan_content("src/fake.rs", bad);
    assert!(
        hits.iter()
            .any(|h| h.contains("instrument-only") && h.contains("return")),
        "A-check must flag a `return` in a marked statement; got {hits:?}"
    );
}

#[test]
fn selftest_clean_instrument_passes() {
    // A `()` emit fn with a plain tracing call + a marked inert tracing block:
    // zero violations.
    let good = "fn flush_daemon_log() {\n    tracing::info!(\"flushed?\");\n}\n\
                fn caller() {\n    // instrument-only: marks the gap\n    \
                tracing::info!(event = \"x\", \"why? because\");\n}\n";
    let hits = scan_content("src/fake.rs", good);
    assert!(hits.is_empty(), "clean instrument must pass; got {hits:?}");
}

#[test]
fn selftest_exempt_marker_suppresses_b() {
    let bad = "// instrument-cf-exempt: legacy emitter, tracked in #9999\n\
               fn flush_daemon_log() -> bool {\n    true\n}\n";
    let hits = scan_content("src/fake.rs", bad);
    assert!(hits.is_empty(), "exempt marker must suppress; got {hits:?}");
}
