[繁體中文](SIDEBAR-SPIKE-3645.zh-TW.md)

# Spike: Persistent Sidebar (spaces × agents) and Attention Queue (issue #3645)

> Design spike for issue #3645. This is a decision document only: nothing here
> changes production code. It answers the seven spike questions with the exact
> `path:line` each cut would touch, then sequences the work so the first cut is
> a read-only sidebar that reuses the state snapshot the tab bar already
> computes every frame. The model to borrow from herdr is the persistent
> spaces/agents sidebar plus an attention queue; its full status-detection
> externalisation and its notifications are explicitly out of scope here.

## 0. Status, scope, and non-goals

Today the TUI is a single vertical stack: the tab bar, the pane area, and the
status bar (`src/render/core_render.rs:469-476`). Agent state is only visible as
a coloured dot per tab (`state_color` at `src/render/core_render.rs:18`,
`highest_priority_state` at `:507`), and the only fleet-wide overview is the
modal Fleet view inside the Board overlay (`Ctrl+B f`, dispatched at
`src/app/dispatch.rs:243-253`, rendered by `render_fleet_view` at
`src/render/panels_fleet.rs:83`). There is no persistent glance and no
"jump to the next agent that needs me".

The cheap path the spike must validate: `build_agent_state_snapshot`
(`src/render/core_render.rs:364`, remote variant `:371`) already resolves every
pane's state for the tab bar each frame, and the team grouping already exists in
`plan_team_order` (`src/team_order.rs:47`). A sidebar is therefore mostly a new
projection of data the render loop already holds.

- Goals: answer questions 1-7 with `path:line`; give an ordered implementation
  split with explicit dependencies; prove the data is already available.
- Non-goals: screen-detection rule externalisation (#3647, separate spike);
  toast or audio notifications; any daemon-side or state-machine behaviour
  change; the `context%` field plumbing beyond noting where it lands.
- Deliverable: this document pair. No production-code change.

## 1. Question 1 — What a space maps to

Decision: the default space is the **team**, and a worktree is a nested second
dimension under its source space. A third dimension (source repository or
project board) is deferred, not designed here.

Team is the canonical, already-built grouping: `plan_team_order` returns
deterministic groups in configured member order (`src/team_order.rs:47`,
`src/team_order.rs:118-154`), and both the Fleet view (`src/render/panels_fleet.rs:127`)
and the remote roster (`src/app/app_state_remote.rs:43`) already consume it. The
#3630 deterministic-ordering contract is this function, so the sidebar shares it
rather than inventing a parallel order.

Worktree nesting uses the binding record, which already persists `branch`,
`worktree`, and the owning `source_repo` (`src/binding.rs:233`, `:380`). A space
whose panes carry a managed-worktree binding renders as a child of the space
whose `source_repo` matches; when a binding is absent the space stays flat.

Instances with no team fall into a synthetic `unassigned` space, exactly as the
Fleet view already does (`src/render/panels_fleet.rs:156-161`); `plan_team_order`
returns them in its `ungrouped` list (`src/team_order.rs:156-161`).

| Dimension | Source of truth | Decision |
|---|---|---|
| Team | `plan_team_order` (`src/team_order.rs:47`) | Primary space; shared with the Fleet view |
| Worktree | binding `source_repo`/`branch` (`src/binding.rs:233,380`) | Nested child space under its source space |
| No team | `TeamOrderPlan.ungrouped` (`src/team_order.rs:156-161`) | Synthetic `unassigned` space |
| Project / repo | local `fleet.yaml` only | Deferred; not a selectable dimension in the first cut |

## 2. Question 2 — Agents-row content

Each row is a fixed-column projection of data already computed per frame, with
one honest exception: `context%` is tracked only as a private `Option<(f32,
Instant)>` on the tracker (`src/state/mod.rs:322`) and has no lock-free published
handle, unlike `published_state` (`src/agent/mod.rs:105`, `:141`). Adding it
would mirror that pattern; until then the sidebar shows `—` for it.

The name, backend, and team are local to the pane record; task and branch are the
same lookups the Fleet view already performs (`src/render/panels_fleet.rs:114-125`,
`:209`). The state glyph and colour reuse `state_color` (`src/render/core_render.rs:18`),
so the sidebar and the tab dot never disagree.

| Field | Source | Notes |
|---|---|---|
| State glyph + colour | snapshot + `state_color` (`src/render/core_render.rs:18`) | Same resolved map the tab bar uses |
| Name | `pane.agent_name` | Short display name, like Fleet `build_agent_line` |
| Backend | `pane.backend` / `Backend::name()` (`src/backend.rs:828`) | Rendering only; no behaviour |
| Team | `plan_team_order` membership | Also selects the space |
| Task title | local board match (`src/render/panels_fleet.rs:114-125`) | Truncated; `—` when unclaimed |
| Branch | `binding::read` (`src/binding.rs:710`) | Truncated (`build_agent_line`, `:209`) |
| Context % | tracker field (`src/state/mod.rs:322`) | Needs a new published handle; `—` meanwhile |

Narrow-width truncation follows the tab-bar precedent: measure with
`unicode_width` and render only the highest-value prefix (`src/app/mouse.rs:391`).
A wide row shows glyph, name, branch, and task; a narrow row drops branch then
task; the compact mode (section 5) drops to the glyph alone. Truncation is
display-only and never changes the ordering key.

## 3. Question 3 — Ordering and grouping

Two modes, both keyed off the same canonical membership:

- `spaces` mode groups rows under their space and orders members exactly as
  `plan_team_order` does (`src/team_order.rs:118-154`), matching the Fleet view.
- `priority` mode flattens everything into one attention queue.

The attention order must NOT be `AgentState::priority()` (`src/state/mod.rs:148`).
That scale is tuned for the latch and tab-dot race, not for "who needs a human":
`Active` (6) outranks `Idle` (4) although neither needs anyone, while
`AwaitingOperator` (2) is near the bottom despite being a hard block on a human.
The sidebar therefore derives a dedicated `attention_rank(AgentState) -> u8` from
the existing predicates (`is_error` at `src/state/mod.rs:181`, `is_unavailable`
at `:186`, `wants_raw_keystrokes` at `:194`) plus explicit matches.

Ties inside a tier break by canonical team-order index, then by name, so the
queue is stable frame to frame. The order is total and deterministic, which is
what makes cut 3 (jump-to-next) reproducible.

| Tier | Member states | Why it ranks here |
|---|---|---|
| 1 | PermissionPrompt, InteractivePrompt, AwaitingOperator | A human gate is blocking the agent now |
| 2 | AuthError, ApiError, ModelUnsupported, UsageLimit | An error the operator must fix or acknowledge |
| 3 | GitConflict | Blocked, but an in-agent action can clear it |
| 4 | Crashed, Restarting, Hang | Lifecycle fault; recovery may need a hand |
| 5 | RateLimit, ServerRateLimit, ContextFull | Usually self-heals, but worth a look |
| 6 | Starting, Active | Busy; nothing owed to the operator |
| 7 | Idle | Nothing pending; sorted last |

## 4. Question 4 — Division of labour with the Fleet overlay

The sidebar is a persistent, compact projection of the same data; the Board
overlay's Fleet view stays a full-screen detail surface with the sibling Status,
Monitor, and Tasks tabs (`src/render/panels.rs:233-272`). The overlay is kept,
not merged.

The reason is structural: the overlay is a modal mode inside the Board overlay
(`src/app/dispatch.rs:243-253`) and its rows are built from a different source
set — instance metrics, the task board, and synchronous `binding::read` and
`FleetConfig::load` calls (`src/render/panels_fleet.rs:89-98`, `:209`). Merging
would pull those per-frame disk reads into the always-on render path. The
sidebar instead reads only the in-memory snapshot plus pane records.

De-duplication happens later and in one direction: extract a shared row builder
so the overlay and the sidebar agree on formatting (cut 4), not by deleting the
overlay. The overlay remains the place for columns the sidebar deliberately
omits (health, memory, CPU, uptime).

| Surface | Lifecycle | Data source | Decision |
|---|---|---|---|
| Sidebar | Always on (toggleable) | Per-frame snapshot + pane records | New, read-only first |
| Fleet view | Modal inside Board overlay | Metrics + tasks + binding disk reads | Kept unchanged |
| Status/Monitor/Tasks | Modal siblings | Own sources | Untouched |
| Shared formatting | n/a | n/a | Extract a row builder in cut 4 |

## 5. Question 5 — Layout, width, and hit-testing

Add the sidebar inside the middle band only: wrap the current `chunks[1]`
(`src/render/core_render.rs:469-476`) in a horizontal layout of `sidebar_width`
plus the remaining pane area. The tab bar and the status bar stay full width, so
their render and hit-testing are untouched (tab-bar hit test at
`src/app/mouse.rs:382-399`; shared-width contract noted at
`src/render/core_render.rs:527-529`).

The pane tree already records its rects from whatever area it is given
(`render_pane_tree`, `src/render/core_render.rs:616-674`), and `pane_at` /
`title_bar_at_with_team` read those rects (`src/layout/tab.rs:346`, `:362`). So
the only mouse change is the hardcoded pane-area rect: `handle_down` builds
`Rect::new(0, 1, c, r-2)` (`src/app/mouse.rs:169`), and the mouse-forward path
uses the same assumption (`:108`). Both must use `x = sidebar_width` and
`width = c - sidebar_width`.

Sidebar clicks get a dedicated hit test evaluated after the tab-bar row check
(`src/app/mouse.rs:144-207`) and before pane/border handling, so a click on a row
cannot fall through to a pane underneath. Width is a runtime-config value
following the `observed_badge` precedent (`src/runtime_config.rs:75-78`),
clamped to a sane minimum and maximum.

```text
+--------------------------------------------+  tab bar (full width, row 0)
| spaces            |                         |
|   dev             |                         |
|     dev/wt-3645   |      pane grid          |
| agents            |   (shifted right by     |
|   * dev-1  busy   |    sidebar_width)       |
|   ! dev-2  perm   |                         |
+--------------------------------------------+  status bar (full width)
```

Compact mode is worth doing, but last: it is a rendering change to the sidebar
width and row content (a one- or two-column strip of state glyphs) and does not
touch the geometry plumbing, so it belongs in cut 4 alongside width config.

| Concern | Location | Change |
|---|---|---|
| Top-level split | `src/render/core_render.rs:469-476` | Split only the middle band horizontally |
| Pane rects | `src/render/core_render.rs:616-674` | Unchanged; derives from the given area |
| Pane hit test | `src/layout/tab.rs:346`, `:362` | Unchanged; reads recorded rects |
| Mouse pane area | `src/app/mouse.rs:169`, `:108` | Offset `x` by sidebar width |
| Row hit test | `src/app/mouse.rs:144-207` | New sidebar branch, above pane handling |

## 6. Question 6 — Interaction and keybindings

All keys stay inside the existing `Ctrl+B` prefix (`src/keybinds.rs:102`), and
the two new letters are currently unmapped in `dispatch_prefix`
(`src/keybinds.rs:167-239`), so there is no conflict with the existing map.

- Toggle the sidebar: `Ctrl+B v` (new `Action::ToggleSidebar`). `v` is free.
- Next agent needing attention: `Ctrl+B u` (new `Action::NextAttentionAgent`).
  `u` is free, and the action should be added to `is_repeatable`
  (`src/keybinds.rs:242`) so `u u u` walks the queue like `Ctrl+B o` cycles panes.
- Switch `spaces` / `priority`: a runtime config key (`:set sidebar_sort=...`),
  not a key, to avoid burning prefix letters on a rare toggle.
- Click a row: focuses that agent's pane (mouse only), reusing the existing
  focus/`goto_tab` plumbing (`src/app/mouse.rs:146-153`).

Adding an `Action` variant is compile-forced through the exhaustive `app::dispatch`
match (the contract documented at `src/keybinds.rs:62-66`), so a new key cannot
land live-in-attach-but-dead-in-app.

| Interaction | Binding | Conflict check |
|---|---|---|
| Toggle sidebar | `Ctrl+B v` | `v` unmapped in `dispatch_prefix` |
| Next attention agent | `Ctrl+B u` | `u` unmapped; add to repeat keys |
| Sort mode | `:set sidebar_sort` | Not a prefix key |
| Focus a row | Mouse click | New hit test above pane handling |

## 7. Question 7 — Remote and multi-daemon consistency

State is consistent by construction because the sidebar reads the *same resolved
snapshot* the tab bar uses: `render_with_team` selects either the local
`build_agent_state_snapshot` or the remote-aware variant from
`remote_states` (`src/render/core_render.rs:478-483`), and the remote map is the
name-keyed `remote_agent_states` produced by the daemon RPC and passed only in
attached mode (`src/app/app_state.rs:247`, `:943`, set at `:1217`). Ordering and
navigation therefore stay coherent across local and remote agents.

Metadata is the weak half. Backend comes from the local pane record, but branch,
task, and team come from local disk (`src/binding.rs:710`, the local task board,
local `fleet.yaml`). In attached or multi-daemon mode a remote instance's binding
lives on the daemon host, so a local `binding::read` can miss and the local
`fleet.yaml` can differ. The rule is: show `—` rather than fabricate, and treat
those columns as best-effort enrichment.

Extending the daemon's agent-state snapshot with metadata would fix this, but it
is a daemon-side change and is out of scope for this spike. It is named as a
follow-up, not silently assumed.

| Data | Local source | Remote source | Decision |
|---|---|---|---|
| State | registry atomics (`src/agent/mod.rs:105`) | `remote_agent_states` (`src/app/app_state.rs:247`) | Same resolved map as the tab bar |
| Backend | `pane.backend` | local pane record | Reliable |
| Branch | `binding::read` (`src/binding.rs:710`) | daemon host only | Best-effort; `—` when absent |
| Task / team | local board / `fleet.yaml` | daemon host | Best-effort; `—` when absent |

## 8. Implementation cuts and dependencies

Each cut is independently reviewable and keeps the first cut read-only.

| Cut | Change | Depends on |
|---|---|---|
| 1 | Read-only sidebar: layout split + `spaces`/`priority` render from the existing snapshot | none |
| 2 | `attention_rank` sort + shared `plan_team_order` grouping; mode via config | cut 1 |
| 3 | Click-to-focus hit test + `Ctrl+B u` next-attention navigation | cut 2 |
| 4 | Extract shared row builder; compact mode; `sidebar_width` config | cuts 1, 3 |
| 5 | Publish a lock-free `context%` handle and show it | cut 1 |

Cut 5 is deliberately separate: it is the only field that needs new producer-side
plumbing, and it must not gate the read-only sidebar.

## Appendix A — Decision summary

| Question | Decision |
|---|---|
| 1 Space identity | Team is the primary space; worktree nested under `source_repo`; no-team falls to `unassigned`; project deferred |
| 2 Row content | Glyph/name/backend/team/task/branch from current data; `context%` needs a new published handle |
| 3 Ordering | `spaces` uses `plan_team_order` (#3630); `priority` uses a dedicated `attention_rank`, not `AgentState::priority()` |
| 4 Fleet overlay | Kept as the modal detail surface; sidebar is the persistent compact projection; share a row builder later |
| 5 Layout | Sidebar inside the middle band only; tab/status bars stay full width; mouse pane-area `x` shifts; compact is cut 4 |
| 6 Interaction | `Ctrl+B v` toggle, `Ctrl+B u` next attention, `:set sidebar_sort`; no prefix conflict; click focuses a row |
| 7 Remote | State uses the same resolved snapshot; metadata is best-effort with `—`, daemon enrichment is a follow-up |
