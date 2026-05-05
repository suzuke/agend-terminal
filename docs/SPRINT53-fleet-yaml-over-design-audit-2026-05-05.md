# Fleet.yaml Over-Design Audit

**Date**: 2026-05-05
**Author**: dev
**Sprint**: 53 candidate
**Status**: Finding doc (operator review before action)

---

## Summary

Audit of fleet.yaml schema fields for over-design. Each field assessed for:
runtime wiring, documentation, user configurability, and recommendation.

## Findings

### 1. `defaults.command` / `defaults.args`

| Metric | Value |
|--------|-------|
| Runtime callsites | 4 / 3 (fleet.rs resolve_instance) |
| Documented | Yes (fleet.yaml examples) |
| User-configurable | Yes |
| **Verdict** | **KEEP** — actively used for multi-backend fleets |

**Reasoning**: Operators with mixed fleets (claude + kiro) set `defaults.backend: claude`
and override per-instance. The `command` field is the legacy equivalent. Low cost to maintain.

---

### 2. `defaults.ready_pattern`

| Metric | Value |
|--------|-------|
| Runtime callsites | 1 (fleet.rs L306, fallback chain) |
| Documented | Implicitly |
| User-configurable | Yes (per-instance override available) |
| **Verdict** | **KEEP** — used in resolve_instance fallback chain |

**Reasoning**: Per-backend presets provide defaults, but operators can override for custom
CLIs. The defaults-level fallback is the correct layering.

---

### 3. `defaults.cols` / `defaults.rows`

| Metric | Value |
|--------|-------|
| Runtime callsites | 2 (fleet.rs L360-361) |
| Documented | No |
| User-configurable | Yes |
| **Verdict** | **KEEP (low priority)** — 2 callsites, minimal maintenance cost |

**Reasoning**: Useful for operators running on small terminals or wanting consistent
pane sizes. Zero maintenance burden.

---

### 4. `channel:` (singular)

| Metric | Value |
|--------|-------|
| Runtime callsites | 9 (fleet.rs — normalize, tests) |
| Documented | Yes (Telegram setup docs) |
| User-configurable | Yes |
| **Verdict** | **KEEP** — primary channel config path |

**Reasoning**: This is the main Telegram/Discord configuration entry point.
Actively used by every fleet with a channel binding.

---

### 5. `channels:` (plural)

| Metric | Value |
|--------|-------|
| Runtime callsites | 18 (fleet.rs — normalize, tests) |
| Documented | Partially (comment says "multi-channel routing not yet implemented") |
| User-configurable | Yes (schema accepts it) |
| **Verdict** | **REFACTOR (Sprint 53)** — normalize collapses to singular; multi-channel not wired |

**Reasoning**: The `normalize()` function collapses `channels:` (plural) into `channel:`
(singular) by picking the first entry. Multi-channel routing is explicitly deferred
("T1 will merge inbound streams"). The field exists for forward-compat but adds schema
complexity. **Recommend**: keep the field but add a clear deprecation comment + log warning
when >1 channel is declared. Full multi-channel routing is Sprint 53+ scope.

**LOC to remove if cut**: ~30 (normalize logic + tests). Not recommended — forward-compat value.

---

### 6. `templates:`

| Metric | Value |
|--------|-------|
| Runtime callsites | 2 (fleet.rs L32 schema + L388 lookup) |
| Documented | Yes (deployment docs) |
| User-configurable | Yes |
| **Verdict** | **KEEP** — actively used by `deployment` MCP tool |

**Reasoning**: `src/deployments.rs` reads templates from fleet.yaml to batch-spawn
instances. The `deploy` MCP tool is used in production for team creation.

---

### 7. Per-instance `topic_id`

| Metric | Value |
|--------|-------|
| Runtime callsites | 3 (fleet.rs + telegram binding) |
| Documented | Yes (Telegram topic routing) |
| User-configurable | Yes |
| **Verdict** | **KEEP** — Telegram topic routing depends on it |

---

### 8. Per-instance `outbound_capabilities`

| Metric | Value |
|--------|-------|
| Runtime callsites | 0 (skip_serializing, dead_code allow) |
| Documented | No |
| User-configurable | Absorbed but ignored |
| **Verdict** | **CUT** — dead field, 0 runtime usage |

**Reasoning**: Legacy field with `#[allow(dead_code)]` + `skip_serializing`. Only exists
to prevent deserialization errors on old fleet.yaml files. Can be safely removed after
one release cycle (users have had time to update their fleet.yaml).

**LOC to remove**: ~5 (struct field + allow + skip_serializing)

---

## Recommendations Summary

| Field | Verdict | Action |
|-------|---------|--------|
| defaults.command/args | KEEP | None |
| defaults.ready_pattern | KEEP | None |
| defaults.cols/rows | KEEP | None |
| channel: (singular) | KEEP | None |
| channels: (plural) | REFACTOR | Add deprecation warning for >1 entry |
| templates: | KEEP | None |
| topic_id | KEEP | None |
| outbound_capabilities | **CUT** | Remove after 1 release cycle |

## Immediate Cleanup (this PR)

Remove `outbound_capabilities` field — confirmed 0 runtime callsites, dead code.
