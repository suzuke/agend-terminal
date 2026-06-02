//! #1630 invariant: a production inbox/notification **enqueue** whose `Result`
//! is silently dropped is the silent-message-loss bug class (#1614/#1618/#1622
//! lineage — a dropped `enqueue` means the message never lands on disk and the
//! recipient never sees it). All 23 historical drops shared the reflexive
//! `let _ = …enqueue…(…)` shape. This test fails CI if a dropped enqueue
//! reappears, forcing the author to propagate the `Err` or route it through
//! `persist_or_log!`.
//!
//! **Statement/span-aware** (not line-oriented): after locating a call it
//! balances parens to the end of the call expression, then inspects the
//! *leading* binding and the *trailing* consumption — so a multi-line drop
//! (rustfmt's normal layout for a multi-arg call, e.g. `.ok()` on the line
//! after the closing paren) is caught, which a per-line scan would miss.
//!
//! Drop shapes flagged: `let _ = <call>`, `<call>.ok()`, bare `<call>;`
//! — even when the binding / `.ok()` / `;` lands on a different line than the
//! call token. Allow-listed: `persist_or_log!(<call>, …)`, propagation
//! (`return <call>…`, `<call>?`), binding (`let x = <call>`, `match <call>`),
//! and tail-expression returns (`<call>` with no trailing `;`).
//!
//! Scope / honest limits:
//! - Scans `src/` only; skips test code (test-module files + `#[cfg(test)]`
//!   regions) — test cleanup legitimately discards these Results.
//! - Paren balancing is string-literal aware but not char-literal/block-comment
//!   aware; line (`//`) comments are skipped. These gaps are irrelevant to the
//!   enqueue call sites in this codebase.
//! - It CANNOT stop a determined evader who binds the Result then drops it
//!   (`let r = enqueue(..); /* ignore r */`). That's by design — the goal is to
//!   kill the reflexive `let _ =` / `.ok()` / bare-`;` that produced the 23.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Enqueue/notification fns whose dropped `Result` is the #1630 class. Matched
/// as call tokens `<name>(`.
const ENQUEUE_FNS: &[&str] = &["enqueue", "enqueue_with_idle_hint", "enqueue_classified"];

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

/// File-level test convention: `*/tests.rs`, anything under a `tests/`/`test/`
/// dir. Mirrors `anti_pattern_invariant::is_test_module_path`.
fn is_test_module_file(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
        return true;
    }
    path.components()
        .any(|c| c.as_os_str() == "tests" || c.as_os_str() == "test")
}

/// 1-based line numbers inside `#[cfg(test)]` module regions (brace-depth
/// tracked), so in-file test modules are prod-sliced out.
fn test_region_lines(content: &str) -> HashSet<usize> {
    let mut out = HashSet::new();
    let mut depth: i32 = 0;
    let mut pending_cfg_test = false;
    let mut region_depth: Option<i32> = None;
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        if region_depth.is_none() {
            if t.starts_with("#[cfg(test)]") {
                pending_cfg_test = true;
            } else if pending_cfg_test && t.contains("mod ") && line.contains('{') {
                region_depth = Some(depth);
                pending_cfg_test = false;
            }
        }
        if region_depth.is_some_and(|d| depth >= d) {
            out.insert(i + 1);
        }
        let next = depth + line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if let Some(d) = region_depth {
            if next <= d {
                region_depth = None;
            }
        }
        depth = next;
    }
    out
}

fn line_of(line_starts: &[usize], pos: usize) -> usize {
    match line_starts.binary_search(&pos) {
        Ok(i) => i + 1,
        Err(i) => i, // i = number of starts <= pos
    }
}

/// True if `enqueue(`-style needle at `idx` is a real call (non-identifier char
/// before it — excludes `my_enqueue(`).
fn is_call_token(bytes: &[u8], idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let c = bytes[idx - 1];
    !(c.is_ascii_alphanumeric() || c == b'_')
}

/// Start of the call path: walk back over `[A-Za-z0-9_:]` from the fn-name
/// token start, so `crate::inbox::enqueue` resolves to the `crate` index.
fn path_start(bytes: &[u8], name_start: usize) -> usize {
    let mut i = name_start;
    while i > 0 {
        let c = bytes[i - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b':' {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

/// Index of the matching `)` for the `(` at `open`, string-literal aware.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[derive(PartialEq)]
enum Lead {
    LetUnderscore, // `let _ = <call>`
    Allowed,       // persist_or_log!(<call>…), return <call>, let x = <call>, match <call>, …
    Bare,          // statement position: preceded by `;` `{` `}` or start
    Other,         // sub-expression (argument, match-arm value, …)
}

fn classify_lead(bytes: &[u8], call_path_start: usize) -> Lead {
    // Skip whitespace/newlines backward.
    let mut i = call_path_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return Lead::Bare;
    }
    let prev = bytes[i - 1];
    match prev {
        b';' | b'{' | b'}' => Lead::Bare,
        b'>' => Lead::Other, // `=>` match arm value
        b'?' | b',' | b'|' | b'&' | b'+' => Lead::Other,
        b'=' => {
            // assignment — `let _ =` is a drop; any other binding is allowed.
            // (`==`/`!=`/`>=` etc. would have a second op char we don't reach.)
            let mut j = i - 1;
            while j > 0 && bytes[j - 1].is_ascii_whitespace() {
                j -= 1;
            }
            // `let _ =` ⇒ char before the ws-run before `=` is `_`, itself
            // preceded by whitespace then `let`.
            if j > 0 && bytes[j - 1] == b'_' {
                let mut k = j - 1;
                while k > 0 && bytes[k - 1].is_ascii_whitespace() {
                    k -= 1;
                }
                if k >= 3 && &bytes[k - 3..k] == b"let" {
                    return Lead::LetUnderscore;
                }
            }
            Lead::Allowed
        }
        b'(' => {
            // Argument to a call/macro — allowed iff the macro is persist_or_log!.
            let paren = i - 1; // index of '('
            let mut k = paren;
            while k > 0 {
                let c = bytes[k - 1];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'!' || c == b':' {
                    k -= 1;
                } else {
                    break;
                }
            }
            let ident = std::str::from_utf8(&bytes[k..paren]).unwrap_or("");
            if ident.contains("persist_or_log!") {
                Lead::Allowed
            } else {
                Lead::Other
            }
        }
        _ => {
            // word like `return` / `await` / `=>`?
            let mut k = i;
            while k > 0 {
                let c = bytes[k - 1];
                if c.is_ascii_alphabetic() || c == b'_' {
                    k -= 1;
                } else {
                    break;
                }
            }
            let word = std::str::from_utf8(&bytes[k..i]).unwrap_or("");
            if word == "return" {
                Lead::Allowed
            } else {
                Lead::Other
            }
        }
    }
}

enum Trail {
    DotOk,     // `.ok()` — drop
    Semicolon, // bare `;` — drop (statement position)
    Ok,        // `?`, `.method(` chain, `}`, `,`, `)`, EOF — propagation/subexpr/tail
}

fn classify_trail(bytes: &[u8], close: usize) -> Trail {
    let mut i = close + 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return Trail::Ok;
    }
    match bytes[i] {
        b';' => Trail::Semicolon,
        b'.' => {
            // `.ok()` ⇒ drop; any other method chain ⇒ not our concern.
            let rest = &bytes[i..];
            if rest.starts_with(b".ok()") || rest.starts_with(b".ok ") || rest.starts_with(b".ok\n")
            {
                Trail::DotOk
            } else {
                Trail::Ok
            }
        }
        _ => Trail::Ok,
    }
}

#[test]
fn production_enqueue_results_must_not_be_silently_dropped() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "no .rs files found under src/");

    let mut violations = Vec::new();
    for f in &files {
        if is_test_module_file(f) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        let bytes = content.as_bytes();
        let test_lines = test_region_lines(&content);
        let mut line_starts = vec![0usize];
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                line_starts.push(i + 1);
            }
        }

        for fname in ENQUEUE_FNS {
            let needle = format!("{fname}(");
            let mut from = 0;
            while let Some(rel) = content[from..].find(&needle) {
                let name_start = from + rel;
                let open = name_start + fname.len(); // index of '('
                from = name_start + 1;

                if !is_call_token(bytes, name_start) {
                    continue;
                }
                let line = line_of(&line_starts, name_start);
                if test_lines.contains(&line) {
                    continue;
                }
                // Skip line-comment occurrences (`//` before the token).
                let ls = line_starts[line - 1];
                let prefix = &content[ls..name_start];
                if prefix.contains("//") {
                    continue;
                }
                // Skip the fn definitions themselves (`fn enqueue(`).
                let ps = path_start(bytes, name_start);
                let mut w = ps;
                while w > 0 && bytes[w - 1].is_ascii_whitespace() {
                    w -= 1;
                }
                let mut k = w;
                while k > 0 && (bytes[k - 1].is_ascii_alphabetic() || bytes[k - 1] == b'_') {
                    k -= 1;
                }
                if std::str::from_utf8(&bytes[k..w]).unwrap_or("") == "fn" {
                    continue;
                }

                let Some(close) = matching_paren(bytes, open) else {
                    continue; // unbalanced (string edge) — don't false-flag
                };

                let lead = classify_lead(bytes, ps);
                let is_drop = match lead {
                    Lead::LetUnderscore => true,
                    Lead::Allowed => false,
                    Lead::Bare | Lead::Other => {
                        matches!(classify_trail(bytes, close), Trail::DotOk)
                            || (lead == Lead::Bare
                                && matches!(classify_trail(bytes, close), Trail::Semicolon))
                    }
                };
                if is_drop {
                    let line_text = content.lines().nth(line - 1).unwrap_or("").trim();
                    violations.push(format!("{}:{}: {}", f.display(), line, line_text));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1630: production enqueue Result silently dropped — propagate the Err \
         (`?`/`return`) where the caller can handle it, or route fire-and-forget \
         sites through `persist_or_log!(<call>, \"op\", target)`:\n{}",
        violations.join("\n")
    );
}
