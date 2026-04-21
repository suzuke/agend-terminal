//! Linux autostart via XDG `.desktop`.
//!
//! Writes `~/.config/autostart/agend-terminal.desktop`. Works across
//! GNOME, KDE, XFCE, Cinnamon, MATE, LXQt — the XDG spec is uniform
//! where the systemd-user-unit path is not.

use std::{fs, path::PathBuf};

use super::Autostart;

pub struct LinuxAutostart;

impl LinuxAutostart {
    pub fn new(_home: PathBuf) -> Self {
        Self
    }

    fn desktop_path() -> anyhow::Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME"))?;
        Ok(home.join(".config/autostart/agend-terminal.desktop"))
    }

    fn render(exe: &std::path::Path) -> String {
        // Exec quoting: `.desktop` files use a restricted quoting grammar
        // (§Exec in the XDG Desktop Entry spec). A plain absolute path
        // with no whitespace is the common case; if the path contains
        // spaces, wrap in double quotes per the spec.
        let exe_str = exe.to_string_lossy();
        let exec = if exe_str.contains(' ') {
            format!("\"{exe_str}\" tray")
        } else {
            format!("{exe_str} tray")
        };
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Agend Terminal\n\
             Exec={exec}\n\
             Terminal=false\n\
             Icon=agend-terminal\n\
             X-GNOME-Autostart-enabled=true\n",
        )
    }
}

impl Autostart for LinuxAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        let path = Self::desktop_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe()?.canonicalize()?;
        fs::write(&path, Self::render(&exe))?;
        Ok(())
    }

    fn disable(&self) -> anyhow::Result<()> {
        let path = Self::desktop_path()?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(Self::desktop_path()?.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn desktop_file_contains_required_keys() {
        let content = LinuxAutostart::render(&PathBuf::from("/usr/local/bin/agend-terminal"));
        assert!(content.contains("Type=Application"));
        assert!(content.contains("Exec=/usr/local/bin/agend-terminal tray"));
        assert!(content.contains("Terminal=false"));
        assert!(content.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn exec_line_quotes_paths_with_spaces() {
        let content = LinuxAutostart::render(&PathBuf::from("/opt/with space/agend-terminal"));
        assert!(content.contains(r#"Exec="/opt/with space/agend-terminal" tray"#));
    }
}
