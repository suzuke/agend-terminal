# PLAN: Sprint 39 — GitLab + Bitbucket CiProvider

**Date:** 2026-04-30
**Status:** plan-first; operator GO already given on §13 1-5 pre-decisions; awaiting GO on remaining §13 + Operator Verification Plan signoff
**Branch:** `docs/sprint39-ci-providers-plan` → PR for plan-doc only
**Origin:** Sprint 38 closed-world NO answer (operator m-20260430020753092663-103) → "future GitLab/Bitbucket" is now per general m-20260430021257912735-108
**Process:** 4-perspective challenge round (Sprint 32/36/38 model)
**Scope decision:** project decision `d-20260430021323643758-1`

---

## 0. KISS gate (§0) + closure of Sprint 38 promise

- **What real problem does this solve?** Sprint 38 deferred async-trait removal *because* operator committed to closed-world NO (Q3 GitLab/Bitbucket truly planned). Sprint 39 redeems that commitment — actually implementing the extensibility that justified preserving the trait abstraction.
- **Would deletion break anyone?** Sprint 39 deletion = future provider work pushed indefinitely; Sprint 38 decision becomes purely speculative. Real cost: continued GitHub-only restriction.
- **Operator philosophy alignment**: extensibility delivered = "永久解" promise honored, not deferred bridge.

---

## 1. Verified current state

`src/daemon/ci_watch.rs` (post Sprint 36 cascade):
- `CiProvider` trait (3 async methods + 1 sync `token_warning`)
- `GitHubCiProvider` impl (~250 LOC; uses `reqwest` directly via teloxide cascade — `reqwest = 0.12` modernized)
- `MockCiProvider` impl (#[cfg(test)])
- `Box<dyn CiProvider>` dispatch at lines 304/312
- `async-trait = "0.1"` direct dep (Sprint 38 deferred-permanent)

Cargo.toml deps already in tree (no new transitive cost): `reqwest 0.12`, `tokio`, `serde`, `serde_json`.

---

## 2. Three perspectives (challenge round summary)

### 2.1 lead — minimal-delta synthesis
Sprint 39 redeems Sprint 38's deferral commitment. Operator pre-decisions resolved 5 of §13 questions before perspectives finished. Two perspectives have substantive dissents from operator pre-decisions worth surfacing (not paper-overing): auto-detect primacy + Bitbucket variant scope. Plan honors operator decisions while documenting reviewer's KISS concerns for operator awareness.

### 2.2 dev (kiro) STRUCTURAL (m-20260430021500222579-114)
**S1 Per-provider API surface mapping** (table comparing CiProvider methods × GitHub | GitLab | Bitbucket):
- `poll_runs`: GitLab `/projects/{id}/pipelines?ref={branch}`; Bitbucket `/repositories/{ws}/{repo}/pipelines?target.branch={br}`
- `check_pr_terminal`: GitLab merge_requests; Bitbucket pullrequests
- `fetch_failure_summary`: GitLab `/jobs/{id}/trace`; Bitbucket `/steps/{uuid}/log`
- Status value differences: GitLab `success/failed/running/pending/canceled`; Bitbucket `COMPLETED + result.name`

**S2 Auth approach** — covers GITHUB_TOKEN (Bearer), GITLAB_TOKEN (PRIVATE-TOKEN header), BITBUCKET_TOKEN (Basic base64). **Gap**: only env-var path; doesn't cover operator's preferred fallback chain (CLI config files). Synthesis adds.

**S3 Pagination + rate-limit** — single-page poll suffices (5 results); per-provider `extract_rate_limit()` helper.

**S4 PR sequencing**: **Option α — 3 PRs** (GitLab impl / Bitbucket impl / config wiring) ~650 LOC. Recommended over β (combined providers) and γ (mega PR).

**S5 Test fixture strategy**: spec-quoted fixtures committed to `tests/fixtures/`; reuse Sprint 32 PR-C `mock_http_server()` raw TCP pattern; production-path-coupled per §3.5.10.

### 2.3 reviewer (codex) PRIOR-ART / CROSS-VANTAGE (m-20260430021625173646-120)
**P1 Sprint 32 pattern carryover**:
- ✓ Serial multi-PR + production-path fixture + scope discipline
- ✗ Gateway scaffold (REST-only, no WS); feature-gate-per-provider (overkill); Tier-2 dual review (less justified by risk)
- Recommend: 2-3 PRs not Discord-style 4

**P2 Dep posture**:
- `gitlab` crate active but pulls heavy
- Bitbucket ecosystem fragmented (`bitbucket-server-rs` for Server only; no mature Cloud lib)
- **Recommend roll-own with existing `reqwest`** for both; minimal cascade

**P3 Cleaner design alternatives**:
- Option 1 (trait impl) vs Option 2 (config-struct + generic HTTP provider). Config-driven hides complexity in DSL; debugging harder.
- **Hybrid recommended**: trait-impl preserved + shared `CiHttpClient` helper for auth/retry/rate-limit boilerplate. Reduces duplication without DSL risk.

**P4 KISS trim per scope candidate**:
- ✓ GitLab provider, Bitbucket provider, config override, auth env vars, per-provider mocks: must-have
- **DEFER auto-detect** to secondary fallback — prioritize explicit config (operator pre-decision goes opposite — surfaced in §3.2)
- Bitbucket variant: **pick ONE for MVP** (Cloud-first unless operator says otherwise) — diverges from operator pre-decision

**P5 Adversarial scenarios**:
1. Token leak in logs (centralized header redaction)
2. Rate-limit semantics divergence (per-provider backoff field)
3. Auto-detect misclassification on self-hosted (mitigation: explicit config precedence)
4. Bitbucket Cloud vs Server API mismatch (force explicit `bitbucket_cloud`/`bitbucket_server` enum)
5. Status normalization drift (per-provider mapping tests with captured fixtures)

---

## 3. Two-perspective dissents from operator pre-decisions

Operator pre-answered §13 1-5 (general m-20260430021548804071-118). Two reviewer-flagged concerns deserve surface for operator awareness:

### 3.1 Auto-detect primacy
- **Operator pre-decision**: auto-detect from git remote URL DEFAULT; fleet.yaml override for self-hosted
- **reviewer P4 dissent**: defer auto-detect to OPTIONAL secondary; prioritize explicit config
- **dev STRUCTURAL didn't address** (operator pre-decisions arrived after dev reply)
- **Synthesis position**: honor operator default but ensure explicit config wins on conflict (pre-decision implies this); add adversarial fixture covering misclassification on self-hosted custom-domain remote (per reviewer P5 #3)

### 3.2 Bitbucket variant scope
- **Operator pre-decision**: include both Bitbucket Cloud + Bitbucket Server in scope
- **reviewer P4/P5 dissent**: pick ONE variant for Sprint 39 MVP (Cloud preferred); force explicit enum config; Server has different API surface (REST 1.0 vs Cloud's REST 2.0)
- **dev S1 only mapped Cloud endpoints**; Server unaddressed
- **Synthesis position**: present operator §13 question reopened — does scope ACTUALLY require both Server + Cloud now, or is Cloud sufficient for MVP with Server deferred to Sprint 40+? KISS pressure says one-at-a-time; operator's "extensibility now" framing might still admit Cloud-first sequencing per dev S4's GitLab→Bitbucket order.

### 3.3 Auth fallback chain (item 3)
- **Operator pre-decision**: prefer Option β (env var → CLI config file → warn-hint)
- **dev S2 didn't address**: only env-var path mapped; no CLI config file integration
- **reviewer P1 didn't have operator pre-decisions when responding**: CLI paths not enumerated
- **Synthesis position**: honor operator pre-decision; defer concrete CLI config paths to follow-up (gh `~/.config/gh/hosts.yml`, glab `~/.config/glab-cli/config.yml`, bb varies) until impl PR can do reading-side prior-art. Bundle the "gap close" into PR-1 (GitLab impl) since gh+glab format documentation is available; Bitbucket CLI fallback can land with PR-2.

---

## 4. Operator pre-decisions (RESOLVED §13 1-5)

| §13 | Operator decision | Source | Notes |
|---|---|---|---|
| 1 | GitLab first, then Bitbucket (sequenced) | m-20260430021548804071-118 | dev S4 confirms 3-PR α ordering |
| 2 | Auto-detect default + fleet.yaml override | m-20260430021548804071-118 | reviewer dissent in §3.1 |
| 3 | Auth fallback chain (ENV → CLI config → warn-hint) | m-20260430021548804071-118 | dev S2 gap; close in PR-1 |
| 4 | Self-hosted variants IN scope (GHE + GitLab self-hosted + Bitbucket Cloud&Server) | m-20260430021548804071-118 | reviewer dissent on Bitbucket Cloud-only MVP in §3.2 |
| 5 | Operator Verification Plan section required | m-20260430021548804071-118 | New §6 below |

---

## 5. PR sequencing — 3 PRs per dev S4 + reviewer P1 confluence

| PR | Scope | LOC est | Tier |
|---|---|---|---|
| **PR-1** | GitLab CiProvider impl + auth (ENV + glab CLI fallback per item 3) + production-path fixture + GitLab self-hosted base URL config support | ~250-300 | Tier-1 single-reviewer |
| **PR-2** | Bitbucket CiProvider impl(s) + auth (ENV + bb CLI fallback) + fixtures + Cloud-vs-Server scope (see §3.2 — operator decides Cloud-only MVP vs both at PLAN review) | ~250-350 | Tier-1 single-reviewer |
| **PR-3** | Provider auto-detect from git remote URL + fleet.yaml `ci_provider:` + `ci_provider_url:` config override + integration tests + GitHub Enterprise base URL support | ~150-200 | Tier-1 single-reviewer |

**Total: ~650-850 LOC across 3 PRs.**

Per reviewer P1 + Sprint 32 precedent: production-path-coupled mock HTTP fixture per provider; spec-quoted REST request/response from official docs; raw TCP `mock_http_server()` reused.

Per reviewer P3 hybrid recommendation: shared `CiHttpClient` helper (auth header + rate-limit parse + retry) — extracted in PR-1 for GitLab use; reused PR-2/3.

Strict serial: PR-1 → PR-2 → PR-3 per Sprint 36 strict-serial discipline.

---

## 6. Operator Verification Plan (per §13 #5 NEW requirement)

### Per-provider verification matrix

| Provider | Mock-server automated test (CI) | Real-account manual verification |
|---|---|---|
| GitHub (existing) | `cargo test ci_watch::tests::*` mock fixture | export `GITHUB_TOKEN`; spawn agent on a repo with workflow; expect `[ci-pass]`/`[ci-fail]` |
| GitLab (new PR-1) | `cargo test ci_watch::gitlab_*` with mock_http_server fixture | export `GITLAB_TOKEN`; create test project on gitlab.com with .gitlab-ci.yml; spawn agend-terminal; expect notification |
| Bitbucket Cloud (new PR-2) | `cargo test ci_watch::bitbucket_*` with mock fixture | export `BITBUCKET_TOKEN`; create test repo on bitbucket.org with bitbucket-pipelines.yml; spawn agend-terminal; expect notification |
| Bitbucket Server (PR-2 if both in scope) | `cargo test ci_watch::bitbucket_server_*` | self-hosted Bitbucket Server eval license OR existing instance; configure base URL via fleet.yaml; expect notification |
| GitHub Enterprise (PR-3) | reuse GitHub mock with custom base URL | requires GHE access; configure base URL via fleet.yaml |
| GitLab self-hosted (PR-3) | reuse GitLab mock with custom base URL | GitLab Omnibus install OR existing instance; configure base URL |

### Step-by-step minimal happy-path verification (any provider)

```bash
# 1. Setup auth
export {PROVIDER}_TOKEN="<your-token>"

# 2. Configure fleet.yaml (or rely on auto-detect from git remote)
# fleet.yaml minimal:
# instances:
#   my-agent:
#     backend: claude
#     ci_provider: gitlab  # explicit; or omit for auto-detect
#     ci_provider_url: https://gitlab.example.com  # for self-hosted only

# 3. Spawn agent + push commit to trigger CI
agend-terminal start
# (in another terminal)
git commit --allow-empty -m "trigger CI"
git push

# 4. Expect notification within ~30-60s (poll cadence)
# In agend-terminal pane:
#   [ci-watch] suzuke/agend-terminal@feat/foo: pending → success ✓
```

### Self-hosted setup overhead estimates

| Provider variant | Setup time | Cost | Notes |
|---|---|---|---|
| GitLab Omnibus self-host | ~30 min | Free (CE) | Docker image preferred for testing |
| Bitbucket Server eval | ~60 min | Free 30-day eval | Atlassian SEN required |
| GitHub Enterprise | ~24h | $$$ | Most operators won't have access; mock-server tests are primary verification |

**Recommendation**: prioritize Cloud variants for operator manual verification; treat self-hosted variant verification as automated-mock-only unless operator has existing self-hosted instance.

### Mock-server tests run on CI (no operator setup required)
Per §3.5.10 production-path-coupled fixture: every PR's CI matrix automatically runs the per-provider mock fixture. No operator setup needed for that level of verification — it's the protocol-floor confidence.

---

## 7. Risks (Sprint 39-specific, beyond per-perspective P5)

| Risk | Mitigation |
|---|---|
| Bitbucket Cloud vs Server API confusion at config time | Explicit `ci_provider: bitbucket_cloud` / `ci_provider: bitbucket_server` enum; reject ambiguous `bitbucket` value with operator-actionable error |
| Auth token in error logs | Centralized header redaction in `CiHttpClient`; NEVER include request context in `ci-warn` payload |
| Auto-detect false positives on self-hosted custom domains (e.g., `git.acme.corp`) | Explicit config wins on conflict; add fixture covering custom-domain misclassification per reviewer P5 #3 |
| CLI fallback config file parsing brittle (gh hosts.yml format) | Use existing crate (`config` / `serde_yaml_ng`) for parse; small bounded scope; validate against gh CLI's own config |
| Sprint 32 Discord twilight-* deps not affected by reqwest re-use | Sprint 36 PR-A cascade already consolidated; verify post-PR-1 with `cargo tree -d` |

---

## 8. Out of scope

- async-trait removal (Sprint 38 deferred-permanent until Rust dyn-async stable)
- CiProvider trait redesign (preserve current shape)
- Discord-feature pre-existing clippy nits (Sprint 32 carry-forward; not Sprint 39 deps)
- Bitbucket Server variants if operator agrees with reviewer P4 Cloud-first MVP (see §3.2)
- Multi-instance per-instance provider override (current scope: per-fleet, not per-instance)
- WebHook-based notification (current arch is polling-only)

---

## 9. Open questions for operator (§13 remaining + flagged dissents)

1. **Bitbucket variant scope** (re-opened per reviewer P4): Sprint 39 MVP includes Cloud + Server, OR Cloud-first MVP with Server deferred to Sprint 40+? Trade-off: doing both = ~350 LOC PR-2 + 2 mocks; Cloud-only = ~250 LOC PR-2 + 1 mock + Server gates on operator's actual Server need
2. **Auto-detect dissent** (reviewer P4): keep operator pre-decision (auto-detect default + override), OR adopt reviewer recommendation (explicit config primary, auto-detect best-effort fallback)? KISS trade-off; impact on user-onboarding friction
3. **CLI fallback paths** (item 3): plan PR-1 to ship glab CLI fallback first; PR-2 ships bb CLI fallback. Or all in PR-3 with config wiring? Latter is more cohesive but delays first-provider real-world usability
4. **Operator Verification Plan signoff** (§6): does the matrix cover what you need to manually verify? Any provider/variant you want explicit walkthrough for?
5. **Tier classification per PR** (recommend Tier-1 — pure impl of existing trait per reviewer P1): confirm OR escalate any PR to Tier-2?
6. **IMPL dispatch ownership**: dev (kiro) for all 3 PRs (continuity) OR rotate dev2 in for one (cross-team experience)?

---

## 11. Operator-proxy decisions (recorded 2026-04-30 per general m-20260430022206205445-129)

operator stepped away "全權處理"; general (operator surrogate) decided all 6 §13 remaining items per lead synthesis recommendations + reviewer dissent integration. Audit log: general m-20260430022206205445-129 timestamps 2026-04-30 02:22; no deviation from lead recommendations; general takes responsibility for unanticipated cost; operator may retrospective on return.

| §13 # | Decision | Rationale |
|---|---|---|
| 1 (Bitbucket variant) | **Cloud-first MVP** (adopts reviewer P4/P5 dissent) | Bitbucket Cloud REST 2.0 ≠ Server REST 1.0 = two independent APIs; bundling = "半吊子"; Server defer Sprint 41+ pending operator self-host need. PR-2 LOC ~350 → ~250. |
| 2 (Auto-detect) | **Default + warning enhancement** (compromise — not full reviewer dissent) | Default still auto-detect from git remote; **but** non-standard host (not github.com/gitlab.com/bitbucket.org/bitbucket.com) → daemon log warn "自訂 host pattern detected, suggest setting fleet.yaml `ci_provider: <kind>` explicitly". Adversarial fixture must include self-hosted custom-domain remote + verify warning fires. Catches reviewer's misclassification concern + preserves operator user-onboarding default. |
| 3 (CLI fallback timing) | **PR-1 ships gh + glab; PR-2 ships bb** (per-provider cohesion) | Not deferred to PR-3 (low cohesion); each provider independently testable with its own fallback chain. |
| 4 (Verification Plan §6 signoff) | **Accept as-is** | operator absent for deep review; matrix design accepted; impl-time gaps surfaced by dev get back-filled into §6. |
| 5 (Tier classification) | **Tier-1 all 3 PRs** | Reviewer P1 confirmed: pure impl of existing trait, no architecture change, no Discord-style novelty. Single-reviewer suffices. |
| 6 (IMPL dispatch ownership) | **dev (kiro) all 3 PRs** | Continuity / context accumulation; dev2 reserved for Sprint 40 Group 1 MCP cleanup. |

### §3.5.13 mandate carried forward
Per PR #343 amendment + Sprint 35-37 lessons: orchestrator pre-dispatch verification of dev's push-notification scope claims is required during Sprint 39 IMPL wave. Each PR push will be verified against scope dispatch before review dispatched (preventing PR-B r1 claim-mislabel pattern).

### Cross-team review authorization (if needed)
If any PR-1/2/3 needs cross-vantage reviewer (reviewer2), dispatch must carry operator authorization line: "Operator-proxy authorized cross-team review per general m-20260430022206205445-129". Same precedent pattern as Sprint 35 PR #333 / Sprint 36 PR-A.

### Updated PR-2 scope (per §13 #1)
PR-2 scope reduced to Bitbucket Cloud only (~250 LOC, was ~350). Bitbucket Server impl deferred to Sprint 41+ as a separate PR when operator confirms self-hosted instance access. PR-2 explicit `ci_provider: bitbucket_cloud` enum value (not bare `bitbucket`) — `bitbucket_server` rejected with operator-actionable error message saying "Bitbucket Server not yet supported; track Sprint 41+ candidate".

---

## 10. Cross-references

- general m-20260430021257912735-108 (operator scope)
- general m-20260430021548804071-118 (operator pre-decisions §13 1-5 + Verification Plan requirement)
- decision `d-20260430021323643758-1` (Sprint 39 plan-first)
- master task `t-20260430021328227291-6`
- dev S1-S5 perspective: m-20260430021500222579-114
- reviewer P1-P5 perspective: m-20260430021625173646-120
- Sprint 38 PR #347 (CiProvider preserved per closed-world NO; this Sprint redeems)
- Sprint 32 PRs #316/#317/#318/#319 (multi-PR adapter precedent + production-path-coupled fixture amendment)
- Sprint 36 PR-A teloxide cascade (reqwest 0.12 modernization, basis for roll-own deps)
- `docs/PLAN-discord-channel-2026-04.md` (4-perspective plan model)
- `docs/PLAN-sprint36-cargo-deps-cleanup-2026-04-29.md` (deps-cleanup model)
- `docs/PLAN-sprint38-async-trait-removal-2026-04-30.md` (Sprint 38 closure)
- `docs/audit-over-engineering-2026-04-28.md` (KISS posture)
- `docs/FLEET-DEV-PROTOCOL-v1.md` §0 / §3.5.10 / §3.5.11 / §3.5.13 / §10.1
- GitLab REST API: https://docs.gitlab.com/ee/api/rest/index.html
- Bitbucket Pipelines API: https://developer.atlassian.com/cloud/bitbucket/rest/api-group-pipelines/
- gh CLI hosts.yml: https://cli.github.com/manual/gh_help_environment
