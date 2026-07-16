[繁體中文](release-smoke-checklist.zh-TW.md)

# Release Smoke-Test Checklist

Copy this file before a release, check each box, and paste the sign-off block at the bottom.  
**Total wall-clock target: ≤ 30 minutes.**

---

## 0. Pre-flight (≤ 3 min)

- [ ] No running daemon from a previous session: `agend-terminal stop` (or confirm `agend-terminal list` returns nothing)
- [ ] Working directory is the repo root
- [ ] Build is current: `cargo build --release` (or CI artifact matches the target commit)
- [ ] `AGEND_HOME` resolves correctly: `agend-terminal doctor` exits 0
- [ ] If testing Telegram channel: `AGEND_TELEGRAM_BOT_TOKEN` is set and bot is online
- [ ] Auth credentials in place for each backend under test (API keys / local installs)

---

## 1. Per-backend smoke (≤ 5 min each)

Run one block per backend. Skip a block if the binary is not installed; note it in sign-off.

### 1a. Claude Code (`claude`)

- [ ] **Spawn** — `agend-terminal start --agents claude:claude` — ready prompt (`❯` or `bypass permissions`) appears within **30 s**
- [ ] **Echo** — inject `echo hello` + Enter; response surfaces in the vterm pane
- [ ] **Tool use** — inject `list files in /tmp`; verify a tool-call affordance fires (file-list output visible)
- [ ] **Quit** — inject `/exit` + Enter; pane closes within 5 s; `ps aux | grep 'claude'` shows no orphan `claude` process
- [ ] **Branch preview** — `agend-terminal admin cleanup-branches` exits 0 (preview is the default; no branch is deleted)

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

### 1e. Antigravity CLI (`agy`, registered as backend `antigravity-cli`) — #987

Gemini CLI's official successor (Gemini CLI sunsets 2026-06-18 for free / Pro / Ultra tiers). The binary command is `agy`; the fleet display name is `antigravity-cli` (#995).

- [ ] **Spawn** — `agend-terminal start --agents agy-smoke:agy` — ready prompt (`Antigravity CLI` banner or `Type your message`) appears within **20 s**; workspace-trust dialog auto-dismissed (#995 dismiss_pattern)
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `/exit` + Enter; pane closes; no orphan `agy` process
- [ ] **Fleet MCP loads (#1547)** — the daemon creates a non-hidden link for the agy workspace (`<base>/<instance>` → the hidden real workspace) and spawns agy with `$PWD` pointed at it, and `configure_agy` writes `.agents/mcp_config.json` + `.agents/AGENTS.md`. Confirm: no `[fleet-mcp-unsupported]` warning; `app.log` shows the `$PWD` link line; the spawned agy has the `send` / `inbox` / `task` MCP tools (e.g. inject "list your fleet via `list_instances`" and verify it returns the fleet).

### 1f. Grok Build (`grok`)

- [ ] **Spawn** — `agend-terminal start --agents grok-smoke:grok` — `Grok Build` / `❯` appears within **30 s**; project-trust dialog auto-dismissed
- [ ] **Echo** — inject `echo hello` + Enter; response visible
- [ ] **Tool use** — inject `list files in /tmp`; tool affordance fires
- [ ] **Quit** — inject `/exit` + Enter; pane closes; no orphan `grok` process

---

## 2. Cross-cutting (≤ 5 min)

- [ ] **Keyboard navigation** — `Ctrl+B n` / `Ctrl+B p` cycles tabs; `Ctrl+B o` cycles panes; `Ctrl+B d` detaches cleanly
- [ ] **Mouse wheel scroll** — in a standard (non-alt-screen) pane, mouse wheel scrolls the vterm history
- [ ] **Telegram channel binding** — `agend-terminal start`; send a message via Telegram; daemon routes it to the correct agent pane (requires `AGEND_TELEGRAM_BOT_TOKEN`)
- [ ] **Worktree lease / release** — from an agent, call MCP `repo(action=checkout, bind=true)` for a disposable branch, then `release_worktree`; `binding_state` returns `bound:false` and no dangling worktree remains
- [ ] **Passive capture opt-in** — set `AGEND_CAPTURE_FIXTURES=1`, run one backend smoke block, verify `$AGEND_HOME/captures/<agent>/` contains a `.cap` and `.cap.meta.json` pair, then `unset AGEND_CAPTURE_FIXTURES`

---

## 3. Sign-off

Fill in and commit alongside this checklist or paste into the release PR.

```
Date: YYYY-MM-DD
Operator: <name>
agend-terminal version: $(agend-terminal --version)
OS / arch: $(uname -srm)

Backends tested (paste `<backend> --version` output for each):
- claude:     <version>
- kiro-cli:   <version>
- codex:      <version>
- opencode:   <version>
- agy:        <version>   # registered as backend `antigravity-cli` per #995
- grok:       <version>

Backends skipped (reason):
-

Known deviations / new failures observed:
-

Overall verdict: [ ] PASS  [ ] PASS with caveats  [ ] FAIL
```

When all backends pass, the release PR body should also include the one-line confirmation:

```
Real-backend smoke: ✓ all 6 backends
```
