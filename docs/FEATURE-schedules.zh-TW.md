[English](FEATURE-schedules.md)

# Schedules & Deployments — 定時任務與批次部署

Schedules 讓你設定 cron 定時任務或一次性排程，Deployments 讓你一鍵部署多 agent 團隊。兩者都不需要手動重複操作。

## 使用情境

> **Target audience:** Both operators and agents.

操作者希望每天早上 9 點自動送出 standup 提醒給 team，這種固定節奏的工作最適合用 cron 排程，因為不需要任何人手動記得觸發時間。

agent 或操作者想在 PR merge 後 30 分鐘做一次 cleanup，這類只會發生一次的工作更適合用一次性排程，而不是重複型 cron。

如果是要一次建立整套多 agent 團隊，deployment 會先把環境搭起來；之後的提醒與跟進工作則交給 schedules。兩者分工不同，但可以一起使用。

## 設計理念

- **Schedules**：每天早上 9 點自動給 team 發 standup 訊息、每小時檢查 CI 狀態——不需要操作者記得做
- **Deployments**：一個指令部署整個團隊（lead + dev + reviewer），包含 worktree 建立、fleet.yaml 寫入、team 建立

---

## Schedules — 定時任務

### 快速開始

```json
// 每天早上 9 點發送 standup 提醒
{
  "action": "create",
  "cron": "0 9 * * *",
  "message": "早安！請回報昨天的進度和今天的計畫。",
  "instance": "lead"
}

// 30 分鐘後執行一次
{
  "action": "create",
  "run_at": "2026-05-25T10:00:00",
  "message": "提醒：PR review deadline 到了",
  "instance": "reviewer"
}
```

### Daemon 管理的 Job 排程

使用 `job` 取代 `instance`，讓 daemon 在到點時建立 worker，負責執行紀錄、重試、完成追蹤與回收，不需要常駐協調者。

```json
{
  "action": "create",
  "cron": "0 15 * * 4,7",
  "timezone": "Asia/Taipei",
  "message": "尋找新 Podcast 集數，沿用已保存逐字稿，產生摘要並依 output_context 推送。依集數 GUID 與目的地保存進度。",
  "job": {
    "backends": ["codex", "claude"],
    "artifact_directory": "/absolute/path/podcast-artifacts",
    "timeout_secs": 3600,
    "max_attempts": 3,
    "retry_delay_secs": 60,
    "output_context": "使用既有設定的 Telegram 與 LINE 目的地推送，各自保存送達紀錄。"
  }
}
```

`backends` 必須是非空、不重複且有順序的標準 backend 名稱列表：`claude`、`codex`、`kiro-cli`、`opencode`、`antigravity-cli` 或 `grok`。不接受任意命令或 shell。`artifact_directory` 必須是絕對路徑。預設值與範圍：`timeout_secs` 3600（60–86400）、`max_attempts` 3（1–10）、`retry_delay_secs` 60（1–3600）；未知 Job 欄位會被拒絕。

`output_context` 是固定的業務推送指示，不是憑證，也不會自動建立通道目的地。Worker 必須使用其工具可存取且已獲授權的目的地設定。每次嘗試有獨立工作目錄；逐字稿、摘要及推送紀錄應保存在 `artifact_directory`，回收 worker 時保留。

選填的 `job.notification` 會在 Run 結束時發出一次狀態通知，與業務推送分開：

```json
"notification": {"channel": "telegram", "chat_id": -1001234567890, "topic_id": 42}
```

請將範例 chat ID 換成 fleet 已設定的 Telegram 群組，topic ID 換成既有主題；省略 `topic_id` 則不指定討論串。Daemon 在設定與發送時核對明確的群組，不從建立者或 worker 推測目的地，也不建立新主題。憑證沿用既有通道環境設定。Telegram 回傳的 message ID 會保存在 Run；發送途中當機或傳輸結果不明時記為 `unknown`，不盲目重送。通知狀態與執行成功、清理分開，可透過 `runs` 核對未知結果。此版狀態通知支援 Telegram；Podcast 的 Telegram／LINE 業務推送仍依任務指示執行。

Job 使用獨立且持久化的排程進度。停機期間錯過的 cron 合併為最近一次到點，不累積 worker；前一 Run 仍在執行時，後續到點記為重疊略過。Job 不走舊的一次性訊息補送或目標 instance 遺失停用流程。切換 backend 前必須觀察到舊 worker 已退出，無法確認退出便不啟動替代者。

使用 `{"action":"runs","id":"<schedule-id>"}` 查看執行紀錄。成功必須由目前 worker 呼叫：

```json
{"action":"complete","run_id":"<run-id>","attempt_id":1,"result":"已完成；成果：/absolute/path/podcast-artifacts/..."}
```

Daemon 核對目前 attempt 與呼叫者身分後保存完成收據。Idle、程序退出、訊息已排入及 task 狀態本身都不代表成功。Task 結案與清理在收據保存後接續處理；這些步驟失敗不會重跑已成功的工作。

Job 與 instance 模式不能互換，需建立新排程。更新 `job` 會替換後續 Run 的設定，既有 Run 保留原快照。Job 不接受 `instance`、`linked_task_id`、`replacement_key` 或 `fire_strategy: "until_success"`。

排程去重不等於外部推送 exactly-once。業務流程必須保存每集、每個目的地的進度；遇到可能已送出但回應遺失時，應先核對再重試不支援冪等的 API。

### 操作

#### create — 建立排程

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `cron` | string | 二選一 | Cron 表達式（重複執行） |
| `run_at` | string | 二選一 | 一次性時間（RFC 3339 或本地時間） |
| `message` | string | 是 | 觸發時發送的訊息內容 |
| `instance` | string | 否 | 目標 agent（預設為建立者自己） |
| `label` | string | 否 | 人類可讀的標籤 |
| `timezone` | string | 否 | IANA 時區；省略時於建立當下偵測 |
| `fire_strategy` | string | 否 | `always`（預設）或 `until_success` |
| `linked_task_id` | string | 視情況 | `until_success` 所需的既有 task ID |

`cron` 和 `run_at` 必須且只能指定一個。

#### list — 列出排程

```json
{"action": "list"}
```

回傳所有排程，預設包含 `next_scheduled_fire_at`、`runs_total` 與最新三筆執行記錄。傳 `full_history: true` 可取得所有保留記錄（最多 50 筆），也可傳 `instance` 依投遞目標篩選。

#### update — 修改排程

| 參數 | 類型 | 說明 |
|------|------|------|
| `id` | string | 排程 ID（必要） |
| `cron` | string | 新的 cron 表達式 |
| `run_at` | string | 改為一次性排程 |
| `message` | string | 新訊息內容 |
| `instance` | string | 新目標 agent |
| `label` | string | 新標籤 |
| `enabled` | bool | 啟用/停用 |
| `timezone` | string | 新 IANA 時區 |
| `fire_strategy` | string | `always` 或 `until_success` |
| `linked_task_id` | string | `until_success` 所連結的 task |

可以在 cron 和一次性之間切換。

#### Fire strategy

`fire_strategy: "always"` 會在每次 cron match 時觸發。使用 `"until_success"` 時，linked task 必須存在；該 task done 後，排程會跳過其時區內同一日剩餘的 matches，下一個 calendar day 再恢復資格。Linked task 後來消失時，排程會停用並記錄 `target_task_missing`。

#### delete — 刪除排程

```json
{"action": "delete", "id": "s-20260525..."}
```

### Cron 格式

支援標準的 5 欄位和 6 欄位 cron 格式：

```
# 5 欄位（系統自動補秒數 0）
分 時 日 月 星期幾
0 9 * * *           → 每天 09:00
30 14 * * 2-6       → 週一到五 14:30
0 */2 * * *         → 每 2 小時

# 6 欄位（秒 分 時 日 月 星期幾）
30 0 9 * * *        → 每天 09:00:30
```

星期幾使用 Quartz 慣例：1=週日, 2=週一, ..., 7=週六。

### 時區處理

每個排程記錄建立時的時區（IANA 格式），cron 表達式在該時區下求值。

偵測順序：
1. `TZ` 環境變數
2. 系統時區（macOS: CoreFoundation, Linux: `/etc/localtime`）
3. 降級為 `UTC`

時區在建立時鎖定，不會因為系統時區變更而改變。

### 觸發機制

Daemon 的主迴圈每 10 秒執行一次 tick：

1. 載入所有已啟用的排程
2. 計算檢查區間：`(上次檢查時間, 現在]`
3. 對每個排程判斷是否應該觸發
4. 觸發時投遞訊息給目標 agent

區間追蹤防止 daemon 重啟時重複觸發。

### 訊息投遞

根據目標 agent 的狀態使用不同投遞方式：

| 狀態 | 投遞方式 | 記錄狀態 |
|------|----------|----------|
| 在線 | 直接注入 PTY stdin | `ok` |
| 離線 | 寫入收件匣 | `ok_inbox` |
| 錯過（daemon 當時沒在跑） | 不投遞 | `missed` |

### 一次性排程

一次性排程（`run_at`）在觸發後自動停用，不會再次觸發。

如果 daemon 在排程時間沒有運行：
- 24 小時內：daemon 啟動時補發（replay）
- 超過 24 小時：標記為 `stale_dropped` 並停用，不補發過時的訊息

### 執行歷史

每個排程最多儲存 50 次執行記錄。`list` 預設只回最新三筆並提供 `runs_total`；設定 `full_history: true` 才回傳全部保留記錄：

```json
{
  "run_history": [
    {"triggered_at": "2026-05-25T09:00:00Z", "status": "ok"},
    {"triggered_at": "2026-05-24T09:00:00Z", "status": "ok_inbox"},
    {"triggered_at": "2026-05-23T09:00:00Z", "status": "missed"}
  ]
}
```

### 儲存

- 位置：`$AGEND_HOME/schedules.json`
- 格式：版本化 JSON（v1 → v2 自動升級）
- 鎖定：flock + atomic write（temp → fsync → rename）

---

## Deployments — 批次部署

### 快速開始

```json
// 部署一個三人團隊
{
  "action": "deploy",
  "template": "fixup-team",
  "directory": "/tmp/fixup-workspace",
  "branch": "main"
}
```

### 部署範本

在 `fleet.yaml` 中定義部署範本：

```yaml
templates:
  fixup-team:
    orchestrator: lead
    instances:
      lead:
        backend: claude
        role: "團隊 orchestrator，負責任務分派和審查結果彙整"
      dev:
        backend: claude
        role: "實作者，負責寫程式碼和修 bug"
      reviewer:
        backend: claude
        role: "審查者，負責 code review"
```

### 操作

#### deploy — 部署

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `template` | string | 是 | 範本名稱（`fleet.yaml` 中定義） |
| `directory` | string | 是 | 工作目錄父路徑 |
| `name` | string | 否 | 部署名稱（預設使用範本名） |
| `branch` | string | 否 | Git 分支（自動建立 worktree） |

部署流程分四個階段：

1. **驗證與 Worktree**：驗證範本，為每個 agent 建立 `<directory>/<name>-<suffix>` 子目錄。如果指定了 `branch`，使用 `git worktree add`
2. **Fleet.yaml 寫入**：將所有 instance 定義寫入 `fleet.yaml`
3. **Agent 啟動**：逐一 spawn 每個 agent
4. **Team 建立**：如果是多 agent 範本，自動建立 team 並指定 orchestrator

#### teardown — 拆除

```json
{
  "action": "teardown",
  "name": "fixup-team"
}
```

拆除流程：
1. 刪除所有 agent instance
2. 清理檔案系統（刪除工作目錄）
3. 從 `fleet.yaml` 移除 instance 定義
4. 刪除 team（如果有）
5. 從部署記錄中移除

如果父目錄在拆除後為空，也會一併清理。

#### list — 列出部署

```json
{"action": "list"}
```

回傳所有部署記錄，包含 instance 清單和建立時間。

### 孤兒部署清理

Daemon 啟動時自動檢查孤兒部署——部署記錄中的 instance 在 `fleet.yaml` 中已不存在的情況。孤兒部署會自動清理相關的 team 和檔案系統。

### 儲存

- 位置：`$AGEND_HOME/deployments.json`
- 格式：版本化 JSON
- 鎖定：flock + atomic write

---

## 典型用法

### 每日 Standup 提醒

```json
{
  "action": "create",
  "cron": "0 9 * * 2-6",
  "message": "早安！請回報：1) 昨天完成了什麼 2) 今天計畫做什麼 3) 有沒有阻塞",
  "instance": "lead",
  "label": "daily-standup"
}
```

### 定期檢查 PR 狀態

```json
{
  "action": "create",
  "cron": "0 */3 * * *",
  "message": "請檢查所有 open PR 的 CI 狀態，回報任何失敗的 check。",
  "instance": "reviewer",
  "label": "pr-health-check"
}
```

### 延遲提醒

```json
{
  "action": "create",
  "run_at": "2026-05-25T15:00:00",
  "message": "提醒：今天 3 點有 release cut，確認所有 PR 已合併",
  "instance": "lead"
}
```

### 一鍵部署團隊

```json
{
  "action": "deploy",
  "template": "fixup-team",
  "directory": "/tmp/sprint-59",
  "branch": "main",
  "name": "sprint-59"
}
```

部署完成後，三個 agent 各自在 `/tmp/sprint-59/sprint-59-lead`、`/tmp/sprint-59/sprint-59-dev`、`/tmp/sprint-59/sprint-59-reviewer` 目錄工作，team 已建立，lead 為 orchestrator。

### 工作結束後拆除

```json
{
  "action": "teardown",
  "name": "sprint-59"
}
```

一個指令清理所有 agent、team、工作目錄和 fleet.yaml 記錄。

---

## 何時用 Schedules vs Deployments

當你需要某個訊息或動作在稍後發生時，使用 **Schedules**。

當你需要現在就建立一個可重複使用的團隊配置時，使用 **Deployments**。

一個好用的判斷準則：

- 如果問題是「這件事什麼時候該發生？」用 schedules
- 如果問題是「現在該存在什麼？」用 deployments

---

## 失效情境

### 無效的 cron

如果 cron 表達式無法解析，建立會立即失敗。重試前請先修正表達式。

### Instance 遺失

如果排程應該觸發給特定 agent，但 `instance` 錯誤，就無法有效投遞。請對照 fleet 驗證 instance 名稱。刪除目標 instance 時，相關排程會被停用並留下 orphaned history entry，而不是直接刪除。

### 部署範本不符

如果部署範本和 fleet 結構不符，產生的配置可能不完整或只填了一部分。請把範本視為該次部署形狀的真實來源。

---

## Source Pointers

- `src/schedules.rs`：排程儲存與驗證
- `src/deployments.rs`：部署編排
- `src/main.rs`：CLI 子指令路由
- `src/mcp/handlers/schedule.rs`：MCP 介面
- `src/daemon/cron_tick.rs`：排程評估與投遞

---

## 實務建議

1. 對於有 deadline 驅動的工作，優先使用一次性排程。
2. 加上幾週後在 log 裡仍看得懂的標籤。
3. deployments 用於可重複的 fleet 配置，而不是臨時提醒。
4. 除非有充分理由，否則讓 cron 表達式保持簡單。

Daemon 突然中斷時，舊 worker 的子程序可能尚未結束。執行器會先保存啟動意圖，再記錄 PID 與程序出生識別。若重啟後沒有可用的 child handle，也沒有已確認停止的持久紀錄，Run 會標示 `recovery_required` 並保留工作目錄，不另開重複 worker。PID 消失或被重用，不能單獨證明所有子程序都已退出；這類不確定情況需要操作人員復原，不會被誤報為自動重試成功。
