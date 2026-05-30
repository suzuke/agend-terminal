//! #1504 L3: the agend-git shim's recursion guard hard-fails (exit 70) when the
//! propagated `AGEND_GIT_SHIM_DEPTH` sentinel reaches the cap, instead of
//! spawning git unbounded (the Windows fork-bomb: `exec_real_git` uses
//! `status()` = spawn, not exec-replace). Process-isolated, so it runs on every
//! OS and never mutates the test process env.

/// RED before the guard exists: depth=3 would pass through to real `git
/// --version` (exit 0) or, in the bug state, re-spawn the shim. GREEN: hard-fail
/// exit 70 with an actionable message.
#[test]
fn recursion_guard_hard_fails_at_max_depth_1504() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agend-git"))
        .env("AGEND_TEST_ISOLATION", "1")
        .env("AGEND_GIT_SHIM_DEPTH", "3")
        .arg("--version")
        .output()
        .expect("run agend-git shim");
    assert_eq!(
        out.status.code(),
        Some(70),
        "#1504: depth>=MAX must hard-fail with exit 70; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("recursion guard"),
        "#1504: guard must emit an actionable message, got: {stderr}"
    );
}

/// Control: below the cap the guard must NOT fire — proves it is depth-gated,
/// not an unconditional fail. (Exact non-70 code depends on git availability;
/// only the guard exit 70 is asserted against.)
#[test]
fn recursion_guard_does_not_fire_below_cap_1504() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agend-git"))
        .env("AGEND_TEST_ISOLATION", "1")
        .env("AGEND_GIT_SHIM_DEPTH", "2")
        .arg("--version")
        .output()
        .expect("run agend-git shim");
    assert_ne!(
        out.status.code(),
        Some(70),
        "#1504: depth below MAX must not trip the recursion guard; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
