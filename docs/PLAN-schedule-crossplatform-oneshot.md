# Schedule — Cross-platform Timezone + One-shot Support

> Date: 2026-04-20
> Branch: `worktree-schedule-xplatform-oneshot`
> Status: in progress

---

## Motivation

Two gaps in the current schedule subsystem:

1. **Timezone detection fails on Windows.** `src/schedules.rs::detect_timezone()`
   only consults `$TZ` and `/etc/localtime`, so a Windows user who omits the
   `timezone` field silently gets UTC — a daily `"0 9 * * *"` in Taipei
   actually fires at 17:00 local.
2. **No one-shot trigger.** `create_schedule` only takes cron. The `cron`
   crate's 6-field format has no year, so there is no expression that
   guarantees "fire once at 2026-04-21 15:30 and never again". Users must
   post-hoc disable or delete the schedule.

The design below fixes both with minimum API disruption.

---

## Phase 1 — Cross-platform timezone detection

### Approach

Replace the homegrown detection with the `iana-time-zone` crate. It returns
an IANA name on all three platforms:
- Linux: reads `/etc/localtime` symlink (same as today).
- macOS: uses Core Foundation APIs.
- Windows: reads the registry and maps Windows TZ names to IANA.

New chain, preserving the `OnceLock` cache and the `$TZ` override:

```
1. $TZ env (first call, explicit override)
2. iana_time_zone::get_timezone()
3. "UTC"
```

Every downstream consumer already takes the IANA name (`chrono_tz::Tz::from_str`
in `cron_tick.rs:62-76`), so there is no knock-on change.

### Files touched

- `Cargo.toml` — add `iana-time-zone = "0.1"` (pure-Rust, MIT/Apache,
  already a transitive dep of parts of the `chrono` ecosystem on Windows).
- `src/schedules.rs::detect_timezone()` — swap the second step of the chain.

### Tests

- `detect_timezone` test that sets `TZ=Asia/Tokyo` → returns the override.
  Because `OnceLock` caches for process lifetime, run this as a standalone
  binary test or gate it on the cache being empty.
- A smoke test that simply asserts the result parses as `chrono_tz::Tz`
  (the guarantee we actually depend on downstream).

### Risk

Low. The function returns `&'static str`; behaviour change only when the
old code would have hit the UTC fallback unnecessarily.

---

## Phase 2 — One-shot trigger

### Data model

Move from a scalar `cron: String` to a tagged enum so the schedule type is
explicit in the data:

```rust
pub struct Schedule {
    pub id: String,
    pub trigger: Trigger,            // replaces `cron: String`
    pub message: String,
    pub target: String,
    pub label: Option<String>,
    pub timezone: String,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub run_history: Vec<ScheduleRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// Recurring cron expression (5- or 6-field). Behaviour identical to v1.
    Cron { expr: String },
    /// Fire exactly once at `at`, interpreted as a wall-clock time in the
    /// enclosing schedule's `timezone`. After a successful trigger the
    /// schedule is auto-disabled.
    Once { at: String }, // RFC 3339 / ISO 8601
}
```

`at` is stored as a string (RFC 3339 with offset) rather than
`DateTime<FixedOffset>` so the on-disk shape stays text-canonical and
diff-friendly, matching the rest of the store.

### Schema migration (v1 → v2)

`ScheduleStore::CURRENT` bumps from `1` to `2`. Migration logic runs on
first `load_versioned`:

- If a row has a top-level `cron: "..."` field, rewrite to
  `trigger: { "kind": "cron", "expr": "..." }`.
- Drop the legacy `cron` field.
- Bump stored `schema_version` to 2 on next save (handled automatically by
  `mutate_versioned`).

Implementation note: because `load_versioned` deserialises into the typed
struct, we need a custom `Deserialize` for `Schedule` (or a two-step:
peek JSON → migrate in-memory → deserialize) to accept legacy rows.
The cleanest path is a `#[serde(default)]` + custom `deserialize_with` on
`trigger` that can read either the new shape or a legacy `cron` sibling.
Alternative: a helper `fn migrate_json(v: &mut Value)` that runs once in
`load_versioned` before the typed decode. The latter keeps schedule.rs
free of serde tricks and is what we'll do.

### `check_schedules()` changes

In `src/daemon/cron_tick.rs`, replace the `cron` extraction with a match
on `trigger`:

- `Cron { expr }` — unchanged behaviour.
- `Once { at }` — parse `at` as RFC 3339 with offset. Fire if
  `last_check_utc < at_utc <= now_utc`. If `at_utc < last_check_utc`, the
  shot was missed (daemon down / long gap) — record `status = "missed"`
  and still auto-disable so it does not lurk forever.

After any `Once` trigger (whether `ok` / `ok_inbox` / `missed` /
`inject_failed`), set `enabled = false` in the store.

### MCP interface

`create_schedule` tool input schema adds a mutually-exclusive `run_at`:

```
{
  "message":  "required",
  "target":   "optional (defaults to caller)",
  "timezone": "optional (defaults to detected)",
  "label":    "optional",

  // exactly one of:
  "cron":     "5/6-field cron expression",
  "run_at":   "ISO 8601 wall-clock, e.g. 2026-04-21T15:30:00"
}
```

Rules:
- Neither field present → 400 "must supply cron or run_at".
- Both present → 400 "cron and run_at are mutually exclusive".
- `run_at` with an offset (e.g. `+08:00`) → use as-is.
- `run_at` without offset → combine with `timezone` (resolved via
  `chrono_tz`) to produce a concrete instant. Ambiguous / non-existent
  wall-clocks (DST transitions) → 400 with the conflict reason.
- `run_at` resolving to `<= now` → 400 "run_at must be in the future".

`update_schedule` gains `run_at` with the same rules (still mutually
exclusive with `cron`). Changing trigger kind on update is allowed;
re-enabling a fired one-shot requires also supplying a new `run_at` in
the future.

`list_schedules` response shape: the legacy `cron` top-level field is
replaced by `trigger`. Callers already parse JSON dynamically; no MCP
consumer in the repo destructures `cron` directly (verified via grep at
implementation time).

### Edge cases

| Case | Behaviour |
|---|---|
| One-shot `at` in the past at create time | reject |
| Daemon down through one-shot `at` | `status = "missed"`, auto-disable |
| `timezone` unknown → falls back to UTC (existing) | unchanged; `run_at` without offset uses UTC |
| DST ambiguous local time | reject with clear error |
| `run_once` fires `inject_failed` | still auto-disable (one-shot is not retry-safe; user can re-create) |

### Files touched

- `src/schedules.rs` — struct, enum, migration helper, `create` / `update`
  validation.
- `src/daemon/cron_tick.rs` — match on `trigger`, one-shot firing window,
  auto-disable.
- `src/mcp/tools.rs` — update `create_schedule` / `update_schedule` schema.
- `docs/MCP-TOOLS.md` — update the two tool descriptions.
- Tests in both modules + a migration test in `schedules.rs`.

---

## Task breakdown

1. **P1.a** — Add `iana-time-zone` dep; swap `detect_timezone()` chain.
2. **P1.b** — Test coverage for `detect_timezone()` TZ-override + IANA parse.
3. **P2.a** — Introduce `Trigger` enum; migration helper for v1 JSON.
4. **P2.b** — `create` / `update` accept `run_at`, validate mutual exclusion
   and future-ness; tests.
5. **P2.c** — `check_schedules()` dispatch + `Once` firing window +
   auto-disable + `missed` status; tests including tz + DST edge.
6. **P2.d** — MCP tool schema (`create_schedule`, `update_schedule`) +
   docs refresh (`docs/MCP-TOOLS.md`).
7. **P3** — Cross-platform CI: already green on `windows-latest` +
   `macos-latest` + `ubuntu-latest` in `.github/workflows/ci.yml`; push
   the branch and watch.

---

## Delivery

Single branch `worktree-schedule-xplatform-oneshot`, squash-ready commits
per task. Because this changes the on-disk schema (v1 → v2) and affects
CI, this lands as a PR rather than a pure local merge.
