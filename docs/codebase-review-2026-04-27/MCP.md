# Sprint 20 Track D Audit: src/mcp/ + admin CLI

**Auditor**: dev-reviewer-2
**Date**: 2026-04-27
**audit_mode**: `codebase_audit`
**Scope source**: Sprint 20 final scope freeze `d-20260426210724891457-5` (post-challenge round 11 修正項) + Track D dispatch `m-20260426210915021477-80`
**Tier**: Tier-2 (audit-only, doc-only deliverable; `audit_mode=codebase_audit`)
**Time-box**: 2h hard cap

---

## Methodology

(Per challenge round R1 — audit transparency)

- **audited_head**: `1485e85eab70ceeb43d794ecb586ee0b72d0bf04` (origin/main at audit start)
- **commands_run**:
  - `git fetch origin main`
  - `git worktree add -b sprint20-track-d-mcp-audit ../agend-terminal.worktrees/sprint20-track-d-mcp-audit origin/main`
  - `git log --diff-filter=A -- src/mcp/`, `git log -- src/admin.rs` (comfort-zone first-pass)
  - `wc -l src/mcp/*.rs src/admin.rs src/tasks.rs src/decisions.rs src/inbox.rs`
  - `grep -n '"<tool>" =>' src/mcp/handlers.rs` (handler dispatcher map)
  - `grep -n 'json!({"name":' src/mcp/tools.rs` (tool definition surface)
  - `grep -n 'unwrap()|expect(|panic!|todo!|unimplemented!|unsafe' src/mcp/ src/admin.rs`
  - `grep -nB2 -A6 'fn can_mutate_task' src/tasks.rs`
  - `grep -nA15 'pub fn update' src/decisions.rs`
- **files_scanned** (line counts at audited_head):
  - **Tier-1 hot deep-read**: `src/mcp/tools.rs` (417 lines, full), `src/mcp/mod.rs` (348 lines, full), `src/admin.rs` (264 lines, full), `src/mcp/handlers.rs` (3143 lines — dispatcher map + ~10 critical handler functions read; bulk of handler bodies NOT line-by-line, see Coverage caveat)
  - **Tier-2 walkthrough**: `src/tasks.rs` (1572 lines — `can_mutate_task` + `handle` entry only), `src/decisions.rs` (410 lines — `post` + `update` only)
  - **Tier-3 grep**: `src/inbox.rs` (2396 lines — entry-point grep only)
- **Comfort-zone first-pass**: PR-AW MCP enum (`src/mcp/tools.rs:174`), PR-AR admin (`src/admin.rs`), PR-AY task ownership E3.1 (`src/tasks.rs:can_mutate_task`), PR-AS Discord channel (referenced but Track A scope) — all explicitly visited.

---

## Findings

(Critical/High/Medium/Low. Path-keyword auto-Critical applied per Sprint 19 challenge #2: `auth/security/crypto/handlers/` + `check/verify/validate/audit/authorize` keyword in unused/missing-gate finding → auto-Critical regardless of gut call.)

### Critical

**C1. `decisions::update` has no author/ownership gate** (`src/decisions.rs:184-236`)
Path: authorization handler. Path-keyword auto-Critical.

`pub fn update(home, args)` takes only `home + args` — no `caller / instance_name` parameter. Any agent calling `update_decision` MCP tool can flip `decision.archived = true` on any decision regardless of `decision.author` (which IS recorded at post time per `:75 pub fn post(home, author, args)`).

**Inconsistency with task ownership rule**: `src/tasks.rs:59 fn can_mutate_task(home, caller, task)` correctly gates task mutation by assignee. Decisions miss the parallel gate.

**Concrete attack surface**: a compromised or prompt-injected agent could `update_decision { id: "d-20260425101114174249-11", archive: true }` to archive operator's strategic channel-extension direction. No log of who archived it (handler doesn't capture caller).

**Fix shape**:
- Add `pub fn update(home, caller: &str, args)` signature
- `with_decision_lock` block: read decision, check `decision.author == caller || caller is operator`, fail with `"error": "decision 'X' owned by 'Y', caller 'Z' not authorized"` (mirror existing tasks error wording at `tasks.rs:378`)
- `mcp/handlers.rs:1104` callsite passes `instance_name`

### High

**H1. Destructive-op handlers have no per-agent authorization gate** (`src/mcp/handlers.rs:796 delete_instance`, `:912 replace_instance`, `:969 interrupt`, `:994 tool_kill`, `:1321 clear_blocked_reason`)
Path: `handlers/` + destructive-op semantics.

Each handler validates *names* via `crate::agent::validate_name` (syntax) but does NOT use the caller's `instance_name` as an access check. `instance_name` is captured (e.g., `:956 tracing::info!(%name, %reason, "replace_instance")`) for logging only. Trust model: any agent in scope can call these on any other agent; ACL (`AGEND_MCP_TOOLS_ALLOW`/`DENY`) via env is the only guard.

This is **by design** per the multi-agent fleet model — agents collaborate by destructively replacing each other (operator-authorized respawn flow). But the design is **undocumented at handler-scope**; an unwary contributor could "tighten" the model by accident, breaking the dual-review and replace_instance flows we now rely on.

**Fix shape (doc-only, no code change)**:
- Module-level docstring in `src/mcp/handlers.rs` calling out: "destructive ops trust the MCP ACL as the only auth gate; per-agent authorization is intentionally absent so operator-driven respawn / replace flows work; tighten by editing ACL not by adding handler-level checks"

**H2. Admin `cleanup-branches` from PR-AR (3 sub-findings, already in backlog `t-20260426120555737962-8`)**

H2a. **Detached worktree branches not detected** (`src/admin.rs:25-40 worktree_branches`)
`strip_prefix("branch refs/heads/")` only matches *named-branch* worktrees. Detached worktrees emit `HEAD <sha>` lines instead. Result: a branch checked out via `git worktree add /path <sha>` (detached) is undetected; the admin CLI may flag its branch for deletion if a merged PR exists. `git branch -D` would then error (worktree refuses to delete checked-out branch), causing a "FAILED" log line but the underlying branch is preserved. Annoying noise, not a data-loss bug. **Severity: Medium-High** (path-keyword `audit` keyword).

H2b. **Audit log path in repo root, not gitignored** (`src/admin.rs:96-99`)
`repo.join(format!(".agend-terminal-branch-cleanup-{}.log", ...))` writes to repo root with date suffix. Not in `.gitignore` by default. Risk: operator commits the log accidentally. Fix: write to `~/.agend-terminal/logs/<repo-name>/branch-cleanup-{date}.log` or to `.git/agend-cleanup-log/` (inside `.git` is git-ignored by construction).

H2c. **Hardcoded `"main" || "master"` SkipMain protection** (`src/admin.rs:79`)
Repos using `develop`/`trunk`/non-default primary branch lose the SkipMain guard. **Fix**: detect via `git symbolic-ref refs/remotes/origin/HEAD` or `git config --get init.defaultBranch`, fall back to `"main"`/`"master"` if detection fails.

### Medium

**M1. `MCP ACL` is OnceLock-cached at first call** (`src/mcp/mod.rs:40-44 tool_acl`)
`AGEND_MCP_TOOLS_ALLOW`/`DENY` read once via `OnceLock`; runtime env changes ignored. **By design** for performance, but operator changing ACL must restart the MCP server to take effect. Document this in tool description or `mcp/mod.rs:14-19 docstring` so operator doesn't expect hot-reload.

**M2. `update_decision` silently accepts unknown fields** (`src/decisions.rs:208-218`)
Caller-provided args are read field-by-field via `args["content"].as_str()`, etc. Unknown fields (typo or schema drift) are silently ignored. Future field rename would create silent failure mode. Add a "known fields" check or use serde struct deserialization with `#[serde(deny_unknown_fields)]`.

**M3. `task` MCP tool has two paths to "done"** (`src/tasks.rs:198 handle`)
`task action=done` (line 325) does proper completion (sets timestamp + result + clears assignee semantics). `task action=update status=done` (line 355) just writes string. v1.2 §10.3 prescribes `task update --status done` for the dev-lead-on-merge step, and per PR-AW the enum now includes `done`. Both paths are valid but produce subtly different state (`update`-path may not trigger the dependency-bookkeeping at line 96-106). Audit-time finding: tests cover both paths separately but no test cross-references the resulting decision-board state. Risk: future bug where `update --status done` leaves stale dep linkage.

### Low

**L1. `handlers.rs` is a 3143-line monolithic dispatcher** (`src/mcp/handlers.rs`)
~50-arm match expression in `handle_tool`. Long-term navigability hit; finding individual handlers requires search not directory traversal. Refactor opportunity (see "Refactor opportunities" below).

**L2. Tool description vs handler behavior drift surface** (`src/mcp/tools.rs` vs `src/mcp/handlers.rs`)
Some tool descriptions (e.g., `:152 list_decisions: "List active decisions."`) hide nuance — `list_decisions` filters via args (scope, archived, etc.) per `:1103`. Future tool-description PR could surface filterability for client UX. Not a functional issue.

---

## Praise

(Per challenge round R3 — sub-bucketed: `replicate / preserve-as-is / refactor-eventually`)

### Replicate (pattern worth applying elsewhere)

- **`mod.rs::tool_acl` OnceLock pattern** (`src/mcp/mod.rs:40-44`) — clean process-scope config-from-env caching with parse_csv helper. Suitable template for any "read env vars once at startup, never reload" need (rare-but-real pattern). Testable via injecting allow/deny sets to `check_allowed` directly.

- **`tasks::can_mutate_task`** (`src/tasks.rs:57-72`) — centralized authorization predicate. Single function to audit when changing trust model. Mirror this pattern for decisions (see C1) and any future authority-mutation path.

- **`decisions::with_decision_lock` per-decision flock** (`src/decisions.rs:68-73`) — replaces the earlier load_all+save bug. Pattern: scope concurrency to the entity, not the collection. Mirror for inbox (per-message lock) when contention shows up; today's mutex is still fine.

### Preserve-as-is (load-bearing complexity, do NOT copy mechanically)

- **`mcp/mod.rs::proxy_or_local` daemon proxy fallback** (`src/mcp/mod.rs:260-281`) — looks "redundant" (try API, fall back to local) but the fallback is the only thing keeping standalone (no-daemon) tools working. Don't simplify by removing the local-handler branch.

- **`handlers.rs` `instance_name` threaded through every handler** — repetitive, but every threading site is an explicit accountability decision: "this handler logs / scopes by caller". Removing the parameter would be tempting but each callsite is intentional.

- **`mcp/mod.rs::read_message` Content-Length resync** (`src/mcp/mod.rs:109-145`) — comments document a prior bug (silent stream desync on garbage Content-Length). The current resync logic is verbose because it's load-bearing. Test `read_message_resync_after_bad_content_length` pins the contract. Don't refactor without re-reading the bug history.

### Refactor-eventually (not urgent, ROI when touched)

- **`handlers.rs` 3143-line dispatcher** — split into `mcp/handlers/{messaging,instance,decision,task,team,schedule,deployment,ci,health}.rs` per category. Not bug-fix urgency; pay the cost only when adding 5+ new handlers in a category.

- **`admin.rs` 264 lines, single namespace** — fine today; if more admin commands land (e.g., `cleanup-tasks`, `cleanup-decisions`), promote to `src/admin/{branches,tasks,...}.rs` directory.

---

## Coverage

(Per challenge round 對立 — explicit depth declaration)

| Sub-path | Lines | Depth | Rationale |
|---|---|---|---|
| `src/mcp/tools.rs` | 417 | **Full** read | Hot zone (PR-AW enum) + tool surface map |
| `src/mcp/mod.rs` | 348 | **Full** read | Auth/ACL + Content-Length parser are both load-bearing |
| `src/admin.rs` | 264 | **Full** read | PR-AR backlog (4 known findings) + small enough |
| `src/mcp/handlers.rs` | 3143 | **Surface + 10 critical handlers**: `delete_instance`, `replace_instance`, `start_instance`, `set_display_name`, `interrupt`, `tool_kill`, `clear_blocked_reason`, `post_decision`, `update_decision` route, `task` route. Bulk of message-routing/inbox-lookup handlers NOT line-by-line. | Time-box constraint — 3143 lines exceeds 2h budget alone if read line-by-line. Spot-checked authorization-relevant handlers per R2 path-keyword priority. |
| `src/tasks.rs` | 1572 | **`can_mutate_task` + `handle` entry only** (~5%) | Track D scope is "task ownership rule" not "task internals". Ownership fn audited; rest is Track D-adjacent (Track B might overlap). |
| `src/decisions.rs` | 410 | **`post` + `update` only** (~30%) | Authority-relevant entries deep-read; list/load skim. |
| `src/inbox.rs` | 2396 | **Grep only** | Track D-adjacent (inbox API surface called by MCP handlers); deep audit is Track B/C territory. |

**Honesty caveat**: `handlers.rs` 90% NOT deep-read. The 10% read covers all path-keyword high-risk handlers (auth/destructive/ownership). Findings C1/H1 derive from those reads. **A future audit deep-diving into messaging/inbox/health/instance routes may surface additional findings.**

---

## Refactor opportunities

1. **Decisions ownership gate** (Critical fix per C1) — add `can_mutate_decision` paralleling `can_mutate_task`.
2. **handlers.rs module split** (per Praise: refactor-eventually) — `mcp/handlers/{messaging,instance,decision,task,team,...}.rs`.
3. **PR-AR admin backlog** (per H2a/b/c) — already tracked as `t-20260426120555737962-8`.
4. **`update_decision` schema strictness** — `#[serde(deny_unknown_fields)]` on a typed struct.
5. **MCP tool description filter surface** — list_decisions etc. could surface filter args (e.g., "filter by tag"). Cosmetic.

---

## Cross-area dependencies

(Per challenge round #4 — dual-labeled when crossing area boundaries; primary owner = home of the symbol, secondary = caller)

| Dependency | Primary | Secondary | Audit note |
|---|---|---|---|
| `mcp/handlers.rs` calls `crate::api::call(home, json!{ method: KILL/DELETE/SPAWN/INJECT })` | Track B (daemon) | **Track D** | Every destructive op routes via daemon API. Track B auditor should see the API surface here as a "well-known caller" reference. |
| `mcp/handlers.rs:830` calls `telegram::lookup_topic_for_instance` | Track A (channel) | **Track D** | Couples MCP layer to Telegram-specific topic lookup. Track A may flag as "MCP shouldn't know about telegram"; Track D defers to Track A's channel-abstraction roadmap. |
| `mcp/handlers.rs:841` calls `telegram::delete_topic` | Track A (channel) | **Track D** | Same as above; Track A scope. |
| `mcp/handlers.rs:1107` `task` action delegates to `tasks::handle(home, instance_name, args)` | Track D (task ownership) | — | Internal to Track D; ownership rule is Track D-owned. |
| `admin.rs::execute_cleanup` shells out to `git`/`gh` | Track D (admin CLI) | (peripheral) | External CLI dependency; no tracked area owns shell-out abstraction. |
| `mcp/mod.rs::proxy_or_local` daemon-proxy path | Track B (daemon) | **Track D** | Track D-side caller; Track B owns the API method dispatch. |

**Conflict resolution caveat**: For the `mcp/handlers.rs` ↔ `telegram::*` coupling, the framing here ("Track D defers to Track A's channel-abstraction roadmap") may differ from Track A's framing. Per challenge round #4, both reports file the dependency; dev-lead synthesis triangulates.

---

## Sprint 21 actionable tasks

(Per challenge round #8 — actionable items extracted from this audit)

1. **[Critical] `decisions::update` author gate** — implement `can_mutate_decision(home, caller, decision) -> bool` paralleling `can_mutate_task`; thread `instance_name` through `mcp/handlers.rs:1104` and `decisions::update` signature. Ship as single PR with regression test (`decision_update_rejects_non_author_caller`). Path-keyword auto-Critical.

2. **[High] Destructive-op trust model docstring** — module-level doc in `src/mcp/handlers.rs` documenting "ACL is the only per-agent auth gate; destructive ops have no handler-level check by design (see fleet trust model)". Doc-only PR.

3. **[Medium] PR-AR admin follow-up** — work the 4 known findings (already in backlog `t-20260426120555737962-8`): detached-worktree detection, audit log path move, default-branch lookup, positive Delete-path test.

4. **[Medium] `update_decision` schema strictness** — convert to typed struct + `#[serde(deny_unknown_fields)]`.

5. **[Medium] `task done` vs `task update --status done` reconciliation** — either consolidate or document the semantic difference; add cross-reference test that `update --status done` produces equivalent state to `done` action (or document the divergence).

6. **[Low] `handlers.rs` module split** — `mcp/handlers/{...}.rs` per category. Pure refactor; multiple PRs needed.

7. **[Low] `MCP ACL` hot-reload OR doc** — either implement env-var re-read on SIGHUP or document in `mcp/mod.rs:14-19` "ACL applies at process start; restart daemon after edit".

---

## Peer-pass critique reading Track C TUI.md

(Per challenge round 對立 peer-cross-check, C↔D diagonal. Track C's reciprocal pass on this report is in `TUI.md:248-252`. Read: `b1858aa docs/codebase-review-2026-04-27/TUI.md`, 253 lines, audited `1485e85`.)

**1 paragraph blindspot critique from MCP visibility**:

Track C's strongest finding is H1 (5 unbounded `area.height - N` panic sites in `render.rs` overlays, with PR #194 vterm OOB hotfix as prior incident). The severity is correctly tagged HIGH at the *crash-class* level, but the framing **stops at TUI presentation impact** — from MCP-side visibility, the same panic chain may have **availability consequences for the entire MCP tool surface**. If the TUI thread panics inside `render_tab_list` / `render_move_pane_target` / `render_confirm` and the daemon's panic policy is not strictly contained (e.g. a single-runtime panic propagates up through `tokio::select!` or the supervisor doesn't catch a TUI subtask abort), then MCP handlers `proxy_or_local`-routed via `crate::api::call` start failing with "API unavailable", silently degrading to local-handler fallback — **the very fallback path that Track D's M-tier "Preserve-as-is" Praise warned not to remove**. Track C and Track D should jointly verify in Sprint 21 whether a TUI overlay panic actually kills the API server thread (Track B/D ownership boundary). Second blindspot: Track C's *"(none observed)"* on Critical findings declares Track C is "presentation/state layer; security-relevant surfaces live in Tracks A/D" — but `src/state.rs` is a **PTY-output classifier** (Track C correctly self-corrects this in L1) whose classification signal feeds MCP scheduling decisions (`list_instances` → `agent_state` field → caller decides whether to delegate). A malicious or compromised agent producing classifier-confusion bytes (faking "Ready" state during a hang) would not be caught by any Track A/D auth gate — it's a **Track C trust boundary** that was scope-out in this audit pass. Recommend a Sprint 21 sub-track audit of `state.rs` regex/pattern robustness against adversarial PTY output, which neither this nor Track C's audit covered. Praise for Track C's R1 (overlay-dims helper extraction) — it elegantly closes both H1 and L2 in a ~30-line PR; matches my own "centralize the auth predicate" recommendation pattern (parallel to my `can_mutate_decision` Sprint 21 task #1). **Joint Sprint 21 priority alignment**: both reports flagged `src/app/api_server.rs` (130 lines, TUI↔MCP bridge) as cross-area-blindspot — agree this should be top of Sprint 21 backlog, ahead of either track's Tier-1 follow-ups.

---

*End of Track D audit + peer-pass. Audit time: ~2h within hard cap. Peer-pass time: ~30 min.*

---

## Cross-validation: Channel (Sprint 20.5 missing-pair)

(Per Sprint 20.5 scope freeze `d-20260426225921440175-6` — A↔D peer-pass not done in Sprint 20 main run because B↔A and C↔D were the only diagonals. Read: `docs/codebase-review-2026-04-27/CHANNEL.md` (Track A, dev-impl-1, audited `1485e85`, 277 lines including its own peer-pass to DAEMON.md). Audit metadata: `audit_mode=codebase_audit`, `peer-pass-tier`, time-box 2h hard cap.)

### Confirmed findings from Channel (✅ peer-confirmed from MCP angle)

**A's C1 `is_user_allowed` fail-open** — **confirmed Critical from MCP visibility, with severity elevation rationale**. Track A correctly tags this Critical at the channel layer, but the framing stops at *"anyone in the Telegram group can command the fleet"*. From MCP angle the **full end-to-end attack chain is**:

1. (Channel) `user_allowlist == None` → any Telegram group member's text reaches `handle_message` (telegram.rs:572-589 raw inject path)
2. (Channel) Message becomes `InboxMessage` enqueued for the bound instance — appears in agent's PTY as operator input
3. (Agent) Prompt injection in the message body causes agent to issue MCP tool call (e.g. `update_decision { archive: true }`, `delete_instance`, `replace_instance`)
4. (MCP D-side, my C1) `decisions::update` has **no author gate** — silently archives operator's strategic decisions (e.g. `d-20260425101114174249-11` channel-extension direction)
5. (MCP D-side, my H1) destructive handlers (`delete_instance` / `replace_instance` / `interrupt` / `clear_blocked_reason`) have **no per-agent auth gate** by design — silently respawn or kill any agent
6. (No forensics) Decision schema captures `author` at `post` time but not `archived_by` at `update` time — no audit record of *who* archived

**Cascade summary**: Channel A's C1 (fail-open inbound auth) + MCP D's C1 (no decision-update gate) + MCP D's H1 (no per-agent destructive-op gate) = **end-to-end "any random Telegram group member silently archives operator strategic decisions or kills production agents, with no forensic trail"**. Either layer's fix alone is insufficient — both must close. Track A's F1 (Sprint 21 high) + Track D's Sprint 21 task #1 (Critical) need **explicit cross-track sequencing**: ship together or document the residual exposure window between them.

**A's CD2 `active_channel().create_topic()` ignores Err** — confirmed from MCP angle; correctly identified me (Track D) as `primary_owner`. Agreed it should `tracing::warn!` the error. See "Disagreement" below for severity rebid.

**A's CD3 `ChannelKind` discriminator leak** — confirmed pattern. See "Cross-area systemic" below for elevation.

**A's H3 (mod.rs scaffold-unused doc drift)** — quietly affected my own audit: when I cited `active_channel()` from `mcp/mod.rs` in my Track D report I didn't notice the upstream comment claiming the module was unused. If A had flagged this earlier in the sprint, I would have framed my "preserve-as-is" praise for `proxy_or_local` differently (knowing the trait is in fact load-bearing, not scaffold).

### Missed findings discovered (MCP angle catches what Channel auditor missed)

**MX1 — Outbound notify severity contract drift A↔D** (NEW finding, neither track filed)

Track A's L1 notes `notify` impl drops `severity` (warn / error / info render identically). Track D's PR-AE3 (`active_channel().notify(NotifySeverity::Warn|Error|Info, ...)`) callsites are in `daemon/supervisor.rs:126,134`, `daemon/ci_watch.rs:622`, `daemon/mod.rs:608` — every callsite explicitly tags severity. So we have a **contract drift**: D-side callers believe severity-tagged events flow through; A-side renders all-equal.

**MCP angle impact**: when MCP `clear_blocked_reason` triggers a recovery notify with `NotifySeverity::Info` it visually matches a stall warning (`Severity::Warn`) in the Telegram group. Operator can't distinguish "hey, recovery happened" from "stall is back". This is **silent severity erasure** — a finding neither Track A (scope=channel internals) nor Track D (scope=MCP handlers, didn't trace into channel render) caught individually.

**Severity**: MEDIUM. Surface in Sprint 21 as A+D joint finding, fix in Track A (telegram render adds emoji prefix) per L1 + adds round-trip test "MCP severity → Channel render emoji" in cross-area test.

**MX2 — `mcp/handlers.rs:603-605` ChannelKind string-match parallel to A's CD3**

Track A's CD3 flags `inbox.rs:139-140` matching `ChannelKind::Telegram / Discord` to render strings. **My MCP audit also has the same anti-pattern** at `mcp/handlers.rs:603-605`:
```rust
let kind: &'static str = match kind_str {
    "telegram" => "telegram",
    "discord" => "discord",
    "slack" => "slack",
    _ => continue,
};
```

`"slack"` is referenced even though `ChannelKind` enum (`channel/mod.rs:118-121`) only has `Telegram | Discord`. So MCP D-side has a **stale string** for an enum variant that doesn't exist (yet). Discriminator leak is wider than CD3 captured — it's a systemic anti-pattern repeated in 2+ surfaces, with at least one stale instance.

**Severity**: LOW (today; would be MEDIUM if the stale "slack" string ever matched in production). Sprint 21 R-CD3 should canonicalise `ChannelKind` ↔ `&str` mapping in one helper (e.g. `impl AsRef<str> for ChannelKind`) and replace both Track A's inbox match AND MCP's handler match.

**MX3 — Decision-archive forensics gap (deeper than Track D's own C1)**

My MCP C1 said: "add `can_mutate_decision(home, caller, decision)` paralleling `can_mutate_task`". But re-reading from Channel-angle (which knows about Telegram group membership being the soft user gate): **even with the gate added, we don't log who archived**. Attacker chains to silently archive — gate rejects them — but we have no audit log of attempted archives. Track A's audit-log discipline (`fleet_events.jsonl` from PR #199) is the right pattern; Decisions need parallel `decision_audit.jsonl` capturing: timestamp / caller / decision_id / change-fields. **Missing in my own MCP.md**; surfaces only via cross-validation with A's audit-log pattern.

**Severity**: HIGH — without forensics we can't tell whether a future C1 attack has happened or is in progress.

### Disagreement / scope dispute

**CD2 priority rebid**: Track A labels CD2 (`create_topic().ok()` discards Err in `api/handlers/instance.rs:193-195` and `api/handlers/team.rs:146-147`) as **"low priority"**. From MCP D-angle, **bump to MEDIUM**. Reason: the silent-failure mode is the canonical "operator support burden" — operator calls `create_instance`, sees instance in registry, but Telegram topic missing → "why no topic?" is a recurring support question. The fix is 2 lines (replace `.ok()` with `match { Ok(t) => Some(t.id), Err(e) => { tracing::warn!(error=%e, "create_topic failed"); None } }`). Low effort, MEDIUM operator-ergonomics value. Track A's "low" framing under-weighs the support-cost dimension.

**Path-keyword reframe of A's C1**: Track A applies path-keyword auto-Critical to `is_user_allowed` based on the **`audit/authorize` keyword**. From MCP D's R2 enforcement angle, **a more direct trigger is the `check` keyword** ("is_user_*" is a check predicate). Either trigger lands the same Critical classification, so no operational disagreement — but for protocol consistency, future audits should surface ALL applicable keyword triggers, not just one. (My own R2 application in MCP C1 listed only one trigger; this is reciprocal "do as I should have, not just as I did".)

### Cross-area systemic patterns NOT in SYNTHESIS.md

(SYNTHESIS.md merged in PR #210; flagging patterns that may have surfaced individually but aren't tagged as cross-area systemic.)

**S1 — "absence-of-config defaults permissive" anti-pattern across A and D**

Three instances in this audit alone:
- (A) `is_user_allowed` returns true on `None` allowlist
- (D) `decisions::update` has no caller gate — `None` author check defaults to allow
- (A→D coupling) `create_topic().ok()` — `None` (Err discarded) defaults to "succeed silently"

**Cross-area principle violation**: at every "absence" point, the codebase chose the permissive default. Sprint 21 cross-track contract: **"absence of explicit config → fail closed"**. A's F1 + D's task #1 + CD2 fix — when bundled — establish this as a Track A + D + (B for daemon-side) joint contract.

**S2 — Stale enum-variant string in MCP D mirrors A's discriminator leak**

See MX2. Reciprocal canonicalisation should land in one PR touching both A and D.

**S3 — Severity contract: MCP tags, Channel erases**

See MX1. A+D joint finding; fix lives in A; test lives in cross-area integration.

**S4 — Audit-log pattern uneven across A and D**

A has `fleet_events.jsonl` (post PR #199). D has no decision-audit-log. Tasks have no audit log either (Track D didn't surface this either; came from cross-validation with A). Sprint 21 should adopt the A pattern in D for decisions and tasks. Joint A+D finding.

### Track D Sprint 21 backlog adjustments (from this peer-pass)

Existing Sprint 21 task #1 (Critical: `can_mutate_decision`) **expanded** to:
- 1a: implement `can_mutate_decision(home, caller, decision)` gate
- 1b (NEW): add `<home>/decision_audit.jsonl` write at every `decisions::update` invocation, mirroring `fleet_events.jsonl` pattern from A — captures `ts / caller / decision_id / changed_fields`
- 1c (NEW): cross-track sequencing constraint — must ship within same Sprint as A's F1 (`is_user_allowed` fail-closed) since the attack chain is end-to-end; document residual exposure if shipped in different sprints

NEW Sprint 21 tasks from this peer-pass:
- **#8 [Medium] CD2 priority rebid + fix** — make `create_topic` Err visible (warn log + maybe surface to client). Owner: D.
- **#9 [Medium] Severity contract round-trip test** (MX1) — A renders severity (emoji prefix); D adds integration test asserting MCP-tagged severity reaches Channel render correctly. Joint A+D.
- **#10 [Low] `ChannelKind` ↔ `&str` canonical mapping** (MX2 / S2) — add `AsRef<str>` impl, replace `inbox.rs:139-140` AND `mcp/handlers.rs:603-605` matches with the helper. Joint A+D, single PR.
- **#11 [Critical bundle, sequencing-constrained]** — Track A F1 + Track D #1 (1a+1b) shipped together as bundle, OR explicit operator-acked exposure window decision posted as a fleet decision. **No silent split-sprint.**

### Methodology

(Per challenge round R1 — peer-pass methodology transparency)

- **audited_head**: `0b9b473` (origin/main at peer-pass start, post-PR #210 SYNTHESIS merge)
- **commands_run**:
  - `git fetch origin main`
  - `git worktree add -b sprint20.5-track6-d-reads-a ../agend-terminal.worktrees/sprint20.5-track6-d-reads-a origin/main`
  - Full read of `docs/codebase-review-2026-04-27/CHANNEL.md` (277 lines including A's own peer-pass to DAEMON.md)
  - Targeted re-read of own `MCP.md` (specifically C1 / H1 / Cross-area dependencies / Sprint 21 sections)
  - `grep -n 'try_telegram_\|inject_provenance' src/mcp/handlers.rs` (lines 74/88/102/399 cited in dispatch — verified A↔D bridge surface)
  - `grep -n '"telegram"\|"discord"\|"slack"' src/mcp/handlers.rs` → found MX2 stale "slack" string
  - `grep -n 'NotifySeverity::' src/daemon/ src/mcp/ -r` → confirmed D-side severity tagging (MX1)
- **files_scanned** in this peer-pass:
  - `docs/codebase-review-2026-04-27/CHANNEL.md` — full read (277 lines)
  - `docs/codebase-review-2026-04-27/MCP.md` — re-read of own C1/H1/Sprint 21 sections (~80 of 199 lines)
  - `src/mcp/handlers.rs` lines 74/88/102/399 (A↔D bridge), 603-605 (MX2 stale string)
  - `src/channel/mod.rs:118-121` (ChannelKind enum confirmed only Telegram+Discord)
- Time spent: ~45 min within 2h hard cap

---

*End of Sprint 20.5 Track 6 peer-pass (D reads A). Cross-validation complete.*
