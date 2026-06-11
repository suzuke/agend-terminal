//! Structured event bus for daemon-internal notifications.
//!
//! Each per-pattern producer `emit`s a structured `Event` carrying the
//! notification payload + the `$AGEND_HOME` it occurred in; a per-pattern
//! subscriber (registered once at daemon startup in `run_core`) re-delivers it.
//!
//! End-of-train cutover is COMPLETE (Step 2, legacy-zero): the bus is the SOLE
//! delivery path — `emit` is unconditional (no gate, no env, no legacy fallback).
//! Rollback is `git revert` of the cutover, not an env flag.

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
    // (repo@branch) + `supersede_token` (ci-<run>-<sha>) reproduce the enqueue +
    // supersede bookkeeping. One event is emitted PER recipient.
    CiReady {
        target: String,
        body: String,
        correlation_id: String,
        supersede_token: String,
    },
    CiFail {
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
        /// #2008-p2: this is the "long-running WITH ACTIVITY — confirm expected"
        /// escalation (the auto-extension CAP was hit while the target kept showing
        /// activity), NOT the "went silent / stuck" alarm. Same delivery shape; the
        /// subscriber branches the message text so the dispatcher tells them apart.
        long_running: bool,
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
    /// #event-bus Step 2 (legacy-zero): the `$AGEND_HOME` the event occurred in.
    /// Carried ON the event (not captured at subscriber registration) so a single
    /// globally-registered subscriber delivers to the correct home — required now
    /// that the bus is the ONLY delivery path (no per-home legacy fallback), and so
    /// the multi-home integration tests each deliver to their own tmp home.
    pub home: std::path::PathBuf,
}

impl Event {
    pub fn new(home: std::path::PathBuf, kind: EventKind) -> Self {
        Self { kind, home }
    }
}

pub fn global() -> &'static EventBus {
    static BUS: std::sync::OnceLock<EventBus> = std::sync::OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

/// #event-bus Step 2 (legacy-zero): test-only one-shot registration of ALL pattern
/// subscribers on the process-global bus, mirroring `daemon::run_core`. Since the
/// bus is now the SOLE delivery path, an integration test that drives a production
/// fn (which `emit`s to `global()`) must register the subscribers first, else the
/// emit fans out to nothing. Idempotent via `Once` (subscribe appends, so a second
/// registration would double-deliver). The home travels on each event, so this one
/// registration serves every test's tmp home. cron gets a dummy empty registry —
/// cron integration tests use empty registries + assert the home-driven inbox
/// fallback, so the captured registry's contents don't affect them.
#[cfg(test)]
pub(crate) fn register_all_subscribers_for_test() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // DRY: route through the SAME registration list prod uses
        // (`daemon::register_event_subscribers`), so test wiring can NEVER drift
        // from live wiring — the drift that masked the #1720 app-mode silent-drop
        // (this helper registered cron; the live `agend-terminal app` path did not).
        // cron gets a dummy empty registry — cron integration tests use empty
        // registries + assert the home-driven inbox fallback, so the captured
        // registry's contents don't affect them.
        let dummy_registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        crate::daemon::register_event_subscribers(&dummy_registry);
    });
}

/// #event-bus Step 2 (legacy-zero): register the bus subscribers ONCE at
/// test-binary LOAD — before any `#[test]` runs — via `ctor`. The bus is now the
/// sole delivery path, so integration tests that drive a producer (`emit`→global)
/// would otherwise be order-dependent (only delivering if some earlier test
/// happened to register first). Running registration before any test makes
/// delivery order-INDEPENDENT — no per-test harness call, no order-dependent flake.
#[cfg(test)]
#[ctor::ctor]
fn _register_event_bus_subscribers_at_test_load() {
    register_all_subscribers_for_test();
}

/// A subscriber returns `true` iff it HANDLED the event — i.e. it matched the
/// `EventKind` it cares about AND completed its delivery. Returning `false`
/// (didn't match this kind, or its delivery failed) lets [`EventBus::emit`]
/// report a handled-count, so a producer can detect an event that fanned out to
/// nothing (#1720: a cron fire emitted while its subscriber was unregistered was
/// silently lost — a count lets the producer record `skipped` instead of
/// dropping it without a trace).
type Subscriber = Arc<dyn Fn(&Event) -> bool + Send + Sync>;

pub struct EventBus {
    subscribers: parking_lot::Mutex<Vec<Subscriber>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self, f: impl Fn(&Event) -> bool + Send + Sync + 'static) {
        self.subscribers.lock().push(Arc::new(f));
    }

    /// #event-bus Step 2 (legacy-zero): the bus is the SOLE delivery path, so
    /// `emit` is unconditional (no gate). `home` is the `$AGEND_HOME` the event
    /// occurred in — carried on the `Event` so a single globally-registered
    /// subscriber delivers to the correct home.
    ///
    /// Returns the **handled-count**: how many subscribers reported handling this
    /// event (matched its kind AND delivered). `0` means the event fanned out to
    /// nothing relevant — the producer can then record/log it rather than let it
    /// vanish silently (#1720).
    pub fn emit(&self, home: &std::path::Path, kind: EventKind) -> usize {
        let event = Event::new(home.to_path_buf(), kind);
        // #1745: snapshot the subscriber list under the lock, then DROP the guard
        // before invoking any callback. Running `sub(&event)` while holding the
        // lock (a) deadlocks a re-entrant subscriber — one that itself calls
        // `emit`/`subscribe` — and (b) lets a panicking subscriber abort the
        // fan-out (and risk poisoning). Subscribers are `Arc<dyn Fn>`, so cloning
        // the Vec is a cheap refcount bump.
        let subs: Vec<Subscriber> = self.subscribers.lock().clone();
        let mut handled = 0usize;
        for sub in &subs {
            // #1745: isolate each subscriber — a panic in one must not abort the
            // fan-out to the rest (mirrors the per_tick handler isolation). The
            // handled-count semantics are unchanged: only a subscriber returning
            // `true` (matched + delivered) is counted; a panic counts as not
            // handled.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sub(&event))) {
                Ok(true) => handled += 1,
                Ok(false) => {}
                Err(payload) => {
                    let detail = payload
                        .downcast_ref::<&'static str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    tracing::error!(
                        kind = ?event.kind,
                        panic = %detail,
                        "#1745 event_bus subscriber panicked — continuing fan-out to the rest"
                    );
                }
            }
        }
        tracing::debug!(kind = ?event.kind, handled, "event_bus: emitted");
        handled
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
    fn emit_delivers_to_subscribers() {
        let bus = EventBus::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        bus.subscribe(move |e| {
            r.lock().unwrap().push(e.kind.clone());
            true
        });
        bus.emit(
            std::path::Path::new("/tmp/h"),
            EventKind::TaskStateChanged {
                task_id: "t-1".into(),
                title: "test".into(),
                assignee: Some("dev".into()),
                reason: "stalled".into(),
                started_at: None,
                eta_secs: None,
            },
        );
        let events = received.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], EventKind::TaskStateChanged { task_id, .. } if task_id == "t-1")
        );
    }

    fn dummy_event() -> EventKind {
        EventKind::TaskStateChanged {
            task_id: "t".into(),
            title: "t".into(),
            assignee: None,
            reason: "r".into(),
            started_at: None,
            eta_secs: None,
        }
    }

    #[test]
    fn panicking_subscriber_does_not_abort_fan_out() {
        // #1745: a subscriber that panics must not prevent the remaining
        // subscribers from receiving the event, and must not be counted as
        // handled. (Negative-probe: without the per-subscriber catch_unwind the
        // `after` assertion fails — the panic aborts the fan-out.)
        let bus = EventBus::new();
        let before = Arc::new(Mutex::new(false));
        let after = Arc::new(Mutex::new(false));
        let b = Arc::clone(&before);
        let a = Arc::clone(&after);
        bus.subscribe(move |_e| {
            *b.lock().unwrap() = true;
            true
        });
        bus.subscribe(|_e| panic!("boom subscriber"));
        bus.subscribe(move |_e| {
            *a.lock().unwrap() = true;
            true
        });
        let handled = bus.emit(std::path::Path::new("/tmp/h"), dummy_event());
        assert!(
            *before.lock().unwrap(),
            "subscriber registered before the panicker must run"
        );
        assert!(
            *after.lock().unwrap(),
            "subscriber registered AFTER the panicker must still run — the panic \
             must not abort the fan-out"
        );
        assert_eq!(
            handled, 2,
            "handled-count must exclude the panicking subscriber (2 returned true)"
        );
    }

    #[test]
    fn reentrant_subscribe_during_emit_does_not_deadlock() {
        // #1745: a subscriber that registers another subscriber (re-entrant
        // `subscribe`) must not deadlock. The guard is dropped before any
        // callback runs, so the inner `subscribe` can acquire the lock. With the
        // pre-fix code (callbacks invoked while holding the lock) this re-entrant
        // lock would dead­lock the non-reentrant parking_lot::Mutex and hang.
        let bus = Arc::new(EventBus::new());
        let bus2 = Arc::clone(&bus);
        bus.subscribe(move |_e| {
            bus2.subscribe(|_e| false);
            true
        });
        let handled = bus.emit(std::path::Path::new("/tmp/h"), dummy_event());
        assert_eq!(handled, 1, "the original subscriber handled the event");
        assert_eq!(
            bus.subscribers.lock().len(),
            2,
            "the re-entrant subscribe must have registered the new subscriber"
        );
    }

    #[test]
    fn emit_carries_the_home_on_the_event() {
        let bus = EventBus::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        bus.subscribe(move |e| {
            s.lock().unwrap().push(e.home.clone());
            true
        });
        bus.emit(
            std::path::Path::new("/tmp/agend-home-x"),
            EventKind::CiReady {
                target: "dev".into(),
                body: "[ci-pass]".into(),
                correlation_id: "owner/repo@main".into(),
                supersede_token: "ci-1-abc".into(),
            },
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [std::path::PathBuf::from("/tmp/agend-home-x")],
            "the subscriber must see the home the producer emitted with"
        );
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let bus = EventBus::new();
        let count_a = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_b = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ca = Arc::clone(&count_a);
        let cb = Arc::clone(&count_b);
        bus.subscribe(move |_| {
            ca.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });
        bus.subscribe(move |_| {
            cb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        });
        bus.emit(
            std::path::Path::new("/tmp/h"),
            EventKind::CiFail {
                target: "dev".into(),
                body: "[ci-fail]".into(),
                correlation_id: "o/r@feat".into(),
                supersede_token: "ci-2-def".into(),
            },
        );
        assert_eq!(count_a.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(count_b.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// #1720 (B): `emit` returns the HANDLED-count — only subscribers that report
    /// handling (returned `true`, e.g. matched their EventKind) are counted. A
    /// subscriber that ignores the event (returns `false`, wrong kind) does not.
    /// This is the signal a producer uses to detect a fire that reached nothing
    /// relevant (handled==0) instead of losing it silently.
    #[test]
    fn emit_returns_handled_count() {
        let bus = EventBus::new();
        bus.subscribe(|_| true); // handles
        bus.subscribe(|_| false); // ignores (e.g. a different pattern's kind)
        bus.subscribe(|_| true); // handles
        let handled = bus.emit(
            std::path::Path::new("/tmp/h"),
            EventKind::CiFail {
                target: "dev".into(),
                body: "[ci-fail]".into(),
                correlation_id: "o/r@b".into(),
                supersede_token: "ci-3".into(),
            },
        );
        assert_eq!(
            handled, 2,
            "only the handling (true) subscribers are counted"
        );

        // No subscriber at all → handled == 0 (the #1720 silent-drop signal).
        let empty = EventBus::new();
        let none = empty.emit(
            std::path::Path::new("/tmp/h"),
            EventKind::CiFail {
                target: "dev".into(),
                body: "x".into(),
                correlation_id: "o/r@b".into(),
                supersede_token: "ci-4".into(),
            },
        );
        assert_eq!(none, 0, "no subscriber → handled-count 0");
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
                target: "t".into(),
                body: "[ci-pass]".into(),
                correlation_id: "o/r@b".into(),
                supersede_token: "ci-1-aaa".into(),
            },
            EventKind::CiFail {
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
                long_running: false,
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
