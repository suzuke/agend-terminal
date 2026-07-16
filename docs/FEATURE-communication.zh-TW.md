[English](FEATURE-communication.md)

# Agent 間通訊系統

AgEnD Terminal 的通訊系統讓 agent 之間能進行結構化的訊息傳遞——委派任務、提出查詢、回報結果、廣播更新。

## 使用情境

> **適用對象：** Agent 基礎設施——agent 透過 MCP 工具使用；operator 通常不直接操作。

**Dev 向 lead 回報完成。** 完成 PR 後，dev agent 呼叫 `send`，帶上 `request_kind=report` 和任務的 `correlation_id`。Lead agent 的收件匣收到回報，dispatch idle tracker 清除 pending sidecar——全程不需要人工協調。

**Lead 委派任務。** Lead agent 在任務看板建立任務，然後使用 `send` 帶上 `request_kind=task`、分支名稱和 `next_after_ci=reviewer`。Daemon 自動為 dev 建立 worktree，CI 通過後 reviewer 會被自動通知——整個接力鏈在一次 `send` 呼叫中就設定完成。

**跨團隊狀態廣播。** 某個 agent 需要通知整個團隊 merge freeze。它呼叫 `send` 帶上 `team=fixup` 和 `request_kind=update`。每個團隊成員都會在收件匣中收到訊息；不需要回覆。發送者會自動從廣播清單中排除。

## 設計理念

多 agent 協作需要一個可靠的通訊管道。直接透過終端輸出傳遞訊息既不結構化也不可追蹤。通訊系統提供：

- **結構化訊息**：每則訊息都有明確的類型（任務/查詢/回報/更新）
- **持久化收件匣**：enqueue 成功後，即使接收方暫時離線，訊息仍會持久保存
- **任務追蹤**：委派的任務可以追蹤進度和結果
- **多種投遞方式**：單一目標、多目標廣播、團隊廣播

---

## 三個核心工具

### `send` — 統一發送

所有訊息發送都透過 `send` 工具。根據參數不同，自動分派到對應的處理路徑。

```json
{
  "instance": "dev",
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

Drain 會把回傳的 batch 標為 `delivering`，而不是 processed。處理完後請用 `{"action":"ack"}` 確認整批，或以 `message_id` 確認單一 row。

### `reply` — 回覆外部頻道

透過 Telegram、Discord 等外部頻道回覆使用者或操作者。

```json
{
  "message": "任務已完成，PR 已建立"
}
```

回覆從 `inbox` 取得的訊息時，請傳入它的 `message_id`，讓 `reply` 依該訊息原本的外部 channel 路由，並在傳送成功後 settle channel-reply obligation。

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

用於指派工作給其他 agent。應盡量先建立並傳入 `task_id`。Broadcast task dispatch 必須提供；目前 single-target 相容路徑在省略時可自動建立 task。

```json
{
  "instance": "dev",
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
| `task_id` | 任務看板 ID（broadcast 必填；所有 dispatch 都建議明確提供） |
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
  "instance": "reviewer",
  "message": "這個 race condition 修復方式正確嗎？",
  "request_kind": "query"
}
```

### report — 回報結果

回報任務結果或審查結論。通常搭配 `correlation_id` 指向原始任務。

```json
{
  "instance": "lead",
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
| `ack_inbox` | 設為 `true` 時，在 send 成功後原子確認 reporter 此 `correlation_id` 的 delivering messages |

### update — 狀態更新

通知性質的訊息，不要求回覆。

```json
{
  "instance": "lead",
  "message": "PR #1187 CI 通過，等待審查",
  "request_kind": "update"
}
```

---

## 投遞模式

### 單一目標

```json
{
  "instance": "dev",
  "message": "..."
}
```

訊息直接投遞到指定 agent 的收件匣。

### 多目標廣播

```json
{
  "instances": ["dev", "reviewer", "tester"],
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

當目標 agent 離線或停止，但 daemon 已接受 send 時：

1. Daemon 將訊息持久寫入收件匣 JSONL log
2. Agent 下次上線並呼叫 `inbox` 時收到

### Daemon 失敗行為

Daemon 是唯一的 messaging authority。MCP bridge 無法連線時，呼叫會明確失敗；bridge 不會自行寫 inbox file，也不會在本機執行 `send`。請等 daemon 恢復後重試。Daemon 已成功 enqueue 的訊息會持久保存在 append-only、加鎖並 fsync 的 JSONL inbox。

### Idempotent Retry

MCP bridge 會為每個 proxied request 產生 UUIDv4 `request_id`。遇到可重試的 transport break 時，它會重新連線，並以同一 ID 精確重試一次。Daemon 依該 ID 去重，因此 transport retry 不會重複 enqueue 同一訊息。Application error 不會走此 transport retry，也沒有本機 fallback。

---

## 收件匣操作

### 取出未讀訊息

```json
// inbox（無參數）
{}
```

回傳未讀訊息，並把該 batch 標為 `delivering`。這是目前 turn 的 delivery lease，不代表訊息已處理完成。

處理後呼叫：

```json
{"action": "ack"}
```

這會把整批 in-flight batch 轉為 `processed`；加入 `message_id` 可只確認單一 row。下一次 drain 會隱式確認前一批。若兩者都沒發生，約十分鐘的 reclaim timeout 可能把未確認 rows 放回 unread 並重新投遞。

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

### 清除陳舊 backlog

```json
{"action": "clear"}
```

`clear` 會精簡清除非 obligation 訊息，並回傳 bounded summary。未回答 query 與未 settle task 仍維持 unread，列在 `requires_response`。

### Discharge 外部回覆義務

```json
{
  "action": "discharge",
  "message_id": "m-...",
  "reason": "已在 incident channel 處理"
}
```

只有在確定不欠 channel reply 時才使用 `discharge`。它會在不回答的情況下持久關閉 obligation、記錄原因並通知 operator。若應實際回答，請使用 `reply`。

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

- `request_kind=task` 的廣播模式（`instances`／`team`／`tags`）**必須**提供 `task_id`
- `request_kind=task` 的單一目標模式目前在未提供 `task_id` 時會自動建立；明確建立仍是穩定、可稽核的流程

取得 `task_id` 的方式：

```json
// 先建立任務
{"action": "create", "title": "修復 #1177", "assignee": "dev"}
// 回傳 task_id = "t-20260525..."

// 再發送帶 task_id 的訊息
{"instance": "dev", "request_kind": "task", "task_id": "t-20260525..."}
```

### task_id 格式

- 前綴 `t-`
- 長度 4-128 字元
- 只允許英數字、連字號、底線

### 逾時監控

透過 `expect_reply_within_secs` 啟用逾時監控：

```json
{
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "expect_reply_within_secs": 600
}
```

如果在指定時間內沒有收到對應的 `request_kind=report`（以 `correlation_id` 配對），daemon 會向發送方的收件匣發出 `dispatch_idle_threshold_exceeded` 通知。

fixup 團隊的任務預設自動啟用 10 分鐘逾時。其他團隊需要明確指定。

---

## 忙碌閘門

當目標 agent 已經有認領（claimed）或進行中（in_progress）的任務時，`request_kind=task` 的投遞會被忙碌閘門擋下。

```json
// 覆蓋忙碌閘門
{
  "instance": "dev",
  "request_kind": "task",
  "force": true,
  "force_reason": "緊急修復，需要立即處理"
}
```

`force` 需要搭配 `force_reason` 說明原因，記錄在審計日誌中。

---

## Worktree 自動綁定

當 `request_kind=task` 帶有 `branch` 參數時，系統自動為目標 agent 建立並綁定 git worktree：

```json
{
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "branch": "fix/1177-sha-gate-empty"
}
```

綁定流程：
1. 從來源 repo checkout 指定分支到 worktree
2. 將目標 agent 綁定到該 worktree
3. 對真正的 feature-branch dispatch 自動啟用 CI monitoring；若有 `next_after_ci`，它提供明確的後續 target

設定 `"bind": false` 可以跳過自動綁定（例如只是通知，不需要工作目錄）。

---

## CI 通知串接

`next_after_ci` 實現 CI 通過後的自動接力：

```json
{
  "instance": "dev",
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
Lead: send(instance=dev, request_kind=task, task_id="t-...", branch="fix/1177", next_after_ci="reviewer")
Dev:  inbox() → 取得任務
Dev:  send(instance=lead, request_kind=report, correlation_id="t-...", ack_inbox=true, message="PR 已建立")
CI 通過 → Reviewer 自動收到 [ci-ready-for-action]
Reviewer: send(instance=lead, request_kind=report, correlation_id="t-...", message="VERIFIED")
```

### 團隊廣播 / 跨 Agent 提問

```
send(team="fixup", request_kind=update, message="今日 merge freeze")
send(instance=reviewer, request_kind=query, message="M3 的 session 刪除時機如何？")
```
