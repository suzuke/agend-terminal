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
//! the first `#[cfg(test)]`). Each `Command::new("git")` builder that
//! terminates in `.status()` or `.spawn()` MUST redirect BOTH stdout and
//! stderr (`.stdout(..).stderr(..)`, typically `Stdio::null()`). A chain that
//! ends in `.output()` is inherently captured (pipes both streams) and is
//! safe. A site that genuinely must inherit the terminal can carry a
//! `// tty-inherit-allowed: <reason>` marker.
//!
//! ## Scope + evasion limit (documented, per the invariant-completeness rule)
//!
//! Scope is per-slice: the modules with a confirmed app-mode-reachable git
//! spawn (grow the list as more are found/migrated). The scanner reads the
//! contiguous fluent builder chain `Command::new("git")…terminator`; it does
//! NOT catch a chain split across a `let cmd = Command::new("git"); …;
//! cmd.status()` binding, nor non-git subprocesses. Those are out of this
//! guard's reach by design — it pins the concrete #2071 shape, not every
//! conceivable TTY inheritance.

use std::path::Path;

const SRC_DIR: &str = "src";
const CFG_TEST: &str = "#[cfg(test)]";
const GIT_CMD: &str = "Command::new(\"git\")";
const ALLOW_MARKER: &str = "tty-inherit-allowed:";

/// Modules with a confirmed app-mode-reachable raw git spawn (#2071). GROW
/// this list as further leak sites are found; never shrink it.
const MODULE_SCOPE: &[&str] = &[
    "mcp/handlers/dispatch_hook/mod.rs",
    "skills.rs",
    "instructions.rs",
];

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

fn read_module(rel: &str) -> Option<String> {
    let direct = Path::new(SRC_DIR).join(rel);
    std::fs::read_to_string(&direct)
        .or_else(|_| std::fs::read_to_string(Path::new("agend-terminal").join(SRC_DIR).join(rel)))
        .ok()
}

#[test]
fn app_reachable_git_spawns_redirect_stdio_2071() {
    let mut violations = Vec::new();
    for rel in MODULE_SCOPE {
        let Some(content) = read_module(rel) else {
            violations.push(Violation {
                file: (*rel).to_string(),
                line: 0,
                snippet: format!("MODULE_SCOPE entry not found: {rel}"),
            });
            continue;
        };
        violations.extend(scan_file(rel, &content));
    }
    assert!(
        violations.is_empty(),
        "#2071 TTY-leak invariant FAILED — {} git `.status()`/`.spawn()` site(s) reachable in \
         app mode lack a stdout+stderr redirect, so the child inherits the TUI's terminal and \
         garbles the frame. Fix: add `.stdout(Stdio::null()).stderr(Stdio::null())` (or capture \
         via `.output()`), or justify with a `// {ALLOW_MARKER} <reason>` marker:\n{}",
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
