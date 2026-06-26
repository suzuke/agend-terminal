use super::*;
use std::fs;

/// Shared mutex for tests that mutate process-global env vars.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
    G.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_fleet(dir: &Path, yaml: &str) -> PathBuf {
    fs::create_dir_all(dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(&path, yaml).expect("write fleet.yaml");
    path
}

// #1989: schema_version regression pins — omitted field must stay
// version-1 (every pre-#1989 fleet.yaml), explicit + newer-than-supported
// values must parse (warn-not-refuse), and the derived Default must agree
// with the serde default (both None -> effective 1).
#[test]
fn schema_version_omitted_resolves_to_one() {
    let config: FleetConfig = serde_yaml_ng::from_str("instances: {}").expect("parse");
    assert_eq!(config.schema_version, None);
    assert_eq!(config.effective_schema_version(), 1);
}

#[test]
fn schema_version_explicit_parses() {
    let config: FleetConfig = serde_yaml_ng::from_str(
        "schema_version: 1
instances: {}",
    )
    .expect("parse");
    assert_eq!(config.effective_schema_version(), 1);
}

#[test]
fn schema_version_newer_than_supported_still_loads() {
    // Forward-compat contract: a v2 file on a v1 daemon WARNs but loads —
    // refusing would brick the whole daemon on a hand-edit typo.
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-schema-ver-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        "schema_version: 99\ndefaults:\n  backend: claude\ninstances: {}\n",
    );
    let config = FleetConfig::load(&path).expect("v99 file must still load");
    assert_eq!(config.effective_schema_version(), 99);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn schema_version_default_derive_matches_serde_default() {
    // Two-defaults-disagree trap: derived Default must resolve to the
    // same effective version as an omitted field in YAML.
    assert_eq!(FleetConfig::default().effective_schema_version(), 1);
    assert_eq!(FLEET_SCHEMA_VERSION, 1);
}

#[test]
fn schema_version_none_round_trips_without_emitting_field() {
    // skip_serializing_if: re-serializing a pre-#1989 config must not
    // inject `schema_version:` into the user's file.
    let config: FleetConfig = serde_yaml_ng::from_str("instances: {}").expect("parse");
    let out = serde_yaml_ng::to_string(&config).expect("serialize");
    assert!(
        !out.contains("schema_version"),
        "None must not serialize: {out}"
    );
}

#[test]
fn idle_expectation_parses_and_defaults_to_active() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-idle-exp-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  backend: claude
instances:
  worker:
    role: worker
  general:
    role: General assistant
    idle_expectation: on-demand
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    // #1563: omitted field → Active (zero-migration backward compat).
    assert_eq!(
        config.instances.get("worker").unwrap().idle_expectation,
        IdleExpectation::Active
    );
    // Explicit kebab-case `on-demand` → OnDemand.
    assert_eq!(
        config.instances.get("general").unwrap().idle_expectation,
        IdleExpectation::OnDemand
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_preset_args_not_applied_to_different_command() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-test-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  backend: claude
instances:
  test:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("test").expect("resolve");

    assert_eq!(resolved.backend_command, "/bin/bash");
    // Preset args (--dangerously-skip-permissions) should NOT be applied
    assert!(
        resolved.args.is_empty(),
        "args should be empty for non-preset command, got: {:?}",
        resolved.args
    );
    // Submit key should be default \r, not preset's
    assert_eq!(resolved.submit_key, "\r");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn instance_is_known_true_for_fleet_entry_false_for_ghost() {
    // #1488: instance_is_known gates the cron fail-safe + boot sweep. An
    // entry without an id still counts as known (offline-but-configured);
    // a name absent from fleet.yaml is a deletable ghost.
    let dir = std::env::temp_dir().join(format!("agend-1488-known-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    fs::write(
        fleet_yaml_path(&dir),
        "instances:\n  alive:\n    backend: claude\n",
    )
    .unwrap();
    assert!(instance_is_known(&dir, "alive"), "fleet entry is known");
    assert!(!instance_is_known(&dir, "ghost"), "absent name is a ghost");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_resolved_args_exclude_preset() {
    // resolve_instance returns user-only args; preset args are injected
    // by agent::spawn_agent based on SpawnMode.
    let dir = std::env::temp_dir().join(format!("agend-fleet-test2-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  backend: claude
instances:
  test:
    command: claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("test").expect("resolve");

    assert_eq!(resolved.backend_command, "claude");
    assert!(
        resolved.args.is_empty(),
        "preset args must not appear in resolved.args, got: {:?}",
        resolved.args
    );

    fs::remove_dir_all(&dir).ok();
}

// ── #2344: typed role_kind drives the per-role MCP tool subset ──

fn tool_count(defs: &serde_json::Value) -> usize {
    defs.get("tools")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// #2344: a typed `role_kind: reviewer` parses to the enum AND drives the MCP
/// tool subset (narrower than full) — the per-role subsetting that was inert.
/// The prose `role` description coexists unchanged.
#[test]
fn role_kind_reviewer_parses_and_subsets_tools() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-rk-rev-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  rev:
    role: "Senior code reviewer"
    role_kind: reviewer
    command: claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let inst = config.instances.get("rev").expect("instance");
    assert_eq!(inst.role_kind, Some(RoleKind::Reviewer));
    assert_eq!(
        inst.role.as_deref(),
        Some("Senior code reviewer"),
        "prose role description coexists with the typed role_kind"
    );

    let full = tool_count(&crate::mcp::tools::tool_definitions());
    let subset = tool_count(&crate::mcp::tools::tool_definitions_for_role(
        inst.role_kind,
    ));
    assert!(
        subset < full,
        "role_kind=reviewer must narrow the tool surface ({subset} < {full})"
    );
    fs::remove_dir_all(&dir).ok();
}

/// #2344 REGRESSION PIN: a free-text `role` description with NO `role_kind`
/// stays ALL-OPEN (opt-in) — the original bug class. Prose like "Code reviewer"
/// never matched a subset key, so it must surface the full 36 tools; only an
/// operator-declared typed `role_kind` narrows.
#[test]
fn prose_role_without_role_kind_stays_all_open() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-rk-prose-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  rev:
    role: "Code reviewer"
    command: claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let inst = config.instances.get("rev").expect("instance");
    assert_eq!(
        inst.role_kind, None,
        "no role_kind declared → None (opt-in)"
    );

    let full = tool_count(&crate::mcp::tools::tool_definitions());
    let surface = tool_count(&crate::mcp::tools::tool_definitions_for_role(
        inst.role_kind,
    ));
    assert_eq!(
        surface, full,
        "prose role without role_kind must stay all-open (the #2344 bug: prose never subsets)"
    );
    fs::remove_dir_all(&dir).ok();
}

/// #2344 D2 STRICT: a `role_kind:` present with a value outside the seven
/// variants FAILS fleet.yaml load (returns Err, does NOT panic), naming the bad
/// value + the valid options. A field-ABSENT role_kind is legal (covered
/// above), so existing fleets (no role_kind) are never wrongly blocked.
#[test]
fn unknown_role_kind_value_fails_load_strict() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-rk-bad-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  badagent:
    role_kind: supervisor
    command: claude
"#,
    );
    let err = FleetConfig::load(&path).expect_err("unknown role_kind must fail load (D2 strict)");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("supervisor"),
        "error must name the bad value, got: {msg}"
    );
    assert!(
        msg.contains("reviewer"),
        "error must list the valid role_kind options, got: {msg}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_env_merge_order() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-test3-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  env:
    KEY1: default_val
    KEY2: default_val
instances:
  test:
    command: /bin/bash
    env:
      KEY2: instance_val
      KEY3: instance_only
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("test").expect("resolve");

    assert_eq!(
        resolved.env.get("KEY1").map(|s| s.as_str()),
        Some("default_val")
    );
    assert_eq!(
        resolved.env.get("KEY2").map(|s| s.as_str()),
        Some("instance_val")
    ); // instance overrides
    assert_eq!(
        resolved.env.get("KEY3").map(|s| s.as_str()),
        Some("instance_only")
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channel_config_telegram_parsing() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-chan-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channel:
  type: telegram
  bot_token_env: MY_BOT_TOKEN
  group_id: -100123456
  mode: topic
instances:
  test:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    match config.channel {
        Some(ChannelConfig::Telegram {
            ref bot_token_env,
            group_id,
            ref mode,
            ..
        }) => {
            assert_eq!(bot_token_env, "MY_BOT_TOKEN");
            assert_eq!(group_id, -100123456);
            assert_eq!(mode, "topic");
        }
        None => panic!("channel should be Some"),
        Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
    }

    fs::remove_dir_all(&dir).ok();
}

// #2045: the `{ id, name }` allowlist form parses through the REAL fleet load
// entry (FleetConfig::load), alongside a legacy bare id — the happy path the
// deny test below guards.
#[test]
fn user_allowlist_named_entry_loads_via_real_fleet() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-allow-ok-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channel:
  type: telegram
  bot_token_env: MY_BOT_TOKEN
  group_id: -100
  user_allowlist:
    - 12345
    - { id: 67890, name: "Alice" }
instances: {}
"#,
    );
    let config = FleetConfig::load(&path).expect("named + bare allowlist must load");
    match config.channel {
        Some(ChannelConfig::Telegram {
            ref user_allowlist, ..
        }) => {
            let list = user_allowlist.as_ref().expect("allowlist present");
            assert_eq!(list[0].id(), 12345);
            assert_eq!(list[0].name(), None);
            assert_eq!(list[1].id(), 67890);
            assert_eq!(list[1].name(), Some("Alice"));
        }
        other => panic!("expected telegram channel, got {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}

// #2045 review add 2 (the authz-surface invariant): a MALFORMED allowlist
// entry must FAIL-CLOSED at the real fleet load entry — `FleetConfig::load`
// returns `Err`, so the daemon refuses to boot with a half-parsed allowlist
// rather than silently dropping the bad entry and accepting whatever remains.
// The `#[serde(untagged)]` enum has no fallthrough: an entry that is neither a
// bare integer nor a complete `{ id: i64, name: String }` matches no variant.
#[test]
fn user_allowlist_malformed_entry_fails_closed_via_real_fleet() {
    // A `{ id, name }` map whose id is a quoted string — matches neither
    // `Id(i64)` (it's a map) nor `Named { id: i64, .. }` (id is not i64).
    let dir = std::env::temp_dir().join(format!("agend-fleet-allow-bad-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channel:
  type: telegram
  bot_token_env: MY_BOT_TOKEN
  group_id: -100
  user_allowlist:
    - 12345
    - { id: "not_a_number", name: "Mallory" }
instances: {}
"#,
    );
    assert!(
        FleetConfig::load(&path).is_err(),
        "a malformed allowlist entry must fail the whole fleet load (fail-closed \
             authz surface), not be silently dropped"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channels_plural_single_entry_collapses_to_singular() {
    // `channels:` (plural) with one entry normalizes into `channel:`
    // so downstream readers that only know the singular field keep
    // working unchanged.
    let dir = std::env::temp_dir().join(format!("agend-fleet-chan-plural-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channels:
  tg-main:
    type: telegram
    bot_token_env: MY_BOT_TOKEN
    group_id: -100999
    mode: topic
instances: {}
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    match config.channel {
        Some(ChannelConfig::Telegram {
            ref bot_token_env,
            group_id,
            ..
        }) => {
            assert_eq!(bot_token_env, "MY_BOT_TOKEN");
            assert_eq!(group_id, -100999);
        }
        None => panic!("plural channels: should populate singular channel field"),
        Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
    }
    // Plural is still preserved on the struct for later consumers.
    assert!(config.channels.is_some());
    assert_eq!(config.channels.as_ref().unwrap().len(), 1);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channels_plural_multi_entry_picks_first_by_name() {
    // Multi-channel routing is not yet wired; normalize must pick a
    // deterministic entry (first by sorted key) so runtime behavior
    // does not depend on HashMap iteration order.
    let dir = std::env::temp_dir().join(format!("agend-fleet-chan-multi-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channels:
  zeta:
    type: telegram
    bot_token_env: ZETA_TOKEN
    group_id: -3
  alpha:
    type: telegram
    bot_token_env: ALPHA_TOKEN
    group_id: -1
  mid:
    type: telegram
    bot_token_env: MID_TOKEN
    group_id: -2
instances: {}
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    match config.channel {
        Some(ChannelConfig::Telegram {
            ref bot_token_env, ..
        }) => {
            assert_eq!(
                bot_token_env, "ALPHA_TOKEN",
                "must pick first entry by sorted name"
            );
        }
        None => panic!("channel should be populated"),
        Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channel_singular_wins_when_both_set() {
    // Byte-identical runtime for inputs that already wrote `channel:`:
    // even if `channels:` is also present, the singular form wins and
    // `normalize()` leaves it alone.
    let dir = std::env::temp_dir().join(format!("agend-fleet-chan-both-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channel:
  type: telegram
  bot_token_env: SINGULAR_TOKEN
  group_id: -111
channels:
  plural-entry:
    type: telegram
    bot_token_env: PLURAL_TOKEN
    group_id: -222
instances: {}
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    match config.channel {
        Some(ChannelConfig::Telegram {
            ref bot_token_env, ..
        }) => assert_eq!(bot_token_env, "SINGULAR_TOKEN"),
        None => panic!("singular channel field must be preserved"),
        Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channel_absent_when_neither_form_set() {
    // Zero-config case: no channel wiring, no warnings, no panics.
    let dir = std::env::temp_dir().join(format!("agend-fleet-chan-none-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  a:
    backend: claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert!(config.channel.is_none());
    assert!(config.channels.is_none());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_channel_config_default_mode() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-defmode-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
channel:
  type: telegram
  bot_token_env: TOKEN
  group_id: -999
instances: {}
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    match config.channel {
        Some(ChannelConfig::Telegram { ref mode, .. }) => {
            assert_eq!(mode, "topic", "default mode should be 'topic'");
        }
        None => panic!("channel should be Some"),
        Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
    }

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_missing_defaults_still_works() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-nodef-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  agent1:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert!(config.defaults.backend.is_none());
    assert!(config.defaults.command.is_none());
    assert!(config.defaults.model.is_none());
    let resolved = config.resolve_instance("agent1").expect("resolve");
    assert_eq!(resolved.backend_command, "/bin/bash");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_instance_names_returns_all() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-names-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  alpha:
    command: /bin/bash
  beta:
    command: /bin/sh
  gamma:
    command: /bin/zsh
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let mut names = config.instance_names();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_add_remove_instance_roundtrip() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-addrem-{}", std::process::id()));
    write_fleet(&dir, "instances: {}\n");

    let entry = InstanceYamlEntry {
        backend: Some("claude".to_string()),
        role: Some("tester".to_string()),
        ..Default::default()
    };
    add_instance_to_yaml(&dir, "temp-agent", &entry).expect("add");

    let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
    assert!(config.instances.contains_key("temp-agent"));

    remove_instance_from_yaml(&dir, "temp-agent").expect("remove");
    let config2 = FleetConfig::load(&dir.join("fleet.yaml")).expect("load after remove");
    assert!(!config2.instances.contains_key("temp-agent"));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_working_directory_tilde_expansion() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-tilde-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  agent1:
    command: /bin/bash
    working_directory: "~/project"
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("agent1").expect("resolve");
    let wd = resolved
        .working_directory
        .expect("should have working_directory");
    // Should NOT start with ~
    assert!(
        !wd.to_string_lossy().starts_with('~'),
        "tilde should be expanded, got: {}",
        wd.display()
    );
    // Should end with the `project` component — compare via Path so the
    // separator flip on Windows (`\`) doesn't trip a plain string match.
    assert!(
        wd.ends_with("project"),
        "should end with project, got: {}",
        wd.display()
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_working_directory_absolute_unchanged() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-abs-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  agent1:
    command: /bin/bash
    working_directory: "/absolute/path"
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("agent1").expect("resolve");
    let wd = resolved.working_directory.expect("should have wd");
    assert_eq!(wd.to_string_lossy(), "/absolute/path");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_resolve_nonexistent_instance() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-noinst-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  agent1:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert!(config.resolve_instance("nonexistent").is_none());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_teams_parsing() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-teams-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  a1:
    command: /bin/bash
  a2:
    command: /bin/bash
teams:
  dev:
    members:
      - a1
      - a2
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let team = config.teams.get("dev").expect("team exists");
    assert_eq!(team.members, vec!["a1", "a2"]);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_instance_env_includes_agend_name() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-envname-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  my-agent:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("my-agent").expect("resolve");
    assert_eq!(
        resolved.env.get("AGEND_INSTANCE_NAME").map(|s| s.as_str()),
        Some("my-agent")
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_cols_rows_override() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-colrow-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  cols: 80
  rows: 24
instances:
  default-size:
    command: /bin/bash
  custom-size:
    command: /bin/bash
    cols: 200
    rows: 50
"#,
    );
    let config = FleetConfig::load(&path).expect("load");

    let def = config.resolve_instance("default-size").expect("resolve");
    assert_eq!(def.cols, Some(80));
    assert_eq!(def.rows, Some(24));

    let custom = config.resolve_instance("custom-size").expect("resolve");
    assert_eq!(custom.cols, Some(200));
    assert_eq!(custom.rows, Some(50));

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_git_branch_override() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-test4-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  with_branch:
    command: /bin/bash
    git_branch: "custom/branch"
  without_branch:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");

    let with = config.resolve_instance("with_branch").expect("resolve");
    assert_eq!(with.git_branch.as_deref(), Some("custom/branch"));

    let without = config.resolve_instance("without_branch").expect("resolve");
    assert!(without.git_branch.is_none());

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_topic_id_parsed() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-topic-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(
        &path,
        r#"instances:
  alice:
    backend: claude
    topic_id: 229
  general:
    backend: claude
    topic_id: 1
"#,
    )
    .ok();
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(
        config.instances.get("alice").and_then(|i| i.topic_id),
        Some(229)
    );
    assert_eq!(
        config.instances.get("general").and_then(|i| i.topic_id),
        Some(1)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_topic_id_none_when_missing() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-notopic-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(
        &path,
        r#"instances:
  dev:
    backend: claude
"#,
    )
    .ok();
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(config.instances.get("dev").and_then(|i| i.topic_id), None);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_remove_instance_preserves_other_topics() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-rmtopic-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(
        &path,
        r#"instances:
  alice:
    backend: claude
    topic_id: 229
  bob:
    backend: claude
    topic_id: 300
"#,
    )
    .ok();
    remove_instance_from_yaml(&dir, "alice").expect("remove");
    let config = FleetConfig::load(&path).expect("load");
    assert!(!config.instances.contains_key("alice"));
    assert_eq!(
        config.instances.get("bob").and_then(|i| i.topic_id),
        Some(300)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_default_working_directory() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-defwd-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(
        &path,
        r#"instances:
  alice:
    backend: claude
  bob:
    backend: claude
    working_directory: /tmp/custom
"#,
    )
    .ok();
    let config = FleetConfig::load(&path).expect("load");

    // alice: no working_directory → defaults to $AGEND_HOME/workspace/alice
    let alice = config.resolve_instance("alice").expect("alice");
    let wd = alice.working_directory.expect("wd");
    // Compare components (not strings) so `\` on Windows doesn't fail.
    assert!(
        wd.ends_with("workspace/alice"),
        "expected default workspace path, got: {}",
        wd.display()
    );

    // bob: explicit working_directory → used as-is
    let bob = config.resolve_instance("bob").expect("bob");
    assert_eq!(
        bob.working_directory.expect("wd"),
        std::path::PathBuf::from("/tmp/custom")
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_working_directory_always_some() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-wdsome-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let path = dir.join("fleet.yaml");
    fs::write(
        &path,
        r#"instances:
  minimal:
    backend: claude
"#,
    )
    .ok();
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("minimal").expect("resolve");
    assert!(
        resolved.working_directory.is_some(),
        "working_directory must always be Some after resolve"
    );
    fs::remove_dir_all(&dir).ok();
}

// ── Normalize: backend is derived from legacy `command:` at load ─────

#[test]
fn normalize_legacy_command_only_becomes_backend() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm1-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  command: /bin/bash
instances:
  worker:
    command: /opt/custom/tool
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    // Absolute paths preserve the literal — a later spawn uses them
    // verbatim. Only the bare names `shell|bash|zsh|sh` fold into Shell.
    assert_eq!(
        config.defaults.backend,
        Some(Backend::Raw("/bin/bash".to_string()))
    );
    assert_eq!(
        config
            .instances
            .get("worker")
            .and_then(|i| i.backend.clone()),
        Some(Backend::Raw("/opt/custom/tool".to_string()))
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn normalize_legacy_command_with_known_preset_name() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm2-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  command: claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(config.defaults.backend, Some(Backend::ClaudeCode));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn normalize_explicit_backend_takes_precedence_over_command() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm3-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  worker:
    backend: claude
    command: /custom/claude-v2
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    // Explicit backend wins — command remains for resolve_instance to use as override.
    let inst = config.instances.get("worker").expect("worker");
    assert_eq!(inst.backend, Some(Backend::ClaudeCode));
    assert_eq!(inst.command.as_deref(), Some("/custom/claude-v2"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn parse_new_shell_variant() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm4-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  bash_pane:
    backend: shell
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(
        config
            .instances
            .get("bash_pane")
            .and_then(|i| i.backend.clone()),
        Some(Backend::Shell)
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn parse_new_raw_variant_as_bare_path() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm5-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  custom:
    backend: /opt/foo/bar
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(
        config
            .instances
            .get("custom")
            .and_then(|i| i.backend.clone()),
        Some(Backend::Raw("/opt/foo/bar".to_string()))
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn explicit_backend_plus_command_override_preserves_backend_contract() {
    // `backend:` is the preset contract; `command:` is purely the spawn
    // path. resolve_instance returns user-only args (empty here); the
    // preset flags are injected at spawn time by agent::spawn_agent.
    let dir = std::env::temp_dir().join(format!("agend-fleet-override-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
instances:
  test:
    backend: claude
    command: /opt/claude-v2/my-claude
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("test").expect("resolve");
    assert_eq!(resolved.backend_command, "/opt/claude-v2/my-claude");
    assert!(
        resolved.args.is_empty(),
        "resolved.args must be user-only, got: {:?}",
        resolved.args
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn normalize_is_idempotent() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-norm6-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  command: zsh
"#,
    );
    let mut config = FleetConfig::load(&path).expect("load");
    let before = config.defaults.backend.clone();
    config.normalize();
    // Running it again produces the same result.
    assert_eq!(config.defaults.backend, before);
    // Bare "zsh" (no leading slash) is the shell alias.
    assert_eq!(config.defaults.backend, Some(Backend::Shell));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn worktree_opt_out_parsed() {
    let yaml = "instances:\n  lead:\n    backend: claude\n    worktree: false\n  impl:\n    backend: claude\n";
    let config: FleetConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.instances["lead"].worktree, Some(false));
    assert_eq!(config.instances["impl"].worktree, None);
}

/// §3.5.10 canonical round-trip fixture: parse fleet.yaml via
/// serde_yaml_ng → serialize → verify semantic equivalence +
/// idempotent serialization. Uses Path B (canonical snapshot)
/// because YAML serializers don't preserve comments/quote-style
/// exactly, and HashMap iteration order is nondeterministic.
///
/// Production-path-coupled: uses the real serde_yaml_ng import.
#[test]
fn serde_yaml_ng_canonical_round_trip() {
    // Single instance to avoid HashMap iteration order nondeterminism.
    let input = "defaults:\n  backend: claude\ninstances:\n  dev:\n    backend: kiro-cli\n    topic_id: 42\n";
    let config: FleetConfig = serde_yaml_ng::from_str(input).unwrap();
    let output = serde_yaml_ng::to_string(&config).unwrap();
    let reparsed: FleetConfig = serde_yaml_ng::from_str(&output).unwrap();

    // Semantic equivalence.
    assert_eq!(reparsed.instances.len(), 1);
    assert_eq!(reparsed.instances["dev"].topic_id, Some(42));
    assert_eq!(
        reparsed.instances["dev"].backend,
        Some(crate::backend::Backend::KiroCli)
    );

    // Idempotence: second serialize must match first.
    let output2 = serde_yaml_ng::to_string(&reparsed).unwrap();
    assert_eq!(output, output2, "serialization must be idempotent");

    // Adversarial: numeric as integer, strings unquoted.
    assert!(output.contains("42"), "topic_id must appear as integer");
    assert!(
        output.contains("kiro-cli"),
        "string values preserved: {output}"
    );
}

#[test]
fn backfill_ids_opt_out_no_writeback() {
    // Local env guard for test isolation

    let _g = env_guard();
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-backfill-optout-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    let yaml = "instances:\n  test-agent:\n    backend: claude\n";
    let path = dir.join("fleet.yaml");
    std::fs::write(&path, yaml).expect("write");
    std::env::set_var("AGEND_FLEET_NO_AUTO_MIGRATE", "1");
    let _ = FleetConfig::load(&path);
    std::env::remove_var("AGEND_FLEET_NO_AUTO_MIGRATE");
    let content = std::fs::read_to_string(&path).expect("read");
    assert!(
        !content.contains("id:"),
        "opt-out should prevent id writeback: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn backfill_ids_writes_when_opt_out_unset() {
    // Same guard as backfill_ids_opt_out_no_writeback

    let _g = env_guard();
    let dir =
        std::env::temp_dir().join(format!("agend-fleet-backfill-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let yaml = "instances:\n  test-agent:\n    backend: claude\n";
    let path = dir.join("fleet.yaml");
    std::fs::write(&path, yaml).expect("write");
    std::env::remove_var("AGEND_FLEET_NO_AUTO_MIGRATE");
    let _ = FleetConfig::load(&path);
    let content = std::fs::read_to_string(&path).expect("read");
    assert!(
        content.contains("id:"),
        "writeback should add id field: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ─── Sprint 54 P1-B Bug 2 fix: source_repo decouple tests ───────

/// Backward-compat: fleet.yaml that predates the field deserializes
/// cleanly. `source_repo` defaults to None — `dispatch_auto_bind_lease`
/// will fall back to `working_directory`. Locks the
/// `#[serde(default)]` contract.
#[test]
fn instance_config_deserializes_without_source_repo_field() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-srf-bc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let path = write_fleet(
        &dir,
        r#"
instances:
  legacy-agent:
    backend: claude
    working_directory: /tmp/legacy-agent
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let inst = config.instances.get("legacy-agent").expect("inst present");
    assert!(
        inst.source_repo.is_none(),
        "source_repo defaults to None when omitted: {inst:?}"
    );
    let resolved = config.resolve_instance("legacy-agent").expect("resolve");
    assert!(resolved.source_repo.is_none());
    assert_eq!(
        resolved
            .working_directory
            .as_deref()
            .map(|p| p.to_str().unwrap_or("")),
        Some("/tmp/legacy-agent"),
        "working_directory still resolves for backward-compat callers"
    );
    fs::remove_dir_all(&dir).ok();
}

/// New field round-trip: when fleet.yaml carries `source_repo`,
/// the resolved struct surfaces it as a `PathBuf` distinct from
/// `working_directory`. Locks the schema-decouple contract that
/// dispatch_auto_bind_lease relies on.
#[test]
fn instance_config_resolves_source_repo_when_set() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-srf-set-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let path = write_fleet(
        &dir,
        r#"
instances:
  opted-in-agent:
    backend: claude
    working_directory: /tmp/opted-in-agent-state
    source_repo: /tmp/opted-in-agent-source
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("opted-in-agent").expect("resolve");
    assert_eq!(
        resolved
            .source_repo
            .as_deref()
            .map(|p| p.to_str().unwrap_or("")),
        Some("/tmp/opted-in-agent-source"),
        "source_repo resolves to the explicit fleet.yaml value"
    );
    assert_eq!(
        resolved
            .working_directory
            .as_deref()
            .map(|p| p.to_str().unwrap_or("")),
        Some("/tmp/opted-in-agent-state"),
        "working_directory remains the per-agent state-home dir, decoupled from source_repo"
    );
    fs::remove_dir_all(&dir).ok();
}

/// `~/` expansion applies to source_repo with the same treatment
/// `working_directory` already gets. Locks parity so operator
/// muscle-memory transfers cleanly between the two fields.
#[test]
fn instance_config_source_repo_tilde_expanded() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-srf-tilde-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let path = write_fleet(
        &dir,
        r#"
instances:
  tilde-agent:
    backend: claude
    source_repo: ~/op-clone
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("tilde-agent").expect("resolve");
    let source_repo = resolved.source_repo.expect("source_repo set");
    let expected_home = dirs::home_dir().expect("home dir");
    assert_eq!(
        source_repo,
        expected_home.join("op-clone"),
        "~ expansion must match working_directory's behaviour"
    );
    fs::remove_dir_all(&dir).ok();
}

/// Round-trip: writing an `InstanceYamlEntry` with `source_repo`
/// set persists the field to disk in a form that re-loads
/// identically. Locks the writer-reader symmetry — without this,
/// operators editing fleet.yaml could see their `source_repo`
/// silently disappear on the next daemon write.
#[test]
fn add_instance_to_yaml_round_trips_source_repo() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-srf-rt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    fs::create_dir_all(&dir).ok();
    let entry = InstanceYamlEntry {
        backend: Some("claude".to_string()),
        working_directory: Some("/tmp/rt-state".to_string()),
        role: Some("opted-in test agent".to_string()),
        source_repo: Some("/tmp/rt-source".to_string()),
        ..Default::default()
    };
    add_instance_to_yaml(&dir, "rt-agent", &entry).expect("add");
    let content = std::fs::read_to_string(dir.join("fleet.yaml")).expect("read");
    assert!(
        content.contains("source_repo: /tmp/rt-source"),
        "source_repo must round-trip through the writer: {content}"
    );
    let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
    let resolved = config.resolve_instance("rt-agent").expect("resolve");
    assert_eq!(
        resolved
            .source_repo
            .as_deref()
            .map(|p| p.to_str().unwrap_or("")),
        Some("/tmp/rt-source"),
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn add_instance_to_yaml_round_trips_skills_path() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-sp-rt-{}", std::process::id()));
    fs::create_dir_all(&dir).ok();
    let entry = InstanceYamlEntry {
        backend: Some("claude".to_string()),
        skills_path: Some("/tmp/custom-skills".to_string()),
        ..Default::default()
    };
    add_instance_to_yaml(&dir, "sp-agent", &entry).expect("add");
    let content = std::fs::read_to_string(dir.join("fleet.yaml")).expect("read");
    assert!(
        content.contains("skills_path: /tmp/custom-skills"),
        "skills_path must round-trip: {content}"
    );
    let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
    let resolved = config.instances.get("sp-agent").expect("get");
    assert_eq!(resolved.skills_path.as_deref(), Some("/tmp/custom-skills"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn topic_binding_mode_round_trips_through_fleet_yaml() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-tb-rt-{}", std::process::id()));
    write_fleet(&dir, "instances: {}\n");
    let entry = InstanceYamlEntry {
        backend: Some("claude".to_string()),
        topic_binding_mode: Some("skip".to_string()),
        ..Default::default()
    };
    add_instance_to_yaml(&dir, "internal-helper", &entry).expect("add");
    let content = std::fs::read_to_string(dir.join("fleet.yaml")).expect("read");
    assert!(
        content.contains("topic_binding_mode: skip"),
        "topic_binding_mode must appear in fleet.yaml: {content}"
    );
    let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
    let inst = config.instances.get("internal-helper").expect("exists");
    assert_eq!(
        inst.topic_binding_mode.as_deref(),
        Some("skip"),
        "topic_binding_mode must round-trip through serde"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn topic_binding_mode_absent_parses_as_none() {
    let dir = std::env::temp_dir().join(format!("agend-fleet-tb-absent-{}", std::process::id()));
    write_fleet(&dir, "instances:\n  agent1:\n    backend: claude\n");
    let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
    let inst = config.instances.get("agent1").expect("exists");
    assert!(
        inst.topic_binding_mode.is_none(),
        "absent field must parse as None for back-compat"
    );
    fs::remove_dir_all(&dir).ok();
}

// ─────────────────────────────────────────────────────────────
// ── #962 silent-persist failure tests (Layer 1 internal tracing) ──
//
// Each test pins one of the 3 documented silent no-op paths inside
// `update_instance_field`. Pre-#962 all three returned `Ok(())` and
// callers had no way to distinguish persisted from silently-dropped.
// Post-#962 they return `Ok(false)` and emit `tracing::warn!` with
// a stable `reason` field.

fn tmp_home_962(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("agend-962-{}-{}-{}", tag, std::process::id(), id));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn update_instance_field_returns_false_when_fleet_yaml_absent() {
    let home = tmp_home_962("absent");
    // No fleet.yaml planted.
    let result = update_instance_field(
        &home,
        "any-agent",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
    );
    assert!(
        matches!(result, Ok(false)),
        "missing fleet.yaml must return Ok(false), got {result:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_instance_field_returns_false_when_instance_entry_missing() {
    let home = tmp_home_962("entry-missing");
    // fleet.yaml exists but has no entry for "ghost".
    std::fs::write(
        fleet_yaml_path(&home),
        "instances:\n  alpha:\n    backend: claude\n",
    )
    .unwrap();
    let result = update_instance_field(
        &home,
        "ghost",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
    );
    assert!(
        matches!(result, Ok(false)),
        "missing instance entry must return Ok(false), got {result:?}"
    );
    // Confirm fleet.yaml unchanged.
    let body = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
    assert!(!body.contains("ghost"), "ghost entry must NOT be inserted");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_instance_field_returns_false_when_not_mapping() {
    let home = tmp_home_962("not-mapping");
    // Instance entry exists but is a SCALAR (string), not a mapping.
    std::fs::write(
        fleet_yaml_path(&home),
        "instances:\n  alpha: just-a-string\n",
    )
    .unwrap();
    let result = update_instance_field(
        &home,
        "alpha",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
    );
    assert!(
        matches!(result, Ok(false)),
        "non-mapping entry must return Ok(false), got {result:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn update_instance_field_returns_true_on_successful_persist() {
    let home = tmp_home_962("happy");
    std::fs::write(
        fleet_yaml_path(&home),
        "instances:\n  alpha:\n    backend: claude\n",
    )
    .unwrap();
    let result = update_instance_field(
        &home,
        "alpha",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(123)),
    );
    assert!(
        matches!(result, Ok(true)),
        "happy path must return Ok(true), got {result:?}"
    );
    let body = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
    assert!(
        body.contains("topic_id: 123"),
        "topic_id must be persisted; got:\n{body}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[tracing_test::traced_test]
#[test]
fn update_instance_field_emits_warn_with_reason_on_each_no_op_path() {
    // Path 2: instance entry missing.
    let home = tmp_home_962("warn-entry-missing");
    std::fs::write(
        fleet_yaml_path(&home),
        "instances:\n  alpha:\n    backend: claude\n",
    )
    .unwrap();
    let _ = update_instance_field(
        &home,
        "ghost",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
    );
    assert!(
        logs_contain("update_instance_field skipped"),
        "tracing::warn! must fire on silent no-op path"
    );
    assert!(
        logs_contain("instance_entry_missing"),
        "tracing reason must identify the specific no-op path"
    );
    assert!(logs_contain("ghost"), "tracing must carry instance name");
    assert!(logs_contain("topic_id"), "tracing must carry field name");
    std::fs::remove_dir_all(&home).ok();
}

/// #964 regression anchor — promoted from dev's bisect spike at
/// `/tmp/bisect-964-dev.md`. Documents the SILENT no-op contract:
/// when `update_instance_field` is called against a missing entry, it
/// MUST return `Ok(false)` and leave fleet.yaml unchanged. The #964
/// fix is caller-side (`spawn_single_instance` adds the entry BEFORE
/// SPAWN so the SPAWN-time `register_topic` chain finds it) — this
/// test pins the helper's intentional contract so a future
/// well-meaning refactor to "auto-insert on missing" doesn't silently
/// re-introduce the bootstrap-backfill ambiguity that masked #964 for
/// 27 days.
///
/// Sibling: caller-side regression tests at
/// `src/mcp/handlers/instance.rs::tests_964::t1_create_instance_persists_topic_id_to_fleet_yaml`.
#[test]
fn repro_964_helper_silently_no_ops_on_empty_fleet_yaml() {
    let home = tmp_home_962("repro-964");
    // Plant fleet.yaml with explicitly NO entry for the target name.
    std::fs::write(fleet_yaml_path(&home), "instances: {}\n").unwrap();

    let result = update_instance_field(
        &home,
        "test-964-verify",
        "topic_id",
        serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(5198)),
    );

    assert!(
        matches!(result, Ok(false)),
        "#964 anchor: helper MUST silently no-op (Ok(false)) on \
             missing entry; the fix lives in the caller. Got: {result:?}"
    );

    let cfg = FleetConfig::load(&fleet_yaml_path(&home)).expect("reload");
    assert!(
        !cfg.instances.contains_key("test-964-verify"),
        "#964 anchor: helper MUST NOT auto-insert; got entry {:?}",
        cfg.instances.get("test-964-verify")
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn load_cache_returns_same_result_on_unchanged_file() {
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-cache-hit-{}", std::process::id()));
    let path = write_fleet(&dir, "instances:\n  cached-agent:\n    backend: claude\n");
    let first = FleetConfig::load(&path).expect("first load");
    let second = FleetConfig::load(&path).expect("second load (cached)");
    assert_eq!(first.instances.len(), second.instances.len());
    assert!(second.instances.contains_key("cached-agent"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_cache_invalidates_on_file_change() {
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-cache-inv-{}", std::process::id()));
    let path = write_fleet(&dir, "instances:\n  old-agent:\n    backend: claude\n");
    let mtime_before = std::fs::metadata(&path)
        .expect("stat")
        .modified()
        .expect("mtime");
    let first = FleetConfig::load(&path).expect("first load");
    assert!(first.instances.contains_key("old-agent"));

    // `old-agent` and `new-agent` are the same byte length, so size can't
    // distinguish the rewrite — only mtime can (this test pins the mtime
    // path; the size path is covered by load_cache_detects_same_mtime_different_size).
    // #t-3 audit: force a deterministically-different mtime instead of
    // sleeping 1100ms for the filesystem's mtime granularity — the sleep
    // was the flake and slowed the suite.
    std::fs::write(&path, "instances:\n  new-agent:\n    backend: claude\n").expect("rewrite");
    let f = std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("reopen for set_modified");
    f.set_modified(mtime_before + std::time::Duration::from_secs(2))
        .expect("set mtime");
    drop(f);

    let second = FleetConfig::load(&path).expect("second load (invalidated)");
    assert!(
        !second.instances.contains_key("old-agent"),
        "old-agent should be gone after rewrite"
    );
    assert!(
        second.instances.contains_key("new-agent"),
        "new-agent should appear after rewrite"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_cache_detects_same_mtime_different_size() {
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-cache-size-{}", std::process::id()));
    let path = write_fleet(&dir, "instances:\n  a:\n    backend: claude\n");
    let first = FleetConfig::load(&path).expect("first load");
    assert!(first.instances.contains_key("a"));

    // Rewrite with different content (different size) without sleeping —
    // mtime may be identical on coarse-grained filesystems.
    std::fs::write(
        &path,
        "instances:\n  longer-name-agent:\n    backend: claude\n",
    )
    .expect("rewrite");

    let second = FleetConfig::load(&path).expect("second load");
    // If size changed, cache must invalidate even with same mtime.
    // If mtime also changed, cache invalidates too. Either way: correct.
    assert!(
        second.instances.contains_key("longer-name-agent"),
        "different-size rewrite must not return stale cache"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── #perf-R4: load_arc (Arc refcount bump on cache HIT) vs load (deep clone) ──

/// `load_arc` must return content behaviour-equivalent to `load` (the cold
/// path). Order-independent + eviction-robust (no cache-HIT dependency, so
/// it can't flake on the single-entry global `FLEET_CACHE`).
#[test]
fn load_arc_content_equals_load() {
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-arc-eq-{}", std::process::id()));
    let path = write_fleet(
        &dir,
        "instances:\n  a:\n    backend: claude\n  b:\n    backend: codex\n",
    );
    let arc = FleetConfig::load_arc(&path).expect("load_arc");
    let owned = FleetConfig::load(&path).expect("load");
    let arc_keys: std::collections::BTreeSet<_> = arc.instances.keys().cloned().collect();
    let owned_keys: std::collections::BTreeSet<_> = owned.instances.keys().cloned().collect();
    assert_eq!(
        arc_keys, owned_keys,
        "#perf-R4: load_arc and load must see the same instances"
    );
    let expect: std::collections::BTreeSet<_> =
        ["a".to_string(), "b".to_string()].into_iter().collect();
    assert_eq!(arc_keys, expect);
    assert_eq!(
        arc.resolve_instance("a").map(|r| r.backend_command),
        owned.resolve_instance("a").map(|r| r.backend_command),
        "#perf-R4: load_arc must resolve byte-identically to load"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Regression guard: the supervisor per-tick hot callers must use `load_arc`
/// (Arc bump), not `load` (deep clone). Source-scan because it's
/// eviction-immune — a cache-HIT/`Arc::ptr_eq` assertion would flake (the
/// single-entry `FLEET_CACHE` is evicted by parallel tests in other modules).
#[test]
fn hot_callers_use_load_arc_not_load() {
    fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let start = src
            .find(sig)
            .unwrap_or_else(|| panic!("fn not found: {sig}"));
        let after = &src[start + sig.len()..];
        let a = after.find("\nfn ");
        let b = after.find("\npub fn ");
        let end = match (a, b) {
            (Some(x), Some(y)) => x.min(y),
            (Some(x), None) => x,
            (None, Some(y)) => y,
            (None, None) => after.len(),
        };
        &src[start..start + sig.len() + end]
    }
    let supervisor = include_str!("../daemon/supervisor.rs");
    let agent_ops = include_str!("../agent_ops.rs");
    for (src, sig) in [
        (supervisor, "fn pane_input_backend_supported"),
        (supervisor, "fn idle_expectation_for"),
        (agent_ops, "fn metadata_path_resolved"),
    ] {
        let body = fn_body(src, sig);
        assert!(
            body.contains("FleetConfig::load_arc("),
            "#perf-R4: `{sig}` is a per-tick hot caller and MUST use FleetConfig::load_arc"
        );
        assert!(
                !body.contains("FleetConfig::load("),
                "#perf-R4: `{sig}` must NOT use FleetConfig::load (deep clone) on the per-tick hot path"
            );
    }
}

/// #perf-R4 manual bench (NOT a CI gate — `#[ignore]`, machine-dependent).
/// `cargo nextest run --release --run-ignored all -E 'test(perf_r4_fleet)' --no-capture`
/// Shows the per-call cost of `load` (deep-clones the whole N-instance fleet
/// on every cache HIT) vs `load_arc` (refcount bump), as N grows.
#[test]
#[ignore = "perf measurement, run explicitly in release"]
fn perf_r4_fleet_load_arc_vs_load() {
    use std::time::Instant;
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-arc-bench-{}", std::process::id()));
    let n_instances = 200usize;
    let mut yaml = String::from("instances:\n");
    for i in 0..n_instances {
        yaml.push_str(&format!(
            "  agent-{i:04}:\n    backend: claude\n    role: worker number {i}\n"
        ));
    }
    let path = write_fleet(&dir, &yaml);
    let iters = 50_000u32;
    let _ = FleetConfig::load_arc(&path).expect("populate cache");

    let t0 = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        acc = acc.wrapping_add(FleetConfig::load(&path).expect("load").instances.len());
    }
    let load_dur = t0.elapsed();

    let t1 = Instant::now();
    for _ in 0..iters {
        acc = acc.wrapping_add(
            FleetConfig::load_arc(&path)
                .expect("load_arc")
                .instances
                .len(),
        );
    }
    let arc_dur = t1.elapsed();

    let lo = load_dur.as_nanos() as f64 / iters as f64;
    let ar = arc_dur.as_nanos() as f64 / iters as f64;
    println!("#perf-R4 (N={n_instances} instances, iters={iters}, acc={acc}):");
    println!("  load     (deep-clone on cache HIT): {lo:.0} ns/call");
    println!("  load_arc (Arc refcount bump)      : {ar:.0} ns/call");
    println!("  speedup                           : {:.1}x", lo / ar);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mutate_path_invalidates_cache() {
    let _g = env_guard();
    invalidate_cache();
    let dir = std::env::temp_dir().join(format!("agend-fleet-cache-mutate-{}", std::process::id()));
    let path = write_fleet(&dir, "instances:\n  before:\n    backend: claude\n");
    let first = FleetConfig::load(&path).expect("first load");
    assert!(first.instances.contains_key("before"));

    add_instance_to_yaml(
        &dir,
        "after",
        &InstanceYamlEntry {
            backend: Some("claude-code".to_string()),
            ..Default::default()
        },
    )
    .expect("add instance");

    let second = FleetConfig::load(&path).expect("load after mutate");
    assert!(
        second.instances.contains_key("after"),
        "cache must be invalidated by atomic_write_yaml"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_instance_inherits_defaults_instructions() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-def-instr-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  instructions: shared.md
instances:
  agent1:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("agent1").expect("resolve");
    assert_eq!(
        resolved.instructions.as_deref(),
        Some("shared.md"),
        "defaults.instructions must propagate when instance omits it"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_instance_instructions_override_beats_defaults() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-instr-override-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
defaults:
  instructions: shared.md
instances:
  agent1:
    command: /bin/bash
    instructions: custom.md
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("agent1").expect("resolve");
    assert_eq!(
        resolved.instructions.as_deref(),
        Some("custom.md"),
        "instance instructions must override defaults"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_instance_no_instructions_stays_none() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-no-instr-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
instances:
  agent1:
    command: /bin/bash
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    let resolved = config.resolve_instance("agent1").expect("resolve");
    assert_eq!(
        resolved.instructions, None,
        "no instructions anywhere must remain None"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_instance_model_tier_policy_2477() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-model-tier-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
model_tiers:
  cheap: sonnet
  strong: opus
role_model_tiers:
  orchestrator: strong
  implementer: cheap
defaults:
  backend: claude
  model_tier: cheap
instances:
  lead:
    role_kind: orchestrator
  dev:
    role_kind: implementer
  specialist:
    role_kind: implementer
    model_tier: strong
  exact:
    role_kind: implementer
    model: custom-exact-model
  fallback:
    role: no typed role
"#,
    );
    let config = FleetConfig::load(&path).expect("load");

    assert_eq!(
        config
            .resolve_instance("lead")
            .expect("lead")
            .model
            .as_deref(),
        Some("opus"),
        "role_model_tiers must let orchestrators use the strong tier"
    );
    assert_eq!(
        config
            .resolve_instance("dev")
            .expect("dev")
            .model
            .as_deref(),
        Some("sonnet"),
        "implementers should inherit the cheap tier"
    );
    assert_eq!(
        config
            .resolve_instance("specialist")
            .expect("specialist")
            .model
            .as_deref(),
        Some("opus"),
        "instance model_tier must override the role tier"
    );
    assert_eq!(
        config
            .resolve_instance("exact")
            .expect("exact")
            .model
            .as_deref(),
        Some("custom-exact-model"),
        "concrete model must remain the highest-precedence escape hatch"
    );
    assert_eq!(
        config
            .resolve_instance("fallback")
            .expect("fallback")
            .model
            .as_deref(),
        Some("sonnet"),
        "defaults.model_tier should cover instances without a role policy"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn resolve_instance_instance_model_tier_overrides_defaults_model_2477() {
    let dir = std::env::temp_dir().join(format!(
        "agend-fleet-model-tier-default-model-{}-{}",
        std::process::id(),
        line!()
    ));
    let path = write_fleet(
        &dir,
        r#"
model_tiers:
  strong: opus
defaults:
  backend: claude
  model: default-concrete-model
instances:
  specialist:
    model_tier: strong
"#,
    );
    let config = FleetConfig::load(&path).expect("load");
    assert_eq!(
        config
            .resolve_instance("specialist")
            .expect("specialist")
            .model
            .as_deref(),
        Some("opus"),
        "an instance's tier policy must override defaults.model"
    );
    fs::remove_dir_all(&dir).ok();
}
