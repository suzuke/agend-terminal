# PLAN: Expand state-replay fixtures (ToolUse / PermissionPrompt / ContextFull)

**Filed**: 2026-04-20
**Status**: Open — pick up in a fresh session
**Prerequisite**: Phase 1a–1f complete (on `origin/main`)

## Goal

`tests/fixtures/state-replay/` currently covers the startup → idle →
thinking path for all 5 backends plus codex Update-menu. That leaves
three high-signal state paths with zero real-PTY regression coverage:
`ToolUse`, `PermissionPrompt`, `ContextFull`. This plan adds targeted
fixtures for those paths.

## Why this matters

| Scenario | Pattern risk | Why add coverage |
|----------|-------------|------------------|
| ToolUse | Phase 1b edited claude / opencode tool patterns | No real-PTY test — only synthetic unit tests |
| PermissionPrompt | Critical UX: wrong pattern → operator can't tell CLI is blocked | Only synthetic unit tests |
| ContextFull | Phase 1b removed `\|/compact` from kiro | Already unit-tested; real-PTY regression is bonus |

## Priority / phasing

Do **Stage 2a first, then decide**. Do not try to collect all 15
recordings in one go — the marginal scenarios aren't worth the
recording effort.

### Stage 2a — ToolUse × 5 (recommended: all 5 backends)

Every backend's tool pattern is active code; Phase 1b edited two of
them. This is the whole point of the replay suite.

### Stage 2b — PermissionPrompt × 2 (claude-code + codex only)

These are the two backends where operators encounter permission
prompts most often. The other three (kiro trust-all, opencode, gemini
yolo) require disabling safety flags just to reproduce — high
recording friction, low additional signal.

### Stage 2c — ContextFull (deferred, likely skip)

Hard to reproduce reliably (requires saturating the context window).
Only kiro's pattern changed in Phase 1b and that regression is already
covered by `pipeline_kiro_slash_menu_does_not_trigger_context_full`.
Real value is marginal.

## Recording protocol

Same as Phase 1e: `script -q /tmp/<name>.raw <cli-invocation>`, drive
the CLI to the target state, exit cleanly so the recording captures
the transition.

### Stage 2a ToolUse recordings

Run each in a fresh terminal tab (not inside an AgEnD pane).

```bash
# 1. claude-code
script -q /tmp/claude-tooluse.raw claude
#   (wait for Ready)
#   prompt: list files in current directory
#   (let claude call the bash/ls tool and return the result)
#   /exit

# 2. codex
script -q /tmp/codex-tooluse.raw codex
#   prompt: read the README.md file
#   (let codex read the file and return)
#   /exit

# 3. gemini (--yolo to auto-allow tools)
script -q /tmp/gemini-tooluse.raw gemini --yolo
#   prompt: read package.json
#   /exit

# 4. kiro-cli --trust-all-tools
script -q /tmp/kiro-tooluse.raw kiro-cli --trust-all-tools
#   prompt: list files in current directory
#   /quit

# 5. opencode
script -q /tmp/opencode-tooluse.raw opencode
#   prompt: read README.md
#   /exit
```

### Stage 2b PermissionPrompt recordings (if we do this stage)

CLI must be started WITHOUT the bypass/yolo/trust-all flag so the
permission dialog actually appears. Recording ends with the dialog
still on screen — either respond "deny" or Ctrl-C to close.

```bash
# 1. claude-code (default — no --bypass-permissions)
script -q /tmp/claude-perm.raw claude
#   prompt: write "hello" to /tmp/claude-perm-test.txt
#   when the "Allow once / Deny" dialog appears, choose Deny
#   /exit

# 2. codex (default — no --approve-all or similar)
script -q /tmp/codex-perm.raw codex
#   prompt: write "hello" to /tmp/codex-perm-test.txt
#   when prompted, deny
#   /exit
```

### Stage 2c ContextFull (if we do it at all)

Only kiro, and only if we accept the fragility of trying to trigger
`compacting context` reliably. Likely skip.

## Integration steps (per recording)

After each recording, the integration work follows the Phase 1e /
Phase 1f pattern. **Reference commit: `e5a817c`** (`test(state): add
codex Update-menu replay fixture`) as the minimal template.

1. **Sync git first** (per memory rule — always fetch/pull before new
   work): `git fetch origin main && git pull --rebase origin main`.
2. Create worktree: `git worktree add /Users/suzuke/.agend-terminal/worktrees/<name> -b <branch> main`.
3. Copy recording into `tests/fixtures/state-replay/<backend>-<scenario>.raw`.
4. **Scan for secrets** before committing:
   ```bash
   LC_ALL=C tr -c '[:print:][:space:]' ' ' < <file> | \
     grep -iE "api[_-]?key|secret|token|password|bearer|sk-[a-zA-Z0-9]|xoxp-|ghp_|pat_" | head
   ```
   Matches on "tokens" (as in model token counts) are false positives.
5. Run the `#[ignore]`'d replay harness to get ground-truth transitions:
   ```bash
   cargo test --bin agend-terminal --no-run
   REPLAY_FILE="tests/fixtures/state-replay/<name>.raw" REPLAY_BACKEND="<backend>" \
     target/debug/deps/agend_terminal-* replay_session --ignored --nocapture
   ```
6. Add entry to `tests/fixtures/state-replay/MANIFEST.yaml`:
   ```yaml
   - file: <backend>-<scenario>.raw
     backend: <backend>
     cli_version: "<x.y.z>"
     recorded_on: "2026-MM-DD"
     scenario: "<short description>"
     expected_transitions: [<observed from step 5>]
     expected_final_state: <observed>
     expected_final_detect: <observed, or null>
   ```
7. `cargo test --bin agend-terminal -- replay_manifest_regression` → must pass.
8. `cargo test --bin agend-terminal` (full suite) + `cargo clippy
   --bin agend-terminal --all-targets -- -D warnings`.
9. Commit → local merge → push (`git pull --rebase origin main` before
   push in case origin moved).

## Bundling strategy

**One commit per stage, not per recording.** Stage 2a is 5 recordings
in a single commit; MANIFEST grows by 5 entries. The replay test is
one invocation that iterates the whole manifest, so there's no
granularity gain from splitting.

## Known quirks to document in MANIFEST comments

Observed in Phase 1e/1f replays — likely to recur:

- **Byte-level replay doesn't let hysteresis elapse** (2s active /
  5s passive min_hold). Fixtures that show a transition in production
  may appear "latched" through the replay. Document this per fixture.
- **`expected_final_detect` can differ from `expected_final_state`**
  when hysteresis blocks the downgrade. Capture both.
- **Recordings cut mid-action** (user exited before state cleared)
  end up latched on whatever active state was last fed. Fine; just
  record what's observed.

## Version recording

Each fixture MUST carry `cli_version` + `recorded_on` in MANIFEST.
Test failure message includes these so debuggers can tell "pattern
regression" from "CLI upstream UI drift" at a glance.

Known versions as of 2026-04-20:
- claude-code: 2.1.98
- codex: 0.120.0
- gemini: 0.37.1
- kiro-cli: 2.0.1
- opencode: 1.4.0

If any version has bumped by the time this plan is executed, note the
new version in the fixture entry — don't claim parity with the Phase
1e fixtures.

## Out of scope

- Idle → Ready enum merge (see `docs/FOLLOWUP-merge-idle-ready.md`)
- Changing the replay harness's `+10s pacing` to actually clear
  hysteresis during replay. Real fix is non-trivial (hash dedup logic
  interacts with re-feed) and only matters if we want fixtures to
  cover full transition cycles — which we don't, synthetic unit tests
  already do.
