//! Remote roster placement methods for [`AppState`].
//!
//! Kept separate from the owner/state definitions so the production app-state
//! module stays below the anti-monolith LOC ceiling.

#[allow(clippy::wildcard_imports)]
use super::*;

impl AppState {
    // #3501: team-grouped placement for hot-reload — mirrors
    // session::place_agents_team_grouped. The shared plan owns ordering.
    pub(super) fn place_remote_team_grouped(
        &mut self,
        to_add: &[String],
        home: &std::path::Path,
        pane_builder: &mut dyn FnMut(&str, &mut Layout) -> anyhow::Result<Pane>,
    ) {
        // #3505 P0(b): bound the retry storm — agents in backoff skip this
        // pass (counter re-aged via advance_deferred so the stale hint still
        // fires and attempts resume). Merged with #3501 team grouping: the
        // filter runs first, grouping applies to the eligible remainder.
        let mut eligible: Vec<String> = Vec::new();
        for name in to_add {
            let fails = self.remote_attach_failures.get(name).copied().unwrap_or(0);
            if !attach_retry_due(fails) {
                let (next, emit_hint) = advance_deferred(fails);
                if emit_hint {
                    tracing::warn!(
                        agent = %name,
                        fails = next,
                        "remote pane attach repeatedly failing — stale registry entry or port shell? \
                         check `agend-terminal doctor` (daemon restart / .port cleanup reconciles)",
                    );
                }
                self.remote_attach_failures.insert(name.clone(), next);
                continue;
            }
            eligible.push(name.clone());
        }
        let to_add = &eligible;
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .unwrap_or_default();
        let plan = crate::team_order::plan_team_order(&fleet, to_add.iter().map(String::as_str));
        // Team-grouped: each team shares one tab named after the team. Hot
        // reload deliberately searches all tabs (rather than only the active
        // tab) so a late member joins an already-open team tab. Team and
        // standalone names share the tab-name namespace, so callers should
        // avoid assigning the same name to both.
        for group in &plan.groups {
            for name in &group.members {
                // #3501: if the agent already has a retained pane (disconnected
                // but not removed), reconnect in place to avoid duplicating the
                // leaf — preserves the existing team tab/split.
                let already_has_pane = self.ui.layout.find_agent_pane(name).is_some();
                match pane_builder(name, &mut self.ui.layout) {
                    Ok(mut pane) => {
                        if pane.instance_ref.is_none() {
                            pane.instance_ref = self.remote_instance_refs.get(name).copied();
                        }
                        let Some(pane) = self.place_correlated_remote_pane(pane) else {
                            continue;
                        };
                        let tab_name = pane.agent_name.clone();
                        self.known_remote_agents.insert(tab_name.to_string());
                        self.remote_attach_failures.remove(name);
                        if already_has_pane {
                            // Reuse retained pane (same as standalone's reconnect).
                            match self
                                .ui
                                .layout
                                .reconnect_or_append_agent_pane(&tab_name, pane)
                            {
                                crate::layout::PaneReconnectOutcome::Reconnected => {
                                    tracing::info!(
                                        agent = %name,
                                        team = %group.name,
                                        "reused retained team pane for re-appeared remote agent"
                                    );
                                }
                                crate::layout::PaneReconnectOutcome::Appended => {
                                    tracing::info!(
                                        agent = %name,
                                        team = %group.name,
                                        "opened separate remote pane because identity was unavailable"
                                    );
                                }
                            }
                        } else if let Some(idx) = self
                            .ui
                            .layout
                            .tabs
                            .iter()
                            .position(|tab| tab.name == group.name)
                        {
                            let tab = &mut self.ui.layout.tabs[idx];
                            if let Some(target_id) = tab.root().pane_ids().last().copied() {
                                tab.split_at_pane(
                                    target_id,
                                    crate::layout::SplitDir::Horizontal,
                                    pane,
                                );
                            }
                            tracing::info!(
                                agent = %name,
                                team = %group.name,
                                "added team member pane via split"
                            );
                        } else {
                            // First new member of this team batch — create team
                            // tab. `push_tab_preserve_focus`, never `add_tab`:
                            // this runs on a background roster tick, and
                            // `add_tab` switches the active tab (layout::add_tab
                            // → switch_active), which would pull the operator off
                            // whatever they are working on. The standalone arm
                            // below preserves focus for exactly this reason.
                            let tab = crate::layout::Tab::new(group.name.clone(), pane);
                            self.ui.layout.push_tab_preserve_focus(tab);
                            tracing::info!(
                                agent = %name,
                                team = %group.name,
                                "opened team tab for newly-appeared remote agent"
                            );
                        }
                        self.needs_resize = true;
                    }
                    // #3505 backoff accounting mirrors the standalone arm below:
                    // without the increment a failing team member never enters
                    // backoff and the stale hint never fires for it.
                    Err(e) => {
                        let fails = self.remote_attach_failures.get(name).copied().unwrap_or(0) + 1;
                        self.remote_attach_failures.insert(name.clone(), fails);
                        tracing::warn!(
                            agent = %name,
                            error = %e,
                            fails,
                            "remote pane attach failed during sync",
                        );
                    }
                }
            }
        }
        // Standalone: per-agent tabs as before.
        for name in &plan.ungrouped {
            match pane_builder(name, &mut self.ui.layout) {
                Ok(mut pane) => {
                    if pane.instance_ref.is_none() {
                        pane.instance_ref = self.remote_instance_refs.get(name).copied();
                    }
                    let Some(pane) = self.place_correlated_remote_pane(pane) else {
                        continue;
                    };
                    let tab_name = pane.agent_name.clone();
                    self.known_remote_agents.insert(tab_name.to_string());
                    self.remote_attach_failures.remove(name);
                    // This sync is add-only: a gone agent's pane is retained
                    // for scrollback. Reconnect that leaf in place when the
                    // agent reappears, including inside an operator split.
                    match self
                        .ui
                        .layout
                        .reconnect_or_append_agent_pane(&tab_name, pane)
                    {
                        crate::layout::PaneReconnectOutcome::Reconnected => {
                            tracing::info!(
                                agent = %name,
                                "reused retained pane for re-appeared remote agent (no duplicate)"
                            );
                        }
                        crate::layout::PaneReconnectOutcome::Appended => {
                            tracing::info!(
                                agent = %name,
                                "opened tab for newly-appeared remote agent"
                            );
                        }
                    }
                    self.needs_resize = true;
                }
                Err(e) => {
                    let fails = self.remote_attach_failures.get(name).copied().unwrap_or(0) + 1;
                    self.remote_attach_failures.insert(name.clone(), fails);
                    tracing::warn!(
                        agent = %name,
                        error = %e,
                        fails,
                        "remote pane attach failed during sync",
                    );
                }
            }
        }
    }
}
