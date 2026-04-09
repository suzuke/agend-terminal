mod client;
mod daemon;
mod protocol;
mod pty_session;

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/agend-terminal-{}", nix::unistd::getuid()));
    PathBuf::from(runtime_dir).join("agend-terminal.sock")
}

#[tokio::main]
async fn main() -> Result<()> {
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
            // Parse flags: --env KEY=VALUE, --cols N, --rows N, --ready-pattern REGEX
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
            let env = if env_map.is_empty() {
                None
            } else {
                Some(env_map)
            };
            let sock = socket_path();
            client::spawn_and_attach(
                &sock,
                command,
                &command_args,
                env,
                ready_pattern,
                cols,
                rows,
            )
            .await?;
        }
        "attach" => {
            let session_id: u32 = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
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
        "inject" => {
            let session_id: u32 = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
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
            let session_id: u32 = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| {
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
        _ => {
            eprintln!(
                "AgEnD Terminal MVP\n\n\
                 Usage:\n  \
                   agend-terminal daemon\n  \
                   agend-terminal spawn [flags] [cmd] [-- args...]\n  \
                   agend-terminal attach <id>\n  \
                   agend-terminal list\n  \
                   agend-terminal inject <id> <text>\n  \
                   agend-terminal kill <id> [--quit-cmd CMD] [--grace N]\n\n\
                 Spawn flags:\n  \
                   --env KEY=VALUE      Set environment variable (repeatable)\n  \
                   --cols N             Initial terminal columns\n  \
                   --rows N             Initial terminal rows\n  \
                   --ready-pattern RE   Regex to detect when CLI is ready\n\n\
                 Detach: Ctrl+] d"
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
