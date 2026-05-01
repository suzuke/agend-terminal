# Sprint 45 G3 M3 — Reserved Instance Names Design Proposal

**Date**: 2026-05-01
**Status**: Proposal — awaiting operator decision

## Problem

The team isolation gate in `src/api/handlers/messaging.rs` treats `general` as a cross-team bus: any message from/to an instance named `general` bypasses team isolation checks. If a user creates an agent instance named `general`, it inherits bus privileges unintentionally.

## Current reserved names (implicit, from code)

| Name | Where | Privilege |
|---|---|---|
| `general` | `messaging.rs:52` | Cross-team bus bypass |
| `system:auto_close` | `tasks.rs` SYSTEM_IDENTITIES | ACL bypass for task mutation |
| `system:overdue_sweep` | `tasks.rs` SYSTEM_IDENTITIES | ACL bypass for task mutation |
| `system:task_sweep` | `tasks.rs` SYSTEM_IDENTITIES | ACL bypass for task mutation |

## Proposal options

### Option A: Warn on creation (recommended, minimal)
- `agent::validate_name()` logs a warning when name matches a reserved pattern
- Does NOT block creation — backward compatible with existing fleets
- Warning text: "instance name 'general' has special routing privileges"

### Option B: Block reserved names on creation
- `agent::validate_name()` rejects reserved names
- Breaking change for existing fleets that have `general` instance
- Requires migration path

### Option C: Namespace prefix convention
- Reserved names use `system:` prefix (already in use for SYSTEM_IDENTITIES)
- `general` renamed to `system:general` in fleet.yaml
- Breaking change, requires fleet.yaml migration

## Recommendation

Option A for now — warn but don't block. Operator can decide to escalate to Option B/C in a future sprint.

## Operator decision needed

- [ ] Accept Option A (warn only)?
- [ ] Escalate to Option B (block) or C (namespace)?
- [ ] Add other names to reserved list?
