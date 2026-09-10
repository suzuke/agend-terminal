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
        server.thread = Some(std::thread::spawn(move || serve(
            &home, Arc::new(Mutex::new(HashMap::new())), shutdown,
            Arc::new(Mutex::new(HashMap::new())), Arc::new(Mutex::new(HashMap::new())),
            None, RestartCapability::Unsupported, None,
        )));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(port) = std::fs::read_to_string(run.join("api.port")) {
                if let Ok(port) = port.trim().parse::<u16>() { server.port = port; break; }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(server.port, 0, "API did not start");
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
        "task_id":"nonexistent-3553", "target":"cancelled", "result":"operator inspected",
        "instance":"operator"
    }});
    let denied = server.request(false, request.clone());
    assert_eq!(denied["denied_by"], "capability");
    let response = server.request(true, request);
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "task_not_found", "operator must reach strict task lookup: {response}");
}
