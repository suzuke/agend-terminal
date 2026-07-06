//! #2413 §(a) — ObservedStatus reconcile telemetry (offline-join accuracy campaign).
//!
//! Persists, per decisive-screen tick, a structured record pairing the fused
//! [`ObservedStatus`] with the raw screen baseline it was derived against, so an
//! OFFLINE forward-join (each record → that agent's next definitive convergence
//! event: a later hook terminal event, `usage_limit_detected`, or process
//! liveness) can label ground truth and compute ObservedStatus accuracy + a
//! per-backend breakdown over an N-day window. NO forward-looking logic runs in
//! the daemon — truth is defined offline, iterable without a redeploy (and
//! deliberately NOT the API plane itself, to keep the measurement non-circular).
//!
//! DEFAULT-OFF (`AGEND_SHADOW_RECONCILE_LOG=1` to enable) — opt-in for a bounded
//! campaign, distinct from the always-on shadow observer kill-switch.
//!
//! Volume control (the daemon must not grow a new unbounded log like
//! `state-transitions.jsonl` did): every disagreement AND every fused-state
//! transition is logged in full (`weight=1`); steady agreeing ticks are SAMPLED
//! 1/K (`AGEND_SHADOW_RECONCILE_SAMPLE`, default 60) and stamped `weight=K`, so
//! the offline denominator (total decisive ticks) = Σ`weight` — an unbiased
//! Horvitz–Thompson estimate. Output rotates into daily files
//! `shadow-reconcile.<YYYY-MM-DD>.jsonl`; a total-size budget reaps oldest days.

use super::evidence::{Authority, Confidence};
use super::gate;
use super::reducer::{Liveness, ObservedState, ObservedStatus, ScreenSignal};
use crate::state::AgentState;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

/// Total on-disk budget across all `shadow-reconcile.*.jsonl` day files; oldest
/// days are reaped past it. ~1 MB/day at 13 agents (PR body) ⇒ ~50 days headroom.
const BUDGET_BYTES: u64 = 50 * 1024 * 1024;

/// Default agreement sample rate: 1 in K steady agreeing ticks is logged.
const DEFAULT_SAMPLE_RATE: u64 = 60;

/// Campaign opt-in. Distinct from `shadow::enabled()` (the always-on observer
/// kill-switch): this logging is DEFAULT-OFF, on only for an explicit campaign.
pub(crate) fn enabled() -> bool {
    std::env::var("AGEND_SHADOW_RECONCILE_LOG").as_deref() == Ok("1")
}

fn sample_rate() -> u64 {
    std::env::var("AGEND_SHADOW_RECONCILE_SAMPLE")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&k| k >= 1)
        .unwrap_or(DEFAULT_SAMPLE_RATE)
}

#[derive(Default)]
struct AgentReconcileState {
    last_coarse: Option<ObservedState>,
    /// Continuous count of steady agreeing ticks (NOT reset on transition), so
    /// 1/K sampling stays unbiased across short agree runs.
    agree_run: u64,
}

/// Per-agent transition + sampling state. Touched only while the campaign flag is
/// on; one small entry per live agent (the daemon process is the campaign's life).
static STATE: LazyLock<Mutex<HashMap<String, AgentReconcileState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Serialize)]
struct ReconcileRecord<'a> {
    ts: &'a str,
    agent: &'a str,
    backend: &'a str,
    observed: ObservedState,
    raw_screen: &'static str,
    agreed: bool,
    authority: Authority,
    confidence: Confidence,
    api_in_flight: bool,
    productive_silent_ms: u64,
    since_ms: u64,
    /// # of actual decisive ticks this record stands for (1, or K for a sampled
    /// steady-agree). Offline denominator = Σ weight.
    weight: u64,
}

/// Append one decisive-screen tick's reconcile record (see module docs for the
/// sampling contract). No-op unless the campaign flag is on and the screen is
/// decisive; steady agreeing ticks between samples return without writing.
pub(crate) fn record(
    home: &Path,
    agent: &str,
    backend_command: &str,
    raw_state: AgentState,
    screen: ScreenSignal,
    status: &ObservedStatus,
    live: &Liveness,
) {
    if !enabled() {
        return;
    }
    // Decisive screen only — a non-decisive ("Other") screen has no baseline to
    // agree/disagree against (mirrors `shadow_observe::log_correction`'s gate).
    let Some(screen_state) = gate::screen_as_observed(screen) else {
        return;
    };
    let coarse = status.state.coarse();
    let agreed = screen_state == coarse;

    let k = sample_rate();
    let weight = {
        let mut guard = STATE.lock();
        let st = guard.entry(agent.to_string()).or_default();
        let changed = st.last_coarse != Some(coarse);
        st.last_coarse = Some(coarse);
        if !agreed || changed {
            // Disagreement or a fused-state transition: always logged in full.
            1
        } else {
            // Steady agreement: continuous 1/K sample; weight K stands for the
            // K-1 unlogged steady ticks. Counter is NOT reset on transitions, so
            // short agree runs still contribute unbiasedly to the denominator.
            st.agree_run += 1;
            if st.agree_run.is_multiple_of(k) {
                k
            } else {
                return;
            }
        }
    };

    let backend = crate::backend::Backend::from_command(backend_command)
        .map(|b| b.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let ts = chrono::Utc::now().to_rfc3339();
    let rec = ReconcileRecord {
        ts: &ts,
        agent,
        backend: &backend,
        observed: status.state,
        raw_screen: raw_state.display_name(),
        agreed,
        authority: status.authority,
        confidence: status.confidence,
        api_in_flight: live.api_in_flight,
        productive_silent_ms: live.productive_silent_ms,
        since_ms: status.since_ms,
        weight,
    };
    if let Ok(line) = serde_json::to_string(&rec) {
        append(home, &ts, &line);
    }
}

fn append(home: &Path, ts: &str, line: &str) {
    // `ts` is rfc3339 (`YYYY-MM-DDT…`); its first 10 chars are the UTC date.
    let date = ts.get(..10).unwrap_or("0000-00-00");
    let path = home.join(format!("shadow-reconcile.{date}.jsonl"));
    let new_day = !path.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
    // Enforce the disk budget only when a new day file first appears — O(1) per
    // day, not per write.
    if new_day {
        enforce_budget(home, BUDGET_BYTES);
    }
}

fn enforce_budget(home: &Path, budget: u64) {
    let Ok(rd) = std::fs::read_dir(home) else {
        return;
    };
    let mut files: Vec<(String, std::path::PathBuf, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.starts_with("shadow-reconcile.") && name.ends_with(".jsonl")).then(|| {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                (name, e.path(), len)
            })
        })
        .collect();
    let total: u64 = files.iter().map(|(_, _, l)| *l).sum();
    if total <= budget {
        return;
    }
    // Date-stamped names sort lexicographically = chronologically; reap oldest
    // first and NEVER the newest (today's active) file.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let last = files.len().saturating_sub(1);
    let mut over = total - budget;
    for (i, (_, path, len)) in files.into_iter().enumerate() {
        if over == 0 || i == last {
            break;
        }
        let _ = std::fs::remove_file(&path);
        over = over.saturating_sub(len);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-reconcile-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn status(state: ObservedState) -> ObservedStatus {
        ObservedStatus {
            state,
            confidence: Confidence::Strong,
            authority: Authority::Hook,
            evidence: vec![],
            since_ms: 42,
        }
    }

    fn live() -> Liveness {
        Liveness {
            api_in_flight: true,
            productive_silent_ms: 100,
            child_alive: true,
        }
    }

    fn read_records(home: &Path) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(home).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("shadow-reconcile.") && n.ends_with(".jsonl") {
                let content = std::fs::read_to_string(e.path()).unwrap_or_default();
                for line in content.lines().filter(|l| !l.is_empty()) {
                    out.push(serde_json::from_str(line).unwrap());
                }
            }
        }
        out
    }

    /// The campaign flag is default-OFF: `record` writes nothing without it, even
    /// on a genuine disagreement.
    #[test]
    #[serial]
    fn disabled_writes_nothing() {
        std::env::remove_var("AGEND_SHADOW_RECONCILE_LOG");
        let home = tmp_home("off");
        record(
            &home,
            "off-agent",
            "claude",
            AgentState::Idle,
            ScreenSignal::Idle,
            &status(ObservedState::Active),
            &live(),
        );
        assert!(read_records(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// A disagreement (screen Idle, fused Active) is always logged in full,
    /// weight 1, agreed=false, carrying the backend field.
    #[test]
    #[serial]
    fn disagreement_logged_weight_1_with_backend() {
        std::env::set_var("AGEND_SHADOW_RECONCILE_LOG", "1");
        let home = tmp_home("disagree");
        record(
            &home,
            "disagree-agent",
            "/opt/homebrew/bin/codex",
            AgentState::Idle,
            ScreenSignal::Idle,
            &status(ObservedState::Active),
            &live(),
        );
        record(
            &home,
            "disagree-agent-2",
            "mystery-binary",
            AgentState::Idle,
            ScreenSignal::Idle,
            &status(ObservedState::Active),
            &live(),
        );
        let recs = read_records(&home);
        assert_eq!(recs.len(), 2, "disagreements must be logged: {recs:?}");
        assert_eq!(recs[0]["agreed"], serde_json::json!(false));
        assert_eq!(recs[0]["weight"], serde_json::json!(1));
        // basename of the command path resolves the backend; unknown → "unknown".
        assert_eq!(recs[0]["backend"], serde_json::json!("codex"));
        assert_eq!(recs[1]["backend"], serde_json::json!("unknown"));
        assert_eq!(recs[0]["observed"], serde_json::json!("active"));
        std::env::remove_var("AGEND_SHADOW_RECONCILE_LOG");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Steady agreement is sampled 1/K with weight K; the first tick (a
    /// transition from None) always logs. Over 4 identical agreeing ticks at K=3:
    /// records at tick1 (transition, w1) and tick4 (3rd steady, w3); Σweight = 4
    /// = the true tick count (unbiased denominator).
    #[test]
    #[serial]
    fn steady_agreement_sampled_denominator_preserved() {
        std::env::set_var("AGEND_SHADOW_RECONCILE_LOG", "1");
        std::env::set_var("AGEND_SHADOW_RECONCILE_SAMPLE", "3");
        let home = tmp_home("sample");
        for _ in 0..4 {
            record(
                &home,
                "sample-agent",
                "claude",
                AgentState::Idle,
                ScreenSignal::Idle,
                &status(ObservedState::Idle),
                &live(),
            );
        }
        let recs = read_records(&home);
        assert_eq!(recs.len(), 2, "transition + 1 sampled steady: {recs:?}");
        let weights: Vec<u64> = recs.iter().map(|r| r["weight"].as_u64().unwrap()).collect();
        assert_eq!(
            weights.iter().sum::<u64>(),
            4,
            "Σweight must equal tick count"
        );
        assert!(recs.iter().all(|r| r["agreed"] == serde_json::json!(true)));
        std::env::remove_var("AGEND_SHADOW_RECONCILE_LOG");
        std::env::remove_var("AGEND_SHADOW_RECONCILE_SAMPLE");
        std::fs::remove_dir_all(&home).ok();
    }

    /// The size budget reaps oldest day files first and never the newest.
    #[test]
    fn enforce_budget_reaps_oldest_keeps_newest() {
        let home = tmp_home("budget");
        for (day, bytes) in [
            ("2026-01-01", 4000usize),
            ("2026-01-02", 4000),
            ("2026-01-03", 10),
        ] {
            std::fs::write(
                home.join(format!("shadow-reconcile.{day}.jsonl")),
                "x".repeat(bytes),
            )
            .unwrap();
        }
        enforce_budget(&home, 5000); // total 8010 > 5000 → reap oldest
        assert!(!home.join("shadow-reconcile.2026-01-01.jsonl").exists());
        assert!(
            home.join("shadow-reconcile.2026-01-03.jsonl").exists(),
            "newest kept"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
