//! Backend presets for CLI agent tools.

use serde::{Deserialize, Serialize};

/// Known backend presets. Serde names match the actual CLI command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Backend {
    #[serde(rename = "claude", alias = "claude-code")]
    ClaudeCode,
    #[serde(rename = "kiro-cli", alias = "kiro")]
    KiroCli,
    #[serde(rename = "codex", alias = "codex-cli")]
    Codex,
    #[serde(rename = "opencode", alias = "opencode-cli")]
    OpenCode,
    #[serde(rename = "gemini", alias = "gemini-cli")]
    Gemini,
}

/// How to resume a previous session.
#[derive(Debug, Clone)]
pub enum ResumeMode {
    /// Resumes most recent session in cwd (safe if each instance has own working_dir).
    /// flag is the CLI flag to use (e.g., "--continue" for Claude, "--resume" for Kiro).
    ContinueInCwd { flag: &'static str },
    /// Use --resume-id <saved-session-id> (needs captured ID from file)
    SavedSession { flag: &'static str },
    /// Fixed args (e.g., Gemini --resume latest)
    Fixed { args: &'static [&'static str] },
    /// Not supported
    NotSupported,
}

impl ResumeMode {
    /// Get resume args. For ContinueInCwd, always returns --continue.
    /// For others, reads saved session ID from file.
    pub fn args_for(&self, home: &std::path::Path, instance_name: &str) -> Vec<String> {
        match self {
            ResumeMode::ContinueInCwd { flag } => vec![flag.to_string()],
            ResumeMode::Fixed { args } => args.iter().map(|s| s.to_string()).collect(),
            ResumeMode::NotSupported => vec![],
            _ => {
                // Read saved session ID
                let sid_file = home.join("sessions").join(format!("{instance_name}.sid"));
                let session_id = std::fs::read_to_string(&sid_file)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());

                match (self, session_id) {
                    (ResumeMode::SavedSession { flag }, Some(sid)) => vec![flag.to_string(), sid],
                    _ => vec![], // No saved session
                }
            }
        }
    }
}

/// Read session_id from an agent's statusline.json.
pub fn read_session_id(working_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(working_dir.join("statusline.json"))
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|d| d.get("session_id").and_then(|v| v.as_str()).map(String::from))
}

/// Save a captured session ID for an instance.
pub fn save_session_id(home: &std::path::Path, instance_name: &str, session_id: &str) {
    let dir = home.join("sessions");
    std::fs::create_dir_all(&dir).ok();
    let _ = std::fs::write(dir.join(format!("{instance_name}.sid")), session_id);
}

/// Preset configuration for a backend.
#[derive(Debug, Clone)]
pub struct BackendPreset {
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub ready_pattern: &'static str,
    pub submit_key: &'static str,
    /// Prefix sent before inject text to activate input field.
    pub inject_prefix: &'static str,
    /// Whether inject should use per-byte typed write (for bubbletea TUIs).
    pub typed_inject: bool,
    /// Resume strategy for this backend.
    pub resume_mode: ResumeMode,
    pub quit_command: &'static str,
    /// Patterns to auto-dismiss (trust dialogs, update prompts).
    /// Each entry: (pattern_text, key_sequence_to_send).
    pub dismiss_patterns: &'static [(&'static str, &'static [u8])],
    /// Relative path for instructions file from working dir.
    pub instructions_path: &'static str,
    /// Relative path for MCP config file from working dir.
    /// Timeout in seconds for ready detection.
    pub ready_timeout_secs: u64,
    /// Args to use when resuming is not possible (fresh start after crash).
    /// Falls back to `args` if None.
    pub fresh_args: Option<&'static [&'static str]>,
}

impl Backend {
    pub fn preset(&self) -> BackendPreset {
        match self {
            Backend::ClaudeCode => BackendPreset {
                command: "claude",
                args: &["--dangerously-skip-permissions"],
                ready_pattern: "bypass permissions|❯",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::SavedSession { flag: "--resume" },
                quit_command: "/exit",
                instructions_path: ".claude/rules/agend.md",
                ready_timeout_secs: 30,
                dismiss_patterns: &[
                    ("Yes, I trust", b"\x1b[A\x1b[A\r"),
                    ("Yes, proceed", b"\x1b[A\x1b[A\r"),
                ],
                fresh_args: None, // same as args (no resume in preset)
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "Trust All Tools active|ask a question or describe a task|/quit to exit",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::ContinueInCwd { flag: "--resume" },
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                ready_timeout_secs: 30,
                dismiss_patterns: &[
                    // Trust-all-tools confirmation: cursor defaults to "No, exit"
                    // Down moves to "Yes, I accept", Enter confirms
                    // Keys sent with per-byte delay in try_dismiss_dialog
                    ("No, exit", b"\x1b[B\r"),
                ],
                fresh_args: None, // same as args
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &[
                    "resume",
                    "--last",
                    "--dangerously-bypass-approvals-and-sandbox",
                ],
                ready_pattern: "OpenAI Codex|›",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::NotSupported,
                quit_command: "exit",
                instructions_path: "AGENTS.md",
                ready_timeout_secs: 20,
                dismiss_patterns: &[
                    // Trust directory prompt: "Yes, continue" is pre-selected → Enter
                    ("Do you trust", b"\n"),
                    // Auto-update prompt: "Please restart Codex" → Enter
                    ("Please restart", b"\n"),
                ],
                // Codex: "resume --last" → fresh start drops the resume subcommand
                fresh_args: Some(&["--dangerously-bypass-approvals-and-sandbox"]),
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "Ask anything|tab agents",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                instructions_path: "AGENTS.md",
                ready_timeout_secs: 45,
                dismiss_patterns: &[
                    ("Update Available", b"\r"),
                    ("Skip  Confirm", b"\r"),
                    ("Update Complete", b"\r"),
                    ("Please restart", b"\r"),
                ],
                fresh_args: None, // same as args (resume is in resume_mode, not args)
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: "Type your message|YOLO",
                submit_key: "\n\r",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::Fixed {
                    args: &["--resume", "latest"],
                },
                quit_command: "/exit",
                instructions_path: "GEMINI.md",
                ready_timeout_secs: 20,
                // Auto-approve: MCP tools ("3" = all server tools for session),
                // shell commands ("2" = allow for session)
                dismiss_patterns: &[
                    ("Allow execution of MCP tool", b"3\n"),
                    ("Allow execution of:", b"2\n"),
                ],
                fresh_args: None, // same as args (resume is in resume_mode, not args)
            },
        }
    }

    /// Try to detect backend from a command string (matches basename, not full path).
    pub fn from_command(command: &str) -> Option<Backend> {
        let basename = std::path::Path::new(command)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(command)
            .to_lowercase();
        if basename == "claude" || basename.starts_with("claude-") {
            Some(Backend::ClaudeCode)
        } else if basename == "kiro-cli" || basename == "kiro" || basename.starts_with("kiro-") {
            Some(Backend::KiroCli)
        } else if basename == "codex" || basename.starts_with("codex-") {
            Some(Backend::Codex)
        } else if basename == "opencode" || basename.starts_with("opencode-") {
            Some(Backend::OpenCode)
        } else if basename == "gemini" || basename.starts_with("gemini-") {
            Some(Backend::Gemini)
        } else {
            None
        }
    }

    /// Get all backends.
    pub fn all() -> &'static [Backend] {
        &[
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
        ]
    }

    /// Format a `--model` value for this backend.
    /// OpenCode requires `provider/model` format — auto-prefixes `anthropic/`
    /// if the value doesn't already contain a `/`.
    pub fn format_model_arg(&self, model: &str) -> String {
        if matches!(self, Backend::OpenCode) && !model.contains('/') {
            format!("anthropic/{model}")
        } else {
            model.to_string()
        }
    }

    /// Display name matching the CLI command.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::ClaudeCode => "claude",
            Backend::KiroCli => "kiro-cli",
            Backend::Codex => "codex",
            Backend::OpenCode => "opencode",
            Backend::Gemini => "gemini",
        }
    }

    /// Check if the backend binary is in PATH.
    pub fn is_installed(&self) -> bool {
        let preset = self.preset();
        std::process::Command::new("which")
            .arg(preset.command)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Get installed version via --version. Returns None if not installed.
    pub fn get_version(&self) -> Option<String> {
        let preset = self.preset();
        let output = std::process::Command::new(preset.command)
            .arg("--version")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            return None;
        }
        // Extract version number from various formats
        // "2.1.89 (Claude Code)" → "2.1.89"
        // "kiro-cli 1.29.6" → "1.29.6"
        // "codex-cli 0.118.0" → "0.118.0"
        // "1.3.10" → "1.3.10"
        // "0.37.1" → "0.37.1"
        let version = text
            .split_whitespace()
            .find(|w| {
                w.chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            })
            .unwrap_or(&text)
            .trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.')
            .to_string();
        Some(version)
    }

    /// Version used when patterns were last calibrated.
    pub fn calibrated_version(&self) -> &'static str {
        match self {
            Backend::ClaudeCode => "2.1.89",
            Backend::KiroCli => "1.29.6",
            Backend::Codex => "0.118.0",
            Backend::OpenCode => "1.4.0",
            Backend::Gemini => "0.37.1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_command_detection() {
        assert_eq!(Backend::from_command("claude"), Some(Backend::ClaudeCode));
        assert_eq!(Backend::from_command("kiro-cli"), Some(Backend::KiroCli));
        assert_eq!(Backend::from_command("codex"), Some(Backend::Codex));
        assert_eq!(Backend::from_command("opencode"), Some(Backend::OpenCode));
        assert_eq!(Backend::from_command("gemini"), Some(Backend::Gemini));
        // Case insensitive
        assert_eq!(Backend::from_command("Claude"), Some(Backend::ClaudeCode));
        assert_eq!(
            Backend::from_command("/usr/bin/claude"),
            Some(Backend::ClaudeCode)
        );
    }

    #[test]
    fn from_command_unknown() {
        assert_eq!(Backend::from_command("unknown-tool"), None);
        assert_eq!(Backend::from_command("vim"), None);
        assert_eq!(Backend::from_command(""), None);
    }

    #[test]
    fn preset_args_correct() {
        let claude = Backend::ClaudeCode.preset();
        assert!(claude.args.contains(&"--dangerously-skip-permissions"));
        assert_eq!(claude.command, "claude");

        let kiro = Backend::KiroCli.preset();
        assert!(kiro.args.contains(&"chat"));
        assert!(kiro.args.contains(&"--trust-all-tools"));

        let gemini = Backend::Gemini.preset();
        assert!(gemini.args.contains(&"--yolo"));
    }

    #[test]
    fn resume_mode_types() {
        let claude = Backend::ClaudeCode.preset();
        assert!(matches!(
            claude.resume_mode,
            ResumeMode::SavedSession { .. }
        ));

        let kiro = Backend::KiroCli.preset();
        assert!(matches!(kiro.resume_mode, ResumeMode::ContinueInCwd { .. }));

        let codex = Backend::Codex.preset();
        assert!(matches!(codex.resume_mode, ResumeMode::NotSupported));

        let gemini = Backend::Gemini.preset();
        assert!(matches!(gemini.resume_mode, ResumeMode::Fixed { .. }));

        let opencode = Backend::OpenCode.preset();
        assert!(matches!(
            opencode.resume_mode,
            ResumeMode::ContinueInCwd { .. }
        ));
    }

    #[test]
    fn backend_name_roundtrip() {
        assert_eq!(Backend::ClaudeCode.name(), "claude");
        assert_eq!(Backend::KiroCli.name(), "kiro-cli");
        assert_eq!(Backend::Codex.name(), "codex");
        assert_eq!(Backend::OpenCode.name(), "opencode");
        assert_eq!(Backend::Gemini.name(), "gemini");
    }

    #[test]
    fn all_backends_returns_five() {
        assert_eq!(Backend::all().len(), 5);
    }

    #[test]
    fn resume_mode_continue_in_cwd_args() {
        let mode = ResumeMode::ContinueInCwd { flag: "--continue" };
        let args = mode.args_for(std::path::Path::new("/tmp"), "test");
        assert_eq!(args, vec!["--continue".to_string()]);
    }

    #[test]
    fn resume_mode_fixed_args() {
        let mode = ResumeMode::Fixed {
            args: &["--resume", "latest"],
        };
        let args = mode.args_for(std::path::Path::new("/tmp"), "test");
        assert_eq!(args, vec!["--resume".to_string(), "latest".to_string()]);
    }

    #[test]
    fn resume_mode_not_supported_args() {
        let mode = ResumeMode::NotSupported;
        let args = mode.args_for(std::path::Path::new("/tmp"), "test");
        assert!(args.is_empty());
    }

    #[test]
    fn preset_ready_pattern_nonempty() {
        for backend in Backend::all() {
            let preset = backend.preset();
            assert!(
                !preset.ready_pattern.is_empty(),
                "Backend {:?} has empty ready_pattern",
                backend
            );
        }
    }

    #[test]
    fn calibrated_version_nonempty() {
        for backend in Backend::all() {
            let version = backend.calibrated_version();
            assert!(!version.is_empty());
            // Should look like a semver: at least one dot
            assert!(
                version.contains('.'),
                "Version {version} for {:?} doesn't look like semver",
                backend
            );
        }
    }

    #[test]
    fn format_model_arg_opencode_adds_prefix() {
        assert_eq!(Backend::OpenCode.format_model_arg("opus"), "anthropic/opus");
        assert_eq!(
            Backend::OpenCode.format_model_arg("anthropic/opus"),
            "anthropic/opus"
        );
        assert_eq!(
            Backend::OpenCode.format_model_arg("openai/gpt-4"),
            "openai/gpt-4"
        );
    }

    #[test]
    fn format_model_arg_other_backends_passthrough() {
        assert_eq!(Backend::ClaudeCode.format_model_arg("opus"), "opus");
        assert_eq!(Backend::Gemini.format_model_arg("gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(Backend::Codex.format_model_arg("o3"), "o3");
    }
}
