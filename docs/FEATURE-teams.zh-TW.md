[English](FEATURE-teams.md)

# Teams

Team 將 agent 組成具名單位，以進行結構化協作。每個 team 都有 members、optional（但強烈建議設定）的 orchestrator，並儲存在 `fleet.yaml` 中，而不是獨立的資料來源。

## 使用情境

> **適用對象：** Operator 與 agent。

**Operator 設定 team。** Operator 在 `fleet.yaml` 或透過 `team action=create` MCP 工具定義新 team——例如建立一個由 lead、dev 與 reviewer 組成的「fixup」team，並指定 lead 為 orchestrator。這會建立 task routing 的結構，並明確指定由誰協調團隊工作。

**Agent 對 team 廣播。** Lead agent 需要通知 team 的所有 member 某項狀態變更。它不必逐一發送訊息，而是使用 `send team=fixup` 一次廣播給所有 member。Daemon 解析 team membership，並將訊息送到每個 member 的 inbox。

**以 orchestrator 為基礎的 task routing。** 當 task 被指派給 team name，而非特定 agent，task board 會透過 `resolve_team_orchestrator` 將它 route 給 team orchestrator。如果 orchestrator 已被移除、team 處於 degraded 狀態，routing 會失敗，提示 operator 指定新的 orchestrator。

## 1. 設計理念

- Team 是一組具名 agent，並具有指定的 orchestrator。
- Orchestrator 協調 team 內的工作。
- Team 支援結構化協作、分工與針對性 broadcast。
- Team data 位於 `fleet.yaml` 的 `teams:` key 下。
- `teams.json` 是 legacy bridge path；runtime CRUD 會直接讀寫 `fleet.yaml`。
- Team 狀態看起來不對時，先檢查 `fleet.yaml`。

## 2. 檔案與模組

- `src/teams.rs`——主要實作（projection model）。
- `src/fleet/mod.rs`——實際 storage layer。
- `src/mcp/handlers/dispatch.rs`——將 `team` parameter route 到 `send`。
- `src/mcp/handlers/comms.rs`——檢查 team 與 orchestrator relationship。
- `src/api/handlers/instance.rs`——讀取 team info 以注入 prompt。
- `Team` 是 projection model；`TeamConfig` 是 `fleet.yaml` 的 write type。
- `stale_members` 只會在 list projection 中填入。
- `degraded` 是 view-layer signal，不是 persisted field。

## 3. 資料模型

| 欄位 | 說明 |
|------|------|
| `name` | Team name |
| `members` | Member instance name 清單 |
| `orchestrator` | 選填；team 的 coordinator（應為 member） |
| `description` | Optional description |
| `created_at` | 建立時間戳 |
| `source_repo` | Team 的 source repository path |
| `project_id` | Optional 的 explicit project-board id override |
| `accept_from` | 可直接傳訊給 orchestrator 的外部 agent name；空值會拒絕 cross-team direct send |
| `stale_members` | 僅供 view；在 live registry 找不到的 member |

## 4. `team action=create`

- `name` 與 `members` 必填。
- `orchestrator` 選填但建議設定；必須是 member 之一。
- `repository_path` 選填；省略時會警告 dispatch binding fallback。
- `project_id` 可選擇性覆寫 project-board slug derivation，且不變更 `repository_path`。
- `accept_from` 設定 cross-team direct-send allowlist；預設空 list 為 fail-closed。
- 若同名 team 已存在則拒絕建立。
- 強制執行 **one-agent-one-team** constraint：若任何 member 已屬於另一 team，便拒絕。
- 成功時將 team 寫入 `fleet.yaml`，並回傳 `status=created`。

## 5. `team action=list`

- 回傳 `fleet.yaml` 中的所有 team。
- 將 member 與 live agent registry 交叉比對。
- 在 live registry 中找不到的 member 會出現在 `stale_members`（已排序）。
- Orchestrator 缺漏時加入 `degraded=true`。
- Pure read operation——不會修改 team。

## 6. `team action=update`

- 需要 `name`。
- 支援 `add`（新 member）、`remove`（既有 member）、`orchestrator`、`repository_path`、`project_id` 與 `accept_from`。
- 未先指定新 orchestrator，不得移除目前的 orchestrator。
- 新 orchestrator 必須位於 update 後的 member list 中。
- `add` 強制執行 one-agent-one-team（不能加入已屬於另一 team 的 member）。
- 未明確變更時，保留 `repository_path`。
- 未明確提供時，也保留 `project_id` 與 `accept_from`。
- 成功時寫回 `fleet.yaml`。

## 7. `team action=delete`

- 需要 `name`。
- 透過 `full_delete_instance` cascade delete 每一個 member instance。
- 若個別 member delete 失敗，會收集 warning 並繼續清理。
- 從 `fleet.yaml` 移除 team。
- 回傳 `members_cleaned` 與所有 `cascade_warnings`。

## 8. Member 移除與自動降級

- `remove_member_from_all` 會從 instance 所屬的每個 team 移除它。
- 若被移除的 member 是 orchestrator，且仍有其他 member，team 會變成 **degraded**（orchestrator 設為 None）。
- 若被移除的 member 是最後一位 member，整個 team 會被刪除。
- Degraded team 不會自動選出新 orchestrator——這需要 operator 介入。
- 每個新 degraded team 都會建立一個 urgent task。
- 此 function 是 instance teardown 的一部分，不是一般 collaboration 操作。

## 9. Reference query

- `find_team_for(home, member)`——回傳 member 所屬的 team。
- `get_members(home, team_name)`——回傳 member list。
- `resolve_team_orchestrator(home, name)`——解析供 routing 使用的 orchestrator；degraded team 會回傳 error。
- `is_orchestrator_of(home, caller, member)`——ACL check。
- 這些 helper 公開 read model，不會改變 team state。

## 10. 與 `send team=...` 的關係

- `send` 支援 `team` parameter 進行 broadcast delivery。
- `team=fixup` 會廣播給 fixup team 的所有 member。
- 每當 member list 變更，broadcast target 也隨之變更。
- `stale_members` 有助於識別不會收到 broadcast 的 member。
- Team broadcast（message delivery）與 orchestrator routing（task/ACL routing）是兩種不同操作，但經常一起出現。

## 11. 與 task board 的關係

- Task 的 `assignee` 可以是 team name。
- 指派給 team 時，task 會透過 `resolve_team_orchestrator` route 給 orchestrator。
- Degraded team 無法 route task。
- `team delete` 可能觸發 task orphan cleanup 與 urgent task 建立。

## 12. 與 instance lifecycle 的關係

- Delete instance 會呼叫 `remove_member_from_all`。
- 若 instance 原本是 orchestrator，其 team 會 degrade。
- 若 instance 是唯一 member，其 team 會被刪除。
- `team list` 的 `stale_members` 會顯示 member roster 與 live registry 之間的差異。

## 13. 行為約束

- Team name 應保持穩定。
- Member list 不得含有重複項目。
- Orchestrator 必須永遠是 member。
- 應設定 `source_repo`；若未設定，dispatch auto-bind 會退回較弱的路徑。
- 當 repository-path slug derivation 有歧義時，應讓 `project_id` 與預期 task board 保持一致。
- `accept_from` 應保持最小範圍；空 list 會刻意拒絕對 orchestrator 的 cross-team direct send。
- `update` 不應讓 team 留在無法 routing 的狀態。
- `delete` cascade failure 必須可見。
- `stale_members` 輸出必須排序。
- `degraded` status 必須讓 operator 一眼可辨。

## 14. 典型工作流程

1. 以 `team action=create` 建立 team（從一開始就指定 orchestrator）。
2. 以 `team action=update` 調整 member。
3. 要更換 orchestrator，請確保新 orchestrator 仍在 member list 中。
4. 要解散 team，使用 `team action=delete`。
5. 要檢查 health，使用 `team action=list` 並查看 `stale_members` / `degraded`。
6. 要廣播訊息，使用 `send team=...`。
7. 要找出誰協調誰，使用 `find_team_for`。

## 15. 實作檢查清單

- `fleet.yaml` 是唯一 write target。
- `list` 是 projection——不能成為 write path。
- 缺少 `source_repo` 時應顯示 warning。
- `degraded` 不得持久化。
- `stale_members` 不得寫入 `fleet.yaml`。
- `delete` cascade 必須保留 error aggregation。
- Orchestrator update 必須針對 mutation 後的 member 驗證。
- One-agent-one-team 是關鍵 invariant。
- 不得混淆 broadcast 與 routing。

## 16. 總結

Team 是協作分組的基本單位，其唯一真相來源為 `fleet.yaml`。CRUD operation 為 `team create/delete/list/update`；broadcast 則透過 `send team=...`。Orchestrator 是 team 的協調點。Degraded team 不是損壞的資料，而是需要 operator 注意的狀態。`stale_members` 是 observability field。Team 行為不如預期時，先檢查 fleet，再檢查 live registry。
