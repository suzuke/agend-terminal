# #1509 Analysis Spike — `now=` injection coverage audit (#1487 extension)

**Author:** fixup-dev-2 · **Status:** spike (read-only) · **Base:** main @ (fresh origin/main)

## TL;DR

`now=` (operator-TZ timestamp, #1487) is produced by a single value-source —
`operator_now_field()` (notify.rs:106). **Three of the four header formatters
call it; one does not** — `format_notification_for_inject` (notify.rs:217), the
formatter behind `notify_agent`. **Every** `notify_agent` path therefore ships
without `now=`: telegram-inbound, daemon conflict notices, supervisor
state-change notices, and boot-hygiene notices (7 call sites, 4 modules).

**Root fix (KISS + central): add `operator_now_field()` to
`format_notification_for_inject`.** One formatter → fixes all 7 sites at once.
The `now=` *value* is already centralized; the gap is one formatter not calling
the shared source. Plus a RED grep-invariant so a future formatter can't drift.

## ① + ② Coverage map (enumerated + empirically traced on origin/main)

| Delivery path | Formatter | `now=`? |
|---|---|---|
| agent→agent `send` (all request_kind) | `enqueue_with_idle_hint` → `build_pending_pointer` (notify.rs:475) | ✅ |
| system notices via idle-hint: **ci-ready / watchdog / dispatch_idle** | `enqueue_with_idle_hint` (475) | ✅ |
| full message header / event header | `format_header` (92) / `format_event_header` (140) | ✅ |
| **telegram user→agent** (inbound.rs:207/466/513) | `notify_agent` → `format_notification_for_inject` (217) | ❌ |
| **daemon conflict_notify** (conflict_notify.rs:177/198) | `notify_agent` → `format_notification_for_inject` | ❌ |
| **supervisor state-change** (RateLimit/AuthError → orchestrator, supervisor.rs:404) | `notify_agent` → `format_notification_for_inject` | ❌ |
| **boot canonical-hygiene** notice (canonical_hygiene.rs:271) | `notify_agent` → `format_notification_for_inject` | ❌ |
| discord user→agent | — **no inbound→agent inject path found** (discord is outbound/notification-only in current code) | N/A |
| schedule/cron-triggered dispatch | flows through `send`/`enqueue_with_idle_hint` | ✅ |

**Root cause:** `format_notification_for_inject` builds `[{source}] [AGEND-MSG]
size=N (use inbox tool)` (pointer) or `[{source}] {text}` (body) and never
appends `operator_now_field()` — unlike the other three formatters. So the
single shared value-source exists, but one of four call sites was missed in
#1487.

## ③ Root fix vs per-path — centralize wins, and it's also the KISS one

This is the rare case where central **==** minimal:
- **Per-path patch** would touch 7 call sites across 4 modules (telegram,
  conflict_notify, supervisor, canonical_hygiene) — fragile, easy to miss the
  next one.
- **Central fix** = add `operator_now_field()` to the ONE formatter those 7
  sites share (`format_notification_for_inject`). One line in one function fixes
  all of them and any future `notify_agent` caller automatically.

The `now=` value is already unified in `operator_now_field()`; only the *call*
is missing in formatter #4. We do NOT need a bigger "merge all 4 formatters into
one header-builder" refactor — that's higher blast radius for no extra coverage.

## ④ Gap-fill design + blast radius

In `format_notification_for_inject`:
- **pointer branch** (`[{source}] [AGEND-MSG] size=N (use inbox tool)`): append
  ` {now}` after the existing fields — same space-delimited shape as the other
  headers; agents already tokenize `now=` here.
- **body branch** (`[{source}] {display_text}`): this inline form has **no
  `[AGEND-MSG]` header** to host a field. Options for the dialectic: (a) leave
  body-mode unstamped (no header convention applies inline), or (b) carry `now=`
  on the `[{source}]` prefix. Recommend (a) for KISS unless body-mode is the
  production default — confirm: `pointer_only_inject` is a `DaemonConfig`/env
  flag (`AGEND_POINTER_ONLY_INJECT`), so the deployed mode decides whether the
  body branch matters at all.

**Blast radius:** one function; telegram-inbound + conflict + supervisor +
hygiene injects GAIN a `now=` field. Additive and harmless — agents already
parse `now=` from the other paths. No lock/IPC surface (`operator_now_field` is
a pure time-format, same as its existing 3 call sites). No §3.15 concern.

## ⑤ RED

1. **Unit (direct):** `format_notification_for_inject(pointer_only=true, …)`
   output contains `now=` (RED today). Mirror for the other 3 formatters as a
   regression lock.
2. **Integration (per-path):** the telegram-inbound inject contains `now=`
   (reproduces the empirically-reported gap; RED today).
3. **Anti-drift grep-invariant (the durable #1509 value):** scan `notify.rs` so
   every header builder that emits `[AGEND-MSG` / `[AGEND-MSG-PENDING` also
   references `operator_now_field()`. This is what stops a *future* formatter
   from silently dropping `now=` again (the #1502/#1476 invariant pattern).

## KISS assessment

The fix itself is one line (call the already-shared `operator_now_field()` in
the fourth formatter). The lasting value is the grep-invariant that converts
"remember to stamp now= in every new formatter" into a CI gate. Recommend: the
one-line fix + the invariant + a couple of unit/integration asserts. Decide the
body-branch question (a/b) based on the deployed `pointer_only_inject` mode.
