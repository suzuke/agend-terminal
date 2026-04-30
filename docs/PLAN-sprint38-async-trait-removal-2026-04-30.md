# PLAN: Sprint 38 — async-trait removal

**Date:** 2026-04-30
**Status:** plan-first; awaiting operator GO before any `Cargo.toml` / `src/*` writes
**Branch:** `docs/sprint38-async-trait-plan` → PR for plan-doc only
**Origin:** operator pushback on Sprint 36 PR-D close report's "architectural blocker" framing per general m-20260430015658047285-86
**Process:** 4-perspective challenge round (Sprint 32/36 model)
**Scope decision:** project decision `d-20260430015740605755-0`

---

## 0. KISS gate (§0) + operator philosophy

- **Operator pushback**: PR-D close framing said "DEFERRED architectural blocker". Operator overrode: actual scope is 3 use sites + 1 prod impl + 1 test mock — not architectural. PR-D framing was over-conservative.
- **Operator philosophy**: **「做就一次做到好不要半吊子」** — chosen option must be *permanent solution*, not bridge state requiring re-work on next Rust upgrade.
- **What real problem does this solve?** async-trait crate dependency for a small abstraction surface (1 trait, 3 async methods, 1 prod impl, 1 test mock). The crate adds 0 transitive deps (shared with 20+ others); cost is conceptual not material. Operator's request is to validate whether the dep is structurally necessary.
- **Would deletion break anyone?** Removing async-trait without replacement breaks the dyn dispatch architecture; with replacement (enum / generic / native), depends on which path.

---

## 1. Verified current state (lead minimal-delta)

`src/daemon/ci_watch.rs` per dev S1:
| File:line | Element | Notes |
|---|---|---|
| line 49 | `#[async_trait::async_trait] pub trait CiProvider` | trait def with 3 async methods + 1 sync (`token_warning`) |
| line 94 | `#[async_trait::async_trait] impl CiProvider for GitHubCiProvider` | prod impl |
| line 1494 | `#[async_trait::async_trait] impl CiProvider for MockCiProvider` (#[cfg(test)]) | test mock |
| line 304/312 | `Box<dyn CiProvider>` | dyn dispatch use |

Cargo.toml: `async-trait = "0.1"` direct dep + `rust-version = "1.87"` (already declared in Sprint 36 PR-D).

Reviewer P2 archaeology: `CiProvider` trait introduced commit `09f4e6a` (2026-04-25) explicitly for "runtime swappable provider + mockability + provider-neutral DTOs". Pre-trait state was hard-coded GitHub REST inline.

---

## 2. Three perspectives (challenge round summary)

### 2.1 lead — minimal-delta
The trait introduction had explicit architecture intent (PR-AD: provider-neutral DTOs + runtime-swappable). Removing the trait fully is a real architectural shift, even if surface size is small. Operator's KISS-once-and-for-all framing demands the chosen option not require re-doing — but "永久解" can mean either:
- (i) zero-dep enum dispatch that's stable forever, OR
- (ii) keep the trait pattern that was deliberately introduced; revisit when native `dyn AsyncTrait` lands.

Both are defensible. Plan presents both honestly with operator-decidable §13.

### 2.2 dev (kiro) STRUCTURAL (m-20260430020020987562-93)
Concrete LOC + recommendation.

**Option per-impact table**:
| Option | Δ LOC | Binary size | Readability | Future-Rust risk |
|---|---|---|---|---|
| A enum dispatch | +27 | slightly smaller (no vtable) | exhaustive match arms; explicit | **zero** — pure Rust |
| B generic `<P: CiProvider>` | +12 | slightly larger (monomorphized) | propagates P up call chain | zero — standard generics |
| C status quo (async-trait) | 0 | baseline | familiar | tied to crate; proc-macro2/syn already shared |
| D `trait_variant` macro | +2 | same as async-trait | near-identical to status quo | aligned with Rust lang team direction; but `trait_variant` 0.x not 1.0 |

**Key quantitative findings**:
- **async-trait removal eliminates 0 transitive deps** (proc-macro2/syn/quote shared with 20+ crates)
- **Compile-time overhead negligible**: single-digit ms for proc-macro expansion
- **Native Rust 1.75+ async-fn-in-trait** works for static dispatch; does NOT support `dyn Trait` (our use case)
- **Enum dispatch wins on extensibility safety** — only option with compiler-enforced exhaustiveness when adding GitLabCiProvider

**Recommendation**: **Option A (enum dispatch)** — zero external deps, permanent pattern, compiler-enforced exhaustiveness, +27 LOC modest cost.

### 2.3 reviewer (codex) PRIOR-ART / CROSS-VANTAGE (m-20260430020010341246-92)
**KEY DISSENT**: reviewer recommends **Option C (status quo)** — disagrees with operator's enum-dispatch preference.

Substantive findings:
- **P1 codebase precedent**: enum-dispatch exists for `Backend` (closed taxonomy) but runtime extension surfaces (Channel, CiProvider) all use `dyn Trait`. "Enum dispatch is the house style" is NOT strongly supported; "dyn for runtime extension" IS stronger precedent.
- **P2 trait introduction intent**: `CiProvider` was deliberate refactor (2026-04-25 commit `09f4e6a`) for runtime swappable provider + mockability + provider-neutral DTOs. Removing trait ≠ "syntax cleanup"; it dismantles deliberate architecture.
- **P3 native dyn-async timeline**: tracking issue `rust-lang/rust#133119` open experimental; **no committed stable date**. Reasonable bridge horizon, NOT imminent.
- **P4 KISS verdict**: Option C most KISS for current shape; Option A only viable if explicitly trading open extensibility for closed-world.
- **P5 adversarial**: each option has scenarios; mitigations sized.

**Reviewer's bottom line dissent**: "Option C now, explicit revisit when `async_fn_in_dyn_trait` stabilizes — least-risk high-KISS path. If operator explicitly prioritizes closed-world permanence over extensibility, Option A defensible; otherwise scope-expanding refactor with limited near-term payoff."

---

## 3. Two-perspective dissent — operator must resolve

dev (structural) recommends **A**.
reviewer (prior-art) recommends **C**.

The disagreement isn't on facts — it's on **values**:
- dev weights "zero crate deps + permanent pattern + compiler enforcement" higher
- reviewer weights "preserving deliberate architecture intent + ecosystem-aligned bridge" higher

Operator's "永久解 / 一次做到好不要半吊子" framing favors A *if* enum dispatch is genuinely permanent. reviewer challenges the premise: A is permanent only if operator commits to closed-world (no GitLabCiProvider need ever); otherwise A is itself a bridge that gets re-traited when extensibility is needed.

---

## 4. Per-option detailed analysis

### Option A — enum dispatch (operator preference + dev recommend)

**Pseudocode**:
```rust
// In src/daemon/ci_watch.rs
pub enum CiProviderKind {
    GitHub(GitHubCiProvider),
    #[cfg(test)] Mock(MockCiProvider),
}

impl CiProviderKind {
    async fn poll_runs(&self, ...) -> ... {
        match self {
            Self::GitHub(p) => p.poll_runs(...).await,
            #[cfg(test)] Self::Mock(p) => p.poll_runs(...).await,
        }
    }
    // ...same for check_pr_terminal, fetch_failure_summary, token_warning
}
```

**Pros (per dev S5)**:
- Zero external deps (no async-trait, no trait_variant, no proc-macro)
- Compiler-enforced exhaustiveness on adding new provider
- +27 LOC modest cost
- Permanent pattern; won't need re-doing on Rust upgrades

**Cons (per reviewer P4)**:
- Closed-world: every new provider edits central enum + all match arms
- Trades away PR-AD's original runtime extension intent
- Heterogeneous runtime composition harder (e.g., per-instance pluggable provider via config)

**KISS test**: passes IF closed-world is the intentional architectural goal.

### Option B — generic `<P: CiProvider>`

**Pseudocode**:
```rust
fn check_ci_watches_with_provider<P: CiProvider>(
    home: &Path, registry: &AgentRegistry,
    make_provider: impl Fn() -> Option<P> + Send + Sync + 'static,
) { ... }
```

**Pros**:
- Zero-cost abstraction; no dyn dispatch overhead
- Native Rust 1.75+ async-fn-in-trait works (static dispatch)
- +12 LOC modest

**Cons**:
- Generic param P propagates through 3+ fn signatures up call chain
- Monomorphization duplicates code per provider
- Harder runtime-selected provider (would need erase-at-boundary adapter)

**KISS test**: passes if static dispatch acceptable; awkward if runtime selection needed.

### Option C — keep async-trait (status quo, reviewer recommend)

**Pros (per reviewer P4)**:
- 0 LOC change
- Preserves deliberate architecture intent (runtime swappable)
- Bridge until native `dyn AsyncTrait` stabilizes
- async-trait crate is mature, low maintenance

**Cons**:
- Ongoing crate dependency (conceptual; 0 transitive deps)
- Macro-generated boxing per call (negligible runtime cost)
- "Operator wants removal" — political pressure, even if technically sound

**KISS test**: most KISS by reviewer's lens (smallest change, preserves intent).

### Option D — `trait_variant` macro

**Pros**:
- Aligned with Rust lang team direction
- +2 LOC; near-identical to status quo

**Cons (per dev S4 + reviewer P3)**:
- `trait_variant` is 0.x (not 1.0 stable)
- Still a proc-macro (same dep chain category as async-trait)
- Doesn't auto-give dyn AsyncTrait drop-in for our design
- Swaps one proc-macro for another with marginal benefit

**KISS test**: marginal value; not a permanent fix either.

---

## 5. Risks / counter-examples per option (synthesized from reviewer P5)

| Option | Top adversarial scenario | Cheapest mitigation |
|---|---|---|
| A | Need pluggable runtime provider per-instance via config (PR-AD's original intent) | Reintroduce trait at adapter layer; defeats Option A |
| B | Generic P leaks across daemon API boundaries | Erase at outer boundary (`Box<dyn>`) — defeats Option B |
| C | Future Rust native dyn-async lands; codebase lags using legacy macro | Periodic migration checkpoint; revisit when stable |
| D | `trait_variant` 0.x breaks/yanks | Pilot in non-critical trait first; rollback path |

---

## 6. Operator §13 decisions

1. **Option selection**: A (enum, zero-dep permanent if closed-world) / B (generic) / **C (status quo, preserve runtime extensibility)** / D (trait_variant)
2. **Closed-world acceptable?** If A picked: do you commit "1-2 providers forever, no per-instance config dispatch needed"? If yes A defensible; if no, A is itself a bridge.
3. **Future provider extensibility**: GitLabCiProvider / BitbucketCiProvider in next 6-12 months actually planned, or speculative?
4. **Test-mock concern**: dev S3 confirms `#[cfg(test)]` is clean per-option; not a deciding factor
5. **Sprint number**: Sprint 38 (post 36) confirmed
6. **IMPL dispatch**: dev (kiro) or dev2 (kiro2) — both idle per general; no preference signal received
7. **Rust dyn-async stabilization timeline**: per reviewer P3 issue #133119 still experimental; if you want to wait for native, Option C is the bridge
8. **Reviewer dissent override**: do you accept reviewer's prior-art read that Option C is most KISS, or override per "永久解" framing?

---

## 7. Recommendation

**My (lead) read**:

If operator's "永久解 / 一次做到好不要半吊子" framing is the authoritative tiebreaker, **Option A** wins under one strong condition: **commit explicitly to closed-world CiProvider taxonomy** (no per-instance config dispatch ever). Without that commitment, Option A is itself a bridge — and reviewer's argument that Option C is the smaller-blast-radius bridge holds.

If operator wants to defer the bridge cost: **Option C** preserves architecture intent + minimal change + clean migration path when native dyn-async lands (timing uncertain but tractable).

Recommendation: **Operator answers §13 #2 first** (closed-world commitment yes/no). If yes → A. If no → C with explicit revisit checkpoint.

Both options are honest; both have reviewer-grade objections; both can ship. The choice is values-driven, not facts-driven.

---

## 8. PR sequencing for chosen option (if operator picks A)

| PR | Scope | LOC | Tier |
|---|---|---|---|
| Sprint 38 IMPL | enum dispatch for CiProvider; remove async-trait dep; remove dyn use sites | ~30 LOC + tests | Tier-1 single-reviewer |

Single PR sufficient for Option A surface size. §3.5.10 fixture: behavior-equivalence test asserting all 3 enum methods produce same result as pre-removal trait dispatch path (or empirical-revert per §3.5.11 r3 if state preserved).

If operator picks C: this plan PR closes; IMPL wave is null (status quo).

---

## 9. Out of scope

- Refactoring CiProvider to add new methods (out of audit scope)
- reqwest 0.11→0.12 (already cascade-upgraded by Sprint 36 PR-A teloxide)
- Other async-trait usage in codebase (only ci_watch.rs has direct usage; verify via grep)

---

## 9.1 Operator decision (recorded 2026-04-30 per general m-20260430020753092663-103)

**Decision**: **Option C (status quo)** — keep `async-trait` + `trait CiProvider` + `Box<dyn>`.

**Reason**: Operator answered §13 #2 — closed-world commitment **NO**. Future GitLab/Bitbucket provider extension is concretely planned, not speculative. Per §7 conditional framing, "if no → C": Option A would itself be a bridge (since the trait would need re-introducing for new providers), making C the smaller-blast-radius choice that preserves PR-AD's deliberate runtime-extensibility intent.

Operator explicitly accepted reviewer (codex) prior-art DISSENT and overrode their own "永久解" framing on this case — the chosen path is "preserve architecture intent + revisit when Rust native dyn-async stabilizes".

**Trigger for revisit**: Rust `async_fn_in_dyn_trait` stabilization (tracking issue rust-lang/rust#133119, currently experimental).

**No IMPL wave dispatched.** This plan PR itself is the deliverable — it records the trade-off analysis + decision + future trigger. async-trait audit item from Sprint 36 marked **deferred-permanent until Rust dyn-async stable**.

**Process retrospective note**: this decision validates the PLAN-first 4-perspective process. operator initially preferred Option A, lead's PR-D close framing was over-conservative, dev recommended A, reviewer dissented to C — surfacing the dissent honestly let operator weigh trade-offs and reverse to C when Q3 (closed-world) couldn't be committed. Avoiding paper-over preserved decision quality.

---

## 10. Cross-references

- general m-20260430015658047285-86 (operator pushback + Sprint 38 scope)
- decision `d-20260430015740605755-0` (Sprint 38 plan-first)
- master task `t-20260430015744601453-4`
- dev S1-S5 perspective: m-20260430020020987562-93
- reviewer P1-P5 perspective: m-20260430020010341246-92
- Sprint 36 PR-D close report (where deferral framing originated): correlation_id `t-20260429212214457239-1`
- `docs/PLAN-discord-channel-2026-04.md`, `docs/PLAN-sprint36-cargo-deps-cleanup-2026-04-29.md` (4-perspective plan model precedent)
- `docs/audit-over-engineering-2026-04-28.md` (KISS posture)
- `docs/FLEET-DEV-PROTOCOL-v1.md` §0 / §3.5.10 / §3.5.11 / §3.5.12
- Rust tracking issue: rust-lang/rust#133119 (`async_fn_in_dyn_trait`)
