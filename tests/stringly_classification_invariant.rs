//! de2eb8 smells#2 Pattern-A: forbid classifying an ERROR by `.contains("…")`
//! substring of its message when a typed alternative exists.
//!
//! Two real bugs came from this anti-pattern (#1024 dropped `reviewed_head`;
//! #1833/HIGH-1 dropped dispatch directives), and bind_self
//! (`worktree.rs`) re-derived `ErrorCode` from `msg.contains("already leased")`
//! — misclassifying the lease-conflict producers whose message lacked that
//! phrase (de2eb8 finding #1). The fix is to dispatch on the TYPED field
//! (`ErrorCode`, `CiConclusion`, …) and let this guard stop the substring
//! pattern from creeping back into production.
//!
//! SCOPE (deliberately narrow, per the right-sized spike vet): production
//! (non-`#[cfg(test)]`) code only, and only the `<error-ish var>.contains("…")`
//! shape — NOT every `.contains` (the tree has ~1647, overwhelmingly legit
//! Vec/path/str matching + test assertions), and NOT the CI-conclusion
//! `== Some("success"/"failure")` pattern (deferred — it needs `CiConclusion`
//! adoption on the write side, a separate refactor).
//!
//! ESCAPE HATCH: a genuine string→type boundary (e.g. a third-party API that
//! exposes no typed error variant — teloxide delete; the lease layer's anyhow
//! string) marks the site with `// stringly-allow: <reason>` within the
//! preceding few lines. HONEST LIMIT: a grep can't catch a novel-literal
//! classifier on a non-error-named variable; the complete solution is typed
//! enums + a real clippy lint (the larger Pattern-A refactor, out of scope).

use std::path::{Path, PathBuf};

/// Error-ish receiver roots — the anti-pattern classifies a Rust error's
/// MESSAGE (where a typed `code`/variant exists), so the receiver is named like
/// one. Deliberately EXCLUDES `stdout`/`stderr`: parsing a subprocess's output
/// (launchctl/schtasks/git stderr) has no typed alternative and is a different,
/// legitimate concern — out of this lint's scope.
const ROOTS: &[&str] = &["msg", "err", "error", "reason"];

const ESCAPE: &str = "// stringly-allow:";

/// Whole test FILES (included via `#[cfg(test)] #[path="…"] mod …` from a
/// parent, so the `#[cfg(test)]` is NOT in the file itself) — their assertions
/// legitimately `.contains("…")` on error messages.
fn is_test_file(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name.starts_with("review_repro_")
        || p.components().any(|c| c.as_os_str() == "tests")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// The identifier immediately before a `.contains("` (a string-literal arg ⇒
/// `str::contains`, so the receiver is a string), or None.
fn classification_receiver(line: &str) -> Option<&str> {
    let needle = ".contains(\"";
    let pos = line.find(needle)?;
    let bytes = line.as_bytes();
    let mut start = pos;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    (start < pos).then(|| &line[start..pos])
}

fn receiver_is_error_ish(ident: &str) -> bool {
    ROOTS.iter().any(|r| {
        ident == *r || ident.ends_with(&format!("_{r}")) || ident.starts_with(&format!("{r}_"))
    })
}

/// Does this single line classify an error by substring? (Comment lines and
/// the escape hatch are handled by the caller's context scan.)
fn line_classifies_error(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('*') {
        return false;
    }
    // A test/invariant `assert!(err.contains("…"))` is an assertion, not
    // control-flow classification — and is robust against brace-mask drift when a
    // `#[cfg(test)]` lives inside a string fixture (claim_verifier).
    if line.contains("assert") {
        return false;
    }
    classification_receiver(line).is_some_and(receiver_is_error_ish)
}

/// Per-line `in #[cfg(test)] module` flags via brace tracking. Approximate
/// (ignores braces inside strings/comments) — adequate for an invariant scan,
/// matching the other source-scan guards in this suite.
fn test_module_mask(lines: &[&str]) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    let mut depth: i32 = 0;
    let mut pending_cfg_test = false;
    let mut in_test = false;
    let mut test_depth: i32 = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.contains("#[cfg(test)]") {
            pending_cfg_test = true;
        }
        let opens = line.matches('{').count() as i32;
        let closes = line.matches('}').count() as i32;
        if pending_cfg_test && line.contains("mod ") && line.contains('{') {
            in_test = true;
            test_depth = depth;
            pending_cfg_test = false;
        }
        mask[i] = in_test;
        depth += opens - closes;
        if in_test && depth <= test_depth {
            in_test = false;
        }
    }
    mask
}

/// True if `// stringly-allow:` appears on this line or the preceding `window`.
fn escape_hatched(lines: &[&str], idx: usize, window: usize) -> bool {
    let lo = idx.saturating_sub(window);
    lines[lo..=idx].iter().any(|l| l.contains(ESCAPE))
}

fn violations_in(src: &str) -> Vec<usize> {
    let lines: Vec<&str> = src.lines().collect();
    let mask = test_module_mask(&lines);
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if mask[i] {
            continue; // #[cfg(test)] code — test assertions on messages are fine
        }
        // Multi-line `assert!(\n    err.contains("…"))` — the `assert!` opener is
        // on a prior line, so also treat the receiver as an assertion (not
        // control-flow) when `assert` appears just above.
        let in_assert = lines[i.saturating_sub(2)..=i]
            .iter()
            .any(|l| l.contains("assert"));
        if line_classifies_error(line) && !in_assert && !escape_hatched(&lines, i, 12) {
            out.push(i + 1);
        }
    }
    out
}

#[test]
fn no_stringly_error_classification_in_production() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    assert!(!files.is_empty(), "no src files found");
    let mut findings = Vec::new();
    for f in &files {
        if is_test_file(f) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        for ln in violations_in(&src) {
            findings.push(format!("{}:{ln}", f.display()));
        }
    }
    assert!(
        findings.is_empty(),
        "stringly-typed error classification (`<err>.contains(\"…\")`) in production — \
         dispatch on the typed field (ErrorCode/CiConclusion/…) instead, or mark a genuine \
         string→type boundary with `// stringly-allow: <reason>`. Offenders:\n{}",
        findings.join("\n")
    );
}

/// Detector self-test: the guard FIRES on the anti-pattern (neuter-RED
/// equivalent) and does NOT false-fire on legit `.contains` / assertions /
/// escape-hatched boundaries.
#[test]
fn detector_fires_on_violation_and_not_on_legit() {
    // VIOLATIONS (production error-classification by substring):
    assert!(line_classifies_error(
        r#"    let code = if msg.contains("E4.5") {"#
    ));
    assert!(line_classifies_error(
        r#"        || err.contains("already leased")"#
    ));
    assert!(line_classifies_error(
        r#"    if err_msg.contains("no binding") {"#
    ));
    assert!(line_classifies_error(
        r#"    if reason.contains("cross-branch") {"#
    ));

    // NOT violations (legit):
    assert!(!line_classifies_error(
        r#"    if path.contains("/messages/") {"#
    )); // not error-ish
    assert!(!line_classifies_error(r#"    if text.contains("hello") {"#));
    assert!(!line_classifies_error(
        r#"    let v = names.contains("x");"#
    ));
    assert!(!line_classifies_error(
        r#"    // err.contains("x") in a comment"#
    )); // comment
    assert!(!line_classifies_error(r#"    let n = msg.len();"#)); // no .contains
    assert!(!line_classifies_error(
        r#"        assert!(err.contains("x"));"#
    )); // test assertion

    // Escape hatch + test-module stripping (whole-source level):
    let hatched =
        "fn f(msg: &str) {\n    // stringly-allow: boundary\n    if msg.contains(\"E4.5\") {}\n}\n";
    assert!(
        violations_in(hatched).is_empty(),
        "escape hatch must exempt"
    );

    let in_test =
        "#[cfg(test)]\nmod tests {\n    fn t(err: &str) { assert!(err.contains(\"x\")); }\n}\n";
    assert!(
        violations_in(in_test).is_empty(),
        "#[cfg(test)] assertions on messages must not be flagged"
    );

    let real = "fn f(msg: &str) -> bool {\n    msg.contains(\"E4.5\")\n}\n";
    assert_eq!(
        violations_in(real),
        vec![2],
        "a bare production classifier is flagged"
    );
}
