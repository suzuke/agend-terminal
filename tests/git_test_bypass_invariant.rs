//! #1463 invariant — any TEST helper that runs a MUTATING git subcommand
//! against a scratch repo (`.current_dir(...)`) MUST set `AGEND_GIT_BYPASS=1`
//! on that Command. Otherwise, when an agent runs the suite (its env carries
//! `AGEND_INSTANCE_NAME`), the agend-git shim ChdirPass-redirects the commit
//! into the bound worktree — the #1463 init-pile pollution.
//!
//! Scope: only git Commands AFTER a file's `#[cfg(test)]` boundary are checked
//! — production git (before it) runs daemon-context with the shim inactive and
//! is exempt. Detection is per-statement (a git `Command` … `;` block)
//! and keys off LITERAL mutating subcommand tokens; helpers that pass the
//! subcommand via a variable (e.g. `git_in(dir, &["commit", …])`) already set
//! the bypass on the shared builder, so the literal-token scope is sufficient.

use std::path::Path;

/// Mutating subcommands whose ChdirPass redirect pollutes the worktree. Plumbing
/// `read-tree`/`update-index`/`apply` are omitted (not used by scratch fixtures);
/// `init`/`clone`/`config`/`rev-parse` are NON-mutating and intentionally absent.
const MUTATING: &[&str] = &[
    "\"commit\"",
    "\"add\"",
    "\"reset\"",
    "\"revert\"",
    "\"cherry-pick\"",
    "\"rebase\"",
    "\"merge\"",
    "\"am\"",
    "\"stash\"",
    "\"rm\"",
    "\"mv\"",
];

#[test]
fn test_scratch_mutating_git_sets_bypass_1463() {
    let mut violations = Vec::new();
    visit_dir(Path::new("src"), &mut violations);
    assert!(
        violations.is_empty(),
        "#1463: test helper(s) run mutating git in a scratch dir WITHOUT \
         AGEND_GIT_BYPASS=1 — the agend-git shim will redirect the commit into \
         the bound worktree (init-pile pollution). Add \
         `.env(\"AGEND_GIT_BYPASS\", \"1\")` to the Command:\n  {}",
        violations.join("\n  ")
    );
}

/// Self-check: the detector actually fires on a synthetic offending block.
#[test]
fn invariant_detects_synthetic_offender_1463() {
    // Build the git token from fragments so THIS file never contains the literal
    // the #821 git_subprocess_invariant scans for (we reference the pattern; we
    // never spawn git).
    let git_call = concat!("std::process::Command::new(", "\"git\")");
    let offending = format!(
        "\n        #[cfg(test)]\n        mod t {{\n            fn setup() {{\n                \
         {git_call}\n                    .args([\"commit\", \"--allow-empty\", \"-m\", \"init\"])\
         \n                    .current_dir(dir)\n                    .output()\n                    \
         .unwrap();\n            }}\n        }}\n    "
    );
    assert!(
        block_is_violation_in_test(&offending),
        "detector must flag a mutating scratch-git Command lacking the bypass"
    );
    let compliant = offending.replace(
        ".current_dir(dir)",
        ".current_dir(dir)\n.env(\"AGEND_GIT_BYPASS\", \"1\")",
    );
    assert!(
        !block_is_violation_in_test(&compliant),
        "detector must NOT flag once the bypass is present"
    );
}

fn visit_dir(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            visit_dir(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            check_file(&p, out);
        }
    }
}

fn check_file(path: &Path, out: &mut Vec<String>) {
    let Ok(src) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = src.lines().collect();
    // git below the first `#[cfg(test)]` is test code; above it is production.
    let Some(test_start) = lines.iter().position(|l| l.contains("#[cfg(test)]")) else {
        return; // no tests in this file → no agent-run pollution surface
    };
    for (i, l) in lines.iter().enumerate() {
        if i < test_start || !l.contains("Command::new(\"git\")") {
            continue;
        }
        if block_is_violation(&lines, i) {
            out.push(format!("{}:{}", path.display(), i + 1));
        }
    }
}

/// The git `Command` statement starting at `start` (gathered to the
/// first `;`) runs a mutating subcommand against a `.current_dir(...)` without
/// `AGEND_GIT_BYPASS`.
fn block_is_violation(lines: &[&str], start: usize) -> bool {
    let mut block = String::new();
    for l in &lines[start..] {
        block.push_str(l);
        block.push('\n');
        if l.contains(';') {
            break;
        }
    }
    let mutating = MUTATING.iter().any(|m| block.contains(m));
    mutating && block.contains(".current_dir(") && !block.contains("AGEND_GIT_BYPASS")
}

/// Helper for the self-check: does the snippet contain a violating git block
/// inside its `#[cfg(test)]` region?
fn block_is_violation_in_test(snippet: &str) -> bool {
    let lines: Vec<&str> = snippet.lines().collect();
    let Some(test_start) = lines.iter().position(|l| l.contains("#[cfg(test)]")) else {
        return false;
    };
    lines.iter().enumerate().any(|(i, l)| {
        i >= test_start && l.contains("Command::new(\"git\")") && block_is_violation(&lines, i)
    })
}
