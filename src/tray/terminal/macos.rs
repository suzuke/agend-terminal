//! macOS: dispatch through `open(1)` for .app bundles, osascript for
//! iTerm2 (which ignores `--args` because it reuses sessions).

use std::process::Command;

use super::OpenInTerminal;

pub struct MacTerminal {
    terminal: String,
}

impl MacTerminal {
    pub fn new(terminal: String) -> Self {
        Self { terminal }
    }
}

impl OpenInTerminal for MacTerminal {
    fn open(&self, cmd: &[&str]) -> anyhow::Result<()> {
        if cmd.is_empty() {
            anyhow::bail!("OpenInTerminal::open: empty cmd");
        }
        match self.terminal.as_str() {
            "default" | "Terminal" => run_open("Terminal", cmd),
            "iTerm" => run_iterm(cmd),
            "Ghostty" => run_open_dash_e("Ghostty", cmd),
            other => run_open(other, cmd),
        }
    }
}

/// `open -na <app> --args <cmd...>` — Terminal.app interprets the trailing
/// args as the command to run in the new window.
fn run_open(app: &str, cmd: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("open")
        .arg("-na")
        .arg(app)
        .arg("--args")
        .args(cmd)
        .status()?;
    if !status.success() {
        anyhow::bail!("open -na {app} failed with {status}");
    }
    Ok(())
}

/// `open -na Ghostty --args -e '<shell-quoted cmd>'` — Ghostty treats `-e`
/// as a shell command string.
fn run_open_dash_e(app: &str, cmd: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("open")
        .arg("-na")
        .arg(app)
        .arg("--args")
        .arg("-e")
        .arg(shell_quote(cmd))
        .status()?;
    if !status.success() {
        anyhow::bail!("open -na {app} -e failed with {status}");
    }
    Ok(())
}

/// iTerm2 ignores `open --args` on an already-running instance, so we
/// drive it via AppleScript. `create window with default profile command
/// "..."` opens a new window running the given shell command.
fn run_iterm(cmd: &[&str]) -> anyhow::Result<()> {
    let cmd_str = shell_quote(cmd).replace('"', "\\\"");
    let script = format!(
        r#"tell application "iTerm2"
    create window with default profile command "{cmd_str}"
end tell"#
    );
    let status = Command::new("osascript").arg("-e").arg(&script).status()?;
    if !status.success() {
        anyhow::bail!("osascript (iTerm2) failed with {status}");
    }
    Ok(())
}

/// Minimal POSIX-style shell quoting. Used for terminals that take a
/// single command string (iTerm2, Ghostty). Characters that need no
/// quoting are passed through; everything else is single-quoted with
/// `'\''` escaping.
pub(super) fn shell_quote(cmd: &[&str]) -> String {
    cmd.iter()
        .map(|arg| quote_one(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_one(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '=' | ',' | ':')
        })
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_passes_safe_args_through() {
        assert_eq!(
            shell_quote(&["agend-terminal", "app"]),
            "agend-terminal app"
        );
    }

    #[test]
    fn shell_quote_wraps_args_with_spaces_or_specials() {
        assert_eq!(shell_quote(&["echo", "hi there"]), "echo 'hi there'");
        assert_eq!(shell_quote(&["run", "$VAR"]), "run '$VAR'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote(&["say", "it's fine"]), r"say 'it'\''s fine'");
    }

    #[test]
    fn empty_string_is_quoted() {
        assert_eq!(shell_quote(&[""]), "''");
    }
}
