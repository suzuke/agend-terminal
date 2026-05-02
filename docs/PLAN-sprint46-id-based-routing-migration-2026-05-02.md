# Sprint 46 PLAN — ID-Based Routing Migration

**Date**: 2026-05-02
**Author**: lead
**Status**: PLAN (awaiting §8 GO)
**Source-of-truth**: `origin/main` HEAD `8410e05`
**Synthesis inputs**:
- dev STRUCTURAL — m-20260502103252234987-238
- reviewer PRIOR-ART — m-20260502103238404085-237
- reviewer COST-BENEFIT — m-20260502103540323016-240
- lead MINIMAL-DELTA — this document

**Supersedes**: PR #405 (closed) — bandaid Option A scope (instance-name precedence over team-template lookup at single callsite)

---

## §0 Context

Sprint 44 M5 PR #383 attempted to fix a "cannot send to self" rejection when dispatching `kind=task` to instances whose name collides with a `templates.X.orchestrator` entry in `fleet.yaml`. The fix only improved the error message — root cause persists: `src/mcp/handlers/comms.rs::handle_delegate_task` line 159 calls `resolve_team_orchestrator(home, raw_target)` which shadows instance-name lookup whenever the team-template name matches.

**Live workaround active**: For instance `dev` (collides with `templates.dev`), the fleet uses plain `send` with `[delegate_task]` header instead of `kind=task`. Tracked in `feedback_kind_task_self_route_workaround.md`.

Operator decision m-20260502103006966627-233 (2026-05-02 10:30 UTC): pivot Sprint 46 from Option A bandaid to **ID-based routing migration** as root fix.

## §1 Goal

Make routing identity **immutable** (ID) and naming a **mutable display alias**, eliminating an entire class of name-collision bugs at design level rather than per-callsite.

**Non-goals**:
- Multi-fleet / cross-daemon routing
- Reserved-name policy enforcement (warn-only in P1, no block)
- Distributed UID across hosts (intra-daemon scope only)

## §2 Verified state (origin/main 8410e05)

```
fleet.yaml:
  instances.dev (real worker, kiro-cli backend)
  templates.dev (orchestrator: lead)  <-- collision
  instances.reviewer (codex backend)
  instances.lead (claude backend, orchestrator)
  instances.general (claude backend, operator-proxy)

src/mcp/handlers/comms.rs:148-160 handle_delegate_task — pre-check at L159 calls resolve_team_orchestrator BEFORE checking instance_exists
src/teams.rs:330 resolve_team_orchestrator — returns templates.{name}.orchestrator when name matches a template
src/api/handlers/messaging.rs:29 self-route reject if sender == target
src/inbox.rs inbox_path uses {name}.jsonl path
src/agent.rs lock_registry HashMap<String, Handle> keyed on name
src/tasks.rs:51 instance_exists helper (existing, returns bool)
```

## §3 Design — UUIDv4 + short-id alias

### §3.1 ID schema

```rust
// src/types.rs (new)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(Uuid);  // wraps uuid::Uuid

impl InstanceId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn short(&self) -> String {
        // first 8 hex chars from UUID display form
        format!("{:.8}", self.0.simple())
    }
    pub fn parse(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(Self)
    }
}
```

**Rationale**: UUIDv4 (per reviewer COST-BENEFIT m-240 §1) — collision risk eliminated for any future scale; short-id (8 hex chars) is display/CLI alias only, not primary key. Storage cost negligible (16 bytes binary or 36 chars string).

### §3.2 fleet.yaml schema

```yaml
instances:
  dev:
    id: 6f1a2b9c-...-...-...  # UUIDv4, additive #[serde(default = "InstanceId::new")]
    backend: kiro-cli
    ...
```

`#[serde(default)]` on `id` field — existing fleet.yaml without `id` round-trips fine; daemon backfills on first load and writes back.

### §3.3 Resolution order (per reviewer m-237)

```
1. exact UUIDv4 string match (caller passed full ID)
2. exact short-id (8-char) match — must be unique across fleet
3. exact name match — collision returns Ambiguous(name, ids[])
```

Helper `resolve_instance(home, name_or_id) -> Result<(InstanceId, String), ResolveError>`. ~25 LOC.

### §3.4 Migration writeback

Daemon startup:
1. Load fleet.yaml
2. For each instance lacking `id`, generate UUIDv4
3. Serialize back via `save_atomic` (existing `tmp + fsync + rename`)
4. Log one-time `[fleet-migration]` notice with diff summary
5. Operator opt-out via `AGEND_FLEET_NO_AUTO_MIGRATE=1` env (writeback skipped, runtime IDs only)

### §3.5 Display surface (per reviewer m-240 §5)

- Pane title: `dev` (name only, current behavior preserved)
- Inbox header: `[from:dev]` (LLM token stable — no `id` injection)
- Decision board / status_summary: name default; `name (inst_a3k9p2xf)` only when collision detected
- Debug logs: full `name (id)`
- Error messages on Ambiguous: `name "dev" matches 2 instances: a3k9p2xf, b8c2d1ef — pass id explicitly`

## §4 Phase split (per reviewer COST-BENEFIT m-240, **revised 2026-05-02 per operator m-298**)

> **Re-scope note (2026-05-02 16:00 UTC)**: P1 IMPL (PR #407 r1-fix) shipped ID infrastructure
> + an instance-first name-lookup quick-fix for M5 (NOT ID-based dispatch). This is honest re-scope:
> P1 retitled to "ID infra + M5 instance-first quick-fix"; P2 retitled to "ID-based routing migration
> (true)" and gains a binding commitment to remove the name-string fallback.
> Operator ruling m-20260502154707580980-298.

### Phase 1 — ID infra + M5 instance-first quick-fix (Tier-2) — SHIPPED in PR #407

**Scope** ~140 LOC actual (vs ~75 LOC PLAN estimate):
- `src/types.rs` InstanceId UUIDv4 type + 2 tests (~59 LOC)
- `src/fleet.rs` backfill_ids on load + reserved-name warn + writeback (~39 LOC)
- `src/api/handlers/messaging.rs` inbox `from` field includes `(id8)` per §3.5 / operator §13.5 (~11 LOC)
- `src/mcp/handlers/comms.rs` **instance-first lookup** (NOT ID routing): if `fleet.instances.contains_key(raw_target)` skip team resolution (~22 LOC)
- `src/main.rs` wire backfill on daemon start (~1 LOC)
- Cargo.toml uuid v4+serde feature

**Tests**:
- ID generation + UUIDv4 format ✓
- Default impl ✓
- M5 regression: `delegate_task_instance_first_bypasses_team_orchestrator_collision` ✓
- (deferred to P2: name→id resolution, Ambiguous error, registry dual-index round-trip)

**Files touched**: 6 (types.rs new, fleet.rs, messaging.rs, comms.rs, main.rs, Cargo.toml/lock).

**NOT done in P1** (deferred to P2 with binding commitment):
- ❌ `src/agent.rs` registry `HashMap<InstanceId, Handle>` dual-index
- ❌ `src/mcp/handlers/comms.rs` resolve name→id and route by id (currently routes by name)
- ❌ `resolve_instance(home, name_or_id) -> Result<(InstanceId, String), ResolveError>` helper
- ❌ Ambiguous-name error path

**Done definition (revised)**: Sprint 44 M5 unblocked via instance-first lookup. Workaround `feedback_kind_task_self_route_workaround.md` retired. ID infra in place but not yet routing-bearing.

### Phase 2 — ID-based routing migration (true) + file path migration (Tier-2) — **binding**

> Re-scope per operator m-298: P2 absorbs the routing migration de-scoped from P1.
> **Binding constraint**: P2 MUST eliminate the name-string fallback in dispatch path.
> No more `if instances.contains_key(name) { route_by_name }` shortcut — all routing must
> resolve through `InstanceId`. Bandaid pattern is non-negotiable to remove.

**Scope** ~75-90 LOC (revised up from ~40):

Routing migration (de-scoped from P1):
- `src/agent.rs` registry: `HashMap<InstanceId, Handle>` + `name_to_id_index: HashMap<String, Vec<InstanceId>>` (~15 LOC)
- `src/mcp/handlers/comms.rs` `handle_send_to_instance` + `handle_delegate_task`: replace instance-first name lookup with `resolve_instance(home, raw_target) -> Result<(InstanceId, String), ResolveError>`; route by id (~25 LOC, REPLACES P1 r1-fix bandaid)
- `src/api/handlers/messaging.rs` self-route check by id (~5 LOC)
- `resolve_instance` helper (~15 LOC)

File path migration (original P2):
- `src/inbox.rs::inbox_path` accepts `&InstanceId`, writes to `inbox/{id}.jsonl` (~15 LOC)
- Migration: name-based file exists → create id-based file/symlink (~15 LOC, with Windows file-copy fallback per operator §13.7)
- `metadata/{id}.json` + `ipc/{id}.port` similar (~10 LOC)
- Cleanup sweep: deferred to Sprint 47

**Tests**:
- name resolves to single id (1)
- collision returns Ambiguous (1)
- M5 regression survives — routing now via id, name-string fallback REMOVED (1)
- new instance writes to `{id}.jsonl` directly (1)
- legacy instance: name-based file → id-based file via migration (1)
- writes after migration go to id-based path (1)

**Tier**: Tier-2 dual review — routing core + file path migration both high blast radius.

**Dependency**: requires P1 merged.

**Done definition**: Bandaid pattern in comms.rs removed. All routing resolves through InstanceId.

### Phase 3 — Audit trail (Tier-1)

**Scope** ~25 LOC:
- `task_events.rs::InstanceName` add `emitter_id: Option<InstanceId>` field (~10 LOC)
- `dispatch_tracking.rs` add `from_id` / `to_id` fields (~10 LOC)
- Display layer prefers `name` for human-readable output (~5 LOC)

**Tier**: Tier-1 single (codex PRIMARY) — observability layer, low blast radius.

**Dependency**: requires P1 merged. P2 not strictly required (audit can ship without file rename).

## §5 MINIMAL-DELTA verification (lead vantage)

**Question**: Can the M5 unblock ship in <65 LOC?

**Answer**: No. Phase 1's ~75 LOC is the irreducible floor for ID-based routing. Sub-bandaid alternatives (e.g. per-callsite instance-name precedence) were the rejected PR #405 approach. The 10 LOC reserved-name warn could be deferred to a follow-up PR but operator m-20260502102244341875-219 had originally bundled it; reviewer m-240 §6 recommends keeping in P1 for config-readability.

**Prior dispatcher trace (m-225) reuse**: 100% applicable — the 6 callsites in comms.rs / messaging.rs / inbox.rs are exactly the Phase 1 + Phase 2 boundary. Trace remains the source-of-truth callsite list.

## §6 Backward compat checklist

- [x] `fleet.yaml` without `id` field — round-trips via `#[serde(default)]`
- [x] Existing inbox files name-based — symlink layer in P2
- [x] MCP `target_instance: name` — resolves transparently via name→id index
- [x] Plain `send` workaround — continues to work; M5 fix removes need
- [x] Operator can opt out of writeback via env flag
- [x] Display layer (pane title / inbox header) — name still primary

**Non-compat (intentional)**:
- Future `target_id` MCP field — additive, optional
- Audit trail gains `_id` columns — additive, doesn't break readers

## §7 Risks

**HIGH**:
- P2 symlink migration on Windows: NTFS junction vs symlink semantics differ. **Mitigation**: detect platform, fallback to copy-then-rename strategy on Windows. Add CI matrix test.
- Daemon restart mid-write: writeback uses `save_atomic` (existing pattern) — fsync invariant ensures partial-write safety.

**MED**:
- Operator sees fleet.yaml diff post-migration: 3-line diff per instance + log notice. **Mitigation**: clear `[fleet-migration]` log + reference to this PLAN doc + opt-out flag.
- ResolveError::Ambiguous user-facing: error message must include candidate IDs. Test coverage above.

**LOW**:
- UUIDv4 vs short-id confusion in CLI: short-id only valid via `inst_<8chars>` prefix syntax to avoid collision with name parser. Documented in resolve_instance helper.

## §8 §13 candidate questions for operator

1. **ID format final**: UUIDv4 binary (16B) + 8-char hex short-alias OK, or prefer base32 short-id `inst_<8chars>` as PRIMARY (smaller display, sufficient at 4-instance scale)?
2. **Migration writeback default**: enabled by default with opt-out env flag, or opt-in (operator runs explicit `agend-terminal migrate-ids` command)?
3. **Reserved-name warning**: ship in P1 (warn-only) or drop entirely (ID migration eliminates collision class — warn redundant)?
4. **Phase 1 standalone merge OK**: P1 alone unblocks M5 + ships warn — can merge before P2/P3 land, or require all 3 phases land together for atomic semantics?
5. **Display surface — inbox header**: keep `[from:dev]` (LLM token stable) or surface `[from:dev (a3k9p2xf)]` for debugging? Default = name-only; ID added only on collision.
6. **fleet.yaml git tracking**: writeback creates 3-line diff per existing instance — accept or stash IDs in `.agend-terminal/state.json` outside git?
7. **Symlink fallback on Windows**: copy-and-rename (P2) — accept storage 2x for transition period, or hard-cutover with downtime?
8. **Tier classification confirm**: P1/P2 Tier-2 dual + P3 Tier-1 single — agree?
9. **Phase split confirm**: 3 sequential PRs vs 1 bundled — sequential OK per reviewer COST-BENEFIT, but each adds review cycle overhead?
10. **Workaround sunset**: `feedback_kind_task_self_route_workaround.md` retired after P1 merge confirmed live, or wait for full P3?

## §9 Estimates

- Phase 1: code 1.5h + test 1h + review cycle ~3-6h = ~6-9h elapsed
- Phase 2: code 1h + test + Windows CI debug 2h + review 3-6h = ~6-10h elapsed
- Phase 3: code 0.5h + test 0.5h + review 2-4h = ~3-5h elapsed
- Total: ~15-24h elapsed across ~3-4 working days

## §10 Reuse breakdown from prior synthesis

- m-225 dispatcher trace: 100% applicable to P1 callsite list
- m-224 namespace shadowing PRIOR-ART: 30% (k8s UID, Linux PID applicable; Bash builtin off-topic)
- m-227 COST-BENEFIT (Option A bandaid): 0% — superseded
- §1 verified state: 80% — fleet.yaml + comms.rs:148-160 unchanged

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
