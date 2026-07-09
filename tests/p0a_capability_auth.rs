//! #2342 P0a — control-socket per-method capability authorization (dev2 A1).
//!
//! The loopback control socket authenticates a SINGLE shared `api.cookie` that
//! BOTH the operator CLI and the agent MCP bridge present identically; today the
//! operator-vs-agent distinction is made purely by method-shape
//! (`operator_gate::check_operation_allowed` short-circuits every non-`mcp_tool`
//! method to full authority). So ANY holder of the shared cookie — the agent
//! bridge, or any same-user process that can read the cookie file — can drive
//! every injection-equivalent DIRECT method (`inject`/`send`/`spawn`/`kill`/…)
//! by simply sending its method string. That is the method-shape subcase of dev2 A1.
//!
//! P0a closes the **method-shape / sidecar-agent-cookie** subcase: authority is
//! proven by the AUTHENTICATED PRINCIPAL (which secret was presented at
//! handshake), not by method-shape. A boot-minted operator full-capability token
//! (`api.operator`) → allow-all; the shared agent cookie → the MCP tunnel ONLY,
//! every direct method default-DENIED.
//!
//! NOT closed here — the **same-user-agent** subcase: `api.operator` is 0600 in
//! run_dir, which isolates cross-USER only, so a same-uid agent (a future #2342
//! responder, if prompt-injected) can read it and impersonate the operator. That
//! is a HARD Phase-2 prerequisite tracked by task t-20260709010037959088-61315-1
//! and pinned by the `responder_inbound_requires_same_uid_isolation` guard in
//! `src/auth_cookie.rs` (see `auth_cookie::SAME_UID_OPERATOR_ISOLATION`).
//!
//! Unix-only: mirrors the other socket-level harness tests.
#![cfg(unix)]

mod common;

use common::harness::AgendHarness;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Minimal fleet: one idle instance so the daemon's API socket comes up.
fn minimal_fleet() -> String {
    "instances:\n  probe:\n    command: /bin/cat\n".to_string()
}

/// A hermetic /tmp home (NEVER ~/.agend-terminal).
fn hermetic_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-p0a-{}-{}-{}",
        tag,
        std::process::id(),
        // Monotonic-ish disambiguator without wall-clock (avoid Date::now bans in
        // scripts; here in a test std::time is fine but keep it simple).
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    assert!(
        home.starts_with(std::env::temp_dir()),
        "hermetic: home must be under tmp"
    );
    home
}

/// The daemon's run dir (the harness owns the only one under <home>/run/<pid>/).
fn run_dir_of(h: &AgendHarness) -> std::path::PathBuf {
    std::fs::read_dir(h.home.join("run"))
        .expect("run dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join("api.cookie").exists())
        .expect("run dir with api.cookie")
}

fn hex_of_file(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read secret file");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Outcome of one control-socket round-trip.
enum Outcome {
    /// The `{"auth":…}` handshake was rejected (connection refused pre-method).
    AuthRejected(Value),
    /// Handshake accepted; here is the method response.
    Response(Value),
}

/// Connect, optionally send the `{"auth":"<hex>"}` handshake line, then the
/// request. `auth_hex = None` sends NO handshake line (the request is the first
/// line the server reads — i.e. a no-token connection).
fn socket_call(port: u16, auth_hex: Option<&str>, request: &Value) -> Outcome {
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect api");
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut writer = stream.try_clone().expect("clone stream");
    let mut reader = BufReader::new(stream);

    if let Some(hex) = auth_hex {
        writeln!(writer, r#"{{"auth":"{hex}"}}"#).expect("write auth");
        writer.flush().ok();
        let mut auth_resp = String::new();
        reader.read_line(&mut auth_resp).expect("read auth resp");
        let parsed: Value = serde_json::from_str(auth_resp.trim())
            .unwrap_or_else(|e| panic!("parse auth resp '{auth_resp}': {e}"));
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Outcome::AuthRejected(parsed);
        }
    }

    writeln!(
        writer,
        "{}",
        serde_json::to_string(request).expect("serialize")
    )
    .expect("write request");
    writer.flush().ok();
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read response");
    let parsed: Value =
        serde_json::from_str(resp.trim()).unwrap_or_else(|e| panic!("parse resp '{resp}': {e}"));
    // A no-token connection has its (unauth) first line parsed AS the handshake:
    // the server replies `{"ok":false,"error":"auth"}` and closes. Classify it.
    if auth_hex.is_none() && parsed.get("error").and_then(Value::as_str) == Some("auth") {
        return Outcome::AuthRejected(parsed);
    }
    Outcome::Response(parsed)
}

/// RED-first (dev2 A1): a holder of the shared AGENT cookie must NOT be able to
/// reach the injection-equivalent DIRECT methods. On the OLD (pre-P0a) code the
/// gate short-circuits every non-`mcp_tool` method to full authority, so the
/// cookie holder's `inject`/`send`/`spawn` reach the handler (NOT capability-
/// denied) → this test FAILS. On P0a code the cookie authenticates as the Agent
/// principal whose capability is the MCP tunnel ONLY → each direct method is
/// hard-denied with `denied_by == "capability"`.
#[test]
fn agent_cookie_cannot_reach_direct_methods() {
    let home = hermetic_home("red-direct");
    let h = AgendHarness::spawn_with(home.clone(), &minimal_fleet(), "start").expect("daemon boot");
    let cookie_hex = hex_of_file(&run_dir_of(&h).join("api.cookie"));

    // Every injection-equivalent direct method the manifest names.
    for (method, params) in [
        ("inject", json!({"instance": "probe", "text": "x"})),
        ("send", json!({"instance": "probe", "message": "x"})),
        ("spawn", json!({"name": "evil", "command": "/bin/cat"})),
    ] {
        let req = json!({"method": method, "params": params});
        let out = socket_call(h.api_port, Some(&cookie_hex), &req);
        let resp = match out {
            Outcome::Response(v) => v,
            Outcome::AuthRejected(v) => {
                panic!("cookie handshake unexpectedly rejected for {method}: {v}")
            }
        };
        assert_eq!(
            resp.get("denied_by").and_then(Value::as_str),
            Some("capability"),
            "dev2 A1: agent-cookie holder reached direct method '{method}' \
             (expected capability-deny). response={resp}"
        );
    }

    std::fs::remove_dir_all(&home).ok();
}

/// Compat (the #1 load-bearing risk = operator lockout): the operator
/// full-capability token MUST reach the direct methods its own CLI/TUI drive.
/// This is the end-to-end "operator transport is NOT locked" proof.
#[test]
fn operator_token_reaches_direct_methods() {
    let home = hermetic_home("op-allow");
    let h = AgendHarness::spawn_with(home.clone(), &minimal_fleet(), "start").expect("daemon boot");
    let op_hex = hex_of_file(&run_dir_of(&h).join("api.operator"));

    // A read (`list`) and a structural op (`spawn`) — both must reach the handler
    // (never capability-denied) for the operator principal.
    for (method, params) in [
        ("list", json!({})),
        (
            "spawn",
            json!({"name": "p0a-op-spawned", "command": "/bin/cat"}),
        ),
    ] {
        let req = json!({"method": method, "params": params});
        let resp = match socket_call(h.api_port, Some(&op_hex), &req) {
            Outcome::Response(v) => v,
            Outcome::AuthRejected(v) => {
                panic!("operator token handshake unexpectedly rejected for {method}: {v}")
            }
        };
        assert_ne!(
            resp.get("denied_by").and_then(Value::as_str),
            Some("capability"),
            "operator lockout regression: operator token was capability-denied for \
             '{method}'. response={resp}"
        );
    }

    // `list` specifically returns a successful handler result for the operator.
    let list = match socket_call(
        h.api_port,
        Some(&op_hex),
        &json!({"method": "list", "params": {}}),
    ) {
        Outcome::Response(v) => v,
        Outcome::AuthRejected(v) => panic!("operator list rejected: {v}"),
    };
    assert_eq!(
        list.get("ok").and_then(Value::as_bool),
        Some(true),
        "operator `list` should succeed. response={list}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Compat: the shared agent cookie must STILL reach the MCP tunnel — narrowing
/// the sidecar/agent surface must not break the agent bridge's only real path.
#[test]
fn agent_cookie_still_reaches_mcp_tunnel() {
    let home = hermetic_home("agent-mcp");
    let h = AgendHarness::spawn_with(home.clone(), &minimal_fleet(), "start").expect("daemon boot");
    let cookie_hex = hex_of_file(&run_dir_of(&h).join("api.cookie"));

    let req = json!({
        "method": "mcp_tool",
        "params": {"tool": "list_instances", "arguments": {}, "instance": "probe"}
    });
    let resp = match socket_call(h.api_port, Some(&cookie_hex), &req) {
        Outcome::Response(v) => v,
        Outcome::AuthRejected(v) => panic!("agent cookie handshake rejected: {v}"),
    };
    assert_ne!(
        resp.get("denied_by").and_then(Value::as_str),
        Some("capability"),
        "compat regression: agent cookie was capability-denied for the mcp tunnel. \
         response={resp}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Regression guard: a connection presenting NEITHER secret is refused at the
/// handshake (fail-closed), before any method is dispatched.
#[test]
fn unauthenticated_and_wrong_token_are_rejected() {
    let home = hermetic_home("no-auth");
    let h = AgendHarness::spawn_with(home.clone(), &minimal_fleet(), "start").expect("daemon boot");

    let inject = json!({"method": "inject", "params": {"instance": "probe", "text": "x"}});

    // No handshake line at all.
    match socket_call(h.api_port, None, &inject) {
        Outcome::AuthRejected(_) => {}
        Outcome::Response(v) => panic!("no-token connection was NOT rejected: {v}"),
    }

    // A well-formed hex that matches neither the operator token nor the cookie.
    let stranger_hex = "aa".repeat(32);
    match socket_call(h.api_port, Some(&stranger_hex), &inject) {
        Outcome::AuthRejected(_) => {}
        Outcome::Response(v) => panic!("wrong-token connection was NOT rejected: {v}"),
    }

    std::fs::remove_dir_all(&home).ok();
}
