mod agent;
mod daemon;
mod framing;
mod tui;
mod vterm;

use std::path::PathBuf;

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AGEND_TERMINAL_HOME") {
        return PathBuf::from(home);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(".agend-terminal")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let home = home_dir();
    std::fs::create_dir_all(&home)?;

    match cmd {
        "start" | "daemon" => {
            // Parse agents from CLI: start name:command name2:command2
            let agents: Vec<_> = args[2..]
                .iter()
                .map(|a| {
                    if let Some((name, cmd)) = a.split_once(':') {
                        (
                            name.to_string(),
                            cmd.to_string(),
                            Vec::new(),
                            None,
                            None,
                            "\r".to_string(),
                        )
                    } else {
                        (
                            a.to_string(),
                            a.to_string(),
                            Vec::new(),
                            None,
                            None,
                            "\r".to_string(),
                        )
                    }
                })
                .collect();

            let agents = if agents.is_empty() {
                vec![(
                    "shell".to_string(),
                    "/bin/bash".to_string(),
                    Vec::new(),
                    None,
                    None,
                    "\r".to_string(),
                )]
            } else {
                agents
            };

            daemon::run(&home, agents)?;
        }
        "attach" => {
            let name = args.get(2).map(|s| s.as_str()).unwrap_or("shell");
            let sock = daemon::agent_socket_path(&home, name);
            tui::attach(&sock)?;
        }
        "inject" => {
            let name = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: agend-terminal inject <name> <text>");
                std::process::exit(1);
            });
            let text = args.get(3..).unwrap_or_default().join(" ");
            if text.is_empty() {
                eprintln!("Usage: agend-terminal inject <name> <text>");
                std::process::exit(1);
            }
            let sock = daemon::agent_socket_path(&home, name);
            inject(&sock, text.as_bytes())?;
        }
        "list" | "ls" => {
            // List socket files in home directory
            for entry in std::fs::read_dir(&home)?.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".sock") {
                    let agent = &name[..name.len() - 5];
                    println!("  {agent}");
                }
            }
        }
        _ => {
            eprintln!(
                "AgEnD Terminal v2\n\n\
                 Usage:\n  \
                   agend-terminal start [name:cmd ...]    Start daemon with agents\n  \
                   agend-terminal attach <name>           Attach to agent (Ctrl+B d to detach)\n  \
                   agend-terminal inject <name> <text>    Send input to agent\n  \
                   agend-terminal list                    List active agents\n"
            );
        }
    }

    Ok(())
}

fn inject(socket_path: &str, data: &[u8]) -> anyhow::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path)?;
    // The TUI socket server expects tagged frames
    // We need to send a data frame that the input handler will forward to PTY
    // But the socket server's input loop only runs when a TUI client connects.
    // For inject without attach, we connect, send the data frame, and disconnect.
    framing::write_frame(&mut stream, data)?;
    println!("Injected {} bytes", data.len());
    Ok(())
}
