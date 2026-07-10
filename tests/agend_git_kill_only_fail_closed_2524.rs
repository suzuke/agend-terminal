//! #2524 P3b: agend-git is now KILL-only. Any non-kill invocation — especially a
//! `git`-style call reaching agend-git via a flag-OFF or agentic-missing
//! PATH-shadow — MUST fail closed: non-zero exit + an ACTIONABLE breadcrumb,
//! never a silent passthrough to real git (which would bypass ALL agentic-git
//! enforcement).
//!
//! RED before P3b: agend-git handled `status` as a git subcommand and (as a
//! non-agent / unbound caller) passed through to real git → exit 0, no message.
//! GREEN after: fail-closed non-zero exit with the migration breadcrumb.

use std::process::Command;

/// A git-style invocation of the kill-only agend-git binary must fail closed with
/// a non-zero exit and an actionable message. argv[0] is the binary path
/// (basename `agend-git`, not a kill name), so the kill dispatch is skipped and
/// the fail-closed arm fires.
#[test]
fn non_kill_git_invocation_fails_closed_with_actionable_message() {
    let out = Command::new(env!("CARGO_BIN_EXE_agend-git"))
        .arg("status")
        .output()
        .expect("spawn agend-git");

    assert!(
        !out.status.success(),
        "a git-style (non-kill) invocation must exit non-zero (fail closed), got {:?}",
        out.status
    );

    // Pin the actionable breadcrumb (lead vet): every agent may hit this wall, so
    // the message is the operator's first-line diagnosis.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no longer serves git"),
        "fail-closed message must state agend-git no longer serves git; got: {stderr}"
    );
    assert!(
        stderr.contains("agentic-git"),
        "fail-closed message must name the expected route (agentic-git); got: {stderr}"
    );
    assert!(
        stderr.contains("revert the P3b commit") || stderr.contains("vendor/agentic-git"),
        "fail-closed message must give an actionable fix (rebuild/revert); got: {stderr}"
    );
}
