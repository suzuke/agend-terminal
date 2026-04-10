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

/// Preset configuration for a backend.
#[derive(Debug, Clone)]
pub struct BackendPreset {
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub ready_pattern: &'static str,
    pub submit_key: &'static str,
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
                ready_pattern: "bypass permissions|❯", // [実測]
                submit_key: "\r",
                quit_command: "/exit",
                instructions_path: ".claude/rules/agend.md",
                mcp_config_path: ".claude/settings.json",
                ready_timeout_secs: 30,
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "All tools are now trusted|!>", // [実測]
                submit_key: "\r",
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                mcp_config_path: ".kiro/settings/mcp.json",
                ready_timeout_secs: 30,
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &["--full-auto"],
                ready_pattern: "OpenAI Codex|›", // [実測]
                submit_key: "\r",
                quit_command: "exit", // Ctrl+C based
                instructions_path: "AGENTS.md",
                mcp_config_path: "opencode.json", // codex doesn't have file-based MCP config
                ready_timeout_secs: 20,
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "Ask anything|tab agents", // [実測]
                submit_key: "\n", // [実測] OpenCode uses LF (0x0A) not CR
                quit_command: "/exit",
                instructions_path: "instructions/agend.md",
                mcp_config_path: "opencode.json",
                ready_timeout_secs: 45, // May be blocked by update dialog
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: "Type your message|YOLO", // [実測]
                submit_key: "\n\r", // [実測 v1] Gemini needs LF+CR
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
