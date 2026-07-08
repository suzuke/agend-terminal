# Spike #2342 — Hardening decision-manifest (resolving the dialectic's 6 blockers)

**Status:** design only, no impl code. Gated behind reviewer4 + dev2 **re-vet**.
**Inputs:** reviewer4 §2 verdict `VERDICT-2342-secmodel.md` (F1–F5) + dev2 §5.1 verdict (task `t-…66713-7`, A1–A5). Both REJECTED the as-specified spike; three pillars directionally right, individually necessary-not-sufficient.
**Author:** gapfix-dev (original spike author).

---

## 0. Conclusion (read first)

The structural-unprivilege model **can** hold, but only once the load-bearing hole is closed and each pillar is made an *invariant* rather than a *convention*. Two framing corrections drive the ordering:

1. **dev2 A1 (IPC auth) dominates everything.** An unauthenticated loopback control socket where `inject` is gated by method-shape not caller-identity means a spoofed local process injects any bound instance directly — bypassing the sidecar, HMAC, role, and all three pillars. **No inbound adapter may ship before IPC auth lands.** It is a *pre-existing fleet vulnerability*, fixed first, independently.
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

## Blocker 4 — IPC authentication + sidecar capability-narrowing  (dev2 A1, LOAD-BEARING, high)

**Hole:** the daemon control socket is unauthenticated loopback TCP (`src/ipc.rs:3-10`) and `inject` is a direct method the operator gate *always* allows because it gates by method-shape, not caller identity (`operator_gate.rs:319-326`,`:78`; `api/mod.rs:204-206`). Any local process / spoofed sidecar injects any bound instance = #2158 amplification that bypasses sidecar+HMAC+role. This nullifies the whole "untrusted surface stays outside the daemon" premise.

**Mechanism:**
- **(a) authenticate the IPC** — per-connection capability token (shared secret / cookie handshake) on the control socket. **[PRE-EXISTING]** load-bearing fleet vulnerability (a compromised local process can already drive the daemon today) → fix FIRST, independently, own review.
- **(b) capability-scope the sidecar token** to a SINGLE method: `enqueue-to-responder-inbox`. The sidecar cannot call `inject`, any direct-method, or operator ops — its token doesn't grant them. Even a fully-compromised sidecar can only drop a message into a responder's inbox, which then runs the allowlist + Conversational-role + unbindable gates.
- **(c) identity-gate `inject`** — the direct `inject` method must require an authenticated identity, not just method-shape (`api/mod.rs:204-206`, `operator_gate.rs:319-326`). **[PRE-EXISTING]**.
**Touchpoints:** `ipc.rs:3-10` (auth handshake), `operator_gate.rs:319-326`/`api/mod.rs:204-206` (identity-gate inject), new capability-token layer.
**Tag:** **[PRE-EXISTING]** (IPC auth + identity-gated inject — the load-bearing foundation) + **[#2342]** (enqueue-only scoped token).
**Build:** **Phase 0a — FIRST. Nothing ships before this.**

---

## Blocker 5 — anti-replay + global budget + instance cap  (dev2 A2/A3, high/med)

**Hole:** (A2) webhook HMAC signs only the body — no timestamp/nonce → a captured payload **replays** and re-triggers a turn. (A3) rate-limiting sits *after* the IPC boundary, there's no global compute budget, and resident spawn has no instance cap → quota/compute exhaustion.

**Mechanism (all at/ before the untrusted boundary):**
- **anti-replay in the sidecar:** LINE `eventId` dedup (bounded LRU of seen ids) + timestamp freshness window (reject stale) + **constant-time** HMAC compare (no timing oracle); verify over body+timestamp.
- **layered budgets:** (i) per-conversation/per-sender **pre-throttle at the sidecar** (before IPC, closing A3's "limit is post-IPC"); (ii) a **global inbound budget** in the daemon (total inbound turns/min across all conversations — no limiter exists today, `RateBudget` `caps.rs:127-141` is an unused struct); (iii) **responder instance cap** — inherently satisfied by pre-provisioned-only responders (Blocker 2a); enforce an explicit ceiling if/when auto-spawn is added; (iv) a per-conversation **compute/turn budget** to bound quota burn.
**Touchpoints:** sidecar (replay + pre-throttle), new inbound-budget module in daemon, instance-cap at route/spawn, `caps.rs:127-141` (wire the dormant `RateBudget`).
**Tag:** **[#2342]** (inbound budget + replay are feature-specific); the *absence* of any enforced limiter is a pre-existing gap but the inbound limiter itself is new.
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
- **P0a [LOAD-BEARING]** IPC authentication + identity-gated `inject` + enqueue-only capability token (Blocker 4). *Nothing proceeds until merged.*
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
