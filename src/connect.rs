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
    if let Ok(resp) = crate::api::call(home, &serde_json::json!({"method": "list"})) {
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

    // 5. Resolve working directory
    let work_dir = match working_dir {
        Some(d) => {
            let p = PathBuf::from(d);
            if p.starts_with("~/") {
                let home_str = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                PathBuf::from(home_str).join(p.strip_prefix("~/").unwrap_or(&p))
            } else {
                p
            }
        }
        None => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&work_dir)?;

    // 6. Generate MCP config
    crate::instructions::generate(&work_dir, &command);
    eprintln!("[connect] MCP config written to {}", work_dir.display());

    // 7. Register with daemon
    let pid = std::process::id();
    match crate::api::call(
        home,
        &serde_json::json!({
            "method": "register_external",
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

    eprintln!("[connect] Registered '{name}' with daemon (pid={pid})");

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
    eprintln!("[connect] Starting {command} {}", args.join(" "));
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

    // 11. Wait for child to exit
    let status = child.wait()?;

    // 12. Deregister from daemon
    let _ = crate::api::call(
        home,
        &serde_json::json!({
            "method": "deregister_external",
            "params": { "name": name }
        }),
    );
    eprintln!("[connect] Agent '{name}' disconnected");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
