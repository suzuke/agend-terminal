# PTY Fixture Capture Playbook

Structured recipes for operator-side batch capture of PTY fixtures.
Agents cannot spawn fresh interactive backends (no real PTY allocator),
so all fixture recording must be done by the operator in a live terminal
session. Each `.raw` file records the exact byte stream a backend emits,
including ANSI escapes, cursor positioning, and SGR color codes. These
bytes are the ground truth for state detection, dismiss-pattern matching,
and the composite-signature framework (#996).

`cli_version` matters: patterns detect literal strings that change across
CLI versions. Always record the version and date so regressions can be
distinguished from upstream UI changes.

---

## Setup Checklist

Before starting the capture session, prepare the environment:

- [ ] Create a throwaway working directory:
  ```bash
  export CAPTURE_DIR=$(mktemp -d /tmp/agend-fixture-capture-XXXXX)
  cd "$CAPTURE_DIR"
  git init  # some backends require a git repo
  ```

- [ ] Verify target backend binary is on PATH:
  ```bash
  which claude && claude --version
  which codex && codex --version
  which gemini && gemini --version
  which kiro && kiro --version
  which opencode && opencode --version
  ```

- [ ] Back up backend config if you need a clean-state capture:
  ```bash
  # Only if needed for trust-prompt / first-run scenarios
  mv ~/.claude ~/.claude.bak 2>/dev/null
  mv ~/.codex ~/.codex.bak 2>/dev/null
  ```

- [ ] Confirm `script` syntax (macOS BSD vs GNU):
  ```bash
  # macOS BSD (default):
  script -q /tmp/test-capture.raw claude --version
  # GNU (Linux):
  script -q -c "claude --version" /tmp/test-capture.raw
  ```
  All recipes below use **macOS BSD** syntax. On Linux, swap to
  `script -q -c "<command>" <output-file>`.

- [ ] Prepare MANIFEST.yaml entry template (copy this for each capture):
  ```yaml
  - file: <backend>-<scenario>.raw
    backend: <backend-name>
    cli_version: "<version>"
    recorded_on: "<YYYY-MM-DD>"
    scenario: "<one-line description>"
    expected_transitions: [starting, ...]
    expected_final_state: <state>
    expected_final_detect: <state-or-null>
    capture_kind: real_pty
    provenance: "<issue-ref> batch capture by operator"
  ```

---

## Per-Scenario Recipes

### Priority 1: Phase 2a Urgent

#### R1. `claude-yes-proceed.raw` (~3 min)

**Goal**: Capture the "Yes, proceed" confirmation modal that appears when
Claude Code asks for permission (e.g. `--dangerously-skip-permissions`
confirmation or an update prompt). Needed for Phase 2a keystroke audit.

```bash
# 1. Start capture
script -q "$CAPTURE_DIR/claude-yes-proceed.raw" claude

# 2. Trigger: type a request that causes a "Yes, proceed" confirmation.
#    Alternatively, if an update is available, the update-now prompt
#    should show "Yes, proceed" as an option.

# 3. DO NOT dismiss the modal yet -- let it sit for 2-3 seconds so the
#    full ANSI rendering is captured.

# 4. Press Ctrl-C or type /exit to end the session.
```

**Verify**: `xxd claude-yes-proceed.raw | grep -c '1b\['` should show
ANSI escape sequences. The file should contain the literal string
"Yes, proceed".

```bash
grep -c "Yes, proceed" "$CAPTURE_DIR/claude-yes-proceed.raw"
# Expected: >= 1
```

**Time estimate**: ~3 min

---

### Priority 2: #996 Framework Support

#### R2. `claude-trust-prompt.raw` (~2 min)

**Goal**: Capture the trust-folder modal that appears when Claude Code is
launched in an untrusted directory for the first time. Needed for
composite-signature discriminator calibration.

```bash
# 1. Ensure clean state (no prior trust for this dir)
rm -rf /tmp/agend-untrusted-test && mkdir /tmp/agend-untrusted-test
cd /tmp/agend-untrusted-test && git init

# 2. Start capture
script -q "$CAPTURE_DIR/claude-trust-prompt.raw" claude

# 3. Wait for the trust modal to appear ("Do you trust the files in
#    this folder?" or "Yes, I trust the files in this folder").
#    Let it render fully (2-3 sec).

# 4. Press Ctrl-C to exit WITHOUT dismissing (we want the modal bytes).
```

**Verify**: file should contain "trust" and ANSI box-drawing characters.

```bash
grep -c "trust" "$CAPTURE_DIR/claude-trust-prompt.raw"
```

**Time estimate**: ~2 min

---

### Priority 3: Productive Marker Captures (#1014 / S2)

For each backend, we need two scenarios:

- **productive_marker_fire**: tool result returns and a visible marker
  appears on screen (e.g. file-write confirmation, command output).
- **productive_silence**: long pause in an active session with no visible
  progress markers (the "silent stuck" case for hung detection).

#### R3. Claude productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/claude-productive-marker.raw" claude

# 1. Ask Claude to create a small file:
#    "Create a file called hello.txt with the content 'hello world'"
# 2. Wait for the tool use to complete and the file-write confirmation
#    to appear on screen.
# 3. Let the response finish fully.
# 4. Type /exit
```

#### R4. Claude productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/claude-productive-silence.raw" claude

# 1. Ask a complex question that requires long thinking:
#    "Explain the mathematical proof of Fermat's Last Theorem in detail"
# 2. Wait 30-60 seconds while Claude is thinking/streaming (spinner
#    visible, no tool-use markers).
# 3. Press Ctrl-C mid-response to capture the "stuck" state.
```

#### R5. Codex productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/codex-productive-marker.raw" codex

# 1. Ask Codex to create a file.
# 2. Wait for the apply-patch confirmation and completion.
# 3. Type /exit
```

#### R6. Codex productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/codex-productive-silence.raw" codex

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking/streaming.
# 3. Press Ctrl-C mid-response.
```

#### R7. Gemini productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/gemini-productive-marker.raw" gemini

# 1. Ask Gemini to create a file.
# 2. Wait for the MCP-allow prompt and tool completion.
# 3. Type /exit
```

#### R8. Gemini productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/gemini-productive-silence.raw" gemini

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking.
# 3. Press Ctrl-C mid-response.
```

#### R9. Kiro productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/kiro-productive-marker.raw" kiro

# 1. Ask Kiro to create a file.
# 2. Wait for the file-write tool completion marker.
# 3. Type /exit
```

#### R10. Kiro productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/kiro-productive-silence.raw" kiro

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking.
# 3. Press Ctrl-C mid-response.
```

#### R11. OpenCode productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/opencode-productive-marker.raw" opencode

# 1. Ask OpenCode to create a file.
# 2. Wait for the tool completion.
# 3. Type /exit
```

#### R12. OpenCode productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/opencode-productive-silence.raw" opencode

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking.
# 3. Press Ctrl-C mid-response.
```

---

## Post-Capture Workflow

### 1. Sanitize

Review each `.raw` file for sensitive content before committing:

```bash
# Check for API keys, tokens, personal paths
for f in "$CAPTURE_DIR"/*.raw; do
  echo "=== $(basename $f) ==="
  strings "$f" | grep -iE 'api.key|token|secret|password|/Users/[^/]+' | head -5
done
```

If sensitive content is found, re-capture in a clean environment or
use `sed` to replace specific strings (preserving byte offsets is
critical -- only replace content that doesn't affect ANSI escape
positioning).

### 2. Copy to fixture directory

```bash
cp "$CAPTURE_DIR"/*.raw tests/fixtures/state-replay/
```

### 3. Add MANIFEST.yaml entries

For each new fixture, add an entry to `tests/fixtures/state-replay/MANIFEST.yaml`
using the template from the setup checklist. Fields to fill:

- `file`: filename (e.g. `claude-yes-proceed.raw`)
- `backend`: backend identifier (`claude-code`, `codex`, `gemini`, `kiro-cli`, `opencode`, `agy`)
- `cli_version`: exact version string from `<backend> --version`
- `recorded_on`: today's date in YYYY-MM-DD
- `scenario`: one-line description of what was captured
- `expected_transitions`: leave as `[starting]` initially; fill after replay test
- `capture_kind`: `real_pty`
- `provenance`: reference the batch capture session and issue number

### 4. Verify with replay harness

```bash
cargo test --test state_replay -- --nocapture
```

If a fixture doesn't match expected transitions, update the MANIFEST
entry -- the fixture is ground truth, not the expectation.

### 5. PR shape

- Branch: `fixtures/<batch-description>`
- Files: `.raw` fixtures + MANIFEST.yaml updates only (no src changes)
- Title: `fixtures: batch capture for #<issue> (<backend list>)`
- Cross-ref: link to the issue(s) this batch unblocks

---

## Time Estimates

| Recipe | Time |
|--------|------|
| R1. claude-yes-proceed | ~3 min |
| R2. claude-trust-prompt | ~2 min |
| R3-R12. productive markers (5 backends x 2) | ~50 min |
| Post-capture sanitize + MANIFEST | ~15 min |
| **Total session** | **~70 min** |

Tip: batch the productive captures per-backend to minimize context
switching. Do all claude captures, then all codex, etc.

---

## Reference

- S2 memo capture protocols: `/tmp/dialectic-996-s2-signatures-dev.md` sections 2.1-2.4
- MANIFEST.yaml recording protocol: header comment in `tests/fixtures/state-replay/MANIFEST.yaml`
- Fixture corpus measurement: `tests/fixture_corpus_measurement.rs`
- Existing real-PTY fixtures: `codex-update.raw` (2026-04-20), `kiro-tooluse.raw` (2026-04-20), `agy-thinking.raw` (2026-05-20)
