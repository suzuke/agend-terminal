//! macOS autostart via a per-user LaunchAgent.
//!
//! Writes `~/Library/LaunchAgents/io.github.suzuke.agend-terminal.plist`
//! and registers it with `launchctl bootstrap gui/$UID`. Disable removes
//! the plist and calls `bootout`.
//!
//! Label choice is locked by PLAN — a future signed `.app` bundle must
//! reuse it as `CFBundleIdentifier` so Login Items entries don't split.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use super::Autostart;

/// LaunchAgent label + file stem. Reverse-DNS form documented in PLAN.
const LABEL: &str = "io.github.suzuke.agend-terminal";

pub struct MacAutostart {
    home: PathBuf,
}

impl MacAutostart {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    fn plist_path() -> anyhow::Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME"))?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    /// `gui/<uid>` — launchd's modern per-user domain. `load` is deprecated;
    /// `bootstrap` needs this explicit domain target.
    fn launchctl_domain() -> String {
        // SAFETY: getuid() is thread-safe and always returns a valid uid.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}")
    }

    fn render_plist(&self, exe: &Path) -> String {
        let log_path = self.home.join("tray.log");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key>             <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>tray</string>
  </array>
  <key>RunAtLoad</key>         <true/>
  <key>KeepAlive</key>         <dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key>       <string>Interactive</string>
  <key>StandardOutPath</key>   <string>{log}</string>
  <key>StandardErrorPath</key> <string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict><key>AGEND_HOME</key>  <string>{home}</string></dict>
</dict></plist>
"#,
            label = LABEL,
            exe = xml_escape(&exe.to_string_lossy()),
            log = xml_escape(&log_path.to_string_lossy()),
            home = xml_escape(&self.home.to_string_lossy()),
        )
    }
}

/// XML-escape the five characters that matter inside `<string>` bodies.
/// Paths seldom contain these, but a user with `&` in their home path
/// would otherwise produce a malformed plist.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl Autostart for MacAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        let path = Self::plist_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let exe = std::env::current_exe()?.canonicalize()?;
        fs::write(&path, self.render_plist(&exe))?;

        // bootstrap fails if the label is already loaded; bootout first
        // so `enable()` stays idempotent. Ignore bootout's exit status —
        // "not loaded" is the common and acceptable case.
        let domain = Self::launchctl_domain();
        let target = format!("{domain}/{LABEL}");
        let _ = Command::new("launchctl")
            .args(["bootout", &target])
            .output();
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&path)
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootstrap {domain} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn disable(&self) -> anyhow::Result<()> {
        let path = Self::plist_path()?;
        let target = format!("{}/{LABEL}", Self::launchctl_domain());
        // Best effort — if not loaded, bootout returns non-zero; we still
        // want to remove the file below.
        let _ = Command::new("launchctl")
            .args(["bootout", &target])
            .output();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        Ok(Self::plist_path()?.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_renders_well_formed_xml_with_required_keys() {
        let auto = MacAutostart::new(PathBuf::from("/tmp/agend-home"));
        let exe = PathBuf::from("/opt/homebrew/bin/agend-terminal");
        let plist = auto.render_plist(&exe);

        assert!(plist.contains("<string>io.github.suzuke.agend-terminal</string>"));
        assert!(plist.contains("<string>/opt/homebrew/bin/agend-terminal</string>"));
        assert!(plist.contains("<string>tray</string>"));
        assert!(plist.contains("<string>/tmp/agend-home/tray.log</string>"));
        assert!(plist.contains("<key>AGEND_HOME</key>"));
        assert!(plist.contains("<string>/tmp/agend-home</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn xml_escape_handles_the_five_classes() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn plist_escapes_paths_with_xml_special_chars() {
        let auto = MacAutostart::new(PathBuf::from("/tmp/a&b"));
        let exe = PathBuf::from("/tmp/<weird>/agend-terminal");
        let plist = auto.render_plist(&exe);
        assert!(plist.contains("/tmp/a&amp;b"));
        assert!(plist.contains("/tmp/&lt;weird&gt;/agend-terminal"));
    }
}
