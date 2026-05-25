# Agent Interaction — 與 Agent 的終端互動

## 設計初衷

AgEnD 的 agent 運行在獨立的虛擬終端（PTY）中。雖然大部分操作透過 MCP 工具
和 Telegram 完成，但有些時候你需要直接看到 agent 的終端畫面——觀察它在想
什麼、它卡在哪裡、或是手動輸入一些文字來引導它。

AgEnD 提供四個 CLI 命令來實現這件事：

| 命令 | 用途 |
|------|------|
| `attach` | 連接到 agent 的終端，像 tmux attach 一樣 |
| `inject` | 送一段文字到 agent 的輸入 |
| `list` | 列出所有正在執行的 agent |
| `kill` | 終止一個 agent |

---

## attach — 連接到 Agent 的終端

```
agend-terminal attach <agent-name>
```

`attach` 將你的終端連接到指定 agent 的 PTY。連接後你看到的就是 agent 的
即時輸出，你打的字就是 agent 的輸入。體驗上跟 `tmux attach-session` 或
`screen -r` 幾乎一樣。

### 使用方式

```bash
# 連接到名為 dev 的 agent
agend-terminal attach dev

# 如果不指定名稱，預設連接 "shell"
agend-terminal attach
```

### 分離（Detach）

在 attach 狀態下，按 **Ctrl+B** 然後按 **d** 即可分離（detach）。

這個按鍵組合與 tmux 的預設 prefix 相同：先按 Ctrl+B 作為前綴鍵，
放開後再按 d 觸發分離。分離後 agent 繼續在背景執行，你隨時可以
再次 attach。

### 技術原理

1. attach 透過 TCP 連接到 daemon 的 bridge server
2. 使用 API cookie 進行身份驗證
3. 將本機終端切換到 raw mode（直接轉發所有按鍵）
4. 啟動一個背景執行緒讀取 agent 的 PTY 輸出並顯示
5. 主執行緒讀取本機鍵盤輸入並轉發給 agent
6. 自動處理終端大小變更（resize event）

結束連接時（Ctrl+B d 或 agent 停止），自動恢復終端狀態。

### 注意事項

- 同一時間可以有多個 attach 連接到同一個 agent（但輸入會互相競爭）
- attach 不會影響 agent 的執行狀態——它只是觀察和輸入的通道
- 如果 agent 正在等待互動式提示（如權限確認），你可以透過 attach 直接回應

---

## inject — 送文字到 Agent

```
agend-terminal inject <agent-name> <text...>
```

`inject` 將指定的文字送到 agent 的 PTY 輸入，就像你在 agent 的終端裡
打字一樣。適合用來自動化操作或從腳本中控制 agent。

### 使用方式

```bash
# 送一段文字給 dev agent
agend-terminal inject dev "請幫我 review 這個 PR"

# 送多個字詞（會以空格連接）
agend-terminal inject dev fix the bug in main.rs
```

### 注入機制

inject 有兩種模式：

**一般模式（預設）：**
1. 將文字寫入 agent 的 PTY
2. 等待 50ms
3. 送出提交鍵（通常是 Enter）

**Typed inject 模式：**
- 系統訊息（如 `[AGEND-MSG]` 前綴）使用此模式
- 將文字拆分成 64 bytes 的 chunk，每個 byte 之間間隔 2ms
- 模擬真人打字速度，避免 backend 的輸入 buffer 溢出

### 安全處理

inject 會自動移除文字中的 ANSI 控制序列，避免 ESC 字元干擾 agent 的
終端狀態。

### 回傳值

成功時回傳注入的 byte 數：

```json
{"ok": true, "result": {"bytes": 42}}
```

如果 agent 不存在或正在重啟中，回傳錯誤。

---

## list — 列出所有 Agent

```
agend-terminal list [--detailed] [--json]
```

列出 daemon 中所有正在執行的 agent。

### 輸出模式

**簡易模式（預設）：**

```
$ agend-terminal list
lead
dev
reviewer
```

簡易模式直接讀取 run directory 中的 port 檔案，即使 daemon API 暫時
無回應也能顯示。

**詳細模式（`--detailed` 或 `-d`）：**

```
$ agend-terminal list --detailed
NAME       BACKEND     STATE    HEALTH
lead       claude      ready    healthy
dev        claude      thinking healthy
reviewer   kiro-cli    idle     healthy
```

詳細模式透過 daemon API 查詢，顯示每個 agent 的即時狀態。

**JSON 模式（`--json`）：**

```bash
$ agend-terminal list --json
```

輸出完整的 JSON 結構，適合用於腳本或自動化工具。`--json` 隱含
`--detailed`。

### Agent 狀態欄位

| 欄位 | 說明 |
|------|------|
| `agent_state` | 即時狀態：`starting` / `ready` / `idle` / `thinking` / `tool_use` / `restarting` / `crashed` |
| `health_state` | 健康狀態：`healthy` / `recovering` / `unstable` / `failed` / `hung` / `idle_long` / `paused` |
| `backend` | Backend 名稱 |
| `kind` | `managed`（daemon 管理）或 `external`（外部連接） |

### 別名

`list` 有兩個別名：

```bash
agend-terminal ls       # 等同 list
agend-terminal status   # 等同 list（向後相容）
```

---

## kill — 終止 Agent

```
agend-terminal kill <agent-name>
```

強制終止指定的 agent 程序。

### 使用方式

```bash
# 終止 dev agent
agend-terminal kill dev
```

### 終止流程

1. 驗證 agent 名稱格式（`[a-zA-Z0-9_-]`）
2. 在 registry 中查找 agent
3. 將 agent 狀態標記為 `restarting`
4. 取得子程序的 PID
5. 呼叫 `kill_process_tree(pid)` 終止整個程序樹（包含子程序）
6. 作為備用措施，同時呼叫 PTY handle 的 `kill()` 方法
7. 記錄事件到 event log

### 自動重啟

`kill` 之後，daemon 的健康監控系統可能會自動重啟 agent（取決於
目前的 crash 計數和退避狀態）。如果你想永久停止一個 agent，需要
從 fleet.yaml 中移除它然後重啟 daemon，或使用 `agend-terminal stop`
停止整個 daemon。

### 外部 Agent

對於透過 `connect` 命令加入的外部 agent，`kill` 會將它從 external
registry 中移除。外部 agent 的實際程序不由 daemon 管理。

---

## connect — 連接外部 Agent

```
agend-terminal connect <name> --backend <backend> [--working-dir <path>] [-- extra-args...]
```

將一個本地運行的 agent 連接到 daemon，讓它可以使用 daemon 提供的
MCP 工具和通訊功能。

### 使用方式

```bash
# 連接一個 Claude Code instance
agend-terminal connect my-agent --backend claude --working-dir ~/Projects/foo

# 傳遞額外參數給 backend
agend-terminal connect my-agent --backend gemini -- --model pro
```

### 與 fleet.yaml 的區別

- fleet.yaml 中的 agent 是 **managed**（由 daemon spawn 和管理生命週期）
- `connect` 加入的是 **external**（daemon 只提供工具，不管理生命週期）
- external agent 沒有自動重啟、健康監控等功能

---

## 典型工作流

### 場景 1：觀察 Agent 工作

```bash
# 啟動 daemon
agend-terminal start

# 查看所有 agent 狀態
agend-terminal list --detailed

# 連接到 dev agent 觀察它在做什麼
agend-terminal attach dev

# 看完了，分離回到自己的終端
# (按 Ctrl+B, 然後按 d)
```

### 場景 2：手動引導 Agent

```bash
# 連接到 agent
agend-terminal attach lead

# 在 agent 的終端中直接打字互動
# ...觀察 agent 的回應...

# 分離
# (Ctrl+B d)
```

### 場景 3：從腳本自動化

```bash
#!/bin/bash

# 確認 agent 在運行
agend-terminal list --json | jq '.agents[] | select(.name == "dev")'

# 送指令給 agent
agend-terminal inject dev "請執行 cargo test 並回報結果"

# 等待一段時間後檢查狀態
sleep 30
agend-terminal list --detailed
```

### 場景 4：處理卡住的 Agent

```bash
# 查看哪個 agent 卡住了
agend-terminal list --detailed
# 如果看到 health_state: hung

# 嘗試 attach 看看它卡在哪裡
agend-terminal attach dev

# 如果需要強制重啟
agend-terminal kill dev
# daemon 會自動重啟 agent
```

---

## 常見問題

### Q: attach 後看不到任何輸出？

可能的原因：
- Agent 正在 idle 狀態，等待輸入——嘗試打字看看
- Agent 的 PTY 輸出被 backend 的 TUI 吃掉——某些 backend 使用 alternate
  screen buffer，attach 可能看到的是上次的畫面

### Q: inject 的文字沒有被 agent 執行？

確認 agent 的狀態是 `ready` 或 `idle`。如果 agent 正在 `thinking` 或
`tool_use` 狀態，inject 的文字會進入 PTY buffer 但 agent 可能不會
立即處理。

### Q: kill 之後 agent 自動重啟了，怎麼阻止？

daemon 的健康監控預設會自動重啟 crash 的 agent。如果要永久停用一個
agent，從 fleet.yaml 中移除它並重啟 daemon。或者直接 `agend-terminal stop`
停止整個 daemon。

### Q: 可以同時 attach 多個 agent 嗎？

每個 `agend-terminal attach` 指令佔用一個終端。如果你想同時看多個
agent，可以開多個終端各自 attach。或者使用 `agend-terminal app` 啟動
TUI 多面板介面，可以在一個畫面中看到所有 agent。
