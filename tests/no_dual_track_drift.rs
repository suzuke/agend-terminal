//! Dual-track fn drift detector (Task #9 Option C epilogue).
//!
//! Background — 2026-04-14 incident: `cleanup_working_dir` drifted between
//! `src/ops.rs` (19 entries) and `src/mcp/handlers.rs` (14 entries). The
//! drift sat undetected for 8 days until Task #9 audit surfaced it. Option C
//! consolidated all 7 known dual-track fn into `crate::agent_ops`, but
//! nothing prevented a future fn from being introduced into both files
//! again.
//!
//! This test closes that loop: it scans `src/ops.rs` and `src/mcp/handlers.rs`
//! for top-level fn definitions that share a name across the two files.
//! - Divergent bodies  → panic (active drift, CI fail).
//! - Identical bodies  → eprintln warning + pass (dedup hazard; consolidate
//!   into `crate::agent_ops` before bodies diverge).
//!
//! Scope is intentionally narrow — only these two files. Macro-generated
//! fn and non-top-level definitions (inside `mod tests`, `impl`, etc.) are
//! deliberately ignored per Task #9 Option C epilogue spec.
//!
//! Parser hardening (issue #28 — robustness epilogue):
//! - `extern "ABI" fn` / `pub extern "C" fn` now strip the ABI clause, so
//!   FFI exports register for drift detection. Previously only bare
//!   `extern fn` (no ABI literal) matched — a silent miss.
//! - Raw string literals (`r"..."` / `r#"..."#`) inside extracted top-level
//!   fn bodies fail loudly rather than silently mis-parse. `skip_string`
//!   uses bare `"` as delimiter; it is incompatible with raw-string
//!   termination (`"#`). The guard is scoped to bodies that the parser
//!   actually extracts — raw strings inside `mod tests {...}` or other
//!   indented blocks (which `detect_fn_name` never enters) do not false-
//!   fail. "Fail loud, don't over-engineer" — upgrade `skip_string` to
//!   real raw-string support if/when a top-level fn needs it.
//! - An unparseable top-level fn body (raw-string-induced miscount or
//!   malformed source) panics with a fix-it hint rather than silently
//!   dropping the fn from the drift map.

use std::collections::HashMap;
use std::path::Path;

/// Extract (fn_name → normalized_body) map from a Rust source file.
/// Only considers fn definitions at indentation column 0 (top-level).
///
/// Panics (issue #28 hardening) rather than silently dropping a fn:
/// - No opening `{` found after a detected fn name → source likely
///   malformed or fn is a forward declaration at column 0 (unusual).
/// - `match_balanced_brace` returns None → brace count never reached 0,
///   usually because a raw string literal inside the body desynced
///   `skip_string`.
/// - Extracted body contains a raw string literal (`r"..."` /
///   `r#"..."#`) at token boundary → parser cannot reliably count
///   braces across raw-string delimiters; refuse rather than guess.
fn extract_top_level_fns(src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cursor = 0;

    while cursor < src.len() {
        let line_end = src[cursor..]
            .find('\n')
            .map(|n| cursor + n)
            .unwrap_or(src.len());
        let line = &src[cursor..line_end];

        if let Some(name) = detect_fn_name(line) {
            let open = find_body_brace(src, cursor).unwrap_or_else(|| {
                panic!(
                    "PARSER LIMITATION: top-level fn `{name}` detected at column 0, \
                     but no opening `{{` found before EOF. Likely causes: \
                     (1) forward declaration `fn {name}();` at column 0 (uncommon — \
                     trait/extern decls are usually indented), \
                     (2) truncated source."
                )
            });
            let close = match_balanced_brace(src, open).unwrap_or_else(|| {
                panic!(
                    "PARSER LIMITATION: top-level fn `{name}` body could not be \
                     brace-balanced. Most likely cause: a raw string literal \
                     (`r\"...\"` or `r#\"...\"#`) inside the body — `skip_string` \
                     uses bare `\"` as delimiter, which desyncs on raw strings \
                     and lets `{{` / `}}` inside the literal corrupt the depth \
                     counter. Fix: either (a) move the raw string out of the \
                     top-level fn, or (b) upgrade `skip_string` to handle raw \
                     strings (and delete this guard)."
                )
            });
            let body = &src[open..=close];
            if body_contains_raw_string_token(body) {
                panic!(
                    "PARSER LIMITATION: top-level fn `{name}` body contains a raw \
                     string literal (`r\"...\"` or `r#\"...\"#`). The detector \
                     does not parse raw-string delimiters — brace counting past \
                     them is unreliable, so drift results would be silently \
                     wrong. Fix: either (a) move the raw string out of the \
                     top-level fn, or (b) upgrade `skip_string` to handle raw \
                     strings (and delete this guard)."
                )
            }
            map.insert(name.to_string(), normalize(body));
            cursor = close + 1;
            continue;
        }

        cursor = line_end + 1;
    }

    map
}

/// Strip known qualifier prefixes (pub, pub(crate), async, const, unsafe,
/// extern, extern "ABI") from a column-0 line, then require `fn NAME` to
/// follow. Returns NAME.
fn detect_fn_name(line: &str) -> Option<&str> {
    let mut rest = line;
    loop {
        let next = rest
            .strip_prefix("pub(crate) ")
            .or_else(|| rest.strip_prefix("pub(super) "))
            .or_else(|| rest.strip_prefix("pub "))
            .or_else(|| rest.strip_prefix("async "))
            .or_else(|| rest.strip_prefix("const "))
            .or_else(|| rest.strip_prefix("unsafe "))
            .or_else(|| strip_extern_maybe_abi(rest));
        match next {
            Some(r) => rest = r,
            None => break,
        }
    }
    let rest = rest.strip_prefix("fn ")?;
    let end = rest
        .find(|c: char| c == '(' || c == '<' || c.is_whitespace())
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// Strip `extern ` optionally followed by an ABI clause `"..." `.
/// Handles `extern fn`, `extern "C" fn`, `extern "Rust" fn`, etc.
/// Returns the remainder after the clause (and any trailing whitespace).
fn strip_extern_maybe_abi(rest: &str) -> Option<&str> {
    let after = rest.strip_prefix("extern ")?;
    match after.strip_prefix('"') {
        Some(after_open_quote) => {
            let end = after_open_quote.find('"')?;
            Some(after_open_quote[end + 1..].trim_start())
        }
        None => Some(after),
    }
}

/// Scan forward from `from` (inclusive) until the first unescaped `{`,
/// skipping string literals, char literals, and comments.
fn find_body_brace(src: &str, from: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'\'' => i = skip_char_lit(bytes, i),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => i = skip_line_comment(bytes, i),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => i = skip_block_comment(bytes, i),
            b'{' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Given `open` pointing at `{`, find the matching `}` accounting for
/// nesting, strings, char literals, and comments.
fn match_balanced_brace(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth = 0_i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = skip_string(bytes, i);
                continue;
            }
            b'\'' => {
                i = skip_char_lit(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i = skip_line_comment(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = skip_block_comment(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() && bytes[i] != b'"' {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else {
            i += 1;
        }
    }
    i + 1
}

fn skip_char_lit(bytes: &[u8], start: usize) -> usize {
    // Char literal: 'x', '\n', '\u{...}'. Cap scan at 10 bytes to avoid
    // misreading Rust lifetimes ('a) — if no closing ' found quickly,
    // treat as not-a-literal and advance one byte.
    let mut i = start + 1;
    let cap = (start + 10).min(bytes.len());
    while i < cap && bytes[i] != b'\'' {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else {
            i += 1;
        }
    }
    if i < cap && bytes[i] == b'\'' {
        i + 1
    } else {
        start + 1
    }
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        i += 1;
    }
    i + 2
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Walk `body` using the same string/char/comment skippers as
/// `match_balanced_brace` and return `true` if a raw-string literal
/// (`r"..."` / `r#"..."#` / `r##"..."##` / ...) is encountered at
/// a token boundary (i.e., not inside a regular string, char literal,
/// or comment, and not a suffix of an identifier like `foo_r`).
///
/// Scoped to an extracted top-level fn body, not the whole source, so
/// raw strings inside `mod tests {...}` or other indented definitions
/// (which the parser never enters) do not trigger a false positive.
fn body_contains_raw_string_token(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'\'' => i = skip_char_lit(bytes, i),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i = skip_line_comment(bytes, i);
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = skip_block_comment(bytes, i);
            }
            b'r' => {
                let at_boundary = i == 0 || !is_ident_char(bytes[i - 1]);
                if at_boundary {
                    let rest = i + 1;
                    if rest < bytes.len() && bytes[rest] == b'"' {
                        return true;
                    }
                    if rest < bytes.len() && bytes[rest] == b'#' {
                        let mut j = rest;
                        while j < bytes.len() && bytes[j] == b'#' {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b'"' {
                            return true;
                        }
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// Collapse whitespace to single spaces so fmt-cosmetic diffs don't
/// register as drift.
fn normalize(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Compare top-level fn bodies between two source strings. Returns
/// `(divergent, identical)` sorted name lists for fn present in both.
///
/// Passing an empty string (e.g. when a file has been deleted) yields
/// an empty fn set for that side, so no intersection is possible and
/// both returned vectors are empty — a "trivial pass" for the caller.
fn compare_fns_from_sources(ops_src: &str, handlers_src: &str) -> (Vec<String>, Vec<String>) {
    let ops_fns = extract_top_level_fns(ops_src);
    let handlers_fns = extract_top_level_fns(handlers_src);

    let mut divergent: Vec<String> = Vec::new();
    let mut identical: Vec<String> = Vec::new();

    for (name, ops_body) in &ops_fns {
        if let Some(h_body) = handlers_fns.get(name) {
            if ops_body == h_body {
                identical.push(name.clone());
            } else {
                divergent.push(name.clone());
            }
        }
    }

    divergent.sort();
    identical.sort();
    (divergent, identical)
}

#[test]
fn no_dual_track_fn_drift_between_ops_and_mcp_handlers() {
    // `unwrap_or_default()` (not `.expect()`) so a future reorg that deletes
    // either file (e.g. Task #12 collapsing `src/ops.rs`) cannot crash the
    // test. A missing file → empty source → 0 top-level fn → no possible
    // intersection → trivial pass. Detection remains active for whichever
    // file does still exist; when both exist the behavior is unchanged.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let ops_src =
        std::fs::read_to_string(Path::new(manifest).join("src/ops.rs")).unwrap_or_default();
    let handlers_src = std::fs::read_to_string(Path::new(manifest).join("src/mcp/handlers/mod.rs"))
        .unwrap_or_default();

    let (divergent, identical) = compare_fns_from_sources(&ops_src, &handlers_src);

    if !identical.is_empty() {
        eprintln!(
            "WARNING: dual-track DEDUP HAZARD — fn shared (byte-identical) between \
             src/ops.rs and src/mcp/handlers.rs:\n  {}\n\
             Consolidate into `crate::agent_ops` before bodies diverge \
             (Task #9 Option C precedent).",
            identical.join(", ")
        );
    }

    assert!(
        divergent.is_empty(),
        "dual-track DRIFT between src/ops.rs and src/mcp/handlers.rs — these fn \
         share a name but their bodies differ, indicating silent divergence:\n  {}\n\n\
         Fix: consolidate into `crate::agent_ops` (single source of truth). \
         Root cause reference: 2026-04-14 `cleanup_working_dir` Kiro drift \
         (handlers copy stalled at 14 entries; ops canonical 19).",
        divergent.join(", ")
    );
}

#[test]
fn handles_missing_ops_rs_gracefully() {
    // Defensive pin: if a future reorg removes `src/ops.rs` (Task #12
    // relocation is the immediate trigger; any future collapse would
    // behave the same), the detector must treat the missing side as
    // an empty source and report no drift — not panic.
    //
    // This test hits the pure helper directly (no filesystem) so it
    // fails loudly if someone removes the graceful fallback from the
    // integration test above.

    // Both sources empty — trivially no intersection.
    let (div, ident) = compare_fns_from_sources("", "");
    assert!(div.is_empty(), "empty sources must not produce drift");
    assert!(
        ident.is_empty(),
        "empty sources must not produce dedup hazard"
    );

    // Only handlers populated (simulates `src/ops.rs` having been deleted
    // while `src/mcp/handlers.rs` still defines real fn).
    let handlers_only = "pub fn example() {\n    let _ = 0;\n}\n";
    let (div, ident) = compare_fns_from_sources("", handlers_only);
    assert!(
        div.is_empty(),
        "missing ops.rs must not yield divergent list"
    );
    assert!(
        ident.is_empty(),
        "missing ops.rs must not yield identical-dup list"
    );

    // Reverse: only ops populated (symmetric — future `mcp/handlers.rs`
    // removal would fall into this branch).
    let ops_only = "pub fn example() {\n    let _ = 0;\n}\n";
    let (div, ident) = compare_fns_from_sources(ops_only, "");
    assert!(
        div.is_empty(),
        "missing handlers.rs must not yield divergent list"
    );
    assert!(
        ident.is_empty(),
        "missing handlers.rs must not yield identical-dup list"
    );
}

#[test]
#[should_panic(expected = "raw string literal")]
fn raw_string_in_top_level_fn_fails_loudly() {
    // Issue #28 robustness pin: a top-level fn body containing a raw
    // string literal must fail loudly rather than silently mis-parse.
    // `skip_string` treats bare `"` as delimiter, so the closing `"#`
    // of a raw string can be mis-identified — `{` / `}` inside the
    // literal would then corrupt the brace depth counter and either
    // drop the fn from the map (silent miss) or extract a wrong body
    // (silent drift false-positive/negative).
    let src = "pub fn example() -> &'static str {\n    r#\"hello { world\"#\n}\n";
    let _ = compare_fns_from_sources(src, "");
}

#[test]
fn raw_string_in_indented_block_is_ignored() {
    // Scope pin: raw strings inside indented definitions (`#[test]` fns
    // inside `mod tests {}`, impl blocks, extern C blocks, etc.) are
    // invisible to the drift detector — `detect_fn_name` only matches
    // column-0 lines, so the parser never enters those bodies. This
    // pins that behavior so the raw-string guard does not false-fail
    // on source files that legitimately use raw strings in test modules
    // (e.g. `src/mcp/handlers.rs` L787/L833 on 2026-04-22).
    let src = "\
pub fn real_fn() {
    let _ = 0;
}
mod tests {
    #[test]
    fn test_with_raw_string() {
        let _ = r#\"abc{def\"#;
    }
}
";
    let (div, ident) = compare_fns_from_sources(src, src);
    assert!(
        ident.contains(&"real_fn".to_string()),
        "real_fn (outside indented block) must register: {ident:?}"
    );
    assert!(
        div.is_empty(),
        "identical sources must not produce divergent: {div:?}"
    );
}

#[test]
fn extern_abi_fn_detected() {
    // Issue #28 parser extension: `extern "C" fn` / `extern "Rust" fn`
    // at column 0 must strip both `extern ` and the quoted ABI clause
    // so the fn name registers in the drift map. Prior to this fix,
    // `detect_fn_name` only recognized bare `extern ` (no ABI literal)
    // and silently dropped any `extern "ABI" fn` — a particularly
    // dangerous blind spot for FFI exports, which are exactly the
    // kind of boundary code where drift between two files would be
    // most damaging.
    let src_ops = "pub extern \"C\" fn ffi_entry() {\n    let _ = 0;\n}\n";
    let src_hdl = "pub extern \"C\" fn ffi_entry() {\n    let _ = 1;\n}\n";
    let (div, _) = compare_fns_from_sources(src_ops, src_hdl);
    assert_eq!(
        div,
        vec!["ffi_entry".to_string()],
        "extern \"C\" fn must register for drift detection: {div:?}"
    );

    // Also verify `extern "Rust" fn` (another valid ABI spelling) —
    // guards against accidental over-specialization to `"C"`.
    let src_a = "extern \"Rust\" fn helper() {\n    let _ = 0;\n}\n";
    let src_b = "extern \"Rust\" fn helper() {\n    let _ = 0;\n}\n";
    let (_, ident) = compare_fns_from_sources(src_a, src_b);
    assert_eq!(
        ident,
        vec!["helper".to_string()],
        "extern \"Rust\" fn must register: {ident:?}"
    );
}
