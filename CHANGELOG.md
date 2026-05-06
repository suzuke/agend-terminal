# Changelog

## [Unreleased] — 2026-05-06

### Sprint summary — agend-git-shim + production hardening

**agend-git-shim 5 phases** (PR #444 proposal, #446–455 IMPL):
- Phase 1: trailer hook + binding.json (PR #446)
- Phase 2: Rust shim with passthrough/chdir/deny dispatch (PR #447)
- Phase 3: worktree lease/release with main-branch rejection (PR #449)
- Phase 4: GC dry-run + cutover gate via `AGEND_WORKTREE_GC=1` (PR #454)
- Phase 5: hotspot detection with hourly daemon sweep (PR #455)

**6 hotfixes**:
- Hotfix A: retry header dedup — PR #452 (#436 follow-up)
- Hotfix B: provenance truncate for Telegram limit — PR #453
- Hotfix C: CI auto-watch on dispatch — PR #451
- Hotfix D: agend-git-shim app-mode wiring — PR #457
- Hotfix E: ci-watch grace period for young watches — PR #458
- Issue #456: deployment teardown cleanup — PR #459

**Sprint 52 router-layer** (PR #437 + #440):
- PR-A: observer infra + reply_to wiring + lock ordering invariants
- PR-B: mirror dispatch + dedup + proptest + stress gate

**Other**:
- Agent CLI /exit /quit no longer triggers respawn (PR #430)
- Anthropic server-side rate limit auto-retry (PR #432, #436)
- codex + gemini + opencode migrated to agend-mcp-bridge (PR #438)
- kiro mcp config uses env field, eliminates wrapper.sh (PR #439)
- fleet.yaml per-instance instructions field (PR #442)
- fleet.yaml over-design audit (PR #443)
