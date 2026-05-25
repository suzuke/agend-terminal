# Agent 間通訊系統

AgEnD Terminal 的通訊系統讓 agent 之間能進行結構化的訊息傳遞——委派任務、提出查詢、回報結果、廣播更新。

## 設計理念

多 agent 協作需要一個可靠的通訊管道。直接透過終端輸出傳遞訊息既不結構化也不可追蹤。通訊系統提供：

- **結構化訊息**：每則訊息都有明確的類型（任務/查詢/回報/更新）
- **持久化收件匣**：即使接收方暫時離線，訊息也不會遺失
- **任務追蹤**：委派的任務可以追蹤進度和結果
- **多種投遞方式**：單一目標、多目標廣播、團隊廣播

---

## 三個核心工具

### `send` — 統一發送

所有訊息發送都透過 `send` 工具。根據參數不同，自動分派到對應的處理路徑。

```json
{
  "target_instance": "dev",
  "message": "請修復 #123 的回歸問題",
  "request_kind": "task",
  "task_id": "t-20260525..."
}
```

### `inbox` — 接收訊息

查看收件匣中的待處理訊息。

```json
{}
```

無參數呼叫會取出所有未讀訊息。也可以查詢特定訊息狀態或對話串。

### `reply` — 回覆外部頻道

透過 Telegram、Discord 等外部頻道回覆使用者或操作者。

```json
{
  "text": "任務已完成，PR 已建立"
}
```

---

## 訊息類型（request_kind）

每則訊息都有一個 `request_kind`，決定接收方的處理義務和系統行為：

| 類型 | 用途 | 回覆義務 |
|------|------|----------|
| `task` | 委派工作給其他 agent | 完成後需回報結果 |
| `query` | 向其他 agent 提問 | 需要回覆答案 |
| `report` | 回報任務結果或審查結論 | 通常不需回覆 |
| `update` | 通知狀態更新 | 不需回覆 |

### task — 任務委派

用於指派工作給其他 agent。必須搭配 `task_id`（從任務看板取得）。

```json
{
  "target_instance": "dev",
  "message": "修復 sha_gate.rs 中空字串繞過的問題",
  "request_kind": "task",
  "task_id": "t-20260525040842727169-9",
  "branch": "fix/1177-sha-gate-empty",
  "success_criteria": "修復完成 + cargo test 通過 + PR 建立"
}
```

task 類型支援額外的任務管理參數：

| 參數 | 說明 |
|------|------|
| `task_id` | 任務看板 ID（必要，透過 `task action=create` 取得） |
| `branch` | 指定 git 分支（自動綁定 worktree） |
| `success_criteria` | 完成標準 |
| `eta_minutes` | 預期完成時間 |
| `force` / `force_reason` | 覆蓋忙碌閘門（需要說明原因） |
| `expect_reply_within_secs` | 逾時監控（秒數） |
| `next_after_ci` | CI 通過後自動通知的下一個 agent |

### query — 提問

向其他 agent 詢問問題。接收方需要回覆。

```json
{
  "target_instance": "reviewer",
  "message": "這個 race condition 修復方式正確嗎？",
  "request_kind": "query"
}
```

### report — 回報結果

回報任務結果或審查結論。通常搭配 `correlation_id` 指向原始任務。

```json
{
  "target_instance": "lead",
  "message": "審查完成。VERIFIED — 4M/2L/1I。",
  "request_kind": "report",
  "correlation_id": "t-20260525040842727169-9",
  "parent_id": "m-20260525044640824746-72",
  "reviewed_head": "1c78314"
}
```

| 參數 | 說明 |
|------|------|
| `correlation_id` | 對應的任務 ID（用於追蹤配對） |
| `parent_id` | 回覆的訊息 ID（對話串關聯） |
| `reviewed_head` | 審查時的 git HEAD SHA |

### update — 狀態更新

通知性質的訊息，不要求回覆。

```json
{
  "target_instance": "lead",
  "message": "PR #1187 CI 通過，等待審查",
  "request_kind": "update"
}
```

---

## 投遞模式

### 單一目標

```json
{
  "target_instance": "dev",
  "message": "..."
}
```

訊息直接投遞到指定 agent 的收件匣。

### 多目標廣播

```json
{
  "targets": ["dev", "reviewer", "tester"],
  "message": "..."
}
```

同一則訊息投遞給多個指定的 agent。

### 團隊廣播

```json
{
  "team": "fixup",
  "message": "..."
}
```

投遞給指定團隊的所有成員。

### 標籤廣播

```json
{
  "tags": ["backend"],
  "message": "..."
}
```

根據標籤篩選投遞對象。

廣播模式下，發送方自動排除在接收清單之外。每則訊息會附帶 `broadcast_context`，讓接收方知道這是一對多的訊息。

---

## 訊息投遞機制

### PTY 注入（預設）

當目標 agent 正在運行：

1. 訊息寫入收件匣（append-only JSONL）
2. 同時注入一行提示到 agent 的終端：
   ```
   [AGEND-MSG-PENDING] id=m-20260525... kind=task from=lead inbox=1
   ```
3. Agent 看到提示後，呼叫 `inbox` 工具讀取完整訊息

### 收件匣備用

當目標 agent 不在線上（未啟動、跨團隊、daemon 離線）：

1. 訊息直接寫入收件匣 JSONL 檔案
2. Agent 下次上線並呼叫 `inbox` 時收到

### 失敗降級

當 daemon API 呼叫失敗：

1. 自動降級為直接寫入收件匣檔案
2. 解析對話串（從 `parent_id` 推導 `thread_id`）
3. 記錄投遞模式為 `inbox_fallback`

無論哪種模式，訊息都不會遺失。收件匣使用 append-only JSONL 格式和檔案鎖，確保並行寫入的安全性。

---

## 收件匣操作

### 取出未讀訊息

```json
// inbox（無參數）
{}
```

回傳所有未讀訊息，並標記為已讀。已讀訊息不會在下次呼叫時重複出現。

### 查詢特定訊息狀態

```json
// inbox（帶 message_id）
{
  "message_id": "m-20260525042040527659-39"
}
```

回傳訊息的投遞狀態：已讀（含時間和投遞方式）、未讀過期、或找不到。

### 取得對話串

```json
// inbox（帶 thread_id）
{
  "thread_id": "m-20260525035931943006-17"
}
```

回傳指定對話串中的所有訊息，包含已讀和未讀。

---

## 對話串

訊息可以透過 `thread_id` 和 `parent_id` 組成對話串：

```
訊息 A (id=m-001, thread_id=null)       ← 對話串起始
  └─ 訊息 B (parent_id=m-001)           ← thread_id 自動繼承為 m-001
      └─ 訊息 C (parent_id=m-002)       ← thread_id 繼承 m-001
```

規則：
- 指定 `parent_id` 但未指定 `thread_id` 時，自動從父訊息繼承 `thread_id`
- 如果父訊息本身沒有 `thread_id`，父訊息的 `id` 成為 thread root

---

## 任務看板整合

### task_id 要求

Sprint 58 Wave 4 引入了反停滯（anti-stall）契約：

- `kind=task` 的廣播模式（team/targets/tags）**必須**提供 `task_id`
- `kind=task` 的單一目標模式，如果未提供 `task_id`，系統自動建立

取得 `task_id` 的方式：

```json
// 先建立任務
{"action": "create", "title": "修復 #1177", "assignee": "dev"}
// 回傳 task_id = "t-20260525..."

// 再發送帶 task_id 的訊息
{"target_instance": "dev", "request_kind": "task", "task_id": "t-20260525..."}
```

### task_id 格式

- 前綴 `t-`
- 長度 4-128 字元
- 只允許英數字、連字號、底線

### 逾時監控

透過 `expect_reply_within_secs` 啟用逾時監控：

```json
{
  "target_instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "expect_reply_within_secs": 600
}
```

如果在指定時間內沒有收到對應的 `kind=report`（以 `correlation_id` 配對），daemon 會向發送方的收件匣發出 `dispatch_idle_threshold_exceeded` 通知。

fixup 團隊的任務預設自動啟用 10 分鐘逾時。其他團隊需要明確指定。

---

## 忙碌閘門

當目標 agent 已經有認領（claimed）或進行中（in_progress）的任務時，`kind=task` 的投遞會被忙碌閘門擋下。

```json
// 覆蓋忙碌閘門
{
  "target_instance": "dev",
  "request_kind": "task",
  "force": true,
  "force_reason": "緊急修復，需要立即處理"
}
```

`force` 需要搭配 `force_reason` 說明原因，記錄在審計日誌中。

---

## Worktree 自動綁定

當 `kind=task` 帶有 `branch` 參數時，系統自動為目標 agent 建立並綁定 git worktree：

```json
{
  "target_instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "branch": "fix/1177-sha-gate-empty"
}
```

綁定流程：
1. 從來源 repo checkout 指定分支到 worktree
2. 將目標 agent 綁定到該 worktree
3. 如果設定了 `next_after_ci`，自動監控 CI 結果

設定 `"bind": false` 可以跳過自動綁定（例如只是通知，不需要工作目錄）。

---

## CI 通知串接

`next_after_ci` 實現 CI 通過後的自動接力：

```json
{
  "target_instance": "dev",
  "request_kind": "task",
  "branch": "fix/1177",
  "next_after_ci": "reviewer"
}
```

流程：
1. Lead 委派任務給 dev，指定 `next_after_ci=reviewer`
2. Dev 完成工作，推送 PR
3. CI 通過後，daemon 自動通知 reviewer
4. Reviewer 收到 `[ci-ready-for-action]` 訊息

不需要手動設定 CI watch——委派時一步到位。

---

## 典型通訊流程

### 完整的任務委派循環

```
Lead: task(create, title="修復 #1177") → task_id="t-..."
Lead: send(target=dev, kind=task, task_id="t-...", branch="fix/1177", next_after_ci="reviewer")
Dev:  inbox() → 取得任務
Dev:  send(target=lead, kind=report, correlation_id="t-...", message="PR 已建立")
CI 通過 → Reviewer 自動收到 [ci-ready-for-action]
Reviewer: send(target=lead, kind=report, correlation_id="t-...", message="VERIFIED")
```

### 團隊廣播 / 跨 Agent 提問

```
send(team="fixup", kind=update, message="今日 merge freeze")
send(target=reviewer, kind=query, message="M3 的 session 刪除時機如何？")
```
