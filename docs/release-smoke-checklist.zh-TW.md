[English](release-smoke-checklist.md)

# Release Smoke-Test Checklist — 發版前冒煙測試檢查清單

發版前先複製這個檔案，逐項打勾，最後把底部的 sign-off 區塊貼上。  
**整體掛鐘時間目標：≤ 30 分鐘。**

---

## 0. 起飛前檢查（≤ 3 分鐘）

- [ ] 沒有上一個 session 殘留的 daemon 在跑：`agend stop`（或確認 `agend list` 沒有任何輸出）
- [ ] 工作目錄是 repo 根目錄
- [ ] build 是最新的：`cargo build --release`（或 CI artifact 對應到目標 commit）
- [ ] `AGEND_HOME` 解析正確：`agend doctor` 以 0 結束
- [ ] 若要測試 Telegram channel：`AGEND_BOT_TOKEN` 已設定且 bot 在線上
- [ ] 每個要測試的 backend 都已備妥 auth 憑證（API key／本機安裝）

---

## 1. 各 backend 冒煙測試（每個 ≤ 5 分鐘）

每個 backend 跑一個區塊。如果該 binary 未安裝就跳過該區塊，並在 sign-off 中註記。

### 1a. Claude Code（`claude`）

- [ ] **Spawn** — `agend start --agents claude:claude` — 就緒提示字元（`❯` 或 `bypass permissions`）在 **30 秒** 內出現
- [ ] **Echo** — 注入 `echo hello` + Enter；回應出現在 vterm pane 中
- [ ] **Tool use** — 注入 `list files in /tmp`；確認觸發了 tool-call 行為（看得到檔案列表輸出）
- [ ] **Quit** — 注入 `/exit` + Enter；pane 在 5 秒內關閉；`ps aux | grep 'claude'` 沒有殘留的孤兒 `claude` process
- [ ] **Worktree** — `agend admin cleanup-branches --dry-run` 以 0 結束（git wrapper 路徑沒有 crash）

### 1b. Kiro CLI（`kiro-cli`）

- [ ] **Spawn** — 就緒提示字元（`Trust All Tools active`／`ask a question`）在 **30 秒** 內出現；trust 對話框被自動關閉
- [ ] **Echo** — 注入 `echo hello` + Enter；看得到回應
- [ ] **Tool use** — 注入 `list files in /tmp`；觸發 tool 行為
- [ ] **Quit** — 注入 `/quit` + Enter；pane 關閉；沒有殘留的孤兒 `kiro-cli` process

### 1c. Codex（`codex`）

- [ ] **Spawn** — 就緒提示字元（`OpenAI Codex`／`›`）在 **20 秒** 內出現；trust-directory 對話框被自動關閉
- [ ] **Echo** — 注入 `echo hello` + Enter；看得到回應
- [ ] **Tool use** — 注入 `list files in /tmp`；觸發 tool 行為
- [ ] **Quit** — 注入 `exit` + Enter；pane 關閉；沒有殘留的孤兒 `codex` process

### 1d. OpenCode（`opencode`）

- [ ] **Spawn** — 就緒提示字元（`Ask anything`／`tab agents`）在 **45 秒** 內出現；update 對話框被自動關閉
- [ ] **Echo** — 注入 `echo hello` + Enter；看得到回應
- [ ] **Tool use** — 注入 `list files in /tmp`；觸發 tool 行為
- [ ] **Quit** — 注入 `/exit` + Enter；pane 關閉；沒有殘留的孤兒 `opencode` process
- [ ] **滑鼠滾輪回歸測試（#744）** — 當 pane 處於 alt-screen 模式時，在 opencode pane *內部* 滾動滑鼠滾輪；該 pane 不應該捲動（SGR-forwarded 的滾輪事件會送到 backend，而不是外層的 TUI 捲動器）

### 1e. Gemini（`gemini`）

- [ ] **Spawn** — 就緒提示字元（`Type your message`／`YOLO`）在 **20 秒** 內出現；MCP／shell-trust 對話框被自動關閉
- [ ] **Echo** — 注入 `echo hello` + Enter；看得到回應
- [ ] **Tool use** — 注入 `list files in /tmp`；觸發 tool 行為
- [ ] **Quit** — 注入 `/exit` + Enter；pane 關閉；沒有殘留的孤兒 `gemini` process

### 1f. Antigravity CLI（`agy`，註冊為 backend `antigravity-cli`）— #987

Gemini CLI 的官方接班人（Gemini CLI 將於 2026-06-18 對 free／Pro／Ultra 方案停止服務）。binary 命令是 `agy`；fleet 中的顯示名稱是 `antigravity-cli`（#995）。

- [ ] **Spawn** — `agend start --agents agy-smoke:agy` — 就緒提示字元（`Antigravity CLI` banner 或 `Type your message`）在 **20 秒** 內出現；workspace-trust 對話框被自動關閉（#995 dismiss_pattern）
- [ ] **Echo** — 注入 `echo hello` + Enter；看得到回應
- [ ] **Tool use** — 注入 `list files in /tmp`；觸發 tool 行為
- [ ] **Quit** — 注入 `/exit` + Enter；pane 關閉；沒有殘留的孤兒 `agy` process
- [ ] **Fleet MCP 載入（#1547）** — daemon 為 agy workspace 建立一個非隱藏的連結（`<base>/<instance>` → 隱藏的真實 workspace），並以 `$PWD` 指向它的方式 spawn agy，且 `configure_agy` 寫入 `.agents/mcp_config.json` + `.agents/AGENTS.md`。確認：沒有 `[fleet-mcp-unsupported]` 警告；`app.log` 顯示 `$PWD` 連結那一行；spawn 出來的 agy 有 `send`／`inbox`／`task` MCP 工具（例如注入「list your fleet via `list_instances`」並確認它回傳整個 fleet）。

---

## 2. 跨功能測試（≤ 5 分鐘）

- [ ] **鍵盤導覽** — `Ctrl+B n`／`Ctrl+B p` 在 pane 之間循環；`Ctrl+B d` 乾淨地 detach
- [ ] **滑鼠滾輪捲動** — 在一般（非 alt-screen）pane 中，滑鼠滾輪能捲動 vterm 歷史
- [ ] **Telegram channel 綁定** — `agend start`；透過 Telegram 送一則訊息；daemon 把它路由到正確的 agent pane（需要 `AGEND_BOT_TOKEN`）
- [ ] **Worktree lease／release** — `agend repo checkout`；`agend repo release`；`git worktree list` 中沒有殘留的 worktree 項目
- [ ] **被動擷取 opt-in** — 設定 `AGEND_CAPTURE_FIXTURES=1`，跑一個 backend 冒煙測試區塊，確認 `~/.agend-terminal/captures/<agent>/` 內含一對 `.cap` 與 `.cap.meta.json`，然後 `unset AGEND_CAPTURE_FIXTURES`

---

## 3. Sign-off

填好後與這份檢查清單一起 commit，或貼到 release PR 中。

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
- agy:        <version>   # registered as backend `antigravity-cli` per #995

Backends skipped (reason):
-

Known deviations / new failures observed:
-

Overall verdict: [ ] PASS  [ ] PASS with caveats  [ ] FAIL
```

當所有 backend 都通過時，release PR 內文還應該附上這一行確認：

```
Real-backend smoke: ✓ all 6 backends
```