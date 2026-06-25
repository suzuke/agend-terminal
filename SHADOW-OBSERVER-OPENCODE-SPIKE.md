# #2413 Shadow Observer — opencode plane: confirm-first SPIKE (2026-06-24)

Task t-20260623214003295940-24134-6. HARD confirm-first: **can our observer
tap a structured during-TUI signal from opencode while its native TUI runs?**

## VERDICT: ✅ FEASIBLE — opencode is the *strongest* during-TUI structured source so far.

opencode is client-server. The **TUI embeds an HTTP server** (same binary/event-bus
as `opencode serve`). A second, external client can subscribe to `/event` (SSE)
concurrently with the TUI, and the bus carries **native session lifecycle**:
`session.status {busy|idle|retry|...}` + `session.idle` + tool/step lifecycle.
This is far cleaner than screen-scrape (explicit status flags, not regex-on-grid).

## Evidence (all run isolated on custom ports — live fleet untouched)

### 1. TUI embeds a reachable server (PTY test, `tap_tui.py`)
- `opencode [project]` (the default TUI command — the way agend launches it via
  `backend.rs:572 command:"opencode"`) accepts `--port`/`--hostname`.
- Launched `opencode --port 7655 …` in a real PTY → `GET /global/health` on :7655 → **reachable=True**.

### 2. Concurrent external `/event` tap during the TUI (`tap_tui.py`)
- While the TUI ran, an external client subscribed to `/event` → received
  `{"type":"server.connected",...}`, and `GET /api/session` showed the TUI's 2 live
  sessions. → **multi-subscriber tap works; observer ⟂ TUI client.**

### 3. Full real-turn lifecycle (race-free via `serve` + `run --attach`, `attach_capture.py`)
Model `opencode/north-mini-code-free` (free), prompt "Reply … pong". 118 frames in ~3s:
```
server.connected → session.created → session.next.{agent,model}.switched
→ session.status {type:"busy"}        ← NATIVE working flag (4× transitions)
→ message.part.delta ×93              ← streamed responding
→ session.idle                        ← NATIVE idle flag
```
Histogram: server.connected1 session.created1 session.updated3 session.next.agent.switched1
session.next.model.switched1 message.updated5 message.part.updated7 **session.status4**
session.diff1 message.part.delta93 **session.idle1**.

### 4. Event schema (from `GET /doc` OpenAPI, 283KB)
`/event` is SSE of tagged-union `Event` `{id,type,properties}`. Relevant members
map cleanly to our `EvidenceKind` (authority=Stream):
| opencode event (`type`) | → EvidenceKind |
|---|---|
| `session.next.prompted` / `session.next.step.started` | `TurnStarted` |
| `session.next.tool.called` (has `tool`,`callID`,`input`) | `ToolStarted{name}` |
| `session.next.tool.success` / `.tool.failed` | `ToolEnded` |
| `session.next.text.*` / `message.part.delta` | `Responding` |
| `session.next.reasoning.started` | `Responding` (reducer refines→Thinking) |
| `session.status {busy}` | active (working) |
| `session.status {retry}` (has attempt/next) | `RateLimited{retry_at_ms}` |
| `session.idle` | `TurnEnded` |
| `permission.asked` / `.replied` | `ApprovalRequired` |
| token usage (message.updated tokens) | `TokenUsage{in,out}` |
`SessionStatus` = anyOf `{idle}|{retry,attempt,next,...}|{busy,…}`.

## Design (mirrors codex Phase D `rollout.rs`, reuses Evidence+reducer unchanged)
- **New observer module** `src/daemon/shadow/opencode.rs`: fire-and-forget daemon
  thread `spawn(registry, home)` (gated `AGEND_SHADOW_OBSERVER=1`), wired into BOTH
  `run_core` and `run_app` exactly like `rollout::spawn`. Per live opencode agent
  (`backend_command.contains("opencode")`), maintain a `/event` SSE subscription;
  map frames → `Evidence::stream(...)` → `super::push(agent, ev)`. Reducer + Evidence
  schema **untouched** (plane-agnostic; `Authority::Stream` already exists from #2437).
- **Port discovery** (the one real design choice — needs lead nod):
  - TUI does **not** log its port (only `serve` does — verified `port_discovery.py`);
    cross-platform port-from-PID (lsof/proc/netstat) is Windows-painful.
  - **Recommended: inject `--port <allocated>`** into the opencode launch when the
    flag is ON (agent/mod.rs, same site as the existing opencode `XDG_DATA_HOME` /
    `OPENCODE_CONFIG_CONTENT` injections, ~:951/:974). Daemon binds an ephemeral
    port, captures N, closes, passes `--port N`; observer connects 127.0.0.1:N.
  - **Spawn-safety**: a passive observer must NEVER break agent spawn. flag default-OFF
    ⇒ prod unaffected. flag-ON: alloc immediately pre-spawn (sub-ms race); if alloc
    fails, **skip injection** (agent spawns normally, observer just skips that agent).
    Record N on the agent so the observer thread knows where to connect.
- **authority=Stream, confidence=Strong** (native flags ⇒ arguably could be Confirmed,
  but keep Strong parity with codex unless a real reason — same discipline as #2437).

## Discipline carried
flag-OFF additive; reuse Evidence+reducer (no reducer-core change); app-mode real-entry
regression + reverse-mutation (#2434/#2437); cross-platform incl Windows (SSE = std HTTP,
but the spawn-port-alloc + thread must compile/behave on Windows); DUAL cross-model.

## Scripts (scratchpad, recreate if wiped)
`tap_tui.py` (TUI embed + concurrent tap), `attach_capture.py` (real-turn lifecycle),
`port_discovery.py` (TUI doesn't log port), `run_turn.py` (run --port race demo).
