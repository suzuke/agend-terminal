mod client;
mod daemon;
mod fleet;
mod instructions;
mod protocol;
mod pty_session;
mod telegram;
mod vterm;

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AGEND_TERMINAL_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(".agend-terminal")
}

fn socket_path() -> PathBuf {
    home_dir().join("agend-terminal.sock")
}

fn session_id_from_env() -> Option<u32> {
    std::env::var("AGEND_SESSION_ID")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Load .env file from AGEND_TERMINAL_HOME, setting vars into process env.
fn load_dotenv() {
    let env_path = home_dir().join(".env");
    if !env_path.exists() {
        return;
    }
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip optional "export " prefix
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            // Strip surrounding quotes from value
            let value = value.trim();
            let value = if let Some(quoted) = value
                .strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            {
                quoted // Quoted: preserve everything including #
            } else {
                // Unquoted: strip inline comments
                value.split('#').next().unwrap_or(value).trim()
            };
            if !key.is_empty() {
                std::env::set_var(key, value);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();

    tracing_subscriber::fmt()
        .with_env_filter("agend_terminal=info")
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "daemon" => {
            let sock = socket_path();
            println!("Starting daemon, socket: {}", sock.display());
            daemon::run(&sock).await?;
        }
        "spawn" => {
            let mut env_map: HashMap<String, String> = HashMap::new();
            let mut cols: Option<u16> = None;
            let mut rows: Option<u16> = None;
            let mut ready_pattern: Option<String> = None;
            let mut command: Option<String> = None;
            let mut command_args: Vec<String> = Vec::new();
            let mut past_flags = false;

            let mut i = 2;
            while i < args.len() {
                if past_flags {
                    command_args.push(args[i].clone());
                    i += 1;
                    continue;
                }
                match args[i].as_str() {
                    "--env" => {
                        i += 1;
                        if let Some(kv) = args.get(i) {
                            if let Some((k, v)) = kv.split_once('=') {
                                env_map.insert(k.to_string(), v.to_string());
                            }
                        }
                    }
                    "--cols" => {
                        i += 1;
                        cols = args.get(i).and_then(|s| s.parse().ok());
                    }
                    "--rows" => {
                        i += 1;
                        rows = args.get(i).and_then(|s| s.parse().ok());
                    }
                    "--ready-pattern" => {
                        i += 1;
                        ready_pattern = args.get(i).cloned();
                    }
                    "--" => {
                        past_flags = true;
                    }
                    other => {
                        if command.is_none() {
                            command = Some(other.to_string());
                        } else {
                            command_args.push(other.to_string());
                        }
                    }
                }
                i += 1;
            }

            let command = command.as_deref().unwrap_or("/bin/bash");
            let env = if env_map.is_empty() { None } else { Some(env_map) };
            let sock = socket_path();
            client::spawn_and_attach(&sock, command, &command_args, env, ready_pattern, cols, rows)
                .await?;
        }
        "attach" => {
            let session_id: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal attach <session_id>");
                std::process::exit(1);
            });
            let sock = socket_path();
            client::attach(&sock, session_id).await?;
        }
        "list" | "ls" => {
            let sock = socket_path();
            client::list_sessions(&sock).await?;
        }
        "create-instance" => {
            let mut name: Option<String> = None;
            let mut command: Option<String> = None;
            let mut cmd_args: Vec<String> = Vec::new();
            let mut env_map: HashMap<String, String> = HashMap::new();
            let mut working_dir: Option<String> = None;
            let mut topic_name: Option<String> = None;
            let mut ready_pattern: Option<String> = None;
            let mut cols: Option<u16> = None;
            let mut rows: Option<u16> = None;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--name" => { i += 1; name = args.get(i).cloned(); }
                    "--command" => { i += 1; command = args.get(i).cloned(); }
                    "--args" => {
                        i += 1;
                        if let Some(a) = args.get(i) {
                            cmd_args = a.split_whitespace().map(|s| s.to_string()).collect();
                        }
                    }
                    "--env" => {
                        i += 1;
                        if let Some(kv) = args.get(i) {
                            if let Some((k, v)) = kv.split_once('=') {
                                env_map.insert(k.to_string(), v.to_string());
                            }
                        }
                    }
                    "--working-directory" => { i += 1; working_dir = args.get(i).cloned(); }
                    "--topic-name" => { i += 1; topic_name = args.get(i).cloned(); }
                    "--ready-pattern" => { i += 1; ready_pattern = args.get(i).cloned(); }
                    "--cols" => { i += 1; cols = args.get(i).and_then(|s| s.parse().ok()); }
                    "--rows" => { i += 1; rows = args.get(i).and_then(|s| s.parse().ok()); }
                    _ => {}
                }
                i += 1;
            }

            let name = name.unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal create-instance --name NAME --command CMD [options]");
                std::process::exit(1);
            });
            let command = command.unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal create-instance --name NAME --command CMD [options]");
                std::process::exit(1);
            });
            let env = if env_map.is_empty() { None } else { Some(env_map) };
            let sock = socket_path();
            client::create_instance(
                &sock, &name, &command, &cmd_args, env,
                working_dir, topic_name, ready_pattern, cols, rows,
            ).await?;
        }
        "inject" => {
            let session_id: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal inject <session_id> <text>");
                std::process::exit(1);
            });
            let text = args.get(3..).unwrap_or_default().join(" ");
            if text.is_empty() {
                eprintln!("Usage: agend-terminal inject <session_id> <text>");
                std::process::exit(1);
            }
            let data = unescape(&text);
            let sock = socket_path();
            client::inject(&sock, session_id, &data).await?;
        }
        "kill" => {
            let session_id: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal kill <session_id> [--quit-cmd CMD] [--grace N]");
                std::process::exit(1);
            });
            let mut quit_command: Option<String> = None;
            let mut grace_seconds: Option<u32> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--quit-cmd" => {
                        i += 1;
                        quit_command = args.get(i).cloned();
                    }
                    "--grace" => {
                        i += 1;
                        grace_seconds = args.get(i).and_then(|s| s.parse().ok());
                    }
                    _ => {}
                }
                i += 1;
            }
            let sock = socket_path();
            client::kill_session(&sock, session_id, quit_command, grace_seconds).await?;
        }

        // --- Fleet commands ---
        "fleet" => {
            let subcmd = args.get(2).map(|s| s.as_str()).unwrap_or("help");
            let sock = socket_path();
            match subcmd {
                "start" => {
                    let default_config = home_dir().join("fleet.yaml");
                    let default_str = default_config.to_string_lossy().to_string();
                    let config_path = args
                        .get(3)
                        .map(|s| s.as_str())
                        .unwrap_or(&default_str);
                    let names: Vec<String> = args.get(4..).unwrap_or_default().to_vec();
                    client::fleet_start(&sock, config_path, names).await?;
                }
                "stop" => {
                    let names: Vec<String> = args.get(3..).unwrap_or_default().to_vec();
                    client::fleet_stop(&sock, names).await?;
                }
                _ => {
                    eprintln!(
                        "Fleet commands:\n  \
                           agend-terminal fleet start [config.yaml] [name...]\n  \
                           agend-terminal fleet stop [name...]"
                    );
                }
            }
        }

        // --- Agent communication (reads AGEND_SESSION_ID from env) ---
        "reply" => {
            let session_id = session_id_from_env().unwrap_or_else(|| {
                eprintln!("Error: AGEND_SESSION_ID not set. This command is for agents.");
                std::process::exit(1);
            });
            let text = args.get(2..).unwrap_or_default().join(" ");
            if text.is_empty() {
                eprintln!("Usage: agend-terminal reply <text>");
                std::process::exit(1);
            }
            let sock = socket_path();
            client::reply(&sock, session_id, &text).await?;
        }
        "send" => {
            let session_id = session_id_from_env().unwrap_or_else(|| {
                eprintln!("Error: AGEND_SESSION_ID not set. This command is for agents.");
                std::process::exit(1);
            });
            let target = args.get(2).cloned().unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal send <target> <text> [--kind K] [--correlation-id ID]");
                std::process::exit(1);
            });
            // Parse optional flags and text
            let mut text_parts: Vec<String> = Vec::new();
            let mut kind: Option<String> = None;
            let mut correlation_id: Option<String> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--kind" => {
                        i += 1;
                        kind = args.get(i).cloned();
                    }
                    "--correlation-id" => {
                        i += 1;
                        correlation_id = args.get(i).cloned();
                    }
                    other => {
                        text_parts.push(other.to_string());
                    }
                }
                i += 1;
            }
            let text = text_parts.join(" ");
            if text.is_empty() {
                eprintln!("Usage: agend-terminal send <target> <text>");
                std::process::exit(1);
            }
            let sock = socket_path();
            client::send_message(&sock, session_id, &target, &text, kind, correlation_id).await?;
        }
        "inbox" => {
            let session_id = session_id_from_env().unwrap_or_else(|| {
                eprintln!("Error: AGEND_SESSION_ID not set. This command is for agents.");
                std::process::exit(1);
            });
            let sock = socket_path();
            client::inbox(&sock, session_id).await?;
        }

        _ => {
            eprintln!(
                "AgEnD Terminal\n\n\
                 Session management:\n  \
                   agend-terminal daemon\n  \
                   agend-terminal spawn [flags] [cmd] [-- args...]\n  \
                   agend-terminal attach <id>\n  \
                   agend-terminal list\n  \
                   agend-terminal inject <id> <text>\n  \
                   agend-terminal kill <id> [--quit-cmd CMD] [--grace N]\n\n\
                 Fleet management:\n  \
                   agend-terminal fleet start [config.yaml] [name...]\n  \
                   agend-terminal fleet stop [name...]\n  \
                   agend-terminal create-instance --name N --command C [opts]\n\n\
                 Agent communication (requires AGEND_SESSION_ID):\n  \
                   agend-terminal reply <text>\n  \
                   agend-terminal send <target> <text> [--kind K]\n  \
                   agend-terminal inbox\n\n\
                 Spawn flags:\n  \
                   --env KEY=VALUE      Set environment variable (repeatable)\n  \
                   --cols N / --rows N  Terminal size\n  \
                   --ready-pattern RE   Regex to detect CLI ready\n  \
                 Detach: Ctrl+B d"
            );
        }
    }

    Ok(())
}

fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('r') => out.push(b'\r'),
                Some('\\') => out.push(b'\\'),
                Some(other) => {
                    out.push(b'\\');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => out.push(b'\\'),
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}
