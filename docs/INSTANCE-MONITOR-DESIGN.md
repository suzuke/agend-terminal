# Instance Monitor Panel Design Proposal

**Sprint 17 PR-AX** — Design phase only, 0 production code.

**Problem**: Operator wants a persistent view of all instance health — memory usage, uptime, CPU, heartbeat lag. Currently no single surface shows per-instance resource metrics. `list_instances` returns agent_state + health_state but no OS-level metrics.

**Source**: Operator telegram request 2026-04-26 "要有一個地方可以觀察目前所有 instance 的狀態 — 記憶體使用量 / 使用時間 等等".

---

## Phase 1: Data Source Audit

### What the daemon already knows

| Metric | Source | Location |
|---|---|---|
| agent_state (Ready/Thinking/ToolUse/...) | `StateTracker` | `src/state.rs` via `handle_list` |
| health_state (ok/crashed/hung/...) | `HealthTracker` | `src/health.rs` via `handle_list` |
| last_heartbeat | metadata JSON | `$AGEND_HOME/metadata/{name}.json` |
| pending_pickup_ids count | metadata JSON | same |
| backend command | `AgentHandle` | `src/agent.rs` |
| PID | `AgentHandle.child` | `src/agent.rs` (managed) / `ExternalHandle.pid` (external) |
| waiting_on | metadata JSON | `$AGEND_HOME/metadata/{name}.json` |

### What requires OS-level collection (new)

| Metric | Source | Crate candidate | Notes |
|---|---|---|---|
| RSS memory (MB) | `/proc/{pid}/status` or `sysinfo` | `sysinfo` (well-maintained, 60M downloads) | Cross-platform; macOS uses `mach_task_info` |
| CPU % | Process CPU time delta / wall time | `sysinfo` | Needs two samples; 5s tick natural fit |
| Uptime | Process start time or spawn timestamp | `sysinfo` or track in `AgentHandle` | Spawn timestamp is simpler + daemon-restart-safe |
| Child process count | Process tree walk | `sysinfo` or `/proc/{pid}/task` | Kiro-cli spawns subprocesses; tree size indicates activity |

### Trade-off: `sysinfo` vs manual `/proc` parsing

| | `sysinfo` | Manual |
|---|---|---|
| Cross-platform | ✅ macOS + Linux + Windows | ❌ Linux-only `/proc` |
| Dependency weight | ~2MB compile, well-maintained | 0 deps |
| API stability | Stable (v0.30+) | N/A |
| **Recommendation** | ✅ Use `sysinfo` | Only if dep budget is strict |

**Recommendation**: Use `sysinfo` behind a feature gate (`monitor` feature) to keep the default binary lean. The monitor tab is opt-in for operators who want resource visibility.

---

## Phase 2: TUI Surface Design

### Tab vs Overlay

| | Tab (persistent) | Overlay (transient) |
|---|---|---|
| Always visible | ✅ Dedicated tab, switch anytime | ❌ Covers current pane |
| Auto-refresh | ✅ Natural with tick loop | ⚠️ Needs manual refresh or timer |
| Coexists with agent panes | ✅ Separate tab | ❌ Blocks interaction |
| PR-AT precedent | — | Status summary is overlay |

**Decision**: Tab. Operator wants to "observe" (persistent monitoring), not "glance" (transient check). Tab allows leaving the monitor open on a second screen while working in agent tabs.

### Row format (one row per instance)

```
 NAME          STATE      HEALTH   MEM(MB)  CPU%  UPTIME    HB-LAG  PICKUP#
 dev-lead      ready      ok         142    2.1   3h 12m      5s       0
 dev-impl-1    thinking   ok         287   18.4   3h 12m      2s       0
 dev-impl-2    tool_use   ok         195    8.7   1h 45m      1s       0
 dev-reviewer  idle       ok          98    0.3   3h 12m    120s       3
 general       ready      rate_limit  64    0.1   3h 12m     45s       0
```

### Sort & filter

- Default sort: by name (alphabetical, stable)
- Keybinds: `m` sort by memory, `c` sort by CPU, `h` sort by heartbeat lag
- Color coding: health_state `ok` = green, `hung`/`crashed` = red, `rate_limit` = yellow

### Update frequency

- **5 seconds** — matches supervisor tick interval. Fast enough for monitoring, slow enough to avoid CPU overhead from `sysinfo` sampling.

---

## Phase 3: Data Collection Backend

### Architecture: daemon-side single collection point

```
supervisor tick (every 10s)
  └─ collect_instance_metrics()  ← NEW
       ├─ sysinfo::System::refresh_processes()
       ├─ for each agent in registry:
       │    ├─ pid → sysinfo process lookup
       │    ├─ rss_bytes, cpu_percent, start_time
       │    └─ store in InstanceMetrics cache
       └─ cache expires after 15s (stale = "no data")

TUI render tick (every ~100ms)
  └─ read InstanceMetrics cache (no OS calls)
```

**Why daemon-side, not TUI-side**: The daemon already owns the agent registry + PID. Having the TUI call `sysinfo` would require PID forwarding over the bridge protocol. Daemon collects once, TUI reads the cache — single writer, multiple readers.

### Hook point

`src/daemon/supervisor.rs::tick()` already iterates all agents every 10s. Add metric collection as a second pass after the existing health/state checks. Alternatively, a dedicated `monitor_tick()` at 5s interval (half the supervisor tick) for fresher data.

### Cache structure (design only — no code)

```rust
// Conceptual — actual struct in implementation PR
pub struct InstanceMetrics {
    pub name: String,
    pub pid: Option<u32>,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f32>,
    pub uptime_secs: Option<u64>,
    pub agent_state: String,
    pub health_state: String,
    pub heartbeat_lag_secs: Option<u64>,
    pub pending_pickup_count: usize,
    pub collected_at: Instant,
}
```

---

## Phase 4: Edge Cases

| Case | Handling |
|---|---|
| **Hung/dead process** | PID exists but process gone → `sysinfo` returns None → display "—" for metrics, highlight row red |
| **rate_limit state** | Show in HEALTH column with yellow highlight; metrics still collected (process is alive, just throttled) |
| **Kiro-cli child process tree** | `sysinfo` can walk children; display aggregate RSS for the process tree, not just the leader |
| **External agents** | Have PID but no `AgentHandle.core` → show PID-based metrics only, state = "external" |
| **Daemon restart** | Metrics cache is in-memory; fresh start = empty until first tick. Uptime resets (use spawn timestamp from metadata if available) |
| **Many instances (>20)** | Scrollable list with viewport. Header row stays pinned. |
| **No `sysinfo` feature** | Feature gate off → monitor tab shows "enable `monitor` feature for resource metrics" with agent_state/health_state only (no OS metrics) |

---

## Phase 5: MVP Priority

### P0 — Ship first (1 PR)
- Monitor tab with name / state / health / heartbeat-lag / pickup-count
- Data from existing `list_instances` + metadata files (no new dependency)
- 5s refresh via existing daemon tick
- Keybind: `Ctrl+B m` to switch to monitor tab

### P1 — Resource metrics (1 PR)
- Add `sysinfo` dependency behind `monitor` feature
- RSS memory + CPU% columns
- Daemon-side collection in supervisor tick

### P2 — Polish (1 PR)
- Sort keybinds (m/c/h)
- Color coding by health state
- Uptime column
- Child process tree aggregation

### P3 — Future candidates (not this sprint)
- Historical sparkline (last 5 min trend)
- Alerting thresholds (memory > 500MB → warn)
- Token usage tracking (requires Anthropic API integration)
- Export to JSON/CSV

---

## Open Questions for Operator

1. **Feature gate or always-on?** `sysinfo` adds ~2MB to binary. Should resource metrics be behind `--features monitor` or always compiled in?

2. **Child process aggregation?** Kiro-cli spawns node/cargo subprocesses. Show aggregate tree RSS or just the leader process?

3. **Refresh rate?** 5s proposed. Faster (1s) is possible but adds CPU overhead. Preference?

4. **Tab keybind?** `Ctrl+B m` (monitor) proposed. Conflicts with anything in your workflow?
