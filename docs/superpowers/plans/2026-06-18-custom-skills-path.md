# Custom Per-Instance Skills Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the ability to configure different skill paths for different agent instances via `fleet.yaml` (and templates), enabling modular, decoupled skills for different projects.

**Architecture:** We will add a `skills_path` field to `InstanceConfig` and `InstanceYamlEntry`, ensure the deployment processor copies this field from templates, modify the skill installation backend to support a custom source path, and update all spawn/recovery choke points to load, resolve, and pass this path.

**Tech Stack:** Rust (standard library, serde, serde_yaml_ng)

---

### Task 1: Add Configuration Schema & YAML Mapping

**Files:**
- Modify: `src/fleet/mod.rs`
- Modify: `src/fleet/merge.rs`
- Test: `src/fleet/mod.rs` (Add test case in mod tests)

- [ ] **Step 1: Modify `InstanceConfig` in `src/fleet/mod.rs`**
  Add the `skills_path` field to `InstanceConfig` right after `skills`:
  ```rust
      /// Custom skills path override. When present, the daemon pulls skills
      /// from this path instead of `<home>/skills`.
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub skills_path: Option<String>,
  ```

- [ ] **Step 2: Modify `InstanceYamlEntry` in `src/fleet/mod.rs`**
  Add the `skills_path` field to `InstanceYamlEntry` right after `topic_binding_mode`:
  ```rust
      /// Custom skills path override.
      pub skills_path: Option<String>,
  ```

- [ ] **Step 3: Modify `build_instance_mapping` in `src/fleet/merge.rs`**
  Include the `skills_path` key in `build_instance_mapping` list:
  ```rust
          ("skills_path", &config.skills_path),
  ```

- [ ] **Step 4: Add round-trip test to `src/fleet/mod.rs`**
  Add a new test `add_instance_to_yaml_round_trips_skills_path` in `mod tests`:
  ```rust
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
  ```

- [ ] **Step 5: Run tests to verify**
  Run: `cargo test --lib fleet::tests`
  Expected: PASS

- [ ] **Step 6: Commit**
  ```bash
  git add src/fleet/mod.rs src/fleet/merge.rs
  git commit -m "feat(fleet): add skills_path config schema and YAML mapping"
  ```

---

### Task 2: Update Template Deployer

**Files:**
- Modify: `src/deployments.rs`

- [ ] **Step 1: Modify `create_instance_entries` in `src/deployments.rs`**
  Populate `skills_path` in the `InstanceYamlEntry` builder inside the template deployment loop:
  ```rust
                  skills_path: yaml_str(inst_val, "skills_path"),
  ```

- [ ] **Step 2: Commit**
  ```bash
  git add src/deployments.rs
  git commit -m "feat(deploy): copy skills_path from templates during deployment"
  ```

---

### Task 3: Support Custom Source in Skills Installer

**Files:**
- Modify: `src/skills.rs`
- Test: `src/skills.rs` (Add test case in mod tests)

- [ ] **Step 1: Modify `install_for_agent_backend` in `src/skills.rs`**
  Create `install_for_agent_backend_with_source` and delegate `install_for_agent_backend` to it:
  ```rust
  pub fn install_for_agent_backend(
      home: &Path,
      working_dir: &Path,
      filter: Option<&[String]>,
      backend: Option<&str>,
  ) -> Result<Vec<InstallOutcome>> {
      install_for_agent_backend_with_source(home, working_dir, filter, backend, None)
  }

  pub fn install_for_agent_backend_with_source(
      home: &Path,
      working_dir: &Path,
      filter: Option<&[String]>,
      backend: Option<&str>,
      custom_source: Option<&Path>,
  ) -> Result<Vec<InstallOutcome>> {
      let source = match custom_source {
          Some(path) => path.to_path_buf(),
          None => ensure_skills_root(home)?,
      };
      let staged_source = match filter {
          None => source,
          Some(allowlist) => stage_filtered_source(home, &source, allowlist)?,
      };
      let dirs: Vec<_> = BACKEND_SKILL_DIRS
          .iter()
          .filter(|(name, _)| backend.is_none_or(|b| *name == b))
          .collect();
      let mut outcomes = Vec::with_capacity(dirs.len());
      for (name, rel) in dirs {
          let target = working_dir.join(rel);
          let outcome = install_one(&staged_source, &target, name);
          outcomes.push(outcome);
      }
      Ok(outcomes)
  }
  ```

- [ ] **Step 2: Add test to `src/skills.rs`**
  Add `install_for_agent_backend_with_source_links_from_custom_path` to `mod tests`:
  ```rust
      #[test]
      fn install_for_agent_backend_with_source_links_from_custom_path() {
          let home = tmp_home("custom-src-home");
          let working = home.join("working");
          let custom_src = home.join("custom_skills");
          seed_skill_source(&custom_src, "custom-skill");
          
          let outcomes = install_for_agent_backend_with_source(
              &home,
              &working,
              None,
              None,
              Some(&custom_src),
          ).unwrap();
          
          assert!(!outcomes.is_empty());
          for outcome in outcomes {
              assert_eq!(outcome.mode, InstallMode::Symlink);
              assert!(outcome.target.join("custom-skill").join("SKILL.md").exists());
          }
          std::fs::remove_dir_all(&home).ok();
      }
  ```

- [ ] **Step 3: Run tests to verify**
  Run: `cargo test --lib skills::tests`
  Expected: PASS

- [ ] **Step 4: Commit**
  ```bash
  git add src/skills.rs
  git commit -m "feat(skills): support custom source path in skills installer"
  ```

---

### Task 4: Hook Up Callers

**Files:**
- Modify: `src/agent_ops.rs`
- Modify: `src/app/pane_factory.rs`
- Modify: `src/daemon/mod.rs`
- Modify: `src/daemon/crash_respawn.rs`

- [ ] **Step 1: Modify `src/agent_ops.rs`**
  Load `skills_path` from fleet config, resolve it via `expand_tilde_path`, and pass it to `install_for_agent_backend_with_source`:
  ```rust
      let custom_skills_source: Option<std::path::PathBuf> =
          crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
              .ok()
              .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
              .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
  ```
  Pass `custom_skills_source.as_deref()` as the fifth parameter to `install_for_agent_backend_with_source`.

- [ ] **Step 2: Modify `src/app/pane_factory.rs`**
  Extract `custom_skills_source` from fleet config and pass it:
  ```rust
          let custom_skills_source: Option<std::path::PathBuf> =
              crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                  .ok()
                  .and_then(|c| c.instances.get(&name).and_then(|i| i.skills_path.clone()))
                  .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
  ```
  Pass `custom_skills_source.as_deref()` as the fifth parameter to `install_for_agent_backend_with_source`.

- [ ] **Step 3: Modify `src/daemon/mod.rs` (around line 1829)**
  Extract `custom_skills_source` and pass it:
  ```rust
          let custom_skills_source: Option<std::path::PathBuf> =
              crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                  .ok()
                  .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
                  .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
  ```
  Pass `custom_skills_source.as_deref()` as the fifth parameter to `install_for_agent_backend_with_source`.

- [ ] **Step 4: Modify `src/daemon/mod.rs` (around line 2043)**
  Extract `custom_skills_source` and pass it:
  ```rust
          let custom_skills_source: Option<std::path::PathBuf> =
              crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                  .ok()
                  .and_then(|c| c.instances.get(name).and_then(|i| i.skills_path.clone()))
                  .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
  ```
  Pass `custom_skills_source.as_deref()` as the fifth parameter to `install_for_agent_backend_with_source`.

- [ ] **Step 5: Modify `src/daemon/crash_respawn.rs`**
  Extract `custom_skills_source` and pass it:
  ```rust
          let custom_skills_source: Option<std::path::PathBuf> =
              crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                  .ok()
                  .and_then(|c| c.instances.get(&config.name).and_then(|i| i.skills_path.clone()))
                  .map(|p| crate::fleet::resolve::expand_tilde_path(&p));
  ```
  Pass `custom_skills_source.as_deref()` as the fifth parameter to `install_for_agent_backend_with_source`.

- [ ] **Step 6: Run all tests**
  Run: `cargo test`
  Expected: PASS

- [ ] **Step 7: Commit**
  ```bash
  git add src/agent_ops.rs src/app/pane_factory.rs src/daemon/mod.rs src/daemon/crash_respawn.rs
  git commit -m "feat(skills): hook up all spawn/respawn entry points with custom skills_path"
  ```
