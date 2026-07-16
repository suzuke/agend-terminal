[English](FEATURE-task-board.md)

# Task Board

Task board 是 fleet 共用的工作追蹤介面。它採用 event-sourced 模型：每次變更都會附加到 `task_events.jsonl`，目前狀態則透過 replay（fold）這些 event 重建。這使 board 具有完整 audit trail、狀態可重現，並為 sweep、health 與 dependency evaluation 提供基礎。

## 使用情境

> **適用對象：** Agent infrastructure——agent 透過 MCP 工具使用；operator 通常不會直接操作。

**Task 派發與追蹤。** Lead agent 在 board 上建立 task，設定 title、priority 與 assignee，再派發給 dev agent。Dev 會在 inbox 收到 task、透過 `task action=claim` 認領並開始工作。隨工作推進，dev 將 task 狀態更新為 `in_progress`。完成後，dev 以結果摘要將 task 標記為 `done`。整個生命週期都會以 append-only event 記錄在 `task_events.jsonl`。

**跨 agent 可見性。** Reviewer agent 查看 task board，找出已標記為 `done`、可供驗證的 task。Review 關聯 PR 後，reviewer 將 task 標記為 `verified`。同時，operator 隨時都能透過 TUI 觀察完整 board 狀態，不必逐一查詢 agent。

**Board 衛生。** Task 會隨時間累積——有些 owner 已不在 fleet 中（ghost owner），有些則超過期限。Operator 執行 `task action=health` 取得 board health snapshot，再以 dry-run 模式執行 `task action=sweep`，先找出清理候選再套用變更。

## 1. 設計理念

- Task board 是所有 agent 共用的唯一真相來源。
- Lead 用它派發工作；agent 認領並執行；reviewer 驗證。
- Operator 可隨時觀察全域狀態。
- 它是 append-only、可 replay 的 state machine，不是可直接修改的 JSON 檔。
- `task_events.jsonl` 是 canonical source；`tasks.json` 是 legacy bridge。
- 所有新 read 都透過 replay-fold；所有新 write 都透過 event append。

## 2. 檔案與模組

- `src/task_events.rs`——event 格式定義與 replay 邏輯。
- `src/tasks/mod.rs`——board 介面與 legacy data bridge。
- `src/mcp/handlers/task.rs`——將請求委派給 `tasks` module 的薄 MCP handler。
- `task_events.jsonl`——canonical event log。
- `task_events_archive/`——歷史 archive 目錄。
- `tasks.json`——legacy bridge 檔案（migration 期間唯讀）。
- `TaskBoardState`——replay-fold 的輸出。
- `TaskRecord`——來自 replay 的 canonical read model。
- `TaskEvent`——write model。
- `TaskId` 與 `InstanceName` 是 newtype，可避免混用 ID。
- Replay 遇到未知 event variant 或不支援的 schema version 會失敗（fail-closed）。
- Append 使用每個 emitter 單調遞增的 sequence number 排序。

## 3. 資料模型

- `Task` 是透過 MCP 公開的 structure（為相容性使用 string status）。
- `TaskRecord` 是 replay 產生的 canonical view（使用 enum status）。
- 主要 `TaskEvent` variant：
  - `Created`——攜帶 title、description、priority，以及 optional owner/due_at/depends_on/routed_to/branch/bind/eta_secs。
  - `Claimed`——設定 owner。
  - `InProgress`——設定 owner 並標記工作已開始。
  - `Done`——轉換至 Done status。
  - `Cancelled`——轉換至 Cancelled status。
  - `Blocked` / `Unblocked`——設定或清除 block reason。
  - `Released`——清除 owner 與 routed_to。
  - `Reopened`——重新開啟已完成 task（保留 owner）。
  - `OwnerAssigned`——只變更 owner/routed_to。
  - `PriorityChanged`——只變更 priority。
  - `Verified`——記錄 reviewer 核准，但不關閉 task。
  - `Linked`——附加 PR link。
  - `TaskCloseProposed`——需要 review 的 close proposal。
- 有些功能會組合既有 metadata，而不新增 variant——例如 plan-ack gate（§10）以 `MetadataSet` 儲存 `plan_ack_required`/`plan_ack_reason`/`plan_acks`。

## 4. 狀態語意

| Status | 意義 |
|--------|------|
| `backlog` | 已記錄，但尚不可執行 |
| `open` | 等待認領 |
| `claimed` | 已有人取得 ownership |
| `in_progress` | 已開始執行 |
| `in_review` | 實作正在等待 review |
| `verified` | Reviewer 已核准 |
| `done` | 已完成 |
| `cancelled` | 已取消 |
| `blocked` | 受外部因素或 dependency 阻擋 |

### Dependency evaluation

- `depends_on` 會影響 view layer 的 effective status。
- Dependency 尚未完成的 open task 會顯示為 blocked。
- Dependency 完成後，task 會自動恢復為 open。
- Claimed / done / cancelled task 永遠不會被 dependency 覆寫。
- 此 evaluation 只存在記憶體內，不會發出 Blocked/Unblocked event。
- 循環或缺漏的 dependency 視為未完成。
- Claim 會尊重套用 dependency 後的 view（不能認領因 dependency 而 blocked 的 task）。
- `started_at` 只會在第一次轉換成 in_progress 時設定一次。

## 5. `task action=create`

- `title` 必填；`description`、`priority`、`assignee`、`depends_on`、`parent_id`、`due_at`、`branch`、`bind`、`eta_secs`、`tags`、`project`、`plan_ack_required`、`plan_ack_reason` 與 `review_class` 選填。
- 預設 priority 為 `normal`。
- `due_at` 接受 RFC 3339。MCP action 不接受 duration 簡寫。
- `project` 選擇 project board；否則使用 caller 目前的 project。由 `parent_id` 指定的 child 必須建立在 parent 的 project board 上。
- `review_class` 儲存 PR-producing work 的 durable `single`/`dual` review threshold。
- 附加 `Created` event，並回傳 `event=created`。
- 不會自動 claim、start 或 complete。

## 6. `task action=list`

- 預設 view 只顯示 actionable task：backlog、open、claimed、in_progress、in_review、blocked。
- `include_history=true` 會包含已完成項目。
- `filter_status`、`filter_assignee` 與 `filter_tag` 可縮小結果；也接受 `status`、`assignee` 與 `tag` alias。
- `project=all` 或 `scope=fleet` 會聚合所有 project board；否則使用 caller 目前的 board。
- `verbose=true` 保留完整 free text，預設則可能截短過長的 description/result。`fields=minimal` 只回傳 id/title/status/assignee/priority。
- `limit` 依 `updated_at` 截取（最新優先）。
- 超過 14 天前完成的項目會從預設 view 移除。
- Response 中的 `filtered_default` 表示是否套用了預設 trimming。
- List 是 pure read，不會變更 board。

## 7. `task action=claim`

- 需要 `id`。
- 驗證 calling instance 存在於 fleet.yaml。
- 尊重 dependency evaluation——因 dependency 而 blocked 的 task 不能被 claim。
- 允許 self-reclaim（重新認領自己的 task）。
- 附加 `Claimed` event，並將 caller 設為 owner。
- 清除 `routed_to`，以反映 ownership 已移轉。

## 8. `task action=done`

- 需要 `id`（或 `task_id` alias）；`result` 選填。
- `done_source` 是 provenance object。一般 caller 只能宣告 `OperatorManual`；只有 daemon system identity 能持久化 PR merge observation 等 forensic variant。
- 以 task owner 作為 `by`（沒有 owner 時退回 caller）。
- `force=true` 可清理 ghost owner，並要求 `force_reason`。
- Force mode 會在 event log 記錄 audit entry。
- 附加 `Done` event。
- 完成後，會 best-effort 嘗試清理綁定 worktree 的 init commit。

## 9. `task action=update`

- 需要 `id`（或 `task_id`）；可變更 `status`、`priority`、`assignee` 與 `tags`。
- Status transition 對應至 canonical event：
  - open → claimed：`Claimed`
  - open → in_progress：`InProgress`
  - any → done：`Done`
  - any → cancelled：`Cancelled`
  - any → blocked：`Blocked`
  - blocked → open：`Unblocked`
  - claimed/in_progress → open：`Released`
  - done → open：`Reopened`
- 可在單次 `append_batch` 中批次處理多項變更，以原子方式持久化。
- ACL rule 與 `done` 相同（owner / orchestrator）。
- 若 task 建立時設定 `plan_ack_required > 0`，`status: in_progress` 還會通過 plan-ack gate（§10）。

### 其他 task action

- `get` 依 `id`/`task_id` 回傳單一 task 的完整 record。
- `activity` 回傳 task 的 event history。
- `metadata_set` 與 `metadata_get` 寫入／讀取具名 metadata value；mutation 遵循 task ACL。
- `ack_plan` 記錄 plan-ack gate 使用、具 idempotency 的非 assignee acknowledgement。

## 10. Plan-Ack Gate（`#2249`）

這是一個 opt-in 的工作前對齊 gate：task 開始前，要求外部人員 ack 其 plan。

- `task action=create`（以及 `send(kind=task)` 的 auto-create 路徑）接受 `plan_ack_required`（integer，預設 `0` = 關閉）與 `plan_ack_reason`（當 `plan_ack_required > 0` 時必須為非空值，其 validation shape 與 `second_reviewer_reason` 相同）。
- 不新增 `TaskEvent` variant——`Created` 之後立即透過兩個 `MetadataSet` event，將 `plan_ack_required`/`plan_ack_reason` 寫入 `Task.metadata`。
- Assignee 透過既有的 `task action=metadata_set metadata_key=plan` 分享 plan。
- `task action=ack_plan`（需要 `id`）以 idempotent 方式將 caller 附加到 `metadata.plan_acks`：
  - Task 自己的 assignee 永遠不能 ack 自己的 plan（`code: self_ack_forbidden`）。
  - Plan 尚未設定就 ack 會被拒絕（`code: plan_not_set`）。
  - 同一 caller 重複 ack 是 no-op（`already_acked: true`，不會重複計數）。
- Gate 位於唯一經驗證的 live chokepoint：`task action=update status=in_progress`。若 `plan_ack_required > 0` 且 distinct ack 數低於 threshold，transition 會以 `{code: "plan_ack_pending", required, acked}` 拒絕，task status 不會前進。
- `plan_ack_required == 0`（預設／缺省情況）會完全略過檢查——對所有未 opt in 的 task 而言，與 #2249 之前的行為 byte-identical。
- 刻意排除的範圍：daemon 不會依 priority/tag 自動觸發、不整合 decision board、不變更 protocol clause——這是純 opt-in primitive，日後其他 automation 可在其上建構。

## 11. `task action=sweep`

- Board hygiene 工具，不是 always-on enforcer。
- 預設為 dry-run（`apply=false`）。
- `apply=true` 需要前一次 dry-run 取得的 `confirm_ids`。
- 處理 stale task 與 linked PR 已關閉的 task。
- Cancelled task 會連同 audit reason 以 batch 發出。

## 12. `task action=health`

- 回傳 read-only board snapshot：total、by_status breakdown、ghost_owners、stale_claims、age aggregate 與 recommendation。
- Ghost owner 分為 **strict**（不在 fleet 或 live registry）與 **soft**（在 fleet，但不在 live registry）。
- Stale claim：超過 `due_at` 的 claimed task。
- Age statistic 只涵蓋 non-terminal task。
- Recommendation 是面向 operator 的下一步提示。

## 13. Event 記錄與 migration

- Append 在寫入前取得 lock；`append_batch` 以原子方式 fsync 多個 event。
- Replay 先 fold archive，再 fold hot log。它是 strict reader——未知 variant 或較高版本 schema 會造成 abort（fail-closed）。
- Legacy `tasks.json` 只在 migration 期間讀取；migration 會將舊 task 轉為 event，並把檔案重新命名為 `.legacy_pre_v2`。
- **不做 single→multi-project backfill（#2117 P3 Gap1）。** Migrated legacy task 沒有 `project_id`，因此會落在 default board 並留在那裡。日後採用 per-project board（#2125）也不會回溯重新分類；只有新建立的 task 會帶有 per-project stamp。這種不對稱是已接受的語意，而不是缺口：legacy task 沒有可供自動分類的訊號，cross-board lookup 仍能透過 full-board-scan fallback 正確運作。要重新安置 legacy task，operator 必須明確移動它。

## 14. ACL 與權限

- 未指派的 task 可由任何 agent 變更。
- Task owner 與其 team orchestrator 可進行變更。
- System identity（`system:auto_orphan`、`system:task_sweep` 等）可 bypass ACL。
- `force` mode 用於歷史資料清理，不是捷徑——必須提供理由。
- ACL 依 replay snapshot 評估（存在小型 TOCTOU window；canonical truth 是 event log）。

## 15. 與其他子系統的互動

- **Teams**——影響 assignee resolution。
- **Worktree / Binding**——`done` 會觸發 best-effort worktree cleanup。
- **Dispatch**——建立 task-to-branch association。
- **CI Watch**——PR merge 時可能將 task 標記為 done。
- **Inbox**——承載 task-related notification。
- **Health**——將 board 作為 operator snapshot。
- **Sweep**——通常會與 CI sweep 一起 review。

## 16. 使用指南

- 建立時一律提供 `title`。
- 以 `branch` 追蹤分支、`eta_secs` 供 stall watchdog 使用、`depends_on` 控制順序、`assignee` 進行 routing。
- 使用 `task action=list` 查看完整 board；加上 `filter_assignee` 可取得個人 view。
- 使用 `task action=health` 檢查卡住或 orphaned 的 task。
- 先以 dry-run 執行 `task action=sweep`，再 apply。
- `force=true` 只保留給歷史資料清理。
- 為維持可追溯性，優先使用 `done` event，不要只提供 plain-text result report。

## 17. 實作檢查清單

- 任何新 event variant 都必須更新 replay fold。
- 任何新 status 都必須更新 list/health projection。
- 所有 write 都必須遵守 `append_batch` atomicity。
- 新 action 必須更新 MCP schema。
- ACL 變更必須包含測試。
- Migration 必須維持 idempotent。
- Board operation 絕不能靜默吞掉 error。
- 新的 report-only feature 應使用 read model。
- 新 write path 應透過 `task_events`。
- 新 sweep rule 也應反映在 health 中。

## 18. 總結

Task board 是 fleet 共用的工作 protocol。其語意由 event 維護，而不是單一可變檔案。主要介面為 `task create/list/get/claim/done/update/sweep/health/activity/metadata_set/metadata_get`，另有 opt-in 的 `ack_plan` 工作前對齊 gate（§10）。預設 list 只顯示 actionable task。Dependency 在 view layer 評估。ACL 為 owner / orchestrator / system identity。Batch append 與 strict replay 是兩項最重要的 invariant。若狀態看起來不對，先檢查 event log，再碰 view。
