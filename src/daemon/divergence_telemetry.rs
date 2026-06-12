//! #1523 phase-2 prerequisite — heuristic⨯hook state-divergence telemetry
//! (instrument-only, shadow-mode).
//!
//! ## Why
//!
//! The #1523 epic promotes per-tick state DECIDERS from the raw screen
//! heuristic to the hook-aware authoritative state. Phase-1 promoted only the
//! snapshot surface; phase-2 (the W3.3 refactor) flips the remaining deciders
//! (hang / recovery / watchdog / supervisor reactions / conflict_notify). That
//! flip should rest on DATA, not a code-trace hunch: how often does the screen
//! heuristic actually disagree with the hook authoritative state, and — the
//! signal phase-2 most needs — WHICH two states clash when they do (e.g. the
//! #1985 `heuristic=Idle` vs `hook=ToolUse` long-tool case).
//!
//! survey 04 §6 flagged the `behavioral.rs` divergence telemetry (silence-vs-
//! regex `DivergenceStats`) as "built but unused in production". This is the
//! production realization of that recommendation, on the axis phase-2 needs
//! (heuristic⨯hook), with a per-state-pair breakdown the older agree/diverge
//! counter lacks. The behavioral.rs telemetry is left as-is.
//!
//! ## Discipline (mirrors the #1808 / #2055 instrument-only probes)
//!
//! - ZERO behaviour: pure observation. No decider reads this; it gates nothing.
//! - Best-effort: a flush error is swallowed — telemetry must never be
//!   load-bearing.
//! - Noise budget: the per-tick handler only increments in-memory counters;
//!   aggregation is flushed PERIODICALLY (hourly) as ONE JSONL line + one INFO
//!   log, never per tick.
//! - Persistence: an append-only `<home>/state-divergence-shadow.jsonl` (its
//!   own file, not the rotating daemon log — same shape as `mcp-usage-stats`).
//!   In-memory window state resets on restart (the partial current hour is
//!   lost); flushed lines persist. One line per window:
//!
//! ```json
//! {"ts":"2026-06-12T05:00:00Z","window_secs":3600,"observations":420,
//!  "agree":390,"disagree":12,"no_hook_signal":18,
//!  "disagree_pairs":{"Idle>ToolUse":9,"Thinking>Idle":3}}
//! ```
//!
//! `disagree_pairs` keys are `"<heuristic>><hook>"`. `no_hook_signal` =
//! Fresh-less resolutions (Stale/Unknown) where the heuristic stands alone —
//! it measures hook COVERAGE, distinct from disagreement.
//!
//! Scope: hook-capable backends only (those with `has_state_hooks()` — claude
//! today). Observation is INDEPENDENT of `AGEND_HOOK_STATE_POC`: we measure the
//! divergence in order to DECIDE whether to flip the promotion flag, so the
//! data must accrue whether or not promotion is currently on.

use crate::daemon::hook_shadow::HookResolution;
use crate::state::AgentState;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// In-memory accumulator for the current (hourly) window. `None` until the
/// first observation; reset to `None` on flush. Mirrors `behavioral.rs`'s
/// `DIVERGENCE` const-init pattern.
static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

#[derive(Default)]
struct Window {
    observations: u64,
    agree: u64,
    disagree: u64,
    /// Fresh-less hook resolutions (Stale/Unknown) — heuristic stands alone.
    no_hook_signal: u64,
    /// `"<heuristic>><hook>"` → count, recorded only on a Fresh disagreement.
    /// String-keyed because `AgentState` is not `Ord`/`Hash`; the formatted
    /// key sorts deterministically for stable JSONL output.
    disagree_pairs: BTreeMap<String, u64>,
}

/// The dedicated shadow-telemetry log, separate from `daemon.log*` so rotation
/// never discards it.
fn stats_path(home: &Path) -> PathBuf {
    home.join("state-divergence-shadow.jsonl")
}

/// Record one per-tick comparison for a hook-capable agent. `heuristic` is the
/// screen-derived state; `hook` is the freshness-resolved hook reading. Pure
/// in-memory counter bump — called every tick, aggregated on flush.
pub fn record(heuristic: AgentState, hook: &HookResolution) {
    let mut guard = WINDOW.lock();
    let w = guard.get_or_insert_with(Window::default);
    w.observations += 1;
    match hook {
        HookResolution::Fresh(hook_state) => {
            if *hook_state == heuristic {
                w.agree += 1;
            } else {
                w.disagree += 1;
                let key = format!("{}>{}", heuristic.display_name(), hook_state.display_name());
                *w.disagree_pairs.entry(key).or_default() += 1;
            }
        }
        HookResolution::Stale | HookResolution::Unknown => {
            w.no_hook_signal += 1;
        }
    }
}

/// Build the periodic-window JSONL line (pure — unit-tested). `window_secs` is
/// the wall-clock the window covered (cadence × tick interval).
fn build_line(w: &Window, window_secs: u64, ts: String) -> Value {
    let pairs: serde_json::Map<String, Value> = w
        .disagree_pairs
        .iter()
        .map(|(key, n)| (key.clone(), Value::from(*n)))
        .collect();
    serde_json::json!({
        "ts": ts,
        "window_secs": window_secs,
        "observations": w.observations,
        "agree": w.agree,
        "disagree": w.disagree,
        "no_hook_signal": w.no_hook_signal,
        "disagree_pairs": pairs,
    })
}

/// Flush the current window: take + reset the accumulator, then (if anything
/// was observed) append one JSONL line + emit one INFO log. Best-effort.
pub fn flush(home: &Path, window_secs: u64) {
    let window = { WINDOW.lock().take() };
    let Some(w) = window else { return };
    if w.observations == 0 {
        return;
    }
    let line = build_line(&w, window_secs, chrono::Utc::now().to_rfc3339());
    tracing::info!(
        target: "divergence_shadow",
        observations = w.observations,
        agree = w.agree,
        disagree = w.disagree,
        no_hook_signal = w.no_hook_signal,
        pairs = %line["disagree_pairs"],
        "#1523 heuristic⨯hook divergence (last window)"
    );
    let _ = append_line(&stats_path(home), &line);
}

/// Append one compact JSONL line (create-if-missing + append). A torn line is
/// tolerable (the analyst's jq skips it); a failed write is silently dropped.
fn append_line(path: &Path, line: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn reset() {
        *WINDOW.lock() = None;
    }

    #[test]
    #[serial(divergence_window)] // shares the global WINDOW accumulator
    fn record_buckets_agree_disagree_no_signal() {
        reset();
        // Fresh + same state → agree.
        record(
            AgentState::Thinking,
            &HookResolution::Fresh(AgentState::Thinking),
        );
        // Fresh + different → disagree, pair captured.
        record(
            AgentState::Idle,
            &HookResolution::Fresh(AgentState::ToolUse),
        );
        record(
            AgentState::Idle,
            &HookResolution::Fresh(AgentState::ToolUse),
        );
        // Stale / Unknown → no_hook_signal (no comparison).
        record(AgentState::Idle, &HookResolution::Stale);
        record(AgentState::Idle, &HookResolution::Unknown);

        let guard = WINDOW.lock();
        let w = guard.as_ref().unwrap();
        assert_eq!(w.observations, 5);
        assert_eq!(w.agree, 1);
        assert_eq!(w.disagree, 2);
        assert_eq!(w.no_hook_signal, 2);
        let pair_key = format!(
            "{}>{}",
            AgentState::Idle.display_name(),
            AgentState::ToolUse.display_name()
        );
        assert_eq!(
            w.disagree_pairs.get(&pair_key),
            Some(&2),
            "the Idle-vs-ToolUse clash (the #1985 shape) is counted per pair"
        );
    }

    #[test]
    fn build_line_shape_for_phase2() {
        let mut disagree_pairs = BTreeMap::new();
        disagree_pairs.insert(
            format!(
                "{}>{}",
                AgentState::Idle.display_name(),
                AgentState::ToolUse.display_name()
            ),
            2,
        );
        let w = Window {
            observations: 10,
            agree: 7,
            disagree: 2,
            no_hook_signal: 1,
            disagree_pairs,
        };
        let line = build_line(&w, 3600, "2026-06-12T00:00:00Z".into());
        assert_eq!(line["observations"], 10);
        assert_eq!(line["agree"], 7);
        assert_eq!(line["disagree"], 2);
        assert_eq!(line["no_hook_signal"], 1);
        assert_eq!(line["window_secs"], 3600);
        // Pair key is "<heuristic>><hook>" so phase-2 sees which states clash.
        let pairs = line["disagree_pairs"].as_object().unwrap();
        let key = format!(
            "{}>{}",
            AgentState::Idle.display_name(),
            AgentState::ToolUse.display_name()
        );
        assert_eq!(pairs.get(&key), Some(&Value::from(2)));
    }

    #[test]
    #[serial(divergence_window)] // shares the global WINDOW accumulator
    fn flush_takes_and_resets_window_and_writes_jsonl() {
        reset();
        let home = std::env::temp_dir().join(format!("agend-div-tel-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let _ = std::fs::remove_file(stats_path(&home));

        record(
            AgentState::Idle,
            &HookResolution::Fresh(AgentState::ToolUse),
        );
        flush(&home, 3600);

        // Window reset after flush.
        assert!(WINDOW.lock().is_none(), "flush resets the window");
        let body = std::fs::read_to_string(stats_path(&home)).expect("jsonl written");
        let line: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line["disagree"], 1);

        // Empty window → no line appended (best-effort, no noise).
        flush(&home, 3600);
        let body2 = std::fs::read_to_string(stats_path(&home)).unwrap();
        assert_eq!(body2.lines().count(), 1, "empty window flushes nothing");

        std::fs::remove_dir_all(&home).ok();
    }
}
