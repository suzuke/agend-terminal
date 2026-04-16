//! `agend-terminal connect` — dynamically join an agent to a running daemon.
//!
//! Launches a backend CLI in the current terminal with MCP config pointing to
//! the daemon. The agent is registered as "external" (no PTY owned by daemon).

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Run the connect flow: register, configure, spawn backend, wait, deregister.
pub fn run(
    home: &Path,
    name: &str,
    backend_name: &str,
    working_dir: Option<&str>,
    extra_args: &[String],
) -> Result<()> {
    // 1. Validate daemon is running
    if crate::daemon::find_active_run_dir(home).is_none() {
        bail!("Daemon is not running.\n  Start it with:  agend-terminal start");
    }

    // 2. Validate name
    crate::agent::validate_name(name).map_err(|e| anyhow::anyhow!(e))?;

    // 3. Check name not already taken
    if let Ok(resp) = crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::LIST}),
    ) {
        if let Some(agents) = resp["result"]["agents"].as_array() {
            for a in agents {
                if a["name"].as_str() == Some(name) {
                    let kind = a["kind"].as_str().unwrap_or("managed");
                    bail!("Agent '{name}' already exists ({kind})");
                }
            }
        }
    }

    // 4. Resolve backend
    let backend = crate::backend::Backend::from_command(backend_name);
    let (command, default_args) = match &backend {
        Some(b) => {
            if !b.is_installed() {
                bail!(
                    "Backend '{}' ({}) not found in PATH",
                    backend_name,
                    b.preset().command
                );
            }
            let preset = b.preset();
            let args: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
            (preset.command.to_string(), args)
        }
        None => (backend_name.to_string(), Vec::new()),
    };

    // 5. Resolve working directory (expand ~ and ~/)
    let work_dir = match working_dir {
        Some(d) => {
            if d == "~" || d.starts_with("~/") {
                let home_dir = std::env::var("HOME")
                    .map(PathBuf::from)
                    .map_err(|_| anyhow::anyhow!("HOME not set, cannot expand ~"))?;
                if d == "~" {
                    home_dir
                } else {
                    home_dir.join(d.strip_prefix("~/").unwrap_or(d))
                }
            } else {
                PathBuf::from(d)
            }
        }
        None => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&work_dir)?;

    // 6. Generate MCP config
    crate::instructions::generate(&work_dir, &command);
    tracing::info!(dir = %work_dir.display(), "MCP config written");

    // 7. Register with daemon
    let pid = std::process::id();
    match crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::REGISTER_EXTERNAL,
            "params": { "name": name, "backend": backend_name, "pid": pid }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {}
        Ok(resp) => {
            bail!(
                "Registration failed: {}",
                resp["error"].as_str().unwrap_or("unknown error")
            );
        }
        Err(e) => bail!("Failed to connect to daemon: {e}"),
    }

    tracing::info!(%name, %pid, "registered with daemon");

    // 8. Build command args
    let mut args = default_args;
    // Add MCP config flag for Claude Code
    if backend.as_ref() == Some(&crate::backend::Backend::ClaudeCode) {
        let mcp_config = work_dir.join("mcp-config.json");
        if mcp_config.exists() {
            args.push("--mcp-config".to_string());
            args.push(mcp_config.display().to_string());
        }
        let settings = work_dir.join("claude-settings.json");
        if settings.exists() {
            args.push("--settings".to_string());
            args.push(settings.display().to_string());
        }
    }
    args.extend_from_slice(extra_args);

    // 9. Add agend-terminal to PATH
    let self_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let path_env = {
        let current = std::env::var("PATH").unwrap_or_default();
        match self_path {
            Some(dir) => format!("{}:{current}", dir.display()),
            None => current,
        }
    };

    // 10. Spawn backend process
    tracing::info!(%command, args = %args.join(" "), "starting backend");
    let mut child = std::process::Command::new(&command)
        .args(&args)
        .current_dir(&work_dir)
        .env("AGEND_INSTANCE_NAME", name)
        .env("AGEND_HOME", home.as_os_str())
        .env("PATH", &path_env)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    // Install signal handler to kill child and deregister on SIGINT/SIGTERM
    let child_id = child.id();
    let deregister_name = name.to_string();
    let deregister_home = home.to_path_buf();
    let cleanup = move || {
        crate::process::terminate(child_id);
        let _ = crate::api::call(
            &deregister_home,
            &serde_json::json!({
                "method": crate::api::method::DEREGISTER_EXTERNAL,
                "params": { "name": deregister_name }
            }),
        );
    };
    let cleanup_for_handler = std::sync::Arc::new(std::sync::Mutex::new(Some(cleanup)));
    let handler_ref = cleanup_for_handler.clone();
    ctrlc::set_handler(move || {
        if let Ok(mut guard) = handler_ref.lock() {
            if let Some(f) = guard.take() {
                f();
            }
        }
        std::process::exit(130);
    })
    .ok();

    // 11. Wait for child to exit
    let status = child.wait()?;

    // 12. Deregister from daemon (normal exit path — handler not triggered)
    // Consume the handler so it won't run again
    if let Ok(mut guard) = cleanup_for_handler.lock() {
        guard.take();
    }
    let _ = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::DEREGISTER_EXTERNAL,
            "params": { "name": name }
        }),
    );
    tracing::info!(%name, "agent disconnected");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
