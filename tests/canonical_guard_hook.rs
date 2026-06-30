//! e2e test for the L1 PreToolUse guard hook
//! (`scripts/claude-hooks/block-canonical-write.sh`): a Write/Edit/NotebookEdit
//! whose target is inside a managed canonical root is blocked (exit 2); a write to
//! a worktree (outside any root) is allowed (exit 0); and the hook fails OPEN
//! (exit 0) when no roots file is published. Unix-only (the hook is a bash + python3
//! script; the Windows CI job has neither in the same shape).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

fn script_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/claude-hooks/block-canonical-write.sh")
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the hook with `AGEND_HOME=home` and the given PreToolUse JSON on stdin;
/// return its exit code.
fn run_hook(home: &std::path::Path, stdin_json: &str) -> i32 {
    let mut child = Command::new("bash")
        .arg(script_path())
        .env("AGEND_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .unwrap();
    child.wait().unwrap().code().unwrap_or(-1)
}

fn tool_json(file_path: &std::path::Path) -> String {
    format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":{:?}}}}}"#,
        file_path.to_string_lossy()
    )
}

#[test]
fn blocks_write_into_canonical_root_allows_worktree_and_fails_open() {
    if !python3_available() {
        eprintln!("python3 unavailable — hook fails open; skipping e2e assertions");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("agend-l1-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let canonical = tmp.join("canonical-repo");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::write(
        tmp.join("canonical-roots.json"),
        format!("[{:?}]", canonical.to_string_lossy()),
    )
    .unwrap();

    // 1. Write INTO the canonical working tree (the SESSION-HANDOFF-006.md class) → blocked.
    let inside = canonical.join("SESSION-HANDOFF-006.md");
    assert_eq!(
        run_hook(&tmp, &tool_json(&inside)),
        2,
        "a write inside a canonical root must be blocked (exit 2)"
    );

    // 2. Write into a worktree (NOT under any canonical root) → allowed.
    let worktree_file = tmp.join("worktree").join("src").join("lib.rs");
    assert_eq!(
        run_hook(&tmp, &tool_json(&worktree_file)),
        0,
        "a write outside every canonical root (a worktree) must be allowed (exit 0)"
    );

    // 3. No roots file published → fail OPEN (advisory guard must not block).
    let empty_home = tmp.join("empty-home");
    std::fs::create_dir_all(&empty_home).unwrap();
    assert_eq!(
        run_hook(&empty_home, &tool_json(&inside)),
        0,
        "with no canonical-roots.json the hook must fail open (exit 0)"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
