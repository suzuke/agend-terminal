# Health Audit 2026-05 — Operator Decisions & Rationale

**Date**: 2026-05-12 to 2026-05-13
**Audit team**: 5-member dialectic (auditor-claude / auditor-codex / auditor-kiro / auditor-gemini / auditor-opencode)
**Facilitator**: general
**Process**: Phase 1 silent independent pass (45 min) → Phase 2 cross-challenger voting (60 min) → Phase 3 1-by-1 operator decision (multi-day)
**Outcome**: 40 unique findings deduped from 67 raw; 29 GitHub issues filed under `health-audit-2026-05` label; 6 skipped with rationale; 5 bundled

This document records the **operator decisions** that shaped the audit outcome. It's intended as a cross-session reference for future audits / sprint planning / scope-creep prevention.

---

## A. Threat model decisions

### A1. Prompt-inject defense is NOT agend-terminal's responsibility

**Context**: F1 (API DoS via slow client, prompt-inject scenario)

**Decision**: LLM prompt-inject defense belongs at the **backend / LLM provider layer** (Anthropic / OpenAI / Google / etc), not at agend-terminal. agend-terminal won't build mitigation against agents being prompt-inject compromised.

**Implications**:
- Same-user threats are out of scope (operator already has filesystem write access)
- Pure DoS defenses (slow-client `read_line` timeout etc) deliberately not added back; Sprint 29 timeout removal upheld for session-phase
- But: pre-auth handshake timeout (5s) is OK to re-add — different timing characteristics from session phase

**Applied to**:
- F1 (#680) — scope reduced to concurrency cap + warning, no DoS prevention claim
- F4 (skipped) — telegram topic-close authz is group config concern, not agend-terminal
- F12 (skipped) — Windows ACL single-user assumption stays
- F21 (skipped) — backend whitelist spoof requires PATH poisoning, attacker already has arbitrary execution

**Recurring principle**: "If the precondition for the attack is something the attacker can do anyway via simpler means outside agend-terminal, fixing agend-terminal doesn't add defense — it adds maintenance cost."

---

### A2. fleet.yaml mutated by agents → defense-in-depth justified

**Context**: F14 (source_repo path traversal), F30 (working_directory permissive)

**Decision**: `fleet.yaml` is NOT just operator-edited config. Multiple agent-callable MCP code paths (`team_create`, `create_instance`, `delete_instance` cleanup, etc) write to fleet.yaml. **Agents can be prompt-inject compromised** (per A1, that's the backend's responsibility to prevent, but agend-terminal still shouldn't trust agent-written config blindly). Defense-in-depth on path validation is therefore valuable.

**Implications**:
- F14 (#689) filed: add `..` path component check + canonicalize for `source_repo`
- F30 (#707) filed: REVERSE Sprint 29 audit #4 decision on `working_directory` permissiveness
- Both bundled into single PR (same fix pattern: shared `validate_path` helper)

**Distinction from A1**: A1 says "don't build mitigation for prompt-inject inside LLM". A2 says "do validate inputs coming through MCP write paths because those paths are agent-callable and validation is cheap".

---

### A3. Single-user same-machine assumption is load-bearing

**Context**: F12 (Windows ACL), F21 (PATH poisoning)

**Decision**: agend-terminal's threat model assumes:
- Operator and daemon run as same user on same machine
- Same-user processes can already read each other's files, kill each other, modify PATH, etc
- No defense built for "malicious same-user process attacking daemon"

**Implications**:
- Windows shared-profile / RDP / multi-user workstation scenarios are **out of scope**
- API server is bound to 127.0.0.1 (localhost-only), no remote attack surface
- Cookie file at mode 0600 on Unix provides UDS-equivalent access control on TCP transport
- Threats coming from same-user attacker are explicitly accepted

**Quote (paraphrased operator 2026-05-12)**: "能在我 user 權限下動手腳的人，已經能做更糟的事了。"

---

## B. Architecture / engineering decisions

### B1. Architecture refactor cost is acceptable

**Context**: F20 (god block refactor), F26 (connect.rs static)

**Decision**: When auditor identifies architectural anti-pattern (god block, function-scope static, etc), file as actionable refactor — **do not gate on "no immediate user impact" or "cost too high"**. Maintainability + future enabling is worth the refactor cost.

**Quote (operator 2026-05-12)**: "有需要就 refactor、不要太在意成本"

**Implications**:
- F20 (#694) filed as actionable: god block refactor with 6-10 PR incremental plan
- F26 (#702) filed: connect.rs static → OnceLock<Arc<AtomicBool>> hygiene fix
- General's "discussion / RFC issue" framing rejected for these cases — file as actionable

**Process correction**: facilitator (general) was too conservative on architecture findings, defaulted to "RFC issue / not actively prioritized" framing. Operator corrected with "file + refactor when there's a need".

---

### B2. Per-file judgment for splits, not mechanical LOC cap

**Context**: F25 (mega-files split)

**Decision**: Don't apply mechanical "LOC > 2500 → must split" rule. Evaluate each file's cohesion vs concern-mixing:

| File | LOC | Decision |
|---|---|---|
| `ci_watch.rs` | 4829 | ✓ Strongly split — 5 distinct concerns |
| `agent.rs` | 2429 | ✓ Split — 4 distinct concerns |
| `inbox.rs` | 3334 | ✓ Split — 5 cohesive sub-concerns |
| `state.rs` | 2789 | Partial — split per-backend patterns out, keep `StateTracker` cohesive |
| `fleet.rs` | 3131 | Defer — single concept (fleet.yaml shape), splitting fragments |

**Principle**: Cohesion-first, not LOC-first. Big can be OK if responsibilities are clear.

**Implementation order** (#701): F11 coverage prerequisite → ci_watch → agent → inbox → state-partial → fleet-evaluate

---

### B3. Sprint-level audit decisions can be reversed when context changes

**Context**: F30 reversed Sprint 29 audit #4

**Decision**: Past audit decisions are not immutable. When new threat model context emerges (e.g. F30's "agent can write fleet.yaml under prompt-inject" wasn't modeled in Sprint 29), reverse the prior decision with explicit acknowledgment in CHANGELOG + PR description.

**Implications**:
- F30 (#707) explicitly cites Sprint 29 audit #4 reversal in issue body
- Standard pattern: "Sprint X audit Y decided Z, current audit reverses Z because [new context]"
- Avoids "but the audit said..." paralysis when new threats emerge

---

## C. Test / CI / coverage decisions

### C1. CI coverage tooling = maintainer-side, zero user impact

**Context**: F11 (coverage tooling), F23 (cargo audit / supply chain)

**Decision**: CI quality infrastructure (coverage, supply-chain scan, lint) is **purely maintainer-side**:
- End users running `cargo install agend-terminal` see no difference
- No new user-facing UI / config / dependency
- Codecov free tier applies (public repo, OIDC auto-trust, no token)

**Applied to**:
- F11 (#686) — add `cargo-llvm-cov` + Codecov
- F23 (#696) — add `cargo audit` + Dependabot
- Both filed as actionable, no user-impact concern

**Recurring framing**: When evaluating CI changes, separate "maintainer-side QA infra" (file freely) from "user-facing behavior change" (heavier scrutiny).

---

### C2. Subscription-based auth blocks GitHub-hosted real-backend nightly

**Context**: F28 (real-backend nightly)

**Decision**: operator uses subscription-based auth for LLM backends (Claude Pro, Gemini subscription, Kiro, etc) — OAuth interactive login, no API key string injectable as GitHub Secret. GitHub-hosted runner is **not viable** for real-backend nightly because:
- Ephemeral VM has no persistent OAuth state
- API key headless setup not applicable to subscription auth
- Self-hosted runner setup + maintenance cost judged too high

**Decision (alternative path)**: 3 cumulative strategies replace nightly automation:
1. Expand fixture corpus (catch pattern regressions)
2. Production telemetry collection (detect-after-fact, surface trend)
3. Per-release manual smoke checklist (human deterministic gate)

**Filed**: F28 (#704) with 3-sub-task structure

**Principle**: Don't fight reality. If automation is blocked by operator's subscription model, design alternative regression protection that works within constraints, don't pretend automation will magically appear.

---

### C3. Test infrastructure debt is real but bounded

**Context**: F35 (macOS-flaky), F36 (hard sleeps), F37 (bridge invariant gaps), F27 (shutdown-under-load)

**Decision**: File each test infra finding as actionable with concrete fix path. Don't accumulate as "tech debt backlog" — flaky tests erode CI signal trust, which is more expensive than the fix.

**Applied**:
- F35 (#712) — macOS-flaky cluster
- F36 (#713) — hard sleep migration to `wait_until`
- F37 (#714) — bridge invariant adjacent state coverage
- F27 (#703) — shutdown-under-load integration test

**Cross-reference**: F35 + F36 share `harness::wait_until` pattern — bundle as "Test Infrastructure Hardening Sprint"

---

## D. Skip rationale (6 findings)

| F# | Skipped reason | Doc reference |
|---|---|---|
| F4 | Telegram group config concern, not agend-terminal (admin vs member topic-close permission is operator-controlled in Telegram, not in code) | A1 / A3 |
| F7 | DROP — anchor tests at `src/inbox.rs:3175-3284+` actually exist; CHANGELOG over-claimed by 1 test (3 vs 2) — minor accuracy nit, not critical | Phase 2 finding rebut |
| F12 | Windows ACL single-user threat model accepted | A3 |
| F19 | `git fetch --prune` on every release is intentional accuracy trade-off (worktree merged detection requires fresh refs) | Operator judgment "合理 trade-off" |
| F21 | Backend whitelist PATH spoofing — attacker capable of PATH manipulation already has arbitrary code execution; flag-passing not novel surface | A1 |
| F40 | LATCHED_STATE_EXPIRY 30s rigid — intentional spinner debounce, `state.rs:800-805` documents trade-off | Phase 2 finding rebut |

---

## E. Bundle decisions

| Bundled findings | Target issue | Rationale |
|---|---|---|
| F31 (stale API timeout comment) | #680 (F1) | Same code area, doc fix belongs with code fix that resolves underlying inconsistency |
| F16 (hardcoded `origin` remote) | #690 (F15) | Sibling fix to F15's hardcoded `main`, same `default_branch()` / `primary_remote()` helper pattern |
| F9, F10, F39 (hang detection sub-concerns) | #685 (F8) | All instances of "Phase 1 detection accuracy validation" prerequisite to hang auto-recovery |

**Bundle principle**: When findings share fix path or are concrete instances of same parent issue, bundle into one GitHub issue with task-list sub-tracking. Avoids backlog noise + maintains context cohesion. **But**: separate issues OK when findings are independent and have different priority / implementer / timeline.

---

## F. Process / facilitator lessons (general's own)

### F1. Verify state before discuss

Facilitator initially discussed scope based on paper plan + memory rather than reading current source. Caught by operator with "你有先確認過現況事實之後才跟我討論嗎？"

**Updated memory**: `feedback_verify_state_before_discuss.md`

Applied throughout Phase 3: every finding verified via `grep` / `wc -l` / source reading before file decision.

---

### F2. Recap context per discussion, don't assume operator memory

Facilitator jumped into 4 open questions for #664 without recapping what #664 was about. Caught by operator with "再來這個 issue 到底是為什麼解決什麼問題，每次討論都要適當的提醒我，不要預設我能記得所有的事情"

**Updated memory**: `feedback_recap_context_each_discussion.md`

Applied throughout Phase 3: every finding starts with "Recap context" / "簡單講" / problem framing before option list.

---

### F3. Operator-direct decisions outweigh facilitator preferences

Facilitator twice over-recommended "skip" / "RFC-only" for architecture findings (F20 god blocks, F26 connect.rs static). Operator corrected: "有需要就 refactor、不要太在意成本"

**Lesson**: facilitator's cost-aware framing is one input, not the deciding voice. Surface trade-offs honestly, let operator weigh.

---

### F4. Cross-challenger dissent has high signal value

Phase 2 caught several findings the original raiser got wrong:
- F7 — claude (raiser) had medium confidence; gemini + codex + kiro found the anchor tests exist → DROP
- F10 — gemini (raiser) described `maybe_decay` mechanism wrong; opencode + codex + kiro all caught the misframing → RECLASSIFY
- F29 — original 1748 count; codex recount yielded 2145 → REFINE
- F34 — original "CI disabled" claim; codex + kiro caught CI debug build actually enforces → RECLASSIFY production-only

**Process value confirmed**: heterogeneous backend audit catches single-auditor blind spots. Repeat audit class every 6-12 months.

---

## G. Filed issues quick reference

All issues labeled `health-audit-2026-05`. Total 29 filed.

### Critical / security family
- #680 F1 — API server pre-auth concurrency cap + WARN log + handshake timeout (+ F31 stale comment bundle)
- #681 F2 — kill_process_tree PID 0 guard
- #682 F3 — Worktree GC layout mismatch (Wave-4 migration gap)
- #708 F32 — agent spawn strip AGEND_GIT_BYPASS from PTY env
- #709 F33 — github_token format validation

### Defense-in-depth family (A2)
- #689 F14 — source_repo path traversal validation
- #707 F30 — working_directory canonicalize (reverses Sprint 29 audit #4)

### Hang detection / reliability family
- #685 F8 — Hang detection accuracy (Phase 1) + auto-recovery (Phase 2), with F9/F10/F39 sub-tracking

### Worktree / infra hardcoded family
- #690 F15 — default branch resolve (no more hardcoded `main`), + F16 (origin remote) bundle
- #692 F17 — ci-watch concurrent modification flock fix
- #693 F18 — reconcile_orphans cross-check before stale binding deletion
- #695 F22 — Channel adapter generic supervisor framework

### Process / closure family
- #683 F5 — task_sweep enforce mode docs clarify
- #684 F6 — MCP-TOOLS.md tool count drift + restart_daemon verification

### Test infrastructure family
- #703 F27 — daemon shutdown-under-load integration test
- #704 F28 — real-backend regression protection (fixture corpus + telemetry + manual checklist)
- #712 F35 — macOS-flaky test cluster
- #713 F36 — integration.rs hard sleep migration
- #714 F37 — bridge runtime invariant adjacent states
- #699 F24 — MCP tools smoke test (22 untested)

### Architecture / refactor family
- #694 F20 — god block refactor (event loop + MCP dispatcher)
- #701 F25 — mega-file split + size invariant extension
- #702 F26 — connect.rs static AtomicBool hygiene
- #706 F29 — file-level allow narrow to test mod scope

### CI / supply chain family
- #686 F11 — cargo-llvm-cov + Codecov coverage tooling
- #696 F23 — cargo audit + Dependabot supply-chain scanning

### Docs accuracy family
- #688 F13 — auth_cookie verify() doc-impl contradiction
- #715 F38 — architecture.md UDS → TCP loopback rewrite

### Daemon-core observability
- #711 F34 — Lock-tier audit production surface (hybrid panic + log + alert)

---

## H. Suggested follow-up

After 29 issues are triaged + worked off:
- Re-audit every 6-12 months (re-run 5-backend dialectic on whichever codebase areas have changed most)
- Verify recurring docs-drift pattern (F6 / F13 / F31 / F38) — consider CI invariant test for doc/code consistency where measurable
- Calibrate "tier system" for next audit: critical / important / nice-to-have proved useful but multiple findings shifted tier during Phase 2 — define tier criteria more explicitly upfront

---

## End of document

This decisions log is intended for cross-session reference. When future audits / sprint planning / scope-creep discussions invoke "didn't we decide X about Y?", check this document.
