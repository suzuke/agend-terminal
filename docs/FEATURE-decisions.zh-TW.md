# Decisions — 決策追溯系統

Decisions 系統讓團隊記錄重要的架構和流程決策，提供可查詢的決策歷史，讓任何人都能追溯「為什麼我們做了這個選擇」。

## 設計理念

多 agent 協作中，決策散落在對話、PR 描述、commit message 裡，很難回溯。Decisions 提供：

- **集中記錄**：所有重要決策存在同一個地方
- **結構化資料**：標題、內容、範圍、標籤、作者
- **修正追蹤**：新決策可以明確取代舊決策（supersedes）
- **自動過期**：TTL 機制避免過時決策造成混淆

---

## 快速開始

```json
// 記錄一個決策
{
  "action": "post",
  "title": "使用 prefix match 比對 SHA",
  "content": "reviewed_head 使用 7 字元以上的 prefix match，而非完整 SHA 比對。原因：git log --oneline 預設顯示 7 字元。",
  "scope": "project",
  "tags": ["sha-gate", "sprint-58"]
}

// 列出所有有效決策
{
  "action": "list"
}

// 修正先前的決策
{
  "action": "post",
  "title": "SHA prefix match 改為最少 7 字元",
  "content": "原先沒有最短長度限制，空字串會繞過。現在要求至少 7 字元。",
  "scope": "project",
  "tags": ["sha-gate", "sprint-58"],
  "supersedes": "d-20260525040000000000-1"
}
```

---

## 操作

### post — 記錄決策

建立一筆新決策。

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `title` | string | 是 | 決策標題 |
| `content` | string | 是 | 決策內容和理由 |
| `scope` | string | 是 | `"project"`（專案級）或 `"fleet"`（團隊級） |
| `tags` | string[] | 否 | 分類標籤 |
| `ttl_days` | number | 否 | 自動過期天數（預設 90 天） |
| `supersedes` | string | 否 | 被取代的決策 ID |

回應包含自動產生的決策 ID：

```json
{
  "id": "d-20260525040000000000-1",
  "status": "created"
}
```

### list — 查詢決策

列出所有有效決策。

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `tags` | string[] | 否 | 按標籤篩選（任一符合即納入） |
| `include_archived` | bool | 否 | 是否包含已封存的決策（預設 false） |

結果按建立時間倒序排列（最新的在前）。

### update — 修改決策

修改現有決策的內容、標籤、或狀態。

| 參數 | 類型 | 必要 | 說明 |
|------|------|------|------|
| `id` | string | 是 | 決策 ID |
| `content` | string | 否 | 新內容 |
| `tags` | string[] | 否 | 新標籤 |
| `ttl_days` | number | 否 | 新的過期天數 |
| `archive` | bool | 否 | 設為 true 手動封存 |

修改權限：只有原作者或其所屬團隊的 orchestrator 可以修改。

---

## 決策結構

每筆決策包含以下欄位：

```json
{
  "id": "d-20260525040000000000-1",
  "title": "使用 prefix match 比對 SHA",
  "content": "reviewed_head 使用 7 字元以上的 prefix match...",
  "scope": "project",
  "author": "fixup-dev-2",
  "tags": ["sha-gate", "sprint-58"],
  "ttl_days": 90,
  "created_at": "2026-05-25T04:00:00Z",
  "updated_at": "2026-05-25T04:00:00Z",
  "archived": false,
  "supersedes": null
}
```

### 決策 ID 格式

`d-<時間戳微秒>-<序號>`，例如 `d-20260525040000000000-1`。微秒精度加上原子計數器保證唯一性。

### Scope（範圍）

- `project`：專案級決策，與當前工作目錄相關
- `fleet`：團隊級決策，跨專案的共通規則

Scope 目前作為元資料使用，不影響存取權限。

---

## Supersedes（取代機制）

當需要修正先前的決策時，使用 `supersedes` 建立新舊關聯：

```json
{
  "action": "post",
  "title": "SHA 最短長度改為 7 字元",
  "content": "修正 d-20260525040000000000-1，加入最短長度檢查",
  "supersedes": "d-20260525040000000000-1"
}
```

執行流程：

1. 取得舊決策的鎖定
2. 將舊決策標記為 `archived: true`
3. 更新舊決策的 `updated_at`
4. 建立新決策，記錄 `supersedes` 指向舊 ID

這整個流程在檔案鎖下原子執行，不會有兩個 agent 同時取代同一筆決策的 race condition。

`list` 預設不顯示已封存的決策。要查看完整歷史（包含被取代的舊決策），使用 `include_archived: true`。

---

## 標籤系統

標籤是任意字串陣列，用於分類和篩選：

```json
{
  "tags": ["sha-gate", "sprint-58", "security"]
}
```

查詢時，`tags` 篩選使用「任一符合」邏輯——只要決策包含篩選標籤中的任何一個就會被納入。

### 受保護標籤

在 `fleet.yaml` 中可以定義受保護的標籤：

```yaml
retention:
  protected_decision_tags:
    - SPRINT_99
    - ARCHITECTURE
```

帶有受保護標籤的決策不會被自動過期，無論 TTL 設定為何。適合用於長期有效的架構決策。

---

## 自動過期

決策有 TTL（Time To Live）機制，過期的決策會自動封存：

| 參數 | 預設值 | 說明 |
|------|--------|------|
| 預設 TTL | 90 天 | 未指定 `ttl_days` 時的過期時間 |
| 最低保護期 | 14 天 | 不論 TTL 多短，至少保留 14 天 |
| 受保護標籤 | — | 有受保護標籤的決策永不過期 |

過期流程：

1. Daemon 定期掃描所有決策
2. 跳過：已封存、建立不足 14 天、帶受保護標籤
3. 符合過期條件的決策移至 `decisions/.archive/`

需要設定環境變數 `AGEND_RETENTION_CUTOVER=1` 才會啟用自動過期掃描。

---

## 儲存方式

- 位置：`$AGEND_HOME/decisions/`
- 格式：每筆決策一個 JSON 檔案（`{id}.json`）
- 鎖定：每筆決策有獨立的 flock（`{id}.lock`），不影響其他決策的併發操作
- 寫入：使用 `atomic_write()`（暫存檔 → fsync → rename），crash-safe
- 封存：過期的決策移至 `decisions/.archive/` 子目錄

---

## TUI 檢視

在 TUI 中按 `Ctrl+B D`（大寫 D）開啟決策面板：

- `j` / `k` 或 `↑` / `↓`：上下捲動
- `PgUp` / `PgDn`：快速捲動
- `q` / `Esc`：關閉面板

面板顯示每筆決策的標題、作者、時間戳、內容和標籤。選中的決策會展開完整內容。

---

## 決策逾時（Decision Timeout）

Agent 可以在 `reply` 中設定自動決策：

```json
{
  "text": "是否要繼續使用精簡方案？",
  "default_action": "proceed-with-lean",
  "timeout_secs": 1800
}
```

如果操作者在 30 分鐘內沒有回覆，daemon 自動執行 `default_action`。

流程：
1. Agent 呼叫 `reply` 帶 `default_action` 和 `timeout_secs`
2. 建立 pending decision sidecar（`pending-decisions/{id}.json`）
3. 操作者回覆 → 標記為 `resolved`，取消自動執行
4. 逾時 → 標記為 `timeout`，發送帶有 default action 的通知到 agent 收件匣

同一個 agent 同時只能有一個 pending decision。新的 pending 會自動取消前一個。

---

## 修改權限

| 角色 | 權限 |
|------|------|
| 原作者 | 可修改自己建立的決策 |
| 團隊 orchestrator | 可修改所屬團隊成員建立的決策 |
| 其他 agent | 不可修改，回傳授權錯誤 |

---

## 典型用法

### 記錄架構決策

```json
{
  "action": "post",
  "title": "Agent 間通訊使用 inbox JSONL 而非 RPC",
  "content": "選擇 append-only JSONL 因為：1) crash-safe 2) 離線 agent 可延遲讀取 3) 除錯時 cat 就能看。RPC 需要兩端都在線，且 crash 時訊息遺失。",
  "scope": "fleet",
  "tags": ["architecture", "communication"]
}
```

### 追溯決策原因

```json
{
  "action": "list",
  "tags": ["sha-gate"]
}
```

### 修正錯誤的決策

```json
{
  "action": "post",
  "title": "SHA gate 需要最少 7 字元（修正）",
  "content": "原決策未考慮空字串情境。空字串是任何字串的 prefix，會繞過所有驗證。",
  "supersedes": "d-20260525040000000000-1",
  "tags": ["sha-gate", "security"]
}
```
