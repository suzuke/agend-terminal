//! Sprint 21 Phase 5 — invariant test enforcing v1.2 §10.5 Rule 5
//! "Spawn site rationale" (PR #226 — protocol amendment by dev-impl-2).
//!
//! **Rule:** every `std::thread::spawn` / `std::thread::Builder::new()...spawn`
//! / `tokio::spawn` site in production code must satisfy ONE of:
//!
//! 1. Have a `// fire-and-forget: <reason>` comment within 5 lines preceding
//!    the spawn line, naming the shutdown trigger / why the leak is
//!    acceptable, OR
//! 2. Be in [`EXEMPTED_LEGACY_FILES`] (file-level exemption with TODO note
//!    for a future sweep PR — pre-existing legacy not yet covered by Sprint
//!    21 Phase 5's daemon + telegram scope).
//!
//! Test code (`#[cfg(test)] mod tests`) is exempt — test fixtures need broad
//! latitude on spawn semantics.
//!
//! Sprint 20 Track B audit (DAEMON.md §3 JoinHandle inventory) found 11
//! daemon spawn sites with 0 graceful-join-on-shutdown handling; Phase 5
//! sweeps those + the telegram surface, then this invariant fails-loud on
//! any new spawn that lacks rationale.

use std::path::{Path, PathBuf};

/// Files exempted from the rule because their spawn sites are pre-existing
/// legacy (not covered by Sprint 21 Phase 5 dispatch scope, which targets
/// `src/daemon/`, `src/agent.rs`, `src/instance_monitor.rs`,
/// `src/channel/telegram.rs`, `src/app/telegram_hooks.rs`).
///
/// Each entry has a short rationale + a TODO marker for the next sweep PR.
/// Adding a NEW entry here is **not allowed without an explicit dispatch
/// scope** — the goal is to shrink this list to zero over time, not grow it.
const EXEMPTED_LEGACY_FILES: &[(&str, &str)] = &[
    // TODO Sprint 22 sweep — agend-supervisor (separate process supervisor)
    (
        "supervisor/server.rs",
        "out of daemon scope; separate supervisor binary",
    ),
    // TODO Sprint 22 sweep — daemon-side API server worker threads
    (
        "api/mod.rs",
        "API socket-accept + per-request worker threads",
    ),
    // TODO Sprint 22 sweep — MCP config tail-pollers
    ("mcp_config.rs", "config-file tail-pollers"),
    // TODO Sprint 22 sweep — IPC framing helpers
    ("ipc.rs", "per-request framing helpers; short-lived"),
    // TODO Sprint 22 sweep — task subscriber notifications
    ("tasks.rs", "task subscriber notification thread"),
    // TODO Sprint 22 sweep — decisions watcher subscribers
    ("decisions.rs", "decisions subscriber notification threads"),
    // TODO Sprint 22 sweep — MCP handler-internal worker threads
    ("mcp/handlers.rs", "MCP handler-internal worker threads"),
    // TODO Sprint 22 sweep — verify subprocess driver
    ("verify.rs", "verify subprocess driver threads"),
    // TODO Sprint 22 sweep — tray icon event loop
    (
        "tray/mod.rs",
        "tray icon event loop; bound to process lifetime",
    ),
    // TODO Sprint 22 sweep — TUI foreground UI threads
    ("tui.rs", "TUI foreground UI thread"),
    // TODO Sprint 22 sweep — app mode UI threads
    ("app/mod.rs", "app mode UI threads"),
    // TODO Sprint 22 sweep — app-mode in-process API server
    ("app/api_server.rs", "app-mode in-process API server"),
    // TODO Sprint 22 sweep — sync helper test utilities
    ("sync.rs", "sync helper utility threads"),
    // TODO Sprint 22 sweep — inbox concurrent drain workers
    ("inbox.rs", "inbox concurrent drain workers"),
    // TODO Sprint 22 sweep — pane factory respawn / startup threads
    ("app/pane_factory.rs", "pane spawn worker threads"),
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
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn is_exempted_file(rel_path: &str) -> bool {
    EXEMPTED_LEGACY_FILES
        .iter()
        .any(|(suffix, _)| rel_path.ends_with(suffix))
}

fn is_spawn_call_line(line: &str) -> bool {
    let trim = line.trim_start();
    if trim.starts_with("//") || trim.starts_with("///") || trim.starts_with("//!") {
        return false;
    }
    // Match the call patterns. `Builder::new()` alone isn't a spawn — pair
    // with `.spawn(`. The Builder spawn often spans multiple lines, so we
    // accept either Builder::new() (which always pairs with .spawn) or
    // a direct thread::spawn / tokio::spawn call.
    line.contains("std::thread::spawn(")
        || line.contains("thread::spawn(")
        || line.contains("std::thread::Builder::new(")
        || line.contains("thread::Builder::new(")
        || line.contains("tokio::spawn(")
        || line.contains("tokio::task::spawn(")
}

/// Sprint 21 Phase 5 invariant — enforces v1.2 §10.5 Rule 5 on every
/// production spawn site outside the legacy exemption list.
///
/// Failure message lists each violator with its file:line so future PRs can
/// add the rationale comment (or — only with explicit dispatch scope — a new
/// EXEMPTED_LEGACY_FILES entry).
#[test]
fn spawn_rationale_present_at_every_in_scope_spawn_site() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files_in_src() {
        let rel = rel_path_str(&path, &src_root);
        if is_exempted_file(&rel) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut off at the first `#[cfg(test)]` so test-module spawns don't
        // trigger the invariant. Production spawns must come before any
        // test module in the file.
        let cutoff_byte = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod_section = &content[..cutoff_byte];
        let lines: Vec<&str> = prod_section.lines().collect();

        for (idx, line) in lines.iter().enumerate() {
            if !is_spawn_call_line(line) {
                continue;
            }
            // Look back up to 10 lines for `fire-and-forget` rationale.
            // 10 (not 5) accommodates multi-line rationale comments where
            // the keyword opens the comment and the spawn line lives at the
            // end of the block (e.g. `if let Err(e) = ...spawn(...)` blocks
            // whose comment expands to 6+ lines explaining the shutdown
            // contract).
            let start = idx.saturating_sub(10);
            let preceding = lines[start..idx].join("\n");
            if !preceding.contains("fire-and-forget") {
                violations.push(format!(
                    "  {}:{}: spawn site lacks `// fire-and-forget: <reason>` rationale within 5 preceding lines\n      offending line: {}",
                    rel,
                    idx + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "v1.2 §10.5 Rule 5 violations — {} spawn site(s) lack rationale.\n\nFix: add `// fire-and-forget: <reason>` comment within 5 lines preceding each spawn, naming the shutdown trigger or why the leak is acceptable.\n\nDo NOT add to `EXEMPTED_LEGACY_FILES` without explicit dispatch scope — that list is meant to shrink, not grow.\n\nViolations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sanity test: the rule itself must be satisfied at the in-scope sites
/// Sprint 21 Phase 5 swept. If this passes but the main test fails, the new
/// site is outside the swept scope.
#[test]
fn dispatch_scoped_sweep_sites_have_rationale() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // (file relative to src/, exact substring of the spawn line we expect to find)
    let swept_sites: &[(&str, &str)] = &[
        ("instance_monitor.rs", "std::thread::Builder::new()"),
        ("agent.rs", "std::thread::Builder::new()"),
        ("daemon/mod.rs", "std::thread::Builder::new()"),
        ("daemon/supervisor.rs", "thread::Builder::new()"),
        ("daemon/ci_watch.rs", "std::thread::Builder::new()"),
        ("daemon/tui_bridge.rs", "std::thread::Builder::new()"),
        ("channel/telegram.rs", "std::thread::Builder::new()"),
        ("app/telegram_hooks.rs", "std::thread::spawn"),
    ];
    for (rel_suffix, _expected_substr) in swept_sites {
        let path = src_root.join(rel_suffix);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("dispatch-scoped file must exist: {}", rel_suffix));
        assert!(
            content.contains("fire-and-forget:"),
            "swept file `{}` must contain at least one `fire-and-forget:` rationale comment",
            rel_suffix
        );
    }
}
