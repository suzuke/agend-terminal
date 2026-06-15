//! M2 (#2090) — progress-mirror per-tick handler.
//!
//! When `runtime_config.progress_mode == 1` (mirror, the default), this
//! handler tails each agent's Claude Code transcript and relays NEW assistant
//! *text* blocks back to the channel the agent's current request came from —
//! CLI-parity operator visibility on Telegram, WITHOUT the raw PTY/ANSI/tool
//! noise.
//!
//! Only agents with an ACTIVE external-channel turn are mirrored: the origin
//! channel is read from heartbeat-pair `reply_to_channel`, set on inbox
//! dequeue and cleared at the turn boundary. An idle agent (no origin channel)
//! is skipped, so we never spam the operator with background chatter.
//!
//! Fail-open: every per-agent step is best-effort and wrapped so one bad agent
//! can't kill the sweep. The tail itself reads only newly-appended bytes, so
//! running every tick (~10s) is cheap.

use super::{PerTickHandler, TickContext};
use crate::daemon::cadence_gate::CadenceGate;
use crate::daemon::transcript_tail::{self, TailPos};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Telegram-friendly cap on a single mirrored relay. Joined text past this is
/// truncated on a char boundary with a trailing ellipsis.
const MAX_MIRROR_LEN: usize = 3500;

pub(crate) struct ProgressMirrorHandler {
    gate: CadenceGate,
    /// Per-agent tail position (keyed by agent name). `None`-valued entries are
    /// not stored; absence == first sight.
    state: Mutex<HashMap<String, TailPos>>,
}

impl ProgressMirrorHandler {
    pub(crate) fn new() -> Self {
        Self {
            // Every tick — the cost is just reading new bytes since last offset.
            gate: CadenceGate::new(1),
            state: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for ProgressMirrorHandler {
    fn name(&self) -> &'static str {
        "progress_mirror"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Mode gate: only mirror mode (1) relays here. 0 = off, 2 = report
        // (agent owns updates) — both skip this handler.
        if crate::runtime_config::get().progress_mode != 1 {
            return;
        }
        if !self.gate.fire() {
            return;
        }

        // Snapshot (name, working_dir) under the configs lock, then release it
        // before doing any IO / channel sends.
        let agents: Vec<(String, std::path::PathBuf)> = {
            let configs = ctx.configs.lock();
            configs
                .iter()
                .filter_map(|(name, cfg)| cfg.working_dir.clone().map(|wd| (name.clone(), wd)))
                .collect()
        };

        let mut state = self.state.lock();
        for (name, working_dir) in agents {
            // Per-agent panic isolation: a single malformed transcript or
            // channel hiccup must not abort the rest of the sweep.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Self::mirror_agent(&name, &working_dir, &mut state);
            }));
            if result.is_err() {
                tracing::warn!(agent = %name, "progress_mirror: per-agent panic isolated");
            }
        }
    }
}

impl ProgressMirrorHandler {
    fn mirror_agent(
        name: &str,
        working_dir: &std::path::Path,
        state: &mut HashMap<String, TailPos>,
    ) {
        // Only mirror an agent with an ACTIVE external-channel turn.
        let Some(channel) = crate::daemon::heartbeat_pair::snapshot_for(name).reply_to_channel
        else {
            return;
        };

        // Tail the transcript for new assistant text. The state map stores the
        // TailPos directly; we lift it into an Option for the extractor and
        // write it back.
        let mut entry = state.remove(name);
        let texts = transcript_tail::extract_new_assistant_text(working_dir, &mut entry);
        if let Some(pos) = entry {
            state.insert(name.to_string(), pos);
        }
        if texts.is_empty() {
            return;
        }

        let joined = texts.join("\n\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            return;
        }
        let text = truncate_on_boundary(trimmed, MAX_MIRROR_LEN);

        // Fire-and-forget relay to the origin channel.
        if let Some(ch) = crate::channel::lookup_channel_by_name(&channel) {
            let _ = ch.send_from_agent(name, crate::channel::AgentOutboundOp::Reply { text });
        }
    }
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending " …" when
/// truncation occurs.
fn truncate_on_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …", &s[..end])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_on_boundary("hello", 3500), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis_on_boundary() {
        let s = "a".repeat(4000);
        let out = truncate_on_boundary(&s, 3500);
        assert!(out.starts_with(&"a".repeat(3500)));
        assert!(out.ends_with(" …"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // A multi-byte char straddling the cut must not be split.
        let s = format!("{}é", "a".repeat(3499)); // 'é' is 2 bytes at offset 3499..3501
        let out = truncate_on_boundary(&s, 3500);
        // Cut lands inside 'é' → backs off to 3499.
        assert_eq!(&out, &format!("{} …", "a".repeat(3499)));
    }
}
