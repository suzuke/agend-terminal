# Spike #2342 — Hardening decision-manifest (resolving the dialectic's 6 blockers)

**Status:** design only, no impl code. Gated behind reviewer4 + dev2 **re-vet**. **REV2** — dev2 re-vet CLOSED B5-main/B6a and flagged a load-bearing residual in B4 (only `inject` was gated; `send`/`spawn`/`mcp_tool` equally injection-equivalent) → B4 reworked to per-method default-DENY capability + server-side enqueue target; B5 patched with 2 residuals.
**Inputs:** reviewer4 §2 verdict `VERDICT-2342-secmodel.md` (F1–F5) + dev2 §5.1 verdict (task `t-…66713-7`, A1–A5) + dev2 re-vet (B4/B5 residuals). Both REJECTED the as-specified spike; three pillars directionally right, individually necessary-not-sufficient.
**Author:** gapfix-dev (original spike author).

---

## 0. Conclusion (read first)

The structural-unprivilege model **can** hold, but only once the load-bearing hole is closed and each pillar is made an *invariant* rather than a *convention*. Two framing corrections drive the ordering:

1. **dev2 A1 (IPC auth) dominates everything.** The loopback control socket allows every injection-equivalent method (`inject`/`send`/`spawn`/`mcp_tool`/…) by method-shape, not caller-identity, so a spoofed local process drives any bound instance directly — bypassing the sidecar, HMAC, role, and all three pillars. The fix is a **per-method default-DENY capability** model (not connection-auth + a single-method special-case), with the sidecar holding exactly one `enqueue-only` capability and the daemon resolving the target server-side. **No inbound adapter ships before this lands.** It is a *pre-existing fleet vulnerability*, fixed first, independently.
2. **Pillars 1+2 carry the load; pillar 3 (backend sandbox) is defense-in-depth only** — and is an empirically-unverified integration claim (reviewer4 F5), so the model must NOT rest on it. It is gated on a real spawn probe, not asserted.

Each blocker below → concrete mechanism + touchpoint + **[PRE-EXISTING]** (independently fixable, fleet-valuable) or **[#2342]** + build phase. Consolidated build order in §7.

---

## Blocker 1 — Conversational hard un-bindable  (reviewer4 F1, high)

**Hole:** binding is granted by the *dispatcher*, not self-controlled. `send(kind=task,branch=X,instance=<responder>)` from any peer → `dispatch_auto_bind_lease_with_source_and_chain` (`src/mcp/handlers/comms.rs:269-279`) → `bind_full` (`src/binding.rs:270-441`), which has **zero role awareness**; #2158 guard-b (`binding.rs:296-317`) only blocks a live *cross-branch rebind*, explicitly allows first-bind. Opting out of *spawn* auto-bind (spike §2.3) does nothing against a later *dispatch* bind.

**Mechanism:** make `role_kind == Conversational` a HARD-REFUSE at the single bind chokepoint. `bind_full` is the one funnel for both dispatch-auto-bind and `bind_self` → add a role resolution + guard there: if target's resolved `role_kind == Conversational` → `Err(BindRefused)`. Add a second early refuse in `dispatch_auto_bind_lease_*` (`comms.rs:269-279`) for a clear caller error + defense-in-depth. Result: a Conversational instance can *never* acquire a `binding.json` → `agend-git::classify` deny-when-unbound (`src/bin/agend-git.rs:1091-1101`,`:1062-1065`) holds as an **invariant**, not a convention.
**Touchpoints:** `binding.rs:270-441` (load-bearing guard), `comms.rs:269-279` (early refuse), resolve via `fleet::role_kind_for_instance`.
**Tag:** **[#2342]** mechanism, but it lands a *reusable role-gate hook in `bind_full`* (fleet-valuable primitive). **Test:** `bind_full(target=Conversational)→Err`; invariant test "no Conversational instance ever has binding.json".
**Build:** Phase 1b (needs the RoleKind variant from Blocker 2 first).

---

## Blocker 2 — Conversational role = explorer-MINUS, fail-closed *without* fleet-wide regression  (reviewer4 F2 / dev2 A5, high)

**Hole:** dynamic-spawn responder has no `role_kind` ingress — `def_create_instance` schema lacks the field (`src/mcp/tools.rs:100-119`); `spawn_single_instance` leaves it unset (`spawn.rs:173-195`); a known-instance-but-absent role resolves to `Ok(None)` → all-open (`mcp_proxy.rs:280`, `registry.rs:189`). So a spawned responder boots fully privileged. Globally flipping "absent→deny" would break every legitimately-role-less existing instance.

**Mechanism (compat-preserving, two parts):**
- **(a) New `RoleKind::Conversational` = explorer-MINUS.** The exhaustive `match RoleKind` (`src/mcp/registry.rs:203-213`) forces a compile-time subset for the new variant (reviewer4-confirmed strength). Start from `explorer` (read-only) and **drop even further**: NO `repo` (critical — `repo action=checkout bind:true` self-grants a binding, defeating Blocker 1), NO `bind_self`, NO `task`, NO `send`-to-arbitrary. Keep only `reply` to the *bound* conversation. NOT reviewer-based (reviewer keeps `repo`/`ci`/`task`, `registry.rs:709-718`).
- **(b) Scope fail-closed to the inbound path, not fleet-wide.** Do NOT change the global `absent→all-open` default (that preserves existing no-role instances). Instead: a **channel-inbound turn may only be delivered to a target whose resolved `role_kind == Conversational`; anything else (absent/None/other) → drop+log** (fail-closed at the inbound router, not at the global ACL). For v1, **responders are pre-provisioned `fleet.yaml` entries** (operator declares `role_kind: conversational`) — no new `create_instance` spawn surface, which *also* satisfies "resident, not cold-spawn" and naturally caps instance count (Blocker 5). Auto-spawn + a `role_kind` create_instance ingress + spawn-time reject is a deferred enhancement, not v1.
**Touchpoints:** `registry.rs:203-213`/`:709-718` (variant + subset), `mcp_proxy.rs:280`/`registry.rs:189` (unchanged — compat), inbound router (new target-role gate), `fleet/mod.rs:433` (RoleKind enum).
**Tag:** **[#2342]** (Conversational variant + inbound target-role gate). The missing `create_instance` role ingress is **[PRE-EXISTING]** but *sidestepped* in v1 by pre-provisioning.
**Build:** Phase 1a (foundational; Blocker 1 depends on it).

---

## Blocker 3 — real no-tools backend sandbox (defense-in-depth, empirically gated)  (reviewer4 F5, med)

**Hole:** `Backend::ClaudeCode` hardcodes `args:&["--dangerously-skip-permissions"]` (`src/backend.rs:401-403`); there's no per-instance knob to omit it, AND simply removing it makes a headless (no-operator) responder **hang on the interactive approval prompt** — not run tool-less. Claude has no bare "no-tools" flag.

**Mechanism:**
- **(a) per-instance backend-args override** — the preset is fixed today; add a per-instance args override so a responder can spawn with a *tool-denying* headless profile instead of the blanket bypass. **[PRE-EXISTING]** gap (fleet-valuable: any restricted role wants this).
- **(b) tool-deny headless profile** — candidates to *probe*, not assert: `claude -p` (headless, non-interactive: unapproved tools are auto-denied, not prompted, so it neither hangs nor uses tools) with `--disallowedTools`/an empty allowed set, or a restrictive `--permission-mode`/settings deny-all. **Exact flag MUST be validated by a real spawn probe** (empirical negative control) per reviewer4 F5 / §3.17 — static review is insufficient for backend-spawn semantics.
- **(c) framing:** this is **depth only**. The model's guarantee rests on Blockers 1+2 (unbindable + explorer-MINUS ACL) + Blocker 4 (IPC). If the sandbox flag proves imperfect, the feature is still safe; it must never be the sole barrier.
**Touchpoints:** `backend.rs:401-403` (args override), a new spawn-probe test.
**Tag:** **[PRE-EXISTING]** (per-instance args) + **[#2342]** (tool-deny profile + probe).
**Build:** Phase 3 (parallelizable; not load-bearing → does not block the primary gates).

---

## Blocker 4 — per-method default-DENY IPC capability + server-side enqueue target  (dev2 A1 + re-vet residual, LOAD-BEARING, high)  ⟵ REV2

**Hole (re-vet-sharpened):** authenticating the connection and special-casing only `inject` (rev1 B4c) is insufficient — the control socket dispatches *many* injection-equivalent direct methods, ALL "operator-transport always allowed", gated by method-shape not caller identity: `INJECT` (`src/api/mod.rs:621`), `SEND` (`:626`), `SPAWN` (`:625`), `MCP_TOOL` (`:645`), plus `KILL`/`DELETE`/`CREATE_TEAM`/… (`api/mod.rs:620-645`; `operator_gate.rs:319-326`; `api/mod.rs:204-206`). A blanket-authenticated sidecar still reaches `send`/`spawn`/`mcp_tool` → A1 stays open. dev2 CLOSED B5-main/B6a; B4 needs this rework.

**Mechanism (per dev2 re-vet):**
- **(4a) per-method default-DENY capability authorization.** EVERY control-socket method (`inject`/`send`/`spawn`/`mcp_tool`/`kill`/`delete`/…) requires the caller's token to hold that method's capability; absent capability → DENY. "enqueue-only" is only *provable* against a default-deny base — never a default-allow with one method special-cased. The sidecar token grants exactly ONE capability: `enqueue-to-responder-inbox`; it cannot reach `inject`/`send`/`spawn`/`mcp_tool` at all.
- **(4b) server-side enqueue-target resolution + role gate.** The DAEMON (not the sidecar) resolves conversation-id → responder instance and enforces `role_kind == Conversational` (else drop+log). The sidecar's enqueue request carries the **conversation-id ONLY** — it must NOT name a target instance, else a compromised sidecar aims its permitted enqueue at a bound instance = equivalent injection. Target choice is the server's, keyed on the allowlisted conversation map.
- **(4c) token hardening.** Token file mode **0600** + missing/unreadable token → **fail-CLOSED** (refuse connection) + **per-boot rotation** + **publish-before-accept** (token written+fsynced before the socket accepts). ⚠ `store::atomic_write` (used by `ipc.rs:47-49 write_port`) does **not** `set_permissions` → a token written that way is world-readable; the token writer must chmod 0600 (and ideally teach `atomic_write` an optional mode — fleet-valuable).
**Touchpoints:** `api/mod.rs:620-645` (per-method dispatch — capability check per arm), `operator_gate.rs:319-326`/`api/mod.rs:204-206` (replace method-shape gate with capability gate), `ipc.rs:47-49` (token write + 0600 + fail-closed), new capability-token + capability-set layer.
**Tag:** **[PRE-EXISTING, LOAD-BEARING]** (per-method capability auth + token 0600 = general fleet hardening) + **[#2342]** (enqueue-only capability + server-side conversation→responder resolution).
**Build:** **Phase 0a — FIRST. Nothing ships before this.**

---

## Blocker 5 — anti-replay + global budget + instance cap  (dev2 A2/A3 — main CLOSED, 2 residuals patched)  ⟵ REV2

**Hole:** (A2) webhook HMAC signs only the body — no timestamp/nonce → a captured payload **replays** and re-triggers a turn. (A3) rate-limiting sits *after* the IPC boundary, no global compute budget, resident spawn has no instance cap → quota/compute exhaustion. *dev2 re-vet CLOSED the main mechanism; two residuals below.*

**Mechanism (all at/ before the untrusted boundary):**
- **anti-replay in the sidecar:** LINE `eventId` dedup (bounded LRU) + timestamp freshness window + **constant-time** HMAC compare; verify over body+timestamp. **⟵ REV2 residual (a):** the dedup set must **persist** (small on-disk store) OR the ts freshness window must be **aggressively narrowed** — otherwise a sidecar restart clears the in-memory LRU and an in-window replay passes. Prefer persisted dedup keyed on `eventId` (survives restart); a narrow ts window alone still admits fast replays.
- **layered budgets:** (i) per-conversation/per-sender **pre-throttle at the sidecar** (before IPC); **⟵ REV2 residual (b):** the **global inbound/compute budget must ALSO be enforced at the sidecar (pre-IPC)**, not only in-daemon (post-IPC) — a post-IPC-only ceiling still lets a flood reach and load the daemon. Enforce the global cap at the sidecar first; keep the in-daemon ceiling as defense-in-depth. (ii) **global inbound budget** in the daemon (`RateBudget` `caps.rs:127-141` is an unused struct to wire); (iii) **responder instance cap** — inherently satisfied by pre-provisioned-only responders (Blocker 2a); explicit ceiling if auto-spawn is added; (iv) per-conversation **compute/turn budget**.
**Touchpoints:** sidecar (persisted dedup + pre-IPC global budget + pre-throttle), in-daemon inbound-budget module, instance-cap at route/spawn, `caps.rs:127-141`.
**Tag:** **[#2342]** (inbound budget + replay are feature-specific).
**Build:** Phase 2 (with the sidecar).

---

## Blocker 6 — all-resolver fail-closed drop+log + independent enqueue-only handler  (reviewer4 F3/F4, dev2 A4, high)

**Hole:** (F3/A4) an unrecognized conversation falls back to the **full-capability `general`** instance (`src/channel/telegram/inbound.rs:160-178`; `src/channel/discord/adapter.rs:142-155`) — and tests treat this as *normal* (`discord/…/tests.rs:533`,`:1617-1618`). This is a **live** default-allow today, even without LINE. (F4) after the allowlist gate, the existing telegram handler still runs status-summary enqueue + `加 task:` board-write via `tasks::handle(home,"operator",…)` **directly**, bypassing the socket-scoped `operator_gate` (`api/mod.rs:611`; `telegram/inbound.rs:~396`).

**Mechanism:**
- **(a) every resolver fail-closed:** telegram `topic_registry`, discord `channel_to_instance`, and the new LINE map must **drop+log** on an unrecognized/unallowlisted conversation — never fall back to `general` or any default. **Invert the existing tests** (`tests.rs:533`,`:1617-1618`) to assert drop+log. **[PRE-EXISTING]** (live hole across telegram/discord today) → fix independently, fleet-valuable.
- **(b) independent enqueue-only inbound handler:** the conversational path is a *separate* code path doing only `allowlist → @mention-gate → rate-limit → enqueue-to-responder-inbox`. It must NOT reuse the telegram handler's status/`加task` branches, must NOT call `tasks::handle`, must NOT write the board or touch any operator op. **[#2342]**. (Note: the existing handler's direct `tasks::handle(home,"operator",…)` forging "operator" identity is itself a pre-existing gate-bypass worth a separate ticket; the conversational path simply must not replicate it.)
**Touchpoints:** `telegram/inbound.rs:160-178`, `discord/adapter.rs:142-155`, `discord/…/tests.rs:533`/`:1617-1618`, new inbound handler module.
**Tag:** **[PRE-EXISTING]** (general-fallback drop+log) + **[#2342]** (enqueue-only handler).
**Build:** Phase 0b (general-fallback, independent) + Phase 2c (handler, with adapter).

---

## 7. Consolidated staging / build order (de-risk: pre-existing hardening lands + is reviewed first)

**Phase 0 — pre-existing fleet vulnerabilities, independent PRs, land + review BEFORE any inbound adapter:**
- **P0a [LOAD-BEARING]** per-method default-DENY IPC capability (covers `inject`/`send`/`spawn`/`mcp_tool`/…) + server-side enqueue-target resolution + enqueue-only sidecar capability + token 0600/fail-closed/per-boot-rotation (Blocker 4). *Nothing proceeds until merged.*
- **P0b** all-resolver fail-closed drop+log, invert general-fallback tests (Blocker 6a).
- **P0c** per-instance backend-args override (Blocker 3a enabler).

**Phase 1 — the primary structural defense (#2342 role + gates):**
- **P1a** `RoleKind::Conversational` = explorer-MINUS + exhaustive-match ACL subset (Blocker 2a).
- **P1b** Conversational hard un-bindable — role-gate in `bind_full` + dispatch refuse (Blocker 1). *depends P1a.*
- **P1c** channel-inbound target-role gate, pre-provisioned responders only (Blocker 2b).

**Phase 2 — untrusted ingress + budgets:**
- **P2a** sidecar webhook (TLS + X-Line-Signature HMAC + eventId dedup + ts-window + constant-time) → authenticated enqueue-only IPC (Blocker 5 replay + Blocker 4 sidecar side). *depends P0a.*
- **P2b** global inbound budget + responder cap + sidecar pre-throttle (Blocker 5).
- **P2c** independent enqueue-only inbound handler + LINE adapter (Discord/`protocol.rs` path) (Blocker 6b).

**Phase 3 — defense-in-depth (empirically gated, non-blocking):**
- **P3** responder tool-deny backend profile via P0c knob + **mandatory spawn probe** (Blocker 3b/c).

**Re-vet mapping:** reviewer4 re-attacks §2 → Blockers 1,2,3,6b (does the model still leak authority?). dev2 re-attacks ingress/IPC → Blockers 4,5,6a (is the IPC/replay/fallback boundary closed?). Each verifies the mechanism actually shuts *their* finding.

## 8. Impl gating
Design only. No adapter/guard code until reviewer4 + dev2 re-vet confirm each mechanism closes its finding. Then **phased** impl (Phase 0 first, independently reviewable).
