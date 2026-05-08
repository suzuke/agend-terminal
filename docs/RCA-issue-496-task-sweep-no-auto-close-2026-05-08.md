# RCA — issue #496: task_sweep merged-PR auto-close silently no-op

**Date:** 2026-05-08
**Author:** dev (Sprint 56 Track C-RCA, Path B doc-only)
**Issue:** [#496](https://github.com/suzuke/agend-terminal/issues/496) — *task_sweep enabled but merged PR with `Closes` marker does not auto-close task*
**Reporter:** cheerc — repro on `cheerc/talented-payroll`, PR #8 merged 2026-05-07T08:03:37Z, task `t-20260507080318189041-0` remained `open`
**Verdict:** **Root cause identified — namespace mismatch in the authorship gate at `src/daemon/task_sweep.rs:200-216`.**
The fix is a small structural change (instance↔GitHub identity mapping) plus a doctor diagnostic, recommended Path A standard.

This document covers the four RCA dimensions requested by lead in the dispatch (m-20260508091807405547-59):

1. tick scheduling evidence
2. marker parser audit
3. cross-repo configuration audit
4. silent-drop class audit (PASS/SWALLOW points)

It closes by naming the root cause, ruling out the other three, and proposing the fix shape for Track F.

---

## 1 — Tick scheduling evidence

### Spawn site
`src/daemon/mod.rs:325-327`

```rust
let _task_sweep =
    crate::daemon::task_sweep::TaskSweep::spawn(home.to_path_buf(), Arc::clone(&shutdown));
```

Spawned unconditionally during daemon `prepare`; the holding handle is `_task_sweep` (not `let _ = …`, so the ticker thread is owned for the daemon's lifetime).

### Tick body construction
`src/daemon/task_sweep.rs:98-110`

```rust
pub fn spawn(home: PathBuf, shutdown: Arc<AtomicBool>) -> Self {
    let ticker = DaemonTicker::spawn(
        "task_sweep",
        Duration::from_secs(DEFAULT_SWEEP_TICK_SECS), // 300s
        shutdown,
        move || {
            if let Err(e) = sweep_tick(&home) {
                tracing::warn!(error = %e, "task_sweep tick failed");
            }
        },
    );
    Self { _ticker: ticker }
}
```

`DEFAULT_SWEEP_TICK_SECS = 300` (5 min). Per `DaemonTicker::spawn` docs (`src/daemon/ticker.rs:73`), the body is **invoked once immediately at thread start** and then on every `tick_dur` boundary. So a daemon that has been running ≥1 second has had at least one tick execution; one that has been running ≥5 min has had at least two.

### What the tick body does at the very top
`src/daemon/task_sweep.rs:115-128`

```rust
fn sweep_tick(home: &Path) -> anyhow::Result<()> {
    let cfg = load_config(home);
    if cfg.paused { return Ok(()); }
    let repo = match &cfg.repo {
        Some(r) if !r.is_empty() => r.clone(),
        _ => return Ok(()),
    };
    let prs = list_recently_merged_prs(&repo)?;
    if prs.is_empty() { return Ok(()); }
    …
```

The config is loaded fresh every tick (no in-memory cache), so an `mcp__task_sweep_config` mutation lands on the next tick boundary. cheerc's repro pre-set `repo=cheerc/talented-payroll`, `paused=false`, `dry_run=false` and waited >5 min, so the early `return Ok(())` short-circuits **cannot** explain the no-op.

### Conclusion (§1)
Tick scheduling is **correct and fired**. cheerc's >5 min wait window guarantees at least two complete tick passes, both of which would have reached `list_recently_merged_prs`. The bug is downstream of the GitHub fetch.

Operator-side independent verification (no code change required): `grep "task_sweep tick failed" ~/.agend/app.log` should be empty (a tick that fails would surface), and `grep "sweep:" ~/.agend/app.log` should show diagnostic lines from §4 below.

---

## 2 — Marker parser audit

### Regex
`src/daemon/task_sweep.rs:282-284`

```rust
regex::Regex::new(r"(?m)Closes\s+(t-[0-9]+-[0-9]+)\b")
```

- Multi-line (matches anywhere in body).
- Strict ASCII digits only (zero-width-char homoglyph attack defeated; covered by `non_ascii_task_id_rejected` test at line 466).
- Word-boundary terminator (`\b`) so `Closes t-1-1foo` doesn't false-match.

### Validation pre-step
`src/daemon/task_sweep.rs:177` — `crate::daemon::utils::strip_html_comments(&pr.body)` runs before the regex, so `<!-- Closes t-victim -->` is stripped (HTML-comment injection sanitiser; covered by `html_comment_injection_stripped` test at line 444).

### cheerc's marker
PR body contains `Closes t-20260507080318189041-0`. The regex breakdown:

- `Closes` literal — ✓
- `\s+` (one space) — ✓
- `t-[0-9]+-[0-9]+` — `t-20260507080318189041-0` — ✓
- `\b` boundary — ✓ (next char is end-of-line / EOF)

Manual trace of the production regex against the literal string yields exactly one capture: `t-20260507080318189041-0`.

### Conclusion (§2)
Marker parser is **correct**. cheerc's marker shape matches; existing test fixtures cover both injection vectors and ASCII-digit strictness; no false-positive surface around the marker.

---

## 3 — Cross-repo configuration audit

### What `cfg.repo` controls
`src/daemon/task_sweep.rs:308-321`

```rust
fn list_recently_merged_prs(repo: &str) -> anyhow::Result<Vec<PrMeta>> {
    …
    let url = format!(
        "https://api.github.com/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page={PR_LIST_LIMIT}"
    );
    …
}
```

`cfg.repo` is the **GitHub `owner/repo` slug to poll for merged PRs**. Single repo per daemon instance. cheerc set `cheerc/talented-payroll` correctly, and the GitHub API call would succeed (private-repo case requires `GITHUB_TOKEN`; auth path at line 325 is `if let Ok(token) = std::env::var("GITHUB_TOKEN")` — works for both public and private).

### What the local task board lookup uses
`src/daemon/task_sweep.rs:133`

```rust
let open_tasks = crate::tasks::list_all(home);
```

`tasks::list_all` reads `<home>/tasks.json` — the **local agend daemon's task board**, which is a single global list across every project the daemon serves. There is no per-repo scoping; a task is identified by its ID alone (`t-<unix-ms>-<seq>`).

### The two namespaces involved

| Surface | Namespace | Where it lives | Example |
|---|---|---|---|
| `cfg.repo` | GitHub `owner/repo` | daemon's `task_sweep.json` | `cheerc/talented-payroll` |
| Task board entries | Daemon-local task list | `<home>/tasks.json` (or replay-derived view) | `t-20260507080318189041-0` |

These are **independent**. The task board has no repo association at all, so a sweep targeting `cheerc/talented-payroll` will happily match `Closes t-20260507080318189041-0` against any task with that ID in the local board, regardless of which project the task was originally created for.

cheerc's repro: the task `t-20260507080318189041-0` was created locally on cheerc's daemon (the only `Created` event observed per the issue body), so the local lookup at `open_ids.get(&marker)` would succeed.

### Conclusion (§3)
Cross-repo configuration is **correct** for cheerc's case. The lookup at line 186 finds the open task. **The bug is not here either** — control flow proceeds past the marker match into the authorship gate.

---

## 4 — Silent-drop class audit

This is where the bug lives. The full validation pipeline after a marker match is:

```rust
// src/daemon/task_sweep.rs:185-225 (abridged)
for marker in markers {
    let task = match open_ids.get(&marker) {              // §3 — would succeed
        Some(t) => t,
        None => { tracing::debug!(...); continue; }
    };

    // ── must-have #3: PR.user.login authorship ONLY ──
    let creator = task.created_by.as_str();               // (A)
    let assignee = task.assignee.as_deref();              // (B)
    let author_ok = pr.author_login.eq_ignore_ascii_case(creator)
        || assignee
            .map(|a| pr.author_login.eq_ignore_ascii_case(a))
            .unwrap_or(false);
    if !author_ok {
        tracing::warn!(                                   // (C) silent at default level
            pr = pr.number,
            marker = %marker,
            pr_author = %pr.author_login,
            task_creator = creator,
            task_assignee = ?assignee,
            "sweep: PR.user.login not authorised to close — rejected"
        );
        continue;                                         // (D) the swallow point
    }

    if cfg.dry_run { … continue; }
    // emit Linked + Done events …
}
```

### Namespace mismatch at lines (A) + (B) + (C)

`Task.created_by` and `Task.assignee` are **agend-local instance names** (defined at `src/tasks.rs:8-18`), populated by `mcp__task action=create/claim` from the calling instance's identity. Search for any cross-namespace mapping:

```
grep -rn "github_login|gh_login|github_user|agent_to_github" src/  →  0 hits
```

There is no instance↔GitHub identity table anywhere in the codebase. The compare at line (C) takes `pr.author_login` (a **GitHub username**) and stringly-compares it case-insensitively against `task.created_by` (a **local agend instance name**). For these to match, the operator must coincidentally use the *same string* as both their agend instance name and their GitHub login — a coincidence that's not enforced, documented, or guaranteed.

### cheerc's specific case (per issue body)

| Field | Value | Source |
|---|---|---|
| `task.created_by` | `dev-lead` | "only event for this task is the original `Created` event by `dev-lead`" |
| `task.assignee` | unspecified | issue body silent (likely also a local instance name, e.g. `dev-impl-1`) |
| `pr.author_login` | `cheerc` | GitHub user who opened PR #8 |

`"cheerc".eq_ignore_ascii_case("dev-lead")` → `false`. Even if `task.assignee == "dev-lead"`, the comparison still fails. The branch falls into (C) → warn → (D) `continue`, swallowing the closure intent. No `Linked` event, no `Done` event, task remains `open`.

### Why this is a "silent-drop class" pattern

The `tracing::warn!` at (C) is the *only* operator-visible signal, and it has the same problem cheerc-class users keep hitting (cf. issue #525 item 1):

- `tracing::warn!` lands in the daemon's log destination (typically a file, not a foreground terminal in `start --detached` mode).
- The level is `warn`, not `error`, so it doesn't carry the `FATAL:` prefix that the existing `warn_once_user_allowlist_unconfigured` helper uses for D001-class signalling.
- It fires per-(PR, marker) on every tick that observes the merged PR. Without dedup or once-per-author logic, a long-running daemon spams the log with the same warn every 5 min until the PR scrolls off the `per_page=30` window — but the operator never connects the spam to "my sweep doesn't work".

This is exactly the class lead arbitrated for #525 item 1 (Track B): a fail-closed gate whose warn-level signal disappears into a log file most operators never check.

### Other potential silent-drop sites in this module (none load-bearing for cheerc)

- `extract_closes_markers` returns empty → `continue` at line 182 with no log. (Cheerc's marker matches, so this isn't hit.)
- `parse_pr_meta` returns `None` (missing `user.login`) → drop the PR. (Not hit unless the GitHub user is deleted.)
- `merge_commit_sha` / `merged_at` empty → tracing::warn! + continue. Schema-mismatch fail-closed; the warn level matches §4(C) so it shares the silent-drop class but isn't cheerc's specific failure.

The authorship gate is the one that fires for cheerc, and per the trace above, it is structurally guaranteed to fire for any user whose agend instance name ≠ GitHub login.

### Conclusion (§4)
The PR.user.login authorship gate at lines 200-216 is comparing two strings drawn from disjoint namespaces with no mapping between them. Every cross-namespace mismatch silently swallows the close intent and emits only a `tracing::warn!` that operators never see. cheerc's repro hits this gate and is structurally guaranteed to fail.

---

## Verdict

**Root cause:** Authorship gate (`src/daemon/task_sweep.rs:200-216`) compares `pr.author_login` (GitHub username, e.g. `cheerc`) against `task.created_by` / `task.assignee` (agend-local instance name, e.g. `dev-lead`). No identity mapping exists, so the gate fails for any user whose two identifiers don't coincidentally match. The failure path is `tracing::warn! + continue`, a silent-drop class that doesn't surface to the operator.

The other three RCA dimensions ruled out:
- §1 tick scheduling: correct, fires immediately + every 300s.
- §2 marker parser: correct, cheerc's marker matches the regex.
- §3 cross-repo config: correct, sweep polls the right repo and the local task lookup succeeds.

### Why the gate exists (defensive context to preserve)

Per `src/daemon/task_sweep.rs:196-199` and module docs, the gate "defends the pre-PR-220 `update_decision` bug class where a malicious PR body could close another agent's task". The threat model is:

- An untrusted PR (drafted by anyone with push to a feature branch) puts `Closes t-victim-task` in the body.
- Without an authorship check, the sweep would auto-close the victim's task without their consent.

So the gate is load-bearing for security; we cannot defang it without re-opening the PR-220 vulnerability class.

### Fix shape recommendation (Path A standard, ≈40-80 LOC)

Track F should land both pieces:

**Piece 1 — instance↔GitHub identity mapping** (the structural fix):
- Add `github_login: Option<String>` to fleet.yaml `instances:<name>:` entries (mirrors the existing `working_directory` / `repo` / `source_repo` shape from Sprint 54-55).
- Sweep resolves `task.created_by` / `task.assignee` → `github_login` via the fleet config before comparing. If no mapping exists, fall back to direct string compare so existing deployments where instance name == github login keep working.
- Documents the 1:1 expectation that has been implicit since PR-220.

**Piece 2 — D002 doctor diagnostic for unresolvable authorship** (UX completion):
- Mirror the D001 pattern from Sprint 56 Track B.
- At doctor validation time, if `task_sweep.json` has `repo` set AND any open task has `created_by/assignee` not present in the fleet `github_login` map, emit a Critical D002 with copy-paste fix stanza:
  ```yaml
  instances:
    dev-lead:
      github_login: <YOUR_GITHUB_LOGIN>
  ```
- Routes through existing `bootstrap/doctor.rs::emit_diagnostics` → `tracing::error!("FATAL: …")`, which `main.rs:344` writes to stderr for non-`app` commands.

**Out of scope for Track F (separate items in #525-class follow-ups):**
- Upgrading the gate's silent `tracing::warn! + continue` (line 207-216) to a once-per-(marker, author) error-level emit a la `warn_once_user_allowlist_unconfigured`. Improves discoverability *if* an operator misconfigures the new mapping. Optional polish; D002 already covers the common case.
- Generalising the mapping beyond Telegram-style instances (Discord, Slack contributors). The fleet.yaml entry point keeps the mapping per-instance so it composes with whatever channel the instance lives on.

### Risk assessment

- **Behavioural compatibility:** existing deployments where `instance_name == github_login` (Sprint 23 P1's implicit assumption) keep working — the proposed fix's fallback preserves the current direct-compare path when no mapping is configured.
- **Security regression risk:** zero — the gate's authorisation contract is unchanged; only the comparison input is corrected.
- **Test surface:** new unit tests for the mapping resolver (`mapping_present_compares_against_github_login`, `mapping_absent_falls_back_to_direct_compare`, `mapping_collision_resolves_first_match`), plus a sibling doctor test for D002 firing under the empty-mapping case.

### Operator escape hatch (zero-LOC, available today)

Until Track F lands, cheerc can unblock by setting their daemon instance name to match their GitHub login. For example, if their lead instance was created with name `dev-lead` and they are GitHub user `cheerc`, they need to either:
- recreate the lead instance with name `cheerc` (`mcp__create_instance(name="cheerc", …)`), or
- reassign the task with `task action=update assignee=cheerc` after coordinating who actually opens the merging PR.

This is fragile (relies on string coincidence) and is exactly what Track F's mapping replaces with an explicit, documented contract.
