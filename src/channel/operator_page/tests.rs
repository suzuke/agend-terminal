//! #3480 RED→GREEN suite. Every case drives the REAL MCP entry
//! (`mcp::handlers::handle_tool("operator_page", …)`), not the module function,
//! so the tool registration, schema validation and dispatch are all covered.
//!
//! Delivery is asserted through a recording `Channel` registered in the process
//! channel registry: that is the far side of the existing chokepoint, so a test
//! that sees a message there has proven the page really traversed
//! `notify_all_escalation_channels` → `gated_notify` rather than some private
//! shortcut.

use super::*;
use crate::channel::channel_registry_test_guard;
use crate::channel::{
    register_active_channel, reset_active_channel_for_test, BindingOpts, BindingRef, Channel,
    ChannelCapabilities, ChannelError, ChannelEvent, MsgRef, NotifySeverity, OutMsg,
};
use crate::mcp::handlers::{fleet_test_guard, handle_tool};
use parking_lot::Mutex as PlMutex;
use serial_test::serial;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── fixtures ────────────────────────────────────────────────────────────

/// Records every `notify` the chokepoint delivers: the instance it routed to
/// (which is how the dedicated-topic decision is observed), the severity that
/// passed the mode gate, and the exact text.
struct Recorder {
    /// Unique per test: `register_active_channel` keys by `kind()`, and other
    /// suites register their own "telegram" channel without taking the registry
    /// lock. Our own slot cannot be evicted by them, and the fan-out reaches
    /// every registered channel anyway.
    kind: &'static str,
    caps: ChannelCapabilities,
    authorized: AtomicBool,
    seen: PlMutex<Vec<(String, NotifySeverity, String)>>,
}

impl Recorder {
    fn arc(kind: &'static str, authorized: bool) -> Arc<Self> {
        Arc::new(Self {
            kind,
            caps: ChannelCapabilities::default(),
            authorized: AtomicBool::new(authorized),
            seen: PlMutex::new(Vec::new()),
        })
    }
    /// Only OUR pages. The channel registry is process-global and other suites
    /// fan their own escalation notices through it while this test holds the
    /// registry lock, so counting everything the recorder saw would count their
    /// traffic too. Pages are identified by the prefix the tool itself stamps.
    fn pages(&self) -> Vec<(String, NotifySeverity, String)> {
        self.seen
            .lock()
            .iter()
            .filter(|(_, _, text)| text.starts_with("[operator-page from "))
            .cloned()
            .collect()
    }
    fn count(&self) -> usize {
        self.pages().len()
    }
    fn last(&self) -> Option<(String, NotifySeverity, String)> {
        self.pages().last().cloned()
    }
}

impl Channel for Recorder {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn caps(&self) -> &ChannelCapabilities {
        &self.caps
    }
    fn poll_event(&self) -> Option<ChannelEvent> {
        None
    }
    fn send(&self, _: &BindingRef, _: OutMsg) -> anyhow::Result<MsgRef> {
        anyhow::bail!("mock")
    }
    fn edit(&self, _: &MsgRef, _: OutMsg) -> anyhow::Result<()> {
        anyhow::bail!("mock")
    }
    fn delete(&self, _: &MsgRef) -> anyhow::Result<()> {
        anyhow::bail!("mock")
    }
    fn create_binding(&self, _: &str, _: BindingOpts) -> anyhow::Result<BindingRef> {
        anyhow::bail!("mock")
    }
    fn remove_binding(&self, _: &BindingRef) -> anyhow::Result<()> {
        anyhow::bail!("mock")
    }
    fn has_binding(&self, _: &str) -> bool {
        false
    }
    fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
    fn take_binding(&self, _: &str) -> Option<BindingRef> {
        None
    }
    fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
    fn notify(
        &self,
        instance: &str,
        severity: NotifySeverity,
        message: &str,
        _silent: bool,
    ) -> std::result::Result<(), ChannelError> {
        self.seen
            .lock()
            .push((instance.to_string(), severity, message.to_string()));
        Ok(())
    }
    fn outbound_authorized(&self) -> bool {
        self.authorized.load(Ordering::Relaxed)
    }
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU32;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-operator-page-{}-{tag}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `enabled` toggles the opt-in switch; `allowlisted` toggles the CHANNEL's
/// outbound authorization (the fail-closed `user_allowlist` gate's observable).
fn setup(tag: &str, enabled: bool, allowlisted: bool) -> (std::path::PathBuf, Arc<Recorder>) {
    let home = tmp_home(tag);
    std::env::set_var("AGEND_HOME", &home);
    let page_stanza = if enabled {
        "  operator_page:\n    enabled: true\n"
    } else {
        ""
    };
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
             teams:\n  archfix:\n    orchestrator: lead\n    members: [lead, worker]\n\
             channel:\n  type: telegram\n\
             \x20 bot_token_env: AGEND_TEST_UNSET_TOKEN_3480\n\
             \x20 group_id: -100123\n  mode: topic\n\
             \x20 user_allowlist: [42]\n{page_stanza}"
        ),
    )
    .expect("write fleet.yaml");
    reset_active_channel_for_test();
    let kind: &'static str = Box::leak(format!("telegram-op-page-{tag}").into_boxed_str());
    let rec = Recorder::arc(kind, allowlisted);
    register_active_channel(rec.clone());
    (home, rec)
}

fn teardown(home: &std::path::Path) {
    reset_active_channel_for_test();
    std::env::remove_var("AGEND_HOME");
    std::fs::remove_dir_all(home).ok();
}

/// Drives the REAL tool entry, re-asserting `AGEND_HOME` immediately before the
/// call.
///
/// 116 tests in this crate mutate `AGEND_HOME`, and one file does so under a
/// NAMED `#[serial(...)]` key, which does not mutually exclude with our global
/// `#[serial]`. Setting it once in `setup` leaves a window as wide as the whole
/// test in which a foreign mutation can redirect the handler at another
/// fixture's home; re-asserting per call shrinks that window to microseconds.
/// Observed as a real flake: this suite failed once in a full-suite run and
/// passed clean in the next.
fn page(home: &std::path::Path, caller: &str, text: &str) -> serde_json::Value {
    std::env::set_var("AGEND_HOME", home);
    handle_tool(
        "operator_page",
        &serde_json::json!({ "message": text }),
        caller,
    )
}

// ── the eight required cases, plus the ordering case ───────────────────

/// AUTHORITY: only the caller's own team orchestrator may page. A member is
/// refused and told who to route through — and nothing reaches the channel.
#[test]
#[serial]
fn non_orchestrator_caller_is_refused() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("authz", true, true);

    let out = page(&home, "worker", "milestone");

    assert_eq!(out["code"], "not_orchestrator", "{out}");
    assert_eq!(
        out["your_orchestrator"], "lead",
        "the refusal must name the orchestrator to route through: {out}"
    );
    assert_eq!(rec.count(), 0, "a refused page must send nothing");
    teardown(&home);
}

/// DEFAULT OFF: with no `operator_page` stanza the tool refuses and sends
/// nothing — the switch is the operator's master control.
#[test]
#[serial]
fn disabled_switch_refuses_and_sends_nothing() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("disabled", false, true);

    let out = page(&home, "lead", "milestone");

    assert_eq!(out["code"], "operator_page_disabled", "{out}");
    assert_eq!(rec.count(), 0, "a disabled tool must send nothing");
    teardown(&home);
}

/// RATE: 3 per rolling hour, the 4th DROPPED (never queued) with a retry-after
/// so the caller can fall back to SESSION-HANDOFF.md.
#[test]
#[serial]
fn fourth_page_in_the_hour_is_dropped_with_retry_after() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("rate", true, true);

    for i in 1..=RATE_LIMIT_PER_HOUR {
        let out = page(&home, "lead", &format!("milestone {i}"));
        assert_eq!(out["sent"], true, "page {i} must succeed: {out}");
    }
    assert_eq!(rec.count(), RATE_LIMIT_PER_HOUR, "three pages delivered");

    let out = page(&home, "lead", "milestone 4");

    assert_eq!(out["code"], "rate_limited", "{out}");
    let retry = out["retry_after_secs"].as_i64().unwrap_or(-1);
    assert!(
        retry > 0 && retry <= 3600,
        "refusal must carry a usable retry-after, got {retry}: {out}"
    );
    assert_eq!(
        rec.count(),
        RATE_LIMIT_PER_HOUR,
        "the 4th page must be DROPPED, not queued or sent"
    );
    teardown(&home);
}

/// RATE DURABILITY: the counter lives on disk, so a daemon restart is not a
/// bypass. The sidecar is the only state; re-reading it must still refuse.
#[test]
#[serial]
fn rate_counter_survives_a_simulated_restart() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("durable", true, true);

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&home, "lead", &format!("m{i}"))["sent"], true);
    }
    let sidecar = home.join("operator_page_rate.json");
    assert!(
        sidecar.exists(),
        "the rate counter must be durable on disk, not in memory"
    );

    // Simulated restart: nothing in this process is reused — the next call must
    // rebuild its count from the sidecar alone.
    reset_active_channel_for_test();
    let rec2 = Recorder::arc("telegram-op-page-durable-2", true);
    register_active_channel(rec2.clone());

    let out = page(&home, "lead", "after restart");

    assert_eq!(
        out["code"], "rate_limited",
        "a restart must not reset the hourly budget: {out}"
    );
    assert_eq!(rec2.count(), 0, "nothing sent after the restart");
    assert_eq!(rec.count(), RATE_LIMIT_PER_HOUR);
    teardown(&home);
}

/// ROUTING: pages land in the DEDICATED operator topic, not the sender's own
/// topic, so they collect in one place the operator can mute.
#[test]
#[serial]
fn dedicated_topic_is_used_when_registered() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("topic", true, true);
    // Pre-registering is what `create_topic_for_instance` finds on the reuse
    // path; creating one for real needs the Telegram API, which this suite
    // deliberately never calls.
    crate::channel::telegram::topic_registry::register_topic(&home, 77, "operator-notifications")
        .expect("register dedicated topic");

    assert_eq!(page(&home, "lead", "milestone")["sent"], true);

    let (routed_to, _, _) = rec.last().expect("a page was delivered");
    assert_eq!(
        routed_to, "operator-notifications",
        "the page must route to the dedicated topic, not the sender's"
    );
    teardown(&home);
}

/// ROUTING FALLBACK: when the dedicated topic cannot be resolved or created,
/// the page still goes out — on the sender's own topic. Routing may fail open
/// because both topics sit inside the allowlisted group; the gates do not.
#[test]
#[serial]
fn topic_creation_failure_falls_back_to_sender_topic() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    // No topics.json entry and no reachable Telegram API, so creation fails.
    let (home, rec) = setup("fallback", true, true);

    assert_eq!(page(&home, "lead", "milestone")["sent"], true);

    let (routed_to, _, _) = rec.last().expect("a page was delivered");
    assert_eq!(
        routed_to, "lead",
        "a failed dedicated-topic resolution must fall back to the sender topic"
    );
    teardown(&home);
}

/// CONTENT: the operator always learns who paged, and no caller can page a wall
/// of text.
#[test]
#[serial]
fn page_carries_sender_prefix_and_respects_length_cap() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("content", true, true);

    let long = "x".repeat(MAX_PAGE_CHARS * 3);
    assert_eq!(page(&home, "lead", &long)["sent"], true);

    let (_, _, text) = rec.last().expect("a page was delivered");
    assert!(
        text.contains("lead"),
        "the page must name its sender: {text:.80}"
    );
    let body_len = text.matches('x').count();
    assert_eq!(
        body_len, MAX_PAGE_CHARS,
        "the body must be capped at MAX_PAGE_CHARS"
    );
    teardown(&home);
}

/// The pre-existing fail-closed allowlist gate must still bind: an unauthorized
/// channel drops the page even though every one of the tool's own gates passed.
#[test]
#[serial]
fn unauthorized_channel_still_drops_the_page() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("allowlist", true, false);

    let out = page(&home, "lead", "milestone");

    assert_eq!(rec.count(), 0, "the fail-closed gate must not be bypassed");
    assert_eq!(
        out["sent"], false,
        "the caller must learn the page was not delivered: {out}"
    );
    assert_eq!(out["code"], "not_delivered", "{out}");
    teardown(&home);
}

/// Decision d-20260902104216571473-11 condition 2: Error severity exists to
/// survive Away/Sleep, and must NOT let a page skip the tool's own gates. Under
/// Sleep the first three arrive (proving the severity choice works) and the 4th
/// is still refused by the rate gate (proving the gate runs first).
#[test]
#[serial]
fn rate_gate_binds_even_though_severity_would_pass_the_mode_gate() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let (home, rec) = setup("mode", true, true);
    crate::operator_mode::set_mode(
        &home,
        crate::operator_mode::OperatorMode::Sleep,
        None,
        vec![],
    )
    .expect("set sleep mode");

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(
            page(&home, "lead", &format!("m{i}"))["sent"],
            true,
            "an operator page must survive Sleep — that is the whole point of #3480"
        );
    }
    let (_, severity, _) = rec.last().expect("delivered under Sleep");
    assert_eq!(
        severity,
        NotifySeverity::Error,
        "pages ride Error severity so the Sleep gate passes them"
    );

    let out = page(&home, "lead", "m4");

    assert_eq!(
        out["code"], "rate_limited",
        "the rate gate must bind before severity buys anything: {out}"
    );
    assert_eq!(rec.count(), RATE_LIMIT_PER_HOUR);
    teardown(&home);
}
