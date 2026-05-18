//! #879v3 PR2 invariants — pin the canonical self-spawn topology so future
//! regressions catch at test time, not at production-fork-bomb time.
//!
//! Reviewer pushback #2 (per dispatch): "new spawn entrypoint without depth
//! accounting MUST fail". These tests scan the source tree and enforce two
//! load-bearing rules:
//!
//! 1. Every self-spawn site funnels through
//!    [`bootstrap::spawn_depth::canonical_spawn_args`] — no caller may
//!    hardcode `"--foreground"` by hand and bypass the
//!    `AGEND_SPAWN_DEPTH` increment + args/env invariants. The SPEC
//!    helper is import-clean for tray (per #548 Q7) and reused by
//!    `bootstrap::daemon_spawn::spawn_detached` for the CLI Start + app
//!    auto-spawn paths.
//!
//! 2. The canonical spec helper itself sets `AGEND_SPAWN_DEPTH` on the
//!    returned spec's env. If a future refactor removes the env set,
//!    this test fails with a clear message naming the safeguard.
//!
//! Why "scan the source tree" rather than "scan compiled artifacts": grep-
//! at-test-time is the same technique `tests/issue_548_phase2_invariants.rs`
//! uses for the post-#548 tray-spawn topology, and it catches both the
//! "new call site forgets the rule" regression AND the "rule removed from
//! the canonical helper" regression at zero runtime cost.

use std::fs;
use std::path::Path;

/// Files that LEGITIMATELY contain the canonical-spawn building blocks —
/// the spec helper itself, plus its tests, plus the test fixture you're
/// reading now. Every other source file must source its args + env via
/// `bootstrap::spawn_depth::canonical_spawn_args(...).apply_to(&mut cmd)`
/// rather than hardcoding `"--foreground"` (or other invariant args)
/// directly.
const CANONICAL_SPAWN_OWNERS: &[&str] = &[
    "src/bootstrap/spawn_depth.rs",
    "tests/issue_879v3_canonical_spawn_invariants.rs",
];

fn workspace_root() -> &'static Path {
    // Cargo sets CARGO_MANIFEST_DIR to the package root at build time.
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read_text(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn iter_rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip target/ — built artifacts contain stringified source.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            out.extend(iter_rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Reviewer-required invariant (per dispatch pushback #2): if a new code
/// path adds `Command::new(...).arg("start").arg("--foreground")` directly
/// without routing through `canonical_spawn_daemon`, it would silently
/// disable the `AGEND_SPAWN_DEPTH` increment for that path — same class as
/// the bug that broke #882.
///
/// Pre-fix state (e.g. tray/mod.rs:154 pre-PR2): this test FAILS — tray
/// builds its own Command with `.arg("start").arg("--foreground").spawn()`.
/// Post-fix: tray funnels through `canonical_spawn_daemon`, no raw build.
#[test]
fn new_spawn_entrypoint_without_depth_accounting_fails() {
    let workspace = workspace_root();
    let src_dir = workspace.join("src");
    let mut offenders = Vec::new();

    for path in iter_rust_files(&src_dir) {
        let rel = path
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.display().to_string());

        if CANONICAL_SPAWN_OWNERS.iter().any(|owner| rel == *owner) {
            continue;
        }
        // Skip tests modules inside src — they may exercise spawn-shaped
        // mocks. The actual invariant we care about is production code.
        // Tests live under `tests/` and are handled separately.
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        // Smoking-gun signature: any hardcoded `"--foreground"` string
        // literal. After #879v3 PR2's SPEC refactor, the only legitimate
        // place for that literal is inside `canonical_spawn_args` (in
        // `bootstrap::spawn_depth.rs`). Every other self-spawn caller
        // must source the args from the spec via `.apply_to(...)`, NOT
        // hardcode `--foreground` (or `.arg("--foreground")`) inline.
        // Catches both shapes: `.arg("--foreground")` (the old shape) and
        // `["start", "--foreground", ...]` Vec literals (the new shape if
        // someone re-introduces an inline build).
        //
        // We deliberately do NOT key on `"start"` alone, because
        // `Command::new("cmd").arg("/c").arg("start")` is the Windows
        // cmd.exe builtin used by `src/tray/terminal/windows.rs` to
        // detach EXTERNAL terminals — unrelated to recursion guard.
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if trimmed.contains("\"--foreground\"") {
                offenders.push(format!("{rel}:{}", lineno + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "#879v3 invariant violation — these source locations call \
         `Command::new(...).arg(\"start\")` directly instead of routing \
         through `bootstrap::daemon_spawn::canonical_spawn_daemon`. \
         Doing so silently disables the AGEND_SPAWN_DEPTH guard for that \
         path (same bug class as #882's fork bomb). Refactor each to use \
         the canonical helper.\n\nOffenders: {offenders:?}"
    );
}

/// Pin the canonical spec helper's body so a future refactor can't
/// accidentally remove the depth increment. If `canonical_spawn_args`
/// ever stops setting the `AGEND_SPAWN_DEPTH` env entry on the returned
/// spec, this test fails with a message that points at the safeguard.
#[test]
fn canonical_spawn_args_sets_depth_env_on_spec() {
    let text = read_text("src/bootstrap/spawn_depth.rs");
    assert!(
        text.contains("pub fn canonical_spawn_args("),
        "src/bootstrap/spawn_depth.rs must declare canonical_spawn_args"
    );
    assert!(
        text.contains("ENV_KEY.to_string(), next_depth.to_string()"),
        "canonical_spawn_args must push (ENV_KEY, next_depth.to_string()) \
         into the spec's env — otherwise children spawn at the parent's \
         depth and the recursion guard fails to advance the counter"
    );
    assert!(
        text.contains("let next_depth = check()?;"),
        "canonical_spawn_args must call `check()?` first — bailing before \
         allocating any further resources is the whole point of the \
         fork-bomb guard"
    );
}

/// #879v3 C2.6 invariant: no source file may build `api_server::noop_guard()`
/// from inside an `Err(_) =>` (or `Err(e) =>`) match arm — that's the
/// silent-degrade shape that produced the #881 `{tools:[]}` symptom.
///
/// Generalized framing per reviewer pushback #3: "any bootstrap Err path
/// silently degrades MCP". The scanner is line-window based: any line that
/// matches an `Err(...)` arm followed within a small window by `noop_guard()`
/// counts as a violation.
///
/// `noop_guard()` itself remains legitimate for the OK-Attached arm (where
/// the daemon owns the run dir). The signature we forbid is specifically
/// "Err arm → noop_guard".
#[test]
fn invariant_no_noop_guard_in_err_arms() {
    let workspace = workspace_root();
    let src_dir = workspace.join("src");
    let mut offenders = Vec::new();

    for path in iter_rust_files(&src_dir) {
        let rel = path
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.display().to_string());
        // The helper module that DEFINES noop_guard is naturally excluded
        // — its body returns the noop, but that's not an Err arm.
        if rel == "src/app/api_server.rs" {
            continue;
        }
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            // Detect an `Err(...)` arm head: starts with `Err(` and ends
            // with `=>` somewhere on the same line. Skip pattern matches
            // in non-match contexts by also requiring an `=>`.
            let is_err_arm_head = trimmed.starts_with("Err(") && trimmed.contains("=>");
            if !is_err_arm_head {
                continue;
            }
            // Scan the next ~30 lines (a generous arm body) for `noop_guard(`.
            let end = (idx + 30).min(lines.len());
            for body_line in &lines[idx + 1..end] {
                let body_trim = body_line.trim_start();
                if body_trim.starts_with("//") || body_trim.starts_with("///") {
                    continue;
                }
                if body_trim.contains("noop_guard(") {
                    offenders.push(format!("{rel}:{}", idx + 1));
                    break;
                }
                // Heuristic stop: the closing `}` at top-of-line of the
                // match block (the line is just `}` or `};`) ends the arm.
                if body_trim == "}" || body_trim == "};" || body_trim == "}," {
                    break;
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "#879v3 C2.6 invariant violation — `noop_guard()` appears inside an \
         `Err(_) =>` match arm. This is the silent-degrade shape that produced \
         the #881 `{{tools:[]}}` symptom: the TUI runs with no in-process API \
         server, and MCP tool registration sees an empty surface. The correct \
         response to a bootstrap Err is `cleanup_on_bail(...)` + `return Err(...)`. \
         \n\nOffenders: {offenders:?}"
    );
}

/// #879v3 safeguard 3 + 7 surface pin: the `agend-terminal status` /
/// `list` output MUST include the `agend_spawn_depth_guard_fires` key
/// (JSON shape) or the `AGEND_SPAWN_DEPTH guard fires:` line (plain
/// shape) so soak-window monitoring scripts can read the counter
/// without parsing daemon.log. Pre-fix (review at 821b27d): grep for
/// `fire_count(` outside `src/bootstrap/spawn_depth.rs` returned ZERO
/// hits, which is the gap reviewer flagged.
///
/// Post-fix: `src/main.rs`'s `Commands::List` arm reads
/// `spawn_depth::fire_count()` and surfaces it in every output branch
/// (json, detailed-text, plain-list, no-daemon).
#[test]
fn main_status_command_surfaces_guard_fire_counter() {
    let text = read_text("src/main.rs");
    assert!(
        text.contains("spawn_depth::fire_count()"),
        "src/main.rs must call `crate::bootstrap::spawn_depth::fire_count()` \
         inside the Commands::List arm so `agend-terminal status` surfaces \
         the AGEND_SPAWN_DEPTH guard counter for soak monitoring"
    );
    assert!(
        text.contains("agend_spawn_depth_guard_fires"),
        "src/main.rs must surface the guard counter under the \
         `agend_spawn_depth_guard_fires` JSON key — soak scripts grep / jq \
         for this exact name"
    );
    assert!(
        text.contains("AGEND_SPAWN_DEPTH guard fires:"),
        "src/main.rs must emit the `AGEND_SPAWN_DEPTH guard fires:` plain \
         line so operators reading `agend-terminal status` (without --json) \
         see the counter"
    );
}

/// Pin the deny-list: `AGEND_SPAWN_DEPTH` must be a sensitive env key, so
/// fleet.yaml templates and host env CANNOT override the depth value
/// (would create a back door to disable the guard).
#[test]
fn agend_spawn_depth_is_on_sensitive_env_deny_list() {
    let text = read_text("src/agent.rs");
    assert!(
        text.contains("\"AGEND_SPAWN_DEPTH\""),
        "AGEND_SPAWN_DEPTH must appear in SENSITIVE_ENV_KEYS at \
         src/agent.rs — fleet.yaml override would silently disable the \
         #879v3 fork-bomb guard"
    );
}
