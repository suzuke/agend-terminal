# PLAN: Sprint 44 — Push-time semantic gate (code-level enforcement of dev push claims, reviewer SHA freshness, prior-art grounding, daemon QoL)

**Date:** 2026-04-30
**Status:** Operator GO 2026-04-30T13:55 UTC (general m-20260430135522392196-658). Tier-1 docs-only self-merge on this PLAN PR; M5 hotfix dispatchable immediately; Phase A bundle holds for M5 merge.
**Branch:** `lead/sprint44-plan-push-time-gate`
**Origin:** general m-20260430132952774332-647 onboarding + first task to fresh dev team (lead = claude / dev = kiro-cli / reviewer = codex)
**Process:** PLAN-first 4-perspective synthesis (STRUCTURAL · PRIOR-ART · MINIMAL-DELTA · COST-BENEFIT)
**Scope decision:** project decision `d-20260430134400004302-10`; operator GO answers folded throughout — see §8 for resolved candidate questions and §9 for execution sequence.

---

## 0. KISS gate (§0) + operator constraint

- **What real problem does this solve?** Four recurring fleet-coordination failures across two prior dev teams (Sprint 39/41/42/43):
  1. dev push claims (§3.5.13 "no other changes" / "byte-equal verified" / "scope follows dispatch spec X") routinely diverge from the actual diff. dev self-checks via syntactic keyword grep, reviewer catches the semantic gap, costing ≥1 review round per occurrence. Tightening §3.5.13 wording in the protocol has been tried twice and failed — pure-text rules don't enforce.
  2. Reviewer issues verdicts against a stale local checkout (no `git fetch -f` before review). PR #380 r2/r3 produced two false-REJECT verdicts caught only by lead's manual grep; otherwise dev would have run two corrective rounds against an imaginary regression.
  3. PRIOR-ART claims occasionally reference functions/symbols that do not exist in the repo (dev2 m-343 hallucinated function), cascading into 3+ silent-substitution rejects.
  4. Daemon QoL bugs that drag on every dispatch: (a) `kind=task` `target_instance` field is silently rerouted via team-orchestrator resolution and then rejected as "cannot send to self"; (b) ci-watch notifications for force-pushed PRs leave stale `ci-pass`/`ci-fail` events in inbox without invalidation.
- **Would deletion break anyone?** No deletion — all four are net-add gates. Failure mode = continued recurring rejects + ad-hoc lead-side firefighting + protocol bloat.
- **Operator hard constraint:** *"盡可能從程式端的流程去規範、而不是僅僅只有 prompt 的口頭要求"* (m-20260430132952774332-647). Code-level gate ≥80% of effort; protocol amendment ≤20%. Pure-text protocol additions are explicitly the failure mode being repaired, not the fix.

---

## 1. Verified current state (lead minimal-delta vantage)

### Existing infrastructure relevant to the gate

| Component | File | Status |
|---|---|---|
| Unified send (MCP) | `src/mcp/handlers/comms.rs` | `handle_unified_send` routes `kind=task` → `handle_delegate_task` → `resolve_team_orchestrator` (line 159). |
| Team resolver | `src/teams.rs:330` `resolve_team_orchestrator` | Returns orchestrator if `name` matches a team; `Ok(None)` otherwise. **No instance-name-first preference.** |
| API send self-check | `src/api/handlers/messaging.rs:29` | Rejects `sender == target`. Fires AFTER team resolution, so `target_instance: dev` (instance) collides with team template `dev` whose orchestrator is `lead`. |
| Inbox message struct | `src/inbox.rs::InboxMessage` | Has `correlation_id`, `parent_id`, `task_id`, `reviewed_head`, `kind`. **No `superseded_by` / cancellation field.** |
| ci-watch | `src/daemon/ci_watch.rs:1283` `inbox::enqueue` | Already dedupes via `last_notified_head_sha` (line 1098), but old inbox messages from prior SHAs are not invalidated. |
| reviewed_head metadata | `src/inbox.rs:202` def, `src/api/handlers/messaging.rs:116` store | **Pass-through only — zero validation.** |
| Pre-commit hook | `.git/hooks/pre-commit` (rustfmt, ~33 LOC), `scripts/install-hooks.sh` | Per-clone install. No pre-push hook. |
| Worktree | `src/worktree.rs` (491 LOC), `src/worktree_cleanup.rs` (344 LOC) | §10.4 mandatory. `.git/hooks` not inherited per-worktree. |
| `dispatch_tracking.rs` | scope-source-of-truth for dispatched task spec | Available for cross-ref by claim verifier. |

### Bug #4a root cause confirmed (lead reproduced live)

`mcp__agend-terminal__send target_instance=dev request_kind=task` → daemon returns `{"error": "cannot send to self"}` consistently. Tracing path:

1. `handle_unified_send` → `kind=task` branch → `handle_delegate_task` (`comms.rs:147`).
2. `handle_delegate_task` line 159: `resolve_team_orchestrator(home, "dev")`.
3. `teams.rs::resolve_team_orchestrator` finds team `dev` (declared in `fleet.yaml templates.dev` with `orchestrator: lead`) → returns `Some("lead")`.
4. `resolved_target` becomes `"lead"`; API call from sender `lead` with target `lead` hits `messaging.rs:29` → reject.

The bug is **template-name vs instance-name shadowing** in the resolver. `fleet.yaml` declares both `instances.dev: backend: kiro-cli` AND `templates.dev: orchestrator: lead`; the resolver doesn't prefer instance-first. `handle_send_to_instance` (plain `kind=null` send) skips team resolution entirely — that's why the workaround `target_instance=dev message=…` (no `request_kind`) works. dev S5 confirms the same root cause independently.

### Bug #4b root cause confirmed

`src/daemon/ci_watch.rs:1283` `inbox::enqueue` adds new ci-watch notification on every SHA change; no caller scans for prior `kind=ci-watch` messages with the same `repo+branch` to mark as superseded. `InboxMessage` has no `superseded_by` field, so even with caller co-operation there's no place to record the supersede edge.

---

## 2. Three perspectives (synthesis)

### 2.1 lead — MINIMAL-DELTA

Six fixes (M1-M6) split into three layered phases. The push-time claim verifier (M1) is the single highest-leverage change — it converts §3.5.13 from prompt-rule to mechanical gate, defeats the recurring class entirely, and provides the integration point for M4 (hallucinated-fn) since both share the claim-parsing front-end.

Worktree-friendly hook delivery via `core.hooksPath = scripts/hooks/` keeps M2 a zero-dep shell layer — no Python/Node toolchain creep. M3 reuses the existing `reviewed_head` field (zero schema churn). M5 is an 8-LOC pre-check (already proven necessary by live reproduction). M6 needs one new field on `InboxMessage` plus a forward-only tag at enqueue time — ~90 LOC.

Aggregate code-LOC : protocol-amendment-LOC ≈ **91 / 9**, well above the operator's 80/20 floor.

### 2.2 dev (kiro) STRUCTURAL — m-654

- **S1**: claim parser as new `src/claim_verifier.rs` module + new CLI subcommand `agend verify-push` + daemon API endpoint, **~480 LOC**. Reuses `store.rs::SchemaVersioned` for versioned claim AST. 5 sentence→check mappings drafted: `"no other changes"` → `git diff --stat` scope filter; `"byte-equal verified"` → `git diff <path>` empty check; `"scope follows dispatch spec X"` → `dispatch_tracking.json` cross-ref; `"only formatting"` → `rustfmt --check` on diff; `"deps unchanged"` → `Cargo.lock` diff empty.
- **S2**: pure shell + `core.hooksPath` chosen (zero dep). `git push --no-verify` as emergency override. **~80 LOC**. Daemon-side complementary gate recommended (hook bypass still caught by API endpoint).
- **S3**: `reviewed_head` is pass-through-only across `inbox.rs:202` / `comms.rs:362-392` / `messaging.rs:116` — **zero validation**. Add `git ls-remote` SHA fetch + compare in `handle_report_result`. **~80 LOC**. Open Q: sync vs async fetch in MCP handler.
- **S4**: `claim_verifier.rs` extension. Light-only (ripgrep) recommended by dev; reviewer P4 recommended middle (syn) + light fallback. **~120 LOC** for either path; this is the main perspective divergence — see COST-BENEFIT (§2.4).
- **S5**: bug confirmed, fix is `handle_delegate_task` post-resolve `if *sender == target` pre-check with informative error "task target resolved to sender (team orchestrator loop)". **~8 LOC + 1 test**. Open Q: does `handle_send_to_instance` need parallel team resolution for consistency, or should the resolver prefer instance-first globally? (Folded into §3.5 IMPL design — see §3.)
- **S6**: forward-only tag chosen (no inbox scan). Add `superseded_by: Option<String>` to `InboxMessage` + emit at `ci_check_repo` SHA-change event. **~90 LOC** (60 struct + 30 read filter).

### 2.3 reviewer (codex) PRIOR-ART — m-650

- **P1** Claim-vs-diff: Changesets/PR-bot pattern (cost med, scope-text vs structured-diff gap), commitlint (low cost, format-only — insufficient on its own), GitHub semantic-pull-request action (low, similar limit), Datadog OBA-style (high, full assertion-DSL). Verdict: borrow Changesets's "declared changeset vs file-diff" primitive into our scope-text grammar.
- **P2** Hooks: `core.hooksPath` (low cost, multi-worktree-friendly, recommended), cargo xtask (med, full Rust integration), lefthook (med, extra binary), pre-commit (med, Python burden), cargo-husky (med, weak worktree story). Recommendation matches dev S2.
- **P3** SHA freshness: GitHub "stale review dismissal on push" + GitLab re-approval — directly portable to our `reviewed_head` field. stack-base diff hash (Sprint 22 §10 E2.4) suitable for cherry-pick/backport edge cases.
- **P4** Hallucinated-fn: layered recommendation — middle (syn AST walk) primary + light (ripgrep) fallback for 4-person fleet scale. Heavy LSP / rustdoc-JSON disproportionate.
- **P5** 80/20 verdict: feasible **with one compromise** — hallucination cannot be 100% solved by any lightweight gate; daemon+hook covers ~80%, residual via spot-check + future heavy symbol verification.

### 2.4 reviewer (codex) COST-BENEFIT — m-656

| M | LOC | complexity | severity | verdict | rationale |
|---|---|---|---|---|---|
| M1 push-time claim verifier | 480 | high | high | **KEEP** | P1 (Changesets/OBA claim-vs-diff) hits the recurring class directly; heavy but necessary; ship minimal validatable claim schema first. |
| M2 pre-push hook | 80 | low | high | **KEEP** | P2 `core.hooksPath` is the cheapest defense-in-depth and worktree-friendly. |
| M3 reviewer SHA-gate | 80 | med | med-high | **KEEP** | P3 GitHub stale-review-dismissal directly portable to `reviewed_head` — low-cost-high-value. |
| M4 hallucinated-fn | 120 | med | low-med | **SHRINK** | NOT ripgrep-only per P4; **switch to syn-lite + rg fallback** (layered). Ripgrep-only produces noise from re-exports / generated code. |
| M5 self-route bug | 8 | trivial | low (但頻踩) | **RAISE** | Cheap to fix and reproducible; pull forward to Phase A as ground-clearing. |
| M6 ci-watch stale | 90 | low | low-med | **KEEP** | Aligns with P3's "forward-only state-invalidation" lens; reduces noise without inbox rescan. |

`code:protocol` 驗算：M1 protocol ≈15% + M3 protocol ≈10%, otherwise 0% → aggregate ≈ **91/9**, ≥80/20 floor ✓.

**Phase regrouping (operator §13 #7 amendment — M5 pulled out as standalone hotfix):**
- **Hotfix = M5 only** — 8 LOC + 1 test, ~30min cycle; ships ahead of Phase A so plain `kind=task` dispatch unblocks immediately and the workaround retires.
- **A = M1 + M2** — 主路徑 (claim gate core + hook layer)
- **B = M3 + M6** — 一致性 / 新鮮度 (reviewed_head SHA gate + ci-watch supersede)
- **C = M4** — syn-lite + rg fallback per operator §13 #6 (hallucinated-fn extension on M1's `claim_verifier.rs`)

Why M5 is hotfix-first (operator answer §13 #7): M5 is the cheapest fix in the entire sprint, the bug is reproducible now, and folding it into Phase A delays a 30-min unblock by the full Phase A cycle (~1.5 days). Pull-forward retires the workaround documented in §1 verified state.

---

## 3. BLOCKING items

The COST-BENEFIT round produced no BLOCKING items requiring PLAN amendment before IMPL dispatch — all six Ms are KEEP/SHRINK/RAISE with clear rationale. The single divergence (M4 grammar choice) is captured in §13 #6 for operator pick.

**One latent BLOCKING risk** identified by lead: if M3's `git ls-remote` sync call exceeds the MCP handler tick budget under load, fail-closed degrades dispatch latency. Mitigation pre-IMPL: require dev to bench `git ls-remote` against this repo (~50ms cold, <10ms with HTTP keepalive). If >100ms p95, switch to async fetch with cached PR HEAD per-PR (30s TTL). Captured in §13 #5.

---

## 4. Phase rollout (final, post operator §13 GO)

| Phase | Scope | LOC est | Tier | Reviewer config | Trigger |
|---|---|---|---|---|---|
| **Hotfix** | M5 self-route bug — `handle_delegate_task` post-resolve `if *sender == target` pre-check | ~8 LOC + 1 test | Tier-1 single (LOW, trivial, isolated) | codex single PRIMARY | Dispatchable on PLAN merge |
| **A** | M1 push-time claim verifier + M2 pre-push hook (hard-reject from day 1 per §13 #3) | ~560 (480+80) | **Tier-2 dual** (touches dispatch core + new daemon endpoint + new shell hook surface) | codex single PRIMARY + lead cross-vantage check (§13 reviewer config) | After M5 merge |
| **B** | M3 reviewer SHA-staleness gate (fail-closed per §13 #5) + M6 ci-watch supersede (`InboxMessage::superseded_by`) | ~170 (80+90) | **Tier-2 dual** (touches inbox schema + verdict path) | codex single PRIMARY + lead cross-vantage | After Phase A merge |
| **C** | M4 hallucinated-fn extension on M1's `claim_verifier.rs` — syn-lite primary + rg fallback per §13 #6 | ~120 | **Tier-2 dual** (operator §13 #8: cross-team blast radius justifies upgrade from Tier-1) | codex single PRIMARY + lead cross-vantage | After Phase B merge |

Total **~858 LOC** across **4 PRs** (1 hotfix + 3 phases), est. **2.5–3.5 working days** for dev (kiro) sequential.

`code:protocol` ratio per phase:
- Hotfix: 8 code / 0 protocol ≈ **100/0** ✓
- Phase A: 560 code / ~75 protocol (§3.5 claim format spec one-block) ≈ **88/12** ✓
- Phase B: 170 code / ~25 protocol (§3.5.13 "reviewed_head MUST match PR HEAD at verdict time") ≈ **87/13** ✓
- Phase C: 120 code / 0 protocol ≈ **100/0** ✓
- **Aggregate ~91/9** vs operator floor 80/20 ✓

### Reviewer configuration rationale (operator §13 reviewer-config answer)

The fresh dev team has only one reviewer (codex). For Tier-2 dual on Phase A/B/C:
- **Primary**: codex single PRIMARY (full review, VERIFIED/REJECTED verdict)
- **Cross-vantage**: lead reads the PR independently (different vantage from author/orchestrator role) and posts cross-vantage attestation in the PR comment thread + inbox `kind=report` to satisfy §3.5.4 dual-review.
- Self-merge after both verdicts converge VERIFIED + CI green per §3.6.3.
- This avoids cross-team borrow (which the prior fleet did) since the new fleet is single-team. Documented in operator GO m-658.

---

## 5. §3.5 / §3.6 application per phase

### Phase A
- **§3.5.10 production-path-coupled**: claim verifier ships with integration test that pushes a real PR-shaped commit through the daemon endpoint; pre-push hook tested via shell harness against scripted divergence cases.
- **§3.5.11 test-first**: RED commit lands first — claim "no other changes" against a multi-file diff must reject; refactor to GREEN.
- **§3.5.13 verdict externalization**: applies to the M1 PR's own review, not the gate it ships.

### Phase B
- **§3.5.13 amendment**: tighten "reviewed_head MUST match PR HEAD at verdict time" — daemon enforces; reviewer no longer self-asserts.
- **§3.5.10**: M3 SHA gate exercised against an intentionally-stale verdict in test.

### Phase C
- **§3.5.5 LOW docs-only exception** does not apply (code change). Tier-1 single review per §3.5.4 default for non-shared-behavior code.

---

## 6. Cumulative risks

| Risk | Mitigation |
|---|---|
| M1 claim parser false-positives reject legitimate pushes | Strict opt-in initial deployment (claim phrases must be explicitly known; unknown phrases pass-through without check). v1.1 expands grammar based on rejection log. |
| M1 + M4 grammar collision (same parser two consumers) | Versioned via `SchemaVersioned`; M4 lands as v1.1 of the parser, not v1.0 reshape. |
| M3 sync `git ls-remote` blocks MCP handler tick | Cache GH PR HEAD per-PR with 30s TTL; fall back to "warn but accept" on fetch failure. (Open Q to operator — see §9.) |
| M5 fix changes `handle_delegate_task` semantics for non-conflicted team names | Add explicit instance-first lookup; team resolution remains as fallback. Test covers `instance == team-template` collision case. |
| M6 `superseded_by` field churns all `InboxMessage` consumers | `Option<String>` default `None` — backward-compatible for readers that ignore the field. |
| Phase A scope creep (M1 grows beyond 480 LOC) | Hard cap: defer M1.5 features (multi-line claim grammar, regex-template DSL) to Sprint 45. v1.0 grammar is the 5 sentences listed in S1. |

---

## 7. Out of scope (this Sprint)

- Rewriting §3.5.13 / §3.6.1 / §3.6 protocol text wholesale. Operator: code enforces, protocol describes the gate (one-line update only).
- 16-pattern reviewer prompt catalog reshape — atrophies naturally after M1+M4 land.
- IMPL of any phase. This is PLAN-only.
- SchemaVersioned extension to `fleet.yaml` (operator previously discussed — out-of-scope creep).
- Bash-script regression test migration (sibling Sprint 42 territory).
- ci-watch event-source change to GitHub `pull_request.synchronize` webhook (poll-based dedup retained — webhook infra is a separate sprint).
- Heavy symbol verification (rust-analyzer LSP / rustdoc-JSON) for M4 — light/middle path covers 80/20; heavy reserved for Sprint 45+ if rejection log demands.

---

## 8. §13 questions — RESOLVED (operator GO m-658)

| # | Question | Operator answer | Folded into |
|---|---|---|---|
| 1 | Phase A immediate dispatch or wait Sprint 41/43? | Sprint 41/43 already closed — no dep | §4 Hotfix immediate, A after M5 |
| 2 | M1 grammar v1.0 — 5 sentences or expand? | **5 sentences only**, KISS, expansion needs amendment | §1 verified state, §6 risk row "M1.5 deferred" |
| 3 | M1 deployment hard reject vs soft mode? | **Hard reject from day 1** — code-level enforce, soft mode = wasted work + future amendment | §4 Phase A row, §5 Phase A §3.5.10 |
| 4 | M2 emergency override path? | `git push --no-verify` standard escape hatch — operator-signed token = over-engineering for single-user repo | §4 Phase A spec |
| 5 | M3 fetch failure fallback? | **Fail-closed (reject)** — false-negative cost (bad verdict) ≫ false-positive (manual retry) | §4 Phase B row, §6 risk row M3 |
| 6 | M4 grammar — light or middle? | **syn-lite + rg fallback** (reviewer COST-BENEFIT middle path) — Phase C only | §4 Phase C row |
| 7 | M5 bundling — Phase A or hotfix? | **Standalone hotfix first** — 8 LOC ~30min cycle; bundling delays unblock by full Phase A cycle (~1.5d) | §4 Hotfix row, §1 verified state workaround sunset |
| 8 | Phase C tier — Tier-1 or Tier-2? | **Tier-2 dual** — inbox schema touch + cross-team blast radius | §4 Phase C row |
| 9 | PLAN-doc self-merge — §3.5.5 LOW docs-only OK? | YES, Tier-1 docs-only self-merge | §9 status |

All 9 RESOLVED.

---

## 9. Status & execution sequence

**PLAN authored 2026-04-30T13:43 UTC, operator GO 13:55 UTC (m-658), 9/9 §13 RESOLVED.**

4-perspective synthesis ledger:
- STRUCTURAL (dev kiro) — m-654 ✓
- PRIOR-ART (reviewer codex) — m-650 ✓
- MINIMAL-DELTA (lead claude) — §2.1 + §1 verified state ✓
- COST-BENEFIT (reviewer codex cross-vantage) — m-656 ✓

Execution sequence (per operator GO):

1. **Lead self-merges this PLAN PR** (Tier-1 docs-only per §3.5.5; codex single PRIMARY; CI green).
2. **Lead dispatches M5 hotfix** to dev (kiro) — 8 LOC pre-check in `handle_delegate_task` + 1 test, ~30min cycle, separate PR.
3. **M5 merges → workaround retires** → Phase A bundle dispatched (M1+M2, ~560 LOC, Tier-2 dual: codex PRIMARY + lead cross-vantage).
4. **Phase A merge → Phase B** dispatched (M3+M6, ~170 LOC, Tier-2 dual).
5. **Phase B merge → Phase C** dispatched (M4, ~120 LOC, Tier-2 dual upgraded per §13 #8).
