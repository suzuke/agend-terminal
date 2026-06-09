//! #1900 — e2e self-verification harness (MVP).
//!
//! ONE hermetic scenario through a REAL daemon on an isolated `AGEND_HOME` (never
//! `~/.agend-terminal`) + a local git repo (no network): lead dispatches a task to
//! mock-dev with `branch=B` + `next_after_ci=mock-reviewer`; the dispatch provisions
//! a bound worktree; mock-dev commits + reports.
//!
//! It asserts four "seam" invariants — the MCP-received-directive-dropped-before-
//! the-mechanism class this sprint kept breaking (#931 / #1833 / #1877):
//!   ① directive-survival — the bound worktree is on branch B, NOT main  [#1833]
//!   ② worktree-bind      — bound to mock-dev, no cross-agent leak
//!   ③ report-route       — mock-dev's report reaches lead's inbox
//!   ④ chain-armed        — `next_after_ci` survived into the ci-watch sidecar  [#931/#1877]
//!
//! B1 harness-driven: each agent's actions are driven via
//! `api_call({method:"mcp_tool", params:{instance:…}})` — the SAME daemon code path
//! a real agent's MCP-bridge call takes — so no mock-agent binary is needed (that
//! is the Phase-2 autonomous-agent fidelity upgrade). Hermetic: the `/tmp` home is
//! asserted-tmp on creation and removed on teardown; the daemon never touches the
//! real `~/.agend-terminal`.
//!
//! Unix-only: the mock agents idle on `/bin/cat` and the fixture drives POSIX
//! git/paths. Gating the whole file (not just the test fn) keeps the helper fns
//! from being dead code under `-D warnings` on the Windows CI runner.
#![cfg(unix)]

mod common;

use common::git_isolated;
use common::harness::AgendHarness;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const BRANCH: &str = "feat/e2e-b";
const REPO_SLUG: &str = "e2e-org/e2e-repo";

/// Run git in `dir` via the canonical isolated helper (cwd isolation + shim
/// bypass + pinned identity, per #821) and assert success.
fn git(dir: &Path, args: &[&str]) {
    let out = git_isolated::git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_isolated::git(dir, args).stdout)
        .trim()
        .to_string()
}

/// Hermetic source repo (a clone of a local bare origin) with one commit on
/// `main`. mock-dev's worktrees are created from this. No network.
fn setup_source_repo(base: &Path) -> PathBuf {
    let origin = base.join("origin.git");
    std::fs::create_dir_all(&origin).expect("mkdir origin");
    git(&origin, &["init", "--bare", "-b", "main"]);

    let src = base.join("source");
    std::fs::create_dir_all(&src).expect("mkdir source");
    git(&src, &["init", "-b", "main"]);
    git(
        &src,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    std::fs::write(src.join("README.md"), "e2e\n").expect("write README");
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "init"]);
    git(&src, &["push", "-u", "origin", "main"]);
    src
}

/// One authenticated daemon API round-trip: the P1-10 `{"auth":"<hex>"}` cookie
/// handshake (the daemon rejects any request before it), then the NDJSON request.
/// (`AgendHarness::api_call` is dead code that predates the handshake — it sends an
/// inline `cookie` field the server no longer accepts — so the e2e drives the
/// socket directly, like the harness's own `authed_api_call`.)
fn call_api(h: &AgendHarness, request: &Value) -> Value {
    use std::io::{BufRead, BufReader, Write};
    let stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", h.api_port))
        .unwrap_or_else(|e| panic!("connect daemon api: {e}"));
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .ok();
    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);

    // The harness owns the only run dir under <home>/run/<pid>/.
    let run_dir = std::fs::read_dir(h.home.join("run"))
        .expect("run dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("api.cookie").exists())
        .expect("run dir with api.cookie");
    let cookie = std::fs::read(run_dir.join("api.cookie")).expect("read cookie");
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(writer, r#"{{"auth":"{hex}"}}"#).expect("write auth");
    writer.flush().ok();
    let mut auth_resp = String::new();
    reader.read_line(&mut auth_resp).expect("read auth resp");

    writeln!(
        writer,
        "{}",
        serde_json::to_string(request).expect("serialize request")
    )
    .expect("write request");
    writer.flush().ok();
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read response");
    serde_json::from_str(resp.trim()).unwrap_or_else(|e| panic!("parse response '{resp}': {e}"))
}

/// Drive an MCP tool AS `instance` through the real daemon API.
fn mcp_tool(h: &AgendHarness, instance: &str, tool: &str, arguments: Value) -> Value {
    call_api(
        h,
        &json!({
            "method": "mcp_tool",
            "params": {"tool": tool, "arguments": arguments, "instance": instance}
        }),
    )
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

/// The auto-armed ci-watch sidecar for `repo@branch` (under `<home>/ci-watches/`).
fn read_ci_watch(home: &Path, branch: &str) -> Value {
    let dir = home.join("ci-watches");
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("ci-watches dir {}: {e}", dir.display()));
    for ent in entries.flatten() {
        if ent.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Ok(v) =
            serde_json::from_str::<Value>(&std::fs::read_to_string(ent.path()).unwrap_or_default())
        {
            if v["branch"].as_str() == Some(branch) {
                return v;
            }
        }
    }
    panic!(
        "no ci-watch sidecar for branch {branch} in {}",
        dir.display()
    );
}

#[test]
fn e2e_dispatch_review_workflow_seam_invariants_1900() {
    // ── hermetic isolated home — MUST be a /tmp dir, NEVER ~/.agend-terminal ──
    let home = std::env::temp_dir().join(format!(
        "agend-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("mkdir home");
    assert!(
        home.starts_with(std::env::temp_dir()),
        "hermetic: the e2e home must be under the tmp dir, got {}",
        home.display()
    );

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_scenario(&home)));

    // Hermetic teardown — always remove the /tmp home, even on assertion failure.
    if std::env::var("AGEND_E2E_KEEP").is_err() {
        std::fs::remove_dir_all(&home).ok();
    } else {
        eprintln!("AGEND_E2E_KEEP set — home preserved at {}", home.display());
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_scenario(home: &Path) {
    let src = setup_source_repo(home);

    // mock agents idle on `/bin/cat` (blocks on the PTY, never crashes). B1 drives
    // their MCP tools directly, so they need not be "ready" — only routable.
    let fleet = format!(
        "instances:\n  \
         lead:\n    command: /bin/cat\n  \
         mock-dev:\n    command: /bin/cat\n    source_repo: {src}\n  \
         mock-reviewer:\n    command: /bin/cat\n\
         teams:\n  e2e:\n    members: [lead, mock-dev, mock-reviewer]\n    orchestrator: lead\n",
        src = src.display(),
    );

    let h = AgendHarness::spawn_with(home.to_path_buf(), &fleet, "start").expect("daemon boot");

    // ── lead creates a task, then dispatches mock-dev (branch=B, next_after_ci) ──
    let created = mcp_tool(
        &h,
        "lead",
        "task",
        json!({"action": "create", "title": "e2e review"}),
    );
    let task_id = created["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no task id in create response: {created}"))
        .to_string();

    let dispatch = mcp_tool(
        &h,
        "lead",
        "send",
        json!({
            "instance": "mock-dev",
            "message": "implement on the branch",
            "request_kind": "task",
            "task_id": task_id,
            "branch": BRANCH,
            "repository": REPO_SLUG,
            "next_after_ci": "mock-reviewer",
        }),
    );
    assert_eq!(
        dispatch["ok"].as_bool(),
        Some(true),
        "dispatch failed: {dispatch}"
    );

    // ── SEAM ① directive-survival: the bound worktree is on branch B, not main ──
    let binding = read_json(&home.join("runtime").join("mock-dev").join("binding.json"));
    assert_eq!(
        binding["branch"].as_str(),
        Some(BRANCH),
        "#1833 directive-survival: binding must record branch B: {binding}"
    );
    let wt = binding["worktree"]
        .as_str()
        .expect("worktree path in binding")
        .to_string();
    let head = git_stdout(Path::new(&wt), &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(
        head, BRANCH,
        "#1833 directive-survival: the bound worktree HEAD must be branch B, not '{head}'"
    );

    // ── SEAM ② worktree-bind: bound to mock-dev, no cross-agent leak ──
    for other in ["lead", "mock-reviewer"] {
        let p = home.join("runtime").join(other).join("binding.json");
        if p.exists() {
            assert_ne!(
                read_json(&p)["branch"].as_str(),
                Some(BRANCH),
                "worktree-bind leak: {other} must NOT be bound to branch B"
            );
        }
    }

    // ── SEAM ④ chain-armed: next_after_ci survived into the ci-watch sidecar ──
    let watch = read_ci_watch(home, BRANCH);
    assert_eq!(
        watch["next_after_ci"].as_str(),
        Some("mock-reviewer"),
        "#931/#1877 chain-armed: the auto-armed ci-watch must carry next_after_ci=mock-reviewer: {watch}"
    );

    // ── mock-dev commits in the bound worktree + reports to lead ──
    git(
        Path::new(&wt),
        &["commit", "--allow-empty", "-m", "e2e work"],
    );
    let report = mcp_tool(
        &h,
        "mock-dev",
        "send",
        json!({
            "instance": "lead",
            "message": "done — PR ready for review",
            "request_kind": "report",
            "correlation_id": task_id,
        }),
    );
    assert_eq!(
        report["ok"].as_bool(),
        Some(true),
        "report send failed: {report}"
    );

    // ── SEAM ③ report-route: lead received mock-dev's report ──
    let inbox = mcp_tool(&h, "lead", "inbox", json!({}));
    let inbox_str = inbox.to_string();
    assert!(
        inbox_str.contains("PR ready for review") && inbox_str.contains("mock-dev"),
        "report-route: lead's inbox must contain mock-dev's report: {inbox}"
    );

    drop(h);
}
