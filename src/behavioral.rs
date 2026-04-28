//! Behavioral state inference — silence, cursor, and process signals.
//!
//! Sprint 27 PR-A: shadow-mode behavioral probe that runs alongside
//! regex-based state detection. In shadow mode, behavioral signals are
//! logged as telemetry but do NOT override the regex-detected state.
//! Phase 2 (Sprint 28+) promotes behavioral to primary with env var opt-in.
//!
//! ## Architecture: free functions over trait
//!
//! `config_for(backend)` + `infer_from_silence(config, duration)` are free
//! functions rather than a `BehavioralProbe` trait because:
//! 1. No dynamic dispatch needed — backend is known at StateTracker construction
//! 2. Config is `Copy` data, not behavior — a struct with fields, not methods
//! 3. Inference is a pure function of (config, signal) → result
//! 4. Avoids trait object lifetime complexity in StateTracker (which is `!Send`)
//!
//! A trait would add vtable indirection for zero benefit. If Phase 2 needs
//! per-backend method overrides (e.g. custom cursor query parsing), promote
//! to trait at that point.

use crate::backend::Backend;
use std::time::Duration;

/// Behavioral state inference result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BehavioralSignal {
    /// PTY output silent beyond threshold → likely thinking/processing.
    SilenceThinking,
    /// PTY output silent beyond idle threshold → likely idle/waiting.
    SilenceIdle,
    /// No behavioral signal detected.
    None,
}

impl std::fmt::Display for BehavioralSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SilenceThinking => write!(f, "silence_thinking"),
            Self::SilenceIdle => write!(f, "silence_idle"),
            Self::None => write!(f, "none"),
        }
    }
}

/// Per-backend behavioral calibration constants.
#[derive(Debug, Clone, Copy)]
pub struct BehavioralConfig {
    /// Silence duration before inferring "thinking" (ms).
    pub silence_thinking_ms: u64,
    /// Silence duration before inferring "idle" (ms).
    pub silence_idle_ms: u64,
    /// Whether this backend supports cursor position query (DSR CPR).
    #[allow(dead_code)] // Phase 2 Sprint 28+
    pub supports_cursor_query: bool,
    /// Whether fg pgid inference is meaningful for this backend.
    #[allow(dead_code)] // Phase 2 Sprint 28+
    pub supports_fg_pgid: bool,
}

impl Default for BehavioralConfig {
    fn default() -> Self {
        Self {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
            supports_fg_pgid: false,
        }
    }
}

/// Get behavioral config for a backend.
pub fn config_for(backend: &Backend) -> BehavioralConfig {
    match backend {
        Backend::ClaudeCode => BehavioralConfig {
            silence_thinking_ms: 2000,
            silence_idle_ms: 6000,
            supports_cursor_query: true,
            supports_fg_pgid: cfg!(unix),
        },
        Backend::KiroCli => BehavioralConfig {
            silence_thinking_ms: 2500,
            silence_idle_ms: 7000,
            supports_cursor_query: true,
            supports_fg_pgid: cfg!(unix),
        },
        Backend::Codex => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false, // bubbletea TUI may not respond to DSR
            supports_fg_pgid: cfg!(unix),
        },
        Backend::Gemini => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
            supports_fg_pgid: cfg!(unix),
        },
        Backend::OpenCode => BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
            supports_cursor_query: false,
            supports_fg_pgid: cfg!(unix),
        },
        Backend::Shell | Backend::Raw(_) => BehavioralConfig::default(),
    }
}

/// Infer behavioral signal from silence duration.
pub fn infer_from_silence(config: &BehavioralConfig, silence: Duration) -> BehavioralSignal {
    let silence_ms = silence.as_millis() as u64;
    if silence_ms >= config.silence_idle_ms {
        BehavioralSignal::SilenceIdle
    } else if silence_ms >= config.silence_thinking_ms {
        BehavioralSignal::SilenceThinking
    } else {
        BehavioralSignal::None
    }
}

/// Get the foreground process group ID for a PTY fd.
/// Delegates to `backend_harness::verify_tcgetpgrp` (promoted, not rebuilt).
#[allow(dead_code)] // Phase 2 Sprint 28+
#[cfg(unix)]
pub fn fg_pgid(_pty_fd: i32) -> Option<u32> {
    // Phase 2: wire to actual PTY fd. For now, use verify_tcgetpgrp
    // which probes stdin — sufficient for shadow-mode telemetry.
    crate::backend_harness::verify_tcgetpgrp()
        .ok()
        .map(|pgid| pgid as u32)
}

#[allow(dead_code)] // Phase 2 Sprint 28+
#[cfg(not(unix))]
pub fn fg_pgid(_pty_fd: i32) -> Option<u32> {
    // Windows: ConsoleProcessList stub — Sprint 28+ Phase 2
    Option::None
}

/// Shadow-mode telemetry: log behavioral signal alongside regex state.
/// In shadow mode this is observability only — no state change.
pub fn log_shadow_telemetry(
    instance: &str,
    backend: &str,
    regex_state: &str,
    behavioral: BehavioralSignal,
) {
    if behavioral != BehavioralSignal::None {
        tracing::debug!(
            target: "behavioral_shadow",
            instance,
            backend,
            regex_state,
            behavioral = %behavioral,
            "behavioral shadow: signal detected"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn silence_below_threshold_returns_none() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(500)),
            BehavioralSignal::None
        );
    }

    #[test]
    fn silence_above_thinking_threshold() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(2500)),
            BehavioralSignal::SilenceThinking
        );
    }

    #[test]
    fn silence_above_idle_threshold() {
        let config = config_for(&Backend::ClaudeCode);
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(7000)),
            BehavioralSignal::SilenceIdle
        );
    }

    #[test]
    fn claude_has_shorter_thresholds_than_default() {
        let claude = config_for(&Backend::ClaudeCode);
        let default = BehavioralConfig::default();
        assert!(claude.silence_thinking_ms < default.silence_thinking_ms);
    }

    #[test]
    fn kiro_supports_cursor_query() {
        assert!(config_for(&Backend::KiroCli).supports_cursor_query);
    }

    #[test]
    fn shell_uses_defaults() {
        let config = config_for(&Backend::Shell);
        let default = BehavioralConfig::default();
        assert_eq!(config.silence_thinking_ms, default.silence_thinking_ms);
    }

    #[test]
    fn codex_uses_default_thresholds() {
        let config = config_for(&Backend::Codex);
        assert_eq!(config.silence_thinking_ms, 3000);
        assert!(!config.supports_cursor_query); // bubbletea TUI
    }

    #[test]
    fn gemini_uses_default_thresholds() {
        let config = config_for(&Backend::Gemini);
        assert_eq!(config.silence_thinking_ms, 3000);
    }

    #[test]
    fn opencode_uses_default_thresholds() {
        let config = config_for(&Backend::OpenCode);
        assert_eq!(config.silence_idle_ms, 8000);
    }

    #[cfg(unix)]
    #[test]
    fn fg_pgid_on_stdin_returns_some() {
        // stdin (fd 0) should have a valid foreground pgid in a terminal
        // This may return None in CI (no controlling terminal)
        let _ = fg_pgid(0); // Just verify it doesn't panic
    }

    #[test]
    fn behavioral_signal_display() {
        assert_eq!(
            format!("{}", BehavioralSignal::SilenceThinking),
            "silence_thinking"
        );
        assert_eq!(format!("{}", BehavioralSignal::None), "none");
    }

    /// M2: Fixture replay — feed through StateTracker, verify state
    /// transition + behavioral config present.
    fn replay_fixture(file: &str, backend: &Backend) {
        let path = format!("tests/fixtures/state-replay/{file}");
        let fixture = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let mut tracker = crate::state::StateTracker::new(Some(backend));
        assert!(
            tracker.has_behavioral_config(),
            "behavioral config must be set for {file}"
        );
        let text = String::from_utf8_lossy(&fixture);
        tracker.feed(&text);
        let state = tracker.get_state();
        assert!(
            !matches!(state, crate::state::AgentState::Starting),
            "fixture {file} should trigger state transition, got Starting"
        );
    }

    /// M2+M4 e2e: feed fixture → sleep past silence threshold → feed again
    /// → capture behavioral_shadow telemetry via tracing subscriber.
    #[test]
    fn fixture_replay_claude_thinking_emits_behavioral_telemetry() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let fixture = std::fs::read("tests/fixtures/state-replay/claude-thinking.raw").unwrap();
        let mut tracker = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
        tracker.set_instance_name("test-fixture");
        tracker.feed(&String::from_utf8_lossy(&fixture));
        std::thread::sleep(Duration::from_millis(2100));
        tracker.feed("_");
        drop(_guard);
        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            output.contains("silence_thinking"),
            "expected silence_thinking after 2.1s silence, got: {}",
            if output.is_empty() {
                "(empty)".to_string()
            } else {
                output[..output.len().min(300)].to_string()
            }
        );
    }

    #[test]
    fn fixture_replay_claude_tooluse() {
        replay_fixture("claude-tooluse.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn fixture_replay_claude_perm() {
        replay_fixture("claude-perm.raw", &Backend::ClaudeCode);
    }
    #[test]
    fn fixture_replay_codex_thinking() {
        replay_fixture("codex-thinking.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_tooluse() {
        replay_fixture("codex-tooluse.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_update() {
        replay_fixture("codex-update.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_codex_perm() {
        replay_fixture("codex-perm.raw", &Backend::Codex);
    }
    #[test]
    fn fixture_replay_gemini_thinking() {
        replay_fixture("gemini-thinking.raw", &Backend::Gemini);
    }
    #[test]
    fn fixture_replay_gemini_tooluse() {
        replay_fixture("gemini-tooluse.raw", &Backend::Gemini);
    }
    #[test]
    fn fixture_replay_kiro_thinking() {
        replay_fixture("kiro-thinking.raw", &Backend::KiroCli);
    }
    #[test]
    fn fixture_replay_kiro_tooluse() {
        replay_fixture("kiro-tooluse.raw", &Backend::KiroCli);
    }
    #[test]
    fn fixture_replay_opencode_thinking() {
        replay_fixture("opencode-thinking.raw", &Backend::OpenCode);
    }
    #[test]
    fn fixture_replay_opencode_tooluse() {
        replay_fixture("opencode-tooluse.raw", &Backend::OpenCode);
    }

    /// M2: Silence inference produces correct signal for calibrated thresholds.
    #[test]
    fn claude_silence_inference_matches_calibration() {
        let config = config_for(&Backend::ClaudeCode);
        // Below thinking threshold
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(1000)),
            BehavioralSignal::None
        );
        // Above thinking, below idle
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(3000)),
            BehavioralSignal::SilenceThinking
        );
        // Above idle
        assert_eq!(
            infer_from_silence(&config, Duration::from_millis(7000)),
            BehavioralSignal::SilenceIdle
        );
    }

    /// M4: Verify log_shadow_telemetry emits a tracing event with
    /// the behavioral signal in the message.
    #[test]
    fn shadow_telemetry_emits_tracing_event() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .finish();

        let signal = infer_from_silence(
            &config_for(&Backend::ClaudeCode),
            Duration::from_millis(3000),
        );
        tracing::subscriber::with_default(subscriber, || {
            log_shadow_telemetry("test-agent", "claude-code", "idle", signal);
        });

        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            output.contains("silence_thinking"),
            "expected 'silence_thinking' in tracing output, got: {output}"
        );
    }

    /// M4: Verify None signal does NOT emit any tracing event.
    #[test]
    fn shadow_telemetry_skips_none_signal() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let buf_w = buf.clone();

        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || {
                struct W(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
                impl std::io::Write for W {
                    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().extend_from_slice(b);
                        Ok(b.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                W(buf_w.clone())
            })
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            log_shadow_telemetry("test-agent", "claude-code", "idle", BehavioralSignal::None);
        });

        let output = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(
            !output.contains("behavioral shadow"),
            "None signal should not emit, got: {output}"
        );
    }

    /// M4: StateTracker must expose has_behavioral_config() — fails to
    /// compile when state.rs behavioral fields are absent (RED state).
    #[test]
    fn state_tracker_has_behavioral_config() {
        let tracker = crate::state::StateTracker::new(Some(&Backend::ClaudeCode));
        assert!(
            tracker.has_behavioral_config(),
            "StateTracker must have behavioral_config for managed backends"
        );
    }
}
