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

---

# Sprint 20.5 cross-validation update (v2)

> **Source**: 4 missing-pair peer-passes (Sprint 20.5 PR #211 A↔C / #213 A↔D / #212 B↔C / #214 B↔D) merged 2026-04-27. Per Sprint 20 partial diagonal coverage (only A↔B + C↔D), 4 missing pairs cross-validated 1-direction each. Sprint 21 roadmap finalized post-this-update。
>
> **Scope freeze v2**: `d-20260426225921440175-6` (Sprint 20.5 sub-tracks within Sprint 20 umbrella)

## Sprint 20.5 NEW findings discovered via cross-validation

| Track | Finding | Pair | Severity | Impact |
|---|---|---|---|---|
| **Track 8 (B→D)** | C1-A outbound notify leak — `supervisor::tick` + `ci_watch.rs:622` push notices unfiltered, leak PTY tails (40 lines/stall) regardless inbound auth | B peer-reviews A | **Critical** | C1 fail-closed inbound 解 1 半 — outbound 需獨立 gate（Sprint 21 task #11 cascade fix） |
| **Track 5 (A→C)** | Telegram topic leak on app-mode kill — `kill_agent` (app.rs:859-866) 不 call `take_binding`，TUI-mode kill 後 `instance_to_topic` 仍 alive | A peer-reviews B | **High** | F3 cross-area extension — app-mode kill asymmetric vs API delete，operator 看到 orphan topics |
| **Track 5 (A→C)** | Spawn-site graceful count understate — `telegram.rs:78-89` runtime + `start_polling` 加 2 sites（0 graceful）→ fleet **13+/0**（B 原 11） | A peer-reviews B | **High** | Systemic graceful-shutdown debt 比 v1 數字大 |
| **Track 8 (B→D)** | Empty MsgRef prevents future cleanup — H1+H4 opaque binding + id="0" 讓「delete_instance 清 bot 訊息」impossible | B peer-reviews A | **High** | F2 partial-failure × MsgRef 不 recoverable cascade |
| **Track 7 (C→B)** | api_server.rs is dual-bridge — TUI↔MCP **AND** Channel↔MCP — C1 攻擊面 widens beyond 「compromised agent」to 「any prompt-injection reaching TUI input」 | C peer-reviews D earlier confirmed by 7 | **High** | Sub-track scope expand C↔D → B+C+D triangulation（~200 LOC） |

## Confirmed cascade attack chain — full end-to-end

**Channel C1 (fail-open inbound) + outbound notify leak + MCP C1 (decisions::update no auth) + MCP H1 (destructive handlers no per-agent gate) + api_server.rs untrusted bridge** =

> **「any Telegram group member silently archives operator strategic decisions OR kills production agents」** — Track 6 headline framing。

**Each layer fix alone insufficient**：
- C1-A inbound 修但 outbound notify 漏 → tail leak persists
- C1-D auth gate 修但 C1-A inbound 漏 → still trigger via Telegram inject
- H1-D handler gate 修但 api_server bridge untrusted → still trigger via TUI input

**Sprint 21 task #11 mandate**: bundle-or-explicit-exposure-decision sequencing — 不能 partial fix。

## NEW systemic patterns NOT in v1 SYNTHESIS

5 clusters dedupe + cross-validated:

| Cluster | Sub-findings | v1 gap | Sprint 21 |
|---|---|---|---|
| **Partial-failure rollback asymmetry** | F1-F5 + binding orphan A + MCP handler 90% unread | v1 traced lifecycle invariants but 漏 binding/handler downstream | unified rollback transaction across A+B+D (Phase 1 expand) |
| **Grace shutdown stance undocumented** | 13+/0 fleet-wide; respawn loop assumption 未明示 | 各 track 數自己 spawn 不知 fleet 規模 | A+B joint task — daemon/mod.rs document stance + unified inventory |
| **Tick-layer contention undercounted** | crash_tx 64-bounded burst → supervisor + main-loop + ci_watch + instance_monitor 全 lock 同 TelegramState | 各 track 量自己 contention 不見 cross-track | A: contention note; B: tick registry R3 |
| **Metadata persistence atomicity scattered** | F7 dual-write + M5 parallel-map + M2 untyped serde | 各 track patches own slice, no transactional boundary | A: bidi-map helper; B: atomic multi-field; D: `#[serde(deny_unknown_fields)]` |
| **API server bridge un-audited fleet-wide** | api_server.rs 130 LOC + handlers.rs 90% unread + mcp/handlers.rs channel-bridge ~70 LOC | 3 tracks each flagged 1 side, neither audit covered both | B+C+D triangulation joint sub-track（取代 v1 C↔D only） |

## JoinHandle systemic UPDATED count

- **v1**: 11 daemon spawn / 0 graceful (Track B inventory)
- **v2 cross-validated**: **13+ spawn / 0 graceful** (+ 2 from Track 5 A peer-pass: `telegram.rs:78-89` runtime + `start_polling`)
- Track 7 (C peer-pass) 進一步 grep `app/telegram_hooks.rs:56,76` — 2 more unnamed `std::thread::spawn` → potentially **15+/0**

**Sprint 21 protocol-level rule (per Track 7 NEW S21-FLEET-SYSTEMIC-2)**: `thread::spawn` allowlist invariant test — every spawn site MUST `Builder::new().name(...)` + `// fire-and-forget: <reason>` comment OR store JoinHandle。Pattern from `handle_message_body_has_no_block_on` invariant test。

## app/api_server.rs sub-track UPDATED scope

- **v1**: C↔D joint of `app/api_server.rs:1-130` only
- **v2 expanded** (Track 7 + Track 8 confirmation): **B+C+D triangulation** scope:
  - `src/app/api_server.rs:1-130`（TUI→MCP entry surface — C1 attack chain entry）
  - `src/mcp/handlers.rs:74,88,102,399`（MCP→Channel outbound bridge `try_telegram_*` + `inject_provenance` — A↔D coupling）
  - 含 `serve_agent_tui`（B spawn site + C server body + D tool routing — Track 7 提）
  - Total scope ~200 LOC

**Sprint 21 priority**: 移到 Phase 1 列表（同 lifecycle Critical），因 cascade attack chain 跨 3-track 無 isolated audit 可 cover。

## Disagreements raised + resolution

| Disagreement | Resolution |
|---|---|
| B's "orphan PID" complete vs A's binding orphan | **Compounding not conflict** — S21-B1 must include A's binding cleanup |
| A's CD4 ownership ambiguous (channel vs daemon) | **Resolved by PR #199 architecture**: persistence=B, format=A. S21 task wording clarified |
| A's `TelegramState` "no read-heavy" vs B's burst contention | **Both correct**: Mutex still right; 但 contention profile understated. S21: add note |
| C's auto-Critical state.rs flag wrong | **C self-corrected**: state.rs is PTY classifier 不是 session restoration. session.rs trust acceptable |

**No disagreements escalated to conflict** — all refinements / compounding。

## Sprint 21 roadmap FINAL (post-Sprint 20.5 v2)

**Original 5 phases + Sprint 20.5 8 NEW tasks integrated**:

| Phase | Epic | Tasks (含 v2 NEW) | Est. LOC | Cross-Track |
|---|---|---|---|---|
| **1** | Lifecycle transaction + outbound auth gate | S21-B1/B2/B5 + **S21-A-new1 binding rollback** + S21-A1 outbound notify gate (NEW Critical from Track 8) + S21-B3 + S21-A/B-joint-1 spawn inventory unification | ~300 | A+B+D Critical bundle |
| **2** | Auth gates + cascade fix | S21-A1 inbound + S21-D1 `can_mutate_decision` + **S21-A/D-joint1 api_server.rs bridge audit (B+C+D triangulation, ≤200 LOC)** + Track 6 task #11 bundle sequencing | ~200 + 1 audit | A+D joint |
| **3** | Trait contract wiring | S21-A2 Channel send/edit/delete dispatcher | ~200 | A independent post-Phase 1 |
| **4** | Robustness | S21-C-R1 overlay_dims helper + saturating arithmetic + Track 7 NEW S21-C-SYSTEMIC-1 transient-state badge | ~50 | C independent |
| **5** | Systemic patterns | S21-A-new2 contention note + S21-B-new1 atomic multi-field metadata + S21-FLEET-SYSTEMIC-2 spawn allowlist invariant test + Track 8 P1+P2 vocabulary unification | ~100 | systemic across tracks |

**Estimated total**: ~850 LOC refactor + 12 PRs + 1 joint audit （v1 600 → v2 850 + 4 PRs）

**Critical phase ordering**:
- **Phase 1 must precede Phase 2** — auth fixes useless if lifecycle leaks bindings/registry/handler-bridge
- **Phase 2 includes B+C+D bridge audit** as part of cascade fix (was Phase 5 in v1, elevated)
- Phase 3-5 parallel independent

## Operator wake actions UPDATED (priority-sorted v2)

1. ⚠️ **Sprint 21 scope-freeze approval** — 5 phases + 12 tasks + 1 joint audit (Phase 2 elevation key change)
2. ⚠️ **Cascade attack chain bundle decision** — Channel C1 + outbound notify + MCP C1 + MCP H1 + api_server bridge — must ship as bundle per Track 6 task #11 sequencing
3. ⚠️ **Joint sub-track scope decision** — B+C+D triangulation `app/api_server.rs:1-130` + `mcp/handlers.rs` channel-bridge ~70 LOC = total ~200 LOC
4. **JoinHandle systemic stance** — fleet-wide 13-15+/0 graceful (per Track 5 + Track 7 extensions); explicit decision + S21-FLEET-SYSTEMIC-2 spawn allowlist invariant test approval
5. **fleet_broadcast ownership clarification** (RESOLVED Sprint 20.5: persistence=B, format=A, document in module headers)
6. **Praise pattern adoption** — `can_mutate_*` D template + `render.rs:823-832` saturating arithmetic comment pattern
7. **PR #194 vterm root cause + H1-C overlay** — same class single fix sweep
8. **Sprint 19 backlog hygiene 28 tasks** — operator decisions still pending including Sprint 11 backend semantics cancel

## Cross-validation methodology observations (v2 meta)

What Sprint 20.5 peer-passes surfaced that single-track Sprint 20 missed:

1. **Cascade consequence tracing** — forward (Sprint 20) + backward (Sprint 20.5 peer-pass) together catches chains no single vector catches。Channel C1 + MCP C1 chain only emerges from cross-validation。
2. **Tick-layer contention never visible in isolation** — 各 track 數自己 lock 不知 cross-track concurrency。
3. **Trust boundary drift across tracks** — state.rs PTY classifier (C scope) feeds MCP scheduling (D scope) — neither solo audit catches end-to-end。
4. **Scope-out assumptions leak** — A 8-command 漏 grep `set_waiting_on`; D 90% handlers.rs unread; C 漏 api_server.rs。 Each track's out-of-scope = other's blind spot。
5. **Atomicity is distributed** — F7 + M5 + M2 各 layer Medium severity；cluster-level 才見 systemic refactor opportunity (R1 unified rollback)。

**Implication**: Diagonal peer-pass (A↔B + C↔D + A↔C + A↔D + B↔C + B↔D = full mesh) 不是 polish 是 systemic-finding mandatory。Sprint 20 partial → Sprint 20.5 補 → 完整 cross-validation。Future audit sprints 該預設 full-mesh。

---

# Sprint 20.5 wrap-up

| Track | PR | Outcome |
|---|---|---|
| Track 5 (A↔C) | #211 ✅ | impl-1 reads TUI: 4 confirmed + 3 systemic + spawn-count update +2 → 13+/0 |
| Track 6 (A↔D) | #213 ✅ | reviewer-2 reads CHANNEL: 3 confirmed + 3 NEW (severity drift / stale enum / decision audit-log) + 4 systemic patterns + cascade chain headline |
| Track 7 (B↔C) | #212 ✅ | reviewer reads DAEMON: 4 confirmed + 3 missed (B+C cross-area: F2×session.rs / F4×vterm flicker / F8×state_color) + 3 systemic (transient badge / spawn allowlist / session-restore downstream) |
| Track 8 (B↔D) | #214 ✅ | impl-2 reads MCP: 5 confirmed + 4 missed + 4 P-systemic patterns (auth×atomicity / vocabulary fragmented / coverage caveats correlated / api_server B+C+D triple-blindspot) |
| Synthesis v2 | (this PR) | Aggregate Sprint 20 + 20.5 → 12 phases + 12 tasks + 1 joint audit roadmap, cascade attack chain end-to-end, systemic patterns 5 clusters, JoinHandle 13-15+/0 |

**Sprint 20 + 20.5 fully wrapped** — 4 audit + 4 peer-pass + 4 cross-validation peer-pass + 2 synthesis = **14 markdown deliverables**, 0 code change, 0 fleet structural ops。Sprint 21 roadmap **ready for operator wake decision**。Cross-validation full-mesh complete。
