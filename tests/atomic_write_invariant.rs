#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Governance D2 invariant: PRODUCTION code MUST publish daemon/agent STATE
//! files via `store::atomic_write` (unique-tmp + rename), never a plain
//! `std::fs::write` / `fs::write`. A non-atomic write is readable mid-write
//! (torn / truncated) by a concurrent reader, which then parse-fails — e.g. a
//! half-written `.daemon` `pid:now:token` identity record, or a truncated
//! `{name}.port`. #2315 fixed the last production stragglers (`.daemon`,
//! `.port`); this invariant prevents the class from regressing.
//!
//! ## Scope — NARROW (footgun-only)
//!
//! Per the governance balance (only mechanise a demonstrated footgun, never
//! block a capable op): a plain `fs::write` is flagged ONLY when its target
//! path references a known STATE-FILE surface — the daemon run-dir lifecycle +
//! identity + cross-agent coordination files in [`STATE_FILE_TOKENS`], whose
//! readers expect a complete document at all times.
//!
//! Intentionally OUT of scope (NOT footguns — regenerated/idempotent or
//! append-structured, so a plain write is fine): generated config the daemon
//! re-extracts each boot (`.default` protocol, agent instructions,
//! `.gitignore`), append-only logs (`*.jsonl` event-log / inbox), `fleet.yaml`
//! (its own lock), and scratch/tmp. The agent `metadata/` store is handled
//! separately (its fix involves cross-platform symlink/rename, not a bare
//! atomic_write — staleness A4), so it is excluded here to avoid overlap.
//!
//! ## Exemptions
//!
//! - `src/store.rs` — the atomic_write primitive itself (it owns the raw tmp
//!   write + rename).
//! - `#[cfg(test)]` modules and test-only files (`*_tests.rs`, `review_repro_*`,
//!   any path under `tests/`) — fixtures legitimately hand-write fake state.
//! - comment lines (a `fs::write` named in prose is a claim, not a call).
//! - a `// atomic-write-exempt: <rationale>` marker on the call's line or in the
//!   contiguous comment block immediately above it.

use std::path::{Path, PathBuf};

/// State-file surfaces whose readers require a complete document at all times.
/// A plain `fs::write` whose path argument references one of these is the
/// torn-read footgun this invariant forbids in production.
const STATE_FILE_TOKENS: &[&str] = &[
    ".daemon",                // daemon identity (pid:now:token) — #2315 / #2170
    ".port",                  // per-agent + api.port liveness — #2315
    "api.cookie",             // daemon API shared secret
    ".ready",                 // boot-complete signal — #922
    "binding.json",           // agent↔worktree binding
    "topics.json",            // telegram topic registry
    "dispatch_tracking.json", // dispatch sidecars
    "usage_limit_notify",     // quota-notify suppression state
    "session.json",           // TUI layout restore state (already atomic — guard)
];

const EXEMPT_MARKER: &str = "atomic-write-exempt:";
/// The atomic_write primitive itself owns the raw tmp write + rename.
const PRIMITIVE_FILE: &str = "store.rs";

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

/// A path is test-only (exempt) when it is a dedicated test/fixture file. Mixed
/// production files keep their `#[cfg(test)]` modules stripped separately below.
fn is_test_only_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.contains("/tests/")
        || p.ends_with("_tests.rs")
        || p.ends_with("/tests.rs") // `#[cfg(test)] mod tests;` in a separate file
        || p.contains("review_repro")
        || p.ends_with("atomic_write_invariant.rs") // this file's own fixtures
        || p.ends_with(PRIMITIVE_FILE)
}

/// Drop inline `"..."` string contents + trailing `// ...` comments so the
/// brace/semicolon CLASSIFICATION below isn't fooled by a `{`/`;` inside a
/// string or comment. Used ONLY for the structural `#[cfg(test)]` scan — NOT
/// for fs::write/token detection, which must keep string contents (the
/// `".daemon"` path literal lives in a string).
fn code_only(line: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let mut esc = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            break;
        }
        out.push(c);
    }
    out
}

/// Blank out a `#[cfg(test)]` item's lines so test fixtures that legitimately
/// hand-write fake state files don't trip the scan — WITHOUT over-stripping
/// production code. Each `#[cfg(test)]` attribute is bounded to its OWN item: a
/// braced item (`mod` / `fn` / `impl` / `struct {}`) is brace-matched; a
/// statement item (`use ...;`, `const ...;`) ends at its `;`. Line numbers are
/// preserved (offending content → empty lines).
///
/// #2323-r6 fix: the prior version scanned forward to "the next brace block"
/// from ANY `#[cfg(test)]` line — so a NON-module `#[cfg(test)] use ...;`
/// blanked everything up to a far-off brace, hiding a production state-file
/// write placed after it (a false negative that defeats the forcing function).
fn strip_cfg_test_modules(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("#[cfg(test)]") {
            // Classify the attributed item: which comes first, `{` or `;`?
            let mut braced = false;
            'scan: for line in lines.iter().skip(i) {
                for ch in code_only(line).chars() {
                    if ch == '{' {
                        braced = true;
                        break 'scan;
                    }
                    if ch == ';' {
                        braced = false;
                        break 'scan;
                    }
                }
            }
            if braced {
                // Brace-match the item body.
                let mut depth = 0i32;
                let mut opened = false;
                let mut k = i;
                while k < lines.len() {
                    for ch in code_only(lines[k]).chars() {
                        if ch == '{' {
                            depth += 1;
                            opened = true;
                        } else if ch == '}' {
                            depth -= 1;
                        }
                    }
                    out.push(String::new());
                    k += 1;
                    if opened && depth <= 0 {
                        break;
                    }
                }
                i = k;
            } else {
                // Statement item: blank through the line carrying its `;` only.
                let mut k = i;
                loop {
                    let has_semi = code_only(lines[k]).contains(';');
                    out.push(String::new());
                    k += 1;
                    if has_semi || k >= lines.len() {
                        break;
                    }
                }
                i = k;
            }
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    out.join("\n")
}

/// Strip a trailing `// ...` line comment (so a `fs::write` mentioned in prose
/// is not a call). Naive but sufficient: a `//` inside a string literal before a
/// real `fs::write(` on the same line is not a pattern that occurs in practice.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Accumulate a `fs::write(` call's text from `start` until its parens balance,
/// returning (call_text, lines_consumed). Bounded so a malformed file can't loop.
fn gather_call(lines: &[&str], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut depth = 0i32;
    let mut started = false;
    let mut consumed = 0usize;
    for line in lines.iter().skip(start).take(40) {
        let code = strip_line_comment(line);
        text.push_str(code);
        text.push(' ');
        consumed += 1;
        for ch in code.chars() {
            if ch == '(' {
                depth += 1;
                started = true;
            } else if ch == ')' {
                depth -= 1;
            }
        }
        if started && depth <= 0 {
            break;
        }
    }
    (text, consumed.max(1))
}

/// Whether `call_text` (a gathered `fs::write(...)` statement) targets a state
/// file — its first argument (the path, up to the top-level comma) references a
/// [`STATE_FILE_TOKENS`] entry.
fn targets_state_file(call_text: &str) -> bool {
    let Some(open) = call_text.find("fs::write(") else {
        return false;
    };
    let args = &call_text[open + "fs::write(".len()..];
    // Path arg = up to the first comma at paren-depth 0.
    let mut depth = 0i32;
    let mut path_arg = String::new();
    for ch in args.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => break,
            _ => {}
        }
        path_arg.push(ch);
    }
    STATE_FILE_TOKENS.iter().any(|tok| path_arg.contains(tok))
}

/// A violation: a production plain `fs::write` to a state file with no
/// atomic-write-exempt marker. Returns (1-based line, snippet).
fn find_violations(path: &str, content: &str) -> Vec<(usize, String)> {
    if is_test_only_file(path) {
        return Vec::new();
    }
    let stripped = strip_cfg_test_modules(content);
    let lines: Vec<&str> = stripped.lines().collect();
    let mut violations = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let code = strip_line_comment(lines[i]);
        if code.contains("fs::write(") {
            let (call_text, consumed) = gather_call(&lines, i);
            if call_text.contains("fs::write(") && targets_state_file(&call_text) {
                // Exempt: marker on the call's first line or the contiguous
                // comment block immediately above.
                let exempt_here = lines[i].contains(EXEMPT_MARKER);
                let exempt_above = {
                    let mut k = i;
                    let mut found = false;
                    while k > 0 {
                        let prev = lines[k - 1].trim_start();
                        if prev.starts_with("//") {
                            if prev.contains(EXEMPT_MARKER) {
                                found = true;
                                break;
                            }
                            k -= 1;
                        } else {
                            break;
                        }
                    }
                    found
                };
                if !exempt_here && !exempt_above {
                    violations.push((i + 1, code.trim().to_string()));
                }
            }
            i += consumed;
        } else {
            i += 1;
        }
    }
    violations
}

#[test]
fn production_state_file_writes_use_atomic_write() {
    let mut files = Vec::new();
    collect_rs_files(Path::new("src"), &mut files);
    assert!(!files.is_empty(), "no .rs files found under src/");

    let mut offenders: Vec<String> = Vec::new();
    for f in &files {
        let path = f.to_string_lossy().replace('\\', "/");
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        for (line, snippet) in find_violations(&path, &content) {
            offenders.push(format!("{path}:{line}: {snippet}"));
        }
    }

    assert!(
        offenders.is_empty(),
        "Governance D2: production state-file writes MUST use \
         `store::atomic_write` (a plain fs::write is torn-readable). Offenders:\n{}\n\
         Fix: route through crate::store::atomic_write. If genuinely safe, add a \
         `// atomic-write-exempt: <rationale>` marker on the call or the comment \
         block above it.",
        offenders.join("\n")
    );
}

/// Non-vacuity + scope guards: the scanner must CATCH a production state-file
/// plain-write, and must NOT flag the exempt/out-of-scope cases.
#[test]
fn scanner_catches_state_writes_and_respects_scope() {
    // CAUGHT: production plain write to a state file.
    let bad = "fn publish(run: &Path) { std::fs::write(run.join(\".daemon\"), body); }";
    assert_eq!(
        find_violations("src/daemon/mod.rs", bad).len(),
        1,
        "must flag a production plain fs::write to .daemon"
    );

    // NOT caught: atomic_write is the fix.
    let good = "fn publish(run: &Path) { crate::store::atomic_write(&run.join(\".daemon\"), b); }";
    assert!(find_violations("src/daemon/mod.rs", good).is_empty());

    // NOT caught: write to a state file inside a #[cfg(test)] module.
    let test_region = "fn p() {}\n#[cfg(test)]\nmod tests {\n  fn t() { fs::write(run.join(\".port\"), x); }\n}\n";
    assert!(
        find_violations("src/daemon/mod.rs", test_region).is_empty(),
        "#[cfg(test)] fixtures are exempt"
    );

    // NOT caught: out-of-scope benign config (no state-file token).
    let benign = "fn gen() { std::fs::write(dir.join(\"AGENTS.md\"), content); }";
    assert!(find_violations("src/instructions.rs", benign).is_empty());

    // NOT caught: explicit exempt marker.
    let marked = "fn p() {\n  // atomic-write-exempt: empty truncate, no reader races\n  std::fs::write(run.join(\".ready\"), \"\");\n}";
    assert!(find_violations("src/daemon/mod.rs", marked).is_empty());

    // NOT caught: test-only file path.
    assert!(find_violations("src/runtime_tests.rs", bad).is_empty());

    // CAUGHT (#2323-r6 regression): a NON-module `#[cfg(test)]` item (a `use`,
    // `const`, …) must NOT cause the cfg(test) stripper to swallow a production
    // state-file write placed after it. Pre-fix, the stripper blanked forward to
    // the next brace block and hid this write.
    let after_cfg_use = "#[cfg(test)]\nuse std::time::Duration;\n\n\
         fn publish(run: &Path) { std::fs::write(run.join(\".daemon\"), body); }\n";
    assert_eq!(
        find_violations("src/notification_queue.rs", after_cfg_use).len(),
        1,
        "a production state write after a non-module #[cfg(test)] item must still be flagged"
    );
    // Also a `#[cfg(test)] const` (braces only inside its string value) must not
    // misclassify as a braced item and over-strip the following write.
    let after_cfg_const = "#[cfg(test)]\nconst SAMPLE: &str = \"a { b\";\n\
         fn publish(run: &Path) { std::fs::write(run.join(\".port\"), p); }\n";
    assert_eq!(
        find_violations("src/x.rs", after_cfg_const).len(),
        1,
        "a brace inside a #[cfg(test)] const's string literal must not over-strip the following write"
    );
}
