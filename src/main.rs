mod client;
mod daemon;
mod protocol;
mod pty_session;

use anyhow::Result;
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
            let shell = args.get(2).map(|s| s.as_str()).unwrap_or("/bin/bash");
            let shell_args: Vec<String> = args.get(3..).unwrap_or_default().to_vec();
            let sock = socket_path();
            client::spawn_and_attach(&sock, shell, &shell_args).await?;
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
        _ => {
            eprintln!(
                "AgEnD Terminal PoC\n\n\
                 Usage:\n  \
                   agend-terminal daemon          Start the daemon\n  \
                   agend-terminal spawn [cmd]     Spawn a new session (default: /bin/bash)\n  \
                   agend-terminal attach <id>     Attach to existing session\n  \
                   agend-terminal list            List sessions\n\n\
                 Detach: Ctrl+B d"
            );
        }
    }

    Ok(())
}
