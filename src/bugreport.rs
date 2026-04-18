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
    out.push_str(&format!(
        "Generated: {}\n\n",
        chrono::Utc::now().to_rfc3339()
    ));

    // Version info
    section(&mut out, "Version");
    out.push_str(&format!("agend-terminal: {}\n", env!("CARGO_PKG_VERSION")));
    for (label, cmd, arg) in [("rustc", "rustc", "--version"), ("system", "uname", "-a")] {
        if let Ok(o) = std::process::Command::new(cmd).arg(arg).output() {
            out.push_str(&format!(
                "{label}: {}\n",
                String::from_utf8_lossy(&o.stdout).trim()
            ));
        }
    }
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        out.push_str(&format!("terminal: {cols}x{rows}\n"));
    }
    out.push('\n');

    // Home directory
    section(&mut out, "Home Directory");
    out.push_str(&format!(
        "AGEND_HOME: {}\nexists: {}\n\n",
        home.display(),
        if home.exists() { "yes" } else { "no" }
    ));

    // File sections
    for (title, path, redact, fallback) in [
        (
            "Fleet Config (tokens redacted)",
            home.join("fleet.yaml"),
            true,
            "(not found)",
        ),
        ("Schedules", home.join("schedules.json"), false, "(none)"),
    ] {
        section(&mut out, title);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let s = if redact {
                    redact_secrets(&content)
                } else {
                    content
                };
                out.push_str(&s);
            }
            Err(_) => out.push_str(fallback),
        }
        out.push('\n');
    }

    // Daemon status
    section(&mut out, "Daemon Status");
    match crate::api::call(
        home,
        &serde_json::json!({"method": crate::api::method::LIST}),
    ) {
        Ok(resp) => out.push_str(&serde_json::to_string_pretty(&resp).unwrap_or_default()),
        Err(e) => out.push_str(&format!("(daemon not running: {e})")),
    }
    out.push_str("\n\n");

    // Snapshot
    section(&mut out, "Latest Snapshot");
    match crate::snapshot::load(home) {
        Some(snapshot) => {
            out.push_str(&format!(
                "timestamp: {}\nagents: {}\n",
                snapshot.timestamp,
                snapshot.agents.len()
            ));
            for a in &snapshot.agents {
                out.push_str(&format!(
                    "  {} state={} health={} cmd={}\n",
                    a.name, a.agent_state, a.health_state, a.backend_command
                ));
            }
        }
        None => out.push_str("(no snapshot)\n"),
    }
    out.push('\n');

    // Event log (last 50 lines)
    section(&mut out, "Event Log (last 50)");
    match std::fs::read_to_string(home.join("event-log.jsonl")) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            for line in &lines[lines.len().saturating_sub(50)..] {
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(_) => out.push_str("(no event log)\n"),
    }
    out.push('\n');

    // Installed backends
    section(&mut out, "Installed Backends");
    for b in crate::backend::Backend::all() {
        let (name, cmd) = (b.name(), b.preset().command);
        if b.is_installed() {
            out.push_str(&format!(
                "  {name} ({cmd}) v{}\n",
                b.get_version().unwrap_or_else(|| "?".into())
            ));
        } else {
            out.push_str(&format!("  {name} ({cmd}) — not installed\n"));
        }
    }
    out.push('\n');

    // Active sockets
    section(&mut out, "Active Sockets");
    if let Some(run) = crate::daemon::find_active_run_dir(home) {
        out.push_str(&format!("run dir: {}\n", run.display()));
        if let Ok(entries) = std::fs::read_dir(&run) {
            for entry in entries.flatten() {
                out.push_str(&format!("  {}\n", entry.file_name().to_string_lossy()));
            }
        }
    } else {
        out.push_str("(no active daemon)\n");
    }
    out.push('\n');

    // .env (redacted)
    section(&mut out, "Environment (.env redacted)");
    match std::fs::read_to_string(home.join(".env")) {
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
        Err(_) => out.push_str("(no .env)\n"),
    }
    out.push('\n');

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

/// Redact sensitive values (bot tokens, API keys, secrets).
///
/// Match is case-insensitive. `group_id` is intentionally kept on the list
/// because public IDs have moderately sensitive privacy implications for
/// Telegram groups.
fn redact_secrets(content: &str) -> String {
    // Lowercase substrings that trigger redaction if present anywhere in the
    // key portion of a `key: value` line.
    const TRIGGERS: &[&str] = &[
        "token",
        "key",
        "secret",
        "password",
        "passwd",
        "bearer",
        "authorization",
        "credential",
        "api_key",
        "apikey",
        "auth",
        "group_id",
    ];
    let mut result = String::new();
    for line in content.lines() {
        let (redactable, key_part) = if let Some((k, _)) = line.split_once(':') {
            let k_lower = k.to_ascii_lowercase();
            let hit = TRIGGERS.iter().any(|t| k_lower.contains(t));
            (hit, Some(k))
        } else {
            (false, None)
        };
        if redactable {
            if let Some(k) = key_part {
                result.push_str(&format!("{k}: ***REDACTED***\n"));
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

#[cfg(test)]
mod tests {
    use super::redact_secrets;

    #[test]
    fn redacts_common_secret_keys_case_insensitive() {
        let input = "\
bot_token: abc123
API_KEY: xyz
Password: p@ss
bearer_TOKEN: eyJhbG
authorization: Bearer xxx
api_key: k
ApiKey: k2
CREDENTIAL: c
normal_value: 42
";
        let out = redact_secrets(input);
        assert!(out.contains("bot_token: ***REDACTED***"));
        assert!(out.contains("API_KEY: ***REDACTED***"));
        assert!(out.contains("Password: ***REDACTED***"));
        assert!(out.contains("bearer_TOKEN: ***REDACTED***"));
        assert!(out.contains("authorization: ***REDACTED***"));
        assert!(out.contains("api_key: ***REDACTED***"));
        assert!(out.contains("ApiKey: ***REDACTED***"));
        assert!(out.contains("CREDENTIAL: ***REDACTED***"));
        assert!(out.contains("normal_value: 42"));
    }

    #[test]
    fn group_id_is_redacted() {
        let out = redact_secrets("group_id: -100123\n");
        assert!(out.contains("group_id: ***REDACTED***"));
    }

    #[test]
    fn non_keyed_lines_are_passed_through() {
        let out = redact_secrets("just a comment\n---\n");
        assert_eq!(out, "just a comment\n---\n");
    }

    #[test]
    fn does_not_redact_when_trigger_only_in_value() {
        // "token" appearing in the VALUE side of a legit key must not trigger.
        let out = redact_secrets("notes: the token was rotated\n");
        assert!(out.contains("notes: the token was rotated"));
    }
}
