# PLAN: Sprint 36 — Cargo dependency cleanup

**Date:** 2026-04-29
**Status:** plan-first; awaiting operator GO before any `Cargo.toml` / `Cargo.lock` / `src/*` writes
**Branch:** `docs/sprint36-deps-cleanup-plan` → PR for plan-doc only
**Origin:** operator directive via general m-20260429152250325238-74; source audit by `claude-a1f200` (operator-vetted)
**Process:** 4-perspective challenge round (Sprint 32 model) — synthesis below
**Scope decision:** project decision `d-20260429152323966190-3`

---

## 0. KISS gate (§0)

- **What real problem does this solve?** `Cargo.lock` carries 9+ duplicate transitive groups (reqwest 0.11/0.12, hyper 0.14/1.x, rustls 0.21/0.23, http 0.2/1.x, base64 0.21/0.22, darling 0.13/0.23, hyper-rustls 0.24/0.27, serde_with 1/newer) primarily from `teloxide 0.13`. The `serde_yaml` crate is officially deprecated by its author. Cumulative cost: build time + binary size + maintenance surface (deprecated crate as a critical-path dep).
- **Would deletion break anyone?** Delete this *plan* — no users affected; deps remain as-is, debt accumulates. Delete the *cleanup work* — depends on tier (see §6).

The KISS pressure here is real but bounded. Cross-vantage perspective (codex reviewer) flags that aggressive cleanup carries reintroduction-churn risk if not fixture-anchored.

---

## 1. Verified current state (lead minimal-delta)

Grep / file inventory at HEAD `5ece359`:

| Item (audit ordering) | Direct deps state | LOC sites in `src/*` | MSRV constraint |
|---|---|---|---|
| 1 `serde_yaml = "0.9.34+deprecated"` | `+deprecated` annotation in Cargo.toml line 65 | 4 sites in `src/fleet.rs` (per audit); dev structural counts 41 refs across 8 files | n/a |
| 2 `teloxide = "0.13"` | rustls feature, default-features=false (line 68) | 4114 LOC in `src/channel/telegram.rs`; dev counts 62 refs | dev confirms 0.16 needs 1.82, 0.17 needs ≤1.82 — toolchain 1.95 OK |
| 3 `crossbeam = "0.8"` | umbrella crate (line 56) | dev counts 67 refs across 16 files; only `crossbeam::channel::*` used | n/a |
| 4 `sysinfo = "0.30"` | not in head of Cargo.toml above | dev counts 4 refs in 2 files (`instance_monitor.rs:56,90,125`, `api/mod.rs:353`) | n/a |
| 5 `async-trait = "0.1"` | (line 76) | dev counts 3 refs in `daemon/ci_watch.rs:49,94,1494` | needs ≥1.75; toolchain 1.95 |
| 6 `getrandom = "0.2"` | indirect-only (transitive of `ring`?) — verify | dev counts 2 refs in `auth_cookie.rs:32,34` | n/a |
| 7 `fs2 = "0.4.3"` | (line 77) | dev counts 6 refs in 5 files (`bootstrap/mod.rs:228`, `store.rs:75,303`, `daemon/mod.rs:156`, `inbox.rs:28,29`) | `fs4` feature-equivalent |
| 8 `embed-resource = "2"` | Windows build-only | build.rs only (not `src/*`) | n/a |

**MSRV state**: no `rust-version` field in Cargo.toml; no `rust-toolchain.toml`. Implicit MSRV = whatever the dev's toolchain. dev confirms current toolchain 1.95.0; native async-fn-in-trait stabilized 1.75. `async-trait` removal is safely above threshold but adopting an explicit `rust-version = "1.75"` (or higher) is a separate decision.

---

## 2. Three perspectives (challenge round summary)

### 2.1 lead — minimal-delta
Sprint 35 just shipped `~700 LOC` removal cleanly. Sprint 36 has stronger KISS payoff (deprecated crate + cascade elimination from teloxide), but each of 8 items has different risk shape. The smallest-viable Sprint 36 is **mechanical-only batch** (items 3 / 5 / 6 / 8 — pure name swap / attribute removal / version bump). Items 1, 2, 4, 7 each carry semantic risk and merit isolated PRs with §3.5.10 fixture per change.

### 2.2 dev (kiro) — STRUCTURAL (m-20260429152649545833-81)
8 items all feasible. Per-item impact table (LOC + breaking + cascade + risk class) shows:
- teloxide is the riskiest (62 refs, behavior-divergent) but breaking surface to 0.17 is only ~20 LOC of adapter changes.
- async-trait removal: zero MSRV risk on toolchain 1.95.
- Recommended **4-PR partition**: PR-A teloxide / PR-B serde_yaml / PR-C crossbeam / PR-D batch (4-8). Total ~440 LOC.
- **Operator's dispatch order confirmed correct**: teloxide → serde_yaml → crossbeam → sysinfo → cleanup batch. No hidden coupling found.
- **Deviations from audit's 8-item list flagged**: dev silently dropped `embed-resource` (audit #8) and added `iana-time-zone` (new 9th item). Synthesis defers to operator's original 8 (see §9).

### 2.3 reviewer (codex) — PRIOR-ART / CROSS-VANTAGE (m-20260429160018164326-94)
**Notable caveat**: reviewer's report indicates the original dispatch was already marked read; reviewer reconstructed scope from local trace + repo evidence rather than from the dispatch text directly. Their P1-P5 structure does not match my dispatch's prompt headers (their own structure: scope/method, archaeology, keep/kill, risk, plan).

Substantive findings:
- **Archaeology**: each crate's introduction commit identified (crossbeam `4212ae6`, serde_yaml `3719fa7`, fs2 `e584d94`, async-trait `09f4e6a`, sysinfo `3883a34`, etc.). No prior delete/revert precedent for any of the 8 items.
- **Keep/kill pressure**: `crossbeam`, `serde_yaml`, `regex`, `fs2`, `cron`, `chrono-tz` — KEEP (high-coupling). `which`, `async-trait`, `dirs` — KEEP with possible refactor later (medium-coupling). `iana-time-zone` — DEFER-NOT-KILL. `sysinfo` — strongest true cleanup candidate.
- **Anti-pattern flag**: "deps removals without fixture-anchored behavior tests lead to re-introduction churn"; "mixing 'policy simplification' with 'dependency removal' in one PR causes review ambiguity".
- **Recommended plan**: 3 PRs — PR-A analysis-to-contract (formalize target list + non-goals + acceptance tests), PR-B `sysinfo` cleanup only with before/after fixtures, PR-C optional micro-cleanups (`dirs` / `iana-time-zone`).
- **Strong cross-vantage signal**: reviewer's read narrows operator's 8-item scope to ~1 confidently-supported (sysinfo). This is dissent from operator's pre-approved scope — surfaced honestly here for §13 decision.

---

## 3. Tier classification (operator picks one — §13.1)

### TIER-A — narrowest (reviewer-aligned)
**1 PR, ~50 LOC.** sysinfo cleanup only with before/after behavior fixtures (pid-alive path + monitor metrics output contract).

- Defer ALL of: teloxide, serde_yaml, crossbeam, async-trait, getrandom, fs2, embed-resource
- Accepts reviewer's prior-art warning that other items are high-coupling and risk reintroduction churn
- Strongest fixture-anchored test discipline; minimal blast radius
- **Cost**: leaves serde_yaml deprecation + teloxide cascade undone for another sprint

### TIER-B — dev's 4-PR partition (middle)
**4 PRs, ~440 LOC.** PR-A teloxide / PR-B serde_yaml / PR-C crossbeam / PR-D batch (4+5+6+7+8).

- Operator's pre-approved 8-item list, partitioned for bisect-friendliness
- teloxide isolated due to highest risk + Tier-2 review
- Mechanical items batched to save review cycles
- **Cost**: more cycles than TIER-A; some risk on serde_yaml (config parsing critical path) and teloxide (4114 LOC adapter)

### TIER-C — full operator scope, 8 independent PRs
**8 PRs, ~440 LOC.** One PR per audit item.

- Maximum bisect granularity / clean revertability
- Highest review cycle cost
- **Cost**: 8 separate review cycles for items where 4-8 are nearly trivial

### Recommendation: **TIER-B**
Aligns with operator's pre-approved 8-item scope without creating 8 separate review cycles. Item-grouping (PR-D batch of mechanical items) is reviewer-friendly while keeping risky items isolated. Reviewer's prior-art conservatism is partially absorbed by the §3.5.10 fixture requirement on each PR (per-PR behavior anchor, addressing "fixture-anchored behavior tests" warning).

If operator agrees with reviewer's risk read: TIER-A is acceptable as conservative narrow first wave; can add subsequent waves later.

---

## 4. PR sequencing for chosen tier (TIER-B reference)

Per dev structural confirmation of operator's order: **teloxide → serde_yaml → crossbeam → batch**.

| PR | Items | LOC est | Tier | §3.5.10 fixture |
|---|---|---|---|---|
| **PR-A** | 2 (teloxide 0.13→0.15.x) | ~200 (incl. ~20 LOC adapter changes per dev S4) | **Tier-2 dual reviewer** | telegram bot api spec for forum-topic / message_create payload + production-path mock listener (per just-shipped §3.5.10 amendment) |
| **PR-B** | 1 (serde_yaml → serde_yaml_ng) | ~80 (4-41 sites per audit-vs-dev count discrepancy) | Tier-1 | round-trip fixture: parse + serialize representative fleet.yaml; assert byte-equal output (catches whitespace/quote-style divergence per reviewer P5 adversarial scenario) |
| **PR-C** | 3 (crossbeam → crossbeam-channel) | ~100 (67 refs across 16 files, mechanical) | Tier-1 | concurrent-state fixture: producer/consumer channel passing through new crate; assert message ordering invariant unchanged |
| **PR-D** | 4 + 5 + 6 + 7 + 8 (sysinfo, async-trait, getrandom, fs2, embed-resource) | ~60 cumulative | Tier-1 | per-item behavior fixture: sysinfo pid-alive path + async-trait native-fn signature parity + getrandom version-bump byte-equivalence + fs2→fs4 lock semantics + embed-resource Windows build smoke |

Linear dependency (E1.1 strict on-top-of-main): each PR branches from main; previous merges before next dispatched.

---

## 5. Cargo.lock cascade analysis

dev S1 cites teloxide 0.13 → 0.15.x cascading to eliminate 9+ duplicate transitive groups. reviewer's archaeology found no historical delete/revert precedent — first-time cleanup attempt at this surface area.

Per Sprint 32 (Discord adapter shipped via `twilight-*` modular crates), the daemon now has TWO async-runtime-using channels. teloxide cascade may also touch `reqwest` / `hyper` / `rustls` versions that twilight depends on. **Pre-PR-A check**: dev must verify `twilight-gateway` / `twilight-http` / `twilight-model` (Sprint 32 deps) don't conflict with teloxide 0.15.x's transitive set.

---

## 6. Risks (Sprint 36-specific, beyond per-item analysis)

| Risk | Mitigation |
|---|---|
| teloxide 0.15.x cascade conflicts with twilight (Sprint 32) | Pre-PR-A `cargo tree -d` audit; if conflict, defer teloxide or pin twilight |
| serde_yaml_ng output divergent from serde_yaml (whitespace/quote style) → fleet.yaml round-trip pollution | Round-trip fixture per PR-B §3.5.10 |
| sysinfo 0.30 → 0.32+ behavior change in process tree RSS calculation regresses health classifier | per-PR behavior fixture; reviewer prior-art P5 scenario |
| async-trait removal regresses CI-provider trait dispatch under specific runtime patterns | dev structural confirms zero-LOC change; reviewer prior-art recommends KEEP (medium-coupling) — disagreement surfaced, fixture absorbs |
| Mixing dependency upgrade with policy simplification in same PR (reviewer anti-pattern) | TIER-B partition deliberately keeps each PR scoped to dependency change only |
| fs2 → fs4 API divergence on platform-specific lock semantics (e.g., macOS flock vs Linux fcntl) | Multi-platform CI matrix (already in place); per-PR fixture |

---

## 7. Out of scope (operator-decided 2026-04-29 + plan synthesis)

- **`embed-resource = "2" → "3.x"`** is included in operator's 8 (item 8) but dev structural omitted it from S1. **Plan defers to operator**: keep in PR-D batch (Windows build smoke fixture) per operator scope, OR defer if PR-D scope grows.
- **`iana-time-zone`** is dev's S1 added 9th item but NOT in operator's 8. **Plan excludes** as out-of-scope creep; can be Sprint 37+ candidate.
- **MSRV bump declaration** — explicit `rust-version = "1.75"` (or higher) — not gated on any single item but is a separate clarification candidate for a follow-up docs PR.
- **`twilight-*` (Sprint 32) version pinning policy** — no Sprint 36 work; future maintenance candidate.
- **`reqwest` / `hyper` / `rustls` direct-dep declaration** (currently transitive-only) — out of scope; teloxide cascade may surface this question.

---

## 8. Verification

### Per-PR (within TIER-B)
- §3.5.10 fixture per PR per §4 table
- §3.5.11 test-first per PR
- §3.5.13 mirror per verdict
- Pre-push checklist (per PR #330 amendment): `cargo test --all-features` green + `cargo clippy --all-targets -- -D warnings` strict-clean + push-form (a/b)

### TIER-B exit criteria
- All 4 PRs merged with cumulative `Cargo.lock` showing no duplicate transitive groups for the affected crates
- Telegram + Discord (Sprint 32) integration tests both pass on main
- Build time / binary size deltas measured + reported (operator-visible)

### Stage B abort gate (general practice)
N/A here; no Channel trait touched.

---

## 9. Open questions for operator (§13)

1. **TIER selection** — A (reviewer-narrow, sysinfo only) / B (dev partition, recommended) / C (full 8 independent)?
2. **teloxide pre-eval** — should we measure teloxide 0.13 → 0.15.x breaking surface against `twilight-*` cascade conflict before committing PR-A scope? OR ship and react to CI?
3. **embed-resource (audit #8)** in or out of Sprint 36? — dev structural silently dropped; operator's 8-item list keeps. Plan defers to operator.
4. **iana-time-zone (dev's added 9th)** — Sprint 36 or Sprint 37+?
5. **Sprint number** — Sprint 36 (per general suggestion) confirmed; dev2 team is on Sprint 34 with their own numbering; no conflict.
6. **Dispatch wave parallelism** — TIER-B PRs strictly serial (operator's order) or allow PR-A teloxide + PR-D batch in parallel (different files; bisect harder if both fail)?
7. **Tier classification per PR** — confirm PR-A teloxide Tier-2 dual; PR-B/C/D Tier-1?
8. **MSRV declaration** — explicit `rust-version = "1.75"` to lock async-trait removal floor? Or leave implicit?
9. **Reviewer's narrow recommendation as fallback** — if TIER-B's PR-A teloxide work uncovers blocking issues mid-sprint, fall back to TIER-A or call out in re-plan?

---

## 10. Cross-references

- general m-20260429152250325238-74 (operator dispatch + 8-item priority)
- decision `d-20260429152323966190-3` (Sprint 36 plan-first scope)
- master task `t-20260429152328261856-8`
- claude-a1f200 audit (operator-vetted source; not in repo)
- dev S1-S5 perspective: m-20260429152649545833-81
- reviewer P1-P5 perspective: m-20260429160018164326-94
- `docs/PLAN-discord-channel-2026-04.md` (Sprint 32 plan structure model)
- `docs/audit-over-engineering-2026-04-28.md` (KISS posture for cleanup work)
- `docs/FLEET-DEV-PROTOCOL-v1.md` §3.5.10 / §3.5.11 / §3.5.13 / §10.1
