//! #3480 RED→GREEN suite. Every case drives the REAL MCP entry
//! (`mcp::handlers::handle_tool*("operator_page", …)`), not the module function,
//! so the tool registration, schema validation and dispatch are all covered.
//!
//! Delivery is asserted through a recording `Channel` registered in the process
//! channel registry: that is the far side of the existing chokepoint, so a test
//! that sees a message there has proven the page really traversed
//! `notify_all_escalation_channels` → `gated_notify` rather than some private
//! shortcut.
//!
//! **Hermeticity (#3480 C).** Two structural guarantees, not naming conventions:
//! the fixture pins BOTH bot-token env names (`creds.rs` falls back from a
//! configured name to the canonical one AND to the legacy one, so leaving a
//! fixture's `bot_token_env` unset does not stop a host-exported operator token
//! from being used), and every case installs the `topic_registry` creation seam,
//! which answers before `resolve_channel_only_from` / `Bot::new` are reached.
//! `bot_api_entered_for_home` is the standing proof that the API branch was never
//! entered for this fixture's home.
//!
//! **Trust model (d-20260902…-36).** Every agent and the daemon share one OS
//! user, so no in-daemon gate can make the caller-supplied instance name
//! spoof-proof, and nothing here claims otherwise. What the authority gate does
//! claim, and what these tests pin, is narrower: a name that does not resolve to
//! a LIVE instance is refused, an AMBIGUOUS name is refused, a caller owned by
//! more than one team is refused, and the resolved live instance must be its
//! team's current orchestrator. The boundary that remains is pinned by
//! `any_seat_presenting_the_orchestrators_live_name_is_admitted_by_design`.
//!
//! **Why the fixture enables paging through `runtime_config::set`.** The rate
//! budget is SEEDED by the operator's enable command and refuses to invent a
//! snapshot at initialisation (an absent one denies — see
//! `operator_page/budget.rs`). Hand-writing `runtime-config.json` would set the
//! switch without seeding, so every case here goes through the real operator path
//! and therefore starts from a state a real deployment can actually be in.
//!
//! **Why every case carries TWO serial keys.** `#[serial]` (the unnamed group)
//! excludes against the other suites that mutate `AGEND_HOME` and the bot-token
//! env pair. `#[serial(runtime_config)]` excludes against the runtime-config
//! suite: the master switch now lives in the process-global `RuntimeConfig`, and
//! ANY `runtime_config::reload` from another home resets `operator_page.enabled`
//! to its default of false. The gap between this suite's own reload and the
//! handler's `get()` spans two disk writes (usage stats + heartbeat), which is
//! milliseconds — wide enough that this was not theoretical: it reddened one test
//! per run, a different one each time, until both keys were held.

use super::*;
use crate::channel::channel_registry_test_guard;
use crate::channel::telegram::topic_registry::{
    bot_api_entered_for_home, install_topic_seam_for_test, reset_topic_seam_for_test,
    topic_seam_calls_for_test,
};
use crate::channel::{
    register_active_channel, reset_active_channel_for_test, BindingOpts, BindingRef, Channel,
    ChannelCapabilities, ChannelError, ChannelEvent, MsgRef, NotifySeverity, OutMsg,
};
use crate::mcp::handlers::dispatch::RuntimeContext;
use crate::mcp::handlers::{fleet_test_guard, handle_tool, handle_tool_with_runtime};
use parking_lot::Mutex as PlMutex;
use serial_test::serial;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── fixtures ────────────────────────────────────────────────────────────

/// Records every `notify` the chokepoint delivers: the instance it routed to
/// (which is how the dedicated-topic decision is observed), the severity that
/// passed the mode gate, and the exact text.
struct Recorder {
    /// #3480 review fix: the deliverability gate now demands a TELEGRAM channel
    /// specifically (a Discord-only allowlist used to return `sent: true` while
    /// nothing reached the phone), so the recorder must claim the kind under
    /// test. Registration keys by `kind()`; every case here holds
    /// `channel_registry_test_guard()`, which is what keeps that safe.
    kind: &'static str,
    caps: ChannelCapabilities,
    authorized: AtomicBool,
    /// When set, the FIRST `outbound_authorized()` query clears the process
    /// channel registry — the only way to reach the fan-out with zero channels
    /// from outside the handler, which is what makes the zero-channel rollback
    /// observable. (Zero channels is the only outcome the fan-out's return value
    /// actually proves; see the rollback site in `operator_page.rs`.)
    vanish_on_query: AtomicBool,
    seen: PlMutex<Vec<(String, NotifySeverity, String)>>,
}

impl Recorder {
    fn arc(kind: &'static str, authorized: bool) -> Arc<Self> {
        Arc::new(Self {
            kind,
            caps: ChannelCapabilities::default(),
            authorized: AtomicBool::new(authorized),
            vanish_on_query: AtomicBool::new(false),
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
        if self.vanish_on_query.swap(false, Ordering::Relaxed) {
            reset_active_channel_for_test();
        }
        self.authorized.load(Ordering::Relaxed)
    }
}

/// Both names `creds.rs` can resolve a bot token under. Pinned to dummies for the
/// whole suite so an operator token exported on the host can never be picked up.
const CANONICAL_TOKEN_ENV: &str = "AGEND_TELEGRAM_BOT_TOKEN";
const LEGACY_TOKEN_ENV: &str = "AGEND_BOT_TOKEN";
const DUMMY_TOKEN: &str = "0000000000:agend-test-dummy-token";

struct TokenEnvGuard {
    canonical: Option<String>,
    legacy: Option<String>,
}

/// Free function (not a `Drop` impl) on purpose: the env-mutation census in
/// `tests/env_mutation_serialization_invariant.rs` only walks module-level
/// functions, so keeping the mutation here is what makes the two token keys
/// visible to it — and therefore what forces the registered SERIALIZED_PAIRS rows.
fn install_dummy_bot_tokens(canonical: &str) -> TokenEnvGuard {
    let guard = TokenEnvGuard {
        canonical: std::env::var(CANONICAL_TOKEN_ENV).ok(),
        legacy: std::env::var(LEGACY_TOKEN_ENV).ok(),
    };
    std::env::set_var(CANONICAL_TOKEN_ENV, canonical);
    std::env::set_var(LEGACY_TOKEN_ENV, DUMMY_TOKEN);
    guard
}

fn restore_bot_tokens(guard: &TokenEnvGuard) {
    match &guard.canonical {
        Some(value) => std::env::set_var(CANONICAL_TOKEN_ENV, value),
        None => std::env::remove_var(CANONICAL_TOKEN_ENV),
    }
    match &guard.legacy {
        Some(value) => std::env::set_var(LEGACY_TOKEN_ENV, value),
        None => std::env::remove_var(LEGACY_TOKEN_ENV),
    }
}

/// One team, one orchestrator — the shape most cases want.
const STD_FLEET: &str =
    "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
     teams:\n  archfix:\n    orchestrator: lead\n    members: [lead, worker]\n";

/// Two teams that BOTH list `lead` — the ambiguity a `.values().find(...)` lookup
/// would answer from HashMap order.
const TWO_OWNING_TEAMS: &str =
    "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
     teams:\n  archfix:\n    orchestrator: lead\n    members: [lead, worker]\n\
     \x20 hotfix:\n    orchestrator: lead\n    members: [lead]\n";

/// Two teams with DIFFERENT orchestrators — distinct budgets must not share.
const TWO_ORCHESTRATORS: &str =
    "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
     \x20 lead2:\n    backend: claude\n\
     teams:\n  archfix:\n    orchestrator: lead\n    members: [lead, worker]\n\
     \x20 other:\n    orchestrator: lead2\n    members: [lead2]\n";

/// A team whose declared orchestrator is not a live instance.
const STALE_ORCHESTRATOR: &str =
    "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
     teams:\n  archfix:\n    orchestrator: departed-lead\n    members: [lead, worker]\n";

const CHANNEL_STANZA: &str = "channel:\n  type: telegram\n\
     \x20 bot_token_env: AGEND_TEST_UNSET_TOKEN_3480\n\
     \x20 group_id: -100123\n  mode: topic\n  user_allowlist: [42]\n";

struct Spec {
    /// The master switch, now a DAEMON-PRIVATE runtime-config stanza rather than
    /// an agent-writable fleet.yaml one.
    enabled: bool,
    channel_kind: &'static str,
    authorized: bool,
    fleet: &'static str,
    /// Instance names present in the live registry the handler resolves against.
    live: &'static [&'static str],
    /// What the topic-creation seam answers with.
    topic_outcome: Option<i32>,
    canonical_token: &'static str,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            enabled: true,
            channel_kind: "telegram",
            authorized: true,
            fleet: STD_FLEET,
            live: &["lead", "worker"],
            topic_outcome: None,
            canonical_token: DUMMY_TOKEN,
        }
    }
}

struct Fixture {
    home: PathBuf,
    rec: Arc<Recorder>,
    runtime: RuntimeContext,
    tokens: TokenEnvGuard,
}

fn tmp_home(tag: &str) -> PathBuf {
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

/// Lay down a `runtime-config.json` directly. Used to create the file and to
/// leave the switch OFF at teardown; ENABLING goes through
/// `runtime_config::set` instead (see `setup`), because that is the call that
/// also seeds the rate snapshot.
fn write_runtime_config(home: &Path, enabled: bool) {
    std::fs::write(
        home.join("runtime-config.json"),
        format!(
            r#"{{"operator_page":{{"enabled":{enabled},"topic_name":"operator-notifications"}}}}"#
        ),
    )
    .expect("write runtime-config.json");
}

fn live_registry(names: &[&str]) -> crate::agent::AgentRegistry {
    let mut map = std::collections::HashMap::new();
    for name in names {
        let id = crate::types::InstanceId::new();
        map.insert(id, crate::agent::mk_test_handle(name, id));
    }
    Arc::new(PlMutex::new(map))
}

fn runtime_with(registry: crate::agent::AgentRegistry) -> RuntimeContext {
    RuntimeContext {
        registry,
        configs: Default::default(),
        externals: Arc::new(PlMutex::new(std::collections::HashMap::new())),
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: None,
        notifier: None,
        shutdown: None,
    }
}

fn setup(tag: &str, spec: Spec) -> Fixture {
    let home = tmp_home(tag);
    std::env::set_var("AGEND_HOME", &home);
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("{}{CHANNEL_STANZA}", spec.fleet),
    )
    .expect("write fleet.yaml");
    write_runtime_config(&home, false);
    if spec.enabled {
        // The operator's own act, not a hand-written file: `set` writes the switch
        // AND seeds `operator_page_rate.json`. Initialisation deliberately will not
        // seed itself, so a case that skipped this would find a poisoned budget.
        crate::runtime_config::set(&home, "operator_page.enabled", "true")
            .expect("the operator enables paging through the CLI path");
    }
    crate::runtime_config::reload(&home);
    reset_active_channel_for_test();
    let rec = Recorder::arc(spec.channel_kind, spec.authorized);
    register_active_channel(rec.clone());
    install_topic_seam_for_test(&home, spec.topic_outcome);
    Fixture {
        home,
        rec,
        runtime: runtime_with(live_registry(spec.live)),
        tokens: install_dummy_bot_tokens(spec.canonical_token),
    }
}

fn teardown(fx: &Fixture) {
    reset_active_channel_for_test();
    reset_topic_seam_for_test();
    // The rate budget is a process-global. Per-test homes already force a
    // re-initialisation, but clearing it explicitly means no case can inherit a
    // poisoned latch from the one before it.
    budget::reset_for_test();
    restore_bot_tokens(&fx.tokens);
    // Leave the process-global runtime config switched OFF: it is default-off in
    // production and a test must not hand the next suite an enabled pager.
    write_runtime_config(&fx.home, false);
    crate::runtime_config::reload(&fx.home);
    std::env::remove_var("AGEND_HOME");
    std::fs::remove_dir_all(&fx.home).ok();
}

fn rate_snapshot(home: &Path) -> PathBuf {
    home.join("operator_page_rate.json")
}

/// The real tool entry with a live daemon runtime attached — the in-process API
/// path, which is the only path that can resolve a caller to a live instance.
fn page_with(runtime: &RuntimeContext, caller: &str, text: &str) -> serde_json::Value {
    handle_tool_with_runtime(
        "operator_page",
        &serde_json::json!({ "message": text }),
        caller,
        Some(runtime.clone()),
    )
}

/// Drives the REAL tool entry, re-asserting `AGEND_HOME` and the runtime-config
/// snapshot immediately before the call.
///
/// 116 tests in this crate mutate `AGEND_HOME`, and one file does so under a
/// NAMED `#[serial(...)]` key, which does not mutually exclude with our global
/// `#[serial]`. Setting it once in `setup` leaves a window as wide as the whole
/// test in which a foreign mutation can redirect the handler at another
/// fixture's home; re-asserting per call shrinks that window to microseconds.
/// Observed as a real flake: this suite failed once in a full-suite run and
/// passed clean in the next. The runtime-config global is process-wide for the
/// same reason and gets the same treatment.
fn page(fx: &Fixture, caller: &str, text: &str) -> serde_json::Value {
    std::env::set_var("AGEND_HOME", &fx.home);
    crate::runtime_config::reload(&fx.home);
    page_with(&fx.runtime, caller, text)
}

/// The standalone-bridge entry: no daemon runtime, therefore no live registry.
fn page_without_runtime(fx: &Fixture, caller: &str, text: &str) -> serde_json::Value {
    std::env::set_var("AGEND_HOME", &fx.home);
    crate::runtime_config::reload(&fx.home);
    handle_tool(
        "operator_page",
        &serde_json::json!({ "message": text }),
        caller,
    )
}

// ── authority ───────────────────────────────────────────────────────────

/// AUTHORITY: only the caller's own team orchestrator may page. A member is
/// refused and told who to route through — and nothing reaches the channel.
#[test]
#[serial]
#[serial(runtime_config)]
fn non_orchestrator_caller_is_refused() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("authz", Spec::default());

    let out = page(&fx, "worker", "milestone");

    assert_eq!(out["code"], "not_orchestrator", "{out}");
    assert_eq!(
        out["your_orchestrator"], "lead",
        "the refusal must name the orchestrator to route through: {out}"
    );
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

/// AUTHORITY: the claimed name must resolve to a LIVE instance. A name that no
/// live handle answers to — never spawned, or already dead — is refused before
/// any team lookup, so a stale fleet.yaml row cannot stand in for a running seat.
#[test]
#[serial]
#[serial(runtime_config)]
fn unknown_or_dead_claimed_name_is_refused() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("unknown", Spec::default());

    let out = page(&fx, "ghost", "milestone");

    assert_eq!(out["code"], "unknown_caller", "{out}");
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

/// AUTHORITY: an AMBIGUOUS name (two live handles answering to it) resolves to
/// nothing. `live_requester_id` is already fail-closed on this; the tool must not
/// paper over it with a first-match.
#[test]
#[serial]
#[serial(runtime_config)]
fn ambiguous_live_name_is_refused() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "ambiguous-name",
        Spec {
            live: &["lead", "lead"],
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["code"], "unknown_caller", "{out}");
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

/// AUTHORITY: a caller owned by TWO teams has no single orchestrator, and the
/// answer must not come from HashMap iteration order. Refuse instead of guessing.
#[test]
#[serial]
#[serial(runtime_config)]
fn caller_owned_by_two_teams_is_refused_as_ambiguous() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "ambiguous-team",
        Spec {
            fleet: TWO_OWNING_TEAMS,
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["code"], "ambiguous_team", "{out}");
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

/// AUTHORITY: a team naming an orchestrator that is not a live instance grants
/// nobody the page. The live member is not promoted into the vacancy.
#[test]
#[serial]
#[serial(runtime_config)]
fn stale_orchestrator_grants_nobody_the_page() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "stale-orch",
        Spec {
            fleet: STALE_ORCHESTRATOR,
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["code"], "not_orchestrator", "{out}");
    assert_eq!(
        out["your_orchestrator"], "departed-lead",
        "the refusal must still name the configured orchestrator: {out}"
    );
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

/// THE HONEST BOUNDARY (d-20260902…-36). Every agent and the daemon run as ONE
/// OS user, so the daemon cannot tell which seat typed a tool call: a worker that
/// presents the orchestrator's live name IS admitted, by design, and no gate
/// added here changes that. This test exists to pin that fact so nobody later
/// mistakes the live-identity gate for spoof resistance.
///
/// What the gate does buy is the rest of the matrix above (dead names, ambiguous
/// names, ambiguous teams, non-orchestrators). What actually bounds the damage is
/// elsewhere: default-OFF, 3 pages per rolling hour, one dedicated topic inside
/// the allowlisted group, and fail-closed budget state.
#[test]
#[serial]
#[serial(runtime_config)]
fn any_seat_presenting_the_orchestrators_live_name_is_admitted_by_design() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("boundary", Spec::default());

    let out = page(&fx, "lead", "milestone");

    assert_eq!(
        out["sent"], true,
        "presenting the orchestrator's live name is admitted — the shared-OS-user \
         boundary this tool cannot close: {out}"
    );
    assert_eq!(fx.rec.count(), 1);
    teardown(&fx);
}

/// AUTHORITY: with no daemon runtime there is no registry, so there is no live
/// identity to bind authority to. A standalone bridge call must fail CLOSED
/// rather than fall back to trusting the name it was handed.
#[test]
#[serial]
#[serial(runtime_config)]
fn standalone_call_without_runtime_is_refused() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("no-runtime", Spec::default());

    let out = page_without_runtime(&fx, "lead", "milestone");

    assert_eq!(out["code"], "no_live_identity", "{out}");
    assert_eq!(fx.rec.count(), 0, "a refused page must send nothing");
    teardown(&fx);
}

// ── switch ──────────────────────────────────────────────────────────────

/// DEFAULT OFF: with the runtime-config stanza disabled the tool refuses and
/// sends nothing — the switch is the operator's master control, and it now lives
/// where an agent cannot write it.
#[test]
#[serial]
#[serial(runtime_config)]
fn disabled_switch_refuses_and_sends_nothing() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "disabled",
        Spec {
            enabled: false,
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["code"], "operator_page_disabled", "{out}");
    assert_eq!(fx.rec.count(), 0, "a disabled tool must send nothing");
    teardown(&fx);
}

/// The switch must NOT be reachable from fleet.yaml any more: that file is
/// agent-writable, which is exactly why the master control moved. An
/// `operator_page` stanza left in fleet.yaml grants nothing.
#[test]
#[serial]
#[serial(runtime_config)]
fn fleet_yaml_stanza_cannot_enable_the_tool() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "fleet-switch",
        Spec {
            enabled: false,
            ..Spec::default()
        },
    );
    let yaml = crate::fleet::fleet_yaml_path(&fx.home);
    let body = std::fs::read_to_string(&yaml).expect("read fleet.yaml");
    std::fs::write(
        &yaml,
        format!("{body}  operator_page:\n    enabled: true\n"),
    )
    .expect("write fleet.yaml");

    let out = page(&fx, "lead", "milestone");

    assert_eq!(
        out["code"], "operator_page_disabled",
        "an agent-writable fleet.yaml stanza must not be a master switch: {out}"
    );
    assert_eq!(fx.rec.count(), 0);
    teardown(&fx);
}

// ── rate budget ─────────────────────────────────────────────────────────

/// RATE: 3 per rolling hour, the 4th DROPPED (never queued) with a retry-after
/// so the caller can fall back to SESSION-HANDOFF.md.
#[test]
#[serial]
#[serial(runtime_config)]
fn fourth_page_in_the_hour_is_dropped_with_retry_after() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("rate", Spec::default());

    for i in 1..=RATE_LIMIT_PER_HOUR {
        let out = page(&fx, "lead", &format!("milestone {i}"));
        assert_eq!(out["sent"], true, "page {i} must succeed: {out}");
    }
    assert_eq!(fx.rec.count(), RATE_LIMIT_PER_HOUR, "three pages delivered");

    let out = page(&fx, "lead", "milestone 4");

    assert_eq!(out["code"], "rate_limited", "{out}");
    let retry = out["retry_after_secs"].as_i64().unwrap_or(-1);
    assert!(
        retry > 0 && retry <= 3600,
        "refusal must carry a usable retry-after, got {retry}: {out}"
    );
    assert_eq!(
        fx.rec.count(),
        RATE_LIMIT_PER_HOUR,
        "the 4th page must be DROPPED, not queued or sent"
    );
    teardown(&fx);
}

/// RATE DURABILITY: the in-memory counter is snapshotted so a daemon restart is
/// not a bypass. The snapshot must hold the full spent budget.
#[test]
#[serial]
#[serial(runtime_config)]
fn rate_counter_survives_a_simulated_restart() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("durable", Spec::default());

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&fx, "lead", &format!("m{i}"))["sent"], true);
    }
    let snapshot = rate_snapshot(&fx.home);
    let stamps: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snapshot).expect("snapshot readable"))
            .expect("snapshot parses");
    assert_eq!(
        stamps["lead"].as_array().map(Vec::len),
        Some(RATE_LIMIT_PER_HOUR),
        "the snapshot must carry the whole spent budget for restart continuity: {stamps}"
    );

    // The restart itself: drop everything the process was holding, so the next
    // call has to rebuild the spent budget from the snapshot alone.
    budget::reset_for_test();
    let out = page(&fx, "lead", "after restart");

    assert_eq!(
        out["code"], "rate_limited",
        "a restart must not reset the hourly budget: {out}"
    );
    assert_eq!(fx.rec.count(), RATE_LIMIT_PER_HOUR);
    teardown(&fx);
}

/// RATE INTEGRITY: the budget is authoritative IN MEMORY behind a lock; the file
/// is only a restart snapshot. Deleting it is tampering, not a fresh install, and
/// must DENY rather than refill — and it must say so with its own code, so an
/// operator can tell "the state is untrustworthy" from "you used your 3 pages".
#[test]
#[serial]
#[serial(runtime_config)]
fn deleting_the_snapshot_denies_and_is_distinguishable_from_a_rate_cap() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("tamper", Spec::default());

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&fx, "lead", &format!("m{i}"))["sent"], true);
    }
    let capped = page(&fx, "lead", "m4");
    assert_eq!(capped["code"], "rate_limited", "{capped}");

    std::fs::remove_file(rate_snapshot(&fx.home)).expect("remove the snapshot");
    let tampered = page(&fx, "lead", "m5");

    assert_eq!(
        tampered["code"], "budget_unavailable",
        "a snapshot that vanished after init is tampering, not a fresh install: {tampered}"
    );
    assert_ne!(
        tampered["code"], capped["code"],
        "a poisoned budget must not be reported as an ordinary rate cap"
    );
    assert!(
        tampered["error"]
            .as_str()
            .unwrap_or_default()
            .contains("missing"),
        "the refusal must name the cause so the operator can act: {tampered}"
    );
    assert_eq!(
        tampered["cause"], "snapshot_missing",
        "a deletion caught by a RUNNING daemon is distinct from an absent one: {tampered}"
    );
    assert!(
        !tampered["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("STARTS A NEW ROLLING HOUR"),
        "memory still holds the spent count here, so re-seeding does NOT restart the hour — \
         the hint must not warn about a reset that will not happen: {tampered}"
    );
    assert_eq!(
        fx.rec.count(),
        RATE_LIMIT_PER_HOUR,
        "deleting the counter must not buy a page"
    );
    teardown(&fx);
}

/// RATE INTEGRITY, the defect an adversarial verification pass PROVED. The first
/// design treated an absent snapshot at initialisation as a fresh deployment and
/// started empty. Since every agent and the daemon share one OS user, an agent
/// could delete `operator_page_rate.json`, trigger a daemon restart, and have the
/// new process refill the hour — a 3-per-hour cap degraded to 3-per-restart.
///
/// Absent at initialisation now DENIES with its own cause. This does not make the
/// file tamper-proof (nothing under one shared OS user is); it makes deleting it
/// a denial the operator has to undo, never a way through.
#[test]
#[serial]
#[serial(runtime_config)]
fn deleting_the_snapshot_and_restarting_denies_instead_of_refilling() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("absent", Spec::default());

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&fx, "lead", &format!("m{i}"))["sent"], true);
    }
    std::fs::remove_file(rate_snapshot(&fx.home)).expect("delete the snapshot");
    // The restart: the new process has only the (now absent) snapshot to go on.
    budget::reset_for_test();

    let out = page(&fx, "lead", "delete the counter, then restart");

    assert_eq!(out["code"], "budget_unavailable", "{out}");
    assert_eq!(
        out["cause"], "snapshot_absent",
        "an absent snapshot must be distinguishable from a corrupt one: {out}"
    );
    assert_eq!(
        fx.rec.count(),
        RATE_LIMIT_PER_HOUR,
        "delete-plus-restart must not buy a fourth page"
    );
    // …and the operator is told, BEFORE they run it, what the remedy costs. By
    // this point the spent count has been destroyed with the snapshot, so
    // re-seeding writes `{}` and hands back a full budget inside the same hour.
    // That reset is acceptable — it is operator-gated and denies by default — but
    // asking the operator to perform it blind is not, which is what a second
    // adversarial pass caught. Nothing here claims the snapshot is tamper-PROOF;
    // under one shared OS user it is tamper-EVIDENT and operator-recoverable.
    let hint = out["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("STARTS A NEW ROLLING HOUR"),
        "the remedy hint must say that re-seeding restarts the rolling hour: {out}"
    );
    assert!(
        hint.contains("config-set operator_page.enabled true"),
        "the remedy hint must still name the operator command: {out}"
    );
    teardown(&fx);
}

/// …and the operator's enable command is what seeds the snapshot in the first
/// place, with an EMPTY budget. That is the whole reason initialisation is
/// allowed to refuse: a fresh install is not denied its first page, because
/// turning the tool on is itself the seeding act.
#[test]
#[serial]
#[serial(runtime_config)]
fn enabling_through_the_operator_path_seeds_an_empty_snapshot() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "seed",
        Spec {
            enabled: false,
            ..Spec::default()
        },
    );
    assert!(
        !rate_snapshot(&fx.home).exists(),
        "a home where paging was never enabled has no snapshot"
    );

    crate::runtime_config::set(&fx.home, "operator_page.enabled", "true")
        .expect("the operator turns paging on");

    let raw = std::fs::read_to_string(rate_snapshot(&fx.home))
        .expect("the enable command must lay down the snapshot");
    let seeded: serde_json::Value = serde_json::from_str(&raw).expect("the seed parses");
    assert_eq!(
        seeded.as_object().map(serde_json::Map::len),
        Some(0),
        "the seed must be an EMPTY budget, not a pre-spent one: {raw}"
    );
    assert_eq!(
        page(&fx, "lead", "first page on a fresh install")["sent"],
        true,
        "seeding must produce a usable budget, not merely a file"
    );
    teardown(&fx);
}

/// …and re-running it never clobbers a live snapshot, so an operator who enables
/// twice in an hour does not refund the pages already spent.
#[test]
#[serial]
#[serial(runtime_config)]
fn re_enabling_does_not_overwrite_a_spent_snapshot() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("reseed", Spec::default());

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&fx, "lead", &format!("m{i}"))["sent"], true);
    }
    let before = std::fs::read_to_string(rate_snapshot(&fx.home)).expect("snapshot readable");

    crate::runtime_config::set(&fx.home, "operator_page.enabled", "true")
        .expect("the operator enables again");

    assert_eq!(
        std::fs::read_to_string(rate_snapshot(&fx.home)).expect("snapshot readable"),
        before,
        "seeding must leave an existing snapshot byte-for-byte alone"
    );
    assert_eq!(
        page(&fx, "lead", "m4")["code"],
        "rate_limited",
        "re-enabling must not refund the hour"
    );
    teardown(&fx);
}

/// RATE INTEGRITY: a snapshot that will not parse is poison, not an empty budget.
/// The pre-#3480 read silently produced an EMPTY map, so corrupting the file
/// refilled the hour — fail OPEN on the one file an agent can write.
#[test]
#[serial]
#[serial(runtime_config)]
fn corrupt_snapshot_denies_instead_of_resetting_the_budget() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("corrupt", Spec::default());
    std::fs::write(rate_snapshot(&fx.home), "{ this is not json").expect("write corrupt snapshot");

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["code"], "budget_unavailable", "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("corrupt"),
        "the refusal must name the cause so the operator can act: {out}"
    );
    assert_eq!(out["cause"], "snapshot_corrupt", "{out}");
    assert!(
        out["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("STARTS A NEW ROLLING HOUR"),
        "a corrupt snapshot destroys the count exactly as an absent one does, so the \
         remedy hint must carry the same warning: {out}"
    );
    assert_eq!(fx.rec.count(), 0, "a poisoned budget must send nothing");
    teardown(&fx);
}

/// RATE INTEGRITY: stamps dated in the FUTURE (a clock jump, or a hand-edited
/// snapshot) must still count against the hour. Pruning on "older than an hour"
/// alone would keep them; pruning on absolute distance would drop them and hand
/// the caller a refilled budget.
#[test]
#[serial]
#[serial(runtime_config)]
fn future_dated_stamps_do_not_grant_extra_slots() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("skew", Spec::default());
    let now = chrono::Utc::now().timestamp();
    std::fs::write(
        rate_snapshot(&fx.home),
        serde_json::json!({ "lead": [now + 9000, now + 9001, now + 9002] }).to_string(),
    )
    .expect("write skewed snapshot");

    let out = page(&fx, "lead", "milestone");

    assert_eq!(
        out["code"], "rate_limited",
        "future-dated stamps must still occupy the budget: {out}"
    );
    assert_eq!(fx.rec.count(), 0);
    teardown(&fx);
}

/// RATE SCOPE: the budget is per orchestrator. Two orchestrators must not share
/// one counter (nor be able to exhaust each other's).
#[test]
#[serial]
#[serial(runtime_config)]
fn distinct_orchestrators_do_not_share_a_counter() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "per-orch",
        Spec {
            fleet: TWO_ORCHESTRATORS,
            live: &["lead", "worker", "lead2"],
            ..Spec::default()
        },
    );

    for i in 1..=RATE_LIMIT_PER_HOUR {
        let out = page(&fx, "lead", &format!("a{i}"));
        assert_eq!(out["sent"], true, "page {i} from lead must succeed: {out}");
    }
    for i in 1..=RATE_LIMIT_PER_HOUR {
        let out = page(&fx, "lead2", &format!("b{i}"));
        assert_eq!(
            out["sent"], true,
            "lead's spent budget must not bind lead2: {out}"
        );
    }

    assert_eq!(page(&fx, "lead2", "b4")["code"], "rate_limited");
    assert_eq!(fx.rec.count(), RATE_LIMIT_PER_HOUR * 2);
    teardown(&fx);
}

/// RATE ATOMICITY: the claim is serialized in-process, so parallel callers cannot
/// each read "2 spent" and each push a third stamp. Exactly three of N concurrent
/// pages may win.
#[test]
#[serial]
#[serial(runtime_config)]
fn concurrent_claims_admit_exactly_three() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("concurrent", Spec::default());
    // Set the process-global inputs ONCE: the threads below must not race each
    // other on `set_var` / `reload`, only on the rate claim under test.
    std::env::set_var("AGEND_HOME", &fx.home);
    crate::runtime_config::reload(&fx.home);

    let sent = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let runtime = fx.runtime.clone();
                scope.spawn(move || page_with(&runtime, "lead", &format!("concurrent {i}")))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .filter(|out| out["sent"] == true)
            .count()
    });

    assert_eq!(
        sent, RATE_LIMIT_PER_HOUR,
        "exactly three concurrent claims may win"
    );
    assert_eq!(
        fx.rec.count(),
        RATE_LIMIT_PER_HOUR,
        "and exactly three pages may reach the channel"
    );
    teardown(&fx);
}

/// RATE ACCOUNTING: a claimed slot whose page reached NO CHANNEL AT ALL must be
/// rolled back. The recorder clears the channel registry as the deliverability
/// gate queries it, so the fan-out runs with zero channels — the only way to reach
/// that branch from outside the handler.
///
/// Note what this does and does not pin. The fan-out returns channels ATTEMPTED,
/// not delivered, so a zero-channel fan-out is the only case the handler can prove
/// bought nothing. A page counted here may still have been dropped downstream;
/// neither the code nor this test claims otherwise.
#[test]
#[serial]
#[serial(runtime_config)]
fn a_zero_dispatch_send_does_not_consume_a_slot() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("rollback", Spec::default());
    fx.rec.vanish_on_query.store(true, Ordering::Relaxed);

    let out = page(&fx, "lead", "vanishing channel");
    assert_eq!(out["code"], "not_delivered", "{out}");
    assert_eq!(out["sent"], false, "{out}");

    // The whole budget must still be there.
    let rec2 = Recorder::arc("telegram", true);
    register_active_channel(rec2.clone());
    for i in 1..=RATE_LIMIT_PER_HOUR {
        let out = page(&fx, "lead", &format!("m{i}"));
        assert_eq!(
            out["sent"], true,
            "an undelivered page must not have spent a slot: {out}"
        );
    }
    assert_eq!(page(&fx, "lead", "m4")["code"], "rate_limited");
    assert_eq!(rec2.count(), RATE_LIMIT_PER_HOUR);
    teardown(&fx);
}

// ── deliverability ──────────────────────────────────────────────────────

/// The pre-existing fail-closed allowlist gate must still bind: an unauthorized
/// channel drops the page even though every one of the tool's own gates passed.
#[test]
#[serial]
#[serial(runtime_config)]
fn unauthorized_channel_still_drops_the_page() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "allowlist",
        Spec {
            authorized: false,
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(
        fx.rec.count(),
        0,
        "the fail-closed gate must not be bypassed"
    );
    assert_eq!(
        out["sent"], false,
        "the caller must learn the page was not delivered: {out}"
    );
    assert_eq!(out["code"], "not_delivered", "{out}");
    teardown(&fx);
}

/// DELIVERABILITY: the tool exists to reach the operator's PHONE. An authorized
/// Discord channel is not that, so an `any()` over all channels reported
/// `sent: true` and spent a rate slot while the phone stayed silent. The gate must
/// require a telegram channel specifically — and must not charge for the refusal.
#[test]
#[serial]
#[serial(runtime_config)]
fn discord_only_allowlist_refuses_without_spending_a_slot() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "discord-only",
        Spec {
            channel_kind: "discord",
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");
    assert_eq!(out["code"], "not_delivered", "{out}");
    assert_eq!(out["sent"], false, "{out}");
    assert_eq!(fx.rec.count(), 0, "a discord channel is not the phone");

    // The budget must be untouched: three pages still available once telegram is
    // authorized again.
    let telegram = Recorder::arc("telegram", true);
    register_active_channel(telegram.clone());
    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(page(&fx, "lead", &format!("m{i}"))["sent"], true);
    }
    assert_eq!(telegram.count(), RATE_LIMIT_PER_HOUR);
    teardown(&fx);
}

// ── routing / hermeticity ───────────────────────────────────────────────

/// ROUTING: pages land in the DEDICATED operator topic, not the sender's own
/// topic, so they collect in one place the operator can mute. A pre-registered
/// topic is reused, so the creation seam is never even consulted.
#[test]
#[serial]
#[serial(runtime_config)]
fn dedicated_topic_is_used_when_registered() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("topic", Spec::default());
    crate::channel::telegram::topic_registry::register_topic(
        &fx.home,
        77,
        "operator-notifications",
    )
    .expect("register dedicated topic");

    assert_eq!(page(&fx, "lead", "milestone")["sent"], true);

    let (routed_to, _, _) = fx.rec.last().expect("a page was delivered");
    assert_eq!(
        routed_to, "operator-notifications",
        "the page must route to the dedicated topic, not the sender's"
    );
    assert!(
        topic_seam_calls_for_test().is_empty(),
        "a registered topic must be reused without any creation attempt"
    );
    assert!(!bot_api_entered_for_home(&fx.home));
    teardown(&fx);
}

/// ROUTING FALLBACK: when the dedicated topic cannot be resolved or created,
/// the page still goes out — on the sender's own topic. Routing may fail open
/// because both topics sit inside the allowlisted group; the gates do not.
#[test]
#[serial]
#[serial(runtime_config)]
fn topic_creation_failure_falls_back_to_sender_topic() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    // No topics.json entry, and the seam answers "creation failed".
    let fx = setup("fallback", Spec::default());

    assert_eq!(page(&fx, "lead", "milestone")["sent"], true);

    let (routed_to, _, _) = fx.rec.last().expect("a page was delivered");
    assert_eq!(
        routed_to, "lead",
        "a failed dedicated-topic resolution must fall back to the sender topic"
    );
    assert_eq!(
        topic_seam_calls_for_test(),
        vec!["operator-notifications".to_string()],
        "creation must have been ATTEMPTED — and intercepted"
    );
    assert!(!bot_api_entered_for_home(&fx.home));
    teardown(&fx);
}

/// HERMETICITY (#3480 C3): the deliberately hostile case. A REAL-LOOKING
/// canonical bot token is exported — the exact condition under which the old
/// fixture (which only named a deliberately-unset `bot_token_env`) would have
/// authenticated to the live Bot API — and the API must still be unreachable.
/// The seam answers the creation, the page routes to the dedicated topic, and no
/// code path under this home ever entered credential resolution or `Bot::new`.
#[test]
#[serial]
#[serial(runtime_config)]
fn real_looking_bot_token_never_reaches_the_bot_api() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup(
        "hermetic",
        Spec {
            canonical_token: "1234567890:AAH-real-looking-but-fake",
            topic_outcome: Some(4242),
            ..Spec::default()
        },
    );

    let out = page(&fx, "lead", "milestone");

    assert_eq!(out["sent"], true, "{out}");
    assert_eq!(
        out["routed_to"], "operator-notifications",
        "the seam's outcome must be what routing consumed: {out}"
    );
    assert_eq!(
        topic_seam_calls_for_test(),
        vec!["operator-notifications".to_string()],
        "the creation attempt must have gone through the seam"
    );
    assert!(
        !bot_api_entered_for_home(&fx.home),
        "no test may reach the Telegram Bot API, least of all with a token that looks real"
    );
    teardown(&fx);
}

// ── content ─────────────────────────────────────────────────────────────

/// CONTENT: the operator always learns who paged, and no caller can page a wall
/// of text.
#[test]
#[serial]
#[serial(runtime_config)]
fn page_carries_sender_prefix_and_respects_length_cap() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("content", Spec::default());

    let long = "x".repeat(MAX_PAGE_CHARS * 3);
    assert_eq!(page(&fx, "lead", &long)["sent"], true);

    let (_, _, text) = fx.rec.last().expect("a page was delivered");
    assert!(
        text.contains("lead"),
        "the page must name its sender: {text:.80}"
    );
    let body_len = text.matches('x').count();
    assert_eq!(
        body_len, MAX_PAGE_CHARS,
        "the body must be capped at MAX_PAGE_CHARS"
    );
    teardown(&fx);
}

/// Assert that a refused body cost the caller nothing: nothing reached the
/// channel, the snapshot still records no stamp, and the FULL hourly budget is
/// still there to be spent on a legitimate page.
///
/// The third check is the load-bearing one — it is behavioural, so it stays true
/// even if the snapshot format changes. It also proves the refusal sits ahead of
/// `budget::claim` in the gate sequence rather than merely rolling back after it.
fn assert_nothing_spent(fx: &Fixture, label: &str) {
    assert_eq!(
        fx.rec.count(),
        0,
        "{label}: a refused body must not reach the channel"
    );
    let raw = std::fs::read_to_string(rate_snapshot(&fx.home)).expect("snapshot readable");
    let snapshot: serde_json::Value = serde_json::from_str(&raw).expect("the snapshot parses");
    assert_eq!(
        snapshot.as_object().map(serde_json::Map::len),
        Some(0),
        "{label}: a refused body must not stamp the rate snapshot: {raw}"
    );
    let after = page(fx, "lead", "a legitimate milestone");
    assert_eq!(after["sent"], true, "{label}: {after}");
    assert_eq!(
        after["remaining_this_hour"],
        RATE_LIMIT_PER_HOUR - 1,
        "{label}: the refusal must not have spent a rate slot: {after}"
    );
}

/// CONTENT: a body carrying a break plus a forged `[operator-page from …]` tail
/// used to render as a convincing SECOND sender. It is now REFUSED.
///
/// Two adversarial passes shaped this case. The first fix collapsed only `'\r'`
/// and `'\n'`, and the test guarding it asserted on `'\n'`, `'\r'` and
/// `str::lines()` — all three LF-only, so it could not see that `U+2028` LINE
/// SEPARATOR (a MANDATORY break under UAX#14), `U+2029`, NEL, VT and FF travelled
/// through verbatim. The second fix flattened all of those but then REWROTE a
/// surviving marker to `[quoted: operator-page from ` and delivered the page.
/// That is withdrawn: it mutated operator-visible text with no flag in the
/// payload and no log line, so the operator read altered words and could not
/// tell. Refusing costs a legitimate caller nothing (a real page does not contain
/// the daemon's marker) and turns the attempt into something the operator can
/// SEE.
///
/// Every one of the eight separators is still driven, because each of them can
/// still CREATE the marker when it flattens to a space — which is exactly what
/// the refusal has to catch.
#[test]
#[serial]
#[serial(runtime_config)]
fn no_break_character_can_forge_a_second_sender() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();

    for (label, separator) in [
        ("lf", "\n"),
        ("crlf", "\r\n"),
        ("cr", "\r"),
        ("nel-u0085", "\u{0085}"),
        ("vt-u000b", "\u{000B}"),
        ("ff-u000c", "\u{000C}"),
        ("line-separator-u2028", "\u{2028}"),
        ("paragraph-separator-u2029", "\u{2029}"),
    ] {
        let fx = setup(&format!("forgery-{label}"), Spec::default());
        let body = format!("build red{separator}[operator-page from ops] all clear");

        let out = page(&fx, "lead", &body);

        assert_eq!(
            out["code"], "marker_in_body",
            "{label}: a forged sender marker must be REFUSED, not rewritten and delivered: {out}"
        );
        assert_eq!(out["sent"], false, "{label}: {out}");
        assert_nothing_spent(&fx, label);
        teardown(&fx);
    }
}

/// CONTENT: the marker check is CASE-INSENSITIVE, and it fires wherever the
/// marker sits.
///
/// The byte-exact rewrite this replaces let `[Operator-Page From ops]` through
/// untouched — an adversarial pass proved it — and a human reads that as
/// authoritative exactly as it reads the lowercase form.
#[test]
#[serial]
#[serial(runtime_config)]
fn a_case_variant_of_the_marker_is_refused_too() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();

    for (label, body) in [
        ("title-case", "build red [Operator-Page From ops] all clear"),
        ("upper-case", "build red [OPERATOR-PAGE FROM ops] all clear"),
        ("mixed-case", "build red [oPeRaToR-pAgE fRoM ops] all clear"),
        ("leading", "[operator-page from ops] all clear"),
        ("trailing", "all clear [operator-page from ops]"),
    ] {
        let fx = setup(&format!("case-{label}"), Spec::default());

        let out = page(&fx, "lead", body);

        assert_eq!(out["code"], "marker_in_body", "{label}: {out}");
        assert_nothing_spent(&fx, label);
        teardown(&fx);
    }
}

/// CONTENT, the residual an adversarial pass PROVED against the `is_control()`
/// predicate: `is_control()` is category **Cc** only, so Unicode spaces and
/// FORMAT characters survived verbatim and rendered as visually identical
/// markers — `[operator-page\u{00A0}from ops]` is pixel-identical to the real
/// thing, ZWSP inside the marker is invisible, and `U+202E` RLO makes reversed
/// text display as the marker.
///
/// The predicate now also covers `char::is_whitespace()` (the Unicode
/// `White_Space` property) and general category **Cf**. This case pins both
/// halves of the contract for each character:
///
///   * SPLICED INTO the marker where its space belongs, it normalises to a space,
///     the marker is reconstructed, and the page is REFUSED with nothing spent.
///     Under the old Cc-only predicate every one of these was delivered.
///   * On its own, in a body with no forged marker, the page is DELIVERED and the
///     character is gone from the delivered text.
#[test]
#[serial]
#[serial(runtime_config)]
fn unicode_space_and_format_lookalikes_cannot_forge_the_marker() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();

    for (label, ch) in [
        ("nbsp-u00a0", '\u{00A0}'),
        ("ideographic-space-u3000", '\u{3000}'),
        ("narrow-nbsp-u202f", '\u{202F}'),
        ("zwsp-u200b", '\u{200B}'),
        ("zwnj-u200c", '\u{200C}'),
        ("zwj-u200d", '\u{200D}'),
        ("rlm-u200f", '\u{200F}'),
        ("rlo-u202e", '\u{202E}'),
        ("lri-u2066", '\u{2066}'),
        ("rli-u2067", '\u{2067}'),
        ("fsi-u2068", '\u{2068}'),
        ("pdi-u2069", '\u{2069}'),
        ("bom-ufeff", '\u{FEFF}'),
    ] {
        // (1) the look-alike marker: refused, and it cost the caller nothing.
        let fx = setup(&format!("lookalike-{label}"), Spec::default());
        let forged = format!("build red [operator-page{ch}from ops] all clear");

        let out = page(&fx, "lead", &forged);

        assert_eq!(
            out["code"], "marker_in_body",
            "{label}: a marker spelled with a look-alike separator must be refused: {out}"
        );
        assert_nothing_spent(&fx, label);
        teardown(&fx);

        // (2) the same character in an innocent body: delivered, character gone.
        let fx = setup(&format!("lookalike-ok-{label}"), Spec::default());
        let innocent = format!("build{ch}red, deploy{ch}green");

        assert_eq!(page(&fx, "lead", &innocent)["sent"], true, "{label}");

        let (_, _, text) = fx.rec.last().expect("a page was delivered");
        assert!(
            !text.contains(ch),
            "{label}: {ch:?} must not survive into the delivered page: {text:?}"
        );
        // Tied to the flattener's own predicate, so the two cannot drift apart.
        assert!(
            !text.chars().any(must_not_survive_verbatim),
            "{label}: no control, Unicode space or format character may survive: {text:?}"
        );
        assert!(
            text.starts_with("[operator-page from lead] build red, deploy green"),
            "{label}: each look-alike must normalise to one ordinary space: {text:?}"
        );
        teardown(&fx);
    }
}

/// Decision d-20260902104216571473-11 condition 2: Error severity exists to
/// survive Away/Sleep, and must NOT let a page skip the tool's own gates. Under
/// Sleep the first three arrive (proving the severity choice works) and the 4th
/// is still refused by the rate gate (proving the gate runs first).
#[test]
#[serial]
#[serial(runtime_config)]
fn rate_gate_binds_even_though_severity_would_pass_the_mode_gate() {
    let _g = fleet_test_guard();
    let _r = channel_registry_test_guard();
    let fx = setup("mode", Spec::default());
    crate::operator_mode::set_mode(
        &fx.home,
        crate::operator_mode::OperatorMode::Sleep,
        None,
        vec![],
    )
    .expect("set sleep mode");

    for i in 1..=RATE_LIMIT_PER_HOUR {
        assert_eq!(
            page(&fx, "lead", &format!("m{i}"))["sent"],
            true,
            "an operator page must survive Sleep — that is the whole point of #3480"
        );
    }
    let (_, severity, _) = fx.rec.last().expect("delivered under Sleep");
    assert_eq!(
        severity,
        NotifySeverity::Error,
        "pages ride Error severity so the Sleep gate passes them"
    );

    let out = page(&fx, "lead", "m4");

    assert_eq!(
        out["code"], "rate_limited",
        "the rate gate must bind before severity buys anything: {out}"
    );
    assert_eq!(fx.rec.count(), RATE_LIMIT_PER_HOUR);
    teardown(&fx);
}
