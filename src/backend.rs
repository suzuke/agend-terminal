//! Backend presets for CLI agent tools.

use serde::{Deserialize, Serialize};

/// Known backend presets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    ClaudeCode,
    KiroCli,
    Codex,
    OpenCode,
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
    pub mcp_config_path: &'static str,
    /// Timeout in seconds for ready detection.
    pub ready_timeout_secs: u64,
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
                mcp_config_path: ".claude/settings.local.json",
                ready_timeout_secs: 30,
                dismiss_patterns: &[
                    ("Yes, I trust", b"\x1b[A\x1b[A\r"),
                    ("Yes, proceed", b"\x1b[A\x1b[A\r"),
                ],
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "All tools are now trusted|!>",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::ContinueInCwd { flag: "--resume" },
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                mcp_config_path: ".kiro/settings/mcp.json",
                ready_timeout_secs: 30,
                dismiss_patterns: &[],
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
                mcp_config_path: "opencode.json",
                ready_timeout_secs: 20,
                dismiss_patterns: &[],
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
                instructions_path: "instructions/agend.md",
                mcp_config_path: "opencode.json",
                ready_timeout_secs: 45,
                dismiss_patterns: &[
                    ("Update Available", b"\r"),
                    ("Skip  Confirm", b"\r"),
                    ("Update Complete", b"\r"),
                    ("Please restart", b"\r"),
                ],
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
                mcp_config_path: ".gemini/settings.json",
                ready_timeout_secs: 20,
                dismiss_patterns: &[],
            },
        }
    }

    /// Try to detect backend from a command string.
    pub fn from_command(command: &str) -> Option<Backend> {
        let cmd = command.to_lowercase();
        if cmd.contains("claude") {
            Some(Backend::ClaudeCode)
        } else if cmd.contains("kiro") {
            Some(Backend::KiroCli)
        } else if cmd.contains("codex") {
            Some(Backend::Codex)
        } else if cmd.contains("opencode") {
            Some(Backend::OpenCode)
        } else if cmd.contains("gemini") {
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

    /// Kebab-case name for this backend.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::ClaudeCode => "claude-code",
            Backend::KiroCli => "kiro-cli",
            Backend::Codex => "codex",
            Backend::OpenCode => "open-code",
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
