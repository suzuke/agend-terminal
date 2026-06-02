//! #1630 invariant: a production inbox/notification **enqueue** whose `Result`
//! is silently dropped is the silent-message-loss bug class (#1614/#1618/#1622
//! lineage — a dropped `enqueue` means the message never lands on disk and the
//! recipient never sees it). All 23 historical drops shared one reflexive
//! shape: `let _ = …enqueue…(…)`. This test fails CI if that shape (or `.ok()`
//! / a bare-`;` discard) reappears in production code, forcing the author to
//! either propagate the `Err` or route it through `persist_or_log!`.
//!
//! Scope / honesty about limits:
//! - Scans `src/` only, and skips test code (test-module files + `#[cfg(test)]`
//!   regions) — test cleanup legitimately discards these Results.
//! - Catches the three common reflexive drop shapes (`let _ =`, `.ok()`,
//!   bare statement). It CANNOT stop a determined evader who binds the Result
//!   to a named variable and then drops it (`let r = enqueue(..); /* ignore */`)
//!   — and that is fine: the goal is to kill the reflexive `let _ =` that
//!   produced every one of the 23, not to be a watertight escape analysis.
//! - The bare-`;` shape is also caught by rustc's `unused_must_use`
//!   (`enqueue` returns `#[must_use] Result`) under CI `-D warnings`; this test
//!   makes the intent explicit and co-locates all three shapes.
//!
//! Allow-listed: any drop routed through `persist_or_log!(…)`.

use std::path::{Path, PathBuf};

/// The persistence/notification enqueue functions whose dropped `Result` is the
/// #1630 silent-loss class. Matched as call tokens (`<name>(`).
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

/// Does `line` contain a call to one of `ENQUEUE_FNS` (as `<name>(`), and is it
/// NOT a `fn` definition of one of them?
fn has_enqueue_call(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with('*') || t.starts_with("///") {
        return false; // comment / doc
    }
    // Skip the fn definitions themselves.
    if t.contains("fn enqueue") {
        return false;
    }
    ENQUEUE_FNS.iter().any(|f| {
        // Match `name(` and `name_with_idle_hint(` etc. precisely: the token
        // must be followed by `(` and preceded by a non-identifier char.
        let needle = format!("{f}(");
        find_call(line, &needle)
    })
}

/// True if `needle` (e.g. `enqueue(`) appears with a non-identifier char before
/// it — so `enqueue(` matches but `dequeue(` / `my_enqueue(` do not, and
/// `enqueue_with_idle_hint(` is not matched by the bare `enqueue(` needle.
fn find_call(line: &str, needle: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find(needle) {
        let idx = from + rel;
        let prev_ok = idx == 0 || {
            let c = bytes[idx - 1] as char;
            !(c.is_alphanumeric() || c == '_')
        };
        if prev_ok {
            return true;
        }
        from = idx + 1;
    }
    false
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
        let lines: Vec<&str> = content.lines().collect();

        // Track `#[cfg(test)]` regions by brace depth so in-file test modules
        // are skipped (the prod-slice requirement — can't false-fail on test
        // helpers, can't false-pass since prod code is never inside one).
        let mut depth: i32 = 0;
        let mut pending_cfg_test = false;
        let mut test_region_depth: Option<i32> = None;

        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            // Enter/track a #[cfg(test)] module region.
            if test_region_depth.is_none() {
                if trimmed.starts_with("#[cfg(test)]") {
                    pending_cfg_test = true;
                } else if pending_cfg_test && trimmed.contains("mod ") && line.contains('{') {
                    test_region_depth = Some(depth);
                    pending_cfg_test = false;
                }
            }
            let in_test_region = test_region_depth.is_some_and(|d| depth >= d);

            // Update brace depth for the NEXT line.
            let opens = line.matches('{').count() as i32;
            let closes = line.matches('}').count() as i32;
            let next_depth = depth + opens - closes;
            if let Some(d) = test_region_depth {
                if next_depth <= d {
                    test_region_depth = None;
                }
            }
            depth = next_depth;

            if in_test_region {
                continue;
            }
            if !has_enqueue_call(line) {
                continue;
            }

            // Allow-listed: routed through persist_or_log! (the macro opener may
            // be on this line or up to a few lines above for multi-line calls).
            let guarded = (idx.saturating_sub(4)..=idx)
                .any(|i| lines.get(i).is_some_and(|l| l.contains("persist_or_log!")));
            if guarded {
                continue;
            }

            // Legit handling: result is propagated or bound, not dropped.
            //   `return <fn>(…)`, `<fn>(…)?`, `… = <fn>(…)` (incl `let x =`,
            //   `if let Ok(_) = …`), `match <fn>(…)`.
            let t = line.trim_start();
            let propagated_or_bound = t.starts_with("return ")
                || t.starts_with("match ")
                || (t.contains("= ") && !t.contains("let _ ="))
                || line.contains(")?")
                || line.contains(").await?");
            // Drop shapes we DO flag:
            let let_underscore = t.contains("let _ =");
            let ok_discard = line.contains(".ok()");
            // Bare statement DROP: the line starts with the call path AND ends
            // in `);` (the must_use Result discarded as a statement). A tail-
            // expression `enqueue(…)` that RETURNS the Result ends in `)` with
            // no `;`, so it is correctly excluded (it's propagation, not a drop).
            // Multi-line bare-`;` drops are rare and additionally caught by
            // rustc's `unused_must_use` under CI `-D warnings`.
            let starts_with_call = ENQUEUE_FNS.iter().any(|f| {
                t.starts_with(&format!("{f}("))
                    || t.starts_with(&format!("crate::inbox::{f}("))
                    || t.starts_with(&format!("inbox::{f}("))
                    || t.starts_with(&format!("crate::inbox::storage::{f}("))
                    || t.starts_with(&format!("storage::{f}("))
                    || t.starts_with(&format!("crate::notification_queue::{f}("))
                    || t.starts_with(&format!("notification_queue::{f}("))
            });
            let bare_stmt = starts_with_call && line.trim_end().ends_with(");");

            let is_drop = let_underscore || ok_discard || (bare_stmt && !propagated_or_bound);
            if is_drop {
                violations.push(format!("{}:{}: {}", f.display(), idx + 1, line.trim()));
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
