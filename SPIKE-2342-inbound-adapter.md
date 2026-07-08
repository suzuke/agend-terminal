# Spike #2342 — Inbound conversational channel adapter (security-model-first)

**Status:** design spike, impl-gated behind lead/operator vet. No adapter code written.
**Author:** gapfix-dev. **Investigations:** mechanism / security-authority / routing+ingress (file:line cited inline; full notes in `scratchpad/spike-{mechanism,security,routing}.md`).

---

## 0. Conclusion (read this first)

**Feasible on the existing `Channel` trait — but the hard security gate is solved by _structural unprivilege_, not per-turn authority scoping.** An externally-triggered turn must run on a **dedicated resident "responder" instance that is powerless by construction**:

1. **Unbound** — never given a `binding.json`. Binding is the *sole* artifact granting git/FS-write power (`src/binding.rs::bind_full`), and the `agend-git` shim denies mutating subcommands when `is_bound(binding)` is false (`src/bin/agend-git.rs::classify:1035`). No binding ⇒ no git, no worktree, no cross-workspace write. #2158's guard-b (reject live cross-branch rebind, `binding.rs:296`) + mandatory operator-notification on out-of-dispatch self-claim (`binding.rs:427`) make any `bind_self` attempt both blocked and loud.
2. **Explicit restrictive `RoleKind::Conversational`** (new) — the MCP ACL exists (`tool_allowed_for_role_action`, `src/mcp/registry.rs:333`, enforced `mcp_proxy.rs:134-145`) but is **default-all-open** (no role ⇒ `return true` full authority). So the responder must declare a fail-closed role allowing only `reply`-to-bound-conversation; deny `bind_self`/`repo`/`task`/`send`-to-arbitrary/`config`/spawn/delete. A conversational instance with no role_kind is **rejected at spawn**.
3. **Backend sandbox ON** — the default `Backend::ClaudeCode` spawns `--dangerously-skip-permissions` (`src/backend.rs:403`), i.e. actively permission-bypassing. The responder instance must spawn WITHOUT that flag, in a restricted permission/no-tools mode (defense-in-depth below the MCP proxy).

This reuses three existing mechanisms (binding-grants-power *inverted*, RoleKind ACL, backend spawn args) + one new role variant, and directly neutralizes the #2158 amplification. Per-turn trust-label plumbing is the rejected heavier alternative (§2.2).

**Two premise corrections the lead must weigh (§5):** (a) **#1954 does NOT provide HTTP ingress** — it's ephemeral-backend headless; AgEnD has *no* HTTP server, and LINE *requires* an inbound webhook. (b) **Model the LINE adapter on Discord, not Telegram** — Telegram's `poll_event`/`ChannelEvent` path is vestigial.

---

## 1. Conversational binding on the `Channel` trait + minimal build sequence

**Gaps in the current trait (mechanism investigation):**
- `ChannelEvent::MessageIn` (`src/channel/event.rs:20-25`) carries only `binding / from(User{id,handle}) / payload(text) / ts` — **no group-vs-1:1 flag, no @mention detection** anywhere (`caps.rs`'s `MentionStyle` is outbound-render-only). → add `conversation_kind: Group|Direct` + `mentions_bot: bool` to `MessageIn`; detection happens in the adapter.
- Binding registry is strictly **1:1 `HashMap<instance, single_id>`** with a hardcoded **`"general"` fallback on miss** (`telegram/state.rs:71`, `discord/state.rs:17`). That default-*allow* fallback is a security bug on this path: an unrecognized conversation must **drop + log**, never route to `general`. Conversational routing = a new `conversation_id → dedicated_instance` map behind the existing `BindingRef`/`Channel::record_binding` abstraction (`binding.rs:23`, `mod.rs:339-396`), same shape as `topics.json`/`channel_to_instance`.

**Minimal build sequence (each step independently reviewable):**
1. Extend `ChannelEvent::MessageIn` with `conversation_kind` + `mentions_bot` (+ contract test still green).
2. `InboundRouter` gate order (all fail-closed, before any side-effect — #2369 pattern): **allowlist deny-default → @mention gate (group: only `@bot`; direct: all) → per-conversation rate-limit → enqueue to responder inbox.**
3. Dedicated-responder spawn/route: unbound + `RoleKind::Conversational` + sandboxed backend (§0).
4. LINE adapter (Discord-path-modeled, cargo-feature-gated) passing `run_registry_contract` (§4).
5. Invariant tests (§2.3).

---

## 2. Security model — the hard gate (the actual deliverable)

### 2.1 Threat: externally-triggered turn = amplified #2158
A `Task` sub-agent is identity-indistinguishable from its primary (`binding.rs:447`); #2158 hardened the "sub-agent silently rebinds parent + cross-workspace writes" surface. An **anonymous internet stranger** whose message can trigger a tool-capable turn is the *same class*, worse: prompt-injection → tool-use → rebind / git / exfil, with no operator in the loop. `externally-triggered ≠ operator-authorized` must be enforced by construction.

### 2.2 Why per-turn scoping is the WRONG primitive here
- **It doesn't exist.** Inject (`instance.rs::handle_inject`) and inbox `[AGEND-MSG]` delivery carry **zero code-enforced scoping** (security investigation Q5). Provenance markers (`[user:… via telegram]`, `[AGEND-AUTO]`) are **prose-only instructions** (`src/instructions.rs:355-380`), not boundaries — and a prose tag is precisely what prompt-injection defeats.
- **Backends default to unsafe.** `--dangerously-skip-permissions` (`backend.rs:403`) means the turn's tools are wide open below the daemon.
- Plumbing a trust label end-to-end (inject → backend → MCP ACL + `operator_gate`) is a large cross-cutting build that relies on backend cooperation that doesn't exist. **Rejected** in favour of structural unprivilege (§0), which rides the existing grain: *power comes from the binding + role + spawn args, so withhold all three.*

### 2.3 Concrete convergence (what to touch) + invariants
- **No auto-bind** for responder instances (dispatch auto-bind is the normal grant path; conversational spawn opts out). Linchpin: `agend-git::classify` deny-when-unbound.
- **New `RoleKind::Conversational`** in the #2344/#2367 registry (`src/fleet/mod.rs:433` + `mcp/registry.rs`): allowlist = `{reply}` to the *bound* conversation only. Fail-closed: reject a conversational instance lacking it (counter the default-all-open).
- **Restricted backend spawn** (drop `--dangerously-skip-permissions`; restricted permission mode).
- **Side-effect-free inbound handler.** The operator gate (`operator_gate::check_operation_allowed`, `api/mod.rs:611`) is **socket-scoped and bypassed by in-process channel handlers** — `telegram/inbound.rs:395` creates board tasks directly. The conversational path must therefore do *nothing* but enqueue to the responder's inbox (never board writes / operator ops), allowlist-checked first (#2369, `inbound.rs:263-313`).
- **Invariant test (#2158 echo):** an external inbound turn cannot emit a bind/rebind event or any cross-workspace write; a conversational instance has no `binding.json`; the ACL table denies `bind_self`/`repo`/`task` for `RoleKind::Conversational`.

---

## 3. conversation-id ↔ resident instance session routing

- **No daemon "conversation session" concept exists** (routing Q2): instances are UUID-keyed (`InstanceId`, `types.rs:7`; `fleet::resolve_uuid`, `fleet/mod.rs:58`); Claude's own resume is *working-dir*-keyed (`backend.rs:701-888`), not conversation-id.
- **Design:** one **dedicated resident responder per allowlisted conversation (group)**; the instance's own rolling context *is* the session → same conversation has memory, different conversations are different instances ⇒ no cross-talk (satisfies the AC). "Resident, not cold-spawn" = the instance persists across messages (spawned on first allowlisted contact or pre-provisioned in `fleet.yaml`), resolved via the existing binding→instance pattern (`topics.json`/`channel_to_instance`, routing Q3).
- **Injection** rides the live path: `route_and_deliver → inbox::enqueue_with_idle_hint → inject_with_target_gated` (`agent/mod.rs:2824`) — no new inject mechanism (`compose_aware_send` was removed in #1065).

---

## 4. LINE adapter passing `run_registry_contract`

- `contract.rs` = **9 registry-side invariants** (round-trip, unknown-is-total, double-take-None, last-write-wins, stable `kind()`/`display_tag`, repeatable `attach_registry`; `contract.rs:50-201`); **excludes `send`/`edit`/`poll_event`** by its own scope (`contract.rs:11-18`).
- LINE registers via `register_active_channel` keyed by `kind()="line"` (`mod.rs:137-140`) + declares `ChannelCapabilities` (`caps.rs`).
- **Model on the trait-faithful Discord `protocol.rs` mapper (uses `ChannelEvent`), NOT Telegram** (whose `poll_event` returns `None`; real inbound bypasses `ChannelEvent`). Passing the contract = implement `record_binding`/resolve round-trip for the LINE conversation-id map + stable `kind()`/`display_tag`.

---

## 5. Premise corrections for lead vet

1. **Ingress — #1954 does NOT give us HTTP webhook ingress** (routing Q4). AgEnD has *no* HTTP server (only the loopback-only control socket, `ipc.rs:1-26`); Telegram=long-poll, Discord=outbound Gateway WS, GitHub=poll. **LINE requires an inbound HTTPS webhook (no long-poll option).** This is the biggest new build **and the untrusted boundary**. **Recommendation:** a **sidecar webhook receiver** (separate process) doing TLS + LINE `X-Line-Signature` HMAC verification, forwarding *validated* `ChannelEvent`s over the existing loopback IPC — keeping the public untrusted HTTP surface **out of the daemon** (matches the abstraction doc's sidecar model, §3.7/§4). Building HTTP ingress into the daemon is the higher-risk alternative.
2. **Rate limit is new** (routing Q5): `RateBudget` (`caps.rs:127-141`) is a never-consulted, outbound-shaped struct. A per-conversation/per-sender **inbound** limiter is new work at the `InboundRouter`, before injection.

---

## 6. Impl gating
Design only. No adapter code until lead/operator vet — especially: `RoleKind::Conversational` ACL, unbound-responder + sandboxed spawn, and sidecar-vs-in-daemon ingress. Security-sensitive → recommend **dialectic vet** on §2 and §5.1.
