# AgEnD Terminal — Terminal Emulator 實作計劃

> 目標：把 agend-terminal 從 CLI 工具升級成一個內建 agent 管理功能的 terminal application。
> 使用者打開它就是一個 terminal，每個 tab/pane 就是一個 agent，自動接入 daemon，自動有 MCP tools。

## 現有架構

```
agend-terminal (CLI)
├── daemon.rs      — 管理 agent registry、auto-respawn、fleet lifecycle
├── agent.rs       — PTY spawn、AgentHandle、AgentCore（VTerm + StateTracker + HealthTracker）
├── vterm.rs       — alacritty_terminal 封裝，screen buffer + ANSI dump
├── tui.rs         — 單一 agent attach（crossterm raw mode + Unix socket）
├── api.rs         — daemon JSON API over Unix socket
├── ops.rs         — 所有操作的統一入口（20+ functions：send、delegate、spawn、kill...）
├── mcp_config.rs  — 為各 backend 自動寫入 MCP server config
├── fleet.rs       — fleet.yaml 解析、instance 解析
├── state.rs       — agent 狀態偵測（14 states：Idle/Thinking/ToolUse/RateLimit/Crashed/...）
├── health.rs      — auto-respawn backoff、crash tracking
├── worktree.rs    — git worktree 自動建立與隔離
├── inbox.rs       — agent 間訊息佇列（file-based JSONL）
├── connect.rs     — 動態連接外部 agent 到 daemon（無 PTY 管理，inbox-only）
├── mcp/           — MCP stdio server（35 tools）
└── ...
```

關鍵事實：
- PTY 管理、VTerm、狀態偵測、MCP config 注入、worktree 隔離全部已實作
- 缺的是「多 pane TUI 框架」和「in-process daemon」
- `connect` command 提供了 headless 模式的動態 agent 加入，terminal app 完成後作為無 GUI 環境的替代方案保留

---

## Phase 1：Terminal Application 核心

### 1.1 多 Tab/Pane TUI 框架

現有 `tui.rs` 只支援 attach 單一 agent，需要改成多 pane 佈局。

**佈局結構：**
```
┌─ tab bar ──────────────────────────────────────────────┐
│  [dev ●]  [reviewer]  [shell]  [+]                     │
├─────────────────────────┬──────────────────────────────┤
│                         │                              │
│   agent "dev" PTY       │   agent "reviewer" PTY       │
│   (VTerm render)        │   (VTerm render)             │
│                         │                              │
│                         │                              │
├─ status bar ────────────┴──────────────────────────────┤
│  dev: Thinking | reviewer: Idle | 2 agents | fleet.yaml│
└────────────────────────────────────────────────────────┘
```

**快捷鍵（tmux 風格，prefix = Ctrl+B）：**

| 快捷鍵 | 動作 |
|---------|------|
| `Ctrl+B c` | 新建 tab（選 backend 或 fleet instance） |
| `Ctrl+B n` / `Ctrl+B p` | 下一個 / 上一個 tab |
| `Ctrl+B 0-9` | 跳到第 N 個 tab |
| `Ctrl+B "` | 水平分割 pane |
| `Ctrl+B %` | 垂直分割 pane |
| `Ctrl+B o` | 切換 focus 到下一個 pane |
| `Ctrl+B x` | 關閉當前 pane |
| `Ctrl+B z` | 當前 pane 全螢幕 toggle |
| `Ctrl+B [` | 進入 scroll mode（方向鍵/PgUp/PgDn 捲動，q 退出） |
| `Ctrl+B :` | 開啟 command palette |
| `Ctrl+B d` | detach（見下方 detach 語義） |
| `Ctrl+B ?` | 顯示快捷鍵說明 |

**實作要點：**
- 用 `ratatui` 做全螢幕 TUI layout（tab bar + pane area + status bar）
- 每個 pane 持有一個**本地 VTerm** 實例（見 1.2）
- 鍵盤輸入：focus pane 的按鍵直接寫入對應 agent 的 `pty_writer`

**Event Loop 架構：**
- `app.rs` 的 main loop 用統一的 `crossbeam::channel::select!` 同時監聽多個 event source，避免 polling：
  ```
  loop {
      select! {
          recv(crossterm_rx) -> event => { /* 鍵盤/滑鼠/resize */ }
          recv(pane_output_rx) -> (pane_id, data) => { /* agent PTY output → local VTerm */ }
          recv(crash_rx) -> agent_name => { /* daemon crash event → 更新 tab 狀態 */ }
          recv(notification_rx) -> notif => { /* MCP inter-agent 訊息 → tab badge */ }
          default(Duration::from_millis(100)) => { /* 定期 render：狀態更新、閃爍動畫 */ }
      }
  }
  ```
- crossterm events 需要一個 dedicated thread 讀取，透過 channel 轉發（crossterm 的 `event::read()` 是 blocking）
- 所有 pane 的 subscriber channel 匯聚到一個 `pane_output_rx`（每個 pane subscriber 用一個 forwarder thread，附帶 pane_id）

**Terminal Resize 處理：**
- 監聽 crossterm `Event::Resize(cols, rows)`
- ratatui re-layout 計算每個 pane 的新大小
- 對每個 pane：`local_vterm.resize(pane_cols, pane_rows)`
- 對每個 agent PTY：`pty_master.resize(PtySize { cols, rows })`，觸發 child process 收到 SIGWINCH

### 1.2 Pane Local VTerm（避免 lock contention）

現有 `AgentCore` 中的 VTerm 由 `Mutex` 保護，PTY read thread 每次收到 output 都會 lock core 執行 `vterm.process(data)` + 通知 subscribers。如果 render thread 也 lock core 讀 grid，會產生 lock contention。

**架構：每個 pane 持有自己的 local VTerm**

```
agent PTY output
  → daemon VTerm.process(data)       # 現有：PTY read thread 更新（用於 attach reconnect）
  → broadcast to subscribers          # 現有：crossbeam channel
  → pane 收到 raw bytes               # subscriber channel
  → pane.local_vterm.process(data)    # 新增：pane 自己的 VTerm
  → VTermWidget.render(area)          # 新增：直接讀 local VTerm grid → ratatui buffer
  → ratatui 差分更新到 stdout         # ratatui 內建
```

好處：
- Render thread 和 PTY read thread 零 lock contention
- 每個 pane 獨立，可以有不同的 scroll position
- 記憶體代價很小（VTerm 只佔幾十 KB per agent）

**Resize 同步注意事項：**
- Local VTerm 的 size = pane 的實際顯示大小（隨 layout 變動）
- Daemon VTerm 的 size = PTY 的 size（由 `pty_master.resize()` 控制）
- 兩者必須保持一致：resize 時先更新 PTY size，daemon VTerm 和 local VTerm 同步 resize
- 如果 terminal app 是 client 模式連接外部 daemon，resize 需要透過 API 通知 daemon 更新 PTY size

**連接已有 agent 時：**
- 呼叫 `subscribe_with_dump(agent)`，取得 `(Receiver<Vec<u8>>, Vec<u8>)`
- 先把 `dump`（ANSI screen dump）feed 進 local VTerm 重建畫面
- 後續持續從 Receiver 讀取新 output

### 1.3 VTermWidget

`VTerm.term` 是 private field，需要新增 API 讓 render layer 存取 grid cells。

**方案：在 VTerm 上新增 render 方法**

```rust
impl VTerm {
    /// Render current screen state into a ratatui Buffer at the given area.
    pub fn render_to_buffer(&self, buf: &mut ratatui::buffer::Buffer, area: ratatui::layout::Rect) {
        let grid = self.term.grid();
        for row in 0..area.height {
            for col in 0..area.width {
                let cell = &grid[Point::new(Line(row as i32), Column(col as usize))];
                let style = cell_to_style(cell);  // fg/bg/flags → ratatui::style::Style
                buf.set_string(area.x + col, area.y + row, &cell.c.to_string(), style);
            }
        }
        // Render cursor position
    }
}
```

**Color 映射：**
- `Color::Named(NamedColor)` → ratatui `Color::Indexed(0..15)` 或具名色
- `Color::Spec(Rgb { r, g, b })` → ratatui `Color::Rgb(r, g, b)`
- `Color::Indexed(idx)` → ratatui `Color::Indexed(idx)`

**Cell Flags 映射：**
- `BOLD` → `Modifier::BOLD`
- `DIM` → `Modifier::DIM`
- `ITALIC` → `Modifier::ITALIC`
- `UNDERLINE` → `Modifier::UNDERLINED`
- `INVERSE` → `Modifier::REVERSED`
- `STRIKEOUT` → `Modifier::CROSSED_OUT`
- `WIDE_CHAR_SPACER` → skip（前一個 wide char 已佔位）

**Scrollback：**
- 啟用 `alacritty_terminal` 的 scrollback buffer（提高 `scrolling_history` 設定值，例如 10000）
- Scroll mode 時調整 grid 讀取的 offset（grid 支援 scroll display offset）
- `Ctrl+B [` 進入 scroll mode，方向鍵/PgUp/PgDn 控制 offset，`q` / `ESC` 退出

### 1.4 內建 Daemon（in-process）

現在 daemon 是獨立 process（`agend-terminal start` 啟動）。Terminal app 應該內建 daemon。

**策略：**
- 啟動 terminal app 時，檢查是否已有 daemon 在跑（`find_active_run_dir()`）
- 如果沒有：在 background thread 啟動 daemon 功能（registry、API socket、crash respawn、health monitor）
- 如果已有：連接到現有 daemon 的 API socket，terminal app 作為 client
- 兩種模式共用同一套 TUI，差別只在 agent spawn 是 local 還是透過 API

**修改 `daemon.rs`：**
- 抽出 `DaemonCore` struct，包含 registry、configs、shutdown flag、crash channel
- `daemon::run()` 改成建立 `DaemonCore` + 啟動 background threads
- Terminal app 持有 `DaemonCore`，直接操作 registry（不需要走 API socket）

**Detach 語義（兩種模式不同）：**

| 模式 | `Ctrl+B d` 行為 |
|------|-----------------|
| **Client 模式**（連接外部 daemon） | 斷開 TUI，daemon 繼續運行。可隨時重新打開 terminal app 連回 |
| **In-process 模式** | 保存 session layout 到 `~/.agend/session.json`，然後正常退出。Agent processes 跟著結束。下次啟動時 resume（透過各 backend 的 `ResumeMode`） |

為什麼 in-process 不 fork daemon？因為 agent PTY 的 file descriptor 跨 fork 後行為不確定，且增加大量複雜度。保存 session + resume 更可靠。

---

## Phase 2：Agent 自動接入

### 2.1 新建 Tab = 自動 Spawn Agent

使用者按 `Ctrl+B c` 新建 tab 時的流程：

```
Ctrl+B c
  → 顯示選單：
      1. 從 fleet.yaml 選 instance（dev, reviewer, ...）
      2. 選 backend（Claude / Kiro / Gemini / OpenCode / Codex）
      3. 開 bash shell
  → 使用者選擇後：
      → worktree::create()              # git worktree 隔離
      → mcp_config::configure()         # 寫入 MCP server config
      → instructions::generate()        # 寫入 statusline 等 backend 設定
      → agent::spawn_agent()            # spawn PTY
      → 新 pane 訂閱 agent output       # subscribe_with_dump()
      → pane.local_vterm.process(dump)  # 重建畫面
      → tab 標題 = agent name
```

注意：`instructions::generate()` 負責 MCP config + statusline 腳本。Agent-specific instructions（`.claude/rules/agend.md` 等）由 fleet 啟動路徑中的其他機制處理，需要在此流程中補上或從 fleet.rs 中提取出來。

### 2.2 Fleet 一鍵啟動

```bash
agend-terminal          # 無參數：啟動 terminal app
```

啟動時：
1. 如果有 `fleet.yaml`，自動為每個 instance 開一個 tab
2. 等同於現在的 `agend-terminal start`，但每個 agent 直接顯示在 tab 裡
3. 不需要再 `agend-terminal attach`

如果沒有 `fleet.yaml`，開一個空的 terminal，使用者可以手動新建 tab。

### 2.3 Agent 狀態即時顯示

- Tab 標題格式：`name [state]`，例如 `dev [Thinking]`
- 狀態顏色映射（完整 14 states）：

| 狀態 | 顏色 | 說明 |
|------|------|------|
| Starting | 白色 | 啟動中 |
| Ready | 綠色 | 就緒等待輸入 |
| Idle | 灰色 | 閒置 |
| Thinking | 黃色 | 推理中 |
| ToolUse | 藍色 | 執行工具 |
| PermissionPrompt | 品紅色（閃爍） | 需要使用者介入 |
| ContextFull | 橘色 | 上下文已滿 |
| RateLimit | 橘色 | 速率限制 |
| UsageLimit | 紅色 | 額度用完 |
| AuthError | 紅色 | 認證失敗 |
| ApiError | 紅色 | API 錯誤 |
| Hang | 紅色（閃爍） | 疑似卡住 |
| Crashed | 紅色 | 崩潰 |
| Restarting | 紅色（閃爍） | 重啟中 |

- Status bar 顯示所有 agent 的狀態摘要
- 複用現有 `state.rs` 的 `StateTracker`，每次 render 時讀取 `get_state()`

---

## Phase 3：Agent 間通訊視覺化

### 3.1 訊息通知

- Agent A 透過 MCP `send_to_instance` 發訊息給 Agent B 時：
  - B 的 tab 標題顯示通知 badge
  - Status bar 顯示 `[alice → bob] "Hey, can you review this?"`
- 實作：MCP handler（`mcp/handlers.rs`）在 `send_to` 時發一個 event 到 TUI 的 event channel

### 3.2 決策/任務面板

- `Ctrl+B D`（大寫）開 decisions overlay panel
- `Ctrl+B T`（大寫）開 task board overlay panel
- 顯示 `decisions.rs` 和 `tasks.rs` 的資料
- Overlay 蓋在 pane 上方，按 ESC 關閉

---

## Phase 4：進階功能

### 4.1 Session 持久化

- `Ctrl+B d` detach 時（in-process 模式），保存 layout 到 `~/.agend/session.json`：
  ```json
  {
    "tabs": [
      {"name": "dev", "panes": [{"agent": "dev", "split": "full"}]},
      {"name": "review", "panes": [
        {"agent": "reviewer", "split": "left"},
        {"agent": "shell", "split": "right"}
      ]}
    ],
    "active_tab": 0
  }
  ```
- 重新打開時恢復 layout + resume agent sessions（複用各 backend 的 `ResumeMode`）
- 複用現有 `snapshot.rs` 保存 agent 狀態

### 4.2 Command Palette

- `Ctrl+B :` 開啟 command input
- 支援指令（對應 `ops.rs` 函數）：
  ```
  spawn <name> <backend>       # ops::create_instance()
  kill <name>                  # ops::delete_instance()
  broadcast <message>          # ops::broadcast()
  send <from> <to> <message>   # ops::send_message()
  delegate <from> <to> <task>  # ops::delegate_task()
  status                       # ops::list_instances()
  ```

### 4.3 Mouse 與 Clipboard

- 滑鼠滾輪：scroll pane 歷史
- 滑鼠選取：選取文字（進入 visual mode）
- `Ctrl+Shift+C` / `Ctrl+Shift+V`：複製 / 貼上
- crossterm 已支援 mouse events，ratatui 可以轉發

### 4.4 Telegram 整合顯示

- Status bar 顯示 Telegram 連線狀態
- 來自 Telegram 的訊息在對應 agent tab 顯示通知 badge

---

## 技術選型

| 元件 | 方案 | 原因 |
|------|------|------|
| TUI 框架 | `ratatui` + `crossterm` | 已用 crossterm，ratatui 加上去就有 layout/widget |
| Terminal emulation | 現有 `alacritty_terminal` 0.26 | 已在用，grid cells 可直接存取 |
| PTY 管理 | 現有 `portable-pty` | 已在用 |
| Pane render | VTerm `render_to_buffer()`（新）| 直接讀 grid cells → ratatui buffer，不走 dump → re-parse |
| Scrollback | alacritty_terminal 內建 | 調高 `scrolling_history`，grid 支援 scroll offset |

### 新增依賴

```toml
ratatui = "0.29"   # 確認實作時的最新穩定版
```

### 新增/修改檔案

| 檔案 | 類型 | 說明 | 預估行數 |
|------|------|------|----------|
| `src/app.rs` | 新增 | Terminal app 主迴圈、App state、event loop | ~350 |
| `src/layout.rs` | 新增 | Tab/Pane 佈局管理、分割/合併邏輯 | ~250 |
| `src/render.rs` | 新增 | VTermWidget、tab bar widget、status bar widget、Color/Flags 映射 | ~400 |

> `render.rs` 預估最大，因為 color mapping（Named/Indexed/Rgb 三種）、wide char 處理（CJK 字元佔 2 columns、WIDE_CHAR_SPACER skip）、cursor rendering（block/underline/bar + 閃爍）的邊界情況較多。

| `src/keybinds.rs` | 新增 | 快捷鍵處理、prefix mode、scroll mode、command palette | ~200 |
| `src/vterm.rs` | 修改 | 新增 `render_to_buffer()`、啟用 scrollback、scroll offset API | ~100 改動 |
| `src/tui.rs` | 修改 | 保留 attach 功能，新增 app 模式入口 | ~50 改動 |
| `src/daemon.rs` | 修改 | 抽出 DaemonCore，支援 in-process 模式 | ~120 改動 |
| `src/main.rs` | 修改 | 無參數時啟動 terminal app | ~30 改動 |

**總計新增約 1200 行，修改約 300 行。**

---

## 實作進度

### 已完成 ✅

| Phase | 功能 | 說明 |
|-------|------|------|
| 1.1 | 多 Tab TUI 框架 | app.rs + layout.rs + keybinds.rs + render.rs |
| 1.2 | Pane Local VTerm | subscriber 接線，避免 lock contention |
| 1.3 | VTermWidget render | render_to_buffer + Color/Flags 映射 + scrollback |
| 1.4 | In-process daemon | 共用 AgentRegistry，API server + MCP tools 自動可用 |
| 2.1 | 新建 tab = spawn agent | Ctrl+B c 選單（fleet/backend/shell） |
| 2.2 | Fleet 啟動 | `--fleet` flag 指定 fleet.yaml |
| 2.3 | 狀態即時顯示 | 14 states 完整顏色映射，tab 標題即時更新 |
| 3.1 | 訊息通知 | Tab badge `!`，`[from:...]` 偵測 |
| 4.1 | Session 持久化 | Ctrl+B d 保存 tree layout，下次 resume |
| 4.3 | Mouse scroll | 滑鼠滾輪 per-pane scroll |
| - | Unified PTY flow | app 用 spawn_agent()，auto-dismiss/state/broadcast 繼承 daemon |
| - | Connect command | `agend-terminal connect` 動態連接外部 agent |
| - | ExternalRegistry | 外部 agent 註冊、PID liveness check |
| - | Split tree | 巢狀 split + 空間方向導航 + split 持久化 |
| - | Tmux 快捷鍵 | c/n/p/l/0-9/&/,/./w/"//%/o/x/z/[/d/? + repeat mode |
| - | Pane display name | Ctrl+B . 重命名，agent_name 路由不動 |
| - | Close confirmation | Ctrl+B x / & 確認對話框 |
| - | Self-send guard | 擋自己發給自己 + shells 從 list_instances 隱藏 |
| - | IME cursor | terminal cursor 定位在 focused pane，中文輸入正常 |
| - | Color fallback | RGB→256 for macOS Terminal.app |

### 未完成 ❌

| Phase | 功能 | 複雜度 | 說明 |
|-------|------|--------|------|
| 2.2 | Fleet 自動啟動 | 低 | 無參數啟動時自動載入 fleet.yaml（目前停用避免衝突） |
| 3.2 | 決策/任務面板 | 中 | Ctrl+B D/T overlay，顯示 decisions.rs / tasks.rs |
| 4.2 | Command palette | 中 | Ctrl+B : 輸入指令（spawn/kill/send/broadcast） |
| 4.4 | Telegram 整合顯示 | 低 | Status bar 顯示 Telegram 連線狀態 |
| - | Clipboard | 中 | Pane 內文字選取 + 複製（需要自訂 copy mode） |
| - | Agent instructions | 低 | 自動寫入 `.claude/rules/agend.md` 等指引檔案 |
| - | Resize propagation | 低 | 精確的 resize 從 render layout → PTY（目前在 render 時做） |

---

## 使用者體驗（完成後）

```bash
# 方式一：有 fleet.yaml，一鍵啟動
agend-terminal
# → 打開 terminal app
# → 自動讀 fleet.yaml
# → 每個 instance 一個 tab，agent 自動跑起來
# → 每個 agent 自動有 35 個 MCP tools
# → 每個 agent 在獨立 git worktree 裡

# 方式二：空白啟動，手動加 agent
agend-terminal
# → 打開空 terminal
# → Ctrl+B c → 選 Claude Code → 輸入 name "dev"
# → agent 自動 spawn，worktree 自動建立，MCP 自動設定
# → Ctrl+B c → 再加一個 reviewer
# → 兩個 agent 可以互相溝通

# 方式三：CLI 模式（向後相容）
agend-terminal start      # 跟現在一樣
agend-terminal attach dev # 跟現在一樣
agend-terminal connect my-agent --backend claude  # 動態連接（headless 環境用）
```
