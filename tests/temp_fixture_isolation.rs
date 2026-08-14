//! #3245 invariant: test temp-home fixtures must not be able to delete each
//! other's directory.
//!
//! Two collision shapes are possible and this guard rejects both:
//!
//! 1. **Prefix overlap between helpers.** `agend-986-{tag}` and
//!    `agend-986-int-{tag}` are different helpers, yet the first with tag
//!    `int-foo` resolves to the same path as the second with tag `foo`. Any two
//!    shapes where one literal prefix is a prefix of the other can collide this
//!    way, whatever their labels.
//! 2. **The same literal label in two different test functions** of one helper:
//!    both resolve to one directory, and helpers that wipe on entry then delete
//!    a neighbour's fixture mid-run. Reuse *within a single test* is legitimate
//!    — `tmp_home_starts_clean_when_suffix_is_reused` exists precisely to assert
//!    that reuse wipes stale content — so the check is per test function, not
//!    per file.
//!
//! Selection is BEHAVIORAL, not by name: any function whose body reaches
//! `std::env::temp_dir()` is a fixture helper here, whether it is called
//! `tmp_home`, `home_with_state`, or anything else. A name-anchored guard is
//! the kind a future `fn sandbox()` walks straight past.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Source files this guard scans: the crate's own Rust sources, excluding
/// vendored trees (which are not ours to police).
fn owned_rust_sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for dir in ["src", "tests"] {
        collect_rs(&root.join(dir), &mut out);
    }
    out.sort();
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A temp-dir fixture helper: the function's name, the file it lives in, and
/// every `format!` literal its body builds a path from.
#[derive(Debug)]
struct Helper {
    file: String,
    name: String,
    shapes: Vec<String>,
    /// Wipes its directory on entry — the precondition for destroying a
    /// neighbour's fixture. Non-destructive helpers may share a path shape
    /// without anyone losing state.
    destructive: bool,
}

/// Split a file into `fn` bodies by brace depth, so "the body reaches
/// `temp_dir()`" is a structural question rather than a line-window guess.
fn functions_of(src: &str) -> Vec<(String, String)> {
    let bytes: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(&['f', 'n', ' ']) && (i == 0 || !bytes[i - 1].is_alphanumeric()) {
            let name: String = bytes[i + 3..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '_')
                .collect();
            if let Some(open) = bytes[i..].iter().position(|c| *c == '{') {
                let start = i + open;
                let mut depth = 0i32;
                let mut j = start;
                while j < bytes.len() {
                    match bytes[j] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if depth == 0 && !name.is_empty() {
                    out.push((name, bytes[start..=j.min(bytes.len() - 1)].iter().collect()));
                    i = start;
                }
            }
        }
        i += 1;
    }
    out
}

/// Path shapes only: the literal a temp-dir path is actually built from.
///
/// A helper body may contain unrelated `format!`s (YAML fixtures, log lines).
/// Anchor on `temp_dir()` and take the literal that the immediately following
/// `join`/`format!` uses, so content strings never masquerade as path shapes.
fn path_shapes(body: &str) -> Vec<String> {
    const WINDOW: usize = 240;
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = body[from..].find("temp_dir()") {
        let start = from + pos + "temp_dir()".len();
        let mut end = (start + WINDOW).min(body.len());
        while end > start && !body.is_char_boundary(end) {
            end -= 1;
        }
        let window = &body[start..end];
        if let Some(lit) = first_literal_after(window, "format!(")
            .or_else(|| first_literal_after(window, ".join("))
        {
            out.push(lit);
        }
        from = start;
    }
    out
}

fn first_literal_after(window: &str, marker: &str) -> Option<String> {
    let idx = window.find(marker)?;
    let rest = window[idx + marker.len()..].trim_start();
    let stripped = rest.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(stripped[..end].to_string())
}

fn temp_dir_helpers() -> Vec<Helper> {
    let mut helpers = Vec::new();
    for path in owned_rust_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !src.contains("temp_dir()") {
            continue;
        }
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        for (name, body) in functions_of(&src) {
            if !body.contains("temp_dir()") {
                continue;
            }
            let shapes = path_shapes(&body);
            if shapes.is_empty() {
                continue;
            }
            let destructive =
                body.contains("remove_dir_all") || body.contains("remove_dir(");
            helpers.push(Helper {
                file: file.clone(),
                name,
                shapes,
                destructive,
            });
        }
    }
    helpers
}

/// The literal text before the first `{}`/`{name}` placeholder — the part of a
/// path shape that is fixed regardless of the caller's label.
fn fixed_prefix(shape: &str) -> String {
    match shape.find('{') {
        Some(idx) => shape[..idx].to_string(),
        None => shape.to_string(),
    }
}

#[test]
fn temp_fixture_shapes_do_not_prefix_overlap() {
    let helpers = temp_dir_helpers();
    assert!(
        helpers.len() > 20,
        "behavioral selection found only {} temp-dir helpers — the scan is broken, \
         not the tree",
        helpers.len()
    );

    let mut shapes: Vec<(String, String, String)> = Vec::new();
    for helper in helpers.iter().filter(|h| h.destructive) {
        for shape in &helper.shapes {
            let prefix = fixed_prefix(shape);
            // Only path-like shapes participate; a bare "{}" carries no family.
            if prefix.len() >= 4 {
                shapes.push((prefix, helper.file.clone(), helper.name.clone()));
            }
        }
    }

    let mut offenders = Vec::new();
    for (a_prefix, a_file, a_name) in &shapes {
        for (b_prefix, b_file, b_name) in &shapes {
            if (a_file, a_name) == (b_file, b_name) || a_prefix == b_prefix {
                continue;
            }
            if b_prefix.starts_with(a_prefix.as_str()) {
                offenders.push(format!(
                    "{a_file}::{a_name} {a_prefix:?} is a prefix of {b_file}::{b_name} {b_prefix:?} \
                     — a label starting with {:?} makes the two resolve to one directory",
                    &b_prefix[a_prefix.len()..]
                ));
            }
        }
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "temp-fixture path shapes can collide across helpers:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn temp_fixture_labels_are_not_reused_across_test_functions() {
    // Only helpers that WIPE on entry can destroy a neighbour's fixture, so the
    // harm condition — not merely a shared name — defines the offence.
    let mut offenders = Vec::new();
    for path in owned_rust_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !src.contains("temp_dir()") {
            continue;
        }
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        let destructive: Vec<String> = functions_of(&src)
            .into_iter()
            .filter(|(_, body)| {
                body.contains("temp_dir()")
                    && (body.contains("remove_dir_all") || body.contains("remove_dir("))
            })
            .map(|(name, _)| name)
            .collect();
        if destructive.is_empty() {
            continue;
        }
        // label -> the test functions using it. Reuse inside ONE function is
        // legitimate; reuse across two is the collision.
        let mut by_label: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for helper in &destructive {
            let marker = format!("{helper}(");
            let mut from = 0usize;
            while let Some(pos) = src[from..].find(&marker) {
                let at = from + pos;
                from = at + marker.len();
                let rest = src[from..].trim_start();
                let Some(stripped) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = stripped.find('"') else {
                    continue;
                };
                let label = stripped[..end].to_string();
                let caller = enclosing_fn(&src, at);
                if caller == *helper {
                    continue;
                }
                let users = by_label.entry(label).or_default();
                if !users.contains(&caller) {
                    users.push(caller);
                }
            }
        }
        for (label, users) in by_label {
            if users.len() > 1 {
                offenders.push(format!("{file}: label {label:?} used by {users:?}"));
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "labels are reused across different test functions of a WIPE-ON-ENTRY temp \
         helper, so one test can delete another's directory:\n  {}",
        offenders.join("\n  ")
    );
}

/// Name of the `fn` textually preceding `offset` — attribution that does not
/// depend on brace matching over nested items.
fn enclosing_fn(src: &str, offset: usize) -> String {
    let head = &src[..offset];
    match head.rfind("fn ") {
        Some(idx) => head[idx + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
        None => "<file>".to_string(),
    }
}
