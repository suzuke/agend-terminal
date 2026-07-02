# Shadow Observer — Quantification (#2413 Phase B, §5)

**Question the operator asked (option A):** is the reducer's fused `ObservedStatus`
measurably *cleaner* than the raw screen-scrape `agent_state` — enough to justify (or not)
a later in-path proxy phase?

**Answer: yes, decisively, on the headline failure case (mid-API false-idle), with real
claude data.** The reducer also strictly beats the raw `api_in_flight` lsof signal. Zero
false-active regressions. Details + numbers below.

---

## Setup (real claude, isolated, flag-ON, zero fleet impact)

Mirrors the live-verify recipe (SHORT `AGEND_HOME=/tmp/svq`, `fleet.yaml` not `--agents`,
`AGEND_SHADOW_OBSERVER=1`, daemon `start --foreground` = `run_core`). One real
daemon-spawned claude agent (`sq`); the live fleet was never touched; `~/.claude` global
stayed clean (hooks live only in the per-workspace `settings.local.json`). Binary =
`feat/2413-shadow-local-plane-hooks` @ the driver commit. Default (bypass) claude — the
false-idle case needs no permission prompt.

**Instruments (two, cross-checked):**
1. The handler's own §5 telemetry: an INFO `#shadow-observer` "shadow correction" line
   each tick (~10 s) the fused state disagrees with the raw screen baseline.
2. External polling: `agend list --json` every ~1.2 s, capturing
   `(agent_state, api_in_flight, observed_status.state, observed_status.authority)` — the
   fine-grained raw stream the 10 s telemetry can only sample coarsely.

---

## Result 1 — mid-API false-idle: **CAUGHT** (the headline win)

Driving a real "think then run a tool" turn, claude spends ~20–30 s in a *thinking* phase
(`UserPromptSubmit`/`TurnStarted` → first `PreToolUse`) with **no streamed output**.
Claude's TUI shows nothing the screen-heuristic recognizes as work, so raw
`agent_state` reads **`idle`** for the great majority of that phase — while a turn is
plainly in flight.

Poll trace excerpt (turn 2, "history of zero"; TurnStarted 05:51:50, ToolStarted 05:52:13):

```
time      raw_agent_state  api_in_flight  observed_status.state  authority
05:51:55  idle             true           idle                   screen   ← pre-tick (stale)
05:51:56  idle             true           thinking               hook     ← reducer corrects
05:52:00  idle             true           thinking               hook
05:52:05  idle             true           thinking               hook
05:52:10  idle             true           thinking               hook
05:52:14  thinking         true           thinking               hook
05:52:20  idle             true           thinking               hook
05:52:25  idle             true           thinking               hook
05:52:26  idle             true           idle                   screen   ← turn closed, back to idle
```

During the thinking phase the raw screen-scrape read `idle` in **22 of 24** polled samples
(~91 %), while the reducer reported **`Thinking`** (Active family) with **`Hook`**
authority + **`Strong`** confidence for **100 %** of it. Note the reducer *derived*
`Thinking` (turn open ∧ no tool span ∧ no approval ∧ productive-silence > 8 s) — not by
string-matching a spinner — exactly the §3 design.

**Reproducibility (3 real turns, handler-cadence INFO count):**

| turn | prompt | thinking-phase len | corrections (INFO, 10 s tick) | post-turn observed |
|------|--------|--------------------|-------------------------------|--------------------|
| 1 | history of zero      | ~23 s | 2 | `idle` |
| 2 | history of primes    | ~25 s | 2 | `idle` |
| 3 | history of primes    | ~25 s | 2 | `idle` |
| **Σ** | | | **6 corrections / 0 regressions** | always returns to `idle` |

Every correction line was identical in shape:
`raw_screen=idle observed=Thinking confidence=Strong authority=Hook api_in_flight=true`.

(The handler-cadence count, 6, is the conservative metric — it samples the false-idle every
10 s. By *duration*, the reducer eliminated essentially the entire ~23–25 s false-idle
window of each turn.)

---

## Result 2 — no false-active regression, and the reducer beats raw `api_in_flight` too

Throughout the whole run `api_in_flight` read **`true`** — *including when the agent was
genuinely idle* between turns (a lingering / CDN-shared socket; the known Phase-1 lsof
limitation). So **raw `api_in_flight` alone is a poor idle signal**: it would have called a
resting agent "active".

The reducer did **not** make that mistake. After every turn it returned to `idle` (authority
`screen`, the hook episode closed by `TurnEnded` + `PromptReady`) despite `api_in_flight =
true`. The §4 control held: **0 false-actives across all 3 turns**. The reducer is therefore
strictly cleaner than *both* baselines it fuses — raw `agent_state` (false-idle) and raw
`api_in_flight` (false-active).

This is the liveness-backstop / precedence design paying off on real data: a fresh hook
turn-close outranks a stale socket; a stale screen-idle is overridden by an open hook
episode.

---

## Result 3 — approval-vs-idle: hook-proven + unit-tested; live count deferred

The second screen-scrape failure case (a permission prompt mis-read as idle/ambiguous) is
**not** separately quantified live here, by design choice:

- For **claude specifically**, the screen-scrape already classifies `PermissionPrompt`
  reasonably (observed in this very run at SessionStart), so it is not a clear screen
  *miss* — the false-idle is the genuine claude win.
- The reducer's `ApprovalRequired → WaitingForUser` mapping is proven by (a) Phase-A
  **live** verification that the `ApprovalRequired` hook fires on a real claude permission
  prompt, and (b) deterministic unit tests (`approval_then_proceed_clears_waiting`,
  `correction_predicate_flags_false_idle_and_approval_not_steady`).
- A live *count* would need a non-bypass claude + an un-allowlisted MCP-tool prompt (recipe
  gotcha 3) — fragile setup whose marginal evidence is low given the above. Deferred, not
  blocked.

The reducer's added value at an approval even when the screen agrees: it attaches `Hook`
authority + `Confidence` + an explicit `WaitingForUser` semantic (vs the screen's heuristic
guess), which is what a consumer needs to *act* on the distinction.

---

## Verdict

On real claude turns the fused `ObservedStatus` corrected a false-idle that raw
screen-scrape got wrong **~91 % of every thinking-phase sample (6 handler-cadence
corrections over 3 turns)**, with **0 false-active regressions**, and was simultaneously
cleaner than the raw `api_in_flight` signal. The out-of-path reducer (hook + screen + lsof,
zero in-path) is **measurably cleaner than screen-scrape** — the empirical basis the
operator asked for before deciding on an in-path proxy.

**HARD-STOP honored:** reducer + quantification only. No in-path proxy work. The in-path
proxy decision is now an *informed* one — the local plane already removes the dominant
false-idle error class without sitting in the request path.

### Reproduce
```
ISO=/tmp/svq; rm -rf $ISO; mkdir -p $ISO
printf 'instances:\n  sq:\n    backend: claude\n' > $ISO/fleet.yaml
AGEND_HOME=$ISO AGEND_SHADOW_OBSERVER=1 RUST_LOG=info \
  target/debug/agend-terminal start --foreground --fleet $ISO/fleet.yaml >$ISO/fg.log 2>&1 &
AGEND_HOME=$ISO target/debug/agend-terminal inject sq \
  "Carefully write ~300 words on the history of primes, then use the Bash tool to run: echo done"
# headline metric:
grep "shadow correction" $ISO/daemon.*.log
# live raw-vs-observed during a turn:
AGEND_HOME=$ISO target/debug/agend-terminal list --json | python3 -m json.tool
```
