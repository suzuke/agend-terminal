#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Sprint 53 P0-3: anti-pattern lint gates.
//!
//! Two CI lints that trip future PRs at gate time, not in production smoke:
//!
//! - **Rule 1: dead-code-helper-pattern** — a test fn named identically to a
//!   `pub`/`pub(crate)`/`pub(super)` fn in `src/` shadows the production
//!   symbol at the call site (Rust resolves to the closest scope). Removing
//!   or refactoring the production fn no longer fails the test, because the
//!   test is silently calling the local helper. Caught in P0-1 r1.
//!
//! - **Rule 2: shared-source_repo-fleet-yaml-pattern** — a test fleet.yaml
//!   fixture with two `instances:` entries pointing at the same
//!   `working_directory:` masks production cross-agent topology (each agent
//!   has its own clone). Caught in P0-1 production smoke (Test 2), led to
//!   P0-1.5 central-lease-registry fix.
//!
//! Allowlist mechanism: prefix the offending line (or the line immediately
//! above) with `// allow: dead-code-helper` or `// allow: shared-source_repo`
//! and the lint skips it. Always include a rationale next to the comment so
//! future readers understand why the exception is justified.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SRC_DIR: &str = "src";
const TESTS_DIR: &str = "tests";

const ALLOW_RULE_1: &str = "allow: dead-code-helper";
const ALLOW_RULE_2: &str = "allow: shared-source_repo";

// ── shared file walking ────────────────────────────────────────────

fn rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    walk(root, &mut out);
    out
}

/// True if the lint should skip this file. The lint file itself is excluded
/// (its `fn` definitions trivially collide with itself if matched against
/// a hypothetical future `pub fn` in src/ — but more importantly, the
/// allowlist comments inside the lint's own helper-fixture strings would
/// otherwise interfere with the regex.
fn is_self(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("anti_pattern_invariant.rs")
}

/// True iff the violation line (or any contiguous `//` comment line
/// immediately preceding it) carries the given allow-marker comment.
/// Walks upward through comment lines until hitting a non-comment line, so
/// a multi-line rationale block above the offending code keeps working.
fn has_allowlist_marker(content_lines: &[&str], line_idx: usize, marker: &str) -> bool {
    if content_lines
        .get(line_idx)
        .map(|l| l.contains(marker))
        .unwrap_or(false)
    {
        return true;
    }
    let mut cursor = line_idx;
    while let Some(prev_idx) = cursor.checked_sub(1) {
        let Some(prev_line) = content_lines.get(prev_idx) else {
            break;
        };
        let trimmed = prev_line.trim_start();
        if !(trimmed.starts_with("//") || trimmed.is_empty()) {
            break;
        }
        if trimmed.contains(marker) {
            return true;
        }
        cursor = prev_idx;
    }
    false
}

// ── Rule 1: dead-code-helper-pattern ───────────────────────────────

/// Extract names of column-0 `pub`, `pub(crate)`, `pub(super)`, and
/// `pub(in ...)` fn declarations from a Rust source file's text. Matches
/// sync, `async`, and `const fn`.
///
/// r1 fix (PR #471 reviewer): the **column-0** filter excludes `pub fn`
/// methods nested inside `impl Type { ... }` blocks. Without this filter,
/// a test helper named the same as an impl method gets falsely flagged —
/// calling `helper_name()` in test code never shadows `Type::helper_name()`,
/// since the latter requires receiver-method syntax. Free functions are
/// conventionally column-0 in Rust style; an indented public free fn would
/// already be a refactor candidate.
fn extract_exposed_fn_names(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        // Require `pub`/`pub(...)` at column 0 — symmetric with the
        // test-side `extract_all_fn_definitions` filter. Indented `pub fn`
        // inside `impl Type {}` blocks is excluded.
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let after_pub = if let Some(rest) = line.strip_prefix("pub ") {
            Some(rest.trim_start())
        } else if let Some(rest) = line.strip_prefix("pub(") {
            // pub(crate) / pub(super) / pub(in ::path)
            rest.find(')').map(|i| rest[i + 1..].trim_start())
        } else {
            None
        };
        let Some(after_pub) = after_pub else {
            continue;
        };
        if let Some(name) = parse_fn_name(after_pub) {
            out.push(name);
        }
    }
    out
}

/// Extract every **free** `fn <name>(...)` declaration the file defines.
/// Free = at column 0 (no leading whitespace before `pub`/`fn`). Impl
/// methods (`impl Foo { fn new(...) {} }` indented inside the block) are
/// scoped to their type and don't shadow production free fns at the call
/// site, so they're skipped to avoid false positives.
///
/// Returns (name, 1-based line, line_text).
fn extract_all_fn_definitions(content: &str) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        // Free-fn filter: line must start at column 0. Indented fns are
        // either impl methods (false positive) or nested fns (also scoped).
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let mut rest = line;
        if let Some(after) = line.strip_prefix("pub ") {
            rest = after.trim_start();
        } else if let Some(after) = line.strip_prefix("pub(") {
            if let Some(i) = after.find(')') {
                rest = after[i + 1..].trim_start();
            }
        }
        if let Some(name) = parse_fn_name(rest) {
            out.push((name, idx + 1, line.to_string()));
        }
    }
    out
}

/// Given the source text immediately after an optional `pub*` modifier,
/// return the fn name when the line declares one. Skips trait-method
/// signatures inside `impl` blocks by requiring the bare `fn ` token to be
/// at the start of the (possibly modifier-stripped) trimmed line.
fn parse_fn_name(s: &str) -> Option<String> {
    let s = s.trim_start();
    // Allow async / const / unsafe / extern modifiers in any order.
    let mut s = s;
    loop {
        let t = s.trim_start();
        s = if let Some(r) = t.strip_prefix("async ") {
            r
        } else if let Some(r) = t.strip_prefix("const ") {
            r
        } else if let Some(r) = t.strip_prefix("unsafe ") {
            r
        } else if let Some(r) = t.strip_prefix("extern ") {
            // skip optional ABI string: extern "C" fn ...
            let r = r.trim_start();
            if let Some(after_quote) = r.strip_prefix('"') {
                if let Some(end) = after_quote.find('"') {
                    after_quote[end + 1..].trim_start()
                } else {
                    r
                }
            } else {
                r
            }
        } else {
            break;
        };
    }
    let rest = s.strip_prefix("fn ")?;
    let rest = rest.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Run Rule 1 against the given `src/` and `tests/` roots. Returns one
/// human-readable violation string per offending test fn. Empty result
/// means the lint passes.
fn rule1_violations(src_root: &Path, tests_root: &Path) -> Vec<String> {
    // 1. Collect production exposed fn names from every .rs under src/.
    //    Skip files inside `mod tests` blocks at file scope — those are
    //    test-only and should not be considered "production" symbols.
    //    Heuristic: skip files ending in `tests.rs` and files under any
    //    `tests/` sub-directory (`src/foo/tests/bar.rs`, etc.) — these
    //    are conventional test-module locations in this codebase.
    let mut prod_names: BTreeSet<String> = BTreeSet::new();
    for path in rs_files(src_root) {
        if is_test_module_path(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for name in extract_exposed_fn_names(&content) {
            prod_names.insert(name);
        }
    }

    // 2. Walk tests/ and report any fn that matches a production name.
    let mut violations = Vec::new();
    for path in rs_files(tests_root) {
        if is_self(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (name, lineno, line_text) in extract_all_fn_definitions(&content) {
            if !prod_names.contains(&name) {
                continue;
            }
            // 0-based index for marker lookup.
            let idx = lineno - 1;
            if has_allowlist_marker(&lines, idx, ALLOW_RULE_1) {
                continue;
            }
            violations.push(format!(
                "{path}:{lineno}: test fn `{name}` shadows a production fn — \
                 calls bind locally, hiding production-symbol removal. \
                 Rename the helper or add `// {ALLOW_RULE_1}` with rationale.\n  \
                 line: {line}",
                path = path.display(),
                line = line_text.trim_end(),
            ));
        }
    }
    violations
}

/// Returns true for paths that are conventionally test-only:
/// `*/tests.rs`, `*/tests/*.rs`, file basename starts with `test_`.
fn is_test_module_path(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
        return true;
    }
    path.components()
        .any(|c| c.as_os_str() == "tests" || c.as_os_str() == "test")
}

// ── Rule 2: shared source_repo / fleet.yaml ────────────────────────

/// Detect `format!("…working_directory: {}…working_directory: {}…", a, b)`
/// where `a` and `b` are textually identical. That's the canonical shape of
/// the bug P0-1 production smoke caught: two agents bound to the same
/// source repo, masking the cross-agent topology.
///
/// Multi-line `format!()` calls are out of scope for this linter — every
/// occurrence in the codebase as of P0-3 dispatch fits on a single line.
/// If a future contributor spreads the call across lines, the lint won't
/// catch it; track that in the documented limitation rather than chasing
/// a fragile multi-line parser.
fn rule2_violations_in(file: &Path, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    for (idx, raw_line) in lines.iter().enumerate() {
        let line = *raw_line;
        if !line.contains("working_directory") || !line.contains("{}") {
            continue;
        }
        // Need at least two `working_directory: {}` placeholders.
        if line.matches("working_directory: {}").count() < 2 {
            continue;
        }
        // Allowlist: check this line and the line above.
        if has_allowlist_marker(&lines, idx, ALLOW_RULE_2) {
            continue;
        }
        // Find the closing of the format-string, then split args by
        // top-level commas. Heuristic: the first `",` after the `format!(`
        // or `format!  ("…` ends the string. Naïve but adequate for the
        // shape we want to flag.
        let Some(string_close) = line.find("\",") else {
            continue;
        };
        let args_str = line[string_close + 2..].trim();
        let args_str = args_str
            .trim_end_matches(';')
            .trim_end_matches(')')
            .trim_end_matches(',')
            .trim();
        let args = split_top_level_args(args_str);
        // Aggregate identical args.
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for a in &args {
            let k = a.trim().to_string();
            if k.is_empty() {
                continue;
            }
            *counts.entry(k).or_insert(0) += 1;
        }
        let mut dups: Vec<&String> = counts
            .iter()
            .filter(|(_, n)| **n >= 2)
            .map(|(k, _)| k)
            .collect();
        dups.sort();
        if dups.is_empty() {
            continue;
        }
        out.push(format!(
            "{path}:{lineno}: fleet.yaml format!() repeats the same \
             working_directory expression for multiple `instances:` — \
             tests must give each agent its own source repo to mirror \
             production topology. Add `// {ALLOW_RULE_2}` with rationale \
             if intentional (e.g. testing the central lease registry).\n  \
             repeated arg(s): {dups}",
            path = file.display(),
            lineno = idx + 1,
            dups = dups
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    out
}

fn split_top_level_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_bracket = 0i32;
    let mut in_str = false;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '"' if i == 0 || bytes[i - 1] != b'\\' => in_str = !in_str,
            '(' if !in_str => depth_paren += 1,
            ')' if !in_str => {
                if depth_paren == 0 {
                    // Closing paren of the enclosing format!() call —
                    // everything after this is no longer arg territory.
                    if start < i {
                        out.push(s[start..i].to_string());
                    }
                    return out;
                }
                depth_paren -= 1;
            }
            '[' if !in_str => depth_bracket += 1,
            ']' if !in_str => depth_bracket -= 1,
            ',' if !in_str && depth_paren == 0 && depth_bracket == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < s.len() {
        out.push(s[start..].to_string());
    }
    out
}

fn rule2_violations(roots: &[&Path]) -> Vec<String> {
    let mut out = Vec::new();
    for root in roots {
        for path in rs_files(root) {
            if is_self(&path) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            out.extend(rule2_violations_in(&path, &content));
        }
    }
    out
}

// ── Tests: end-to-end against current codebase ──────────────────────

#[test]
fn no_dead_code_helper_pattern() {
    let v = rule1_violations(Path::new(SRC_DIR), Path::new(TESTS_DIR));
    assert!(
        v.is_empty(),
        "Rule 1 violations — test fns shadowing production fn names:\n{}",
        v.join("\n")
    );
}

#[test]
fn no_shared_source_repo_pattern() {
    let v = rule2_violations(&[Path::new(SRC_DIR), Path::new(TESTS_DIR)]);
    assert!(
        v.is_empty(),
        "Rule 2 violations — fleet.yaml fixtures with shared working_directory:\n{}",
        v.join("\n")
    );
}

// ── Tests: synthetic fixtures (pos + neg) ───────────────────────────

#[test]
fn rule1_detects_helper_matching_production_name() {
    // Synthetic test file defining a fn named after a real production fn.
    let tmp = tempdir();
    let src = tmp.join("src");
    let tests = tmp.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn dispatch_auto_bind_lease() {}\n").unwrap();
    std::fs::write(
        tests.join("integration.rs"),
        "fn dispatch_auto_bind_lease() {}\n#[test]\nfn t() {}\n",
    )
    .unwrap();
    let v = rule1_violations(&src, &tests);
    assert!(
        !v.is_empty(),
        "Rule 1 must flag a test helper sharing a production fn name"
    );
    assert!(
        v[0].contains("dispatch_auto_bind_lease"),
        "violation must name the offending fn: {}",
        v[0]
    );
    cleanup(&tmp);
}

#[test]
fn rule1_passes_when_helper_name_is_test_only() {
    // Test helper named `setup_test_repo` does NOT shadow any production fn.
    let tmp = tempdir();
    let src = tmp.join("src");
    let tests = tmp.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn dispatch_auto_bind_lease() {}\n").unwrap();
    std::fs::write(
        tests.join("integration.rs"),
        "fn setup_test_repo() {}\nfn tmp_home() {}\n#[test]\nfn t() {}\n",
    )
    .unwrap();
    let v = rule1_violations(&src, &tests);
    assert!(
        v.is_empty(),
        "Rule 1 must NOT flag pure test helpers: {v:?}"
    );
    cleanup(&tmp);
}

#[test]
fn rule1_ignores_pub_fn_impl_method() {
    // r1 reviewer regression-proof (PR #471): a `pub fn` nested inside
    // `impl Type {}` is a method, not a free function. Calling
    // `helper_name()` in test code never shadows `Type::helper_name()`,
    // so the lint must NOT flag a test helper that happens to share the
    // method's bare name. The column-0 filter on `extract_exposed_fn_names`
    // is what enforces this.
    let tmp = tempdir();
    let src = tmp.join("src");
    let tests = tmp.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub struct Thing;\nimpl Thing {\n    pub fn helper_name(&self) {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        tests.join("integration.rs"),
        "fn helper_name() {}\n#[test]\nfn t() {}\n",
    )
    .unwrap();
    let v = rule1_violations(&src, &tests);
    assert!(
        v.is_empty(),
        "Rule 1 must NOT flag impl-method/test-helper name collisions: {v:?}"
    );
    cleanup(&tmp);
}

#[test]
fn rule1_still_flags_real_free_fn_shadow() {
    // Positive control: with the column-0 filter, a genuine `pub fn` at
    // module scope MUST still trip the lint when the test redefines its
    // name. Asserts the r1 fix doesn't over-narrow detection.
    let tmp = tempdir();
    let src = tmp.join("src");
    let tests = tmp.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn module_level_fn() {}\nimpl SomeType { pub fn nested(&self) {} }\n",
    )
    .unwrap();
    std::fs::write(
        tests.join("integration.rs"),
        "fn module_level_fn() {}\n#[test]\nfn t() {}\n",
    )
    .unwrap();
    let v = rule1_violations(&src, &tests);
    assert!(
        !v.is_empty(),
        "Rule 1 must still flag column-0 pub fn shadowed by test helper"
    );
    assert!(
        v[0].contains("module_level_fn"),
        "violation must name the module-level fn: {}",
        v[0]
    );
    cleanup(&tmp);
}

#[test]
fn rule1_respects_allowlist_marker() {
    // A clearly-named violation can be bypassed with `// allow: dead-code-helper`.
    let tmp = tempdir();
    let src = tmp.join("src");
    let tests = tmp.join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn shared_name() {}\n").unwrap();
    std::fs::write(
        tests.join("integration.rs"),
        "// allow: dead-code-helper (intentional shadow for test isolation)\n\
         fn shared_name() {}\n#[test]\nfn t() {}\n",
    )
    .unwrap();
    let v = rule1_violations(&src, &tests);
    assert!(v.is_empty(), "allowlist marker must suppress: {v:?}");
    cleanup(&tmp);
}

#[test]
fn rule2_detects_shared_working_directory() {
    // Synthetic source with the exact format!() shape lead pointed out.
    let tmp = tempdir();
    let f = tmp.join("test.rs");
    std::fs::write(
        &f,
        "fn t() { format!(\"instances:\\n  agent-a:\\n    working_directory: {}\\n  agent-b:\\n    working_directory: {}\\n\", repo.display(), repo.display()); }\n",
    )
    .unwrap();
    let content = std::fs::read_to_string(&f).unwrap();
    let v = rule2_violations_in(&f, &content);
    assert!(
        !v.is_empty(),
        "Rule 2 must flag duplicate working_directory args"
    );
    assert!(
        v[0].contains("repo.display()"),
        "violation must name the duplicate expression: {}",
        v[0]
    );
    cleanup(&tmp);
}

#[test]
fn rule2_passes_when_each_agent_has_own_dir() {
    let tmp = tempdir();
    let f = tmp.join("test.rs");
    std::fs::write(
        &f,
        "fn t() { format!(\"instances:\\n  agent-a:\\n    working_directory: {}\\n  agent-b:\\n    working_directory: {}\\n\", path_a.display(), path_b.display()); }\n",
    )
    .unwrap();
    let content = std::fs::read_to_string(&f).unwrap();
    let v = rule2_violations_in(&f, &content);
    assert!(
        v.is_empty(),
        "Rule 2 must NOT flag distinct working_directory args: {v:?}"
    );
    cleanup(&tmp);
}

#[test]
fn rule2_respects_allowlist_marker() {
    let tmp = tempdir();
    let f = tmp.join("test.rs");
    std::fs::write(
        &f,
        "// allow: shared-source_repo (testing central lease registry cross-agent collision)\n\
         fn t() { format!(\"instances:\\n  agent-a:\\n    working_directory: {}\\n  agent-b:\\n    working_directory: {}\\n\", repo.display(), repo.display()); }\n",
    )
    .unwrap();
    let content = std::fs::read_to_string(&f).unwrap();
    let v = rule2_violations_in(&f, &content);
    assert!(v.is_empty(), "allowlist marker must suppress: {v:?}");
    cleanup(&tmp);
}

#[test]
fn parse_fn_name_handles_modifier_stack() {
    assert_eq!(parse_fn_name("fn foo()"), Some("foo".to_string()));
    assert_eq!(parse_fn_name("async fn bar()"), Some("bar".to_string()));
    assert_eq!(parse_fn_name("const fn baz()"), Some("baz".to_string()));
    assert_eq!(parse_fn_name("unsafe fn quux()"), Some("quux".to_string()));
    assert_eq!(
        parse_fn_name("extern \"C\" fn cfun()"),
        Some("cfun".to_string())
    );
    assert_eq!(parse_fn_name("struct NotFn {}"), None);
}

// ── tempdir helper (no extra dep beyond stdlib) ─────────────────────

fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "agend-anti-pattern-lint-{}-{}",
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn cleanup(p: &Path) {
    std::fs::remove_dir_all(p).ok();
}
