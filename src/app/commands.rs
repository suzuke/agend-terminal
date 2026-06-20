//! Command-palette command execution (`:spawn`, `:kill`, `:restart`, `:layout`,
//! `:send`, `:broadcast`, `:status`, `:config`, `:set`).
//!
//! Input is a whitespace-split command line. Returns `true` iff the command
//! mutated the layout in a way that requires a resize pass.

use crate::agent;
use crate::agent::AgentRegistry;
use crate::layout::{Layout, SplitDir, Tab};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Bundle of mutable references the command handler needs to affect the
/// running TUI state. Constructed by the caller for each invocation.
pub(super) struct CommandCtx<'a> {
    pub layout: &'a mut Layout,
    pub registry: &'a AgentRegistry,
    pub home: &'a Path,
    pub wakeup_tx: &'a crossbeam_channel::Sender<usize>,
    pub name_counter: &'a mut HashMap<String, usize>,
    pub telegram_state: &'a Option<Arc<dyn crate::channel::Channel>>,
}

/// #t-5 command-palette completion: declarative metadata for the `:` commands —
/// keyword + usage + one-line description. Consumed ONLY by the completion list
/// (`matching_specs` + `render::overlay::render_command_palette`); `execute`
/// below stays the source of truth for BEHAVIOR and is unchanged. ⚠ Adding or
/// renaming a command in `execute` REQUIRES a matching entry here — the
/// `command_specs_match_execute_arms_bidirectional` test fails otherwise.
pub(crate) struct CommandSpec {
    pub keyword: &'static str,
    pub usage: &'static str,
    pub desc: &'static str,
}

pub(crate) const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        keyword: "spawn",
        usage: "spawn [name] [backend]",
        desc: "New agent in a new tab",
    },
    CommandSpec {
        keyword: "vsplit",
        usage: "vsplit [name] [backend]",
        desc: "New agent; split focused pane vertically",
    },
    CommandSpec {
        keyword: "hsplit",
        usage: "hsplit [name] [backend]",
        desc: "New agent; split focused pane horizontally",
    },
    CommandSpec {
        keyword: "kill",
        usage: "kill <agent>",
        desc: "Close an agent's pane",
    },
    CommandSpec {
        keyword: "restart",
        usage: "restart [agent]",
        desc: "Restart an agent (focused pane if omitted)",
    },
    CommandSpec {
        keyword: "layout",
        usage: "layout [preset]",
        desc: "Apply a layout preset (cycles if omitted)",
    },
    CommandSpec {
        keyword: "send",
        usage: "send <agent> <message>",
        desc: "Send a message to one agent",
    },
    CommandSpec {
        keyword: "broadcast",
        usage: "broadcast <message>",
        desc: "Broadcast a message to all agents",
    },
    CommandSpec {
        keyword: "status",
        usage: "status",
        desc: "Log every agent's current state",
    },
    CommandSpec {
        keyword: "config",
        usage: "config get|set|list [key] [value]",
        desc: "Runtime config get / set / list",
    },
    CommandSpec {
        keyword: "set",
        usage: "set <key> <value>",
        desc: "Set a runtime config key (`config set` shorthand)",
    },
];

/// The command specs whose keyword (first token) starts with `input`'s first
/// token — the palette's prefix-match completion candidates. Empty/whitespace
/// input matches ALL (list-on-open). Keyword-only (P0); argument-value completion
/// (backend / agent / config-key / preset names) is a follow-up.
pub(crate) fn matching_specs(input: &str) -> Vec<&'static CommandSpec> {
    let first = input.split_whitespace().next().unwrap_or("");
    COMMAND_SPECS
        .iter()
        .filter(|s| s.keyword.starts_with(first))
        .collect()
}

/// Execute a command palette command. Returns true if layout changed (needs resize).
///
/// ⚠ Adding/renaming a command keyword here? Sync `COMMAND_SPECS` above (the
/// completion list) — `command_specs_match_execute_arms_bidirectional` asserts the
/// two sets are exactly equal (both directions).
pub(super) fn execute(cmd: &str, ctx: &mut CommandCtx<'_>) -> bool {
    let parts: Vec<&str> = cmd.trim().splitn(3, ' ').collect();
    if parts.is_empty() {
        return false;
    }
    match parts[0] {
        "spawn" | "vsplit" | "hsplit" => {
            let base_name = parts.get(1).unwrap_or(&"agent");
            let backend_name = parts.get(2).unwrap_or(&"claude");
            let fleet_path = crate::fleet::fleet_yaml_path(ctx.home);
            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
            let pc = cols.saturating_sub(2);
            let pr = rows.saturating_sub(4);

            // unique_fleet_name guarantees inst_name is not yet in fleet.yaml
            let inst_name = super::pane_factory::unique_fleet_name(ctx.home, base_name);
            // #966: palette spawn previously bypassed channel-topic creation
            // (`maybe_create_telegram_topic` at the `Ok(pane)` arm below
            // creates the BINDING via create_binding, NOT the topic_id-
            // returning create_topic). Route through
            // `tui_spawn::add_instance_with_topic` so the topic is created
            // + topic_id persisted before the spawn proceeds.
            if let Err(e) = super::tui_spawn::add_instance_with_topic(
                ctx.home,
                &inst_name,
                &crate::fleet::InstanceYamlEntry {
                    backend: Some(backend_name.to_string()),
                    ..Default::default()
                },
            ) {
                tracing::warn!(name = %inst_name, error = %e, "failed to write fleet.yaml");
            }
            let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
            let pane_result = if let Some(resolved) =
                fleet.as_ref().and_then(|f| f.resolve_instance(&inst_name))
            {
                super::pane_factory::create_pane_from_resolved(
                    &inst_name,
                    &resolved,
                    ctx.layout,
                    ctx.registry,
                    ctx.home,
                    pc,
                    pr,
                    ctx.wakeup_tx,
                    ctx.name_counter,
                    crate::backend::SpawnMode::Fresh,
                )
            } else {
                let (command, submit_key) = super::pane_factory::resolve_backend(backend_name);
                super::pane_factory::create_pane(
                    ctx.layout,
                    ctx.registry,
                    ctx.home,
                    &inst_name,
                    &command,
                    &[],
                    crate::backend::SpawnMode::Fresh,
                    None,
                    &HashMap::new(),
                    &submit_key,
                    pc,
                    pr,
                    ctx.wakeup_tx,
                    ctx.name_counter,
                    super::pane_factory::SpawnIdentity::Managed,
                )
            };
            match pane_result {
                Ok(pane) => {
                    super::telegram_hooks::maybe_create_telegram_topic(
                        ctx.telegram_state,
                        ctx.registry,
                        ctx.home,
                        &pane,
                    );
                    match parts[0] {
                        "vsplit" => {
                            if let Some(tab) = ctx.layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Vertical, pane);
                            }
                        }
                        "hsplit" => {
                            if let Some(tab) = ctx.layout.active_tab_mut() {
                                tab.split_focused(SplitDir::Horizontal, pane);
                            }
                        }
                        _ => {
                            let tab_name = pane.agent_name.clone();
                            ctx.layout.add_tab(Tab::new(tab_name.to_string(), pane));
                        }
                    }
                    return true;
                }
                Err(e) => {
                    tracing::error!(name = %inst_name, backend = *backend_name, error = %e, "spawn failed")
                }
            }
        }
        "kill" => {
            if let Some(name) = parts.get(1) {
                if let Some(fleet_name) = lookup_fleet_name(ctx.layout, name) {
                    super::telegram_hooks::maybe_delete_telegram_topic(
                        ctx.telegram_state,
                        ctx.home,
                        &fleet_name,
                    );
                    let _ = crate::fleet::remove_instance_from_yaml(ctx.home, &fleet_name);
                }
                super::kill_agent(ctx.home, ctx.registry, name);
                super::tui_events::remove_agent_pane(name, ctx.layout);
                return true;
            }
        }
        "restart" => {
            let target_name = parts.get(1).map(|s| s.to_string()).or_else(|| {
                ctx.layout
                    .active_tab()
                    .and_then(|t| t.focused_pane())
                    .map(|p| p.agent_name.to_string())
            });
            if let Some(name) = target_name {
                // Single pass: find pane info, fleet name, and location
                #[allow(clippy::type_complexity)]
                let mut pane_info: Option<(
                    String,
                    Option<PathBuf>,
                    Option<String>,
                    Option<String>,
                )> = None;
                let mut pane_loc: Option<(usize, usize)> = None;
                'outer: for (ti, tab) in ctx.layout.tabs.iter().enumerate() {
                    for id in tab.root().pane_ids() {
                        if let Some(p) = tab.root().find_pane(id) {
                            if p.agent_name.as_str() == name {
                                let cmd = match &p.backend {
                                    Some(b) => b.preset().command.to_string(),
                                    None => {
                                        tracing::warn!(agent = name, "cannot restart shell pane");
                                        break 'outer;
                                    }
                                };
                                pane_info = Some((
                                    cmd,
                                    p.working_dir.clone(),
                                    p.display_name.clone(),
                                    p.fleet_instance_name.clone(),
                                ));
                                pane_loc = Some((ti, id));
                                break 'outer;
                            }
                        }
                    }
                }

                if let Some((backend_cmd, work_dir, display_name, fleet_name)) = pane_info {
                    super::kill_agent(ctx.home, ctx.registry, &name);

                    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
                    let pc = cols.saturating_sub(2);
                    let pr = rows.saturating_sub(4);
                    ctx.name_counter.remove(&name);

                    let pane_result = if let Some(ref fname) = fleet_name {
                        // Fleet agent — resolve from fleet.yaml (full config)
                        let fleet_path = crate::fleet::fleet_yaml_path(ctx.home);
                        let fleet = crate::fleet::FleetConfig::load(&fleet_path).ok();
                        if let Some(resolved) =
                            fleet.as_ref().and_then(|f| f.resolve_instance(fname))
                        {
                            super::pane_factory::create_pane_from_resolved(
                                fname,
                                &resolved,
                                ctx.layout,
                                ctx.registry,
                                ctx.home,
                                pc,
                                pr,
                                ctx.wakeup_tx,
                                ctx.name_counter,
                                crate::backend::SpawnMode::Resume,
                            )
                        } else {
                            let (command, submit_key) =
                                super::pane_factory::resolve_backend(&backend_cmd);
                            super::pane_factory::create_pane(
                                ctx.layout,
                                ctx.registry,
                                ctx.home,
                                &name,
                                &command,
                                &[],
                                // Fleet resolve failed — no resume metadata,
                                // so start fresh rather than guess.
                                crate::backend::SpawnMode::Fresh,
                                work_dir.as_deref(),
                                &HashMap::new(),
                                &submit_key,
                                pc,
                                pr,
                                ctx.wakeup_tx,
                                ctx.name_counter,
                                super::pane_factory::SpawnIdentity::Managed,
                            )
                        }
                    } else {
                        let (command, submit_key) =
                            super::pane_factory::resolve_backend(&backend_cmd);
                        super::pane_factory::create_pane(
                            ctx.layout,
                            ctx.registry,
                            ctx.home,
                            &name,
                            &command,
                            &[],
                            crate::backend::SpawnMode::Fresh,
                            work_dir.as_deref(),
                            &HashMap::new(),
                            &submit_key,
                            pc,
                            pr,
                            ctx.wakeup_tx,
                            ctx.name_counter,
                            super::pane_factory::SpawnIdentity::Managed,
                        )
                    };
                    if let Ok(new_pane) = pane_result {
                        // Swap only vterm + rx into the existing pane slot
                        if let Some((ti, pid)) = pane_loc {
                            if let Some(pane) = ctx.layout.tabs[ti].root_mut().find_pane_mut(pid) {
                                let old_id = pane.id;
                                let old_selection = pane.selection.clone();
                                let old_last_input = pane.last_input_at;

                                *pane = new_pane;

                                pane.id = old_id;
                                pane.selection = old_selection;
                                pane.last_input_at = old_last_input;
                                pane.display_name = display_name;
                                pane.scroll_offset = 0;
                                pane.has_notification = false;
                                return true;
                            }
                        }
                        // Fallback: add as new tab
                        let tab_name = new_pane.agent_name.clone();
                        ctx.layout.add_tab(Tab::new(tab_name.to_string(), new_pane));
                        return true;
                    }
                }
            }
        }
        "layout" => {
            let Some(tab) = ctx.layout.active_tab_mut() else {
                return false;
            };
            let Some(name) = parts.get(1) else {
                tab.next_layout();
                return true;
            };
            let Some(preset) = crate::layout::LayoutPreset::from_name(name) else {
                tracing::warn!(
                    name = *name,
                    valid = crate::layout::LayoutPreset::all_names(),
                    "unknown layout preset"
                );
                return false;
            };
            tab.apply_layout(preset);
            return true;
        }
        "send" => {
            if parts.len() >= 3
                && !agent::send_to_registry(ctx.registry, ctx.home, "user", parts[1], parts[2])
            {
                tracing::warn!(target = parts[1], "send: agent not found in registry");
            }
        }
        "broadcast" => {
            if let Some(msg) = parts.get(1) {
                agent::broadcast_registry(ctx.registry, "user", msg, None);
            }
        }
        "status" => {
            let reg = agent::lock_registry(ctx.registry);
            for handle in reg.values() {
                {
                    let core = handle.core.lock();
                    tracing::info!(agent = %handle.name, state = ?core.state.get_state(), "status");
                }
            }
        }
        "config" => handle_config_command(&parts, ctx.home),
        "set" => handle_set_command(&parts, ctx.home),
        _ => {
            tracing::warn!(cmd = cmd, "unknown command");
        }
    }
    false
}

/// Handle `:config get <key>` / `:config set <key> <value>` / `:config list`.
/// Mirrors the `config` MCP tool path (`runtime_config::get_key/set/list`) so the
/// operator can toggle runtime config from the TUI without invoking an MCP tool.
/// Split out of `execute` so it is unit-testable without a full `CommandCtx`.
///
/// `parts` is the `splitn(3, ' ')` split, so for `set` the third element holds
/// `"<key> <value>"` glued together; we re-split it on the first space.
/// Feedback goes through `tracing` like the sibling `:status` command.
fn handle_config_command(parts: &[&str], home: &Path) {
    match parts.get(1).copied() {
        Some("list") => {
            tracing::info!(config = %crate::runtime_config::list(), "config list");
        }
        Some("get") => match parts.get(2) {
            Some(key) => match crate::runtime_config::get_key(key) {
                Ok(value) => tracing::info!(key = key, value = %value, "config get"),
                Err(e) => tracing::warn!(error = %e, "config get failed"),
            },
            None => tracing::warn!("config get requires <key>"),
        },
        Some("set") => match parts.get(2).and_then(|kv| kv.split_once(' ')) {
            Some((key, value)) => match crate::runtime_config::set(home, key, value.trim()) {
                Ok(result) => tracing::info!(result = %result, "config set"),
                Err(e) => tracing::warn!(error = %e, "config set failed"),
            },
            None => tracing::warn!("config set requires <key> <value>"),
        },
        _ => tracing::warn!("config: use `get <key>` | `set <key> <value>` | list"),
    }
}

/// #2325: `:set <key> <value>` — shorthand for `:config set`, routing to the same
/// persisted runtime-config store (e.g. `set copy_on_select on`). Split out of
/// `execute` so it is unit-testable without a full `CommandCtx`. `parts` is the
/// `splitn(3, ' ')` split, so key is `parts[1]` and value is `parts[2]`.
fn handle_set_command(parts: &[&str], home: &Path) {
    match (parts.get(1), parts.get(2)) {
        (Some(key), Some(value)) => match crate::runtime_config::set(home, key, value.trim()) {
            Ok(result) => tracing::info!(result = %result, "set"),
            Err(e) => tracing::warn!(error = %e, "set failed"),
        },
        _ => tracing::warn!("set requires <key> <value>"),
    }
}

/// Look up the fleet_instance_name for an agent by scanning the layout.
fn lookup_fleet_name(layout: &Layout, agent_name: &str) -> Option<String> {
    for tab in &layout.tabs {
        for id in tab.root().pane_ids() {
            if let Some(pane) = tab.root().find_pane(id) {
                if pane.agent_name.as_str() == agent_name {
                    return pane.fleet_instance_name.clone();
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, Pane, PaneSource, Tab};
    use crate::vterm::VTerm;
    // #1763 residual: `config_set_via_palette_*` mutates the process-global
    // `RUNTIME_CONFIG` (via `runtime_config::set` → `*global().write()`), the same
    // singleton the `runtime_config::tests` serialize on. Join the SAME
    // `runtime_config` serial group so this cross-module test can't clobber their
    // global mid-assertion.
    use serial_test::serial;

    fn test_pane(id: usize, agent: &str, fleet_name: Option<&str>) -> Pane {
        Pane {
            agent_name: agent.into(),
            instance_id: crate::types::InstanceId::default(),
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id,
            backend: None,
            working_dir: None,
            display_name: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: fleet_name.map(String::from),
            last_input_at: None,
            pending_notification_count: 0,
            selection: None,
            source: PaneSource::Local,
        }
    }

    #[test]
    fn lookup_fleet_name_found() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "t".to_string(),
            test_pane(1, "dev", Some("dev-a1b2c3")),
        ));
        assert_eq!(
            lookup_fleet_name(&layout, "dev"),
            Some("dev-a1b2c3".to_string())
        );
    }

    #[test]
    fn lookup_fleet_name_not_found() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new(
            "t".to_string(),
            test_pane(1, "dev", Some("dev-x")),
        ));
        assert_eq!(lookup_fleet_name(&layout, "ghost"), None);
    }

    #[test]
    fn lookup_fleet_name_no_fleet_name_returns_none() {
        let mut layout = Layout::new();
        layout.add_tab(Tab::new("t".to_string(), test_pane(1, "dev", None)));
        assert_eq!(lookup_fleet_name(&layout, "dev"), None);
    }

    #[test]
    fn command_parsing_splits_at_most_3_parts() {
        // Pin the parsing shape: splitn(3, ' ') means at most 3 parts
        let parts: Vec<&str> = "send target hello world".trim().splitn(3, ' ').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "send");
        assert_eq!(parts[1], "target");
        assert_eq!(parts[2], "hello world"); // remainder preserved
    }

    #[test]
    fn command_parsing_empty_input() {
        let parts: Vec<&str> = "".trim().splitn(3, ' ').collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], "");
    }

    /// `:config set <key> <value>` parse shape: splitn(3) glues "<key> <value>"
    /// into parts[2]; the handler re-splits on the first space. Pin it so a future
    /// change to the split width doesn't silently break `set`.
    #[test]
    fn config_set_parse_shape_glues_key_value() {
        let parts: Vec<&str> = "config set show_pane_state true".splitn(3, ' ').collect();
        assert_eq!(parts[1], "set");
        assert_eq!(parts[2], "show_pane_state true");
        assert_eq!(parts[2].split_once(' '), Some(("show_pane_state", "true")));
    }

    /// `:config set` from the palette must actually reach `runtime_config::set`
    /// and persist. Assert against the written file (deterministic — avoids the
    /// process-global cache shared across parallel tests).
    #[test]
    #[serial(runtime_config)]
    fn config_set_via_palette_persists_to_runtime_config() {
        let dir =
            std::env::temp_dir().join(format!("agend-test-cmd-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        handle_config_command(&["config", "set", "show_pane_state false"], &dir);
        let raw = std::fs::read_to_string(dir.join("runtime-config.json")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        assert_eq!(
            v["show_pane_state"],
            serde_json::json!(false),
            "palette :config set must persist to runtime-config.json"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #2325: the `:set <key> <value>` shorthand (parts already split into
    /// key=parts[1], value=parts[2]) must reach `runtime_config::set` and persist,
    /// accepting the `on`/`off` vocabulary for `copy_on_select`.
    #[test]
    #[serial(runtime_config)]
    fn set_command_persists_copy_on_select() {
        let dir = std::env::temp_dir().join(format!("agend-test-cmd-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        handle_set_command(&["set", "copy_on_select", "off"], &dir);
        let raw = std::fs::read_to_string(dir.join("runtime-config.json")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        assert_eq!(
            v["copy_on_select"],
            serde_json::json!(false),
            "palette :set copy_on_select off must persist to runtime-config.json"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── #t-5 command-palette completion ──────────────────────────────────────

    #[test]
    fn matching_specs_prefix_filters_first_token() {
        // Empty / whitespace input → ALL specs (list-on-open, the operator's
        // "what commands exist?" entry point).
        assert_eq!(matching_specs("").len(), COMMAND_SPECS.len());
        assert_eq!(matching_specs("   ").len(), COMMAND_SPECS.len());
        // Prefix narrows; only `config` starts with "co".
        let co = matching_specs("co");
        assert!(co.iter().any(|s| s.keyword == "config"));
        assert!(co.iter().all(|s| s.keyword.starts_with("co")));
        // First token only: trailing args don't drop the keyword match.
        assert!(matching_specs("config get foo")
            .iter()
            .any(|s| s.keyword == "config"));
        // No match → empty (so Tab-complete is a no-op).
        assert!(matching_specs("zzz").is_empty());
    }

    /// Bidirectional sync guard: the `COMMAND_SPECS` keyword set must EXACTLY
    /// equal the set of `execute` command arms — so a stale/typo'd spec can't
    /// offer a completion the dispatcher won't handle (forward), AND a newly added
    /// command can't be missing from the palette (reverse).
    ///
    /// Source-scan, NOT live `execute` calls (`spawn`/`restart` fork real PTY
    /// processes); pure + cross-platform. Two precision measures vs a naive
    /// substring scan:
    /// 1. Bound to the `execute` fn body (`fn execute(` → next top-level `fn`), so
    ///    `handle_config_command`'s `Some("get")`/`Some("set")` sub-arms and the
    ///    test module don't leak in.
    /// 2. Match a quoted keyword ONLY when immediately followed by `|` (group arm)
    ///    or `=>` (arm body). This excludes non-arm literals like
    ///    `unwrap_or(&"agent")` and `Some("get")` (both followed by `)`), which a
    ///    bare `contains("\"agent\"")` false-matched.
    #[test]
    fn command_specs_match_execute_arms_bidirectional() {
        use std::collections::BTreeSet;
        let src = include_str!("commands.rs");
        let from_execute = src
            .split_once("fn execute(cmd:")
            .map(|(_, rest)| rest)
            .expect("execute fn must exist");
        let exec_body = from_execute
            .split_once("\nfn ")
            .map(|(body, _)| body)
            .unwrap_or(from_execute);
        let arm_re = regex::Regex::new(r#""([a-z_]+)"\s*(?:\||=>)"#).expect("valid regex");
        let arm_keywords: BTreeSet<&str> = arm_re
            .captures_iter(exec_body)
            .map(|c| c.get(1).expect("group 1").as_str())
            .collect();
        let spec_keywords: BTreeSet<&str> = COMMAND_SPECS.iter().map(|s| s.keyword).collect();
        assert_eq!(
            spec_keywords,
            arm_keywords,
            "COMMAND_SPECS must EXACTLY match execute's command arms.\n  \
             in specs only (offered but unhandled): {:?}\n  \
             in execute only (handled but not in palette): {:?}",
            spec_keywords.difference(&arm_keywords).collect::<Vec<_>>(),
            arm_keywords.difference(&spec_keywords).collect::<Vec<_>>(),
        );
    }
}
