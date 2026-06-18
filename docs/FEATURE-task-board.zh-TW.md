[English](FEATURE-task-board.md)

# Task Board

這份文件說明共享任務板的設計目的、資料流、MCP 使用方式、以及
哪些行為是有保證的，哪些只是 best-effort。

## 使用情境

> **適用對象：** Agent 基礎設施——agent 透過 MCP tools 使用，operator 通常不直接操作。

**任務分派與追蹤。** Lead agent 在任務板上建立一個任務，填入標題、優先級和指派對象，然後分派給 dev agent。Dev 在 inbox 中收到任務後，透過 `task action=claim` 認領並開始工作。工作過程中，dev 將任務狀態更新為 `in_progress`。完成後，dev 以 `done` 標記任務並附上結果摘要。整個生命週期以 append-only 事件記錄在 `task_events.jsonl` 中。

**跨 agent 可見性。** Reviewer agent 查看任務板，找出已標記 `done` 且待驗證的任務。完成 PR 審查後，reviewer 將任務標記為 `verified`。同時，operator 可以隨時透過 TUI 觀察完整的任務板狀態，無需逐一查詢各個 agent。

**任務板衛生。** 隨著時間推移，任務會堆積——有些 owner 已不在 fleet 中（ghost owners），有些截止日期已過。Operator 執行 `task action=health` 取得任務板健康快照，再使用 `task action=sweep` 的 dry-run 模式找出清理候選項，確認後才實際執行。

## 1. 設計初衷

- Task board 是整個 fleet 共用的工作清單。
- lead 用它分派工作。
- agent 用它認領工作。
- reviewer 用它做驗收與收尾。
- operator 可以直接觀察全局狀態。
- 它的核心價值是「每個人看同一份狀態」。
- 它不是個人待辦清單。
- 它也不是單純的訊息信箱。
- 它是可回放的狀態機。
- 目前 canonical source 不是 `tasks.json`。
- canonical source 是 `task_events.jsonl`。
- `tasks.json` 是舊格式遷移來源。
- 新的讀路徑透過 replay 折疊事件。
- 這表示 board 的歷史是 append-only。
- 也表示狀態可重建。
- 這是審計與回溯的基礎。
- 也是 task sweep 與 health 的共同基礎。
- 任何新功能都應該沿用這個模型。
- 如果要改 board 狀態，優先考慮事件，而不是直接改檔。

## 2. 檔案與模組

- `src/task_events.rs` 負責事件格式與 replay。
- `src/tasks.rs` 負責 MCP board surface 與舊資料橋接。
- `src/mcp/handlers/task.rs` 只是把 MCP call 轉給 `tasks.rs`。
- `task_events.jsonl` 是 canonical event log。
- `task_events_archive/` 是歷史封存區。
- `tasks.json` 是 legacy bridge 檔。
- 讀者應把 `tasks.json` 視為過渡資料。
- 寫入者應把 `task_events.jsonl` 視為真相。
- `TaskBoardState` 是 replay fold 的輸出。
- `TaskRecord` 是 board 的讀模型。
- `TaskEvent` 是寫模型。
- `TaskId` 與 `InstanceName` 都是 newtype。
- 這是為了避免 ID 混用。
- replay 對未知 variant 失敗。
- replay 對高於支援版本的 envelope 失敗。
- 這是 fail-closed。
- append 路徑採用單調 seq。
- seq 是每個 emitter 各自遞增。
- 這讓同一 emitter 的事件可以排序。
- 同時也保留跨 emitter 的折疊順序。

## 3. 資料模型

- `Task` 是 MCP/reader 用的公共結構。
- `TaskRecord` 是 replay 的 canonical view。
- `Task` 內的 `status` 是字串。
- `TaskRecord` 內的 `status` 是 enum。
- `Task` 保留前相容欄位。
- `TaskRecord` 保留歷史欄位。
- `TaskEvent::Created` 帶 title、description、priority。
- `TaskEvent::Created` 可帶 owner。
- `TaskEvent::Created` 可帶 due_at。
- `TaskEvent::Created` 可帶 depends_on。
- `TaskEvent::Created` 可帶 routed_to。
- `TaskEvent::Created` 可帶 branch。
- `TaskEvent::Created` 可帶 bind。
- `TaskEvent::Created` 可帶 eta_secs。
- `TaskEvent::Claimed` 會設定 owner。
- `TaskEvent::InProgress` 會設定 owner。
- `TaskEvent::Done` 會把狀態變 Done。
- `TaskEvent::Cancelled` 會把狀態變 Cancelled。
- `TaskEvent::Blocked` 會寫入 block_reason。
- `TaskEvent::Unblocked` 會清掉 block_reason。
- `TaskEvent::Released` 會清掉 owner 與 routed_to。
- `TaskEvent::Reopened` 會保留 owner。
- `TaskEvent::OwnerAssigned` 只改 owner/routed_to。
- `TaskEvent::PriorityChanged` 只改 priority。
- `TaskEvent::Verified` 會更新狀態但不 close。
- `TaskEvent::Linked` 會追加 PR link。
- `TaskEvent::TaskCloseProposed` 是審核提案事件。

## 4. 狀態語意

- `open` 是待認領。
- `claimed` 是有人接手。
- `in_progress` 是已開始執行。
- `verified` 是 reviewer 已核准。
- `done` 是完成。
- `cancelled` 是取消。
- `blocked` 是被阻擋。
- `depends_on` 會影響 view 層的有效狀態。
- 依賴未完成時，open 會視為 blocked。
- 依賴完成後，blocked 會自動回 open。
- claimed / done / cancelled 不會被依賴覆寫。
- 這個依賴 eval 是 in-memory。
- 它不會寫成 Blocked/Unblocked 事件。
- 這樣可避免把 view 狀態誤寫進歷史。
- circular dependency 會被視為 blocked。
- 缺失的 dependency 也會視為未完成。
- claim 會尊重依賴後的 view。
- 這避免認領一個尚未解除依賴的 task。
- `started_at` 只在第一次進入 in_progress 時寫入。
- `updated_at` 會隨事件更新。

## 5. `task action=create`

- `title` 是必要欄位。
- `description` 可選。
- `priority` 可選。
- 預設 priority 是 `normal`。
- `assignee` 可選。
- `depends_on` 可選。
- `due_at` 可直接傳 RFC3339。
- `duration` 可傳 `30m`、`1h`、`2d`。
- `branch` 會進入 record。
- `bind` 可關閉自動 bind。
- `eta_secs` 可啟用 stall watchdog。
- create 會先 append `Created` 事件。
- 成功後回傳 `event=created`。
- 回傳內的 `task.status` 會是 `open`。
- 回傳也保留舊欄位 `status=created`。
- 這是 back-compat alias。
- create 後可直接被 list 看見。
- create 不會自動 claim。
- create 不會自動 start。
- create 也不會自動 done。

## 6. `task action=list`

- list 預設只列 actionable。
- actionable 包含 `open`、`claimed`、`in_progress`、`blocked`。
- 若 `include_history=true`，會列出歷史完成項目。
- 若指定 `filter_status`，會尊重明確過濾。
- 若指定 `filter_assignee`，會只看該 owner。
- `limit` 會按 `updated_at` 由新到舊截斷。
- 完成超過 14 天的項目在預設視圖會被壓掉。
- `filtered_default` 會告知 caller 是否套用預設 trim。
- 這讓 UI 不用自己猜是否少了項目。
- list 的來源是 replay 之後再做 view 過濾。
- 所以 list 看到的是 canonical fold 後的結果。
- list 不會直接讀舊 `tasks.json`。
- list 也不會修改 board。
- list 是純讀操作。

## 7. `task action=claim`

- claim 需要 `id`。
- claim 前會確認 instance 存在於 fleet.yaml。
- claim 會先看 `list_all` 的 view。
- 這表示它會尊重 dependency eval。
- 若 task 不是 open，通常拒絕。
- 例外是 self reclaim。
- self reclaim 指的是本人再次 claim 自己已 claimed 的 task。
- claim 會 append `Claimed` 事件。
- 成功後回傳 `event=claimed`。
- 回傳中的 `task.status` 會是 `claimed`。
- claim 會把 owner 寫成呼叫者。
- claim 不會保留原本 routed_to。
- routed_to 會在 claim 時清掉。
- 這反映 ownership 已移轉到具體 instance。

## 8. `task action=done`

- done 需要 `id`。
- done 可帶 `result`。
- done 可帶 `done_source`。
- done 會先讀 replay record。
- done 預設採 owner 作為 by。
- 若沒有 owner，就用 caller。
- `force=true` 會啟用 ghost-owner cleanup。
- `force_reason` 在 force 模式下必填。
- force 會記錄 event_log audit entry。
- force 也會把 caller 與 reason 寫進事件 result。
- 這樣 replay trail 也看得到審計原因。
- done 會 append `Done` 事件。
- 成功後回傳 `event=done`。
- 回傳中的 `task.status` 會是 `done`。
- done 後會嘗試清理 bound worktree 的 init commit。
- 那是 best-effort。
- 清理失敗不會阻塞 done 回應。

## 9. `task action=update`

- update 需要 `id`。
- update 可改 `status`。
- update 可改 `priority`。
- update 可改 `assignee`。
- update 可帶 `force`。
- update 可帶 `force_reason`。
- status transition 會對應 canonical event。
- `open -> claimed` 會發 Claimed。
- `open -> in_progress` 會發 InProgress。
- `* -> done` 會發 Done。
- `* -> cancelled` 會發 Cancelled。
- `* -> blocked` 會發 Blocked。
- `blocked -> open` 會發 Unblocked。
- `claimed/in_progress -> open` 會發 Released。
- `done -> open` 會發 Reopened。
- priority change 會發 PriorityChanged。
- assignee change 會發 OwnerAssigned。
- 多個變更可合併在同一次 append_batch。
- 這讓一個 update 呼叫可以原子落盤。
- update 的 ACL 跟 done 一樣看 owner/orchestrator。

## 10. `task action=sweep`

- sweep 是操作板清理工具。
- 它不是自動常駐的強制行為。
- 它支援 dry-run。
- `apply=false` 是預設。
- apply 前需要先有 `confirm_ids`。
- apply 會依 dry-run 提供的候選執行取消。
- sweep 主要處理陳舊任務分類。
- 它也會處理 PR close 對應的 task 結案。
- sweep 的實作會走 `scan_categories`。
- sweep 的 apply 會批次 emit Cancelled。
- sweep 會記錄 audit reason。
- sweep 會把結果寫進 event_log。
- 這讓 operator 可以先看 plan 再決定。

## 11. `task action=health`

- health 是一張板的快照。
- 它不是變更。
- 它回傳 totals。
- 它回傳 by_status。
- 它回傳 ghost_owners。
- 它回傳 stale_claims。
- 它回傳 age aggregates。
- 它回傳 recommendations。
- ghost_owners 分 strict 和 soft。
- strict 代表 owner 已不在 fleet 和 live。
- soft 代表 owner 還在 fleet 但不在 live。
- stale_claims 會找過期 due_at 的 claimed tasks。
- age 統計只看 non-terminal。
- recommendations 是 operator 的 next-step hint。
- health 對 board hygiene 很重要。
- 它也讓 operator 先看到風險再 sweep。

## 12. 事件記錄與遷移

- canonical log 是 `task_events.jsonl`。
- append 會先拿 lock 再寫。
- append_batch 會一次 fsync 多個事件。
- replay 會先 fold archive，再 fold hot log。
- replay 是 strict reader。
- replay 看到未知 event variant 會 abort。
- replay 看到高版本 schema 會 abort。
- 這是刻意 fail-closed。
- `tasks.json` 只在 migration 中出現。
- migration 會把 legacy tasks 轉成事件。
- migration 成功後會把舊檔改名成 `.legacy_pre_v2`。
- 這樣 operator 還能考古。
- **不做單→多 project 回溯(#2117 P3 Gap1)。** 遷移過來的 legacy task 沒有 `project_id`,會落在 default board 並留在那裡。日後改用 per-project boards(#2125)不會回溯重歸這些舊任務——只有新建任務才會 per-project stamp。這個不對稱是接受的語意、不是缺口:舊任務沒有 auto-bucket 的訊號,而跨 board 查詢靠 full-board-scan fallback 仍正確。要把舊任務改 board,由 operator 顯式搬移。
- board 的讀面已不依賴舊檔。
- board 的寫面也不應回去改舊檔。

## 13. ACL 與權限

- 未指派 task 可由任何人 mutating。
- owner 可以改自己的 task。
- owner 的 team orchestrator 也可以改。
- team assignee 的 orchestrator 也可以改。
- system identities 有 allow-list bypass。
- `system:auto_orphan`、`system:task_sweep` 等屬於系統身分。
- force 模式是歷史資料清理用途。
- force 不是一般使用者的捷徑。
- force 必須有 reason。
- ACL 判斷是 replay snapshot 上的 view。
- 這表示有很小的 TOCTOU 窗口。
- 但 canonical truth 還是 event log。
- 衝突最後會由 replay 後序事件決定。

## 14. 與其他子系統的關係

- `team` 模組會影響 assignee 解析。
- `worktree` / `binding` 會影響 done 後清理。
- `dispatch` 會建立 task 與 branch 的關聯。
- `ci_watch` 可能把 PR 成果回寫成 Done。
- `decision` 不直接管 task board。
- `inbox` 會承載 task-related 通知。
- `health` 會把 task board 當作 operator snapshot。
- `task sweep` 常跟 `ci sweep` 一起看。
- `release_worktree` 不會直接改 task board。
- 但 release 後常會清理與 task 相關的檔案狀態。

## 15. 操作範例

- 建立任務時先填 `title`。
- 如果是追蹤性工作，填 `branch`。
- 如果要估時，填 `eta_secs`。
- 如果有依賴，就填 `depends_on`。
- 如果任務屬於某人，填 `assignee`。
- 如果要跨 team，先確認 team 已建立。
- 如果要查全板，先跑 `task action=list`。
- 如果只看自己，搭配 `filter_assignee`。
- 如果要看卡住狀態，查 `task action=health`。
- 如果要清理過期 claim，用 `task action=sweep`。
- 如果要驗證 sweep 影響，先 dry-run 再 apply。
- 如果要做歷史清理，才考慮 `force=true`。
- 如果 reviewer 要接棒，通常看 `branch` 與 `task_id`。
- 如果要回報結果，done 事件比純文字更可追溯。

## 16. 實作檢查點

- 任何新事件 variant 都要更新 replay fold。
- 任何新狀態都要更新 list/health 投影。
- 任何寫入都要考慮 append_batch 原子性。
- 任何新增 action 都要更新 MCP schema。
- 任何 ACL 變更都要補測試。
- 任何 migration 都要保留 idempotency。
- 任何 board 行為都不要默默吞掉錯誤。
- 若要做 report-only 功能，優先做 read model。
- 若要改 board 寫路徑，先看 task_events。
- 若要修改 task UI，先確認 replay 仍能重建。
- 若要新增 sweep 類規則，先檢查 health 是否也要反映。
- 若要改 legacy `tasks.json`，要確認它是否仍是 bridge。

## 17. 總結

- Task board 是 fleet 的共享工作協議。
- 它的語意靠事件維持，而不是靠單一 mutable 檔案。
- `task create/list/claim/done/update/sweep/health` 是主要使用面。
- 預設清單是 actionable。
- 依賴是 view 層自動計算。
- ACL 是 owner / orchestrator / system identity。
- 批次 append 與 strict replay 是最重要的兩個 invariant。
- 如果看到 board 狀態不對，先查事件，不要先改視圖。
- 如果要擴充 board，先更新 event model。
- 如果要 debug，先看 `task_events.jsonl` 與 replay。
- 這就是這個模組的設計核心。