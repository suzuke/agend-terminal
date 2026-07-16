[繁體中文](SOLO-PROFILE.zh-TW.md)

# Solo Profile — Running AgEnD as Operator + One Agent

**Status:** informative, non-normative. Where this document and
`FLEET-DEV-PROTOCOL.md` disagree, the protocol always wins. This document only
helps apply §3.21 proportional-ceremony judgment when there is no peer to
coordinate with; it does not waive task tracking, CI, worktree, review, or
merge-authority gates.

## Why this exists

The quickstart path — one `general` instance talking to the operator, no
team, no fleet peers — is AgEnD's official entry point. But
`FLEET-DEV-PROTOCOL.md` is written for the *fleet* case: dispatch
contracts, dual review, decision boards other agents read, timeout
staircases for a peer who's gone quiet. A solo agent following every rule
literally ends up doing ceremony whose entire purpose is "keep another
agent from being surprised" — when there is no other agent.

#2524's workflow-gap audit (three perspectives: multi-agent / solo /
non-Claude-backend) named this directly: *"單人：quickstart 單 instance 是官方入
門路徑，但 protocol ceremony 全是 fleet 導向，輕量化只靠 §3.21 lead judgment"* — solo
right-sizing had no written guidance, only case-by-case judgment. This
document is that guidance, not a new gate.

## You're solo if

No team (`team` is absent from your identity block) and no other fleet
peers are listed. If either is present, you're in a fleet context —
follow `FLEET-DEV-PROTOCOL.md` as written.

## What still applies solo

These protect *you*, the operator's repo, or the merge pipeline — not a
peer agent. Nothing about being alone changes their purpose:

- **Task board (§1).** A solo agent handling a direct operator request creates
  and claims its own task, then records the evidence-backed result. The board is
  still the durable source of truth across restarts and handoff.
- **Worktree discipline (§10/§12.4).** Still use a daemon-managed worktree +
  branch, never commit to main and never create the worktree with raw git. This
  isolates your changes from the operator's canonical working tree, which
  exists whether or not you have teammates.
- **Test-first (§3.10).** Still write the failing test before the fix.
  This catches *your own* regressions — the value isn't "so a reviewer can
  verify," it's "so you don't ship a fix that doesn't fix anything."
- **CI fail-closed merge.** CI doesn't know or care how many agents are
  on the repo. Green is green.
- **Evidence-gated claims (§3.3's "comments are claims, not evidence").**
  Still true when you're the only one who'll ever read your own claim back.

## What's lighter solo

- **Review coordination (§3.2–3.5).** Do not manufacture a fake reviewer. The
  review tier is still selected under §3.21, and merge authority still follows
  §3.5: an implementer self-merge requires two independent VERIFIED verdicts;
  an operator may review/merge directly only through an applicable protocol
  path such as §3.6. If the required reviewer does not exist, hand the merge
  decision back to the operator or add a real reviewer instance.
- **Decision-board discussion.** There may be no peer discussion thread, but a
  scope decision, correction, or unresolved operator fork still belongs in
  `decision`. Use the timeout/default path below only when a default is safe and
  explicitly declared.
- **`send`/`inbox`/team communication tools.** There may be no peer recipient,
  so peer dispatch is unnecessary. Operator communication still goes through
  the channel-appropriate `reply` path — that's unrelated to fleet size.
- **Timeout staircase for a "stale peer" (§9).** Nothing to escalate
  against when there's no peer.

## The one hard blocker this closed

Before #2531, a `decision(needs_answer: true)` with the operator offline
had no resolution path — it waited indefinitely. That's a real solo/
overnight failure mode: no peer to answer in the operator's place, and no
default to fall back to.

**Fixed**: `decision(action: post, needs_answer: true, timeout_secs: N,
timeout_default: "...")` — after `timeout_secs` unanswered, the daemon
auto-resolves to `timeout_default` and notifies the decision's `author`.
Solo and overnight decisions now have a real exit path instead of an
indefinite wait. This was #2524's *only* hard blocker across the
multi-agent / solo / non-Claude-backend audit — everything else in this
document is right-sizing, not a missing mechanism.

## When to escalate from solo to fleet

Use §3.21 axis A verbatim — it's already general-purpose, not
fleet-specific:

> FLEET iff *"wrong = expensive"* AND *"only an adversary-who-tries-to-
> break-it catches the flaw — a test you could write would not"*. Else
> SINGLE.

Lead's 5-second question, restated for a solo agent deciding for itself:
*"If I'm subtly wrong here, how bad is it, and would my own test catch
it, or only someone actively trying to break it?"* If the honest answer
is "only an adversary," don't stay solo out of momentum — pull in a
second perspective (a reviewer instance, or the operator) before shipping.

## See also

- `FLEET-DEV-PROTOCOL.md` §3.6 (LOW docs-only exception) is an existing
  example of the protocol already carving out a lighter path when the
  blast radius is small — the same instinct this document generalizes.
- `FLEET-DEV-PROTOCOL.md` §3.21 (Proportional Ceremony) — the three
  independent axes (fleet-vs-single, spike-vs-skip, review tier) this
  document's escalation criterion is drawn from.
