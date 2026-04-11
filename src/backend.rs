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
    /// Use --resume <session-id> (Claude Code)
    SessionId { flag: &'static str },
    /// Use --resume-id <session-id> (Kiro)
    ResumeId { flag: &'static str },
    /// Use --session <session-id> (OpenCode)
    SessionFlag { flag: &'static str },
    /// Index-based, not safe for multi-instance (Gemini)
    IndexBased,
    /// Not supported (Codex — needs different command structure)
    NotSupported,
}

impl ResumeMode {
    /// Generate resume args for a given instance name.
    /// Uses a deterministic UUID derived from instance name for session ID.
    pub fn args_for(&self, instance_name: &str) -> Vec<String> {
        let session_id = deterministic_session_id(instance_name);
        match self {
            ResumeMode::SessionId { flag } => vec![flag.to_string(), session_id],
            ResumeMode::ResumeId { flag } => vec![flag.to_string(), session_id],
            ResumeMode::SessionFlag { flag } => vec![flag.to_string(), session_id],
            ResumeMode::IndexBased => vec![], // Can't safely resume with index
            ResumeMode::NotSupported => vec![],
        }
    }
}

/// Generate a deterministic UUID v5-like ID from instance name.
/// Same name always produces same ID → stable across restarts.
fn deterministic_session_id(name: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    "agend-terminal".hash(&mut h);
    name.hash(&mut h);
    let hash = h.finish();
    // Format as UUID-like string
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hash >> 32) as u32,
        (hash >> 16) as u16 & 0xffff,
        hash as u16 & 0x0fff | 0x4000, // version 4
        (hash >> 48) as u16 & 0x3fff | 0x8000, // variant
        hash & 0xffffffffffff,
    )
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
                resume_mode: ResumeMode::SessionId { flag: "--resume" },
                quit_command: "/exit",
                instructions_path: ".claude/rules/agend.md",
                mcp_config_path: ".claude/settings.local.json",
                ready_timeout_secs: 30,
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "All tools are now trusted|!>",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::ResumeId { flag: "--resume-id" },
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                mcp_config_path: ".kiro/settings/mcp.json",
                ready_timeout_secs: 30,
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &["--full-auto"],
                ready_pattern: "OpenAI Codex|›",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::NotSupported,
                quit_command: "exit",
                instructions_path: "AGENTS.md",
                mcp_config_path: "opencode.json",
                ready_timeout_secs: 20,
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "Ask anything|tab agents",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::SessionFlag { flag: "--session" },
                quit_command: "/exit",
                instructions_path: "instructions/agend.md",
                mcp_config_path: "opencode.json",
                ready_timeout_secs: 45,
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: "Type your message|YOLO",
                submit_key: "\n\r",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::IndexBased, // Gemini uses index, not ID — can't safely resume
                quit_command: "/exit",
                instructions_path: "GEMINI.md",
                mcp_config_path: ".gemini/settings.json",
                ready_timeout_secs: 20,
            },
        }
    }

    /// Try to detect backend from a command string.
    #[allow(dead_code)]
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

    /// Get all known backend names (kebab-case).
    #[allow(dead_code)]
    pub fn all_names() -> &'static [&'static str] {
        &["claude-code", "kiro-cli", "codex", "open-code", "gemini"]
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
            .find(|w| w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
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
