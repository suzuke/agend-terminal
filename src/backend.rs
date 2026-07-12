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
    // #1580: `Gemini` retired (gemini-cli sunset 2026-06-18). Its successor `Agy`
    // (Google Antigravity CLI) remains and inherits the shared productivity
    // markers (renamed GEMINI_*→AGY_*).
    /// Google Antigravity CLI (`agy`). Gemini CLI's official successor —
    /// shares the same Google agent engine. Standard `mcpServers` schema +
    /// project-local config at `<workdir>/.antigravitycli/mcp_config.json`.
    /// Added in #987.
    Agy,
    /// xAI Grok Build CLI (`grok`). Full-screen TUI coding agent.
    /// Project-local MCP at `<workdir>/.grok/config.toml` (`[mcp_servers.*]`).
    /// MVP: typed inject + trust dismiss; no lifecycle hooks yet.
    Grok,
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
    /// Codex, OpenCode, Agy. False for Shell + Raw — generic
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
            | Backend::Agy
            | Backend::Grok => true,
            Backend::Shell | Backend::Raw(_) => false,
        }
    }

    /// #1523: STRONG backends — those whose lifecycle hooks `mcp_config` injects
    /// and that therefore emit authoritative `hook_shadow` state events. Only
    /// these have hook data to PROMOTE over the screen heuristic; every other
    /// backend always uses the heuristic (no hooks fire, so `resolved_state_for`
    /// stays `Unknown` → heuristic fallback).
    ///
    /// **claude + agy (#2413 Phase D).** Both inject lifecycle hooks that emit
    /// token-authenticated shadow Evidence: claude via `.claude` settings;
    /// **agy via per-workspace `.agents/hooks.json`** written by `configure_agy`
    /// — its `PreInvocation`/`Stop` command hooks POST claude-compatible frames to
    /// the same shadow socket via `hook-event --event` (agy fires no tool hooks —
    /// t-…93090-0; busy/idle only, no tool granularity). The earlier "claude-only" gate was because Agy's hooks were
    /// never injected (2026-06-11: agy emitted 0 hook events); the prerequisite
    /// it named — "injection implementation AND shadow-data evidence that its
    /// hooks fire" — is now met (RE-spike t-…39100-6 proved per-workspace hooks
    /// fire live; injection added below). Other backends have no hooks (heuristic
    /// fallback). Gates: socket-env injection (agent/mod.rs), hook→authoritative
    /// promotion (hook_shadow.rs, also `AGEND_HOOK_STATE_POC`-gated), divergence
    /// telemetry, supervisor SRL hook-recovery.
    pub fn has_state_hooks(&self) -> bool {
        matches!(self, Backend::ClaudeCode | Backend::Agy)
    }

    /// #1440: credential env-var names this backend legitimately needs to
    /// authenticate to its LLM provider. Under `AGEND_ENV_ISOLATION`, only
    /// these (plus the base runtime allowlist + operator `passthrough_env`)
    /// survive `env_clear()` — so a Claude agent never inherits `OPENAI_API_KEY`
    /// and vice versa (cross-backend credential isolation).
    ///
    /// These intentionally override the `SENSITIVE_ENV_KEYS` deny-list for the
    /// owning backend only. `OpenCode` is multi-provider by design, so it
    /// declares the union of the providers agend drives; long-tail providers
    /// (Groq, OpenRouter, …) go through `passthrough_env`. `KiroCli`'s AWS
    /// operation creds (`AWS_*`) are deliberately excluded — pass them via
    /// `passthrough_env` only when that Kiro agent actually performs AWS work.
    pub fn credential_env_keys(&self) -> &'static [&'static str] {
        match self {
            Backend::ClaudeCode => &[
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ],
            Backend::Codex => &["OPENAI_API_KEY"],
            // #1580: Gemini retired; Agy (its successor) keeps the Google creds.
            Backend::Agy => &[
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "GOOGLE_APPLICATION_CREDENTIALS",
            ],
            Backend::KiroCli => &["KIRO_API_KEY"],
            Backend::OpenCode => &[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "OPENCODE_CONFIG",
                "OPENCODE_API_KEY",
            ],
            // OAuth also lives under ~/.grok/auth.json (file-based); the env
            // key is the headless/CI path. GROK_HOME may be used for isolation.
            Backend::Grok => &["XAI_API_KEY"],
            Backend::Shell | Backend::Raw(_) => &[],
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
            // #1580: "gemini"/"gemini-cli" retired → fall through to Raw.
            "agy" | "antigravity" | "antigravity-cli" => Backend::Agy,
            "grok" | "grok-build" | "grok-cli" | "grok-build-cli" => Backend::Grok,
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
            // #995: display name is the product short form `antigravity-cli`,
            // not the binary `agy`. Binary command remains `agy` (preset.command);
            // parse_str still accepts the "agy" alias for backward-compat with
            // any fleet.yaml entries written before #995.
            Backend::Agy => "antigravity-cli",
            Backend::Grok => "grok",
            Backend::Shell => "shell",
            Backend::Raw(s) => s.as_str(),
        }
    }

    /// #1944: the rendered input-box prompt marker, used by the draft-gate to
    /// decide whether the operator's input line is actually EMPTY (vs the
    /// timestamp-only `draft_state` heuristic, which can read a stale
    /// type-then-clear as a live draft). `None` = no detectable prompt widget →
    /// the caller falls back to the timestamp behavior (fail toward
    /// draft-protection). Set only for backends whose EMPTY-box render is a clean
    /// `<marker> ` with nothing after it, verified against a real capture:
    /// claude `❯` and agy `>` (live `pane_snapshot`, `tests/fixtures/state-replay`).
    ///
    /// codex is INTENTIONALLY excluded (#1948 follow-up): its empty box renders a
    /// rotating GHOST/placeholder phrase after the `›` (`› Explain this codebase`,
    /// `› Write tests for @filename`, …), so the marker probe reads the ghost as
    /// typed content → always defers (the v1 `Some("›")` claim was non-functional:
    /// fail-protect-safe but never delivered on an empty box). codex therefore
    /// falls back to the timestamp behavior until a colour/dim-based empty-box
    /// signal lands (the ghost is dim — see the #1948 codex spike). Operator live
    /// test (codex-44cea9, 2026-06-10) surfaced this.
    pub fn input_prompt_marker(&self) -> Option<&'static str> {
        match self {
            Backend::ClaudeCode | Backend::Grok => Some("❯"),
            Backend::Agy => Some(">"),
            _ => None,
        }
    }

    /// #1948 v2: the empty-input-box PLACEHOLDER for backends with no stable
    /// prompt-line marker but a hint string that the TUI shows ONLY while the box
    /// is empty (and replaces the moment the operator types). The draft-gate
    /// treats "placeholder visible" as "box empty". `None` = no placeholder probe
    /// (falls back to `input_prompt_marker`, then the timestamp behavior — fail
    /// toward draft-protection).
    ///
    /// kiro renders ` ask a question or describe a task ↵` when empty (verified
    /// against a live `pane_snapshot` of a just-cleared kiro pane, 2026-06-10).
    /// opencode was assessed and INTENTIONALLY left `None`: its `┃`-bordered box
    /// has no placeholder, and its `┃`-prefixed model/path footer is always
    /// non-empty, so input-vs-footer geometry can't be distinguished robustly
    /// (version-coupled) — fail-toward-protection fallback is safer. If a future
    /// opencode build adds a stable empty-box placeholder, add it here.
    pub fn input_empty_placeholder(&self) -> Option<&'static str> {
        match self {
            Backend::KiroCli => Some("ask a question or describe a task"),
            _ => None,
        }
    }

    /// #1948(b): the prompt marker for backends whose EMPTY box renders a
    /// rotating ghost phrase after the marker in the DIM attribute (so a plain
    /// `input_prompt_marker` probe mis-reads the ghost as typed content). The
    /// draft-gate routes these through `input_box_dim_aware_empty`, which uses the
    /// per-char DIM mask to tell the ghost (dim) from real input (normal).
    ///
    /// codex `›`: verified against a raw PTY capture (2026-06-10) — the prompt is
    /// `ESC[1m›` (bold) and the ghost (`Use /skills …`, `Explain this codebase`,
    /// …) is `ESC[2m` (dim) on the default colour. Operator confirmed the ghost is
    /// visually dim and real input is normal/bright, so the dim signal can't
    /// false-deliver on a real draft. `None` for everyone else.
    pub fn input_dim_ghost_marker(&self) -> Option<&'static str> {
        match self {
            Backend::Codex => Some("›"),
            _ => None,
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
            | Backend::Agy
            | Backend::Grok => self.preset().command.to_string(),
            Backend::Shell => crate::shell_command(),
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
    /// Not supported.
    NotSupported,
}

impl ResumeMode {
    /// Get resume args for spawning.
    pub fn args_for(&self) -> Vec<String> {
        match self {
            ResumeMode::ContinueInCwd { flag } => vec![flag.to_string()],
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

/// A pattern/keystroke pair for auto-dismissing a backend dialog (trust
/// prompt, update notice). When `label`'s regex matches the rendered screen,
/// `sequence` is written to the PTY.
#[derive(Debug, Clone)]
pub struct DismissPattern {
    /// Regex matched against the rendered screen (anchored per #468).
    pub label: &'static str,
    /// Key bytes sent to the PTY when the pattern matches.
    pub sequence: &'static [u8],
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
    pub dismiss_patterns: &'static [DismissPattern],
    /// Relative path for instructions file from working dir.
    pub instructions_path: &'static str,
    /// Whether `instructions_path` is a file shared with the user (e.g. AGENTS.md,
    /// GEMINI.md). When true, writes use marker-merge to preserve user content;
    /// when false the whole file is agend-owned and rewritten in full.
    pub instructions_shared: bool,
    /// Inject instructions as the first message once the agent reaches Idle.
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
    /// #7: emit a redraw trigger (Ctrl+L) after a PTY resize. True ONLY for
    /// backends whose TUI does not repaint on SIGWINCH — kiro-cli 2.1.x TUI v2,
    /// which otherwise leaves the pane blank until the next keystroke. Read by
    /// [`crate::layout::pane::Pane::resize_pty`]; `false` for every other
    /// backend, so they are provably untouched by the redraw injection.
    pub redraw_after_resize: bool,
}

/// #2744 PR-A: a backend's declared model-flag grammar, captured verbatim
/// from its CLI help into `tests/fixtures/cli-help/` at the calibrated
/// version. `None` (Shell/Raw/custom) means the backend has no proven model
/// semantics: the injection path skips with a warning and `set_model`
/// hard-errors instead of guessing.
#[derive(Debug, PartialEq)]
pub struct ModelCapability {
    /// Long flag exactly as the CLI help declares it.
    pub long_flag: &'static str,
    /// Short spelling — ONLY where the CLI help proves it (codex/opencode/
    /// grok declare `-m`; claude/kiro-cli/agy are long-flag-only).
    pub short_flag: Option<&'static str>,
    /// CLI version the grammar was captured at. Evidence/health metadata
    /// only — never a runtime gate (decision d-20260712101306674407-19).
    pub calibrated_version: &'static str,
}

/// Classified hit from [`ModelCapability::scan`].
///
/// `Confirmed` spellings (`--model`, `--model=X`, separate `-m` where
/// declared) are fixture-proven for the backend. `Ambiguous` covers glued
/// `-mVAL` / `-m=VAL` tokens: their parser acceptance is NOT fixture-proven
/// (clap and yargs differ), so they are treated as conservative conflicts —
/// suppressing injection / rejecting set_model beats ever risking a double
/// model flag. Disambiguation for operators: payload text belongs after a
/// bare `--`; a real model choice belongs in `set_model`, not raw args.
#[derive(Debug, PartialEq)]
pub enum ModelFlagHit {
    Confirmed(String),
    Ambiguous(String),
}

impl ModelCapability {
    /// Scan flag territory — tokens BEFORE the first bare `--` delimiter —
    /// for existing spellings of this backend's model flag. Tokens after
    /// `--` are payload and never match.
    pub fn scan(&self, args: &[String]) -> Vec<ModelFlagHit> {
        let mut hits = Vec::new();
        for tok in args {
            if tok == "--" {
                break;
            }
            if tok == self.long_flag {
                hits.push(ModelFlagHit::Confirmed(tok.clone()));
                continue;
            }
            if let Some(rest) = tok.strip_prefix(self.long_flag) {
                // `--model=X` is confirmed; `--model-foo` is a different
                // flag and must not match.
                if rest.starts_with('=') {
                    hits.push(ModelFlagHit::Confirmed(tok.clone()));
                }
                continue;
            }
            let Some(short) = self.short_flag else {
                continue;
            };
            if tok == short {
                hits.push(ModelFlagHit::Confirmed(tok.clone()));
            } else if tok.strip_prefix(short).is_some_and(|rest| !rest.is_empty()) {
                // Glued value (or `=`-glued): conservative ambiguous match.
                // Long flags never reach here: `--…` fails the `-m` prefix.
                hits.push(ModelFlagHit::Ambiguous(tok.clone()));
            }
        }
        hits
    }
}

impl Backend {
    pub fn preset(&self) -> BackendPreset {
        // W2.5: shared defaults. Each arm below specifies ONLY the fields where
        // it differs from these; struct-update (`..DEFAULTS`) fills the rest, so
        // adding a new `BackendPreset` field with a common value means editing
        // just this const (plus the arms that genuinely differ) instead of all 6
        // arms. Byte-identical to the prior per-arm literals — every backend's
        // effective field value is unchanged (the value a backend used to write
        // explicitly is either still written here or equals the default below).
        const DEFAULTS: BackendPreset = BackendPreset {
            command: "",
            args: &[],
            ready_pattern: "",
            submit_key: "\r",
            inject_prefix: "",
            typed_inject: false,
            resume_mode: ResumeMode::NotSupported,
            quit_command: "exit",
            dismiss_patterns: &[],
            instructions_path: "",
            instructions_shared: false,
            inject_instructions_on_ready: false,
            ready_timeout_secs: 30,
            fresh_args: None,
            fleet_mcp_supported: true,
            redraw_after_resize: false,
        };
        match self {
            Backend::ClaudeCode => BackendPreset {
                command: "claude",
                args: &["--dangerously-skip-permissions"],
                ready_pattern: "bypass permissions|❯",
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                // Not under `.claude/rules/` to avoid double-loading: we pass this
                // file explicitly via `--append-system-prompt-file` (see spawn_flags).
                instructions_path: ".claude/agend.md",
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
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust",
                        sequence: b"\r",
                    },
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]{0,8}Yes, proceed",
                        sequence: b"\x1b[A\x1b[A\r",
                    },
                ],
                ..DEFAULTS
            },
            Backend::KiroCli => BackendPreset {
                // #7: kiro-cli 2.1.x TUI v2 does not repaint on SIGWINCH, so a
                // pane switch (which resizes the PTY) leaves it blank until the
                // next keystroke. resize_pty sends Ctrl+L to force a repaint.
                redraw_after_resize: true,
                command: "kiro-cli",
                args: &["chat", "--trust-all-tools"],
                ready_pattern:
                    "Trust All Tools active|ask a question or describe a task|/quit to exit",
                resume_mode: ResumeMode::ContinueInCwd { flag: "--resume" },
                quit_command: "/quit",
                instructions_path: ".kiro/steering/agend.md",
                // Kiro CLI auto-loads .kiro/steering/*.md as context entries since
                // its initial release (v1.20.0, 2025-11-17), so no
                // inject_instructions_on_ready (DEFAULTS = false) is needed.
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
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]{0,8}No, exit",
                        sequence: b"\x1b[B\r",
                    },
                ],
                ..DEFAULTS
            },
            Backend::Codex => BackendPreset {
                command: "codex",
                // #1626: `-c check_for_update_on_startup=false` disables codex's
                // blocking startup update modal ("Update available!"). This is a
                // per-invocation config override on the child argv — it does NOT
                // write `~/.codex/config.toml`, so it stays fleet-scoped and never
                // touches the operator's global codex config. Must precede the
                // `resume` subcommand (`-c` is a global option). Primary fix; the
                // #1069 dismiss below stays as a fallback (see its comment).
                args: &[
                    "-c",
                    "check_for_update_on_startup=false",
                    "resume",
                    "--last",
                    "--dangerously-bypass-approvals-and-sandbox",
                ],
                ready_pattern: "OpenAI Codex|›",
                // #1670: paced (typed) inject. codex's `›` input widget
                // (ratatui-style, re-renders/debounces) does not reliably commit
                // a BULK-written line before the trailing submit `\r` arrives — the
                // wake sits un-submitted and the agent never wakes; claude's `❯`
                // tolerates the same bulk bytes, which is why ci-ready auto-waking
                // worked for claude-reviewer but not codex-reviewer. Typed inject
                // writes the line in 2ms/byte chunks so the box keeps up and the
                // line commits before `\r` submits it. The actionable-wake pointer
                // (`[AGEND-MSG-PENDING]…`) is NOT a system header — it does not
                // start with `[AGEND-MSG]`/`[from:` — so it takes the CHUNKED path
                // (not the atomic-header path), i.e. it is genuinely paced. Mirrors
                // the already-typed backends (opencode/gemini/agy). Paste-race
                // hypothesis (the A/B that'd confirm is operator-vetoed); the
                // dogfood — next real ci-green→codex handoff after merge+restart —
                // is the empirical test (can't validate on this PR).
                typed_inject: true,
                instructions_path: "AGENTS.md",
                instructions_shared: true,
                ready_timeout_secs: 20,
                dismiss_patterns: &[
                    // Trust directory prompt: "Yes, continue" is pre-selected → Enter.
                    // CR (\r), not LF — Ink's keyboard reader treats CR as Enter.
                    // macOS openpty doesn't translate LF→CR on input (ConPTY does),
                    // so LF here would silently no-op on mac.
                    // Issue #468: anchored regex (see ClaudeCode comment above).
                    // #1087: `*` instead of `{0,8}` — TUI centered modals have 40+ char prefix.
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Do you trust",
                        sequence: b"\r",
                    },
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Please restart",
                        sequence: b"\r",
                    },
                    // #1069: version-update modal blocks agent until operator
                    // selects an option. "2\r" = "Skip" (least invasive).
                    // #1626 FALLBACK: the `-c check_for_update_on_startup=false`
                    // flag above normally suppresses this modal entirely, so this
                    // dismiss never fires in the happy path. Kept as belt-and-
                    // suspenders: codex silently no-ops unknown `-c` keys, so if
                    // upstream ever renames the key the flag dormant-fails and this
                    // dismiss degrades the failure from "blocking hang" to a racy
                    // (but non-blocking) auto-skip.
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Update available!",
                        sequence: b"2\r",
                    },
                ],
                // Codex: "resume --last" → fresh start drops the resume subcommand.
                // #1626: keep the `-c check_for_update_on_startup=false` override
                // in fresh mode too (no subcommand here, so order is unconstrained).
                fresh_args: Some(&[
                    "-c",
                    "check_for_update_on_startup=false",
                    "--dangerously-bypass-approvals-and-sandbox",
                ]),
                ..DEFAULTS
            },
            Backend::OpenCode => BackendPreset {
                command: "opencode",
                ready_pattern: "Ask anything|tab agents",
                inject_prefix: "\r",
                typed_inject: true,
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                instructions_path: "AGENTS.md",
                instructions_shared: true,
                ready_timeout_secs: 45,
                dismiss_patterns: &[
                    // Issue #468: anchored regex (see ClaudeCode comment above).
                    // #1069: Esc = "Skip" (don't auto-update; let operator decide).
                    // #1087: `*` instead of `{0,8}` — TUI centered modals have 40+ char prefix.
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Update Available",
                        sequence: b"\x1b",
                    },
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Skip  Confirm",
                        sequence: b"\x1b",
                    },
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Update Complete",
                        sequence: b"\r",
                    },
                    DismissPattern {
                        label: r"(?m)^[^A-Za-z\n]*Please restart",
                        sequence: b"\r",
                    },
                ],
                ..DEFAULTS
            },
            Backend::Agy => BackendPreset {
                command: "agy",
                args: &["--dangerously-skip-permissions"],
                // #987: agy's interactive TUI renders an "Antigravity CLI <ver>"
                // banner on startup (calibrated against
                // tests/fixtures/state-replay/agy-thinking.raw). The pipe-OR
                // covers post-banner "Idle" state matchable variants in case
                // future TUI iterations rename the banner.
                ready_pattern: "Antigravity CLI|Type your message",
                inject_prefix: "\r",
                typed_inject: true,
                // agy --continue is the documented resume path (matches the
                // `ResumeMode::ContinueInCwd { flag }` shape used by claude /
                // codex / kiro). Operator-verified in issue body.
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                // #1547: agy reads agent instructions from the official
                // Customization Roots dir `<workspace>/.agents/AGENTS.md`
                // (same dir as its MCP config). `create_dir_all` handles the
                // `.agents/` parent. Shared AGENTS.md format.
                instructions_path: ".agents/AGENTS.md",
                instructions_shared: true,
                ready_timeout_secs: 20,
                // #995: --dangerously-skip-permissions auto-approves tool
                // permission requests (per `agy --help`), but does NOT bypass
                // the workspace-trust prompt that fires on every fresh spawn
                // ("Do you trust the contents of this project?"). The prompt
                // pre-selects "Yes, I trust this folder" (cursor `>` marker
                // on first row), so Enter alone confirms. Mirrors Codex's
                // "Do you trust" pattern with anchored regex per #468.
                dismiss_patterns: &[DismissPattern {
                    label: r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust",
                    sequence: b"\r",
                }],
                // #1547: agy loads project-scoped MCP via the official
                // Customization Roots dir `<workspace>/.agents/mcp_config.json`
                // (operator-verified: plain + yolo both load
                // `✓ agend-terminal Tools`). `configure_agy`
                // (src/mcp_config.rs) writes that file + busts agy's HOME
                // discovery cache so recovery respawns reload the bridge. This
                // supersedes the dead `.antigravitycli/mcp_config.json` write
                // (#995 Bug 3 — agy ignored that file's `mcpServers`).
                fleet_mcp_supported: true,
                ..DEFAULTS
            },
            Backend::Grok => BackendPreset {
                command: "grok",
                // Unattended tool execution (fleet dispatch). Matches Claude's
                // --dangerously-skip-permissions / Agy's same flag.
                args: &["--always-approve"],
                // Splash + empty prompt (smoke-verified against Grok 0.2.93).
                ready_pattern: "Grok Build|❯|always-approve",
                // Full-screen TUI: bulk write does not land; paced inject required
                // (inject-focus smoke 2026-07-10).
                typed_inject: true,
                // Grok `-c/--continue` resumes the most recent session for the
                // cwd (grok 0.2.93 `--help`: "Continue the most recent session
                // for the current working directory"). It EXITs 2s ("No session
                // found for current directory") when the cwd has no session and,
                // unlike Claude, does NOT soft-fallback — so resume is GATED on
                // `has_resumable_session` (wired to `grok_session`, which probes
                // ~/.grok/sessions/<percent-encoded-cwd>/ for a session subdir).
                // `SpawnMode::downgraded_for` turns Resume→Fresh when there is
                // nothing to resume, so fresh spawns and the --agents path never
                // append a doomed `--continue`.
                resume_mode: ResumeMode::ContinueInCwd { flag: "--continue" },
                quit_command: "/exit",
                // Shared AGENTS.md (also Codex/OpenCode). Grok reads project rules
                // from AGENTS.md / Claude.md automatically.
                instructions_path: "AGENTS.md",
                instructions_shared: true,
                ready_timeout_secs: 30,
                // Project trust modal fires on first tool use in a new workdir
                // ("Run Grok Build in a project directory?"). Default cursor is
                // the accept option → single Enter. Anchored per #468. The
                // inject path submits with one CR; this dismiss covers the
                // follow-up trust modal (empirically needed for a turn to run).
                dismiss_patterns: &[DismissPattern {
                    label: r"(?m)^[^A-Za-z\n]*Run Grok Build in a project directory\?",
                    sequence: b"\r",
                }],
                // configure_grok writes <workdir>/.grok/config.toml
                // [mcp_servers.agend-terminal] (project-scoped; no HOME write).
                fleet_mcp_supported: true,
                ..DEFAULTS
            },
            // Shell and Raw have no preset behavior. `command` is `""` as a
            // sentinel — callers that need the actual spawn path should use
            // [`Backend::command_string`], which resolves Shell to `$SHELL`
            // and Raw to its stored path.
            Backend::Shell | Backend::Raw(_) => BackendPreset {
                ready_timeout_secs: 10,
                // Shell / Raw: no MCP discovery; the bridge does not apply.
                // `false` is the safe sentinel (no warning fires because
                // these backends don't go through the dispatch warning path
                // anyway — Backend::from_command returns None for raw paths).
                fleet_mcp_supported: false,
                ..DEFAULTS
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
        } else if basename == "agy" || basename.starts_with("antigravity") {
            // #987: agy (binary name) + antigravity-cli (full product name).
            // basename match handles `/usr/local/bin/agy`; prefix match
            // handles future `antigravity-foo` variants. parse_str above
            // covers the user-facing "antigravity" alias for hand-edited
            // fleet.yaml entries.
            Some(Backend::Agy)
        } else if basename == "grok" || basename.starts_with("grok-") {
            Some(Backend::Grok)
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
            // #1580: Gemini retired; Agy (its successor) carries the Google engine.
            Backend::Agy,
            Backend::Grok,
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
            Backend::Grok => {
                grok_session::has_resumable(working_dir, &grok_session::default_sessions_root())
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

    /// #2744 PR-A: the DECLARED backend's model-flag grammar. Keyed off the
    /// enum — never off a command string (`from_command` basename guessing
    /// misclassifies wrappers like `claude-wrapper.sh` and must not appear
    /// anywhere in the model path). Grammar pinned by
    /// `tests/fixtures/cli-help/` at each `calibrated_version`.
    pub fn model_capability(&self) -> Option<&'static ModelCapability> {
        const CLAUDE: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: None,
            calibrated_version: "2.1.207",
        };
        const CODEX: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: Some("-m"),
            calibrated_version: "0.144.1",
        };
        const KIRO: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: None,
            calibrated_version: "2.12.1",
        };
        const OPENCODE: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: Some("-m"),
            calibrated_version: "1.17.5",
        };
        const AGY: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: None,
            calibrated_version: "1.0.15",
        };
        const GROK: ModelCapability = ModelCapability {
            long_flag: "--model",
            short_flag: Some("-m"),
            calibrated_version: "0.2.93",
        };
        match self {
            Backend::ClaudeCode => Some(&CLAUDE),
            Backend::Codex => Some(&CODEX),
            Backend::KiroCli => Some(&KIRO),
            Backend::OpenCode => Some(&OPENCODE),
            Backend::Agy => Some(&AGY),
            Backend::Grok => Some(&GROK),
            Backend::Shell | Backend::Raw(_) => None,
        }
    }

    /// #2038/#2744: apply the fleet-resolved model intent to a spawn argv,
    /// gated on the DECLARED backend's [`ModelCapability`].
    ///
    /// - No capability (Shell/Raw/custom) → warn + skip: `bash --model X`
    ///   breaks the spawn outright, so unsupported backends fail loud here
    ///   (and hard-error in `set_model`) instead of guessing.
    /// - Existing model-flag spellings win (caller args > fleet intent,
    ///   #2038). Confirmed spellings skip silently — that precedence is by
    ///   design. Ambiguous glued spellings also skip, but WITH a warning:
    ///   fleet intent is being suppressed by a token whose parser
    ///   acceptance is unproven.
    /// - The flag pair is inserted BEFORE the first bare `--` — everything
    ///   after the delimiter is payload, not flag territory. Presets never
    ///   carry `--`, so production argv is unchanged (append position).
    /// - Formatting goes through [`Backend::format_model_arg`] (OpenCode
    ///   needs a provider prefix). Empty model is a no-op.
    pub fn push_model_arg(args: &mut Vec<String>, backend: &Backend, model: &str) {
        if model.is_empty() {
            return;
        }
        let Some(cap) = backend.model_capability() else {
            tracing::warn!(
                backend = %backend.name(),
                model = %model,
                "model intent configured for a backend with no declared model \
                 capability — skipping --model injection (#2744)"
            );
            return;
        };
        let hits = cap.scan(args);
        if !hits.is_empty() {
            if let Some(ModelFlagHit::Ambiguous(tok)) = hits
                .iter()
                .find(|h| matches!(h, ModelFlagHit::Ambiguous(_)))
            {
                tracing::warn!(
                    backend = %backend.name(),
                    token = %tok,
                    model = %model,
                    "ambiguous model-flag-like token suppresses fleet model \
                     injection; move payload after `--` or remove the token (#2744)"
                );
            }
            return;
        }
        let model_val = backend.format_model_arg(model);
        let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
        args.insert(at, cap.long_flag.to_string());
        args.insert(at + 1, model_val);
    }

    /// Display name matching the CLI command. For [`Backend::Raw`] returns the
    /// stored path verbatim (borrow tied to self).
    pub fn name(&self) -> &str {
        self.as_str()
    }

    /// Skill directory key matching `BACKEND_SKILL_DIRS`. Returns `None`
    /// for backends without a skill directory (Shell, Raw).
    pub fn skill_dir_name(&self) -> Option<&'static str> {
        match self {
            Backend::ClaudeCode => Some("claude"),
            Backend::Codex => Some("codex"),
            Backend::OpenCode => Some("opencode"),
            Backend::KiroCli => Some("kiro"),
            Backend::Agy => Some("agy"),
            Backend::Grok => Some("grok"),
            Backend::Shell | Backend::Raw(_) => None,
        }
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
            Backend::Agy => "1.0.0",
            // Patterns + trust dismiss calibrated against live PTY smoke.
            Backend::Grok => "0.2.93",
            Backend::Shell | Backend::Raw(_) => "n/a",
        }
    }
}

/// Detection helpers for `Backend::Grok`'s on-disk session storage.
///
/// Grok persists sessions under `~/.grok/sessions/<percent-encoded-cwd>/`, where
/// the cwd is percent-encoded RFC-3986-style (`/` → `%2F`; unreserved
/// `A-Za-z0-9-._~` kept) — reversible, not a hash. Inside each cwd dir live a
/// `prompt_history.jsonl` file plus one UUIDv7 SUBDIRECTORY per session. `grok
/// --continue` resumes the most recent session for the current cwd.
pub(crate) mod grok_session {
    use std::path::{Path, PathBuf};

    /// `~/.grok/sessions/`. Falls back to `$TMPDIR/.grok/sessions` when `$HOME`
    /// is unresolvable — that path almost certainly won't exist and
    /// `has_resumable` returns false, the correct conservative answer.
    pub fn default_sessions_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".grok")
            .join("sessions")
    }

    /// Whether `working_dir` has a resumable Grok session under `sessions_root`.
    ///
    /// "Resumable" = the encoded-cwd dir exists AND holds at least one session
    /// SUBDIRECTORY (a UUIDv7 dir). A cwd dir with only `prompt_history.jsonl`
    /// (a file — no session yet) is NOT resumable, matching `grok --continue`,
    /// which exits ("No session found for current directory") when there is no
    /// session dir to resume.
    pub fn has_resumable(working_dir: &Path, sessions_root: &Path) -> bool {
        let canonical = canonicalize_for_encode(working_dir);
        let session_dir = sessions_root.join(encode_session_dir(&canonical));
        let Ok(entries) = std::fs::read_dir(&session_dir) else {
            return false;
        };
        entries.flatten().any(|e| e.path().is_dir())
    }

    /// Canonicalize `working_dir` before naming the session dir (Grok resolves
    /// the real cwd), mirroring `claude_session::canonicalize_for_encode`. Falls
    /// back to the raw input on canonicalize Err so a cold spawn (cwd not yet on
    /// disk) preserves the conservative-false branch of [`has_resumable`].
    fn canonicalize_for_encode(working_dir: &Path) -> PathBuf {
        dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf())
    }

    /// Percent-encode an absolute path the way Grok names session dirs: every
    /// byte that is not an RFC-3986 unreserved char (`A-Za-z0-9-._~`) is
    /// `%`-escaped as uppercase hex, so `/` → `%2F`. Verified against the live
    /// dir `%2FUsers%2Fsuzuke%2F.agend-terminal%2Fworkspace%2Fgrok-soak`.
    pub(crate) fn encode_session_dir(path: &Path) -> String {
        let s = path.to_string_lossy();
        let mut out = String::new();
        for &b in s.as_bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        fn unique_tmp(label: &str) -> PathBuf {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "agend-grok-session-test-{}-{}-{}",
                std::process::id(),
                label,
                id
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn encode_session_dir_matches_groks_percent_scheme() {
            // Real dir name observed under ~/.grok/sessions/ on macOS.
            assert_eq!(
                encode_session_dir(Path::new(
                    "/Users/suzuke/.agend-terminal/workspace/grok-soak"
                )),
                "%2FUsers%2Fsuzuke%2F.agend-terminal%2Fworkspace%2Fgrok-soak"
            );
            // A branch slash in a worktree cwd also encodes to %2F; `.`/`-` kept.
            assert_eq!(
                encode_session_dir(Path::new("/w/fix/grok-resume.wiring")),
                "%2Fw%2Ffix%2Fgrok-resume.wiring"
            );
        }

        #[test]
        fn has_resumable_true_only_when_a_session_subdir_exists() {
            let root = unique_tmp("root");
            // The cwd we probe never exists on disk, so canonicalize is a no-op
            // fallback and the encoded name matches what has_resumable computes.
            let wd = Path::new("/nonexistent/grok/cwd/xyz");

            // No encoded-cwd dir at all → not resumable.
            assert!(!has_resumable(wd, &root));

            // Encoded-cwd dir with ONLY prompt_history.jsonl (no session) → false.
            let enc = root.join(encode_session_dir(wd));
            std::fs::create_dir_all(&enc).unwrap();
            std::fs::write(enc.join("prompt_history.jsonl"), b"{}\n").unwrap();
            assert!(
                !has_resumable(wd, &root),
                "a cwd dir with only prompt_history.jsonl has no session to resume"
            );

            // Add a UUIDv7 session SUBDIR → resumable.
            std::fs::create_dir_all(enc.join("019f4a82-05c2-7263-a0e7-8fcdf1b4fe95")).unwrap();
            assert!(
                has_resumable(wd, &root),
                "a session subdir makes the cwd resumable"
            );
        }

        #[test]
        fn backend_grok_gates_resume_on_real_sessions() {
            // Wiring pin: `Backend::Grok` no longer optimistically returns `true`
            // (the old `_ => true` arm) — a unique cwd with no grok session under
            // the real ~/.grok/sessions/ resolves to false, so `downgraded_for`
            // turns Resume→Fresh instead of appending a doomed `--continue`.
            let wd = unique_tmp("no-session");
            assert!(!crate::backend::Backend::Grok.has_resumable_session(&wd));
        }
    }
}

/// Detection helpers for `Backend::ClaudeCode`'s on-disk session storage.
///
/// Claude Code persists every conversation under
/// `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. The encoding is
/// undocumented but stable in practice: every char that isn't `[A-Za-z0-9-]`
/// is replaced with `-` (so `/Users/x/.foo/bar` → `-Users-x--foo-bar`).
// #2234 (B): `pub(crate)` so the workspace-as-worktree reconcile e2e
// (`worktree_pool::tests`) can assert the production session-locating path
// (`has_resumable` + `encode_project_dir`) survives reconcile byte-identically —
// the #1919 property. Pure read-only fns; no new mutation surface.
pub(crate) mod claude_session {
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
    pub(crate) fn encode_project_dir(path: &Path) -> String {
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

/// #8 Phase 1 (parent t-20260530160634485744-8): a behavior facade over the
/// [`Backend`] enum. This is a PURE STRUCTURAL SCAFFOLD — every method delegates
/// VERBATIM to the existing inherent method / free function; NO detection logic
/// moves here (that is Phase 2). The seam exists so Phase 2 (per-backend
/// detection), #1580 (retire Gemini), and #7 (Kiro redraw quirk) can land
/// without churning call sites.
///
/// Dispatch is HAND-ROLLED (`impl BackendBehavior for Backend`), not the
/// `enum_dispatch` crate. Rationale: `enum_dispatch` generates the enum→trait
/// fan-out by requiring one concrete TYPE per variant, but `Backend` is a flat
/// data enum (incl. `Raw(String)`), so adopting it would mean restructuring
/// `Backend` into per-variant structs — a large, behavior-risky change that has
/// no place in a zero-behavior-change Phase 1 — plus a proc-macro dependency
/// (audit + build cost). The hand-rolled impl delegating to the already-
/// exhaustive `match self` sites is strictly simpler and truly zero-change. The
/// exhaustiveness stays compiler-checked in those existing matches (`preset`,
/// `should_anchor_on_red`, `has_resumable_session`, `StatePatterns::for_backend`).
///
/// Trait method set is the REAL per-backend behavior that exists today; the
/// task's `redraw_after_resize` is intentionally NOT added — there is no current
/// per-backend redraw logic to delegate to (adding one would be new behavior),
/// so it is deferred to #7 which introduces it.
///
/// `#[allow(dead_code)]`: by design no PRODUCTION call site uses the facade in
/// Phase 1 (call sites still hit the inherent methods, unchanged — that is the
/// zero-behavior-change guarantee). Phase 2 migrates them onto this trait. The
/// `backend_behavior_delegates_verbatim` test exercises + parity-checks it now.
#[allow(dead_code)]
pub trait BackendBehavior {
    /// Static config table for this backend. Delegates to [`Backend::preset`].
    fn preset(&self) -> BackendPreset;
    /// Classify a PTY-output frame into a blocked state, if any. Delegates to
    /// the existing free fn [`crate::state::classify_pty_output`].
    fn detect_state(&self, output: &str) -> Option<crate::health::BlockedReason>;
    /// Whether this backend has a resumable session in `working_dir`. Delegates
    /// to [`Backend::has_resumable_session`].
    fn has_resumable_session(&self, working_dir: &std::path::Path) -> bool;
    /// Whether state detection anchors on a red-rendered line (#1450). Delegates
    /// to [`Backend::should_anchor_on_red`].
    fn should_anchor_on_red(&self) -> bool;
    /// #8 Phase 2: the co-located [`crate::backend_profile::BackendProfile`] for
    /// this backend, or `None` while it's still on the legacy match path. The
    /// Phase-2 seam — the migration train routes migrated backends through this.
    fn profile(&self) -> &'static crate::backend_profile::BackendProfile;
}

impl BackendBehavior for Backend {
    fn preset(&self) -> BackendPreset {
        Backend::preset(self)
    }
    fn detect_state(&self, output: &str) -> Option<crate::health::BlockedReason> {
        crate::state::classify_pty_output(self, output)
    }
    fn has_resumable_session(&self, working_dir: &std::path::Path) -> bool {
        Backend::has_resumable_session(self, working_dir)
    }
    fn should_anchor_on_red(&self) -> bool {
        Backend::should_anchor_on_red(self)
    }
    fn profile(&self) -> &'static crate::backend_profile::BackendProfile {
        crate::backend_profile::profile(self)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #2413 Phase D: agy joins claude as a hook backend (its `.agents/hooks.json`
    /// lifecycle hooks emit shadow Evidence). This gates socket-env injection +
    /// hook promotion. Every other backend stays heuristic-only.
    #[test]
    fn agy_and_claude_have_state_hooks_others_do_not() {
        assert!(
            Backend::Agy.has_state_hooks(),
            "agy is a hook backend (#2413 Phase D)"
        );
        assert!(Backend::ClaudeCode.has_state_hooks());
        for b in [
            Backend::Codex,
            Backend::OpenCode,
            Backend::KiroCli,
            Backend::Grok,
            Backend::Shell,
            Backend::Raw("x".into()),
        ] {
            assert!(!b.has_state_hooks(), "{b:?} must stay heuristic-only");
        }
    }

    /// #8 Phase 1 parity proof: the `BackendBehavior` facade returns EXACTLY
    /// what the existing inherent methods / free fn return for every variant —
    /// i.e. the trait delegates verbatim and nothing moved. (Also exercises the
    /// otherwise-unused scaffold trait.) `<Backend as BackendBehavior>::m(&b)`
    /// hits the trait; `b.m()` hits the inherent (inherent wins method
    /// resolution) — they must agree.
    #[test]
    fn backend_behavior_delegates_verbatim() {
        let sample = "ThrottlingError: Too Many Requests";
        let tmp = std::env::temp_dir();
        for b in [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Agy,
            Backend::Grok,
            Backend::Shell,
            Backend::Raw("x".to_string()),
        ] {
            assert_eq!(
                BackendBehavior::should_anchor_on_red(&b),
                b.should_anchor_on_red(),
                "should_anchor_on_red parity for {b:?}"
            );
            assert_eq!(
                BackendBehavior::detect_state(&b, sample),
                crate::state::classify_pty_output(&b, sample),
                "detect_state parity for {b:?}"
            );
            assert_eq!(
                BackendBehavior::has_resumable_session(&b, &tmp),
                b.has_resumable_session(&tmp),
                "has_resumable_session parity for {b:?}"
            );
            // BackendPreset isn't PartialEq; compare a stable field.
            assert_eq!(
                BackendBehavior::preset(&b).command,
                b.preset().command,
                "preset parity for {b:?}"
            );
        }
    }

    // #7: redraw_after_resize is set on EXACTLY one backend (Kiro). If a new
    // backend ever needs it, flip it deliberately + update this pin.
    #[test]
    fn redraw_after_resize_is_kiro_only_7() {
        assert!(
            Backend::KiroCli.preset().redraw_after_resize,
            "Kiro must opt into redraw-after-resize"
        );
        for b in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Agy,
            Backend::Grok,
            Backend::Shell,
            Backend::Raw("x".to_string()),
        ] {
            assert!(
                !b.preset().redraw_after_resize,
                "{b:?} must NOT opt into redraw-after-resize"
            );
        }
    }

    #[test]
    fn grok_preset_mvp_inject_and_trust() {
        let p = Backend::Grok.preset();
        assert_eq!(p.command, "grok");
        assert_eq!(p.args, &["--always-approve"]);
        assert!(p.typed_inject, "Grok TUI requires paced inject");
        assert_eq!(p.submit_key, "\r");
        assert_eq!(p.instructions_path, "AGENTS.md");
        assert!(p.instructions_shared);
        assert!(p.fleet_mcp_supported);
        // Grok resumes the most recent cwd session via `--continue`, gated by
        // `has_resumable_session` (grok_session probe of ~/.grok/sessions/).
        assert!(matches!(
            p.resume_mode,
            ResumeMode::ContinueInCwd { flag: "--continue" }
        ));
        assert_eq!(p.resume_mode.args_for(), vec!["--continue".to_string()]);
        let trust = p
            .dismiss_patterns
            .iter()
            .find(|dp| dp.label.contains("project directory"))
            .expect("Grok must dismiss project-directory trust modal");
        assert_eq!(trust.sequence, b"\r");
        assert_eq!(Backend::Grok.input_prompt_marker(), Some("❯"));
        assert_eq!(Backend::Grok.credential_env_keys(), &["XAI_API_KEY"]);
    }

    #[test]
    fn from_command_detection() {
        assert_eq!(Backend::from_command("claude"), Some(Backend::ClaudeCode));
        assert_eq!(Backend::from_command("kiro-cli"), Some(Backend::KiroCli));
        assert_eq!(Backend::from_command("codex"), Some(Backend::Codex));
        assert_eq!(Backend::from_command("opencode"), Some(Backend::OpenCode));
        // #1580: gemini retired — `gemini` no longer maps to a managed backend.
        assert_eq!(Backend::from_command("gemini"), None);
        assert_eq!(Backend::from_command("agy"), Some(Backend::Agy));
        assert_eq!(Backend::from_command("grok"), Some(Backend::Grok));
        assert_eq!(
            Backend::from_command("/Users/x/.grok/bin/grok"),
            Some(Backend::Grok)
        );
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

        // #987 + #995: agy mirrors the existing preset shape — verify command,
        // args, and the dangerously-skip-permissions flag. #995 added a
        // workspace-trust dismiss_pattern after live-spawn proved the flag
        // doesn't bypass the trust-folder prompt.
        let agy = Backend::Agy.preset();
        assert_eq!(agy.command, "agy");
        assert!(agy.args.contains(&"--dangerously-skip-permissions"));
        assert_eq!(agy.dismiss_patterns.len(), 1, "#995 trust dismiss");
        assert!(agy.dismiss_patterns[0].label.contains("Yes, I trust"));
        assert_eq!(agy.dismiss_patterns[0].sequence, b"\r");
        // #1547: agy instructions live in the official Customization Roots dir
        // alongside its MCP config (was "AGY.md" pre-un-gate).
        assert_eq!(agy.instructions_path, ".agents/AGENTS.md");
    }

    /// #995 Bug 3: `fleet_mcp_supported` flag pins which backends ship with
    /// the `agend-mcp-bridge` working in fleet mode. Agy was `false` under
    /// #995 Bug 3 because its MCP discovery ignored
    /// `<workdir>/.antigravitycli/mcp_config.json`'s `mcpServers` field
    /// (only HOME-level loads, which the scope rule at
    /// `src/mcp_config.rs:5-11` forbids). #1547 fixed this: Agy now loads
    /// the bridge via `<workspace>/.agents/mcp_config.json` (the official
    /// Customization Roots dir), so `fleet_mcp_supported` is `true` for
    /// every backend except the no-MCP-discovery sentinels (Shell, Raw).
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
        // #1547: Agy now loads the bridge via `<workspace>/.agents/mcp_config.json`
        // (official Customization Roots; configure_agy writes it). Was `false`
        // under #995 Bug 3 (agy ignored the old `.antigravitycli/` write).
        assert!(Backend::Agy.preset().fleet_mcp_supported);
        // Shell / Raw — no MCP discovery; sentinel `false`.
        assert!(!Backend::Shell.preset().fleet_mcp_supported);
        assert!(
            !Backend::Raw("/opt/foo".to_string())
                .preset()
                .fleet_mcp_supported
        );
    }

    /// W2.5 byte-identity regression guard for the `DEFAULTS` merge. Pins every
    /// field a backend now INHERITS from the shared `DEFAULTS` (or that the
    /// default-merge could silently flip). The field-specific tests above cover
    /// command/args/ready_pattern/dismiss/resume; this table covers the rest, so
    /// a wrong `DEFAULTS` value (which would change a preset's effective output
    /// without tripping those) is caught here. The expected column equals the
    /// pre-refactor per-arm literal — reviewer can read it to verify all 6
    /// presets stayed equivalent.
    #[test]
    fn preset_default_merged_fields_byte_identical_w2_5() {
        // (backend, submit_key, inject_prefix, typed_inject, quit_command,
        //  instructions_shared, inject_instructions_on_ready, ready_timeout_secs,
        //  fresh_args.is_some(), fleet_mcp_supported, redraw_after_resize)
        // Parallel arrays (aligned to `backends`) of the PRE-refactor per-arm
        // literal for each default-merged field. A wrong `DEFAULTS` value would
        // change a backend's effective output without tripping the
        // field-specific tests above — caught here, reviewer-scannable.
        let backends = [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Codex,
            Backend::OpenCode,
            Backend::Agy,
            Backend::Grok,
            Backend::Shell,
            Backend::Raw("/x".to_string()),
        ];
        let submit_key = ["\r"; 8];
        let inject_prefix = ["", "", "", "\r", "\r", "", "", ""];
        let typed_inject = [false, false, true, true, true, true, false, false];
        let quit_command = [
            "/exit", "/quit", "exit", "/exit", "/exit", "/exit", "exit", "exit",
        ];
        let instructions_shared = [false, false, true, true, true, true, false, false];
        let inject_on_ready = [false; 8];
        let ready_timeout = [30u64, 30, 20, 45, 20, 30, 10, 10];
        let fresh_some = [false, false, true, false, false, false, false, false];
        let fleet_mcp = [true, true, true, true, true, true, false, false];
        let redraw = [false, true, false, false, false, false, false, false];
        for (i, b) in backends.iter().enumerate() {
            let p = b.preset();
            assert_eq!(p.submit_key, submit_key[i], "submit_key {b:?}");
            assert_eq!(p.inject_prefix, inject_prefix[i], "inject_prefix {b:?}");
            assert_eq!(p.typed_inject, typed_inject[i], "typed_inject {b:?}");
            assert_eq!(p.quit_command, quit_command[i], "quit_command {b:?}");
            assert_eq!(
                p.instructions_shared, instructions_shared[i],
                "instructions_shared {b:?}"
            );
            assert_eq!(
                p.inject_instructions_on_ready, inject_on_ready[i],
                "inject_on_ready {b:?}"
            );
            assert_eq!(
                p.ready_timeout_secs, ready_timeout[i],
                "ready_timeout_secs {b:?}"
            );
            assert_eq!(p.fresh_args.is_some(), fresh_some[i], "fresh_args {b:?}");
            assert_eq!(
                p.fleet_mcp_supported, fleet_mcp[i],
                "fleet_mcp_supported {b:?}"
            );
            assert_eq!(
                p.redraw_after_resize, redraw[i],
                "redraw_after_resize {b:?}"
            );
        }
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
            .find(|dp| dp.label.contains("Yes, I trust"))
            .expect("ClaudeCode must have a `Yes, I trust` dismiss pattern");

        assert_eq!(
            trust.sequence, b"\r",
            "#996 Phase 1: trust-prompt keystroke must be single Enter"
        );

        // Negative pin: no ESC bytes allowed — historical up-arrow (\x1b[A)
        // and any other CSI sequence in the keystroke is a regression.
        assert!(
            !trust.sequence.contains(&0x1b),
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
            .find(|dp| dp.label.contains("No, exit"))
            .expect("KiroCli must have a `No, exit` dismiss pattern");

        // Positive pin: exact keystroke shape Down + Enter
        assert_eq!(
            entry.sequence, b"\x1b[B\r",
            "#996 Phase 2a: kiro trust-all-tools dismiss must walk off the \
             destructive default (\\x1b[B = Down) before confirming (\\r). \
             Empirical evidence: kiro-tooluse.raw byte offsets 5900 + 6691 \
             show the modal opens with cursor on `No, exit`."
        );

        // Negative pin: must NOT collapse to bare Enter — that would EXIT kiro
        // on the empirically-verified destructive default.
        assert_ne!(
            entry.sequence, b"\r",
            "#996 Phase 2a regression guard: bare Enter would select kiro's \
             destructive default (`No, exit`). Verify `kiro-tooluse.raw` \
             fixture before changing the keystroke shape."
        );

        // Negative pin: must START with ESC + `[B` (Down arrow CSI), not
        // some other CSI sequence. Defends against off-by-one keystroke
        // typos like `\x1b[A` (Up — wrong direction).
        assert!(
            entry.sequence.starts_with(b"\x1b[B"),
            "#996 Phase 2a: keystroke must start with Down-arrow CSI \
             (\\x1b[B), got: {:?}",
            entry.sequence
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
        // #995: agy display name is the product short form, not the binary.
        // The binary (`agy`) is exposed via preset().command instead.
        assert_eq!(Backend::Agy.name(), "antigravity-cli");
        assert_eq!(Backend::Agy.preset().command, "agy");
    }

    #[test]
    fn all_backends_returns_six() {
        // #987: 5 → 6 with Agy; #1580: back to 5 (gemini retired); Grok MVP: 6.
        // ClaudeCode, KiroCli, Codex, OpenCode, Agy, Grok.
        assert_eq!(Backend::all().len(), 6);
        assert!(Backend::all().contains(&Backend::Grok));
    }

    #[test]
    fn parse_str_known_presets() {
        assert_eq!(Backend::parse_str("claude"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("claude-code"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("kiro-cli"), Backend::KiroCli);
        assert_eq!(Backend::parse_str("kiro"), Backend::KiroCli);
        assert_eq!(Backend::parse_str("codex"), Backend::Codex);
        assert_eq!(Backend::parse_str("opencode"), Backend::OpenCode);
        // #1580: gemini retired — `gemini` now falls through to Raw, not a managed
        // backend.
        assert_eq!(
            Backend::parse_str("gemini"),
            Backend::Raw("gemini".to_string())
        );
        // #987: agy + antigravity + antigravity-cli all resolve to Backend::Agy.
        assert_eq!(Backend::parse_str("agy"), Backend::Agy);
        assert_eq!(Backend::parse_str("antigravity"), Backend::Agy);
        assert_eq!(Backend::parse_str("antigravity-cli"), Backend::Agy);
        assert_eq!(Backend::parse_str("grok"), Backend::Grok);
        assert_eq!(Backend::parse_str("grok-build"), Backend::Grok);
        assert_eq!(Backend::parse_str("grok-cli"), Backend::Grok);
        // Case insensitive
        assert_eq!(Backend::parse_str("Claude"), Backend::ClaudeCode);
        assert_eq!(Backend::parse_str("AGY"), Backend::Agy);
        assert_eq!(Backend::parse_str("GROK"), Backend::Grok);
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
        assert_eq!(Backend::Codex.format_model_arg("o3"), "o3");
    }

    /// #2038: `push_model_arg` appends the formatted flag pair and respects
    /// an existing caller-supplied `--model` (separate or `=`-glued form).
    #[test]
    fn push_model_arg_appends_and_dedupes_2038() {
        let mut args = vec!["--continue".to_string()];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "claude-opus-4-8");
        assert_eq!(args, vec!["--continue", "--model", "claude-opus-4-8"]);

        // OpenCode gets the provider prefix via format_model_arg.
        let mut args = Vec::new();
        Backend::push_model_arg(&mut args, &Backend::OpenCode, "opus");
        assert_eq!(args, vec!["--model", "anthropic/opus"]);

        // Caller already passed --model (separate form) — no duplicate.
        let mut args = vec!["--model".to_string(), "explicit".to_string()];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
        assert_eq!(args, vec!["--model", "explicit"]);

        // Glued form counts too.
        let mut args = vec!["--model=explicit".to_string()];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
        assert_eq!(args, vec!["--model=explicit"]);

        // Empty model is a no-op.
        let mut args = vec!["--continue".to_string()];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "");
        assert_eq!(args, vec!["--continue"]);
    }

    /// #2744 PR-A: scan classification — Confirmed (fixture-proven
    /// spellings) vs Ambiguous (glued short, conservative), plus the
    /// false-positive pins: `--max-turns` (grok help) and `--model-foo`
    /// must never match.
    #[test]
    fn model_capability_scan_classifies_hits_2744() {
        let cap = Backend::Grok.model_capability().unwrap();
        let args: Vec<String> = ["--max-turns", "3", "--model-foo", "-m", "x", "-m=y", "-mz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            cap.scan(&args),
            vec![
                ModelFlagHit::Confirmed("-m".into()),
                ModelFlagHit::Ambiguous("-m=y".into()),
                ModelFlagHit::Ambiguous("-mz".into()),
            ]
        );

        // Long-flag-only backend: `-m` never matches at all.
        let cap = Backend::ClaudeCode.model_capability().unwrap();
        let args: Vec<String> = ["-m", "x", "-mz"].iter().map(|s| s.to_string()).collect();
        assert!(cap.scan(&args).is_empty());
    }

    /// #2744 PR-A L2: every declared ModelCapability is pinned by a verbatim
    /// help fixture captured at its calibrated version; absent short flags
    /// are pinned by ABSENCE in the fixture (kiro has no `-m` — the spike's
    /// earlier assumption, refuted by capture, must not resurface).
    #[test]
    fn model_capability_grammar_pinned_by_help_fixtures_2744() {
        let cases: Vec<(Backend, &str)> = vec![
            (Backend::ClaudeCode, "claude-2.1.207.txt"),
            (Backend::Codex, "codex-0.144.1-root.txt"),
            (Backend::Codex, "codex-0.144.1-resume.txt"),
            (Backend::KiroCli, "kiro-cli-2.12.1-chat.txt"),
            (Backend::OpenCode, "opencode-1.17.5.txt"),
            (Backend::Agy, "agy-1.0.15.txt"),
            (Backend::Grok, "grok-0.2.93.txt"),
        ];
        for (backend, fixture) in cases {
            let cap = backend
                .model_capability()
                .unwrap_or_else(|| panic!("{backend:?} must declare a capability"));
            let path = format!(
                "{}/tests/fixtures/cli-help/{fixture}",
                env!("CARGO_MANIFEST_DIR")
            );
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"));
            assert!(
                text.contains("# Provenance:"),
                "{fixture}: fixture must carry a provenance header"
            );
            assert!(
                fixture.contains(cap.calibrated_version),
                "{fixture}: filename must carry calibrated version {}",
                cap.calibrated_version
            );
            assert!(
                text.contains(cap.long_flag),
                "{fixture}: help must declare {}",
                cap.long_flag
            );
            match cap.short_flag {
                Some(short) => assert!(
                    text.contains(&format!("{short}, {}", cap.long_flag)),
                    "{fixture}: short flag {short} must be help-declared"
                ),
                None => assert!(
                    !text.contains(&format!("-m, {}", cap.long_flag)),
                    "{fixture}: claims long-flag-only but help declares -m"
                ),
            }
        }
        assert!(Backend::Shell.model_capability().is_none());
        assert!(Backend::Raw("/opt/x".into()).model_capability().is_none());
    }

    /// #2744 PR-A: Shell/Raw (any command without a declared model
    /// capability) must never receive a blind `--model` injection — `bash
    /// --model X` fails to spawn, and an arbitrary executable's argv
    /// semantics are unknown. Reachable today via create_instance's
    /// unrestricted `model` param.
    #[test]
    fn push_model_arg_shell_raw_never_inject_2744() {
        let mut args: Vec<String> = Vec::new();
        Backend::push_model_arg(&mut args, &Backend::Shell, "opus");
        assert!(
            args.is_empty(),
            "shell must not receive --model, got {args:?}"
        );

        let mut args: Vec<String> = Vec::new();
        Backend::push_model_arg(
            &mut args,
            &Backend::Raw("/opt/custom/agent-bin".into()),
            "opus",
        );
        assert!(
            args.is_empty(),
            "raw must not receive --model, got {args:?}"
        );
    }

    /// #2744 PR-A (B8): dedupe must recognize the short `-m` spelling on
    /// backends whose CLI help declares it (codex/opencode/grok — see
    /// tests/fixtures/cli-help/) instead of appending a second model flag.
    #[test]
    fn push_model_arg_dedupes_short_m_on_declaring_backends_b8_2744() {
        for backend in [Backend::Codex, Backend::OpenCode, Backend::Grok] {
            let mut args = vec!["-m".to_string(), "explicit".to_string()];
            Backend::push_model_arg(&mut args, &backend, "from-fleet");
            assert_eq!(
                args,
                vec!["-m", "explicit"],
                "backend {backend:?}: separate -m must dedupe"
            );
        }
    }

    /// #2744 PR-A: claude/kiro-cli/agy help declares NO `-m` short flag — a
    /// `-m` token there is not a model flag, so fleet injection must still
    /// happen. Pins the per-backend alias set so the scanner never
    /// over-matches on long-flag-only backends.
    #[test]
    fn push_model_arg_ignores_short_m_on_non_declaring_backends_2744() {
        for backend in [Backend::ClaudeCode, Backend::KiroCli, Backend::Agy] {
            let mut args = vec!["-m".to_string(), "unrelated".to_string()];
            Backend::push_model_arg(&mut args, &backend, "from-fleet");
            assert_eq!(
                args,
                vec!["-m", "unrelated", "--model", "from-fleet"],
                "backend {backend:?}: undeclared -m must not suppress injection"
            );
        }
    }

    /// #2744 PR-A: `-m=X` / `-mVAL` glued spellings are a CONSERVATIVE
    /// conflict on -m-declaring backends: the glued-value acceptance is not
    /// fixture-proven per CLI (clap vs yargs differ), so suppressing
    /// injection is the fail-loud choice vs risking a double model flag.
    #[test]
    fn push_model_arg_conservative_conflict_on_glued_short_m_2744() {
        for tok in ["-m=explicit", "-mexplicit"] {
            let mut args = vec![tok.to_string()];
            Backend::push_model_arg(&mut args, &Backend::Codex, "from-fleet");
            assert_eq!(
                args,
                vec![tok],
                "glued {tok} must suppress injection (conservative conflict)"
            );
        }
    }

    /// #2744 PR-A: a bare `--` is the end-of-options delimiter. Injection
    /// must place the flag pair BEFORE it (options territory), and model
    /// tokens AFTER it are payload — never a dedupe/conflict match.
    #[test]
    fn push_model_arg_respects_double_dash_delimiter_2744() {
        // Inject before the delimiter, not appended into payload.
        let mut args = vec!["--".to_string(), "some prompt text".to_string()];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
        assert_eq!(
            args,
            vec!["--model", "from-fleet", "--", "some prompt text"]
        );

        // `--model` inside payload is prompt text, not a real flag: fleet
        // injection must still happen (before the delimiter).
        let mut args = vec![
            "--".to_string(),
            "--model".to_string(),
            "quoted".to_string(),
        ];
        Backend::push_model_arg(&mut args, &Backend::ClaudeCode, "from-fleet");
        assert_eq!(
            args,
            vec!["--model", "from-fleet", "--", "--model", "quoted"]
        );
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
            .find(|dp| dp.label.contains("Update available!"))
            .expect("#1069: codex must have an `Update available!` dismiss pattern");
        assert_eq!(
            entry.sequence, b"2\r",
            "#1069: keystroke must be `2\\r` (option 2 = Skip)"
        );
    }

    /// #1670: codex must use paced (typed) inject, and the load-bearing reason
    /// it actually paces the wake — the actionable-wake pointer is NOT a system
    /// header, so it takes the CHUNKED path in `inject_with_target`, not the
    /// atomic-header path. If `PENDING_HEADER_PREFIX` were ever changed to start
    /// with `[AGEND-MSG]`/`[from:`, the pointer would be written atomically and
    /// the pacing fix would silently regress — this test pins both halves.
    #[test]
    fn codex_uses_paced_inject_and_wake_pointer_is_not_a_system_header_1670() {
        assert!(
            Backend::Codex.preset().typed_inject,
            "#1670: codex must paced-inject so the wake line commits before submit"
        );
        // The pointer's visible (ANSI-stripped) prefix is `[AGEND-MSG-PENDING]`.
        let stripped_pointer_prefix = "[AGEND-MSG-PENDING]";
        assert!(
            crate::inbox::PENDING_HEADER_PREFIX.contains(stripped_pointer_prefix),
            "pointer builder must still emit the [AGEND-MSG-PENDING] marker"
        );
        // is_system_header in inject_with_target checks these two prefixes; the
        // PENDING pointer must match NEITHER so it takes the paced chunk path.
        assert!(
            !stripped_pointer_prefix.starts_with(crate::inbox::SYSTEM_MSG_PREFIX),
            "#1670: [AGEND-MSG-PENDING] must NOT be a system header (else paced inject regresses to atomic)"
        );
        assert!(
            !stripped_pointer_prefix.starts_with(crate::inbox::AGENT_MSG_PREFIX),
            "#1670: [AGEND-MSG-PENDING] must NOT match the agent-msg prefix"
        );
    }

    #[test]
    fn codex_update_dismiss_anchored_rejects_mid_line() {
        let codex = Backend::Codex.preset();
        let pattern = codex
            .dismiss_patterns
            .iter()
            .find(|dp| dp.label.contains("Update available!"))
            .expect("#1069: codex update dismiss pattern must exist")
            .label;
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

    /// #1626: the `-c check_for_update_on_startup=false` override must be present
    /// in BOTH spawn modes and, in Resume mode, must come BEFORE the `resume`
    /// subcommand — `-c` is a global option, so codex's clap rejects it if it
    /// trails the subcommand. Pins both the presence and the ordering so a future
    /// preset edit can't silently drop the flag or move it past `resume`.
    #[test]
    fn codex_disables_startup_update_check_before_resume() {
        for mode in [SpawnMode::Resume, SpawnMode::Fresh] {
            let argv = Backend::Codex.preset_spawn_args(mode);
            let c_idx = argv
                .iter()
                .position(|a| a == "-c")
                .unwrap_or_else(|| panic!("#1626: codex {mode:?} argv must contain `-c`"));
            assert_eq!(
                argv[c_idx + 1],
                "check_for_update_on_startup=false",
                "#1626: `-c` must be followed by the update-check override in {mode:?} mode"
            );
            if let Some(resume_idx) = argv.iter().position(|a| a == "resume") {
                assert!(
                    c_idx < resume_idx,
                    "#1626: `-c check_for_update_on_startup=false` must precede the \
                     `resume` subcommand (global option), got argv={argv:?}"
                );
            }
        }
    }

    #[test]
    fn opencode_update_available_dismiss_uses_esc() {
        let oc = Backend::OpenCode.preset();
        let entry = oc
            .dismiss_patterns
            .iter()
            .find(|dp| dp.label.contains("Update Available"))
            .expect("#1069: opencode must have an `Update Available` dismiss pattern");
        assert_eq!(
            entry.sequence, b"\x1b",
            "#1069: keystroke must be ESC (skip update, not confirm)"
        );
    }

    #[test]
    fn opencode_update_dismiss_anchored_rejects_mid_line() {
        let oc = Backend::OpenCode.preset();
        let pattern = oc
            .dismiss_patterns
            .iter()
            .find(|dp| dp.label.contains("Update Available"))
            .expect("#1069: opencode Update Available pattern must exist")
            .label;
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
        for b in [Backend::KiroCli, Backend::Codex, Backend::OpenCode] {
            assert!(
                b.spawn_flags(&dir).is_empty(),
                "{b:?} must not emit spawn flags"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
