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
use std::sync::Arc;

/// The pane lines that mean "Codex refused an MCP tool call". The first is
/// the literal seen in the incident pane; the second is the sibling wording
/// found elsewhere in the Codex binary.
pub(crate) const CODEX_MCP_REFUSAL_LITERALS: &[&str] = &[
    "MCP tool call requires approval, but approval policy is never",
    "Codex delegates require approval policy",
];

/// The refusal literals for `backend`, empty for every backend whose CLI
/// cannot produce them. Codex-only today — see the module doc for why this
/// table is handler-local instead of a `BackendProfile` field.
fn refusal_literals(backend: &Backend) -> &'static [&'static str] {
    match backend {
        Backend::Codex => CODEX_MCP_REFUSAL_LITERALS,
        _ => &[],
    }
}

/// The first screen line that carries a refusal literal and is NOT the
/// backend's composer line. Returns the trimmed line so the notice quotes it
/// without the pane's leading indentation.
fn first_refusal_line(screen: &str, literals: &[&str], markers: &[&str]) -> Option<String> {
    screen
        .lines()
        .map(str::trim)
        .find(|line| {
            !markers.iter().any(|m| line.starts_with(m))
                && literals.iter().any(|lit| line.contains(lit))
        })
        .map(str::to_string)
}

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

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }

        // Backend per agent, snapshot off the configs lock before touching the
        // registry (never two of these locks held at once).
        let backends: HashMap<String, Backend> = ctx
            .configs
            .lock()
            .iter()
            .filter_map(|(name, cfg)| cfg.backend.clone().map(|b| (name.clone(), b)))
            .collect();

        // Phase 1: per live agent, the first refusal line on its screen (or
        // `None`). The `core` lock is held only long enough to copy the screen
        // text out — no IO, no fleet load, no notify underneath it.
        let mut observed: Vec<(String, Option<String>)> = Vec::new();
        // #latch-prune (cleanup-on-delete, #1923 G5 class): capture ALL live
        // agent names, not just the codex ones, so a same-name redeploy of a
        // deleted agent never inherits a stale latch.
        let live: HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            let mut live = HashSet::new();
            for handle in reg.values() {
                let name = handle.name.as_str().to_string();
                live.insert(name.clone());
                // An agent with no AgentConfig entry has no known backend
                // and is never scanned — by design, pinned by
                // `agent_without_config_entry_is_skipped`.
                let Some(backend) = backends.get(&name) else {
                    continue;
                };
                let literals = refusal_literals(backend);
                if literals.is_empty() {
                    continue;
                }
                let markers = crate::backend_profile::profile(backend).input_line_markers;
                let screen = {
                    let core = handle.core.lock();
                    let rows = core.vterm.rows() as usize;
                    // Dewrapped so a narrow attached pane (tui_bridge.rs
                    // resizes the agent vterm to the attached client's
                    // width) cannot split the literal across soft-wrapped
                    // physical rows and hide it from the scan — the same
                    // accessor the state hot path uses (agent/mod.rs).
                    core.vterm.tail_lines_dewrapped(rows)
                };
                observed.push((name, first_refusal_line(&screen, literals, markers)));
            }
            live
        };

        // Phase 2: evaluate each latch, then fire outside every agent lock.
        let fleet = crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(ctx.home))
            .unwrap_or_else(|_| Arc::new(crate::fleet::FleetConfig::default()));
        let mut fired: Vec<(String, String)> = Vec::new();
        {
            let mut states = self.states.lock();
            for (name, line) in observed {
                let latch = states.entry(name.clone()).or_default();
                let Some(line) = line else {
                    // The line is gone — re-arm for the next episode.
                    latch.armed = true;
                    continue;
                };
                if !latch.armed {
                    continue;
                }
                latch.armed = false;
                fired.push((name, line));
            }
            // #latch-prune: drop latches for agents gone from the registry.
            states.retain(|name, _| live.contains(name));
        }

        let now = chrono::Utc::now();
        for (name, line) in fired {
            crate::event_log::log(ctx.home, "codex_mcp_refusal_detected", &name, &line);
            record_evidence(ctx.registry, &name, now, &line);

            // Notify the agent's team orchestrator. Never the agent about
            // itself — it is precisely the one that cannot act right now.
            let recipient = crate::daemon::inbox_stuck_watchdog::orchestrator_for(&fleet, &name)
                .unwrap_or_else(|| {
                    crate::daemon::inbox_stuck_watchdog::FALLBACK_RECIPIENT.to_string()
                });
            if recipient == name {
                continue;
            }
            let (model, args) = instance_overrides(&fleet, &name);
            let text = format!(
                "[codex_mcp_refusal] agent '{name}': MCP tool calls are being refused by \
                 Codex — pane line: \"{line}\". Likely cause: model/effort overrides in \
                 fleet.yaml (fleet.yaml for this instance: model={model}, args={args}); \
                 `gpt-5.6-luna` + `-c model_reasoning_effort=xhigh` on Codex 0.150.1 is a \
                 known trigger (d-20260902145934537160-41). Remove the overrides and \
                 fresh-restart. This notice is edge-triggered: it re-fires only after the \
                 line disappears and reappears."
            );
            if let Err(e) = crate::inbox::notify_system(
                ctx.home,
                &recipient,
                "system:codex_mcp_refusal",
                "codex_mcp_refusal",
                text,
                Some(&name),
                None,
            ) {
                tracing::warn!(agent = %name, %recipient, error = %e, "codex_mcp_refusal: notify failed");
                continue;
            }
            tracing::warn!(agent = %name, %recipient, %line, "codex_mcp_refusal: notified orchestrator");
        }
    }
}

/// Stamp the evidence on the agent's own `HealthTracker`. A second, brief
/// registry lock (phase 1's was already released) — deliberately NOT a
/// `BlockedReason` / `HealthState` change: this is an evidence slot the
/// inbox-stuck watchdog quotes, never a dispatch-gating signal.
fn record_evidence(
    registry: &crate::agent::AgentRegistry,
    name: &str,
    at: chrono::DateTime<chrono::Utc>,
    line: &str,
) {
    let reg = crate::agent::lock_registry(registry);
    if let Some(handle) = reg.values().find(|h| h.name.as_str() == name) {
        handle
            .core
            .lock()
            .health
            .record_mcp_refusal(at, line.to_string());
    }
}

/// The instance's fleet.yaml `model` / `args` overrides, rendered for the
/// notice. They are the actionable part: the incident was cured by removing
/// exactly these two keys.
fn instance_overrides(fleet: &crate::fleet::FleetConfig, name: &str) -> (String, String) {
    let Some(instance) = fleet.instances.get(name) else {
        return ("default".to_string(), "[]".to_string());
    };
    let model = instance
        .model
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let args = if instance.args.is_empty() {
        "[]".to_string()
    } else {
        format!("{:?}", instance.args)
    };
    (model, args)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::AgentConfig;

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

    /// (g) A narrow attached pane soft-wraps the refusal literal across
    /// physical rows; the detector must scan the dewrapped screen so the
    /// wrapped line is still caught (verifier finding: `tui_bridge.rs`
    /// resizes the agent vterm to the attached client's width).
    #[test]
    fn wrapped_refusal_line_is_still_detected() {
        let h = Harness::new("wrap", Backend::Codex);
        h.core.lock().vterm.resize(60, 10);
        h.feed(&tool_result_frame(REFUSAL));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);

        let notices = h.notices();
        assert_eq!(
            notices.len(),
            1,
            "a soft-wrapped refusal line must still raise exactly one notice: {notices:?}"
        );
        let ev = h
            .evidence()
            .expect("health must carry refusal evidence even when the pane is narrow");
        assert!(
            ev.line.contains(REFUSAL),
            "evidence line must contain the full literal, not a wrapped fragment: {}",
            ev.line
        );
    }

    /// (h) An agent with no `AgentConfig` entry has no known backend and is
    /// never scanned — by design (pinned, not a bug).
    #[test]
    fn agent_without_config_entry_is_skipped() {
        let h = Harness::new("noconfig", Backend::Codex);
        h.configs.lock().remove("worker");
        h.feed(&tool_result_frame(REFUSAL));
        let handler = CodexMcpRefusalHandler::new(1);
        h.tick(&handler);
        assert!(
            h.notices().is_empty(),
            "an agent with no AgentConfig entry must never be scanned"
        );
        assert!(h.evidence().is_none(), "and must record no evidence");
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
