# Release Smoke-Test Checklist

Copy this file before a release, check each box, and paste the sign-off block at the bottom.  
**Total wall-clock target: ≤ 30 minutes.**

---

## 0. Pre-flight (≤ 3 min)

- [ ] No running daemon from a previous session: `agend stop` (or confirm `agend list` returns nothing)
- [ ] Working directory is the repo root
- [ ] Build is current: `cargo build --release` (or CI artifact matches the target commit)
- [ ] `AGEND_HOME` resolves correctly: `agend doctor` exits 0
- [ ] If testing Telegram channel: `AGEND_BOT_TOKEN` is set and bot is online
- [ ] Auth credentials in place for each backend under test (API keys / local installs)

---

## 1. Per-backend smoke (≤ 5 min each)

Run one block per backend. Skip a block if the binary is not installed; note it in sign-off.

### 1a. Claude Code (`claude`)

- [ ] **Spawn** — `agend start --agents claude:claude` — ready prompt (`❯` or `bypass permissions`) appears within **30 s**
- [ ] **Echo** — inject `echo hello` + Enter; response surfaces in the vterm pane
- [ ] **Tool use** — inject `list files in /tmp`; verify a tool-call affordance fires (file-list output visible)
- [ ] **Quit** — inject `/exit` + Enter; pane closes within 5 s; `ps aux | grep 'claude'` shows no orphan `claude` process
- [ ] **Worktree** — `agend admin cleanup-branches --dry-run` exits 0 (no crash in git wrapper path)

### 1b. Kiro CLI (`kiro-cli`)

- [ ] **Spawn** — ready prompt (`Trust All Tools active` / `ask a question`) appears within **30 s**; trust-dialog auto-dismissed
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `/quit` + Enter; pane closes; no orphan `kiro-cli` process

### 1c. Codex (`codex`)

- [ ] **Spawn** — ready prompt (`OpenAI Codex` / `›`) appears within **20 s**; trust-directory dialog auto-dismissed
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `exit` + Enter; pane closes; no orphan `codex` process

### 1d. OpenCode (`opencode`)

- [ ] **Spawn** — ready prompt (`Ask anything` / `tab agents`) appears within **45 s**; update dialogs auto-dismissed
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `/exit` + Enter; pane closes; no orphan `opencode` process
- [ ] **Mouse wheel regression (#744)** — while the pane is in alt-screen mode, scroll the mouse wheel *inside* the opencode pane; the pane must NOT scroll (SGR-forwarded wheel events go to the backend, not the outer TUI scroller)

### 1e. Gemini (`gemini`)

- [ ] **Spawn** — ready prompt (`Type your message` / `YOLO`) appears within **20 s**; MCP/shell-trust dialogs auto-dismissed
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `/exit` + Enter; pane closes; no orphan `gemini` process

---

## 2. Cross-cutting (≤ 5 min)

- [ ] **Keyboard navigation** — `Ctrl+B n` / `Ctrl+B p` cycles panes; `Ctrl+B d` detaches cleanly
- [ ] **Mouse wheel scroll** — in a standard (non-alt-screen) pane, mouse wheel scrolls the vterm history
- [ ] **Telegram channel binding** — `agend start`; send a message via Telegram; daemon routes it to the correct agent pane (requires `AGEND_BOT_TOKEN`)
- [ ] **Worktree lease / release** — `agend repo checkout`; `agend repo release`; no dangling worktree entries in `git worktree list`
- [ ] **Passive capture opt-in** — set `AGEND_CAPTURE_FIXTURES=1`, run one backend smoke block, verify `~/.agend-terminal/captures/<agent>/` contains a `.cap` and `.cap.meta.json` pair, then `unset AGEND_CAPTURE_FIXTURES`

---

## 3. Sign-off

Fill in and commit alongside this checklist or paste into the release PR.

```
Date: YYYY-MM-DD
Operator: <name>
agend-terminal version: $(agend --version)
OS / arch: $(uname -srm)

Backends tested (paste `<backend> --version` output for each):
- claude:     <version>
- kiro-cli:   <version>
- codex:      <version>
- opencode:   <version>
- gemini:     <version>

Backends skipped (reason):
-

Known deviations / new failures observed:
-

Overall verdict: [ ] PASS  [ ] PASS with caveats  [ ] FAIL
```
