[English](USAGE.md)

# Usage Guide — 使用指南

## Binaries

| Binary | 用途 |
|---|---|
| `agend-terminal` | 主程式——所有功能都從這裡進入 |
| `agend-git` | 舊版 kill-only 相容 helper；git interception 已由 vendored `agentic-git` shim 負責 |
| `agend-mcp-bridge` | MCP stdio ↔ daemon bridge；由 AI backend 為每個 agent 啟動 |

## Startup Modes

### `agend-terminal app` — 互動式 TUI 工作台

```bash
agend-terminal app [--fleet fleet.yaml]
```

以 ratatui 打造的完整多分頁／多 pane 終端 UI。在本機管理 agent：spawn、kill、respawn、在分頁之間拖放 pane。

如果已有 daemon 在運行，就連接到它（attached 模式）。否則啟動自己的本機 fleet（owned 模式）。

**何時使用：** 日常開發。互動式的多 agent 工作。

### `agend-terminal start` — 無介面 daemon

```bash
agend-terminal start [--fleet fleet.yaml] [--foreground]
```

沒有 TUI 的背景服務。讀取 fleet.yaml、管理 agent、自動 respawn 已 crash 的 agent，並執行 scheduler 與 CI watch。

預設即以 detached service mode 執行。使用 `--foreground` 可讓 stdio 保持連接並阻塞呼叫端 shell，適合除錯或交由 OS service manager 管理。

**何時使用：** 伺服器部署。CI/CD。無人值守的 fleet 運作。

### `agend-terminal attach <name>` — 輕量終端 client

```bash
agend-terminal attach at-dev-2
```

透過 daemon 的 API socket 連接到單一 agent 的 PTY 的極簡 raw-mode 終端。沒有 pane、沒有分頁——只有一個 agent 的終端串流。

用 `Ctrl+B d` 卸離（detach）。

**何時使用：** SSH 進遠端機器檢視某一個 agent。輕量除錯。在不開啟完整 TUI 的情況下閱讀 agent 輸出。

### `agend-terminal tray` — 系統列常駐程式

```bash
agend-terminal tray   # requires: cargo build --features tray
```

選單列圖示（macOS／Linux）。以顏色標示 daemon 狀態：灰色＝離線、琥珀色＝idle、綠色＝活躍。

若 daemon 未運行會自動啟動。點選「Open App」即可開啟完整 TUI。

**何時使用：** 背景監控。開機自動啟動。不必一直開著終端就能快速存取。

### `agend-mcp-bridge` — MCP 伺服器（供 AI backend 使用）

```bash
agend-mcp-bridge
```

Stdio JSON-RPC 2.0 伺服器，提供 32 個工具（任務管理、決策、訊息傳遞、CI watch 等）。並非設計來手動執行。

每個支援的 AI backend（Claude Code、Kiro、Codex、OpenCode、Antigravity 與 Grok）會依其原生 MCP 設定，自動以子程序啟動它；daemon 會寫入對應 backend 格式的 bridge 設定。

**何時使用：** 你不會直接執行它。當 AI agent 需要與 daemon 溝通時，它會自動被啟動。

### Daemon supervision 與 restart

沒有獨立的 `agend-supervisor` binary。daemon 內建 supervisor，負責 auto-respawn、健康監控與 hung detection。`restart_daemon` MCP 工具會依模式執行受控重啟；透過 `agend-terminal service install` 安裝後，也可由 systemd／launchd／Task Scheduler 管理 crash 與開機重啟。

**何時使用：** 要讓 OS 管理 daemon 時執行 `agend-terminal service install`；更新 binary 後需要 reload 時呼叫 `restart_daemon`。

## Architecture

```
agend-terminal start（headless daemon；內建 supervisor）
  └── （選用）OS service manager／restart_daemon
        ├── Agent PTYs (managed by daemon)
        ├── MCP servers (one per agent, started by AI backends)
        ├── Telegram polling
        ├── Scheduler (cron + one-shot)
        └── API socket
              └── agend-terminal attach <name> (thin clients connect here)

agend-terminal app (standalone TUI)
  ├── Daemon running → attached mode (connects to existing daemon)
  └── No daemon → owned mode (manages its own local fleet)

agend-terminal tray (menu-bar resident)
  └── Auto-starts daemon → click "Open App" → launches TUI
```

## Channel: Telegram

將 fleet 綁定到一個 Telegram 群組，以進行遠端控制（從手機向 agent 發送訊息）以及對外通知（將停滯／crash／CI 警示推送回群組）。

### 最小設定

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_TELEGRAM_BOT_TOKEN # env var holding the bot token
  group_id: -1001234567890                # Telegram chat id of the group
  user_allowlist: [123456789]             # operator Telegram user_id(s)
```

接著在 `agend-terminal start` 之前 export bot token：

```bash
export AGEND_TELEGRAM_BOT_TOKEN="123456:abcdef..."
```

### 如何取得這些值

- **Bot token**（`AGEND_TELEGRAM_BOT_TOKEN`）：透過 [@BotFather](https://t.me/BotFather) 建立一個 bot，複製它回傳的 token。
- **Group id**：把你的 bot 加入目標群組，然後發送任意訊息並檢查 bot 的 `getUpdates` API（`https://api.telegram.org/bot<TOKEN>/getUpdates`）——其中的 `chat.id` 就是你的 `group_id`（群組／超級群組為負數）。
- **User id**：在 Telegram 上私訊 [@userinfobot](https://t.me/userinfobot)，它會回覆你的數字 user id。把每一位應被允許指揮 fleet 的 operator 都加進去。

### `user_allowlist` 語意（Sprint 21 fail-closed 預設）

| `user_allowlist` 值 | Inbound（寄件者過濾） | Outbound（通知 gate） |
|---|---|---|
| `[123, 456]`（≥ 1 筆） | 僅限列出的使用者——其他人遭拒 | ✅ 通知會送達 |
| `[]`（空列表） | 所有人遭拒 | 🔇 通知被丟棄（fail-closed） |
| 欄位缺漏／`null` | 所有人遭拒 | 🔇 通知被丟棄（fail-closed） |

allowlist 缺漏或為空時，inbound 指令與 outbound 通知都會 fail-closed；不存在 legacy accept-all fallback。

### 遷移：從 < Sprint 21 升級

若 Telegram channel 沒有非空的 `user_allowlist`，daemon 會同時停用 inbound 指令與 outbound 通知。加入 operator 的 Telegram user ID 即可恢復兩個方向；bot token 與 group ID 不需變更。

舊的 per-instance `outbound_capabilities` layer 已移除。新設定不應加入此欄位；channel authorization 由 channel allowlist 與 operator authority gate 負責。

## Other Commands

| 指令 | 用途 |
|---|---|
| `start --agents <name:cmd>...` | 以明確的 agent spec 啟動 daemon（不用 fleet.yaml） |
| `list` / `ls` | 列出運行中的 agent |
| `status` | 詳細的 agent 狀態（state、health） |
| `inject <name> <text>` | 將文字注入 agent 的 PTY |
| `kill <name>` | kill 特定 agent |
| `connect <name>` | 在暫時 daemon registration 下執行 backend |
| `stop` | 停止 daemon |
| `quickstart` | 互動式設定（偵測 backend、設定 Telegram、產生 fleet.yaml） |
| `doctor` | 健康檢查（驗證安裝、backend、連線） |
| `bugreport` | 產生含 log 與設定的診斷報告 |
| `verify [--quick]` | 完整 E2E 驗證；`--quick` 執行快速 probe |
| `mode <active|away|sleep>` | 設定 operator availability 與 delegation |
| `service <install|uninstall|status>` | 管理 OS service |
| `skills <action>` | 管理共用 backend skills |
| `capture backend|promote` | 擷取 backend 輸出或提升 fixture |
| `verify-push` | 以 diff 驗證 semantic push claim |
| `completions <shell>` | 產生 shell 自動補全（bash、zsh、fish、powershell） |
| `admin cleanup-branches [--yes]` | 刪除 PR 已合併的本機分支（預設為 dry-run） |
| `admin cleanup-zombies [--age <D>] [--yes]` | kill 掉持有過期 `run/<pid>/` 的 zombie daemon（#927；預設 `--age 14d`） |

## TUI Keyboard Shortcuts

所有快捷鍵都以 `Ctrl+B` 作為前綴鍵（類似 tmux）。

### 分頁管理

| 快捷鍵 | 動作 |
|---|---|
| `Ctrl+B c` | 新分頁（開啟選單） |
| `Ctrl+B n` / `Ctrl+B p` | 下一個／上一個分頁 |
| `Ctrl+B 0-9` | 依編號切換分頁 |
| `Ctrl+B l` | 最近使用的分頁 |
| `Ctrl+B w` | 列出所有分頁 |

### Pane 管理

| 快捷鍵 | 動作 |
|---|---|
| `Ctrl+B %` | 垂直分割（左右） |
| `Ctrl+B "` | 水平分割（上下） |
| `Ctrl+B arrows` | 聚焦 pane（可重複） |
| `Ctrl+B o` | 循環聚焦（可重複） |
| `Ctrl+B z` | 放大／還原 pane |
| `Ctrl+B x` | 關閉 pane |
| `Ctrl+B &` | 關閉分頁 |

### 捲動

| 快捷鍵 | 動作 |
|---|---|
| 滑鼠滾輪 | 捲動聚焦中的 pane |
| `Ctrl+B [` | 捲動模式（用 Esc 離開） |
| `Ctrl+B PageUp/Down` | 整頁捲動 |

### 其他

| 快捷鍵 | 動作 |
|---|---|
| `Ctrl+B ~` | 暫存 shell 浮層 |
| `Ctrl+B :` | 命令選盤 |
| `Ctrl+B ?` | 顯示鍵位說明 |
| `Ctrl+B d` | 卸離（離開 TUI，daemon 繼續運行） |
| `Ctrl+B t` / `Ctrl+B s` / `Ctrl+B m` / `Ctrl+B f` | 開啟 task board 的 Tasks／Status／Monitor／Fleet view |
| `Ctrl+B D` | 開啟 pending-decisions board |
| `Shift+Enter` | 換行但不送出（需要終端支援鍵盤增強功能） |
| `Alt+Enter` | 換行但不送出（與 Shift+Enter 相同） |

### 滑鼠

- **點選分頁**——切換到該分頁
- **拖曳分頁**——重新排序分頁
- **點選 pane 標籤**——聚焦 pane
- **拖曳 pane 標籤**——移動 pane（支援跨分頁）
- **滑鼠選取**——在 pane 中選取文字
