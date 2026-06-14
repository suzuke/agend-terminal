[English](FEATURE-ci-watch.md)

# CI Watch — 自動監控 PR 的 CI 狀態

## 使用情境

> **適用對象：** Agent 基礎設施——agent 透過 MCP tools 使用，operator 通常不直接操作。

**自動 CI 到 reviewer 的交接。** Dev agent 完成實作並建立 PR。Daemon 自動在該 PR 的分支上掛載 CI watch。當所有 GitHub Actions check 通過後，daemon 向 reviewer agent 發送 `[ci-pass]` 通知，reviewer 立即開始 code review——完全不需要人工介入。

**選擇性 workflow 監控。** 一個 repo 有多個 CI workflow，但只有 "build" 和 "test" 是合併的必要條件。Dev 在建立 watch 時指定 `required_checks: ["build", "test"]`，這樣不穩定的 "windows-compat" workflow 不會阻擋 reviewer 的交接。

**衝突早期預警。** 在監控 CI 的同時，daemon 也會檢查 PR 的 mergeable 狀態。如果上游變更導致合併衝突，daemon 會立即向 dev agent 發送 `[ci-conflict-detected]` 通知，讓 dev 可以在 reviewer 浪費時間審查衝突分支之前先完成 rebase。

## 設計初衷

在多 agent 協作的工作流中，dev agent push 完 PR 之後，通常需要等 CI 跑完
才能通知 reviewer 開始 review。如果沒有自動化機制，operator 必須手動盯著
GitHub Actions，等綠燈亮了再去戳 reviewer。

CI Watch 解決這個問題：dev 建立 PR 時自動掛上 CI 監控，daemon 定期輪詢
GitHub（或 GitLab / Bitbucket）的 CI 狀態。CI 通過時自動通知訂閱者，並且
可以透過 `next_after_ci` 參數自動串接下一個 agent——例如直接通知 reviewer
開始工作。

整個流程零人工介入：

```
dev push PR → daemon 自動掛 ci watch →
CI 跑完 → daemon 通知 reviewer →
reviewer 自動開始 review
```

---

## 使用方式

### 訂閱 CI 監控

透過 MCP 工具 `ci action=watch`：

```json
{
  "tool": "ci",
  "action": "watch",
  "repo": "owner/repo",
  "branch": "feat/my-feature",
  "next_after_ci": "reviewer"
}
```

參數說明：

| 參數 | 必填 | 說明 |
|------|------|------|
| `repo` | 是 | GitHub 倉庫（`owner/repo` 格式） |
| `branch` | 是 | 要監控的分支 |
| `next_after_ci` | 否 | CI 通過後自動通知的 agent 名稱 |
| `interval_secs` | 否 | 輪詢間隔（預設 60 秒） |
| `task_id` | 否 | 關聯的 task board ID |
| `required_checks` | 否 | 只追蹤這些 workflow 名稱（其餘忽略） |

### 取消訂閱

```json
{
  "tool": "ci",
  "action": "unwatch",
  "repo": "owner/repo",
  "branch": "feat/my-feature"
}
```

### 查詢狀態

```json
{
  "tool": "ci",
  "action": "status"
}
```

回傳所有活躍的 CI watch 及其最近一次輪詢結果。

### 自動掛載

在 lead 使用 `send` 工具分派任務（`kind=task`）時，如果指定了 `branch` 和
`next_after_ci`，daemon 會自動為該分支掛上 CI watch。dev 不需要手動呼叫
`ci action=watch`。

---

## 運作原理

### 檔案結構

每個 CI watch 對應 `$AGEND_HOME/ci-watches/` 目錄下的一個 JSON 檔案。
檔名是 `{repo}:{branch}` 的 SHA-256 雜湊值（64 hex chars + `.json`），
避免 repo 名稱中的 `/` 造成路徑問題。

### 輪詢機制

daemon 在背景持續執行 CI watch 輪詢迴圈：

1. **掃描 ci-watches 目錄**：讀取所有 `.json` 檔案
2. **節流判斷**：根據 `last_polled_at` + `effective_interval_secs` 決定是否該輪詢
3. **呼叫 CI Provider API**：查詢最新的 workflow run 狀態
4. **狀態比較**：與上次記錄的 `last_run_id` / `head_sha` 比較
5. **通知發送**：如果有新的終態結果（success / failure / cancelled），通知所有訂閱者
6. **狀態持久化**：將最新狀態寫回 watch JSON

### 自適應輪詢間隔

為了避免超過 GitHub API rate limit，daemon 會根據剩餘配額自動調整輪詢間隔：

| 剩餘配額 | 區間 | 乘數 |
|---------|------|------|
| > 50% | Healthy | ×1（使用設定的 interval） |
| 10%–50% | Cautious | ×2 |
| ≤ 10% | Critical | ×4 |

例如設定 60 秒的 watch，在 Critical 區間會變成 240 秒輪詢一次。
上限為設定值的 4 倍，不會更長。

如果 API 沒有回傳 rate limit header（GitLab / Bitbucket），保持原始間隔。

### 通知去重

CI Watch 使用兩層去重機制，避免同一個事件重複通知：

**第一層：Run 選取**
`select_runs_to_notify` 比較最新的 workflow run 與已記錄的 `last_run_id`，
只選取新的終態 run。

**第二層：SHA 去重**
`dedupe_notifications_by_head_sha` 確保同一個 `head_sha` 的結論
（success / failure）只通知一次。處理 `gh run rerun --failed` 的情境：
run_id 不變但 conclusion 改變。

### 多訂閱者支援

一個 CI watch 可以有多個訂閱者。輪詢只執行一次，通知分別送到每個
訂閱者的 inbox。每個通知都帶有 supersede token，如果 inbox 中已有
同一個 repo@branch 的舊通知，新通知會取代舊的（避免通知堆積）。

### Zombie 訂閱者過濾

如果某個訂閱者已經從 fleet 中移除（agent 被刪除），daemon 會在通知時
跳過該訂閱者，避免向不存在的 inbox 寫入。判斷條件是同時不在 agent
registry 和 fleet.yaml 的 instances 中。

---

## PR 衝突偵測

CI Watch 同時監控 PR 的 mergeable 狀態。

### Watch 建立時

`ci action=watch` 時會立即查詢 PR 的 mergeable 狀態。如果偵測到
`CONFLICTING`，會向所有訂閱者送出 `[ci-conflict-detected]` 通知。

### 定期檢查

輪詢過程中也會定期重新檢查 mergeable 狀態。如果狀態從非衝突變成
衝突（transition detection），同樣送出衝突通知。

---

## 停滯偵測

如果 CI watch 連續被 rate limit 跳過（無法輪詢），daemon 會追蹤
連續跳過次數。超過 3 次後，向所有訂閱者送出 `[ci-watch-stalled]`
通知，包含：

- 停滯開始時間
- 預估下次輪詢時間（rate limit reset 時間）
- 設定建議（如何取得更高的 API 配額）

當輪詢恢復正常時，送出 `[ci-watch-resumed]` 通知。

---

## TTL 與自動清理

CI Watch 有兩種過期機制：

### 絕對 TTL

每個 watch 建立時設定 `expires_at`（預設 72 小時後）。超過此時間
無條件移除。

### 不活動 TTL

如果 `last_terminal_seen_at`（最後一次看到終態結果的時間）超過
設定的小時數，watch 會因不活動而被移除。

### 啟動清掃

daemon 啟動時會執行一次 startup sweep，清理上次 daemon 停止期間
過期的 watch。

### 保護分支過濾

對 `main` / `master` 等保護分支的 watch 會被自動移除（保護分支
不應該有 CI watch，因為它們不是 PR 分支）。

---

## CI Provider 支援

CI Watch 支援三種 CI 提供者：

| Provider | 偵測方式 | API |
|----------|---------|-----|
| GitHub | 預設，或 remote URL 含 `github` | GitHub Actions API |
| GitLab | Remote URL 含 `gitlab` | GitLab CI/CD API |
| Bitbucket | Remote URL 含 `bitbucket` | Bitbucket Pipelines API |

Provider 透過 repo URL 自動偵測，也可以在 watch 時透過 `ci_provider`
參數手動指定。

### GitHub Token

GitHub API 需要認證才能避免嚴格的 rate limit。daemon 使用環境變數
`GITHUB_TOKEN` 或 `GH_TOKEN`。如果沒有設定，CI Watch 仍然可以
運作但會更容易觸發 rate limit 停滯。

---

## 常見問題

### Q: CI Watch 監控的是什麼？

監控 GitHub Actions 的 workflow run（或 GitLab CI / Bitbucket Pipelines
的等價物）。當所有 check 都完成時，根據聚合結論（success / failure）
發送通知。

### Q: 可以只監控特定的 workflow 嗎？

可以。使用 `required_checks` 參數指定 workflow 名稱列表。只有這些
workflow 會被納入通過/失敗判斷，其他的（例如不穩定的 Windows CI）
會被忽略。

### Q: CI Watch 會自動取消嗎？

會。三種情況下 watch 會自動移除：
1. 超過絕對 TTL（預設 72 小時）
2. 超過不活動 TTL
3. PR 進入終態（merged / closed）且滿足條件

### Q: 多個 agent 可以訂閱同一個 watch 嗎？

可以。多次呼叫 `ci action=watch` 指定同一個 repo + branch 但不同的
agent 名稱，該 agent 會被加入訂閱者清單。輪詢只執行一次，通知分別
送到每個訂閱者。

### Q: rate limit 怎麼辦？

daemon 的自適應輪詢間隔會自動降速。如果持續被限速，會送出 stall
通知。建議設定 `GITHUB_TOKEN` 環境變數，authenticated 請求的 rate
limit 是每小時 5,000 次（vs 未認證的 60 次）。