//! #3417: the live config map must describe every agent the daemon actually
//! runs, not only the ones present at boot.
//!
//! `ctx.configs` is written in exactly one production place —
//! `spawn_and_register_agent`, the BOOT path. Every post-boot surface (direct
//! API SPAWN, MCP SPAWN, deployment spawn, team spawn, restart replacement)
//! registers an agent without ever recording its resolved configuration, so
//! two things go wrong for those instances:
//!
//! * `snapshot.json` reports `args: []` and `working_dir: null` — the snapshot
//!   writer's `cfgs.get(name)` misses, and `unwrap_or_default()` turns "unknown"
//!   into a plausible-looking empty list rather than an error;
//! * crash respawn is not merely degraded but ABSENT: `crash_respawn.rs` reads
//!   `ctx.configs.lock().get(name)` and, finding nothing, logs "no config for
//!   respawn (likely deleted)" and discards the recovery.
//!
//! These tests drive the REAL production entry points and the REAL
//! `SnapshotRotationHandler`; none of them constructs an `AgentSnapshot`, which
//! is what let the existing serde round-trip test in `src/snapshot.rs` look like
//! coverage while the writer's lookup was the step that failed.

use crate::daemon::per_tick::{PerTickHandler, TickContext};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

struct Fixture {
    home: PathBuf,
    registry: crate::agent::AgentRegistry,
    configs: crate::api::ConfigRegistry,
    externals: crate::agent::ExternalRegistry,
}

fn fixture(tag: &str) -> Fixture {
    let home = std::env::temp_dir().join(format!(
        "agend-3417-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).expect("home");
    Fixture {
        home,
        registry: Arc::new(Mutex::new(HashMap::new())),
        configs: Arc::new(Mutex::new(HashMap::new())),
        externals: Arc::new(Mutex::new(HashMap::new())),
    }
}

impl Fixture {
    /// Seed `fleet.yaml` so a managed spawn resolves to a stable id, exactly as
    /// the API handler tests do.
    fn seed_instance(&self, name: &str) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(&self.home),
            format!(
                "instances:\n  {name}:\n    id: {}\n",
                crate::types::InstanceId::new().full()
            ),
        )
        .expect("seed fleet.yaml");
    }

    /// Seed an instance whose desired command and argv are resolvable, which is
    /// what a restart needs to build its replacement.
    fn seed_runnable_instance(&self, name: &str, command: &str, args: &[&str]) {
        let rendered = args
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");
        // An explicit working_directory under THIS home is required: unset, fleet
        // resolution defaults to the real `home_dir()/workspace/<name>`, which the
        // SPAWN validator then rejects as outside the allowed roots for this home.
        let work_dir = self.home.join("workspace").join(name);
        std::fs::create_dir_all(&work_dir).expect("workspace dir");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&self.home),
            format!(
                "instances:\n  {name}:\n    id: {}\n    command: {command}\n    args: [{rendered}]\n    working_directory: {}\n",
                crate::types::InstanceId::new().full(),
                work_dir.display()
            ),
        )
        .expect("seed fleet.yaml");
    }

    /// Run the REAL per-tick snapshot writer and read back what it persisted.
    fn snapshot_args(&self, name: &str) -> Option<Vec<String>> {
        let handler = crate::daemon::per_tick::snapshot::SnapshotRotationHandler::new();
        handler.run(&TickContext {
            home: &self.home,
            registry: &self.registry,
            externals: &self.externals,
            configs: &self.configs,
        });
        crate::snapshot::load(&self.home)?
            .agents
            .into_iter()
            .find(|a| a.name == name)
            .map(|a| a.args)
    }

    /// What `crash_respawn` asks for before it will respawn anything.
    fn respawn_config(&self, name: &str) -> Option<crate::daemon::AgentConfig> {
        self.configs.lock().get(name).cloned()
    }

    fn kill_all(&self) {
        let mut reg = crate::agent::lock_registry(&self.registry);
        for handle in reg.values() {
            let _ = handle.child.lock().kill();
        }
        reg.clear();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_all();
        std::fs::remove_dir_all(&self.home).ok();
    }
}

fn api_ctx<'a>(fx: &'a Fixture) -> crate::api::handlers::HandlerCtx<'a> {
    crate::api::handlers::HandlerCtx {
        registry: &fx.registry,
        configs: &fx.configs,
        externals: &fx.externals,
        notifier: None,
        home: &fx.home,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    }
}

fn mcp_runtime(fx: &Fixture) -> crate::mcp::handlers::dispatch::RuntimeContext {
    crate::mcp::handlers::dispatch::RuntimeContext {
        registry: Arc::clone(&fx.registry),
        configs: Arc::clone(&fx.configs),
        externals: Arc::clone(&fx.externals),
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: None,
        notifier: None,
        shutdown: None,
    }
}

/// Direct API SPAWN: the resolved argv must reach the snapshot.
#[test]
fn api_spawn_records_resolved_args_for_the_snapshot() {
    let fx = fixture("api-spawn");
    let name = "cfg-api";
    fx.seed_instance(name);

    let result = crate::api::handlers::instance::handle_spawn(
        &serde_json::json!({
            "name": name,
            "backend": crate::default_shell(),
            "args": "--login",
        }),
        &api_ctx(&fx),
    );
    assert_eq!(
        result["ok"],
        serde_json::json!(true),
        "spawn must succeed: {result}"
    );

    assert_eq!(
        fx.snapshot_args(name).as_deref(),
        Some(["--login".to_string()].as_slice()),
        "the snapshot must carry the argv the daemon actually spawned, not an empty list that \
         reads as valid data"
    );
}

/// The same gap, in the form that changes behaviour rather than reporting: an
/// agent with no config entry is not crash-respawn eligible at all.
#[test]
fn a_runtime_spawned_agent_is_crash_respawn_eligible() {
    let fx = fixture("respawn-eligible");
    let name = "cfg-respawn";
    fx.seed_instance(name);

    let result = crate::api::handlers::instance::handle_spawn(
        &serde_json::json!({
            "name": name,
            "backend": crate::default_shell(),
            "args": "--login",
        }),
        &api_ctx(&fx),
    );
    assert_eq!(result["ok"], serde_json::json!(true), "spawn: {result}");

    let config = fx.respawn_config(name).expect(
        "crash_respawn looks the crashed agent up in this map and discards the recovery when it \
         is absent — a runtime-spawned agent would never be respawned",
    );
    assert_eq!(config.args, vec!["--login".to_string()]);
    assert_eq!(config.name, name);
}

/// MCP `create_instance` — the same convergence, through the tool surface.
#[test]
fn mcp_create_instance_records_resolved_args_for_the_snapshot() {
    let fx = fixture("mcp-create");
    let name = "cfg-mcp";
    fx.seed_instance(name);
    let runtime = mcp_runtime(&fx);
    let args = serde_json::json!({
        "name": name,
        "backend": crate::default_shell(),
        "args": "--login",
    });
    let result = crate::mcp::handlers::dispatch::dispatch_create_instance(
        &crate::mcp::handlers::dispatch::HandlerCtx {
            home: &fx.home,
            args: &args,
            instance_name: "operator",
            sender: &None,
            runtime: Some(&runtime),
        },
    );
    assert!(
        result.get("error").is_none(),
        "create_instance must succeed: {result}"
    );
    let spawned_name = result["name"]
        .as_str()
        .expect("create_instance returns the effective name")
        .to_string();

    assert_eq!(
        fx.snapshot_args(&spawned_name).as_deref(),
        Some(["--login".to_string()].as_slice()),
        "an MCP-created instance must be described by the live config map too"
    );
}

/// Restart replaces the process; the replacement's resolved argv must be what
/// the map describes afterwards.
#[test]
fn restart_replacement_records_resolved_args_for_the_snapshot() {
    let fx = fixture("restart");
    let name = "cfg-restart";
    fx.seed_runnable_instance(name, crate::default_shell(), &["--login"]);
    let spawned = crate::api::handlers::instance::handle_spawn(
        &serde_json::json!({
            "name": name,
            "backend": crate::default_shell(),
            "args": "--login",
        }),
        &api_ctx(&fx),
    );
    assert_eq!(spawned["ok"], serde_json::json!(true), "spawn: {spawned}");

    let runtime = mcp_runtime(&fx);
    let args = serde_json::json!({"instance": name, "mode": "fresh", "reason": "test"});
    let result = crate::mcp::handlers::dispatch::dispatch_restart_instance(
        &crate::mcp::handlers::dispatch::HandlerCtx {
            home: &fx.home,
            args: &args,
            instance_name: "operator",
            sender: &None,
            runtime: Some(&runtime),
        },
    );
    assert!(
        result.get("error").is_none(),
        "restart must succeed: {result}"
    );

    assert_eq!(
        fx.snapshot_args(name).as_deref(),
        Some(["--login".to_string()].as_slice()),
        "the replacement process must leave the map describing what it actually runs"
    );
}

/// Team creation spawns its members through `spawn_one` directly — the one
/// surface with no config plumbing at all.
#[test]
fn team_spawn_records_resolved_args_for_the_snapshot() {
    let fx = fixture("team");
    let member = "cfg-team-1";
    std::fs::write(
        crate::fleet::fleet_yaml_path(&fx.home),
        format!(
            "instances:\n  {member}:\n    id: {}\n    args: [\"--login\"]\n",
            crate::types::InstanceId::new().full()
        ),
    )
    .expect("seed fleet.yaml");

    let result = crate::team_ops::create(
        &fx.home,
        crate::team_ops::CreateTeamRequest {
            name: "cfg-team".to_string(),
            per_member_backends: vec![crate::default_shell().to_string()],
            existing_members: vec![],
            topic_binding_mode: None,
            orchestrator: None,
            description: None,
            repository_path: None,
            project_id: None,
            accept_from: vec![],
        },
        &fx.registry,
        &fx.configs,
        None,
    );
    assert_eq!(
        result["ok"],
        serde_json::json!(true),
        "team create must succeed: {result}"
    );

    let spawned = {
        let reg = crate::agent::lock_registry(&fx.registry);
        reg.values().map(|h| h.name.to_string()).collect::<Vec<_>>()
    };
    assert!(!spawned.is_empty(), "team create must spawn a member");
    for name in spawned {
        assert!(
            fx.respawn_config(&name).is_some(),
            "team member {name} must be described by the live config map, or it can never be \
             crash-respawned"
        );
    }
}

/// Deployment spawn — the fourth public create path. Asserted on whatever the
/// deployment actually named its instances, so the test cannot pass by guessing
/// a name that was never spawned.
#[test]
fn deployment_spawn_records_resolved_args_for_the_snapshot() {
    let fx = fixture("deploy");
    let workspace = fx.home.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&fx.home),
        format!(
            "templates:\n  tmpl:\n    directory: {}\n    instances:\n      worker:\n        command: {}\n        args: [\"--login\"]\n",
            workspace.display(),
            crate::default_shell()
        ),
    )
    .expect("seed fleet.yaml");

    let runtime = crate::deployments::DeploymentRuntime {
        registry: &fx.registry,
        configs: &fx.configs,
        externals: &fx.externals,
        notifier: None,
    };
    let result = crate::deployments::deploy_with_runtime(
        &fx.home,
        "operator",
        &serde_json::json!({"template": "tmpl", "name": "dep"}),
        Some(&runtime),
    );
    assert!(
        result.get("error").is_none(),
        "deploy must succeed: {result}"
    );

    let spawned = {
        let reg = crate::agent::lock_registry(&fx.registry);
        reg.values().map(|h| h.name.to_string()).collect::<Vec<_>>()
    };
    assert!(
        !spawned.is_empty(),
        "deploy must spawn at least one instance"
    );
    for name in spawned {
        assert_eq!(
            fx.snapshot_args(&name).as_deref(),
            Some(["--login".to_string()].as_slice()),
            "a deployed instance must be described by the live config map too"
        );
    }
}

fn config_for(name: &str, args: &[&str]) -> crate::daemon::AgentConfig {
    crate::daemon::AgentConfig {
        name: name.to_string(),
        backend: None,
        backend_command: crate::default_shell().to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        env: None,
        working_dir: None,
        submit_key: "\r".to_string(),
    }
}

/// A spawn that fails must leave the map as it found it — including the case
/// where it OVERWROTE an existing entry. Blindly removing would strip a restart
/// of the config its predecessor was still described by.
#[test]
fn a_failed_spawn_restores_the_previous_config() {
    let fx = fixture("rollback-restore");
    let name = "cfg-rollback";
    fx.configs
        .lock()
        .insert(name.to_string(), config_for(name, &["--previous"]));

    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--attempted"]),
        || Err(anyhow::anyhow!("spawn refused")),
    );

    assert!(result.is_err(), "the failure must reach the caller");
    assert_eq!(
        fx.respawn_config(name).map(|c| c.args),
        Some(vec!["--previous".to_string()]),
        "a failed spawn must restore what it overwrote, not delete it"
    );
}

/// The other half: an entry this transaction INTRODUCED must not survive its
/// own failure, or the map would describe an agent that does not exist.
#[test]
fn a_failed_spawn_removes_a_config_it_introduced() {
    let fx = fixture("rollback-remove");
    let name = "cfg-new";

    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--attempted"]),
        || Err(anyhow::anyhow!("spawn refused")),
    );

    assert!(result.is_err());
    assert!(
        fx.respawn_config(name).is_none(),
        "a config introduced by a failed spawn must not outlive it"
    );
}

/// The race the transaction must not lose, pinned at its root.
///
/// Nothing else serializes same-name spawns: `spawn_instance`'s duplicate check
/// reads the registry and the registration happens later. Two weaker designs
/// were tried and rejected before this one — rolling back on a value comparison
/// cannot tell two identical configs apart, and consulting the registry after a
/// failure is still check-then-act, since the winner can register between the
/// check and the restore. So the property pinned here is the lane: while one
/// spawn transaction for a name is in flight, another for the SAME name cannot
/// be inside it.
///
/// The ordering is observed, not timed. B announces itself before entering and
/// again from inside; A waits — bounded — for B's INSIDE signal and only stops
/// waiting when it does not come. A design without the lane hands A that signal
/// immediately and the recorded order shows B inside while A still is; with the
/// lane, B's entry can only be recorded after A has left.
#[test]
fn one_spawn_transaction_per_name_at_a_time() {
    let fx = fixture("lane");
    let name = "cfg-lane";
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let (b_inside_tx, b_inside_rx) = crossbeam_channel::bounded::<()>(1);
    let (a_inside_tx, a_inside_rx) = crossbeam_channel::bounded::<()>(1);

    let (b_attempt_tx, b_attempt_rx) = crossbeam_channel::bounded::<()>(1);
    let b_events = Arc::clone(&events);
    let b_home = fx.home.clone();
    let b_configs = Arc::clone(&fx.configs);
    let b = std::thread::spawn(move || {
        // Only try once A is demonstrably inside its own transaction.
        a_inside_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("A must enter first");
        // The attempt latch: B has committed to entering. A waits on THIS with a
        // blocking recv — no timeout, no sleep — so the interleave is established
        // deterministically rather than by elapsed time.
        let _ = b_attempt_tx.send(());
        let _ = crate::agent_ops::spawn_one_recording_config(
            &b_home,
            &b_configs,
            name,
            config_for(name, &["--b"]),
            || {
                b_events.lock().push("B:inside");
                let _ = b_inside_tx.send(());
                Ok(crate::backend::SpawnMode::Fresh)
            },
        );
    });

    let a_events = Arc::clone(&events);
    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--a"]),
        || {
            a_events.lock().push("A:inside");
            let _ = a_inside_tx.send(());
            // Deterministic: B has committed to entering before this returns.
            b_attempt_rx
                .recv()
                .expect("B must announce its attempt before A can conclude anything");
            // Only the leak observation is bounded, and only as a fail-safe: with
            // the lane this recv is SUPPOSED to expire, and without it B — already
            // running and needing only an uncontended lock — answers at once.
            let leaked = b_inside_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            a_events.lock().push(if leaked {
                "A:saw-B-inside"
            } else {
                "A:exclusive"
            });
            Ok(crate::backend::SpawnMode::Fresh)
        },
    );
    events.lock().push("A:left");
    assert!(result.is_ok());
    let _ = b.join();

    let observed = events.lock().clone();
    assert!(
        !observed.contains(&"A:saw-B-inside"),
        "a second spawn transaction for the same name entered while the first was still inside: \
         {observed:?}"
    );
    let a_left = observed
        .iter()
        .position(|e| *e == "A:left")
        .expect("A recorded leaving");
    let b_inside = observed
        .iter()
        .position(|e| *e == "B:inside")
        .expect("B eventually enters — the lane serializes, it does not exclude");
    assert!(
        b_inside > a_left,
        "B must only be inside after A has left: {observed:?}"
    );
}

/// A child that starts and then exits immediately is not a failed spawn: it
/// registered, so it must keep its config and stay crash-respawn eligible.
#[test]
fn a_successful_spawn_that_exits_immediately_keeps_its_config() {
    let fx = fixture("fast-exit");
    let name = "cfg-fast-exit";

    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--login"]),
        || Ok(crate::backend::SpawnMode::Fresh),
    );

    assert!(result.is_ok());
    assert_eq!(
        fx.respawn_config(name).map(|c| c.args),
        Some(vec!["--login".to_string()]),
        "a fast-exiting child must not be mistaken for a failed spawn: crash respawn is exactly \
         the path that needs its config"
    );
}

/// Lock discipline, asserted from inside the spawn itself: neither lock may be
/// held across it. `try_lock` from this same thread would fail if the
/// transaction were still holding one, and no disk I/O may run under either.
#[test]
fn the_config_transaction_holds_no_lock_across_the_spawn() {
    let fx = fixture("lock-order");
    let name = "cfg-locks";

    let observed = std::cell::RefCell::new((false, false));
    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--login"]),
        || {
            let configs_free = fx.configs.try_lock().is_some();
            let registry_free = fx.registry.try_lock().is_some();
            *observed.borrow_mut() = (configs_free, registry_free);
            Ok(crate::backend::SpawnMode::Fresh)
        },
    );

    assert!(result.is_ok());
    let (configs_free, registry_free) = *observed.borrow();
    assert!(
        configs_free,
        "the configs lock must be released before the spawn, or the spawn's own registry work \
         inverts the documented registry-then-configs order"
    );
    assert!(
        registry_free,
        "the registry lock must not be held across the spawn either"
    );
}

/// #3417 correction: DELETE and clean-exit are the removal authority, so a
/// failed spawn must never bring back what they retired.
///
/// The transaction captures `previous` before spawning. If a delete removes the
/// entry while the spawn is in flight, restoring that capture resurrects a
/// config the removal authority just retired — and the agent it describes is
/// gone, so crash respawn would be handed a config for a deleted instance.
///
/// The ordering point is the configs lock itself: the check and the write happen
/// in ONE critical section, so this is not a post-failure check-then-act. The
/// lane makes presence a sound ownership token — no other spawn can have written
/// this key — so an entry that is GONE can only have been removed by the
/// deletion authority, and the rollback stands down.
#[test]
fn a_delete_during_a_failed_spawn_wins_over_the_rollback() {
    let fx = fixture("delete-wins");
    let name = "cfg-deleted";
    fx.configs
        .lock()
        .insert(name.to_string(), config_for(name, &["--previous"]));

    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--attempted"]),
        || {
            // The removal authority runs while the spawn is in flight, exactly as
            // delete_transaction step 6 and handle_clean_exit do.
            fx.configs.lock().remove(name);
            Err(anyhow::anyhow!("spawn failed after the delete"))
        },
    );

    assert!(result.is_err());
    assert!(
        fx.respawn_config(name).is_none(),
        "the rollback resurrected a config that DELETE had already retired: {:?}",
        fx.respawn_config(name).map(|c| c.args)
    );
}

/// The config must be readable by the time anything can observe the child —
/// crash respawn reads this map and can be reached before the spawn call
/// returns. Asserting it AFTER the transaction cannot tell "inserted before" from
/// "inserted after"; this looks from INSIDE the spawn.
#[test]
fn the_config_is_visible_from_inside_the_spawn() {
    let fx = fixture("inside-visibility");
    let name = "cfg-visible";

    let seen = std::sync::Mutex::new(None);
    let result = crate::agent_ops::spawn_one_recording_config(
        &fx.home,
        &fx.configs,
        name,
        config_for(name, &["--login"]),
        || {
            *seen.lock().expect("poisoned") = fx.configs.lock().get(name).map(|c| c.args.clone());
            Ok(crate::backend::SpawnMode::Fresh)
        },
    );

    assert!(result.is_ok());
    assert_eq!(
        seen.into_inner().expect("poisoned"),
        Some(vec!["--login".to_string()]),
        "a crash arriving while the spawn is still running would find no config"
    );
}

/// A panicking spawn must leave the map as a failed spawn does. Without this the
/// invariant holds only on the paths that return.
#[test]
fn a_panicking_spawn_rolls_back_like_a_failed_one() {
    let fx = fixture("panic-rollback");
    let name = "cfg-panic";
    fx.configs
        .lock()
        .insert(name.to_string(), config_for(name, &["--previous"]));

    let home = fx.home.clone();
    let configs = Arc::clone(&fx.configs);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = crate::agent_ops::spawn_one_recording_config(
            &home,
            &configs,
            name,
            config_for(name, &["--attempted"]),
            || panic!("spawn panicked"),
        );
    }));

    assert!(outcome.is_err(), "the panic must propagate");
    assert_eq!(
        fx.respawn_config(name).map(|c| c.args),
        Some(vec!["--previous".to_string()]),
        "a panicking spawn must restore the previous config, not leave its own attempt behind"
    );
}

/// The lane is keyed by the home PATH, so two spellings of the same home must not
/// hand out two different lanes — that would serialize nothing while looking like
/// it did.
///
/// The spellings have to be ones that genuinely differ as raw keys. A trailing
/// separator and a `.` component do NOT: `Path` compares by component and
/// normalizes those away, so a test built on them passes with or without
/// canonicalization and proves nothing. These two do differ: a `..` round-trip
/// through a real subdirectory, and a symlink to the home.
#[test]
#[cfg(unix)]
fn equivalent_home_spellings_share_one_lane() {
    let fx = fixture("lane-identity");
    let name = "cfg-spelling";

    let sub = fx.home.join("sub");
    std::fs::create_dir_all(&sub).expect("subdir");
    let via_parent = sub.join("..");
    assert_ne!(
        via_parent, fx.home,
        "the fixture must present a spelling that differs as a raw key"
    );
    assert!(
        crate::agent_ops::lanes_are_the_same(&fx.home, &via_parent, name),
        "a `..` round-trip must not fork the lane"
    );

    let link = fx.home.with_extension("link");
    std::os::unix::fs::symlink(&fx.home, &link).expect("symlink");
    assert_ne!(link, fx.home);
    let shared = crate::agent_ops::lanes_are_the_same(&fx.home, &link, name);
    std::fs::remove_file(&link).ok();
    assert!(shared, "a symlinked home must not fork the lane");
}
