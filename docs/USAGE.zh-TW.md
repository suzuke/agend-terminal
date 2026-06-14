[English](USAGE.md)

# Usage Guide — 使用指南

## Binaries

| Binary | 用途 |
|---|---|
| `agend-terminal` | 主程式——所有功能都從這裡進入 |
| `agend-supervisor` | 凍結的 supervisor，負責 daemon 熱升級與 crash 復原（僅限 Unix） |

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
agend-terminal start [--fleet fleet.yaml] [--detached]
```

沒有 TUI 的背景服務。讀取 fleet.yaml、管理 agent、自動 respawn 已 crash 的 agent，並執行 scheduler 與 CI watch。

使用 `--detached` 可 fork 到背景執行。

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

Stdio JSON-RPC 2.0 伺服器，提供 35 種以上的工具（任務管理、決策、訊息傳遞、CI watch 等）。並非設計來手動執行。

每個 AI backend（Claude Code、Kiro、Codex、Gemini、OpenCode）會根據自身的 MCP 設定，自動以子程序的形式啟動它——daemon 在每次啟動時都會把 bridge 路徑寫入每個 backend 的 mcp.json。

**何時使用：** 你不會直接執行它。當 AI agent 需要與 daemon 溝通時，它會自動被啟動。

### `agend-supervisor` — 熱升級 supervisor

```bash
agend-supervisor [--home ~/.agend-terminal]
```

位於 daemon 之上。管理 daemon 的生命週期：啟動、crash 復原，以及零停機的二進位升級。

升級流程：暫存新二進位 → 自我測試 → 切換 → 監控穩定窗口 → commit 或 rollback。

**何時使用：** daemon 必須在不中斷 agent session 的情況下完成二進位升級的正式環境。

## Architecture

```
agend-supervisor (frozen binary, rarely upgraded)
  └── agend-terminal start/daemon (headless, long-running)
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
  bot_token_env: AGEND_BOT_TOKEN          # env var holding the bot token
  group_id: -1001234567890                # Telegram chat id of the group
  user_allowlist: [123456789]             # operator Telegram user_id(s)
```

接著在 `agend-terminal start` 之前 export bot token：

```bash
export AGEND_BOT_TOKEN="123456:abcdef..."
```

### 如何取得這些值

- **Bot token**（`AGEND_BOT_TOKEN`）：透過 [@BotFather](https://t.me/BotFather) 建立一個 bot，複製它回傳的 token。
- **Group id**：把你的 bot 加入目標群組，然後發送任意訊息並檢查 bot 的 `getUpdates` API（`https://api.telegram.org/bot<TOKEN>/getUpdates`）——其中的 `chat.id` 就是你的 `group_id`（群組／超級群組為負數）。
- **User id**：在 Telegram 上私訊 [@userinfobot](https://t.me/userinfobot)，它會回覆你的數字 user id。把每一位應被允許指揮 fleet 的 operator 都加進去。

### `user_allowlist` 語意（Sprint 21 fail-closed 預設）

| `user_allowlist` 值 | Inbound（寄件者過濾） | Outbound（通知 gate） |
|---|---|---|
| `[123, 456]`（≥ 1 筆） | 僅限列出的使用者——其他人遭拒 | ✅ 通知會送達 |
| `[]`（空列表） | 所有人遭拒 | 🔇 通知被丟棄（fail-closed） |
| 欄位缺漏／`null` | 舊行為：所有人皆被接受（已棄用） | 🔇 通知被丟棄（fail-closed） |

對外的 gate 在 [PR #216](https://github.com/suzuke/agend-terminal/pull/216)（Sprint 21 Phase 1）中導入，用來修補 [Sprint 20.5 cross-validation](codebase-review-2026-04-27/SYNTHESIS.md) 發現的對外資訊外洩問題（40 行的 PTY 尾段會洩漏給任何被加入綁定群組的人，無論其 inbound 認證狀態為何）。Inbound 的 fail-closed 將在 Phase 2 導入。

### 遷移：從 < Sprint 21 升級

如果你的 `fleet.yaml` 先前就有一個 `channel.telegram` 區塊但**沒有** `user_allowlist`，升級後 fleet 仍可運行，但**對外通知現在會靜默丟棄**（fail-closed）。你會看到：

```
WARN: telegram channel.user_allowlist is not set — any group member can command the fleet. \
      Set `user_allowlist: [123, 456]` in fleet.yaml to lock this down.
```

要恢復對外通知，把你的 operator user_id 加進 `user_allowlist`。這是**唯一必要的遷移步驟**；bot token 與 group id 維持不變。

如果你先前依賴舊有的「群組裡任何人都能指揮 fleet」行為，inbound 端在 Phase 2 導入之前仍會接受所有使用者；現在就設定 `user_allowlist`，可同時關閉兩端。

### `outbound_capabilities` 語意（Sprint 23 P1——default-open）

針對 **agent 可呼叫的**對外 MCP→Channel 操作（`reply`／`react`／`edit_message`／`delegate_task` provenance）的逐 instance gate。與 `user_allowlist` 互相獨立（後者控管 inbound 與 daemon 內部通知，且仍維持 **fail-closed**）。

| `outbound_capabilities` 值 | 行為 |
|---|---|
| 欄位缺漏 | **Default-open——允許所有操作** |
| `[reply, react, edit, inject_provenance]` | 僅允許列出的操作 |
| `[]`（明確為空） | 拒絕所有操作（operator 主動退出，保留此選項） |

**為何採 default-open？** 單一 operator 的威脅模型。TUI 本身已是完整的機器存取權限；Sprint 22 P0 的 cascade-attack-chain 防禦對於實際部署形態而言過度規格化。Operator 明確接受此安全取捨（Sprint 23 P1 反轉）。

內建 instance（`general` 以及任何未來自動建立的 coordinator）繼承 default-open——不需要自動注入的列表（這在 Sprint 22 P0 PR #230 中曾是如此，現已退役）。

### 限制／退出

對於單一 operator 的部署，default-open 是建議的姿態。如果你想啟用此 gate，有兩種退出形態：

**限制為操作的子集合**（例如僅允許 `reply`）：

```yaml
instances:
  my-worker:
    backend: claude
    outbound_capabilities: [reply]
    # … other fields …
```

**封鎖所有 agent 對外操作**（relay／唯讀角色）：

```yaml
instances:
  my-readonly-relay:
    backend: claude
    outbound_capabilities: []                # explicit "no agent outbound"
```

完整的轉換指南（Sprint 22 P0 fail-closed → Sprint 23 P1 default-open 反轉章節）與 `ChannelOpKind` enum 參考，請見 `docs/archived/MIGRATION-OUTBOUND-CAPS.md`。

## Other Commands

| 指令 | 用途 |
|---|---|
| `daemon <name:cmd>...` | 以明確的 agent spec 啟動 daemon（不用 fleet.yaml） |
| `list` / `ls` | 列出運行中的 agent |
| `status` | 詳細的 agent 狀態（state、health） |
| `inject <name> <text>` | 將文字注入 agent 的 PTY |
| `kill <name>` | kill 特定 agent |
| `connect <name>` | 將外部 agent 連接到 daemon |
| `fleet start/stop` | 從 fleet.yaml 批次 start/stop |
| `stop` | 停止 daemon |
| `quickstart` | 互動式設定（偵測 backend、設定 Telegram、產生 fleet.yaml） |
| `demo` | 30 秒的多 agent 編排互動示範 |
| `doctor` | 健康檢查（驗證安裝、backend、連線） |
| `bugreport` | 產生含 log 與設定的診斷報告 |
| `upgrade` | 觸發熱升級（需要 supervisor） |
| `verify` | 完整 E2E 驗證 |
| `test [suite]` | 執行內建測試（mcp、attach、inbox、api、all） |
| `capture` | 擷取 backend 輸出（除錯用） |
| `completions <shell>` | 產生 shell 自動補全（bash、zsh、fish、powershell） |
| `admin cleanup-branches [--yes]` | 刪除 PR 已合併的本機分支（預設為 dry-run） |
| `admin cleanup-zombies [--age <D>] [--yes]` | kill 掉持有過期 `run/<pid>/` 的 zombie daemon（#927；預設 `--age 14d`） |

## TUI Keyboard Shortcuts

所有快捷鍵都以 `Ctrl+B` 作為前綴鍵（類似 tmux）。

### 分頁管理

| 快捷鍵 | 動作 |
|---|---|
| `Ctrl+B n` | 新分頁（開啟選單） |
| `Ctrl+B 1-9` | 依編號切換分頁 |
| `Ctrl+B Tab` | 下一個分頁 |
| `Ctrl+B Shift+Tab` | 上一個分頁 |
| `Ctrl+B l` | 最近使用的分頁 |
| `Ctrl+B w` | 列出所有分頁 |

### Pane 管理

| 快捷鍵 | 動作 |
|---|---|
| `Ctrl+B \|` | 垂直分割 |
| `Ctrl+B -` | 水平分割 |
| `Ctrl+B arrows` | 聚焦 pane（可重複） |
| `Ctrl+B o` | 循環聚焦（可重複） |
| `Ctrl+B z` | 放大／還原 pane |
| `Ctrl+B x` | 關閉 pane |
| `Ctrl+B X` | 關閉分頁 |

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
| `Ctrl+B m` | 切換 mirror 靜音（未來：TUI channel mirror） |
| `Shift+Enter` | 換行但不送出（需要終端支援鍵盤增強功能） |
| `Alt+Enter` | 換行但不送出（與 Shift+Enter 相同） |

### 滑鼠

- **點選分頁**——切換到該分頁
- **拖曳分頁**——重新排序分頁
- **點選 pane 標籤**——聚焦 pane
- **拖曳 pane 標籤**——移動 pane（支援跨分頁）
- **滑鼠選取**——在 pane 中選取文字