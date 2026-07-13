//! #879v4 RED tests — pin the two pre-existing bugs unmasked by PR #903
//! (then reverted via fe528c1) when always-Attached mode removed the in-process
//! `api::serve` shim that previously masked the daemon-side startup race.
//!
//! ## Bug 1 — daemon-side startup ordering race
//!
//! Before #879v4, `daemon::run_core` spawned all queued agents BEFORE starting
//! the `api::serve` thread (src/daemon/mod.rs:452-483). With N agents and the
//! default 500 ms spawn stagger, `api.port` could be missing for ~N×500 ms +
//! tens of ms while the agents' MCP bridges were already trying to connect.
//!
//! The RED test asserts `api.port` is published within a budget that's strictly
//! smaller than the agent-spawn loop's wall time. On main (HEAD = fe528c1, the
//! revert of #903) the test fails because the loop completes before
//! `api::serve` binds; after the C1 reorder it passes because the API server
//! starts before any agent is spawned.
//!
//! ## Bug 2 — bridge silent-degrade on tools/list error
//!
//! `src/bin/agend-mcp-bridge.rs:75` previously returned `{tools: []}` on ANY
//! `proxy_tools_list` error, including "no run dir" and "connection refused".
//! Operators saw an empty tool list with no clue why — same antipattern shape
//! as the #881 noop_guard regression but a different module.
//!
//! The RED test boots the bridge with an empty `AGEND_HOME` (no daemon
//! anywhere), sends an `initialize` + `tools/list`, and asserts the response
//! carries a visible JSON-RPC `error` field instead of a silent empty list. On
//! main the bridge replies `{result: {tools: []}}` immediately; after C2 it
//! retries for the bounded window (test override via
//! `AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS`) and then surfaces a -32603 error.

#![allow(clippy::unwrap_used)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
// `Instant` is only used by the unix-gated daemon-ordering test below;
// importing it unconditionally trips Windows clippy's `unused_imports`.
#[cfg(unix)]
use std::time::Instant;

const CLAUDE_CODE_INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;

fn bridge_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
    assert!(
        path.exists(),
        "bridge binary missing at {} — run `cargo build --bin agend-mcp-bridge` first",
        path.display()
    );
    path
}

#[cfg(unix)]
fn agend_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-terminal"));
    assert!(
        path.exists(),
        "agend-terminal binary missing at {} — run `cargo build --bin agend-terminal` first",
        path.display()
    );
    path
}

/// Scan `$AGEND_HOME/run/*` for the first run-dir containing `api.port`.
/// Mirrors `agend-mcp-bridge::find_run_dir` so the assertion uses the same
/// discovery contract as a real bridge would.
#[cfg(unix)]
fn find_api_port(home: &std::path::Path) -> Option<PathBuf> {
    let run_base = home.join("run");
    let entries = std::fs::read_dir(&run_base).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join("api.port").exists() {
            return Some(p.join("api.port"));
        }
    }
    None
}

/// Bug 1 contract: `api.port` MUST be published before the agent spawn loop
/// completes. With N=3 agents and a 1000 ms stagger, the legacy ordering
/// (agents-first) takes ~2 s before `api::serve` even starts; the budget here
/// is 1500 ms so the test fails on main and passes after C1.
#[cfg(unix)]
#[test]
fn api_port_published_before_agent_spawn_loop_completes() {
    let bin = agend_binary();
    let tmp = std::env::temp_dir().join(format!(
        "agend-879v4-daemon-order-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let start = Instant::now();
    let mut child = Command::new(&bin)
        .arg("start")
        .arg("--foreground")
        .args(["--agents", "t1:/bin/sh"])
        .args(["--agents", "t2:/bin/sh"])
        .args(["--agents", "t3:/bin/sh"])
        .env("AGEND_HOME", &tmp)
        // Pin the stagger so the legacy agents-first ordering takes a
        // deterministic ~2 s; the C1 reorder makes `api.port` appear
        // independently of this.
        .env("AGEND_SPAWN_STAGGER_MS", "1000")
        // Disable any background work that could compete for startup time
        // and inflate the legacy budget (false positives), and silence
        // logs going to a real $HOME log dir.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agend-terminal daemon");

    // Budget: well below 2 s (legacy wall time with N=3 + 1000 ms stagger)
    // and well above the C1-reordered actual wall time (~50-200 ms even on
    // slow CI). Poll cheaply so we don't add measurement noise.
    let deadline = start + Duration::from_millis(1500);
    let mut api_port_path: Option<PathBuf> = None;
    while Instant::now() < deadline {
        if let Some(p) = find_api_port(&tmp) {
            api_port_path = Some(p);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let elapsed = start.elapsed();

    // Best-effort teardown. SIGKILL is fine — the test owns the tempdir
    // and OS releases the .daemon.lock file lock on process death.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        api_port_path.is_some(),
        "Bug 1: api.port MUST be published before the agent spawn loop completes. \
         With N=3 agents at 1000ms stagger, the legacy agents-first ordering \
         delays api.port by ~2s; this test budget is 1500ms. Elapsed: {elapsed:?}. \
         Without C1 (api::serve spawn BEFORE agent loop in src/daemon/mod.rs), \
         the agents' mcp-bridges race against an unwritten api.port file."
    );
}

/// Bug 2 contract: when the daemon is unreachable, the bridge's `tools/list`
/// arm MUST surface a visible JSON-RPC error, NOT silently return
/// `{result: {tools: []}}`. Forcing the timeout small via
/// `AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS` keeps the test fast on CI.
#[test]
fn bridge_tools_list_no_silent_empty_when_daemon_unreachable() {
    let bridge = bridge_binary();
    let tmp = std::env::temp_dir().join(format!(
        "agend-879v4-bridge-silent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &tmp)
        .env("AGEND_INSTANCE_NAME", "test-silent-degrade")
        // C2 introduces this knob; on main (no C2) the bridge ignores it
        // and returns `{tools: []}` immediately. The contract this test
        // pins is the visible-error response, not the timeout duration.
        .env("AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS", "300")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    writeln!(stdin, "{CLAUDE_CODE_INIT}").expect("write init");
    writeln!(stdin, "{TOOLS_LIST}").expect("write tools/list");
    stdin.flush().expect("flush");
    drop(stdin);

    let (tx, rx) = mpsc::channel::<Vec<String>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let lines: Vec<String> = reader
            .lines()
            .take(2)
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .collect();
        let _ = tx.send(lines);
    });

    let lines = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("bridge must respond within 5s");

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(
        lines.len() >= 2,
        "expected init + tools/list responses, got {lines:?}"
    );
    let tools_resp: Value = serde_json::from_str(lines[1].trim()).unwrap_or_else(|e| {
        panic!(
            "tools/list response must be NDJSON, got {:?}: {e}",
            lines[1]
        )
    });

    let has_error = tools_resp.get("error").is_some();
    let has_empty_tools_result = tools_resp["result"]["tools"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(false);

    assert!(
        !has_empty_tools_result,
        "Bug 2: tools/list MUST NOT silently return `{{result: {{tools: []}}}}` \
         when daemon is unreachable. This silent-degrade pattern hides \
         bootstrap races from operators (see #881 for the same antipattern \
         in noop_guard). Got: {tools_resp}"
    );
    assert!(
        has_error,
        "Bug 2: tools/list MUST surface a visible JSON-RPC error when \
         daemon is unreachable, NOT a silent empty list. Got: {tools_resp}"
    );
}
