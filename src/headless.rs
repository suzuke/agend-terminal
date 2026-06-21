//! #1967 Phase-1 PR2: headless (no-PTY) process transport for ephemeral workers.
//!
//! PR1 spawned a FAKE `/bin/sleep` child; PR2 swaps that for a REAL headless
//! process — the resolved backend command run via `std::process` with piped
//! stdio (NO PTY, NO agent registry). The captured stdio handles ride on
//! [`HeadlessHandle`] for PR3 (the ACP protocol) to drive; PR2 only does the
//! process LIFECYCLE (spawn + cancel), never protocol I/O.
//!
//! ## ⚠ PR3 PREREQUISITES — deliberately deferred here (lead-vetted Option Y)
//! [`resolve_headless_command`] reuses the SAME single-source helpers as the PTY
//! path (`which` / [`Backend::preset_spawn_args`] / [`Backend::spawn_flags`] /
//! [`crate::agent::resolve_child_env`]) so argv + the #1440 env-isolation can't
//! drift. But the PTY spawn (`agent::build_command`) ALSO does several
//! PTY/real-backend-specific steps that PR2's *stub* worker (no protocol → no
//! real backend doing work) does NOT need — and which are NOT done here. Before
//! PR3 lets a real backend run headless, these MUST be added (each is a
//! correctness or SECURITY prerequisite — do not ship PR3 without them):
//!
//! 1. **git-shim PATH shadowing + `AGEND_REAL_GIT` (#1504)** — without prepending
//!    `$AGEND_HOME/bin` to PATH, a headless backend's `git` calls bypass the
//!    `agend-git` shim → ESCAPE the git safety gate (push-denylist, worktree
//!    guards). SECURITY-critical.
//! 2. **cwd validation + provisioning** (`api::validate_working_directory` +
//!    `create_dir_all` + symlink revalidation) — PR2 sets no cwd (stub inherits
//!    the daemon cwd); a real backend needs a validated, provisioned workdir.
//! 3. **opencode per-instance XDG isolation (#1519) + autoupdate disable
//!    (#1956/#1970)** — session-isolation correctness + self-update-modal hang.
//! 4. **fleet.yaml user-env passthrough with #2106 credential filtering** —
//!    operator per-instance creds/env.
//! 5. terminal env (`TERM`/`COLORTERM`/`FORCE_COLOR`) + git-editor env
//!    (`GIT_EDITOR`/`GIT_SEQUENCE_EDITOR`/`EDITOR`/`VISUAL`) + `LANG` — only matter
//!    once a real backend runs (the git-editor pair pairs with #1).
//!
//! See also docs/design/1967-ephemeral-phase1.md (the same deferral list).

use crate::backend::{Backend, SpawnMode};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

/// A resolved headless command. Built ONLY from the same single-source helpers
/// the PTY path uses, so headless argv/env cannot drift from `build_command`.
#[derive(Debug, Clone)]
pub struct HeadlessCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Mirror the #1440 isolation: clear inherited env before injecting.
    pub env_clear: bool,
    pub envs: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

/// A spawned headless child + its captured stdio pipes. PR2 holds the pipes
/// unused; PR3 (ACP) drives stdin/stdout for the protocol — they are captured
/// NOW so the spawn wiring (piped stdio) is complete and PR3 only adds the
/// protocol driver.
#[derive(Debug)]
pub struct HeadlessHandle {
    pub child: Child,
    /// PR3 (ACP) writes protocol requests here. Captured but unused in PR2.
    #[allow(dead_code)]
    pub stdin: Option<ChildStdin>,
    /// PR3 (ACP) reads protocol responses here. Captured but unused in PR2.
    #[allow(dead_code)]
    pub stdout: Option<ChildStdout>,
    /// PR3 reads diagnostics here. Captured but unused in PR2.
    #[allow(dead_code)]
    pub stderr: Option<ChildStderr>,
}

impl HeadlessHandle {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

/// Process LIFECYCLE for a headless worker (spawn + cancel). The ACP protocol
/// methods (handshake / prompt / stream) are intentionally NOT on this trait
/// yet — they land in PR3 once the wire shape is verified against a real backend.
pub trait HeadlessTransport {
    /// Spawn `cmd` as a headless child with piped stdio.
    fn spawn(&self, cmd: &HeadlessCommand) -> std::io::Result<HeadlessHandle>;

    /// Cancel a running worker: single-process SIGTERM + reap. PR3 adds a
    /// protocol-level graceful cancel BEFORE this hard stop.
    ///
    /// ⚠ group-kill DEFERRED: this terminates only the worker process, not a
    /// process GROUP. PR2's worker is a single stub process with no children, so
    /// a single SIGTERM fully reaps it. When PR3 runs a real backend that may
    /// fork subprocesses, switch to `process_group(0)` at spawn +
    /// [`crate::process::kill_process_tree`] (group kill) here.
    fn cancel(&self, handle: &mut HeadlessHandle);
}

/// The PR2 transport: a real OS process via `std::process` with piped stdio.
pub struct StdioTransport;

impl HeadlessTransport for StdioTransport {
    fn spawn(&self, cmd: &HeadlessCommand) -> std::io::Result<HeadlessHandle> {
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args);
        if cmd.env_clear {
            c.env_clear();
        }
        for (k, v) in &cmd.envs {
            c.env(k, v);
        }
        if let Some(dir) = &cmd.cwd {
            c.current_dir(dir);
        }
        // Piped stdio: NO PTY. PR3 (ACP) drives these pipes for the protocol.
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(HeadlessHandle {
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    fn cancel(&self, handle: &mut HeadlessHandle) {
        crate::process::terminate(handle.pid());
        let _ = handle.child.wait();
    }
}

/// Resolve the headless command for `backend_command`, reusing the SAME
/// single-source helpers as the PTY path (`which` / `preset_spawn_args` /
/// `spawn_flags` / [`crate::agent::resolve_child_env`]) — so argv and the #1440
/// env-isolation cannot drift between the PTY and headless paths.
///
/// PR2 applies the CORE only: resolved program, enriched argv, #1440 env
/// isolation (must-carry: the security invariant), and the re-injected identity
/// env (`AGEND_INSTANCE_NAME` / `AGEND_HOME` — both isolation-dropped, mirroring
/// `build_command`). All other `build_command` setup is a PR3 prerequisite — see
/// the module doc.
pub fn resolve_headless_command(
    backend_command: &str,
    args: &[String],
    mode: SpawnMode,
    cwd: Option<&Path>,
    name: &str,
    home: Option<&Path>,
) -> HeadlessCommand {
    let backend = Backend::from_command(backend_command);

    // argv = preset (per mode) + caller args + backend spawn_flags — the SAME
    // helpers `build_command` uses (single source, no re-impl).
    let mut argv: Vec<String> = backend
        .as_ref()
        .map(|b| b.preset_spawn_args(mode))
        .unwrap_or_default();
    argv.extend(args.iter().cloned());
    if let (Some(b), Some(wd)) = (backend.as_ref(), cwd) {
        argv.extend(b.spawn_flags(wd));
    }

    let program = which::which(backend_command).unwrap_or_else(|_| PathBuf::from(backend_command));

    // #1440 env isolation — MUST apply to the headless child (must-carry #1: the
    // child-env-isolation security invariant). Same helper + same gating as
    // `build_command`: when isolation is on, clear + inject only the allowed env.
    let passthrough = home
        .and_then(|h| crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(h)).ok())
        .map(|f| f.resolve_passthrough_env(name))
        .unwrap_or_default();
    let source_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let plan = crate::agent::resolve_child_env(backend.as_ref(), &passthrough, &source_env);

    let (env_clear, mut envs) = if crate::agent::env_isolation_enabled() {
        (true, plan.injected)
    } else {
        // Isolation off (default) — inherit env, exactly like `build_command`'s
        // non-isolated branch. (The one-time operator "dropped keys" hint is the
        // PTY path's; not re-emitted here to avoid touching `agent` internals.)
        (false, Vec::new())
    };

    // Identity env — re-injected AFTER the clear because both are isolation-
    // SENSITIVE (dropped by `env_clear`), mirroring `build_command`. The worker
    // needs these to identify itself + resolve the daemon home.
    envs.push(("AGEND_INSTANCE_NAME".to_string(), name.to_string()));
    if let Some(h) = home {
        envs.push(("AGEND_HOME".to_string(), h.display().to_string()));
    }

    HeadlessCommand {
        program,
        args: argv,
        env_clear,
        envs,
        cwd: cwd.map(Path::to_path_buf),
    }
}
