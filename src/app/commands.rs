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

/// The dynamic source feeding a command-argument position, declared per token in
/// [`CommandSpec::args`] and resolved to concrete candidate strings by
/// [`arg_values`]. `Free` = no finite candidate set (the palette shows a usage
/// hint instead of a value list). Drives argument-value completion; `execute`
/// is unaffected (it still parses the raw input string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgSource {
    /// Spawn-able backend names (`Backend::all`).
    Backend,
    /// Layout preset names (`LayoutPreset::names`).
    Preset,
    /// Runtime-config key names (`runtime_config::keys`).
    ConfigKey,
    /// `config` sub-command: `get` | `set` | `list`.
    ConfigSub,
    /// Live agent names (`agent::live_agent_names_vec`).
    Agent,
    /// Free-form value with no finite candidate set (show usage hint).
    Free,
}

/// #t-5 command-palette completion: declarative metadata for the `:` commands —
/// keyword + usage + one-line description + per-argument source. Consumed ONLY by
/// the completion list (`matching_specs` / `palette_completion` +
/// `render::overlay::render_command_palette`); `execute` below stays the source
/// of truth for BEHAVIOR and is unchanged. ⚠ Adding or renaming a command in
/// `execute` REQUIRES a matching entry here — the
/// `command_specs_match_execute_arms_bidirectional` test fails otherwise.
pub(crate) struct CommandSpec {
    pub keyword: &'static str,
    pub usage: &'static str,
    pub desc: &'static str,
    /// Source for each positional argument, in order. `args[i]` feeds the value
    /// completion for the `i`-th argument (the token AFTER the keyword).
    pub args: &'static [ArgSource],
}

pub(crate) const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        keyword: "spawn",
        usage: "spawn [name] [backend]",
        desc: "New agent in a new tab",
        args: &[ArgSource::Free, ArgSource::Backend],
    },
    CommandSpec {
        keyword: "vsplit",
        usage: "vsplit [name] [backend]",
        desc: "New agent; split focused pane vertically",
        args: &[ArgSource::Free, ArgSource::Backend],
    },
    CommandSpec {
        keyword: "hsplit",
        usage: "hsplit [name] [backend]",
        desc: "New agent; split focused pane horizontally",
        args: &[ArgSource::Free, ArgSource::Backend],
    },
    CommandSpec {
        keyword: "kill",
        usage: "kill <agent>",
        desc: "Close an agent's pane",
        args: &[ArgSource::Agent],
    },
    CommandSpec {
        keyword: "restart",
        usage: "restart [agent]",
        desc: "Restart an agent (focused pane if omitted)",
        args: &[ArgSource::Agent],
    },
    CommandSpec {
        keyword: "layout",
        usage: "layout [preset]",
        desc: "Apply a layout preset (cycles if omitted)",
        args: &[ArgSource::Preset],
    },
    CommandSpec {
        keyword: "send",
        usage: "send <agent> <message>",
        desc: "Send a message to one agent",
        args: &[ArgSource::Agent, ArgSource::Free],
    },
    CommandSpec {
        keyword: "broadcast",
        usage: "broadcast <message>",
        desc: "Broadcast a message to all agents",
        args: &[ArgSource::Free],
    },
    CommandSpec {
        keyword: "status",
        usage: "status",
        desc: "Log every agent's current state",
        args: &[],
    },
    CommandSpec {
        keyword: "config",
        usage: "config get|set|list [key] [value]",
        desc: "Runtime config get / set / list",
        args: &[ArgSource::ConfigSub, ArgSource::ConfigKey, ArgSource::Free],
    },
    CommandSpec {
        keyword: "set",
        usage: "set <key> <value>",
        desc: "Set a runtime config key (`config set` shorthand)",
        args: &[ArgSource::ConfigKey, ArgSource::Free],
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

/// Max candidate rows the palette popup renders (and the operator can navigate to)
/// at once. Beyond this the list is windowed to the first `MAX_PALETTE_ROWS` and
/// the operator narrows by typing — so the renderer, `tab_complete`, and the Down
/// bound all index within the SAME window and the highlight can never point off
/// screen from what Tab completes.
pub(crate) const MAX_PALETTE_ROWS: usize = 12;

/// What the palette should offer for the current `input`: the command keyword
/// list (still typing the command), the dynamic value list for the argument
/// position under the cursor, or a usage hint (free-form argument). Computed by
/// [`palette_completion`]; consumed identically by the key handler (navigation /
/// Tab) and the renderer so the highlighted candidate always matches what Tab
/// completes.
pub(crate) enum Completion {
    Keyword(Vec<&'static CommandSpec>),
    Values(Vec<String>),
    UsageHint(&'static str),
}

impl Completion {
    /// Number of selectable candidates (a usage hint is informational, not
    /// selectable, so it counts as zero — Down/clamp must not land on it).
    pub(crate) fn candidate_count(&self) -> usize {
        match self {
            Completion::Keyword(v) => v.len(),
            Completion::Values(v) => v.len(),
            Completion::UsageHint(_) => 0,
        }
    }

    /// Candidates actually shown and reachable — the count capped to the popup
    /// window. The Down bound uses this so `selected` can't run past the visible
    /// rows.
    pub(crate) fn visible_count(&self) -> usize {
        self.candidate_count().min(MAX_PALETTE_ROWS)
    }

    /// Clamp a (possibly stale or off-window) `selected` to the visible window —
    /// the SINGLE source of truth for "which candidate is highlighted". Both the
    /// renderer and [`tab_complete`] index with this, so the on-screen highlight
    /// and the Tab target are always the same candidate.
    pub(crate) fn clamp_selected(&self, selected: usize) -> usize {
        selected.min(self.visible_count().saturating_sub(1))
    }
}

/// The token under the cursor for palette completion. Mirrors `execute`'s
/// whitespace tokenization so a completed candidate lands at the position
/// `execute` will parse it as: `pos` is the token index under the cursor (0 = the
/// command keyword), and `partial` is the text already typed for that token
/// (empty when a trailing space has opened a fresh token).
struct Cursor<'a> {
    tokens: Vec<&'a str>,
    pos: usize,
    partial: &'a str,
}

fn cursor(input: &str) -> Cursor<'_> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if input.ends_with(char::is_whitespace) || tokens.is_empty() {
        // Trailing space (or empty input) → the cursor opens a fresh token.
        Cursor {
            pos: tokens.len(),
            partial: "",
            tokens,
        }
    } else {
        let pos = tokens.len() - 1;
        Cursor {
            partial: tokens[pos],
            tokens,
            pos,
        }
    }
}

/// Concrete candidate strings for an argument source. Only [`ArgSource::Agent`]
/// touches the registry (a single lock, taken only while completing an agent
/// argument — never on the per-pane render path), keeping the cost off the
/// hot path the way #2380's lock-free render snapshot intends.
fn arg_values(src: ArgSource, registry: &AgentRegistry) -> Vec<String> {
    match src {
        ArgSource::Backend => crate::backend::Backend::all()
            .iter()
            .map(|b| b.as_str().to_string())
            .collect(),
        ArgSource::Preset => crate::layout::LayoutPreset::names()
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        ArgSource::ConfigKey => crate::runtime_config::keys(),
        ArgSource::ConfigSub => {
            vec!["get".to_string(), "set".to_string(), "list".to_string()]
        }
        ArgSource::Agent => crate::agent::live_agent_names_vec(registry),
        ArgSource::Free => Vec::new(),
    }
}

/// Compute the completion for the palette `input`. While typing the command word
/// (or when the first token is not yet a complete command) this is the keyword
/// list (P0 behavior, unchanged). Once the keyword is complete and a space has
/// been typed, it switches to the value list for the argument position under the
/// cursor, prefix-filtered by what's typed so far; free-form arguments yield a
/// usage hint instead.
pub(crate) fn palette_completion(input: &str, registry: &AgentRegistry) -> Completion {
    let cur = cursor(input);
    let known_keyword = cur
        .tokens
        .first()
        .is_some_and(|kw| COMMAND_SPECS.iter().any(|c| c.keyword == *kw));

    // Still completing the command word, or the first token isn't a real command
    // yet → keyword completion (identical to P0).
    if cur.pos == 0 || !known_keyword {
        return Completion::Keyword(matching_specs(input));
    }

    let keyword = cur.tokens[0];
    let Some(spec) = COMMAND_SPECS.iter().find(|c| c.keyword == keyword) else {
        return Completion::Keyword(matching_specs(input));
    };

    let arg_index = cur.pos - 1;
    let Some(&src) = spec.args.get(arg_index) else {
        // Past the last declared argument → nothing to complete.
        return Completion::UsageHint(spec.usage);
    };

    // `config`'s key (the 2nd argument) is only meaningful after `get`/`set`;
    // `config list` takes no key, so don't offer one there.
    if keyword == "config"
        && arg_index == 1
        && !matches!(cur.tokens.get(1).copied(), Some("get") | Some("set"))
    {
        return Completion::UsageHint(spec.usage);
    }

    match src {
        ArgSource::Free => Completion::UsageHint(spec.usage),
        src => {
            let mut values = arg_values(src, registry);
            values.retain(|v| v.starts_with(cur.partial));
            Completion::Values(values)
        }
    }
}

/// Tab handler: complete the token under the cursor to the highlighted candidate
/// of the precomputed `completion`, keeping the palette open with a trailing space
/// so the next argument can be typed. Returns `None` (a no-op) when there is
/// nothing to complete (usage hint or empty candidate list). The keyword path
/// preserves any already-typed arguments (P0 behavior); the value path replaces
/// only the current token. Takes the same `completion` the renderer drew, indexed
/// via the shared [`Completion::clamp_selected`], so Tab can never complete a
/// candidate other than the one highlighted on screen (the >12-candidate
/// highlight/Tab desync r4 caught).
pub(crate) fn tab_complete(
    input: &str,
    completion: &Completion,
    selected: usize,
) -> Option<String> {
    let cur = cursor(input);
    let idx = completion.clamp_selected(selected);
    match completion {
        Completion::Keyword(specs) => {
            let keyword = specs.get(idx)?.keyword;
            // Keep arguments already typed after the keyword (P0 behavior).
            let rest = cur
                .tokens
                .iter()
                .skip(1)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            Some(if rest.is_empty() {
                format!("{keyword} ")
            } else {
                format!("{keyword} {rest}")
            })
        }
        Completion::Values(values) => {
            let value = values.get(idx)?;
            // `pos >= 1` on the value path, so the prefix is never empty.
            let prefix = cur.tokens[..cur.pos].join(" ");
            Some(format!("{prefix} {value} "))
        }
        Completion::UsageHint(_) => None,
    }
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
            offthread: None,
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

    // ── #t-… param-value (argument) completion ───────────────────────────────

    fn empty_registry() -> crate::agent::AgentRegistry {
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    /// The declared per-argument sources — the token→source table the operator
    /// sees. A wrong entry would offer the wrong candidate list at that position.
    #[test]
    fn command_specs_arg_sources_match_table() {
        use ArgSource::*;
        let want: &[(&str, &[ArgSource])] = &[
            ("spawn", &[Free, Backend]),
            ("vsplit", &[Free, Backend]),
            ("hsplit", &[Free, Backend]),
            ("kill", &[Agent]),
            ("restart", &[Agent]),
            ("layout", &[Preset]),
            ("send", &[Agent, Free]),
            ("broadcast", &[Free]),
            ("status", &[]),
            ("config", &[ConfigSub, ConfigKey, Free]),
            ("set", &[ConfigKey, Free]),
        ];
        for (kw, args) in want {
            let spec = COMMAND_SPECS
                .iter()
                .find(|s| s.keyword == *kw)
                .unwrap_or_else(|| panic!("no spec for {kw}"));
            assert_eq!(spec.args, *args, "arg sources for `{kw}`");
        }
    }

    /// Each command/token position resolves to the right candidate kind, and the
    /// dynamic value lists hold the real candidates, prefix-filtered.
    #[test]
    fn palette_completion_routes_each_position_to_correct_source() {
        let reg = empty_registry();
        let kind = |input: &str| match palette_completion(input, &reg) {
            Completion::Keyword(_) => "keyword",
            Completion::Values(_) => "values",
            Completion::UsageHint(_) => "hint",
        };
        // Position 0 (the command word) → keyword list.
        assert_eq!(kind("sp"), "keyword");
        assert_eq!(kind(""), "keyword");
        // FREE first arg → usage hint (spawn name / broadcast message).
        assert_eq!(kind("spawn "), "hint");
        assert_eq!(kind("broadcast "), "hint");

        // backend (spawn t2): the value list holds the real backends, filtered.
        let Completion::Values(backends) = palette_completion("spawn foo ", &reg) else {
            panic!("spawn t2 must be a value list");
        };
        assert!(
            backends.contains(&"claude".to_string()) && backends.contains(&"codex".to_string()),
            "backends: {backends:?}"
        );
        let Completion::Values(filtered) = palette_completion("spawn foo cl", &reg) else {
            panic!("filtered backend must be a value list");
        };
        assert_eq!(filtered, vec!["claude".to_string()]);

        // preset (layout t1): exactly LayoutPreset::names().
        let Completion::Values(presets) = palette_completion("layout ", &reg) else {
            panic!("layout t1 must be a value list");
        };
        let want_presets: Vec<String> = crate::layout::LayoutPreset::names()
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(presets, want_presets);

        // config-key (set t1): exactly runtime_config::keys(); value (t2) free.
        let Completion::Values(keys) = palette_completion("set ", &reg) else {
            panic!("set t1 must be a value list");
        };
        assert_eq!(keys, crate::runtime_config::keys());
        assert_eq!(kind("set somekey "), "hint");

        // config sub-command (config t1): get/set/list.
        let Completion::Values(subs) = palette_completion("config ", &reg) else {
            panic!("config t1 must be a value list");
        };
        assert_eq!(
            subs,
            vec!["get".to_string(), "set".to_string(), "list".to_string()]
        );
        // config key (t2) only after get/set; `list` takes none.
        assert_eq!(kind("config set "), "values");
        assert_eq!(kind("config get "), "values");
        assert_eq!(kind("config list "), "hint");

        // agent positions route to the Agent value source (empty registry → empty
        // list, but the Values variant proves it's a value position, not keyword).
        assert_eq!(kind("kill "), "values");
        assert_eq!(kind("restart "), "values");
        assert_eq!(kind("send "), "values");
        // send message (t2) is free-form → hint.
        assert_eq!(kind("send bob "), "hint");
    }

    /// The palette's token splitting agrees with `execute`'s `splitn(3, ' ')` so a
    /// completed value lands in the slot the dispatcher reads it from. `config` is
    /// the only 4-token command: key at token 2, value at token 3 — exactly where
    /// `handle_config_command` re-splits `parts[2]`.
    #[test]
    fn palette_completion_token_split_aligns_with_execute() {
        let reg = empty_registry();
        // `config set <key> <value>`: token2 = key (values), token3 = value (hint).
        assert!(matches!(
            palette_completion("config set ", &reg),
            Completion::Values(_)
        ));
        assert!(matches!(
            palette_completion("config set show_pane_state ", &reg),
            Completion::UsageHint(_)
        ));
        // `set <key> <value>`: token1 = key, token2 = value.
        assert!(matches!(
            palette_completion("set ", &reg),
            Completion::Values(_)
        ));
        assert!(matches!(
            palette_completion("set show_pane_state ", &reg),
            Completion::UsageHint(_)
        ));
        // Mid-token (no trailing space) still completes the position it's editing.
        let Completion::Values(keys) = palette_completion("set show", &reg) else {
            panic!("partial key must still be the key position");
        };
        assert!(
            !keys.is_empty() && keys.iter().all(|k| k.starts_with("show")),
            "partial-filtered keys: {keys:?}"
        );
    }

    /// Compute the completion for `input` (as the renderer + key handler do) and
    /// Tab-complete at `selected`.
    fn tab(input: &str, selected: usize, reg: &crate::agent::AgentRegistry) -> Option<String> {
        let comp = palette_completion(input, reg);
        tab_complete(input, &comp, selected)
    }

    /// Tab completes the token under the cursor (keyword or value), leaving a
    /// trailing space for the next argument; the keyword path keeps already-typed
    /// arguments, and there's no-op when nothing is completable.
    #[test]
    fn tab_complete_completes_current_token_with_trailing_space() {
        let reg = empty_registry();
        // Keyword: prefix → full keyword + space.
        assert_eq!(tab("la", 0, &reg).as_deref(), Some("layout "));
        // Keyword while an arg is already typed → keyword filled, arg kept (P0).
        assert_eq!(tab("sp foo", 0, &reg).as_deref(), Some("spawn foo"));
        // Value (backend): replace current token, keep preceding, add space.
        assert_eq!(
            tab("spawn foo cl", 0, &reg).as_deref(),
            Some("spawn foo claude ")
        );
        // Empty partial → first candidate.
        assert_eq!(
            tab("layout ", 0, &reg).as_deref(),
            Some("layout even-horizontal ")
        );
        // config sub-command.
        assert_eq!(tab("config s", 0, &reg).as_deref(), Some("config set "));
        // Free-form arg → no-op (nothing to complete).
        assert_eq!(tab("set key ", 0, &reg), None);
        // No candidate matches the partial → no-op.
        assert_eq!(tab("spawn foo zzz", 0, &reg), None);
    }

    /// r4 regression: with >MAX_PALETTE_ROWS candidates the renderer windows the
    /// list and clamps the highlight to the last visible row, so Tab MUST complete
    /// that same visible candidate — never the off-screen raw `selected`. The
    /// shared `clamp_selected` guarantees it; this pins the contract (and is RED if
    /// Tab ever re-clamps against the full list again).
    #[test]
    fn tab_complete_targets_visible_highlight_not_offscreen_when_over_window() {
        // 20 synthetic agent-style candidates (the real >12 trigger is a fleet of
        // >12 live agents for `:kill`/`:restart`/`:send <agent>`).
        let values: Vec<String> = (0..20).map(|i| format!("agent{i:02}")).collect();
        let comp = Completion::Values(values);
        // The renderer highlights `clamp_selected(selected)` = last visible row.
        let visible_last = MAX_PALETTE_ROWS - 1; // 11
        assert_eq!(comp.clamp_selected(14), visible_last);
        assert_eq!(comp.clamp_selected(99), visible_last);
        // Tab at a past-the-window `selected` completes the VISIBLE candidate
        // (agent11), not the off-screen agent14.
        assert_eq!(
            tab_complete("kill ", &comp, 14).as_deref(),
            Some("kill agent11 ")
        );
        // Within the window, Tab tracks `selected` exactly.
        assert_eq!(
            tab_complete("kill ", &comp, 3).as_deref(),
            Some("kill agent03 ")
        );
    }

    /// `visible_count`/`clamp_selected` window math: never exceeds the cap, and a
    /// short list clamps to its real last index.
    #[test]
    fn completion_window_clamps_selected_into_visible_range() {
        let many = Completion::Values((0..20).map(|i| format!("v{i}")).collect());
        assert_eq!(many.visible_count(), MAX_PALETTE_ROWS);
        assert_eq!(many.clamp_selected(0), 0);
        assert_eq!(
            many.clamp_selected(MAX_PALETTE_ROWS - 1),
            MAX_PALETTE_ROWS - 1
        );
        assert_eq!(
            many.clamp_selected(MAX_PALETTE_ROWS + 5),
            MAX_PALETTE_ROWS - 1
        );

        let few = Completion::Values(vec!["x".into(), "y".into(), "z".into()]);
        assert_eq!(few.visible_count(), 3);
        assert_eq!(few.clamp_selected(99), 2);
    }
}
