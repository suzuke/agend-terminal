//! Sprint 57 Wave 3 PR-3 (#548 Phase 3) — cross-platform service helper.
//!
//! `agend-terminal service install / uninstall / status` registers the
//! daemon with the host OS service manager so the OS owns lifecycle
//! (auto-start at login, restart on crash). Per Q3, the daemon does
//! NOT supervise itself — this helper is the integration point that
//! wires the OS supervisor in.
//!
//! ## Platforms
//!
//! - **macOS**: `launchd` user-level LaunchAgent at
//!   `~/Library/LaunchAgents/com.agend-terminal.daemon.plist`.
//!   Loaded via `launchctl load -w` / unloaded via `launchctl unload -w`.
//! - **Linux**: `systemd` user-level unit at
//!   `~/.config/systemd/user/agend-terminal-daemon.service`.
//!   Activated via `systemctl --user enable --now` / deactivated via
//!   `systemctl --user disable --now`.
//! - **Windows**: Task Scheduler at-logon task `\AgendTerminalDaemon`.
//!   Registered via `schtasks /Create /XML` / removed via
//!   `schtasks /Delete /F`.
//!
//! All three are USER-LEVEL — no admin / root required.
//!
//! ## Idempotency
//!
//! - `install` on an existing install: regenerates the template
//!   (operator-friendly: picks up new daemon binary path / fresh AGEND_HOME),
//!   re-registers with the service manager (which itself is idempotent
//!   on the per-platform helpers), reports success.
//! - `uninstall` on a missing install: no-op success.
//! - `status` returns `NotInstalled` when no template file is present;
//!   `Running` / `Stopped` otherwise based on per-platform query.

#![allow(dead_code)] // module is wired through Commands::Service in main.rs (clippy doesn't see cross-bin usage)

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Service-manager status as returned by the per-platform query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Service is registered AND currently running.
    Running,
    /// Service is registered but not currently running.
    Stopped,
    /// No service registration found (template file missing).
    NotInstalled,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::NotInstalled => "not_installed",
        }
    }
}

/// Canonical service label / unit name across platforms (where the
/// platform allows a label string). Windows uses `\AgendTerminalDaemon`
/// (the leading backslash is required by `schtasks /TN`).
pub const SERVICE_LABEL: &str = "com.agend-terminal.daemon";
pub const SYSTEMD_UNIT: &str = "agend-terminal-daemon.service";
pub const WINDOWS_TASK: &str = "AgendTerminalDaemon";

/// Embedded template strings (substituted at install time). Each
/// `__PLACEHOLDER__` is replaced with the resolved value via
/// `apply_substitutions`. Per the Cargo.toml `include` list, these
/// assets ship in the published crate so `cargo publish` verify
/// builds pick them up cleanly.
pub const LAUNCHD_TEMPLATE: &str = include_str!("../../assets/service/launchd.plist.template");
pub const SYSTEMD_TEMPLATE: &str = include_str!("../../assets/service/systemd.service.template");
pub const WINDOWS_TEMPLATE: &str = include_str!("../../assets/service/scheduler.task.xml.template");

/// Substitute `__PLACEHOLDER__` tokens in a template body. Pure
/// helper — caller is responsible for format-appropriate escaping
/// of the substitution values (see `xml_escape` for launchd/plist
/// and Task Scheduler XML, and `systemd_quote` for systemd
/// ExecStart tokenization-safety). The Wave 3 PR-3 r1 review
/// (Tier-2 Pass 2) caught a Class-A cross-platform bug where this
/// helper was being called with raw paths, producing malformed XML
/// or mis-tokenized systemd ExecStart on values containing `&`,
/// `<`, `>`, `"`, `'`, or whitespace.
pub fn apply_substitutions(template: &str, substitutions: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (placeholder, value) in substitutions {
        out = out.replace(placeholder, value);
    }
    out
}

/// Sprint 57 Wave 3 PR-3 r2 (#548 Phase 3, Tier-2 Pass 2 fixup):
/// XML entity-escape a text-node value for inclusion in plist /
/// Task Scheduler XML templates. Order matters — `&` must be
/// substituted FIRST to avoid double-escaping subsequent entities.
///
/// Covers the full attribute-value-safe set: `& < > " '`. Values
/// inside `<string>...</string>` and `<UserId>...</UserId>` text
/// nodes only need `&`, `<`, `>` strictly, but escaping the quote
/// chars too keeps the helper safe for both text and attribute
/// contexts (the templates use both).
pub fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Sprint 57 Wave 3 PR-3 r2 (#548 Phase 3, Tier-2 Pass 2 fixup):
/// systemd ExecStart-safe quoting for executable / argument paths.
/// Per systemd.exec(5) "Command lines": values containing
/// whitespace or special chars must be `"`-quoted, with internal
/// `"` escaped as `\"` and `\` escaped as `\\`.
///
/// Strategy: if the value contains nothing requiring quoting
/// (alphanumeric + `/_.-`), return as-is. Otherwise wrap in `"..."`
/// with `\` and `"` backslash-escaped inside.
pub fn systemd_quote(value: &str) -> String {
    let needs_quoting = value
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-')));
    if !needs_quoting {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 4);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str(r#"\""#),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Resolve the absolute path to the currently-running
/// `agend-terminal` binary. The service manager records THIS path
/// in the template; later daemon spawns from the OS supervisor
/// land at the same binary the operator originally installed.
fn current_executable() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| format!("could not resolve current_exe for service install: {e}"))
}

/// Install the daemon as a user-level OS service. Idempotent:
/// re-running regenerates the template + re-registers with the
/// platform service manager. Returns the path to the registered
/// service-manager artifact (plist / unit / task XML) on success.
#[allow(clippy::needless_return)] // mutually-exclusive cfg blocks need explicit return
pub fn install(home: &Path) -> Result<PathBuf, String> {
    let exe = current_executable()?;
    #[cfg(target_os = "macos")]
    {
        return macos::install(home, &exe);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::install(home, &exe);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::install(home, &exe);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (home, exe);
        return Err(
            "agend-terminal service install: this platform is not supported. \
             Supported: macOS (launchd), Linux (systemd user), Windows (Task Scheduler)."
                .into(),
        );
    }
}

/// Uninstall the daemon from the OS service manager + remove the
/// template file. Idempotent on missing install (returns Ok with
/// `was_installed = false`).
#[allow(clippy::needless_return)]
pub fn uninstall(home: &Path) -> Result<UninstallOutcome, String> {
    #[cfg(target_os = "macos")]
    {
        return macos::uninstall(home);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::uninstall(home);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::uninstall(home);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home;
        return Err("agend-terminal service uninstall: this platform is not supported.".into());
    }
}

/// Query the service manager for the current state of the
/// registered daemon. Returns `NotInstalled` if no template /
/// registration is found, `Running` / `Stopped` otherwise.
#[allow(clippy::needless_return)]
pub fn status(home: &Path) -> Result<ServiceState, String> {
    #[cfg(target_os = "macos")]
    {
        return macos::status(home);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::status(home);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::status(home);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = home;
        return Err("agend-terminal service status: this platform is not supported.".into());
    }
}

/// Outcome of `uninstall` — distinguishes "nothing to remove"
/// (`was_installed = false`, exit 0) from "removed something"
/// (`was_installed = true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UninstallOutcome {
    pub was_installed: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn assets_service_templates_exist() {
        // Pin the templates at compile time via include_str!. If a
        // template file goes missing, the build itself fails (this
        // test is a belt-and-braces sanity pin).
        assert!(LAUNCHD_TEMPLATE.contains("<plist"));
        assert!(LAUNCHD_TEMPLATE.contains("__EXECUTABLE__"));
        assert!(SYSTEMD_TEMPLATE.contains("[Service]"));
        assert!(SYSTEMD_TEMPLATE.contains("__EXECUTABLE__"));
        assert!(WINDOWS_TEMPLATE.contains("<Task"));
        assert!(WINDOWS_TEMPLATE.contains("__EXECUTABLE__"));
    }

    #[test]
    fn apply_substitutions_replaces_all_placeholders() {
        let template = "exe=__EXECUTABLE__ home=__HOME__ exe2=__EXECUTABLE__";
        let resolved = apply_substitutions(
            template,
            &[
                ("__EXECUTABLE__", "/opt/bin/agend-terminal"),
                ("__HOME__", "/Users/dev/.agend"),
            ],
        );
        assert_eq!(
            resolved,
            "exe=/opt/bin/agend-terminal home=/Users/dev/.agend exe2=/opt/bin/agend-terminal"
        );
    }

    #[test]
    fn apply_substitutions_no_placeholders_is_identity() {
        let template = "no placeholders here";
        let resolved = apply_substitutions(template, &[("__X__", "y")]);
        assert_eq!(resolved, template);
    }

    #[test]
    fn service_state_string_taxonomy() {
        // Pin the string identifiers downstream consumers grep against.
        assert_eq!(ServiceState::Running.as_str(), "running");
        assert_eq!(ServiceState::Stopped.as_str(), "stopped");
        assert_eq!(ServiceState::NotInstalled.as_str(), "not_installed");
    }

    #[test]
    fn launchd_template_carries_keepalive_and_runatload() {
        // Pin the lifecycle policy: KeepAlive (auto-restart on crash)
        // + RunAtLoad (start at login). These are the operator-facing
        // contract — disabling them would silently change behaviour.
        assert!(LAUNCHD_TEMPLATE.contains("<key>KeepAlive</key>"));
        assert!(LAUNCHD_TEMPLATE.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn systemd_template_carries_restart_on_failure_and_wantedby_default() {
        // Pin: Restart=on-failure (auto-restart on crash matches Q3
        // semantic — daemon doesn't self-restart, systemd does).
        // WantedBy=default.target (start at user login).
        assert!(SYSTEMD_TEMPLATE.contains("Restart=on-failure"));
        assert!(SYSTEMD_TEMPLATE.contains("WantedBy=default.target"));
        assert!(SYSTEMD_TEMPLATE.contains("KillSignal=SIGTERM"));
    }

    #[test]
    fn windows_template_carries_logon_trigger_and_least_privilege() {
        // Pin: LogonTrigger (start at user logon, no admin).
        // RunLevel LeastPrivilege (no admin escalation).
        assert!(WINDOWS_TEMPLATE.contains("<LogonTrigger>"));
        assert!(WINDOWS_TEMPLATE.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        // Restart-on-failure parity with systemd's on-failure semantic.
        assert!(WINDOWS_TEMPLATE.contains("<RestartOnFailure>"));
    }

    // -----------------------------------------------------------------
    // Sprint 57 Wave 3 PR-3 r2 (#548 Phase 3, Tier-2 Pass 2 fixup) —
    // format-aware escaping helpers.
    // -----------------------------------------------------------------

    #[test]
    fn xml_escape_handles_all_five_special_chars_in_correct_order() {
        // `&` MUST be replaced FIRST or subsequent entity replacements
        // double-escape (e.g. `<` → `&lt;` becomes `&amp;lt;`).
        // The implementation iterates char-by-char so order is implicit
        // — pin the round-trip behaviour explicitly.
        let raw = "a&b<c>d\"e'f";
        let escaped = xml_escape(raw);
        assert_eq!(escaped, "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    #[test]
    fn xml_escape_no_op_on_safe_string() {
        // Plain ASCII alnum + path chars round-trip unchanged.
        let safe = "/Users/dev/.agend/agend-terminal";
        assert_eq!(xml_escape(safe), safe);
    }

    #[test]
    fn xml_escape_preserves_unicode() {
        // Non-ASCII chars are passed through unchanged. macOS / Windows
        // user paths can contain CJK / other Unicode and these are
        // valid XML content (UTF-8 / UTF-16 native).
        let unicode = "/Users/開發/.agend";
        assert_eq!(xml_escape(unicode), unicode);
    }

    #[test]
    fn systemd_quote_no_op_on_safe_path() {
        // Alphanumeric + `/_.-` paths don't need quoting per
        // systemd.exec(5).
        let safe = "/usr/local/bin/agend-terminal";
        assert_eq!(systemd_quote(safe), safe);
    }

    #[test]
    fn systemd_quote_wraps_path_with_spaces() {
        // The Class-A bug: a path with spaces splits into multiple
        // tokens at ExecStart= time. Wrap in `"..."` to preserve as
        // single token.
        let with_space = "/Users/Test User/.cargo/bin/agend-terminal";
        let quoted = systemd_quote(with_space);
        assert_eq!(quoted, r#""/Users/Test User/.cargo/bin/agend-terminal""#);
    }

    #[test]
    fn systemd_quote_escapes_internal_quotes_and_backslashes() {
        // Belt-and-braces: a path containing literal `"` or `\` (rare
        // on Unix but legal) gets the chars escaped inside the wrapping
        // quotes per systemd's exec quoting rules.
        let weird = r#"/path with "quote" and\back"#;
        let quoted = systemd_quote(weird);
        assert_eq!(quoted, r#""/path with \"quote\" and\\back""#);
    }

    #[test]
    fn systemd_quote_handles_hyphen_underscore_dot_in_basename() {
        // Common conventional filename chars shouldn't trigger
        // wrapping — keeps systemd unit files readable when no
        // special handling is needed.
        let conventional = "/usr/local/bin/agend-terminal_helper.v2";
        assert_eq!(systemd_quote(conventional), conventional);
    }

    #[test]
    fn xml_escape_under_substitution_produces_well_formed_plist_fragment() {
        // Empirical regression-proof: the path that crashed pre-r2
        // (`__EXECUTABLE__` containing `&`) round-trips through
        // xml_escape + apply_substitutions and lands as a valid XML
        // text-node value.
        let template = "<string>__EXECUTABLE__</string>";
        let raw = "/path/with&ampersand/agend-terminal";
        let escaped = xml_escape(raw);
        let resolved = apply_substitutions(template, &[("__EXECUTABLE__", escaped.as_str())]);
        assert_eq!(
            resolved,
            "<string>/path/with&amp;ampersand/agend-terminal</string>"
        );
        // Negative pin: raw `&` would be invalid XML.
        assert!(!resolved.contains("with&ampersand/"));
    }

    #[test]
    fn systemd_quote_under_substitution_preserves_argv_safety() {
        // Empirical regression-proof: the path that crashed pre-r2
        // (whitespace in `__EXECUTABLE__`) round-trips through
        // systemd_quote + apply_substitutions and produces an
        // ExecStart= line that systemd will tokenize as a SINGLE
        // executable + the literal `start --foreground` args.
        let template = "ExecStart=__EXECUTABLE__ start --foreground";
        let raw = "/Users/Test User/.cargo/bin/agend-terminal";
        let quoted = systemd_quote(raw);
        let resolved = apply_substitutions(template, &[("__EXECUTABLE__", quoted.as_str())]);
        assert_eq!(
            resolved,
            r#"ExecStart="/Users/Test User/.cargo/bin/agend-terminal" start --foreground"#
        );
    }
}
