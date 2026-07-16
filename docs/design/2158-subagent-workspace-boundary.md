# #2158 Design Spike — sub-agent workspace boundary hardening

> **Historical design snapshot:** The SHA and gaps below describe the code at
> investigation time. This file is not a current security or worktree contract;
> re-verify every claim against current source and `docs/FEATURE-worktree.md`.

**Status:** SPIKE (analysis only — no production code). For lead dialectic (dual reviewer, security).
**Freshness:** origin/main 509b7af. **Author:** fixup-dev-2.
**Proportionality:** single-machine single-user → defense-in-depth + fail-closed, NOT OS isolation.

## 1. The incident (3 distinct gaps, issue #2158)
A transient sub-agent sharing the primary's identity `claude-fb80a0`, handed a bad path (literal `"undefined"`):
1. **Silently rebound the PARENT** via `repo checkout bind:true` — moved the primary's `binding.json` to a branch it never chose + armed a CI watch.
2. **Cross-workspace FS writes** — `*.zh-TW.md` into the main working tree, peer agents' workspaces, and 2 stray bypass-worktrees.
3. **Stray worktrees via `AGEND_GIT_BYPASS=1`** — created, un-audited.

## 2. CRUX finding (corrects the issue's framing)
**A sub-agent is INDISTINGUISHABLE from the primary.** Both read the same `AGEND_INSTANCE_NAME` (`identity.rs:29-33`); the MCP handler receives only the string identity (`HandlerCtx.instance_name` / `sender`, `mcp/handlers/mod.rs:81`) — **no PID, ppid, or sub-agent marker** (`std::process::id()`/`getppid()` absent from the identity path). The sub-agent spawner (Claude Code Task tool) is EXTERNAL — we cannot make it set an `AGEND_SUB_AGENT=1` marker.

⟹ The issue's direction-1 ("make bind a no-op when invoked from a sub-agent context") is **NOT implementable** — there is no signal to gate on. Direction-1 must be reframed:
- **prevent the TRIGGER** (bad/ambiguous path → fail-closed), and
- **DETECT the rebind** (audit-log with process context), since identity-based prevention is impossible.

## 3. Per-gap mechanism (file:line)
- **Rebind:** `handle_bind_self` (`mcp/handlers/worktree.rs:32`) takes the verified `sender` (good — no sibling impersonation) but the sub-agent IS the primary's name → rebinds it. `handle_checkout_repo` (`ci/mod.rs:264`) → `binding::bind_full` with `instance_name` and **no owner-authority guard** (no `ctx.sender == target` precedent anywhere — Q4).
- **Bad-path resolve:** `ci/mod.rs:91-107` — source path resolution: absolute/`~` expand; else agent-name lookup; **else FALLBACK `source.to_string()` (line 106)** → `canonicalize()` (`:109`). A truly-nonexistent `"undefined"` → `canonicalize` Err → **already fails**. BUT a **relative** bad path canonicalizes against the daemon's implicit cwd → silently resolves to *something* (the silent-resolve gap). System-path guard exists (`/etc,/usr,…` `:113`) but not a "must be a known source repo" allow-list. `bind_self` guards `..` (`worktree.rs:73`) but not relative-vs-absolute.
- **Bypass:** honored centrally at `git_helpers.rs:59` (`git_bypass_timeout`) — **zero event-log audit**; the instance/caller context isn't even threaded to that site (would need an API change).
- **Cross-workspace writes:** raw FS — daemon cannot gate (no syscall hook). `has_uncommitted_changes` (`worktree.rs:445`) is per-worktree; **no cross-workspace/main-tree sweep exists**.

## 4. Proposed boundaries (proportional, phased — each its own PR + dual-review)

### PR1 — bad-path fail-closed (the actual trigger; smallest, highest-confidence)
Tighten the source-repo path contract in `handle_checkout_repo` + `handle_bind_self`:
- **Reject a non-absolute source path** before canonicalize (kills `"undefined"`/relative resolving against the daemon cwd) — the resolution must be a known absolute repo, never an implicit-cwd guess.
- Optionally constrain canonicalized source to an **allow-list root set** (registered source repos + `$AGEND_HOME`); reject out-of-root.
- ⚠ Verify-before-impl: confirm no legitimate caller passes a relative `repository_path` (the agent-name lookup path stays — only the literal-fallback relative resolve is removed).

### PR2 — audit bind/rebind + AGEND_GIT_BYPASS (detection, since prevention-by-identity is impossible)
- `event_log` every `bind_full` / rebind with **caller process context** (`std::process::id()`, `libc::getppid()`, cwd) so an unexpected rebind ("primary never requested this") is visible post-facto.
- `event_log` every `AGEND_GIT_BYPASS` worktree creation with caller context (needs threading the context to `git_bypass_timeout` — small API change).

### PR3 — cross-workspace / stray-worktree detection sweep (direction-2; largest, lowest-urgency)
- New sweep hooked into `GcTickHandler::run` (`per_tick/gc_tick.rs`, ~60-min cadence): enumerate agent worktrees + main tree, flag (a) `??` untracked files in a worktree whose owner isn't live, (b) untracked files in the main working tree, (c) git worktrees outside the canonical `$AGEND_HOME/worktrees/<agent>/` layout (strays).
- Surface via `event_log` + a new `BlockedReason::WorkspaceBoundaryViolation { detail }` (`health.rs`, ~13 LOC) so operator sees it through `health action=report` — not by accident.

## 5. Decision points (for dialectic / operator)
- **DP1 — rebind: audit-only, or also a guard?** Identity can't distinguish sub-agent, so we can't *prevent* by context. Options: (a) audit-only (recommended — matches the issue's "at minimum audit-log"); (b) ALSO require a re-bind confirmation token / reject a bind that *changes* an existing live binding without a release first (risk: breaks legit release→re-checkout flows). Recommend (a) + PR1 fail-closed; flag (b) as heavier.
- **DP2 — fail-closed strictness.** Reject relative paths only (minimal), or full allow-list-of-roots (stricter, more blast — must enumerate legit roots)? Recommend relative-reject first; allow-list as a follow-up if needed.
- **DP3 — scope/phasing.** PR1 (fail-closed) is the direct trigger fix and highest-value; PR2 (audit) makes the rebind visible; PR3 (detection sweep) is the broadest but lowest-urgency. Recommend PR1 → PR2 → PR3, sequenced (lead may defer PR3).

## 6. Blast / risk
- PR1: path-validation tightening — risk = a legit relative-path caller breaks; mitigated by verify-first (the agent-name lookup path is preserved). Surgical.
- PR2: additive logging — near-zero blast. Bypass-audit needs caller-context threading (touches `git_helpers` signature + call sites).
- PR3: additive read-only detection (no mutation) + 1 enum variant — low blast; main risk is false-positive noise (apply the t-127/t-116 noise-reduction lens: fire-once/dedup, surface once not per-tick).

## 7. Recommendation
Land **PR1 (fail-closed)** first — it directly closes the actual incident trigger and is the most surgical. **PR2 (audit)** next — the realistic answer to silent rebind given the indistinguishability crux. **PR3 (detection)** as a sequenced follow-up. All proportional; none needs OS isolation.
