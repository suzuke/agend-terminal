#![allow(clippy::unwrap_used, clippy::expect_used)]

//! #2071 — TTY-leak invariant: daemon subprocess spawns reachable in app
//! mode must not inherit the TUI's controlling terminal.
//!
//! ## Why
//!
//! In app mode (`agend-terminal app` — the live daemon) the daemon runs
//! IN-PROCESS with the ratatui TUI. A child process spawned with `.status()`
//! / `.spawn()` and no stdout/stderr redirect inherits the TUI's controlling
//! terminal: its output writes straight onto the physical screen, diverging
//! from ratatui's previous-buffer. ratatui's diff then only repaints the
//! actively-changing cells, so the stray bytes persist as whole-frame garble
//! until a forced full redraw (the operator's tab-switch recovery). That is
//! the #2071 sighting (heavy concurrent agent output → a daemon-side git
//! subprocess leaked onto the frame).
//!
//! ## What it checks
//!
//! For each module in `MODULE_SCOPE`, scan the PRODUCTION portion only (up to
//! the first `#[cfg(test)]`). Each git `Command` builder that
//! terminates in `.status()` or `.spawn()` MUST redirect BOTH stdout and
//! stderr (`.stdout(..).stderr(..)`, typically `Stdio::null()`). A chain that
//! ends in `.output()` is inherently captured (pipes both streams) and is
//! safe. A site that genuinely must inherit the terminal can carry a
//! `// tty-inherit-allowed: <reason>` marker.
//!
//! ## Scope
//!
//! ALL of `src/` production code (every `.rs`, each scanned up to its first
//! `#[cfg(test)]`), EXCEPT `src/bin/` — those are standalone exec'd binaries
//! (notably the `agend-git` shim, whose entire job is to BE git and pass git's
//! output through to its caller's stdio); they never run in the in-process
//! TUI/daemon, so terminal inheritance there is correct by design, not a leak.
//!
//! ## Evasion limit (documented, per the invariant-completeness rule)
//!
//! The scanner reads forward from each literal git-`Command` constructor (the
//! `GIT_CMD` pattern) to its first terminator within a 30-line window — so a
//! rebound builder
//! (`let cmd = …; cmd.stdout(..); cmd.status();`) within that window IS
//! covered (its redirect lands in the accumulated chain). What it does NOT
//! catch: (a) a non-literal program name (`Command::new(git_var)`), (b) a
//! `Command` returned from a helper and terminated at a distant call site that
//! has no git-`Command` constructor on it, and (c) a terminator more than 30 lines
//! after the constructor. It pins the concrete #2071 shape — a direct git
//! spawn — not every conceivable TTY inheritance.

use std::path::{Path, PathBuf};

const SRC_DIR: &str = "src";
const CFG_TEST: &str = "#[cfg(test)]";
const GIT_CMD: &str = "Command::new(\"git\")";
const ALLOW_MARKER: &str = "tty-inherit-allowed:";

#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub snippet: String,
}

/// Index of the first `#[cfg(test)]` line, or `len` if none.
fn prod_boundary(lines: &[&str]) -> usize {
    lines
        .iter()
        .position(|l| l.trim_start().starts_with(CFG_TEST))
        .unwrap_or(lines.len())
}

/// True iff the line (or a contiguous `//` block immediately above) carries
/// the inherit-allowed marker.
fn has_allow_marker(lines: &[&str], line_idx: usize) -> bool {
    if lines
        .get(line_idx)
        .map(|l| l.contains(ALLOW_MARKER))
        .unwrap_or(false)
    {
        return true;
    }
    let mut cursor = line_idx;
    while let Some(prev) = cursor.checked_sub(1) {
        let Some(l) = lines.get(prev) else { break };
        let t = l.trim_start();
        if !t.starts_with("//") {
            break;
        }
        if t.contains(ALLOW_MARKER) {
            return true;
        }
        cursor = prev;
    }
    false
}

/// Scan one file's production portion for un-redirected git `.status()` /
/// `.spawn()`. Public so the RED/GREEN probes drive the SAME scanner.
pub fn scan_file(path: &str, content: &str) -> Vec<Violation> {
    let lines: Vec<&str> = content.lines().collect();
    let boundary = prod_boundary(&lines);
    let mut violations = Vec::new();
    const MAX_CHAIN_LINES: usize = 30;
    for idx in 0..boundary {
        if !lines[idx].contains(GIT_CMD) {
            continue;
        }
        // Walk the fluent builder chain forward to its terminator.
        let mut chain = String::new();
        for j in idx..boundary.min(idx + MAX_CHAIN_LINES) {
            chain.push_str(lines[j]);
            chain.push('\n');
            let has_status = lines[j].contains(".status(");
            let has_spawn = lines[j].contains(".spawn(");
            if lines[j].contains(".output(") && !has_status && !has_spawn {
                // Captured (both streams piped by `output()`) — safe.
                break;
            }
            if has_status || has_spawn {
                let redirected = chain.contains(".stdout(") && chain.contains(".stderr(");
                if !redirected && !has_allow_marker(&lines, idx) {
                    violations.push(Violation {
                        file: path.to_string(),
                        line: idx + 1,
                        snippet: lines[idx].trim().to_string(),
                    });
                }
                break;
            }
        }
    }
    violations
}

/// Locate the crate `src/` dir (cwd is the crate root under nextest, but fall
/// back to the workspace-nested path the way other repo invariants do).
fn src_root() -> PathBuf {
    let direct = PathBuf::from(SRC_DIR);
    if direct.is_dir() {
        return direct;
    }
    PathBuf::from("agend-terminal").join(SRC_DIR)
}

/// Recursively collect every `.rs` under `dir`, skipping any `bin/` subtree
/// (standalone exec'd binaries — see the module-doc Scope note).
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("bin") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            // Skip split test-module files (`#[cfg(test)] mod tests;` → a
            // sibling `tests.rs` with no inline `#[cfg(test)]`, so the
            // prod-boundary heuristic can't see it's test-only).
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem == "tests" || stem == "test" {
                continue;
            }
            out.push(path);
        }
    }
}

#[test]
fn app_reachable_git_spawns_redirect_stdio_2071() {
    let root = src_root();
    assert!(
        root.is_dir(),
        "could not locate src/ (cwd-relative) — got {}",
        root.display()
    );
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "scanned zero src files — path bug?");

    let mut violations = Vec::new();
    for path in &files {
        if let Ok(content) = std::fs::read_to_string(path) {
            violations.extend(scan_file(&path.display().to_string(), &content));
        }
    }
    assert!(
        violations.is_empty(),
        "#2071 TTY-leak invariant FAILED — {} git `.status()`/`.spawn()` site(s) in src/ \
         production lack a stdout+stderr redirect, so (when reachable in app mode) the child \
         inherits the TUI's terminal and garbles the frame. Fix: add \
         `.stdout(Stdio::null()).stderr(Stdio::null())` (or capture via `.output()`), or justify \
         with a `// {ALLOW_MARKER} <reason>` marker:\n{}",
        violations.len(),
        violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.file, v.line, v.snippet))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn scanner_flags_unredirected_status() {
    let src = "fn naughty() {\n    \
        let _ = std::process::Command::new(\"git\")\n        \
            .args([\"init\"])\n        \
            .status();\n}\n";
    let v = scan_file("synthetic.rs", src);
    assert_eq!(v.len(), 1, "must flag the un-redirected git status: {v:?}");
}

#[test]
fn scanner_accepts_redirected_status() {
    let src = "fn ok() {\n    \
        let _ = std::process::Command::new(\"git\")\n        \
            .args([\"init\"])\n        \
            .stdout(std::process::Stdio::null())\n        \
            .stderr(std::process::Stdio::null())\n        \
            .status();\n}\n";
    assert!(
        scan_file("synthetic.rs", src).is_empty(),
        "redirected stdout+stderr must pass"
    );
}

#[test]
fn scanner_accepts_output_capture() {
    // `.output()` pipes both streams → never inherits the terminal.
    let src = "fn ok() {\n    \
        let _ = std::process::Command::new(\"git\").args([\"status\"]).output();\n}\n";
    assert!(
        scan_file("synthetic.rs", src).is_empty(),
        "`.output()` is captured and safe"
    );
}

#[test]
fn scanner_respects_inherit_allowed_marker() {
    let src = "fn ok() {\n    \
        // tty-inherit-allowed: this runs only in the CLI, never under the TUI\n    \
        let _ = std::process::Command::new(\"git\").args([\"x\"]).status();\n}\n";
    assert!(
        scan_file("synthetic.rs", src).is_empty(),
        "explicit marker exempts the site"
    );
}

#[test]
fn scanner_ignores_cfg_test_portion() {
    let src = "fn prod() {}\n\
        #[cfg(test)]\n\
        mod tests {\n    \
            fn f() { let _ = std::process::Command::new(\"git\").args([\"x\"]).status(); }\n\
        }\n";
    assert!(
        scan_file("synthetic.rs", src).is_empty(),
        "test-only git spawns are exempt"
    );
}
