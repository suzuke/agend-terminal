[English](FEATURE-health.md)

# Health & Monitoring — Agent 健康狀態與自動恢復

## 使用情境

> **適用對象：** Daemon 基礎設施——全自動運作，operator 透過 TUI 觀察。

**自動 crash 恢復。** Dev agent 的程序因為意外的 API 錯誤而 crash。Daemon 偵測到程序結束，將 crash 記錄在滑動窗口中，等待指數退避延遲（從 5 秒開始），然後自動重啟 agent。如果 agent 穩定下來，它會像什麼都沒發生一樣繼續工作。如果連續 crash（5 次以上），daemon 停止重啟並通知 operator。

**Hang 偵測與恢復。** Agent 收到任務但在 `Thinking` 狀態下超過 600 秒沒有 PTY 輸出。Daemon 將其分類為 `Hung`，啟動三階段恢復階梯：先發送 ESC 鍵嘗試中斷，若 ESC 無效則重啟程序，若重啟也無效則暫停 agent 並通知 operator 手動介入。

**Operator 健康儀表板。** Operator 查看 TUI，發現一個 agent 處於 `Unstable` 狀態（10 分鐘內 3 次 crash），另一個顯示 `IdleLong`（沒有待處理工作，只是在等待）。結構化的健康狀態讓 operator 能快速區分需要關注的 agent 和只是閒置的 agent。

## 設計初衷

AI coding agent 會遇到各種異常：程序 crash、API rate limit、回應超時
（hang）、進入錯誤迴圈。如果每一種都需要 operator 手動介入，那多 agent
的系統就無法長時間無人值守。

Health & Monitoring 是 AgEnD daemon 的核心子系統，它持續監控每個 agent
的健康狀態，自動執行恢復動作（重啟、發送 ESC 鍵、通知 operator），
並提供結構化的狀態報告讓 operator 和其他 agent 了解系統狀況。

---

## 兩層狀態模型

AgEnD 將 agent 的狀態分為兩個獨立的層次：

### AgentState — 即時偵測

AgentState 是從 PTY 輸出即時解析出的瞬間狀態。每一行輸出都可能
改變 AgentState。

| 狀態 | 說明 |
|------|------|
| `Starting` | Agent 程序剛啟動，尚未就緒 |
| `Ready` | 就緒，等待輸入 |
| `Idle` | 閒置，無 pending 工作 |
| `Thinking` | 正在思考（產生中間輸出） |
| `ToolUse` | 正在執行工具呼叫 |
| `RateLimit` | 遇到 API rate limit |
| `InteractivePrompt` | 顯示互動式提示（如權限確認） |
| `PermissionPrompt` | 等待權限確認 |
| `AwaitingOperator` | 等待 operator 操作 |
| `Hang` | 疑似卡住 |
| `Crashed` | 程序已結束 |

### HealthState — 累積生命週期

HealthState 是從多次事件累積推導出的生命週期狀態。它反映的是
agent 的整體健康趨勢，不會因為一行輸出就改變。

| 狀態 | 說明 | 是否觸發自動恢復 |
|------|------|----------------|
| `Healthy` | 正常運行 | 否 |
| `Recovering` | Crash 後正在恢復 | 否 |
| `Unstable` | 10 分鐘內 3+ 次 crash | 否 |
| `Failed` | 超過最大重試次數（5 次），停止自動重啟 | 否（終態） |
| `Hung` | 有 pending input 但超時未回應 | 是 |
| `IdleLong` | 長時間無活動，但沒有 pending input（非異常） | 否 |
| `ErrorLoop` | 10 分鐘內同一狀態錯誤 3+ 次 | 否 |
| `Paused` | 自動恢復三階段全部失敗，等待 operator 手動介入 | 否（終態） |

---

## Crash 處理

### 自動重啟

當 agent 程序 crash 時，daemon 會自動重啟：

1. **記錄 crash**：將 crash 時間加入滑動窗口（10 分鐘）
2. **計算退避延遲**：指數退避，從 5 秒開始，每次加倍，上限 5 分鐘
3. **判斷是否重啟**：
   - 總 crash 次數 < 5 次：重啟
   - 總 crash 次數 ≥ 5 次：進入 `Failed` 狀態，停止重啟
4. **通知判斷**：
   - 第 1 次 crash：靜默重啟
   - 第 2 次起：發送通知（受 5 分鐘冷卻時間限制）

### 狀態轉換

```
Healthy → (1 crash) → Recovering → (respawn OK) → Healthy
Healthy → (3 crashes in 10min) → Unstable
任何狀態 → (5+ crashes) → Failed（終態，需 operator 介入或等待衰減）
```

### Crash 衰減

如果 agent 穩定運行 30 分鐘（無新 crash），crash 計數會自動衰減：

- `total_crashes` 每 30 分鐘減 1
- `Failed` → `Recovering`（當 crash 計數降到 5 以下）
- `Recovering` → `Healthy`（當 crash 計數降到 3 以下）
- `Unstable` → `Healthy`（當 crash 計數降到 3 以下）

---

## Hang 偵測

### 判斷邏輯

daemon 每 tick 檢查每個 agent 的沉默時間。超過門檻值時進入分類流程：

| AgentState | 沉默門檻 |
|------------|---------|
| `Idle` | 永不視為 hang |
| `Starting` | 120 秒 |
| `Thinking` / `ToolUse` | 600 秒 |
| 其他 | 120 秒 |

### Hung vs IdleLong 分辨

沉默超過門檻後，daemon 進一步判斷是真 hang 還是正常閒置：

**Hung（真卡住）**：有 pending input 但 agent 沒回應
- 條件：`last_input_at_ms > last_heartbeat_at_ms + 5s`
- 意思是：operator 送了輸入，但 agent 超過 5 秒沒有任何 MCP 呼叫（heartbeat）
- 觸發自動恢復階梯

**Hung（F1 交叉檢查）**：heartbeat 新鮮但 PTY 無輸出
- 條件：heartbeat 最近有更新，但 PTY 已沉默超過門檻
- 意思是：agent 在呼叫 MCP 工具（heartbeat 刷新），但沒有產生 PTY 輸出
- 可能是 agent 陷入緊密的 MCP 迴圈

**IdleLong（正常閒置）**：沒有 pending input，agent 只是在等下一個任務
- 不觸發任何恢復動作
- operator 04:00 UTC 離開後的正常狀態

### 5 秒寬限窗口

input 送達和 heartbeat 刷新之間有 5 秒的寬限窗口，避免在 MCP roundtrip
完成前誤判為 Hung。

---

## 自動恢復階梯

當 agent 被分類為 `Hung` 時，daemon 啟動三階段恢復：

### Stage 1：ESC 鍵

- 向 agent 的 PTY 發送 ESC 鍵（中斷當前操作）
- 等待 10 秒看是否恢復
- 冷卻時間 60 秒（避免頻繁發送 ESC）

### Stage 2：自動重啟

- Stage 1 失敗後，自動重啟 agent 程序
- 等待 30 秒看是否恢復
- 最多重啟 3 次（跨 Hung 週期累計）
- 退避延遲 1 秒

### Stage 3：暫停

- Stage 2 的 3 次重啟都失敗後
- 將 HealthState 設為 `Paused`（終態）
- 通知 operator 需要手動介入
- `check_hang` 不再觸發（短路返回 false）
- crash 衰減不會改變 `Paused` 狀態

整個階梯可以透過 runtime config 的 `hang_auto_recovery_enabled` 開關控制。

---

## Watchdog — PTY 輸出分類

daemon 的 watchdog 每 tick 掃描每個 agent 的最新 PTY 輸出，比對
已知的異常模式：

| 偵測到的模式 | 設定的 BlockedReason |
|-------------|---------------------|
| "rate limit" / "Too Many Requests" | `RateLimit` |
| "quota exceeded" | `QuotaExceeded` |
| "awaiting operator" / 互動式提示 | `AwaitingOperator` |
| "permission" 提示 | `PermissionPrompt` |

設定 BlockedReason 後：
- `RateLimit` / `QuotaExceeded` / `AwaitingOperator`：抑制 hang 偵測（避免 false alarm）
- `PermissionPrompt`：**不**抑制 hang 偵測

### 試運行模式

設定 `AGEND_WATCHDOG_DRY_RUN=1` 環境變數可以啟用試運行模式：
watchdog 只記錄偵測結果到 event log，不實際修改 health 狀態。
適合在上線前測試 pattern matching 的準確性。

---

## Idle Watchdog — 閒置偵測

獨立於 health monitoring 的另一個 watchdog，追蹤 agent 和整個 fleet
的活動狀態。

### 兩個觀測角度

**Dev 角度**：單一 agent 閒置超過 60 分鐘
- 通知 lead：「dev 已經閒置 60 分鐘了」

**Fleet 角度**：所有 agent 都閒置超過 30 分鐘
- 通知 general：「整個 fleet 已經閒置 30 分鐘了」
- 在 task board 為空且沒有 pending dispatch 時，抑制 alert（沒有預期工作）

### 活動追蹤

每個 agent 的活動狀態記錄在 `$AGEND_HOME/agent-activity/<agent>.json`
sidecar 中。agent 每次透過 `send` 工具發送訊息時自動更新時間戳。

### Snooze 與 Ack

| 操作 | 說明 |
|------|------|
| Snooze | 暫停 fleet idle alert 到指定時間 |
| Ack | 確認收到 alert，下次有新活動前不再通知 |
| Resume | 清除 snooze / ack，恢復正常偵測 |

---

## MCP 工具

### health report

向 daemon 回報目前的 blocked reason：

```json
{
  "tool": "health",
  "action": "report",
  "reason": "rate_limit",
  "retry_after_secs": 60
}
```

Agent 可以主動回報自己遇到的問題，讓 watchdog 知道不需要觸發
hang 偵測。

### health clear_blocked_reason

清除先前設定的 blocked reason，恢復正常 hang 偵測：

```json
{
  "tool": "health",
  "action": "clear_blocked_reason"
}
```

---

## 常見問題

### Q: agent 被判定為 Hung 但其實在正常工作？

可能的原因：
1. Agent 的 PTY 輸出被 backend 的 TUI 框架吃掉（沒有到達 daemon 的 vterm）
2. Agent 在執行長時間的工具呼叫（超過 600 秒）

解決方式：
- 調整 `AGEND_PRODUCTIVE_GATE=1` 啟用 F9 productive-output gate
- 使用 `health action=report` 主動回報 agent 的狀態

### Q: Failed 狀態可以自動恢復嗎？

可以，但需要時間。crash 計數每 30 分鐘衰減 1 次。如果 agent 的
total_crashes 降到 5 以下，會自動轉為 Recovering，再降到 3 以下
轉為 Healthy。但在 Failed 期間程序已停止，需要 operator 手動重啟
或等待 Stage 2 auto-recovery 觸發。

### Q: Paused 狀態怎麼解除？

目前 Paused 是終態，需要 operator 手動介入。未來會加入 operator
unpause 命令界面。

### Q: 如何查看 agent 的當前健康狀態？

```bash
agend-terminal list --detailed
```

或透過 MCP 工具查詢 agent registry。