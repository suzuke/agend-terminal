//! Codex MCP-refusal detector (task t-20260902154222470714-82348-82).
//!
//! Incident: a codex agent (Codex CLI 0.150.1) had EVERY agend-terminal MCP
//! call refused for ~15h. Its pane showed the literal line
//! `MCP tool call requires approval, but approval policy is never`. The gate
//! is Codex-internal — the daemon cannot fix it (the trigger was the
//! instance's fleet.yaml `model: gpt-5.6-luna` + `args: [-c,
//! model_reasoning_effort=xhigh]`; removing them fixed it instantly, decision
//! `d-20260902145934537160-41`). What the daemon CAN do is NOTICE: the fleet
//! only learned via `inbox_stuck_watchdog` after 30min, and that text said
//! nothing about why.
//!
//! Detection + notification ONLY. It never restarts, never injects a
//! keystroke, and never touches dispatch gating (`BlockedReason` /
//! `HealthState` are untouched — the evidence lands in a dedicated
//! [`crate::health::McpRefusalEvidence`] slot).
//!
//! False-positive controls:
//! - **codex-only**: [`refusal_literals`] is empty for every other backend, so
//!   a claude/agy/grok pane quoting the phrase is never scanned.
//! - **composer-line exclusion** (#1947 precedent): a line whose trimmed start
//!   is one of the backend's `input_line_markers` (codex = `›`) is
//!   operator/agent-typed composer text, never evidence.
//! - **edge-triggered**: the latch fires once per appearance-episode and
//!   re-arms only after the line disappears from the pane.
//!
//! Accepted residue: a codex agent writing PROSE that quotes the literal
//! (this file's own doc comment rendered in a pager, say) raises one
//! informational notice per appearance-episode. It is orchestrator-facing
//! only and changes no state and sends no keystroke.
//!
//! Why the pattern table is handler-local rather than a
//! `crate::backend_profile::BackendProfile` field: two in-flight PRs (#3486,
//! #3489) edit every profile builder in `src/backend_profile.rs`, so a third
//! edit there would conflict three ways. This is the same "pattern surface"
//! (one const, one backend→literals function) without the conflict; promote
//! it into `BackendProfile` once a second backend needs it.

use super::{PerTickHandler, TickContext};
use crate::backend::Backend;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};

/// The pane lines that mean "Codex refused an MCP tool call". The first is
/// the literal seen in the incident pane; the second is the sibling wording
/// found elsewhere in the Codex binary.
#[allow(dead_code)] // RED scaffolding: no production reader until GREEN
pub(crate) const CODEX_MCP_REFUSAL_LITERALS: &[&str] = &[
    "MCP tool call requires approval, but approval policy is never",
    "Codex delegates require approval policy",
];

/// Per-agent edge latch. `armed` = the next appearance of a refusal line
/// notifies immediately; firing disarms; the line vanishing re-arms.
struct RefusalLatch {
    armed: bool,
}

impl Default for RefusalLatch {
    fn default() -> Self {
        Self { armed: true }
    }
}

pub(crate) struct CodexMcpRefusalHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    states: Mutex<HashMap<String, RefusalLatch>>,
}

impl CodexMcpRefusalHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for CodexMcpRefusalHandler {
    fn name(&self) -> &'static str {
        "codex_mcp_refusal"
    }

    fn run(&self, _ctx: &TickContext<'_>) {
        // RED scaffolding: no behavior yet (see the RED commit body).
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::AgentConfig;
    use std::sync::Arc;

    const REFUSAL: &str = CODEX_MCP_REFUSAL_LITERALS[0];
    const SIBLING: &str = CODEX_MCP_REFUSAL_LITERALS[1];

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-codex-refusal-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A team so the orchestrator (lead) is resolvable for the notice.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&dir),
            concat!(
                "instances:\n",
                "  lead:\n",
                "    backend: claude\n",
                "  worker:\n",
                "    backend: codex\n",
                "    model: gpt-5.6-luna\n",
                "    args: [\"-c\", \"model_reasoning_effort=xhigh\"]\n",
                "teams:\n",
                "  t:\n",
                "    members: [worker, lead]\n",
                "    orchestrator: lead\n",
            ),
        )
        .unwrap();
        dir
    }

    fn agent_config(name: &str, backend: Backend) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            backend: Some(backend),
            backend_command: "codex".to_string(),
            args: Vec::new(),
            env: None,
            working_dir: None,
            submit_key: "\r".to_string(),
        }
    }

    /// A codex pane whose tool-result line carries `body`, under a healthy
    /// `› ` composer line. `\x1b[2J\x1b[H` clears first so successive frames
    /// replace each other (the re-arm test needs a genuinely clean screen).
    fn tool_result_frame(body: &str) -> String {
        format!("\x1b[2J\x1b[H  \u{2514} {body}\r\n\r\n\u{203a} \r\n")
    }

    fn healthy_frame() -> String {
        "\x1b[2J\x1b[H  \u{2514} tool call ok\r\n\r\n\u{203a} \r\n".to_string()
    }

    /// The literal typed into the composer itself — never evidence (#1947).
    fn composer_frame(body: &str) -> String {
        format!("\x1b[2J\x1b[H  \u{2514} tool call ok\r\n\r\n\u{203a} {body}\r\n")
    }

    struct Harness {
        home: std::path::PathBuf,
        registry: crate::agent::AgentRegistry,
        externals: crate::agent::ExternalRegistry,
        configs: Arc<Mutex<HashMap<String, AgentConfig>>>,
        core: Arc<crate::sync_audit::CoreMutex<crate::agent::AgentCore>>,
        _reader: Box<dyn std::io::Read + Send>,
    }

    impl Harness {
        fn new(tag: &str, backend: Backend) -> Self {
            let home = tmp_home(tag);
            let registry: crate::agent::AgentRegistry =
                Arc::new(Mutex::new(std::collections::HashMap::new()));
            let (handle, reader) = crate::daemon::per_tick::mock_live_agent_no_context("worker");
            let core = Arc::clone(&handle.core);
            registry.lock().insert(handle.id, handle);
            let configs: Arc<Mutex<HashMap<String, AgentConfig>>> =
                Arc::new(Mutex::new(HashMap::new()));
            configs
                .lock()
                .insert("worker".to_string(), agent_config("worker", backend));
            Self {
                home,
                registry,
                externals: Arc::new(Mutex::new(std::collections::HashMap::new())),
                configs,
                core,
                _reader: reader,
            }
        }

        fn feed(&self, frame: &str) {
            self.core.lock().vterm.process(frame.as_bytes());
        }

        fn tick(&self, h: &CodexMcpRefusalHandler) {
            let ctx = TickContext {
                home: &self.home,
                registry: &self.registry,
                externals: &self.externals,
                configs: &self.configs,
            };
            h.run(&ctx);
        }

        /// Drain `lead` and return only the refusal notices (drain is
        /// destructive, so each call reports what THIS tick produced).
        fn notices(&self) -> Vec<String> {
            crate::inbox::drain(&self.home, "lead")
                .into_iter()
                .map(|m| m.text)
                .filter(|t| t.contains("[codex_mcp_refusal]"))
                .collect()
        }

        fn evidence(&self) -> Option<crate::health::McpRefusalEvidence> {
            self.core.lock().health.last_mcp_refusal.clone()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    /// (a) One refusal line → exactly one notice naming the agent, the pane
    /// line and the instance's fleet.yaml model/args; evidence recorded; a
    /// second tick on the SAME frame is silent (edge-triggered).
    #[test]
    fn refusal_line_raises_exactly_one_notice() {
        let h = Harness::new("one", Backend::Codex);
        h.feed(&tool_result_frame(REFUSAL));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);

        let notices = h.notices();
        assert_eq!(
            notices.len(),
            1,
            "exactly one codex_mcp_refusal notice must reach the orchestrator: {notices:?}"
        );
        let text = &notices[0];
        assert!(text.contains("'worker'"), "must name the agent: {text}");
        assert!(text.contains(REFUSAL), "must quote the pane line: {text}");
        assert!(
            text.contains("model=gpt-5.6-luna"),
            "must carry the instance model override: {text}"
        );
        assert!(
            text.contains("args=") && text.contains("model_reasoning_effort=xhigh"),
            "must carry the instance args override: {text}"
        );

        let ev = h.evidence().expect("health must carry refusal evidence");
        assert!(
            ev.line.contains(REFUSAL),
            "evidence line must be the pane line: {}",
            ev.line
        );

        h.tick(&handler);
        assert!(
            h.notices().is_empty(),
            "the same still-present line must NOT re-fire (edge-triggered)"
        );
    }

    /// (b) Negative control: a healthy codex pane raises nothing.
    #[test]
    fn healthy_frame_raises_nothing() {
        let h = Harness::new("healthy", Backend::Codex);
        h.feed(&healthy_frame());
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);
        assert!(h.notices().is_empty(), "healthy pane must be silent");
        assert!(h.evidence().is_none(), "healthy pane records no evidence");
    }

    /// (c) The latch re-arms once the line disappears: two appearance
    /// episodes → two notices.
    #[test]
    fn line_re_arms_after_it_disappears() {
        let h = Harness::new("rearm", Backend::Codex);
        let handler = CodexMcpRefusalHandler::new(1);

        h.feed(&tool_result_frame(REFUSAL));
        h.tick(&handler);
        let first = h.notices().len();

        h.feed(&healthy_frame());
        h.tick(&handler);
        let quiet = h.notices().len();

        h.feed(&tool_result_frame(REFUSAL));
        h.tick(&handler);
        let second = h.notices().len();

        assert_eq!(first, 1, "first episode fires once");
        assert_eq!(quiet, 0, "the line's disappearance is silent (it re-arms)");
        assert_eq!(second, 1, "the second episode fires again");
        assert_eq!(
            first + quiet + second,
            2,
            "two episodes → two notices total"
        );
    }

    /// (d) FP control: the literal typed on the `›` composer line is
    /// operator/agent-typed text, never evidence (#1947 precedent).
    #[test]
    fn composer_typed_line_is_not_evidence() {
        let h = Harness::new("composer", Backend::Codex);
        h.feed(&composer_frame(REFUSAL));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);
        assert!(
            h.notices().is_empty(),
            "a composer-line match must never fire"
        );
        assert!(h.evidence().is_none(), "and must record no evidence");
    }

    /// (e) FP control: the detector is codex-only.
    #[test]
    fn non_codex_backend_is_ignored() {
        let h = Harness::new("nonocodex", Backend::ClaudeCode);
        h.feed(&tool_result_frame(REFUSAL));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);
        assert!(
            h.notices().is_empty(),
            "a non-codex backend is never scanned"
        );
        assert!(h.evidence().is_none(), "and records no evidence");
    }

    /// (f) The sibling wording from the Codex binary is detected too.
    #[test]
    fn sibling_literal_is_detected() {
        let h = Harness::new("sibling", Backend::Codex);
        h.feed(&tool_result_frame(SIBLING));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);
        let notices = h.notices();
        assert_eq!(
            notices.len(),
            1,
            "the sibling literal must fire one notice: {notices:?}"
        );
        assert!(
            notices[0].contains(SIBLING),
            "and must quote it: {}",
            notices[0]
        );
    }
}
