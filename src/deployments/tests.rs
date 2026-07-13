use super::*;

// #2764 R10 (item 2): the deploy tests predate the spawn seam and encoded
// "record created even when every daemon-less spawn fails". The seam makes
// spawn outcomes drive the transaction, so the suite injects a SUCCEEDING
// spawn stub — the failure arms get their own dedicated REDs below. This
// local `deploy` shadows the production wrapper for every existing call.
fn deploy(home: &std::path::Path, caller: &str, args: &serde_json::Value) -> serde_json::Value {
    super::deploy_impl(home, caller, args, &|_h, _req| {
        Ok(serde_json::json!({"ok": true, "result": {}}))
    })
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-deploy-test-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// §3.9 (MED-2): the binary deploy's Phase-3 SPAWN must run a template's
/// `command:` override, not the `backend:` preset. `resolve_spawn_backend`
/// (which feeds `params["backend"]`, run AS the command by the SPAWN handler)
/// must return the `command:` for an entry that declares one. Regression-proof:
/// revert to raw `entry.backend` and the override assertion fails ("claude").
#[test]
fn resolve_spawn_backend_honors_command_override_med2() {
    let home = tmp_home("med2-backend");
    // The entries are persisted to fleet.yaml in Phase 2 before the spawn.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  worker:\n    backend: claude\n    command: ./my-runner.sh\n",
    )
    .unwrap();
    // `entry` is only the resolve-failure fallback; the fix resolves via fleet.yaml.
    let entry = crate::fleet::InstanceYamlEntry {
        backend: Some("claude".into()),
        ..Default::default()
    };

    // Template `command:` override reaches the spawn binary (was: "claude").
    assert_eq!(
        resolve_spawn_backend(&home, "worker", &entry),
        "./my-runner.sh",
        "MED-2: a template `command:` override must reach the Phase-3 spawn"
    );
    // An entry absent from fleet.yaml falls back to its declared backend.
    assert_eq!(
        resolve_spawn_backend(&home, "ghost", &entry),
        "claude",
        "fallback to entry.backend when the instance can't be resolved"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_rejects_bad_deploy_name() {
    let home = tmp_home("bad_deploy");
    let args = serde_json::json!({
        "template": "ok-template",
        "directory": home.display().to_string(),
        "name": "../escape",
    });
    let out = deploy(&home, "caller", &args);
    let err = out["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("invalid deploy name"),
        "expected deploy-name rejection, got: {out}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_rejects_bad_template_name() {
    let home = tmp_home("bad_tpl");
    let args = serde_json::json!({
        "template": "tpl with space",
        "directory": home.display().to_string(),
    });
    let out = deploy(&home, "caller", &args);
    let err = out["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("invalid template name"),
        "expected template-name rejection, got: {out}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_role_into_fleet_yaml() {
    // Role declared on a template instance must flow into fleet.yaml's
    // instances: block so pane_factory::create_pane_from_resolved can
    // render Identity/Role into the agent's agend.md. Before this PR,
    // template schema ignored role entirely and no fleet.yaml entry was
    // written on deploy.
    let home = tmp_home("role_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        role: orchestrator
      impl:
        backend: kiro-cli
        role: implementer
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({
        "template": "dev",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let lead = reloaded
        .instances
        .get("dev-lead")
        .expect("dev-lead must be persisted");
    assert_eq!(lead.role.as_deref(), Some("orchestrator"));
    let imp = reloaded
        .instances
        .get("dev-impl")
        .expect("dev-impl must be persisted");
    assert_eq!(imp.role.as_deref(), Some("implementer"));
    // Template block untouched by the instances: mutation.
    assert!(
        reloaded.templates.is_some(),
        "templates section must survive the write"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_accepts_description_alias_for_role() {
    // Mirror InstanceConfig's `#[serde(alias = "description")]`. Users
    // coming from the TS version write `description:` — accept both so
    // the schemas stay in sync.
    let home = tmp_home("role_alias");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        description: orchestrator via alias
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    assert_eq!(
        reloaded
            .instances
            .get("dev-lead")
            .and_then(|i| i.role.clone())
            .as_deref(),
        Some("orchestrator via alias")
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_instructions_into_fleet_yaml() {
    let home = tmp_home("instructions_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        instructions: ./instructions/lead.md
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    assert_eq!(
        reloaded
            .instances
            .get("dev-lead")
            .and_then(|i| i.instructions.as_deref()),
        Some("./instructions/lead.md")
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2104 (cheerc): template deployment must carry the template instance's
/// operator-controlled override fields — `github_login` AND `repo` — into the
/// deployed instance. Both were hardcoded `None` at the same site
/// (`create_instance_entries`), so a deployed fleet had NO github_login
/// mapping (→ `task_sweep` D002 false-fired) and lost any explicit `repo`
/// owner/name override (→ daemon fell back to source_repo derivation, wrong
/// for non-GitHub remotes / fork disambiguation).
#[test]
fn deploy_persists_github_login_and_repo_into_fleet_yaml() {
    let home = tmp_home("github_login_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      impl:
        backend: claude
        github_login: cheerc
        repo: cheerc/talented-payroll
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-impl").expect("dev-impl");
    assert_eq!(
        inst.github_login.as_deref(),
        Some("cheerc"),
        "deployed instance must carry the template's github_login (D002 false-fire root cause)"
    );
    assert_eq!(
        inst.repo.as_deref(),
        Some("cheerc/talented-payroll"),
        "deployed instance must carry the template's explicit repo owner/name override"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Sprint 56 Track E (#450): template params passthrough ──────────

#[test]
fn deploy_persists_args_into_fleet_yaml() {
    let home = tmp_home("args_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        args:
          - --resume
          - --model
          - opus
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
    assert_eq!(
        inst.args,
        vec![
            "--resume".to_string(),
            "--model".to_string(),
            "opus".to_string()
        ]
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_model_into_fleet_yaml() {
    let home = tmp_home("model_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      specialist:
        backend: claude
        model: opus
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded
        .instances
        .get("dev-specialist")
        .expect("dev-specialist");
    assert_eq!(inst.model.as_deref(), Some("opus"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_env_into_fleet_yaml() {
    let home = tmp_home("env_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        env:
          MCP_SERVER_URL: https://example.com
          DEBUG: "1"
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
    assert_eq!(
        inst.env.get("MCP_SERVER_URL").map(|s| s.as_str()),
        Some("https://example.com")
    );
    assert_eq!(inst.env.get("DEBUG").map(|s| s.as_str()), Some("1"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_ready_pattern_into_fleet_yaml() {
    let home = tmp_home("ready_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      worker:
        backend: claude
        ready_pattern: "ready for input"
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-worker").expect("dev-worker");
    assert_eq!(inst.ready_pattern.as_deref(), Some("ready for input"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_worktree_opt_out_into_fleet_yaml() {
    // reviewer / orchestrator roles often want `worktree: false` so
    // the worktree pool skips creation. Template passthrough must
    // preserve this signal.
    let home = tmp_home("worktree_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      reviewer:
        backend: claude
        worktree: false
      impl:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let reviewer = reloaded
        .instances
        .get("dev-reviewer")
        .expect("dev-reviewer");
    assert_eq!(
        reviewer.worktree,
        Some(false),
        "reviewer must round-trip `worktree: false`"
    );
    let imp = reloaded.instances.get("dev-impl").expect("dev-impl");
    assert!(
        imp.worktree.is_none(),
        "instance without `worktree:` field must stay None for default auto-create behavior"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_persists_command_into_fleet_yaml() {
    // `command:` template field — non-backend custom invocation.
    let home = tmp_home("command_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      script:
        backend: claude
        command: ./scripts/my-runner.sh
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-script").expect("dev-script");
    assert_eq!(
        inst.command.as_deref(),
        Some("./scripts/my-runner.sh"),
        "custom command must round-trip via the template passthrough"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_omits_template_params_when_not_set_backwards_compat() {
    // Critical backwards-compat invariant: existing templates that
    // declare none of args/model/env/ready_pattern/command/worktree
    // must continue to deploy unchanged. The fleet.yaml stanza for
    // the deployed instance must have NO sentinel values for those
    // fields — operator can't tell whether they were "passed
    // through as None" vs "never declared" otherwise.
    let home = tmp_home("compat_omit");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        role: orchestrator
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("dev-lead").expect("dev-lead");
    assert!(
        inst.args.is_empty(),
        "args must default to empty Vec when template doesn't declare it"
    );
    assert!(inst.model.is_none(), "model must stay None");
    assert!(inst.env.is_empty(), "env must default to empty HashMap");
    assert!(inst.ready_pattern.is_none(), "ready_pattern must stay None");
    assert!(
        inst.worktree.is_none(),
        "worktree must stay None (preserves default auto-create behavior)"
    );
    // `command` is a special case — the existing fallback at
    // deploy()'s line 142-146 reads `command` OR `backend` for the
    // SPAWN-time backend label; the template-passthrough path only
    // captures it when the operator explicitly declared `command:`.
    // A template with only `backend: claude` writes neither field
    // value into the durable command slot.
    assert!(
        inst.command.is_none(),
        "command must stay None when template only declared `backend:`"
    );
    assert!(
            inst.topic_binding_mode.is_none(),
            "#991: topic_binding_mode must stay None (auto default) when template doesn't declare `topic_binding`"
        );
    std::fs::remove_dir_all(&home).ok();
}

/// #991 PR-B: a deployment template's per-instance `topic_binding: skip`/
/// `deferred` must persist to fleet.yaml — mirrors the same filter
/// `create_instance` uses (mcp/handlers/instance_state/spawn.rs): only
/// "skip"/"deferred" persist, an invalid value is silently treated as
/// unset (auto default), same as an invalid `create_instance` call.
#[test]
fn deploy_persists_topic_binding_into_fleet_yaml() {
    let home = tmp_home("topic_binding_persist");
    let yaml = r#"
templates:
  dev:
    instances:
      quiet:
        backend: claude
        topic_binding: skip
      retrofit:
        backend: claude
        topic_binding: deferred
      bogus:
        backend: claude
        topic_binding: not_a_real_mode
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    assert_eq!(
        reloaded
            .instances
            .get("dev-quiet")
            .and_then(|i| i.topic_binding_mode.clone()),
        Some("skip".to_string())
    );
    assert_eq!(
        reloaded
            .instances
            .get("dev-retrofit")
            .and_then(|i| i.topic_binding_mode.clone()),
        Some("deferred".to_string())
    );
    assert_eq!(
        reloaded
            .instances
            .get("dev-bogus")
            .and_then(|i| i.topic_binding_mode.clone()),
        None,
        "an invalid topic_binding value must not persist — same as an invalid create_instance call"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_passes_all_seven_params_simultaneously() {
    // End-to-end pin: all passthrough fields round-trip through fleet.yaml.
    let home = tmp_home("all_six");
    let yaml = r#"
templates:
  full:
    instances:
      worker:
        backend: claude
        args:
          - --resume
        model: sonnet
        model_tier: cheap
        env:
          API_KEY_VAR: KEY
        ready_pattern: "now ready"
        command: my-runner
        worktree: false
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "full", "directory": home.display().to_string()}),
    );
    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let inst = reloaded.instances.get("full-worker").expect("full-worker");
    assert_eq!(inst.args, vec!["--resume".to_string()]);
    assert_eq!(inst.model.as_deref(), Some("sonnet"));
    assert_eq!(inst.model_tier.as_deref(), Some("cheap"));
    assert_eq!(inst.env.get("API_KEY_VAR").map(|s| s.as_str()), Some("KEY"));
    assert_eq!(inst.ready_pattern.as_deref(), Some("now ready"));
    assert_eq!(inst.command.as_deref(), Some("my-runner"));
    assert_eq!(inst.worktree, Some(false));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_omits_role_when_not_set() {
    // A template without `role:` must not write a blank role field —
    // empty Role lines in agend.md would mislead agents into thinking
    // "" is their role.
    let home = tmp_home("role_absent");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({"template": "dev", "directory": home.display().to_string()});
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let lead = reloaded.instances.get("dev-lead").expect("dev-lead");
    assert!(
        lead.role.is_none(),
        "unset role must stay None, got {:?}",
        lead.role
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_sets_orchestrator_from_template_suffix() {
    // Template nominates orchestrator by suffix; deploy must rewrite it
    // to the fully-prefixed name (`<deploy_name>-<suffix>`) before calling
    // teams::create, otherwise the member-of-team check rejects it.
    let home = tmp_home("orch_ok");
    let yaml = r#"
templates:
  dev:
    orchestrator: lead
    instances:
      lead:
        backend: claude
        role: orchestrator
      impl:
        backend: claude
        role: implementer
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );

    let orch = crate::teams::resolve_team_orchestrator(&home, "dev").expect("team dev must exist");
    assert_eq!(
        orch.as_deref(),
        Some("dev-lead"),
        "orchestrator suffix must be expanded to full name"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_ignores_unknown_orchestrator_suffix() {
    // Typo protection: a template pointing orchestrator at a non-existent
    // suffix must not fail the deploy. The team gets created without an
    // orchestrator — operator sees a warn log and can fix via update_team.
    let home = tmp_home("orch_typo");
    let yaml = r#"
templates:
  dev:
    orchestrator: captian   # typo — should be "captain" (or a real suffix)
    instances:
      lead:
        backend: claude
      impl:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );

    // resolve_team_orchestrator errors on degraded teams ("no orchestrator,
    // cannot route"), so probe via list and inspect the orchestrator field
    // directly — we want to assert the team exists but is orchestrator-less,
    // not prove routing works.
    let listed = crate::teams::list(&home);
    let team = listed["teams"]
        .as_array()
        .and_then(|ts| ts.iter().find(|t| t["name"] == "dev"))
        .cloned()
        .expect("team 'dev' must still be created");
    assert!(
        team["orchestrator"].is_null(),
        "unknown orchestrator suffix must leave team with no orchestrator, got {}",
        team["orchestrator"]
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_gives_each_member_its_own_workdir() {
    // Regression: same-backend teammates used to share `directory` when
    // no `branch:` was given, which made them clobber each other's
    // `.kiro/steering/agend.md` (and `.claude/mcp-config.json`) on
    // every respawn. Each member must land in `<directory>/<inst_name>`.
    let home = tmp_home("workdir_isolate");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
      impl-1:
        backend: kiro-cli
      impl-2:
        backend: kiro-cli
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let workdirs: std::collections::HashSet<String> = ["dev-lead", "dev-impl-1", "dev-impl-2"]
        .iter()
        .filter_map(|name| {
            reloaded
                .instances
                .get(*name)
                .and_then(|i| i.working_directory.clone())
        })
        .collect();

    assert_eq!(
        workdirs.len(),
        3,
        "every member must get a unique working_directory, got {workdirs:?}"
    );
    for name in ["dev-lead", "dev-impl-1", "dev-impl-2"] {
        let wd = reloaded
            .instances
            .get(name)
            .and_then(|i| i.working_directory.clone())
            .unwrap_or_default();
        assert!(
            wd.ends_with(name),
            "{name}'s working_directory must end with its own name, got {wd}"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_skips_bad_instance_suffix_but_keeps_good_ones() {
    let home = tmp_home("mixed_suffix");
    // Minimal fleet.yaml with one bad suffix and one good one.
    let yaml = r#"
defaults:
  cols: 80
  rows: 24
  layout: grid
templates:
  tpl:
    instances:
      "../etc":
        backend: claude
      good:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    // Point the daemon-less API call at a non-running daemon: `api::call`
    // just returns an error, but `deploy` itself only tracks the names it
    // accepted, so that's enough to verify filtering.
    let args = serde_json::json!({
        "template": "tpl",
        "directory": home.display().to_string(),
        "name": "dep",
    });
    let out = deploy(&home, "caller", &args);
    let instances = out["instances"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Vec<_>>();
    assert!(
        instances.iter().any(|n| n == "dep-good"),
        "good suffix dropped: {out}"
    );
    assert!(
        !instances.iter().any(|n| n.contains("..")),
        "bad suffix accepted: {out}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── Issue #456: teardown cleanup tests ───────────────────────────

#[test]
fn teardown_clears_configs_and_bindings() {
    let home = tmp_home("teardown_configs");
    let yaml = r#"
templates:
  dev:
    instances:
      agent:
        backend: claude
instances: {}
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({"template": "dev", "directory": home.display().to_string()}),
    );
    // Create binding (simulates active task).
    crate::binding::bind(&home, "dev-agent", "T-1", "feat");
    assert!(crate::binding::read(&home, "dev-agent").is_some());

    let _ = teardown(&home, &serde_json::json!({"name": "dev"}));

    // Binding should be cleared by cleanup_working_dir or DELETE.
    // Workspace dir should not exist.
    let workspace = crate::paths::workspace_dir(&home).join("dev-agent");
    assert!(!workspace.exists(), "workspace must be cleaned");
    std::fs::remove_dir_all(&home).ok();
}

// ── Issue #474: TUI close path bypassed teardown ──────────────────
//
// The TUI close overlay (`Ctrl-B x` / tab close) calls
// `fleet::remove_instance(s)_from_yaml` + `kill_agent` but doesn't
// touch `deployments.json`. Result: stale entries in `deployment list`
// after every TUI-triggered close. The fix wires
// `deployments::reconcile_after_close` into the same overlay code path
// (Option 1, auto-cleanup) and adds `reconcile_orphans` to the daemon
// boot path (Option 3, defensive sweep).
//
// These tests target the production reconcile function that the
// overlay calls — they exercise the same code path the TUI close
// overlay does, just without the ratatui input boilerplate.

/// Build a fleet.yaml + deploy a 1-instance template, returning the
/// home dir. Used by tests that need a baseline post-deploy state.
fn deploy_single_instance_for_test(tag: &str, deploy_name: &str) -> std::path::PathBuf {
    let home = tmp_home(tag);
    let yaml = r#"
templates:
  tpl:
    instances:
      worker:
        backend: claude
instances: {}
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({
            "template": "tpl",
            "name": deploy_name,
            "directory": home.display().to_string(),
        }),
    );
    home
}

/// #bughunt2: a deploy whose `deployments.json` save fails must surface an
/// error (instances are live but untracked), NOT report `status:deployed`.
#[test]
fn deploy_surfaces_record_save_failure_not_fake_deployed() {
    let home = tmp_home("deploy-save-fail");
    let yaml = r#"
templates:
  tpl:
    instances:
      worker:
        backend: claude
instances: {}
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    // Force the record save to fail: a DIRECTORY at the target path makes
    // atomic_write's final rename unable to replace it.
    std::fs::create_dir_all(home.join("deployments.json")).unwrap();
    let result = deploy(
        &home,
        "caller",
        &serde_json::json!({
            "template": "tpl",
            "name": "tpl",
            "directory": home.display().to_string(),
        }),
    );
    assert!(
        result.get("error").and_then(|e| e.as_str()).is_some(),
        "a failed record save must surface as an error, not status:deployed: {result}"
    );
    assert_ne!(
        result.get("status").and_then(|s| s.as_str()),
        Some("deployed"),
        "must NOT report deployed when the record was not persisted"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #bughunt2 (codex review): teardown's record-cleanup save-failure branch
/// must surface a stale-record error, not report `torn_down`. Unix-only: the
/// failure is injected by making `home` read-only AFTER deploy (the
/// `deployments.lock` already exists so the flock still opens, `load` still
/// reads the record, but the `atomic_write` tmp create in the read-only dir
/// fails the save).
#[cfg(unix)]
#[test]
fn teardown_surfaces_record_save_failure_not_fake_torn_down() {
    use std::os::unix::fs::PermissionsExt;
    let home = deploy_single_instance_for_test("teardown-save-fail", "tpl");
    assert!(
        load(&home).deployments.iter().any(|d| d.name == "tpl"),
        "pre: deployment must exist for the teardown to find"
    );
    let ro = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(&home, ro).unwrap();

    let result = teardown(&home, &serde_json::json!({"name": "tpl"}));

    // Restore write perms so cleanup (and any later test) works.
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        result.get("error").and_then(|e| e.as_str()).is_some(),
        "a failed record-cleanup save must surface an error, not torn_down: {result}"
    );
    assert_ne!(
        result.get("status").and_then(|s| s.as_str()),
        Some("torn_down"),
        "must NOT report torn_down when the record cleanup was not persisted"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn close_non_last_instance_keeps_deployment_intact() {
    // Multi-instance deployment: closing one of three keeps the
    // deployment entry intact — only when ALL members are gone does
    // the entry get pruned.
    let home = tmp_home("close_non_last");
    let yaml = r#"
templates:
  tpl:
    instances:
      a:
        backend: claude
      b:
        backend: claude
      c:
        backend: claude
instances: {}
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let _ = deploy(
        &home,
        "caller",
        &serde_json::json!({
            "template": "tpl",
            "name": "tpl",
            "directory": home.display().to_string(),
        }),
    );

    // Close the first instance only.
    let names: Vec<String> = vec!["tpl-a".to_string()];
    let _ = crate::fleet::remove_instances_from_yaml(&home, &names);
    let pruned = reconcile_after_close(&home, &names);
    assert!(
        pruned.is_empty(),
        "reconcile must NOT prune when 2/3 members remain, got {pruned:?}"
    );

    let store = load(&home);
    let entry = store
        .deployments
        .iter()
        .find(|d| d.name == "tpl")
        .expect("deployment must remain in store");
    // The deployment record's `instances` list is the deploy-time
    // snapshot; we don't shrink it as members leave. The only
    // invariant the lint protects is "entry survives if any member
    // still in fleet.yaml".
    assert_eq!(
        entry.instances.len(),
        3,
        "instances list unchanged: {entry:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #787: deploy backend/command field conflation ─────────────────

/// #787 §3.10 anchor — a template declaring BOTH `backend:` and
/// `command:` must preserve each field independently on deploy.
/// Pre-fix, the local `command` variable at deployments.rs:142
/// fell back to `inst_val.get("backend")` and then got written to
/// `InstanceYamlEntry.backend` at line 260, so the `command:` path
/// silently overwrote the `backend:` label.
///
/// RED on §3.10 RED: assertion fails because `backend` is
/// "/tmp/fake-proxy" (the command path) instead of "claude".
/// GREEN once the local `command` resolution is renamed to
/// `backend_label` and reads only the `backend:` key.
#[test]
fn deploy_template_with_backend_and_command_preserves_both_fields() {
    let home = tmp_home("backend_command_split");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: claude
        command: /tmp/fake-proxy
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({
        "template": "dev",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let lead = reloaded
        .instances
        .get("dev-lead")
        .expect("dev-lead must be persisted");

    assert_eq!(
        lead.backend.as_ref().map(|b| b.as_str()),
        Some("claude"),
        "backend field must hold the label, not the command path"
    );
    assert_eq!(
        lead.command.as_deref(),
        Some("/tmp/fake-proxy"),
        "command field must preserve the custom invocation path"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #787 back-compat invariant — a template with only `backend:`
/// (no `command:`) is the common case and must continue to write
/// the user-supplied backend label verbatim. Pins the post-fix
/// behavior so the rename refactor doesn't accidentally regress
/// the normal path.
#[test]
fn deploy_template_with_only_backend_persists_backend_label() {
    let home = tmp_home("only_backend");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        backend: kiro-cli
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({
        "template": "dev",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let lead = reloaded
        .instances
        .get("dev-lead")
        .expect("dev-lead must be persisted");

    assert_eq!(lead.backend.as_ref().map(|b| b.as_str()), Some("kiro-cli"));
    assert!(
        lead.command.is_none(),
        "no `command:` in template ⇒ `command` field must be None"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #787 — a template with ONLY `command:` (no explicit `backend:`)
/// must NOT smuggle the command path into the backend field. After
/// the fix, backend falls back to the "claude" default (which is
/// the same default rustup-init users get from `fleet new`); the
/// custom invocation lives in the `command:` field only.
///
/// This pins the behavior-change called out in the decision spec:
/// pre-fix `backend: <command-path>`, post-fix `backend: "claude"`.
#[test]
fn deploy_template_with_only_command_defaults_backend_to_claude() {
    let home = tmp_home("only_command");
    let yaml = r#"
templates:
  dev:
    instances:
      lead:
        command: /tmp/fake-proxy
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();

    let args = serde_json::json!({
        "template": "dev",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("reload fleet.yaml");
    let lead = reloaded
        .instances
        .get("dev-lead")
        .expect("dev-lead must be persisted");

    assert_eq!(
            lead.backend.as_ref().map(|b| b.as_str()),
            Some("claude"),
            "no explicit backend ⇒ default to claude (label); command path must NOT smuggle into backend"
        );
    assert_eq!(lead.command.as_deref(), Some("/tmp/fake-proxy"));

    std::fs::remove_dir_all(&home).ok();
}

/// #1320: deploy without directory falls back to $AGEND_HOME/workspace/<deploy_name>/.
#[test]
fn deploy_defaults_directory_to_workspace_deploy_name() {
    let home = tmp_home("dir_default");
    let yaml = r#"
templates:
  svc:
    instances:
      worker:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
    });
    let out = deploy(&home, "caller", &args);
    assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
    let store = load(&home);
    let dep = store.deployments.iter().find(|d| d.name == "svc");
    if let Some(dep) = dep {
        let expected = crate::paths::workspace_dir(&home)
            .join("svc")
            .display()
            .to_string();
        assert_eq!(
            dep.directory, expected,
            "#1320: default dir must be $AGEND_HOME/workspace/<deploy_name>"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #1320: template-level directory takes effect when args omits it.
#[test]
fn deploy_reads_template_directory_field() {
    let home = tmp_home("dir_tpl");
    let yaml = r#"
templates:
  svc:
    directory: /tmp/custom-workspace
    instances:
      worker:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
    });
    let out = deploy(&home, "caller", &args);
    assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
    let store = load(&home);
    let dep = store.deployments.iter().find(|d| d.name == "svc");
    if let Some(dep) = dep {
        assert_eq!(
            dep.directory, "/tmp/custom-workspace",
            "#1320: template directory must take effect"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

/// #1320: explicit args directory still wins over template and default.
#[test]
fn deploy_args_directory_overrides_template_and_default() {
    let home = tmp_home("dir_override");
    let yaml = r#"
templates:
  svc:
    directory: /tmp/template-dir
    instances:
      worker:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
        "directory": "/tmp/explicit-dir",
    });
    let out = deploy(&home, "caller", &args);
    assert_ne!(out.get("error"), None.or(Some(&serde_json::Value::Null)),);
    let store = load(&home);
    let dep = store.deployments.iter().find(|d| d.name == "svc");
    if let Some(dep) = dep {
        assert_eq!(
            dep.directory, "/tmp/explicit-dir",
            "#1320: explicit args directory must win"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_propagates_template_source_repo_to_instances() {
    let home = tmp_home("tpl_source_repo");
    let yaml = r#"
templates:
  svc:
    source_repo: /repos/my-project
    instances:
      lead:
        backend: claude
      dev:
        backend: kiro-cli
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
    assert_eq!(
        lead.source_repo.as_deref(),
        Some("/repos/my-project"),
        "template source_repo must propagate to instances"
    );
    let dev = reloaded.instances.get("svc-dev").expect("svc-dev");
    assert_eq!(
        dev.source_repo.as_deref(),
        Some("/repos/my-project"),
        "template source_repo must propagate to all instances"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_instance_source_repo_overrides_template() {
    let home = tmp_home("inst_override_sr");
    let yaml = r#"
templates:
  svc:
    source_repo: /repos/default
    instances:
      lead:
        backend: claude
        source_repo: /repos/override
      dev:
        backend: kiro-cli
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
    assert_eq!(
        lead.source_repo.as_deref(),
        Some("/repos/override"),
        "instance source_repo must override template"
    );
    let dev = reloaded.instances.get("svc-dev").expect("svc-dev");
    assert_eq!(
        dev.source_repo.as_deref(),
        Some("/repos/default"),
        "instance without override inherits template"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_no_source_repo_stays_none() {
    let home = tmp_home("no_source_repo");
    let yaml = r#"
templates:
  svc:
    instances:
      lead:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let reloaded = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let lead = reloaded.instances.get("svc-lead").expect("svc-lead");
    assert_eq!(
        lead.source_repo, None,
        "no source_repo in template or instance must remain None"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn deploy_propagates_template_source_repo_to_team() {
    let home = tmp_home("tpl_sr_team");
    let yaml = r#"
templates:
  svc:
    source_repo: /repos/team-project
    instances:
      lead:
        backend: claude
      dev:
        backend: kiro-cli
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "svc",
        "directory": home.display().to_string(),
    });
    let _ = deploy(&home, "caller", &args);

    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
    let team = fleet.teams.get("svc").expect("team 'svc' must exist");
    assert_eq!(
        team.source_repo.as_ref().map(|p| p.display().to_string()),
        Some("/repos/team-project".to_string()),
        "template source_repo must propagate to team"
    );
    std::fs::remove_dir_all(&home).ok();
}

// #1629 invariant (#1617 lock-while-blocking class): the deployment-store
// flock must be acquired AFTER the loopback `api::call`s (SPAWN/CREATE_TEAM
// in deploy, DELETE in teardown), never around them — a self-IPC held under
// that flock deadlocks (the loopback handler needs the registry lock). These
// structural source-scans slice each fn and assert the api::call site index
// precedes the `acquire_file_lock` index. Prod-sliced + `concat` needles so
// they can't self-satisfy.
fn prod_src() -> &'static str {
    let src = include_str!("../deployments.rs");
    let cfg_test = ["#[cfg(", "test)]"].concat();
    match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => src,
    }
}

fn fn_body<'a>(prod: &'a str, sig: &str) -> &'a str {
    let start = prod.find(sig).expect("fn present");
    let rest = &prod[start + sig.len()..];
    let end = rest.find("\npub fn ").unwrap_or(rest.len());
    &prod[start..start + sig.len() + end]
}

#[test]
fn deploy_api_calls_not_under_flock() {
    let prod = prod_src();
    // #2764 R10: the transaction body moved to `deploy_impl` (spawn seam).
    let body = fn_body(prod, "pub(crate) fn deploy_impl(");
    // H14: deploy's duplicate-name guard is a plain `load()` READ (no flock)
    // before spawn — #1629 forbids holding ANY flock across the self-IPC
    // spawn/team. So the ONLY `acquire_file_lock` in deploy is still the store
    // flock, and it must come AFTER spawn_instances/create_deployment_team.
    let lock_at = body
        .find(&["acquire_file", "_lock"].concat())
        .expect("deploy locks the store save");
    let spawn_at = body
        .find("spawn_instances_impl(")
        .expect("deploy spawns instances");
    let team_at = body
        .find("create_deployment_team(")
        .expect("deploy creates the team");
    assert!(
        spawn_at < lock_at,
        "spawn_instances (api::call SPAWN) must run BEFORE the deployment flock (#1617 class)"
    );
    assert!(
            team_at < lock_at,
            "create_deployment_team (api::call CREATE_TEAM) must run BEFORE the deployment flock (#1617 class)"
        );
}

// ── #2764 D: deployment teardown/reconcile mutation embargo ────────────────

/// teardown performs ZERO destructive/authority mutation: it records a durable
/// cleanup-pending/audit entry and returns `torn_down:false, cleanup_pending:true`
/// — the deployment record, fleet entries, and directories all remain.
#[test]
fn teardown_is_embargoed_records_pending_mutates_nothing() {
    let home = deploy_single_instance_for_test("d_embargo_td", "emb");
    let inst = "emb-worker";
    // Pre: the deployment record + fleet entry + subdir all exist.
    assert!(load(&home).deployments.iter().any(|d| d.name == "emb"));
    let fleet_before = std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap();
    assert!(fleet_before.contains(inst), "pre: fleet entry present");
    let subdir = home.join(inst);
    assert!(subdir.exists(), "pre: deploy subdir exists");

    let resp = teardown(&home, &serde_json::json!({"name": "emb"}));

    assert_eq!(resp["torn_down"], false);
    assert_eq!(resp["cleanup_pending"], true);
    assert_eq!(
        resp["reason"],
        "ownership_safe_teardown_temporarily_unavailable"
    );
    // Zero mutation: record kept, fleet entry kept, subdir kept.
    assert!(
        load(&home).deployments.iter().any(|d| d.name == "emb"),
        "deployment record must NOT be removed"
    );
    assert_eq!(
        std::fs::read_to_string(crate::fleet::fleet_yaml_path(&home)).unwrap(),
        fleet_before,
        "fleet.yaml must be byte-identical (no instance removal)"
    );
    assert!(subdir.exists(), "deploy subdir must NOT be removed");
    // The pending/audit ledger was written with the deployment identity.
    let ledger = std::fs::read_to_string(home.join("deploy-cleanup-pending.jsonl")).unwrap();
    assert!(
        ledger.contains("\"emb\"") && ledger.contains("\"source\":\"teardown\""),
        "pending ledger must record the deployment identity, got: {ledger}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// The cleanup-pending ledger is idempotent per generation: teardown + repeated
/// reconcile for the SAME deployment generation record exactly ONE entry.
#[test]
fn deployment_pending_ledger_is_idempotent_per_generation() {
    let home = deploy_single_instance_for_test("d_embargo_idem", "idem");
    // Remove the instance from fleet.yaml so reconcile sees an orphan.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "templates:\n  tpl:\n    instances:\n      worker:\n        backend: claude\ninstances: {}\n",
    )
    .unwrap();

    let _ = teardown(&home, &serde_json::json!({"name": "idem"}));
    // Reconcile several times — each is a no-op prune and an idempotent record.
    let r1 = reconcile_orphans(&home);
    let r2 = reconcile_orphans(&home);
    assert!(
        r1.is_empty() && r2.is_empty(),
        "reconcile must prune NOTHING under D"
    );

    let ledger = std::fs::read_to_string(home.join("deploy-cleanup-pending.jsonl")).unwrap();
    let count = ledger.lines().filter(|l| l.contains("\"idem\"")).count();
    assert_eq!(count, 1, "one entry per generation, got {count}:\n{ledger}");
    // The orphan deployment record is retained (reconcile prunes nothing).
    assert!(
        load(&home).deployments.iter().any(|d| d.name == "idem"),
        "reconcile must NOT prune the deployment record under D"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2764 R10 (item 2) RED: EVERY member spawn fails → full rollback (no
/// fleet entries, created dirs removed), NO team, NO deployment record, loud
/// error.
#[test]
fn deploy_total_spawn_failure_rolls_back_everything_2764_r10() {
    let home = tmp_home("r10-total-fail");
    let yaml = r#"
templates:
  duo:
    instances:
      a:
        backend: claude
      b:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "duo",
        "directory": home.display().to_string(),
    });

    let out = super::deploy_impl(&home, "caller", &args, &|_h, _req| {
        Ok(serde_json::json!({"ok": false, "error": "spawn boom"}))
    });

    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("every member spawn failed"),
        "total failure must be a loud error: {out}"
    );
    assert!(
        super::load(&home).deployments.is_empty(),
        "NO deployment record may persist after total spawn failure"
    );
    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("fleet.yaml parses");
    assert!(
        !cfg.instances.contains_key("duo-a") && !cfg.instances.contains_key("duo-b"),
        "fleet entries must be rolled back, got {:?}",
        cfg.instances.keys().collect::<Vec<_>>()
    );
    assert!(
        cfg.teams.is_empty(),
        "no team may be created after total spawn failure"
    );
    assert!(
        !home.join("duo-a").exists() && !home.join("duo-b").exists(),
        "created member dirs must be rolled back"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2764 R10 (item 2) RED: PARTIAL spawn failure → failed member rolled back
/// (entry + created dir), NO team, deployment record persisted as the LOUD
/// recovery record (spawn_failures listed, instances = survivors only),
/// response is NOT "deployed".
#[test]
fn deploy_partial_spawn_failure_records_recovery_no_team_2764_r10() {
    let home = tmp_home("r10-partial-fail");
    let yaml = r#"
templates:
  duo:
    instances:
      a:
        backend: claude
      b:
        backend: claude
"#;
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    let args = serde_json::json!({
        "template": "duo",
        "directory": home.display().to_string(),
    });

    let out = super::deploy_impl(&home, "caller", &args, &|_h, req| {
        let name = req["params"]["name"].as_str().unwrap_or_default();
        if name == "duo-b" {
            Ok(serde_json::json!({"ok": false, "error": "b refused"}))
        } else {
            Ok(serde_json::json!({"ok": true, "result": {}}))
        }
    });

    assert_eq!(
        out["status"].as_str(),
        Some("partial_spawn_failure"),
        "partial failure must NEVER report deployed: {out}"
    );
    let store = super::load(&home);
    let rec = store
        .deployments
        .iter()
        .find(|d| d.name == "duo")
        .expect("the loud recovery record must persist");
    assert_eq!(
        rec.instances,
        vec!["duo-a".to_string()],
        "record.instances must list survivors only"
    );
    assert_eq!(
        rec.spawn_failures,
        vec!["duo-b".to_string()],
        "spawn_failures must name the failed member"
    );
    assert!(
        rec.team.is_none(),
        "no team may be created past a spawn failure"
    );
    let cfg = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home))
        .expect("fleet.yaml parses");
    assert!(
        !cfg.instances.contains_key("duo-b"),
        "the failed member's fleet entry must be rolled back"
    );
    assert!(
        cfg.instances.contains_key("duo-a"),
        "the surviving member's entry stays"
    );
    assert!(cfg.teams.is_empty(), "no team in fleet.yaml either");
    assert!(
        !home.join("duo-b").exists(),
        "the failed member's created dir must be rolled back"
    );
    std::fs::remove_dir_all(&home).ok();
}
