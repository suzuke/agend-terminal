//! Sprint 23 P0 — heartbeat-pair atomicity invariant test.
//!
//! Mirrors PR #233 dual-coverage anti-rollback pattern (source-grep guard
//! plus behavioural test) applied to F6 lock-around-pair (Sprint 20
//! docs/DAEMON-LOCK-ORDERING.md). Extends Sprint 22 P0 anti-growth contract pattern.
//!
//! ## Two enforcement layers
//!
//! 1. **Source-grep guard**: every `save_metadata` / `save_metadata_batch`
//!    call site that writes `"last_heartbeat"` OR `"waiting_on_since"`
//!    MUST be paired with a `heartbeat_pair::update_with` (or
//!    `heartbeat_pair::pair_for(...).lock()`) call within the preceding
//!    10 lines. Pre-pair writes that skip the in-memory update would
//!    re-introduce the F6 race window — caught here.
//!
//! 2. **`EXEMPTED_LEGACY_FILES` anti-growth contract**: no entries by
//!    intent. Adding entries requires explicit dispatch scope per
//!    Sprint 23 P0 dispatch. Sprint 22 P0 pattern transfer.

use std::path::{Path, PathBuf};

/// Files exempted from the pair-update invariant (legacy / bootstrap /
/// test-fixture sites). Empty by intent — new entries forbidden without
/// explicit dispatch scope per Sprint 23 P0 anti-growth contract.
const EXEMPTED_LEGACY_FILES: &[&str] = &[
    // No exemptions today.
];

fn rust_files_in_src() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&src, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn rel_path_str(path: &Path, root: &Path) -> String {
    // Sprint 23 P1 r2 — normalize Windows backslash to forward-slash for
    // cross-platform EXEMPTED-list / inline `ends_with("daemon/heartbeat_pair.rs")`
    // suffix-match. See PR #240 r2.
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_exempted(rel_path: &str) -> bool {
    EXEMPTED_LEGACY_FILES
        .iter()
        .any(|suffix| rel_path.ends_with(suffix))
}

#[derive(Clone, Copy)]
enum ScanState {
    Normal,
    DoubleQuoted { escaped: bool },
    CharLiteral { escaped: bool },
    LineComment,
    BlockComment { depth: usize },
    RawString { hashes: usize },
}

fn advance_scan_state(state: ScanState, line: &str) -> ScanState {
    scan_line(state, line).0
}

fn scan_line(mut state: ScanState, line: &str) -> (ScanState, i32, bool) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut delta = 0;
    let mut saw_code_brace = false;
    while i < bytes.len() {
        state = match state {
            ScanState::Normal => {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    ScanState::LineComment
                } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    ScanState::BlockComment { depth: 1 }
                } else if bytes[i] == b'"' {
                    ScanState::DoubleQuoted { escaped: false }
                } else if bytes[i] == b'\'' {
                    ScanState::CharLiteral { escaped: false }
                } else if bytes[i] == b'r' {
                    let mut j = i + 1;
                    while bytes.get(j) == Some(&b'#') {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'"') {
                        let hashes = j - i - 1;
                        i = j;
                        ScanState::RawString { hashes }
                    } else {
                        ScanState::Normal
                    }
                } else if bytes[i] == b'{' {
                    delta += 1;
                    saw_code_brace = true;
                    ScanState::Normal
                } else if bytes[i] == b'}' {
                    delta -= 1;
                    saw_code_brace = true;
                    ScanState::Normal
                } else {
                    ScanState::Normal
                }
            }
            ScanState::DoubleQuoted { escaped } => {
                if escaped {
                    ScanState::DoubleQuoted { escaped: false }
                } else if bytes[i] == b'\\' {
                    ScanState::DoubleQuoted { escaped: true }
                } else if bytes[i] == b'"' {
                    ScanState::Normal
                } else {
                    ScanState::DoubleQuoted { escaped: false }
                }
            }
            ScanState::CharLiteral { escaped } => {
                if escaped {
                    ScanState::CharLiteral { escaped: false }
                } else if bytes[i] == b'\\' {
                    ScanState::CharLiteral { escaped: true }
                } else if bytes[i] == b'\'' {
                    ScanState::Normal
                } else {
                    ScanState::CharLiteral { escaped: false }
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' {
                    ScanState::Normal
                } else {
                    ScanState::LineComment
                }
            }
            ScanState::BlockComment { mut depth } => {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 1;
                    ScanState::BlockComment { depth }
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth = depth.saturating_sub(1);
                    i += 1;
                    if depth == 0 {
                        ScanState::Normal
                    } else {
                        ScanState::BlockComment { depth }
                    }
                } else {
                    ScanState::BlockComment { depth }
                }
            }
            ScanState::RawString { hashes } => {
                if bytes[i] == b'"' {
                    let mut j = i + 1;
                    while bytes.get(j) == Some(&b'#') {
                        j += 1;
                    }
                    if j - i - 1 == hashes {
                        i = j - 1;
                        ScanState::Normal
                    } else {
                        ScanState::RawString { hashes }
                    }
                } else {
                    ScanState::RawString { hashes }
                }
            }
        };
        i += 1;
    }
    if matches!(state, ScanState::LineComment) {
        state = ScanState::Normal;
    }
    (state, delta, saw_code_brace)
}

fn first_test_only_boundary(content: &str) -> Option<usize> {
    let mut state = ScanState::Normal;
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if matches!(state, ScanState::Normal)
            && matches!(line.trim(), "#![cfg(test)]" | "#[cfg(test)]")
        {
            return Some(offset);
        }
        state = advance_scan_state(state, line);
        offset += line.len();
    }
    None
}

fn production_source(content: &str) -> String {
    if let Some(offset) = first_test_only_boundary(content) {
        if content[offset..].starts_with("#![cfg(test)]") {
            return content[..offset].to_string();
        }
    }

    let mut production = String::with_capacity(content.len());
    let mut lines = content.split_inclusive('\n').peekable();
    let mut state = ScanState::Normal;
    while let Some(line) = lines.next() {
        if !matches!(state, ScanState::Normal) || line.trim() != "#[cfg(test)]" {
            production.push_str(line);
            state = advance_scan_state(state, line);
            continue;
        }

        production.push('\n');
        state = ScanState::Normal;
        let mut started = false;
        let mut brace_depth = 0usize;
        let mut saw_brace = false;
        let mut item_scan_state = ScanState::Normal;
        for item_line in lines.by_ref() {
            let trimmed = item_line.trim();
            if !started && (trimmed.is_empty() || trimmed.starts_with("#[")) {
                continue;
            }
            started = true;
            let (next_scan_state, delta, line_has_brace) = scan_line(item_scan_state, item_line);
            item_scan_state = next_scan_state;
            saw_brace |= line_has_brace;
            brace_depth = (brace_depth as i32 + delta).max(0) as usize;
            if (saw_brace && brace_depth == 0) || (!saw_brace && item_line.contains(';')) {
                break;
            }
        }
    }
    production
}

fn source_code_lines(source: &str) -> Vec<(&str, usize)> {
    let mut lines = Vec::new();
    let mut brace_depth = 0i32;
    let mut next_scope = 1usize;
    let mut function_scope = None;
    let mut scan_state = ScanState::Normal;
    for line in source.lines() {
        let trimmed = line.trim();
        let code_at_line_start = matches!(scan_state, ScanState::Normal);
        let starts_function =
            function_scope.is_none() && code_at_line_start && is_function_start(trimmed);
        if starts_function {
            let function_id = next_scope;
            next_scope += 1;
            function_scope = Some((function_id, brace_depth));
        }
        let scope = function_scope.map_or(0, |(function_id, _)| function_id);
        let (next_scan_state, delta, _) = scan_line(scan_state, line);
        scan_state = next_scan_state;
        brace_depth += delta;
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        lines.push((line, scope));
        if let Some((_, function_start_depth)) = function_scope {
            if brace_depth <= function_start_depth
                || (starts_function && delta == 0 && trimmed.ends_with(';'))
            {
                function_scope = None;
            }
        }
    }
    lines
}

fn is_function_start(line: &str) -> bool {
    let mut signature = line;
    if let Some(rest) = signature.strip_prefix("pub") {
        signature = if let Some(rest) = rest.strip_prefix(' ') {
            rest.trim_start()
        } else if rest.starts_with('(') {
            let Some(end) = rest.find(')') else {
                return false;
            };
            rest[end + 1..].trim_start()
        } else {
            return false;
        };
    }
    for modifier in ["async ", "const ", "unsafe "] {
        if let Some(rest) = signature.strip_prefix(modifier) {
            signature = rest;
        }
    }
    signature.starts_with("fn ")
}

#[test]
fn test_only_boundary_recognizes_inner_and_outer_attributes() {
    let inner = "fn production() {}\n#![cfg(test)]\nfn fixture() {}\n";
    let outer = "fn production() {}\n#[cfg(test)]\nmod tests {}\n";
    assert_eq!(
        first_test_only_boundary(inner),
        Some("fn production() {}\n".len())
    );
    assert_eq!(
        first_test_only_boundary(outer),
        Some("fn production() {}\n".len())
    );
}

#[test]
fn test_only_boundary_ignores_comments_and_literals() {
    let source = r###"// #![cfg(test)]
let marker = "#[cfg(test)]";
let multiline = r#"
#[cfg(test)]
"#;
fn production() {}
"###;
    assert_eq!(first_test_only_boundary(source), None);
}

#[test]
fn outer_test_item_does_not_hide_later_production() {
    let source = r#"fn before() {}
#[cfg(test)]
fn fixture() {
    save_metadata("waiting_on_since");
}
fn after() {
    save_metadata("waiting_on_since");
}
"#;
    let production = production_source(source);
    assert!(!production.contains("fn fixture"));
    assert!(production.contains("fn after"));
    assert!(production.contains("save_metadata(\"waiting_on_since\")"));
}

#[test]
fn outer_test_item_with_noncode_braces_does_not_hide_later_production() {
    let source = r##"#[cfg(test)]
fn fixture() {
    let raw = r#"{ an unmatched brace in a raw string "#;
    /* { an unmatched brace in a block comment */
}
fn production() {
    save_metadata("waiting_on_since");
}
"##;
    let production = production_source(source);
    assert!(!production.contains("fn fixture"));
    assert!(production.contains("fn production"));
}

#[test]
fn detector_rejects_unpaired_production_after_outer_test_item() {
    let source = r#"#[cfg(test)]
fn fixture() {}
fn production() {
    save_metadata("waiting_on_since");
}
"#;
    let violations = unpaired_pair_writes("synthetic.rs", source);
    assert_eq!(
        violations.len(),
        1,
        "unpaired production write must be reported"
    );
}

#[test]
fn detector_accepts_pair_separated_only_by_comments() {
    let source = r#"fn production() {
    heartbeat_pair::update_with(name, |_| {});
    // explain the lock ordering
    // explain the fallback
    // explain the disk write
    // explain the cold start
    // explain the supervisor reader
    // explain the transition
    // explain the pair invariant
    // explain the ordering
    // explain the bounded window
    // explain the retry
    // explain the test
    save_metadata(name, "last_heartbeat", value);
}
"#;
    assert!(
        unpaired_pair_writes("synthetic.rs", source).is_empty(),
        "comments must not consume the executable pairing window"
    );
}

#[test]
fn detector_rejects_pair_from_a_different_function() {
    let source = r#"fn updater() {
    heartbeat_pair::update_with(name, |_| {});
}
fn writer() {
    save_metadata(name, "last_heartbeat", value);
}
"#;
    let violations = unpaired_pair_writes("synthetic.rs", source);
    assert_eq!(
        violations.len(),
        1,
        "an update in another function must not satisfy the write"
    );
}

#[test]
fn detector_rejects_qualified_async_pair_from_a_different_function() {
    let source = r#"mod nested {
pub(super) fn updater() {
    heartbeat_pair::update_with(name, |_| {});
}
pub(crate) async fn writer() {
    save_metadata(name, "last_heartbeat", value);
}
}
"#;
    let violations = unpaired_pair_writes("synthetic.rs", source);
    assert_eq!(
        violations.len(),
        1,
        "qualified async function boundaries must not cross-pair"
    );
}

#[test]
fn detector_ignores_noncode_braces_when_tracking_scope() {
    let source = r##"fn production() {
    heartbeat_pair::update_with(name, |_| {});
    let raw = r#"{ a brace in a raw string }"#;
    // { a brace in a comment }
    /* { a brace in a block comment } */
    save_metadata(name, "last_heartbeat", value);
}
"##;
    assert!(
        unpaired_pair_writes("synthetic.rs", source).is_empty(),
        "non-code braces must not end the function scope early"
    );
}

#[test]
fn detector_documents_textual_pairing_not_instance_identity() {
    let source = r#"fn production() {
    heartbeat_pair::update_with(other_name, |_| {});
    save_metadata(name, "last_heartbeat", value);
}
"#;
    assert!(
        unpaired_pair_writes("synthetic.rs", source).is_empty(),
        "this source audit proves call pairing only; instance identity remains a runtime concern"
    );
}

fn unpaired_pair_writes(rel: &str, content: &str) -> Vec<String> {
    let prod = production_source(content);
    let lines = source_code_lines(&prod);
    let mut violations = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let (line, scope) = *line;
        let writes_pair_field =
            line.contains("\"last_heartbeat\"") || line.contains("\"waiting_on_since\"");
        if !writes_pair_field {
            continue;
        }
        let look_back_start = idx.saturating_sub(5);
        let preceding = lines[look_back_start..=idx]
            .iter()
            .filter(|(_, candidate_scope)| *candidate_scope == scope)
            .map(|(candidate, _)| *candidate)
            .collect::<Vec<_>>()
            .join("\n");
        if !preceding.contains("save_metadata") {
            continue;
        }

        let pair_look_back_start = idx.saturating_sub(10);
        let pair_window = lines[pair_look_back_start..=idx]
            .iter()
            .filter(|(_, candidate_scope)| *candidate_scope == scope)
            .map(|(candidate, _)| *candidate)
            .collect::<Vec<_>>()
            .join("\n");
        let has_pair_update = pair_window.contains("heartbeat_pair::update_with")
            || pair_window.contains("heartbeat_pair::pair_for")
            || pair_window.contains("heartbeat_pair::snapshot_for");
        if !has_pair_update {
            violations.push(format!(
                "  {}:{}: save_metadata write of pair-relevant field without preceding \
                 heartbeat_pair update — re-introduces F6 race window\n      offending line: {}",
                rel,
                idx + 1,
                line.trim()
            ));
        }
    }
    violations
}

/// Sprint 23 P0 source-grep guard: every save_metadata write of a
/// pair-relevant field must have a heartbeat_pair update within the
/// preceding 10 lines.
#[test]
fn heartbeat_pair_writes_paired_with_in_memory_update() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files_in_src() {
        let rel = rel_path_str(&path, &src_root);
        if is_exempted(&rel) {
            continue;
        }
        // The pair module itself + the lock-ordering doc references its
        // own primitives — exempt by definition.
        if rel.ends_with("daemon/heartbeat_pair.rs") {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        violations.extend(unpaired_pair_writes(&rel, &content));
    }

    assert!(
        violations.is_empty(),
        "Sprint 23 P0 F6 lock-around-pair invariant violations — {} write site(s) skip the \
         in-memory pair update.\n\nFix: add `crate::daemon::heartbeat_pair::update_with(name, |p| {{ ... }})` \
         (or equivalent) before the save_metadata call. The in-memory pair update + disk persist \
         pair must remain symmetric so supervisor's snapshot-read sees consistent state.\n\n\
         Do NOT add to EXEMPTED_LEGACY_FILES without explicit dispatch scope per Sprint 23 P0.\n\n\
         Violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sprint 23 P0 sanity test: the pair module exists and exposes the
/// expected public API. If this test fails, F6 was reverted at module
/// level — the source-grep guard above wouldn't fire because there's
/// nothing to grep against.
#[test]
fn heartbeat_pair_module_exposes_required_api() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join("daemon/heartbeat_pair.rs");
    let content = std::fs::read_to_string(&path).expect("daemon/heartbeat_pair.rs must exist");
    assert!(
        content.contains("pub fn pair_for("),
        "pair_for(name) must be public — this is the lock acquisition entry point"
    );
    assert!(
        content.contains("pub fn snapshot_for("),
        "snapshot_for(name) must be public — readers depend on it"
    );
    assert!(
        content.contains("pub fn update_with<"),
        "update_with(name, f) must be public — writers depend on it"
    );
    assert!(
        content.contains("pub fn now_ms()"),
        "now_ms() must be public — common utility for callers updating the timestamp"
    );
}
