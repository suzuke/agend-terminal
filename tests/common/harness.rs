//! AgendHarness — spawn a real `agend-terminal` daemon for integration tests.
//!
//! Sprint 42 Phase 2: foundational test harness for TUI/daemon integration.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A running daemon instance for integration testing.
pub struct AgendHarness {
    pub home: PathBuf,
    pub api_port: u16,
    child: Child,
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job_handle: *mut std::ffi::c_void,
}

impl AgendHarness {
    /// Spawn a real daemon with the given fleet.yaml content.
    /// Waits up to 15s for the API port file to appear.
    pub fn spawn(home: PathBuf, fleet_yaml: &str) -> Result<Self, String> {
        Self::spawn_with(home, fleet_yaml, "daemon")
    }
    pub fn spawn_with(home: PathBuf, fleet_yaml: &str, subcommand: &str) -> Result<Self, String> {
        std::fs::create_dir_all(&home).map_err(|e| format!("create home: {e}"))?;
        std::fs::write(home.join("fleet.yaml"), fleet_yaml)
            .map_err(|e| format!("write fleet.yaml: {e}"))?;

        let binary = binary_path();
        let mut cmd = Command::new(&binary);
        cmd.args([&subcommand])
            .env("AGEND_HOME", &home)
            .env("AGEND_TEST_ISOLATION", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // BLOCKING 3.1: Unix process-group via setsid pre_exec
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
        let pid = child.id();

        #[cfg(unix)]
        let pgid = pid as i32; // setsid makes pid == pgid

        // BLOCKING 3.2: poll api.port AND try_wait for early exit
        let start = Instant::now();
        let timeout = platform_timeout(Duration::from_secs(15));
        let run_dir = home.join("run").join(pid.to_string());

        let mut child = child;
        loop {
            if start.elapsed() > timeout {
                let _ = child.kill();
                return Err("daemon startup timeout (15s) — api.port never appeared".into());
            }

            // Check for early exit
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = child
                        .stderr
                        .take()
                        .map(|s| {
                            BufReader::new(s)
                                .lines()
                                .take(10)
                                .filter_map(|l| l.ok())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    return Err(format!(
                        "daemon exited early with {status}. stderr:\n{stderr}"
                    ));
                }
                Ok(None) => {} // still running
                Err(e) => return Err(format!("try_wait: {e}")),
            }

            // Check for api.port file
            let port_path = run_dir.join("api.port");
            if let Ok(contents) = std::fs::read_to_string(&port_path) {
                if let Ok(port) = contents.trim().parse::<u16>() {
                    // BLOCKING 3.1 Windows: create job object + assign process
                    #[cfg(windows)]
                    let job_handle = unsafe {
                        use std::os::windows::io::AsRawHandle;
                        use windows_sys::Win32::System::JobObjects::*;

                        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                        if job.is_null() {
                            return Err(format!(
                                "CreateJobObjectW failed: {}",
                                std::io::Error::last_os_error()
                            ));
                        }
                        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                        if SetInformationJobObject(
                            job,
                            JobObjectExtendedLimitInformation,
                            &info as *const _ as *const _,
                            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                        ) == 0
                        {
                            return Err(format!(
                                "SetInformationJobObject failed: {}",
                                std::io::Error::last_os_error()
                            ));
                        }
                        if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                            return Err(format!(
                                "AssignProcessToJobObject failed: {}",
                                std::io::Error::last_os_error()
                            ));
                        }
                        job
                    };

                    return Ok(Self {
                        home,
                        api_port: port,
                        child,
                        #[cfg(unix)]
                        pgid,
                        #[cfg(windows)]
                        job_handle,
                    });
                }
            }

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Send an API request and get the response.
    #[allow(dead_code)]
    pub fn api_call(&self, request: &serde_json::Value) -> Result<serde_json::Value, String> {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", self.api_port))
            .map_err(|e| format!("connect: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();

        // Read api.cookie
        let run_dir = self.home.join("run").join(self.child.id().to_string());
        let cookie = std::fs::read_to_string(run_dir.join("api.cookie"))
            .unwrap_or_default()
            .trim()
            .to_string();

        let mut req = request.clone();
        if let Some(obj) = req.as_object_mut() {
            obj.insert("cookie".into(), serde_json::json!(cookie));
        }

        let line = serde_json::to_string(&req).map_err(|e| format!("serialize: {e}"))?;
        writeln!(stream, "{line}").map_err(|e| format!("write: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .map_err(|e| format!("read: {e}"))?;

        serde_json::from_str(response.trim()).map_err(|e| format!("parse: {e}"))
    }
}

impl Drop for AgendHarness {
    fn drop(&mut self) {
        // BLOCKING 3.1: kill process group on Unix
        #[cfg(unix)]
        {
            // SIGTERM the process group
            unsafe {
                libc::kill(-self.pgid, libc::SIGTERM);
            }
            // Wait up to 3s for graceful shutdown
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(3) {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // SIGKILL if still alive
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }

        #[cfg(not(unix))]
        {
            // BLOCKING 3.1 Windows: close job handle → kills entire job
            // (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE set at creation)
            #[cfg(windows)]
            unsafe {
                if !self.job_handle.is_null() {
                    windows_sys::Win32::Foundation::CloseHandle(self.job_handle as _);
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// TuiClient — connect to daemon API and parse responses.
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config};
use alacritty_terminal::vte::ansi::Processor;

/// Noop listener for test vterm (no PTY write-back needed).
struct NoopListener;
impl EventListener for NoopListener {
    fn send_event(&self, _event: Event) {}
}

/// Minimal vterm wrapper mirroring production `src/vterm.rs` API.
struct TestVTerm {
    term: term::Term<NoopListener>,
    processor: Processor,
    cols: u16,
    rows: u16,
}

impl TestVTerm {
    fn new(cols: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: 10000,
            ..Default::default()
        };
        struct Size {
            cols: u16,
            rows: u16,
        }
        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                self.rows as usize
            }
            fn screen_lines(&self) -> usize {
                self.rows as usize
            }
            fn columns(&self) -> usize {
                self.cols as usize
            }
        }
        let term = term::Term::new(config, &Size { cols, rows }, NoopListener);
        Self {
            term,
            processor: Processor::new(),
            cols,
            rows,
        }
    }

    fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    #[allow(dead_code)]
    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        struct Size {
            cols: u16,
            rows: u16,
        }
        impl Dimensions for Size {
            fn total_lines(&self) -> usize {
                self.rows as usize
            }
            fn screen_lines(&self) -> usize {
                self.rows as usize
            }
            fn columns(&self) -> usize {
                self.cols as usize
            }
        }
        self.term.resize(Size { cols, rows });
    }

    fn tail_lines(&self, n: usize) -> String {
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        let mut lines: Vec<String> = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let point = Point::new(Line(row as i32), Column(col));
                if point.line >= grid.topmost_line()
                    && point.line <= grid.bottommost_line()
                    && col < grid.columns()
                {
                    let cell = &grid[point];
                    if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        let ch = if cell.c == '\0' { ' ' } else { cell.c };
                        line.push(ch);
                    }
                }
            }
            lines.push(line.trim_end().to_string());
        }
        let first = lines
            .iter()
            .position(|l| !l.is_empty())
            .unwrap_or(lines.len());
        let last = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map(|i| i + 1)
            .unwrap_or(first);
        let visible = &lines[first..last];
        let tail = if visible.len() > n {
            &visible[visible.len() - n..]
        } else {
            visible
        };
        tail.join("\n")
    }
}

/// TuiClient — connect to daemon API + in-process vterm for PTY output parsing.
/// Mirrors production `src/vterm.rs` API via alacritty_terminal::Term.
#[allow(dead_code)]
pub struct TuiClient {
    port: u16,
    home: PathBuf,
    vterm: TestVTerm,
}

impl TuiClient {
    pub fn new(harness: &AgendHarness, cols: u16, rows: u16) -> Self {
        Self {
            port: harness.api_port,
            home: harness.home.clone(),
            vterm: TestVTerm::new(cols, rows),
        }
    }

    /// Call the daemon API with a method + params.
    #[allow(dead_code)]
    pub fn call(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let req = serde_json::json!({"method": method, "params": params});
        authed_api_call(self.port, &self.home, &req)
    }

    /// Resize the vterm (simulates terminal resize).
    #[allow(dead_code)]
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vterm.resize(cols, rows);
    }

    /// Feed raw bytes into the vterm (simulates PTY output).
    #[allow(dead_code)]
    pub fn feed(&mut self, data: &[u8]) {
        self.vterm.process(data);
    }

    /// Read the last N lines from the vterm as plain text (ANSI stripped).
    #[allow(dead_code)]
    pub fn screen_text(&self, lines: usize) -> String {
        self.vterm.tail_lines(lines)
    }

    /// Read scrollback + visible screen as plain text.
    #[allow(dead_code)]
    pub fn read_scrollback(&self, max_lines: usize) -> String {
        self.vterm.tail_lines(max_lines) // TestVTerm uses tail_lines for both
    }

    /// Feed bytes into vterm and return screen text. Single-pass — does not
    /// poll or wait. Use for deterministic test content where all bytes are
    /// available upfront.
    #[allow(dead_code)]
    pub fn feed_and_extract(&mut self, data: &[u8]) -> String {
        self.vterm.process(data);
        let rows = self.vterm.rows as usize;
        self.vterm.tail_lines(rows)
    }

    /// Feed bytes, then poll until predicate matches screen content or timeout.
    /// The predicate is checked against the vterm screen after the initial feed.
    /// Useful when the fed content triggers state changes that need time to settle.
    #[allow(dead_code)]
    pub fn wait_for<F>(&mut self, data: &[u8], predicate: F, timeout: Duration) -> bool
    where
        F: Fn(&str) -> bool,
    {
        // Windows CI runners are slower; apply timeout multiplier.
        let effective_timeout = platform_timeout(timeout);
        self.vterm.process(data);
        let start = Instant::now();
        loop {
            let rows = self.vterm.rows as usize;
            let screen = self.vterm.tail_lines(rows);
            if predicate(&screen) {
                return true;
            }
            if start.elapsed() > effective_timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Platform-aware timeout: Windows CI runners are ~3-5x slower than
/// macOS/Linux. Multiply timeouts to avoid flaky failures.
fn platform_timeout(timeout: Duration) -> Duration {
    if cfg!(windows) {
        timeout * 3
    } else {
        timeout
    }
}

#[allow(dead_code)]
fn authed_api_call(
    port: u16,
    home: &Path,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let stream =
        TcpStream::connect(format!("127.0.0.1:{port}")).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut writer = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    let mut reader = BufReader::new(stream);

    // Auth handshake
    let run_dir = find_run_dir(home).ok_or("no run dir".to_string())?;
    let cookie_bytes =
        std::fs::read(run_dir.join("api.cookie")).map_err(|e| format!("read cookie: {e}"))?;
    let cookie_hex: String = cookie_bytes.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(writer, r#"{{"auth":"{cookie_hex}"}}"#).map_err(|e| format!("write auth: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;
    let mut auth_resp = String::new();
    reader
        .read_line(&mut auth_resp)
        .map_err(|e| format!("read auth: {e}"))?;

    // Send request
    let line = serde_json::to_string(request).map_err(|e| format!("serialize: {e}"))?;
    writeln!(writer, "{line}").map_err(|e| format!("write: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_str(response.trim()).map_err(|e| format!("parse response: {e}"))
}

fn binary_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // strip test binary name
    path.pop(); // strip deps/
    path.push("agend-terminal");
    path
}

fn find_run_dir(home: &Path) -> Option<PathBuf> {
    let run = home.join("run");
    for entry in std::fs::read_dir(&run).ok()?.flatten() {
        let p = entry.path();
        if p.join("api.port").exists() {
            return Some(p);
        }
    }
    None
}
