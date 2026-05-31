# Analysis Spike — CI fail notify quality (log-tail inject + fingerprint dedup)

**Author:** fixup-dev-2 · **Status:** spike (read-only) · **Base:** main @ fresh origin/main
**Task:** t-20260529025709429604-1 (Wave-3)

## TL;DR

Today CI-fail notices carry only the failed **job/step names** plus a checklist
telling the agent to run `gh run view <id> --log-failed` itself (extra round-trip),
and dedup is **conclusion-level** (won't re-notify a still-"failure" run even if a
*different* check now fails). Fix: (a) the daemon fetches the failed-log tail
(~120 lines, byte-capped) **async on the existing ci runtime** and embeds it in
the body — keeping the `gh run view` line only as a full-log footer; (b) add a
**failed-check-set fingerprint** so re-notify fires iff the failing set *changes*.
No lock/self-IPC surface (ci poll holds no registry/core lock) — the only §3.15
rule is *stay async, never `block_on` the shared ci runtime* (#1476).

## ① Existing path (located)

Two emit paths, both embedding the same checklist:
- **Regular conclusion path:** `ci_check_repo` (async) → `build_inbox_body`
  (poller.rs:604, used at :1201). `failure_detail` is the first failed job/step
  **name** from `fetch_failure_summary` (provider.rs:495 — GH API
  `actions/runs/{id}/jobs`, returns `"job / step"`, no log content). Checklist
  step 1 (poller.rs:619): `` `gh run view {id} --log-failed` — read the actual error``.
- **Early-fail path (#1326):** poller.rs:755-800 — `Detail:` is the failed job
  names joined; same checklist step 1 (poller.rs:778).

**Dedup today:**
- `last_notified_conclusion` (poller.rs:490) — re-notify only when the aggregate
  conclusion (pass/fail/ended) changes.
- `early_fail_notified_sha` — early-fail guarded per SHA.
- **No failed-check-SET fingerprint** → if the conclusion stays "failure" but a
  *different* check fails on a re-run, no re-notify (stale detail), or conversely
  flapping can't be distinguished.

## ② Design

**(a) Daemon-side log tail.** Add a provider method:
```rust
async fn fetch_failure_log_tail(&self, repo: &str, run_id: u64, max_lines: usize) -> Option<String>;
```
Two viable fetches (both async-safe on the ci runtime):
- **GH API logs** (`GET actions/runs/{id}/logs` → redirect to a zip) — consistent
  with `fetch_failure_summary`'s reqwest usage, but the zip parse is fiddly.
- **`gh run view <id> --log-failed`** via `tokio::process::Command` — gh already
  flattens the failed-job logs to text; simplest to tail. Recommend this (the
  notice already references the same command, and gh auth is already configured).

Pipe the tail into `build_inbox_body` (new `log_tail: Option<&str>` param) and the
early-fail body — **replace checklist step 1's instruction with the actual tail**,
and demote `gh run view … --log-failed` to a footer ("full log") so the escape
hatch stays. Mock provider keeps returning `None` → falls back to today's body.

**(b) Failed-set fingerprint dedup.** Pure fn:
```rust
fn failure_fingerprint(failed_checks: &[String]) -> String // stable hash of the SORTED set
```
Persist `failed_set_fingerprint: Option<String>` on the watch state (next to
`last_notified_conclusion`). On a failure emit: **skip if fingerprint == stored;
emit if it changed.** This is strictly finer than conclusion-level dedup —
re-notifies when the *set of failing checks* changes even while the conclusion
stays "failure", and suppresses identical re-polls. (Complements, doesn't replace,
the SHA/conclusion guards.)

## ③ §3.15 / lock

- ci_watch polls run **async on `shared_ci_runtime()`** (multi-thread, poller.rs:80),
  spawned at :354; `ci_check_repo` is `async fn` (:848). The registry lock is
  taken only transiently for a membership check (:1224, scoped `{}`) — **no
  registry/core lock is held across the fetch or the inject.**
- Inject is `enqueue_with_idle_hint` (file enqueue + PTY hint) — file I/O, **no
  self-IPC, no #1492**. The new log fetch adds no lock.
- ⚠️ **The one rule:** the fetch MUST stay async (`.await` / `tokio::process` /
  `spawn_blocking`) — **never `block_on` on `shared_ci_runtime()`** from within the
  runtime (the #1476 hard rule: it's a shared runtime, would panic "cannot start a
  runtime from within a runtime"). `tokio::process::Command::…output().await` is the
  safe shape.

## ④ Tail cap (anti-flood)

Two-axis cap: **≤120 lines AND ≤~8 KiB** (defends against one pathological long
line). Take the LAST `max_lines` lines, then byte-truncate from the front if still
over budget, and append a footer:
`… (truncated; `gh run view <id> --log-failed` for full log)`. This bounds the
injected context while preserving the full-log path.

## ⑤ RED

Pure seams (no network, no lock):
1. `failure_fingerprint(&["Check (ubuntu)","Coverage"]) == failure_fingerprint(&["Coverage","Check (ubuntu)"])` (order-independent) AND `!= failure_fingerprint(&["Check (ubuntu)"])` (set change).
2. `should_notify(prev_fp, new_fp)` → false on equal, true on change.
3. `format_log_tail(raw, 120)` caps to ≤120 lines + byte cap + footer.
4. `build_inbox_body(..., Some(tail))` body contains the tail; `..., None` falls back to today's checklist (regression lock).

Integration (mock provider): a failure run whose `fetch_failure_log_tail` returns
a tail → emitted body contains the tail, not just the bare instruction; a second
poll with the SAME failed set → no re-notify; a CHANGED set → re-notify.

## KISS

Surface: +1 provider method, the 2 body builders gain a `log_tail` param, +1 watch-
state field, +1 dedup check, +2 pure helpers. The fingerprint is the higher-value
half (precise re-notify) and is trivially unit-testable. Keep `gh run view` as a
footer rather than deleting it — it's the full-log escape hatch and costs nothing.
Recommend the `gh run view --log-failed` (tokio::process) fetch over the GH API zip
for KISS.
