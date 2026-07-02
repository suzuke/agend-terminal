//! #2524 P2c / #2313: daemon-side timeout+default for decision-board
//! questions (`decision action=post needs_answer=true timeout_secs=N`).
//!
//! Same idiom as `daemon::decision_timeout` (per-tick `CadenceGate` scan,
//! flock-guarded read-modify-write, emit-on-success) — deliberately NOT
//! shared code with it. The #2524 P2b/P2c manifest
//! (decision `d-20260702044452277394-4`) found the two mechanisms'
//! store/data-model/routing genuinely differ (single-sender-cancels-prior
//! sidecar vs. multi-author board; fixed fleet-wide recipient vs.
//! per-decision author) — reusing the *idiom*, not the code, is the
//! "new but small" resolution (matches #2249's own precedent).
//!
//! ## Scope (per decision `d-20260702044620796021-5`, forks C1a/C2a)
//!
//! - Auto-answers with `timeout_default` once `created_at + timeout_secs`
//!   elapses. Notifies the decision's own `author` field — no
//!   current-owner resolution (that's #2313's P2d clarify-routing scope).
//! - Decisions without `timeout_secs` are never touched (byte-identical to
//!   pre-#2313 behavior — the default, opt-in only).
//! - Idempotent: a decision already answered (by the operator, or a prior
//!   scan) is left alone — `decisions::auto_answer_timeout` re-checks
//!   `status == Pending` under the per-decision flock before writing.

use std::path::Path;

/// Scheduler throttle in supervisor TICK iterations. 30 × 10 s = 5 min —
/// matches `daemon::decision_timeout::TICKS_PER_DECISION_SCAN`.
pub(crate) const TICKS_PER_DECISION_BOARD_SCAN: u64 = 30;

pub(crate) struct DecisionBoardTimeoutTracker {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl Default for DecisionBoardTimeoutTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(
                TICKS_PER_DECISION_BOARD_SCAN,
            ),
        }
    }
}

impl DecisionBoardTimeoutTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        scan_and_answer(home);
        true
    }
}

/// Pure scan: detect decision-board questions whose `timeout_secs` has
/// elapsed, auto-answer them with `timeout_default`, and notify the
/// author. Exposed for tests.
pub(crate) fn scan_and_answer(home: &Path) {
    let now = chrono::Utc::now();
    for d in crate::decisions::list_pending(home) {
        let (Some(timeout_secs), Some(default_label)) = (d.timeout_secs, d.timeout_default.clone())
        else {
            continue;
        };
        let Ok(created) = chrono::DateTime::parse_from_rfc3339(&d.created_at) else {
            continue;
        };
        let created = created.with_timezone(&chrono::Utc);
        let elapsed_secs = now.signed_duration_since(created).num_seconds();
        if elapsed_secs <= timeout_secs as i64 {
            continue;
        }
        let Some((author, title)) = crate::decisions::auto_answer_timeout(home, &d.id) else {
            continue;
        };
        let text = format!(
            "[decision_board_timeout] decision {} ('{title}') timed out after {elapsed_secs}s \
             (threshold {timeout_secs}s). Auto-answered: '{default_label}'. Operator can still \
             override via `decision action=answer`.",
            d.id
        );
        if let Err(e) = crate::inbox::notify_system(
            home,
            &author,
            "system:decision_board_timeout",
            "decision_board_timeout",
            text,
            Some(&d.id),
            None,
        ) {
            tracing::warn!(
                decision_id = %d.id,
                error = %e,
                "decision_board_timeout: notify enqueue failed"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-decision-board-timeout-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Backdate a posted decision's `created_at` in place, so timeout
    /// scenarios don't require sleeping (mirrors `decision_timeout.rs`'s
    /// `write_pending_at` helper — same technique, different store).
    fn backdate_created_at(home: &Path, id: &str, created_at: chrono::DateTime<chrono::Utc>) {
        let path = crate::decisions::decision_path(home, id);
        let content = std::fs::read_to_string(&path).expect("read decision file");
        let mut v: serde_json::Value = serde_json::from_str(&content).expect("parse decision");
        v["created_at"] = serde_json::json!(created_at.to_rfc3339());
        std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).expect("write back");
    }

    #[test]
    fn scan_and_answer_auto_answers_elapsed_decision_2313() {
        let home = tmp_home("elapsed");
        let created = crate::decisions::post(
            &home,
            "alice",
            &serde_json::json!({
                "title": "risky call", "content": "?", "needs_answer": true,
                "timeout_secs": 60, "timeout_default": "proceed"
            }),
        );
        let id = created["id"].as_str().expect("id").to_string();
        backdate_created_at(
            &home,
            &id,
            chrono::Utc::now() - chrono::Duration::seconds(120),
        );

        scan_and_answer(&home);

        let listed = crate::decisions::list(&home, &serde_json::json!({"include_archived": true}));
        let decisions = listed["decisions"].as_array().expect("array");
        let d = decisions.iter().find(|d| d["id"] == id).expect("found");
        assert_eq!(
            d["answer"], "proceed",
            "must auto-answer with timeout_default: {d}"
        );
        assert_eq!(d["answered_by"], "timeout-default");

        let inbox = crate::inbox::drain(&home, "alice");
        assert!(
            inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_board_timeout")
                    && m.correlation_id.as_deref() == Some(id.as_str())),
            "author must be notified: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn scan_and_answer_skips_not_yet_elapsed_2313() {
        let home = tmp_home("not-elapsed");
        let created = crate::decisions::post(
            &home,
            "alice",
            &serde_json::json!({
                "title": "x", "content": "?", "needs_answer": true,
                "timeout_secs": 6000, "timeout_default": "proceed"
            }),
        );
        let id = created["id"].as_str().expect("id").to_string();
        // created_at left at "now" — far from the 6000s threshold.

        scan_and_answer(&home);

        let listed = crate::decisions::list(&home, &serde_json::json!({}));
        let decisions = listed["decisions"].as_array().expect("array");
        let d = decisions.iter().find(|d| d["id"] == id).expect("found");
        assert!(
            d["answer"].is_null(),
            "must not answer before timeout_secs elapses: {d}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn scan_and_answer_ignores_decisions_without_timeout_2313() {
        let home = tmp_home("no-timeout");
        let created = crate::decisions::post(
            &home,
            "alice",
            &serde_json::json!({"title": "x", "content": "?", "needs_answer": true}),
        );
        let id = created["id"].as_str().expect("id").to_string();
        backdate_created_at(
            &home,
            &id,
            chrono::Utc::now() - chrono::Duration::seconds(999_999),
        );

        scan_and_answer(&home);

        let listed = crate::decisions::list(&home, &serde_json::json!({}));
        let decisions = listed["decisions"].as_array().expect("array");
        let d = decisions.iter().find(|d| d["id"] == id).expect("found");
        assert!(
            d["answer"].is_null(),
            "a decision with no timeout_secs must never be auto-answered, however old: {d}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn scan_and_answer_idempotent_single_notify_2313() {
        let home = tmp_home("idempotent");
        let created = crate::decisions::post(
            &home,
            "alice",
            &serde_json::json!({
                "title": "x", "content": "?", "needs_answer": true,
                "timeout_secs": 60, "timeout_default": "proceed"
            }),
        );
        let id = created["id"].as_str().expect("id").to_string();
        backdate_created_at(
            &home,
            &id,
            chrono::Utc::now() - chrono::Duration::seconds(120),
        );

        scan_and_answer(&home);
        scan_and_answer(&home);

        let inbox = crate::inbox::drain(&home, "alice");
        let count = inbox
            .iter()
            .filter(|m| {
                m.kind.as_deref() == Some("decision_board_timeout")
                    && m.correlation_id.as_deref() == Some(id.as_str())
            })
            .count();
        assert_eq!(count, 1, "second scan must not re-notify: {inbox:?}");
        std::fs::remove_dir_all(&home).ok();
    }
}
