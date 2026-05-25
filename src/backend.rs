//! Backend presets for CLI agent tools.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Everything that can run in a pane. Serialized as a bare YAML/JSON string:
/// known presets by their canonical name (`claude`, `kiro-cli`, ...), generic
/// shells as `shell`, and anything else round-trips as the literal command
/// string.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    ClaudeCode,
    KiroCli,
    Codex,
    OpenCode,
    Gemini,
    /// Google Antigravity CLI (`agy`). Gemini CLI's official successor —
    /// Google announced gemini-cli sunset for AI Pro / AI Ultra / free-tier
    /// consumers on 2026-06-18. Shares the same agent engine as the
    /// Antigravity 2.0 desktop app. Standard `mcpServers` schema + project-
    /// local config at `<workdir>/.antigravitycli/mcp_config.json`. Added in
    /// #987.
    Agy,
    /// Generic shell (bash/zsh/sh). No preset wiring — inject/ready/resume are
    /// all no-ops. Command defaults to `$SHELL` or the platform default
    /// (`/bin/bash` on Unix, `cmd.exe` on Windows).
    Shell,
    /// Arbitrary executable path. No preset behavior; the stored string is the
    /// command to spawn verbatim.
    Raw(String),
}

impl Backend {
    /// #919: should this backend's PTY output be checked against the
    /// red-ANSI anchor for HIGH_FP state-detection patterns?
    ///
    /// True for backends that consistently emit red SGR escapes
    /// (`\x1b[31m` / `\x1b[91m`) when rendering errors — ClaudeCode,
    /// Codex, OpenCode, Gemini. False for Shell + Raw — generic
    /// shells don't have a uniform color convention and arbitrary
    /// commands may render errors uncolored.
    ///
    /// When false, the HIGH_FP gate falls back to fail-open: pattern
    /// match → transition fires (pre-#919 behavior).
    pub fn should_anchor_on_red(&self) -> bool {
        match self {
            Backend::ClaudeCode
            | Backend::KiroCli
            | Backend::Codex
            | Backend::OpenCode
            | Backend::Gemini
            | Backend::Agy => true,
            Backend::Shell | Backend::Raw(_) => false,
        }
    }

    /// Parse a bare string form (yaml scalar or MCP tool argument).
    /// Known names → preset variants; shell aliases → [`Backend::Shell`];
    /// anything else becomes [`Backend::Raw`].
    pub fn parse_str(s: &str) -> Backend {
        let trimmed = s.trim();
        let lower = trimmed.to_lowercase();
        match lower.as_str() {
            "claude" | "claude-code" => Backend::ClaudeCode,
            "kiro-cli" | "kiro" => Backend::KiroCli,
            "codex" | "codex-cli" => Backend::Codex,
            "opencode" | "opencode-cli" => Backend::OpenCode,
            "gemini" | "gemini-cli" => Backend::Gemini,
            "agy" | "antigravity" | "antigravity-cli" => Backend::Agy,
            "shell" | "bash" | "zsh" | "sh" => Backend::Shell,
            _ => Backend::Raw(trimmed.to_string()),
        }
    }

    /// Canonical string form (inverse of [`parse_str`]). For [`Backend::Raw`]
    /// returns the stored command verbatim.
    pub fn as_str(&self) -> &str {
        match self {
            Backend::ClaudeCode => "claude",
            Backend::KiroCli => "kiro-cli",
            Backend::Codex => "codex",
            Backend::OpenCode => "opencode",
            Backend::Gemini => "gemini",
            // #995: display name is the product short form `antigravity-cli`,
            // not the binary `agy`. Binary command remains `agy` (preset.command);
            // parse_str still accepts the "agy" alias for backward-compat with
            // any fleet.yaml entries written before #995.
            Backend::Agy => "antigravity-cli",
            Backend::Shell => "shell",
            Backend::Raw(s) => s.as_str(),
        }
    }

    /// Actual command path to spawn. For [`Backend::Shell`] resolves to
    /// `$SHELL` (falling back to the platform default — `/bin/bash` on Unix,
    /// `cmd.exe` on Windows). For [`Backend::Raw`] returns the literal stored
    /// path. For presets returns the static preset command.
    pub fn command_string(&self) -> String {
        match self {
            Backend::ClaudeCode
            | Backend::KiroCli
            | Backend::Codex
            | Backend::OpenCode
            | Backend::Gemini
            | Backend::Agy => self.preset().command.to_string(),
            Backend::Shell => {
                std::env::var("SHELL").unwrap_or_else(|_| crate::default_shell().to_string())
            }
            Backend::Raw(path) => path.clone(),
        }
    }
}

impl Serialize for Backend {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Backend {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Backend::parse_str(&s))
    }
}

/// Whether a spawn starts a fresh session or resumes the previous one.
///
/// Selects which preset args `preset_spawn_args` returns: `Fresh` uses
/// `fresh_args` (falling back to `args`); `Resume` uses `args` plus
/// `resume_mode.args_for()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SpawnMode {
    #[default]
    Fresh,
    Resume,
}

/// How to resume a previous session.
#[derive(Debug, Clone)]
pub enum ResumeMode {
    /// Resumes most recent session in cwd (safe when each instance has its own
    /// working_dir — the fleet's auto-worktree ensures this for git repos).
    /// `flag` is the CLI flag to use (e.g., `--continue` for Claude/OpenCode,
    /// `--resume` for Kiro).
    ContinueInCwd { flag: &'static str },
    /// Fixed args (e.g., Gemini `--resume latest`).
    Fixed { args: &'static [&'static str] },
    /// Not supported.
    NotSupported,
}

impl ResumeMode {
    /// Get resume args for spawning.
    pub fn args_for(&self) -> Vec<String> {
        match self {
            ResumeMode::ContinueInCwd { flag } => vec![flag.to_string()],
            ResumeMode::Fixed { args } => args.iter().map(|s| s.to_string()).collect(),
            ResumeMode::NotSupported => vec![],
        }
    }
}

impl SpawnMode {
    /// Downgrade `Resume` to `Fresh` when the backend has no resumable session
    /// in `working_dir`. A no-op for `Fresh` inputs and for backends / paths
    /// where detection is unavailable.
    ///
    /// Call this at every "auto-resume on startup / session-restore" site so
    /// the Claude "opened-but-idle pane" case never issues `--continue` (which
    /// would error out and flash the failure into the pane's vterm before the
    /// daemon's crash-respawn falls back to Fresh).
    pub fn downgraded_for(self, command: &str, working_dir: Option<&std::path::Path>) -> Self {
        if !matches!(self, SpawnMode::Resume) {
            return self;
        }
        let Some(wd) = working_dir else { return self };
        let Some(backend) = Backend::from_command(command) else {
            return self;
        };
        if backend.has_resumable_session(wd) {
            self
        } else {
            SpawnMode::Fresh
        }
    }
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
    /// Whether `instructions_path` is a file shared with the user (e.g. AGENTS.md,
    /// GEMINI.md). When true, writes use marker-merge to preserve user content;
    /// when false the whole file is agend-owned and rewritten in full.
    pub instructions_shared: bool,
    /// Inject instructions as the first message once the agent reaches Ready.
    /// Needed for backends (Kiro) whose CLI does not auto-load the instructions file.
    pub inject_instructions_on_ready: bool,
    /// Relative path for MCP config file from working dir.
    /// Timeout in seconds for ready detection.
    pub ready_timeout_secs: u64,
    /// Args to use when resuming is not possible (fresh start after crash).
    /// Falls back to `args` if None.
    pub fresh_args: Option<&'static [&'static str]>,
    /// Whether the backend loads the `agend-mcp-bridge` server when spawned in
    /// fleet mode. `false` means the backend's MCP discovery is incompatible
    /// with `<workdir>/.<vendor-dir>/mcp_config.json` writes — the bridge is
    /// configured on disk but the backend ignores it, leaving the spawned
    /// instance without fleet `send` / `inbox` / `task` tools.
    ///
    /// Empirical: AGY (#987 #995 Bug 3) discovers project-local
    /// `.antigravitycli/mcp_config.json` for project-ID storage but ignores
    /// its `mcpServers` field — only HOME-level
    /// `~/.gemini/antigravity-cli/mcp_config.json` loads. The fleet
    /// scope rule (`src/mcp_config.rs:5-11`) forbids HOME-level writes,
    /// so this backend ships with `fleet_mcp_supported: false` until
    /// upstream supports project-local `mcpServers` loading.
    ///
    /// Daemon spawn path emits a `[fleet-mcp-unsupported]` warning when
    /// this is `false` so operators are not surprised by missing tools.
    pub fleet_mcp_supported: bool,
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
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                // Not under `.claude/rules/` to avoid double-loading: we pass this
                // file explicitly via `--append-system-prompt-file` (see spawn_flags).
                instructions_path: ".claude/agend.md",
                instructions_shared: false,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 30,
                // Issue #468: regex anchored to line start + optional TUI prefix
                // ([^A-Za-z\n]{0,8}) instead of bare substring, so user-typed
                // text or scrollback containing the phrase mid-line cannot
                // trigger an unauthorized auto-dismiss. The prefix class
                // accepts any non-alpha non-newline byte (covers Ink box-
                // drawing chars, `>` `)` `(` cursors, digit-bracket choice
                // rows, etc.) and is length-capped at 8 chars so a long
                // indent before alpha text cannot match the phrase.
                // #996 Phase 1: keystroke changed from up+up+Enter to single
                // Enter. Modern Claude (v2.1.145+) defaults cursor to "Yes,
                // I trust this folder" (row 1, `❯` marker). The old
                // up+up+Enter sequence was correct when the prompt
                // defaulted to "No, exit" but is now actively harmful:
                // - TRUE positive: navigates AWAY from default-Yes → may
                //   confirm "No, exit" → Claude exits.
                // - FALSE positive on operator-quoted content matching
                //   the anchored regex: up+up+Enter re-submits prior
                //   history message → message duplication loop (the #996
                //   bug observed 37× today on fixup-lead pane).
                // Single `\r` resolves both: true-positive confirms the
                // default-Yes; false-positive adds a newline (no
                // destructive blast). Same shape as Agy #995/#997 dismiss.
                //
                // `Yes, proceed` deliberately retained on old keystroke
                // pending empirical verification at follow-up — modal +
                // default-cursor for that prompt not yet captured.
                dismiss_patterns: &[
                    (r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust", b"\r"),
                    (r"(?m)^[^A-Za-z\n]{0,8}Yes, proceed", b"\x1b[A\x1b[A\r"),
                ],
                fresh_args: None, // same as args (no resume in preset)
                fleet_mcp_supported: true,
            },
            Backend::KiroCli => BackendPreset {
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern:
                    "Trust All Tools active|ask a question or describe a task|/quit to exit",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::ContinueInCwd { flag: "--resume" },
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                instructions_shared: false,
                // Kiro CLI auto-loads .kiro/steering/*.md as context entries since
                // its initial release (v1.20.0, 2025-11-17). No injection needed.
                inject_instructions_on_ready: false,
                ready_timeout_secs: 30,
                dismiss_patterns: &[
                    // Trust-all-tools confirmation: cursor defaults to "No, exit"
                    // Down moves to "Yes, I accept", Enter confirms
                    // Keys sent with per-byte delay in try_dismiss_dialog
                    // Issue #468: anchored regex (see ClaudeCode comment above).
                    //
                    // #996 Phase 2a empirical verification (2026-05-21):
                    // byte-level analysis of `tests/fixtures/state-replay/
                    // kiro-tooluse.raw` confirms the modal opens with the
                    // `❯` cursor marker + magenta SGR (\x1b[38;2;255;0;255m)
                    // on "No, exit" (destructive default). State 2 in the
                    // same fixture shows the marker shifted to "Yes, I
                    // accept" after a Down-arrow press. The current
                    // `\x1b[B\r` (Down + Enter) keystroke correctly walks
                    // off the destructive default before confirming —
                    // unlike ClaudeCode `Yes, I trust` (which Phase 1
                    // #1001 fixed by changing to bare `\r` because that
                    // backend's modern modal defaults the cursor on the
                    // SAFE option). Do NOT collapse this to bare `\r` —
                    // see `kiro_no_exit_dismiss_uses_down_then_enter` test
                    // for the regression pin.
                    (r"(?m)^[^A-Za-z\n]{0,8}No, exit", b"\x1b[B\r"),
                ],
                fresh_args: None, // same as args
                fleet_mcp_supported: true,
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
                instructions_shared: true,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 20,
                dismiss_patterns: &[
                    // Trust directory prompt: "Yes, continue" is pre-selected → Enter.
                    // CR (\r), not LF — Ink's keyboard reader treats CR as Enter.
                    // macOS openpty doesn't translate LF→CR on input (ConPTY does),
                    // so LF here would silently no-op on mac.
                    // Issue #468: anchored regex (see ClaudeCode comment above).
                    // #1087: `*` instead of `{0,8}` — TUI centered modals have 40+ char prefix.
                    (r"(?m)^[^A-Za-z\n]*Do you trust", b"\r"),
                    (r"(?m)^[^A-Za-z\n]*Please restart", b"\r"),
                    // #1069: version-update modal blocks agent until operator
                    // selects an option. "2\r" = "Skip" (least invasive).
                    (r"(?m)^[^A-Za-z\n]*Update available!", b"2\r"),
                ],
                // Codex: "resume --last" → fresh start drops the resume subcommand
                fresh_args: Some(&["--dangerously-bypass-approvals-and-sandbox"]),
                fleet_mcp_supported: true,
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
                instructions_shared: true,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 45,
                dismiss_patterns: &[
                    // Issue #468: anchored regex (see ClaudeCode comment above).
                    // #1069: Esc = "Skip" (don't auto-update; let operator decide).
                    // #1087: `*` instead of `{0,8}` — TUI centered modals have 40+ char prefix.
                    (r"(?m)^[^A-Za-z\n]*Update Available", b"\x1b"),
                    (r"(?m)^[^A-Za-z\n]*Skip  Confirm", b"\x1b"),
                    (r"(?m)^[^A-Za-z\n]*Update Complete", b"\r"),
                    (r"(?m)^[^A-Za-z\n]*Please restart", b"\r"),
                ],
                fresh_args: None, // same as args (resume is in resume_mode, not args)
                fleet_mcp_supported: true,
            },
            Backend::Gemini => BackendPreset {
                command: "gemini",
                args: &["--yolo"],
                ready_pattern: "Type your message|YOLO",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::Fixed {
                    args: &["--resume", "latest"],
                },
                quit_command: "/exit",
                instructions_path: "GEMINI.md",
                instructions_shared: true,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 20,
                // Auto-approve: MCP tools ("3" = all server tools for session),
                // shell commands ("2" = allow for session).
                // Issue #468: anchored regex (line start + optional TUI prefix
                // [^A-Za-z\n]{0,8}). Substring match was false-positiving on
                // user input and scrollback content that contained the phrase
                // mid-paragraph, auto-injecting `2\n` / `3\n` without consent.
                // Gemini's Ink-based TUI renders dialogs with `│ ` prefix at
                // line start, which the anchor accepts.
                dismiss_patterns: &[
                    (r"(?m)^[^A-Za-z\n]{0,8}Allow execution of MCP tool", b"3\n"),
                    (r"(?m)^[^A-Za-z\n]{0,8}Allow execution of:", b"2\n"),
                ],
                fresh_args: None, // same as args (resume is in resume_mode, not args)
                fleet_mcp_supported: true,
            },
            Backend::Agy => BackendPreset {
                command: "agy",
                args: &["--dangerously-skip-permissions"],
                // #987: agy's interactive TUI renders an "Antigravity CLI <ver>"
                // banner on startup (calibrated against
                // tests/fixtures/state-replay/agy-thinking.raw). The pipe-OR
                // covers post-banner "Ready" state matchable variants in case
                // future TUI iterations rename the banner.
                ready_pattern: "Antigravity CLI|Type your message",
                submit_key: "\r",
                inject_prefix: "\r",
                typed_inject: true,
                // agy --continue is the documented resume path (matches the
                // `ResumeMode::ContinueInCwd { flag }` shape used by claude /
                // codex / kiro). Operator-verified in issue body.
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                instructions_path: "AGY.md",
                instructions_shared: true,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 20,
                // #995: --dangerously-skip-permissions auto-approves tool
                // permission requests (per `agy --help`), but does NOT bypass
                // the workspace-trust prompt that fires on every fresh spawn
                // ("Do you trust the contents of this project?"). The prompt
                // pre-selects "Yes, I trust this folder" (cursor `>` marker
                // on first row), so Enter alone confirms. Mirrors Codex's
                // "Do you trust" pattern with anchored regex per #468.
                dismiss_patterns: &[(r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust", b"\r")],
                fresh_args: None,
                // #995 Bug 3: AGY discovers `<workdir>/.antigravitycli/mcp_config.json`
                // for project-ID storage but its `mcpServers` map is ignored —
                // only HOME-level `~/.gemini/antigravity-cli/mcp_config.json`
                // loads (empirically proven; see PR description). The fleet
                // scope rule (src/mcp_config.rs:5-11) forbids HOME-level
                // writes, so we ship the agy backend without the bridge until
                // upstream supports project-local mcpServers. Daemon spawn
                // path emits a warning so operators are not surprised.
                fleet_mcp_supported: false,
            },
            // Shell and Raw have no preset behavior. `command` is `""` as a
            // sentinel — callers that need the actual spawn path should use
            // [`Backend::command_string`], which resolves Shell to `$SHELL`
            // and Raw to its stored path.
            Backend::Shell | Backend::Raw(_) => BackendPreset {
                command: "",
                args: &[],
                ready_pattern: "",
                submit_key: "\r",
                inject_prefix: "",
                typed_inject: false,
                resume_mode: ResumeMode::NotSupported,
                quit_command: "exit",
                instructions_path: "",
                instructions_shared: false,
                inject_instructions_on_ready: false,
                ready_timeout_secs: 10,
                dismiss_patterns: &[],
                fresh_args: None,
                // Shell / Raw: no MCP discovery; the bridge does not apply.
                // `false` is the safe sentinel (no warning fires because
                // these backends don't go through the dispatch warning path
                // anyway — Backend::from_command returns None for raw paths).
                fleet_mcp_supported: false,
            },
        }
    }

    /// Try to detect backend from a command string (matches basename, not full path).
    /// Prefix matching (e.g. `claude-*`) handles versioned binaries like `claude-2.1.89`.
    /// This is intentionally broader than `parse_str`, which only accepts exact names.
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
        } else if basename == "agy" || basename.starts_with("antigravity") {
            // #987: agy (binary name) + antigravity-cli (full product name).
            // basename match handles `/usr/local/bin/agy`; prefix match
            // handles future `antigravity-foo` variants. parse_str above
            // covers the user-facing "antigravity" alias for hand-edited
            // fleet.yaml entries.
            Some(Backend::Agy)
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
            // #987: agy slots after gemini chronologically — they share
            // Google's agent engine; gemini-cli sunsets 2026-06-18 and agy
            // is its official successor.
            Backend::Agy,
        ]
    }

    /// Whether the previous session in `working_dir` is actually resumable.
    ///
    /// On daemon (re)start every agent is spawned with [`SpawnMode::Resume`]
    /// (`spawn_and_register_agent` in `daemon/mod.rs`). Some backends — Claude
    /// in particular — only persist a session file once the user sends a
    /// message; if a pane was opened but never used, `claude --continue` fails
    /// with "No conversation found to continue" and the daemon falls back to a
    /// Fresh spawn via crash-respawn — but the failure briefly renders into
    /// the pane's vterm before recovery, which looks broken.
    ///
    /// Returning `false` lets callers downgrade Resume → Fresh up front so the
    /// user never sees the failure flash. Backends without their own detection
    /// here return `true` (optimistic) and rely on the existing crash-respawn
    /// path as a safety net.
    pub fn has_resumable_session(&self, working_dir: &std::path::Path) -> bool {
        match self {
            Backend::ClaudeCode => {
                claude_session::has_resumable(working_dir, &claude_session::default_projects_root())
            }
            _ => true,
        }
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

    /// Display name matching the CLI command. For [`Backend::Raw`] returns the
    /// stored path verbatim (borrow tied to self).
    pub fn name(&self) -> &str {
        self.as_str()
    }

    /// Extra CLI flags to pass on spawn, derived from files that
    /// `instructions::generate` has written to `working_dir`. Only emits a flag
    /// when the corresponding file is present, so this is safe to call
    /// unconditionally from every spawn path.
    ///
    /// Claude Code gets `--append-system-prompt-file` (instructions) plus
    /// `--mcp-config` (MCP wiring). Other backends rely on their own
    /// auto-discovery mechanisms and return an empty vec.
    pub fn spawn_flags(&self, working_dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        if matches!(self, Backend::ClaudeCode) {
            let instr = working_dir.join(self.preset().instructions_path);
            if instr.exists() {
                out.push("--append-system-prompt-file".to_string());
                out.push(instr.display().to_string());
            }
            let mcp = working_dir.join("mcp-config.json");
            if mcp.exists() {
                out.push("--mcp-config".to_string());
                out.push(mcp.display().to_string());
            }
        }
        out
    }

    /// Preset args to prepend on spawn. See [`SpawnMode`] for the selection
    /// rule. Shell/Raw variants return an empty vec.
    pub fn preset_spawn_args(&self, mode: SpawnMode) -> Vec<String> {
        let preset = self.preset();
        match mode {
            SpawnMode::Fresh => preset
                .fresh_args
                .unwrap_or(preset.args)
                .iter()
                .map(|s| s.to_string())
                .collect(),
            SpawnMode::Resume => {
                let mut out: Vec<String> = preset.args.iter().map(|s| s.to_string()).collect();
                out.extend(preset.resume_mode.args_for());
                out
            }
        }
    }

    /// Check if the backend binary is in PATH.
    ///
    /// Uses the `which` crate so Windows honors `PATHEXT` (npm-installed
    /// backends live at `claude.cmd`, `codex.ps1`, etc., not bare
    /// `claude`). The previous implementation shelled out to a `which`
    /// binary that isn't in the default Windows PATH, so this always
    /// reported "not installed" on Windows even when the backend was
    /// working fine.
    pub fn is_installed(&self) -> bool {
        let preset = self.preset();
        which::which(preset.command).is_ok()
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

    /// Version used when patterns were last calibrated. Non-preset variants
    /// return `"n/a"` (no pattern calibration).
    pub fn calibrated_version(&self) -> &'static str {
        match self {
            Backend::ClaudeCode => "2.1.89",
            Backend::KiroCli => "1.29.6",
            Backend::Codex => "0.118.0",
            Backend::OpenCode => "1.4.0",
            Backend::Gemini => "0.37.1",
            Backend::Agy => "1.0.0",
            Backend::Shell | Backend::Raw(_) => "n/a",
        }
    }
}

/// Detection helpers for `Backend::ClaudeCode`'s on-disk session storage.
///
/// Claude Code persists every conversation under
/// `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. The encoding is
/// undocumented but stable in practice: every char that isn't `[A-Za-z0-9-]`
/// is replaced with `-` (so `/Users/x/.foo/bar` → `-Users-x--foo-bar`).
mod claude_session {
    use std::io::{BufRead, BufReader};
    use std::path::{Path, PathBuf};

    /// `~/.claude/projects/`. Falls back to `$TMPDIR/.claude/projects` when
    /// `$HOME` is unresolvable — that path almost certainly won't exist and
    /// `has_resumable` will return false, which is the correct conservative
    /// answer.
    pub fn default_projects_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".claude")
            .join("projects")
    }

    /// Whether `working_dir` has a resumable Claude session under
    /// `projects_root`.
    ///
    /// "Resumable" here means: at least one `.jsonl` in the project dir has a
    /// `"type":"user"` entry. Claude writes metadata-only files
    /// (`custom-title`, `agent-name`, `pr-link`) before the first user
    /// message, and `claude --continue` cannot resume from those.
    pub fn has_resumable(working_dir: &Path, projects_root: &Path) -> bool {
        let canonical = canonicalize_for_encode(working_dir);
        let project_dir = projects_root.join(encode_project_dir(&canonical));
        let Ok(entries) = std::fs::read_dir(&project_dir) else {
            return false;
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .any(|e| jsonl_has_user_entry(&e.path()))
    }

    /// Canonicalize `working_dir` so the encoded project-dir name matches what
    /// claude CLI's Node `fs.realpathSync.native` produces before writing the
    /// session jsonl. Falls back to the raw input on canonicalize Err so cold
    /// spawns (working_dir not yet on disk) preserve the conservative-false
    /// branch in [`has_resumable`].
    ///
    /// Uses `dunce::canonicalize` rather than `std::fs::canonicalize`: on
    /// Windows the former strips `\\?\` UNC verbatim prefixes when safe,
    /// matching node's behavior; on Unix the two are identical.
    fn canonicalize_for_encode(working_dir: &Path) -> PathBuf {
        dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf())
    }

    /// Encode an absolute path the way Claude names project dirs.
    ///
    // TODO: option B (--session-id) follow-up — see #893 spike artifacts for
    // storage design (fixup-dev-2's metadata/<agent>.json proposal). Trigger:
    // if a new encode-mismatch class appears that `--continue` newest-wins
    // doesn't cover.
    fn encode_project_dir(path: &Path) -> String {
        path.to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }

    /// Streamed scan: returns true on the first line containing
    /// `"type":"user"`. Line-buffered, so a multi-MB session file aborts after
    /// the first match without loading the rest into memory.
    fn jsonl_has_user_entry(path: &Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("\"type\":\"user\""))
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::*;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn unique_tmp(label: &str) -> PathBuf {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agend-claude-session-test-{}-{}-{}",
                std::process::id(),
                label,
                id
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn encode_project_dir_matches_claudes_scheme() {
            // Real path encoding seen under ~/.claude/projects/ on macOS:
            // /Users/x/.agend-terminal/workspace/general
            //   → -Users-x--agend-terminal-workspace-general
            assert_eq!(
                encode_project_dir(Path::new("/Users/x/.agend-terminal/workspace/general")),
                "-Users-x--agend-terminal-workspace-general"
            );
            // Underscore is not in [A-Za-z0-9-] so it becomes `-`.
            assert_eq!(
                encode_project_dir(Path::new("/tmp/with_underscore")),
                "-tmp-with-underscore"
            );
            // Existing dashes pass through unchanged.
            assert_eq!(
                encode_project_dir(Path::new("/private/tmp/agend-terminal-test")),
                "-private-tmp-agend-terminal-test"
            );
        }

        #[test]
        fn missing_project_dir_is_not_resumable() {
            let root = unique_tmp("missing");
            assert!(!has_resumable(Path::new("/nonexistent/work/dir"), &root));
        }

        #[test]
        fn empty_project_dir_is_not_resumable() {
            let root = unique_tmp("empty");
            let work = Path::new("/work/empty");
            std::fs::create_dir_all(root.join(encode_project_dir(work))).unwrap();
            assert!(!has_resumable(work, &root));
        }

        #[test]
        fn metadata_only_jsonl_is_not_resumable() {
            // Mirrors a real "opened-but-never-used" session captured on
            // disk: only custom-title + agent-name lines, no user entry.
            let root = unique_tmp("metadata");
            let work = Path::new("/work/metadata");
            let proj = root.join(encode_project_dir(work));
            std::fs::create_dir_all(&proj).unwrap();
            std::fs::write(
                proj.join("a.jsonl"),
                "{\"type\":\"custom-title\",\"customTitle\":\"x\",\"sessionId\":\"a\"}\n\
                 {\"type\":\"agent-name\",\"agentName\":\"x\",\"sessionId\":\"a\"}\n",
            )
            .unwrap();
            assert!(!has_resumable(work, &root));
        }

        #[test]
        fn user_bearing_jsonl_is_resumable() {
            let root = unique_tmp("user");
            let work = Path::new("/work/user");
            let proj = root.join(encode_project_dir(work));
            std::fs::create_dir_all(&proj).unwrap();
            std::fs::write(
                proj.join("a.jsonl"),
                "{\"type\":\"file-history-snapshot\",\"sessionId\":\"a\"}\n\
                 {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            )
            .unwrap();
            assert!(has_resumable(work, &root));
        }

        #[test]
        fn mixed_dir_with_any_user_jsonl_is_resumable() {
            // Multiple sessions in the same project dir: one metadata-only,
            // one with a user entry. Either order should resolve to true.
            let root = unique_tmp("mixed");
            let work = Path::new("/work/mixed");
            let proj = root.join(encode_project_dir(work));
            std::fs::create_dir_all(&proj).unwrap();
            std::fs::write(
                proj.join("metadata.jsonl"),
                "{\"type\":\"custom-title\",\"customTitle\":\"x\",\"sessionId\":\"a\"}\n",
            )
            .unwrap();
            std::fs::write(
                proj.join("real.jsonl"),
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            )
            .unwrap();
            assert!(has_resumable(work, &root));
        }

        #[test]
        fn non_jsonl_files_are_ignored() {
            let root = unique_tmp("nonjsonl");
            let work = Path::new("/work/nonjsonl");
            let proj = root.join(encode_project_dir(work));
            std::fs::create_dir_all(&proj).unwrap();
            // A `.txt` file with `"type":"user"` text must not count.
            std::fs::write(proj.join("note.txt"), "{\"type\":\"user\"}\n").unwrap();
            assert!(!has_resumable(work, &root));
        }

        /// Issue #893 regression fixture. On macOS, `/tmp` is a symlink to
        /// `/private/tmp`, and claude CLI's Node `fs.realpathSync.native`
        /// canonicalizes before encoding the project-dir name. Without
        /// caller-side canonicalize, agend encodes the raw `/tmp/...` and
        /// looks at `<root>/-tmp-...` while claude wrote to
        /// `<root>/-private-tmp-...` → `read_dir` ENOENT → `has_resumable`
        /// returns false → `--continue` never fires → context lost on
        /// relaunch. Verified empirically by the #893 spike (filesystem
        /// inspection of `~/.claude/projects/` after `claude -p` on a
        /// `/tmp/...` cwd showed only `-private-tmp-...` entries).
        ///
        /// Pre-fix this test asserts the bug; post-fix `canonicalize_for_encode`
        /// rewrites `/tmp/X` → `/private/tmp/X` so the lookup matches.
        #[test]
        #[cfg(target_os = "macos")]
        fn has_resumable_handles_tmp_to_private_tmp_alias() {
            let root = unique_tmp("tmp-alias");
            let token = format!(
                "tmp-alias-{}-{}",
                std::process::id(),
                root.file_name().and_then(|n| n.to_str()).unwrap_or("x")
            );
            let raw_work = PathBuf::from(format!("/tmp/{}/sub", token));
            std::fs::create_dir_all(&raw_work).unwrap();
            let canonical_work = std::fs::canonicalize(&raw_work).unwrap();
            assert_ne!(
                raw_work, canonical_work,
                "macOS should canonicalize /tmp → /private/tmp; if this asserts the \
                 host /tmp symlink is missing — investigate before treating the rest \
                 of this test as a real failure"
            );
            // Pre-populate projects_root with a user-bearing jsonl under the
            // CANONICAL encoding (what claude CLI would actually write).
            let canonical_dir = root.join(encode_project_dir(&canonical_work));
            std::fs::create_dir_all(&canonical_dir).unwrap();
            std::fs::write(
                canonical_dir.join("a.jsonl"),
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            )
            .unwrap();
            // Sanity: the raw-encoded dir does NOT exist on disk. If this
            // asserts, the fix has accidentally normalized the encoded form
            // too — adjust the fixture rather than relaxing this check.
            let raw_encoded_dir = root.join(encode_project_dir(&raw_work));
            assert!(
                !raw_encoded_dir.exists(),
                "raw-encoded dir should not exist; encoding scheme must preserve \
                 the /tmp vs /private/tmp distinction"
            );
            assert!(
                has_resumable(&raw_work, &root),
                "has_resumable should canonicalize the raw /tmp path before encoding \
                 and find the jsonl at the canonical encoding"
            );
            let _ = std::fs::remove_dir_all(format!("/tmp/{}", token));
        }

        /// Windows-only invariant: `canonicalize_for_encode` must NOT return
        /// a path with the `\\?\` UNC verbatim-prefix that `std::fs::canonicalize`
        /// produces, because Node's `fs.realpathSync.native` (which claude CLI
        /// uses to derive the project-dir name) strips it. If we kept the
        /// verbatim prefix, the encoded project-dir name would diverge from
        /// claude's by the prefix characters → same class of bug as #893's
        /// macOS `/tmp` → `/private/tmp` alias.
        ///
        /// Also exercises Windows path normalization: case-fold round-trip
        /// via `has_resumable` (the real-world cwd casing claude inherits
        /// matches what canonicalize returns).
        #[test]
        #[cfg(target_os = "windows")]
        fn canonicalize_for_encode_strips_verbatim_prefix_on_windows() {
            let root = unique_tmp("win-verbatim");
            let work_root = unique_tmp("win-work");
            let raw_work = work_root.join("sub");
            std::fs::create_dir_all(&raw_work).unwrap();
            let canonical = super::canonicalize_for_encode(&raw_work);
            let canonical_str = canonical.to_string_lossy();
            assert!(
                !canonical_str.starts_with(r"\\?\"),
                "canonicalize_for_encode must strip Windows `\\\\?\\` verbatim prefix \
                 (got {canonical_str:?}); dunce::canonicalize handles this — fall through \
                 to std::fs::canonicalize would re-introduce the prefix"
            );
            // End-to-end: pre-populate projects_root with a user-bearing jsonl
            // under the canonical encoding; has_resumable should find it
            // when called with the raw (pre-canonicalize) cwd.
            let canonical_dir = root.join(encode_project_dir(&canonical));
            std::fs::create_dir_all(&canonical_dir).unwrap();
            std::fs::write(
                canonical_dir.join("a.jsonl"),
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            )
            .unwrap();
            assert!(
                has_resumable(&raw_work, &root),
                "has_resumable should canonicalize the cwd and find the jsonl under \
                 the same encoding claude CLI's fs.realpathSync.native produces"
            );
        }

        /// Cold-spawn invariant: `working_dir` may not exist on disk yet
        /// (first call before claude has created the project). Canonicalize
        /// returns Err in that branch; the fallback to the raw path keeps
        /// `has_resumable`'s conservative-false return intact.
        #[test]
        fn canonicalize_for_encode_falls_back_to_raw_on_err() {
            let nonexistent = Path::new("/this/path/does/not/exist/anywhere");
            let canonical = super::canonicalize_for_encode(nonexistent);
            assert_eq!(canonical, nonexistent.to_path_buf());
            // And has_resumable on a non-existent cwd stays false.
            let root = unique_tmp("nonexistent-cwd");
            assert!(!has_resumable(nonexistent, &root));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn from_command_detection() {
        assert_eq!(Backend::from_command("claude"), Some(Backend::ClaudeCode));
        assert_eq!(Backend::from_command("kiro-cli"), Some(Backend::KiroCli));
        assert_eq!(Backend::from_command("codex"), Some(Backend::Codex));
        assert_eq!(Backend::from_command("opencode"), Some(Backend::OpenCode));
        assert_eq!(Backend::from_command("gemini"), Some(Backend::Gemini));
        assert_eq!(Backend::from_command("agy"), Some(Backend::Agy));
        // Case insensitive
        assert_eq!(Backend::from_command("Claude"), Some(Backend::ClaudeCode));
        assert_eq!(
            Backend::from_command("/usr/bin/claude"),
            Some(Backend::ClaudeCode)
        );
        // Alias: the bare "kiro" input (without the `-cli` suffix) must also
        // resolve to `KiroCli` and round-trip through `preset().command` to
        // the canonical `"kiro-cli"` executable name. Previously covered by
        // the `backend_resolves_to_preset_command` test removed in PR #22.
        assert_eq!(
            Backend::from_command("kiro").map(|b| b.preset().command),
            Some("kiro-cli")
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

        // #987 + #995: agy mirrors the existing preset shape — verify command,
        // args, and the dangerously-skip-permissions flag. #995 added a
        // workspace-trust dismiss_pattern after live-spawn proved the flag
        // doesn't bypass the trust-folder prompt.
        let agy = Backend::Agy.preset();
        assert_eq!(agy.command, "agy");
        assert!(agy.args.contains(&"--dangerously-skip-permissions"));
        assert_eq!(agy.dismiss_patterns.len(), 1, "#995 trust dismiss");
        assert!(agy.dismiss_patterns[0].0.contains("Yes, I trust"));
        assert_eq!(agy.dismiss_patterns[0].1, b"\r");
        assert_eq!(agy.instructions_path, "AGY.md");
    }

    /// #995 Bug 3: `fleet_mcp_supported` flag pins which backends ship with
    /// the `agend-mcp-bridge` working in fleet mode. Currently every
    /// backend except Agy supports it; Agy is `false` because its MCP
    /// discovery ignores `<workdir>/.antigravitycli/mcp_config.json`
    /// mcpServers field (only HOME-level loads, which the scope rule
    /// at `src/mcp_config.rs:5-11` forbids).
    ///
    /// Daemon spawn path (`src/agent.rs spawn_agent`) emits a
    /// `[fleet-mcp-unsupported]` tracing::warn when this is `false` so
    /// operators see the warning in app.log.
    #[test]
    fn fleet_mcp_supported_pins_per_backend() {
        // Currently-supported backends — bridge loads via project-local config.
        assert!(Backend::ClaudeCode.preset().fleet_mcp_supported);
        assert!(Backend::KiroCli.preset().fleet_mcp_supported);
        assert!(Backend::Codex.preset().fleet_mcp_supported);
        assert!(Backend::OpenCode.preset().fleet_mcp_supported);
        assert!(Backend::Gemini.preset().fleet_mcp_supported);
        // Unsupported — see field docstring + Backend::Agy preset comment.
        assert!(
            !Backend::Agy.preset().fleet_mcp_supported,
            "#995 Bug 3: Agy bridge currently doesn't load — flag pins the issue \
             until upstream supports project-local mcpServers"
        );
        // Shell / Raw — no MCP discovery; sentinel `false`.
        assert!(!Backend::Shell.preset().fleet_mcp_supported);
        assert!(
            !Backend::Raw("/opt/foo".to_string())
                .preset()
                .fleet_mcp_supported
        );
    }

    /// #996 Phase 1: ClaudeCode `Yes, I trust` dismiss must send a single
    /// Enter byte (`\r`), NEVER up-arrow sequences. Modern Claude prompts
    /// (v2.1.145+) default the cursor to "Yes, I trust" — the historical
    /// up+up+Enter is now actively harmful (navigates away from default,
    /// or re-submits history on false-positive). Single Enter confirms the
    /// default-Yes AND adds a non-destructive newline on false-positive.
    #[test]
    fn claude_trust_dismiss_uses_single_enter() {
        let claude = Backend::ClaudeCode.preset();
        let trust = claude
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("Yes, I trust"))
            .expect("ClaudeCode must have a `Yes, I trust` dismiss pattern");

        assert_eq!(
            trust.1, b"\r",
            "#996 Phase 1: trust-prompt keystroke must be single Enter"
        );

        // Negative pin: no ESC bytes allowed — historical up-arrow (\x1b[A)
        // and any other CSI sequence in the keystroke is a regression.
        assert!(
            !trust.1.contains(&0x1b),
            "#996: trust dismiss keystroke must not contain ESC (0x1b)"
        );
    }

    /// #996 Phase 2a: KiroCli `No, exit` dismiss must send Down + Enter
    /// (`\x1b[B\r`), NOT bare Enter. Empirical evidence from
    /// `tests/fixtures/state-replay/kiro-tooluse.raw` (byte offsets 5900 +
    /// 6691): the modal opens with the `❯` cursor marker on "No, exit"
    /// (destructive default). Bare Enter would EXIT kiro. Down-arrow
    /// walks the cursor to "Yes, I accept" before Enter confirms.
    ///
    /// This regression pin guards against a future "simplify to bare \r"
    /// refactor (mirror of #1001 / ClaudeCode `Yes, I trust`) that would
    /// be CORRECT for claude but WRONG for kiro — the empirical default
    /// cursor on this backend is the destructive option, not the safe
    /// one. See backend.rs comment block at the dismiss_patterns entry.
    #[test]
    fn kiro_no_exit_dismiss_uses_down_then_enter() {
        let kiro = Backend::KiroCli.preset();
        let entry = kiro
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("No, exit"))
            .expect("KiroCli must have a `No, exit` dismiss pattern");

        // Positive pin: exact keystroke shape Down + Enter
        assert_eq!(
            entry.1, b"\x1b[B\r",
            "#996 Phase 2a: kiro trust-all-tools dismiss must walk off the \
             destructive default (\\x1b[B = Down) before confirming (\\r). \
             Empirical evidence: kiro-tooluse.raw byte offsets 5900 + 6691 \
             show the modal opens with cursor on `No, exit`."
        );

        // Negative pin: must NOT collapse to bare Enter — that would EXIT kiro
        // on the empirically-verified destructive default.
        assert_ne!(
            entry.1, b"\r",
            "#996 Phase 2a regression guard: bare Enter would select kiro's \
             destructive default (`No, exit`). Verify `kiro-tooluse.raw` \
             fixture before changing the keystroke shape."
        );

        // Negative pin: must START with ESC + `[B` (Down arrow CSI), not
        // some other CSI sequence. Defends against off-by-one keystroke
        // typos like `\x1b[A` (Up — wrong direction).
        assert!(
            entry.1.starts_with(b"\x1b[B"),
            "#996 Phase 2a: keystroke must start with Down-arrow CSI \
             (\\x1b[B), got: {:?}",
            entry.1
        );
    }

    #[test]
    fn resume_mode_types() {
        assert_eq!(
            Backend::ClaudeCode.preset().resume_mode.args_for(),
            vec!["--continue"]
        );
        assert_eq!(
            Backend::KiroCli.preset().resume_mode.args_for(),
            vec!["--resume"]
        );
        assert!(Backend::Codex.preset().resume_mode.args_for().is_empty());
        assert_eq!(
            Backend::Gemini.preset().resume_mode.args_for(),
            vec!["--resume", "latest"]
        );
        assert_eq!(
            Backend::OpenCode.preset().resume_mode.args_for(),
            vec!["--continue"]
        );
        // #987: agy uses `--continue` (same shape as claude/opencode/kiro).
        assert_eq!(
            Backend::Agy.preset().resume_mode.args_for(),
            vec!["--continue"]
        );
    }

    #[test]
    fn backend_name_roundtrip() {
        assert_eq!(Backend::ClaudeCode.name(), "claude");
        assert_eq!(Backend::KiroCli.name(), "kiro-cli");
        assert_eq!(Backend::Codex.name(), "codex");
        assert_eq!(Backend::OpenCode.name(), "opencode");
        assert_eq!(Backend::Gemini.name(), "gemini");
        // #995: agy display name is the product short form, not the binary.
        // The binary (`agy`) is exposed via preset().command instead.
        assert_eq!(Backend::Agy.name(), "antigravity-cli");
        assert_eq!(Backend::Agy.preset().command, "agy");
    }

    #[test]
    fn all_backends_returns_six() {
        // #987: bumped from 5 → 6 with Backend::Agy addition (Google
        // Antigravity CLI as gemini-cli's official successor).
        assert_eq!(Backend::all().len(), 6);
    }

    #[test]
    fn parse_str_known_presets() {
        assert_eq!(Backend::parse_str("claude"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("claude-code"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("kiro-cli"), Backend::KiroCli);
        assert_eq!(Backend::parse_str("kiro"), Backend::KiroCli);
        assert_eq!(Backend::parse_str("codex"), Backend::Codex);
        assert_eq!(Backend::parse_str("opencode"), Backend::OpenCode);
        assert_eq!(Backend::parse_str("gemini"), Backend::Gemini);
        // #987: agy + antigravity + antigravity-cli all resolve to Backend::Agy.
        assert_eq!(Backend::parse_str("agy"), Backend::Agy);
        assert_eq!(Backend::parse_str("antigravity"), Backend::Agy);
        assert_eq!(Backend::parse_str("antigravity-cli"), Backend::Agy);
        // Case insensitive
        assert_eq!(Backend::parse_str("Claude"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("AGY"), Backend::Agy);
        // Whitespace trim
        assert_eq!(Backend::parse_str("  claude  "), Backend::ClaudeCode);
    }

    #[test]
    fn parse_str_shell_aliases() {
        assert_eq!(Backend::parse_str("shell"), Backend::Shell);
        assert_eq!(Backend::parse_str("bash"), Backend::Shell);
        assert_eq!(Backend::parse_str("zsh"), Backend::Shell);
        assert_eq!(Backend::parse_str("sh"), Backend::Shell);
        assert_eq!(Backend::parse_str("SHELL"), Backend::Shell);
    }

    #[test]
    fn parse_str_unknown_becomes_raw() {
        assert_eq!(
            Backend::parse_str("/opt/custom/tool"),
            Backend::Raw("/opt/custom/tool".to_string())
        );
        assert_eq!(Backend::parse_str("vim"), Backend::Raw("vim".to_string()));
        assert_eq!(
            Backend::parse_str("/usr/bin/my-agent"),
            Backend::Raw("/usr/bin/my-agent".to_string())
        );
    }

    #[test]
    fn as_str_roundtrip_preserves_raw_path() {
        let raw = Backend::Raw("/opt/foo/bar".to_string());
        assert_eq!(raw.as_str(), "/opt/foo/bar");
        assert_eq!(Backend::Shell.as_str(), "shell");
        assert_eq!(Backend::ClaudeCode.as_str(), "claude");
    }

    #[test]
    fn serde_roundtrip_bare_string() {
        // Preset variant serializes as bare name.
        let yaml = serde_yaml_ng::to_string(&Backend::ClaudeCode).unwrap();
        assert_eq!(yaml.trim(), "claude");

        // Shell → "shell"
        let yaml = serde_yaml_ng::to_string(&Backend::Shell).unwrap();
        assert_eq!(yaml.trim(), "shell");

        // Raw → literal path (no enum tagging like `!Raw`).
        let yaml = serde_yaml_ng::to_string(&Backend::Raw("/opt/x".to_string())).unwrap();
        assert_eq!(yaml.trim(), "/opt/x");

        // Deserialize back to the same value.
        assert_eq!(
            serde_yaml_ng::from_str::<Backend>("claude").unwrap(),
            Backend::ClaudeCode
        );
        assert_eq!(
            serde_yaml_ng::from_str::<Backend>("shell").unwrap(),
            Backend::Shell
        );
        assert_eq!(
            serde_yaml_ng::from_str::<Backend>("/opt/x").unwrap(),
            Backend::Raw("/opt/x".to_string())
        );
    }

    #[test]
    fn preset_shell_and_raw_are_empty() {
        for b in [Backend::Shell, Backend::Raw("/opt/x".to_string())] {
            let p = b.preset();
            assert!(p.args.is_empty(), "{b:?} should have empty args");
            assert!(
                p.ready_pattern.is_empty(),
                "{b:?} should have no ready pattern"
            );
            assert!(
                p.dismiss_patterns.is_empty(),
                "{b:?} should have no dismiss patterns"
            );
            assert!(matches!(p.resume_mode, ResumeMode::NotSupported));
        }
    }

    #[test]
    fn command_string_shell_uses_env_or_fallback() {
        // Whatever $SHELL is in test env, result must be non-empty.
        let cmd = Backend::Shell.command_string();
        assert!(!cmd.is_empty());
        // Unix: `/bin/bash`, `/bin/zsh`, etc.; fallback `/bin/bash`. Windows
        // under Git Bash translates POSIX SHELL into a Win32 path like
        // `C:\Program Files\Git\bin\bash.exe` before the child sees it, so
        // accept drive-letter paths too. CI's plain PowerShell has no $SHELL
        // at all, so the Windows fallback is the bare `cmd.exe` name (PATH
        // resolution handled by the shell spawn later).
        let unixish = cmd.starts_with('/');
        let winish = cmd.chars().nth(1) == Some(':') && cmd.chars().nth(2) == Some('\\');
        let bare_exe = cmd.ends_with(".exe") && !cmd.contains(['/', '\\']);
        assert!(
            unixish || winish || bare_exe,
            "unexpected shell path shape: {cmd:?}"
        );
    }

    #[test]
    fn command_string_raw_returns_literal() {
        assert_eq!(
            Backend::Raw("/opt/x/my-tool".to_string()).command_string(),
            "/opt/x/my-tool"
        );
    }

    #[test]
    fn command_string_preset_returns_static_command() {
        assert_eq!(Backend::ClaudeCode.command_string(), "claude");
        assert_eq!(Backend::KiroCli.command_string(), "kiro-cli");
    }

    #[test]
    fn calibrated_version_na_for_non_preset() {
        assert_eq!(Backend::Shell.calibrated_version(), "n/a");
        assert_eq!(
            Backend::Raw("/opt/x".to_string()).calibrated_version(),
            "n/a"
        );
    }

    #[test]
    fn resume_mode_continue_in_cwd_args() {
        let mode = ResumeMode::ContinueInCwd { flag: "--continue" };
        assert_eq!(mode.args_for(), vec!["--continue".to_string()]);
    }

    #[test]
    fn resume_mode_fixed_args() {
        let mode = ResumeMode::Fixed {
            args: &["--resume", "latest"],
        };
        assert_eq!(
            mode.args_for(),
            vec!["--resume".to_string(), "latest".to_string()]
        );
    }

    #[test]
    fn resume_mode_not_supported_args() {
        assert!(ResumeMode::NotSupported.args_for().is_empty());
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
        assert_eq!(
            Backend::Gemini.format_model_arg("gemini-2.5-pro"),
            "gemini-2.5-pro"
        );
        assert_eq!(Backend::Codex.format_model_arg("o3"), "o3");
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "agend-backend-test-{}-{tag}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).ok();
        d
    }

    #[test]
    fn spawn_flags_claude_emits_only_for_existing_files() {
        let dir = tmp_dir("spawn_flags_claude_partial");
        // Nothing on disk yet — no flags.
        assert!(
            Backend::ClaudeCode.spawn_flags(&dir).is_empty(),
            "expected empty when files missing"
        );
        // Drop just the instructions file.
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/agend.md"), "x").unwrap();
        let flags = Backend::ClaudeCode.spawn_flags(&dir);
        assert!(flags.contains(&"--append-system-prompt-file".to_string()));
        assert!(!flags.contains(&"--mcp-config".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spawn_flags_claude_full_set_when_all_files_present() {
        let dir = tmp_dir("spawn_flags_claude_full");
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/agend.md"), "x").unwrap();
        std::fs::write(dir.join("mcp-config.json"), "{}").unwrap();
        let flags = Backend::ClaudeCode.spawn_flags(&dir);
        // Each flag appears exactly once, followed by its path arg.
        assert_eq!(
            flags
                .iter()
                .filter(|s| s.starts_with("--"))
                .collect::<Vec<_>>()
                .len(),
            2
        );
        assert!(flags
            .windows(2)
            .any(|w| w[0] == "--append-system-prompt-file" && w[1].ends_with(".claude/agend.md")));
        assert!(flags
            .windows(2)
            .any(|w| w[0] == "--mcp-config" && w[1].ends_with("mcp-config.json")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_update_prompt_dismiss_uses_skip() {
        let codex = Backend::Codex.preset();
        let entry = codex
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("Update available!"))
            .expect("#1069: codex must have an `Update available!` dismiss pattern");
        assert_eq!(
            entry.1, b"2\r",
            "#1069: keystroke must be `2\\r` (option 2 = Skip)"
        );
    }

    #[test]
    fn codex_update_dismiss_anchored_rejects_mid_line() {
        let codex = Backend::Codex.preset();
        let (pattern, _) = codex
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("Update available!"))
            .expect("#1069: codex update dismiss pattern must exist");
        let re = regex::Regex::new(pattern).expect("pattern must compile");
        assert!(
            re.is_match("✨ Update available! 0.132.0 -> 0.133.0"),
            "line-start match must succeed"
        );
        assert!(
            !re.is_match("User asked: is there an Update available! for the tool?"),
            "mid-line mention must NOT match (Issue #468 anchoring)"
        );
        // #1087: centered TUI modal with 40+ char prefix must match
        let centered = format!("{}Update available! 1.0 -> 2.0", " ".repeat(45));
        assert!(
            re.is_match(&centered),
            "#1087: centered modal with 45-space prefix must match"
        );
    }

    #[test]
    fn opencode_update_available_dismiss_uses_esc() {
        let oc = Backend::OpenCode.preset();
        let entry = oc
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("Update Available"))
            .expect("#1069: opencode must have an `Update Available` dismiss pattern");
        assert_eq!(
            entry.1, b"\x1b",
            "#1069: keystroke must be ESC (skip update, not confirm)"
        );
    }

    #[test]
    fn opencode_update_dismiss_anchored_rejects_mid_line() {
        let oc = Backend::OpenCode.preset();
        let (pattern, _) = oc
            .dismiss_patterns
            .iter()
            .find(|(re, _)| re.contains("Update Available"))
            .expect("#1069: opencode Update Available pattern must exist");
        let re = regex::Regex::new(pattern).expect("pattern must compile");
        assert!(
            re.is_match("  Update Available"),
            "line-start with TUI prefix must match"
        );
        assert!(
            !re.is_match("The agent mentioned Update Available in its response"),
            "mid-line mention must NOT match (Issue #468 anchoring)"
        );
        // #1087: centered TUI modal with 40+ char prefix must match
        let centered = format!("{}Update Available", " ".repeat(45));
        assert!(
            re.is_match(&centered),
            "#1087: centered modal with 45-space prefix must match"
        );
    }

    #[test]
    fn spawn_flags_non_claude_backends_are_empty() {
        let dir = tmp_dir("spawn_flags_non_claude");
        // Even if random files exist they must not produce flags for these.
        std::fs::write(dir.join("AGENTS.md"), "x").unwrap();
        std::fs::write(dir.join("GEMINI.md"), "x").unwrap();
        for b in [
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Gemini,
        ] {
            assert!(
                b.spawn_flags(&dir).is_empty(),
                "{b:?} must not emit spawn flags"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
