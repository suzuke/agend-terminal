[English](FEATURE-dispatch-idle.md)

# Dispatch Idle Tracking — 任務回應超時追蹤

## 使用情境

> **適用對象：** Agent 基礎設施——agent 透過 MCP tools 使用，operator 通常不直接操作。

**遺漏任務偵測。** Lead agent 以 `expect_reply_within_secs=600` 向 dev agent 分派一項任務。Dev agent 在處理訊息前 crash 了。沉默 10 分鐘後，daemon 向 lead 發送 `dispatch_idle_threshold_exceeded` 通知，提醒任務可能被遺漏。Lead 可以重新分派給其他 agent 或進行調查。

**活躍工作確認。** Dev agent 收到一項複雜任務並開始處理。工作過程中，它發送一條 `kind=update` 訊息回報進度。Daemon 看到這個活動後重設閒置計時器，避免在 dev 明顯在工作時觸發誤報。

**Team 提醒。** 對任何已設定的 team，L1 通知 dispatcher 後，L2 會再等 10 分鐘；若 dispatch 仍未解除，就向目標 agent 發送一次 `dispatch_idle_nudge`。

## 設計初衷

在多 agent 協作中，orchestrator（通常是 lead）會分派任務給 dev 或 reviewer。
但如果接收方沒有回應——可能是 agent 卡住了、crash 了、或被其他工作佔用——
orchestrator 不會知道任務被遺漏了。

Dispatch Idle Tracking 解決這個問題。它在分派任務時啟動一個計時器，如果在
設定的時間窗口內沒有收到回報（`kind=report`），daemon 會自動通知分派者：
「你 10 分鐘前派出去的任務還沒有回應」。

這是一個兩層（L1 + L2）的系統：

- **L1（跨團隊安全）**：通知分派者本人，不含任何團隊名稱或團隊邏輯
- **L2（泛用 per-team automation）**：第二段延遲後，對任何 team dispatch 通知目標 agent

---

## 使用方式

### 啟用追蹤

在使用 `send` 工具分派任務時，加上 `expect_reply_within_secs` 參數：

```json
{
  "tool": "send",
  "instance": "dev",
  "message": "請實作 #123 的修正",
  "request_kind": "task",
  "task_id": "t-20260525-1",
  "expect_reply_within_secs": 600
}
```

這會建立一個 dispatch idle sidecar，daemon 在 600 秒（10 分鐘）後
檢查是否有對應的 `kind=report`（以 `correlation_id` 匹配）。

### Team 預設值

任何 team member 的 `kind=task` dispatch 都會自動繼承 30 分鐘
的 idle 追蹤，不需要手動指定 `expect_reply_within_secs`；明確指定時會覆寫預設值。
Teamless caller 不會自動追蹤。`kind=query` 不會建立此 sidecar。

### Escalation tiering（#2031）

兩段通知依序觸發：門檻到期時先通知 dispatcher 並寫入 `exceeded_at`；若再過
`ESCALATE_TO_AGENT_AFTER_SECS`（10 分鐘）仍未解除，才向目標 agent 發送 nudge。

### 參數說明

| 參數 | 型別 | 說明 |
|------|------|------|
| `expect_reply_within_secs` | int | 預期回覆的秒數，超過後觸發 alert |

---

## 運作原理

### Sidecar 機制

每次帶有 `expect_reply_within_secs` 的 dispatch 會在
`$AGEND_HOME/pending-dispatches/` 目錄下建立一個 JSON sidecar 檔案。

Sidecar 內容：

```json
{
  "dispatch_id": "disp-20260525050000000000-1",
  "dispatcher": "lead",
  "target": "dev",
  "correlation_id": "t-20260525-1",
  "expected_kind": "task",
  "threshold_secs": 600,
  "issued_at": "2026-05-25T05:00:00Z",
  "status": "pending",
  "exceeded_at": null,
  "nudge_sent_at": null
}
```

`dispatch_id` 由微秒時間戳 + 程序內原子計數器生成，確保唯一性。

### L1：超時偵測與通知

daemon 約每 60 秒掃描一次 `pending-dispatches/` 目錄：

1. 讀取所有 `pending` 狀態的 sidecar
2. 計算 `elapsed = now - issued_at`
3. 如果 `elapsed > threshold_secs`：
   - 向 **分派者**（dispatcher）的 inbox 發送 `dispatch_idle_threshold_exceeded` 通知
   - 將 sidecar 狀態改為 `exceeded`
   - 記錄到 event log

通知內容包含分派 ID、目標 agent、經過時間、correlation_id。

### L2：Per-Team Nudge

L2 是 L1 對所有已設定 team 的泛用補充。Teamless dispatcher 沒有自動預設值，也不會收到 L2 team nudge。

當 sidecar 狀態變為 `exceeded` 後，L2 會額外向 **目標 agent**
（被分派者）發送 `dispatch_idle_nudge` 通知，提醒它有任務待處理。

去重機制：每個 sidecar 只 nudge 一次（記錄在 `nudge_sent_at` 欄位）。

L2 的隔離保證：
- L1 的程式碼中不含任何團隊名稱字串（有 CI 測試 `no_team_name_strings_in_l1` 強制保證）
- L2 由獨立的 `team_nudge.rs` 模組載入，並在 runtime 解析 dispatcher 所屬 team

### 解除追蹤

當目標 agent 回傳 `kind=report` 且 `correlation_id` 匹配時，
daemon 會自動刪除對應的 sidecar，解除追蹤。匹配條件是
`correlation_id` 相等（不是比對 sender）。

這個設計支援多 dispatch 場景：同一個 orchestrator 可以同時
分派給多個 agent，每個 sidecar 獨立追蹤。

### Sidecar 重新整理

在 sidecar 尚為 `pending` 且未超時的狀態下，如果目標 agent
發送了相同 correlation 的非 report 訊息（如 `kind=update`），daemon 會
刷新 sidecar 的 `issued_at` 時間戳（重新計時），避免在
agent 明顯活躍時觸發 false alarm。

---

## 過期清理

### 三類過期 Sidecar

daemon 會在掃描時自動清理過期的 sidecar：

| 類型 | 說明 |
|------|------|
| Placeholder correlation_id | `correlation_id` 是 `t-pending` 等佔位符（lead 端的分派衛生問題） |
| 已刪除的目標 | target instance 已從 fleet 中移除 |
| 已完成的任務 | `correlation_id` 對應的 task board 任務已是 done/cancelled 狀態 |

Fail-open 語義：如果清理過程中讀取 fleet.yaml 或 task board 失敗，
將 sidecar 視為活躍（不清理），維持既有行為。

---

## 可見性查詢

Agent 可以查詢與自己相關的 dispatch idle 狀態：

```json
{
  "tool": "dispatch_idle",
  "action": "list"
}
```

回傳分為兩個視角：

- **as_dispatcher**：以分派者身份，看到自己分派出去的所有 pending/exceeded sidecar
- **as_target**：以被分派者身份，看到針對自己的所有 sidecar

已經過期清理或被 report 解除的 sidecar 不會出現。

---

## 設定

門檻是 compile-time constant；單次 dispatch 可用 `expect_reply_within_secs` 覆寫，沒有環境變數 override。

| Constant | 預設值 | 說明 |
|---------|--------|------|
| `DEFAULT_DISPATCH_THRESHOLD_SECS` | 1800（30 分鐘） | team task dispatch 未指定門檻時的 L1 window |
| `ESCALATE_TO_AGENT_AFTER_SECS` | 600（10 分鐘） | `exceeded_at` 後等待多久才向 agent 發送 L2 nudge |

### 行為細節

- 掃描頻率：每 60 秒一次（與 daemon tick 週期同步）
- 每個 sidecar 最多觸發一次 L1 exceeded 通知和一次 L2 nudge
- L1 通知目標：dispatcher（分派者）
- L2 nudge 目標：target（被分派者）
- 匹配解除用的是 `correlation_id`，不是 sender 或 target

### Concurrency（#1340）

MCP report handler 呼叫的 `mark_resolved`，與 daemon tick 呼叫的
`scan_and_emit`，會以 flock 序列化同一個 sidecar 檔案，避免 lost update。
若沒有這層鎖，並行的 resolve 可能覆寫正在掃描的 sidecar，造成漏發或重複通知。

---

## 典型流程

```
1. lead 分派任務給 dev（expect_reply_within_secs=600）
   → daemon 建立 sidecar (status=pending)

2. 600 秒內 dev 回傳 report（correlation_id 匹配）
   → daemon 刪除 sidecar ✓

---- 或 ----

2. 600 秒內 dev 沒有回應
   → daemon 掃描：elapsed > threshold
   → L1：向 lead 發送 exceeded 通知
   → sidecar 狀態 → exceeded

3. L2 掃描（dispatcher 屬於某個 team）
   → 向 dev 發送 nudge 通知
   → nudge_sent_at 記錄時間

4. dev 收到 nudge，回傳 report
   → daemon 刪除 sidecar ✓
```

---

## 常見問題

### Q: 所有 team 都可以用嗎？

可以。L1 是跨團隊安全的，30 分鐘預設值適用於任何 team member 的 task dispatch，L2 會動態解析 team。Teamless caller 仍可用 `expect_reply_within_secs` 明確啟用 L1，但沒有自動 L2 team nudge。

### Q: 如果 agent 正在工作但還沒完成，會被 nudge 嗎？

如果 agent 有發送 `kind=update` 等非 report 訊息，sidecar 會重新計時。
只有完全沒有任何通訊才會觸發超時。

### Q: sidecar 會一直留在磁碟上嗎？

不會。三種清理機制：
1. 目標回傳 report → 立即刪除
2. 過期清理（placeholder / deleted target / closed task）→ 掃描時刪除
3. daemon 重啟時的 startup sweep
