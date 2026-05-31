//! #1535 invariant: `CoreMutex` (src/sync_audit.rs) must be the SOLE mutex
//! wrapper for `AgentCore`. A bare `Mutex<AgentCore>` field or a
//! `Mutex::new(AgentCore { … })` construction bypasses the `CORE_LOCK_DEPTH`
//! tracking and silently reintroduces the #1492 core-lock-held self-IPC blind
//! spot that #1535 closed. This RED fails CI if any such bare usage returns.
//!
//! `CoreMutex` is scrubbed before matching (it textually contains
//! `Mutex<AgentCore>`), and comment/doc lines are skipped, so legitimate
//! `Arc<CoreMutex<AgentCore>>` usage and prose mentions don't false-positive.

use std::path::{Path, PathBuf};

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

#[test]
fn coremutex_is_sole_agentcore_mutex_wrapper_1535() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    // Bare (non-CoreMutex) AgentCore mutex patterns. CoreMutex wraps a generic
    // `parking_lot::Mutex<T>`, so its own definition never matches these.
    let needles = [
        "Mutex<AgentCore>",
        "Mutex<crate::agent::AgentCore>",
        "Mutex::new(AgentCore",
        "Mutex::new(crate::agent::AgentCore",
    ];

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read src file");
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue; // skip comment/doc lines that merely mention the pattern
            }
            // Scrub the allowed wrapper so `CoreMutex<AgentCore>` /
            // `CoreMutex::new(AgentCore` don't match the bare-`Mutex` needles.
            let scrubbed = line.replace("CoreMutex", "__CM__");
            for needle in &needles {
                if scrubbed.contains(needle) {
                    violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1535: bare `Mutex<AgentCore>` usage bypasses CoreMutex's CORE_LOCK_DEPTH \
         tracking → reopens the #1492 core-lock self-IPC blind spot. Use \
         `crate::sync_audit::CoreMutex` instead:\n{}",
        violations.join("\n")
    );
}
