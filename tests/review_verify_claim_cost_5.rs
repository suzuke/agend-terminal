//! Static-invariant repro (verify-claim-cost batch), finding #5.
//!
//! `git_show_and_fmt` (`src/claim_verifier.rs`) writes the file bytes to
//! rustfmt's stdin via `let _ = stdin.write_all(&show.stdout);` — the write
//! error is DISCARDED — and then reads rustfmt's stdout WITHOUT checking the
//! exit status, returning whatever (possibly truncated) output was produced. A
//! partial write / broken pipe yields a truncated format that can silently flip
//! the 'only formatting' verdict (false reject, or worse false accept after
//! equal-truncation). Driving a broken pipe deterministically is not feasible
//! without first taking the fix, so this is a first-class source-scanning guard
//! (the codebase uses this method heavily, e.g. tests/core_mutex_invariant.rs).
//!
//! RED now: the swallowed-write pattern is present AND no rustfmt exit-status
//! guard exists in that function. Green after the fix propagates the write
//! error and treats a non-zero rustfmt exit as Err.

#![allow(clippy::expect_used)]

use std::path::PathBuf;

/// Extract the body of `fn git_show_and_fmt(...)` from the source so the scan
/// is scoped to that function (avoids false hits elsewhere).
fn git_show_and_fmt_body(src: &str) -> String {
    let start = src
        .find("fn git_show_and_fmt")
        .expect("git_show_and_fmt must exist in src/claim_verifier.rs");
    let after = &src[start..];
    // Take a generous window covering the function body. The fn is short; 2000
    // chars comfortably covers it without reaching the next unrelated fn's
    // guards. We stop at the next top-level `\nfn ` if present.
    let end = after[3..]
        .find("\nfn ")
        .map(|i| i + 3)
        .unwrap_or_else(|| after.len().min(2000));
    after[..end].to_string()
}

#[test]
fn git_show_and_fmt_does_not_swallow_stdin_write_error_verify_claim_cost() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("claim_verifier.rs");
    let text = std::fs::read_to_string(&path).expect("read src/claim_verifier.rs");
    let body = git_show_and_fmt_body(&text);

    // (1) The discarded-write anti-pattern must be gone. The fix propagates the
    //     error (`stdin.write_all(&show.stdout).map_err(...)?`) instead.
    let swallows_write = body.contains("let _ = stdin.write_all(");
    assert!(
        !swallows_write,
        "git_show_and_fmt discards rustfmt stdin write errors via \
         `let _ = stdin.write_all(...)`. A partial write / broken pipe then \
         feeds rustfmt a truncated file and the truncated format can silently \
         flip the 'only formatting' verdict. Propagate the write error with \
         `.map_err(...)?` (and close stdin before waiting)."
    );

    // (2) A non-zero rustfmt exit must be surfaced as Err, not returned as
    //     partial stdout. After the fix the function inspects the rustfmt exit
    //     status (`output.status`). Its presence is the guard.
    let checks_rustfmt_exit = body.contains("output.status");
    assert!(
        checks_rustfmt_exit,
        "git_show_and_fmt returns rustfmt stdout without inspecting `output.status`. \
         A non-zero rustfmt exit (e.g. after a truncated stdin) must become an \
         Err, not a partial formatted string. Check `output.status` and return \
         Err on failure."
    );
}
