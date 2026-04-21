//! Linux: resolve terminal via config → `$TERMINAL` →
//! `x-terminal-emulator` → PATH-scan; dispatch with the invocation shape
//! for the chosen emulator.

use std::{path::Path, process::Command};

use super::{spawn_detached, OpenInTerminal};

pub struct LinuxTerminal {
    terminal: String,
}

impl LinuxTerminal {
    pub fn new(terminal: String) -> Self {
        Self { terminal }
    }

    /// Config > `$TERMINAL` > `x-terminal-emulator` > PATH fallback.
    fn resolve(&self) -> anyhow::Result<String> {
        if self.terminal != "default" {
            return Ok(self.terminal.clone());
        }
        if let Ok(t) = std::env::var("TERMINAL") {
            if !t.is_empty() {
                return Ok(t);
            }
        }
        if which::which("x-terminal-emulator").is_ok() {
            return Ok("x-terminal-emulator".to_string());
        }
        for candidate in FALLBACK_CHAIN {
            if which::which(candidate).is_ok() {
                return Ok((*candidate).to_string());
            }
        }
        anyhow::bail!("no terminal emulator found — set `terminal` in $AGEND_HOME/tray.toml")
    }
}

impl OpenInTerminal for LinuxTerminal {
    fn open(&self, cmd: &[&str]) -> anyhow::Result<()> {
        if cmd.is_empty() {
            anyhow::bail!("OpenInTerminal::open: empty cmd");
        }
        let term = self.resolve()?;
        let mut c = Command::new(&term);
        let bin = basename(&term);
        match invocation_shape(bin) {
            InvocationShape::DoubleDash => {
                c.arg("--").args(cmd);
            }
            InvocationShape::DashE => {
                c.arg("-e").args(cmd);
            }
        }
        // Must detach — xterm/kitty/alacritty/many konsole setups stay
        // foreground, so .status() would freeze the tray event loop for
        // the lifetime of the spawned terminal window.
        spawn_detached(c)
    }
}

const FALLBACK_CHAIN: &[&str] = &[
    "gnome-terminal",
    "konsole",
    "xfce4-terminal",
    "kitty",
    "alacritty",
    "xterm",
];

/// How this emulator takes a command to run.
///
/// `gnome-terminal` and `xfce4-terminal` use `--` (args after it are the
/// program); everything else (and the Debian `x-terminal-emulator`
/// alternative) accepts `-e program args...`.
#[derive(Debug, PartialEq, Eq)]
enum InvocationShape {
    DoubleDash,
    DashE,
}

fn invocation_shape(bin: &str) -> InvocationShape {
    match bin {
        "gnome-terminal" | "xfce4-terminal" => InvocationShape::DoubleDash,
        _ => InvocationShape::DashE,
    }
}

/// Like `Path::file_name().to_str()` but without `.unwrap()` sprinkled
/// anywhere (`unwrap_used` is a workspace deny).
fn basename(term: &str) -> &str {
    Path::new(term)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(term)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_and_xfce_use_double_dash() {
        assert_eq!(
            invocation_shape("gnome-terminal"),
            InvocationShape::DoubleDash
        );
        assert_eq!(
            invocation_shape("xfce4-terminal"),
            InvocationShape::DoubleDash
        );
    }

    #[test]
    fn others_use_dash_e_including_xterm_unknown() {
        for bin in [
            "konsole",
            "xterm",
            "kitty",
            "alacritty",
            "x-terminal-emulator",
            "unknown-term",
        ] {
            assert_eq!(invocation_shape(bin), InvocationShape::DashE, "bin = {bin}");
        }
    }

    #[test]
    fn basename_strips_directory() {
        assert_eq!(basename("/usr/bin/gnome-terminal"), "gnome-terminal");
        assert_eq!(basename("konsole"), "konsole");
    }

    #[test]
    fn config_terminal_wins_without_consulting_env() {
        // `resolve()` returns early when config is non-"default", so it
        // never reads `$TERMINAL` or hits PATH. Test without mutating
        // process env (races with other parallel tests — see commit
        // 5c9ca65 for the lesson the project already paid for).
        let t = LinuxTerminal::new("my-custom-term".to_string());
        let got = t
            .resolve()
            .expect("resolve should return config value as-is");
        assert_eq!(got, "my-custom-term");
    }
}
