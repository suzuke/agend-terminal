[English](FEATURE-teams.md)

# Teams

這份文件說明 team 的設計、如何建立/更新/刪除 team、以及它和
`send team=...` 廣播路徑之間的關係。

## 使用情境

> **適用對象：** Operator 與 agent 皆適用。

**Operator 建立團隊。** Operator 在 `fleet.yaml` 中或透過 `team action=create` MCP 工具定義新團隊——例如建立一個包含 lead、dev、reviewer 的 "fixup" 團隊，並指定 lead 為 orchestrator。這決定了任務的路由方式以及由誰協調團隊工作。

**Agent 團隊廣播。** Lead agent 需要通知團隊中的每個成員某項狀態變更。它不需要逐一發送訊息，而是使用 `send team=fixup` 一次廣播給所有成員。Daemon 會解析團隊成員名單，將訊息送到每個成員的 inbox。

**基於 orchestrator 的任務路由。** 當任務指派給團隊名稱而非特定 agent 時，任務板會透過 `resolve_team_orchestrator` 將任務路由到該團隊的 orchestrator。如果 orchestrator 已被移除且團隊處於 degraded 狀態，路由會失敗——提醒 operator 需要指定新的 orchestrator。

## 1. 設計初衷

- Team 是把 agents 組織成群組的方法。
- 一個 team 對應一個名稱。
- 一個 team 可以有多個 members。
- 一個 team 必須有一個 orchestrator。
- orchestrator 是該 team 的主要協調者。
- team 的用途是結構化協作。
- 它比單一 instance 更適合分工。
- 它也讓廣播可以針對群組發送。
- team 資料現在由 `fleet.yaml` 內的 `teams:` 保存。
- `teams.json` 已經是舊橋接路徑。
- runtime CRUD 直接寫 `fleet.yaml`。
- 讀取也直接從 `fleet.yaml` 投影。
- 因此 team 不是另一份平行真相。
- 它只是 fleet 的一部分。
- 若 team 狀態變怪，先看 `fleet.yaml`。
- 若要看 runtime 差異，再看 list 的 stale_members。

## 2. 檔案與模組

- `src/teams.rs` 是主要實作檔。
- `src/fleet.rs` 是實際存放層。
- `src/mcp/handlers/task.rs` 不碰 team。
- `src/mcp/handlers/dispatch.rs` 會把 `team` 參數送進 `send`。
- `src/mcp/handlers/comms.rs` 會檢查 team 與 orchestrator。
- `src/api/handlers/instance.rs` 會讀 team 資訊做提示。
- `teams.rs` 內的 `Team` 是投影模型。
- `TeamConfig` 才是 `fleet.yaml` 寫入型態。
- `stale_members` 只在 list 投影時補上。
- `find_team_for` 的 projection 不需要 stale_members。
- `degraded` 會在 list 回傳裡附加。
- 這是 operator 可見的警訊欄位。
- 讀者不要把 `degraded` 當成持久欄位。
- 它是 view 層訊號。

## 3. team 資料模型

- `name` 是 team 名稱。
- `members` 是 team 成員清單。
- `orchestrator` 是可選欄位，但語意上很重要。
- `orchestrator` 若缺失，team 變 degraded。
- `description` 是可選說明。
- `created_at` 是 runtime 或 operator 建立時間。
- `source_repo` 是 team 的來源 repo 路徑。
- `stale_members` 是 list 投影用。
- `stale_members` 的值來自 live registry 比對。
- `stale_members` 空時不會輸出到 JSON。
- 這維持 back-compat。
- `find_team_for` 回傳完整 Team。
- `list` 回傳 `{"teams": [...]}`。
- `get_members` 只是抓 member 清單。
- `resolve_team_orchestrator` 是 routing 用。
- `is_orchestrator_of` 是 ACL 用。

## 4. `team action=create`

- `name` 是必要欄位。
- `members` 是必要欄位。
- `orchestrator` 可省略，但建議填。
- `orchestrator` 必須是 members 的其中一個。
- 如果 orchestrator 不在 members 內，create 會錯。
- `repository_path` 可省略。
- 省略時會產生 warning。
- 那個 warning 會提醒你 dispatch binding 可能 fall through。
- create 會先讀 fleet.yaml。
- 若同名 team 已存在，create 會拒絕。
- 若任何 member 已在其他 team，也會拒絕。
- 這是 one-agent-one-team 約束。
- create 成功後會把 team 寫進 `fleet.yaml`。
- 成功回應是 `status=created`。
- 回應也可能帶 `warnings`。
- source_repo 缺失時的 warning 不阻擋建立。
- 但 operator 應盡快補上。

## 5. `team action=list`

- list 會回傳所有 team。
- list 的資料來源是 `fleet.yaml`。
- list 會呼叫 `runtime::list_live_agents`。
- 如果 live registry 讀不到，stale_members 會維持空。
- list 會比對每個 member 是否在 live。
- 不在 live 的 member 會進入 stale_members。
- stale_members 會排序。
- team 本體也會排序投影。
- 這讓結果比較穩定。
- list 也會加上 `degraded`。
- `degraded=true` 代表 orchestrator 缺失。
- 這是 operator 要看的重要訊號。
- list 不是 mutating action。
- list 也不會自動修復 team。

## 6. `team action=update`

- update 需要 `name`。
- update 可加 `add`。
- update 可加 `remove`。
- update 可加 `orchestrator`。
- update 可加 `repository_path`。
- `remove` 不能移除目前 orchestrator。
- 若要換 orchestrator，要先指定新的 orchestrator。
- 新 orchestrator 必須在更新後的 members 裡。
- `add` 不能把 member 加進另一個 team。
- 這個檢查遵守 one-agent-one-team。
- `repository_path` 若沒給，會沿用舊值。
- update 成功後會寫回 `fleet.yaml`。
- update 回傳 `status=updated`。
- update 失敗時會回傳 error。
- update 是 team 結構調整的主要工具。

## 7. `team action=delete`

- delete 需要 `name`。
- delete 會先 snapshot team 存在與 members。
- 如果 team 不存在，直接回錯。
- delete 不只是移除 yaml 節點。
- delete 會 cascade 刪除每一個 member instance。
- cascade 會呼叫 `full_delete_instance`。
- 這會沿用 instance teardown 的標準流程。
- 若某 member 刪除失敗，delete 會收集警告。
- delete 會盡可能繼續清理其他 member。
- 如果最後 team 變成空了，也算成功。
- `remove_team_from_yaml` 的結果不一定只有一種成功路徑。
- 只要 team 最後不在 fleet.yaml 就算完成目的。
- delete 成功回應會包含 `members_cleaned`。
- 如果有警告，回應會帶 `cascade_warnings`。

## 8. 成員刪除與自動降級

- `remove_member_from_all` 會把 instance 從所有 team 移除。
- 它也會處理 orchestrator 身分。
- 如果移除後 team 沒成員了，team 直接刪掉。
- 如果移除的是 orchestrator，但 team 還有其他成員，team 會 degraded。
- degraded team 的 orchestrator 會變成 None。
- degraded team 不會自動找新 orchestrator。
- 這是 operator 要接手的部分。
- 函式會對每個 degraded team 建 urgent task。
- 任務標題會提示哪個 team 失去 orchestrator。
- 這把問題從隱性降級轉成明顯待辦。
- `remove_member_from_all` 是 teardown 的一部分。
- 不是一般協作的常用入口。
- 但它對刪除 instance 很重要。

## 9. 參考查詢

- `find_team_for(home, member)` 會回傳 member 所屬 team。
- 找不到就回 `None`。
- `get_members(home, team_name)` 只回 members 清單。
- `resolve_team_orchestrator(home, name)` 用於 assignee routing。
- 若 name 是 team，會回 orchestrator。
- 若 team degraded，會回 Err。
- 若 name 不是 team，會回 Ok(None)。
- `is_orchestrator_of(home, caller, member)` 用於 ACL。
- 它會檢查 caller 是否是 member 所屬 team 的 orchestrator。
- 這些 helper 很常被 task board 讀到。
- 也會被 comms / dispatch 路徑讀到。
- 它們是讀模型，不是 mutation。
- 建文件時要區分這兩類。

## 10. 與 `send team=...` 的關係

- `send` tool 支援 `team` 參數。
- `team=fixup` 代表廣播給整個 team。
- 這不是發給 team 名稱本身。
- 實際上會散發到 team 的成員。
- `send` 也支援 `instances`、`tags`、`instance`。
- team 廣播是其中一種路由方式。
- 只要 team 成員集合變動，廣播目標也會變。
- 這表示 `team list` 的 stale_members 很實用。
- stale_members 幫你看哪些人會收不到廣播。
- team 廣播與 orchestrator routing 是兩件事。
- orchestrator 用於 task 路由與 ACL。
- team 廣播用於訊息分發。
- 兩者常一起出現，但語意不同。

## 11. 與 task board 的關係

- task assignee 可以是一個 team name。
- 如果 assignee 是 team，task 會路由到 orchestrator。
- 這個 route 由 `resolve_team_orchestrator` 決定。
- 如果 team degraded，task 不能 route。
- 因此 team 健康會影響 task 生命週期。
- `team delete` 也會牽動 task orphan cleanup。
- 某些刪除後會產生 urgent task。
- 這是團隊治理的訊號。
- task board 可以幫你追 team 的工作。
- team list 可以幫你追 task board 的可達性。

## 12. 與 instance lifecycle 的關係

- 刪 instance 時會呼叫 `remove_member_from_all`。
- 這會把 instance 從所有 team 中拿掉。
- 如果 instance 是 orchestrator，team 會 degraded。
- 如果 instance 是唯一成員，team 會消失。
- 這是 instance teardown 的一環。
- 因此 team 並不是孤立資料結構。
- 它會跟 instance 生滅一起變動。
- 這也是為什麼 list 要顯示 stale_members。
- 這讓 operator 看到名單落差。

## 13. 行為約束

- team 名稱應該穩定。
- member 名單應該避免重複。
- orchestrator 應該總是屬於 members。
- source_repo 最好不要缺。
- 如果缺了，dispatch 自動 bind 會掉到較弱的 fallback。
- update 不應讓 team 掉到無法路由的狀態。
- delete 不應默默跳過整個 team。
- delete 的 cascade 失敗要可見。
- list 的 stale_members 應該維持排序。
- degraded 狀態要讓 operator 一眼看懂。

## 14. 常見操作流程

- 新團隊：先 create。
- 建議一開始就指定 orchestrator。
- 若要調整成員，使用 update。
- 若要重定向協調者，先確保新 orchestrator 仍在成員中。
- 若要移除成員，注意不要移除最後的 orchestrator。
- 若要解散團隊，使用 delete。
- 若只是確認誰是誰的 orchestrator，用 find_team_for。
- 若要看整體健康，用 team list。
- 若要把訊息丟給整團，使用 send team=...。
- 若要清掉失聯成員，先看 stale_members。

## 15. 實作檢查點

- `fleet.yaml` 是唯一寫入點。
- `list` 只是投影，不要變成寫路徑。
- `source_repo` 缺失應該可被 warning 看見。
- `degraded` 不應被當作持久欄位。
- `stale_members` 不應直接寫進 fleet.yaml。
- delete 的 cascade 要保留錯誤彙整。
- 更新 orchestrator 時要先檢查 post-mutation members。
- one-agent-one-team 是重要 invariant。
- broadcast 與 routing 不可混為一談。
- 任何新欄位都要確認 projection 與存放一致。

## 16. 小結

- Team 是協作分群的基本單位。
- 它的真相在 `fleet.yaml`。
- `team create/delete/list/update` 是主要治理工具。
- `send team=...` 是主要廣播入口。
- orchestrator 是 team 的核心協調點。
- degraded team 不是壞掉的資料，而是需要 operator 處理的狀態。
- stale_members 是可觀測性欄位。
- 如果 team 行為異常，先看 fleet，再看 live registry。
- 這個模組的設計目標是讓 team 成為清楚、可查、可恢復的協作結構。