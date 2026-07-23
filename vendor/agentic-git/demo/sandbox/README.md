# agentic-git sandbox

A **persistent**, hands-on place to poke at agentic-git. It drops you into a
guarded session — a shell where `git` IS the shim — with a cheat-sheet, so you
type each command and watch the guardrail react. Unlike
[`../playground.sh`](../playground.sh) (a throwaway that self-deletes), the repos
here **survive between runs**: break them, re-enter, continue.

Everything lives in this folder. The only "remote" is a **local bare repo**, so
nothing ever touches the network.

## Use it

```sh
cd demo/sandbox      # (or copy this folder anywhere you like)
./enter.sh           # interactive: enter the guarded shell, follow the cheat-sheet
./enter.sh --scripted # run all 8 scenes and assert they pass (CI-safe)
./enter.sh --reset   # wipe and rebuild the sandbox repos
```

It uses your installed `agentic-git` (`cargo install agentic-git`) if present,
otherwise the release build from this repo. First run builds the repos and hands
you a guarded shell; `exit` leaves without deleting anything.

The generated repos (`project/ origin.git/ your-checkout/ home/`) are gitignored,
so running the sandbox in place never dirties the checkout.

## What you can verify

| # | you type (inside the guarded shell) | what happens |
|---|---|---|
| ① isolation | `git rev-parse --abbrev-ref HEAD` | `sandbox/work` — the agent's own branch/worktree |
| ② cross-branch | `git checkout main` | **denied** — can't switch off the assigned branch |
| ③ worktree | `git worktree add /tmp/x -b y` | **denied** — worktree lifecycle is session-managed |
| ④ provenance | commit, then `git log -1 --format=%B` | commit carries `Agentic-Agent: me` … trailers |
| ⑤ push guard | `git push --mirror origin` / push a `fleet.yaml` | **denied** — all-refs push / secret-leak blocked |
| ⑥ recovery | `git reset --hard`, then `$AGENTIC_GIT_BIN snapshots restore --repo "$PWD"` | erased work comes back, unstaged |
| ⑦ containment | `cd your-checkout/` and `git checkout HEAD~1` | **denied** — even in your real checkout the agent can't move HEAD |
| ⑧ audited bypass | `AGENTIC_GIT_BYPASS=1 git push --mirror origin` | **succeeds** — a deliberate override, logged to `home/fleet_events.jsonl` |

`agentic-git` is a **seatbelt, not a cage** — a same-uid userspace shim for
accident-prone agents, not a boundary against a determined adversary. For that
you want kernel isolation (containers, `sandbox-exec`, Landlock) *underneath* it.
