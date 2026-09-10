//! Real socket entry contract, using an isolated home and no agent processes.
use super::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

struct Server {
    home: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(
            &([127, 0, 0, 1], self.port).into(), Duration::from_millis(100),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.thread.as_ref().is_some_and(|t| !t.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            let _ = self.thread.take().unwrap().join();
            let _ = std::fs::remove_dir_all(&self.home);
        } else {
            // Preserve the isolated home if termination cannot be proven.
            eprintln!("settlement test server did not stop: {}", self.home.display());
            if !std::thread::panicking() {
                panic!("settlement test server cleanup was not proven");
            }
        }
    }
}

impl Server {
    fn start() -> Self {
        let home = super::tests::tmp_home("operator-settlement-3553");
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).unwrap();
        crate::auth_cookie::issue(&run).unwrap();
        let mut server = Self { home, shutdown: Arc::new(AtomicBool::new(false)), thread: None, port: 0 };
        let home = server.home.clone();
        let shutdown = server.shutdown.clone();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        server.port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let operator = crate::auth_cookie::read_operator_token(&run).unwrap();
        let agent = crate::auth_cookie::read_cookie(&run).unwrap();
        // Own the accept loop but use the production authentication/dispatch
        // path unchanged. No daemon agents or detached handler threads.
        server.thread = Some(std::thread::spawn(move || {
            let registry = Arc::new(Mutex::new(HashMap::new()));
            let configs = Arc::new(Mutex::new(HashMap::new()));
            let externals = Arc::new(Mutex::new(HashMap::new()));
            while !shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        handle_session(
                            stream, &registry, &home, &shutdown, &configs, &externals,
                            None, operator, agent, RestartCapability::Unsupported, None,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("test accept: {error}"),
                }
            }
        }));
        server
    }

    fn request(&self, operator: bool, request: Value) -> Value {
        let run = crate::daemon::run_dir(&self.home);
        let token = if operator { crate::auth_cookie::read_operator_token(&run) }
            else { crate::auth_cookie::read_cookie(&run) }.unwrap();
        let socket = TcpStream::connect_timeout(&([127,0,0,1], self.port).into(), Duration::from_secs(2)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut writer = socket.try_clone().unwrap();
        let mut reader = BufReader::new(socket);
        crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &token).unwrap();
        writeln!(writer, "{request}").unwrap();
        writer.flush().unwrap();
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();
        serde_json::from_str(&response).unwrap()
    }
}

#[test]
#[serial_test::serial]
fn operator_settlement_3553_real_socket_reaches_preview_not_agent() {
    let server = Server::start();
    let request = json!({"method":"task_settlement_preview", "params":{
        "task_id":"nonexistent-3553", "target":"cancelled", "result":"operator inspected"
    }});
    let mut forged = request.clone();
    forged["params"]["instance"] = json!("operator");
    let denied = server.request(false, forged);
    assert_eq!(denied["denied_by"], "capability");
    let response = server.request(true, request);
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "task_not_found", "operator must reach strict task lookup: {response}");
}

#[test]
#[serial_test::serial]
fn operator_settlement_3553_preview_is_nonmutating_and_exact() {
    let server = Server::start();
    let created = serde_json::from_value(json!({
        "kind":"Created", "task_id":"preview-row", "title":"Exact row",
        "description":"Keep this work", "priority":"normal", "owner":null
    })).unwrap();
    crate::task_events::append(&server.home, &"fixture".into(), created).unwrap();
    let before = serde_json::to_value(crate::task_events::replay(&server.home).unwrap()).unwrap();
    let response = server.request(true, json!({"method":"task_settlement_preview", "params":{
        "task_id":"preview-row", "target":"cancelled", "result":"duplicate confirmed by operator"
    }}));
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["task_id"], "preview-row");
    assert_eq!(response["result"]["target"], "cancelled");
    assert!(response["result"]["confirmation"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(before, serde_json::to_value(crate::task_events::replay(&server.home).unwrap()).unwrap());
}

#[test]
#[serial_test::serial]
fn operator_settlement_3553_apply_once_and_reject_changed_subject() {
    use crate::task_events::{append, replay, TaskEvent};
    let server = Server::start();
    for id in ["settle-row", "stale-row", "child-row"] {
        let created = serde_json::from_value(json!({
            "kind":"Created", "task_id":id, "title":"Exact row",
            "description":"original", "priority":"normal", "owner":null,
            "parent_id": if id == "child-row" {Some("settle-row")} else {None}
        })).unwrap();
        append(&server.home, &"fixture".into(), created).unwrap();
    }
    for id in ["settle-row", "stale-row"] {
        let preview = server.request(true, json!({"method":"task_settlement_preview", "params":{
            "task_id":id, "target":"cancelled", "result":"operator confirmed"
        }}));
        assert_eq!(preview["ok"], true, "{preview}");
        if id == "stale-row" {
            append(&server.home, &"fixture".into(), TaskEvent::DescriptionUpdated {
                task_id:id.into(), description:"changed after preview".into(), by:"fixture".into()
            }).unwrap();
        }
        let request = json!({"method":"task_settlement_apply", "params":{
            "confirmation":preview["result"]["confirmation"]
        }});
        let response = server.request(true, request.clone());
        if id == "stale-row" {
            assert_eq!(response["ok"], false);
            assert_eq!(response["code"], "stale_preview", "{response}");
        } else {
            assert_eq!(response["ok"], true, "{response}");
            let before_retry = serde_json::to_value(replay(&server.home).unwrap()).unwrap();
            let repeated = server.request(true, request);
            assert_eq!(repeated["ok"], true, "{repeated}");
            assert_eq!(repeated["result"]["already_applied"], true);
            assert_eq!(before_retry, serde_json::to_value(replay(&server.home).unwrap()).unwrap());
        }
    }
    let state = replay(&server.home).unwrap();
    assert_eq!(state.tasks[&"settle-row".into()].status.to_string(), "cancelled");
    for id in ["stale-row", "child-row"] {
        assert_eq!(state.tasks[&id.into()].status.to_string(), "open");
    }
}

#[test]
#[serial_test::serial]
fn operator_settlement_3553_expired_confirmation_cannot_mutate() {
    let server = Server::start();
    let created = serde_json::from_value(json!({
        "kind":"Created", "task_id":"expired-row", "title":"Keep open",
        "description":"original", "priority":"normal", "owner":null
    })).unwrap();
    crate::task_events::append(&server.home, &"fixture".into(), created).unwrap();
    let preview = server.request(true, json!({"method":"task_settlement_preview", "params":{
        "task_id":"expired-row", "target":"done", "result":"operator inspected"
    }}));
    assert_eq!(preview["ok"], true, "{preview}");
    let token = preview["result"]["confirmation"].as_str().unwrap();
    let path = server.home.join("operator-task-confirmations").join(format!("{token}.json"));
    let mut confirmation: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    confirmation["created_at"] = json!("2000-01-01T00:00:00Z");
    crate::store::save_atomic(&path, &confirmation).unwrap();
    let before = serde_json::to_value(crate::task_events::replay(&server.home).unwrap()).unwrap();
    let response = server.request(true, json!({"method":"task_settlement_apply", "params":{
        "confirmation":token
    }}));
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["code"], "confirmation_expired", "{response}");
    assert_eq!(before, serde_json::to_value(crate::task_events::replay(&server.home).unwrap()).unwrap());
}

#[test]
#[serial_test::serial]
fn operator_settlement_3553_retry_after_reopen_reports_current_state() {
    use crate::task_events::{append, replay, TaskEvent};
    let server = Server::start();
    let created = serde_json::from_value(json!({
        "kind":"Created", "task_id":"reopened-row", "title":"New work later",
        "description":"original", "priority":"normal", "owner":null
    })).unwrap();
    append(&server.home, &"fixture".into(), created).unwrap();
    let preview = server.request(true, json!({"method":"task_settlement_preview", "params":{
        "task_id":"reopened-row", "target":"done", "result":"original outcome"
    }}));
    assert_eq!(preview["ok"], true, "{preview}");
    let request = json!({"method":"task_settlement_apply", "params":{
        "confirmation":preview["result"]["confirmation"]
    }});
    assert_eq!(server.request(true, request.clone())["ok"], true);
    append(&server.home, &"fixture".into(), TaskEvent::Reopened {
        task_id:"reopened-row".into(), reason:"new work".into(), source_evidence:String::new()
    }).unwrap();
    crate::task_events::compact_with_keep_for_test(&server.home, 1).unwrap();
    let before = serde_json::to_value(replay(&server.home).unwrap()).unwrap();
    let response = server.request(true, request);
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["result"]["already_applied"], true, "{response}");
    assert_eq!(before, serde_json::to_value(replay(&server.home).unwrap()).unwrap());
    assert_eq!(response["result"]["original_target"], "done", "{response}");
    assert_eq!(response["result"]["original_result"], "original outcome", "{response}");
    assert_eq!(response["result"]["current_status"], "open", "{response}");
}
