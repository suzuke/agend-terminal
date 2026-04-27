# Sprint 21 Phase 2 — B+C+D triangulation joint audit (api_server.rs + mcp/handlers.rs channel routes)

**Lead auditor**: dev-reviewer-2 (primary)
**Cross-pass auditors**: dev-reviewer (TUI hot context), dev-impl-1 (channel hot context) — appendices below upon their submission
**Date**: 2026-04-27
**audit_mode**: `codebase_audit` (Tier-2 evidence chain dog-fooded by author of R1 methodology rule)
**Closes**: Sprint 20.5 Track 7+8 B+C+D triple-blindspot finding (api_server.rs C↔D un-deep-audited + mcp/handlers.rs channel routes A↔D un-deep-audited)
**Cascade chain status verification**: post PR #216 (outbound auth) + #218 (migration doc) + #219 (inbound auth) + #220 (decision gate). This audit verifies bridge surface has no hidden bypass after cascade fixes.

---

## Methodology (R1)

- **audited_head**: `2fc76ddef4dfd9dc0a49da7a9a86eb0128fc77ec` (origin/main at audit start, post PR #219 A1 inbound merge)
- **scope_source**: Sprint 21 final scope freeze `d-20260426235704857573-8` + dispatch `m-20260427011604933150-173`
- **commands_run**:
  - `git fetch origin main`
  - `git worktree add -b sprint21-phase2-bcd-triangulation-audit ../agend-terminal.worktrees/sprint21-phase2-bcd-triangulation-audit origin/main`
  - `wc -l src/app/api_server.rs` → 130 lines (matches Track 7 dispatch claim)
  - `grep -n 'try_telegram_reply\|try_telegram_react\|try_telegram_edit\|inject_provenance' src/mcp/handlers.rs` → confirmed lines 74 / 88 / 102 / 399
  - `grep -nB2 -A5 'fn try_telegram_reply\|fn try_telegram_react\|fn try_telegram_edit\|fn inject_provenance' src/channel/telegram.rs` → bridge fn signatures
  - `grep -rn 'is_authorized\|is_user_allowed\|user_allowlist' src/channel/` → post-PR-216 auth primitive consumers
  - `grep -rn 'gated_notify\|outbound_authorized' src/` → post-PR-216 gate consumers
- **files_scanned (full)**:
  - `src/app/api_server.rs` (130 lines, full read)
  - `src/mcp/handlers.rs` lines 60-130 (Channel section: reply/react/edit_message/download_attachment) + 380-420 (delegate_task inject_provenance call site) — ~110 lines deep
  - `src/channel/telegram.rs` bridge fn signatures (try_telegram_reply, try_telegram_reply_no_cleanup, inject_provenance, try_telegram_react, try_telegram_edit) — ~50 lines
  - `src/channel/mod.rs:230-259` (gated_notify post-PR-216 — load-bearing reference)
  - `src/channel/auth.rs` — `is_authorized_recipient` primitive
- Time: ~1.5h within 2h hard cap

---

## Scope-correction note (audit-time discovery)

**Track 7 / Track 8 framing of `app/api_server.rs` as "TUI→MCP bridge" is incorrect.** This file is 130 lines of:
- `ApiGuard` RAII handle for run-dir + `OwnedFleet` lifetime tied to TUI process
- `start_api_server` — spawns the **daemon API server** (`crate::api::serve` on Unix socket) in a background thread for app mode
- `noop_guard` — null-object for attached mode
- `auto_start_fleet` — spawn fleet.yaml instances as TUI tabs on cold-boot when no `session.json` is present

It is **NOT** a TUI→MCP entry surface. The actual TUI→MCP bridge (where TUI key/command input reaches MCP tool routing) lives in `src/app/{commands,dispatch,overlay}.rs` (command palette, key handlers) — these were Track C scope and partially deep-read in Track C's audit but not under the "TUI→MCP bridge" framing.

**Implication**: the cascade attack chain bridge surface that Track 7+8 worried about (`app/api_server.rs`) does not exist as framed. The real bridge attack surface is (a) the MCP→Channel direct outbound bridge in `mcp/handlers.rs` channel routes (still in this audit's scope, see below) and (b) the prompt-input flow where Telegram inbound message → InboxMessage → agent PTY → agent issues MCP tool. Surface (b) was implicitly gated by PR #219 (inbound `is_user_allowed`) so the post-cascade-fix architecture is mostly sound — see Critical finding C1 below for the residual bypass.

---

## Findings

### Critical

**C1 — MCP channel routes (`reply` / `react` / `edit_message` / `delegate_task` provenance) BYPASS post-PR-216 outbound auth gate**

Path-keyword auto-Critical (Sprint 19 challenge round #2): `auth/security/handlers/` paths + this is the single-most-load-bearing observation of the whole audit. Skipping the gate in this surface re-opens the cascade chain headline AFTER the cascade fixes were declared closed.

**Where**:
- `src/mcp/handlers.rs:74` — `"reply"` route → `telegram::try_telegram_reply(instance_name, text)`
- `src/mcp/handlers.rs:88` — `"react"` route → `telegram::try_telegram_react(home, instance_name, emoji, message_id)`
- `src/mcp/handlers.rs:102` — `"edit_message"` route → `telegram::try_telegram_edit(home, instance_name, message_id, text)`
- `src/mcp/handlers.rs:399` — `delegate_task` provenance side-channel → `telegram::inject_provenance(target, sender, task)`

All four bridge fns in `src/channel/telegram.rs` (`try_telegram_reply` line 1293, `try_telegram_reply_no_cleanup` line 1330, `inject_provenance` line 1378, `try_telegram_react` line 1400, `try_telegram_edit` line 1434) call into the Telegram bot API **directly**, bypassing both:
- `Channel::notify()` (and therefore `crate::channel::gated_notify()` from PR #216)
- `outbound_authorized()` per-channel check
- `is_authorized_recipient(allowlist, user_id)` from `src/channel/auth.rs`

**Why this re-opens the cascade chain**: PR #216's `gated_notify` only protects daemon-driven outbound (supervisor stall notice, ci_watch failure, daemon crash notify, fleet_broadcast). The direct MCP entry points `reply` / `react` / `edit_message` / `delegate_task` are agent-callable — a prompt-injected agent that issues `reply { text: "<malicious>" }` reaches the Telegram group **regardless of `user_allowlist` state**. This is the same attack class that Sprint 20.5 Track 6 cross-validation cascade chain headline described.

**Concrete attack chain (post-cascade-fix, with this gap)**:
1. (Channel inbound, post PR #219) `user_allowlist == [authorized_user]` → only authorized user's text reaches `handle_message` ✓ closed
2. BUT once inside the agent's PTY, the agent's prompt logic can be prompt-injected through other surfaces (e.g. another agent's `send_to_instance` carrying attacker-controlled content; an MCP tool result that included an unsanitized URL; a CI failure log embedded in a notify body)
3. Prompt-injected agent issues `reply { text: "leak: <secret>" }` → `try_telegram_reply` → bot API → message lands in Telegram group
4. (Channel outbound, post PR #216) `gated_notify` would have fail-closed this — but the bridge doesn't go through `gated_notify`
5. Decision/destructive ops are now gated (PR #220 + H1 docstring); but the **outbound info-leak vector through MCP tools remains open**.

**Severity**: Critical. Re-classified UP from "post-cascade-fix verification confirms closed" to "post-cascade-fix verification surfaces residual bypass". Sprint 21 Phase 2 is **not yet end-to-end closed**.

**Fix shape (proposed for Sprint 21 Phase 2 third sub-PR-fix or Sprint 22)**:
- Route `try_telegram_reply` / `try_telegram_react` / `try_telegram_edit` / `inject_provenance` through a new gate (likely `crate::channel::gated_outbound_for_agent(home, agent, text) -> Result<()>`) that mirrors `gated_notify`'s contract but for agent-driven outbound.
- The gate must check `outbound_authorized()` AND attribute to a specific agent so audit log records who emitted each outbound message (closes MX3 forensics gap from Sprint 20.5 Track 6).
- `inject_provenance` is the trickiest case — it's intentionally a "side-channel that can fail without blocking the main path" (`mcp/handlers.rs:393-407`). Failure visibility is via `tracing::warn!`. The gate should drop-with-warn semantics, not Err propagation, for this caller.

### High

**H1 — `try_telegram_reply` auto-orphan-cleanup contract creates DoS-shaped attack surface**

Where: `src/channel/telegram.rs:1293-1326` (`try_telegram_reply` doc comment + body).

The fn auto-cleans orphan bindings when send fails because the topic was deleted (per docstring: "auto-cleanup is required so the channel is in a self-healing state"). A separate `try_telegram_reply_no_cleanup` (line 1330) exists for callers that don't want this authority (e.g., S2d provenance per `inject_provenance:1378-1381`).

**Concern**: an agent that can trigger `reply` with crafted timing relative to a topic-recreation race could cause `try_telegram_reply` to mistakenly cleanup a valid binding. With C1 above unfixed, this is a chained DoS: prompt-injected agent → forces binding recreation → other agents' `reply` calls auto-clean the just-created binding → instance loses its Telegram surface entirely.

**Severity**: High in conjunction with C1. With C1 fixed (gate at agent-callable surface), this finding degrades to Medium — only authorized users could trigger the race. Still worth a per-binding rate-limit on cleanup or a "cleanup requires N consecutive failures" debounce.

**Fix shape**: defer until C1 lands; then evaluate whether debounce is needed.

### Medium

**M1 — Track 7 / Track 8 finding misframed `app/api_server.rs` as TUI→MCP bridge — actual bridge surface lives elsewhere**

`app/api_server.rs` is the **API server lifecycle wrapper** for app-mode (RAII over run-dir + OwnedFleet, spawn `crate::api::serve` thread, fleet auto-start). The misframing led the Track 7 / Track 8 finding to flag this file for un-deep audit when in fact:
- The deep risk surface (TUI input → MCP tool dispatch) lives in `src/app/commands.rs`, `src/app/dispatch.rs`, `src/app/overlay.rs` — Track C audit-scope, partially read.
- The MCP→Channel direct outbound bridge (the *real* C↔D coupling worth auditing) lives in `mcp/handlers.rs:74,88,102,399` — covered by this audit (C1).
- `app/api_server.rs` is a thin lifecycle layer with no input validation surface, no auth gate decisions, no command dispatch. Auditable; nothing to find.

**Severity**: Medium (audit-process artifact, not a code defect). Sprint 21 retro should record: "B+C+D triangulation Track 7+8 finding labeled the wrong file; the cross-area surface that mattered was `mcp/handlers.rs` channel routes."

**Fix shape**: docs-only — note in Sprint 22 backlog that the future "TUI→MCP cross-pass" should audit `commands.rs / dispatch.rs / overlay.rs` instead of `api_server.rs`.

### Low

**L1 — `inject_provenance` failure-visibility test pin (`mcp/handlers.rs:2187`) is load-bearing — preserve in any refactor**

Reading `mcp/handlers.rs` test section: `inject_provenance` failure-visibility pin (DESIGN §4 Q4) verifies `tracing::warn!` (not silent debug) on provenance failure. This is intentional per the inline comment: "provenance failure may signal a real routing bug worth an operator's attention." When C1 is fixed and `inject_provenance` routes through a gate that drops-with-warn, the existing pin must continue to assert warn-level visibility, not be relaxed to debug.

**Severity**: Low (preservation note, not a defect). Surface as test-preservation contract in Sprint 21 fix PR.

---

## Praise (3 sub-buckets)

### Replicate (worth applying elsewhere)

- **`crate::channel::gated_notify`** (`src/channel/mod.rs:243-259`) — channel-agnostic, fail-closed default, descriptive `tracing::debug!` on drop with op identification ("kind", "instance"). This is exactly the pattern C1's fix should replicate for agent-callable outbound. Single point of policy; localized to one fn; single source of truth for "who can send what outbound".
- **`is_authorized_recipient(allowlist: &Option<Vec<i64>>, user_id: i64) -> bool`** (`src/channel/auth.rs:24`) — primitive predicate that fail-closes on `None` (legacy deployments require explicit opt-in). Test coverage at lines 51-72 covers all four states (None / empty list / unlisted / listed). Mirror this primitive predicate pattern for any future "set membership with fail-closed legacy" check.

### Preserve as-is (load-bearing complexity, do NOT emulate)

- **`try_telegram_reply` vs `try_telegram_reply_no_cleanup` split** (`telegram.rs:1293, 1330`) — looks redundant ("why two fns that do almost the same thing?") but the split is intentional: `try_telegram_reply` has the *authority* to clean orphan bindings; `try_telegram_reply_no_cleanup` doesn't. This authority-split is correctly load-bearing; do not collapse the two fns. The PR-AS Discord-prep refactor explicitly preserved this split for the same reason.
- **`inject_provenance`'s "side-channel that doesn't propagate failure" contract** (`mcp/handlers.rs:393-407` + `telegram.rs:1376-1381`) — looks like a code smell ("why ignore the error?") but it's deliberate per DESIGN §6: provenance is best-effort observability, the main `delegate_task` result must not bubble side-channel failures. `tracing::warn!` (not silent) is the correct severity.

### Refactor-eventually (not urgent, ROI when touched)

- **`mcp/handlers.rs` channel-route arms** (lines 67-116) — duplicated structure: each tool extracts args, calls a `try_telegram_*` fn, returns `Ok` JSON or `Err` JSON. Could be unified once C1's gate lands as `gated_outbound_for_agent` — each arm becomes a single gate-call. Defer until C1 lands; then collapse.

---

## Coverage

| File / Section | Lines audited | Depth | Audited |
|---|---|---|---|
| `src/app/api_server.rs` | 1-130 | full | yes (130/130) |
| `src/mcp/handlers.rs:60-130` (Channel section: reply/react/edit/download) | ~70 | full | yes |
| `src/mcp/handlers.rs:380-420` (delegate_task inject_provenance call site) | ~40 | full | yes |
| `src/channel/telegram.rs:1283-1450` (bridge fn signatures + docstrings) | ~170 | signatures + docs only | yes (signatures) — bodies skim only |
| `src/channel/mod.rs:230-259` (gated_notify + outbound_authorized) | ~30 | full | yes |
| `src/channel/auth.rs` | 75 | full | yes |

**Honesty caveat**: `try_telegram_reply` / `try_telegram_react` / `try_telegram_edit` body code paths NOT line-by-line audited. Findings derive from signature + docstring + call-site grep. C1 risk holds because the bypass is signature-level (no gate parameter, no gated wrapper) — body details would only deepen the risk surface, not change the conclusion.

---

## Cross-area dependencies (dual-labeled per challenge #4)

- **MCP→Channel bridge** (this audit's C1): `reported_from: D (MCP)`, `primary_owner: A (Channel)`. Fix lives in Track A (channel-side gated_outbound_for_agent helper); call-site updates in Track D (mcp/handlers.rs).
- **`app/api_server.rs` lifecycle**: `reported_from: C (TUI)`, `primary_owner: B (daemon)`. The fn calls `crate::api::serve` (daemon API entry); `app/api_server.rs` is thin glue. Track B owns serve; Track C owns lifecycle wrapper.
- **`auto_start_fleet`**: `reported_from: C`, `primary_owner: C`. Self-contained.
- **`inject_provenance` failure-visibility contract**: `reported_from: D`, `primary_owner: A`. Test pin lives in Track D's `mcp/handlers.rs` test suite; underlying fn in Track A's `channel/telegram.rs`.

---

## Sprint 21 + Sprint 22 actionable

**Sprint 21 Phase 5b extension (Critical, must-ship before declaring Phase 2 closed)**:
1. **C1 fix — agent-callable outbound auth gate**:
   - Add `pub fn crate::channel::gated_outbound_for_agent(home, agent, text, severity) -> Result<()>` (channel-agnostic, mirrors `gated_notify` shape)
   - Route `mcp/handlers.rs:74, 88, 102, 399` through the gate; drop-with-warn semantics for `inject_provenance`, Err propagation for `reply` / `react` / `edit_message`.
   - Mirror `gated_notify` test coverage: gate fail-closed when `outbound_authorized() == false`.
   - Cross-track sequencing: this ships in Track A (gate primitive) + Track D (call-site updates) joint PR. Estimated ~80 LOC + 4 tests.

**Sprint 22 backlog**:
2. **H1 follow-up** — evaluate cleanup-debounce on `try_telegram_reply` after C1 lands.
3. **M1 follow-up** — record in Sprint 21 retro that Track 7+8 misframed `app/api_server.rs`. Future "TUI→MCP cross-pass" should target `commands.rs / dispatch.rs / overlay.rs`.
4. **L1 preservation note** — when C1 fix introduces the gate, ensure `inject_provenance_failure_visibility` test pin remains warn-level.

---

## Cross-pass appendix: dev-reviewer (TUI hot context)

*(To be added by dev-reviewer in their parallel cross-pass critique. Tracking issue: dev-reviewer reads this primary report + offers TUI-side blindspot analysis on whether `app/commands.rs / dispatch.rs / overlay.rs` truly contains additional TUI→MCP bridge attack surface beyond what this audit covers.)*

## Cross-pass appendix: dev-impl-1 (channel hot context)

(PR #216 + #218 + #219 author. Sprint 20 Track A + Sprint 20.5 Track 5 cross-validation hot context. ~200-400 字 blindspots-only critique post both primary push.)

### `gated_outbound_for_agent` proposal — **CONFIRM as natural PR-216 extension; signature preference**

reviewer-2's proposed helper is the right shape and lives in the right module. From PR #216 author angle, the existing primitive surface is:

- `crate::channel::auth::is_authorized_recipient(allowlist, user_id) -> bool` — predicate
- `Channel::outbound_authorized(&self) -> bool` — adapter-level decision (TelegramChannel ties it to allowlist non-empty)
- `crate::channel::gated_notify(channel, instance, severity, message, silent)` — daemon-driven outbound wrapper

`gated_outbound_for_agent` should be a **thin sibling of `gated_notify`** in `src/channel/mod.rs`, NOT a new abstraction. Recommended signature: `pub fn gated_outbound_for_agent(channel: &dyn Channel, caller_agent: &str, op_kind: OutboundOp) -> Result<(), ChannelError>` where `OutboundOp` is an enum (`Reply | React | Edit | Provenance`) — gate-only (Ok = proceed, Err = drop). Caller invokes the existing `try_telegram_*` fn after gate passes. **Two policy variants required** per L1 preservation: `reply / react / edit_message` propagate Err (operator-visible); `inject_provenance` drops-with-warn (matches existing failure-visibility contract). Either two helpers (`gated_outbound_for_agent` + `gated_outbound_for_agent_or_warn`) or single helper with `policy: GatePolicy { Strict, DropWithWarn }` enum param. I lean two helpers — clearer at call site, mirrors `gated_notify` (single call shape per surface).

### Bridge-fn enumeration (4) — **VERIFIED COMPLETE for current production paths**

`grep -rn "try_telegram_reply\|try_telegram_react\|try_telegram_edit\|inject_provenance" src/` in cross-pass worktree: all production callers go through `mcp/handlers.rs:74,88,102,399` (the four reviewer-2 listed). `try_telegram_reply_no_cleanup` is only invoked by `inject_provenance` (telegram.rs:1380, 1391) — transitively gated. `notify_telegram` / `notify_telegram_silent` are only called from `Channel::notify` impl (telegram.rs:1999, 2001) — already gated by PR #216. **Latent surface to flag**: `Channel::send` (bail-stub today, telegram.rs:1860 returns `MsgRef { id: "0" }` placeholder per Sprint 20 Track A H1/H4) — if a future T2 dispatcher wires it into mcp/handlers.rs, that wiring must also route through `gated_outbound_for_agent`. Sprint 21 Phase 5b extension PR should add a structural pin test: `Channel::send` is bail-stub OR gated, never naked.

### Sprint 20.5 Track 7 framing — **A angle was call-graph-correct; vindication, not error**

My Sprint 20.5 Track 5 cross-validation §Missed-findings #3 explicitly wrote: *"the channel-bridge actually lives in `mcp/handlers.rs:74,88,102,399`... the cross-pass should be **C↔D for app-bridge AND A↔D for channel-bridge**."* I correctly named the call-graph node (mcp/handlers.rs channel routes), unlike Track 7's file-path framing. What I did NOT do was DISPROVE the C↔D bridge entirely (dev-reviewer's confession this audit). My A angle stayed additive to Track 7's framing rather than corrective. dev-reviewer's retro entry should note: A angle pointed at the right call graph; C angle pointed at the wrong one. Sprint 22 cross-validation discipline should require call-graph grep evidence (`rg crate::mcp:: src/app/`) before naming any cross-area bridge — would have caught Track 7 mistake at write-time, not audit-time.

### A angle blindspot summary (one item neither primary surfaced)

**Defense-in-depth nuance for cascade chain**: PR #219 closes Telegram inbound auth; C1's gate closes MCP→Channel egress. But the inter-agent ingress (agent A's `send_to_instance` MCP tool carries attacker-text into agent B's PTY → agent B prompt-injected → agent B issues `reply` MCP tool → C1 gate now fires) is the operative attack vector that C1 fix MUST close. PR #219 alone is insufficient because authorized-user A could launder attacker content through inter-agent send (or a tool result with unsanitized URL). **Both primaries describe the attack chain correctly but neither pins this nuance**: agent-callable outbound gate is load-bearing precisely BECAUSE inter-agent ingress is unauthenticated by design (intra-fleet trust). Worth one sentence in C1 fix PR doc-comment: "this gate is the second-line defense against prompt injection laundered through inter-agent communication; do not relax even if inbound auth tightens further."

---

*End of primary audit. Time spent: ~1.5h within 2h hard cap. Awaiting cross-pass appendices.*

---

## TUI/render-angle co-primary findings (dev-reviewer)

**Audit metadata** (per v1.2 §3 Tier-2 evidence chain — co-primary append)
- co-auditor: dev-reviewer (Sprint 20 Track C + Sprint 20.5 Track 7 cross-validation hot context)
- audit_mode: codebase_audit (collaborative co-primary, lead by reviewer-2)
- audited_head: c929e4b (reviewer-2 push HEAD)
- scope_source: dispatch task `t-20260427005707...` co-primary directive + reviewer-2 lead audit M1 reframe
- commands_run:
  - `rg -n "handle_tool|crate::mcp::|mcp::handlers|::handlers::" src/app/commands.rs src/app/dispatch.rs src/app/overlay.rs` → **0 hits**
  - `rg -n "crate::api::call|api::call\(" src/app/commands.rs src/app/dispatch.rs src/app/overlay.rs` → **0 hits**
  - `rg -n "crate::mcp::handlers::handle_tool|mcp::handlers::handle_tool|handle_tool\(" src/` → only `mcp/mod.rs:280` (stdio dispatcher) + `mcp/handlers.rs:1800+` (internal tests)
  - `rg -n "fn execute|Command::|Cmd::" src/app/commands.rs` → 1 entry point `execute(cmd: &str, ctx: &mut CommandCtx<'_>) -> bool` at line 27
  - Read of `app/api_server.rs:40-130`, `app/commands.rs:1-90` (entry + first command arm)
- files_scanned (from TUI angle, deep): `app/api_server.rs` (full read), `app/commands.rs` (entry + dispatch shape), `app/dispatch.rs` (entry surface grep only), `app/overlay.rs` (cross-ref only — Track C audit-of-record from Sprint 20)

---

### Confirm / dispute reviewer-2's M1 reframe — **CONFIRM, AND EXTEND**

reviewer-2 reframe: *"`app/api_server.rs` is NOT TUI→MCP bridge — real bridge in `app/commands.rs/dispatch.rs/overlay.rs` (Track C scope)"*.

From TUI angle: **`api_server.rs` is correctly framed as lifecycle wrapper, not bridge** ✅. But the second half of the reframe ("real bridge in commands/dispatch/overlay") **is also incorrect**. Direct grep of all three files for any of `handle_tool` / `crate::mcp::` / `mcp::handlers` / `crate::api::call` returns **0 hits**.

**The TUI→MCP "bridge" does not exist as conceptualized.** TUI commands and overlay handlers operate on **local process state directly**: `commands.rs::execute` mutates `Layout` / `AgentRegistry` / `fleet.yaml` file in-process; `dispatch.rs::dispatch` mutates `Overlay` / `Layout` state; `overlay.rs::handle_key` mutates overlay enum variants. None of these go through `mcp::handlers::handle_tool`.

The only `handle_tool` callers in the entire codebase are:
1. `src/mcp/mod.rs:280` — stdio MCP server dispatcher (called by external Claude Code / kiro-cli backends via stdin/stdout protocol, not by TUI)
2. `src/mcp/handlers.rs:1800+` — internal unit tests

**Implication for my Sprint 20.5 Track 7 cross-validation finding (which originally proposed this triangulation audit)**: I conflated **MCP server lifecycle thread spawn** (which TUI does start, via `start_api_server`) with **MCP tool entry surface** (which TUI does NOT have). The "B+C+D triangulation joint audit" suggestion was based on a wrong mental model — there is no C↔D bridge attack surface to deep-audit because there is no C↔D direct call path. **I should own this mistake**: my Track 7 finding stated the gap one level too coarse (file-name level instead of call-graph level). The gap that *did* matter (C1 in this audit, agent→MCP reply→Telegram bypass) was inside Track D + A scope all along, not C↔D.

Sprint 21 retro update for M1: not just "Track 7+8 misframed `app/api_server.rs`" but more precisely "Sprint 20.5 Track 7 cross-validation finding's framing of B+C+D triangulation was call-graph-level wrong; the genuine cross-area attack surface that mattered was MCP→Channel direct outbound (C1, surfaced this audit), entirely within D+A scope."

### Sprint 20.5 Track 7 sub-finding disproved: TUI overlay panic does NOT cascade kill API server thread

Track 7 cross-validation flagged: *"TUI overlay panic 是否 cascade kill API server thread (B/D coupling)"*.

Verification (audit-time, this PR): `app/api_server.rs:54-66` spawns the API server in **`std::thread::Builder::new().name("app_api_server").spawn(...)`** — a named separate thread. TUI render runs on the main thread; overlay handlers also on main thread. Rust thread panics **do not propagate across thread boundaries** unless explicit (e.g., `JoinHandle::join().unwrap()` from another thread).

`start_api_server` returns the `ApiGuard` immediately and **does not join the API server thread** (line 66: `.ok();` discards the JoinHandle). TUI main-thread panic → main thread unwinds → process exit signals OS to kill all threads (including API server) — but this is process termination semantics, not "panic cascade". The "cascade kill" failure mode I worried about does not exist as a separable risk; it's just normal process death.

**Track 7 sub-finding closed**: not a real attack surface. The named thread isolation is correct + intentional + documented (line 68 tracing::info "in-process API server started").

### TUI render trust-assumption observation (Low / informational)

TUI's `render.rs::build_summary` (Sprint 20 Track C audit-of-record) renders MCP-tool-produced content (task descriptions, decision contents) into the operator's view directly. Post-PR #220 D1 gate, only the decision's `author` can mutate `decision.content` — but in the prompt-injection threat model the author **is** the prompt-injected agent. So an agent that's prompt-injected via PTY (after Telegram inbound passes PR #219's `is_user_allowed`, AND then content from another agent's `send_to_instance` carrying attacker text reaches the agent's PTY) can plant attacker-controlled content into a decision it owns; TUI render then displays this content to the operator unmodified.

This is **not a code defect** — render fidelity is the design goal; sanitizing the rendered text would obscure legitimate technical content (e.g. code snippets in audit reports). But the **operator-side responsibility for visual sanitation** of MCP-rendered content should be documented somewhere (CONTRIBUTING / USAGE / threat model doc).

Severity: Low / informational. Not a Phase 2 blocker, but a Sprint 22+ doc backlog candidate: *"document TUI render trust assumption: rendered task / decision content is from agent-author, not validated by TUI; operator visually sanitizes."*

### Confirm reviewer-2's C1 from TUI angle — **NO MITIGATION IN TUI**

C1's attack surface is agent→MCP-`reply`-tool→Telegram bot API direct call. TUI **plays no defensive role** in this chain:
- TUI does not see / intercept MCP tool calls (they go via stdio between agent backend ↔ daemon, not via TUI)
- TUI render only observes side-effects after-the-fact (e.g., agent's PTY shows "I sent reply to telegram" log line)
- By the time TUI render shows anything, the leak has already happened
- Operator detection-via-TUI is post-hoc forensics, not prevention

**C1 verdict from TUI angle**: agree Critical, agree fix scope is Track A + Track D joint PR (gate primitive + call-site updates per reviewer-2's proposal). **No additional TUI-side fix surface exists.** The Sprint 21 Phase 5b extension reviewer-2 proposes is the only complete fix.

### TUI angle Sprint 22 actionable additions

5. **Document TUI render trust assumption** (informational) — append to USAGE.md or new threat model doc: rendered content from MCP tools (status_summary, decisions list, tasks list) is author-controlled, not TUI-validated. Operator responsible for visual sanitization. ~10 line doc PR.

6. **Sprint 21 retro entry** — record the Track 7 cross-validation framing error: cross-validation findings should be at **call-graph granularity** (which fn calls which fn), not file-path granularity (which file might be a bridge). I'll volunteer to draft the retro entry from my own perspective post-merge.

---

*End of TUI/render-angle co-primary append. Confirms reviewer-2's C1 (Critical) end-to-end with no TUI mitigation; confirms M1 reframe AND extends it (the entire TUI→MCP "bridge" doesn't exist as conceptualized); disproves Track 7 cascade-kill sub-finding via thread isolation read; adds 1 informational TUI-render trust observation.*
