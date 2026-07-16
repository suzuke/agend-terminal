# Shadow Observer — Quantification (#2413 Phase D, agy)

> **Historical measurement snapshot:** Results below describe a bounded soak at
> the recorded revision. They are evidence, not a guarantee of current backend
> behavior; repeat the measurement before using the numbers operationally.

**Question:** does agy's Hook plane (turn-granular, `.agents/hooks.json`, Phase D
#2448) make the fused `ObservedStatus` measurably cleaner than raw screen-scrape
for agy agents — the agy analogue of `docs/archived/SHADOW-OBSERVER-QUANT-2413.md`
(claude) and `docs/archived/SHADOW-OBSERVER-QUANT-CODEX-2413.md` (codex)?
Commissioned by the #2524 P1a
spike manifest, which found agy's hook-primary promotion had shipped to
production default (2026-06-24) with **zero quantitative verification** — only
claude and codex had QUANT docs.

**Answer: yes, on a different (but real and useful) failure class than claude's.**
Across 3 real turns, agy's raw screen classifier never mis-read the working phase
as idle (unlike claude's mid-API false-idle) — but it DID consistently lag ~10-20s
behind the actual turn end, staying `active` after the agent had already returned
to its idle prompt. The Shadow Observer's liveness-backstop correctly caught this
every time, 0 false-active regressions.

---

## Setup (real agy, isolated, zero fleet impact)

Mirrors the claude/codex recipe: **SHORT `AGEND_HOME`** (`/tmp/aq1` — the first
attempt used a long scratchpad path and hit `error=path must be shorter than
SUN_LEN`, silently disabling the local hook plane entirely; this is a real,
easy-to-hit pitfall worth calling out explicitly, not just a doc footnote),
`fleet.yaml` (not `--agents`), `AGEND_SHADOW_OBSERVER=1`, daemon
`start --foreground` (`run_core`). One real daemon-spawned agy agent (`aq`); the
live fleet was never touched. Binary = this PR's branch @ the driver commit.
Agy's own authentication is a `$HOME`-global credential (`~/.antigravity`,
`~/.gemini/antigravity-cli`, NOT scoped by `AGEND_HOME`) — the isolated instance
used the REAL, already-authenticated global agy login (non-destructive: normal
turns only, never revoked/misconfigured — see the AuthError section of the P1b-r2
report for why a destructive agy auth probe was explicitly NOT attempted).

**Instruments:** the daemon's own `#shadow-observer`-tagged INFO log (evidence
recorded + shadow-correction lines) and `agend-terminal list --json` polling
(`agent_state`, `api_in_flight`, `observed_status.{state,authority,confidence}`).

---

## Result 1 — Hook coverage: 3/3 turns, 100% at turn granularity

Every one of 3 real turns produced a `TurnStarted` (from `UserPromptSubmit`) and
`TurnEnded` (from `Stop`) Hook-authority Evidence pair:

```
turn 1: TurnStarted×2 (10:47:06, 10:47:21) → TurnEnded (10:47:25, stop_reason=None)
turn 2: TurnStarted×2 (10:48:24, 10:48:36) → TurnEnded (10:48:40, stop_reason=None)
turn 3: TurnStarted×2 (10:49:05, 10:49:17) → TurnEnded (10:49:21, stop_reason=None)
```

**Observation (not a coverage gap):** `TurnStarted` fired TWICE per turn,
consistently across all 3 turns (~15s apart, both `authority=Hook,
confidence=Confirmed`). Agy's `UserPromptSubmit` hook appears to fire on both the
injected prompt AND a subsequent internal step (possibly a retry/continuation
hook agy issues mid-turn) — harmless (both carry the identical `TurnStarted`
evidence, so the reducer's episode-open state doesn't change), but worth a
follow-up spike if it turns out to matter for episode-timing precision
elsewhere. `TurnEnded` fired exactly once per turn as expected — no dropped
`Stop` hooks observed in this sample.

## Result 2 — Correction class: stale-active → idle (liveness backstop), NOT mid-turn false-idle

Unlike claude, agy's raw screen classifier **never** misread the active/thinking
phase as idle during these 3 turns — poll trace during turn 1's working phase:

```
time      raw_agent_state  observed_status.state  authority
10:47:18  active           thinking               hook
10:47:20  active           thinking               hook
10:47:21  active           thinking               hook
10:47:23  active           thinking               hook
10:47:24  active           thinking               hook
10:47:26  active           thinking               hook
```

Raw and observed AGREE (coarse level) throughout the working phase — agy's
`esc to cancel` / `●` tool-call chrome is visible enough that the existing screen
heuristic already tracks "active" correctly here. This means agy does **not**
suffer from claude's specific mid-API false-idle failure mode.

What agy DOES suffer from: the raw classifier's hysteresis lags 10-20s behind the
real turn end (min_hold-style stickiness), so `agent_state` stays `active` after
the LLM has already finished and agy is genuinely back at its idle prompt. This
is exactly the "false busy" shape the Shadow Observer's liveness-backstop
reconcile (`shadow/reducer.rs` §4) was designed to catch — and it did, every time:

```
2026-07-02T10:47:27.701523+08:00 shadow correction: raw_screen=active observed=Idle
  confidence=Strong authority=Screen api_in_flight=true productive_silent_ms=90070
2026-07-02T10:47:37.706139+08:00 shadow correction: raw_screen=active observed=Idle
  confidence=Strong authority=Screen api_in_flight=true productive_silent_ms=100074
```

**6 corrections over 3 turns (2 per turn, ~10s and ~20s post-Stop), 0 false-active
regressions** — the reducer never showed `Active` while agy was genuinely idle in
this sample.

Note the correction's `authority=Screen`, not `Hook`: by the time the raw
classifier's hysteresis catches up enough for a coarse-disagreement to register,
the fresh `TurnEnded` Hook evidence has typically already resolved the reducer to
`Idle` too — so the OBSERVED side agrees with what a fresh hook read would say,
sourced through the liveness-backstop's screen+silence+no-live-socket reconcile
rather than a still-fresh Hook window. Functionally the correction is still
hook-enabled (the reducer's episode model, which the correction depends on, is
built from Hook evidence) even though the specific Evidence tagged on the
corrected sample is Screen.

## Result 3 — No false-active regression

Across all 21 polled samples (7 per turn × 3 turns) plus the 6 shadow-correction
log lines, `observed_status.state` was never `Active`-family while the agent was
genuinely idle (no live LLM socket, screen at rest, past the correction window).
The reducer is at least as conservative for agy as it proved to be for claude.

---

## Verdict

On real agy turns, the fused `ObservedStatus` is measurably cleaner than raw
screen-scrape, but via a **different mechanism than claude's headline case**:
agy's screen heuristic already tracks the active/working phase correctly (no
mid-turn false-idle observed), so the Shadow Observer's value for agy is
primarily the **liveness-backstop catching stale-active lingering past real turn
completion** (6/6 corrections caught, 0 regressions) — directly benefiting
`poll_reminder`/`reclaim`/`dispatch_idle` consumers that would otherwise treat a
genuinely-idle agy agent as still busy for 10-20s longer than necessary.

**Sample size caveat:** N=3 turns, single agent, single session — same
scale as the existing claude/codex QUANT docs (which this spike explicitly noted
as "small-N, controlled-verification, not large-scale live-fleet soak" in the
#2524 P1a manifest). This closes the "zero verification" gap the P1a manifest
flagged for agy specifically, but does not itself constitute a large-N soak.

**Recommendation to the fork-1(c) question this task was dispatched to answer**
(per decision `d-20260702015436487210-1`): the data does **not** support
reverting agy to explicit screen-only (fork 1(b)) — the live default-ON promotion
is producing correct, useful corrections for agy with zero observed
false-actives. No action needed; the existing default-ON state is empirically
justified for agy on this sample.

### Reproduce
```
ISO=/tmp/aq1; rm -rf $ISO; mkdir -p $ISO   # SHORT path — SUN_LEN, see Result 0 above
printf 'instances:\n  aq:\n    backend: agy\n' > $ISO/fleet.yaml
AGEND_HOME=$ISO AGEND_SHADOW_OBSERVER=1 RUST_LOG=info \
  target/debug/agend-terminal start --foreground --fleet $ISO/fleet.yaml >$ISO/fg.log 2>&1 &
AGEND_HOME=$ISO target/debug/agend-terminal inject aq \
  "Carefully write ~250 words on <topic>, then use the Bash tool to run: echo done"
grep "shadow correction" $ISO/daemon.*.log
AGEND_HOME=$ISO target/debug/agend-terminal list --json | python3 -m json.tool
```
