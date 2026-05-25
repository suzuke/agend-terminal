# Schedules & Deployments — 定時任務與批次部署

Schedules 讓你設定 cron 定時任務或一次性排程，Deployments 讓你一鍵部署多 agent 團隊。兩者都不需要手動重複操作。

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
  "target": "lead"
}

// 30 分鐘後執行一次
{
  "action": "create",
  "run_at": "2026-05-25T10:00:00",
  "message": "提醒：PR review deadline 到了",
  "target": "reviewer"
}
```

### 操作

#### create — 建立排程

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `cron` | string | 二選一 | Cron 表達式（重複執行） |
| `run_at` | string | 二選一 | 一次性時間（RFC 3339 或本地時間） |
| `message` | string | 是 | 觸發時發送的訊息內容 |
| `target` | string | 否 | 目標 agent（預設為建立者自己） |
| `label` | string | 否 | 人類可讀的標籤 |

`cron` 和 `run_at` 必須且只能指定一個。

#### list — 列出排程

```json
{"action": "list"}
```

回傳所有排程，包含執行歷史（最近 50 次）。

#### update — 修改排程

| 參數 | 類型 | 說明 |
|------|------|------|
| `id` | string | 排程 ID（必要） |
| `cron` | string | 新的 cron 表達式 |
| `run_at` | string | 改為一次性排程 |
| `message` | string | 新訊息內容 |
| `target` | string | 新目標 agent |
| `label` | string | 新標籤 |
| `enabled` | bool | 啟用/停用 |

可以在 cron 和一次性之間切換。

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
30 14 * * 1-5       → 週一到五 14:30
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

每個排程保留最近 50 次執行記錄：

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
  "cron": "0 9 * * 1-5",
  "message": "早安！請回報：1) 昨天完成了什麼 2) 今天計畫做什麼 3) 有沒有阻塞",
  "target": "lead",
  "label": "daily-standup"
}
```

### 定期檢查 PR 狀態

```json
{
  "action": "create",
  "cron": "0 */3 * * *",
  "message": "請檢查所有 open PR 的 CI 狀態，回報任何失敗的 check。",
  "target": "reviewer",
  "label": "pr-health-check"
}
```

### 延遲提醒

```json
{
  "action": "create",
  "run_at": "2026-05-25T15:00:00",
  "message": "提醒：今天 3 點有 release cut，確認所有 PR 已合併",
  "target": "lead"
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
