//! #879v4 RED tests — pin the two pre-existing bugs unmasked by PR #903
//! (then reverted via fe528c1) when always-Attached mode removed the in-process
//! `api::serve` shim that previously masked the daemon-side startup race.
//!
//! ## Bug 1 — daemon-side startup ordering race
//!
//! Before #879v4, `daemon::run_core` spawned all queued agents BEFORE publishing
//! `api.port`. With N agents each agent's MCP bridge could race against a missing
//! `api.port` file. The C1 reorder makes `daemon::run_core` publish `api.port`
//! (via `init_daemon_services`) BEFORE `spawn_fleet_agents`.
//!
//! The test measures this CAUSALLY, not by wall time: each agent runs a helper
//! that records — at its first start — whether `AGEND_HOME/run/*/api.port` already
//! exists, then stays alive. We assert every agent observed it (`1`). Under the
//! legacy agents-first ordering the first agents observe no `api.port` (`0` → RED);
//! under C1 every agent observes it (`1` → GREEN). Because the evidence is ORDER,
//! not elapsed time, a slow CI runner cannot flip the result — the fragile 1500 ms
//! wall-clock budget it replaces false-RED'd on a 1.607 s macOS run (#29281799014).
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

/// True while any member of process group `pgid` is alive (`kill(-pgid, 0)` → 0); false
/// once none remain (`-1`/ESRCH).
#[cfg(unix)]
fn group_alive(pgid: i32) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Poll until process group `pgid` is fully reaped (`kill(-pgid, 0)` → ESRCH), bounded.
#[cfg(unix)]
fn poll_group_reaped(pgid: i32, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if !group_alive(pgid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !group_alive(pgid)
}

/// SIGTERM → (bounded) SIGKILL a process group, then confirm it is reaped (ESRCH). An
/// already-dead group counts as reaped — the daemon's graceful shutdown may already have
/// reaped this PTY agent's session.
#[cfg(unix)]
fn reap_group(pgid: i32) -> bool {
    if !group_alive(pgid) {
        return true;
    }
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    if poll_group_reaped(pgid, Duration::from_secs(3)) {
        return true;
    }
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    poll_group_reaped(pgid, Duration::from_secs(5))
}

/// Bug 1 contract (CAUSAL, not wall-clock): the daemon MUST publish `api.port` before it
/// spawns any agent — i.e. before EVERY agent's FIRST start. Each agent runs a helper that
/// records whether `AGEND_HOME/run/*/api.port` already exists, then stays alive (`exec
/// sleep`) so it is never respawned. Each agent writes its OWN marker (keyed by
/// `AGEND_INSTANCE_NAME`) exactly once — a temp+rename, so the poller only ever reads a
/// COMPLETE marker. We poll for all 3 markers with a GENEROUS liveness bound (not a
/// correctness deadline) and assert every marker is `1`.
///
/// Under the legacy agents-first ordering the first agents observe NO api.port (`0` → RED);
/// under C1 (`init_daemon_services` publishes api.port before `spawn_fleet_agents`,
/// src/daemon/mod.rs run_core) every agent observes it (`1` → GREEN). The evidence is
/// ORDER, not elapsed time, so a slow CI runner cannot flip it — replacing the fragile
/// 1500 ms budget that false-RED'd on a 1.607 s macOS run (#29281799014).
///
/// Teardown must reap TWO kinds of group: portable-pty 0.9.0 (src/unix.rs:257) runs
/// `setsid()` on every spawned agent, so each PTY helper is its OWN session/pgid, NOT a
/// member of the daemon's group. The daemon runs under `setsid` in `pre_exec` (pid == pgid);
/// each helper records its own `$$` (its session pgid) before `exec sleep`. Teardown
/// SIGTERM/SIGKILLs the daemon group AND each recorded agent pgid, and PROVES every one is
/// reaped (`kill(-pgid, 0)` → ESRCH) before removing the tempdir (harness_smoke pattern).
#[cfg(unix)]
#[test]
fn api_port_published_before_every_agent_first_start() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
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
    let markers = tmp.join("agend-markers");
    std::fs::create_dir_all(&markers).unwrap();
    // Per-agent session pgid markers: portable-pty setsid's each agent, so teardown must reap
    // each agent's OWN session group (recorded below) independently of the daemon group.
    let pgids = tmp.join("agend-pgids");
    std::fs::create_dir_all(&pgids).unwrap();

    // Each agent's command is a first-start observer. It (1) records its own `$$` — portable-
    // pty 0.9.0 (src/unix.rs:257) runs setsid() per spawn, so this helper is its session
    // leader and `$$` == its PGID, which the following `exec sleep` preserves — then (2)
    // records whether api.port already exists, then `exec sleep` to stay alive (never
    // respawned). Each marker is keyed by AGEND_INSTANCE_NAME (unique per agent, single
    // writer) and published via temp+rename so the poller only reads a COMPLETE marker. PATH
    // is set explicitly because agent-backend env isolation may clear it.
    //
    // `set -eu` is LOAD-BEARING: the helper must NEVER reach `exec sleep` (stay alive) without
    // first publishing a VALID pgid marker — otherwise a failed pgid write would leave an
    // unreapable orphan while Rust fails closed on the missing pgid. So the pgid write is split
    // into separate statements (a bare `A && B` short-circuit can be treated as a tested
    // condition and NOT trip `set -e`); a failure exits BEFORE `exec sleep`. `set -u` also
    // fail-fasts on an unset AGEND_HOME/AGEND_INSTANCE_NAME. The `ls` stays inside the `if`, so
    // a glob miss (no api.port yet) yields v=0 rather than exiting.
    let helper = tmp.join("first-start-observer.sh");
    std::fs::write(
        &helper,
        "#!/bin/sh\n\
         set -eu\n\
         PATH=/bin:/usr/bin; export PATH\n\
         n=\"$AGEND_INSTANCE_NAME\"; h=\"$AGEND_HOME\"\n\
         printf '%s' \"$$\" > \"$h/agend-pgids/$n.tmp.$$\"\n\
         mv \"$h/agend-pgids/$n.tmp.$$\" \"$h/agend-pgids/$n\"\n\
         if ls \"$h\"/run/*/api.port >/dev/null 2>&1; then v=1; else v=0; fi\n\
         printf '%s' \"$v\" > \"$h/agend-markers/$n.tmp.$$\"\n\
         mv \"$h/agend-markers/$n.tmp.$$\" \"$h/agend-markers/$n\"\n\
         exec sleep 3600\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let agent = |n: &str| format!("{n}:{}", helper.display());
    let mut cmd = Command::new(&bin);
    cmd.arg("start")
        .arg("--foreground")
        .args(["--agents", &agent("t1")])
        .args(["--agents", &agent("t2")])
        .args(["--agents", &agent("t3")])
        .env("AGEND_HOME", &tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // stderr → null: the daemon logs to a file, early-exit diagnostics are not the
        // asserted contract, and a piped-but-undrained stderr could fill and deadlock the
        // daemon over the 30 s liveness window.
        .stderr(Stdio::null());
    // Own process group so teardown can reap the daemon and its SAME-GROUP descendants via
    // `kill(-pgid, …)`. The PTY agents setsid into their OWN groups (portable-pty unix.rs:257),
    // so they are NOT in this group — they are reaped SEPARATELY below via their recorded pgids.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn agend-terminal daemon");
    let pgid = child.id() as i32; // setsid ⇒ pid == pgid

    // GENEROUS liveness bound — we wait for all 3 agents to START ONCE, not race a
    // correctness deadline. The marker VALUES (not the timing) carry the ordering evidence.
    let names = ["t1", "t2", "t3"];
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !names.iter().all(|n| markers.join(n).exists()) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let observed: Vec<(&str, Option<String>)> = names
        .iter()
        .map(|n| {
            let v = std::fs::read_to_string(markers.join(n))
                .ok()
                .map(|s| s.trim().to_string());
            (*n, v)
        })
        .collect();

    // Recorded PTY-agent session pgids (portable-pty setsid'd each — NOT in the daemon group).
    // FAIL-CLOSED: read all 3 NAMED pgids; a missing/unreadable/malformed marker is kept as an
    // Err so the no-orphan proof below PANICS on it rather than silently skipping that group.
    // The helper writes the pgid marker before its value marker, so all 3 exist once liveness
    // holds.
    let agent_pgids: Vec<(&str, Result<i32, String>)> = names
        .iter()
        .map(|n| {
            let r = std::fs::read_to_string(pgids.join(n))
                .map_err(|e| format!("unreadable ({e})"))
                .and_then(|s| {
                    s.trim()
                        .parse::<i32>()
                        .map_err(|e| format!("malformed {s:?} ({e})"))
                });
            (*n, r)
        })
        .collect();

    // Teardown, BOUNDED (a `wait()` before SIGKILL would be unbounded if TERM is ignored).
    // (a) daemon group: SIGTERM, bounded loop watching the GROUP (`try_wait` clears the leader
    //     zombie so it does not count), SIGKILL backstop, then a bounded `wait`.
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    let term_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let _ = child.try_wait();
        if !group_alive(pgid) {
            break;
        }
        if Instant::now() >= term_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // SIGKILL the daemon group only if it is still alive — signalling an already-reaped
    // `-pgid` risks a pgid-reuse window hitting an unrelated group.
    if group_alive(pgid) {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
    let _ = child.wait();
    let daemon_reaped = poll_group_reaped(pgid, Duration::from_secs(5));
    // (b) each PTY agent's OWN session group (for the pgids we read). The daemon's graceful
    //     shutdown / PTY-master close (SIGHUP) may already have reaped it (`reap_group` treats
    //     an already-dead group as clean); otherwise this is the load-bearing reap of the
    //     `exec sleep` helpers the daemon group does NOT contain.
    let agent_reaped: Vec<(&str, Result<bool, String>)> = agent_pgids
        .iter()
        .map(|(n, r)| {
            (
                *n,
                r.as_ref().map(|pg| reap_group(*pg)).map_err(|e| e.clone()),
            )
        })
        .collect();
    let _ = std::fs::remove_dir_all(&tmp);

    // No-orphan proof: the daemon group AND every one of the 3 NAMED PTY agent groups are
    // proven reaped — a missing/malformed pgid FAILS the proof (never silently skipped).
    assert!(
        daemon_reaped,
        "teardown left the daemon process group {pgid} alive (kill(-pgid,0) never hit ESRCH)"
    );
    for (n, r) in &agent_reaped {
        match r {
            Ok(true) => {}
            Ok(false) => panic!(
                "teardown left agent {n}'s PTY session group alive — a helper `sleep` orphan survived"
            ),
            Err(e) => panic!(
                "agent {n} session pgid marker {e} — cannot prove its group reaped (fail-closed)"
            ),
        }
    }
    // Liveness: every agent must have started once within the bound.
    for (n, v) in &observed {
        assert!(
            v.is_some(),
            "liveness: agent {n} did not start within 30s (marker missing) — observed: {observed:?}"
        );
    }
    // Causal invariant: every agent observed api.port ALREADY PUBLISHED at its first start.
    for (n, v) in &observed {
        assert_eq!(
            v.as_deref(),
            Some("1"),
            "Bug 1: agent {n} observed api.port MISSING at its FIRST start — the legacy \
             agents-first ordering. init_daemon_services MUST publish api.port BEFORE \
             spawn_fleet_agents (src/daemon/mod.rs run_core C1). observed: {observed:?}"
        );
    }
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
