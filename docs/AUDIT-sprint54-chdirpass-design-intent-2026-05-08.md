# Audit — Sprint 54 P1-B Bug 2: `agend-git` ChdirPass design intent

**Date**: 2026-05-08
**Author**: dev (kiro-cli, fleet member)
**Scope**: doc-only RCA per operator structural-op embargo. Audit the wrapper's auto-chdir behaviour BEFORE proposing a fix. No production code change in this PR.
**Decision/task**: `t-20260508000911136329-13`
**Predecessor RCA**: `docs/RCA-sprint54-delete-instance-residual-and-git-wrapper-ux-2026-05-08.md` Bug 2 section (PR #509 squash `66682d2`)
**Trigger**: operator m-3611 challenged dev's original Bug 2 fix proposal ("stderr provenance line") as too surface — operator's intuition: wrapper should give real git pass-through, not the apparent "mock view". Lead instructed audit-first.

---

## Operator framing reconciliation

The operator's m-3611 phrasing was: "wrapper should give real git pass-through, not mock view." This audit clarifies a terminology divergence before answering the three questions:

The wrapper does **not** present a mock view. For the **bound** agent case, it does an explicit `git -C <worktree>` chdir-pass to the agent's assigned worktree directory and runs the real `git` binary there. The "mock" perception came from a specific operator scenario (verifiable via my live `binding.json` snapshot in the predecessor RCA) where the assigned worktree directory was a daemon-provisioned sentinel containing only the `.agend-managed` lease marker — not the populated source checkout. `git -C <empty-dir> status` correctly reports a clean tree because that tree IS clean; the wrapper isn't lying, the worktree itself is empty.

This reconciliation matters for the verdict: the bug isn't a wrapper-logic mock; it's a worktree-provisioning gap surfacing as a visibility-deficit at the agent's vantage point.

---

## Q1 — Why does the wrapper ChdirPass instead of pass-through for bound case?

### Code path trace (`src/bin/agend-git.rs`)

1. `main` (L19-47): reads `AGEND_INSTANCE_NAME` + `AGEND_HOME`, calls `read_binding`, then dispatches via `classify(subcommand, args, &binding)`.
2. `read_binding` (L85-115): loads `home/runtime/{agent}/binding.json` into `Binding { task_id, branch, worktree }`. Parse failure → `Binding::default()` (all `None`). Missing-worktree-path also degrades to `default()` (P0-1.6 orphan defense).
3. `is_bound` (L117-119): `binding.task_id.is_some()`. False when default. **Missing-provenance therefore routes to UNBOUND, not to a "mock view".**
4. `classify` (L129-192) — the auto-chdir site:
   - Read-only commands (`status`, `log`, `diff`, `show`, `blame`, `ls-files`, `ls-tree`, `rev-parse`, `fetch`, `remote`, `branch`, `tag`, `describe`, `shortlog`, `reflog`):
     - **bound + worktree resolves** → `Action::ChdirPass(worktree)` → wrapper exec's `git -C <worktree> <subcommand> <args...>`.
     - **unbound** → `Action::Passthrough` → real git in caller's cwd. *(This is exactly the pass-through behaviour the operator described — it already exists for the unbound case.)*
   - Mutating commands (`commit`, `push`, `pull`, `reset`, …): denied if unbound; ChdirPass if bound.
   - `worktree`: always denied.
5. `exec_real_git` (L196-222): when `chdir = Some(dir)`, prepends `-C <dir>` to the real-git arg list and `Command::exec()`s.

The wrapper does **not** emit a marker on `ChdirPass` — the agent has no signal that the chdir happened.

### Why ChdirPass at all?

The bound case routes git ops to the agent's assigned worktree regardless of where the agent's `cwd` actually is. Without this, a bound agent running `git status` or `git commit` from `/tmp`, `$HOME`, or an unrelated workspace directory would target the wrong repository — either silently corrupting the operator's source tree or producing nonsensical results. The chdir is not a coincidence; it's the wrapper's reason to exist for bound agents.

---

## Q2 — Design accident or legitimate?

### Git history evidence

`git log -- src/bin/agend-git.rs` shows the wrapper landed in 4 commits:

| Commit | PR | Title |
|---|---|---|
| `acb3763` | #447 | feat: agend-git-shim Phase 2 — shim binary + binding lifecycle + deny + bypass |
| `42fbe30` | #449 | feat: agend-git-shim Phase 3 — worktree lease/release lifecycle (no GC) |
| `949aca3` | #466 | fix(worktree): verify actual HEAD before reuse — close P0-1.6 silent-lie |
| `f83cc11` | #515 | fix(agend-git): expand emit_deny_error hint to list 3 bypass forms (Sprint 54 P2-4) |

Phase 2 (PR #447) introduced `Action::ChdirPass`; Phase 3 wired worktree lifecycle. P0-1.6 (#466) added the orphan-binding-path defense that demotes bound→unbound when the worktree path doesn't exist on disk. Sprint 54 P2-4 (#515 — my own predecessor PR) expanded the bypass hint without altering ChdirPass logic.

### Design proposal evidence

`docs/proposals/agend-git-shim.md` is the design document for the wrapper. Two passages are decisive:

**§2.3 Shim 行為矩陣** (L171-184): the proposal explicitly enumerates `chdir + pass (worktree)` as the bound-case action for **every** read-only and mutating command. Pass-through is reserved for the unbound case. The matrix shape ships unchanged in `agend-git.rs::classify`.

**§Phase 2 Exit criteria** (L471-481):
> - bound 模式：agent 從任意 cwd 跑 git，shim 自動 chdir 對 worktree
> - unbound 模式：行為跟原本 git 一樣

Translation: in bound mode, the agent runs git from any cwd and the shim auto-chdirs to the worktree; in unbound mode behaviour matches plain git. The cross-cwd-anywhere capability is named as an explicit Phase 2 exit gate.

**§A.3 PoC 驗證範圍** (L755):
> | A2 | shim 自動 chdir 對 agent 透明 | ✅ | 從 `/tmp` / `$HOME` / agend workspace 跑 git，全部看到 bound worktree（feature-x）狀態，**agent 完全不需 cd** |

Translation of the verdict column: from `/tmp`, `$HOME`, or the agend workspace, all see the bound worktree's state; the agent doesn't need to cd at all. **Transparency to the agent — the very property that became my PR #506 trap — is named and validated as Acceptance Criterion A2.** This is not an accident; it's the headline feature.

### Tests / docs evidence

`docs/proposals/agend-git-shim.md` is referenced from the codebase indirectly (the binary doc-comment at `src/bin/agend-git.rs:1-12` recapitulates the same three actions). No test asserts pass-through-instead-of-chdir for bound case — the absence is consistent with the design treating ChdirPass as the contract.

### Conclusion

ChdirPass for bound case is **legitimate and intentional**. The transparency to the agent is an explicit Phase 2 acceptance criterion, validated by PoC, and shipped as the operative wrapper behaviour. Calling it an accident would contradict the design proposal directly.

---

## Q3 — What invariants would `pass-through` (instead of ChdirPass) break?

If the wrapper were changed to pass-through for the bound case (i.e. drop the `-C <worktree>` injection and let real git run in caller cwd), three production invariants break:

### Invariant 1: Cross-cwd-anywhere capability (Phase 2 Exit Criterion §A2)

The proposal's headline feature — a bound agent running `git status` from `/tmp` or `$HOME` getting their assigned worktree's state — relies on the chdir. Pass-through means git operates on whatever the caller's cwd is, which for bound agents is rarely the worktree (PoC scenarios `/tmp`, `$HOME`, "agend workspace" are all non-worktree). The agent would see "fatal: not a git repository" or "fatal: not a git directory" or, worse, the operator's source tree's state — every kind of confusing output the chdir was designed to prevent.

### Invariant 2: Cross-agent isolation (Fleet Protocol §10.4)

§10.4 mandates per-agent worktrees so concurrent agents can't step on each other's commits. The wrapper's chdir is the enforcement point: regardless of where the agent is, mutating git commands land in the agent's assigned worktree. Pass-through removes this enforcement. A bound agent with cwd somewhere in the operator's source tree would `git commit` into the operator's checkout, contaminating it.

### Invariant 3: Deny matrix coverage

The `worktree` subcommand is always denied — the wrapper assumes worktree creation is a daemon responsibility, not an agent responsibility. The deny is enforced at the classify level and doesn't depend on where the actual git command runs. Pass-through alone wouldn't break this (deny still fires before exec). However, the cross-branch protection at `checkout`/`switch` does depend on the wrapper resolving the bound branch and comparing the target — that logic doesn't itself care about chdir. So the deny matrix isn't directly broken by removing chdir, but is hollowed out: the protections against unbound mutation still fire (good), yet bound agents now mutate the wrong repo (bad), so the system loses isolation despite the deny matrix being intact.

### Invariant 4 (latent): Trailer-hook injection

`docs/proposals/agend-git-shim.md` §2.2 + §A.4 describe the `Agend-Agent: …` / `Agend-Task: …` / `Agend-Branch: …` commit-message trailer hook. The hook is configured per-worktree. Pass-through means commits land in caller cwd's git config, which doesn't have the trailer hook installed → trailers stop being written for bound agents. Phase 1 Trailer (cited in `Cargo.toml include` rationale, PR #505) loses its provenance.

### Net

Pass-through breaks at least three production invariants and one latent invariant. None of these is a marginal concern; each is named in the design proposal as a load-bearing property.

---

## Verdict — **RISK-IDENTIFIED-WITH-MITIGATION**

Three findings combine into the verdict:

1. **ChdirPass is by design, not an accident.** Phase 2 §A2 explicitly validates "shim 自動 chdir 對 agent 透明" and the cross-cwd-anywhere capability is named as a load-bearing acceptance criterion. Calling the wrapper's behaviour an accident would contradict the design proposal directly.
2. **The visibility deficit it produces is a real risk surfaced by an edge case the design didn't anticipate.** When a daemon-provisioned worktree is a sentinel-only directory rather than a populated checkout (my PR #506 incident scenario), the auto-chdir-then-clean-status sequence reads to the agent as "nothing to commit" while the operator's actual edits sit elsewhere. The risk is information-deficit, not wrapper-logic-bug.
3. **The original fix proposal — stderr provenance line on each ChdirPass — addresses the risk without sacrificing the design's load-bearing properties.** The operator's m-3611 alternative ("real git pass-through") would break four invariants enumerated above; that proposal was a good-faith intuition formed before the audit clarified that the wrapper IS doing real chdir to a real worktree (just one that happened to be empty in the trigger scenario).

Hence: **LEGITIMATE design, identified visibility risk, mitigation available** — which the audit decision tree maps to RISK-IDENTIFIED-WITH-MITIGATION → fix dispatch with mitigation plan.

The audit's role is to provide the operator with the fuller picture: ChdirPass auto-targeting is the wrapper's reason to exist for bound agents (cross-cwd-anywhere is named in the proposal); the real-world confusion came from a daemon-provisioning edge case combined with the wrapper's silent-transparency property; the original mitigation proposal addresses the visibility half without touching the design half. The recommendation below is the path that respects both.

---

## Recommended re-design — preserve design intent + address the real visibility deficit

The operator's pain stems from one specific scenario:

> Bound agent's worktree binding points to a populated daemon-provisioned directory in nominal cases. In the operator's recent incident scenario the binding pointed to a directory containing only `.agend-managed` (lease marker) and no source checkout. `git -C <that-dir> status` correctly reported "clean" because the dir IS clean — but the agent had no signal that the worktree was a sentinel rather than a proper checkout.

Two fix layers preserve the cross-cwd-anywhere design while closing the visibility deficit:

### Layer 1 — surface the chdir (low-risk, original PR #509 proposal stands)

Wrapper emits one stderr line on every `Action::ChdirPass`:

```
agend-git: ran from worktree binding {wt} (cwd={cwd}, agent={agent}, task={task_id})
```

Suppressible via `AGEND_GIT_QUIET=1` for non-interactive scripts. This does NOT change behaviour (chdir still happens, agent commands still land in the worktree). It changes signal: the agent now sees the chdir event, can compare `wt` vs `cwd`, and notices anomalies like "wt is empty" by following up with `ls {wt}`.

**Tier-1, ~30-50 LOC, no invariant change.** Approved by the audit because it preserves Phase 2 §A2 transparency where it matters (the auto-chdir still happens automatically) while breaking transparency only for an out-of-band debugging signal.

### Layer 2 — empty-worktree warn (optional, addresses the specific operator scenario)

When ChdirPass resolves a worktree path, also emit a stderr warn line if the directory contains only the lease marker:

```
agend-git: worktree {wt} appears to be a lease-only sentinel (no source checkout) — \
           daemon may not have provisioned content; consider release+reissue or contact lead
```

Detection cost: one `read_dir` + one filter for `.agend-managed`. Same opt-out via `AGEND_GIT_QUIET=1`.

**Tier-1, ~20-30 LOC, no invariant change.** Optional — Layer 1 alone gives the agent enough signal to investigate; Layer 2 makes the most common confusion case self-explanatory.

### Combined re-design footprint

~50-80 LOC, single file (`src/bin/agend-git.rs`), all behaviour additive. Tests cover the stderr-line shape and the empty-worktree detection.

The operator's pass-through alternative is **incompatible with Phase 2 §A2**: cross-cwd-anywhere requires the chdir as the enforcement point. Layer 1+2 give the operator the visibility they need without sacrificing that capability or the three other invariants (§10.4 isolation, deny-matrix isolation, trailer-hook provenance). The framing for the operator: this audit clarifies that the wrapper IS doing real chdir to a real worktree — the empty-checkout case that triggered m-3611 was a daemon-provisioning gap, not wrapper-logic; the original Layer 1 mitigation surfaces that gap to the agent so it's catchable in the next incident without changing the design.

### Risk profile of the re-design

- **Low**: stderr noise on every git invocation. Mitigated by `AGEND_GIT_QUIET=1`.
- **Low**: scripted CI tooling that parses stderr might trip on the new line. Same mitigation. Possible secondary mitigation: emit only on bound case (the case agents actually need to know about; unbound passes through unchanged).

---

## Embargo discipline confirmation

`git diff --stat main..HEAD` against this branch should show only `docs/AUDIT-sprint54-chdirpass-design-intent-2026-05-08.md` added. Zero `src/`, `Cargo.toml`, or test changes. Markdown-only, doc-only.

---

## Proposed next-step dispatch

Based on this audit's **RISK-IDENTIFIED-WITH-MITIGATION** verdict + recommended Layer 1 (+ optional Layer 2) mitigation:

1. Lead synth-reports to operator: audit findings + verdict + Layer 1/2 mitigation. Operator's m-3611 intuition ("something off") was correct on the symptom; the audit clarifies the mechanism (auto-chdir to a possibly-empty real worktree, not a mock view) so they can make a fully-informed call.
2. If operator confirms Layer 1 (+ optional Layer 2): dispatch a Tier-1 single-primary fix PR for `src/bin/agend-git.rs` (~50-80 LOC, additive only, tests included).
3. If operator still prefers pass-through after seeing the §A2 / §10.4 / deny-matrix / trailer-hook impacts: re-audit with the specific invariant the operator is willing to relax (likely a §A2 redesign conversation rather than a wrapper patch).

Doc-only embargo path complete; no code change in this PR per dispatch directive.
