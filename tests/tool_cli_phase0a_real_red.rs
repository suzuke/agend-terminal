//! Real-daemon RED coverage for #3411 / #3405 Phase 0a.
//!
//! These tests close the gap where a fake daemon could make a missing fence
//! implementation appear green. They boot the actual daemon under an isolated
//! temporary AGEND_HOME and drive the authenticated API socket directly.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

mod common;

use assert_cmd::Command;
use common::harness::AgendHarness;
use serde_json::{json, Value};
use serial_test::serial;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn hermetic_home(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home = std::env::temp_dir().join(format!(
        "agend-tool-cli-real-{tag}-{}-{stamp}",
        std::process::id()
    ));
    assert!(home.starts_with(std::env::temp_dir()));
    home
}

fn minimal_fleet() -> &'static str {
    "instances:\n  probe:\n    command: /bin/cat\n"
}

fn run_dir_of(harness: &AgendHarness) -> PathBuf {
    std::fs::read_dir(harness.home.join("run"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("api.cookie").is_file())
        .expect("real daemon run dir with api.cookie")
}

fn hex_file(path: &Path) -> String {
    std::fs::read(path)
        .unwrap()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn socket_call(harness: &AgendHarness, auth_hex: &str, request: &Value) -> Value {
    let stream = TcpStream::connect(format!("127.0.0.1:{}", harness.api_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);

    writeln!(writer, r#"{{"auth":"{auth_hex}"}}"#).unwrap();
    writer.flush().unwrap();
    let mut auth_response = String::new();
    reader.read_line(&mut auth_response).unwrap();
    let auth_response: Value = serde_json::from_str(auth_response.trim()).unwrap();
    assert_eq!(
        auth_response["ok"], true,
        "real daemon auth failed: {auth_response}"
    );

    writeln!(writer, "{request}").unwrap();
    writer.flush().unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(response.trim()).unwrap()
}

fn mcp_tool(
    harness: &AgendHarness,
    auth_hex: &str,
    instance: &str,
    tool: &str,
    arguments: Value,
    transport: Option<&str>,
) -> Value {
    let mut params = json!({
        "tool": tool,
        "arguments": arguments,
        "instance": instance
    });
    if let Some(transport) = transport {
        params["transport"] = json!(transport);
    }
    socket_call(
        harness,
        auth_hex,
        &json!({
            "method": "mcp_tool",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "params": params
        }),
    )
}

fn run_tool_cli(home: &Path, args: &[String]) -> std::process::Output {
    Command::cargo_bin("agend-terminal")
        .unwrap()
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", "probe")
        .args(args)
        .output()
        .unwrap()
}

fn home_contains(home: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(home) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            home_contains(&path, needle)
        } else {
            std::fs::read_to_string(path)
                .map(|contents| contents.contains(needle))
                .unwrap_or(false)
        }
    })
}

#[test]
#[serial]
fn real_daemon_default_off_fence_refuses_cli_without_running_handler() {
    let home = hermetic_home("fence");
    let harness = AgendHarness::spawn_with(home.clone(), minimal_fleet(), "start").unwrap();
    assert!(
        !home.join("runtime-config.json").exists(),
        "RED must use config absence, not an explicit disabled file"
    );
    let cookie = hex_file(&run_dir_of(&harness).join("api.cookie"));
    let sentinel = format!("phase0a-cli-fence-sentinel-{}", std::process::id());

    let response = mcp_tool(
        &harness,
        &cookie,
        "probe",
        "task",
        json!({"action": "create", "title": sentinel, "review_class": "single"}),
        Some("cli"),
    );

    assert_eq!(
        response["ok"], false,
        "default-off CLI transport must be refused by the real daemon: {response}"
    );
    let error = response["error"].as_str().unwrap_or("");
    assert!(
        error.contains("disabled") || error.contains("fence") || error.contains("experimental"),
        "fence refusal must identify the disabled CLI gate: {response}"
    );
    assert!(
        !home_contains(&home, &sentinel),
        "fence refusal must happen before task handler side effects: {response}"
    );

    drop(harness);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[serial]
fn admin_config_set_supports_dotted_experimental_tool_cli_key() {
    let home = hermetic_home("config-set");
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::cargo_bin("agend-terminal")
        .unwrap()
        .env("AGEND_HOME", &home)
        .args([
            "admin",
            "config-set",
            "experimental.tool_cli_enabled",
            "true",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "dotted admin config-set must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("runtime-config.json")).unwrap())
            .unwrap();
    assert_eq!(config["experimental"]["tool_cli_enabled"], true);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[serial]
fn config_get_and_list_expose_dotted_experimental_tool_cli_key() {
    let home = hermetic_home("config-read");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("runtime-config.json"),
        r#"{"schema_version":1,"experimental":{"tool_cli_enabled":true}}"#,
    )
    .unwrap();

    let (get, list) = {
        let harness = AgendHarness::spawn_with(home.clone(), minimal_fleet(), "start").unwrap();
        let operator = hex_file(&run_dir_of(&harness).join("api.operator"));
        let get = mcp_tool(
            &harness,
            &operator,
            "probe",
            "config",
            json!({"action": "get", "key": "experimental.tool_cli_enabled"}),
            None,
        );
        let list = mcp_tool(
            &harness,
            &operator,
            "probe",
            "config",
            json!({"action": "list"}),
            None,
        );
        (get, list)
    };

    assert_eq!(
        get["ok"], true,
        "config get must accept the dotted experimental key: {get}"
    );
    assert_eq!(get["result"]["key"], "experimental.tool_cli_enabled");
    assert_eq!(get["result"]["value"], true);
    assert_eq!(list["ok"], true, "config list must succeed: {list}");
    assert_eq!(
        list["result"]["config"]["experimental"]["tool_cli_enabled"],
        true
    );

    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[serial]
fn non_operator_config_set_remains_refused_for_experimental_key() {
    let home = hermetic_home("config-agent");
    let harness = AgendHarness::spawn_with(home.clone(), minimal_fleet(), "start").unwrap();
    let mode = Command::cargo_bin("agend-terminal")
        .unwrap()
        .env("AGEND_HOME", &home)
        .args(["mode", "away"])
        .output()
        .unwrap();
    assert!(
        mode.status.success(),
        "test must set operator mode away: {}",
        String::from_utf8_lossy(&mode.stderr)
    );
    let cookie = hex_file(&run_dir_of(&harness).join("api.cookie"));
    let response = mcp_tool(
        &harness,
        &cookie,
        "probe",
        "config",
        json!({
            "action": "set",
            "key": "experimental.tool_cli_enabled",
            "value": "true"
        }),
        None,
    );

    assert_eq!(
        response["ok"], false,
        "non-operator config set must remain refused: {response}"
    );
    assert!(
        response["error"].as_str().unwrap_or("").contains("blocked")
            || response["error"]
                .as_str()
                .unwrap_or("")
                .contains("operator")
            || response["error"]
                .as_str()
                .unwrap_or("")
                .contains("not available"),
        "refusal must be actionable: {response}"
    );

    drop(harness);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
#[serial]
fn real_daemon_oversized_same_request_id_replay_is_indeterminate() {
    let home = hermetic_home("oversized-replay");
    std::fs::create_dir_all(&home).unwrap();
    let enabled = Command::cargo_bin("agend-terminal")
        .unwrap()
        .env("AGEND_HOME", &home)
        .args([
            "admin",
            "config-set",
            "experimental.tool_cli_enabled",
            "true",
        ])
        .output()
        .unwrap();
    assert!(
        enabled.status.success(),
        "test must enable the experimental CLI fence: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );

    let harness = AgendHarness::spawn_with(home.clone(), minimal_fleet(), "start").unwrap();
    let operator = hex_file(&run_dir_of(&harness).join("api.operator"));
    let created = mcp_tool(
        &harness,
        &operator,
        "probe",
        "task",
        json!({
            "action": "create",
            "title": "oversized replay target",
            "description": "x".repeat(70 * 1024),
        }),
        None,
    );
    assert_eq!(
        created["ok"], true,
        "large task creation must succeed: {created}"
    );
    let task_id = created["result"]["id"]
        .as_str()
        .expect("large task creation must return an id")
        .to_string();
    let request_id = uuid::Uuid::new_v4().to_string();
    let get_json = serde_json::to_string(&json!({
        "action": "get",
        "id": task_id,
    }))
    .unwrap();
    let args = vec![
        "tool".to_string(),
        "task".to_string(),
        "--json".to_string(),
        get_json,
        "--request-id".to_string(),
        request_id,
        "--home".to_string(),
        home.to_string_lossy().into_owned(),
    ];

    let first = run_tool_cli(&home, &args);
    assert_eq!(
        first.status.code(),
        Some(0),
        "first large task get must execute successfully: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        first.stdout.len() > 64 * 1024,
        "first task get must exceed the dedup cache cap: {} bytes",
        first.stdout.len()
    );

    let replay = run_tool_cli(&home, &args);
    assert_eq!(
        replay.status.code(),
        Some(4),
        "oversized same-request-id replay must be indeterminate, not a refused exit 2: stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(
        String::from_utf8_lossy(&replay.stdout).contains("\"status\": \"oversized\""),
        "oversized replay must identify its indeterminate status: stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );

    drop(harness);
    let _ = std::fs::remove_dir_all(home);
}
