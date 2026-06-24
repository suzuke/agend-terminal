//! #2413 Shadow Observer — the shared **Evidence** contract.
//!
//! An observation plane (this is the LOCAL plane: claude lifecycle hooks) never
//! intervenes; it emits typed `Evidence` into a per-agent buffer, tagged with the
//! `authority` (how the truth was learned) and a `confidence`. A later reducer
//! (Phase B, OUT OF SCOPE for this spike) consumes the buffer and normalizes it to
//! an `ObservedStatus`.
//!
//! This type is SHARED with the API/proxy plane (fixup-dev-2). Keep the serde shape
//! stable: both planes serialize/deserialize `Evidence` across the wire and into the
//! buffer.

use serde::{Deserialize, Serialize};

/// One typed observation from a plane. `authority`/`confidence` let a consumer tell a
/// `Confirmed` `Hook` turn-end apart from a `Weak` `Screen` guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Flattened so the internally-tagged `EvidenceKind` merges INTO the `Evidence`
    /// object (`{"kind":"turn_ended","stop_reason":…,"authority":"hook",…}`) — the
    /// shared wire shape both planes agreed on, not a nested `kind.kind`.
    #[serde(flatten)]
    pub kind: EvidenceKind,
    pub authority: Authority,
    pub confidence: Confidence,
    /// Capture time, epoch ms.
    pub at_ms: u64,
    /// Decay budget in ms; `0` = no expiry hint (the reducer decides). The reducer
    /// (Phase B) uses this to age stale evidence so a dropped terminal event can't
    /// wedge a phantom state.
    pub ttl_ms: u64,
}

/// The normalized observation kinds. `#[serde(tag = "kind")]` → an internally-tagged
/// JSON object, e.g. `{"kind":"tool_started","name":"Bash"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A turn began (claude `UserPromptSubmit`).
    TurnStarted,
    /// Assistant tokens are streaming (Stream plane; the Hook plane does not emit it).
    Responding,
    /// A turn ended (claude `Stop` / `StopFailure`).
    TurnEnded { stop_reason: Option<String> },
    /// A tool invocation began (claude `PreToolUse`).
    ToolStarted { name: Option<String> },
    /// A tool invocation ended (claude `PostToolUse`).
    ToolEnded,
    /// The agent is blocked awaiting an approval/permission decision.
    ApprovalRequired,
    /// Rate-limited. `retry_at_ms` is an ABSOLUTE epoch-ms instant (NOT a duration —
    /// the API plane converts the Anthropic `retry-after` delta/HTTP-date to absolute,
    /// per cross-plane agreement). NOTE: claude hooks are BLIND to rate-limiting (it is
    /// the API plane's signal); the local plane never emits this — kept in the shared
    /// contract so the API plane can.
    RateLimited { retry_at_ms: Option<u64> },
    /// Token accounting for a turn (API plane).
    TokenUsage { input: u64, output: u64 },
    /// The agent is idle at a ready prompt (claude `Notification{idle_prompt}`).
    PromptReady,
    /// The session ended (claude `SessionEnd`).
    SessionExited,
}

/// HOW the evidence was learned — drives per-transition precedence in the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// Backend lifecycle hook (this plane). The strongest local-truth source.
    Hook,
    /// A structured event stream (SSE / app-server / proxy — API plane).
    Stream,
    /// A session transcript / event-log tail.
    Transcript,
    /// The VISUAL plane: semantic detection over the shadow screen.
    Screen,
    /// Process-tree liveness (pgrep / proc children).
    ProcessHeuristic,
    /// Derived by the reducer from other evidence (e.g. Thinking).
    Inferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    Strong,
    Probable,
    Weak,
}

impl Evidence {
    /// A `Hook`-authority, `Confirmed` observation stamped now. The local plane's
    /// only constructor — every hook event is a confirmed local truth. #2433: its only
    /// PROD caller is the unix socket-ingest path, so it is dead on non-unix prod;
    /// `any(unix, test)` keeps it for unix prod + EVERY test build (the platform-agnostic
    /// reducer tests construct hook Evidence to drive the state machine).
    #[cfg(any(unix, test))]
    pub fn hook(kind: EvidenceKind, at_ms: u64) -> Self {
        Self {
            kind,
            authority: Authority::Hook,
            confidence: Confidence::Confirmed,
            at_ms,
            // Hook events are confirmed point-in-time facts; the reducer owns decay,
            // so the local plane leaves the hint at 0 (no local expiry opinion).
            ttl_ms: 0,
        }
    }

    /// A `Stream`-authority, `Strong` observation stamped at the rollout record time —
    /// the codex rollout-tail plane's constructor (#2413 Phase D). Codex (TUI) live-flushes
    /// its `~/.codex/sessions/.../rollout-*.jsonl` DURING a turn (confirm-first verified:
    /// function_call/response_item records appear mid-turn), so a tailed event is a strong
    /// real-time truth — one notch below `Hook`/`Confirmed` only because it is an
    /// after-the-fact append-tail, not a synchronous lifecycle callback. Cross-platform
    /// (std::fs tail, unlike the unix-socket hook plane) ⇒ no cfg gate.
    pub fn stream(kind: EvidenceKind, at_ms: u64) -> Self {
        Self {
            kind,
            authority: Authority::Stream,
            confidence: Confidence::Strong,
            at_ms,
            ttl_ms: 0,
        }
    }
}

/// Map a claude lifecycle hook to the `EvidenceKind` it evidences, or `None` for a
/// hook that is not a state transition (`SessionStart`, `PreCompact`). The claude
/// hook event semantics:
/// - `UserPromptSubmit` → a turn started.
/// - `PreToolUse` (carries `tool_name`) / `PostToolUse` → tool start / end.
/// - `PermissionRequest`, or `Notification` with `permission_prompt` → awaiting approval.
/// - `Notification` with `idle_prompt` → idle at a ready prompt.
/// - `Stop` → turn ended; `StopFailure` → turn ended in failure.
/// - `SessionEnd` → session exited.
///
/// Rate-limit is deliberately absent: claude hooks do not fire for it (the API plane
/// owns `RateLimited`).
///
/// #2433: only the unix socket-ingest path (+ the mapping's own tests) calls this, so it
/// is gated like the rest of the hook-ingestion plumbing (dead on non-unix prod).
#[cfg(any(unix, test))]
pub fn evidence_kind_for_hook(
    hook_event_name: &str,
    notification_type: Option<&str>,
    tool_name: Option<&str>,
) -> Option<EvidenceKind> {
    Some(match hook_event_name {
        "UserPromptSubmit" => EvidenceKind::TurnStarted,
        "PreToolUse" => EvidenceKind::ToolStarted {
            name: tool_name.map(str::to_string),
        },
        "PostToolUse" => EvidenceKind::ToolEnded,
        "PermissionRequest" => EvidenceKind::ApprovalRequired,
        "Notification" => match notification_type {
            Some("permission_prompt") => EvidenceKind::ApprovalRequired,
            Some("idle_prompt") => EvidenceKind::PromptReady,
            _ => return None,
        },
        "Stop" => EvidenceKind::TurnEnded { stop_reason: None },
        "StopFailure" => EvidenceKind::TurnEnded {
            stop_reason: Some("failure".to_string()),
        },
        "SessionEnd" => EvidenceKind::SessionExited,
        // SessionStart / PreCompact carry no state transition.
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hook_to_evidence_mapping_covers_the_contract() {
        let m = |e, n, t| evidence_kind_for_hook(e, n, t);
        assert_eq!(
            m("UserPromptSubmit", None, None),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            m("PreToolUse", None, Some("Bash")),
            Some(EvidenceKind::ToolStarted {
                name: Some("Bash".to_string())
            })
        );
        assert_eq!(m("PostToolUse", None, None), Some(EvidenceKind::ToolEnded));
        assert_eq!(
            m("PermissionRequest", None, None),
            Some(EvidenceKind::ApprovalRequired)
        );
        assert_eq!(
            m("Notification", Some("permission_prompt"), None),
            Some(EvidenceKind::ApprovalRequired)
        );
        assert_eq!(
            m("Notification", Some("idle_prompt"), None),
            Some(EvidenceKind::PromptReady)
        );
        assert_eq!(
            m("Stop", None, None),
            Some(EvidenceKind::TurnEnded { stop_reason: None })
        );
        assert_eq!(
            m("StopFailure", None, None),
            Some(EvidenceKind::TurnEnded {
                stop_reason: Some("failure".to_string())
            })
        );
        assert_eq!(
            m("SessionEnd", None, None),
            Some(EvidenceKind::SessionExited)
        );
        // Non-transition hooks + unknown notification → no evidence.
        assert_eq!(m("SessionStart", None, None), None);
        assert_eq!(m("PreCompact", None, None), None);
        assert_eq!(m("Notification", Some("other"), None), None);
        // Rate-limit is never hook-sourced.
        assert!(!matches!(
            m("Stop", None, None),
            Some(EvidenceKind::RateLimited { .. })
        ));
    }

    #[test]
    fn evidence_serde_is_internally_tagged_snake_case() {
        let ev = Evidence::hook(
            EvidenceKind::ToolStarted {
                name: Some("Read".to_string()),
            },
            1_000,
        );
        let j = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(j["kind"], "tool_started");
        assert_eq!(j["name"], "Read");
        assert_eq!(j["authority"], "hook");
        assert_eq!(j["confidence"], "confirmed");
        // round-trips
        let back: Evidence = serde_json::from_value(j).expect("deserialize");
        assert_eq!(back, ev);
    }
}
