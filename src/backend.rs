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
}

impl Backend {
    pub fn preset(&self) -> BackendPreset {
        match self {
            Backend::ClaudeCode => BackendPreset {
                command: "claude",
                args: &["--dangerously-skip-permissions"],
                // Claude Code shows this after loading MCP servers / tools
                ready_pattern: "Type your",
                submit_key: "\r",
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern: "ready|chat|>",
                submit_key: "\r",
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                args: &["--full-auto"],
                ready_pattern: ">|codex",
                submit_key: "\r",
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                args: &[],
                ready_pattern: "opencode|>",
                submit_key: "\r",
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: ">|gemini",
                submit_key: "\r",
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

    /// Get all known backend names.
    #[allow(dead_code)]
    pub fn all_names() -> &'static [&'static str] {
        &["claude-code", "kiro-cli", "codex", "open-code", "gemini"]
    }
}
