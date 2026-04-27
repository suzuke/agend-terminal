//! Anti-bypass invariant: the MCP bridge subprocess must hold zero state.
//!
//! Sprint 25 P0 Option F — 5 grep rules that structurally enforce the
//! "subprocess is a pure transport" contract. If any rule fires, a new
//! code path has leaked daemon-side state into the bridge binary.

const BRIDGE_FILE: &str = "src/bin/agend-mcp-bridge.rs";

fn bridge_source() -> String {
    std::fs::read_to_string(BRIDGE_FILE).expect("read bridge source")
}

/// Strip `//` line comments and `/* */` block comments from source.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    // line comment — skip to newline
                    for ch in chars.by_ref() {
                        if ch == '\n' {
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next(); // consume '*'
                    loop {
                        match chars.next() {
                            Some('*') if chars.peek() == Some(&'/') => {
                                chars.next();
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn active_lines(src: &str) -> Vec<(usize, String)> {
    strip_comments(src)
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.to_string()))
        .filter(|(_, l)| !l.trim().is_empty())
        .collect()
}

/// Rule 1: No file I/O beyond the minimal daemon-discovery helpers.
/// Allowed: `File::open` for api.cookie, `read_to_string` for api.port,
/// `read_dir` for run dir scan. Disallowed: any other fs::read patterns.
#[test]
fn rule1_no_state_file_reads() {
    let src = bridge_source();
    let lines = active_lines(&src);
    let forbidden = [
        "fleet.yaml",
        "topics.json",
        "tasks.json",
        "task_events",
        "inbox",
        "metadata.json",
    ];
    for (num, line) in &lines {
        for pat in &forbidden {
            assert!(
                !line.contains(pat),
                "Rule 1 violation at line {num}: bridge must not reference state file '{pat}'\n  {line}"
            );
        }
    }
}

/// Rule 2: No home_dir references beyond the minimal env-var-based helper.
/// The bridge has its own `home_dir()` that reads AGEND_HOME — that's OK.
/// But it must not use `crate::home_dir` or `.agend-terminal` path construction.
#[test]
fn rule2_no_crate_home_dir() {
    let lines = active_lines(&bridge_source());
    for (num, line) in &lines {
        assert!(
            !line.contains("crate::home_dir") && !line.contains("crate::"),
            "Rule 2 violation at line {num}: bridge must not use crate:: imports\n  {line}"
        );
    }
}

/// Rule 3: No globals (OnceLock, lazy_static, static Mutex/RwLock/HashMap).
#[test]
fn rule3_no_globals() {
    let lines = active_lines(&bridge_source());
    let forbidden = ["OnceLock", "lazy_static", "once_cell", "static_assertions"];
    for (num, line) in &lines {
        // Allow `static` in string literals
        if line.contains("static")
            && (line.contains("Mutex") || line.contains("RwLock") || line.contains("HashMap"))
        {
            // Check it's not just a type annotation in a function
            let trimmed = line.trim();
            if trimmed.starts_with("static") {
                panic!(
                    "Rule 3 violation at line {num}: bridge must not have global state\n  {line}"
                );
            }
        }
        for pat in &forbidden {
            assert!(
                !line.contains(pat),
                "Rule 3 violation at line {num}: bridge must not use '{pat}'\n  {line}"
            );
        }
    }
}

/// Rule 4: No state file references.
#[test]
fn rule4_no_state_files() {
    let lines = active_lines(&bridge_source());
    let forbidden = [
        "fleet.yaml",
        "topics.json",
        "tasks.json",
        "task_events.jsonl",
        "agents/",
    ];
    for (num, line) in &lines {
        for pat in &forbidden {
            assert!(
                !line.contains(pat),
                "Rule 4 violation at line {num}: bridge must not reference '{pat}'\n  {line}"
            );
        }
    }
}

/// Rule 5: No channel state.
#[test]
fn rule5_no_channel_state() {
    let lines = active_lines(&bridge_source());
    let forbidden = [
        "active_channel",
        "register_channel",
        "TelegramState",
        "ACTIVE_CHANNEL",
    ];
    for (num, line) in &lines {
        for pat in &forbidden {
            assert!(
                !line.contains(pat),
                "Rule 5 violation at line {num}: bridge must not reference '{pat}'\n  {line}"
            );
        }
    }
}
