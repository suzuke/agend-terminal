//! Bug report generator — collects diagnostics, logs, and config into a single file.

use std::path::Path;

pub fn run(home: &Path) -> anyhow::Result<()> {
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!("bugreport-{timestamp}.txt");
    let output_path = std::env::current_dir()
        .unwrap_or_else(|_| home.to_path_buf())
        .join(&filename);

    let mut out = String::new();

    section(&mut out, "AgEnD Terminal Bug Report");
    out.push_str(&format!("Generated: {}\n", chrono::Utc::now().to_rfc3339()));
    out.push('\n');

    // Version info
    section(&mut out, "Version");
    out.push_str(&format!("agend-terminal: {}\n", env!("CARGO_PKG_VERSION")));
    if let Ok(o) = std::process::Command::new("rustc")
        .arg("--version")
        .output()
    {
        out.push_str(&format!(
            "rustc: {}\n",
            String::from_utf8_lossy(&o.stdout).trim()
        ));
    }
    if let Ok(o) = std::process::Command::new("uname").arg("-a").output() {
        out.push_str(&format!(
            "system: {}\n",
            String::from_utf8_lossy(&o.stdout).trim()
        ));
    }
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        out.push_str(&format!("terminal: {cols}x{rows}\n"));
    }
    out.push('\n');

    // Home directory
    section(&mut out, "Home Directory");
    out.push_str(&format!("AGEND_TERMINAL_HOME: {}\n", home.display()));
    out.push_str(&format!(
        "exists: {}\n",
        if home.exists() { "yes" } else { "no" }
    ));
    out.push('\n');

    // Fleet config (redacted)
    section(&mut out, "Fleet Config (tokens redacted)");
    let fleet_path = home.join("fleet.yaml");
    if fleet_path.exists() {
        match std::fs::read_to_string(&fleet_path) {
            Ok(content) => {
                let redacted = redact_secrets(&content);
                out.push_str(&redacted);
            }
            Err(e) => out.push_str(&format!("(read error: {e})\n")),
        }
    } else {
        out.push_str("(not found)\n");
    }
    out.push('\n');

    // Daemon status
    section(&mut out, "Daemon Status");
    match crate::api::call(home, &serde_json::json!({"method": "list"})) {
        Ok(resp) => {
            out.push_str(&serde_json::to_string_pretty(&resp).unwrap_or_default());
            out.push('\n');
        }
        Err(e) => out.push_str(&format!("(daemon not running: {e})\n")),
    }
    out.push('\n');

    // Snapshot
    section(&mut out, "Latest Snapshot");
    match crate::snapshot::load(home) {
        Some(snapshot) => {
            out.push_str(&format!("timestamp: {}\n", snapshot.timestamp));
            out.push_str(&format!("agents: {}\n", snapshot.agents.len()));
            for a in &snapshot.agents {
                out.push_str(&format!(
                    "  {} state={} health={} cmd={}\n",
                    a.name, a.agent_state, a.health_state, a.command
                ));
            }
        }
        None => out.push_str("(no snapshot)\n"),
    }
    out.push('\n');

    // Event log (last 50 lines)
    section(&mut out, "Event Log (last 50)");
    let event_log = home.join("event-log.jsonl");
    if event_log.exists() {
        match std::fs::read_to_string(&event_log) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(50);
                for line in &lines[start..] {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Err(e) => out.push_str(&format!("(read error: {e})\n")),
        }
    } else {
        out.push_str("(no event log)\n");
    }
    out.push('\n');

    // Installed backends
    section(&mut out, "Installed Backends");
    for b in crate::backend::Backend::all() {
        let preset = b.preset();
        if b.is_installed() {
            let version = b.get_version().unwrap_or_else(|| "?".into());
            out.push_str(&format!(
                "  {} ({}) v{}\n",
                b.name(),
                preset.command,
                version
            ));
        } else {
            out.push_str(&format!(
                "  {} ({}) — not installed\n",
                b.name(),
                preset.command
            ));
        }
    }
    out.push('\n');

    // Active sockets
    section(&mut out, "Active Sockets");
    if let Some(run) = crate::daemon::find_active_run_dir(home) {
        out.push_str(&format!("run dir: {}\n", run.display()));
        if let Ok(entries) = std::fs::read_dir(&run) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                out.push_str(&format!("  {name}\n"));
            }
        }
    } else {
        out.push_str("(no active daemon)\n");
    }
    out.push('\n');

    // .env (redacted)
    section(&mut out, "Environment (.env redacted)");
    let env_path = home.join(".env");
    if env_path.exists() {
        match std::fs::read_to_string(&env_path) {
            Ok(content) => {
                for line in content.lines() {
                    if let Some((key, _)) = line.split_once('=') {
                        out.push_str(&format!("{key}=***REDACTED***\n"));
                    } else {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            Err(e) => out.push_str(&format!("(read error: {e})\n")),
        }
    } else {
        out.push_str("(no .env)\n");
    }
    out.push('\n');

    // Schedules
    section(&mut out, "Schedules");
    let schedules_path = home.join("schedules.json");
    if schedules_path.exists() {
        match std::fs::read_to_string(&schedules_path) {
            Ok(content) => {
                out.push_str(&content);
                out.push('\n');
            }
            Err(e) => out.push_str(&format!("(read error: {e})\n")),
        }
    } else {
        out.push_str("(none)\n");
    }

    // Write report
    std::fs::write(&output_path, &out)?;
    println!("Bug report saved to: {}", output_path.display());
    println!("Size: {} bytes ({} lines)", out.len(), out.lines().count());
    println!("\nPlease attach this file when reporting issues.");

    Ok(())
}

fn section(out: &mut String, title: &str) {
    let sep = "=".repeat(60);
    out.push_str(&sep);
    out.push('\n');
    out.push_str(&format!("  {title}\n"));
    out.push_str(&sep);
    out.push('\n');
}

/// Redact sensitive values (bot tokens, API keys).
fn redact_secrets(content: &str) -> String {
    let mut result = String::new();
    for line in content.lines() {
        if line.contains("token")
            || line.contains("TOKEN")
            || line.contains("key")
            || line.contains("KEY")
        {
            if let Some((key, _)) = line.split_once(':') {
                result.push_str(&format!("{key}: ***REDACTED***\n"));
            } else {
                result.push_str(line);
                result.push('\n');
            }
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}
