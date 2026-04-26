# Sprint 20 Codebase Audit — Dev-Lead Synthesis

> **Sources**:
> - 4 area reports: [CHANNEL.md](CHANNEL.md), [DAEMON.md](DAEMON.md), [TUI.md](TUI.md), [MCP.md](MCP.md) (all merged 2026-04-27)
> - 4 peer-pass critiques: appended in CHANNEL.md / DAEMON.md / TUI.md / MCP.md tail sections
> - Scope freeze: `d-20260426210724891457-5` (post-challenge-round 11 修正項)
> - Audit window: 21:09–22:00 UTC + peer pass 21:25–21:38 UTC

---

## Executive summary

4-Track parallel codebase audit (audit-only, 0 code change) revealed **6 Critical + 11 High + 10 Medium + 5 Low = 32 actionable findings** across 4 areas. Synthesis identifies **5 risk clusters** and recommends Sprint 21 scope-freeze in this order:

1. **Lifecycle atomicity** (6 Critical/High) — Epic across A/B with cascading failure dependency
2. **Authorization gates** (2 Critical + spillover) — A inbound + outbound + D decisions + C↔D bridge
3. **Trait contract gaps** (4 High) — Channel send/edit/delete stubs vs caps
4. **Robustness panic-class** (1 High) — render overlay resize 5 unbounded subtraction
5. **Joint sub-tracks** — `app/api_server.rs` C↔D bridge un-audited by both

**Total estimated**: ~600 LOC refactor + 8 PRs + 1 joint audit。

**Cross-track validated systemic finding**: **13+ daemon/channel spawn sites / 0 graceful-shutdown** (Track B inventory + Track A extension). Process-exit acceptable for current deployment but require explicit doc rationale.

---

## Critical findings (6)

| ID | File:Line | Issue | Track | Cluster |
|---|---|---|---|---|
| **C1-A** | `src/channel/telegram.rs:197-203` | `is_user_allowed: None` accepts all; fail-open default | A | Auth gates |
| **F1-B** | `src/agent.rs:325-480` | `spawn_agent` partial-failure orphan/phantom registry | B | Lifecycle |
| **F2-B** | `src/api/handlers/instance.rs:84-128` | `delete_instance` mutates registry before child exit | B | Lifecycle/Signal |
| **F3-B** | `src/app/mod.rs:859-866` | `kill_agent` (app mode) SIGKILL leader only — subprocess survives (PR #159 regression) | B | Signal |
| **F4-B** | `src/agent.rs:420-434` | `pty_read_loop` no shutdown observation — thread leaks | B | Lifecycle |
| **C1-D** | `src/decisions.rs:184-236` | `decisions::update` no author/ownership gate (parallels `tasks::can_mutate_task`) | D | Auth gates |

**Path-keyword auto-Critical rule applied** to all 6 (Sprint 19 challenge #2 inheritance) — concentrated in `src/auth/security/handlers/` zones + lifecycle invariant words.

---

## High findings (11) — abbreviated

- **A**: H1-H4 — `Channel::send/edit/delete` trait surface lies, `MsgRef` opaque-empty, `mod.rs:8-13` stale doc
- **B**: H1 supervisor↔main-loop heartbeat dual-tick race (F6), H2 `waiting_on` partial-write (F7), H3 health-tracker fresh-window during respawn (F8), F5 TUI server spawn no rollback
- **C**: H1 `render.rs:618-656` 5 unbounded `area.height/width - N` subtraction sites (same class as PR #194 vterm OOB hotfix)
- **D**: H1 destructive handlers no per-agent auth gate (by-design but undocumented)

---

## Cross-area dependency map

| From → To | Issue | Type |
|---|---|---|
| A → B | `active_channel()` registration silent fall-through (CD1) — supervisor/ci_watch/daemon drop notices when channel unregistered | observability |
| A → D | `active_channel().create_topic()` Err discarded (CD2) — instance boots no Telegram surface | error handling |
| A → B | `ChannelKind` discriminator leak `inbox.rs:139` (CD3) | extensibility |
| A → A | `fleet_broadcast` daemon-side persistence vs channel-side format ownership split (CD4) | clarity |
| **A peer-pass → B** | supervisor outbound `notify` unfiltered (lines 126/134, ci_watch:622) — leaks PTY tails to whole group | **auth consequence** |
| **A peer-pass ↔ B** | `TelegramState` lock contention from tick layer during crash bursts | contention |
| **B peer-pass → A** | F1-F3 lifecycle compound: spawn cascade + kill leak orphan Telegram bindings | **cascading failure** |
| B → B | F1+F2+F3 systemic — respawn / kill / replace 多 partial windows | systemic |
| C peer-pass ↔ D | **`app/api_server.rs` (130 lines) un-deep-audited by both — joint surface** | joint scope |
| **D peer-pass → C** | TUI panic propagation — does overlay panic kill API server thread? cascades to daemon's `proxy_or_local` | cross-thread crash |
| **D peer-pass → C** | `state.rs` PTY classifier adversarial input (compromised agent spoof "Ready") — trust boundary not caught by A/D auth | trust boundary |
| D → B | `set_waiting_on` MCP tool writes 2 fields supervisor reads — F7 race end-to-end | atomic mutation |

**Top systemic theme**: partial-failure lifecycle windows (6 findings cluster A+B+D) — recommend Sprint 21 "lifecycle transaction" Epic spanning 3 tracks.

---

## JoinHandle / lifecycle systemic gap

**Track B inventory** (DAEMON.md):
- 11 spawn sites in daemon
- **0 graceful-shutdown joins** (all fire-and-forget)
- 1 site has rationale comment (supervisor module-doc)

**Track A extension** (peer-pass):
- 2 additional spawn sites in channel layer:
  1. `src/channel/telegram.rs:78-89` — private tokio Runtime via OnceLock
  2. `start_polling()` (line 348) — dispatcher loop into above runtime
- Both have 0 graceful shutdown

**Corrected total**: **13+ spawn / 0 graceful systemic**

**Cross-track impact**:
- F1 partial-failure compounds: PTY thread + Telegram runtime fail unsafely
- F2 delete + F3 kill don't coordinate with A's Telegram cleanup
- F8 respawn doesn't restore Telegram state symmetrically
- B peer-pass: F1-F3 cascade leaves orphan Telegram binding

**Sprint 21 action**: A+B joint sub-task — unify spawn-site inventory + document graceful-shutdown stance (likely "process-exit acceptable but clarify in `daemon/mod.rs` module doc").

---

## `app/api_server.rs` cross-track joint sub-track

**Track C flagged (TUI.md:228)**: 130-line bridge un-deep-audited by Track C.

**Track D peer-pass (MCP.md:192-199)**: Track D handlers.rs 90% NOT line-by-line, compounds same blindspot. **Recommend joint Track-C-Track-D sub-track** for `src/app/api_server.rs:1-130` + corresponding `mcp/handlers.rs` route arms.

**Track A peer-pass (CHANNEL.md:269) extends**: from channel angle, MCP→channel bridge is in `mcp/handlers.rs` (`try_telegram_reply/react/edit` + `inject_provenance` at lines 74/88/102/399). **Cross-pass scope = C↔D for app-bridge AND A↔D for channel-bridge** — both flow into same MCP request handler layer.

**Total joint audit scope**: ~200 lines (`api_server.rs` 130 + `mcp/handlers.rs` channel routes ~70).

**Sprint 21 priority**: First sub-track after Critical lifecycle fixes — bridges authorize the C1-D decision attack chain (compromised TUI input → MCP handler → arbitrary decision archive).

---

## Peer-pass elevations

| Finding | Original | Peer elevation |
|---|---|---|
| C1-A inbound fail-closed | Critical | Scope expanded — outbound `notify` also unfiltered (B peer-pass), needs separate gate |
| H4-A MsgRef empty | High | Cascade clarified — F2 delete cannot reach sent messages (B peer-pass), joint fix needed |
| M5-A parallel maps | Medium | May promote High — tick-layer contention scenario surfaced (B peer-pass) |
| H3-B heartbeat race | High | A peer-pass confirms — F6 + F8 compound, joint fix |
| H1-C overlay panic | High | D peer-pass elevation — TUI thread panic may propagate to API server thread, cascades to daemon proxy_or_local fallback |

**No peer-pass downgrades** — no finding demoted.

---

## Praise patterns worth replicating (top 5)

| Pattern | Track | File:Line | Sprint 21 adoption |
|---|---|---|---|
| `ChannelCapabilities::default()` conservative "nothing supported" | A | caps.rs:66-90 | Apply to any new adapter capability matrix |
| `BindingRef` opaque payload via `Arc<dyn Any>` | A | binding.rs:23-67 | Separates platform concerns; future Discord/Slack benefit |
| `tasks::can_mutate_task` centralized auth predicate | D | tasks.rs:57-72 | **Direct template for fixing C1-D decisions::update** |
| Lock + drop discipline (deadlock avoidance) | B | supervisor.rs:44-121 | Replicate comment pattern at every lock pair |
| Saturating arithmetic with explicit failure-mode comment | C | render.rs:823-832 | **Direct template for fixing H1-C 5 unbounded subtractions** |

---

## Sprint 21 cleanup roadmap (recommended scope-freeze order)

| Phase | Epic | Tasks | Est. LOC | Cross-Track |
|---|---|---|---|---|
| **1** | Lifecycle transaction (Critical) | S21-B1 (F1 spawn rollback) + S21-B2 (F2 delete wait-on-child) + S21-B5 (F5 TUI spawn rollback) + S21-A1 outbound notify gate + S21-B3 (F3 kill_agent parity) | ~250 | A+B joint coordination |
| **2** | Auth gates (Critical) | S21-A1 inbound `is_user_allowed` fail-closed + S21-D1 `decisions::update` `can_mutate_decision` predicate (template: `can_mutate_task`) | ~150 | A independent; D applies B's pattern |
| **3** | Trait contract wiring (High) | S21-A2 — Channel `send/edit/delete` actual dispatcher per cap matrix; couples to F1-F3 cascade fix | ~200 | A independent post-Phase 1 |
| **4** | Robustness (High) | S21-C-R1 extract `overlay_dims` helper + saturating arithmetic — fixes H1 + L2 in single PR | ~30 | C independent |
| **5** | Joint audit | C↔D `app/api_server.rs:1-130` + `mcp/handlers.rs` channel routes ~70 (200 LOC scope, write `app-mcp-bridge-audit.md` | audit | C+D joint audit |

**Estimated total**: ~600 LOC refactor + 8 PRs + 1 joint audit。

**Phase ordering rationale**:
- Phase 1 must precede Phase 2 (auth fixes useless if lifecycle leaks bindings/registry)
- Phase 3 must precede Phase 5 (trait wiring needed before bridge audit can audit "real" send/edit/delete behavior)
- Phase 4 independent — quick win, can parallel any phase

---

## Operator wake actions (priority-sorted)

1. ⚠️ **Sprint 21 scope-freeze**: review this synthesis + approve Phase 1 (lifecycle transaction Epic) as Sprint 21 priority
2. ⚠️ **Joint sub-track decision**: confirm C↔D + A↔D `app/api_server.rs` + `mcp/handlers.rs` joint audit scope (~200 LOC) — Phase 5
3. **JoinHandle systemic**: explicit decision on graceful-shutdown stance (process-exit acceptable vs. always-join) — affects 13+ spawn sites
4. **Cross-Track ownership**: `fleet_broadcast` persistence (B) vs format (A) split — clarify in module docs before Sprint 21
5. **Praise pattern adoption**: standardize `can_mutate_*` centralized auth predicate (D's template) for any future trust-mutation API
6. **PR #194 vterm root cause**: existing backlog `t-20260426150432078733-1` (resize sync race) — same class as H1-C (overlay unbounded subtraction) — consider single fix sweep
7. **Sprint 19 backlog hygiene**: 28 open tasks (cleanup report Track 3 audit) — operator decisions still pending including Sprint 11 backend semantics cancel

---

## Methodology

- **audit_mode**: `codebase_audit` (per Sprint 20 challenge #1)
- **Synthesis source**: 4 markdown reports (CHANNEL/DAEMON/TUI/MCP, total ~98K) + 4 peer-pass appendices
- **Synthesis tool**: Explore subagent extraction → dev-lead human aggregation
- **Cross-validated**: each Critical finding has 1+ Track + 1+ peer-pass touchpoint
- **Time**: ~30 min synthesis (after 4×2h audits + 4×30 min peer-passes)

---

## Sprint 20 wrap-up

| Track | PR | Outcome |
|---|---|---|
| Track A (Channel) | #205 + #209 (peer-pass) ✅ | 1 Critical + 4 High + 5 Medium + 4 Low + cross-area 4 |
| Track B (Daemon) | #207 ✅ (含 peer-pass) | 4 Critical + 4 High + 3 Medium + 1 Low + JoinHandle 13+/0 inventory |
| Track C (TUI) | #206 ✅ (含 self peer-pass) | 0 Critical + 1 High + 2 Medium + 2 Low + first-sweep declaration |
| Track D (MCP) | #204 + #208 (peer-pass) ✅ | 1 Critical + 2 High + 3 Medium + 2 Low + 90% non-deep-read caveat |
| Synthesis (this) | (current PR) | 32 findings → 5 phases → 8 PR Sprint 21 roadmap |

**Sprint 20 fully wrapped** — 4 audit + 4 peer-pass + 1 synthesis = 9 markdown deliverables, 0 code change, 0 fleet structural ops。Operator wake decision required for Sprint 21 scope-freeze。
