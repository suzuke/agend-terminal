# Sprint 22-25 Roadmap (4-sprint framework)

**Status**: general-approved 2026-04-27 per multi-perspective challenge round (m-20260427033102125964-257 thread + 7 修正 + 2 amendments).
**Predecessor**: Sprint 21 close decision `d-20260427040650778876-12`.
**Authority**: per operator delegation `d-20260427025246010573-0` (general代operator全權), framework approval = operator approval.

---

## 問題 → 方案 (一句話)

**問題**: Sprint 21 收 10 PRs 後，剩餘 backlog 含 (1) Phase 5b architectural critical path, (2) workaround scattered 缺 triage, (3) 28 backlog items 缺 hygiene, (4) docs/marketing 沒系統化生產。直接全塞一個 sprint 會 mix concerns + scope 失控。

**方案**: 4-sprint sequential split, 每 sprint 單一主題, P0 critical-path 永遠先 (Sprint 22 P0 = 安全 promise critical path), 跨 sprint 依賴明標。

---

## Sprint 22 — Polish (security promise close + workaround triage)

**Goal**: 收尾 Sprint 21 deferred 軌, 把所有 "workaround / TODO / FIXME" 帶 enforcement promise 的東西落地。

### P0 — Phase 5b hard-cut critical path
- **Scope**: `Channel::send_from_agent` trait extension full migration — 從 Sprint 21 PR #223 gradual bridge 收尾到 hard-cut。Per-instance `outbound_capabilities` 強制 declarative, 移除 fallback default。
- **Why critical path**: security promise 仍依賴 Sprint 21 Phase 1+2 fail-closed default + Phase 5a/b architectural enforcement。Phase 5b 是 capability promise 的最後一步, 沒做完 capability 系統還是 "soft claim"。
- **Dispatch**: 4-perspective challenge round → assign impl-1 OR impl-2 (兩位都 ready)。
- **Reviewer**: dev-reviewer (Tier-2), Phase 5b Touch security adjacent paths → potential auto-Critical scope。
- **Estimated scope**: M-L (架構性 trait extension, ~150-300 LOC + tests + capability table audit)。
- **Predecessor**: Sprint 21 PR #223 (gradual bridge) + PR #224 (real dispatcher)。

### P1 — Workaround triage per-condition checklist
- **Scope**: 列出 codebase 內所有 `// TODO / // FIXME / // HACK / // workaround` comments, 加 per-condition checklist:
  - Sprint 22 inline-fix
  - Sprint 22+ defer with explicit unblock condition
  - Drop (no longer relevant)
- **Initial candidates**:
  - **F-NEW-DAEMON-HEALTH-CLASSIFIER-1** (per general m-20260427040529383849-303): long-idle alive vs hung classifier 區分。Repro: 30+min idle agent → daemon flag hung 即使 state=ready。Fix: `last_input_responsive_at` 軌跡 OR `idle_long` vs `hung_unresponsive` 二態 split。Evidence: 04:00 dev-impl-1 false alarm。LOC est ~30-50 + tests。
  - **EXEMPTED_LEGACY_FILES sweep** (per PR #227): 15 files in `tests/spawn_rationale_audit.rs` allowlist 各帶 `TODO Sprint 22 sweep` marker, 一一 fix-or-justify 收掉。
  - **F1 cosmetic** (per PR #227 dev-reviewer): `tests/spawn_rationale_audit.rs` doc 5-line vs impl 10-line lookback 不一致, 對齊 "10 lines" canonical (impl-2 next-touch 順手即可)。
- **Dispatch**: impl-1 OR impl-2 in parallel with P0。
- **Reviewer**: dev-reviewer (single, per-condition variance allows single-pass)。

### P2 — supervisor.rs F6/F7 call-site refactor + real graceful-join
- **Scope**: PR #227 deferred — Phase 5 ships *primitive* (`save_metadata_batch` helper at `agent_ops.rs:105` + 3 atomic tests), Sprint 22 swaps the call sites in `supervisor.rs`。Real graceful-join refactor 替代 fire-and-forget where viable (per spawn rationale audit each-site judgement)。
- **Predecessor**: PR #227 atomic metadata helper exists ready。
- **Reviewer**: dev-reviewer Tier-2。
- **Risk**: supervisor.rs 高 churn area, conflict-prone — sequence after P0 PR settled。

### P3 — TODO/FIXME triage + §3.5.5 amendment + mechanical polish
- **§3.5.5 amendment** (per general m-20260427034029128729-273):
  - Add exception clause: "LOW + docs-only protocol PR (≤50 LOC, no production code change beyond instructions.rs template strings) → single reviewer OR operator self-merge OK"
  - Mid-scope+ protocol PRs 仍 dual-reviewer enforce
  - Amendment PR 自己走 dual reviewer (meta-recursion fine)
  - PR #226 = case study evidence (operator 03:35 self-merge)
- **TODO/FIXME triage**: P1 workaround triage 的姊妹軌, 走相同 checklist。
- **Mechanical polish**: 任何 doc/impl 不一致 (e.g. F1 cosmetic) batch 收。

### Sprint 22 close gate
- All P0 + P1 + P2 + P3 PRs merged
- §3.5.5 amendment posted as decision (project scope)
- Workaround backlog reduced to "explicit deferred only" (no orphan TODOs)
- F-NEW-DAEMON-HEALTH-CLASSIFIER-1 either fixed inline OR moved to Sprint 23 design refactor

---

## Sprint 23 — Refactor + 28 backlog hygiene

**Goal**: design refactor for accumulated tech-debt + 28 specific backlog items batch hygiene。

### P0 — Design refactor (TBD post Sprint 22 surface)
- 待 Sprint 22 surface "在 polish 過程中暴露的 design-level smells"。Phase 5b post-migration 預期會暴露 trait/capability boundary 是否 over/under-spec。
- **Dispatch**: depends on Sprint 22 outcome — placeholder for now, finalize after Sprint 22 close。

### P1 — 28 backlog items hygiene
- **Source**: Sprint 20 SYNTHESIS.md + Sprint 20.5 cross-validation deferred items
- **Per-item action** (3-state):
  - In-fix: scope-fits-Sprint-23, dispatch as PR
  - Defer: explicit reason + Sprint 24+ pin
  - Drop: no longer relevant, archive
- **Reviewer load expectation**: ~5-10 PRs across batch, dev-reviewer + dev-reviewer-2 split。

### Sprint 23 close gate
- 28 backlog items 全 closed (in-fix / defer-with-reason / drop)
- Design refactor PRs merged with explicit "why this redesign now" rationale
- Codebase no longer carries "investigated but undocumented" backlog

---

## Sprint 24 — Documentation (含 cross-team coord)

**Goal**: 系統化 user-facing + dev-facing docs。

### P0 — User-facing docs
- README + USAGE.md + quickstart (PR #218 baseline) refresh per Sprint 21-23 changes
- API surface docs (capability table, channel auth model, MCP tool catalog)

### P1 — Cross-team migration guide coord (with ts-lead)
- **Dependency**: ts-lead's Sprint 2 deliverable `docs/migration-to-agend-terminal.md` (3-phase split: A=CLI+fleet, B=Backend+MCP, C=Why+Steps+Incompat)
- **Cross-team review**: dev-reviewer-2 audits Rust API correctness side, ts-reviewer audits TS-side, ts-lead merge gate
- **My role**: provide high-friction migration scope input (already sent ts-lead m-20260427034403... 6 priority items mapped to Phase A/B/C)
- **Sync point**: ts Sprint 2 wrap doc 會 include "ready for agend-terminal Sprint 24 input" signal

### P2 — Dev-facing docs
- Architecture overview (Channel/Auth/Capability layered model post Phase 5b)
- Contributing guide (worktree mandatory §10.4 + spawn rationale §10.5 enforcement explained)
- FLEET-DEV-PROTOCOL-v1.md changelog (v1.0 → v1.1 → v1.2 + amendments through Sprint 22)

### Sprint 24 close gate
- Migration guide cross-team merged
- Architecture + contributing + protocol changelog docs published

---

## Sprint 25 — Marketing assets (boundary 明示)

**Goal**: Marketing-facing assets for project surface / outreach。

### Boundary明示 (per general 7 修正 #4)
- **Dev team scope**: docs/code/PR/CI/test surface — what we know
- **NOT in dev scope**: marketing copy / branding / outreach strategy / social media — operator OR external designate
- **Dev team contributes**: technical accuracy of marketing copy (e.g. fact-check claims, capability table verification)
- **Dev team does NOT decide**: positioning / audience / channel selection

### P0 — Technical fact-check pass
- Review any marketing draft for factual accuracy (versions, capability claims, security posture statements)
- Output: pass-fail per claim with evidence pointer

### Sprint 25 close gate
- Marketing assets shipped per operator/designate spec
- Dev team's fact-check pass logged as decision

---

## Cross-sprint dependencies

```
Sprint 22 P0 (Phase 5b hard-cut)
  └─→ Sprint 22 P2 (supervisor.rs refactor uses Phase 5b capability table)
  └─→ Sprint 23 P0 (design refactor surface depends on Phase 5b architecture)
  └─→ Sprint 24 P2 (architecture docs reflect Phase 5b)

Sprint 22 P3 (§3.5.5 amendment)
  └─→ Sprint 24 P2 (FLEET-DEV-PROTOCOL changelog includes amendment)

Sprint 22 P1 (workaround triage incl daemon classifier)
  └─→ Sprint 23 P0 (design refactor may absorb un-inline-fixable workarounds)

ts Sprint 2 (migration guide A/B/C)
  └─→ Sprint 24 P1 (cross-team merged + dev review pass)
```

---

## 7 修正 + 2 amendments (challenge round outcome)

### 7 修正 (from 4-perspective challenge round on framework draft)
1. **Sprint 22 P0 = critical path label** (was "polish" generic) — perspective A insisted security promise can't be "polish-tier"。
2. **Sprint 25 boundary 明示 dev vs marketing** (was assumed "dev does marketing too") — perspective B (operator-vantage) flagged scope creep risk。
3. **Cross-sprint dependency graph 必含** (was implicit) — perspective C (downstream maintainer-vantage) need explicit edge map for prioritization。
4. **§3.5.5 amendment recursion 自走 dual reviewer** (was unspecified) — perspective D (protocol-vantage) caught meta-amendment edge case。
5. **F-NEW-DAEMON-HEALTH-CLASSIFIER-1 加 P1** (was missed) — incident-driven addition post-04:00 false alarm。
6. **Sprint 23 P0 placeholder** (was over-specified pre-Sprint-22) — Sprint 22 surface dependency means design refactor scope can't be locked yet。
7. **ts cross-team coord 進 Sprint 24 P1** (was Sprint 25) — earlier than originally planned, ts-lead Sprint 2 ready aligns with Sprint 24 windows。

### 2 amendments (post-approval refinements)
1. **Phase 5b dispatch 4-perspective challenge round mandatory** (per operator 17:01 strategic order) — applies to Sprint 22 P0 unconditionally。
2. **EXEMPTED_LEGACY_FILES sweep 進 Sprint 22 P1** (per PR #227 dev-reviewer praise of anti-growth contract) — operationalize the "shrink to zero" intent。

---

## Sprint 22 dispatch gate (immediate next)

1. ✅ Sprint 21 close (this decision predecessor d-20260427040650778876-12)
2. ✅ This roadmap merged (LOW docs-only, single reviewer or operator self-merge per pending §3.5.5 amendment)
3. **4-perspective challenge round on Sprint 22 P0** (Phase 5b hard-cut) — mandatory per amendment #1
4. **Dispatch P0 → impl-1 OR impl-2** (whoever ready first, both pre-cleared)
5. **Parallel dispatch P1** (workaround triage) → other impl
6. **P2 / P3 sequence** after P0 settled

---

## Notes
- Roadmap is *living* — Sprint 22 surface may change Sprint 23 scope, Sprint 23 design surface may change Sprint 24 architecture docs, etc. Update as decisions accrue。
- Each sprint close should produce a `d-` decision summarizing actual outcomes vs roadmap (drift visible)。
- Cross-team coord (ts ↔ dev) tracked as separate sync points, not absorbed into single-team roadmap。
