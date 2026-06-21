# SPIKE MANIFEST — #2379 ② unify remaining bare deny strings (task t-…79143-3)

_Author: fixup-dev. EPIC #2379 build-order ② (after ③ denylist-core #2390, merged). Base: origin/main (incl #2390). spike-first → this manifest → lead VET → impl (DUAL, git deny path) → review. Copy rule: NO "安全"/"security" words (operator). ⚠ NOT a behavior change — copy/consistency only; ③ fail-closed untouched._

## 1. Goal
Bring the remaining **bare** deny messages in `src/bin/agend-git.rs` up to the **#2234 context-aware remedy** level + insert **in-scope binding context** (pure `format!`, **zero I/O**). Exempt SilentExempt (silent paths get no prose).

## 2. Deny landscape (grep inventory — `src/bin/agend-git.rs`, non-test)

**(a) The `Action::Deny` path — BARE (the bulk; this is the target).**
`classify()` returns `Action::Deny(reason)`; `main()`'s arm (line 245) renders it via `emit_deny_error(subcmd, reason, agent)` → `format_deny_error` (line 2048). Current format = `ERROR git <subcmd> denied` + `agent=…, reason: …` + a generic HINT ("use the task board…") + the 3 `AGEND_GIT_BYPASS*` forms. **Missing**: the in-scope binding context (branch/worktree/task) AND the #2234-style "cd into the auto-bound worktree / `bind_self`" remedy. Reasons today:
- `"unbound — no active task assignment"` (×4: 944, 994, 1024, 1081)
- `"bound but no worktree path"` (×3: 948, 999, 1122)
- `"fleet-managed — use agend-terminal worktree tools"` (1126, `worktree`)
- `format!("cross-branch — assigned to '{assigned}', cannot switch to '{target_branch}'")` (1114)
- `"agent callers must not checkout in canonical (… repo action=checkout … gh pr diff/view) #852"` (1055)

**(b) #2390 push trust-root denylist — already routes through `format_deny_error`.**
`main():206` calls `emit_deny_error(subcmd, reason, agent)` for `push_trust_root_denylist_violation`. So it inherits whatever `format_deny_error` renders → **auto-enriched** by this work (no separate change). ③'s fail-closed DETECTION logic (`push_trust_root_denylist_violation`) is untouched — only its message rendering.

**(c) #2234 `enforce_agent_canonical_bypass_deny` (line 1427) — the RICH gold standard.**
Hand-rolled `eprintln!` (1444): "agent '{agent}' must not bypass-{sub} in a canonical-rooted repo. If the daemon auto-bound a worktree (check `binding_state`), cd into it and use normal git; otherwise `bind_self` or ask lead. … set AGEND_GIT_ALLOW_CANONICAL_MUTATE=1 for a one-shot." Already context-aware. Runs EARLY (before `classify`) off env/args/cwd — **no `Binding` is loaded here** (so it can't print specific branch/worktree without I/O).

**(d) EXEMPT (no prose).** `Action::SilentExempt` (main:224, gh post-merge) + `Action::CleanupAndChdirPushPass` (silent/pass). Per charter — skip.

**(e) OUT OF SCOPE (not policy denies).** `exec failed` (1939/1961/1969), panic handler EX_SOFTWARE (51), #883 pre-push cleanup warnings (1650-1752), drift warning (627), and forensic instrument logs `[agend-git #2234/#2158/#1463]` passthrough/bypass tracking (1495/1558/1610) — instrument-only, no remedy.

## 3. #2234 format + binding context (confirmed, zero-I/O)
- Shared formatter today: `format_deny_error(subcmd, reason, agent) -> Vec<String>` (2048), wrapped by `emit_deny_error` (2035).
- `Binding { task_id: Option<String>, branch: Option<String>, worktree: Option<String> }` (283). Loaded ONCE by `read_binding` (HMAC-verified, fail-closed) in `main()` **before** `classify` — so at the `Action::Deny` arm (main:245) and the push-denylist emit (main:206), `binding` + `agent` + `home` are all **in scope, already in memory** → folding them into the message is **pure `format!`, zero new I/O**. ✓ (`enforce_2234` (c) is the only deny without a loaded Binding.)

## 4. Design (recommended)
**Single shared builder, binding-aware where binding is in-scope.**
1. Extend `format_deny_error` to accept the in-scope binding context, e.g. `format_deny_error(subcmd, reason, agent, binding: &Binding) -> Vec<String>`, and render a context-aware remedy:
   - **bound** (`binding.worktree`/`branch` present): `your worktree is <worktree> (branch '<branch>', task <task_id>) — cd there and run git (no bypass needed)`.
   - **unbound**: `no active binding — get a worktree via the task board / `repo action=checkout bind=true` / `bind_self`, then run git there`.
   - keep the existing 3-form `AGEND_GIT_BYPASS*` hint block (mirrors #2234's "one-shot / agent / time-limited" affordances).
2. Thread `&binding` to the two `emit_deny_error` call sites (main:206 push-denylist, main:246 `Action::Deny`) — both already hold `binding`.
3. **#2234 consistency (VET decision)**: extract the remedy/bypass block as a small `format!` helper that BOTH `format_deny_error` and `enforce_agent_canonical_bypass_deny` reuse, so the two rich messages stay in sync. `enforce_2234` keeps its own AGEND_GIT_ALLOW_CANONICAL_MUTATE line + generic (no-specific-binding) remedy variant (it has no loaded Binding, and adding one would break the zero-I/O rule). OR leave (c) fully as-is (already rich) and only unify (a)/(b) — smaller blast.
4. **Copy rule**: no "安全"/"security" words (current copy already complies; the new remedy wording must too).

## 5. Blast radius
- `format_deny_error` signature +1 param → `emit_deny_error` (2035) + its 2 callers (main:206, 246). All in-scope.
- Tests asserting deny copy: `deny_hint_lists_all_three_bypass_forms` (2512) + any test matching the `ERROR git … denied` / reason lines. Update to the enriched format.
- No `classify` logic change → the deny DECISIONS (which ops deny) are byte-identical; only the rendered message changes. ③ `push_trust_root_denylist_violation` detection untouched.

## 6. Tests (gate)
1. `format_deny_error` with a **bound** Binding → message contains the branch + worktree + "cd"/"no bypass needed"; with an **unbound** Binding → contains the `bind_self`/`repo action=checkout`/task-board remedy. (No I/O — construct `Binding` in-test.)
2. `deny_hint_lists_all_three_bypass_forms` still passes (3 `AGEND_GIT_BYPASS*` forms retained).
3. NEW meta-test: no deny-copy line contains "安全" or "security" (guards the operator rule).
4. (if (c) unified) a shared-builder test that `enforce_2234` + `format_deny_error` emit the same remedy block.
5. Behavior unchanged: an existing classify/deny decision test still returns the same `Action::Deny` variant (the String reason may change; assert the Action/exit, not the prose, where the test is about behavior).
- Run: `env -u AGEND_INSTANCE_NAME AGEND_GIT_BYPASS=1 cargo nextest run --features tray` (+ full nextest before push for the agend-git invariant scanners; the deny-shim tests false-fail under the fleet `git` shim → confirm env via PATH=/usr/bin, per prior agend-git work).

## 7. Open questions for VET
1. **#2234 (c) scope**: extract a shared remedy builder reused by both (more consistent, bigger blast) vs leave (c) as-is and only enrich (a)/(b) (smaller)? (Recommend: shared builder — it's the charter's "同款 builder" intent.)
2. **Include specific binding fields** (branch/worktree/task_id) in the message, or just the generic remedy? (Recommend: include them where in-scope — that's the charter's "塞 in-scope binding context" value-add over the generic #2234 wording.)
3. Exact remedy wording (avoiding "安全"/"security") — review at impl, or pin in VET?
