# Dispatch Idle Tracking — 任務回應超時追蹤

## 設計初衷

在多 agent 協作中，orchestrator（通常是 lead）會分派任務給 dev 或 reviewer。
但如果接收方沒有回應——可能是 agent 卡住了、crash 了、或被其他工作佔用——
orchestrator 不會知道任務被遺漏了。

Dispatch Idle Tracking 解決這個問題。它在分派任務時啟動一個計時器，如果在
設定的時間窗口內沒有收到回報（`kind=report`），daemon 會自動通知分派者：
「你 10 分鐘前派出去的任務還沒有回應」。

這是一個兩層（L1 + L2）的系統：

- **L1（跨團隊安全）**：通知分派者本人，不含任何團隊名稱或團隊邏輯
- **L2（團隊特化）**：針對 fixup 團隊的額外 nudge，通知目標 agent

---

## 使用方式

### 啟用追蹤

在使用 `send` 工具分派任務時，加上 `expect_reply_within_secs` 參數：

```json
{
  "tool": "send",
  "target_instance": "dev",
  "message": "請實作 #123 的修正",
  "request_kind": "task",
  "task_id": "t-20260525-1",
  "expect_reply_within_secs": 600
}
```

這會建立一個 dispatch idle sidecar，daemon 在 600 秒（10 分鐘）後
檢查是否有對應的 `kind=report`（以 `correlation_id` 匹配）。

### Fixup 團隊預設值

Fixup 團隊的 `kind=task` 和 `kind=query` dispatch 自動繼承 10 分鐘
的 idle 追蹤，不需要手動指定 `expect_reply_within_secs`。其他團隊
必須明確設定。

### 參數說明

| 參數 | 型別 | 說明 |
|------|------|------|
| `expect_reply_within_secs` | int | 預期回覆的秒數，超過後觸發 alert |

---

## 運作原理

### Sidecar 機制

每次帶有 `expect_reply_within_secs` 的 dispatch 會在
`$AGEND_HOME/dispatch-pending/` 目錄下建立一個 JSON sidecar 檔案。

Sidecar 內容：

```json
{
  "dispatch_id": "disp-1716616000000-1",
  "dispatcher": "fixup-lead",
  "target": "fixup-dev",
  "correlation_id": "t-20260525-1",
  "threshold_secs": 600,
  "created_at": "2026-05-25T05:00:00Z",
  "status": "pending",
  "nudge_sent_at": null
}
```

`dispatch_id` 由微秒時間戳 + 程序內原子計數器生成，確保唯一性。

### L1：超時偵測與通知

daemon 每 60 秒掃描一次 `dispatch-pending/` 目錄：

1. 讀取所有 `pending` 狀態的 sidecar
2. 計算 `elapsed = now - created_at`
3. 如果 `elapsed > threshold_secs`：
   - 向 **分派者**（dispatcher）的 inbox 發送 `dispatch_idle_threshold_exceeded` 通知
   - 將 sidecar 狀態改為 `exceeded`
   - 記錄到 event log

通知內容包含分派 ID、目標 agent、經過時間、correlation_id。

### L2：Fixup Nudge

L2 是 L1 的團隊特化補充，僅對 fixup 團隊生效。

當 sidecar 狀態變為 `exceeded` 後，L2 會額外向 **目標 agent**
（被分派者）發送 `dispatch_idle_nudge` 通知，提醒它有任務待處理。

去重機制：每個 sidecar 只 nudge 一次（記錄在 `nudge_sent_at` 欄位）。

L2 的隔離保證：
- L1 的程式碼中不含任何團隊名稱字串（有 CI 測試 `no_team_name_strings_in_l1` 強制保證）
- L2 作為獨立模組 (`fixup_nudge.rs`) 載入，只在分派者屬於 fixup 團隊時生效

### 解除追蹤

當目標 agent 回傳 `kind=report` 且 `correlation_id` 匹配時，
daemon 會自動刪除對應的 sidecar，解除追蹤。匹配條件是
`correlation_id` 相等（不是比對 sender）。

這個設計支援多 dispatch 場景：同一個 orchestrator 可以同時
分派給多個 agent，每個 sidecar 獨立追蹤。

### Sidecar 重新整理

在 sidecar 尚為 `pending` 且未超時的狀態下，如果目標 agent
發送了非 report 類型的訊息（如 `kind=update`），daemon 會
刷新 sidecar 的 `created_at` 時間戳（重新計時），避免在
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

### 環境變數

| 環境變數 | 預設值 | 說明 |
|---------|--------|------|
| `AGEND_DISPATCH_IDLE_THRESHOLD_SECS` | 600 | Fixup 團隊的預設超時秒數 |

### 行為細節

- 掃描頻率：每 60 秒一次（與 daemon tick 週期同步）
- 每個 sidecar 最多觸發一次 L1 exceeded 通知和一次 L2 nudge
- L1 通知目標：dispatcher（分派者）
- L2 nudge 目標：target（被分派者）
- 匹配解除用的是 `correlation_id`，不是 sender 或 target

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

3. L2 掃描（fixup 團隊）
   → 向 dev 發送 nudge 通知
   → nudge_sent_at 記錄時間

4. dev 收到 nudge，回傳 report
   → daemon 刪除 sidecar ✓
```

---

## 常見問題

### Q: 非 fixup 團隊可以用嗎？

可以。L1 是跨團隊安全的，任何 agent 都可以使用 `expect_reply_within_secs`。
只是 L2 的 nudge 功能目前僅對 fixup 團隊生效。

### Q: 如果 agent 正在工作但還沒完成，會被 nudge 嗎？

如果 agent 有發送 `kind=update` 等非 report 訊息，sidecar 會重新計時。
只有完全沒有任何通訊才會觸發超時。

### Q: sidecar 會一直留在磁碟上嗎？

不會。三種清理機制：
1. 目標回傳 report → 立即刪除
2. 過期清理（placeholder / deleted target / closed task）→ 掃描時刪除
3. daemon 重啟時的 startup sweep
