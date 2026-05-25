# TUI 多 Agent 管理介面

AgEnD Terminal 提供一個類似 tmux 的終端多工介面，讓你在單一畫面中同時管理、監控整個 agent fleet 的狀態與輸出。

## 使用情境

> **適用對象：** Operator——透過 TUI 使用。

**監控長時間運行的 fleet。** 你的三 agent 團隊正在通宵開發一個功能。TUI 讓你並排查看所有 agent——分割面板即時顯示每個 agent 的終端輸出。當其中一個完成時，你可以在不離開介面的情況下切換焦點到下一個。

**互動式任務管理。** 你打開任務看板 overlay（`Ctrl+B T`），以看板視圖查看所有任務——待辦、進行中、已完成。你直接從看板將新任務指派給閒置的 agent，然後在旁邊的面板中觀察它開始工作。

**臨時建立 agent。** 作業中途發現需要一個臨時 agent 處理一次性任務。按 `Ctrl+B c` 或在命令面板輸入 `:spawn helper claude`——不需要編輯 fleet.yaml 或重啟 daemon。新 agent 會以 tab 形式出現，幾秒內就能就緒。

## 啟動方式

```bash
# 啟動 TUI（owned 模式，自動啟動 daemon 和所有 fleet agent）
agend-terminal app

# 附加到已運行的 daemon（attached 模式，不啟動新 agent）
agend-terminal app --attach
```

### Owned vs Attached 模式

- **Owned 模式**：TUI 擁有 daemon 生命週期。啟動時自動讀取 `fleet.yaml`，spawn 所有定義的 agent，關閉時同步停止。適合本地開發。
- **Attached 模式**：連接到已運行的 daemon。TUI 只負責顯示，不管理 agent 生命週期。適合遠端連線或多人共用 daemon。

---

## 核心概念

### Tab 與 Pane

每個 tab 包含一個或多個 pane，每個 pane 對應一個 agent 的終端輸出。Pane 可以水平或垂直分割，形成樹狀佈局。

### Prefix 鍵

所有操作都透過 `Ctrl+B` 前綴觸發（與 tmux 相同）。按下 `Ctrl+B` 後進入命令模式，再按對應的鍵執行操作。

部分操作支援連續重複：按住方向鍵調整 pane 大小時，1.5 秒內不需要重新按 `Ctrl+B`。

要向 agent 發送 `Ctrl+B` 本身，按 `Ctrl+B Ctrl+B`。

---

## 快捷鍵一覽

### Tab 管理

| 快捷鍵 | 說明 |
|--------|------|
| `Ctrl+B c` | 新增 tab（開啟選單選擇 agent/backend/shell） |
| `Ctrl+B n` | 下一個 tab |
| `Ctrl+B p` | 上一個 tab |
| `Ctrl+B l` | 切換至上次使用的 tab |
| `Ctrl+B 0-9` | 直接跳到第 N 個 tab |
| `Ctrl+B &` | 關閉 tab（含確認提示） |
| `Ctrl+B ,` | 重新命名 tab |
| `Ctrl+B w` | 列出所有 tab（可搜尋跳轉） |

### Pane 管理

| 快捷鍵 | 說明 |
|--------|------|
| `Ctrl+B "` | 水平分割（上下） |
| `Ctrl+B %` | 垂直分割（左右） |
| `Ctrl+B o` | 循環切換 pane 焦點（可連續按） |
| `Ctrl+B ↑↓←→` | 方向鍵切換 pane 焦點 |
| `Ctrl+B Alt+↑↓←→` | 調整 pane 大小 |
| `Ctrl+B H/J/K/L` | 調整 pane 大小（vim 風格，Shift 按住） |
| `Ctrl+B x` | 關閉 pane（多 pane 時有確認提示） |
| `Ctrl+B z` | 縮放切換（單一 pane 填滿 tab，再按恢復） |
| `Ctrl+B Space` | 切換佈局預設模式 |
| `Ctrl+B .` | 重新命名 pane |
| `Ctrl+B !` | 移動 pane 至其他 tab |
| `Ctrl+B @` | 翻轉分割方向（水平 ↔ 垂直） |

### 捲動模式

| 快捷鍵 | 說明 |
|--------|------|
| `Ctrl+B [` | 進入鍵盤捲動模式 |
| `j` / `k` | 向下 / 向上捲動 |
| `↑` / `↓` | 向上 / 向下捲動 |
| `PgUp` / `PgDn` | 捲動 10 行 |
| `q` / `Esc` | 離開捲動模式 |

### 面板與 Overlay

| 快捷鍵 | 說明 |
|--------|------|
| `Ctrl+B D` | 開啟決策面板（唯讀，可捲動） |
| `Ctrl+B T` | 開啟任務看板（四欄看板視圖） |
| `Ctrl+B ?` | 開啟快捷鍵說明 |
| `Ctrl+B ~` | 開啟浮動 shell（Esc 關閉並終止） |
| `Ctrl+B :` | 開啟命令面板 |
| `Ctrl+B d` | 分離（退出 TUI，相當於關閉） |

### 滑鼠操作

| 操作 | 說明 |
|------|------|
| 點擊 pane 區域 | 切換焦點 |
| 點擊 tab 標籤 | 切換 tab |
| 點擊 `[+]` 按鈕 | 新增 tab 選單 |
| 拖曳 tab 標籤 | 重新排列 tab 順序 |
| 拖曳分割邊框 | 即時調整 pane 大小 |
| 拖曳 pane 標題列 | 交換 pane 位置 |
| 拖曳 pane 標題 → tab 列 | 跨 tab 移動 pane |
| 滾輪 | 捲動焦點 pane（每格 3 行） |
| `Shift+拖曳` | 文字選取 |

---

## 命令面板

按 `Ctrl+B :` 開啟命令面板，輸入命令後按 Enter 執行。

| 命令 | 參數 | 說明 |
|------|------|------|
| `:spawn` | `<name> [backend]` | 新增 tab 並啟動 agent |
| `:vsplit` | `<name> [backend]` | 垂直分割並啟動 agent |
| `:hsplit` | `<name> [backend]` | 水平分割並啟動 agent |
| `:layout` | `[name]` | 設定佈局（無參數則循環切換） |
| `:kill` | `<name>` | 終止 agent 並從 fleet 移除 |
| `:restart` | `[name]` | 重新啟動 agent（預設為焦點 pane） |
| `:send` | `<to> <msg>` | 發送訊息給指定 agent |
| `:broadcast` | `<msg>` | 廣播訊息給所有 agent |
| `:status` | — | 記錄 agent 狀態（除錯用） |

`backend` 預設為 `claude`。支援的 backend 包括 claude、codex、gemini、opencode、kiro。

---

## 任務看板

按 `Ctrl+B T` 開啟任務看板。看板提供四種視圖，按 `Tab` 切換：

- **Tasks**：四欄看板（Backlog / Open / InProgress / Done）
- **Fleet**：agent 列表與狀態
- **Status**：agent 健康狀態儀表板
- **Monitor**：即時監控

### 任務看板操作

| 快捷鍵 | 說明 |
|--------|------|
| `←` / `→`（或 `h` / `l`） | 切換欄位 |
| `↑` / `↓`（或 `j` / `k`） | 在欄位內移動 |
| `Enter` | 檢視任務詳情 |
| `n` | 新增任務 |
| `d` | 取消任務 |
| `D`（Shift+d） | 標記任務完成 |
| `a` | 指派任務給 agent |
| `H`（Shift+h） | 左移狀態（降級） |
| `L`（Shift+l） | 右移狀態（升級） |
| `?` | 顯示看板說明 |
| `q` / `Esc` | 關閉看板 |

---

## Session 持久化

TUI 會自動儲存當前的佈局狀態到 `~/.agend-terminal/session.json`，包含：

- Tab 名稱與順序
- Pane 分割樹結構與比例
- 當前 active tab

### 恢復機制

下次啟動時，TUI 會根據 session 和 agent 來源（fleet.yaml 或 daemon registry）進行對帳：

1. **Rule 1**：從 fleet.yaml 自動啟動所有定義的 agent
2. **Rule 2**：session 中存在但 fleet 中不存在的 agent 靜默移除，兄弟 pane 接管空間
3. **Rule 3**：fleet 中存在但 session 中沒有的 agent 追加為新 tab，按 team 分組
4. **Rule 4**：如果沒有任何 agent，建立一個 fallback shell

這確保 fleet.yaml 永遠是 agent 集合的權威來源，而 session.json 只負責記住佈局偏好。

---

## 終端相容性

### 鍵盤協定

TUI 支援 Kitty 鍵盤協定（disambiguated escape codes）。在不支援的終端上自動降級到標準 ANSI 模式。Shift+字母鍵在兩種模式下都能正常運作。

### 換行輸入

- `Shift+Enter`：不送出的換行（需要現代終端支援）
- `Ctrl+J`：不送出的換行（所有終端通用）

### Panic 恢復

如果 TUI 意外 crash，panic hook 會自動恢復終端狀態：

1. 關閉 Kitty 鍵盤增強
2. 恢復 ratatui 終端設定
3. 關閉滑鼠擷取
4. 顯示游標

確保 crash 後終端仍然可用。

---

## 常見用法

### 快速啟動一個三 agent 團隊

```bash
# 在 fleet.yaml 中定義
instances:
  lead:
    backend: claude
    role: orchestrator
  dev:
    backend: claude
  reviewer:
    backend: claude

# 啟動 TUI
agend-terminal app
```

TUI 會自動為每個 agent 建立一個 tab，依照 team 分組。

### 臨時新增 agent

在 TUI 中按 `Ctrl+B c`，從選單選擇 backend，或按 `Ctrl+B :` 輸入：

```
:spawn helper claude
```

### 分割畫面同時觀察兩個 agent

```
Ctrl+B %    # 垂直分割，選擇第二個 agent
```

或在命令面板：

```
:vsplit reviewer
```

### 跨 tab 移動 pane

```
Ctrl+B !    # 開啟移動選單，選擇目標 tab 或建立新 tab
```
