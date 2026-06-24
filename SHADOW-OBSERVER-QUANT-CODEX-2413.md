# Shadow Observer — Quantification (#2413 Phase D, codex)

**Question:** does the codex **rollout-tail** observer source (`authority=Stream`) make the
fused `ObservedStatus` cleaner than raw screen-scrape for codex agents — the codex analogue
of the claude result in `SHADOW-OBSERVER-QUANT-2413.md`?

**Answer: yes.** On a real codex turn the reducer (fed by the rollout tail) reports the
agent as actively working with **`authority=Stream`** throughout, while the raw screen-scrape
`agent_state` false-reads **`idle`** mid-turn. The reducer corrects it. The Phase-D reducer
generalization ({Hook|Stream} freshness/authority gate) gives codex full parity with claude.

---

## Setup
Isolated real codex agent (`/tmp/svcxq`, flag-on `AGEND_SHADOW_OBSERVER=1`, live fleet
UNTOUCHED), one daemon-spawned codex-tui agent `cq`. The codex rollout tailer started
(`codex rollout tailer listening (stream plane) root=~/.codex/sessions`). Drove a multi-step
turn (echo / sleep / echo …) and polled `list --json` for `(agent_state, observed_status)`.

## Result — codex false-idle CAUGHT + Stream parity
Poll trace during the turn (HH:MM:SS):

```
time      raw_agent_state   observed_status   authority
11:38:41  thinking          responding        stream
...       thinking          responding        stream     (11:38:41–56, 11 samples)
11:38:58  idle              responding        stream   ← raw screen false-idle; reducer holds Responding
11:38:59  idle              responding        stream
11:39:01  idle              responding        stream
11:39:02  idle              responding        stream
11:39:04  idle              responding        stream
11:39:05  idle              idle              screen   ← turn ended; correctly back to idle (no fresh stream → screen)
```

Two wins, both on REAL codex data:
1. **`authority=Stream` for the whole active turn** — the reducer labels codex's status with
   the rollout-stream plane, NOT the `Screen` fallback. This is the Phase-D reducer
   generalization paying off (pre-source, the same daemon showed codex as
   `raw=thinking / observed=Idle / authority=Screen` — the reducer was BLIND to the codex
   turn; now it sees it via the rollout).
2. **Codex false-idle caught** — from 11:38:58–11:39:04 the raw screen-scrape `agent_state`
   read **`idle`** (≈5 of 5 polled samples in that window) while the turn was still in flight;
   the reducer correctly reported **`Responding`** (Stream authority). The handler-cadence
   correction log captured it too:
   `raw_screen=idle observed=Thinking confidence=Strong authority=Stream` (the 10 s tick is
   coarser than the 1.5 s poll, so it under-counts; the poll shows the full ≈6 s false-idle
   window the reducer covered).
3. **No false-active regression** — once the turn ended (`task_complete` → TurnEnded), the
   reducer returned to `idle` (authority `screen`, no fresh stream evidence), matching the
   screen. It did not wedge Active.

## Verdict
The codex rollout-tail observer + the {Hook|Stream} reducer generalization make codex's
`ObservedStatus` measurably cleaner than raw screen-scrape — same headline win as claude
(false-idle eliminated), now with `authority=Stream`. The out-of-path, read-only,
zero-injection rollout tail is the cleanest plane yet (codex writes the file itself).

Bonus over the claude hook plane: codex's `token_count` records carry `rate_limits`, so the
parser also emits `RateLimited` from a definitive signal claude hooks lack (mapped, unit-
tested; not exercised in this short turn).

### Reproduce
```
ISO=/tmp/svcxq; rm -rf $ISO; mkdir -p $ISO
printf 'instances:\n  cq:\n    backend: codex\n' > $ISO/fleet.yaml
AGEND_HOME=$ISO AGEND_SHADOW_OBSERVER=1 RUST_LOG=info \
  target/debug/agend-terminal start --foreground --fleet $ISO/fleet.yaml >$ISO/fg.log 2>&1 &
AGEND_HOME=$ISO target/debug/agend-terminal inject cq "Do as separate shell steps: echo A; sleep 4; echo B"
AGEND_HOME=$ISO target/debug/agend-terminal list --json | python3 -m json.tool   # observed_status.authority == "stream"
grep "shadow correction" $ISO/daemon.*.log                                        # raw=idle observed=... authority=Stream
# TEARDOWN: pkill -f "fleet /tmp/svcxq" (isolated daemon only — NEVER broad-pkill codex).
```
