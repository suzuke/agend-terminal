//! `agend-supervisor` — the frozen self-healing supervisor binary.
//!
//! See [`agend_terminal::supervisor`] for architecture. This entry point
//! stays deliberately tiny: resolve `$AGEND_HOME`, install tracing, hand
//! control to the server loop.

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    #[command(name = "agend-supervisor", version, about = "Frozen self-healing supervisor for agend-terminal")]
    struct Args {
        /// Override `$AGEND_HOME` (defaults to the env var, then `~/.agend`).
        #[arg(long)]
        home: Option<std::path::PathBuf>,
    }

    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("AGEND_SUPERVISOR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let home = args.home.unwrap_or_else(default_home);
    agend_terminal::supervisor::server::run(&home)
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    eprintln!("agend-supervisor is Unix-only");
    std::process::exit(1);
}

#[cfg(unix)]
fn default_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("AGEND_HOME") {
        return std::path::PathBuf::from(home);
    }
    let base = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    let new_path = base.join(".agend");
    let legacy = base.join(".agend-terminal");
    if new_path.exists() || !legacy.exists() {
        new_path
    } else {
        legacy
    }
}
