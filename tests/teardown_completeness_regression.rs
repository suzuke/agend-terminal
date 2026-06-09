//! #1907 — teardown-completeness regression (the high-leverage payoff).
//!
//! Boots a REAL daemon on an isolated `/tmp` `AGEND_HOME` (never `~/.agend-terminal`),
//! gives a victim instance EVERY per-instance state it can hold (MCP-driven +
//! direct-seeded daemon-internal stores), runs the real `delete_instance`, then
//! asserts:
//!   (a) the delete SUCCEEDS — `full_delete_instance` returns `Ok` iff its
//!       `name_residual_anywhere` oracle is empty, so a successful delete IS the
//!       "oracle == clean" assertion, exercised through the production path; and
//!   (b) a WHOLE-HOME SCAN finds zero residual of the victim's name OR uuid,
//!       except an explicitly-annotated allowlist (intentional-retention stores +
//!       structural logs).
//!
//! (b) is the real future-proofing: the production oracle is a *curated* list
//! (only catches stores someone remembered to add), but the whole-home scan
//! catches ANY unexpected residual — including a future store nobody anticipated.
//! A new per-instance store that forgets teardown cleanup turns this test RED.
#![cfg(unix)]

mod common;

use common::git_isolated;
use common::harness::AgendHarness;
use serde_json::{json, Value};
use std::path::Path;

const VICTIM: &str = "victim-agent";
const VICTIM_UUID: &str = "11111111-2222-3333-4444-555555555555";
const BRANCH: &str = "feat/victim-b";
const REPO_SLUG: &str = "e2e-org/victim-repo";

/// Allowlist of residual the scan TOLERATES, each with its rationale. A path is
/// allowed if it starts with any of these (relative to home). Operator/reviewer
/// can reclassify any entry — that is the point of keeping it explicit here.
const ALLOWLIST: &[(&str, &str)] = &[
    // ── fixture, not daemon state ──
    ("source", "the test's source git repo (fixture, not per-instance daemon state)"),
    ("origin.git", "the test's bare origin (fixture)"),
    // ── structural: the victim's name legitimately appears in history/audit trail ──
    ("daemon.", "daemon.<date>.log — tracing legitimately names the deleted instance"),
    ("event-log.jsonl", "append-only audit trail — create/delete events name the instance by design"),
    ("task_events.jsonl", "append-only task-event log — owner/assignee history named by design"),
    // ── intentional retention (audit-confirmed; see #1907 spike) ──
    ("schedules.json", "INTENTIONAL: orphan-disabled, never deleted — operator may re-target a cron"),
    ("tasks.json", "INTENTIONAL: owner cleared but the task row is kept for surviving agents"),
    ("deployments.json", "INTENTIONAL: the instances list is a redeploy recipe"),
    ("topics.json", "INTENTIONAL: telegram-only; delete_topic unregisters on success, boot orphan-sweep self-heals the failure path"),
];

fn is_allowlisted(rel: &str) -> bool {
    ALLOWLIST.iter().any(|(prefix, _)| rel.starts_with(prefix))
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_isolated::git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn call_api(h: &AgendHarness, request: &Value) -> Value {
    use std::io::{BufRead, BufReader, Write};
    let stream =
        std::net::TcpStream::connect(format!("127.0.0.1:{}", h.api_port)).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .ok();
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let run_dir = std::fs::read_dir(h.home.join("run"))
        .expect("run")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("api.cookie").exists())
        .expect("cookie dir");
    let cookie = std::fs::read(run_dir.join("api.cookie")).expect("cookie");
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(writer, r#"{{"auth":"{hex}"}}"#).expect("auth");
    writer.flush().ok();
    let mut a = String::new();
    reader.read_line(&mut a).expect("auth resp");
    writeln!(writer, "{}", serde_json::to_string(request).expect("ser")).expect("write");
    writer.flush().ok();
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read");
    serde_json::from_str(resp.trim()).unwrap_or_else(|e| panic!("parse '{resp}': {e}"))
}

fn mcp(h: &AgendHarness, instance: &str, tool: &str, args: Value) -> Value {
    call_api(
        h,
        &json!({"method":"mcp_tool","params":{"tool":tool,"arguments":args,"instance":instance}}),
    )
}

/// Every path under `home` whose relative path OR text content contains `needle`.
fn scan_for(home: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let rel = p.strip_prefix(home).unwrap_or(&p).display().to_string();
            if p.is_dir() {
                stack.push(p.clone());
                if rel.contains(needle) {
                    hits.push(format!("{rel} (dir path)"));
                }
                continue;
            }
            if rel.contains(needle) {
                hits.push(format!("{rel} (path)"));
            } else if std::fs::read_to_string(&p)
                .map(|c| c.contains(needle))
                .unwrap_or(false)
            {
                hits.push(format!("{rel} (content)"));
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

fn unexpected(home: &Path, needle: &str) -> Vec<String> {
    scan_for(home, needle)
        .into_iter()
        .filter(|h| !is_allowlisted(h))
        .collect()
}

#[test]
fn teardown_leaves_zero_residual_after_full_exercise_1907() {
    let home = std::env::temp_dir().join(format!(
        "agend-teardown-regr-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("mkdir");
    assert!(
        home.starts_with(std::env::temp_dir()),
        "hermetic: home must be under tmp"
    );
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_scenario(&home)));
    std::fs::remove_dir_all(&home).ok();
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

fn run_scenario(home: &Path) {
    // ── hermetic source repo for the binding (local bare origin, no network) ──
    let origin = home.join("origin.git");
    std::fs::create_dir_all(&origin).expect("mkdir origin");
    git(&origin, &["init", "--bare", "-b", "main"]);
    let src = home.join("source");
    std::fs::create_dir_all(&src).expect("mkdir source");
    git(&src, &["init", "-b", "main"]);
    git(
        &src,
        &["remote", "add", "origin", &origin.display().to_string()],
    );
    std::fs::write(src.join("README.md"), "x\n").expect("write README");
    git(&src, &["add", "."]);
    git(&src, &["commit", "-m", "init"]);
    git(&src, &["push", "-u", "origin", "main"]);

    // victim carries an explicit id (uuid) so the uuid-keyed paths exist + are scanned
    let fleet = format!(
        "instances:\n  lead:\n    command: /bin/cat\n  \
         {VICTIM}:\n    command: /bin/cat\n    id: {VICTIM_UUID}\n    source_repo: {src}\n\
         teams:\n  t:\n    members: [lead, {VICTIM}]\n    orchestrator: lead\n",
        src = src.display()
    );
    let h = AgendHarness::spawn_with(home.to_path_buf(), &fleet, "start").expect("boot");

    // ── exercise EVERY hermetic-reachable per-instance state ──
    // inbox (name + uuid path)
    let _ = mcp(
        &h,
        "lead",
        "send",
        json!({"instance": VICTIM, "message": "hi"}),
    );
    // a task OWNED by victim (task-owner orphan path)
    let _ = mcp(
        &h,
        VICTIM,
        "task",
        json!({"action": "create", "title": "owned by the agent"}),
    );
    // dispatch with branch → binding + physical worktree + dispatch_tracking
    //                        + pending-dispatch sidecar + auto-armed ci-watch
    let created = mcp(
        &h,
        "lead",
        "task",
        json!({"action": "create", "title": "do work"}),
    );
    let tid = created["result"]["id"].as_str().unwrap_or("").to_string();
    let _ = mcp(
        &h,
        "lead",
        "send",
        json!({
            "instance": VICTIM, "message": "do it", "request_kind": "task",
            "task_id": tid, "branch": BRANCH, "repository": REPO_SLUG, "next_after_ci": "lead",
        }),
    );

    // ── direct-seed the daemon-internal-only stores (the daemon writes these on
    //    its own loops; seed with the victim's name so teardown is exercised) ──
    std::fs::write(
        home.join("usage_limit_notify.json"),
        json!({VICTIM: {"unlock_at": null, "notified_at": "2026-06-09T00:00:00+00:00"}})
            .to_string(),
    )
    .expect("seed write");
    std::fs::write(
        home.join("health_escalation.json"),
        json!({"schema_version": 1, "agents": {VICTIM: {}}}).to_string(),
    )
    .expect("seed write");
    std::fs::create_dir_all(home.join("agent-activity")).ok();
    std::fs::write(
        home.join("agent-activity").join(format!("{VICTIM}.json")),
        json!({"last_activity_ms": 1_700_000_000_000u64}).to_string(),
    )
    .expect("seed write");
    // pr-state subscriber (gh-poll-driven in prod, so direct-seed it here)
    std::fs::create_dir_all(home.join("pr-state")).ok();
    std::fs::write(
        home.join("pr-state")
            .join("e2e-org__victim-repo__feat-victim-b.json"),
        json!({"subscribers": [VICTIM], "repo": REPO_SLUG, "branch": BRANCH}).to_string(),
    )
    .expect("seed write");

    // sanity: the victim's state really is on disk before the delete
    assert!(
        home.join("runtime")
            .join(VICTIM)
            .join("binding.json")
            .exists(),
        "precondition: victim should be bound after dispatch"
    );
    assert!(
        home.join("workspace").join(VICTIM).exists(),
        "precondition: victim's default workspace dir should exist"
    );

    // ── the real teardown ──
    let del = mcp(&h, "lead", "delete_instance", json!({"instance": VICTIM}));

    // (a) the production residual oracle is clean: `full_delete_instance` returns
    // `Err` when `name_residual_anywhere` is non-empty, and `handle_delete_instance`
    // surfaces that as an `error` field on the result (alongside `name`). So "no
    // `error` field" IS the "oracle == []" assertion, exercised through prod.
    assert!(
        del["result"].get("error").is_none(),
        "delete_instance reported residual state (the oracle still saw a store): {del}"
    );
    assert_eq!(
        del["result"].get("name").and_then(|v| v.as_str()),
        Some(VICTIM),
        "delete_instance should echo the deleted name: {del}"
    );

    // (b) whole-home scan — zero unexpected residual by name OR uuid
    let by_name = unexpected(home, VICTIM);
    let by_uuid = unexpected(home, VICTIM_UUID);
    assert!(
        by_name.is_empty() && by_uuid.is_empty(),
        "teardown left unexpected residual (not in the annotated allowlist):\n  by name {VICTIM}: {by_name:#?}\n  by uuid {VICTIM_UUID}: {by_uuid:#?}\n\
         If a residual is INTENTIONAL, add it to ALLOWLIST with a rationale; otherwise add cleanup to full_delete_instance."
    );

    drop(h);
}
