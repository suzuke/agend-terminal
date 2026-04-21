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

use std::collections::HashMap;
use std::path::Path;

/// Extract (fn_name → normalized_body) map from a Rust source file.
/// Only considers fn definitions at indentation column 0 (top-level).
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
            if let Some(open) = find_body_brace(src, cursor) {
                if let Some(close) = match_balanced_brace(src, open) {
                    let body = &src[open..=close];
                    map.insert(name.to_string(), normalize(body));
                    cursor = close + 1;
                    continue;
                }
            }
        }

        cursor = line_end + 1;
    }

    map
}

/// Strip known qualifier prefixes (pub, pub(crate), async, const, unsafe)
/// from a column-0 line, then require `fn NAME` to follow. Returns NAME.
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
            .or_else(|| rest.strip_prefix("extern "));
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

/// Collapse whitespace to single spaces so fmt-cosmetic diffs don't
/// register as drift.
fn normalize(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn no_dual_track_fn_drift_between_ops_and_mcp_handlers() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let ops_src =
        std::fs::read_to_string(Path::new(manifest).join("src/ops.rs")).expect("read src/ops.rs");
    let handlers_src = std::fs::read_to_string(Path::new(manifest).join("src/mcp/handlers.rs"))
        .expect("read src/mcp/handlers.rs");

    let ops_fns = extract_top_level_fns(&ops_src);
    let handlers_fns = extract_top_level_fns(&handlers_src);

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
