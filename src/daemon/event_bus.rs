//! Structured event bus for daemon-internal notifications.
//!
//! Phase 1: parallel emit spine. Existing `enqueue_with_idle_hint` calls
//! remain untouched; this module emits a structured `Event` alongside them
//! so future subscribers can react without per-module plumbing.
//!
//! Gated by `AGEND_EVENT_BUS=1`. When disabled, `emit()` is a no-op.

#![allow(dead_code)]

use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum EventKind {
    TaskStateChanged {
        task_id: String,
        title: String,
        assignee: Option<String>,
        reason: String,
        // #event-bus first-pattern (anti_stall): carry the task's `started_at` +
        // `eta_secs` so a subscriber can rebuild the BYTE-IDENTICAL notification
        // text the legacy direct enqueue formats from them.
        started_at: Option<String>,
        eta_secs: Option<i64>,
    },
    // #event-bus pattern (ci_watch): the per-subscriber CI pass / fail notify.
    // `body` is the RENDERED inbox body — it embeds a live-fetched failure-log tail
    // (`gh --log-failed`), so re-fetching in the subscriber would burn GitHub
    // rate-limit + could drift; it is frozen at the producer. `correlation_id`
    // (repo@branch) + `supersede_token` (ci-<run>-<sha>) reproduce the legacy
    // enqueue + supersede bookkeeping. One event is emitted PER recipient.
    CiReady {
        repo: String,
        branch: String,
        target: String,
        body: String,
        correlation_id: String,
        supersede_token: String,
    },
    CiFail {
        repo: String,
        branch: String,
        target: String,
        body: String,
        correlation_id: String,
        supersede_token: String,
    },
    DispatchIdleExceeded {
        dispatcher: String,
        target: String,
        elapsed_secs: i64,
        // #event-bus pattern #3 (dispatch_idle): the remaining fields the
        // threshold-exceeded notification formats, so the subscriber rebuilds it
        // byte-identically (overshoot is derived from elapsed - threshold).
        dispatch_id: String,
        expected_kind: String,
        threshold_secs: i64,
        correlation_id: Option<String>,
    },
    // #event-bus pattern #9 (supervisor member-state-change): the structured
    // {agent, team, from/to display} PLUS the fields the shared deliver needs to
    // rebuild BOTH the inbox enqueue (A) and the notify_agent text (B)
    // byte-identically. `detected_at` is FROZEN at the gate (the only now()-derived
    // value in the deliver path); `new_state` is the enum so the subscriber derives
    // the action_hint from the variant (not a fragile display-string match);
    // `consecutive_count` is the producer-side cooldown-track value the subscriber
    // has no track to recompute.
    MemberStateChanged {
        agent: String,
        team: String,
        from_state: String,
        to_state: String,
        orch: String,
        new_state: crate::state::AgentState,
        pane_tail: String,
        unlock_at: Option<String>,
        consecutive_count: u32,
        detected_at: String,
    },
    // #event-bus pattern #2 (decision_timeout): all fields the auto-default
    // timeout notification formats, so the subscriber rebuilds it byte-identically.
    DecisionTimeout {
        decision_id: String,
        sender: String,
        elapsed_secs: i64,
        timeout_secs: i64,
        default_action: String,
    },
    // #event-bus pattern #4 (waiting_on_stale): the fields the stale-waiting
    // notification formats, so the subscriber rebuilds the text byte-identically
    // and re-derives the agent + team-orchestrator fan-out.
    WaitingOnStale {
        agent: String,
        condition: String,
        elapsed_min: i64,
    },
    // #event-bus pattern #5 (helper_staleness_watchdog): the stale-helper alert
    // formats only the helper name (the rest is static), so the subscriber
    // rebuilds the byte-identical text + re-runs the hardcoded recipient fan-out.
    HelperStale {
        helper_name: String,
    },
    // #event-bus pattern #6 (idle_watchdog): the dev-idle / fleet-idle alert.
    // `emit_idle_alert` already takes exactly these fields, so the subscriber
    // re-delivers byte-identically (recipient is resolved at the producer).
    IdleAlert {
        recipient: String,
        kind: String,
        text: String,
        correlation_agent: Option<String>,
    },
    // #event-bus pattern #7 (cascade_cancel): when a parent task is cancelled,
    // each in-progress child's owner gets a notify. The text is rebuilt from
    // parent_id + child_id, so the subscriber re-delivers byte-identically.
    CascadeCancelNotify {
        owner: String,
        parent_id: String,
        child_id: String,
    },
    // #event-bus pattern #8 (poll_reminder) — FIRST PTY-inject pattern.
    // Carries the FULLY-FORMATTED `reminder` string (not raw count/age fields):
    // the text embeds a time-sensitive age ("Xm" from `chrono::Utc::now()`), so a
    // subscriber-side rebuild would recompute a LATER age and drift off
    // byte-identical. Template lesson for every PTY/time-sensitive pattern: any
    // text containing now()/age/timestamp must be frozen into the event, not
    // rebuilt by the subscriber.
    PollReminder {
        agent: String,
        reminder: String,
    },
    // #event-bus pattern (cron_tick): a due schedule fired. All fields are
    // STATIC (the user's schedule message/label + the fire decision), NOT
    // time-sensitive — so the subscriber re-runs the byte-identical effect
    // (resolve target → inject-or-enqueue → record_run → one-shot disable).
    CronFire {
        sched_id: String,
        target: String,
        message: String,
        label: String,
        one_shot: bool,
        missed: bool,
    },
    // #event-bus pattern (conflict_notify): the git-conflict notify / 30-min
    // stale escalation, both delivered via `notify_agent` (PTY-inject). The text
    // is built from live worktree discovery (git status + binding + op-marker) at
    // the producer, so it is carried RENDERED — the subscriber must not re-run the
    // discovery. `escalation` selects the NotifySource tag the deliver uses.
    ConflictAlert {
        agent: String,
        escalation: bool,
        text: String,
    },
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub timestamp: std::time::Instant,
}

impl Event {
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            timestamp: std::time::Instant::now(),
        }
    }
}

pub fn global() -> &'static EventBus {
    static BUS: std::sync::OnceLock<EventBus> = std::sync::OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

type Subscriber = Arc<dyn Fn(&Event) + Send + Sync>;

pub struct EventBus {
    subscribers: parking_lot::Mutex<Vec<Subscriber>>,
    enabled: bool,
}

impl EventBus {
    pub fn new() -> Self {
        let enabled = std::env::var("AGEND_EVENT_BUS")
            .map(|v| v == "1")
            .unwrap_or(false);
        Self {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Test-only: a bus with the gate forced ON (bypasses the env var), so other
    /// modules' tests can exercise the `emit`→subscriber path deterministically
    /// without touching the process-global `global()` bus.
    #[cfg(test)]
    pub(crate) fn new_enabled_for_test() -> Self {
        Self {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        }
    }

    pub fn subscribe(&self, f: impl Fn(&Event) + Send + Sync + 'static) {
        self.subscribers.lock().push(Arc::new(f));
    }

    pub fn emit(&self, kind: EventKind) {
        if !self.enabled {
            return;
        }
        let event = Event::new(kind);
        let subs = self.subscribers.lock();
        for sub in subs.iter() {
            sub(&event);
        }
        tracing::debug!(kind = ?event.kind, "event_bus: emitted");
    }

    pub fn emit_lazy(&self, f: impl FnOnce() -> EventKind) {
        if !self.enabled {
            return;
        }
        self.emit(f());
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("enabled", &self.enabled)
            .field("subscriber_count", &self.subscribers.lock().len())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emit_disabled_by_default() {
        let bus = EventBus::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        bus.subscribe(move |e| r.lock().unwrap().push(e.kind.clone()));
        bus.emit(EventKind::CiReady {
            repo: "owner/repo".into(),
            branch: "main".into(),
            target: "dev".into(),
            body: "[ci-pass]".into(),
            correlation_id: "owner/repo@main".into(),
            supersede_token: "ci-1-abc".into(),
        });
        assert!(
            received.lock().unwrap().is_empty(),
            "emit must be no-op when AGEND_EVENT_BUS != 1"
        );
    }

    #[test]
    fn emit_delivers_to_subscribers_when_enabled() {
        let bus = EventBus {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        };
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        bus.subscribe(move |e| r.lock().unwrap().push(e.kind.clone()));
        bus.emit(EventKind::TaskStateChanged {
            task_id: "t-1".into(),
            title: "test".into(),
            assignee: Some("dev".into()),
            reason: "stalled".into(),
            started_at: None,
            eta_secs: None,
        });
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], EventKind::TaskStateChanged { task_id, .. } if task_id == "t-1")
        );
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let bus = EventBus {
            subscribers: parking_lot::Mutex::new(Vec::new()),
            enabled: true,
        };
        let count_a = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_b = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ca = Arc::clone(&count_a);
        let cb = Arc::clone(&count_b);
        bus.subscribe(move |_| {
            ca.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        bus.subscribe(move |_| {
            cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        bus.emit(EventKind::CiFail {
            repo: "o/r".into(),
            branch: "feat".into(),
            target: "dev".into(),
            body: "[ci-fail]".into(),
            correlation_id: "o/r@feat".into(),
            supersede_token: "ci-2-def".into(),
        });
        assert_eq!(count_a.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(count_b.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn all_event_kinds_constructible() {
        let kinds = vec![
            EventKind::TaskStateChanged {
                task_id: "t-1".into(),
                title: "t".into(),
                assignee: None,
                reason: "r".into(),
                started_at: None,
                eta_secs: None,
            },
            EventKind::CiReady {
                repo: "o/r".into(),
                branch: "b".into(),
                target: "t".into(),
                body: "[ci-pass]".into(),
                correlation_id: "o/r@b".into(),
                supersede_token: "ci-1-aaa".into(),
            },
            EventKind::CiFail {
                repo: "o/r".into(),
                branch: "b".into(),
                target: "t".into(),
                body: "[ci-fail]".into(),
                correlation_id: "o/r@b".into(),
                supersede_token: "ci-1-bbb".into(),
            },
            EventKind::DispatchIdleExceeded {
                dispatcher: "lead".into(),
                target: "dev".into(),
                elapsed_secs: 600,
                dispatch_id: "di-1".into(),
                expected_kind: "task".into(),
                threshold_secs: 300,
                correlation_id: Some("t-1".into()),
            },
            EventKind::MemberStateChanged {
                agent: "dev".into(),
                team: "fixup".into(),
                from_state: "Ready".into(),
                to_state: "ServerRateLimit".into(),
                orch: "lead".into(),
                new_state: crate::state::AgentState::RateLimit,
                pane_tail: "rate limit".into(),
                unlock_at: None,
                consecutive_count: 1,
                detected_at: "2026-06-03T09:00:00+00:00".into(),
            },
            EventKind::DecisionTimeout {
                decision_id: "d-1".into(),
                sender: "general".into(),
                elapsed_secs: 2000,
                timeout_secs: 1800,
                default_action: "proceed".into(),
            },
            EventKind::IdleAlert {
                recipient: "general".into(),
                kind: "fleet_idle_watchdog".into(),
                text: "all idle".into(),
                correlation_agent: None,
            },
            EventKind::CascadeCancelNotify {
                owner: "fixup-dev".into(),
                parent_id: "t-parent".into(),
                child_id: "t-child".into(),
            },
            EventKind::PollReminder {
                agent: "dev".into(),
                reminder: "[AGEND-MSG] kind=poll-reminder unread=3 oldest=5m".into(),
            },
            EventKind::CronFire {
                sched_id: "s-1".into(),
                target: "dev".into(),
                message: "stand-up".into(),
                label: "morning".into(),
                one_shot: false,
                missed: false,
            },
            EventKind::ConflictAlert {
                agent: "fixup-dev".into(),
                escalation: false,
                text: "git conflict".into(),
            },
        ];
        for k in kinds {
            let _ = format!("{k:?}");
        }
    }
}
