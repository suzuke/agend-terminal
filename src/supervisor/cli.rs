//! CLI driver for `agend-terminal upgrade`.
//!
//! Separated from the binary main.rs so the flow is unit-testable and so
//! future commands (e.g. `agend-terminal status --supervisor`) can share
//! the same probe / staging helpers.
//!
//! The driver is deliberately non-interactive: if bootstrap is needed
//! (first upgrade on a supervisor-less install) the caller must pass
//! `--yes` or the command exits with an error message explaining how to
//! proceed.

use super::{client, ipc::UpgradeArgs, paths};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Options parsed from the `upgrade` subcommand.
#[derive(Debug, Clone)]
pub struct UpgradeOptions {
    /// Path to the new daemon binary to upgrade to.
    pub new_binary: PathBuf,
    /// Optional human-visible version string for the new binary. Rendered
    /// to the user and into the system message injected into respawned
    /// agents. Falls back to `new_binary --version` output.
    pub to_version: Option<String>,
    /// Skip interactive confirmation (required for `--install-supervisor`
    /// migration on fresh installs).
    pub assume_yes: bool,
    /// Install the supervisor + set up `$AGEND_HOME/bin/` layout if it
    /// isn't already present. Without this flag, an un-supervised install
    /// that calls `upgrade` will error out with instructions.
    pub install_supervisor: bool,
    /// Stability window (seconds). Defaults to 60; 0 disables the check.
    pub stability_secs: u64,
    /// Ready-ping timeout (seconds). Defaults to 60; 0 disables.
    pub ready_timeout_secs: u64,
}

impl Default for UpgradeOptions {
    fn default() -> Self {
        Self {
            new_binary: PathBuf::new(),
            to_version: None,
            assume_yes: false,
            install_supervisor: false,
            stability_secs: 60,
            ready_timeout_secs: 60,
        }
    }
}

/// Entry point for `agend-terminal upgrade`. Returns `Ok(())` on success,
/// `Err` on any failure including rollback (the rollback itself is
/// considered a failed upgrade even though the system is back in a good
/// state).
#[cfg(unix)]
pub fn run(home: &Path, opts: UpgradeOptions) -> Result<()> {
    // 1. Pre-flight: binary exists and is executable.
    if !opts.new_binary.exists() {
        anyhow::bail!(
            "new binary does not exist: {}",
            opts.new_binary.display()
        );
    }
    let abs = std::fs::canonicalize(&opts.new_binary).with_context(|| {
        format!("canonicalize new binary {}", opts.new_binary.display())
    })?;

    // 2. Quick sanity check via `--version` — also gives us a version string.
    let reported_version = client::probe_new_binary_version(&abs)
        .with_context(|| format!("probe --version of {}", abs.display()))?;
    let to_version = opts
        .to_version
        .clone()
        .or(Some(reported_version.clone()));
    eprintln!(
        "upgrade: new binary {} reports version: {}",
        abs.display(),
        reported_version
    );

    // 3. Detect supervisor.
    let supervisor_pid = client::probe(home).context("probe supervisor")?;
    match supervisor_pid {
        Some(pid) => eprintln!("upgrade: supervisor detected (pid {pid})"),
        None => {
            if !opts.install_supervisor {
                anyhow::bail!(
                    "no supervisor running. This is a supervisor-less install.\n\
                     Re-run with --install-supervisor to bootstrap one (will restart the daemon).\n\
                     Or run `agend-terminal stop` and then `agend-supervisor` manually."
                );
            }
            if !opts.assume_yes && !prompt_yes_no(
                "install agend-supervisor and migrate the daemon to run under it?",
            )? {
                anyhow::bail!("cancelled by user");
            }
            install_supervisor_layout(home, &abs)?;
            eprintln!(
                "upgrade: supervisor layout installed at {}.",
                paths::bin_dir(home).display()
            );
            eprintln!(
                "upgrade: start it with `agend-supervisor --home {}`, then re-run upgrade.",
                home.display()
            );
            // Returning here instead of auto-starting the supervisor: the
            // caller's shell may want to nohup / disown / systemd it, and
            // guessing that policy from inside this process tends to go
            // wrong. Explicit is better.
            return Ok(());
        }
    }

    // 4. Determine from_version by asking supervisor (status).
    let from_version = fetch_current_version(home).unwrap_or_else(|_| "(unknown)".into());
    eprintln!("upgrade: current version: {from_version}");

    // 5. Stage both binaries into the content store.
    let new_hash = client::stage_binary(home, &abs)
        .context("stage new binary into content store")?;
    let prev_hash = client::stage_current_as_prev(home)
        .context("stage current binary as rollback target")?;
    if new_hash == prev_hash {
        eprintln!(
            "upgrade: new binary is identical to current ({}); nothing to do.",
            short_hash(&new_hash)
        );
        return Ok(());
    }
    eprintln!(
        "upgrade: staged new={} prev={}",
        short_hash(&new_hash),
        short_hash(&prev_hash)
    );

    // 6. Flip the `current` symlink. After this point, a rollback (whether
    //    initiated by supervisor or via manual intervention) must restore
    //    it to `store/<prev_hash>`.
    client::swap_current(home, &new_hash, &prev_hash)
        .context("atomic swap of bin/current symlink")?;
    eprintln!("upgrade: bin/current → store/{}", short_hash(&new_hash));

    // 7. Ask the supervisor to perform the switchover.
    let args = UpgradeArgs {
        new_hash,
        prev_hash,
        from_version: Some(from_version.clone()),
        to_version: to_version.clone(),
        stability_secs: opts.stability_secs,
        ready_timeout_secs: opts.ready_timeout_secs,
    };
    let result = client::send_upgrade(home, args, |stage, msg| {
        eprintln!("upgrade [{:?}] {}", stage, msg);
    })?;

    match result {
        super::ipc::Response::Ok {
            message, r#final, ..
        } => {
            if r#final {
                if let Some(m) = message {
                    eprintln!("upgrade: {m}");
                }
                eprintln!(
                    "upgrade: done. {} → {}",
                    from_version,
                    to_version.unwrap_or_else(|| "(unknown)".into())
                );
                Ok(())
            } else {
                anyhow::bail!("supervisor returned a non-terminal Ok response")
            }
        }
        super::ipc::Response::Err { error, .. } => {
            anyhow::bail!("upgrade failed: {error}")
        }
        super::ipc::Response::Progress { .. } => {
            anyhow::bail!("upgrade: protocol error (unexpected terminal progress)")
        }
    }
}

#[cfg(not(unix))]
pub fn run(_home: &Path, _opts: UpgradeOptions) -> Result<()> {
    anyhow::bail!(
        "`agend-terminal upgrade` is not supported on this platform.\n\
         Hot upgrade requires Unix (symlink swap + UDS supervisor IPC)."
    )
}

/// Stage the current running binary as `bin/current` and install the
/// supervisor shim at `bin/supervisor`, so a subsequent `agend-supervisor`
/// start has everything it needs.
///
/// Behaviour:
/// - Copies the running `agend-terminal` binary (the one calling this
///   function) into the content store and points `bin/current` at it.
/// - Resolves the `agend-supervisor` binary — either installed alongside
///   agend-terminal (via `cargo install --bins`) or in the same parent
///   directory as the current binary — and symlinks `bin/supervisor` to
///   it. If we can't find it, we emit an error pointing the user at
///   `cargo install agend-terminal`.
#[cfg(unix)]
fn install_supervisor_layout(home: &Path, new_daemon_binary: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let bin = paths::bin_dir(home);
    std::fs::create_dir_all(&bin)
        .with_context(|| format!("create bin dir {}", bin.display()))?;

    // Stage the new daemon binary and point current → store/<hash>.
    let hash = client::stage_binary(home, new_daemon_binary)
        .context("stage daemon binary during bootstrap")?;
    let cur_tmp = bin.join(".current.new");
    let _ = std::fs::remove_file(&cur_tmp);
    let target = PathBuf::from("store").join(&hash);
    symlink(&target, &cur_tmp)
        .with_context(|| format!("create bootstrap current symlink {}", cur_tmp.display()))?;
    std::fs::rename(&cur_tmp, paths::current_link(home))
        .context("rename bootstrap current symlink into place")?;

    // Locate agend-supervisor. The cheapest + most reliable way: look in
    // the same directory as the currently running agend-terminal. If the
    // user ran `cargo install agend-terminal`, both bins end up in the
    // same cargo-bin dir.
    let self_exe = std::env::current_exe().context("current_exe for bootstrap")?;
    let self_dir = self_exe
        .parent()
        .context("current_exe has no parent dir")?;
    let candidates = [
        self_dir.join("agend-supervisor"),
        // Debug-build fallback (developer installs from source).
        self_dir.join("../agend-supervisor"),
    ];
    let supervisor = candidates
        .iter()
        .find(|p| p.exists())
        .with_context(|| {
            format!(
                "agend-supervisor binary not found near {}. \
                 Install with `cargo install agend-terminal` or build both bins.",
                self_dir.display()
            )
        })?;
    let sup_target = std::fs::canonicalize(supervisor)
        .with_context(|| format!("canonicalize {}", supervisor.display()))?;

    let sup_link = paths::supervisor_link(home);
    let _ = std::fs::remove_file(&sup_link);
    symlink(&sup_target, &sup_link)
        .with_context(|| format!("symlink supervisor into place {}", sup_link.display()))?;

    Ok(())
}

/// Ask the running supervisor for its tracked current version. Falls back
/// to reading the binary's `--version` output if the supervisor doesn't
/// populate one (old wire version).
#[cfg(unix)]
fn fetch_current_version(home: &Path) -> Result<String> {
    use super::ipc::{self, Request, Response};
    let sock = paths::supervisor_sock(home);
    let stream = ipc::uds::connect(&sock).context("connect supervisor socket")?;
    let reader = stream.try_clone().context("clone stream")?;
    let mut writer = stream;
    ipc::write_one(&mut writer, &Request::Status).context("write status request")?;
    let mut br = std::io::BufReader::new(reader);
    let resp: Response = ipc::read_one::<Response, _>(&mut br)
        .context("read status response")?
        .ok_or_else(|| anyhow::anyhow!("supervisor closed without response"))?;
    match resp {
        Response::Ok { data, .. } => Ok(data
            .as_ref()
            .and_then(|v| v.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)")
            .to_string()),
        Response::Err { error, .. } => anyhow::bail!("status error: {error}"),
        Response::Progress { .. } => anyhow::bail!("unexpected progress frame"),
    }
}

fn short_hash(h: &str) -> &str {
    if h.len() >= 12 {
        &h[..12]
    } else {
        h
    }
}

fn prompt_yes_no(question: &str) -> Result<bool> {
    use std::io::BufRead;
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .context("read yes/no answer from stdin")?;
    let line = line.trim().to_ascii_lowercase();
    Ok(matches!(line.as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hash_truncates() {
        assert_eq!(short_hash("abcdefghijklmnop"), "abcdefghijkl");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn default_options_stability_defaults() {
        let o = UpgradeOptions::default();
        assert_eq!(o.stability_secs, 60);
        assert_eq!(o.ready_timeout_secs, 60);
        assert!(!o.assume_yes);
        assert!(!o.install_supervisor);
    }
}
